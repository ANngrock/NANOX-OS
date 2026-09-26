#![no_std]
#![forbid(unsafe_code)]

//! PCI/PCIe configuration space model for NANOX (M9, docs/specs/M9-HARDWARE.md §3.2).
//!
//! The crate never touches hardware itself: every configuration access goes
//! through [`ConfigSpace`], which the caller implements (the kernel over an
//! ECAM mapping, host tests over a model). Everything read from a device is
//! untrusted: pointers, sizes and bus numbers are validated before use, and
//! malformed input yields a [`PciError`] instead of a panic or a partial
//! result. No allocation is performed; results go to caller-owned storage.

mod bar;
mod capability;
mod ecam;
mod enumerate;
mod header;

pub use bar::{decode_bar_type, probe_bar, probe_bars, Bar, BarKind, BarSlot, Bars, MAX_BAR_SLOTS};
pub use capability::{
    capabilities, extended_capabilities, find_capability, find_extended_capability, Capabilities,
    Capability, ExtendedCapabilities, ExtendedCapability, Msi, MsiX, MsixRegion, PcieCapability,
    PciePortType, CAP_ID_MSI, CAP_ID_MSIX, CAP_ID_PCIE, CAP_ID_VENDOR, MSIX_ENTRY_SIZE,
};
pub use ecam::{AccessWidth, EcamSegment, PHYS_ADDRESS_LIMIT};
pub use enumerate::{enumerate, BusNumbering, EnumerationConfig, Function, MAX_BRIDGE_DEPTH};
pub use header::{
    read_bridge_buses, read_header, write_bridge_buses, BridgeBuses, Header, HeaderKind,
};

/// Size of the conventional (PCI-compatible) configuration space of a function.
pub const CONFIG_SPACE_SIZE: u16 = 256;
/// Size of the PCIe extended configuration space of a function.
pub const EXTENDED_CONFIG_SPACE_SIZE: u16 = 4096;

/// Register offsets and bits of the common configuration header.
pub mod regs {
    pub const VENDOR_ID: u16 = 0x00;
    pub const DEVICE_ID: u16 = 0x02;
    pub const COMMAND: u16 = 0x04;
    pub const STATUS: u16 = 0x06;
    pub const REVISION_ID: u16 = 0x08;
    pub const PROG_IF: u16 = 0x09;
    pub const SUBCLASS: u16 = 0x0A;
    pub const CLASS: u16 = 0x0B;
    pub const HEADER_TYPE: u16 = 0x0E;
    pub const BAR0: u16 = 0x10;
    /// Type 1: primary, secondary, subordinate bus and secondary latency timer.
    pub const PRIMARY_BUS: u16 = 0x18;
    pub const SECONDARY_BUS: u16 = 0x19;
    pub const SUBORDINATE_BUS: u16 = 0x1A;
    pub const CARDBUS_CAPABILITY_POINTER: u16 = 0x14;
    pub const CAPABILITY_POINTER: u16 = 0x34;
    pub const EXTENDED_CAPABILITIES: u16 = 0x100;

    pub const COMMAND_IO_SPACE: u16 = 1 << 0;
    pub const COMMAND_MEMORY_SPACE: u16 = 1 << 1;
    pub const COMMAND_BUS_MASTER: u16 = 1 << 2;
    pub const STATUS_CAPABILITIES_LIST: u16 = 1 << 4;
    pub const HEADER_TYPE_MULTI_FUNCTION: u8 = 0x80;
}

/// Everything this crate can reject. Variants are data-free where the
/// offending value is already known to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PciError {
    /// Device number is 32 or larger.
    InvalidDevice,
    /// Function number is 8 or larger.
    InvalidFunction,
    /// Offset plus access width leaves the 4 KiB function space.
    InvalidOffset,
    /// Offset is not a multiple of the access width.
    MisalignedOffset,
    /// Bus number is outside the segment or enumeration bus range.
    BusOutOfRange,
    /// Start bus is greater than end bus.
    InvalidBusRange,
    /// ECAM base is not aligned to the 1 MiB per-bus window.
    EcamAlignment,
    /// ECAM region overflows 64 bits or exceeds [`PHYS_ADDRESS_LIMIT`].
    EcamOverflow,
    /// Header layout (low 7 bits of the header type) is not 0, 1 or 2.
    UnsupportedHeaderType(u8),
    /// A type 1 operation was requested on a function that is not a PCI bridge.
    NotABridge,
    /// BAR index is outside the header's BAR slots.
    BarIndex,
    /// Memory BAR uses a reserved type encoding (01b or 11b).
    BarReservedType,
    /// A 64-bit BAR starts in the last BAR slot, so its upper half does not exist.
    Bar64InLastSlot,
    /// Sizing read-back is inconsistent: type bits changed, no or
    /// non-contiguous writable bits, unaligned or overflowing address.
    BarResponse,
    /// The BAR did not read back its original value after sizing.
    BarNotRestored,
    /// The command register did not read back its original value after sizing.
    CommandNotRestored,
    /// Capability pointer is below the header, misaligned, or outside its space.
    CapabilityPointer,
    /// A capability offset was visited twice.
    CapabilityLoop,
    /// The capability structure extends past the end of its configuration space.
    CapabilityBounds,
    /// An extended capability header after the first reads 0 or all ones
    /// (function removed or list corrupt).
    ExtendedCapabilityHeader,
    /// The structure at the given offset has a different capability ID.
    CapabilityId,
    /// MSI control uses a reserved vector encoding or enables more vectors than capable.
    MsiControl,
    /// MSI-X BIR uses a reserved value (6 or 7).
    MsixBir,
    /// MSI-X BIR does not name an implemented memory BAR.
    MsixBar,
    /// MSI-X table does not fit inside its BAR.
    MsixTableOutOfBar,
    /// MSI-X pending bit array does not fit inside its BAR.
    MsixPbaOutOfBar,
    /// MSI-X table and pending bit array overlap.
    MsixOverlap,
    /// PCI Express capability has an unknown version or reserved port type.
    PcieCapability,
    /// Bridge primary bus number differs from the bus it was found on.
    BridgePrimaryMismatch,
    /// Requested bridge secondary bus number is not above its primary bus.
    SecondaryNotAbovePrimary,
    /// Bridge subordinate bus number is below its secondary bus number.
    SubordinateBelowSecondary,
    /// Bridge bus range leaves the window of its parent bridge.
    BusOutsideWindow,
    /// Bridge bus range reuses a scanned bus or overlaps another bridge's range.
    BusConflict,
    /// No bus number is left for another bridge while assigning.
    BusExhausted,
    /// The bridge did not accept the bus numbers written to it.
    BridgeNotProgrammed,
    /// Bridge nesting is deeper than the configured limit.
    DepthExceeded,
    /// Requested depth limit is above [`MAX_BRIDGE_DEPTH`].
    InvalidDepthLimit,
    /// The caller's result buffer cannot hold every discovered function.
    BufferTooSmall,
}

/// Bus/device/function address of a function inside one PCI segment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bdf {
    bus: u8,
    device: u8,
    function: u8,
}

impl Bdf {
    pub const MAX_DEVICE: u8 = 31;
    pub const MAX_FUNCTION: u8 = 7;

    pub const fn new(bus: u8, device: u8, function: u8) -> Result<Self, PciError> {
        if device > Self::MAX_DEVICE {
            return Err(PciError::InvalidDevice);
        }
        if function > Self::MAX_FUNCTION {
            return Err(PciError::InvalidFunction);
        }
        Ok(Self {
            bus,
            device,
            function,
        })
    }

    pub const fn bus(self) -> u8 {
        self.bus
    }

    pub const fn device(self) -> u8 {
        self.device
    }

    pub const fn function(self) -> u8 {
        self.function
    }
}

/// Inclusive range of bus numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusRange {
    start: u8,
    end: u8,
}

impl BusRange {
    pub const fn new(start: u8, end: u8) -> Result<Self, PciError> {
        if start > end {
            return Err(PciError::InvalidBusRange);
        }
        Ok(Self { start, end })
    }

    pub const fn start(self) -> u8 {
        self.start
    }

    pub const fn end(self) -> u8 {
        self.end
    }

    pub const fn contains(self, bus: u8) -> bool {
        self.start <= bus && bus <= self.end
    }
}

/// Configuration space access for one PCI segment, implemented by the caller.
///
/// Contract offered by this crate to implementations: every `offset` passed
/// is naturally aligned for the access width and `offset + width <= 4096`.
/// `bdf` always has device < 32 and function < 8, but its bus may be one the
/// implementation cannot reach; like PCI hardware on a master abort, such
/// reads must return all ones and such writes must be ignored.
///
/// Sub-dword writes are required as separate operations: emulating them by
/// read-modify-write of the containing dword would write back write-1-to-clear
/// status bits.
pub trait ConfigSpace {
    fn read_u8(&mut self, bdf: Bdf, offset: u16) -> u8;
    fn read_u16(&mut self, bdf: Bdf, offset: u16) -> u16;
    fn read_u32(&mut self, bdf: Bdf, offset: u16) -> u32;
    fn write_u8(&mut self, bdf: Bdf, offset: u16, value: u8);
    fn write_u16(&mut self, bdf: Bdf, offset: u16, value: u16);
    fn write_u32(&mut self, bdf: Bdf, offset: u16, value: u32);
}
