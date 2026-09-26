//! Generic 4 KiB I/O page tables and the per-domain `map`/`unmap` API.
//!
//! VT-d second-level tables and AMD-Vi host page tables share the x86
//! radix layout: 512 eight-byte entries per 4 KiB table, 9 IOVA bits per
//! level, 3 levels for 39-bit and 4 levels for 48-bit IOVAs. They differ
//! only in entry encoding, which [`PteFormat`] abstracts.
//!
//! `map` works in three passes so that an error leaves no trace in
//! IOMMU-visible memory:
//!
//! 1. walk every page of the range, reject conflicts and count the
//!    intermediate tables that must be created;
//! 2. allocate all those frames up front, chaining them through their
//!    first word (the frames are not reachable by the IOMMU yet); on
//!    failure the chain is returned to the allocator;
//! 3. link the zeroed tables and write the leaf entries.
//!
//! Intermediate tables are never freed by `unmap` (the IOMMU may cache
//! them in paging-structure caches); they are released by
//! [`Domain::destroy`] after the domain has been detached and invalidated.

use core::marker::PhantomData;

use crate::inval::{InvalidationQueue, InvalidationToken, QueueFormat};
use crate::{Error, FrameAlloc, Perms, PhysMem, PAGE_SHIFT, PAGE_SIZE};

/// Entries per 4 KiB table.
pub const ENTRIES_PER_TABLE: u64 = 512;
/// Widest physical address the entry formats can hold (bits 51:12).
pub const MAX_PHYS_BITS: u32 = 52;
/// Mask of address bits 51:12 used by both formats.
pub(crate) const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;
/// Terminator of the pre-allocated frame chain; never a frame address
/// because frames are 4 KiB aligned.
const CHAIN_END: u64 = 1;

/// Number of translation levels of a domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PagingLevels {
    /// 3 levels, 39-bit IOVA (VT-d AGAW 39, AMD-Vi mode 3).
    Three,
    /// 4 levels, 48-bit IOVA (VT-d AGAW 48, AMD-Vi mode 4).
    Four,
}

impl PagingLevels {
    /// Number of levels.
    #[must_use]
    pub const fn count(self) -> u32 {
        match self {
            Self::Three => 3,
            Self::Four => 4,
        }
    }

    /// IOVA width translated by this many levels.
    #[must_use]
    pub const fn address_bits(self) -> u32 {
        PAGE_SHIFT + 9 * self.count()
    }

    /// Levels for a count, if supported by this crate.
    #[must_use]
    pub const fn from_count(count: u32) -> Option<Self> {
        match count {
            3 => Some(Self::Three),
            4 => Some(Self::Four),
            _ => None,
        }
    }
}

/// Geometry of a domain: table depth and address widths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DomainConfig {
    levels: PagingLevels,
    iova_bits: u32,
    phys_bits: u32,
}

impl DomainConfig {
    /// `iova_bits` may be narrower than the table depth (VT-d MGAW, AMD-Vi
    /// VAsize); `phys_bits` is the host address width (VT-d DMAR HAW).
    pub const fn new(levels: PagingLevels, iova_bits: u32, phys_bits: u32) -> Result<Self, Error> {
        if iova_bits <= PAGE_SHIFT || iova_bits > levels.address_bits() {
            return Err(Error::Unsupported);
        }
        if phys_bits <= PAGE_SHIFT || phys_bits > MAX_PHYS_BITS {
            return Err(Error::Unsupported);
        }
        Ok(Self {
            levels,
            iova_bits,
            phys_bits,
        })
    }

    /// Table depth.
    #[must_use]
    pub const fn levels(self) -> PagingLevels {
        self.levels
    }

    /// Usable IOVA width.
    #[must_use]
    pub const fn iova_bits(self) -> u32 {
        self.iova_bits
    }

    /// Physical address width of frames and mapped pages.
    #[must_use]
    pub const fn phys_bits(self) -> u32 {
        self.phys_bits
    }

    /// First IOVA beyond the domain.
    #[must_use]
    pub const fn iova_limit(self) -> u64 {
        1 << self.iova_bits
    }

    /// First physical address beyond the host address width.
    #[must_use]
    pub const fn phys_limit(self) -> u64 {
        1 << self.phys_bits
    }
}

/// A decoded page-table entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// Hardware treats the entry as not present.
    NotPresent,
    /// Points to the next-level table; `perms` restrict everything below.
    Table {
        /// Physical address of the next table.
        pa: u64,
        /// Permissions granted at this level.
        perms: Perms,
    },
    /// Maps one 4 KiB page (only in level-1 tables).
    Page {
        /// Physical address of the page.
        pa: u64,
        /// Permissions of the page.
        perms: Perms,
    },
}

/// Entry encoding of one IOMMU page-table format.
///
/// `level` is the level of the table that holds the entry: 1 for the
/// leaf table, `levels.count()` for the root.
pub trait PteFormat {
    /// Non-leaf entry at `level` (>= 2) pointing to a level `level - 1` table.
    fn encode_table(next: u64, level: u32) -> u64;
    /// Level-1 entry mapping the 4 KiB page at `pa`.
    fn encode_page(pa: u64, perms: Perms) -> u64;
    /// Decodes an entry. Level 1 yields `NotPresent` or `Page`; higher
    /// levels yield `NotPresent` or `Table` (large pages are
    /// [`Error::Unsupported`]).
    fn decode(raw: u64, level: u32, phys_bits: u32) -> Result<Entry, Error>;
}

/// True when `pa` can be used as a table frame under `phys_bits`.
pub(crate) const fn frame_ok(pa: u64, phys_bits: u32) -> bool {
    pa.is_multiple_of(PAGE_SIZE) && pa >> phys_bits == 0
}

/// Writes zeroes over the frame at `pa`.
pub(crate) fn zero_frame<M: PhysMem>(mem: &mut M, pa: u64) {
    for i in 0..ENTRIES_PER_TABLE {
        mem.write_u64(pa + i * 8, 0);
    }
}

/// Allocates and zeroes one frame usable under `phys_bits`.
pub(crate) fn alloc_zeroed<M: PhysMem, A: FrameAlloc>(
    mem: &mut M,
    alloc: &mut A,
    phys_bits: u32,
) -> Result<u64, Error> {
    let pa = alloc.alloc_frame().ok_or(Error::OutOfFrames)?;
    if !frame_ok(pa, phys_bits) {
        alloc.free_frame(pa);
        return Err(Error::BadFrame { pa });
    }
    zero_frame(mem, pa);
    Ok(pa)
}

/// Smallest naturally aligned power-of-two block of pages covering
/// `[iova, iova + len)`: returns `(base, order)` with `2^order` pages.
/// Used for page-selective IOTLB invalidation in both formats.
pub fn covering_block(iova: u64, len: u64) -> Result<(u64, u32), Error> {
    if len == 0 {
        return Err(Error::ZeroLength);
    }
    let last = iova.checked_add(len - 1).ok_or(Error::Overflow)?;
    let first_page = iova >> PAGE_SHIFT;
    let last_page = last >> PAGE_SHIFT;
    let order = 64 - (first_page ^ last_page).leading_zeros();
    // order <= 52 because both page numbers are < 2^52.
    let base = ((first_page >> order) << order) << PAGE_SHIFT;
    Ok((base, order))
}

/// Index of `iova` in a table at `level`.
const fn index(iova: u64, level: u32) -> u64 {
    (iova >> (PAGE_SHIFT + 9 * (level - 1))) & (ENTRIES_PER_TABLE - 1)
}

/// Bytes translated by one entry of a table at `level`; a whole table at
/// `level` therefore spans `entry_span(level + 1)`.
const fn entry_span(level: u32) -> u64 {
    1 << (PAGE_SHIFT + 9 * (level - 1))
}

/// Mapped leaf found by a walk.
struct Leaf {
    slot: u64,
    pa: u64,
    perms: Perms,
}

/// One I/O address space: a page-table tree plus bookkeeping.
pub struct Domain<F: PteFormat> {
    id: u16,
    root: u64,
    config: DomainConfig,
    mapped_pages: u64,
    table_frames: u64,
    _format: PhantomData<F>,
}

impl<F: PteFormat> core::fmt::Debug for Domain<F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Domain")
            .field("id", &self.id)
            .field("root", &self.root)
            .field("config", &self.config)
            .field("mapped_pages", &self.mapped_pages)
            .field("table_frames", &self.table_frames)
            .finish()
    }
}

impl<F: PteFormat> Domain<F> {
    /// Creates an empty domain with a zeroed root table.
    /// `id` is the VT-d DID / AMD-Vi DomainID tagging its IOTLB entries.
    pub fn new<M: PhysMem, A: FrameAlloc>(
        mem: &mut M,
        alloc: &mut A,
        id: u16,
        config: DomainConfig,
    ) -> Result<Self, Error> {
        let root = alloc_zeroed(mem, alloc, config.phys_bits)?;
        Ok(Self {
            id,
            root,
            config,
            mapped_pages: 0,
            table_frames: 1,
            _format: PhantomData,
        })
    }

    /// Domain identifier.
    #[must_use]
    pub const fn id(&self) -> u16 {
        self.id
    }

    /// Physical address of the root table.
    #[must_use]
    pub const fn root(&self) -> u64 {
        self.root
    }

    /// Geometry.
    #[must_use]
    pub const fn config(&self) -> DomainConfig {
        self.config
    }

    /// Number of mapped 4 KiB pages.
    #[must_use]
    pub const fn mapped_pages(&self) -> u64 {
        self.mapped_pages
    }

    /// Number of frames owned as page tables (root included).
    #[must_use]
    pub const fn table_frames(&self) -> u64 {
        self.table_frames
    }

    /// Validates an IOVA range and returns its page count.
    fn check_iova_range(&self, iova: u64, len: u64) -> Result<u64, Error> {
        if len == 0 {
            return Err(Error::ZeroLength);
        }
        if !iova.is_multiple_of(PAGE_SIZE) || !len.is_multiple_of(PAGE_SIZE) {
            return Err(Error::Unaligned);
        }
        let end = iova.checked_add(len).ok_or(Error::Overflow)?;
        if end > self.config.iova_limit() {
            return Err(Error::OutOfRange);
        }
        Ok(len / PAGE_SIZE)
    }

    /// Maps `[iova, iova + len)` to `[phys, phys + len)` with `perms`.
    ///
    /// Fails without any change if a page is already mapped, the range is
    /// invalid, or the allocator cannot supply every needed table frame.
    /// If the tables change behind the domain's back between the checking
    /// and the writing pass (unreachable otherwise; debug builds panic), the
    /// written leaves are cleared and unused frames returned; tables linked
    /// by then stay empty and belong to the domain. If the IOMMU caches not-present entries (VT-d `CAP.CM`), the caller
    /// must also invalidate the range after a successful map.
    pub fn map<M: PhysMem, A: FrameAlloc>(
        &mut self,
        mem: &mut M,
        alloc: &mut A,
        iova: u64,
        phys: u64,
        len: u64,
        perms: Perms,
    ) -> Result<(), Error> {
        let pages = self.check_iova_range(iova, len)?;
        if !phys.is_multiple_of(PAGE_SIZE) {
            return Err(Error::Unaligned);
        }
        let phys_end = phys.checked_add(len).ok_or(Error::Overflow)?;
        if phys_end > self.config.phys_limit() {
            return Err(Error::OutOfRange);
        }
        if perms.is_empty() {
            return Err(Error::NoPermissions);
        }

        let needed = self.count_missing_tables(mem, iova, pages)?;
        let mut chain = self.reserve_frames(mem, alloc, needed)?;

        let mut linked = 0;
        for i in 0..pages {
            let va = iova + i * PAGE_SIZE;
            match self.leaf_slot_create(mem, va, &mut chain, &mut linked) {
                Ok(slot) => mem.write_u64(slot, F::encode_page(phys + i * PAGE_SIZE, perms)),
                Err(e) => {
                    self.roll_back_map(mem, alloc, iova, i, chain, linked);
                    // Pass 3 only sees entries pass 1 validated or that it
                    // created itself, so this needs memory changed behind
                    // the domain's back (debug builds stop here, after the
                    // rollback).
                    if cfg!(debug_assertions) {
                        panic!("map pass 3 diverged from pass 1: {e:?}");
                    }
                    return Err(e);
                }
            }
        }
        // Pass 1 counted exactly the tables pass 3 links.
        debug_assert_eq!(chain, CHAIN_END);
        self.mapped_pages += pages;
        self.table_frames += linked;
        Ok(())
    }

    /// Undoes pass 3 of a failed `map`: clears the `written` leaf entries
    /// from `iova` on and returns the unused frames of `chain`. Tables
    /// already linked stay (empty) in the tree and are accounted to the
    /// domain, so no frame leaks and no page becomes mapped.
    fn roll_back_map<M: PhysMem, A: FrameAlloc>(
        &mut self,
        mem: &mut M,
        alloc: &mut A,
        iova: u64,
        written: u64,
        chain: u64,
        linked: u64,
    ) {
        for i in 0..written {
            if let Ok(Some(leaf)) = self.find_leaf(mem, iova + i * PAGE_SIZE) {
                mem.write_u64(leaf.slot, 0);
            }
        }
        release_chain(mem, alloc, chain);
        self.table_frames += linked;
    }

    /// Unmaps `[iova, iova + len)`. Fails without change if any page of
    /// the range is not mapped. The IOTLB still holds the old translation
    /// until the caller invalidates it (see [`crate::dma`]).
    pub fn unmap<M: PhysMem>(&mut self, mem: &mut M, iova: u64, len: u64) -> Result<(), Error> {
        let pages = self.check_iova_range(iova, len)?;
        for i in 0..pages {
            let va = iova + i * PAGE_SIZE;
            if self.find_leaf(mem, va)?.is_none() {
                return Err(Error::NotMapped { iova: va });
            }
        }
        for i in 0..pages {
            let va = iova + i * PAGE_SIZE;
            let leaf = self.find_leaf(mem, va)?.ok_or(Error::Corrupt)?;
            mem.write_u64(leaf.slot, 0);
        }
        self.mapped_pages -= pages;
        Ok(())
    }

    /// Software walk: physical address and effective permissions of
    /// `iova`, or `None` if it is not mapped.
    pub fn lookup<M: PhysMem>(&self, mem: &M, iova: u64) -> Result<Option<(u64, Perms)>, Error> {
        if iova >= self.config.iova_limit() {
            return Err(Error::OutOfRange);
        }
        let offset = iova % PAGE_SIZE;
        Ok(self
            .find_leaf(mem, iova - offset)?
            .map(|leaf| (leaf.pa + offset, leaf.perms)))
    }

    /// Frees every table frame of the domain.
    ///
    /// Requires no mapped pages and a completed invalidation `token` that
    /// the caller issued after detaching the domain from every device
    /// (context-cache/DTE and IOTLB invalidation). On error the domain is
    /// returned unchanged.
    pub fn destroy<M: PhysMem, A: FrameAlloc, Q: QueueFormat>(
        self,
        mem: &M,
        alloc: &mut A,
        queue: &InvalidationQueue<Q>,
        token: InvalidationToken,
    ) -> Result<(), (Self, Error)> {
        if self.mapped_pages != 0 {
            return Err((self, Error::StillMapped));
        }
        match queue.is_complete(token) {
            Ok(true) => {}
            Ok(false) => return Err((self, Error::InvalidationNotComplete)),
            Err(e) => return Err((self, e)),
        }
        self.free_table(mem, alloc, self.root, self.config.levels.count());
        Ok(())
    }

    fn free_table<M: PhysMem, A: FrameAlloc>(&self, mem: &M, alloc: &mut A, pa: u64, level: u32) {
        if level > 1 {
            for i in 0..ENTRIES_PER_TABLE {
                let raw = mem.read_u64(pa + i * 8);
                if let Ok(Entry::Table { pa: child, .. }) =
                    F::decode(raw, level, self.config.phys_bits)
                {
                    self.free_table(mem, alloc, child, level - 1);
                }
            }
        }
        alloc.free_frame(pa);
    }

    /// Pass 1 of `map`: rejects mapped pages and counts missing tables.
    fn count_missing_tables<M: PhysMem>(
        &self,
        mem: &M,
        iova: u64,
        pages: u64,
    ) -> Result<u64, Error> {
        let top = self.config.levels.count();
        let mut needed = 0;
        for i in 0..pages {
            let va = iova + i * PAGE_SIZE;
            let mut table = self.root;
            let mut level = top;
            loop {
                let raw = mem.read_u64(table + index(va, level) * 8);
                match (F::decode(raw, level, self.config.phys_bits)?, level) {
                    (Entry::NotPresent, 1) => break,
                    (Entry::NotPresent, _) => {
                        // Tables at levels 1..level are missing for `va`.
                        // Each is shared by every page of its span; count
                        // it once, at the first page of the range inside it.
                        for l in 1..level {
                            if va == iova || va.is_multiple_of(entry_span(l + 1)) {
                                needed += 1;
                            }
                        }
                        break;
                    }
                    (Entry::Table { pa, .. }, l) if l > 1 => {
                        table = pa;
                        level -= 1;
                    }
                    (Entry::Page { .. }, 1) => return Err(Error::AlreadyMapped { iova: va }),
                    _ => return Err(Error::Corrupt),
                }
            }
        }
        Ok(needed)
    }

    /// Pass 2 of `map`: allocates `needed` frames as a chain.
    fn reserve_frames<M: PhysMem, A: FrameAlloc>(
        &self,
        mem: &mut M,
        alloc: &mut A,
        needed: u64,
    ) -> Result<u64, Error> {
        let mut head = CHAIN_END;
        for _ in 0..needed {
            let Some(pa) = alloc.alloc_frame() else {
                release_chain(mem, alloc, head);
                return Err(Error::OutOfFrames);
            };
            if !frame_ok(pa, self.config.phys_bits) {
                alloc.free_frame(pa);
                release_chain(mem, alloc, head);
                return Err(Error::BadFrame { pa });
            }
            mem.write_u64(pa, head);
            head = pa;
        }
        Ok(head)
    }

    /// Pass 3 helper: walks to the leaf slot of `va`, linking zeroed
    /// tables taken from `chain` where they are missing.
    fn leaf_slot_create<M: PhysMem>(
        &self,
        mem: &mut M,
        va: u64,
        chain: &mut u64,
        linked: &mut u64,
    ) -> Result<u64, Error> {
        let mut table = self.root;
        let mut level = self.config.levels.count();
        while level > 1 {
            let slot = table + index(va, level) * 8;
            match F::decode(mem.read_u64(slot), level, self.config.phys_bits)? {
                Entry::Table { pa, .. } => table = pa,
                Entry::NotPresent => {
                    let frame = *chain;
                    if frame == CHAIN_END {
                        return Err(Error::Corrupt);
                    }
                    *chain = mem.read_u64(frame);
                    zero_frame(mem, frame);
                    // The table is zeroed before it becomes reachable.
                    mem.write_u64(slot, F::encode_table(frame, level));
                    *linked += 1;
                    table = frame;
                }
                Entry::Page { .. } => return Err(Error::Corrupt),
            }
            level -= 1;
        }
        Ok(table + index(va, 1) * 8)
    }

    /// Read-only walk to the leaf of the page-aligned `va`.
    fn find_leaf<M: PhysMem>(&self, mem: &M, va: u64) -> Result<Option<Leaf>, Error> {
        let mut table = self.root;
        let mut level = self.config.levels.count();
        let mut perms = Perms::RW;
        loop {
            let slot = table + index(va, level) * 8;
            match (
                F::decode(mem.read_u64(slot), level, self.config.phys_bits)?,
                level,
            ) {
                (Entry::NotPresent, _) => return Ok(None),
                (Entry::Table { pa, perms: p }, l) if l > 1 => {
                    perms = perms.intersect(p);
                    table = pa;
                    level -= 1;
                }
                (Entry::Page { pa, perms: p }, 1) => {
                    return Ok(Some(Leaf {
                        slot,
                        pa,
                        perms: perms.intersect(p),
                    }));
                }
                _ => return Err(Error::Corrupt),
            }
        }
    }
}

/// Returns every frame of a pre-allocation chain to the allocator.
fn release_chain<M: PhysMem, A: FrameAlloc>(mem: &M, alloc: &mut A, mut head: u64) {
    while head != CHAIN_END {
        let next = mem.read_u64(head);
        alloc.free_frame(head);
        head = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covering_block_is_minimal_and_aligned() {
        assert_eq!(covering_block(0x5000, 0x1000), Ok((0x5000, 0)));
        assert_eq!(covering_block(0x2000, 0x2000), Ok((0x2000, 1)));
        // Pages 1 and 2 straddle a 2-page boundary: a 4-page block is needed.
        assert_eq!(covering_block(0x1000, 0x2000), Ok((0, 2)));
        assert_eq!(covering_block(0x1f_f000, 0x2000), Ok((0, 10)));
        assert_eq!(covering_block(0, 0), Err(Error::ZeroLength));
        assert_eq!(
            covering_block(u64::MAX - 0xfff, 0x2000),
            Err(Error::Overflow)
        );
        let (base, order) = covering_block(0xFFFF_FFFF_F000, 0x1000).unwrap();
        assert_eq!((base, order), (0xFFFF_FFFF_F000, 0));
    }

    /// Memory whose entry at `slot` reads as `value` once more than
    /// `after` reads were made: a change behind the domain's back between
    /// pass 1 and pass 3 of `map`.
    struct DivergingMem {
        words: std::vec::Vec<u64>,
        reads: std::cell::Cell<u64>,
        corrupt: Option<(u64, u64, u64)>,
    }

    const BASE: u64 = 0x10_0000;

    impl PhysMem for DivergingMem {
        fn read_u64(&self, pa: u64) -> u64 {
            self.reads.set(self.reads.get() + 1);
            if let Some((after, slot, value)) = self.corrupt {
                if self.reads.get() > after && pa == slot {
                    return value;
                }
            }
            self.words[((pa - BASE) / 8) as usize]
        }
        fn write_u64(&mut self, pa: u64, value: u64) {
            self.words[((pa - BASE) / 8) as usize] = value;
        }
    }

    struct Frames {
        free: std::vec::Vec<u64>,
        live: std::collections::BTreeSet<u64>,
    }

    impl FrameAlloc for Frames {
        fn alloc_frame(&mut self) -> Option<u64> {
            let pa = self.free.pop()?;
            self.live.insert(pa);
            Some(pa)
        }
        fn free_frame(&mut self, pa: u64) {
            assert!(self.live.remove(&pa));
            self.free.push(pa);
        }
    }

    #[test]
    fn diverging_pass_three_is_rolled_back() {
        use crate::vtd::SecondLevel;
        let mut mem = DivergingMem {
            words: std::vec![0xA5A5_A5A5_A5A5_A5A4; 64 * 512],
            reads: std::cell::Cell::new(0),
            corrupt: None,
        };
        let mut alloc = Frames {
            free: (0..64).rev().map(|f| BASE + f * PAGE_SIZE).collect(),
            live: std::collections::BTreeSet::new(),
        };
        let cfg = DomainConfig::new(PagingLevels::Three, 39, 39).unwrap();
        let mut d = Domain::<SecondLevel>::new(&mut mem, &mut alloc, 1, cfg).unwrap();
        // Tables for the 2 MiB region below 0x20_0000 exist; the region
        // above has none, so the map below pre-allocates one leaf table.
        d.map(
            &mut mem,
            &mut alloc,
            0x1f_0000,
            0x5000,
            PAGE_SIZE,
            Perms::RW,
        )
        .unwrap();
        let level2 = mem.read_u64(d.root()) & ADDR_MASK;
        let snapshot: std::vec::Vec<_> = alloc
            .live
            .iter()
            .map(|&pa| {
                (
                    pa,
                    (0..512)
                        .map(|i| mem.read_u64(pa + i * 8))
                        .collect::<std::vec::Vec<_>>(),
                )
            })
            .collect();
        let frames = d.table_frames();

        // Pass 1 makes 3 + 3 + 2 reads for pages 0x1fe, 0x1ff, 0x200;
        // pass 3 writes both low leaves, then finds the level-2 entry of
        // page 0x200 turned into a large-page entry.
        mem.corrupt = Some((mem.reads.get() + 12, level2 + 8, 0x7000 | 1 << 7 | 3));
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            d.map(
                &mut mem,
                &mut alloc,
                0x1f_e000,
                0x9000,
                3 * PAGE_SIZE,
                Perms::R,
            )
        }));
        match res {
            Err(_) => assert!(cfg!(debug_assertions), "only debug builds panic"),
            Ok(r) => assert_eq!(r, Err(Error::Unsupported)),
        }
        mem.corrupt = None;
        for va in [0x1f_e000, 0x1f_f000, 0x20_0000] {
            assert_eq!(d.lookup(&mem, va), Ok(None), "{va:#x} left mapped");
        }
        assert_eq!(d.table_frames(), frames);
        assert_eq!(d.mapped_pages(), 1);
        let after: std::vec::Vec<_> = alloc
            .live
            .iter()
            .map(|&pa| {
                (
                    pa,
                    (0..512)
                        .map(|i| mem.read_u64(pa + i * 8))
                        .collect::<std::vec::Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            after, snapshot,
            "pre-allocated frame leaked or tables changed"
        );
    }

    #[test]
    fn config_rejects_impossible_widths() {
        assert!(DomainConfig::new(PagingLevels::Three, 40, 39).is_err());
        assert!(DomainConfig::new(PagingLevels::Four, 48, 53).is_err());
        assert!(DomainConfig::new(PagingLevels::Four, 12, 40).is_err());
        let c = DomainConfig::new(PagingLevels::Four, 39, 46).unwrap();
        assert_eq!(c.iova_limit(), 1 << 39);
    }
}
