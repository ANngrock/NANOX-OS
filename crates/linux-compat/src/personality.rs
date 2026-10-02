//! The dispatcher: one Linux system call in, one result out, with the
//! configuration as the limit of what the program can reach.

use crate::abi::*;
use crate::backend::Backend;
use crate::errno::*;
use crate::fdtable::{FdTable, Kind, Obj, Ofd};
use crate::mem::{GuestMem, PATH_MAX};
use crate::policy::{disposition, Disp, PROBED};
use crate::vma::Space;

pub const MAX_FDS: usize = 64;
pub const MAX_VMAS: usize = 128;
const AUDIT: usize = 16;
pub(crate) const DIR_PATH: usize = 1024;
const DIRS: usize = 16;

/// The path an open directory was opened at, for `openat` and `fchdir`.
#[derive(Clone, Copy)]
pub(crate) struct DirSlot {
    pub obj: Obj,
    pub len: u16,
    pub used: bool,
    pub path: [u8; DIR_PATH],
}

pub(crate) const NO_DIR: DirSlot = DirSlot {
    obj: 0,
    len: 0,
    used: false,
    path: [0; DIR_PATH],
};

/// What this process is allowed to be and do.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
    /// The usable address window, and where the heap starts.
    pub window: (u64, u64),
    pub brk_base: u64,
    /// Most bytes of address space the program may map.
    pub mem_limit: u64,
    pub allow_wx: bool,
    /// Programs it may start (absolute paths in its own root); nothing else is `execve`d.
    pub spawnable: &'static [&'static [u8]],
    pub release: &'static [u8],
}

/// The result of one system call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The value for RAX (negative: `-errno`).
    Return(i64),
    /// The program ended (`exit` ends a thread, `exit_group` the process).
    Exit { code: i32, group: bool },
    /// Process creation and waiting need the kernel: handed to the service
    /// loop with the request already checked against the configuration.
    Defer { nr: u32, args: [u64; 6] },
}

/// One refused system call, kept for inspection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub nr: u32,
    pub errno: Errno,
}

pub struct Personality<B: Backend> {
    pub(crate) backend: B,
    pub(crate) cfg: Config,
    pub(crate) fds: FdTable<MAX_FDS>,
    pub(crate) space: Space<MAX_VMAS>,
    pub(crate) cwd: [u8; PATH_MAX],
    pub(crate) cwd_len: usize,
    pub(crate) dirs: [DirSlot; DIRS],
    pub(crate) umask: u32,
    pub(crate) fs_base: u64,
    pub(crate) clear_tid: u64,
    audit: [Refusal; AUDIT],
    audit_len: usize,
    refused: u64,
}

impl<B: Backend> Personality<B> {
    /// A process with descriptors 0, 1, 2 on the terminal object `tty` and
    /// the working directory `/`.
    pub fn new(backend: B, cfg: Config, tty: Obj) -> Self {
        let mut fds = FdTable::new();
        // One description, three descriptors: closing them all closes the terminal once.
        let ofd = Ofd {
            obj: tty,
            kind: Kind::Tty,
            flags: O_RDWR,
            offset: 0,
        };
        if fds.insert(ofd, false, 0).is_ok() {
            for _ in 0..2 {
                let _ = fds.dup(0, 0, false);
            }
        }
        let mut cwd = [0u8; PATH_MAX];
        cwd[0] = b'/';
        Self {
            backend,
            cfg,
            fds,
            space: Space::new(
                cfg.window.0,
                cfg.window.1,
                cfg.brk_base,
                cfg.mem_limit,
                cfg.allow_wx,
            ),
            cwd,
            cwd_len: 1,
            dirs: [NO_DIR; DIRS],
            umask: 0o022,
            fs_base: 0,
            clear_tid: 0,
            audit: [Refusal {
                nr: 0,
                errno: ENOSYS,
            }; AUDIT],
            audit_len: 0,
            refused: 0,
        }
    }

    pub fn backend(&mut self) -> &mut B {
        &mut self.backend
    }

    pub fn fds(&self) -> &FdTable<MAX_FDS> {
        &self.fds
    }

    pub fn space(&self) -> &Space<MAX_VMAS> {
        &self.space
    }

    pub fn cwd(&self) -> &[u8] {
        &self.cwd[..self.cwd_len]
    }

    pub fn fs_base(&self) -> u64 {
        self.fs_base
    }

    /// The most recent refused (denied or unavailable-by-policy) calls, oldest first.
    pub fn refusals(&self) -> &[Refusal] {
        &self.audit[..self.audit_len]
    }

    pub fn refused_total(&self) -> u64 {
        self.refused
    }

    /// Records a refusal and answers with it.
    pub(crate) fn refuse(&mut self, nr: u32, e: Errno) -> Outcome {
        self.note(nr, e);
        Outcome::Return(-i64::from(e.0))
    }

    /// Records a refusal in the audit ring (the most recent `AUDIT` are kept).
    pub(crate) fn note(&mut self, nr: u32, e: Errno) {
        self.refused += 1;
        if self.audit_len == AUDIT {
            self.audit.copy_within(1.., 0);
            self.audit_len -= 1;
        }
        self.audit[self.audit_len] = Refusal { nr, errno: e };
        self.audit_len += 1;
    }

    /// Carries out system call `nr` with arguments `a` on behalf of the program.
    pub fn syscall(&mut self, mem: &mut dyn GuestMem, nr: u64, a: [u64; 6]) -> Outcome {
        let Ok(n) = u32::try_from(nr) else {
            return Outcome::Return(-i64::from(ENOSYS.0));
        };
        match disposition(n) {
            Disp::Stub(v) => Outcome::Return(v),
            Disp::Deny(e) => self.refuse(n, e),
            // A probe for a feature (rseq): "not available" is the expected answer, not an event.
            Disp::Missing if PROBED.contains(&n) => Outcome::Return(-i64::from(ENOSYS.0)),
            Disp::Missing => self.refuse(n, ENOSYS),
            Disp::Handled => self.handled(mem, n, a),
        }
    }
}
