//! DMA mapping lifecycle: every transition out of order is refused, the
//! buffer is only released after confirmed invalidation, and completion
//! tokens cannot be confused (earlier, stale, foreign, bogus values).

mod common;

use common::*;
use hw_iommu::amd::{cmd, AmdDomain, DeviceTable, Invalidator};
use hw_iommu::{
    Access, Bdf, CommandRing, DmaMapping, DmaState, DomainConfig, Error, InvalidationQueue,
    PagingLevels, Perms, PhysMem, PAGE_SIZE,
};

const PAGE: u64 = PAGE_SIZE;
const PHYS_BITS: u32 = 48;

struct Rig {
    mem: ArrayMem,
    alloc: TestAlloc,
    dt: DeviceTable,
    hw: AmdHw,
    queue: InvalidationQueue<Invalidator>,
    domain: AmdDomain,
    dev: Bdf,
}

impl Rig {
    fn new() -> Self {
        let mut mem = ArrayMem::new();
        let mut alloc = TestAlloc::new();
        let mut dt =
            DeviceTable::new(&mut mem, DEV_TABLE_BASE, DEV_TABLE_PAGES, PHYS_BITS).unwrap();
        let hw = AmdHw::new(dt.register_value());
        let ring = CommandRing::new(RING_BASE, 256).unwrap();
        let queue = InvalidationQueue::new(&mut mem, ring, STATUS_ADDR, 1, Invalidator).unwrap();
        let cfg = DomainConfig::new(PagingLevels::Four, 48, PHYS_BITS).unwrap();
        let domain = AmdDomain::new(&mut mem, &mut alloc, 11, cfg).unwrap();
        let dev = Bdf::new(0, 3, 0).unwrap();
        dt.attach(&mut mem, dev, &domain).unwrap();
        Self {
            mem,
            alloc,
            dt,
            hw,
            queue,
            domain,
            dev,
        }
    }

    fn map(&mut self, iova: u64, page: u64, pages: u64) -> DmaMapping<Buf> {
        let buf = Buf {
            phys: DMA_BASE + page * PAGE,
            size: pages * PAGE,
        };
        DmaMapping::map(
            &mut self.domain,
            &mut self.mem,
            &mut self.alloc,
            iova,
            buf,
            Perms::RW,
        )
        .unwrap()
    }

    fn run_hw(&mut self, budget: usize) {
        let head = self.hw.run_queue(&mut self.mem, self.queue.ring(), budget);
        self.queue.update_head(head).unwrap();
        self.queue.poll(&self.mem).unwrap();
    }

    fn dma(&mut self, iova: u64) -> Result<u64, Fault> {
        self.hw
            .translate(&self.mem, self.dev.raw(), iova, Access::Read)
    }
}

/// Every operation that must be refused in the mapping's current state,
/// checked to leave the state unchanged.
fn refuse_all_but(r: &mut Rig, m: &mut DmaMapping<Buf>, allowed: &[&str]) {
    let state = m.state();
    let check = |name: &str, res: Result<(), Error>, m: &DmaMapping<Buf>| {
        if !allowed.contains(&name) {
            assert!(res.is_err(), "{name} accepted in {state:?}");
            assert_eq!(m.state(), state, "{name} changed state after refusal");
        }
    };
    // Only probe operations that are refused; allowed ones would move on.
    if !allowed.contains(&"begin") {
        let res = m.begin_dma();
        check("begin", res, m);
    }
    if !allowed.contains(&"end") {
        let res = m.end_dma();
        check("end", res, m);
    }
    if !allowed.contains(&"quiesce") {
        let res = m.quiesce();
        check("quiesce", res, m);
    }
    if !allowed.contains(&"unmap") {
        let res = m.unmap(&mut r.domain, &mut r.mem);
        check("unmap", res, m);
    }
    if !allowed.contains(&"submit") {
        let tail = r.queue.ring().tail_offset();
        let res = m.submit_invalidation(&mut r.mem, &mut r.queue).map(|_| ());
        check("submit", res, m);
        assert_eq!(
            r.queue.ring().tail_offset(),
            tail,
            "refused submit wrote commands"
        );
    }
    if !allowed.contains(&"confirm") {
        let res = m.confirm_invalidation(&r.queue);
        check("confirm", res, m);
    }
    if !allowed.contains(&"release") {
        let res = m.release().map(|_| ());
        check("release", res, m);
    }
}

#[test]
fn every_out_of_order_transition_is_refused() {
    let mut r = Rig::new();
    let mut m = r.map(0x10_0000, 0, 2);
    assert_eq!(m.state(), DmaState::Mapped);
    refuse_all_but(&mut r, &mut m, &["begin", "quiesce"]);

    m.begin_dma().unwrap();
    m.begin_dma().unwrap();
    assert_eq!(m.state(), DmaState::InFlight(2));
    refuse_all_but(&mut r, &mut m, &["begin", "end", "quiesce"]);

    m.quiesce().unwrap();
    assert_eq!(m.state(), DmaState::Quiescing { outstanding: 2 });
    assert_eq!(
        m.unmap(&mut r.domain, &mut r.mem),
        Err(Error::DmaOutstanding)
    );
    refuse_all_but(&mut r, &mut m, &["end"]);
    m.end_dma().unwrap();
    m.end_dma().unwrap();
    assert_eq!(m.state(), DmaState::Quiescing { outstanding: 0 });
    refuse_all_but(&mut r, &mut m, &["unmap"]);

    m.unmap(&mut r.domain, &mut r.mem).unwrap();
    assert_eq!(m.state(), DmaState::Unmapped);
    refuse_all_but(&mut r, &mut m, &["submit"]);

    let token = m.submit_invalidation(&mut r.mem, &mut r.queue).unwrap();
    assert_eq!(m.state(), DmaState::InvalidationPending(token));
    refuse_all_but(&mut r, &mut m, &[]);

    r.run_hw(usize::MAX);
    m.confirm_invalidation(&r.queue).unwrap();
    assert_eq!(m.state(), DmaState::Invalidated);
    refuse_all_but(&mut r, &mut m, &["release"]);

    assert_eq!(
        m.release(),
        Ok(Buf {
            phys: DMA_BASE,
            size: 2 * PAGE
        })
    );
    assert_eq!(m.state(), DmaState::Freed);
    refuse_all_but(&mut r, &mut m, &[]);
}

#[test]
fn failed_map_returns_the_buffer_and_wrong_domain_is_refused() {
    let mut r = Rig::new();
    let _existing = r.map(0x10_0000, 0, 1);
    let dup = Buf {
        phys: DMA_BASE + 5 * PAGE,
        size: PAGE,
    };
    match DmaMapping::map(
        &mut r.domain,
        &mut r.mem,
        &mut r.alloc,
        0x10_0000,
        dup,
        Perms::R,
    ) {
        Err((buf, Error::AlreadyMapped { iova: 0x10_0000 })) => {
            assert_eq!(buf.phys, DMA_BASE + 5 * PAGE)
        }
        other => panic!("unexpected {:?}", other.map(|m| m.state())),
    }
    r.alloc.fail_after = Some(0);
    let far = Buf {
        phys: DMA_BASE,
        size: PAGE,
    };
    match DmaMapping::map(
        &mut r.domain,
        &mut r.mem,
        &mut r.alloc,
        1 << 40,
        far,
        Perms::R,
    ) {
        Err((buf, Error::OutOfFrames)) => assert_eq!(
            buf,
            Buf {
                phys: DMA_BASE,
                size: PAGE
            }
        ),
        other => panic!("unexpected {:?}", other.map(|m| m.state())),
    }
    r.alloc.fail_after = None;

    let mut other = AmdDomain::new(&mut r.mem, &mut r.alloc, 12, r.domain.config()).unwrap();
    let mut m = r.map(0x20_0000, 1, 1);
    m.quiesce().unwrap();
    assert_eq!(m.unmap(&mut other, &mut r.mem), Err(Error::WrongDomain));
    assert_eq!(m.state(), DmaState::Quiescing { outstanding: 0 });
    m.unmap(&mut r.domain, &mut r.mem).unwrap();
}

#[test]
fn earlier_completion_does_not_confirm_later_token() {
    let mut r = Rig::new();
    let mut a = r.map(0x10_0000, 0, 1);
    let mut b = r.map(0x20_0000, 1, 1);
    assert!(r.dma(0x20_0000).is_ok());
    for m in [&mut a, &mut b] {
        m.quiesce().unwrap();
        m.unmap(&mut r.domain, &mut r.mem).unwrap();
    }
    let ta = a.submit_invalidation(&mut r.mem, &mut r.queue).unwrap();
    let tb = b.submit_invalidation(&mut r.mem, &mut r.queue).unwrap();
    assert!(ta.seq() < tb.seq());
    // Only A's INVALIDATE_IOMMU_PAGES and COMPLETION_WAIT execute.
    r.run_hw(2);
    assert_eq!(r.mem.read_u64(STATUS_ADDR), ta.seq());
    a.confirm_invalidation(&r.queue).unwrap();
    assert_eq!(
        b.confirm_invalidation(&r.queue),
        Err(Error::InvalidationNotComplete)
    );
    assert_eq!(b.release(), Err(Error::InvalidTransition));
    assert_eq!(
        r.dma(0x20_0000),
        Ok(DMA_BASE + PAGE),
        "B still cached: releasing it now would be unsafe"
    );
    r.run_hw(usize::MAX);
    b.confirm_invalidation(&r.queue).unwrap();
    assert_eq!(r.dma(0x20_0000), Err(Fault::NotPresent));
    assert!(a.release().is_ok() && b.release().is_ok());
}

#[test]
fn queue_reset_makes_pending_tokens_stale() {
    let mut r = Rig::new();
    let mut m = r.map(0x10_0000, 0, 1);
    assert!(r.dma(0x10_0000).is_ok());
    m.quiesce().unwrap();
    m.unmap(&mut r.domain, &mut r.mem).unwrap();
    let old = m.submit_invalidation(&mut r.mem, &mut r.queue).unwrap();
    // The IOMMU queue is re-initialised before executing anything.
    r.queue.reset(&mut r.mem);
    r.hw.reset_queue();
    assert_eq!(r.queue.is_complete(old), Err(Error::StaleToken));
    assert_eq!(m.confirm_invalidation(&r.queue), Err(Error::StaleToken));
    assert_eq!(m.release(), Err(Error::InvalidTransition));
    assert_eq!(r.dma(0x10_0000), Ok(DMA_BASE), "nothing was invalidated");

    m.resubmit_after_reset(&r.queue).unwrap();
    assert_eq!(m.state(), DmaState::Unmapped);
    let new = m.submit_invalidation(&mut r.mem, &mut r.queue).unwrap();
    assert!(new.seq() > old.seq() && new.epoch() != old.epoch());
    assert_eq!(
        m.resubmit_after_reset(&r.queue),
        Err(Error::InvalidTransition),
        "token is not stale"
    );
    r.run_hw(usize::MAX);
    m.confirm_invalidation(&r.queue).unwrap();
    assert_eq!(r.dma(0x10_0000), Err(Fault::NotPresent));
    assert!(m.release().is_ok());
}

#[test]
fn bogus_and_foreign_completions_are_rejected() {
    let mut r = Rig::new();
    let mut m = r.map(0x10_0000, 0, 1);
    m.quiesce().unwrap();
    m.unmap(&mut r.domain, &mut r.mem).unwrap();
    let token = m.submit_invalidation(&mut r.mem, &mut r.queue).unwrap();

    // A value never issued (corrupted status word).
    r.mem.write_u64(STATUS_ADDR, token.seq() + 5);
    assert_eq!(r.queue.poll(&r.mem), Err(Error::BogusCompletion));
    assert_eq!(r.queue.tracker().completed(), 0);
    assert_eq!(
        m.confirm_invalidation(&r.queue),
        Err(Error::InvalidationNotComplete)
    );
    r.mem.write_u64(STATUS_ADDR, 0);

    // A second IOMMU's queue whose own wait completed with the same value.
    let ring2 = CommandRing::new(RING_BASE, 256).unwrap();
    let mut mem2 = ArrayMem::new();
    let mut q2 = InvalidationQueue::new(&mut mem2, ring2, STATUS_ADDR_2, 2, Invalidator).unwrap();
    let t2 = q2.submit(&mut mem2, &[cmd::invalidate_all()]).unwrap();
    assert_eq!(t2.seq(), token.seq());
    mem2.write_u64(STATUS_ADDR_2, t2.seq());
    q2.poll(&mem2).unwrap();
    assert_eq!(m.confirm_invalidation(&q2), Err(Error::ForeignToken));
    assert_eq!(q2.is_complete(token), Err(Error::ForeignToken));

    // A head pointer beyond the tail.
    assert_eq!(
        r.queue.update_head(r.queue.ring().tail_offset() + 16),
        Err(Error::BogusCompletion)
    );

    r.run_hw(usize::MAX);
    m.confirm_invalidation(&r.queue).unwrap();
}

#[test]
fn full_queue_refuses_without_side_effects() {
    let mut r = Rig::new();
    let mut m = r.map(0x10_0000, 0, 1);
    m.quiesce().unwrap();
    m.unmap(&mut r.domain, &mut r.mem).unwrap();
    // 255 free slots; each submission takes a command and a wait.
    let mut last = None;
    while r.queue.ring().free_slots() >= 2 {
        last = Some(
            r.queue
                .submit(&mut r.mem, &[cmd::invalidate_all()])
                .unwrap(),
        );
    }
    let issued = r.queue.tracker().issued();
    let tail = r.queue.ring().tail_offset();
    assert_eq!(
        m.submit_invalidation(&mut r.mem, &mut r.queue),
        Err(Error::QueueFull)
    );
    assert_eq!(m.state(), DmaState::Unmapped);
    assert_eq!(
        (r.queue.tracker().issued(), r.queue.ring().tail_offset()),
        (issued, tail)
    );

    r.run_hw(usize::MAX);
    assert!(r.queue.is_complete(last.unwrap()).unwrap());
    m.submit_invalidation(&mut r.mem, &mut r.queue).unwrap();
    r.run_hw(usize::MAX);
    m.confirm_invalidation(&r.queue).unwrap();
    // Unused device table handle kept alive to document the setup.
    assert!(r.dt.entries() >= 256);
}

#[test]
fn token_beyond_issued_window_is_bogus() {
    // Two queues misconfigured with the same id: a token of one that the
    // other never issued is not a completion either may vouch for.
    let mut mem = ArrayMem::new();
    let ring = CommandRing::new(RING_BASE, 256).unwrap();
    let mut q1 = InvalidationQueue::new(&mut mem, ring, STATUS_ADDR, 7, Invalidator).unwrap();
    let mut mem2 = ArrayMem::new();
    let mut q2 = InvalidationQueue::new(&mut mem2, ring, STATUS_ADDR, 7, Invalidator).unwrap();
    let mut last = None;
    for _ in 0..3 {
        last = Some(q1.submit(&mut mem, &[cmd::invalidate_all()]).unwrap());
    }
    let t2 = q2.submit(&mut mem2, &[cmd::invalidate_all()]).unwrap();
    mem2.write_u64(STATUS_ADDR, t2.seq());
    q2.poll(&mem2).unwrap();
    assert_eq!(q2.is_complete(last.unwrap()), Err(Error::BogusCompletion));
    assert_eq!(q2.is_complete(t2), Ok(true));
}
