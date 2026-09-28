//! Nested page tables (APM 15.25): four-level long-mode tables translating
//! guest-physical to host-physical addresses. The processor treats every
//! nested access as a user access, so every level carries U/S = 1.
//!
//! `map` has no partial effect: it checks that the range is free and
//! counts the missing tables per 2 MiB / 1 GiB / 512 GiB region, allocates
//! them all up front (chained through their own first word, so no extra
//! storage is needed) and only then writes entries. `unmap` returns a
//! [`NeedsFlush`] token: the host frames may be reused only after the guest
//! TLB has been flushed ([`crate::vmm::Vcpu::note_unmap`]).

use crate::{Error, FrameAlloc, PhysMem, PAGE_SIZE, PHYS_ADDRESS_LIMIT};

const P: u64 = 1 << 0;
const RW: u64 = 1 << 1;
const US: u64 = 1 << 2;
const PS: u64 = 1 << 7;
const NX: u64 = 1 << 63;
const ADDR: u64 = 0x000F_FFFF_FFFF_F000;
/// Bits this crate writes; anything else in an entry is corruption.
const KNOWN: u64 = P | RW | US | NX | ADDR | (1 << 5) | (1 << 6);
const TABLE: u64 = P | RW | US;

/// Leaf permissions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NptPerms {
    pub write: bool,
    pub exec: bool,
}

impl NptPerms {
    pub const RWX: Self = Self {
        write: true,
        exec: true,
    };
    pub const RW: Self = Self {
        write: true,
        exec: false,
    };
    pub const RX: Self = Self {
        write: false,
        exec: true,
    };
    pub const RO: Self = Self {
        write: false,
        exec: false,
    };
}

/// Proof that entries were removed and the guest TLB must be flushed
/// before the unmapped host frames are reused.
#[must_use = "the guest TLB must be flushed before the frames are reused"]
#[derive(Debug, PartialEq, Eq)]
pub struct NeedsFlush {
    pub(crate) pages: u64,
}

impl NeedsFlush {
    /// Pages that were unmapped.
    pub fn pages(&self) -> u64 {
        self.pages
    }
}

/// A nested page table hierarchy.
#[derive(Debug)]
pub struct Npt {
    root: u64,
    gpa_limit: u64,
    tables: u64,
    mapped: u64,
}

fn index(gpa: u64, level: u32) -> u64 {
    (gpa >> (12 + 9 * level)) & 511
}

fn frame_ok(pa: u64) -> bool {
    pa.is_multiple_of(PAGE_SIZE) && pa != 0 && pa < PHYS_ADDRESS_LIMIT
}

impl Npt {
    /// Creates an empty hierarchy for guest-physical addresses below
    /// `2^gpa_bits` (at most 48 for four levels).
    pub fn new<M: PhysMem + ?Sized, A: FrameAlloc + ?Sized>(
        mem: &mut M,
        alloc: &mut A,
        gpa_bits: u32,
    ) -> Result<Self, Error> {
        if !(21..=48).contains(&gpa_bits) {
            return Err(Error::BadRange);
        }
        let root = alloc.alloc_frame().ok_or(Error::OutOfFrames)?;
        if !frame_ok(root) {
            alloc.free_frame(root);
            return Err(Error::BadAddress);
        }
        zero(mem, root);
        Ok(Self {
            root,
            gpa_limit: 1 << gpa_bits,
            tables: 1,
            mapped: 0,
        })
    }

    /// Physical address for the VMCB's N_CR3.
    pub fn root(&self) -> u64 {
        self.root
    }
    /// Table frames in use (including the root).
    pub fn table_frames(&self) -> u64 {
        self.tables
    }
    /// Mapped 4 KiB pages.
    pub fn mapped_pages(&self) -> u64 {
        self.mapped
    }

    fn check_range(&self, gpa: u64, len: u64) -> Result<u64, Error> {
        if len == 0 || !gpa.is_multiple_of(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE) {
            return Err(Error::BadRange);
        }
        let end = gpa.checked_add(len).ok_or(Error::BadRange)?;
        if end > self.gpa_limit {
            return Err(Error::BadRange);
        }
        Ok(end)
    }

    /// Entry at `level` for `gpa`, following present tables; `None` if a
    /// table on the way is missing. Returns (entry address, entry).
    fn walk<M: PhysMem + ?Sized>(
        &self,
        mem: &mut M,
        gpa: u64,
        level: u32,
    ) -> Result<Option<(u64, u64)>, Error> {
        let mut table = self.root;
        let mut l = 3;
        loop {
            let at = table + 8 * index(gpa, l);
            let e = mem.read_u64(at);
            if e & !KNOWN != 0 || e & PS != 0 {
                return Err(Error::Corrupt);
            }
            if l == level {
                return Ok(Some((at, e)));
            }
            if e & P == 0 {
                return Ok(None);
            }
            table = e & ADDR;
            l -= 1;
        }
    }

    /// Maps `[gpa, gpa + len)` to `[hpa, hpa + len)` with `perms`.
    pub fn map<M: PhysMem + ?Sized, A: FrameAlloc + ?Sized>(
        &mut self,
        mem: &mut M,
        alloc: &mut A,
        gpa: u64,
        hpa: u64,
        len: u64,
        perms: NptPerms,
    ) -> Result<(), Error> {
        let end = self.check_range(gpa, len)?;
        if !hpa.is_multiple_of(PAGE_SIZE)
            || hpa.checked_add(len).is_none_or(|e| e > PHYS_ADDRESS_LIMIT)
        {
            return Err(Error::BadAddress);
        }
        // Pass 1: nothing mapped yet in the range; count missing tables.
        let mut missing = 0u64;
        for level in (1..=3).rev() {
            let span = 1u64 << (12 + 9 * level);
            let mut a = gpa & !(span - 1);
            while a < end {
                if self.walk(mem, a, level)?.is_some_and(|(_, e)| e & P == 0)
                    || self.walk(mem, a, level)?.is_none()
                {
                    missing += 1;
                }
                a += span;
            }
        }
        let mut a = gpa;
        while a < end {
            if let Some((_, e)) = self.walk(mem, a, 0)? {
                if e & P != 0 {
                    return Err(Error::AlreadyMapped);
                }
            }
            a += PAGE_SIZE;
        }
        // Pass 2: allocate every missing table, chained through word 0.
        let mut chain = 0u64;
        for _ in 0..missing {
            match alloc.alloc_frame() {
                Some(f) if frame_ok(f) => {
                    mem.write_u64(f, chain);
                    chain = f;
                }
                Some(f) => {
                    alloc.free_frame(f);
                    free_chain(mem, alloc, chain);
                    return Err(Error::BadAddress);
                }
                None => {
                    free_chain(mem, alloc, chain);
                    return Err(Error::OutOfFrames);
                }
            }
        }
        // Pass 3: write tables and leaves.
        let leaf = P | US | if perms.write { RW } else { 0 } | if perms.exec { 0 } else { NX };
        let mut a = gpa;
        while a < end {
            let mut table = self.root;
            for level in (1..=3).rev() {
                let at = table + 8 * index(a, level);
                let mut e = mem.read_u64(at);
                if e & P == 0 {
                    let f = chain;
                    if f == 0 {
                        // Pass 1 miscounted; nothing reachable points at
                        // the half-built range yet except what we wrote.
                        return Err(Error::Corrupt);
                    }
                    chain = mem.read_u64(f);
                    zero(mem, f);
                    e = f | TABLE;
                    mem.write_u64(at, e);
                    self.tables += 1;
                }
                table = e & ADDR;
            }
            mem.write_u64(table + 8 * index(a, 0), (hpa + (a - gpa)) | leaf);
            self.mapped += 1;
            a += PAGE_SIZE;
        }
        debug_assert_eq!(chain, 0, "tables allocated but not used");
        free_chain(mem, alloc, chain);
        Ok(())
    }

    /// Removes the mapping of `[gpa, gpa + len)`; every page must be
    /// mapped. Intermediate tables stay until [`Npt::destroy`].
    pub fn unmap<M: PhysMem + ?Sized>(
        &mut self,
        mem: &mut M,
        gpa: u64,
        len: u64,
    ) -> Result<NeedsFlush, Error> {
        let end = self.check_range(gpa, len)?;
        let mut a = gpa;
        while a < end {
            match self.walk(mem, a, 0)? {
                Some((_, e)) if e & P != 0 => {}
                _ => return Err(Error::NotMapped),
            }
            a += PAGE_SIZE;
        }
        let mut a = gpa;
        while a < end {
            if let Some((at, _)) = self.walk(mem, a, 0)? {
                mem.write_u64(at, 0);
            }
            self.mapped -= 1;
            a += PAGE_SIZE;
        }
        Ok(NeedsFlush {
            pages: len / PAGE_SIZE,
        })
    }

    /// Host address and permissions for `gpa`, if mapped.
    pub fn translate<M: PhysMem + ?Sized>(
        &self,
        mem: &mut M,
        gpa: u64,
    ) -> Result<Option<(u64, NptPerms)>, Error> {
        if gpa >= self.gpa_limit {
            return Ok(None);
        }
        Ok(match self.walk(mem, gpa, 0)? {
            Some((_, e)) if e & P != 0 => Some((
                (e & ADDR) | (gpa & (PAGE_SIZE - 1)),
                NptPerms {
                    write: e & RW != 0,
                    exec: e & NX == 0,
                },
            )),
            _ => None,
        })
    }

    /// Frees every table frame. Only when no vCPU can use the tables any
    /// more (the VM is stopped); guest memory frames belong to the caller.
    pub fn destroy<M: PhysMem + ?Sized, A: FrameAlloc + ?Sized>(self, mem: &mut M, alloc: &mut A) {
        free_level(mem, alloc, self.root, 3);
    }
}

fn zero<M: PhysMem + ?Sized>(mem: &mut M, f: u64) {
    for i in 0..512 {
        mem.write_u64(f + 8 * i, 0);
    }
}

fn free_chain<M: PhysMem + ?Sized, A: FrameAlloc + ?Sized>(
    mem: &mut M,
    alloc: &mut A,
    mut chain: u64,
) {
    while chain != 0 {
        let next = mem.read_u64(chain);
        alloc.free_frame(chain);
        chain = next;
    }
}

fn free_level<M: PhysMem + ?Sized, A: FrameAlloc + ?Sized>(
    mem: &mut M,
    alloc: &mut A,
    table: u64,
    level: u32,
) {
    if level > 0 {
        for i in 0..512 {
            let e = mem.read_u64(table + 8 * i);
            if e & P != 0 {
                free_level(mem, alloc, e & ADDR, level - 1);
            }
        }
    }
    alloc.free_frame(table);
}
