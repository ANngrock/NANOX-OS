//! `mmap`, `munmap`, `mprotect`, `madvise`, `brk`: validated against the
//! address-space books first, carried out by the backend, and only then
//! recorded, so a refusal by the backend leaves the books as they were.

use crate::abi::*;
use crate::backend::Backend;
use crate::errno::*;
use crate::fdtable::Kind;
use crate::handlers::fd_of;
use crate::mem::PAGE;
use crate::personality::Personality;
use crate::vma::{Backing, PROT_WRITE};

/// Flags that only matter to a kernel with a page cache or a fork: accepted, no effect.
const MAP_IGNORED: u64 = 0x800 | 0x1000 | 0x4000 | 0x8000 | 0x1_0000 | 0x2_0000;

const MADV_DONTNEED: u64 = 4;
const MADV_FREE: u64 = 8;
/// Pure hints: normal, random, sequential, willneed, dontfork, dofork,
/// mergeable, unmergeable, hugepage, nohugepage, dontdump, dodump.
const MADV_HINTS: [u64; 12] = [0, 1, 2, 3, 10, 11, 12, 13, 14, 15, 16, 17];

impl<B: Backend> Personality<B> {
    pub(crate) fn sys_mmap(&mut self, a: [u64; 6]) -> Result<u64, Errno> {
        let (hint, len, flags, fd, off) = (a[0], a[1], a[3], a[4], a[5]);
        let prot = u8::try_from(a[2]).map_err(|_| EINVAL)?;
        let known = MAP_SHARED
            | MAP_PRIVATE
            | MAP_FIXED
            | MAP_ANONYMOUS
            | MAP_FIXED_NOREPLACE
            | MAP_IGNORED;
        if flags & !known != 0 {
            return Err(EINVAL);
        }
        let shared = flags & MAP_SHARED != 0;
        if shared == (flags & MAP_PRIVATE != 0) {
            return Err(EINVAL);
        }
        let backing = if flags & MAP_ANONYMOUS != 0 {
            Backing::Anon
        } else {
            if !off.is_multiple_of(PAGE) {
                return Err(EINVAL);
            }
            let o = *self.fds.get(fd_of(fd)?)?;
            if o.kind != Kind::File {
                return Err(ENODEV);
            }
            if o.flags & O_ACCMODE == O_WRONLY {
                return Err(EACCES);
            }
            // A shared writable view of a file would have to reach the file;
            // the backend has no such path.
            if shared && prot & PROT_WRITE != 0 {
                return Err(ENODEV);
            }
            Backing::File {
                obj: o.obj,
                offset: off,
            }
        };
        let fixed = flags & (MAP_FIXED | MAP_FIXED_NOREPLACE) != 0;
        let plan = self.space.plan_mmap(hint, len, prot, fixed, backing)?;
        if flags & MAP_FIXED_NOREPLACE != 0 && plan.replaces {
            return Err(EEXIST);
        }
        self.backend.map(&plan)?;
        self.space.commit_mmap(plan);
        Ok(plan.addr)
    }

    pub(crate) fn sys_munmap(&mut self, a: [u64; 6]) -> Result<u64, Errno> {
        let (start, end) = self.space.plan_munmap(a[0], a[1])?;
        self.backend.unmap(start, end)?;
        self.space.commit_munmap(start, end);
        Ok(0)
    }

    pub(crate) fn sys_mprotect(&mut self, a: [u64; 6]) -> Result<u64, Errno> {
        let prot = u8::try_from(a[2]).map_err(|_| EINVAL)?;
        let (start, end) = self.space.plan_mprotect(a[0], a[1], prot)?;
        if start != end {
            self.backend.protect(start, end, prot)?;
            self.space.commit_mprotect(start, end, prot);
        }
        Ok(0)
    }

    pub(crate) fn sys_madvise(&mut self, a: [u64; 6]) -> Result<u64, Errno> {
        let (addr, len, advice) = (a[0], a[1], a[2]);
        if !addr.is_multiple_of(PAGE) {
            return Err(EINVAL);
        }
        let known = MADV_HINTS.contains(&advice) || advice == MADV_DONTNEED || advice == MADV_FREE;
        if !known {
            return Err(EINVAL);
        }
        if len == 0 {
            return Ok(0);
        }
        let end = addr.checked_add(len.next_multiple_of(PAGE)).ok_or(ENOMEM)?;
        if !self.space.covers(addr, end - addr, 0) {
            return Err(ENOMEM);
        }
        if advice == MADV_DONTNEED || advice == MADV_FREE {
            self.backend.discard(addr, end)?;
        }
        Ok(0)
    }

    /// `brk`: the new end of the heap, or, when it cannot move, the old one.
    pub(crate) fn sys_brk(&mut self, a: [u64; 6]) -> Result<u64, Errno> {
        let plan = self.space.plan_brk(a[0]);
        if plan.new_end != plan.old_end && self.backend.resize_heap(&plan).is_ok() {
            self.space.commit_brk(plan);
        }
        Ok(self.space.brk_end())
    }
}
