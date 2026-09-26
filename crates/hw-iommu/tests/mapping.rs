//! `Domain::map`/`unmap` boundaries and failure atomicity, run for both
//! entry formats and both table depths.

mod common;

use common::*;
use hw_iommu::amd::HostPageTable;
use hw_iommu::vtd::SecondLevel;
use hw_iommu::{
    CommandRing, Domain, DomainConfig, Error, InvalidationQueue, PagingLevels, Perms, PhysMem,
    PteFormat, PAGE_SIZE,
};

const PAGE: u64 = PAGE_SIZE;
const GIB: u64 = 1 << 30;

fn config(levels: PagingLevels) -> DomainConfig {
    DomainConfig::new(levels, levels.address_bits(), 40).unwrap()
}

fn setup<F: PteFormat>(levels: PagingLevels) -> (ArrayMem, TestAlloc, Domain<F>) {
    let mut mem = ArrayMem::new();
    let mut alloc = TestAlloc::new();
    let d = Domain::new(&mut mem, &mut alloc, 1, config(levels)).unwrap();
    (mem, alloc, d)
}

/// Contents of every frame the allocator has handed out.
fn snapshot(mem: &ArrayMem, alloc: &TestAlloc) -> Vec<(u64, Vec<u64>)> {
    alloc.live.iter().map(|&pa| (pa, mem.frame(pa))).collect()
}

macro_rules! for_formats {
    ($($name:ident => $body:ident),* $(,)?) => {
        mod vtd3 { $( #[test] fn $name() { super::$body::<hw_iommu::vtd::SecondLevel>(hw_iommu::PagingLevels::Three); } )* }
        mod vtd4 { $( #[test] fn $name() { super::$body::<hw_iommu::vtd::SecondLevel>(hw_iommu::PagingLevels::Four); } )* }
        mod amd3 { $( #[test] fn $name() { super::$body::<hw_iommu::amd::HostPageTable>(hw_iommu::PagingLevels::Three); } )* }
        mod amd4 { $( #[test] fn $name() { super::$body::<hw_iommu::amd::HostPageTable>(hw_iommu::PagingLevels::Four); } )* }
    };
}

for_formats! {
    range_validation => range_validation,
    double_map_is_rejected_without_change => double_map_is_rejected_without_change,
    unmap_of_unmapped_page_is_rejected_without_change => unmap_of_unmapped_page_is_rejected_without_change,
    frame_alloc_failure_leaves_no_trace => frame_alloc_failure_leaves_no_trace,
    unusable_frame_is_returned => unusable_frame_is_returned,
    fresh_tables_are_zeroed => fresh_tables_are_zeroed,
    destroy_frees_every_table_frame => destroy_frees_every_table_frame,
}

fn range_validation<F: PteFormat>(levels: PagingLevels) {
    let (mut mem, mut alloc, mut d) = setup::<F>(levels);
    let limit = d.config().iova_limit();
    let phys_limit = d.config().phys_limit();
    let before = snapshot(&mem, &alloc);
    let rw = Perms::RW;
    let mut map = |iova, phys, len, perms| d.map(&mut mem, &mut alloc, iova, phys, len, perms);
    assert_eq!(map(0x1000, DMA_BASE, 0, rw), Err(Error::ZeroLength));
    assert_eq!(map(0x1800, DMA_BASE, PAGE, rw), Err(Error::Unaligned));
    assert_eq!(map(0x1000, DMA_BASE + 8, PAGE, rw), Err(Error::Unaligned));
    assert_eq!(
        map(0x1000, DMA_BASE, PAGE + 0x800, rw),
        Err(Error::Unaligned)
    );
    assert_eq!(
        map(u64::MAX - 0xfff, DMA_BASE, 2 * PAGE, rw),
        Err(Error::Overflow)
    );
    assert_eq!(map(limit, DMA_BASE, PAGE, rw), Err(Error::OutOfRange));
    assert_eq!(
        map(limit - PAGE, DMA_BASE, 2 * PAGE, rw),
        Err(Error::OutOfRange)
    );
    assert_eq!(
        map(0x1000, phys_limit - PAGE, 2 * PAGE, rw),
        Err(Error::OutOfRange)
    );
    assert_eq!(
        map(0x1000, u64::MAX - 0xfff, 2 * PAGE, rw),
        Err(Error::Overflow)
    );
    assert_eq!(
        map(0x1000, DMA_BASE, PAGE, Perms::NONE),
        Err(Error::NoPermissions)
    );
    assert_eq!(
        snapshot(&mem, &alloc),
        before,
        "rejected maps must not touch memory"
    );
    assert_eq!(
        (d.mapped_pages(), d.table_frames(), alloc.live.len()),
        (0, 1, 1)
    );

    assert_eq!(d.unmap(&mut mem, 0x1000, 0), Err(Error::ZeroLength));
    assert_eq!(d.unmap(&mut mem, 0x1001, PAGE), Err(Error::Unaligned));
    assert_eq!(
        d.unmap(&mut mem, u64::MAX - 0xfff, 2 * PAGE),
        Err(Error::Overflow)
    );
    assert_eq!(d.unmap(&mut mem, limit, PAGE), Err(Error::OutOfRange));
    assert_eq!(
        d.unmap(&mut mem, 0x5000, PAGE),
        Err(Error::NotMapped { iova: 0x5000 })
    );
    assert_eq!(d.lookup(&mem, limit), Err(Error::OutOfRange));

    // The highest page and the highest physical page are usable.
    d.map(
        &mut mem,
        &mut alloc,
        limit - PAGE,
        phys_limit - PAGE,
        PAGE,
        Perms::W,
    )
    .unwrap();
    assert_eq!(
        d.lookup(&mem, limit - 1),
        Ok(Some((phys_limit - 1, Perms::W)))
    );
}

fn double_map_is_rejected_without_change<F: PteFormat>(levels: PagingLevels) {
    let (mut mem, mut alloc, mut d) = setup::<F>(levels);
    let base = GIB - 2 * PAGE;
    d.map(&mut mem, &mut alloc, base, DMA_BASE, 4 * PAGE, Perms::RW)
        .unwrap();
    let before = snapshot(&mem, &alloc);
    let frames = d.table_frames();
    // Overlaps the last page and extends into untouched territory.
    assert_eq!(
        d.map(
            &mut mem,
            &mut alloc,
            base + 3 * PAGE,
            DMA_BASE,
            4 * PAGE,
            Perms::R
        ),
        Err(Error::AlreadyMapped {
            iova: base + 3 * PAGE
        })
    );
    // Identical range again.
    assert_eq!(
        d.map(&mut mem, &mut alloc, base, DMA_BASE, 4 * PAGE, Perms::RW),
        Err(Error::AlreadyMapped { iova: base })
    );
    // Conflict only on the last page of a long range.
    assert_eq!(
        d.map(
            &mut mem,
            &mut alloc,
            base - 64 * PAGE,
            DMA_BASE,
            65 * PAGE,
            Perms::RW
        ),
        Err(Error::AlreadyMapped { iova: base })
    );
    assert_eq!(snapshot(&mem, &alloc), before);
    assert_eq!((d.table_frames(), d.mapped_pages()), (frames, 4));
    assert_eq!(d.lookup(&mem, base + 4 * PAGE), Ok(None));
    assert_eq!(
        d.lookup(&mem, base + 3 * PAGE),
        Ok(Some((DMA_BASE + 3 * PAGE, Perms::RW)))
    );
}

fn unmap_of_unmapped_page_is_rejected_without_change<F: PteFormat>(levels: PagingLevels) {
    let (mut mem, mut alloc, mut d) = setup::<F>(levels);
    d.map(
        &mut mem,
        &mut alloc,
        0x10_0000,
        DMA_BASE,
        2 * PAGE,
        Perms::RW,
    )
    .unwrap();
    d.map(&mut mem, &mut alloc, 0x10_3000, DMA_BASE, PAGE, Perms::RW)
        .unwrap();
    let before = snapshot(&mem, &alloc);
    // Hole at 0x10_2000.
    assert_eq!(
        d.unmap(&mut mem, 0x10_0000, 4 * PAGE),
        Err(Error::NotMapped { iova: 0x10_2000 })
    );
    // Range whose tables do not even exist.
    assert_eq!(
        d.unmap(&mut mem, 5 * GIB, PAGE),
        Err(Error::NotMapped { iova: 5 * GIB })
    );
    assert_eq!(snapshot(&mem, &alloc), before);
    assert_eq!(d.mapped_pages(), 3);
    d.unmap(&mut mem, 0x10_0000, 2 * PAGE).unwrap();
    assert_eq!(
        d.unmap(&mut mem, 0x10_1000, PAGE),
        Err(Error::NotMapped { iova: 0x10_1000 })
    );
    assert_eq!(d.lookup(&mem, 0x10_3000), Ok(Some((DMA_BASE, Perms::RW))));
    assert_eq!(d.mapped_pages(), 1);
}

fn frame_alloc_failure_leaves_no_trace<F: PteFormat>(levels: PagingLevels) {
    // A range straddling the 1 GiB boundary: two leaf tables, two
    // level-2 tables and, with 4 levels, one level-3 table.
    let iova = GIB - 2 * PAGE;
    let len = 4 * PAGE;
    let needed_empty: u64 = if levels == PagingLevels::Four { 5 } else { 4 };
    for prepopulated in [false, true] {
        // With the low side already populated (by a page in the same leaf
        // table) only the high side's level-2 and leaf tables are missing,
        // so the failure hits a partially populated tree.
        let needed = if prepopulated { 2 } else { needed_empty };
        for fail_after in 0..needed {
            let (mut mem, mut alloc, mut d) = setup::<F>(levels);
            if prepopulated {
                d.map(
                    &mut mem,
                    &mut alloc,
                    GIB - 64 * PAGE,
                    DMA_BASE,
                    PAGE,
                    Perms::R,
                )
                .unwrap();
            }
            let before = snapshot(&mem, &alloc);
            let frames = d.table_frames();
            alloc.fail_after = Some(fail_after as usize);
            assert_eq!(
                d.map(&mut mem, &mut alloc, iova, DMA_BASE, len, Perms::RW),
                Err(Error::OutOfFrames),
                "prepopulated = {prepopulated}, fail_after = {fail_after}"
            );
            assert_eq!(snapshot(&mem, &alloc), before, "fail_after = {fail_after}");
            assert_eq!(d.table_frames(), frames);
            for p in 0..4 {
                assert_eq!(d.lookup(&mem, iova + p * PAGE), Ok(None));
            }
            alloc.fail_after = None;
            d.map(&mut mem, &mut alloc, iova, DMA_BASE, len, Perms::RW)
                .unwrap();
            assert_eq!(d.table_frames(), frames + needed);
            assert_eq!(alloc.live.len() as u64, d.table_frames());
        }
    }
    // Exactly `needed_empty` frames suffice.
    let (mut mem, mut alloc, mut d) = setup::<F>(levels);
    alloc.fail_after = Some(needed_empty as usize);
    d.map(&mut mem, &mut alloc, iova, DMA_BASE, len, Perms::RW)
        .unwrap();
    assert_eq!(d.table_frames(), needed_empty + 1);
}

fn unusable_frame_is_returned<F: PteFormat>(levels: PagingLevels) {
    let (mut mem, mut alloc, mut d) = setup::<F>(levels);
    let before = snapshot(&mem, &alloc);
    alloc.bad_frame = Some(POOL_BASE + 0x100 * PAGE + 8);
    assert_eq!(
        d.map(&mut mem, &mut alloc, 0x1000, DMA_BASE, PAGE, Perms::RW),
        Err(Error::BadFrame {
            pa: POOL_BASE + 0x100 * PAGE + 8
        })
    );
    // Beyond the 40-bit physical width of the domain.
    alloc.bad_frame = Some(1 << 40);
    assert_eq!(
        d.map(&mut mem, &mut alloc, 0x1000, DMA_BASE, PAGE, Perms::RW),
        Err(Error::BadFrame { pa: 1 << 40 })
    );
    assert_eq!(snapshot(&mem, &alloc), before);
    assert_eq!(alloc.live.len(), 1);
}

fn fresh_tables_are_zeroed<F: PteFormat>(levels: PagingLevels) {
    // The allocator hands out frames full of garbage; neighbours of a
    // fresh mapping must read as not present.
    let (mut mem, mut alloc, mut d) = setup::<F>(levels);
    d.map(&mut mem, &mut alloc, 0x20_0000, DMA_BASE, PAGE, Perms::RW)
        .unwrap();
    for pa in alloc.live.clone() {
        let nonzero = mem.frame(pa).iter().filter(|w| **w != 0).count();
        assert_eq!(nonzero, 1, "table {pa:#x} holds exactly one entry");
    }
    assert_eq!(d.lookup(&mem, 0x20_1000), Ok(None));
    assert_eq!(d.lookup(&mem, 0x1f_f000), Ok(None));
    // Frames recycled from an earlier domain are zeroed too.
    let token_queue = queue(&mut mem);
    d.unmap(&mut mem, 0x20_0000, PAGE).unwrap();
    let token = submit_and_complete(&mut mem, token_queue);
    d.destroy(&mem, &mut alloc, &token.0, token.1).unwrap();
    let mut d2 = Domain::<F>::new(&mut mem, &mut alloc, 2, config(levels)).unwrap();
    d2.map(&mut mem, &mut alloc, 0x40_0000, DMA_BASE, PAGE, Perms::R)
        .unwrap();
    assert_eq!(d2.lookup(&mem, 0x20_0000), Ok(None));
}

fn destroy_frees_every_table_frame<F: PteFormat>(levels: PagingLevels) {
    let (mut mem, mut alloc, mut d) = setup::<F>(levels);
    for (i, iova) in [0, GIB - PAGE, 3 * GIB, (1 << 39) - 2 * PAGE]
        .into_iter()
        .enumerate()
    {
        d.map(&mut mem, &mut alloc, iova, DMA_BASE, 2 * PAGE, Perms::RW)
            .unwrap();
        assert_eq!(d.mapped_pages(), 2 * (i as u64 + 1));
    }
    assert_eq!(alloc.live.len() as u64, d.table_frames());
    let q = queue(&mut mem);
    let (q, token) = submit_and_complete(&mut mem, q);
    let d = match d.destroy(&mem, &mut alloc, &q, token) {
        Err((d, Error::StillMapped)) => d,
        _ => panic!("destroy with live mappings must fail"),
    };
    let mut d = d;
    for iova in [0, GIB - PAGE, 3 * GIB, (1 << 39) - 2 * PAGE] {
        d.unmap(&mut mem, iova, 2 * PAGE).unwrap();
    }
    // An invalidation submitted but not yet executed is not enough.
    let mut q = q;
    let pending = q.submit(&mut mem, &[]).unwrap();
    let d = match d.destroy(&mem, &mut alloc, &q, pending) {
        Err((d, Error::InvalidationNotComplete)) => d,
        _ => panic!("destroy before completion must fail"),
    };
    mem.write_u64(STATUS_ADDR, pending.seq());
    q.poll(&mem).unwrap();
    d.destroy(&mem, &mut alloc, &q, pending).unwrap();
    assert!(alloc.live.is_empty(), "leaked frames: {:x?}", alloc.live);
}

fn queue(mem: &mut ArrayMem) -> InvalidationQueue<hw_iommu::amd::Invalidator> {
    let ring = CommandRing::new(RING_BASE, 256).unwrap();
    InvalidationQueue::new(mem, ring, STATUS_ADDR, 1, hw_iommu::amd::Invalidator).unwrap()
}

/// Submits a wait and plays the hardware by storing its value.
fn submit_and_complete(
    mem: &mut ArrayMem,
    mut q: InvalidationQueue<hw_iommu::amd::Invalidator>,
) -> (
    InvalidationQueue<hw_iommu::amd::Invalidator>,
    hw_iommu::InvalidationToken,
) {
    let token = q
        .submit(mem, &[hw_iommu::amd::cmd::invalidate_all()])
        .unwrap();
    mem.write_u64(STATUS_ADDR, token.seq());
    q.poll(mem).unwrap();
    (q, token)
}

#[test]
fn table_permissions_are_intersected_across_levels() {
    // Hand-made VT-d non-leaf entry granting read only: a RW leaf below it
    // is effectively read-only (VT-d §3.6 / AMD-Vi §2.2.3 combine levels).
    let (mut mem, mut alloc, mut d) = setup::<SecondLevel>(PagingLevels::Three);
    d.map(&mut mem, &mut alloc, 0x1000, DMA_BASE, PAGE, Perms::RW)
        .unwrap();
    let root_entry = mem.read_u64(d.root());
    mem.write_u64(d.root(), root_entry & !2);
    assert_eq!(d.lookup(&mem, 0x1000), Ok(Some((DMA_BASE, Perms::R))));

    let (mut mem, mut alloc, mut d) = setup::<HostPageTable>(PagingLevels::Four);
    d.map(&mut mem, &mut alloc, 0x1000, DMA_BASE, PAGE, Perms::RW)
        .unwrap();
    let root_entry = mem.read_u64(d.root());
    mem.write_u64(d.root(), root_entry & !(1 << 61));
    assert_eq!(d.lookup(&mem, 0x1000), Ok(Some((DMA_BASE, Perms::W))));
}

#[test]
fn corrupt_tables_are_reported_not_followed() {
    let (mut mem, mut alloc, mut d) = setup::<HostPageTable>(PagingLevels::Four);
    d.map(&mut mem, &mut alloc, 0x1000, DMA_BASE, PAGE, Perms::RW)
        .unwrap();
    let before = snapshot(&mem, &alloc);
    // A reserved bit in the root entry stops every walk through it.
    let root_entry = mem.read_u64(d.root());
    mem.write_u64(d.root(), root_entry | 1 << 55);
    assert_eq!(d.lookup(&mem, 0x1000), Err(Error::ReservedBits));
    assert_eq!(
        d.map(&mut mem, &mut alloc, 0x2000, DMA_BASE, PAGE, Perms::RW),
        Err(Error::ReservedBits)
    );
    assert_eq!(d.unmap(&mut mem, 0x1000, PAGE), Err(Error::ReservedBits));
    mem.write_u64(d.root(), root_entry);
    assert_eq!(snapshot(&mem, &alloc), before);
}
