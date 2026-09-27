//! Application-processor start-up as a hardware-independent state machine.
//!
//! Sequence per AP (Intel SDM Vol. 3A §9.4.4, "universal start-up"):
//! `INIT` → wait `init_delay` (10 ms) → `SIPI` → wait `sipi_delay` (200 µs)
//! → a second `SIPI` **only if the AP has not reported alive** → wait up to
//! `response_timeout` for the alive report. The outcome is recorded per CPU;
//! a non-responding AP is a [`StartFailure`], never a panic.
//!
//! APs are started one at a time: the trampoline page carries per-AP boot
//! parameters (stack, CR3, CPU number), so two APs must not run it at once.
//!
//! Trampoline rule: the SIPI vector is `phys >> 12`, so the page must be
//! 4 KiB aligned and below 1 MiB. This crate further requires
//! `0x1000 <= phys < 0xA0000` (vectors `0x01..=0x9F`): page 0 holds the
//! real-mode IVT/BDA and the SDM reserves vectors `0xA0..=0xBF`; the whole
//! `0xA0000..0x100000` range is VGA memory, option ROMs and system BIOS. The
//! caller must still pick a page that the firmware memory map reports as
//! conventional RAM (for example not the EBDA just below `0xA0000`).

#![forbid(unsafe_code)]

use crate::topology::{CpuPresence, Topology};
use crate::PAGE_SIZE;

/// An IPI could not be delivered (for example the ICR delivery-status bit
/// never cleared).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpiNotDelivered;

/// Machine effects needed to start APs, implemented by the caller.
pub trait ApicOps {
    /// Sends an INIT IPI (assert) to `apic_id`.
    fn send_init(&mut self, apic_id: u32) -> Result<(), IpiNotDelivered>;
    /// Sends a start-up IPI with `vector` (trampoline page number).
    fn send_sipi(&mut self, apic_id: u32, vector: u8) -> Result<(), IpiNotDelivered>;
    /// Monotonic tick counter; the unit is fixed by [`StartTiming`].
    fn now_ticks(&mut self) -> u64;
    /// Whether logical CPU `cpu` has reported alive from the trampoline.
    fn ap_alive(&mut self, cpu: usize) -> bool;
}

/// Lowest accepted trampoline address (page 0 is excluded).
pub const TRAMPOLINE_MIN: u64 = 0x1000;
/// Trampoline pages must lie below this address (vector `<= 0x9F`).
pub const TRAMPOLINE_END: u64 = 0xA_0000;

/// A validated trampoline page for the SIPI vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrampolinePage {
    phys: u64,
}

/// Why a trampoline address was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrampolineError {
    /// Not 4 KiB aligned.
    Misaligned(u64),
    /// Page 0 (real-mode IVT/BDA).
    PageZero,
    /// At or above `0xA0000` (reserved SIPI vectors, VGA/ROM/BIOS, or above
    /// 1 MiB where no SIPI vector can point).
    NotConventionalLowMemory(u64),
}

impl TrampolinePage {
    /// Validates `phys` against the rule in the module documentation.
    pub const fn new(phys: u64) -> Result<Self, TrampolineError> {
        if !phys.is_multiple_of(PAGE_SIZE) {
            return Err(TrampolineError::Misaligned(phys));
        }
        if phys < TRAMPOLINE_MIN {
            return Err(TrampolineError::PageZero);
        }
        if phys >= TRAMPOLINE_END {
            return Err(TrampolineError::NotConventionalLowMemory(phys));
        }
        Ok(Self { phys })
    }

    /// Physical address of the page.
    pub const fn phys(&self) -> u64 {
        self.phys
    }

    /// SIPI vector (`phys >> 12`), always in `0x01..=0x9F`.
    pub const fn vector(&self) -> u8 {
        (self.phys >> 12) as u8
    }
}

/// Delays of the start-up sequence, in [`ApicOps::now_ticks`] units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartTiming {
    /// INIT → first SIPI.
    pub init_delay: u64,
    /// First SIPI → alive check / second SIPI.
    pub sipi_delay: u64,
    /// Second SIPI → give up.
    pub response_timeout: u64,
}

impl StartTiming {
    /// INIT → SIPI delay required by the SDM.
    pub const INIT_DELAY_US: u64 = 10_000;
    /// SIPI → SIPI delay required by the SDM.
    pub const SIPI_DELAY_US: u64 = 200;
    /// Default wait for the alive report after the last SIPI (1 s, generous
    /// for firmware-slow parts; only non-responding APs pay it).
    pub const RESPONSE_TIMEOUT_US: u64 = 1_000_000;

    /// SDM delays for a clock ticking `ticks_per_us` times per microsecond.
    /// `None` if the rate is zero or a delay overflows `u64`.
    pub const fn from_ticks_per_us(ticks_per_us: u64) -> Option<Self> {
        if ticks_per_us == 0 {
            return None;
        }
        let (Some(init_delay), Some(sipi_delay), Some(response_timeout)) = (
            Self::INIT_DELAY_US.checked_mul(ticks_per_us),
            Self::SIPI_DELAY_US.checked_mul(ticks_per_us),
            Self::RESPONSE_TIMEOUT_US.checked_mul(ticks_per_us),
        ) else {
            return None;
        };
        Some(Self {
            init_delay,
            sipi_delay,
            response_timeout,
        })
    }
}

/// Why an AP did not start. Recorded, never panicked on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartFailure {
    /// `ap_alive` was already true before INIT: a stale flag or a CPU running
    /// unknown code. No IPI was sent.
    AliveBeforeInit,
    /// `ap_alive` became true before any SIPI; an AP cannot run before SIPI,
    /// so the handshake is not trusted.
    AliveBeforeSipi,
    /// INIT could not be delivered.
    InitNotDelivered,
    /// A SIPI could not be delivered.
    SipiNotDelivered,
    /// No alive report within `response_timeout` after the second SIPI.
    NoResponse,
    /// `now_ticks` went backwards; timing guarantees are void.
    ClockWentBackwards,
}

/// Final result for one AP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApOutcome {
    /// The AP reported alive.
    Started {
        /// SIPIs sent (1 or 2).
        sipis: u8,
        /// Ticks from INIT to the alive observation.
        ticks: u64,
    },
    /// The AP did not start.
    Failed(StartFailure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    InitSent { at: u64 },
    Sipi1Sent { at: u64 },
    Sipi2Sent { at: u64 },
    Done(ApOutcome),
}

/// State machine for starting a single AP. [`begin`](Self::begin) performs the
/// pre-check and INIT; [`poll`](Self::poll) advances by at most one IPI per
/// call and returns the outcome once final.
#[derive(Clone, Copy, Debug)]
pub struct ApStartFsm {
    cpu: usize,
    apic_id: u32,
    vector: u8,
    timing: StartTiming,
    init_at: u64,
    phase: Phase,
}

impl ApStartFsm {
    /// Checks that `cpu` is not already alive, then sends INIT.
    pub fn begin<O: ApicOps>(
        ops: &mut O,
        cpu: usize,
        apic_id: u32,
        trampoline: TrampolinePage,
        timing: StartTiming,
    ) -> Self {
        let mut init_at = 0;
        let phase = if ops.ap_alive(cpu) {
            Phase::Done(ApOutcome::Failed(StartFailure::AliveBeforeInit))
        } else if ops.send_init(apic_id).is_err() {
            Phase::Done(ApOutcome::Failed(StartFailure::InitNotDelivered))
        } else {
            init_at = ops.now_ticks();
            Phase::InitSent { at: init_at }
        };
        Self {
            cpu,
            apic_id,
            vector: trampoline.vector(),
            timing,
            init_at,
            phase,
        }
    }

    /// Final outcome, if reached.
    pub fn outcome(&self) -> Option<ApOutcome> {
        match self.phase {
            Phase::Done(outcome) => Some(outcome),
            _ => None,
        }
    }

    /// Advances the machine; returns the outcome once final.
    pub fn poll<O: ApicOps>(&mut self, ops: &mut O) -> Option<ApOutcome> {
        let at = match self.phase {
            Phase::Done(outcome) => return Some(outcome),
            Phase::InitSent { at } | Phase::Sipi1Sent { at } | Phase::Sipi2Sent { at } => at,
        };
        let now = ops.now_ticks();
        if now < at {
            return self.finish(ApOutcome::Failed(StartFailure::ClockWentBackwards));
        }
        let elapsed = now - at;
        let alive = ops.ap_alive(self.cpu);
        match self.phase {
            Phase::InitSent { .. } => {
                if alive {
                    return self.finish(ApOutcome::Failed(StartFailure::AliveBeforeSipi));
                }
                if elapsed >= self.timing.init_delay {
                    return self.sipi(ops, |at| Phase::Sipi1Sent { at });
                }
            }
            Phase::Sipi1Sent { .. } => {
                if alive {
                    return self.started(1, now);
                }
                if elapsed >= self.timing.sipi_delay {
                    return self.sipi(ops, |at| Phase::Sipi2Sent { at });
                }
            }
            Phase::Sipi2Sent { .. } => {
                if alive {
                    return self.started(2, now);
                }
                if elapsed >= self.timing.response_timeout {
                    return self.finish(ApOutcome::Failed(StartFailure::NoResponse));
                }
            }
            Phase::Done(_) => {}
        }
        None
    }

    fn sipi<O: ApicOps>(&mut self, ops: &mut O, next: fn(u64) -> Phase) -> Option<ApOutcome> {
        if ops.send_sipi(self.apic_id, self.vector).is_err() {
            return self.finish(ApOutcome::Failed(StartFailure::SipiNotDelivered));
        }
        self.phase = next(ops.now_ticks());
        None
    }

    fn started(&mut self, sipis: u8, now: u64) -> Option<ApOutcome> {
        self.finish(ApOutcome::Started {
            sipis,
            ticks: now - self.init_at,
        })
    }

    fn finish(&mut self, outcome: ApOutcome) -> Option<ApOutcome> {
        self.phase = Phase::Done(outcome);
        Some(outcome)
    }
}

/// Start-up state of one logical CPU in an [`ApStartPlan`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApStatus {
    /// Logical CPU 0; already running.
    Bsp,
    /// Online-capable slot; not started at boot.
    Absent,
    /// Enabled AP not yet attempted.
    NotStarted,
    /// Last attempt succeeded.
    Started {
        /// SIPIs sent.
        sipis: u8,
        /// Ticks from INIT to alive.
        ticks: u64,
    },
    /// Last attempt failed (may be retried).
    Failed(StartFailure),
}

impl From<ApOutcome> for ApStatus {
    fn from(outcome: ApOutcome) -> Self {
        match outcome {
            ApOutcome::Started { sipis, ticks } => Self::Started { sipis, ticks },
            ApOutcome::Failed(f) => Self::Failed(f),
        }
    }
}

/// Precondition errors of [`ApStartPlan`]; no IPI is sent when returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartError {
    /// The status buffer is shorter than the topology.
    StatusBufferTooSmall {
        /// Required length.
        needed: usize,
    },
    /// No such logical CPU.
    InvalidCpu(usize),
    /// Logical CPU 0 is the running BSP.
    IsBsp,
    /// The CPU is online capable only (not enabled by firmware).
    NotEnabled(usize),
    /// The CPU was already started successfully.
    AlreadyStarted(usize),
}

/// Counts from [`ApStartPlan::start_all`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StartSummary {
    /// APs attempted in this call.
    pub attempted: usize,
    /// Of those, started.
    pub started: usize,
    /// Of those, failed.
    pub failed: usize,
}

/// Starts the enabled APs of a topology one at a time and keeps a per-CPU
/// report in a caller buffer.
#[derive(Debug)]
pub struct ApStartPlan<'t, 's> {
    topology: Topology<'t>,
    trampoline: TrampolinePage,
    timing: StartTiming,
    status: &'s mut [ApStatus],
}

impl<'t, 's> ApStartPlan<'t, 's> {
    /// Prepares the report: BSP, absent (online-capable) and not-started CPUs.
    pub fn new(
        topology: Topology<'t>,
        trampoline: TrampolinePage,
        timing: StartTiming,
        status: &'s mut [ApStatus],
    ) -> Result<Self, StartError> {
        let needed = topology.len();
        let Some(status) = status.get_mut(..needed) else {
            return Err(StartError::StatusBufferTooSmall { needed });
        };
        for (i, (slot, cpu)) in status.iter_mut().zip(topology.cpus()).enumerate() {
            *slot = match (i, cpu.presence) {
                (0, _) => ApStatus::Bsp,
                (_, CpuPresence::OnlineCapable) => ApStatus::Absent,
                (_, CpuPresence::Enabled) => ApStatus::NotStarted,
            };
        }
        Ok(Self {
            topology,
            trampoline,
            timing,
            status,
        })
    }

    /// Per-CPU report, indexed by logical CPU.
    pub fn status(&self) -> &[ApStatus] {
        self.status
    }

    /// Starts one AP, blocking until its outcome is final. Retrying a failed
    /// CPU is allowed; restarting a started one is an error.
    pub fn start_cpu<O: ApicOps>(
        &mut self,
        ops: &mut O,
        cpu: usize,
    ) -> Result<ApOutcome, StartError> {
        let info = *self.topology.cpu(cpu).ok_or(StartError::InvalidCpu(cpu))?;
        if cpu == 0 {
            return Err(StartError::IsBsp);
        }
        if info.presence != CpuPresence::Enabled {
            return Err(StartError::NotEnabled(cpu));
        }
        if let ApStatus::Started { .. } = self.status[cpu] {
            return Err(StartError::AlreadyStarted(cpu));
        }
        let mut fsm = ApStartFsm::begin(ops, cpu, info.apic_id, self.trampoline, self.timing);
        let outcome = loop {
            if let Some(outcome) = fsm.poll(ops) {
                break outcome;
            }
            core::hint::spin_loop();
        };
        self.status[cpu] = outcome.into();
        Ok(outcome)
    }

    /// Attempts every enabled AP that is not started yet. Never fails as a
    /// whole: per-CPU results land in [`status`](Self::status).
    pub fn start_all<O: ApicOps>(&mut self, ops: &mut O) -> StartSummary {
        let mut summary = StartSummary::default();
        for cpu in 1..self.topology.enabled_count() {
            if let ApStatus::Started { .. } = self.status[cpu] {
                continue;
            }
            summary.attempted += 1;
            match self.start_cpu(ops, cpu) {
                Ok(ApOutcome::Started { .. }) => summary.started += 1,
                Ok(ApOutcome::Failed(_)) | Err(_) => summary.failed += 1,
            }
        }
        summary
    }
}
