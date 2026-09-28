//! Enhanced Configuration Access Mechanism address arithmetic.

use crate::{Bdf, BusRange, PciError, EXTENDED_CONFIG_SPACE_SIZE};

/// Highest physical address (exclusive) accepted for an ECAM region; x86-64
/// defines at most 52 physical address bits.
pub const PHYS_ADDRESS_LIMIT: u64 = 1 << 52;

const BUS_SHIFT: u32 = 20;
const DEVICE_SHIFT: u32 = 15;
const FUNCTION_SHIFT: u32 = 12;
const BUS_WINDOW: u64 = 1 << BUS_SHIFT;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessWidth {
    Byte,
    Word,
    Dword,
}

impl AccessWidth {
    pub const fn bytes(self) -> u16 {
        match self {
            Self::Byte => 1,
            Self::Word => 2,
            Self::Dword => 4,
        }
    }
}

/// One MCFG allocation. As in the ACPI MCFG table, `base` is the address that
/// bus 0 of the segment would have, even when `start_bus` is not 0; only buses
/// `start_bus..=end_bus` are decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EcamSegment {
    base: u64,
    segment: u16,
    buses: BusRange,
}

impl EcamSegment {
    pub fn new(base: u64, segment: u16, start_bus: u8, end_bus: u8) -> Result<Self, PciError> {
        let buses = BusRange::new(start_bus, end_bus)?;
        if !base.is_multiple_of(BUS_WINDOW) {
            return Err(PciError::EcamAlignment);
        }
        let span = (u64::from(end_bus) + 1) << BUS_SHIFT;
        let limit = base.checked_add(span).ok_or(PciError::EcamOverflow)?;
        if limit > PHYS_ADDRESS_LIMIT {
            return Err(PciError::EcamOverflow);
        }
        Ok(Self {
            base,
            segment,
            buses,
        })
    }

    pub const fn base(&self) -> u64 {
        self.base
    }

    pub const fn segment(&self) -> u16 {
        self.segment
    }

    pub const fn buses(&self) -> BusRange {
        self.buses
    }

    /// First physical address decoded for this segment's bus range.
    pub const fn region_start(&self) -> u64 {
        // new() proved base + (end_bus + 1) MiB fits below PHYS_ADDRESS_LIMIT.
        self.base + ((self.buses.start() as u64) << BUS_SHIFT)
    }

    /// Length in bytes of the decoded region.
    pub const fn region_len(&self) -> u64 {
        ((self.buses.end() as u64) - (self.buses.start() as u64) + 1) << BUS_SHIFT
    }

    /// Physical address of a register, after bus, offset and alignment checks.
    pub fn address(&self, bdf: Bdf, offset: u16, width: AccessWidth) -> Result<u64, PciError> {
        if !self.buses.contains(bdf.bus()) {
            return Err(PciError::BusOutOfRange);
        }
        let size = width.bytes();
        if offset >= EXTENDED_CONFIG_SPACE_SIZE || offset > EXTENDED_CONFIG_SPACE_SIZE - size {
            return Err(PciError::InvalidOffset);
        }
        if !offset.is_multiple_of(size) {
            return Err(PciError::MisalignedOffset);
        }
        let relative = (u64::from(bdf.bus()) << BUS_SHIFT)
            | (u64::from(bdf.device()) << DEVICE_SHIFT)
            | (u64::from(bdf.function()) << FUNCTION_SHIFT)
            | u64::from(offset);
        self.base
            .checked_add(relative)
            .ok_or(PciError::EcamOverflow)
    }

    /// Inverse of [`Self::address`]: the function and register an address
    /// inside the decoded region refers to.
    pub fn decode(&self, address: u64) -> Result<(Bdf, u16), PciError> {
        let relative = address
            .checked_sub(self.base)
            .ok_or(PciError::BusOutOfRange)?;
        let bus = u8::try_from(relative >> BUS_SHIFT).map_err(|_| PciError::BusOutOfRange)?;
        if !self.buses.contains(bus) {
            return Err(PciError::BusOutOfRange);
        }
        let device = ((relative >> DEVICE_SHIFT) & 0x1F) as u8;
        let function = ((relative >> FUNCTION_SHIFT) & 0x7) as u8;
        let offset = (relative & 0xFFF) as u16;
        Ok((Bdf::new(bus, device, function)?, offset))
    }
}
