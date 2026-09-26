//! AMD I/O Virtualization Reporting Structure (signature `IVRS`).

use core::iter::FusedIterator;

use crate::bytes::{self, tail, u16_at, u32_at, u64_at, u8_at};
use crate::{AcpiError, Sdt, SdtHeader};

pub const IVRS_SIGNATURE: [u8; 4] = *b"IVRS";
/// Revision 1: IVHD type 10h only; revision 2: types 11h/40h as well.
pub const IVRS_MAX_REVISION: u8 = 2;
/// Header, IVinfo and 8 reserved bytes.
const BLOCKS_OFFSET: usize = 48;

pub const IVHD_TYPE_10: u8 = 0x10;
pub const IVHD_TYPE_11: u8 = 0x11;
pub const IVHD_TYPE_40: u8 = 0x40;
/// IVMD applying to every device.
pub const IVMD_ALL: u8 = 0x20;
/// IVMD applying to one device.
pub const IVMD_SELECT: u8 = 0x21;
/// IVMD applying to a device ID range.
pub const IVMD_RANGE: u8 = 0x22;
const IVMD_LEN: usize = 32;

pub const ENTRY_PAD: u8 = 0x00;
pub const ENTRY_ALL: u8 = 0x01;
pub const ENTRY_SELECT: u8 = 0x02;
pub const ENTRY_START_RANGE: u8 = 0x03;
pub const ENTRY_END_RANGE: u8 = 0x04;
pub const ENTRY_ALIAS_SELECT: u8 = 0x42;
pub const ENTRY_ALIAS_START_RANGE: u8 = 0x43;
pub const ENTRY_EXTENDED_SELECT: u8 = 0x46;
pub const ENTRY_EXTENDED_START_RANGE: u8 = 0x47;
pub const ENTRY_SPECIAL: u8 = 0x48;
pub const ENTRY_ACPI_HID: u8 = 0xf0;
/// Fixed part of an ACPI HID entry before the UID bytes.
const ACPI_HID_HEADER_LEN: usize = 22;

/// Special device variety.
pub const SPECIAL_IOAPIC: u8 = 1;
pub const SPECIAL_HPET: u8 = 2;

/// A verified IVRS whose every block and device entry has been decoded once.
#[derive(Clone, Copy, Debug)]
pub struct Ivrs<'a> {
    sdt: Sdt<'a>,
    iv_info: u32,
}

impl<'a> Ivrs<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, AcpiError> {
        Self::from_sdt(Sdt::parse(bytes)?)
    }

    /// IVHD types are accepted regardless of the table revision: QEMU 9.2
    /// publishes an IVHD 11h inside a revision 1 IVRS.
    pub fn from_sdt(sdt: Sdt<'a>) -> Result<Self, AcpiError> {
        let sdt = sdt.expect(IVRS_SIGNATURE, BLOCKS_OFFSET)?;
        let revision = sdt.header().revision;
        if revision == 0 || revision > IVRS_MAX_REVISION {
            return Err(AcpiError::UnsupportedRevision);
        }
        let table = sdt.bytes();
        let mut rest = tail(table, BLOCKS_OFFSET);
        while !rest.is_empty() {
            let (_, len) = decode_block(rest)?;
            rest = tail(rest, len);
        }
        Ok(Self {
            sdt,
            iv_info: u32_at(table, 36)?,
        })
    }

    pub fn header(&self) -> &SdtHeader {
        self.sdt.header()
    }

    pub fn iv_info(&self) -> u32 {
        self.iv_info
    }

    /// The IVHD 11h/40h EFR image fields are valid.
    pub fn efr_supported(&self) -> bool {
        self.iv_info & 1 != 0
    }

    /// Firmware left DMA remapping regions in place (IVinfo bit 1).
    pub fn dma_remap_supported(&self) -> bool {
        self.iv_info & 2 != 0
    }

    pub fn guest_virtual_address_size(&self) -> u8 {
        ((self.iv_info >> 5) & 0x7) as u8
    }

    pub fn physical_address_size(&self) -> u8 {
        ((self.iv_info >> 8) & 0x7f) as u8
    }

    pub fn virtual_address_size(&self) -> u8 {
        ((self.iv_info >> 15) & 0x7f) as u8
    }

    pub fn blocks(&self) -> IvrsBlocks<'a> {
        IvrsBlocks {
            rest: tail(self.sdt.bytes(), BLOCKS_OFFSET),
        }
    }
}

/// I/O Virtualization Hardware Definition of type 10h, 11h or 40h.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ivhd<'a> {
    pub block_type: u8,
    pub flags: u8,
    /// PCI device ID (bus:dev.fn) of the IOMMU itself.
    pub device_id: u16,
    pub capability_offset: u16,
    pub base_address: u64,
    pub pci_segment: u16,
    pub iommu_info: u16,
    /// Type 10h: IOMMU Feature Reporting; types 11h/40h: IOMMU Attributes.
    pub feature_info: u32,
    /// EFR register image (types 11h/40h).
    pub efr: Option<u64>,
    /// EFR2 register image (type 40h).
    pub efr2: Option<u64>,
    pub entries: IvhdEntries<'a>,
}

/// I/O Virtualization Memory Definition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ivmd {
    pub block_type: u8,
    pub flags: u8,
    pub device_id: u16,
    /// End device ID for [`IVMD_RANGE`], otherwise reserved.
    pub auxiliary_data: u16,
    pub start_address: u64,
    pub memory_length: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IvrsBlock<'a> {
    Ivhd(Ivhd<'a>),
    Ivmd(Ivmd),
    /// Any other block type; `bytes` is the whole block.
    Unknown {
        block_type: u8,
        bytes: &'a [u8],
    },
}

fn ivhd_header_len(block_type: u8) -> Option<usize> {
    match block_type {
        IVHD_TYPE_10 => Some(24),
        IVHD_TYPE_11 | IVHD_TYPE_40 => Some(40),
        _ => None,
    }
}

fn decode_block(rest: &[u8]) -> Result<(IvrsBlock<'_>, usize), AcpiError> {
    let block_type = u8_at(rest, 0)?;
    let len = u16_at(rest, 2).map_err(|_| AcpiError::RecordOverrun)? as usize;
    if len == 0 {
        return Err(AcpiError::ZeroLengthRecord);
    }
    let min = match block_type {
        IVMD_ALL | IVMD_SELECT | IVMD_RANGE => IVMD_LEN,
        _ => ivhd_header_len(block_type).unwrap_or(4),
    };
    if len < min {
        return Err(AcpiError::ShortRecord);
    }
    let r = rest.get(..len).ok_or(AcpiError::RecordOverrun)?;
    let block = if let Some(header_len) = ivhd_header_len(block_type) {
        IvrsBlock::Ivhd(Ivhd {
            block_type,
            flags: u8_at(r, 1)?,
            device_id: u16_at(r, 4)?,
            capability_offset: u16_at(r, 6)?,
            base_address: u64_at(r, 8)?,
            pci_segment: u16_at(r, 16)?,
            iommu_info: u16_at(r, 18)?,
            feature_info: u32_at(r, 20)?,
            efr: (block_type != IVHD_TYPE_10)
                .then(|| u64_at(r, 24))
                .transpose()?,
            efr2: (block_type == IVHD_TYPE_40)
                .then(|| u64_at(r, 32))
                .transpose()?,
            entries: IvhdEntries::validate(tail(r, header_len))?,
        })
    } else if matches!(block_type, IVMD_ALL | IVMD_SELECT | IVMD_RANGE) {
        let ivmd = Ivmd {
            block_type,
            flags: u8_at(r, 1)?,
            device_id: u16_at(r, 4)?,
            auxiliary_data: u16_at(r, 6)?,
            start_address: u64_at(r, 16)?,
            memory_length: u64_at(r, 24)?,
        };
        if ivmd.start_address.checked_add(ivmd.memory_length).is_none() {
            return Err(AcpiError::AddressOverflow);
        }
        if block_type == IVMD_RANGE && ivmd.device_id > ivmd.auxiliary_data {
            return Err(AcpiError::InvalidField);
        }
        IvrsBlock::Ivmd(ivmd)
    } else {
        IvrsBlock::Unknown {
            block_type,
            bytes: r,
        }
    };
    Ok((block, len))
}

#[derive(Clone, Debug)]
pub struct IvrsBlocks<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for IvrsBlocks<'a> {
    type Item = IvrsBlock<'a>;

    fn next(&mut self) -> Option<IvrsBlock<'a>> {
        if self.rest.is_empty() {
            return None;
        }
        match decode_block(self.rest) {
            Ok((block, len)) => {
                self.rest = tail(self.rest, len);
                Some(block)
            }
            // Unreachable after `Ivrs::from_sdt` validated every block.
            Err(_) => {
                self.rest = &[];
                None
            }
        }
    }
}

impl FusedIterator for IvrsBlocks<'_> {}

/// IVHD device entry. `data` is the DTE setting byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IvhdEntry<'a> {
    /// Type 0: 4-byte padding.
    Pad,
    All {
        data: u8,
    },
    Select {
        device_id: u16,
        data: u8,
    },
    StartRange {
        device_id: u16,
        data: u8,
    },
    EndRange {
        device_id: u16,
    },
    AliasSelect {
        device_id: u16,
        data: u8,
        source_id: u16,
    },
    AliasStartRange {
        device_id: u16,
        data: u8,
        source_id: u16,
    },
    ExtendedSelect {
        device_id: u16,
        data: u8,
        extended: u32,
    },
    ExtendedStartRange {
        device_id: u16,
        data: u8,
        extended: u32,
    },
    /// IOAPIC or HPET behind the IOMMU; `handle` is its IOAPIC ID / HPET number.
    Special {
        data: u8,
        handle: u8,
        device_id: u16,
        variety: u8,
    },
    /// ACPI namespace device (variable length).
    AcpiHid {
        device_id: u16,
        data: u8,
        hid: [u8; 8],
        cid: [u8; 8],
        uid_format: u8,
        uid: &'a [u8],
    },
    /// Other types; the length class comes from type bits 7:6.
    Unknown {
        entry_type: u8,
        bytes: &'a [u8],
    },
}

fn entry_len(rest: &[u8], entry_type: u8) -> Result<usize, AcpiError> {
    Ok(match entry_type {
        ENTRY_ACPI_HID => {
            let uid_len =
                u8_at(rest, ACPI_HID_HEADER_LEN - 1).map_err(|_| AcpiError::RecordOverrun)?;
            ACPI_HID_HEADER_LEN + usize::from(uid_len)
        }
        0x00..=0x3f => 4,
        0x40..=0x7f => 8,
        0x80..=0xbf => 16,
        _ => 32,
    })
}

fn decode_entry(rest: &[u8]) -> Result<(IvhdEntry<'_>, usize), AcpiError> {
    let entry_type = u8_at(rest, 0)?;
    let len = entry_len(rest, entry_type)?;
    let r = rest.get(..len).ok_or(AcpiError::RecordOverrun)?;
    let device_id = u16_at(r, 1)?;
    let data = u8_at(r, 3)?;
    let entry = match entry_type {
        ENTRY_PAD => IvhdEntry::Pad,
        ENTRY_ALL => IvhdEntry::All { data },
        ENTRY_SELECT => IvhdEntry::Select { device_id, data },
        ENTRY_START_RANGE => IvhdEntry::StartRange { device_id, data },
        ENTRY_END_RANGE => IvhdEntry::EndRange { device_id },
        ENTRY_ALIAS_SELECT => IvhdEntry::AliasSelect {
            device_id,
            data,
            source_id: u16_at(r, 5)?,
        },
        ENTRY_ALIAS_START_RANGE => IvhdEntry::AliasStartRange {
            device_id,
            data,
            source_id: u16_at(r, 5)?,
        },
        ENTRY_EXTENDED_SELECT => IvhdEntry::ExtendedSelect {
            device_id,
            data,
            extended: u32_at(r, 4)?,
        },
        ENTRY_EXTENDED_START_RANGE => IvhdEntry::ExtendedStartRange {
            device_id,
            data,
            extended: u32_at(r, 4)?,
        },
        ENTRY_SPECIAL => IvhdEntry::Special {
            data,
            handle: u8_at(r, 4)?,
            device_id: u16_at(r, 5)?,
            variety: u8_at(r, 7)?,
        },
        ENTRY_ACPI_HID => IvhdEntry::AcpiHid {
            device_id,
            data,
            hid: bytes::array(r, 4)?,
            cid: bytes::array(r, 12)?,
            uid_format: u8_at(r, 20)?,
            uid: tail(r, ACPI_HID_HEADER_LEN),
        },
        _ => IvhdEntry::Unknown {
            entry_type,
            bytes: r,
        },
    };
    Ok((entry, len))
}

/// Device entries of one IVHD. Validation also checks that range entries
/// pair up: every start has a later end with a device ID not below it, no
/// start is left open and no end appears without a start. Overlapping ranges
/// are allowed (the Lenovo 82K8 aliases 0xff00..=0xffff inside 0x0008..=0xfffe).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IvhdEntries<'a> {
    rest: &'a [u8],
}

impl<'a> IvhdEntries<'a> {
    fn validate(region: &'a [u8]) -> Result<Self, AcpiError> {
        let mut open: Option<u16> = None;
        let mut rest = region;
        while !rest.is_empty() {
            let (entry, len) = decode_entry(rest)?;
            match entry {
                IvhdEntry::StartRange { device_id, .. }
                | IvhdEntry::AliasStartRange { device_id, .. }
                | IvhdEntry::ExtendedStartRange { device_id, .. } => {
                    if open.replace(device_id).is_some() {
                        return Err(AcpiError::InvalidField);
                    }
                }
                IvhdEntry::EndRange { device_id } => match open.take() {
                    Some(start) if start <= device_id => {}
                    _ => return Err(AcpiError::InvalidField),
                },
                _ => {}
            }
            rest = tail(rest, len);
        }
        if open.is_some() {
            return Err(AcpiError::InvalidField);
        }
        Ok(Self { rest: region })
    }
}

impl<'a> Iterator for IvhdEntries<'a> {
    type Item = IvhdEntry<'a>;

    fn next(&mut self) -> Option<IvhdEntry<'a>> {
        if self.rest.is_empty() {
            return None;
        }
        match decode_entry(self.rest) {
            Ok((entry, len)) => {
                self.rest = tail(self.rest, len);
                Some(entry)
            }
            Err(_) => {
                self.rest = &[];
                None
            }
        }
    }
}

impl FusedIterator for IvhdEntries<'_> {}
