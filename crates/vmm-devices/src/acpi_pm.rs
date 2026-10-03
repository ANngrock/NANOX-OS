//! The ACPI power-management hardware of a q35-style chipset, at the ports
//! the guest's FADT names: PM1a event block (status 0x600, enable 0x602),
//! PM1a control (0x604), the 3.579545 MHz power-management timer (0x608,
//! 24 bits), the general-purpose event block GPE0 (status 0x620..0x627,
//! enable 0x628..0x62F), and the SMI command / APM ports 0xB2/0xB3 through
//! which the OS switches the machine into ACPI mode.
//!
//! What the guest relies on: writing the ACPI-enable command to 0xB2 sets
//! SCI_EN; the SCI line is high while SCI_EN is set and an enabled event is
//! pending; status bits clear by writing 1; the timer counts in virtual time
//! and its overflow (bit 23) is an event; the power button is an event the
//! host raises ([`AcpiPm::press_power_button`], how a Proxmox "shutdown"
//! reaches the guest); a write of SLP_EN with SLP_TYP = 5 is the guest
//! powering off ([`AcpiPm::take_sleep`]). Not modeled: bus-master reload,
//! the global lock (GBL_RLS is stored and ignored), wake events and
//! the SMI side of the chipset (writes to 0xB2 other than enable/disable are counted).

pub const PM1_STATUS: u16 = 0x600;
pub const PM1_ENABLE: u16 = 0x602;
pub const PM1_CONTROL: u16 = 0x604;
pub const PM_TIMER: u16 = 0x608;
pub const GPE0_STATUS: u16 = 0x620;
pub const GPE0_ENABLE: u16 = 0x628;
pub const GPE0_END: u16 = 0x62F;
pub const SMI_CMD: u16 = 0xB2;
pub const APM_STATUS: u16 = 0xB3;
/// What the guest writes to `SMI_CMD` to enter and leave ACPI mode (the FADT's values).
pub const ACPI_ENABLE: u8 = 0x02;
pub const ACPI_DISABLE: u8 = 0x03;
pub const TIMER_HZ: u64 = 3_579_545;
/// SLP_TYP for soft-off.
pub const SLEEP_S5: u8 = 5;

pub const STS_TMR: u16 = 1 << 0;
pub const STS_GBL: u16 = 1 << 5;
pub const STS_PWRBTN: u16 = 1 << 8;
pub const STS_SLPBTN: u16 = 1 << 9;
pub const STS_RTC: u16 = 1 << 10;
pub const STS_WAK: u16 = 1 << 15;
/// PM1 status bits this model keeps (the others read zero and ignore writes).
const PM1_BITS: u16 = STS_TMR | STS_GBL | STS_PWRBTN | STS_SLPBTN | STS_RTC | STS_WAK;
const CNT_SCI_EN: u16 = 1;
const CNT_SLP_EN: u16 = 1 << 13;
const CNT_KEEP: u16 = 0x1C07;
const TIMER_MASK: u64 = 0x00FF_FFFF;
const TIMER_MSB: u64 = 1 << 23;

#[derive(Clone, Debug)]
pub struct AcpiPm {
    pm1_sts: u16,
    pm1_en: u16,
    pm1_cnt: u16,
    gpe_sts: u64,
    gpe_en: u64,
    apm: u8,
    sleep: Option<u8>,
    synced: u64,
    pub unsupported: u32,
}

impl Default for AcpiPm {
    fn default() -> Self {
        Self::new()
    }
}

/// Which register a port is in: (name, first port, width in bytes).
fn locate(port: u16) -> Option<(u8, u16, u8)> {
    Some(match port {
        0x600..=0x601 => (0, PM1_STATUS, 2),
        0x602..=0x603 => (1, PM1_ENABLE, 2),
        0x604..=0x605 => (2, PM1_CONTROL, 2),
        0x608..=0x60B => (3, PM_TIMER, 4),
        0x620..=0x627 => (4, GPE0_STATUS, 8),
        0x628..=0x62F => (5, GPE0_ENABLE, 8),
        _ => return None,
    })
}

fn timer_ticks(now: u64) -> u64 {
    (u128::from(now) * u128::from(TIMER_HZ) / 1_000_000_000) as u64
}

impl AcpiPm {
    pub const fn new() -> Self {
        Self {
            pm1_sts: 0,
            pm1_en: 0,
            pm1_cnt: 0,
            gpe_sts: 0,
            gpe_en: 0,
            apm: 0,
            sleep: None,
            synced: 0,
            unsupported: 0,
        }
    }

    pub fn owns(port: u16) -> bool {
        locate(port).is_some() || port == SMI_CMD || port == APM_STATUS
    }

    /// Is ACPI mode on (SCI_EN)?
    pub fn acpi_enabled(&self) -> bool {
        self.pm1_cnt & CNT_SCI_EN != 0
    }

    /// The host presses the power button: the guest sees PWRBTN_STS (and an SCI if it enabled it).
    pub fn press_power_button(&mut self) {
        self.pm1_sts |= STS_PWRBTN;
    }

    /// The host raises general-purpose event `bit` (0..63).
    pub fn raise_gpe(&mut self, bit: u8) {
        if bit < 64 {
            self.gpe_sts |= 1 << bit;
        }
    }

    /// SLP_TYP of a sleep request the guest made (SLP_EN written); reading clears it. `SLEEP_S5` is power off.
    pub fn take_sleep(&mut self) -> Option<u8> {
        self.sleep.take()
    }

    /// Raises TMR_STS for every time bit 23 of the timer toggled since the last call.
    pub fn sync(&mut self, now: u64) {
        let (a, b) = (timer_ticks(self.synced), timer_ticks(now));
        if now > self.synced {
            if (b / TIMER_MSB) != (a / TIMER_MSB) {
                self.pm1_sts |= STS_TMR;
            }
            self.synced = now;
        }
    }

    /// The SCI line at `now`.
    pub fn sci(&mut self, now: u64) -> bool {
        self.sync(now);
        self.acpi_enabled()
            && (self.pm1_sts & self.pm1_en & PM1_BITS != 0 || self.gpe_sts & self.gpe_en != 0)
    }

    /// When the VMM must look at the timer next (its next bit-23 toggle), if the guest enabled its event.
    pub fn next_event(&self, now: u64) -> Option<u64> {
        if self.pm1_en & STS_TMR == 0 {
            return None;
        }
        let next_tick = (timer_ticks(now) / TIMER_MSB + 1) * TIMER_MSB;
        Some(((u128::from(next_tick) * 1_000_000_000).div_ceil(u128::from(TIMER_HZ))) as u64)
    }

    fn reg(&mut self, which: u8, now: u64) -> u64 {
        self.sync(now);
        match which {
            0 => u64::from(self.pm1_sts),
            1 => u64::from(self.pm1_en),
            2 => u64::from(self.pm1_cnt),
            3 => timer_ticks(now) & TIMER_MASK,
            4 => self.gpe_sts,
            _ => self.gpe_en,
        }
    }

    /// Read of `size` (1, 2 or 4) bytes from `port`; None if the port is not claimed or the access straddles registers.
    pub fn read(&mut self, port: u16, size: u8, now: u64) -> Option<u32> {
        if !matches!(size, 1 | 2 | 4) {
            return None;
        }
        match port {
            SMI_CMD => return Some(0),
            APM_STATUS => return Some(u32::from(self.apm)),
            _ => {}
        }
        let (which, base, width) = locate(port)?;
        let shift = 8 * u32::from(port - base);
        if u16::from(size) + (port - base) > u16::from(width) {
            return None;
        }
        let v = self.reg(which, now) >> shift;
        Some(match size {
            1 => (v & 0xFF) as u32,
            2 => (v & 0xFFFF) as u32,
            _ => (v & 0xFFFF_FFFF) as u32,
        })
    }

    /// Write of `size` (1, 2 or 4) bytes to `port`; false if the port is not claimed or the access straddles registers.
    pub fn write(&mut self, port: u16, size: u8, value: u32, now: u64) -> bool {
        if !matches!(size, 1 | 2 | 4) {
            return false;
        }
        match port {
            SMI_CMD => {
                match value as u8 {
                    ACPI_ENABLE => self.pm1_cnt |= CNT_SCI_EN,
                    ACPI_DISABLE => self.pm1_cnt &= !CNT_SCI_EN,
                    _ => self.unsupported += 1,
                }
                return true;
            }
            APM_STATUS => {
                self.apm = value as u8;
                return true;
            }
            _ => {}
        }
        let Some((which, base, width)) = locate(port) else {
            return false;
        };
        if u16::from(size) + (port - base) > u16::from(width) {
            return false;
        }
        self.sync(now);
        let shift = 8 * u32::from(port - base);
        let mask = match size {
            1 => 0xFFu64,
            2 => 0xFFFF,
            _ => 0xFFFF_FFFF,
        } << shift;
        let v = (u64::from(value) << shift) & mask;
        match which {
            // Status registers: write 1 to clear the bits in the written bytes.
            0 => self.pm1_sts &= !(v as u16),
            4 => self.gpe_sts &= !v,
            1 => self.pm1_en = (self.pm1_en & !(mask as u16)) | (v as u16 & PM1_BITS),
            5 => self.gpe_en = (self.gpe_en & !mask) | v,
            2 => {
                let old = u64::from(self.pm1_cnt);
                let new = (old & !mask) | v;
                if new & u64::from(CNT_SLP_EN) != 0 {
                    self.sleep = Some(((new >> 10) & 7) as u8);
                }
                self.pm1_cnt = (new as u16) & CNT_KEEP;
            }
            _ => {} // the timer is read-only
        }
        true
    }
}
