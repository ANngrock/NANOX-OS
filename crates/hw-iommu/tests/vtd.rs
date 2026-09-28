//! VT-d: register decoding against values read from QEMU, structure
//! layouts, and end-to-end translation through the hardware model.

mod common;

use common::*;
use hw_iommu::vtd::{
    decode_root_entry, inv, iqa_value, Capability, ContextEntry, ExtCapability, Invalidator,
    RootTable, TranslationType, VtdDomain,
};
use hw_iommu::{
    Access, Bdf, CommandRing, DmaMapping, DmaState, DomainConfig, Error, InvalidationQueue,
    PagingLevels, Perms, PhysMem, PAGE_SIZE,
};

const DMAR: &[u8] = include_bytes!("../../../tests/fixtures/acpi/q35-smp4-intel-iommu/DMAR.bin");

/// Registers of `-device intel-iommu` (default properties) on
/// `pc-q35-9.2`, QEMU 9.2.4 from the project flake, read with the HMP
/// command `xp /3gx 0xfed90000` on a machine stopped with `-S`
/// (2026-09-26). The register base comes from the DMAR fixture captured
/// with the same QEMU build.
const QEMU_VTD_VER: u64 = 0x10;
const QEMU_VTD_CAP: u64 = 0x00d2_008c_2226_0206;
const QEMU_VTD_ECAP: u64 = 0x0000_0000_00f0_0f4a;

const PAGE: u64 = PAGE_SIZE;

struct Rig {
    mem: ArrayMem,
    alloc: TestAlloc,
    caps: Capability,
    cfg: DomainConfig,
    root: RootTable,
    hw: VtdHw,
    queue: InvalidationQueue<Invalidator>,
}

impl Rig {
    fn new(cap: u64) -> Self {
        let (haw, _) = dmar_drhd(DMAR);
        let mut mem = ArrayMem::new();
        let mut alloc = TestAlloc::new();
        let caps = Capability(cap);
        let cfg = caps.domain_config(haw).unwrap();
        let root = RootTable::new(&mut mem, &mut alloc, haw).unwrap();
        let hw = VtdHw::new(root.address(), caps.max_guest_address_width());
        let ring = CommandRing::new(RING_BASE, 256).unwrap();
        let queue =
            InvalidationQueue::new(&mut mem, ring, STATUS_ADDR, 1, Invalidator::new(caps)).unwrap();
        Self {
            mem,
            alloc,
            caps,
            cfg,
            root,
            hw,
            queue,
        }
    }

    fn domain(&mut self, id: u16) -> VtdDomain {
        VtdDomain::new(&mut self.mem, &mut self.alloc, id, self.cfg).unwrap()
    }

    fn attach(&mut self, bdf: Bdf, domain: &VtdDomain) {
        let entry = ContextEntry::for_domain(domain, self.caps).unwrap();
        self.root
            .attach(&mut self.mem, &mut self.alloc, bdf, &entry)
            .unwrap();
    }

    /// Lets the hardware execute up to `budget` descriptors, then updates
    /// the software view of head and completion.
    fn run_hw(&mut self, budget: usize) {
        let head = self.hw.run_queue(&mut self.mem, self.queue.ring(), budget);
        self.queue.update_head(head).unwrap();
        self.queue.poll(&self.mem).unwrap();
    }

    fn dma(&mut self, bdf: Bdf, iova: u64, access: Access) -> Result<u64, Fault> {
        self.hw.translate(&self.mem, bdf.raw(), iova, access)
    }
}

fn bdf(bus: u8, dev: u8, func: u8) -> Bdf {
    Bdf::new(bus, dev, func).unwrap()
}

/// CAP with SAGAW and MGAW replaced.
fn cap_with(sagaw: u64, mgaw_bits: u64) -> u64 {
    (QEMU_VTD_CAP & !(0x1f << 8) & !(0x3f << 16)) | sagaw << 8 | (mgaw_bits - 1) << 16
}

#[test]
fn qemu_registers_and_dmar_fixture_decode() {
    let (haw, bases) = dmar_drhd(DMAR);
    assert_eq!(haw, 39);
    assert_eq!(bases, vec![0xfed9_0000]);
    assert_eq!(QEMU_VTD_VER, 0x10, "VT-d 1.0");

    let cap = Capability(QEMU_VTD_CAP);
    assert_eq!(cap.num_domains(), Ok(65536));
    assert_eq!(cap.sagaw(), 0b00010);
    assert!(cap.supports(PagingLevels::Three));
    assert!(!cap.supports(PagingLevels::Four));
    assert_eq!(cap.max_guest_address_width(), 39);
    assert!(!cap.caching_mode());
    assert!(!cap.required_write_buffer_flush());
    assert!(cap.page_selective_invalidation());
    assert_eq!(cap.max_address_mask(), 18);
    assert_eq!(cap.fault_recording_offset(), 0x220);
    assert_eq!(cap.fault_recording_count(), 1);
    assert!(cap.read_draining() && cap.write_draining());
    assert_eq!(cap.select_levels(), Ok(PagingLevels::Three));
    let cfg = cap.domain_config(haw).unwrap();
    assert_eq!(
        (cfg.levels(), cfg.iova_bits(), cfg.phys_bits()),
        (PagingLevels::Three, 39, 39)
    );

    let ecap = ExtCapability(QEMU_VTD_ECAP);
    assert!(ecap.queued_invalidation());
    assert!(ecap.interrupt_remapping());
    assert!(ecap.pass_through());
    assert!(!ecap.coherent());
    assert!(!ecap.device_tlb());
    assert!(!ecap.snoop_control());
    assert!(!ecap.extended_interrupt_mode());
    assert_eq!(ecap.iotlb_register_offset(), 0xf0);
    assert_eq!(ecap.max_handle_mask(), 0xf);
}

#[test]
fn capability_edge_cases() {
    let nd7 = Capability(QEMU_VTD_CAP | 7);
    assert_eq!(nd7.num_domains(), Err(Error::ReservedBits));
    assert_eq!(nd7.validate_domain_id(1), Err(Error::ReservedBits));

    let nd0 = Capability(QEMU_VTD_CAP & !7);
    assert_eq!(nd0.num_domains(), Ok(16));
    assert_eq!(nd0.validate_domain_id(15), Ok(()));
    assert_eq!(nd0.validate_domain_id(16), Err(Error::InvalidDomainId));

    let cm = Capability(QEMU_VTD_CAP | 1 << 7);
    assert!(cm.caching_mode());
    assert_eq!(cm.validate_domain_id(0), Err(Error::InvalidDomainId));
    assert_eq!(cm.validate_domain_id(1), Ok(()));
    assert_eq!(Capability(QEMU_VTD_CAP).validate_domain_id(0), Ok(()));

    // 57-bit only: nothing this crate implements.
    assert_eq!(
        Capability(cap_with(0b01000, 57)).select_levels(),
        Err(Error::Unsupported)
    );
    let both = Capability(cap_with(0b00110, 48));
    assert_eq!(both.select_levels(), Ok(PagingLevels::Four));
    assert_eq!(both.domain_config(46).unwrap().iova_bits(), 48);
    // MGAW narrower than the AGAW limits the usable IOVA width.
    assert_eq!(
        Capability(cap_with(0b00110, 41))
            .domain_config(46)
            .unwrap()
            .iova_bits(),
        41
    );
}

#[test]
fn root_and_context_entry_layout() {
    let e = ContextEntry {
        fault_processing_disable: false,
        translation: TranslationType::Untranslated,
        slpt_root: 0x1234_5000,
        levels: PagingLevels::Three,
        domain_id: 0x2a,
    };
    assert_eq!(e.encode(39), Ok([0x1234_5001, 0x2a01]));
    let f = ContextEntry {
        fault_processing_disable: true,
        translation: TranslationType::PassThrough,
        slpt_root: 0x1234_5000,
        levels: PagingLevels::Four,
        domain_id: 0xbeef,
    };
    assert_eq!(f.encode(39), Ok([0x1234_500b, 0x00be_ef02]));
    assert_eq!(
        ContextEntry::decode([0x1234_500b, 0x00be_ef02], 39),
        Ok(Some(f))
    );
    assert_eq!(ContextEntry::decode([0x1234_5001, 0x2a01], 39), Ok(Some(e)));
    let unaligned = ContextEntry {
        slpt_root: 0x1234_5800,
        ..e
    };
    assert_eq!(unaligned.encode(39), Err(Error::Unaligned));

    let [lo, hi] = [0x1234_5001u64, 0x2a01u64];
    // P = 0: the rest is ignored by hardware.
    assert_eq!(
        ContextEntry::decode([lo & !1 | 0xff0, u64::MAX], 39),
        Ok(None)
    );
    assert_eq!(
        ContextEntry::decode([lo | 1 << 4, hi], 39),
        Err(Error::ReservedBits)
    );
    assert_eq!(
        ContextEntry::decode([lo | 1 << 11, hi], 39),
        Err(Error::ReservedBits)
    );
    assert_eq!(
        ContextEntry::decode([lo, hi | 1 << 7], 39),
        Err(Error::ReservedBits)
    );
    assert_eq!(
        ContextEntry::decode([lo, hi | 1 << 24], 39),
        Err(Error::ReservedBits)
    );
    assert_eq!(
        ContextEntry::decode([lo | 3 << 2, hi], 39),
        Err(Error::ReservedBits)
    );
    assert_eq!(
        ContextEntry::decode([lo, hi & !7 | 3], 39),
        Err(Error::Unsupported)
    );
    assert_eq!(
        ContextEntry::decode([lo, hi & !7], 39),
        Err(Error::ReservedBits)
    );
    assert_eq!(
        ContextEntry::decode([lo | 1 << 40, hi], 39),
        Err(Error::ReservedBits)
    );
    // Bits 70:67 are ignored.
    assert_eq!(ContextEntry::decode([lo, hi | 0x78], 39), Ok(Some(e)));

    assert_eq!(decode_root_entry([0x5001, 0], 39), Ok(Some(0x5000)));
    assert_eq!(decode_root_entry([0x5000, 0], 39), Ok(None));
    assert_eq!(decode_root_entry([0x5003, 0], 39), Err(Error::ReservedBits));
    assert_eq!(decode_root_entry([0x5001, 1], 39), Err(Error::ReservedBits));
}

#[test]
fn invalidation_descriptor_layout() {
    let dev = bdf(0, 3, 0);
    assert_eq!(inv::context_global(), [0x11, 0]);
    assert_eq!(inv::context_domain(5), [0x0005_0021, 0]);
    assert_eq!(inv::context_device(5, dev), [0x0000_0018_0005_0031, 0]);
    let drain = inv::Drain {
        reads: true,
        writes: true,
    };
    assert_eq!(inv::iotlb_global(inv::Drain::default()), [0x12, 0]);
    assert_eq!(inv::iotlb_domain(7, drain), [0x0007_00e2, 0]);
    assert_eq!(
        inv::iotlb_pages(7, 0x40_0000, 10, inv::Drain::default()),
        Ok([0x0007_0032, 0x40_000a])
    );
    assert_eq!(inv::iotlb_pages(7, 0x1000, 1, drain), Err(Error::Unaligned));
    assert_eq!(inv::iotlb_pages(7, 0, 52, drain), Err(Error::OutOfRange));
    assert_eq!(
        inv::wait(0x1000, 0xabcd),
        Ok([0x0000_abcd_0000_0025, 0x1000])
    );
    assert_eq!(inv::wait(0x1002, 1), Err(Error::Unaligned));

    use hw_iommu::QueueFormat;
    let qemu = Invalidator::new(Capability(QEMU_VTD_CAP));
    // Pages 1..=2 need an aligned 4-page block: AM = 2 at IOVA 0.
    assert_eq!(qemu.iotlb_range(3, 0x1000, 0x2000), Ok([0x0003_00f2, 0x2]));
    // 2^19 pages exceed MAMV = 18: domain-selective instead.
    assert_eq!(qemu.iotlb_range(3, 0, 1 << 31), Ok([0x0003_00e2, 0]));
    let no_psi = Invalidator::new(Capability(QEMU_VTD_CAP & !(1 << 39) & !(3 << 54)));
    assert_eq!(no_psi.iotlb_range(3, 0x1000, 0x1000), Ok([0x0003_0022, 0]));

    assert_eq!(
        iqa_value(&CommandRing::new(RING_BASE, 256).unwrap()),
        RING_BASE
    );
    assert_eq!(
        iqa_value(&CommandRing::new(RING_BASE, 1024).unwrap()),
        RING_BASE | 2
    );
}

#[test]
fn translation_permissions_and_isolation() {
    let mut r = Rig::new(QEMU_VTD_CAP);
    let mut d1 = r.domain(1);
    let mut d2 = r.domain(2);
    let (a, b, other_bus, same_bus) = (bdf(0, 3, 0), bdf(0, 4, 0), bdf(1, 0, 0), bdf(0, 5, 0));
    r.attach(a, &d1);
    r.attach(b, &d2);
    assert_eq!(
        r.root.attach(
            &mut r.mem,
            &mut r.alloc,
            a,
            &ContextEntry::for_domain(&d2, r.caps).unwrap()
        ),
        Err(Error::AlreadyAttached)
    );
    assert_eq!(
        r.root.context(&r.mem, a).unwrap().map(|c| c.domain_id),
        Some(1)
    );

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
    // Same IOVA, other requester: the other domain's tables.
    assert_eq!(r.dma(b, 0x10_0000, Access::Write), Ok(DMA_BASE + 8 * PAGE));
    assert_eq!(r.dma(b, 0x10_0000, Access::Read), Err(Fault::ReadDenied));
    assert_eq!(r.dma(b, 0x10_1000, Access::Write), Err(Fault::NotPresent));
    assert_eq!(
        r.dma(other_bus, 0x10_0000, Access::Read),
        Err(Fault::RootNotPresent)
    );
    assert_eq!(
        r.dma(same_bus, 0x10_0000, Access::Read),
        Err(Fault::ContextNotPresent)
    );

    // The device's write lands in the buffer.
    let pa = r.dma(a, 0x10_1008, Access::Write).unwrap();
    r.mem.write_u64(pa, 0x1234);
    assert_eq!(r.mem.read_u64(DMA_BASE + PAGE + 8), 0x1234);

    assert_eq!(
        d1.lookup(&r.mem, 0x10_2010),
        Ok(Some((DMA_BASE + 0x2010, Perms::RW)))
    );
    assert_eq!(
        d1.lookup(&r.mem, 0x20_0000),
        Ok(Some((DMA_BASE + 3 * PAGE, Perms::R)))
    );

    // AGAW 39: the last page is usable, the next address is not.
    let top = (1u64 << 39) - PAGE;
    assert_eq!(
        d1.map(&mut r.mem, &mut r.alloc, top, DMA_BASE, 2 * PAGE, Perms::RW),
        Err(Error::OutOfRange)
    );
    d1.map(&mut r.mem, &mut r.alloc, top, DMA_BASE, PAGE, Perms::RW)
        .unwrap();
    assert_eq!(r.dma(a, top + 0xff8, Access::Read), Ok(DMA_BASE + 0xff8));
    assert_eq!(r.dma(a, 1 << 39, Access::Read), Err(Fault::AddressWidth));
}

#[test]
fn four_level_domain_crosses_512g_boundary() {
    let mut r = Rig::new(cap_with(0b00110, 48));
    assert_eq!(r.cfg.levels(), PagingLevels::Four);
    let mut d = r.domain(9);
    let dev = bdf(0, 2, 0);
    r.attach(dev, &d);
    assert_eq!(
        r.root.context(&r.mem, dev).unwrap().unwrap().levels,
        PagingLevels::Four
    );
    let iova = (1u64 << 39) - PAGE;
    d.map(
        &mut r.mem,
        &mut r.alloc,
        iova,
        DMA_BASE,
        2 * PAGE,
        Perms::RW,
    )
    .unwrap();
    // Root + two L3 + two L2 + two L1 tables.
    assert_eq!(d.table_frames(), 7);
    assert_eq!(r.dma(dev, iova, Access::Write), Ok(DMA_BASE));
    assert_eq!(r.dma(dev, 1 << 39, Access::Write), Ok(DMA_BASE + PAGE));
    let top = (1u64 << 48) - PAGE;
    d.map(&mut r.mem, &mut r.alloc, top, DMA_BASE, PAGE, Perms::R)
        .unwrap();
    assert_eq!(r.dma(dev, top | 0x10, Access::Read), Ok(DMA_BASE | 0x10));
    assert_eq!(r.dma(dev, 1 << 48, Access::Read), Err(Fault::AddressWidth));
}

#[test]
fn stale_iotlb_until_invalidation_completes() {
    let mut r = Rig::new(QEMU_VTD_CAP);
    let mut d = r.domain(1);
    let dev = bdf(0, 3, 0);
    r.attach(dev, &d);
    let buf = Buf {
        phys: DMA_BASE,
        size: 2 * PAGE,
    };
    let mut m =
        DmaMapping::map(&mut d, &mut r.mem, &mut r.alloc, 0x40_0000, buf, Perms::RW).unwrap();

    m.begin_dma().unwrap();
    assert_eq!(r.dma(dev, 0x40_0000, Access::Write), Ok(DMA_BASE));
    assert_eq!(r.dma(dev, 0x40_1008, Access::Read), Ok(DMA_BASE + PAGE + 8));
    m.quiesce().unwrap();
    assert_eq!(m.unmap(&mut d, &mut r.mem), Err(Error::DmaOutstanding));
    m.end_dma().unwrap();
    m.unmap(&mut d, &mut r.mem).unwrap();
    assert_eq!(d.lookup(&r.mem, 0x40_0000), Ok(None));

    // Negative control: the tables no longer map the page, yet the device
    // still reaches the buffer through the IOTLB.
    let hits = r.hw.iotlb_hits;
    assert_eq!(r.dma(dev, 0x40_0000, Access::Write), Ok(DMA_BASE));
    assert_eq!(r.dma(dev, 0x40_1000, Access::Write), Ok(DMA_BASE + PAGE));
    assert_eq!(r.hw.iotlb_hits, hits + 2);
    assert_eq!(m.release(), Err(Error::InvalidTransition));

    m.submit_invalidation(&mut r.mem, &mut r.queue).unwrap();
    // Submitted but not executed: still not confirmable, still stale.
    r.queue.poll(&r.mem).unwrap();
    assert_eq!(
        m.confirm_invalidation(&r.queue),
        Err(Error::InvalidationNotComplete)
    );
    assert_eq!(m.release(), Err(Error::InvalidTransition));
    assert_eq!(r.dma(dev, 0x40_0000, Access::Write), Ok(DMA_BASE));

    r.run_hw(usize::MAX);
    m.confirm_invalidation(&r.queue).unwrap();
    assert_eq!(r.dma(dev, 0x40_0000, Access::Write), Err(Fault::NotPresent));
    assert_eq!(r.dma(dev, 0x40_1000, Access::Read), Err(Fault::NotPresent));
    assert_eq!(
        m.release(),
        Ok(Buf {
            phys: DMA_BASE,
            size: 2 * PAGE
        })
    );
    assert_eq!(m.state(), DmaState::Freed);
    assert_eq!(m.release(), Err(Error::InvalidTransition));
}

#[test]
fn partial_queue_progress_confirms_only_earlier_tokens() {
    let mut r = Rig::new(QEMU_VTD_CAP);
    let mut d = r.domain(4);
    let dev = bdf(0, 6, 0);
    r.attach(dev, &d);
    let mut first = DmaMapping::map(
        &mut d,
        &mut r.mem,
        &mut r.alloc,
        0x1000,
        Buf {
            phys: DMA_BASE,
            size: PAGE,
        },
        Perms::R,
    )
    .unwrap();
    let mut second = DmaMapping::map(
        &mut d,
        &mut r.mem,
        &mut r.alloc,
        0x2000,
        Buf {
            phys: DMA_BASE + PAGE,
            size: PAGE,
        },
        Perms::R,
    )
    .unwrap();
    assert!(r.dma(dev, 0x2000, Access::Read).is_ok());
    for m in [&mut first, &mut second] {
        m.quiesce().unwrap();
        m.unmap(&mut d, &mut r.mem).unwrap();
        m.submit_invalidation(&mut r.mem, &mut r.queue).unwrap();
    }
    // Hardware executes only the first IOTLB descriptor and its wait.
    r.run_hw(2);
    assert_eq!(r.queue.tracker().completed(), 1);
    first.confirm_invalidation(&r.queue).unwrap();
    assert_eq!(
        second.confirm_invalidation(&r.queue),
        Err(Error::InvalidationNotComplete)
    );
    assert_eq!(
        r.dma(dev, 0x2000, Access::Read),
        Ok(DMA_BASE + PAGE),
        "second page still cached"
    );
    r.run_hw(usize::MAX);
    second.confirm_invalidation(&r.queue).unwrap();
    assert_eq!(r.dma(dev, 0x2000, Access::Read), Err(Fault::NotPresent));
}

#[test]
fn detach_needs_context_cache_invalidation_before_teardown() {
    let mut r = Rig::new(QEMU_VTD_CAP);
    let frames_before = r.alloc.live.len();
    let mut d = r.domain(3);
    let dev = bdf(0, 7, 1);
    r.attach(dev, &d);
    d.map(
        &mut r.mem,
        &mut r.alloc,
        0x8000_0000,
        DMA_BASE,
        4 * PAGE,
        Perms::RW,
    )
    .unwrap();
    assert!(r.dma(dev, 0x8000_1000, Access::Read).is_ok());

    let old = r.root.detach(&mut r.mem, dev).unwrap();
    assert_eq!(old.domain_id, 3);
    assert_eq!(r.root.context(&r.mem, dev), Ok(None));
    assert_eq!(r.root.detach(&mut r.mem, dev), Err(Error::NotAttached));
    // Negative control: the cached context entry and IOTLB still translate.
    assert_eq!(r.dma(dev, 0x8000_1000, Access::Read), Ok(DMA_BASE + PAGE));

    d.unmap(&mut r.mem, 0x8000_0000, 4 * PAGE).unwrap();
    let token = r.queue.submit(
        &mut r.mem,
        &[
            inv::context_device(3, dev),
            inv::iotlb_domain(3, inv::Drain::default()),
        ],
    );
    let token = token.unwrap();
    let d = match d.destroy(&r.mem, &mut r.alloc, &r.queue, token) {
        Err((d, Error::InvalidationNotComplete)) => d,
        other => panic!("destroy before completion: {:?}", other.err().map(|e| e.1)),
    };
    r.run_hw(usize::MAX);
    assert_eq!(
        r.dma(dev, 0x8000_1000, Access::Read),
        Err(Fault::ContextNotPresent)
    );
    d.destroy(&r.mem, &mut r.alloc, &r.queue, token).unwrap();
    // Only the root table and the bus-0 context table remain allocated.
    assert_eq!(r.alloc.live.len(), frames_before + 1);
}

#[test]
fn root_table_initialisation_never_exposes_a_present_entry() {
    let mut mem = ArrayMem::new();
    let mut alloc = TestAlloc::new();
    // The frame the allocator hands out first is full of entries with P = 1.
    for i in 0..512 {
        mem.write_u64(POOL_BASE + i * 8, 0xffff_f001);
    }
    let violations = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = violations.clone();
    mem.set_observer(Some(Box::new(move |read, pa| {
        if read(pa & !0xf) & 1 != 0 {
            sink.borrow_mut().push(pa);
        }
    })));
    let root = RootTable::new(&mut mem, &mut alloc, 39).unwrap();
    assert_eq!(root.address(), POOL_BASE);
    assert!(
        violations.borrow().is_empty(),
        "present root entry after writes at {:x?}",
        violations.borrow()
    );
    mem.set_observer(None);
    assert!(mem.frame(POOL_BASE).iter().all(|w| *w == 0));
}
