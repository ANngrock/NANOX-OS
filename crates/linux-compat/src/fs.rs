//! Calls that name files by path and calls that move bytes: `open`, `read`,
//! `write`, `lseek`, the `stat` family, directories, renames.
//!
//! A path argument is read from guest memory, resolved lexically against the
//! working directory (or the directory a descriptor was opened at) and only
//! then given to the backend, so no path can leave the delegated root.

use crate::abi::*;
use crate::backend::Backend;
use crate::errno::*;
use crate::fdtable::{Kind, Obj, Ofd};
use crate::handlers::fd_of;
use crate::mem::{check_range, read_cstr, read_u64, write_u32, GuestMem, IOV_MAX, PATH_MAX};
use crate::path::resolve;
use crate::personality::{Personality, DIR_PATH};

/// Bytes moved per backend call.
const CHUNK: usize = 4096;
/// The most one read or write moves, as on Linux.
pub(crate) const MAX_RW: u64 = 0x7fff_f000;

/// A path argument: the string as the program wrote it, and its normalized form.
pub(crate) struct PathArg {
    raw: [u8; PATH_MAX],
    out: [u8; PATH_MAX],
    len: usize,
    dir_only: bool,
}

impl PathArg {
    pub(crate) fn new() -> Self {
        Self {
            raw: [0; PATH_MAX],
            out: [0; PATH_MAX],
            len: 0,
            dir_only: false,
        }
    }

    pub(crate) fn path(&self) -> &[u8] {
        &self.out[..self.len]
    }

    pub(crate) fn is_root(&self) -> bool {
        self.path() == b"/"
    }
}

/// What a path argument turned out to be.
#[derive(PartialEq, Eq)]
pub(crate) enum Located {
    Path,
    /// The empty string with `AT_EMPTY_PATH`: the descriptor itself.
    Empty,
}

impl<B: Backend> Personality<B> {
    /// The directory a descriptor was opened at.
    fn dir_path(&self, fd: usize) -> Result<&[u8], Errno> {
        let o = self.fds.get(fd)?;
        if o.kind != Kind::Dir {
            return Err(ENOTDIR);
        }
        self.dirs
            .iter()
            .find(|d| d.used && d.obj == o.obj)
            .map(|d| &d.path[..usize::from(d.len)])
            .ok_or(EBADF)
    }

    /// Reads the path at `ptr` and resolves it against the working directory
    /// or, for a relative path with a directory descriptor, against that directory.
    pub(crate) fn locate(
        &self,
        mem: &dyn GuestMem,
        dirfd: u64,
        ptr: u64,
        p: &mut PathArg,
        allow_empty: bool,
    ) -> Result<Located, Errno> {
        let n = read_cstr(mem, ptr, &mut p.raw)?;
        if n == 0 {
            return if allow_empty {
                Ok(Located::Empty)
            } else {
                Err(ENOENT)
            };
        }
        let raw = &p.raw[..n];
        let base = if raw[0] == b'/' || dirfd as i32 == AT_FDCWD {
            &self.cwd[..self.cwd_len]
        } else {
            self.dir_path(fd_of(dirfd)?)?
        };
        let r = resolve(base, raw, &mut p.out)?;
        p.len = r.len;
        p.dir_only = r.dir_only;
        Ok(Located::Path)
    }

    /// Closes a backend object and forgets the directory it stood for.
    pub(crate) fn release(&mut self, obj: Obj) {
        self.backend.close(obj);
        for d in self.dirs.iter_mut().filter(|d| d.used && d.obj == obj) {
            d.used = false;
        }
    }

    /// Closes descriptor `fd` (and its object if that was the last use).
    pub(crate) fn drop_fd(&mut self, fd: usize) {
        if let Ok(Some(obj)) = self.fds.close(fd) {
            self.release(obj);
        }
    }

    pub(crate) fn sys_close(&mut self, a: [u64; 6]) -> Result<u64, Errno> {
        let fd = fd_of(a[0])?;
        if let Some(obj) = self.fds.close(fd)? {
            self.release(obj);
        }
        Ok(0)
    }

    /// `open`, `openat`: a new descriptor for the object at the path.
    pub(crate) fn sys_open(
        &mut self,
        mem: &dyn GuestMem,
        dirfd: u64,
        ptr: u64,
        flags: u64,
        mode: u64,
    ) -> Result<u64, Errno> {
        let flags = flags as u32;
        let mut p = PathArg::new();
        self.locate(mem, dirfd, ptr, &mut p, false)?;
        if flags & O_CREAT != 0 && p.dir_only {
            return Err(EISDIR);
        }
        let mode = (mode as u32) & !self.umask & 0o7777;
        let want_dir = p.dir_only || flags & O_DIRECTORY != 0;
        let (obj, kind) = self.backend.open(p.path(), flags, mode)?;
        match self.install(obj, kind, flags, want_dir, p.path()) {
            Ok(fd) => Ok(fd as u64),
            Err(e) => {
                self.backend.close(obj);
                Err(e)
            }
        }
    }

    fn install(
        &mut self,
        obj: Obj,
        kind: FileKind,
        flags: u32,
        want_dir: bool,
        path: &[u8],
    ) -> Result<usize, Errno> {
        if want_dir && kind != FileKind::Dir {
            return Err(ENOTDIR);
        }
        let k = match kind {
            FileKind::File => Kind::File,
            FileKind::Dir => {
                if flags & O_ACCMODE != 0 {
                    return Err(EISDIR);
                }
                Kind::Dir
            }
            FileKind::Symlink => return Err(ELOOP),
            FileKind::Fifo if flags & O_ACCMODE == O_WRONLY => Kind::PipeWrite,
            FileKind::Fifo => Kind::PipeRead,
            FileKind::Char => Kind::Tty,
        };
        let slot = if k == Kind::Dir {
            if path.len() > DIR_PATH {
                return Err(ENAMETOOLONG);
            }
            Some(self.dirs.iter().position(|d| !d.used).ok_or(EMFILE)?)
        } else {
            None
        };
        let ofd = Ofd {
            obj,
            kind: k,
            flags: flags & (O_ACCMODE | O_APPEND | O_NONBLOCK),
            offset: 0,
        };
        let fd = self.fds.insert(ofd, flags & O_CLOEXEC != 0, 0)?;
        if let Some(i) = slot {
            let d = &mut self.dirs[i];
            d.obj = obj;
            d.used = true;
            d.len = path.len() as u16;
            d.path[..path.len()].copy_from_slice(path);
        }
        Ok(fd)
    }

    /// The description for a transfer, with the access mode checked.
    fn transfer_target(&self, fd: usize, write: bool) -> Result<Ofd, Errno> {
        let o = *self.fds.get(fd)?;
        let acc = o.flags & O_ACCMODE;
        if (write && acc == 0) || (!write && acc == O_WRONLY) {
            return Err(EBADF);
        }
        if o.kind == Kind::Dir {
            return Err(EISDIR);
        }
        Ok(o)
    }

    /// Reads up to `count` bytes into guest memory at `addr`: from the current
    /// position (`at == None`, which then advances) or from `at` (`pread`).
    /// Regular files are read until the request is met or the file ends; a
    /// pipe or terminal gives what one backend call has.
    pub(crate) fn read_to_guest(
        &mut self,
        mem: &mut dyn GuestMem,
        fd: usize,
        addr: u64,
        count: u64,
        at: Option<u64>,
    ) -> Result<u64, Errno> {
        let o = self.transfer_target(fd, false)?;
        let count = count.min(MAX_RW);
        check_range(addr, count)?;
        let seekable = o.kind == Kind::File;
        if at.is_some() && !seekable {
            return Err(ESPIPE);
        }
        // `pread` at a negative offset is invalid.
        if at.is_some_and(|a| a > i64::MAX as u64) {
            return Err(EINVAL);
        }
        let mut off = at.unwrap_or(o.offset);
        let mut done = 0u64;
        let mut buf = [0u8; CHUNK];
        while done < count {
            let want = (count - done).min(CHUNK as u64) as usize;
            let n = match self.backend.read(o.obj, off, &mut buf[..want]) {
                Ok(n) => n.min(want),
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            };
            if n == 0 {
                break;
            }
            if let Err(e) = mem.write(addr + done, &buf[..n]) {
                if done == 0 {
                    return Err(e);
                }
                break;
            }
            done += n as u64;
            off += n as u64;
            if n < want || !seekable {
                break;
            }
        }
        if at.is_none() && seekable {
            self.fds.get_mut(fd)?.offset = off;
        }
        Ok(done)
    }

    /// Writes `count` bytes from guest memory at `addr`, as `read_to_guest`
    /// reads. With `O_APPEND` the data always goes to the end of the file.
    pub(crate) fn write_from_guest(
        &mut self,
        mem: &dyn GuestMem,
        fd: usize,
        addr: u64,
        count: u64,
        at: Option<u64>,
    ) -> Result<u64, Errno> {
        let o = self.transfer_target(fd, true)?;
        let count = count.min(MAX_RW);
        check_range(addr, count)?;
        let seekable = o.kind == Kind::File;
        if at.is_some() && !seekable {
            return Err(ESPIPE);
        }
        if at.is_some_and(|a| a > i64::MAX as u64) {
            return Err(EINVAL);
        }
        let mut off = if seekable && o.flags & O_APPEND != 0 {
            self.backend.fstat(o.obj)?.size
        } else {
            at.unwrap_or(o.offset)
        };
        // A file cannot grow past what an offset can name.
        if seekable && off.checked_add(count).is_none_or(|e| e > i64::MAX as u64) {
            return Err(EFBIG);
        }
        let mut done = 0u64;
        let mut buf = [0u8; CHUNK];
        while done < count {
            let want = (count - done).min(CHUNK as u64) as usize;
            if let Err(e) = mem.read(addr + done, &mut buf[..want]) {
                if done == 0 {
                    return Err(e);
                }
                break;
            }
            let n = match self.backend.write(o.obj, off, &buf[..want]) {
                Ok(n) => n.min(want),
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            };
            done += n as u64;
            off += n as u64;
            if n < want {
                break;
            }
        }
        if at.is_none() && seekable {
            self.fds.get_mut(fd)?.offset = off;
        }
        Ok(done)
    }

    /// `readv`, `writev`: the iovec array is validated whole before any byte moves.
    pub(crate) fn sys_vectored(
        &mut self,
        mem: &mut dyn GuestMem,
        fd: usize,
        iov: u64,
        count: u64,
        write: bool,
    ) -> Result<u64, Errno> {
        if count > IOV_MAX as u64 {
            return Err(EINVAL);
        }
        let entry = |m: &dyn GuestMem, i: u64| -> Result<(u64, u64), Errno> {
            let at = iov.checked_add(16 * i).ok_or(EFAULT)?;
            Ok((read_u64(m, at)?, read_u64(m, at + 8)?))
        };
        let mut total = 0u64;
        for i in 0..count {
            let (_, len) = entry(mem, i)?;
            total = total
                .checked_add(len)
                .filter(|t| *t <= i64::MAX as u64)
                .ok_or(EINVAL)?;
        }
        let mut done = 0u64;
        for i in 0..count {
            let (base, len) = entry(mem, i)?;
            if len == 0 {
                continue;
            }
            let r = if write {
                self.write_from_guest(mem, fd, base, len, None)
            } else {
                self.read_to_guest(mem, fd, base, len, None)
            };
            match r {
                Ok(n) => {
                    done += n;
                    if n < len {
                        break;
                    }
                }
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            }
        }
        Ok(done)
    }

    pub(crate) fn sys_lseek(&mut self, fd: usize, off: i64, whence: u64) -> Result<u64, Errno> {
        let o = *self.fds.get(fd)?;
        let new = match o.kind {
            Kind::File => {
                let base = match whence {
                    0 => 0,
                    1 => o.offset as i64,
                    2 => self.backend.fstat(o.obj)?.size as i64,
                    _ => return Err(EINVAL),
                };
                base.checked_add(off).ok_or(EOVERFLOW)?
            }
            // For a directory the position is the index of the next entry.
            Kind::Dir => match whence {
                0 => off,
                1 => (o.offset as i64).checked_add(off).ok_or(EOVERFLOW)?,
                _ => return Err(EINVAL),
            },
            _ => return Err(ESPIPE),
        };
        if new < 0 {
            return Err(EINVAL);
        }
        self.fds.get_mut(fd)?.offset = new as u64;
        Ok(new as u64)
    }

    /// The status of what a path (or, with `AT_EMPTY_PATH`, a descriptor) names.
    fn status_of(
        &mut self,
        mem: &dyn GuestMem,
        dirfd: u64,
        ptr: u64,
        flags: u32,
    ) -> Result<Stat, Errno> {
        let mut p = PathArg::new();
        match self.locate(mem, dirfd, ptr, &mut p, flags & AT_EMPTY_PATH != 0)? {
            Located::Empty if dirfd as i32 == AT_FDCWD => {
                let cwd = self.cwd;
                self.backend.stat(&cwd[..self.cwd_len], true)
            }
            Located::Empty => {
                let o = *self.fds.get(fd_of(dirfd)?)?;
                self.backend.fstat(o.obj)
            }
            Located::Path => {
                let s = self
                    .backend
                    .stat(p.path(), flags & AT_SYMLINK_NOFOLLOW == 0)?;
                if p.dir_only && s.kind != FileKind::Dir {
                    return Err(ENOTDIR);
                }
                Ok(s)
            }
        }
    }

    /// `stat`, `lstat`, `newfstatat`: `flags` carries `AT_SYMLINK_NOFOLLOW` for `lstat`.
    pub(crate) fn sys_stat(
        &mut self,
        mem: &mut dyn GuestMem,
        dirfd: u64,
        ptr: u64,
        buf: u64,
        flags: u32,
    ) -> Result<u64, Errno> {
        let s = self.status_of(mem, dirfd, ptr, flags)?;
        mem.write(buf, &encode_stat(&s, self.cfg.uid, self.cfg.gid))?;
        Ok(0)
    }

    pub(crate) fn sys_fstat(
        &mut self,
        mem: &mut dyn GuestMem,
        fd: usize,
        buf: u64,
    ) -> Result<u64, Errno> {
        let o = *self.fds.get(fd)?;
        let s = self.backend.fstat(o.obj)?;
        mem.write(buf, &encode_stat(&s, self.cfg.uid, self.cfg.gid))?;
        Ok(0)
    }

    pub(crate) fn sys_statx(&mut self, mem: &mut dyn GuestMem, a: [u64; 6]) -> Result<u64, Errno> {
        let flags = a[2] as u32;
        let s = self.status_of(mem, a[0], a[1], flags)?;
        mem.write(a[4], &encode_statx(&s, self.cfg.uid, self.cfg.gid))?;
        Ok(0)
    }

    /// `access`, `faccessat`: existence and the owner permission bits.
    pub(crate) fn sys_access(
        &mut self,
        mem: &dyn GuestMem,
        dirfd: u64,
        ptr: u64,
        mode: u64,
        flags: u32,
    ) -> Result<u64, Errno> {
        let mode = mode as u32;
        if mode & !7 != 0 {
            return Err(EINVAL);
        }
        let s = self.status_of(mem, dirfd, ptr, flags)?;
        if mode & !((s.perm >> 6) & 7) != 0 {
            return Err(EACCES);
        }
        Ok(0)
    }

    pub(crate) fn sys_getcwd(
        &mut self,
        mem: &mut dyn GuestMem,
        buf: u64,
        size: u64,
    ) -> Result<u64, Errno> {
        let n = self.cwd_len as u64 + 1;
        if size < n {
            return Err(ERANGE);
        }
        let mut tmp = [0u8; PATH_MAX + 1];
        tmp[..self.cwd_len].copy_from_slice(&self.cwd[..self.cwd_len]);
        mem.write(buf, &tmp[..n as usize])?;
        Ok(n)
    }

    pub(crate) fn sys_chdir(&mut self, mem: &dyn GuestMem, ptr: u64) -> Result<u64, Errno> {
        let mut p = PathArg::new();
        self.locate(mem, AT_FDCWD as u64, ptr, &mut p, false)?;
        if self.backend.stat(p.path(), true)?.kind != FileKind::Dir {
            return Err(ENOTDIR);
        }
        self.cwd[..p.len].copy_from_slice(p.path());
        self.cwd_len = p.len;
        Ok(0)
    }

    pub(crate) fn sys_fchdir(&mut self, fd: usize) -> Result<u64, Errno> {
        let path = self.dir_path(fd)?;
        let (n, mut tmp) = (path.len(), [0u8; DIR_PATH]);
        tmp[..n].copy_from_slice(path);
        self.cwd[..n].copy_from_slice(&tmp[..n]);
        self.cwd_len = n;
        Ok(0)
    }

    pub(crate) fn sys_mkdir(
        &mut self,
        mem: &dyn GuestMem,
        dirfd: u64,
        ptr: u64,
        mode: u64,
    ) -> Result<u64, Errno> {
        let mut p = PathArg::new();
        self.locate(mem, dirfd, ptr, &mut p, false)?;
        if p.is_root() {
            return Err(EEXIST);
        }
        let mode = (mode as u32) & !self.umask & 0o7777;
        self.backend.mkdir(p.path(), mode)?;
        Ok(0)
    }

    /// `unlink`, `unlinkat` (`AT_REMOVEDIR` makes it `rmdir`), `rmdir`.
    pub(crate) fn sys_unlink(
        &mut self,
        mem: &dyn GuestMem,
        dirfd: u64,
        ptr: u64,
        flags: u32,
    ) -> Result<u64, Errno> {
        if flags & !AT_REMOVEDIR != 0 {
            return Err(EINVAL);
        }
        let mut p = PathArg::new();
        self.locate(mem, dirfd, ptr, &mut p, false)?;
        if p.is_root() {
            return Err(EBUSY);
        }
        if flags & AT_REMOVEDIR != 0 {
            self.backend.rmdir(p.path())?;
        } else {
            // `unlink("name/")` never removes anything: a directory is `EISDIR`, a file `ENOTDIR`.
            if p.dir_only {
                let kind = self.backend.stat(p.path(), false)?.kind;
                return Err(if kind == FileKind::Dir {
                    EISDIR
                } else {
                    ENOTDIR
                });
            }
            self.backend.unlink(p.path())?;
        }
        Ok(0)
    }

    /// `rename`, `renameat`, `renameat2` (only `RENAME_NOREPLACE`).
    pub(crate) fn sys_rename(
        &mut self,
        mem: &dyn GuestMem,
        a: [u64; 5],
        flags: u32,
    ) -> Result<u64, Errno> {
        if flags & !1 != 0 {
            return Err(EINVAL);
        }
        let (mut from, mut to) = (PathArg::new(), PathArg::new());
        self.locate(mem, a[0], a[1], &mut from, false)?;
        self.locate(mem, a[2], a[3], &mut to, false)?;
        if from.is_root() || to.is_root() {
            return Err(EBUSY);
        }
        if flags == 1 && self.backend.stat(to.path(), false).is_ok() {
            return Err(EEXIST);
        }
        self.backend.rename(from.path(), to.path())?;
        Ok(0)
    }

    /// `link`, `linkat`: a hard link; `AT_SYMLINK_FOLLOW` is not supported.
    pub(crate) fn sys_link(
        &mut self,
        mem: &dyn GuestMem,
        a: [u64; 5],
        flags: u32,
    ) -> Result<u64, Errno> {
        if flags != 0 {
            return Err(EINVAL);
        }
        let (mut from, mut to) = (PathArg::new(), PathArg::new());
        self.locate(mem, a[0], a[1], &mut from, false)?;
        self.locate(mem, a[2], a[3], &mut to, false)?;
        if from.is_root() || to.is_root() {
            return Err(EPERM);
        }
        self.backend.link(from.path(), to.path())?;
        Ok(0)
    }

    /// `symlink`, `symlinkat`: the target string is stored as written (the
    /// backend resolves links inside its own root).
    pub(crate) fn sys_symlink(
        &mut self,
        mem: &dyn GuestMem,
        target: u64,
        dirfd: u64,
        ptr: u64,
    ) -> Result<u64, Errno> {
        let mut t = [0u8; PATH_MAX];
        let n = read_cstr(mem, target, &mut t)?;
        if n == 0 {
            return Err(ENOENT);
        }
        let mut p = PathArg::new();
        self.locate(mem, dirfd, ptr, &mut p, false)?;
        if p.is_root() {
            return Err(EEXIST);
        }
        self.backend.symlink(&t[..n], p.path())?;
        Ok(0)
    }

    pub(crate) fn sys_readlink(
        &mut self,
        mem: &mut dyn GuestMem,
        dirfd: u64,
        ptr: u64,
        buf: u64,
        size: u64,
    ) -> Result<u64, Errno> {
        if size == 0 || size > i64::MAX as u64 {
            return Err(EINVAL);
        }
        let mut p = PathArg::new();
        self.locate(mem, dirfd, ptr, &mut p, false)?;
        let mut tmp = [0u8; PATH_MAX];
        let cap = (size as usize).min(PATH_MAX);
        let n = self.backend.readlink(p.path(), &mut tmp[..cap])?.min(cap);
        mem.write(buf, &tmp[..n])?;
        Ok(n as u64)
    }

    /// `getdents64`: the descriptor position is the index of the next entry.
    pub(crate) fn sys_getdents(
        &mut self,
        mem: &mut dyn GuestMem,
        fd: usize,
        addr: u64,
        count: u64,
    ) -> Result<u64, Errno> {
        let o = *self.fds.get(fd)?;
        if o.kind != Kind::Dir {
            return Err(ENOTDIR);
        }
        let count = count.min(MAX_RW);
        check_range(addr, count)?;
        let (mut index, mut done) = (o.offset, 0u64);
        let (mut name, mut rec) = ([0u8; 256], [0u8; 280]);
        while done < count {
            let ent = match self.backend.dirent(o.obj, index, &mut name) {
                Ok(e) => e,
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            };
            let Some((ino, kind, nlen)) = ent else { break };
            let nlen = nlen.min(255);
            let Some(len) = encode_dirent(
                &mut rec,
                ino,
                (index + 1) as i64,
                kind.dtype(),
                &name[..nlen],
            ) else {
                break;
            };
            if done + len as u64 > count {
                if done == 0 {
                    return Err(EINVAL);
                }
                break;
            }
            if let Err(e) = mem.write(addr + done, &rec[..len]) {
                if done == 0 {
                    return Err(e);
                }
                break;
            }
            done += len as u64;
            index += 1;
        }
        self.fds.get_mut(fd)?.offset = index;
        Ok(done)
    }

    /// `utimensat`: the timestamps are accepted but not changed (the backend
    /// has no way to set them); the path must exist.
    pub(crate) fn sys_utimensat(&mut self, mem: &dyn GuestMem, a: [u64; 6]) -> Result<u64, Errno> {
        if a[1] == 0 {
            self.fds.get(fd_of(a[0])?)?;
        } else {
            self.status_of(mem, a[0], a[1], a[3] as u32)?;
        }
        if a[2] != 0 {
            let mut t = [0u8; 32];
            mem.read(a[2], &mut t)?;
        }
        Ok(0)
    }

    /// `statfs`, `fstatfs`: a fixed, plausible answer (4 KiB blocks, 4 GiB).
    pub(crate) fn sys_statfs(
        &mut self,
        mem: &mut dyn GuestMem,
        by_fd: bool,
        arg: u64,
        buf: u64,
    ) -> Result<u64, Errno> {
        if by_fd {
            self.fds.get(fd_of(arg)?)?;
        } else {
            self.status_of(mem, AT_FDCWD as u64, arg, 0)?;
        }
        let mut b = [0u8; 120];
        let put =
            |b: &mut [u8; 120], at: usize, v: u64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
        put(&mut b, 0, 0x8584_58f6); // RAMFS_MAGIC
        put(&mut b, 8, 4096);
        put(&mut b, 16, 1 << 20);
        put(&mut b, 24, 1 << 19);
        put(&mut b, 32, 1 << 19);
        put(&mut b, 40, 1 << 16);
        put(&mut b, 48, 1 << 15);
        put(&mut b, 72, 255);
        put(&mut b, 80, 4096);
        mem.write(buf, &b)?;
        Ok(0)
    }

    /// The `i32` pair `pipe` writes back (kept here so `write_u32` stays the one writer of such words).
    pub(crate) fn put_fd_pair(
        mem: &mut dyn GuestMem,
        at: u64,
        a: usize,
        b: usize,
    ) -> Result<(), Errno> {
        write_u32(mem, at, a as u32)?;
        write_u32(mem, at + 4, b as u32)
    }
}
