//! Intel VT-d DMA Remapping Reporting table (signature `DMAR`).

use core::iter::FusedIterator;

use crate::bytes::{tail, u16_at, u32_at, u64_at, u8_at};
use crate::{AcpiError, Sdt, SdtHeader};

pub const DMAR_SIGNATURE: [u8; 4] = *b"DMAR";
/// Header, host address width, flags and 10 reserved bytes.
const STRUCTURES_OFFSET: usize = 48;

/// DMAR flag: interrupt remapping is supported.
pub const INTR_REMAP: u8 = 1 << 0;
/// DMAR flag: firmware asks the OS not to enable x2APIC mode.
pub const X2APIC_OPT_OUT: u8 = 1 << 1;
/// DMAR flag: platform opts in to DMA control by the OS.
pub const DMA_CTRL_PLATFORM_OPT_IN: u8 = 1 << 2;

pub const TYPE_DRHD: u16 = 0;
pub const TYPE_RMRR: u16 = 1;
pub const TYPE_ATSR: u16 = 2;
pub const TYPE_RHSA: u16 = 3;
pub const TYPE_ANDD: u16 = 4;

/// DRHD flag: the unit covers every PCI device of its segment not claimed
/// by another DRHD.
pub const DRHD_INCLUDE_PCI_ALL: u8 = 1;
/// ATSR flag: every root port of the segment supports ATS.
pub const ATSR_ALL_PORTS: u8 = 1;

pub const SCOPE_PCI_ENDPOINT: u8 = 1;
pub const SCOPE_PCI_SUB_HIERARCHY: u8 = 2;
pub const SCOPE_IOAPIC: u8 = 3;
pub const SCOPE_HPET: u8 = 4;
pub const SCOPE_ACPI_NAMESPACE_DEVICE: u8 = 5;

/// Device scope header: type, length, flags, reserved, enumeration ID, start bus.
const SCOPE_HEADER_LEN: usize = 6;

fn min_len(structure_type: u16) -> usize {
    match structure_type {
        TYPE_DRHD => 16,
        TYPE_RMRR => 24,
        TYPE_ATSR => 8,
        TYPE_RHSA => 20,
        TYPE_ANDD => 8,
        _ => 4,
    }
}

/// A verified DMAR whose every structure and device scope has been decoded once.
#[derive(Clone, Copy, Debug)]
pub struct Dmar<'a> {
    sdt: Sdt<'a>,
    host_address_width: u8,
    flags: u8,
}

impl<'a> Dmar<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, AcpiError> {
        Self::from_sdt(Sdt::parse(bytes)?)
    }

    pub fn from_sdt(sdt: Sdt<'a>) -> Result<Self, AcpiError> {
        let sdt = sdt.expect(DMAR_SIGNATURE, STRUCTURES_OFFSET)?;
        let table = sdt.bytes();
        let mut rest = tail(table, STRUCTURES_OFFSET);
        while !rest.is_empty() {
            let (_, len) = decode_structure(rest)?;
            rest = tail(rest, len);
        }
        Ok(Self {
            sdt,
            host_address_width: u8_at(table, 36)?,
            flags: u8_at(table, 37)?,
        })
    }

    pub fn header(&self) -> &SdtHeader {
        self.sdt.header()
    }

    /// Maximum DMA physical address width in bits (the field stores N-1).
    pub fn host_address_width(&self) -> u16 {
        u16::from(self.host_address_width) + 1
    }

    pub fn flags(&self) -> u8 {
        self.flags
    }

    pub fn interrupt_remapping(&self) -> bool {
        self.flags & INTR_REMAP != 0
    }

    pub fn x2apic_opt_out(&self) -> bool {
        self.flags & X2APIC_OPT_OUT != 0
    }

    pub fn structures(&self) -> DmarStructures<'a> {
        DmarStructures {
            rest: tail(self.sdt.bytes(), STRUCTURES_OFFSET),
        }
    }
}

/// DMA Remapping Hardware Unit Definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Drhd<'a> {
    pub flags: u8,
    /// Register set size as 2^size 4 KiB pages (reserved before VT-d 3.x).
    pub size: u8,
    pub segment: u16,
    pub register_base: u64,
    pub scopes: DeviceScopes<'a>,
}

impl Drhd<'_> {
    pub fn include_pci_all(&self) -> bool {
        self.flags & DRHD_INCLUDE_PCI_ALL != 0
    }
}

/// Reserved Memory Region Reporting; `limit_address` is inclusive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rmrr<'a> {
    pub segment: u16,
    pub base_address: u64,
    pub limit_address: u64,
    pub scopes: DeviceScopes<'a>,
}

/// Root Port ATS Capability Reporting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Atsr<'a> {
    pub flags: u8,
    pub segment: u16,
    pub scopes: DeviceScopes<'a>,
}

impl Atsr<'_> {
    pub fn all_ports(&self) -> bool {
        self.flags & ATSR_ALL_PORTS != 0
    }
}

/// Remapping Hardware Static Affinity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rhsa {
    pub register_base: u64,
    pub proximity_domain: u32,
}

/// ACPI Name-space Device Declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Andd<'a> {
    pub device_number: u8,
    /// ACPI object path without the terminating NUL.
    pub object_name: &'a [u8],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DmarStructure<'a> {
    Drhd(Drhd<'a>),
    Rmrr(Rmrr<'a>),
    Atsr(Atsr<'a>),
    Rhsa(Rhsa),
    Andd(Andd<'a>),
    /// SATC, SIDP and future types; `bytes` is the whole structure.
    Unknown {
        structure_type: u16,
        bytes: &'a [u8],
    },
}

fn decode_structure(rest: &[u8]) -> Result<(DmarStructure<'_>, usize), AcpiError> {
    let structure_type = u16_at(rest, 0).map_err(|_| AcpiError::RecordOverrun)?;
    let len = u16_at(rest, 2).map_err(|_| AcpiError::RecordOverrun)? as usize;
    if len == 0 {
        return Err(AcpiError::ZeroLengthRecord);
    }
    if len < min_len(structure_type) {
        return Err(AcpiError::ShortRecord);
    }
    let r = rest.get(..len).ok_or(AcpiError::RecordOverrun)?;
    let structure = match structure_type {
        TYPE_DRHD => DmarStructure::Drhd(Drhd {
            flags: u8_at(r, 4)?,
            size: u8_at(r, 5)?,
            segment: u16_at(r, 6)?,
            register_base: u64_at(r, 8)?,
            scopes: DeviceScopes::validate(tail(r, 16))?,
        }),
        TYPE_RMRR => {
            let base_address = u64_at(r, 8)?;
            let limit_address = u64_at(r, 16)?;
            if base_address > limit_address {
                return Err(AcpiError::InvalidField);
            }
            DmarStructure::Rmrr(Rmrr {
                segment: u16_at(r, 6)?,
                base_address,
                limit_address,
                scopes: DeviceScopes::validate(tail(r, 24))?,
            })
        }
        TYPE_ATSR => DmarStructure::Atsr(Atsr {
            flags: u8_at(r, 4)?,
            segment: u16_at(r, 6)?,
            scopes: DeviceScopes::validate(tail(r, 8))?,
        }),
        TYPE_RHSA => DmarStructure::Rhsa(Rhsa {
            register_base: u64_at(r, 8)?,
            proximity_domain: u32_at(r, 16)?,
        }),
        TYPE_ANDD => {
            let name = tail(r, 8);
            let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
            DmarStructure::Andd(Andd {
                device_number: u8_at(r, 7)?,
                object_name: name.get(..end).unwrap_or(name),
            })
        }
        _ => DmarStructure::Unknown {
            structure_type,
            bytes: r,
        },
    };
    Ok((structure, len))
}

#[derive(Clone, Debug)]
pub struct DmarStructures<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for DmarStructures<'a> {
    type Item = DmarStructure<'a>;

    fn next(&mut self) -> Option<DmarStructure<'a>> {
        if self.rest.is_empty() {
            return None;
        }
        match decode_structure(self.rest) {
            Ok((structure, len)) => {
                self.rest = tail(self.rest, len);
                Some(structure)
            }
            // Unreachable after `Dmar::from_sdt` validated every structure.
            Err(_) => {
                self.rest = &[];
                None
            }
        }
    }
}

impl FusedIterator for DmarStructures<'_> {}

/// One PCI path hop below `start_bus`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciPathEntry {
    pub device: u8,
    pub function: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceScope<'a> {
    pub scope_type: u8,
    pub flags: u8,
    /// IOAPIC ID, HPET number or ACPI device number, depending on the type.
    pub enumeration_id: u8,
    pub start_bus: u8,
    pub path: DevicePath<'a>,
}

fn decode_scope(rest: &[u8]) -> Result<(DeviceScope<'_>, usize), AcpiError> {
    let scope_type = u8_at(rest, 0)?;
    let len = u8_at(rest, 1).map_err(|_| AcpiError::RecordOverrun)? as usize;
    if len == 0 {
        return Err(AcpiError::ZeroLengthRecord);
    }
    // At least one (device, function) hop.
    if len < SCOPE_HEADER_LEN + 2 {
        return Err(AcpiError::ShortRecord);
    }
    let r = rest.get(..len).ok_or(AcpiError::RecordOverrun)?;
    let path = tail(r, SCOPE_HEADER_LEN);
    if !path.len().is_multiple_of(2) {
        return Err(AcpiError::BadLength);
    }
    if path
        .chunks_exact(2)
        .any(|hop| hop.first().is_some_and(|&d| d > 31) || hop.get(1).is_some_and(|&f| f > 7))
    {
        return Err(AcpiError::InvalidField);
    }
    Ok((
        DeviceScope {
            scope_type,
            flags: u8_at(r, 2)?,
            enumeration_id: u8_at(r, 4)?,
            start_bus: u8_at(r, 5)?,
            path: DevicePath { rest: path },
        },
        len,
    ))
}

/// Device scopes of a DRHD, RMRR or ATSR, validated when the DMAR was parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceScopes<'a> {
    rest: &'a [u8],
}

impl<'a> DeviceScopes<'a> {
    fn validate(region: &'a [u8]) -> Result<Self, AcpiError> {
        let mut rest = region;
        while !rest.is_empty() {
            let (_, len) = decode_scope(rest)?;
            rest = tail(rest, len);
        }
        Ok(Self { rest: region })
    }
}

impl<'a> Iterator for DeviceScopes<'a> {
    type Item = DeviceScope<'a>;

    fn next(&mut self) -> Option<DeviceScope<'a>> {
        if self.rest.is_empty() {
            return None;
        }
        match decode_scope(self.rest) {
            Ok((scope, len)) => {
                self.rest = tail(self.rest, len);
                Some(scope)
            }
            Err(_) => {
                self.rest = &[];
                None
            }
        }
    }
}

impl FusedIterator for DeviceScopes<'_> {}

/// (device, function) hops of a device scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevicePath<'a> {
    rest: &'a [u8],
}

impl DevicePath<'_> {
    pub fn len(&self) -> usize {
        self.rest.len() / 2
    }

    pub fn is_empty(&self) -> bool {
        self.rest.len() < 2
    }
}

impl Iterator for DevicePath<'_> {
    type Item = PciPathEntry;

    fn next(&mut self) -> Option<PciPathEntry> {
        let hop = self.rest.get(..2)?;
        self.rest = tail(self.rest, 2);
        Some(PciPathEntry {
            device: *hop.first()?,
            function: *hop.get(1)?,
        })
    }
}

impl FusedIterator for DevicePath<'_> {}
