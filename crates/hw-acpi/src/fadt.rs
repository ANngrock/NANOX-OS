//! Fixed ACPI Description Table (signature `FACP`).
//!
//! The FADT grew over ACPI revisions by appending fields: 116 bytes in ACPI
//! 1.0 (revision 1), 244 bytes from ACPI 2.0 (revision 3), 268 bytes with
//! the sleep registers (revision 5) and 276 bytes with the hypervisor vendor
//! identity (revision 6). Every accessor for a later field checks the actual
//! table length, so a short FADT simply reports those fields as absent.

use crate::bytes::{u16_at, u32_at, u64_at, u8_at};
use crate::{AcpiError, Gas, Sdt, SdtHeader};

pub const FADT_SIGNATURE: [u8; 4] = *b"FACP";
/// Length of the ACPI 1.0 FADT; nothing shorter is accepted.
pub const FADT_MIN_LEN: usize = 116;
/// Highest FADT major revision whose layout this parser knows (ACPI 6.x).
pub const FADT_MAX_REVISION: u8 = 6;

const FIRMWARE_CTRL: usize = 36;
const DSDT: usize = 40;
const PREFERRED_PM_PROFILE: usize = 45;
const SCI_INT: usize = 46;
const SMI_CMD: usize = 48;
const ACPI_ENABLE: usize = 52;
const ACPI_DISABLE: usize = 53;
const PM1A_EVT_BLK: usize = 56;
const PM1B_EVT_BLK: usize = 60;
const PM1A_CNT_BLK: usize = 64;
const PM1B_CNT_BLK: usize = 68;
const PM2_CNT_BLK: usize = 72;
const PM_TMR_BLK: usize = 76;
const GPE0_BLK: usize = 80;
const GPE1_BLK: usize = 84;
const PM1_EVT_LEN: usize = 88;
const PM1_CNT_LEN: usize = 89;
const PM2_CNT_LEN: usize = 90;
const PM_TMR_LEN: usize = 91;
const GPE0_BLK_LEN: usize = 92;
const GPE1_BLK_LEN: usize = 93;
const CENTURY: usize = 108;
const IAPC_BOOT_ARCH: usize = 109;
const FLAGS: usize = 112;
const RESET_REG: usize = 116;
const RESET_VALUE: usize = 128;
const MINOR_VERSION: usize = 131;
const X_FIRMWARE_CTRL: usize = 132;
const X_DSDT: usize = 140;
const X_PM1A_EVT_BLK: usize = 148;
const X_PM1B_EVT_BLK: usize = 160;
const X_PM1A_CNT_BLK: usize = 172;
const X_PM1B_CNT_BLK: usize = 184;
const X_PM2_CNT_BLK: usize = 196;
const X_PM_TMR_BLK: usize = 208;
const X_GPE0_BLK: usize = 220;
const X_GPE1_BLK: usize = 232;
const SLEEP_CONTROL_REG: usize = 244;
const SLEEP_STATUS_REG: usize = 256;
const HYPERVISOR_VENDOR_ID: usize = 268;

/// FADT fixed feature flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FadtFlags(pub u32);

impl FadtFlags {
    pub const WBINVD: Self = Self(1 << 0);
    pub const PROC_C1: Self = Self(1 << 2);
    pub const PWR_BUTTON: Self = Self(1 << 4);
    pub const SLP_BUTTON: Self = Self(1 << 5);
    pub const RTC_S4: Self = Self(1 << 7);
    /// The PM timer counter is 32 bits wide (24 bits otherwise).
    pub const TMR_VAL_EXT: Self = Self(1 << 8);
    pub const RESET_REG_SUP: Self = Self(1 << 10);
    pub const HEADLESS: Self = Self(1 << 12);
    pub const PCI_EXP_WAK: Self = Self(1 << 14);
    pub const USE_PLATFORM_CLOCK: Self = Self(1 << 15);
    pub const FORCE_APIC_CLUSTER_MODEL: Self = Self(1 << 18);
    pub const FORCE_APIC_PHYSICAL_DESTINATION_MODE: Self = Self(1 << 19);
    pub const HW_REDUCED_ACPI: Self = Self(1 << 20);
    pub const LOW_POWER_S0_IDLE_CAPABLE: Self = Self(1 << 21);

    pub fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PmTimer {
    pub register: Gas,
    /// `TMR_VAL_EXT`: the counter is 32 bits wide instead of 24.
    pub counter_is_32bit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResetRegister {
    pub register: Gas,
    pub value: u8,
}

/// A verified FADT of revision 1..=[`FADT_MAX_REVISION`], at least 116 bytes.
#[derive(Clone, Copy, Debug)]
pub struct Fadt<'a> {
    sdt: Sdt<'a>,
}

impl<'a> Fadt<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Self, AcpiError> {
        Self::from_sdt(Sdt::parse(bytes)?)
    }

    pub fn from_sdt(sdt: Sdt<'a>) -> Result<Self, AcpiError> {
        let sdt = sdt.expect(FADT_SIGNATURE, FADT_MIN_LEN)?;
        let revision = sdt.header().revision;
        if revision == 0 || revision > FADT_MAX_REVISION {
            return Err(AcpiError::UnsupportedRevision);
        }
        Ok(Self { sdt })
    }

    pub fn header(&self) -> &SdtHeader {
        self.sdt.header()
    }

    pub fn revision(&self) -> u8 {
        self.sdt.header().revision
    }

    /// FADT minor version (ACPI 5.1+); `None` when the table is too short.
    pub fn minor_version(&self) -> Option<u8> {
        u8_at(self.bytes(), MINOR_VERSION).ok()
    }

    // Fields below FADT_MIN_LEN always exist after `from_sdt`; the fallback
    // value is never observed.
    fn fixed_u8(&self, offset: usize) -> u8 {
        u8_at(self.bytes(), offset).unwrap_or(0)
    }

    fn fixed_u16(&self, offset: usize) -> u16 {
        u16_at(self.bytes(), offset).unwrap_or(0)
    }

    fn fixed_u32(&self, offset: usize) -> u32 {
        u32_at(self.bytes(), offset).unwrap_or(0)
    }

    fn bytes(&self) -> &'a [u8] {
        self.sdt.bytes()
    }

    /// 64-bit X_ field if the table reaches it and it is non-zero, else the
    /// 32-bit field if non-zero.
    fn address(&self, x_offset: usize, offset: usize) -> Option<u64> {
        u64_at(self.bytes(), x_offset)
            .ok()
            .filter(|&a| a != 0)
            .or_else(|| Some(u64::from(self.fixed_u32(offset))).filter(|&a| a != 0))
    }

    /// X_ GAS if the table reaches it and its address is non-zero, else the
    /// 32-bit I/O port block described as a GAS of `len` bytes.
    fn block(&self, x_offset: usize, offset: usize, len_offset: usize) -> Option<Gas> {
        Gas::at(self.bytes(), x_offset)
            .ok()
            .filter(|gas| gas.address != 0)
            .or_else(|| {
                let port = self.fixed_u32(offset);
                (port != 0).then(|| Gas {
                    address_space: Gas::SYSTEM_IO,
                    bit_width: self.fixed_u8(len_offset).saturating_mul(8),
                    bit_offset: 0,
                    access_size: 0,
                    address: u64::from(port),
                })
            })
    }

    fn optional_gas(&self, offset: usize) -> Option<Gas> {
        Gas::at(self.bytes(), offset)
            .ok()
            .filter(|gas| gas.address != 0)
    }

    /// Physical address of the FACS (X_FIRMWARE_CTRL has priority).
    pub fn firmware_ctrl(&self) -> Option<u64> {
        self.address(X_FIRMWARE_CTRL, FIRMWARE_CTRL)
    }

    /// Physical address of the DSDT (X_DSDT has priority).
    pub fn dsdt(&self) -> Option<u64> {
        self.address(X_DSDT, DSDT)
    }

    pub fn preferred_pm_profile(&self) -> u8 {
        self.fixed_u8(PREFERRED_PM_PROFILE)
    }

    pub fn sci_interrupt(&self) -> u16 {
        self.fixed_u16(SCI_INT)
    }

    pub fn smi_command_port(&self) -> u32 {
        self.fixed_u32(SMI_CMD)
    }

    pub fn acpi_enable(&self) -> u8 {
        self.fixed_u8(ACPI_ENABLE)
    }

    pub fn acpi_disable(&self) -> u8 {
        self.fixed_u8(ACPI_DISABLE)
    }

    pub fn century_register(&self) -> u8 {
        self.fixed_u8(CENTURY)
    }

    /// IA-PC boot architecture flags (reserved, normally 0, in ACPI 1.0).
    pub fn iapc_boot_arch(&self) -> u16 {
        self.fixed_u16(IAPC_BOOT_ARCH)
    }

    pub fn flags(&self) -> FadtFlags {
        FadtFlags(self.fixed_u32(FLAGS))
    }

    pub fn pm1a_event_block(&self) -> Option<Gas> {
        self.block(X_PM1A_EVT_BLK, PM1A_EVT_BLK, PM1_EVT_LEN)
    }

    pub fn pm1b_event_block(&self) -> Option<Gas> {
        self.block(X_PM1B_EVT_BLK, PM1B_EVT_BLK, PM1_EVT_LEN)
    }

    pub fn pm1a_control_block(&self) -> Option<Gas> {
        self.block(X_PM1A_CNT_BLK, PM1A_CNT_BLK, PM1_CNT_LEN)
    }

    pub fn pm1b_control_block(&self) -> Option<Gas> {
        self.block(X_PM1B_CNT_BLK, PM1B_CNT_BLK, PM1_CNT_LEN)
    }

    pub fn pm2_control_block(&self) -> Option<Gas> {
        self.block(X_PM2_CNT_BLK, PM2_CNT_BLK, PM2_CNT_LEN)
    }

    pub fn gpe0_block(&self) -> Option<Gas> {
        self.block(X_GPE0_BLK, GPE0_BLK, GPE0_BLK_LEN)
    }

    pub fn gpe1_block(&self) -> Option<Gas> {
        self.block(X_GPE1_BLK, GPE1_BLK, GPE1_BLK_LEN)
    }

    /// ACPI PM timer; `None` on hardware-reduced platforms without one.
    pub fn pm_timer(&self) -> Option<PmTimer> {
        self.block(X_PM_TMR_BLK, PM_TMR_BLK, PM_TMR_LEN)
            .map(|register| PmTimer {
                register,
                counter_is_32bit: self.flags().contains(FadtFlags::TMR_VAL_EXT),
            })
    }

    /// Reset register, present when the table reaches RESET_VALUE, the
    /// `RESET_REG_SUP` flag is set and the register address is non-zero.
    pub fn reset_register(&self) -> Option<ResetRegister> {
        if !self.flags().contains(FadtFlags::RESET_REG_SUP) {
            return None;
        }
        let value = u8_at(self.bytes(), RESET_VALUE).ok()?;
        self.optional_gas(RESET_REG)
            .map(|register| ResetRegister { register, value })
    }

    /// Hardware-reduced sleep control register (FADT revision 5+).
    pub fn sleep_control_register(&self) -> Option<Gas> {
        self.optional_gas(SLEEP_CONTROL_REG)
    }

    /// Hardware-reduced sleep status register (FADT revision 5+).
    pub fn sleep_status_register(&self) -> Option<Gas> {
        self.optional_gas(SLEEP_STATUS_REG)
    }

    /// Hypervisor vendor identity (FADT revision 6+), when the table reaches it.
    pub fn hypervisor_vendor_id(&self) -> Option<u64> {
        u64_at(self.bytes(), HYPERVISOR_VENDOR_ID).ok()
    }
}
