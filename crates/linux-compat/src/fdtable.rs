//! The file-descriptor table: small integers naming open file descriptions,
//! with POSIX semantics. `dup`, `dup2` and `dup3` make several descriptors
//! share one description (and so one offset); the lowest free number is
//! always handed out; closing the last descriptor of a description tells the
//! caller to close the backend object.

use crate::errno::{Errno, EBADF, EMFILE, ENFILE};

/// The backend handle of an open object (opaque to this crate).
pub type Obj = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    PipeRead,
    PipeWrite,
    Tty,
    Null,
    Socket,
}

/// An open file description: shared by every descriptor that was duplicated from it.
#[derive(Clone, Copy, Debug)]
pub struct Ofd {
    pub obj: Obj,
    pub kind: Kind,
    /// Linux `O_*` status flags (append, nonblock, access mode).
    pub flags: u32,
    pub offset: u64,
}

#[derive(Clone, Copy)]
struct Slot {
    ofd: usize,
    cloexec: bool,
}

pub struct FdTable<const N: usize> {
    fds: [Option<Slot>; N],
    ofds: [Option<(Ofd, u16)>; N],
}

impl<const N: usize> Default for FdTable<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> FdTable<N> {
    pub const fn new() -> Self {
        Self {
            fds: [None; N],
            ofds: [None; N],
        }
    }

    pub fn open_count(&self) -> usize {
        self.fds.iter().flatten().count()
    }

    /// How many distinct open file descriptions the descriptors share.
    pub fn description_count(&self) -> usize {
        self.ofds.iter().flatten().count()
    }

    fn free_fd(&self, min: usize) -> Result<usize, Errno> {
        (min..N).find(|i| self.fds[*i].is_none()).ok_or(EMFILE)
    }

    /// Installs a new description at the lowest free descriptor at or above `min`.
    pub fn insert(&mut self, ofd: Ofd, cloexec: bool, min: usize) -> Result<usize, Errno> {
        let fd = self.free_fd(min)?;
        let slot = self.ofds.iter().position(|o| o.is_none()).ok_or(ENFILE)?;
        self.ofds[slot] = Some((ofd, 1));
        self.fds[fd] = Some(Slot { ofd: slot, cloexec });
        Ok(fd)
    }

    fn slot(&self, fd: usize) -> Result<Slot, Errno> {
        self.fds.get(fd).copied().flatten().ok_or(EBADF)
    }

    pub fn get(&self, fd: usize) -> Result<&Ofd, Errno> {
        let s = self.slot(fd)?;
        self.ofds[s.ofd].as_ref().map(|(o, _)| o).ok_or(EBADF)
    }

    pub fn get_mut(&mut self, fd: usize) -> Result<&mut Ofd, Errno> {
        let s = self.slot(fd)?;
        self.ofds[s.ofd].as_mut().map(|(o, _)| o).ok_or(EBADF)
    }

    pub fn cloexec(&self, fd: usize) -> Result<bool, Errno> {
        Ok(self.slot(fd)?.cloexec)
    }

    pub fn set_cloexec(&mut self, fd: usize, on: bool) -> Result<(), Errno> {
        self.slot(fd)?;
        if let Some(Some(s)) = self.fds.get_mut(fd) {
            s.cloexec = on;
        }
        Ok(())
    }

    /// A new descriptor, at or above `min`, for the same description.
    pub fn dup(&mut self, fd: usize, min: usize, cloexec: bool) -> Result<usize, Errno> {
        let s = self.slot(fd)?;
        let new = self.free_fd(min)?;
        self.share(s.ofd, new, cloexec);
        Ok(new)
    }

    fn share(&mut self, ofd: usize, new: usize, cloexec: bool) {
        if let Some((_, refs)) = self.ofds[ofd].as_mut() {
            *refs += 1;
        }
        self.fds[new] = Some(Slot { ofd, cloexec });
    }

    /// Makes `new` refer to the description of `old`, closing `new` first if
    /// it is open. Returns the object to close if that dropped its last
    /// reference. `old == new` changes nothing.
    pub fn dup_to(&mut self, old: usize, new: usize, cloexec: bool) -> Result<Option<Obj>, Errno> {
        let s = self.slot(old)?;
        if new >= N {
            return Err(EBADF);
        }
        if old == new {
            return Ok(None);
        }
        let freed = self.close(new).unwrap_or(None);
        self.share(s.ofd, new, cloexec);
        Ok(freed)
    }

    /// Closes a descriptor. Returns the object to close if it was the last
    /// reference to its description.
    pub fn close(&mut self, fd: usize) -> Result<Option<Obj>, Errno> {
        let s = self.slot(fd)?;
        self.fds[fd] = None;
        let entry = self.ofds[s.ofd].as_mut().ok_or(EBADF)?;
        entry.1 -= 1;
        if entry.1 == 0 {
            let obj = entry.0.obj;
            self.ofds[s.ofd] = None;
            Ok(Some(obj))
        } else {
            Ok(None)
        }
    }

    /// Closes every close-on-exec descriptor; `f` gets each object that was freed.
    pub fn close_on_exec(&mut self, mut f: impl FnMut(Obj)) {
        for fd in 0..N {
            if matches!(self.fds[fd], Some(Slot { cloexec: true, .. })) {
                if let Ok(Some(obj)) = self.close(fd) {
                    f(obj);
                }
            }
        }
    }

    /// Verifies the table: every description is referenced exactly as often as
    /// its count says, and no unreferenced description is kept.
    pub fn check(&self) -> Result<(), &'static str> {
        for (i, o) in self.ofds.iter().enumerate() {
            let refs = self.fds.iter().flatten().filter(|s| s.ofd == i).count();
            match o {
                Some((_, n)) if usize::from(*n) != refs => {
                    return Err("reference count out of step")
                }
                Some(_) if refs == 0 => return Err("unreferenced description kept"),
                None if refs != 0 => return Err("descriptor points at a free description"),
                _ => {}
            }
        }
        Ok(())
    }
}
