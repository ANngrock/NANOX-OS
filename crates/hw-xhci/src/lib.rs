//! xHCI (eXtensible Host Controller Interface) USB host controller driver
//! core for NANOX M9 (`docs/specs/M9-HARDWARE.md`).
//!
//! The target laptop (LENOVO 82K8) has two AMD xHCI controllers
//! (1022:1639) and no serial port, so USB is the future input path (HID
//! keyboard) and possibly a diagnostics path. This crate is the
//! OS-independent part of that driver:
//!
//! * [`regs`] — capability, operational, runtime and doorbell registers.
//! * [`extcap`] — extended capability list: USB Legacy Support (BIOS/OS
//!   handoff) and Supported Protocol (which ports are USB2 or USB3).
//! * [`port`] — PORTSC decoding and RW1C-safe write values.
//! * [`trb`] — TRB encoding/decoding and completion codes.
//! * [`ring`] — producer rings (command/transfer, Link TRB with Toggle
//!   Cycle) and the event ring consumer (cycle bit, ERDP).
//! * [`context`] — slot/endpoint/input-control contexts for 32- and
//!   64-byte context sizes (HCCPARAMS1.CSZ).
//! * [`descriptor`] — USB setup packets and descriptor parsing with length
//!   checks (device, configuration, interface, endpoint, HID,
//!   SuperSpeed endpoint companion).
//! * [`controller`] — the driver: initialization state machine with
//!   timeouts, command submission and abort, port reset, device
//!   enumeration (Enable Slot → Address Device → GET_DESCRIPTOR →
//!   Configure Endpoint), interrupt IN transfers and recovery (stall,
//!   disconnect, command timeout, host system error).
//!
//! The crate never touches hardware directly. Registers go through
//! [`Mmio`], DMA memory through [`DmaMemory`]/[`DmaAlloc`] and time
//! through [`Clock`]; the caller (kernel or test model) implements them,
//! usually on one object that then satisfies [`Hal`]. The driver is
//! single-threaded and polls; interrupts can be layered on top by calling
//! [`Controller::poll`] from the interrupt handler's bottom half.
//!
//! Spec references ("xHCI §x.y") are to the eXtensible Host Controller
//! Interface for Universal Serial Bus Requirements Specification,
//! revision 1.2 (May 2019). USB descriptor layouts follow USB 2.0 §9.6 and
//! USB 3.2 §9.6. References marked "(unverified)" were written from memory
//! of the document and should be checked against the text.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod context;
pub mod controller;
pub mod descriptor;
pub mod extcap;
pub mod port;
pub mod regs;
pub mod ring;
pub mod trb;

pub use controller::{
    Completion, Config, Controller, EnumeratedDevice, InitPhase, Notification, State, Stats,
    Timeouts, TransferId,
};
pub use descriptor::{DescError, SetupPacket};
pub use port::{LinkState, PortStatus, Speed};
pub use trb::{CompletionCode, Trb};

/// Register access to the controller's MMIO window (BAR0), implemented by
/// the caller.
///
/// Offsets are relative to the start of the window and always 4-byte
/// aligned (8-byte for the 64-bit accessors). Implementations must perform
/// uncached, non-elided accesses of exactly the requested width.
pub trait Mmio {
    /// Reads the 32-bit register at `offset`.
    fn read32(&mut self, offset: u32) -> u32;
    /// Writes the 32-bit register at `offset`.
    fn write32(&mut self, offset: u32, value: u32);
    /// Reads a 64-bit register. The default performs two 32-bit reads,
    /// low dword first (xHCI §5.1).
    fn read64(&mut self, offset: u32) -> u64 {
        let lo = u64::from(self.read32(offset));
        let hi = u64::from(self.read32(offset + 4));
        lo | hi << 32
    }
    /// Writes a 64-bit register. The default performs two 32-bit writes,
    /// low dword first (xHCI §5.1).
    fn write64(&mut self, offset: u32, value: u64) {
        self.write32(offset, value as u32);
        self.write32(offset + 4, (value >> 32) as u32);
    }
}

/// Coherent DMA memory shared with the controller, implemented by the
/// caller.
///
/// Addresses are the physical (bus) addresses the controller uses. Writes
/// must become visible to the device in program order before any later
/// [`Mmio`] write (doorbell), and reads must observe device writes; on
/// x86 with write-back coherent memory this means compiler barriers plus
/// the implicit ordering of UC MMIO. Accesses only target ranges obtained
/// from [`DmaAlloc`] or passed in by the caller.
pub trait DmaMemory {
    /// Reads `buf.len()` bytes starting at `pa`.
    fn read(&mut self, pa: u64, buf: &mut [u8]);
    /// Writes `data` starting at `pa`.
    fn write(&mut self, pa: u64, data: &[u8]);

    /// Reads a little-endian `u32`.
    fn read_u32(&mut self, pa: u64) -> u32 {
        let mut b = [0u8; 4];
        self.read(pa, &mut b);
        u32::from_le_bytes(b)
    }
    /// Writes a little-endian `u32`.
    fn write_u32(&mut self, pa: u64, value: u32) {
        self.write(pa, &value.to_le_bytes());
    }
    /// Reads a little-endian `u64`.
    fn read_u64(&mut self, pa: u64) -> u64 {
        let mut b = [0u8; 8];
        self.read(pa, &mut b);
        u64::from_le_bytes(b)
    }
    /// Writes a little-endian `u64`.
    fn write_u64(&mut self, pa: u64, value: u64) {
        self.write(pa, &value.to_le_bytes());
    }
    /// Zeroes `len` bytes starting at `pa`.
    fn fill_zero(&mut self, pa: u64, len: usize) {
        let zero = [0u8; 64];
        let mut done = 0usize;
        while done < len {
            let n = core::cmp::min(zero.len(), len - done);
            self.write(pa + done as u64, &zero[..n]);
            done += n;
        }
    }
}

/// Allocator of DMA-capable physically contiguous memory, implemented by
/// the caller.
///
/// The driver validates every returned block (alignment, 32-bit limit when
/// HCCPARAMS1.AC64 = 0) and hands a bad one straight back through
/// [`DmaAlloc::free`], reporting [`Error::BadDmaAddress`]. Blocks are
/// zeroed by the driver before the controller can see them.
pub trait DmaAlloc {
    /// Allocates `len` bytes aligned to `align` (a power of two) and
    /// returns the physical address.
    fn alloc(&mut self, len: usize, align: usize) -> Option<u64>;
    /// Frees a block previously returned by [`DmaAlloc::alloc`] with the
    /// same `len`.
    fn free(&mut self, pa: u64, len: usize);
}

/// Monotonic time source, implemented by the caller.
pub trait Clock {
    /// Current time in microseconds. Must never go backwards. Called in
    /// every iteration of the driver's polling loops.
    fn now_us(&mut self) -> u64;
}

/// Everything the driver needs from its environment.
pub trait Hal: Mmio + DmaMemory + DmaAlloc + Clock {}
impl<T: Mmio + DmaMemory + DmaAlloc + Clock + ?Sized> Hal for T {}

/// What a timed-out wait was waiting for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Wait {
    /// BIOS did not release the HC BIOS Owned semaphore (xHCI §7.1.1).
    BiosHandoff,
    /// USBSTS.HCH did not become 1 after clearing USBCMD.R/S.
    Halt,
    /// USBCMD.HCRST did not return to 0.
    Reset,
    /// USBSTS.CNR did not return to 0.
    ControllerNotReady,
    /// USBSTS.HCH did not become 0 after setting USBCMD.R/S.
    Run,
    /// A command did not complete.
    Command,
    /// CRCR.CRR did not return to 0 after a Command Abort.
    CommandAbort,
    /// A control transfer did not complete.
    Transfer,
    /// A port reset did not complete (PRC/WRC).
    PortReset,
    /// A USB3 port did not reach the Enabled/U0 state.
    PortEnable,
}

/// Driver errors. No operation that fails has a partial effect on driver
/// bookkeeping; controller-side effects that cannot be undone (a command
/// that was executed) are documented at the operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A register read returned all ones: the controller is gone.
    ControllerGone,
    /// Capability registers are inconsistent.
    BadCapability(&'static str),
    /// Extended capability list is malformed.
    BadExtCap(&'static str),
    /// A controller feature the driver needs is missing.
    Unsupported(&'static str),
    /// A wait timed out.
    Timeout(Wait),
    /// USBSTS.HSE: host system error; the controller must be reset.
    HostSystemError,
    /// USBSTS.HCE: host controller error; the controller must be reset.
    HostControllerError,
    /// The controller is not running (not initialized or failed).
    NotRunning,
    /// [`DmaAlloc::alloc`] returned `None`.
    OutOfDmaMemory,
    /// A DMA address is misaligned or not reachable by the controller.
    BadDmaAddress,
    /// Not enough free TRBs on a ring.
    RingFull,
    /// The in-flight transfer table is full.
    TooManyTransfers,
    /// A command completed with a completion code other than Success.
    Command(CompletionCode),
    /// A transfer completed with an error completion code.
    Transfer(CompletionCode),
    /// The endpoint returned STALL.
    Stall,
    /// The device was disconnected.
    Disconnected,
    /// The operation was cancelled by a controller reset or shutdown.
    ControllerReset,
    /// Port number outside 1..=MaxPorts.
    InvalidPort,
    /// Nothing is connected to the port.
    PortNotConnected,
    /// The port is not enabled.
    PortNotEnabled,
    /// Slot ID not valid or not owned by the driver.
    InvalidSlot,
    /// Endpoint not configured or of the wrong kind.
    InvalidEndpoint,
    /// The endpoint is halted and waiting for recovery.
    EndpointHalted,
    /// Argument out of range.
    BadRequest(&'static str),
    /// A USB descriptor is malformed.
    Descriptor(DescError),
    /// The caller's buffer is too small.
    BufferTooSmall,
}

impl From<DescError> for Error {
    fn from(e: DescError) -> Self {
        Error::Descriptor(e)
    }
}
