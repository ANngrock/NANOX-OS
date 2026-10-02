//! Process and thread creation, program replacement, waiting and signals to
//! oneself. These need the kernel: the personality only decides whether the
//! request is within what the configuration allows, and hands it on as
//! [`Outcome::Defer`] with the request unchanged. What the service loop does
//! with it (new address space, new thread, the child's descriptor table
//! cloned from [`Personality::fds`]) is the kernel side of route B.

use crate::abi::*;
use crate::backend::Backend;
use crate::errno::*;
use crate::mem::{read_cstr, read_u64, GuestMem, PATH_MAX};
use crate::personality::{Outcome, Personality};
use crate::vma::Space;

const CLONE_THREAD: u64 = 0x1_0000;
/// `CLONE_NEWNS`, `NEWCGROUP`, `NEWUTS`, `NEWIPC`, `NEWUSER`, `NEWPID`, `NEWNET`.
const CLONE_NEW_MASK: u64 =
    0x2_0000 | 0x0200_0000 | 0x0400_0000 | 0x0800_0000 | 0x1000_0000 | 0x2000_0000 | 0x4000_0000;
/// The smallest `struct clone_args` (up to and including `tls`).
const CLONE_ARGS_MIN: u64 = 64;

impl<B: Backend> Personality<B> {
    /// `clone`/`clone3`: threads are always allowed; a new process only when
    /// the configuration lists a program it may start; namespaces never.
    pub(crate) fn sys_clone(
        &mut self,
        mem: &dyn GuestMem,
        nr: u32,
        a: [u64; 6],
        v3: bool,
    ) -> Outcome {
        let flags = if v3 {
            if a[1] < CLONE_ARGS_MIN {
                return Outcome::Return(-i64::from(EINVAL.0));
            }
            match read_u64(mem, a[0]) {
                Ok(f) => f,
                Err(e) => return Outcome::Return(-i64::from(e.0)),
            }
        } else {
            a[0]
        };
        if flags & CLONE_NEW_MASK != 0 {
            return self.refuse(nr, EPERM);
        }
        if flags & CLONE_THREAD == 0 && self.cfg.spawnable.is_empty() {
            return self.refuse(nr, EPERM);
        }
        Outcome::Defer { nr, args: a }
    }

    /// `execve`: only a program the configuration lists, and only if it exists and is executable.
    pub(crate) fn sys_execve(&mut self, mem: &dyn GuestMem, nr: u32, a: [u64; 6]) -> Outcome {
        let mut raw = [0u8; PATH_MAX];
        let n = match read_cstr(mem, a[0], &mut raw) {
            Ok(0) => return Outcome::Return(-i64::from(ENOENT.0)),
            Ok(n) => n,
            Err(e) => return Outcome::Return(-i64::from(e.0)),
        };
        let mut out = [0u8; PATH_MAX];
        let r = match crate::path::resolve(&self.cwd[..self.cwd_len], &raw[..n], &mut out) {
            Ok(r) => r,
            Err(e) => return Outcome::Return(-i64::from(e.0)),
        };
        let path = &out[..r.len];
        if !self.cfg.spawnable.contains(&path) {
            return self.refuse(nr, EACCES);
        }
        match self.backend.stat(path, true) {
            Err(e) => Outcome::Return(-i64::from(e.0)),
            Ok(s) if s.kind != FileKind::File || s.perm & 0o111 == 0 => {
                Outcome::Return(-i64::from(EACCES.0))
            }
            Ok(_) => Outcome::Defer { nr, args: a },
        }
    }

    /// `tgkill`: signal 0 asks whether the thread exists; any other signal to
    /// this process is the kernel's to deliver; other processes do not exist.
    pub(crate) fn sys_tgkill(&mut self, nr: u32, a: [u64; 6]) -> Outcome {
        let me = u64::from(self.cfg.pid);
        if a[0] != me || a[1] != me {
            return Outcome::Return(-i64::from(ESRCH.0));
        }
        match a[2] {
            0 => Outcome::Return(0),
            1..=64 => Outcome::Defer { nr, args: a },
            _ => Outcome::Return(-i64::from(EINVAL.0)),
        }
    }

    /// Called by the service loop once a deferred `execve` has replaced the
    /// program: close-on-exec descriptors go, the address-space books and the
    /// thread pointer start over.
    pub fn exec_committed(&mut self) {
        let mut freed = [0u64; crate::personality::MAX_FDS];
        let mut n = 0;
        self.fds.close_on_exec(|obj| {
            freed[n] = obj;
            n += 1;
        });
        for obj in &freed[..n] {
            self.release(*obj);
        }
        let c = self.cfg;
        self.space = Space::new(c.window.0, c.window.1, c.brk_base, c.mem_limit, c.allow_wx);
        self.fs_base = 0;
        self.clear_tid = 0;
    }

    /// The address the thread-exit code must clear and wake (`set_tid_address`).
    pub fn clear_child_tid(&self) -> u64 {
        self.clear_tid
    }
}
