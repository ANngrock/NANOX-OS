//! Multiple APIC Description Table (signature `APIC`).

use core::iter::FusedIterator;

use crate::bytes::{tail, u16_at, u32_at, u64_at, u8_at};
use crate::{AcpiError, Sdt, SdtHeader};

pub const MADT_SIGNATURE: [u8; 4] = *b"APIC";
/// Header, Local Interrupt Controller Address and Flags.
const ENTRIES_OFFSET: usize = 44;

/// MADT flag: dual 8259 PICs are present and must be masked.
pub const PCAT_COMPAT: u32 = 1;
/// Local APIC / x2APIC flag: the processor is usable now.
pub const LAPIC_ENABLED: u32 = 1;
/// Local APIC / x2APIC flag (MADT revision 5+): may be enabled at runtime.
pub const LAPIC_ONLINE_CAPABLE: u32 = 2;

pub const TYPE_LOCAL_APIC: u8 = 0;
pub const TYPE_IO_APIC: u8 = 1;
pub const TYPE_INTERRUPT_SOURCE_OVERRIDE: u8 = 2;
pub const TYPE_NMI_SOURCE: u8 = 3;
pub const TYPE_LOCAL_APIC_NMI: u8 = 4;
pub const TYPE_LOCAL_APIC_ADDRESS_OVERRIDE: u8 = 5;
pub const TYPE_LOCAL_X2APIC: u8 = 9;
pub const TYPE_LOCAL_X2APIC_NMI: u8 = 0xa;

/// Minimum entry length per type; unknown types need only their 2-byte header.
fn min_len(entry_type: u8) -> usize {
    match entry_type {
        TYPE_LOCAL_APIC => 8,
        TYPE_IO_APIC => 12,
        TYPE_INTERRUPT_SOURCE_OVERRIDE => 10,
        TYPE_NMI_SOURCE => 8,
        TYPE_LOCAL_APIC_NMI => 6,
        TYPE_LOCAL_APIC_ADDRESS_OVERRIDE => 12,
        TYPE_LOCAL_X2APIC => 16,
        TYPE_LOCAL_X2APIC_NMI => 12,
        _ => 2,
    }
}

/// A verified MADT whose every entry has been decoded once.
#[derive(Clone, Copy, Debug)]
pub struct Madt<'a> {
    sdt: Sdt<'a>,
    local_apic_address: u32,
    flags: u32,
    address_override: Option<u64>,
}

impl<'a> Madt<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, AcpiError> {
        Self::from_sdt(Sdt::parse(bytes)?)
    }

    /// The MADT revision is not bounded: entries are self-describing and
    /// unknown types are reported as [`MadtEntry::Unknown`].
    pub fn from_sdt(sdt: Sdt<'a>) -> Result<Self, AcpiError> {
        let sdt = sdt.expect(MADT_SIGNATURE, ENTRIES_OFFSET)?;
        let table = sdt.bytes();
        let mut address_override = None;
        let mut rest = tail(table, ENTRIES_OFFSET);
        while !rest.is_empty() {
            let (entry, len) = decode_entry(rest)?;
            if let MadtEntry::LocalApicAddressOverride(address) = entry {
                // The specification allows at most one override.
                if address_override.replace(address).is_some() {
                    return Err(AcpiError::InvalidField);
                }
            }
            rest = tail(rest, len);
        }
        Ok(Self {
            sdt,
            local_apic_address: u32_at(table, 36)?,
            flags: u32_at(table, 40)?,
            address_override,
        })
    }

    pub fn header(&self) -> &SdtHeader {
        self.sdt.header()
    }

    /// The 32-bit Local Interrupt Controller Address field.
    pub fn local_apic_address(&self) -> u32 {
        self.local_apic_address
    }

    /// The Local APIC Address Override entry wins over the 32-bit field.
    pub fn effective_local_apic_address(&self) -> u64 {
        self.address_override
            .unwrap_or(u64::from(self.local_apic_address))
    }

    pub fn flags(&self) -> u32 {
        self.flags
    }

    pub fn pcat_compat(&self) -> bool {
        self.flags & PCAT_COMPAT != 0
    }

    pub fn entries(&self) -> MadtEntries<'a> {
        MadtEntries {
            rest: tail(self.sdt.bytes(), ENTRIES_OFFSET),
        }
    }
}

/// MPS INTI flags of overrides and NMI entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MpsIntiFlags(pub u16);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    BusDefault,
    ActiveHigh,
    Reserved,
    ActiveLow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerMode {
    BusDefault,
    Edge,
    Reserved,
    Level,
}

impl MpsIntiFlags {
    pub fn polarity(self) -> Polarity {
        match self.0 & 3 {
            0 => Polarity::BusDefault,
            1 => Polarity::ActiveHigh,
            2 => Polarity::Reserved,
            _ => Polarity::ActiveLow,
        }
    }

    pub fn trigger_mode(self) -> TriggerMode {
        match (self.0 >> 2) & 3 {
            0 => TriggerMode::BusDefault,
            1 => TriggerMode::Edge,
            2 => TriggerMode::Reserved,
            _ => TriggerMode::Level,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalApic {
    pub processor_uid: u8,
    pub apic_id: u8,
    pub flags: u32,
}

impl LocalApic {
    pub fn enabled(&self) -> bool {
        self.flags & LAPIC_ENABLED != 0
    }

    pub fn online_capable(&self) -> bool {
        self.flags & LAPIC_ONLINE_CAPABLE != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalX2Apic {
    pub x2apic_id: u32,
    pub flags: u32,
    pub processor_uid: u32,
}

impl LocalX2Apic {
    pub fn enabled(&self) -> bool {
        self.flags & LAPIC_ENABLED != 0
    }

    pub fn online_capable(&self) -> bool {
        self.flags & LAPIC_ONLINE_CAPABLE != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApic {
    pub io_apic_id: u8,
    pub address: u32,
    pub gsi_base: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterruptSourceOverride {
    pub bus: u8,
    pub source: u8,
    pub gsi: u32,
    pub flags: MpsIntiFlags,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NmiSource {
    pub flags: MpsIntiFlags,
    pub gsi: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalApicNmi {
    /// 0xff means all processors.
    pub processor_uid: u8,
    pub flags: MpsIntiFlags,
    pub lint: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct X2ApicNmi {
    pub flags: MpsIntiFlags,
    /// 0xffff_ffff means all processors.
    pub processor_uid: u32,
    pub lint: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MadtEntry<'a> {
    LocalApic(LocalApic),
    IoApic(IoApic),
    InterruptSourceOverride(InterruptSourceOverride),
    NmiSource(NmiSource),
    LocalApicNmi(LocalApicNmi),
    LocalApicAddressOverride(u64),
    LocalX2Apic(LocalX2Apic),
    X2ApicNmi(X2ApicNmi),
    /// Any other type; `bytes` is the whole entry including its header.
    Unknown {
        entry_type: u8,
        bytes: &'a [u8],
    },
}

/// Decode one entry at the start of `rest` (non-empty) and return its length.
fn decode_entry(rest: &[u8]) -> Result<(MadtEntry<'_>, usize), AcpiError> {
    let entry_type = u8_at(rest, 0)?;
    let len = rest.get(1).copied().ok_or(AcpiError::RecordOverrun)? as usize;
    if len == 0 {
        return Err(AcpiError::ZeroLengthRecord);
    }
    if len < min_len(entry_type) {
        return Err(AcpiError::ShortRecord);
    }
    let r = rest.get(..len).ok_or(AcpiError::RecordOverrun)?;
    let entry = match entry_type {
        TYPE_LOCAL_APIC => MadtEntry::LocalApic(LocalApic {
            processor_uid: u8_at(r, 2)?,
            apic_id: u8_at(r, 3)?,
            flags: u32_at(r, 4)?,
        }),
        TYPE_IO_APIC => MadtEntry::IoApic(IoApic {
            io_apic_id: u8_at(r, 2)?,
            address: u32_at(r, 4)?,
            gsi_base: u32_at(r, 8)?,
        }),
        TYPE_INTERRUPT_SOURCE_OVERRIDE => {
            MadtEntry::InterruptSourceOverride(InterruptSourceOverride {
                bus: u8_at(r, 2)?,
                source: u8_at(r, 3)?,
                gsi: u32_at(r, 4)?,
                flags: MpsIntiFlags(u16_at(r, 8)?),
            })
        }
        TYPE_NMI_SOURCE => MadtEntry::NmiSource(NmiSource {
            flags: MpsIntiFlags(u16_at(r, 2)?),
            gsi: u32_at(r, 4)?,
        }),
        TYPE_LOCAL_APIC_NMI => MadtEntry::LocalApicNmi(LocalApicNmi {
            processor_uid: u8_at(r, 2)?,
            flags: MpsIntiFlags(u16_at(r, 3)?),
            lint: u8_at(r, 5)?,
        }),
        TYPE_LOCAL_APIC_ADDRESS_OVERRIDE => MadtEntry::LocalApicAddressOverride(u64_at(r, 4)?),
        TYPE_LOCAL_X2APIC => MadtEntry::LocalX2Apic(LocalX2Apic {
            x2apic_id: u32_at(r, 4)?,
            flags: u32_at(r, 8)?,
            processor_uid: u32_at(r, 12)?,
        }),
        TYPE_LOCAL_X2APIC_NMI => MadtEntry::X2ApicNmi(X2ApicNmi {
            flags: MpsIntiFlags(u16_at(r, 2)?),
            processor_uid: u32_at(r, 4)?,
            lint: u8_at(r, 8)?,
        }),
        _ => MadtEntry::Unknown {
            entry_type,
            bytes: r,
        },
    };
    Ok((entry, len))
}

#[derive(Clone, Debug)]
pub struct MadtEntries<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for MadtEntries<'a> {
    type Item = MadtEntry<'a>;

    fn next(&mut self) -> Option<MadtEntry<'a>> {
        if self.rest.is_empty() {
            return None;
        }
        match decode_entry(self.rest) {
            Ok((entry, len)) => {
                self.rest = tail(self.rest, len);
                Some(entry)
            }
            // Unreachable after `Madt::from_sdt` validated every entry; stop
            // instead of panicking.
            Err(_) => {
                self.rest = &[];
                None
            }
        }
    }
}

impl FusedIterator for MadtEntries<'_> {}
