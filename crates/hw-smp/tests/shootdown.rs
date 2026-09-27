//! TLB shootdown on `std::thread` CPU models.
//!
//! Model: a shared page table (`pte`, one atomic frame id per virtual page),
//! a private software TLB per CPU thread that caches `pte` entries, and a
//! `freed` flag per frame. A CPU that translates through its TLB to a freed
//! frame is a use-after-free: the violation the protocol must prevent.

mod common;

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use hw_smp::{
    CpuMask, FlushRequest, LocalTlb, RetiredFrame, ShootdownDomain, ShootdownError, ShootdownOps,
    PAGE_SIZE,
};

const BASE: u64 = 0x4000_0000;
const DOMAIN: usize = 16;

fn va(idx: usize) -> u64 {
    BASE + idx as u64 * PAGE_SIZE
}

fn mask(cpus: &[usize]) -> CpuMask {
    let mut m = CpuMask::empty();
    for &c in cpus {
        m.insert(c).unwrap();
    }
    m
}

/// Software TLB of one CPU.
struct ModelTlb {
    entries: Vec<Option<u64>>,
    range_flushes: u64,
    full_flushes: u64,
}

impl ModelTlb {
    fn new(pages: usize) -> Self {
        Self {
            entries: vec![None; pages],
            range_flushes: 0,
            full_flushes: 0,
        }
    }
}

impl LocalTlb for ModelTlb {
    fn flush_range(&mut self, start: u64, pages: u64) {
        self.range_flushes += 1;
        let first = (start - BASE) / PAGE_SIZE;
        for p in first..first + pages {
            if let Some(e) = self.entries.get_mut(p as usize) {
                *e = None;
            }
        }
    }
    fn flush_all(&mut self) {
        self.full_flushes += 1;
        self.entries.iter_mut().for_each(|e| *e = None);
    }
}

/// Shared machine state for the stress model.
struct World {
    domain: ShootdownDomain<DOMAIN>,
    pte: Vec<AtomicU64>,
    freed: Vec<AtomicBool>,
    next_frame: AtomicU64,
    ipi: Vec<AtomicBool>,
    violations: AtomicU64,
    timeouts: AtomicU64,
    completed: AtomicU64,
    finished: AtomicUsize,
    epoch: Instant,
}

/// Hook run by the model inside a `ShootdownOps` call, i.e. at a point where
/// the protocol code is suspended, as it would be by an interrupt.
type Hook<'w> = Box<dyn FnMut() + 'w>;

struct Ops<'w> {
    ipi: &'w [AtomicBool],
    epoch: Instant,
    sent: Vec<usize>,
    polls: Option<Arc<AtomicUsize>>,
    on_ipi: Option<Hook<'w>>,
    on_tick: Option<Hook<'w>>,
}

impl<'w> Ops<'w> {
    fn new(ipi: &'w [AtomicBool], epoch: Instant) -> Self {
        Self {
            ipi,
            epoch,
            sent: Vec::new(),
            polls: None,
            on_ipi: None,
            on_tick: None,
        }
    }
}

impl ShootdownOps for Ops<'_> {
    fn send_ipi(&mut self, cpu: usize) {
        self.sent.push(cpu);
        self.ipi[cpu].store(true, Ordering::Release);
        if let Some(hook) = self.on_ipi.as_mut() {
            hook();
        }
    }
    fn now_ticks(&mut self) -> u64 {
        if let Some(polls) = &self.polls {
            polls.fetch_add(1, Ordering::AcqRel);
        }
        if let Some(hook) = self.on_tick.as_mut() {
            hook();
        }
        self.epoch.elapsed().as_micros() as u64
    }
}

fn ipi_flags(n: usize) -> Vec<AtomicBool> {
    (0..n).map(|_| AtomicBool::new(false)).collect()
}

/// Translate `idx` on a CPU and record a violation if the frame is freed.
fn access(world: &World, tlb: &mut ModelTlb, idx: usize) {
    let frame = match tlb.entries[idx] {
        Some(f) => f,
        None => {
            let f = world.pte[idx].load(Ordering::Acquire);
            tlb.entries[idx] = Some(f);
            f
        }
    };
    if world.freed[frame as usize].load(Ordering::Acquire) {
        world.violations.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Protocol {
    /// `shootdown` + `RetiredFrame::release` with the completion token.
    Correct,
    /// Negative control: publish, abandon the wait, free immediately.
    FreeWithoutWaiting,
}

const CPUS: usize = 8;
const VAS_PER_CPU: usize = 4;
const ROUNDS: usize = 200;

fn run_stress(protocol: Protocol, timeout_us: u64) -> Arc<World> {
    let pages = CPUS * VAS_PER_CPU;
    let frames = 1 + pages + CPUS * ROUNDS;
    let world = Arc::new(World {
        domain: ShootdownDomain::new(timeout_us),
        pte: (0..pages).map(|i| AtomicU64::new(1 + i as u64)).collect(),
        freed: (0..frames).map(|_| AtomicBool::new(false)).collect(),
        next_frame: AtomicU64::new(1 + pages as u64),
        ipi: ipi_flags(DOMAIN),
        violations: AtomicU64::new(0),
        timeouts: AtomicU64::new(0),
        completed: AtomicU64::new(0),
        finished: AtomicUsize::new(0),
        epoch: Instant::now(),
    });
    let online = Arc::new(Barrier::new(CPUS));
    let all = CpuMask::first_n(CPUS).unwrap();
    let handles: Vec<_> = (0..CPUS)
        .map(|me| {
            let (world, online) = (world.clone(), online.clone());
            thread::spawn(move || {
                let w = &*world;
                let mut tlb = ModelTlb::new(pages);
                let mut ops = Ops::new(&w.ipi, w.epoch);
                w.domain.mark_online(me, &mut tlb).unwrap();
                online.wait();
                let mut round = 0;
                let mut iter = 0u64;
                loop {
                    for idx in 0..pages {
                        access(w, &mut tlb, idx);
                    }
                    if w.ipi[me].swap(false, Ordering::AcqRel) {
                        w.domain.service(me, &mut tlb).unwrap();
                    }
                    if round < ROUNDS && iter % 3 == me as u64 % 3 {
                        let idx = me * VAS_PER_CPU + round % VAS_PER_CPU;
                        let new = w.next_frame.fetch_add(1, Ordering::Relaxed);
                        let old = w.pte[idx].swap(new, Ordering::AcqRel);
                        let retired = RetiredFrame::retire(&w.domain, me, va(idx), old);
                        let req = FlushRequest::range(va(idx), 1).unwrap();
                        match protocol {
                            Protocol::Correct => {
                                match w.domain.shootdown(me, req, all, &mut ops, &mut tlb) {
                                    Ok(done) => {
                                        assert_eq!(done.acked().count(), CPUS - 1);
                                        let f = retired.release(&done).expect("covering token");
                                        w.freed[f as usize].store(true, Ordering::Release);
                                        w.completed.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(ShootdownError::Timeout { .. }) => {
                                        // Frame leaks: never freed without proof.
                                        w.timeouts.fetch_add(1, Ordering::Relaxed);
                                    }
                                    Err(e) => panic!("cpu {me}: {e:?}"),
                                }
                            }
                            Protocol::FreeWithoutWaiting => {
                                let in_flight =
                                    w.domain.start(me, req, all, &mut ops, &mut tlb).unwrap();
                                drop(in_flight); // BROKEN: no wait for acknowledgements
                                w.freed[old as usize].store(true, Ordering::Release);
                            }
                        }
                        round += 1;
                        if round == ROUNDS {
                            w.finished.fetch_add(1, Ordering::AcqRel);
                        }
                    }
                    if round == ROUNDS && w.finished.load(Ordering::Acquire) == CPUS {
                        break;
                    }
                    iter += 1;
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    world
}

#[test]
fn stress_no_stale_translation_after_acknowledged_shootdown() {
    common::with_watchdog(300, || {
        let world = run_stress(Protocol::Correct, 10_000_000);
        let (v, t, c) = (
            world.violations.load(Ordering::Relaxed),
            world.timeouts.load(Ordering::Relaxed),
            world.completed.load(Ordering::Relaxed),
        );
        eprintln!("correct protocol: {c} shootdowns completed, {t} timeouts, {v} violations");
        assert_eq!(t, 0, "no timeouts expected with a 10 s limit");
        assert_eq!(c, (CPUS * ROUNDS) as u64);
        assert_eq!(v, 0, "a CPU used a freed frame through its TLB");
        assert_eq!(world.domain.generation(), (CPUS * ROUNDS) as u64);
    });
}

#[test]
fn negative_control_freeing_without_acknowledgements_is_detected() {
    common::with_watchdog(300, || {
        let world = run_stress(Protocol::FreeWithoutWaiting, 10_000_000);
        let v = world.violations.load(Ordering::Relaxed);
        eprintln!("broken protocol: {v} violations detected");
        assert!(v > 0, "the model failed to detect use-after-free");
    });
}

/// A target that never services: the initiator must time out, not hang.
#[test]
fn unresponsive_target_times_out() {
    common::with_watchdog(30, || {
        let domain = Arc::new(ShootdownDomain::<4>::new(100_000));
        let ipi = Arc::new(ipi_flags(4));
        let epoch = Instant::now();
        let stop = Arc::new(AtomicBool::new(false));
        let mut tlb0 = ModelTlb::new(8);
        let mut tlb1 = ModelTlb::new(8);
        domain.mark_online(0, &mut tlb0).unwrap();
        domain.mark_online(1, &mut tlb1).unwrap(); // online, but never services
        let ready = Arc::new(Barrier::new(2));
        let responder = {
            let (domain, ipi, stop, ready) =
                (domain.clone(), ipi.clone(), stop.clone(), ready.clone());
            thread::spawn(move || {
                let mut tlb = ModelTlb::new(8);
                domain.mark_online(2, &mut tlb).unwrap();
                ready.wait();
                while !stop.load(Ordering::Acquire) {
                    if ipi[2].swap(false, Ordering::AcqRel) {
                        domain.service(2, &mut tlb).unwrap();
                    }
                }
                tlb.range_flushes
            })
        };
        ready.wait();
        let mut ops = Ops::new(&ipi, epoch);
        let req = FlushRequest::range(va(1), 1).unwrap();
        let t0 = Instant::now();
        let err = domain
            .shootdown(0, req, mask(&[1, 2]), &mut ops, &mut tlb0)
            .unwrap_err();
        let waited = t0.elapsed();
        assert_eq!(
            err,
            ShootdownError::Timeout {
                generation: 1,
                unacked: mask(&[1])
            }
        );
        assert!(waited >= Duration::from_millis(100), "{waited:?}");
        // The slot was released: a shootdown without the dead CPU succeeds.
        let done = domain
            .shootdown(0, req, mask(&[2]), &mut ops, &mut tlb0)
            .unwrap();
        assert_eq!(done.generation(), 2);
        assert_eq!(done.acked(), mask(&[2]));
        stop.store(true, Ordering::Release);
        assert!(responder.join().unwrap() >= 1);
    });
}

/// Flush log entry of the hooked target: (range start or None for full, time).
type FlushLog = Arc<Mutex<Vec<(Option<u64>, Instant)>>>;

/// TLB of a target whose first armed flush blocks until `gate` opens and whose
/// second armed flush announces itself on `second_started` and then blocks
/// until `gate2` opens.
struct HookTlb {
    armed: bool,
    calls: u32,
    gate: Arc<AtomicBool>,
    second_started: Arc<AtomicBool>,
    gate2: Arc<AtomicBool>,
    log: FlushLog,
}

fn spin_until(flag: &AtomicBool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !flag.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "gate never opened");
        thread::yield_now();
    }
}

impl HookTlb {
    fn hook(&mut self, what: Option<u64>) {
        if !self.armed {
            return;
        }
        self.calls += 1;
        match self.calls {
            1 => spin_until(&self.gate),
            2 => {
                self.second_started.store(true, Ordering::Release);
                spin_until(&self.gate2);
            }
            _ => {}
        }
        self.log.lock().unwrap().push((what, Instant::now()));
    }
}

impl LocalTlb for HookTlb {
    fn flush_range(&mut self, start: u64, _pages: u64) {
        self.hook(Some(start));
    }
    fn flush_all(&mut self) {
        self.hook(None);
    }
}

/// A late acknowledgement of a timed-out generation must not complete the
/// next generation: completion requires the flush of the new request.
///
/// Sequencing is explicit, without sleeps. Generation 1 times out while the
/// target is blocked inside its flush (`gate`). The initiator opens `gate`
/// from its `send_ipi` of generation 2, i.e. after `pending[1] = 2` has been
/// published. The target then stores the stale ack of generation 1 and starts
/// the flush of generation 2, which sets `second_started` (after that ack) and
/// blocks on `gate2`. The initiator opens `gate2` only after 100 more polls
/// with the stale ack visible. An implementation that accepted the stale ack
/// would return during those polls, before B is flushed.
#[test]
fn stale_generation_ack_is_ignored() {
    common::with_watchdog(60, || {
        // 1 s real-time limit: generation 1 must expire, generation 2 must not.
        let domain = Arc::new(ShootdownDomain::<4>::new(1_000_000));
        let ipi = Arc::new(ipi_flags(4));
        let gate = Arc::new(AtomicBool::new(false));
        let second_started = Arc::new(AtomicBool::new(false));
        let gate2 = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let log: FlushLog = Arc::default();
        let ready = Arc::new(Barrier::new(2));
        let target = {
            let (domain, ipi, stop, ready) =
                (domain.clone(), ipi.clone(), stop.clone(), ready.clone());
            let mut tlb = HookTlb {
                armed: false,
                calls: 0,
                gate: gate.clone(),
                second_started: second_started.clone(),
                gate2: gate2.clone(),
                log: log.clone(),
            };
            thread::spawn(move || {
                domain.mark_online(1, &mut tlb).unwrap();
                tlb.armed = true;
                ready.wait();
                while !stop.load(Ordering::Acquire) {
                    if ipi[1].swap(false, Ordering::AcqRel) {
                        domain.service(1, &mut tlb).unwrap();
                    }
                }
            })
        };
        ready.wait();
        let mut tlb0 = ModelTlb::new(8);
        domain.mark_online(0, &mut tlb0).unwrap();
        let a = FlushRequest::range(va(1), 1).unwrap();
        let b = FlushRequest::range(va(5), 2).unwrap();

        let mut ops = Ops::new(&ipi, Instant::now());
        let err = domain
            .shootdown(0, a, mask(&[1]), &mut ops, &mut tlb0)
            .unwrap_err();
        assert_eq!(
            err,
            ShootdownError::Timeout {
                generation: 1,
                unacked: mask(&[1])
            }
        );
        assert!(log.lock().unwrap().is_empty(), "target still blocked in A");

        let polls_after_stale = Arc::new(AtomicUsize::new(0));
        let mut ops = Ops::new(&ipi, Instant::now());
        {
            let gate = gate.clone();
            ops.on_ipi = Some(Box::new(move || gate.store(true, Ordering::Release)));
            let (second_started, gate2, polls) = (
                second_started.clone(),
                gate2.clone(),
                polls_after_stale.clone(),
            );
            ops.on_tick = Some(Box::new(move || {
                if second_started.load(Ordering::Acquire)
                    && polls.fetch_add(1, Ordering::Relaxed) + 1 >= 100
                {
                    gate2.store(true, Ordering::Release);
                }
            }));
        }
        let result = domain.shootdown(0, b, mask(&[1]), &mut ops, &mut tlb0);
        let flushed: Vec<Option<u64>> = log.lock().unwrap().iter().map(|e| e.0).collect();
        gate2.store(true, Ordering::Release); // never leave the target blocked
        let done = result.unwrap();
        assert_eq!(done.generation(), 2);
        assert_eq!(done.acked(), mask(&[1]));
        assert_eq!(
            flushed,
            [Some(va(1)), Some(va(5))],
            "generation 2 completed before the target flushed B"
        );
        assert!(polls_after_stale.load(Ordering::Relaxed) >= 100);
        stop.store(true, Ordering::Release);
        target.join().unwrap();
    });
}

/// CPU 0 holds the slot with a request for CPU 1 while CPU 1 initiates a
/// shootdown targeting CPU 0. CPU 1 keeps servicing while it spins for the
/// slot, so neither waits for the other forever. A second initiation by the
/// slot holder itself is refused as re-entrant.
#[test]
fn initiators_targeting_each_other_do_not_deadlock() {
    common::with_watchdog(30, || {
        let domain = Arc::new(ShootdownDomain::<4>::new(5_000_000));
        let ipi = Arc::new(ipi_flags(4));
        let epoch = Instant::now();
        let online = Arc::new(Barrier::new(2));
        let go = Arc::new(Barrier::new(2));
        // Counts CPU 1's clock reads: one before its slot loop, one per turn.
        let cpu1_polls = Arc::new(AtomicUsize::new(0));
        let other = {
            let (domain, ipi, online, go, polls1) = (
                domain.clone(),
                ipi.clone(),
                online.clone(),
                go.clone(),
                cpu1_polls.clone(),
            );
            thread::spawn(move || {
                let mut tlb1 = ModelTlb::new(8);
                domain.mark_online(1, &mut tlb1).unwrap();
                online.wait();
                go.wait();
                let mut ops1 = Ops::new(&ipi, epoch);
                ops1.polls = Some(polls1);
                let req = FlushRequest::range(va(6), 1).unwrap();
                let r = domain.shootdown(1, req, mask(&[0]), &mut ops1, &mut tlb1);
                (r.map(|d| (d.generation(), d.acked())), tlb1.range_flushes)
            })
        };
        let mut tlb0 = ModelTlb::new(8);
        domain.mark_online(0, &mut tlb0).unwrap();
        online.wait();
        let mut ops0 = Ops::new(&ipi, epoch);
        let req = FlushRequest::range(va(2), 1).unwrap();
        let in_flight = domain
            .start(0, req, mask(&[1]), &mut ops0, &mut tlb0)
            .unwrap();
        assert_eq!(in_flight.generation(), 1);
        assert_eq!(in_flight.waiting(), mask(&[1]));
        assert!(matches!(
            domain.start(0, FlushRequest::All, CpuMask::empty(), &mut ops0, &mut tlb0),
            Err(ShootdownError::Reentrant)
        ));
        go.wait();
        // Wait until CPU 1 has completed at least one turn of its slot loop
        // (which services pending requests) before collecting acks. Only that
        // loop can acknowledge generation 1: CPU 1 has no other service path.
        let deadline = Instant::now() + Duration::from_secs(10);
        while cpu1_polls.load(Ordering::Acquire) < 2 {
            assert!(Instant::now() < deadline, "CPU 1 never spun for the slot");
            thread::yield_now();
        }
        let done = in_flight.wait(&mut ops0, &mut tlb0).unwrap();
        assert_eq!(
            done.acked(),
            mask(&[1]),
            "acked while spinning for the slot"
        );
        // Now answer CPU 1's shootdown, as an IPI handler would.
        let deadline = Instant::now() + Duration::from_secs(10);
        while !other.is_finished() {
            assert!(Instant::now() < deadline, "CPU 1 never completed");
            if ipi[0].swap(false, Ordering::AcqRel) {
                domain.service(0, &mut tlb0).unwrap();
            }
        }
        let (r, range_flushes) = other.join().unwrap();
        assert_eq!(r, Ok((2, mask(&[0]))));
        assert_eq!(range_flushes, 1, "CPU 1 flushed CPU 0's range once");
        assert_eq!(tlb0.range_flushes, 1, "CPU 1's request, serviced once");
    });
}

#[test]
fn slot_wait_is_bounded() {
    common::with_watchdog(30, || {
        let domain = Arc::new(ShootdownDomain::<4>::new(50_000));
        let ipi = Arc::new(ipi_flags(4));
        let epoch = Instant::now();
        let mut tlb0 = ModelTlb::new(8);
        let mut ops0 = Ops::new(&ipi, epoch);
        let held = domain
            .start(0, FlushRequest::All, CpuMask::empty(), &mut ops0, &mut tlb0)
            .unwrap();
        let d = domain.clone();
        let ipi2 = ipi.clone();
        let r = thread::spawn(move || {
            let mut tlb1 = ModelTlb::new(8);
            let mut ops1 = Ops::new(&ipi2, epoch);
            d.shootdown(1, FlushRequest::All, CpuMask::empty(), &mut ops1, &mut tlb1)
                .map(|c| c.generation())
        })
        .join()
        .unwrap();
        assert_eq!(r, Err(ShootdownError::SlotTimeout));
        drop(held); // abandoning releases the slot
        let done = domain
            .shootdown(1, FlushRequest::All, CpuMask::empty(), &mut ops0, &mut tlb0)
            .unwrap();
        assert_eq!(done.generation(), 2);
    });
}

#[test]
fn offline_cpus_are_skipped_and_flush_on_return() {
    let domain = ShootdownDomain::<4>::new(1_000);
    let ipi = ipi_flags(4);
    let mut ops = Ops::new(&ipi, Instant::now());
    let mut tlb0 = ModelTlb::new(8);
    let mut tlb1 = ModelTlb::new(8);
    tlb1.entries[3] = Some(99);
    domain.mark_online(0, &mut tlb0).unwrap();
    assert!(!domain.is_online(1));
    let done = domain
        .shootdown(
            0,
            FlushRequest::range(va(3), 1).unwrap(),
            mask(&[0, 1]),
            &mut ops,
            &mut tlb0,
        )
        .unwrap();
    assert_eq!(done.skipped_offline(), mask(&[1]));
    assert!(done.acked().is_empty());
    assert!(ops.sent.is_empty(), "no IPI to an offline CPU");
    assert_eq!(tlb0.range_flushes, 1, "initiator flushed itself directly");
    domain.mark_online(1, &mut tlb1).unwrap();
    assert_eq!(tlb1.entries[3], None, "coming online flushes everything");
    assert_eq!(tlb1.full_flushes, 1);

    // A CPU going offline answers what was already published to it.
    let in_flight = domain
        .start(
            0,
            FlushRequest::range(va(4), 1).unwrap(),
            mask(&[1]),
            &mut ops,
            &mut tlb0,
        )
        .unwrap();
    assert_eq!(ops.sent, [1]);
    domain.mark_offline(1, &mut tlb1).unwrap();
    assert_eq!(tlb1.range_flushes, 1);
    let done = in_flight.wait(&mut ops, &mut tlb0).unwrap();
    assert_eq!(done.acked(), mask(&[1]));
}

#[test]
fn retired_frame_needs_a_matching_later_completion() {
    let domain = ShootdownDomain::<4>::new(1_000);
    let other_domain = ShootdownDomain::<4>::new(1_000);
    let ipi = ipi_flags(4);
    let mut ops = Ops::new(&ipi, Instant::now());
    let mut tlb = ModelTlb::new(16);
    let none = CpuMask::empty();
    let early = domain
        .shootdown(0, FlushRequest::All, none, &mut ops, &mut tlb)
        .unwrap();

    let frame = RetiredFrame::retire(&domain, 0, va(3), 42u64);
    let frame = frame
        .release(&early)
        .expect_err("completion predates retirement");
    let wrong_range = domain
        .shootdown(
            0,
            FlushRequest::range(va(4), 4).unwrap(),
            none,
            &mut ops,
            &mut tlb,
        )
        .unwrap();
    let frame = frame.release(&wrong_range).expect_err("range misses va");
    let other_cpu = domain
        .shootdown(1, FlushRequest::All, none, &mut ops, &mut tlb)
        .unwrap();
    let frame = frame
        .release(&other_cpu)
        .expect_err("initiated by another CPU");
    let foreign = other_domain
        .shootdown(0, FlushRequest::All, none, &mut ops, &mut tlb)
        .unwrap();
    let frame = frame.release(&foreign).expect_err("other domain");
    let covering = domain
        .shootdown(
            0,
            FlushRequest::range(va(2), 2).unwrap(),
            none,
            &mut ops,
            &mut tlb,
        )
        .unwrap();
    assert_eq!(frame.release(&covering).ok(), Some(42));
}

#[test]
fn request_and_target_validation() {
    assert_eq!(
        FlushRequest::range(va(0) + 8, 1),
        Err(ShootdownError::InvalidRange)
    );
    assert_eq!(
        FlushRequest::range(va(0), 0),
        Err(ShootdownError::InvalidRange)
    );
    assert_eq!(
        FlushRequest::range(u64::MAX - PAGE_SIZE + 1, 2),
        Err(ShootdownError::InvalidRange)
    );
    let r = FlushRequest::range(va(2), 3).unwrap();
    assert!(r.covers(va(2)) && r.covers(va(4) + 5) && !r.covers(va(5)) && !r.covers(va(1)));
    assert!(FlushRequest::All.covers(0));

    let domain = ShootdownDomain::<4>::new(1_000);
    let ipi = ipi_flags(4);
    let mut ops = Ops::new(&ipi, Instant::now());
    let mut tlb = ModelTlb::new(8);
    assert!(matches!(
        domain.start(0, FlushRequest::All, mask(&[4]), &mut ops, &mut tlb),
        Err(ShootdownError::InvalidCpu(4))
    ));
    assert!(matches!(
        domain.start(4, FlushRequest::All, CpuMask::empty(), &mut ops, &mut tlb),
        Err(ShootdownError::InvalidCpu(4))
    ));
    let raw = FlushRequest::Range { start: 1, pages: 1 };
    assert!(matches!(
        domain.start(0, raw, CpuMask::empty(), &mut ops, &mut tlb),
        Err(ShootdownError::InvalidRange)
    ));
    assert_eq!(
        domain.generation(),
        0,
        "rejected requests use no generation"
    );
    assert_eq!(
        domain.service(9, &mut tlb),
        Err(ShootdownError::InvalidCpu(9))
    );
}

type NestedLog = Arc<Mutex<Vec<(Result<u64, ShootdownError>, Duration)>>>;

/// Hook that, the first time it runs, behaves like an interrupt handler on
/// CPU 0 calling `start` and records the result and how long it took.
fn nested_start_once<'w>(
    domain: &'w ShootdownDomain<4>,
    ipi: &'w [AtomicBool],
    epoch: Instant,
    log: NestedLog,
) -> Hook<'w> {
    let mut armed = true;
    Box::new(move || {
        if !armed {
            return;
        }
        armed = false;
        let t = Instant::now();
        let mut ops = Ops::new(ipi, epoch);
        let mut tlb = ModelTlb::new(8);
        let r = domain
            .start(0, FlushRequest::All, CpuMask::empty(), &mut ops, &mut tlb)
            .map(|in_flight| in_flight.generation());
        log.lock().unwrap().push((r, t.elapsed()));
    })
}

/// An interrupt on the initiating CPU that calls `start` again must get
/// `Reentrant` at once, wherever it lands: before the slot is acquired (hook
/// in the first `now_ticks`) or while the request is being published (hook in
/// `send_ipi`). Before the per-CPU flag existed, the first case was not
/// recognised (the nested call took the slot itself). The domain timeout is
/// 10 s, so an answer that waited for a timeout would be visible.
#[test]
fn nested_start_on_the_same_cpu_is_refused_immediately() {
    common::with_watchdog(60, || {
        let domain = ShootdownDomain::<4>::new(10_000_000);
        let ipi = ipi_flags(4);
        let epoch = Instant::now();
        let mut tlb0 = ModelTlb::new(8);
        let mut tlb1 = ModelTlb::new(8);
        domain.mark_online(0, &mut tlb0).unwrap();
        domain.mark_online(1, &mut tlb1).unwrap();

        let nested: NestedLog = Arc::default();
        let mut ops = Ops::new(&ipi, epoch);
        ops.on_tick = Some(nested_start_once(&domain, &ipi, epoch, nested.clone()));
        ops.on_ipi = Some(nested_start_once(&domain, &ipi, epoch, nested.clone()));
        let req = FlushRequest::range(va(3), 1).unwrap();
        let in_flight = domain
            .start(0, req, mask(&[1]), &mut ops, &mut tlb0)
            .unwrap();
        domain.service(1, &mut tlb1).unwrap(); // CPU 1 answers
        let done = in_flight.wait(&mut ops, &mut tlb0).unwrap();
        assert_eq!(done.generation(), 1, "nested calls used no generation");
        drop(ops);
        let nested = nested.lock().unwrap();
        assert_eq!(nested.len(), 2, "both interrupt points were exercised");
        for (r, took) in nested.iter() {
            assert_eq!(*r, Err(ShootdownError::Reentrant));
            assert!(*took < Duration::from_secs(1), "waited {took:?}");
        }
        // The flag is cleared when the outer shootdown ends.
        let mut plain = Ops::new(&ipi, epoch);
        let again = domain
            .shootdown(
                0,
                FlushRequest::All,
                CpuMask::empty(),
                &mut plain,
                &mut tlb0,
            )
            .unwrap();
        assert_eq!(again.generation(), 2);
    });
}
