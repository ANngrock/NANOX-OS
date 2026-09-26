//! M9 SMP logic that can be verified on the host: CPU topology, the AP
//! start-up state machine, per-CPU virtual layout, an IRQ-aware ticket
//! spinlock and the TLB shootdown protocol (docs/specs/M9-HARDWARE.md §3.3).
//!
//! The crate never touches hardware. Every machine effect (sending an IPI,
//! reading a clock, masking interrupts, flushing the local TLB) goes through a
//! trait that the caller implements: the kernel with real LAPIC/CR3/`invlpg`
//! code, the host tests with `std::thread` models of CPUs.
//!
//! `unsafe` is confined to [`lock`], where the protected value lives in an
//! `UnsafeCell`; every other module is `#![forbid(unsafe_code)]`.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks)]

pub mod ap_start;
pub mod lock;
pub mod mask;
pub mod percpu;
pub mod shootdown;
pub mod topology;

pub use ap_start::{
    ApOutcome, ApStartFsm, ApStartPlan, ApStatus, ApicOps, IpiNotDelivered, StartError,
    StartFailure, StartSummary, StartTiming, TrampolineError, TrampolinePage,
};
pub use lock::{IrqControl, LockError, TicketGuard, TicketLock, TryLockError};
pub use mask::{CpuMask, InvalidCpu};
pub use percpu::{CpuRegions, LayoutError, LayoutField, PerCpuLayout, PerCpuSpec, VaRange};
pub use shootdown::{
    FlushRequest, InFlight, LocalTlb, RetiredFrame, ShootdownComplete, ShootdownDomain,
    ShootdownError, ShootdownOps,
};
pub use topology::{build_topology, ApicEntry, CpuInfo, CpuPresence, Topology, TopologyError};

/// Hard upper bound on logical CPUs handled by this crate. It is the width of
/// [`CpuMask`]; topologies and shootdown domains never exceed it.
pub const MAX_CPUS: usize = 256;

/// Size of a 4 KiB page, the granule of every layout and flush in this crate.
pub const PAGE_SIZE: u64 = 4096;
