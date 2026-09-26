//! Capability lists and the MSI, MSI-X and PCI Express capabilities.

use crate::{
    regs, BarSlot, Bars, Bdf, ConfigSpace, HeaderKind, PciError, CONFIG_SPACE_SIZE,
    EXTENDED_CONFIG_SPACE_SIZE,
};

pub const CAP_ID_MSI: u8 = 0x05;
pub const CAP_ID_VENDOR: u8 = 0x09;
pub const CAP_ID_PCIE: u8 = 0x10;
pub const CAP_ID_MSIX: u8 = 0x11;
/// Bytes per MSI-X table entry.
pub const MSIX_ENTRY_SIZE: u64 = 16;

/// Lowest offset a capability may occupy: the standard header ends at 0x40.
const FIRST_CAPABILITY: u8 = 0x40;
const FIRST_EXTENDED: u16 = regs::EXTENDED_CAPABILITIES;
const EXTENDED_SLOT_WORDS: usize =
    ((EXTENDED_CONFIG_SPACE_SIZE - FIRST_EXTENDED) as usize / 4).div_ceil(64);

/// One entry of the conventional capability list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability {
    pub id: u8,
    pub offset: u8,
}

/// One entry of the PCIe extended capability list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExtendedCapability {
    pub id: u16,
    pub version: u8,
    pub offset: u16,
}

/// Iterator over the conventional capability list. Each dword slot between
/// 0x40 and 0x100 can be visited once, so the walk ends after at most 48
/// entries; after the first error the iterator yields nothing more.
pub struct Capabilities<'a, C: ConfigSpace + ?Sized> {
    cfg: &'a mut C,
    bdf: Bdf,
    next: u8,
    visited: u64,
    done: bool,
}

/// Start walking the conventional capability list. The list is empty when
/// the status register does not advertise one.
pub fn capabilities<C: ConfigSpace + ?Sized>(
    cfg: &mut C,
    bdf: Bdf,
    kind: HeaderKind,
) -> Capabilities<'_, C> {
    let status = cfg.read_u16(bdf, regs::STATUS);
    let next = if status & regs::STATUS_CAPABILITIES_LIST != 0 {
        cfg.read_u8(bdf, kind.capability_pointer())
    } else {
        0
    };
    Capabilities {
        cfg,
        bdf,
        next,
        visited: 0,
        done: false,
    }
}

/// Validate a conventional capability offset: dword aligned, past the header.
/// Every u8 offset is below 0x100, and an aligned one leaves room for a dword.
const fn check_pointer(offset: u8) -> Result<(), PciError> {
    if offset < FIRST_CAPABILITY || !offset.is_multiple_of(4) {
        return Err(PciError::CapabilityPointer);
    }
    Ok(())
}

impl<C: ConfigSpace + ?Sized> Iterator for Capabilities<'_, C> {
    type Item = Result<Capability, PciError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let offset = self.next;
        if offset == 0 {
            self.done = true;
            return None;
        }
        if let Err(error) = check_pointer(offset) {
            self.done = true;
            return Some(Err(error));
        }
        // Aligned offsets 0x40..=0xFC map to slots 0..48, which fit in a u64.
        let slot = (offset - FIRST_CAPABILITY) / 4;
        let bit = 1u64 << slot;
        if self.visited & bit != 0 {
            self.done = true;
            return Some(Err(PciError::CapabilityLoop));
        }
        self.visited |= bit;
        let header = self.cfg.read_u16(self.bdf, u16::from(offset));
        self.next = (header >> 8) as u8;
        Some(Ok(Capability {
            id: header as u8,
            offset,
        }))
    }
}

/// Iterator over the PCIe extended capability list starting at 0x100. The
/// list is empty when the first header reads 0 or all ones (no extended
/// space). Each of the 960 dword slots can be visited once.
pub struct ExtendedCapabilities<'a, C: ConfigSpace + ?Sized> {
    cfg: &'a mut C,
    bdf: Bdf,
    next: u16,
    first: bool,
    visited: [u64; EXTENDED_SLOT_WORDS],
    done: bool,
}

/// Start walking the extended capability list. Only meaningful when the
/// [`ConfigSpace`] reaches the full 4 KiB (ECAM).
pub fn extended_capabilities<C: ConfigSpace + ?Sized>(
    cfg: &mut C,
    bdf: Bdf,
) -> ExtendedCapabilities<'_, C> {
    ExtendedCapabilities {
        cfg,
        bdf,
        next: FIRST_EXTENDED,
        first: true,
        visited: [0; EXTENDED_SLOT_WORDS],
        done: false,
    }
}

impl<C: ConfigSpace + ?Sized> Iterator for ExtendedCapabilities<'_, C> {
    type Item = Result<ExtendedCapability, PciError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let offset = self.next;
        if offset == 0 {
            self.done = true;
            return None;
        }
        // Next pointers are 12 bits wide, so offset < 0x1000 always holds.
        if !(FIRST_EXTENDED..EXTENDED_CONFIG_SPACE_SIZE).contains(&offset)
            || !offset.is_multiple_of(4)
        {
            self.done = true;
            return Some(Err(PciError::CapabilityPointer));
        }
        let slot = usize::from((offset - FIRST_EXTENDED) / 4);
        let (word, bit) = (slot / 64, 1u64 << (slot % 64));
        let Some(seen) = self.visited.get_mut(word) else {
            self.done = true;
            return Some(Err(PciError::CapabilityPointer));
        };
        if *seen & bit != 0 {
            self.done = true;
            return Some(Err(PciError::CapabilityLoop));
        }
        *seen |= bit;
        let header = self.cfg.read_u32(self.bdf, offset);
        if self.first && (header == 0 || header == u32::MAX) {
            self.done = true;
            return None;
        }
        self.first = false;
        self.next = (header >> 20) as u16;
        Some(Ok(ExtendedCapability {
            id: header as u16,
            version: ((header >> 16) & 0xF) as u8,
            offset,
        }))
    }
}

/// Offset of the first conventional capability with `id`. Entries after the
/// match are not validated.
pub fn find_capability<C: ConfigSpace + ?Sized>(
    cfg: &mut C,
    bdf: Bdf,
    kind: HeaderKind,
    id: u8,
) -> Result<Option<u8>, PciError> {
    for capability in capabilities(cfg, bdf, kind) {
        let capability = capability?;
        if capability.id == id {
            return Ok(Some(capability.offset));
        }
    }
    Ok(None)
}

/// Offset of the first extended capability with `id`.
pub fn find_extended_capability<C: ConfigSpace + ?Sized>(
    cfg: &mut C,
    bdf: Bdf,
    id: u16,
) -> Result<Option<u16>, PciError> {
    for capability in extended_capabilities(cfg, bdf) {
        let capability = capability?;
        if capability.id == id {
            return Ok(Some(capability.offset));
        }
    }
    Ok(None)
}

/// Validate a capability's offset, extent and ID before its body is read.
fn check_structure<C: ConfigSpace + ?Sized>(
    cfg: &mut C,
    bdf: Bdf,
    offset: u8,
    id: u8,
    length: u16,
) -> Result<(), PciError> {
    check_pointer(offset)?;
    if u16::from(offset) + length > CONFIG_SPACE_SIZE {
        return Err(PciError::CapabilityBounds);
    }
    if cfg.read_u8(bdf, u16::from(offset)) != id {
        return Err(PciError::CapabilityId);
    }
    Ok(())
}

/// Parsed MSI capability. Register fields are absolute configuration offsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Msi {
    pub offset: u8,
    pub enabled: bool,
    pub address_64bit: bool,
    pub per_vector_masking: bool,
    /// Vectors the function can request (Multiple Message Capable), 1..=32.
    pub vectors_capable: u8,
    /// Vectors currently allocated (Multiple Message Enable), 1..=32.
    pub vectors_enabled: u8,
    pub address_offset: u16,
    pub upper_address_offset: Option<u16>,
    pub data_offset: u16,
    pub mask_offset: Option<u16>,
    pub pending_offset: Option<u16>,
}

const MSI_ENABLE: u16 = 1 << 0;
const MSI_64BIT: u16 = 1 << 7;
const MSI_PER_VECTOR_MASK: u16 = 1 << 8;
const MSI_MAX_LOG2_VECTORS: u16 = 5;

impl Msi {
    pub fn read<C: ConfigSpace + ?Sized>(
        cfg: &mut C,
        bdf: Bdf,
        offset: u8,
    ) -> Result<Self, PciError> {
        // Header and control word must fit before the layout is known.
        check_structure(cfg, bdf, offset, CAP_ID_MSI, 4)?;
        let base = u16::from(offset);
        let control = cfg.read_u16(bdf, base + 2);
        let capable_log2 = (control >> 1) & 0x7;
        let enabled_log2 = (control >> 4) & 0x7;
        if capable_log2 > MSI_MAX_LOG2_VECTORS || enabled_log2 > capable_log2 {
            return Err(PciError::MsiControl);
        }
        let address_64bit = control & MSI_64BIT != 0;
        let per_vector_masking = control & MSI_PER_VECTOR_MASK != 0;
        let (upper_address_offset, data_offset) = if address_64bit {
            (Some(base + 0x8), base + 0xC)
        } else {
            (None, base + 0x8)
        };
        let (mask_offset, pending_offset, end) = if per_vector_masking {
            (
                Some(data_offset + 4),
                Some(data_offset + 8),
                data_offset + 12,
            )
        } else {
            (None, None, data_offset + 2)
        };
        if end > CONFIG_SPACE_SIZE {
            return Err(PciError::CapabilityBounds);
        }
        Ok(Self {
            offset,
            enabled: control & MSI_ENABLE != 0,
            address_64bit,
            per_vector_masking,
            vectors_capable: 1 << capable_log2,
            vectors_enabled: 1 << enabled_log2,
            address_offset: base + 0x4,
            upper_address_offset,
            data_offset,
            mask_offset,
            pending_offset,
        })
    }
}

/// Location of an MSI-X table or pending bit array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsixRegion {
    /// BAR slot index (BIR) holding the region.
    pub bir: u8,
    /// Offset inside the BAR (8-byte aligned by construction).
    pub offset: u32,
    pub length: u64,
    /// BAR address plus offset, as currently programmed.
    pub address: u64,
}

impl MsixRegion {
    const fn overlaps(&self, other: &Self) -> bool {
        // Both regions were proven to fit inside their BAR, so the sums fit.
        let (start, end) = (self.offset as u64, self.offset as u64 + self.length);
        let (other_start, other_end) = (other.offset as u64, other.offset as u64 + other.length);
        self.bir == other.bir && start < other_end && other_start < end
    }
}

/// Parsed MSI-X capability, cross-checked against the function's BARs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiX {
    pub offset: u8,
    pub enabled: bool,
    pub function_mask: bool,
    /// Number of table entries, 1..=2048.
    pub table_size: u16,
    pub table: MsixRegion,
    pub pba: MsixRegion,
}

const MSIX_LENGTH: u16 = 12;
const MSIX_TABLE_SIZE_MASK: u16 = 0x7FF;
const MSIX_FUNCTION_MASK: u16 = 1 << 14;
const MSIX_ENABLE: u16 = 1 << 15;
const MSIX_BIR_MASK: u32 = 0x7;
const MSIX_MAX_BIR: u8 = 5;

impl MsiX {
    /// `bars` must come from [`crate::probe_bars`] for the same function.
    pub fn read<C: ConfigSpace + ?Sized>(
        cfg: &mut C,
        bdf: Bdf,
        offset: u8,
        bars: &Bars,
    ) -> Result<Self, PciError> {
        check_structure(cfg, bdf, offset, CAP_ID_MSIX, MSIX_LENGTH)?;
        let base = u16::from(offset);
        let control = cfg.read_u16(bdf, base + 2);
        let table_register = cfg.read_u32(bdf, base + 4);
        let pba_register = cfg.read_u32(bdf, base + 8);
        let table_size = (control & MSIX_TABLE_SIZE_MASK) + 1;
        let entries = u64::from(table_size);
        let table = region(
            bars,
            table_register,
            entries * MSIX_ENTRY_SIZE,
            PciError::MsixTableOutOfBar,
        )?;
        // One pending bit per entry, stored in qwords.
        let pba = region(
            bars,
            pba_register,
            entries.div_ceil(64) * 8,
            PciError::MsixPbaOutOfBar,
        )?;
        if table.overlaps(&pba) {
            return Err(PciError::MsixOverlap);
        }
        Ok(Self {
            offset,
            enabled: control & MSIX_ENABLE != 0,
            function_mask: control & MSIX_FUNCTION_MASK != 0,
            table_size,
            table,
            pba,
        })
    }
}

fn region(
    bars: &Bars,
    register: u32,
    length: u64,
    out_of_bar: PciError,
) -> Result<MsixRegion, PciError> {
    let bir = (register & MSIX_BIR_MASK) as u8;
    let offset = register & !MSIX_BIR_MASK;
    if bir > MSIX_MAX_BIR {
        return Err(PciError::MsixBir);
    }
    let bar = match bars.slots().get(usize::from(bir)) {
        Some(BarSlot::Bar(bar)) if bar.kind.is_memory() => bar,
        _ => return Err(PciError::MsixBar),
    };
    if !bar.contains(u64::from(offset), length) {
        return Err(out_of_bar);
    }
    let address = bar
        .address
        .checked_add(u64::from(offset))
        .ok_or(out_of_bar)?;
    Ok(MsixRegion {
        bir,
        offset,
        length,
        address,
    })
}

/// Device/port type field of the PCI Express capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PciePortType {
    Endpoint,
    LegacyEndpoint,
    RootPort,
    UpstreamSwitchPort,
    DownstreamSwitchPort,
    PcieToPciBridge,
    PciToPcieBridge,
    RootComplexIntegratedEndpoint,
    RootComplexEventCollector,
}

impl PciePortType {
    pub const fn from_field(value: u8) -> Result<Self, PciError> {
        Ok(match value {
            0x0 => Self::Endpoint,
            0x1 => Self::LegacyEndpoint,
            0x4 => Self::RootPort,
            0x5 => Self::UpstreamSwitchPort,
            0x6 => Self::DownstreamSwitchPort,
            0x7 => Self::PcieToPciBridge,
            0x8 => Self::PciToPcieBridge,
            0x9 => Self::RootComplexIntegratedEndpoint,
            0xA => Self::RootComplexEventCollector,
            _ => return Err(PciError::PcieCapability),
        })
    }

    /// Whether a function of this type has a type 1 header.
    pub const fn is_bridge(self) -> bool {
        matches!(
            self,
            Self::RootPort
                | Self::UpstreamSwitchPort
                | Self::DownstreamSwitchPort
                | Self::PcieToPciBridge
                | Self::PciToPcieBridge
        )
    }
}

/// Parsed PCI Express capability (header word only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PcieCapability {
    pub offset: u8,
    /// Capability structure version, 1 or 2.
    pub version: u8,
    pub port_type: PciePortType,
    pub slot_implemented: bool,
    pub interrupt_message_number: u8,
}

const PCIE_V1_LENGTH: u16 = 0x24;
const PCIE_V2_LENGTH: u16 = 0x3C;

impl PcieCapability {
    pub fn read<C: ConfigSpace + ?Sized>(
        cfg: &mut C,
        bdf: Bdf,
        offset: u8,
    ) -> Result<Self, PciError> {
        check_structure(cfg, bdf, offset, CAP_ID_PCIE, 4)?;
        let base = u16::from(offset);
        let flags = cfg.read_u16(bdf, base + 2);
        let version = (flags & 0xF) as u8;
        let length = match version {
            1 => PCIE_V1_LENGTH,
            2 => PCIE_V2_LENGTH,
            _ => return Err(PciError::PcieCapability),
        };
        if base + length > CONFIG_SPACE_SIZE {
            return Err(PciError::CapabilityBounds);
        }
        Ok(Self {
            offset,
            version,
            port_type: PciePortType::from_field(((flags >> 4) & 0xF) as u8)?,
            slot_implemented: flags & (1 << 8) != 0,
            interrupt_message_number: ((flags >> 9) & 0x1F) as u8,
        })
    }
}
