//! NVMe controller driver core for NANOX M9 (`docs/specs/M9-HARDWARE.md`).
//!
//! The crate is OS-independent. It never touches hardware itself:
//!
//! * controller registers (BAR0) are accessed through [`Mmio`];
//! * DMA memory (queues, identify data, PRP lists, data buffers) through
//!   [`DmaMemory`], addressed by the physical (bus) address the device uses;
//! * time through [`Clock`].
//!
//! The caller (kernel or test model) implements all three on one object
//! (see [`Hardware`]) and hands in the physical addresses of the memory the
//! driver may use ([`Config`]). No allocation, no `unsafe`.
//!
//! Layers:
//!
//! * [`regs`] — register layout, CAP/VS/CC/CSTS decoding, doorbells;
//! * [`status`] — completion status field (SCT/SC/CRD/More/DNR) and classes;
//! * [`command`] — 64-byte submission entries, 16-byte completion entries,
//!   builders for the commands the driver issues;
//! * [`identify`] — Identify Controller / Namespace parsing;
//! * [`prp`] — PRP entries and PRP lists with page-boundary and size checks;
//! * [`queue`] — submission/completion rings with phase tags and the
//!   command-identifier tracker;
//! * [`controller`] — initialisation state machine, I/O submission and
//!   completion, command timeouts (abort, then controller reset), controller
//!   reset with in-flight commands, shutdown.
//!
//! Section numbers refer to the NVM Express Base Specification 2.0 unless
//! another document is named; numbers marked `(?)` were written from memory
//! and are not verified against the document text.

#![no_std]
#![forbid(unsafe_code)]

pub mod command;
pub mod controller;
pub mod identify;
pub mod prp;
pub mod queue;
pub mod regs;
pub mod status;

pub use command::{Command, CompletionEntry};
pub use controller::{
    AdminStep, Completion, Config, Controller, Outcome, Phase, Progress, QueueMemory, Request,
    ResetReason,
};
pub use identify::{ControllerInfo, IdentifyError, LbaFormat, NamespaceInfo};
pub use prp::{DataBuffer, Prp, PrpError, PrpList};
pub use queue::CompletionError;
pub use regs::{CapError, Capabilities, ControllerStatus, ShutdownStatus, Version};
pub use status::{Status, StatusClass, StatusCodeType};

/// log2 of the host memory page size used for CC.MPS and PRPs (4 KiB).
pub const PAGE_SHIFT: u32 = 12;
/// Host memory page size used for CC.MPS and PRPs.
pub const PAGE_SIZE: u64 = 1 << PAGE_SHIFT;
/// Exclusive upper bound for every physical address handed to the
/// controller: the x86-64 architectural limit (52 bits), as in the other M9
/// crates. Queue, Identify and PRP regions must end at or below it.
pub const PHYS_ADDRESS_LIMIT: u64 = 1 << 52;

/// Access to the controller's register window (BAR0).
///
/// Offsets are relative to the start of BAR0 and naturally aligned. The
/// implementation must perform uncached, non-merged accesses in program
/// order. A 64-bit access may be split into two 32-bit accesses, low half
/// first (NVMe over PCIe allows either).
pub trait Mmio {
    /// Reads the 32-bit register at `offset`.
    fn read32(&mut self, offset: u32) -> u32;
    /// Writes the 32-bit register at `offset`.
    fn write32(&mut self, offset: u32, value: u32);
    /// Reads the 64-bit register at `offset`.
    fn read64(&mut self, offset: u32) -> u64;
    /// Writes the 64-bit register at `offset`.
    fn write64(&mut self, offset: u32, value: u64);
}

/// Access to DMA-visible memory by physical (bus) address.
///
/// Ordering contract the driver relies on:
///
/// * every [`DmaMemory::write`] is visible to the device before any later
///   [`Mmio`] write (a doorbell) — on x86 with write-back memory a compiler
///   fence suffices, elsewhere a write barrier;
/// * a [`DmaMemory::read`] is not satisfied before an earlier read that
///   observed a new phase tag (acquire ordering between the two reads).
///
/// The driver only accesses addresses it was given in [`Config`] or in a
/// [`DataBuffer`]/[`PrpList`].
pub trait DmaMemory {
    /// Copies `buf.len()` bytes starting at `phys` into `buf`.
    fn read(&mut self, phys: u64, buf: &mut [u8]);
    /// Copies `data` to memory starting at `phys`.
    fn write(&mut self, phys: u64, data: &[u8]);
}

/// Monotonic time source.
pub trait Clock {
    /// Current time in nanoseconds; never decreases. Busy-wait loops in
    /// the driver call this on every iteration, so the implementation may
    /// also relax the CPU here.
    fn now_ns(&mut self) -> u64;
}

/// Everything the driver needs from the platform.
pub trait Hardware: Mmio + DmaMemory + Clock {}

impl<T: Mmio + DmaMemory + Clock + ?Sized> Hardware for T {}

/// Driver error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A register read returned all ones: the function is gone or BAR0 is
    /// not decoded.
    DeviceGone,
    /// CAP holds a value the specification does not allow.
    InvalidCapabilities(CapError),
    /// The controller needs a feature this driver does not implement.
    Unsupported(Unsupported),
    /// Caller configuration rejected before touching the device.
    Config(ConfigError),
    /// A bounded wait expired.
    Timeout(TimeoutPhase),
    /// CSTS.CFS was observed while bringing the controller up.
    ControllerFatal,
    /// An admin command issued by the driver completed with an error.
    AdminCommand {
        /// Which command.
        step: AdminStep,
        /// Status returned by the controller.
        status: Status,
    },
    /// Identify data failed validation.
    Identify(IdentifyError),
    /// The configured namespace ID is above Identify Controller NN.
    NamespaceNotFound,
    /// Namespace size or format changed across a controller reset.
    NamespaceChanged,
    /// A completion entry violated the protocol; it was consumed and dropped.
    InvalidCompletion(CompletionError),
    /// The operation is not valid in the current [`Phase`].
    NotReady,
    /// The controller must be reset ([`Controller::reset`]); outstanding
    /// commands stay tracked until then.
    NeedsReset(ResetReason),
    /// No free submission queue entry or command slot.
    QueueFull,
    /// Commands are still outstanding.
    Busy,
    /// Data buffer rejected.
    Buffer(PrpError),
    /// Zero blocks, more than 65536 blocks or LBA range outside the namespace.
    LbaRange,
    /// Transfer longer than [`Controller::max_transfer_bytes`].
    TransferTooLarge,
}

/// Features the driver requires and the controller lacks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unsupported {
    /// CAP.CSS does not include the NVM command set.
    CommandSet,
    /// CAP.MPSMIN is above 4 KiB.
    PageSize,
    /// VS major version is 0.
    Version,
    /// Identify SQES/CQES do not allow 64-byte/16-byte entries.
    EntrySize,
    /// The namespace format carries metadata.
    Metadata,
    /// One logical block does not fit in the largest transfer.
    LbaSize,
}

/// Rejected [`Config`] field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    /// Admin queue size outside 2..=4096.
    AdminEntries,
    /// I/O queue size outside 2..=CAP.MQES+1.
    IoEntries,
    /// A queue, identify buffer or PRP pool is not 4 KiB aligned.
    Alignment,
    /// A doorbell the driver uses lies outside BAR0.
    DoorbellOutsideBar,
    /// Namespace ID 0 or broadcast.
    Namespace,
    /// A timeout of zero.
    Timeout,
    /// Too many PRP list pages per command, or the PRP pool does not end at
    /// or below [`PHYS_ADDRESS_LIMIT`].
    PrpPool,
    /// A queue or the Identify buffer does not end at or below
    /// [`PHYS_ADDRESS_LIMIT`].
    Region,
}

/// Wait that expired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeoutPhase {
    /// CSTS.RDY did not clear after CC.EN = 0 (CAP.TO).
    Disable,
    /// CSTS.RDY did not set after CC.EN = 1 (CAP.TO).
    Enable,
    /// An admin command of the given step did not complete.
    Admin(AdminStep),
    /// CSTS.SHST did not reach "shutdown complete".
    Shutdown,
}

impl From<CapError> for Error {
    fn from(e: CapError) -> Self {
        Error::InvalidCapabilities(e)
    }
}

impl From<PrpError> for Error {
    fn from(e: PrpError) -> Self {
        Error::Buffer(e)
    }
}

impl From<IdentifyError> for Error {
    fn from(e: IdentifyError) -> Self {
        Error::Identify(e)
    }
}

impl From<CompletionError> for Error {
    fn from(e: CompletionError) -> Self {
        Error::InvalidCompletion(e)
    }
}
