//! PCI Express memory-mapped configuration table (signature `MCFG`).

use core::iter::FusedIterator;

use crate::bytes::{tail, u16_at, u64_at, u8_at};
use crate::walk::PHYS_LIMIT;
use crate::{AcpiError, Sdt, SdtHeader};

pub const MCFG_SIGNATURE: [u8; 4] = *b"MCFG";
/// Header plus 8 reserved bytes.
const ENTRIES_OFFSET: usize = 44;
const ENTRY_LEN: usize = 16;
/// ECAM bytes per bus: 32 devices x 8 functions x 4 KiB.
pub const ECAM_BUS_SIZE: u64 = 1 << 20;

/// One ECAM window. `base_address` is where bus 0 of the segment would be,
/// even when `start_bus` is above 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct McfgSegment {
    pub base_address: u64,
    pub segment: u16,
    pub start_bus: u8,
    pub end_bus: u8,
}

impl McfgSegment {
    pub fn contains_bus(&self, bus: u8) -> bool {
        (self.start_bus..=self.end_bus).contains(&bus)
    }

    pub fn bus_count(&self) -> u16 {
        u16::from(self.end_bus) - u16::from(self.start_bus) + 1
    }

    fn decode(entry: &[u8]) -> Result<Self, AcpiError> {
        Ok(Self {
            base_address: u64_at(entry, 0)?,
            segment: u16_at(entry, 8)?,
            start_bus: u8_at(entry, 10)?,
            end_bus: u8_at(entry, 11)?,
        })
    }

    fn validate(&self) -> Result<(), AcpiError> {
        if self.start_bus > self.end_bus {
            return Err(AcpiError::InvalidField);
        }
        // The window must reach the end bus without wrapping or leaving the
        // physical address space.
        let end = self
            .base_address
            .checked_add((u64::from(self.end_bus) + 1) * ECAM_BUS_SIZE)
            .ok_or(AcpiError::AddressOverflow)?;
        if end > PHYS_LIMIT {
            return Err(AcpiError::AddressOverflow);
        }
        Ok(())
    }

    fn overlaps(&self, other: &Self) -> bool {
        self.segment == other.segment
            && self.start_bus <= other.end_bus
            && other.start_bus <= self.end_bus
    }
}

/// A verified MCFG: whole 16-byte entries, ordered bus ranges, no overflow
/// and no two windows claiming the same bus of the same segment.
#[derive(Clone, Copy, Debug)]
pub struct Mcfg<'a> {
    sdt: Sdt<'a>,
}

impl<'a> Mcfg<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, AcpiError> {
        Self::from_sdt(Sdt::parse(bytes)?)
    }

    pub fn from_sdt(sdt: Sdt<'a>) -> Result<Self, AcpiError> {
        let sdt = sdt.expect(MCFG_SIGNATURE, ENTRIES_OFFSET)?;
        let entries = tail(sdt.bytes(), ENTRIES_OFFSET);
        if !entries.len().is_multiple_of(ENTRY_LEN) {
            return Err(AcpiError::BadLength);
        }
        let mcfg = Self { sdt };
        for (index, segment) in mcfg.segments().enumerate() {
            segment.validate()?;
            if mcfg
                .segments()
                .take(index)
                .any(|earlier| earlier.overlaps(&segment))
            {
                return Err(AcpiError::InvalidField);
            }
        }
        Ok(mcfg)
    }

    pub fn header(&self) -> &SdtHeader {
        self.sdt.header()
    }

    pub fn len(&self) -> usize {
        tail(self.sdt.bytes(), ENTRIES_OFFSET).len() / ENTRY_LEN
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn segments(&self) -> McfgSegments<'a> {
        McfgSegments {
            rest: tail(self.sdt.bytes(), ENTRIES_OFFSET),
        }
    }
}

#[derive(Clone, Debug)]
pub struct McfgSegments<'a> {
    rest: &'a [u8],
}

impl Iterator for McfgSegments<'_> {
    type Item = McfgSegment;

    fn next(&mut self) -> Option<McfgSegment> {
        let entry = self.rest.get(..ENTRY_LEN)?;
        self.rest = tail(self.rest, ENTRY_LEN);
        McfgSegment::decode(entry).ok()
    }
}

impl FusedIterator for McfgSegments<'_> {}
