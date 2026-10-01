//! Calls on descriptors rather than paths: duplication, `fcntl`, pipes and
//! socket pairs, `poll`, and the ones that only need the descriptor to exist.

use crate::abi::*;
use crate::backend::Backend;
use crate::errno::*;
use crate::fdtable::{Kind, Obj, Ofd};
use crate::handlers::fd_of;
use crate::mem::{write_u32, GuestMem};
use crate::personality::{Personality, MAX_FDS};

const AF_UNIX: u64 = 1;
const SOCK_STREAM: u64 = 1;
const MSG_DONTWAIT: u64 = 0x40;
/// Most descriptors one `poll` may name.
const POLL_MAX: u64 = 1024;
/// How long `poll` sleeps between looks while nothing is ready.
const POLL_SLICE_NS: u64 = 1_000_000;

impl<B: Backend> Personality<B> {
    pub(crate) fn sys_dup(&mut self, fd: usize) -> Result<u64, Errno> {
        Ok(self.fds.dup(fd, 0, false)? as u64)
    }

    /// `dup2` (`flags == None`) and `dup3`.
    pub(crate) fn sys_dup_to(
        &mut self,
        old: u64,
        new: u64,
        flags: Option<u64>,
    ) -> Result<u64, Errno> {
        let cloexec = match flags {
            None => false,
            Some(f) if f & !(O_CLOEXEC as u64) == 0 => f != 0,
            Some(_) => return Err(EINVAL),
        };
        let (old, new) = (fd_of(old)?, fd_of(new)?);
        self.fds.get(old)?;
        if new >= MAX_FDS {
            return Err(EBADF);
        }
        if old == new && flags.is_some() {
            return Err(EINVAL);
        }
        if let Some(obj) = self.fds.dup_to(old, new, cloexec)? {
            self.release(obj);
        }
        Ok(new as u64)
    }

    pub(crate) fn sys_fcntl(&mut self, fd: usize, cmd: u64, arg: u64) -> Result<u64, Errno> {
        match cmd {
            F_DUPFD | F_DUPFD_CLOEXEC => {
                let min = usize::try_from(arg)
                    .ok()
                    .filter(|m| *m < MAX_FDS)
                    .ok_or(EINVAL)?;
                Ok(self.fds.dup(fd, min, cmd == F_DUPFD_CLOEXEC)? as u64)
            }
            F_GETFD => Ok(if self.fds.cloexec(fd)? { FD_CLOEXEC } else { 0 }),
            F_SETFD => {
                self.fds.set_cloexec(fd, arg & FD_CLOEXEC != 0)?;
                Ok(0)
            }
            F_GETFL => Ok(u64::from(self.fds.get(fd)?.flags)),
            F_SETFL => {
                const CHANGEABLE: u32 = O_APPEND | O_NONBLOCK;
                let o = self.fds.get_mut(fd)?;
                o.flags = (o.flags & !CHANGEABLE) | (arg as u32 & CHANGEABLE);
                Ok(0)
            }
            _ => {
                self.fds.get(fd)?;
                Err(EINVAL)
            }
        }
    }

    /// Every descriptor of a program that is not a terminal answers "not a terminal".
    pub(crate) fn sys_ioctl(&mut self, fd: usize) -> Result<u64, Errno> {
        self.fds.get(fd)?;
        Err(ENOTTY)
    }

    /// Descriptors that exist and need nothing done: `fsync`, `fdatasync`, `flock`.
    pub(crate) fn sys_fd_noop(&mut self, fd: usize) -> Result<u64, Errno> {
        self.fds.get(fd)?;
        Ok(0)
    }

    pub(crate) fn sys_ftruncate(&mut self, fd: usize, len: u64) -> Result<u64, Errno> {
        let o = *self.fds.get(fd)?;
        if o.kind != Kind::File {
            return Err(EINVAL);
        }
        if o.flags & O_ACCMODE == 0 {
            return Err(EBADF);
        }
        if len > i64::MAX as u64 {
            return Err(EINVAL);
        }
        self.backend.truncate(o.obj, len)?;
        Ok(0)
    }

    /// Puts two new objects in the table and writes their numbers to `ptr`.
    fn install_pair(
        &mut self,
        mem: &mut dyn GuestMem,
        ptr: u64,
        ends: [(Obj, Kind, u32); 2],
        flags: u32,
    ) -> Result<u64, Errno> {
        let cloexec = flags & O_CLOEXEC != 0;
        let mut fds = [0usize; 2];
        for (i, (obj, kind, access)) in ends.into_iter().enumerate() {
            let ofd = Ofd {
                obj,
                kind,
                flags: access | (flags & O_NONBLOCK),
                offset: 0,
            };
            match self.fds.insert(ofd, cloexec, 0) {
                Ok(fd) => fds[i] = fd,
                Err(e) => {
                    if i == 1 {
                        self.drop_fd(fds[0]);
                    } else {
                        self.release(obj);
                    }
                    self.release(ends[1].0);
                    return Err(e);
                }
            }
        }
        if let Err(e) = Self::put_fd_pair(mem, ptr, fds[0], fds[1]) {
            self.drop_fd(fds[0]);
            self.drop_fd(fds[1]);
            return Err(e);
        }
        Ok(0)
    }

    pub(crate) fn sys_pipe(
        &mut self,
        mem: &mut dyn GuestMem,
        ptr: u64,
        flags: u64,
    ) -> Result<u64, Errno> {
        let flags = flags as u32;
        if flags & !(O_CLOEXEC | O_NONBLOCK) != 0 {
            return Err(EINVAL);
        }
        let (r, w) = self.backend.pipe(false)?;
        self.install_pair(
            mem,
            ptr,
            [(r, Kind::PipeRead, 0), (w, Kind::PipeWrite, O_WRONLY)],
            flags,
        )
    }

    pub(crate) fn sys_socketpair(
        &mut self,
        mem: &mut dyn GuestMem,
        a: [u64; 4],
    ) -> Result<u64, Errno> {
        if a[0] != AF_UNIX {
            return Err(EAFNOSUPPORT);
        }
        let flags = (a[1] as u32) & (O_CLOEXEC | O_NONBLOCK);
        if a[1] & !(0xf | u64::from(O_CLOEXEC | O_NONBLOCK)) != 0 {
            return Err(EINVAL);
        }
        if a[1] & 0xf != SOCK_STREAM || a[2] != 0 {
            return Err(EOPNOTSUPP);
        }
        let (x, y) = self.backend.pipe(true)?;
        self.install_pair(
            mem,
            a[3],
            [(x, Kind::Socket, O_RDWR), (y, Kind::Socket, O_RDWR)],
            flags,
        )
    }

    fn socket_fd(&self, fd: usize) -> Result<Ofd, Errno> {
        let o = *self.fds.get(fd)?;
        if o.kind == Kind::Socket {
            Ok(o)
        } else {
            Err(ENOTSOCK)
        }
    }

    /// `recvfrom` on a connected stream socket: the peer address is not reported (length 0).
    pub(crate) fn sys_recvfrom(
        &mut self,
        mem: &mut dyn GuestMem,
        fd: usize,
        a: [u64; 6],
    ) -> Result<u64, Errno> {
        let o = self.socket_fd(fd)?;
        if a[3] & !MSG_DONTWAIT != 0 {
            return Err(EINVAL);
        }
        if a[3] & MSG_DONTWAIT != 0 && self.backend.poll(o.obj, POLLIN) & POLLIN == 0 {
            return Err(EAGAIN);
        }
        let n = self.read_to_guest(mem, fd, a[1], a[2], None)?;
        if a[4] != 0 && a[5] != 0 {
            write_u32(mem, a[5], 0)?;
        }
        Ok(n)
    }

    /// `sendto` on a connected stream socket (an address is refused, as Linux does).
    pub(crate) fn sys_sendto(
        &mut self,
        mem: &mut dyn GuestMem,
        fd: usize,
        a: [u64; 6],
    ) -> Result<u64, Errno> {
        let o = self.socket_fd(fd)?;
        if a[3] & !MSG_DONTWAIT != 0 {
            return Err(EINVAL);
        }
        if a[4] != 0 {
            return Err(EISCONN);
        }
        if a[3] & MSG_DONTWAIT != 0 && self.backend.poll(o.obj, POLLOUT) & POLLOUT == 0 {
            return Err(EAGAIN);
        }
        self.write_from_guest(mem, fd, a[1], a[2], None)
    }

    /// Fills in `revents` of every entry; how many are ready.
    fn poll_scan(&mut self, mem: &mut dyn GuestMem, ptr: u64, n: u64) -> Result<u64, Errno> {
        let mut ready = 0;
        for i in 0..n {
            let at = ptr.checked_add(8 * i).ok_or(EFAULT)?;
            let mut b = [0u8; 8];
            mem.read(at, &mut b)?;
            let fd = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            let want = u16::from_le_bytes([b[4], b[5]]) & (POLLIN | POLLOUT);
            let revents = match usize::try_from(fd) {
                Err(_) => 0,
                Ok(fd) => match self.fds.get(fd) {
                    Err(_) => POLLNVAL,
                    Ok(o) if matches!(o.kind, Kind::File | Kind::Dir | Kind::Null) => want,
                    Ok(o) => {
                        let obj = o.obj;
                        self.backend.poll(obj, want)
                    }
                },
            };
            mem.write(at + 6, &revents.to_le_bytes())?;
            if revents != 0 {
                ready += 1;
            }
        }
        Ok(ready)
    }

    /// `poll`: with nothing ready it sleeps in 1 ms slices until something is
    /// or the timeout (milliseconds, negative: none) is over.
    pub(crate) fn sys_poll(
        &mut self,
        mem: &mut dyn GuestMem,
        ptr: u64,
        n: u64,
        timeout: i32,
    ) -> Result<u64, Errno> {
        if n > POLL_MAX {
            return Err(EINVAL);
        }
        if n == 0 && timeout > 0 {
            self.backend.sleep(timeout as u64 * 1_000_000)?;
            return Ok(0);
        }
        let limit = (timeout >= 0).then(|| timeout as u64 * 1_000_000);
        let mut waited = 0u64;
        loop {
            let ready = self.poll_scan(mem, ptr, n)?;
            if ready > 0 || limit.is_some_and(|l| waited >= l) {
                return Ok(ready);
            }
            let slice = limit.map_or(POLL_SLICE_NS, |l| POLL_SLICE_NS.min(l - waited));
            self.backend.sleep(slice)?;
            waited += slice;
        }
    }
}
