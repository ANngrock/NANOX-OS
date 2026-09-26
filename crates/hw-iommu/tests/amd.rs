//! AMD-Vi: EFR against QEMU and the candidate machine, structure layouts,
//! and end-to-end translation through the hardware model.

mod common;

use common::*;
use hw_iommu::amd::{
    cmd, command_buffer_base_value, AmdDomain, DeviceTable, DeviceTableEntry, DteTranslation,
    ExtendedFeatures, Invalidator,
};
use hw_iommu::{
    Access, Bdf, CommandRing, DmaMapping, DomainConfig, Error, InvalidationQueue, PagingLevels,
    Perms, PhysMem, PAGE_SIZE,
};

const IVRS_QEMU: &[u8] = include_bytes!("../../../tests/fixtures/acpi/q35-smp4-amd-iommu/IVRS.bin");
const IVRS_LENOVO: &[u8] = include_bytes!("../../../tests/fixtures/acpi/lenovo-82k8/IVRS.bin");

/// Extended Feature Register of `-device amd-iommu` on `pc-q35-9.2`,
/// QEMU 9.2.4 from the project flake, read with HMP `xp /1gx 0xfed80030`
/// on a machine stopped with `-S` (2026-09-26).
const QEMU_AMD_EFR: u64 = 0x29d3;

const PAGE: u64 = PAGE_SIZE;
const PHYS_BITS: u32 = 48;

struct Rig {
    mem: ArrayMem,
    alloc: TestAlloc,
    cfg: DomainConfig,
    dt: DeviceTable,
    hw: AmdHw,
    queue: InvalidationQueue<Invalidator>,
}

impl Rig {
    fn new() -> Self {
        let (ivinfo, blocks) = ivrs(IVRS_QEMU);
        let efr = ExtendedFeatures(blocks.iter().find_map(|b| b.efr).unwrap());
        let va_bits = (ivinfo >> 8) & 0x7f;
        let cfg = efr.domain_config(va_bits, PHYS_BITS).unwrap();
        let mut mem = ArrayMem::new();
        let dt = DeviceTable::new(&mut mem, DEV_TABLE_BASE, DEV_TABLE_PAGES, PHYS_BITS).unwrap();
        let hw = AmdHw::new(dt.register_value());
        let ring = CommandRing::new(RING_BASE, 256).unwrap();
        let queue = InvalidationQueue::new(&mut mem, ring, STATUS_ADDR, 1, Invalidator).unwrap();
        Self {
            mem,
            alloc: TestAlloc::new(),
            cfg,
            dt,
            hw,
            queue,
        }
    }

    fn domain(&mut self, id: u16) -> AmdDomain {
        AmdDomain::new(&mut self.mem, &mut self.alloc, id, self.cfg).unwrap()
    }

    fn attach(&mut self, dev: Bdf, domain: &AmdDomain) {
        self.dt.attach(&mut self.mem, dev, domain).unwrap();
        self.queue
            .submit(&mut self.mem, &[cmd::invalidate_devtab_entry(dev.raw())])
            .unwrap();
        self.run_hw(usize::MAX);
    }

    fn run_hw(&mut self, budget: usize) {
        let head = self.hw.run_queue(
            &mut self.mem,
            RING_BASE,
            self.queue.ring().tail_offset(),
            budget,
        );
        self.queue.update_head(head).unwrap();
        self.queue.poll(&self.mem).unwrap();
    }

    fn dma(&mut self, dev: Bdf, iova: u64, access: Access) -> Result<u64, Fault> {
        self.hw.translate(&self.mem, dev.raw(), iova, access)
    }
}

fn bdf(bus: u8, dev: u8, func: u8) -> Bdf {
    Bdf::new(bus, dev, func).unwrap()
}

#[test]
fn efr_from_qemu_register_and_ivrs_fixtures() {
    let (ivinfo, blocks) = ivrs(IVRS_QEMU);
    assert_eq!(blocks.len(), 2);
    assert_eq!(
        (blocks[0].ty, blocks[0].devid, blocks[0].base),
        (0x10, 0x0008, 0xfed8_0000)
    );
    assert_eq!((blocks[1].ty, blocks[1].base), (0x11, 0xfed8_0000));
    // The EFR image in IVHD 11h equals the live register.
    assert_eq!(blocks[1].efr, Some(QEMU_AMD_EFR));
    assert_eq!((ivinfo >> 8) & 0x7f, 40, "VAsize");

    let efr = ExtendedFeatures(QEMU_AMD_EFR);
    assert_eq!(efr.max_host_levels(), Ok(6));
    assert_eq!(efr.select_levels(), Ok(PagingLevels::Four));
    assert!(efr.prefetch() && efr.ppr() && efr.guest_translation());
    assert!(efr.invalidate_all() && efr.guest_vapic() && efr.hardware_error());
    assert!(!efr.x2apic() && !efr.no_execute() && !efr.perf_counters());
    let cfg = efr.domain_config(40, PHYS_BITS).unwrap();
    assert_eq!((cfg.levels(), cfg.iova_bits()), (PagingLevels::Four, 40));

    // Candidate machine (LENOVO 82K8, Ryzen 7 5800H).
    let (ivinfo, blocks) = ivrs(IVRS_LENOVO);
    let types: Vec<u8> = blocks.iter().map(|b| b.ty).collect();
    assert_eq!(types, vec![0x10, 0x11, 0x40]);
    assert!(blocks
        .iter()
        .all(|b| b.base == 0xfdd0_0000 && b.devid == 0x0002));
    assert_eq!(blocks[1].efr, Some(0x206d_73ef_2225_4ade));
    assert_eq!(blocks[1].efr, blocks[2].efr);
    assert_eq!((ivinfo >> 8) & 0x7f, 48, "VAsize");
    let efr = ExtendedFeatures(blocks[1].efr.unwrap());
    assert_eq!(efr.max_host_levels(), Ok(6));
    assert!(efr.x2apic() && efr.no_execute() && efr.invalidate_all() && efr.perf_counters());
    assert!(!efr.prefetch() && !efr.hardware_error());
    assert_eq!(efr.domain_config(48, PHYS_BITS).unwrap().iova_bits(), 48);

    assert_eq!(
        ExtendedFeatures(3 << 10).max_host_levels(),
        Err(Error::ReservedBits)
    );
    assert_eq!(ExtendedFeatures(0).max_host_levels(), Ok(4));
}

#[test]
fn device_table_entry_layout() {
    let mut mem = ArrayMem::new();
    let mut alloc = TestAlloc::new();
    let cfg = DomainConfig::new(PagingLevels::Four, 48, PHYS_BITS).unwrap();
    let dom = AmdDomain::new(&mut mem, &mut alloc, 0x1234, cfg).unwrap();
    let root = dom.root();
    let e = DeviceTableEntry::for_domain(&dom);
    let raw = e.encode(PHYS_BITS).unwrap();
    assert_eq!(raw, [root | 0x6000_0000_0000_0803, 0x1234, 0, 0]);
    assert_eq!(DeviceTableEntry::decode(raw, PHYS_BITS), Ok(e));
    assert_eq!(
        DeviceTableEntry::blocked().encode(PHYS_BITS),
        Ok([0b11, 0, 0, 0])
    );
    assert_eq!(
        DeviceTableEntry::decode([0, 0, 0, 0], PHYS_BITS),
        Ok(DeviceTableEntry {
            translation: DteTranslation::Invalid,
            domain_id: 0
        })
    );

    let [q0, q1, _, _] = raw;
    let bad = |r: [u64; 4]| DeviceTableEntry::decode(r, PHYS_BITS);
    assert_eq!(bad([q0 | 1 << 5, q1, 0, 0]), Err(Error::ReservedBits));
    assert_eq!(bad([q0 | 7 << 9, q1, 0, 0]), Err(Error::ReservedBits));
    assert_eq!(
        bad([q0 & !(7 << 9) | 5 << 9, q1, 0, 0]),
        Err(Error::Unsupported)
    );
    assert_eq!(bad([q0 & !2, q1, 0, 0]), Err(Error::Unsupported));
    assert_eq!(bad([q0 | 1 << 63, q1, 0, 0]), Err(Error::ReservedBits));
    assert_eq!(bad([q0, q1 | 1 << 16, 0, 0]), Err(Error::ReservedBits));
    assert_eq!(bad([q0, q1, 1, 0]), Err(Error::ReservedBits));
    assert_eq!(bad([q0 & !1, q1, 0, 0]), Err(Error::ReservedBits));
    assert_eq!(
        bad([q0 | 1 << 50, q1, 0, 0]),
        Err(Error::ReservedBits),
        "root above 48 bits"
    );
    assert_eq!(
        bad([0b11 | 1 << 12, 0, 0, 0]),
        Err(Error::ReservedBits),
        "root with Mode 0"
    );
}

#[test]
fn command_layout() {
    assert_eq!(
        cmd::completion_wait(0x1000, 0x55),
        Ok([0x1000_0000_0000_1001, 0x55])
    );
    assert_eq!(cmd::completion_wait(0x1004, 1), Err(Error::Unaligned));
    assert_eq!(cmd::completion_wait(1 << 52, 1), Err(Error::OutOfRange));
    assert_eq!(
        cmd::invalidate_devtab_entry(0x0018),
        [0x2000_0000_0000_0018, 0]
    );
    let op = 0x3000_0005_0000_0000;
    assert_eq!(
        cmd::invalidate_pages(5, 0x40_0000, PAGE),
        Ok([op, 0x40_0002])
    );
    assert_eq!(
        cmd::invalidate_pages(5, 0x40_0000, 2 * PAGE),
        Ok([op, 0x40_0003])
    );
    assert_eq!(
        cmd::invalidate_pages(5, 0x40_0000, 8 * PAGE),
        Ok([op, 0x40_3003])
    );
    assert_eq!(cmd::invalidate_all_pages(5), [op, 0x7fff_ffff_ffff_f003]);
    assert_eq!(cmd::invalidate_all(), [0x8000_0000_0000_0000, 0]);
    assert_eq!(cmd::invalidate_pages(5, 0, 0), Err(Error::ZeroLength));
    let ring = CommandRing::new(RING_BASE, 256).unwrap();
    assert_eq!(command_buffer_base_value(&ring), RING_BASE | 8 << 56);
}

#[test]
fn invalidate_pages_range_always_covers_request() {
    let mut rng = Prng::new(0x1a2b_3c4d);
    for _ in 0..20_000 {
        let first = rng.below(1 << 36);
        let span = if rng.below(4) == 0 { 1 << 20 } else { 64 };
        let pages = 1 + rng.below(span);
        let [_, q1] = cmd::invalidate_pages(9, first << 12, pages << 12).unwrap();
        let (start, count) = decode_pages_range(q1);
        assert!(
            start <= first && first + pages - 1 <= start + (count - 1),
            "{first:#x}+{pages} vs {start:#x}+{count}"
        );
        // Naturally aligned power-of-two block.
        assert_eq!(start % count, 0);
        if pages == 1 {
            assert_eq!((start, count), (first, 1));
        }
    }
    assert_eq!(
        decode_pages_range(cmd::invalidate_all_pages(1)[1]),
        (0, u64::MAX)
    );
}

#[test]
fn zeroed_device_table_passes_dma_through_so_new_blocks_all() {
    let mut mem = ArrayMem::new();
    for i in 0..u64::from(DEV_TABLE_PAGES) * PAGE / 8 {
        mem.write_u64(DEV_TABLE_BASE + i * 8, 0);
    }
    let dev = bdf(0, 3, 0);
    // Negative control: V = 0 entries are not translated or checked.
    let mut raw_hw = AmdHw::new(DEV_TABLE_BASE | u64::from(DEV_TABLE_PAGES - 1));
    assert_eq!(
        raw_hw.translate(&mem, dev.raw(), 0x1234_5000, Access::Write),
        Ok(0x1234_5000)
    );

    let dt = DeviceTable::new(&mut mem, DEV_TABLE_BASE, DEV_TABLE_PAGES, PHYS_BITS).unwrap();
    assert_eq!(dt.register_value(), DEV_TABLE_BASE | 3);
    assert_eq!(dt.entries(), 512);
    let mut hw = AmdHw::new(dt.register_value());
    assert_eq!(
        hw.translate(&mem, dev.raw(), 0x1234_5000, Access::Write),
        Err(Fault::TargetAbort)
    );
    assert_eq!(
        hw.translate(&mem, bdf(1, 31, 7).raw(), 0, Access::Read),
        Err(Fault::TargetAbort)
    );
    assert_eq!(dt.entry(&mem, dev), Ok(DeviceTableEntry::blocked()));
    assert_eq!(dt.entry(&mem, bdf(2, 0, 0)), Err(Error::OutOfRange));
    assert_eq!(
        hw.translate(&mem, bdf(2, 0, 0).raw(), 0, Access::Read),
        Err(Fault::DevTableRange)
    );

    assert_eq!(
        DeviceTable::new(&mut mem, DEV_TABLE_BASE + 8, 1, PHYS_BITS).err(),
        Some(Error::Unaligned)
    );
    assert_eq!(
        DeviceTable::new(&mut mem, DEV_TABLE_BASE, 0, PHYS_BITS).err(),
        Some(Error::OutOfRange)
    );
    assert_eq!(
        DeviceTable::new(&mut mem, DEV_TABLE_BASE, 513, PHYS_BITS).err(),
        Some(Error::OutOfRange)
    );
}

#[test]
fn translation_permissions_and_isolation() {
    let mut r = Rig::new();
    assert_eq!(r.cfg.iova_bits(), 40);
    let mut d1 = r.domain(1);
    let mut d2 = r.domain(2);
    let (a, b, unattached) = (bdf(0, 3, 0), bdf(0, 4, 0), bdf(1, 0, 0));
    r.attach(a, &d1);
    r.attach(b, &d2);
    assert_eq!(r.dt.attach(&mut r.mem, a, &d2), Err(Error::AlreadyAttached));

    d1.map(
        &mut r.mem,
        &mut r.alloc,
        0x10_0000,
        DMA_BASE,
        3 * PAGE,
        Perms::RW,
    )
    .unwrap();
    d1.map(
        &mut r.mem,
        &mut r.alloc,
        0x20_0000,
        DMA_BASE + 3 * PAGE,
        PAGE,
        Perms::R,
    )
    .unwrap();
    d2.map(
        &mut r.mem,
        &mut r.alloc,
        0x10_0000,
        DMA_BASE + 8 * PAGE,
        PAGE,
        Perms::W,
    )
    .unwrap();

    assert_eq!(r.dma(a, 0x10_2010, Access::Write), Ok(DMA_BASE + 0x2010));
    assert_eq!(r.dma(a, 0x20_0000, Access::Read), Ok(DMA_BASE + 3 * PAGE));
    assert_eq!(r.dma(a, 0x20_0008, Access::Write), Err(Fault::WriteDenied));
    assert_eq!(r.dma(a, 0x30_0000, Access::Read), Err(Fault::NotPresent));
    assert_eq!(r.dma(b, 0x10_0000, Access::Write), Ok(DMA_BASE + 8 * PAGE));
    assert_eq!(r.dma(b, 0x10_0000, Access::Read), Err(Fault::ReadDenied));
    assert_eq!(r.dma(b, 0x10_1000, Access::Write), Err(Fault::NotPresent));
    assert_eq!(
        r.dma(unattached, 0x10_0000, Access::Read),
        Err(Fault::TargetAbort)
    );

    // Crossing the 512 GiB boundary needs every level on both sides and
    // fits the 40-bit VAsize of the QEMU IOMMU; beyond it is out of range.
    let iova = (1u64 << 39) - PAGE;
    assert_eq!(
        d1.map(
            &mut r.mem,
            &mut r.alloc,
            iova,
            DMA_BASE,
            2 * PAGE,
            Perms::RW
        ),
        Ok(())
    );
    assert_eq!(r.dma(a, 1 << 39, Access::Read), Ok(DMA_BASE + PAGE));
    assert_eq!(
        d1.map(
            &mut r.mem,
            &mut r.alloc,
            (1 << 40) - PAGE,
            DMA_BASE,
            2 * PAGE,
            Perms::RW
        ),
        Err(Error::OutOfRange)
    );
    assert_eq!(r.dma(a, 1 << 48, Access::Read), Err(Fault::AddressWidth));
}

#[test]
fn stale_iotlb_until_invalidation_completes() {
    let mut r = Rig::new();
    let mut d = r.domain(7);
    let dev = bdf(0, 3, 0);
    r.attach(dev, &d);
    let buf = Buf {
        phys: DMA_BASE + 4 * PAGE,
        size: 4 * PAGE,
    };
    let mut m = DmaMapping::map(
        &mut d,
        &mut r.mem,
        &mut r.alloc,
        0x7_0000_0000,
        buf,
        Perms::RW,
    )
    .unwrap();
    for p in 0..4 {
        assert_eq!(
            r.dma(dev, 0x7_0000_0000 + p * PAGE, Access::Read),
            Ok(DMA_BASE + (4 + p) * PAGE)
        );
    }
    m.quiesce().unwrap();
    m.unmap(&mut d, &mut r.mem).unwrap();
    // Negative control: every page is still reachable through the IOTLB.
    for p in 0..4 {
        assert_eq!(
            r.dma(dev, 0x7_0000_0000 + p * PAGE, Access::Write),
            Ok(DMA_BASE + (4 + p) * PAGE)
        );
    }
    m.submit_invalidation(&mut r.mem, &mut r.queue).unwrap();
    assert_eq!(
        m.confirm_invalidation(&r.queue),
        Err(Error::InvalidationNotComplete)
    );
    assert_eq!(m.release(), Err(Error::InvalidTransition));
    r.run_hw(usize::MAX);
    m.confirm_invalidation(&r.queue).unwrap();
    for p in 0..4 {
        assert_eq!(
            r.dma(dev, 0x7_0000_0000 + p * PAGE, Access::Write),
            Err(Fault::NotPresent)
        );
    }
    assert_eq!(m.release().unwrap().phys, DMA_BASE + 4 * PAGE);
}

#[test]
fn detach_blocks_device_after_devtab_invalidation() {
    let mut r = Rig::new();
    let mut d = r.domain(3);
    let dev = bdf(0, 9, 2);
    r.attach(dev, &d);
    d.map(&mut r.mem, &mut r.alloc, 0x4000, DMA_BASE, PAGE, Perms::RW)
        .unwrap();
    assert!(r.dma(dev, 0x4000, Access::Write).is_ok());

    let old = r.dt.detach(&mut r.mem, dev).unwrap();
    assert_eq!(old, DeviceTableEntry::for_domain(&d));
    assert_eq!(r.dt.entry(&r.mem, dev), Ok(DeviceTableEntry::blocked()));
    assert_eq!(r.dt.detach(&mut r.mem, dev), Err(Error::NotAttached));
    // Negative control: the cached DTE and IOTLB entry still translate.
    assert_eq!(r.dma(dev, 0x4000, Access::Write), Ok(DMA_BASE));

    r.queue
        .submit(
            &mut r.mem,
            &[
                cmd::invalidate_devtab_entry(dev.raw()),
                cmd::invalidate_all_pages(3),
            ],
        )
        .unwrap();
    r.run_hw(usize::MAX);
    assert_eq!(r.dma(dev, 0x4000, Access::Write), Err(Fault::TargetAbort));
    assert_eq!(r.dma(dev, 0x4000, Access::Read), Err(Fault::TargetAbort));
}
