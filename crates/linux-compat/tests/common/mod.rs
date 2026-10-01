//! Test support for the personality: guest memory made of pages with
//! protections, and an in-memory backend (a small file system with
//! directories, symlinks and hard links; pipes and socket pairs; a terminal;
//! a virtual clock; futexes) that checks, on every call, the promises the
//! personality makes to a backend: paths arrive absolute and normalized,
//! objects are never closed twice or used after closing.
#![allow(dead_code)]

use linux_compat::abi::*;
use linux_compat::backend::Backend;
use linux_compat::errno::*;
use linux_compat::fdtable::Obj;
use linux_compat::mem::GuestMem;
use linux_compat::path::{resolve, split_last};
use linux_compat::vma::{Backing, BrkPlan, MmapPlan};
use linux_compat::{Config, Outcome, Personality};
use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

const PG: u64 = 4096;
/// The largest file the in-memory backend keeps.
const MAX_FILE: usize = 1 << 24;

struct Page {
    data: Box<[u8; 4096]>,
    prot: u8,
}

#[derive(Default)]
struct Inner {
    pages: BTreeMap<u64, Page>,
}

/// Guest memory: mapped pages with read/write protections. Shared between
/// the test (which scripts the program) and the backend (which maps).
#[derive(Clone, Default)]
pub struct Mem(Rc<RefCell<Inner>>);

impl Mem {
    pub fn map(&self, addr: u64, len: u64, prot: u8) {
        let mut m = self.0.borrow_mut();
        let mut a = addr & !(PG - 1);
        while a < addr + len {
            m.pages.insert(
                a,
                Page {
                    data: Box::new([0; 4096]),
                    prot,
                },
            );
            a += PG;
        }
    }

    pub fn unmap(&self, a: u64, b: u64) {
        self.0
            .borrow_mut()
            .pages
            .retain(|p, _| *p + PG <= a || *p >= b);
    }

    pub fn protect(&self, a: u64, b: u64, prot: u8) {
        for (p, pg) in self.0.borrow_mut().pages.iter_mut() {
            if *p + PG > a && *p < b {
                pg.prot = prot;
            }
        }
    }

    pub fn zero(&self, a: u64, b: u64) {
        for (p, pg) in self.0.borrow_mut().pages.iter_mut() {
            if *p + PG > a && *p < b {
                pg.data.fill(0);
            }
        }
    }

    pub fn prot_at(&self, addr: u64) -> Option<u8> {
        self.0
            .borrow()
            .pages
            .get(&(addr & !(PG - 1)))
            .map(|p| p.prot)
    }

    pub fn pages(&self) -> usize {
        self.0.borrow().pages.len()
    }

    /// Reads ignoring protection (the test looking at memory, not the program).
    pub fn peek(&self, addr: u64, n: usize) -> Vec<u8> {
        let m = self.0.borrow();
        (0..n as u64)
            .map(|i| m.pages[&((addr + i) & !(PG - 1))].data[((addr + i) % PG) as usize])
            .collect()
    }

    pub fn poke(&self, addr: u64, data: &[u8]) {
        let mut m = self.0.borrow_mut();
        for (i, b) in data.iter().enumerate() {
            let at = addr + i as u64;
            m.pages.get_mut(&(at & !(PG - 1))).unwrap().data[(at % PG) as usize] = *b;
        }
    }

    fn check(&self, addr: u64, len: usize, need: u8) -> Result<(), Errno> {
        let m = self.0.borrow();
        let mut done = 0u64;
        while done < len as u64 {
            let at = addr.checked_add(done).ok_or(EFAULT)?;
            match m.pages.get(&(at & !(PG - 1))) {
                Some(p) if p.prot & need == need => done += PG - at % PG,
                _ => return Err(EFAULT),
            }
        }
        Ok(())
    }
}

impl GuestMem for Mem {
    fn read(&self, addr: u64, out: &mut [u8]) -> Result<(), Errno> {
        self.check(addr, out.len(), 1)?;
        out.copy_from_slice(&self.peek(addr, out.len()));
        Ok(())
    }

    fn write(&mut self, addr: u64, data: &[u8]) -> Result<(), Errno> {
        self.check(addr, data.len(), 2)?;
        self.poke(addr, data);
        Ok(())
    }
}

#[derive(Clone)]
pub struct Inode {
    pub kind: FileKind,
    pub perm: u32,
    pub data: Vec<u8>,
    pub target: Vec<u8>,
}

enum H {
    File(u64),
    Dir(Vec<u8>),
    Tty,
    PipeR(usize),
    PipeW(usize),
    Sock { tx: usize, rx: usize },
}

#[derive(Default)]
struct Pipe {
    buf: VecDeque<u8>,
    readers: u32,
    writers: u32,
}

pub struct Mock {
    pub mem: Mem,
    names: BTreeMap<Vec<u8>, u64>,
    inodes: BTreeMap<u64, Inode>,
    next_ino: u64,
    handles: BTreeMap<Obj, H>,
    next_obj: Obj,
    pipes: Vec<Pipe>,
    pub tty_out: Vec<u8>,
    pub tty_in: VecDeque<u8>,
    pub now_ns: u64,
    pub fs_base: u64,
    pub fail_map: bool,
    pub fail_heap: bool,
    pub futex_waiters: u32,
    pub futex_calls: Vec<(u64, u32, Option<u64>)>,
    pub sleeps: Vec<u64>,
    rng: u64,
    /// Every path the personality handed over (for the confinement checks).
    pub paths_seen: Vec<Vec<u8>>,
}

impl Mock {
    pub fn new(mem: Mem) -> Self {
        let mut m = Mock {
            mem,
            names: BTreeMap::new(),
            inodes: BTreeMap::new(),
            next_ino: 2,
            handles: BTreeMap::new(),
            next_obj: 100,
            pipes: Vec::new(),
            tty_out: Vec::new(),
            tty_in: VecDeque::new(),
            now_ns: 5_000_000_000,
            fs_base: 0,
            fail_map: false,
            fail_heap: false,
            futex_waiters: 0,
            futex_calls: Vec::new(),
            sleeps: Vec::new(),
            rng: 0x9e37_79b9_7f4a_7c15,
            paths_seen: Vec::new(),
        };
        m.add(b"/", FileKind::Dir, 0o755, b"");
        for d in ["/tmp", "/etc", "/bin", "/dev", "/home"] {
            m.add(d.as_bytes(), FileKind::Dir, 0o755, b"");
        }
        m.add(
            b"/etc/passwd",
            FileKind::File,
            0o644,
            b"root:x:0:0:root:/root:/bin/sh\n",
        );
        m.add(b"/bin/true", FileKind::File, 0o755, b"\x7fELF");
        m.add(b"/bin/data", FileKind::File, 0o644, b"not executable");
        m
    }

    /// A node for tests to start from.
    pub fn add(&mut self, path: &[u8], kind: FileKind, perm: u32, data: &[u8]) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(
            ino,
            Inode {
                kind,
                perm,
                data: data.to_vec(),
                target: Vec::new(),
            },
        );
        self.names.insert(path.to_vec(), ino);
        ino
    }

    pub fn file(&self, path: &str) -> Option<&Vec<u8>> {
        let ino = self.names.get(path.as_bytes())?;
        Some(&self.inodes[ino].data)
    }

    pub fn exists(&self, path: &str) -> bool {
        self.names.contains_key(path.as_bytes())
    }

    /// Objects the backend still has open.
    pub fn live(&self) -> usize {
        self.handles.len()
    }

    fn check_path(&mut self, p: &[u8]) {
        assert!(
            p.first() == Some(&b'/'),
            "path not absolute: {:?}",
            String::from_utf8_lossy(p)
        );
        assert!(
            p == b"/" || p.last() != Some(&b'/'),
            "trailing slash: {:?}",
            String::from_utf8_lossy(p)
        );
        let s = String::from_utf8_lossy(p).into_owned();
        assert!(
            !s.contains("//") && !s.split('/').any(|c| c == "." || c == ".."),
            "not normalized: {s}"
        );
        self.paths_seen.push(p.to_vec());
    }

    fn new_handle(&mut self, h: H) -> Obj {
        let o = self.next_obj;
        self.next_obj += 1;
        self.handles.insert(o, h);
        o
    }

    fn handle(&self, o: Obj) -> &H {
        self.handles
            .get(&o)
            .unwrap_or_else(|| panic!("object {o} used after close or never opened"))
    }

    fn parent_ok(&self, path: &[u8]) -> Result<(), Errno> {
        let Some((parent, _)) = split_last(path) else {
            return Err(EEXIST);
        };
        match self.names.get(parent) {
            None => Err(ENOENT),
            Some(i) if self.inodes[i].kind == FileKind::Dir => Ok(()),
            Some(_) => Err(ENOTDIR),
        }
    }

    fn link_dest(&self, at: &[u8], target: &[u8]) -> Result<Vec<u8>, Errno> {
        let (parent, _) = split_last(at).ok_or(ELOOP)?;
        let mut out = [0u8; 4096];
        let r = resolve(parent, target, &mut out)?;
        Ok(out[..r.len].to_vec())
    }

    fn lookup(&self, path: &[u8], follow: bool) -> Result<u64, Errno> {
        let mut cur = path.to_vec();
        for _ in 0..8 {
            self.parent_ok(&cur)
                .or_else(|e| if e == EEXIST { Ok(()) } else { Err(e) })?;
            let ino = *self.names.get(&cur).ok_or(ENOENT)?;
            let n = &self.inodes[&ino];
            if n.kind == FileKind::Symlink && follow {
                cur = self.link_dest(&cur, &n.target)?;
                continue;
            }
            return Ok(ino);
        }
        Err(ELOOP)
    }

    fn create(&mut self, path: &[u8], kind: FileKind, perm: u32) -> Result<u64, Errno> {
        self.parent_ok(path)?;
        Ok(self.add(path, kind, perm, b""))
    }

    fn stat_of(&self, ino: u64) -> Stat {
        let n = &self.inodes[&ino];
        let links = self.names.values().filter(|i| **i == ino).count() as u32;
        Stat {
            kind: n.kind,
            perm: n.perm,
            size: if n.kind == FileKind::Symlink {
                n.target.len() as u64
            } else {
                n.data.len() as u64
            },
            ino,
            nlink: links,
            mtime_sec: 1_700_000_000,
            mtime_nsec: 0,
        }
    }

    fn children(&self, dir: &[u8]) -> Vec<(Vec<u8>, u64)> {
        self.names
            .iter()
            .filter(|(p, _)| p.as_slice() != b"/" && split_last(p).map(|(par, _)| par) == Some(dir))
            .map(|(p, i)| (split_last(p).unwrap().1.to_vec(), *i))
            .collect()
    }

    /// An inode with no name and no open handle is gone (an unlinked file stays while it is open).
    fn gc(&mut self, ino: u64) {
        let named = self.names.values().any(|i| *i == ino);
        let open = self
            .handles
            .values()
            .any(|h| matches!(h, H::File(i) if *i == ino));
        if !named && !open {
            self.inodes.remove(&ino);
        }
    }

    fn rand(&mut self) -> u8 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 24) as u8
    }
}

impl Backend for Mock {
    fn open(&mut self, path: &[u8], flags: u32, _mode: u32) -> Result<(Obj, FileKind), Errno> {
        self.check_path(path);
        if path == b"/dev/tty" {
            return Ok((self.new_handle(H::Tty), FileKind::Char));
        }
        let ino = match self.lookup(path, flags & O_NOFOLLOW == 0) {
            Ok(_) if flags & O_CREAT != 0 && flags & O_EXCL != 0 => return Err(EEXIST),
            Ok(i) => i,
            Err(e) if e == ENOENT && flags & O_CREAT != 0 => {
                self.create(path, FileKind::File, _mode)?
            }
            Err(e) => return Err(e),
        };
        let kind = self.inodes[&ino].kind;
        if kind == FileKind::File && flags & O_TRUNC != 0 && flags & O_ACCMODE != 0 {
            self.inodes.get_mut(&ino).unwrap().data.clear();
        }
        let h = if kind == FileKind::Dir {
            H::Dir(path.to_vec())
        } else {
            H::File(ino)
        };
        Ok((self.new_handle(h), kind))
    }

    fn stat(&mut self, path: &[u8], follow: bool) -> Result<Stat, Errno> {
        self.check_path(path);
        Ok(self.stat_of(self.lookup(path, follow)?))
    }

    fn fstat(&mut self, obj: Obj) -> Result<Stat, Errno> {
        match self.handle(obj) {
            H::File(i) => Ok(self.stat_of(*i)),
            H::Dir(p) => self.names.get(p).map(|i| self.stat_of(*i)).ok_or(ENOENT),
            H::Tty => Ok(Stat {
                kind: FileKind::Char,
                perm: 0o620,
                size: 0,
                ino: 1,
                nlink: 1,
                mtime_sec: 0,
                mtime_nsec: 0,
            }),
            _ => Ok(Stat {
                kind: FileKind::Fifo,
                perm: 0o600,
                size: 0,
                ino: 3,
                nlink: 1,
                mtime_sec: 0,
                mtime_nsec: 0,
            }),
        }
    }

    fn read(&mut self, obj: Obj, off: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        match self.handle(obj) {
            H::File(i) => {
                let d = &self.inodes[i].data;
                let off = (off as usize).min(d.len());
                let n = buf.len().min(d.len() - off);
                buf[..n].copy_from_slice(&d[off..off + n]);
                Ok(n)
            }
            H::Dir(_) => Err(EISDIR),
            H::Tty => {
                let n = buf.len().min(self.tty_in.len());
                for b in buf.iter_mut().take(n) {
                    *b = self.tty_in.pop_front().unwrap();
                }
                Ok(n)
            }
            H::PipeR(p) | H::Sock { rx: p, .. } => {
                let p = *p;
                let pipe = &mut self.pipes[p];
                if pipe.buf.is_empty() {
                    return if pipe.writers == 0 {
                        Ok(0)
                    } else {
                        Err(EAGAIN)
                    };
                }
                let n = buf.len().min(pipe.buf.len());
                for b in buf.iter_mut().take(n) {
                    *b = pipe.buf.pop_front().unwrap();
                }
                Ok(n)
            }
            H::PipeW(_) => Err(EBADF),
        }
    }

    fn write(&mut self, obj: Obj, off: u64, data: &[u8]) -> Result<usize, Errno> {
        match self.handle(obj) {
            H::File(i) => {
                let i = *i;
                let end = off as usize + data.len();
                if end > MAX_FILE {
                    return Err(EFBIG);
                }
                let d = &mut self.inodes.get_mut(&i).unwrap().data;
                if d.len() < end {
                    d.resize(end, 0);
                }
                d[off as usize..end].copy_from_slice(data);
                Ok(data.len())
            }
            H::Dir(_) => Err(EISDIR),
            H::Tty => {
                self.tty_out.extend_from_slice(data);
                Ok(data.len())
            }
            H::PipeW(p) | H::Sock { tx: p, .. } => {
                let p = *p;
                if self.pipes[p].readers == 0 {
                    return Err(EPIPE);
                }
                self.pipes[p].buf.extend(data);
                Ok(data.len())
            }
            H::PipeR(_) => Err(EBADF),
        }
    }

    fn close(&mut self, obj: Obj) {
        let h = self
            .handles
            .remove(&obj)
            .unwrap_or_else(|| panic!("object {obj} closed twice or never opened"));
        if let H::File(ino) = &h {
            self.gc(*ino);
        }
        match h {
            H::PipeR(p) => self.pipes[p].readers -= 1,
            H::PipeW(p) => self.pipes[p].writers -= 1,
            H::Sock { tx, rx } => {
                self.pipes[tx].writers -= 1;
                self.pipes[rx].readers -= 1;
            }
            _ => {}
        }
    }

    fn dirent(
        &mut self,
        obj: Obj,
        index: u64,
        name: &mut [u8; 256],
    ) -> Result<Option<(u64, FileKind, usize)>, Errno> {
        let H::Dir(path) = self.handle(obj) else {
            return Err(ENOTDIR);
        };
        let path = path.clone();
        let own = *self.names.get(&path).ok_or(ENOENT)?;
        let parent = split_last(&path)
            .and_then(|(p, _)| self.names.get(p).copied())
            .unwrap_or(own);
        let mut all = vec![(b".".to_vec(), own), (b"..".to_vec(), parent)];
        all.extend(self.children(&path));
        let Some((n, ino)) = all.get(index as usize) else {
            return Ok(None);
        };
        name[..n.len()].copy_from_slice(n);
        Ok(Some((*ino, self.inodes[ino].kind, n.len())))
    }

    fn mkdir(&mut self, path: &[u8], mode: u32) -> Result<(), Errno> {
        self.check_path(path);
        if self.names.contains_key(path) {
            return Err(EEXIST);
        }
        self.create(path, FileKind::Dir, mode)?;
        Ok(())
    }

    fn unlink(&mut self, path: &[u8]) -> Result<(), Errno> {
        self.check_path(path);
        let ino = self.lookup(path, false)?;
        if self.inodes[&ino].kind == FileKind::Dir {
            return Err(EISDIR);
        }
        self.names.remove(path);
        self.gc(ino);
        Ok(())
    }

    fn rmdir(&mut self, path: &[u8]) -> Result<(), Errno> {
        self.check_path(path);
        let ino = self.lookup(path, false)?;
        if self.inodes[&ino].kind != FileKind::Dir {
            return Err(ENOTDIR);
        }
        if !self.children(path).is_empty() {
            return Err(ENOTEMPTY);
        }
        self.names.remove(path);
        self.gc(ino);
        Ok(())
    }

    fn rename(&mut self, from: &[u8], to: &[u8]) -> Result<(), Errno> {
        self.check_path(from);
        self.check_path(to);
        let ino = self.lookup(from, false)?;
        self.parent_ok(to)?;
        if let Some(old) = self.names.get(to).copied() {
            if self.inodes[&old].kind == FileKind::Dir && !self.children(to).is_empty() {
                return Err(ENOTEMPTY);
            }
            self.names.remove(to);
        }
        let moved: Vec<Vec<u8>> = self
            .names
            .keys()
            .filter(|k| k.as_slice() == from || k.starts_with(&[from, b"/"].concat()))
            .cloned()
            .collect();
        for k in moved {
            let i = self.names.remove(&k).unwrap();
            let mut nk = to.to_vec();
            nk.extend_from_slice(&k[from.len()..]);
            self.names.insert(nk, i);
        }
        let _ = ino;
        Ok(())
    }

    fn readlink(&mut self, path: &[u8], buf: &mut [u8]) -> Result<usize, Errno> {
        self.check_path(path);
        let ino = self.lookup(path, false)?;
        let n = &self.inodes[&ino];
        if n.kind != FileKind::Symlink {
            return Err(EINVAL);
        }
        let k = buf.len().min(n.target.len());
        buf[..k].copy_from_slice(&n.target[..k]);
        Ok(k)
    }

    fn symlink(&mut self, target: &[u8], path: &[u8]) -> Result<(), Errno> {
        self.check_path(path);
        if self.names.contains_key(path) {
            return Err(EEXIST);
        }
        let ino = self.create(path, FileKind::Symlink, 0o777)?;
        self.inodes.get_mut(&ino).unwrap().target = target.to_vec();
        Ok(())
    }

    fn link(&mut self, from: &[u8], to: &[u8]) -> Result<(), Errno> {
        self.check_path(from);
        self.check_path(to);
        let ino = self.lookup(from, false)?;
        if self.inodes[&ino].kind == FileKind::Dir {
            return Err(EPERM);
        }
        if self.names.contains_key(to) {
            return Err(EEXIST);
        }
        self.parent_ok(to)?;
        self.names.insert(to.to_vec(), ino);
        Ok(())
    }

    fn truncate(&mut self, obj: Obj, len: u64) -> Result<(), Errno> {
        let H::File(i) = self.handle(obj) else {
            return Err(EINVAL);
        };
        let i = *i;
        if len > MAX_FILE as u64 {
            return Err(EFBIG);
        }
        self.inodes
            .get_mut(&i)
            .unwrap()
            .data
            .resize(len as usize, 0);
        Ok(())
    }

    fn pipe(&mut self, duplex: bool) -> Result<(Obj, Obj), Errno> {
        if duplex {
            let (x, y) = (self.pipes.len(), self.pipes.len() + 1);
            for _ in 0..2 {
                self.pipes.push(Pipe {
                    buf: VecDeque::new(),
                    readers: 1,
                    writers: 1,
                });
            }
            let a = self.new_handle(H::Sock { tx: x, rx: y });
            let b = self.new_handle(H::Sock { tx: y, rx: x });
            Ok((a, b))
        } else {
            let p = self.pipes.len();
            self.pipes.push(Pipe {
                buf: VecDeque::new(),
                readers: 1,
                writers: 1,
            });
            let r = self.new_handle(H::PipeR(p));
            let w = self.new_handle(H::PipeW(p));
            Ok((r, w))
        }
    }

    fn poll(&mut self, obj: Obj, events: u16) -> u16 {
        match self.handle(obj) {
            H::PipeR(p) | H::Sock { rx: p, .. } => {
                let pipe = &self.pipes[*p];
                let mut r = 0;
                if events & POLLIN != 0 && !pipe.buf.is_empty() {
                    r |= POLLIN;
                }
                if pipe.writers == 0 && pipe.buf.is_empty() {
                    r |= 0x10; // POLLHUP
                }
                if matches!(self.handle(obj), H::Sock { .. }) && events & POLLOUT != 0 {
                    r |= POLLOUT;
                }
                r
            }
            H::PipeW(_) => events & POLLOUT,
            H::Tty => {
                (if events & POLLIN != 0 && !self.tty_in.is_empty() {
                    POLLIN
                } else {
                    0
                }) | (events & POLLOUT)
            }
            _ => events,
        }
    }

    fn map(&mut self, plan: &MmapPlan) -> Result<(), Errno> {
        if self.fail_map {
            return Err(ENOMEM);
        }
        self.mem.unmap(plan.addr, plan.addr + plan.len);
        self.mem.map(plan.addr, plan.len, 3);
        if let Backing::File { obj, offset } = plan.backing {
            let H::File(i) = self.handle(obj) else {
                return Err(ENODEV);
            };
            let d = self.inodes[i].data.clone();
            let start = (offset as usize).min(d.len());
            let n = (plan.len as usize).min(d.len() - start);
            self.mem.poke(plan.addr, &d[start..start + n]);
        }
        self.mem.protect(plan.addr, plan.addr + plan.len, plan.prot);
        Ok(())
    }

    fn unmap(&mut self, start: u64, end: u64) -> Result<(), Errno> {
        self.mem.unmap(start, end);
        Ok(())
    }

    fn protect(&mut self, start: u64, end: u64, prot: u8) -> Result<(), Errno> {
        self.mem.protect(start, end, prot);
        Ok(())
    }

    fn discard(&mut self, start: u64, end: u64) -> Result<(), Errno> {
        self.mem.zero(start, end);
        Ok(())
    }

    fn resize_heap(&mut self, plan: &BrkPlan) -> Result<(), Errno> {
        if self.fail_heap {
            return Err(ENOMEM);
        }
        if plan.new_end > plan.old_end {
            self.mem.map(plan.old_end, plan.new_end - plan.old_end, 3);
        } else {
            self.mem.unmap(plan.new_end, plan.old_end);
        }
        Ok(())
    }

    fn clock_gettime(&mut self, clock: u32) -> Result<(i64, u32), Errno> {
        let ns = if clock == 0 {
            self.now_ns + 1_700_000_000 * 1_000_000_000
        } else {
            self.now_ns
        };
        Ok(((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as u32))
    }

    fn sleep(&mut self, nanos: u64) -> Result<(), Errno> {
        self.sleeps.push(nanos);
        self.now_ns += nanos;
        Ok(())
    }

    fn random(&mut self, buf: &mut [u8]) {
        for b in buf {
            *b = self.rand();
        }
    }

    fn futex_wait(
        &mut self,
        addr: u64,
        expected: u32,
        timeout_ns: Option<u64>,
    ) -> Result<(), Errno> {
        self.futex_calls.push((addr, expected, timeout_ns));
        let word = u32::from_le_bytes(self.mem.peek(addr, 4).try_into().unwrap());
        if word != expected {
            return Err(EAGAIN);
        }
        match timeout_ns {
            Some(t) => {
                self.now_ns += t;
                Err(ETIMEDOUT)
            }
            None => Ok(()),
        }
    }

    fn futex_wake(&mut self, _addr: u64, count: u32) -> Result<u32, Errno> {
        Ok(self.futex_waiters.min(count))
    }

    fn set_fs_base(&mut self, value: u64) -> Result<(), Errno> {
        self.fs_base = value;
        Ok(())
    }
}

pub const SCRATCH: u64 = 0x1000_0000;
pub const SCRATCH_LEN: u64 = 0x40_000;

pub fn config() -> Config {
    Config {
        pid: 4242,
        uid: 1000,
        gid: 1000,
        window: (0x1_0000, 0x7fff_ffff_f000),
        brk_base: 0x4000_0000,
        mem_limit: 1 << 30,
        allow_wx: false,
        spawnable: &[b"/bin/true"],
        release: b"6.1.0-nanox",
    }
}

/// A program under the personality: scripted from the test, one system call at a time.
pub struct Proc {
    pub p: Personality<Mock>,
    pub mem: Mem,
    next: u64,
}

impl Proc {
    pub fn new() -> Self {
        Self::with(config())
    }

    pub fn with(cfg: Config) -> Self {
        let mem = Mem::default();
        mem.map(SCRATCH, SCRATCH_LEN, 3);
        let mut be = Mock::new(mem.clone());
        let tty = be.open(b"/dev/tty", O_RDWR, 0).unwrap().0;
        Proc {
            p: Personality::new(be, cfg, tty),
            mem,
            next: SCRATCH,
        }
    }

    pub fn be(&mut self) -> &mut Mock {
        self.p.backend()
    }

    pub fn outcome(&mut self, nr: u32, a: &[u64]) -> Outcome {
        let mut args = [0u64; 6];
        args[..a.len()].copy_from_slice(a);
        let mut mem = self.mem.clone();
        self.p.syscall(&mut mem, u64::from(nr), args)
    }

    /// The value of a call that returns (negative: `-errno`).
    pub fn call(&mut self, nr: u32, a: &[u64]) -> i64 {
        match self.outcome(nr, a) {
            Outcome::Return(v) => v,
            other => panic!("syscall {nr} did not return: {other:?}"),
        }
    }

    pub fn put(&mut self, data: &[u8]) -> u64 {
        let at = self.next;
        self.mem.poke(at, data);
        self.next += (data.len() as u64 + 16).next_multiple_of(16);
        assert!(self.next < SCRATCH + SCRATCH_LEN, "scratch exhausted");
        at
    }

    pub fn cstr(&mut self, s: &str) -> u64 {
        let mut v = s.as_bytes().to_vec();
        v.push(0);
        self.put(&v)
    }

    pub fn buf(&mut self, n: usize) -> u64 {
        self.put(&vec![0u8; n])
    }

    pub fn bytes(&self, addr: u64, n: usize) -> Vec<u8> {
        self.mem.peek(addr, n)
    }

    pub fn u32_at(&self, addr: u64) -> u32 {
        u32::from_le_bytes(self.mem.peek(addr, 4).try_into().unwrap())
    }

    pub fn u64_at(&self, addr: u64) -> u64 {
        u64::from_le_bytes(self.mem.peek(addr, 8).try_into().unwrap())
    }

    /// `open(path, flags, mode)` returning the descriptor (panics on error).
    pub fn open(&mut self, path: &str, flags: u32) -> i64 {
        let p = self.cstr(path);
        self.call(linux_compat::table::SYS_OPEN, &[p, u64::from(flags), 0o644])
    }

    pub fn write_str(&mut self, fd: i64, s: &str) -> i64 {
        let b = self.put(s.as_bytes());
        self.call(
            linux_compat::table::SYS_WRITE,
            &[fd as u64, b, s.len() as u64],
        )
    }

    pub fn read_str(&mut self, fd: i64, n: usize) -> (i64, String) {
        let b = self.buf(n);
        let r = self.call(linux_compat::table::SYS_READ, &[fd as u64, b, n as u64]);
        let got = if r > 0 {
            self.bytes(b, r as usize)
        } else {
            Vec::new()
        };
        (r, String::from_utf8_lossy(&got).into_owned())
    }
}

pub fn neg(e: Errno) -> i64 {
    -i64::from(e.0)
}
