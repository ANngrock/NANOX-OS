mod common;

use hw_smp::{
    build_topology, ApOutcome, ApStartFsm, ApStartPlan, ApStatus, ApicEntry, ApicOps, CpuInfo,
    IpiNotDelivered, StartError, StartFailure, StartTiming, Topology, TrampolineError,
    TrampolinePage,
};

/// Clock advances this many ticks (= µs with the timing used here) per read.
const STEP: u64 = 7;

#[derive(Clone, Copy, Debug)]
enum Behavior {
    /// Alive `latency` ticks after the `n`-th SIPI.
    AfterSipi {
        n: usize,
        latency: u64,
    },
    Never,
    AliveBeforeInit,
    AliveRightAfterInit,
    InitFails,
    SipiFails,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Init { apic: u32, at: u64 },
    Sipi { apic: u32, vector: u8, at: u64 },
}

/// Model of the local APIC and of the APs' trampoline handshake.
struct ModelApic {
    now: u64,
    apic_of_cpu: Vec<u32>,
    behavior: Vec<Behavior>,
    events: Vec<Event>,
}

impl ModelApic {
    fn new(topology: &Topology<'_>, default: Behavior) -> Self {
        Self {
            now: 1_000,
            apic_of_cpu: topology.cpus().iter().map(|c| c.apic_id).collect(),
            behavior: vec![default; topology.len()],
            events: Vec::new(),
        }
    }

    fn cpu_of(&self, apic: u32) -> usize {
        self.apic_of_cpu.iter().position(|&a| a == apic).unwrap()
    }

    fn sipi_times(&self, apic: u32) -> Vec<u64> {
        self.events
            .iter()
            .filter_map(|e| match *e {
                Event::Sipi { apic: a, at, .. } if a == apic => Some(at),
                _ => None,
            })
            .collect()
    }

    /// INIT resets the AP: only SIPIs after the latest INIT count.
    fn sipis_since_init(&self, apic: u32) -> Vec<u64> {
        let last_init = self
            .events
            .iter()
            .rposition(|e| matches!(*e, Event::Init { apic: a, .. } if a == apic));
        let Some(start) = last_init else {
            return Vec::new();
        };
        self.events[start..]
            .iter()
            .filter_map(|e| match *e {
                Event::Sipi { apic: a, at, .. } if a == apic => Some(at),
                _ => None,
            })
            .collect()
    }

    fn events_of(&self, apic: u32) -> Vec<Event> {
        self.events
            .iter()
            .copied()
            .filter(|e| match *e {
                Event::Init { apic: a, .. } | Event::Sipi { apic: a, .. } => a == apic,
            })
            .collect()
    }

    fn inited(&self, apic: u32) -> bool {
        self.events
            .iter()
            .any(|e| matches!(*e, Event::Init { apic: a, .. } if a == apic))
    }
}

impl ApicOps for ModelApic {
    fn send_init(&mut self, apic_id: u32) -> Result<(), IpiNotDelivered> {
        if let Behavior::InitFails = self.behavior[self.cpu_of(apic_id)] {
            return Err(IpiNotDelivered);
        }
        self.events.push(Event::Init {
            apic: apic_id,
            at: self.now,
        });
        Ok(())
    }

    fn send_sipi(&mut self, apic_id: u32, vector: u8) -> Result<(), IpiNotDelivered> {
        if let Behavior::SipiFails = self.behavior[self.cpu_of(apic_id)] {
            return Err(IpiNotDelivered);
        }
        self.events.push(Event::Sipi {
            apic: apic_id,
            vector,
            at: self.now,
        });
        Ok(())
    }

    fn now_ticks(&mut self) -> u64 {
        self.now += STEP;
        self.now
    }

    fn ap_alive(&mut self, cpu: usize) -> bool {
        let apic = self.apic_of_cpu[cpu];
        match self.behavior[cpu] {
            Behavior::AfterSipi { n, latency } => self
                .sipis_since_init(apic)
                .get(n - 1)
                .is_some_and(|&t| self.now >= t + latency),
            Behavior::AliveBeforeInit => true,
            Behavior::AliveRightAfterInit => self.inited(apic),
            Behavior::Never | Behavior::InitFails | Behavior::SipiFails => false,
        }
    }
}

fn timing() -> StartTiming {
    StartTiming::from_ticks_per_us(1).unwrap()
}

fn trampoline() -> TrampolinePage {
    TrampolinePage::new(0x8000).unwrap()
}

fn topology_from<'a>(rel: &str, buf: &'a mut [CpuInfo]) -> Topology<'a> {
    build_topology(common::madt_cpus(rel), 0, buf).unwrap()
}

#[test]
fn ap_answering_first_sipi_gets_exactly_one_sipi() {
    let mut buf = [CpuInfo::EMPTY; 4];
    let topo = topology_from("q35-smp4/APIC.bin", &mut buf);
    let mut apic = ModelApic::new(&topo, Behavior::AfterSipi { n: 1, latency: 50 });
    let mut status = [ApStatus::NotStarted; 4];
    let mut plan = ApStartPlan::new(topo, trampoline(), timing(), &mut status).unwrap();
    let summary = plan.start_all(&mut apic);
    assert_eq!(
        (summary.attempted, summary.started, summary.failed),
        (3, 3, 0)
    );
    assert_eq!(plan.status()[0], ApStatus::Bsp);
    for cpu in 1..4u32 {
        let ev = apic.events_of(cpu);
        let [Event::Init { at: init, .. }, Event::Sipi {
            vector, at: sipi, ..
        }] = ev[..]
        else {
            panic!("cpu {cpu}: unexpected sequence {ev:?}");
        };
        assert_eq!(vector, 0x08);
        assert!(sipi - init >= 10_000, "INIT->SIPI must be >= 10 ms");
        assert!(
            matches!(
                plan.status()[cpu as usize],
                ApStatus::Started { sipis: 1, .. }
            ),
            "{:?}",
            plan.status()[cpu as usize]
        );
    }
}

#[test]
fn ap_answering_only_second_sipi_gets_two_spaced_sipis() {
    let mut buf = [CpuInfo::EMPTY; 4];
    let topo = topology_from("q35-smp4/APIC.bin", &mut buf);
    let mut apic = ModelApic::new(&topo, Behavior::AfterSipi { n: 2, latency: 30 });
    let mut status = [ApStatus::NotStarted; 4];
    let mut plan = ApStartPlan::new(topo, trampoline(), timing(), &mut status).unwrap();
    let summary = plan.start_all(&mut apic);
    assert_eq!((summary.started, summary.failed), (3, 0));
    for cpu in 1..4u32 {
        let sipis = apic.sipi_times(cpu);
        assert_eq!(sipis.len(), 2, "cpu {cpu}");
        assert!(sipis[1] - sipis[0] >= 200, "SIPI->SIPI must be >= 200 us");
        assert!(matches!(
            plan.status()[cpu as usize],
            ApStatus::Started { sipis: 2, .. }
        ));
    }
}

#[test]
fn second_sipi_depends_on_response_within_sipi_delay() {
    let mut buf = [CpuInfo::EMPTY; 4];
    let topo = topology_from("q35-smp4/APIC.bin", &mut buf);
    let mut apic = ModelApic::new(&topo, Behavior::Never);
    apic.behavior[1] = Behavior::AfterSipi { n: 1, latency: 150 }; // inside 200 us
    apic.behavior[2] = Behavior::AfterSipi { n: 1, latency: 500 }; // after 200 us
    let mut status = [ApStatus::NotStarted; 4];
    let mut plan = ApStartPlan::new(topo, trampoline(), timing(), &mut status).unwrap();
    assert!(matches!(
        plan.start_cpu(&mut apic, 1),
        Ok(ApOutcome::Started { sipis: 1, .. })
    ));
    // A running AP ignores the extra SIPI; the report says two were sent.
    assert!(matches!(
        plan.start_cpu(&mut apic, 2),
        Ok(ApOutcome::Started { sipis: 2, .. })
    ));
    assert_eq!(apic.sipi_times(1).len(), 1);
    assert_eq!(apic.sipi_times(2).len(), 2);
}

#[test]
fn silent_ap_times_out_after_second_sipi() {
    let mut buf = [CpuInfo::EMPTY; 4];
    let topo = topology_from("q35-smp4/APIC.bin", &mut buf);
    let mut apic = ModelApic::new(&topo, Behavior::Never);
    let mut status = [ApStatus::NotStarted; 4];
    let t = timing();
    let mut plan = ApStartPlan::new(topo, trampoline(), t, &mut status).unwrap();
    assert_eq!(
        plan.start_cpu(&mut apic, 3),
        Ok(ApOutcome::Failed(StartFailure::NoResponse))
    );
    let sipis = apic.sipi_times(3);
    assert_eq!(sipis.len(), 2);
    assert!(apic.now - sipis[1] >= t.response_timeout);
    assert!(
        apic.now - sipis[1] < t.response_timeout + 10 * STEP,
        "gave up promptly"
    );
    assert_eq!(plan.status()[3], ApStatus::Failed(StartFailure::NoResponse));
}

#[test]
fn lenovo_mixed_outcomes_are_reported_per_cpu_without_panic() {
    let mut buf = [CpuInfo::EMPTY; 16];
    let topo = topology_from("lenovo-82k8/APIC.bin", &mut buf);
    let mut apic = ModelApic::new(&topo, Behavior::AfterSipi { n: 1, latency: 20 });
    apic.behavior[3] = Behavior::Never;
    apic.behavior[5] = Behavior::AliveBeforeInit;
    apic.behavior[7] = Behavior::AfterSipi { n: 2, latency: 20 };
    apic.behavior[9] = Behavior::InitFails;
    apic.behavior[12] = Behavior::SipiFails;
    apic.behavior[14] = Behavior::AliveRightAfterInit;
    let mut status = [ApStatus::NotStarted; 16];
    let mut plan = ApStartPlan::new(topo, trampoline(), timing(), &mut status).unwrap();
    let summary = plan.start_all(&mut apic);
    assert_eq!(
        (summary.attempted, summary.started, summary.failed),
        (15, 10, 5)
    );
    let st = plan.status();
    assert_eq!(st[3], ApStatus::Failed(StartFailure::NoResponse));
    assert_eq!(st[5], ApStatus::Failed(StartFailure::AliveBeforeInit));
    assert!(matches!(st[7], ApStatus::Started { sipis: 2, .. }));
    assert_eq!(st[9], ApStatus::Failed(StartFailure::InitNotDelivered));
    assert_eq!(st[12], ApStatus::Failed(StartFailure::SipiNotDelivered));
    assert_eq!(st[14], ApStatus::Failed(StartFailure::AliveBeforeSipi));
    assert!(
        apic.events_of(5).is_empty(),
        "no IPI to a CPU that is already alive"
    );
    assert_eq!(apic.events_of(14).len(), 1, "stopped after INIT");

    // One AP at a time: the IPIs of different APs never interleave.
    let mut seen = Vec::new();
    for e in &apic.events {
        let (Event::Init { apic, .. } | Event::Sipi { apic, .. }) = *e;
        if seen.last() != Some(&apic) {
            assert!(
                !seen.contains(&apic),
                "APIC {apic} resumed after another AP"
            );
            seen.push(apic);
        }
    }
}

#[test]
fn restart_rules() {
    let entries = [
        ApicEntry::from_madt_flags(0, 1),
        ApicEntry::from_madt_flags(1, 1),
        ApicEntry::from_madt_flags(2, 1),
        ApicEntry::from_madt_flags(3, 2), // online capable
    ];
    let mut buf = [CpuInfo::EMPTY; 4];
    let topo = build_topology(entries, 0, &mut buf).unwrap();
    let mut apic = ModelApic::new(&topo, Behavior::AfterSipi { n: 1, latency: 10 });
    apic.behavior[2] = Behavior::Never;
    let mut status = [ApStatus::NotStarted; 4];
    let mut plan = ApStartPlan::new(topo, trampoline(), timing(), &mut status).unwrap();
    assert_eq!(plan.status()[3], ApStatus::Absent);

    let s = plan.start_all(&mut apic);
    assert_eq!((s.attempted, s.started, s.failed), (2, 1, 1));
    assert_eq!(
        plan.status()[3],
        ApStatus::Absent,
        "online-capable not started"
    );

    let before = apic.events.len();
    assert_eq!(
        plan.start_cpu(&mut apic, 1),
        Err(StartError::AlreadyStarted(1))
    );
    assert_eq!(plan.start_cpu(&mut apic, 0), Err(StartError::IsBsp));
    assert_eq!(plan.start_cpu(&mut apic, 3), Err(StartError::NotEnabled(3)));
    assert_eq!(plan.start_cpu(&mut apic, 4), Err(StartError::InvalidCpu(4)));
    assert_eq!(apic.events.len(), before, "precondition errors send no IPI");

    // A failed CPU may be retried; start_all skips started CPUs. The model
    // forgets the old IPIs: otherwise the earlier SIPIs would make the AP look
    // alive before INIT, which the plan correctly refuses (AliveBeforeInit).
    apic.behavior[2] = Behavior::AfterSipi { n: 1, latency: 10 };
    apic.events.clear();
    let s = plan.start_all(&mut apic);
    assert_eq!((s.attempted, s.started, s.failed), (1, 1, 0));
    assert!(matches!(plan.status()[2], ApStatus::Started { .. }));
}

#[test]
fn status_buffer_must_cover_topology() {
    let mut buf = [CpuInfo::EMPTY; 4];
    let topo = topology_from("q35-smp4/APIC.bin", &mut buf);
    let mut status = [ApStatus::NotStarted; 3];
    assert_eq!(
        ApStartPlan::new(topo, trampoline(), timing(), &mut status).unwrap_err(),
        StartError::StatusBufferTooSmall { needed: 4 }
    );
}

#[test]
fn trampoline_page_rule() {
    assert_eq!(TrampolinePage::new(0x8000).unwrap().vector(), 0x08);
    assert_eq!(TrampolinePage::new(0x1000).unwrap().vector(), 0x01);
    assert_eq!(TrampolinePage::new(0x9F000).unwrap().vector(), 0x9F);
    assert_eq!(
        TrampolinePage::new(0x8800),
        Err(TrampolineError::Misaligned(0x8800))
    );
    assert_eq!(TrampolinePage::new(0), Err(TrampolineError::PageZero));
    for bad in [0xA0000, 0xB8000, 0xF0000, 0x100000, 0x1_0000_0000] {
        assert_eq!(
            TrampolinePage::new(bad),
            Err(TrampolineError::NotConventionalLowMemory(bad))
        );
    }
}

#[test]
fn timing_conversion() {
    let t = StartTiming::from_ticks_per_us(3).unwrap();
    assert_eq!(
        (t.init_delay, t.sipi_delay, t.response_timeout),
        (30_000, 600, 3_000_000)
    );
    assert_eq!(StartTiming::from_ticks_per_us(0), None);
    assert_eq!(StartTiming::from_ticks_per_us(u64::MAX / 1_000), None);
}

/// Scripted clock that jumps backwards after INIT.
struct BackwardsClock {
    reads: Vec<u64>,
}

impl ApicOps for BackwardsClock {
    fn send_init(&mut self, _: u32) -> Result<(), IpiNotDelivered> {
        Ok(())
    }
    fn send_sipi(&mut self, _: u32, _: u8) -> Result<(), IpiNotDelivered> {
        panic!("no SIPI may be sent after the clock went backwards");
    }
    fn now_ticks(&mut self) -> u64 {
        self.reads.remove(0)
    }
    fn ap_alive(&mut self, _: usize) -> bool {
        false
    }
}

#[test]
fn clock_going_backwards_fails_the_ap() {
    let mut ops = BackwardsClock {
        reads: vec![1_000_000, 10],
    };
    let mut fsm = ApStartFsm::begin(&mut ops, 1, 1, trampoline(), timing());
    assert_eq!(fsm.outcome(), None);
    let outcome = Some(ApOutcome::Failed(StartFailure::ClockWentBackwards));
    assert_eq!(fsm.poll(&mut ops), outcome);
    assert_eq!(fsm.poll(&mut ops), outcome, "final outcome is sticky");
}
