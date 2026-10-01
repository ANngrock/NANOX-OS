//! Bookkeeping of the program address space: which ranges are mapped, with
//! what protection and backing, for `mmap`, `munmap`, `mprotect` and `brk`.
//! The kernel does the mapping; this keeps the personality honest about what
//! the program asked for. Operations come in two steps, so that a failing
//! backend leaves the books untouched: `plan_*` validates and computes,
//! `commit_*` applies.
//!
//! Rules: ranges are page aligned and inside the usable window; a mapping may
//! not be writable and executable at once unless the configuration allows it;
//! allocation without a hint goes top-down below the stack; the table is full
//! when two more entries (the worst case of a split) do not fit.

use crate::errno::{Errno, EINVAL, ENOMEM, EPERM};
use crate::fdtable::Obj;
use crate::mem::PAGE;

pub const PROT_READ: u8 = 1;
pub const PROT_WRITE: u8 = 2;
pub const PROT_EXEC: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backing {
    Anon,
    File { obj: Obj, offset: u64 },
    Heap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vma {
    pub start: u64,
    pub end: u64,
    pub prot: u8,
    pub backing: Backing,
}

const EMPTY: Vma = Vma {
    start: 0,
    end: 0,
    prot: 0,
    backing: Backing::Anon,
};

pub fn round_up(x: u64) -> Option<u64> {
    x.checked_add(PAGE - 1).map(|v| v & !(PAGE - 1))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MmapPlan {
    pub addr: u64,
    pub len: u64,
    pub prot: u8,
    pub backing: Backing,
    /// An existing mapping in the range is replaced (`MAP_FIXED`).
    pub replaces: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrkPlan {
    pub old_end: u64,
    pub new_end: u64,
}

pub struct Space<const N: usize> {
    v: [Vma; N],
    len: usize,
    lo: u64,
    hi: u64,
    brk_base: u64,
    brk_end: u64,
    allow_wx: bool,
    limit: u64,
}

impl<const N: usize> Space<N> {
    /// `lo..hi` is the usable window, `brk_base` where the heap starts, `limit`
    /// the most bytes that may be mapped at once.
    pub fn new(lo: u64, hi: u64, brk_base: u64, limit: u64, allow_wx: bool) -> Self {
        let brk_base = brk_base.next_multiple_of(PAGE);
        Self {
            v: [EMPTY; N],
            len: 0,
            lo: lo.next_multiple_of(PAGE),
            hi: hi & !(PAGE - 1),
            brk_base,
            brk_end: brk_base,
            allow_wx,
            limit,
        }
    }

    pub fn entries(&self) -> &[Vma] {
        &self.v[..self.len]
    }

    pub fn mapped_bytes(&self) -> u64 {
        self.entries().iter().map(|v| v.end - v.start).sum()
    }

    pub fn brk_end(&self) -> u64 {
        self.brk_end
    }

    /// The mapping containing `addr`.
    pub fn find(&self, addr: u64) -> Option<&Vma> {
        self.entries()
            .iter()
            .find(|v| v.start <= addr && addr < v.end)
    }

    /// Is every byte of `addr .. addr + len` mapped with all of `need`?
    pub fn covers(&self, addr: u64, len: u64, need: u8) -> bool {
        let Some(end) = addr.checked_add(len) else {
            return false;
        };
        let mut at = addr;
        while at < end {
            match self.find(at) {
                Some(v) if v.prot & need == need => at = v.end,
                _ => return false,
            }
        }
        true
    }

    fn room(&self) -> bool {
        self.len + 2 <= N
    }

    fn check_prot(&self, prot: u8) -> Result<(), Errno> {
        if prot & !7 != 0 {
            return Err(EINVAL);
        }
        if !self.allow_wx && prot & PROT_WRITE != 0 && prot & PROT_EXEC != 0 {
            return Err(EPERM);
        }
        Ok(())
    }

    fn overlaps(&self, a: u64, b: u64) -> bool {
        self.entries().iter().any(|v| v.start < b && a < v.end)
    }

    /// The highest free gap that fits `len`, placed at its top.
    fn find_free(&self, len: u64) -> Option<u64> {
        let mut gap_end = self.hi;
        for v in self.entries().iter().rev() {
            if v.end <= gap_end && gap_end - v.end >= len {
                return Some(gap_end - len);
            }
            gap_end = gap_end.min(v.start);
        }
        (gap_end >= self.lo && gap_end - self.lo >= len).then(|| gap_end - len)
    }

    pub fn plan_mmap(
        &self,
        hint: u64,
        len: u64,
        prot: u8,
        fixed: bool,
        backing: Backing,
    ) -> Result<MmapPlan, Errno> {
        self.check_prot(prot)?;
        if len == 0 {
            return Err(EINVAL);
        }
        let len = round_up(len).ok_or(ENOMEM)?;
        if !self.room() || self.mapped_bytes().saturating_add(len) > self.limit {
            return Err(ENOMEM);
        }
        let in_window = |a: u64| a >= self.lo && a.checked_add(len).is_some_and(|e| e <= self.hi);
        if fixed {
            if !hint.is_multiple_of(PAGE) {
                return Err(EINVAL);
            }
            if !in_window(hint) {
                return Err(ENOMEM);
            }
            let replaces = self.overlaps(hint, hint + len);
            return Ok(MmapPlan {
                addr: hint,
                len,
                prot,
                backing,
                replaces,
            });
        }
        let addr = if hint != 0
            && hint.is_multiple_of(PAGE)
            && in_window(hint)
            && !self.overlaps(hint, hint + len)
        {
            hint
        } else {
            self.find_free(len).ok_or(ENOMEM)?
        };
        Ok(MmapPlan {
            addr,
            len,
            prot,
            backing,
            replaces: false,
        })
    }

    pub fn commit_mmap(&mut self, p: MmapPlan) {
        self.cut(p.addr, p.addr + p.len);
        self.insert(Vma {
            start: p.addr,
            end: p.addr + p.len,
            prot: p.prot,
            backing: p.backing,
        });
    }

    /// Validates a `munmap`: the range is page aligned and in the window.
    pub fn plan_munmap(&self, addr: u64, len: u64) -> Result<(u64, u64), Errno> {
        if !addr.is_multiple_of(PAGE) || len == 0 {
            return Err(EINVAL);
        }
        let len = round_up(len).ok_or(EINVAL)?;
        let end = addr.checked_add(len).ok_or(EINVAL)?;
        if addr < self.lo || end > self.hi {
            return Err(EINVAL);
        }
        if !self.room() {
            return Err(ENOMEM);
        }
        Ok((addr, end))
    }

    pub fn commit_munmap(&mut self, a: u64, b: u64) {
        self.cut(a, b);
    }

    pub fn plan_mprotect(&self, addr: u64, len: u64, prot: u8) -> Result<(u64, u64), Errno> {
        self.check_prot(prot)?;
        if !addr.is_multiple_of(PAGE) {
            return Err(EINVAL);
        }
        if len == 0 {
            return Ok((addr, addr));
        }
        let end = addr
            .checked_add(round_up(len).ok_or(ENOMEM)?)
            .ok_or(ENOMEM)?;
        if !self.covers(addr, end - addr, 0) || !self.room() {
            return Err(ENOMEM);
        }
        Ok((addr, end))
    }

    pub fn commit_mprotect(&mut self, a: u64, b: u64, prot: u8) {
        if a == b {
            return;
        }
        // Cut the range out, then put the pieces back with the new protection.
        let mut saved = [EMPTY; 8];
        let mut n = 0;
        for v in self.entries() {
            if v.start < b && a < v.end && n < saved.len() {
                let s = v.start.max(a);
                saved[n] = Vma {
                    start: s,
                    end: v.end.min(b),
                    prot,
                    backing: match v.backing {
                        Backing::File { obj, offset } => Backing::File {
                            obj,
                            offset: offset + (s - v.start),
                        },
                        other => other,
                    },
                };
                n += 1;
            }
        }
        self.cut(a, b);
        for p in &saved[..n] {
            self.insert(*p);
        }
    }

    /// A request to move the heap end to `want` (an address below the heap
    /// base only asks). On any problem the plan keeps the current end, as
    /// Linux does.
    pub fn plan_brk(&self, want: u64) -> BrkPlan {
        let cur = self.brk_end;
        let keep = BrkPlan {
            old_end: cur,
            new_end: cur,
        };
        if want < self.brk_base {
            return keep;
        }
        let Some(new_end) = round_up(want) else {
            return keep;
        };
        if new_end > self.hi || (new_end > cur && self.overlaps(cur, new_end)) {
            return keep;
        }
        if new_end > cur
            && (self.mapped_bytes().saturating_add(new_end - cur) > self.limit || !self.room())
        {
            return keep;
        }
        BrkPlan {
            old_end: cur,
            new_end,
        }
    }

    pub fn commit_brk(&mut self, p: BrkPlan) {
        if p.new_end > p.old_end {
            if p.old_end == self.brk_base {
                let prot = PROT_READ | PROT_WRITE;
                self.insert(Vma {
                    start: p.old_end,
                    end: p.new_end,
                    prot,
                    backing: Backing::Heap,
                });
            } else {
                for v in self.v[..self.len].iter_mut() {
                    if v.backing == Backing::Heap && v.end == p.old_end {
                        v.end = p.new_end;
                    }
                }
            }
        } else if p.new_end < p.old_end {
            self.cut(p.new_end, p.old_end);
        }
        self.brk_end = p.new_end;
    }

    /// Removes `a..b` from the table, splitting entries that stick out.
    fn cut(&mut self, a: u64, b: u64) {
        let mut out = [EMPTY; N];
        let mut n = 0;
        for v in self.v[..self.len].iter() {
            if v.end <= a || v.start >= b {
                out[n] = *v;
                n += 1;
                continue;
            }
            if v.start < a && n < N {
                out[n] = Vma { end: a, ..*v };
                n += 1;
            }
            if v.end > b && n < N {
                let backing = match v.backing {
                    Backing::File { obj, offset } => Backing::File {
                        obj,
                        offset: offset + (b - v.start),
                    },
                    other => other,
                };
                out[n] = Vma {
                    start: b,
                    backing,
                    ..*v
                };
                n += 1;
            }
        }
        self.v = out;
        self.len = n;
    }

    /// Inserts a non-overlapping entry in address order and merges neighbours
    /// that are identical and contiguous.
    fn insert(&mut self, e: Vma) {
        let at = self
            .entries()
            .iter()
            .position(|v| v.start > e.start)
            .unwrap_or(self.len);
        if self.len < N {
            let mut i = self.len;
            while i > at {
                self.v[i] = self.v[i - 1];
                i -= 1;
            }
            self.v[at] = e;
            self.len += 1;
        }
        self.merge();
    }

    fn merge(&mut self) {
        let mut i = 0;
        while i + 1 < self.len {
            let (a, b) = (self.v[i], self.v[i + 1]);
            let joinable = a.end == b.start
                && a.prot == b.prot
                && match (a.backing, b.backing) {
                    (Backing::Anon, Backing::Anon) | (Backing::Heap, Backing::Heap) => true,
                    (
                        Backing::File {
                            obj: o1,
                            offset: f1,
                        },
                        Backing::File {
                            obj: o2,
                            offset: f2,
                        },
                    ) => o1 == o2 && f1 + (a.end - a.start) == f2,
                    _ => false,
                };
            if joinable {
                self.v[i].end = b.end;
                for j in i + 1..self.len - 1 {
                    self.v[j] = self.v[j + 1];
                }
                self.len -= 1;
            } else {
                i += 1;
            }
        }
    }

    /// The table is sorted, page aligned, non-empty per entry, non-overlapping,
    /// inside the window, and nothing is both writable and executable unless allowed.
    pub fn check(&self) -> Result<(), &'static str> {
        let mut prev_end = 0;
        for v in self.entries() {
            if !v.start.is_multiple_of(PAGE) || !v.end.is_multiple_of(PAGE) {
                return Err("entry not page aligned");
            }
            if v.start >= v.end {
                return Err("empty or inverted entry");
            }
            if v.start < prev_end {
                return Err("entries overlap or are out of order");
            }
            if v.start < self.lo.min(self.brk_base) || v.end > self.hi {
                return Err("entry outside the window");
            }
            if !self.allow_wx && v.prot & PROT_WRITE != 0 && v.prot & PROT_EXEC != 0 {
                return Err("writable and executable entry");
            }
            prev_end = v.end;
        }
        Ok(())
    }
}
