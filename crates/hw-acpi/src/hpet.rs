//! IA-PC High Precision Event Timer table (signature `HPET`).

use crate::bytes::{u16_at, u32_at, u8_at};
use crate::{AcpiError, Gas, Sdt, SdtHeader};

pub const HPET_SIGNATURE: [u8; 4] = *b"HPET";
const HPET_LEN: usize = 56;

/// A verified HPET description. The timer block must be memory-mapped at a
/// non-zero address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hpet {
    pub header: SdtHeader,
    /// Copy of the General Capabilities register bits 31:0.
    pub event_timer_block_id: u32,
    pub base_address: Gas,
    pub hpet_number: u8,
    /// Minimum main-counter ticks for periodic mode without lost interrupts.
    pub minimum_tick: u16,
    pub page_protection: u8,
}

impl Hpet {
    pub fn parse(bytes: &[u8]) -> Result<Self, AcpiError> {
        Self::from_sdt(Sdt::parse(bytes)?)
    }

    pub fn from_sdt(sdt: Sdt<'_>) -> Result<Self, AcpiError> {
        let sdt = sdt.expect(HPET_SIGNATURE, HPET_LEN)?;
        let b = sdt.bytes();
        let hpet = Self {
            header: *sdt.header(),
            event_timer_block_id: u32_at(b, 36)?,
            base_address: Gas::at(b, 40)?,
            hpet_number: u8_at(b, 52)?,
            minimum_tick: u16_at(b, 53)?,
            page_protection: u8_at(b, 55)?,
        };
        if hpet.base_address.address_space != Gas::SYSTEM_MEMORY || hpet.base_address.address == 0 {
            return Err(AcpiError::InvalidField);
        }
        Ok(hpet)
    }

    pub fn hardware_revision(&self) -> u8 {
        self.event_timer_block_id as u8
    }

    /// Number of comparators (the field stores the last comparator index).
    pub fn comparator_count(&self) -> u8 {
        ((self.event_timer_block_id >> 8) & 0x1f) as u8 + 1
    }

    pub fn counter_is_64bit(&self) -> bool {
        self.event_timer_block_id & (1 << 13) != 0
    }

    pub fn legacy_replacement_capable(&self) -> bool {
        self.event_timer_block_id & (1 << 15) != 0
    }

    pub fn pci_vendor_id(&self) -> u16 {
        (self.event_timer_block_id >> 16) as u16
    }
}
