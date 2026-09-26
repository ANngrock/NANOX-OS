//! Common configuration header and the type 1 bus-number registers.

use crate::{regs, Bdf, ConfigSpace, PciError};

/// Header layout selected by the low 7 bits of the header type register.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HeaderKind {
    /// Type 0: endpoint or host bridge.
    #[default]
    Endpoint,
    /// Type 1: PCI-to-PCI bridge (including PCIe root and switch ports).
    PciBridge,
    /// Type 2: CardBus bridge. Recognised but never descended into.
    CardBus,
}

impl HeaderKind {
    pub const fn from_register(header_type: u8) -> Result<Self, PciError> {
        match header_type & 0x7F {
            0 => Ok(Self::Endpoint),
            1 => Ok(Self::PciBridge),
            2 => Ok(Self::CardBus),
            other => Err(PciError::UnsupportedHeaderType(other)),
        }
    }

    /// Number of 32-bit BAR slots starting at offset 0x10.
    pub const fn bar_slots(self) -> u8 {
        match self {
            Self::Endpoint => 6,
            Self::PciBridge => 2,
            Self::CardBus => 1,
        }
    }

    /// Offset of the capabilities pointer register for this layout.
    pub const fn capability_pointer(self) -> u16 {
        match self {
            Self::Endpoint | Self::PciBridge => regs::CAPABILITY_POINTER,
            Self::CardBus => regs::CARDBUS_CAPABILITY_POINTER,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub vendor_id: u16,
    pub device_id: u16,
    pub command: u16,
    pub status: u16,
    pub revision: u8,
    pub prog_if: u8,
    pub subclass: u8,
    pub class: u8,
    pub kind: HeaderKind,
    /// Header type bit 7. Only meaningful on function 0.
    pub multi_function: bool,
}

impl Header {
    pub const fn has_capabilities(&self) -> bool {
        self.status & regs::STATUS_CAPABILITIES_LIST != 0
    }
}

/// Read the common header. `Ok(None)` means no function responds: the vendor
/// ID reads 0xFFFF (master abort) or 0x0000 (never a valid vendor).
pub fn read_header<C: ConfigSpace + ?Sized>(
    cfg: &mut C,
    bdf: Bdf,
) -> Result<Option<Header>, PciError> {
    let id = cfg.read_u32(bdf, regs::VENDOR_ID);
    let vendor_id = id as u16;
    if vendor_id == 0xFFFF || vendor_id == 0 {
        return Ok(None);
    }
    let command = cfg.read_u16(bdf, regs::COMMAND);
    let status = cfg.read_u16(bdf, regs::STATUS);
    let class = cfg.read_u32(bdf, regs::REVISION_ID);
    let header_type = cfg.read_u8(bdf, regs::HEADER_TYPE);
    let kind = HeaderKind::from_register(header_type)?;
    Ok(Some(Header {
        vendor_id,
        device_id: (id >> 16) as u16,
        command,
        status,
        revision: class as u8,
        prog_if: (class >> 8) as u8,
        subclass: (class >> 16) as u8,
        class: (class >> 24) as u8,
        kind,
        multi_function: header_type & regs::HEADER_TYPE_MULTI_FUNCTION != 0,
    }))
}

/// Primary, secondary and subordinate bus numbers of a type 1 header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BridgeBuses {
    pub primary: u8,
    pub secondary: u8,
    pub subordinate: u8,
}

fn require_bridge<C: ConfigSpace + ?Sized>(cfg: &mut C, bdf: Bdf) -> Result<(), PciError> {
    let header_type = cfg.read_u8(bdf, regs::HEADER_TYPE);
    match HeaderKind::from_register(header_type)? {
        HeaderKind::PciBridge => Ok(()),
        _ => Err(PciError::NotABridge),
    }
}

pub fn read_bridge_buses<C: ConfigSpace + ?Sized>(
    cfg: &mut C,
    bdf: Bdf,
) -> Result<BridgeBuses, PciError> {
    require_bridge(cfg, bdf)?;
    let value = cfg.read_u32(bdf, regs::PRIMARY_BUS);
    Ok(BridgeBuses {
        primary: value as u8,
        secondary: (value >> 8) as u8,
        subordinate: (value >> 16) as u8,
    })
}

/// Program the bus-number registers, preserving the secondary latency timer,
/// and verify the bridge accepted them.
pub fn write_bridge_buses<C: ConfigSpace + ?Sized>(
    cfg: &mut C,
    bdf: Bdf,
    buses: BridgeBuses,
) -> Result<(), PciError> {
    if buses.subordinate < buses.secondary {
        return Err(PciError::SubordinateBelowSecondary);
    }
    if buses.secondary <= buses.primary {
        return Err(PciError::BusConflict);
    }
    require_bridge(cfg, bdf)?;
    // Offsets 0x18..0x1B hold no write-1-to-clear bits, so a dword
    // read-modify-write is safe here.
    let old = cfg.read_u32(bdf, regs::PRIMARY_BUS);
    let value = (old & 0xFF00_0000)
        | (u32::from(buses.subordinate) << 16)
        | (u32::from(buses.secondary) << 8)
        | u32::from(buses.primary);
    cfg.write_u32(bdf, regs::PRIMARY_BUS, value);
    if read_bridge_buses(cfg, bdf)? != buses {
        return Err(PciError::BridgeNotProgrammed);
    }
    Ok(())
}
