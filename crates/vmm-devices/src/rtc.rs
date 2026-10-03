//! The MC146818 real-time clock and the 128 bytes of CMOS behind it (ports
//! 0x70 index, 0x71 data), as a guest kernel uses them: read and set the time
//! (BCD or binary, 12 or 24 hours, the SET freeze), the alarm, the periodic
//! interrupt, the update-ended interrupt, status registers A..D, and the
//! NVRAM bytes the firmware leaves for the OS (memory size, century 0x32).
//!
//! Time is virtual: the caller passes nanoseconds and gets the same answer
//! every run. Interrupts are flags in register C raised by [`Rtc::sync`]
//! (called by every access, and by the VMM at [`Rtc::next_event`]); the
//! output [`Rtc::irq`] is IRQ8. Not modeled, and counted: the divider
//! settings other than 32.768 kHz, the square-wave output, the second CMOS
//! bank at 0x72/0x73 (not claimed), daylight-saving and the update cycle
//! (UIP is high for the last 244 µs before each second, as the datasheet says).

pub const PORT_INDEX: u16 = 0x70;
pub const PORT_DATA: u16 = 0x71;

pub const REG_SECONDS: u8 = 0;
pub const REG_SECONDS_ALARM: u8 = 1;
pub const REG_MINUTES: u8 = 2;
pub const REG_MINUTES_ALARM: u8 = 3;
pub const REG_HOURS: u8 = 4;
pub const REG_HOURS_ALARM: u8 = 5;
pub const REG_WEEKDAY: u8 = 6;
pub const REG_DAY: u8 = 7;
pub const REG_MONTH: u8 = 8;
pub const REG_YEAR: u8 = 9;
pub const REG_A: u8 = 0x0A;
pub const REG_B: u8 = 0x0B;
pub const REG_C: u8 = 0x0C;
pub const REG_D: u8 = 0x0D;
pub const REG_CENTURY: u8 = 0x32;

const A_UIP: u8 = 0x80;
const B_SET: u8 = 0x80;
const B_PIE: u8 = 0x40;
const B_AIE: u8 = 0x20;
const B_UIE: u8 = 0x10;
const B_DM: u8 = 0x04;
const B_24H: u8 = 0x02;
const C_IRQF: u8 = 0x80;
const C_PF: u8 = 0x40;
const C_AF: u8 = 0x20;
const C_UF: u8 = 0x10;
const NS: u64 = 1_000_000_000;
/// UIP is high this long before the update.
const UIP_NS: u64 = 244_000;
/// The oscillator and divider setting for a 32.768 kHz crystal.
const DIVIDER_OK: u8 = 0x20;
/// Seconds a single `sync` examines for alarm matches before giving up on the older ones.
const ALARM_WINDOW: u64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DateTime {
    pub year: u32,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Days since 1970-01-01 for a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

impl DateTime {
    pub fn from_unix(secs: i64) -> Self {
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        Self {
            year: y.clamp(0, 9999) as u32,
            month: m as u8,
            day: d as u8,
            hour: (rem / 3600) as u8,
            minute: (rem % 3600 / 60) as u8,
            second: (rem % 60) as u8,
        }
    }

    pub fn to_unix(&self) -> i64 {
        days_from_civil(
            i64::from(self.year),
            i64::from(self.month),
            i64::from(self.day),
        ) * 86_400
            + i64::from(self.hour) * 3600
            + i64::from(self.minute) * 60
            + i64::from(self.second)
    }

    /// Sunday is 1, as the chip numbers days.
    pub fn weekday(&self) -> u8 {
        let days = self.to_unix().div_euclid(86_400);
        ((days + 4).rem_euclid(7) + 1) as u8
    }
}

fn to_bcd(v: u8) -> u8 {
    (v / 10) << 4 | (v % 10)
}

fn from_bcd(v: u8) -> u8 {
    (v >> 4) * 10 + (v & 15)
}

#[derive(Clone, Debug)]
pub struct Rtc {
    cmos: [u8; 128],
    index: u8,
    /// Unix time at `ns_base`.
    sec_base: i64,
    ns_base: u64,
    /// While SET is on, the time stands still at this.
    frozen: Option<i64>,
    /// Flags raised so far in C (bits 6:4), cleared by reading C.
    flags: u8,
    synced: u64,
    pub unsupported: u32,
}

impl Rtc {
    /// A clock showing unix time `epoch_secs` at virtual time 0, 24-hour BCD,
    /// the 32.768 kHz divider and a 1024 Hz periodic rate preselected as firmware leaves them.
    pub fn new(epoch_secs: i64) -> Self {
        let mut cmos = [0u8; 128];
        cmos[usize::from(REG_A)] = DIVIDER_OK | 0x06;
        cmos[usize::from(REG_B)] = B_24H;
        cmos[usize::from(REG_D)] = 0x80;
        Self {
            cmos,
            index: 0,
            sec_base: epoch_secs,
            ns_base: 0,
            frozen: None,
            flags: 0,
            synced: 0,
            unsupported: 0,
        }
    }

    pub fn owns(port: u16) -> bool {
        port == PORT_INDEX || port == PORT_DATA
    }

    /// Firmware's side: store a byte in the NVRAM area (registers 0x0E..0x7F).
    pub fn set_nvram(&mut self, reg: u8, value: u8) {
        if (0x0E..0x80).contains(&reg) {
            self.cmos[usize::from(reg)] = value;
        }
    }

    pub fn nvram(&self, reg: u8) -> u8 {
        self.cmos[usize::from(reg & 0x7F)]
    }

    pub fn nmi_masked(&self) -> bool {
        self.index & 0x80 != 0
    }

    fn b(&self) -> u8 {
        self.cmos[usize::from(REG_B)]
    }

    fn secs_at(&self, now: u64) -> i64 {
        self.frozen
            .unwrap_or_else(|| self.sec_base + (now.saturating_sub(self.ns_base) / NS) as i64)
    }

    /// The time the chip shows at virtual time `now`.
    pub fn datetime(&self, now: u64) -> DateTime {
        DateTime::from_unix(self.secs_at(now))
    }

    fn encode(&self, v: u8) -> u8 {
        if self.b() & B_DM != 0 {
            v
        } else {
            to_bcd(v)
        }
    }

    fn decode(&self, v: u8) -> u8 {
        if self.b() & B_DM != 0 {
            v
        } else {
            from_bcd(v)
        }
    }

    fn encode_hour(&self, h: u8) -> u8 {
        if self.b() & B_24H != 0 {
            self.encode(h)
        } else {
            let pm = if h >= 12 { 0x80 } else { 0 };
            let h12 = match h % 12 {
                0 => 12,
                x => x,
            };
            self.encode(h12) | pm
        }
    }

    fn decode_hour(&self, v: u8) -> u8 {
        if self.b() & B_24H != 0 {
            self.decode(v)
        } else {
            let pm = v & 0x80 != 0;
            (self.decode(v & 0x7F) % 12) + if pm { 12 } else { 0 }
        }
    }

    fn periodic_hz(&self) -> Option<u64> {
        let a = self.cmos[usize::from(REG_A)];
        if a & 0x70 != DIVIDER_OK {
            return None;
        }
        match a & 0x0F {
            0 => None,
            1 => Some(256),
            2 => Some(128),
            r => Some(32_768 >> (r - 1)),
        }
    }

    /// Does the alarm (in the chip's own register format) match `t`? Alarm bytes with bits 7:6 set match anything.
    fn alarm_matches(&self, t: &DateTime) -> bool {
        let fields = [
            (REG_SECONDS_ALARM, self.encode(t.second)),
            (REG_MINUTES_ALARM, self.encode(t.minute)),
            (REG_HOURS_ALARM, self.encode_hour(t.hour)),
        ];
        fields.iter().all(|(reg, now)| {
            let a = self.cmos[usize::from(*reg)];
            a & 0xC0 == 0xC0 || a == *now
        })
    }

    /// Raises the flags for everything that happened between the last call and `now`.
    pub fn sync(&mut self, now: u64) {
        let last = self.synced;
        if now <= last {
            return;
        }
        self.synced = now;
        if let Some(hz) = self.periodic_hz() {
            if crate::events_in(now, hz) > crate::events_in(last, hz) {
                self.flags |= C_PF;
            }
        }
        if self.frozen.is_some() {
            return;
        }
        // Second boundaries are at ns_base + k * 1s.
        let tick = |t: u64| t.saturating_sub(self.ns_base) / NS;
        let (first, lastk) = (tick(last), tick(now));
        if lastk > first {
            self.flags |= C_UF;
            let from = (first + 1).max(lastk.saturating_sub(ALARM_WINDOW));
            for k in from..=lastk {
                let t = DateTime::from_unix(self.sec_base + k as i64);
                if self.alarm_matches(&t) {
                    self.flags |= C_AF;
                    break;
                }
            }
        }
    }

    fn status_c(&self) -> u8 {
        let b = self.b();
        let on = (self.flags & C_PF != 0 && b & B_PIE != 0)
            || (self.flags & C_AF != 0 && b & B_AIE != 0)
            || (self.flags & C_UF != 0 && b & B_UIE != 0);
        self.flags | if on { C_IRQF } else { 0 }
    }

    /// The IRQ8 output at `now`.
    pub fn irq(&mut self, now: u64) -> bool {
        self.sync(now);
        self.status_c() & C_IRQF != 0
    }

    /// The next virtual time at which an enabled interrupt may be raised, if any.
    pub fn next_event(&self, now: u64) -> Option<u64> {
        let b = self.b();
        let mut best: Option<u64> = None;
        let mut consider = |t: u64| best = Some(best.map_or(t, |x| x.min(t)));
        if b & B_PIE != 0 {
            if let Some(hz) = self.periodic_hz() {
                consider(crate::ns_for_events(crate::events_in(now, hz) + 1, hz));
            }
        }
        if b & (B_UIE | B_AIE) != 0 && self.frozen.is_none() {
            let k = now.saturating_sub(self.ns_base) / NS + 1;
            consider(self.ns_base + k * NS);
        }
        best
    }

    /// OUT to port 0x70 or 0x71; false if `port` is not the RTC's.
    pub fn write(&mut self, port: u16, value: u8, now: u64) -> bool {
        match port {
            PORT_INDEX => self.index = value,
            PORT_DATA => {
                self.sync(now);
                self.write_reg(self.index & 0x7F, value, now);
            }
            _ => return false,
        }
        true
    }

    fn write_reg(&mut self, reg: u8, v: u8, now: u64) {
        match reg {
            REG_SECONDS | REG_MINUTES | REG_HOURS | REG_DAY | REG_MONTH | REG_YEAR
            | REG_CENTURY => self.set_field(reg, v, now),
            REG_WEEKDAY => {} // derived from the date
            REG_SECONDS_ALARM | REG_MINUTES_ALARM | REG_HOURS_ALARM => {
                self.cmos[usize::from(reg)] = v
            }
            REG_A => {
                let new = v & 0x7F;
                if new & 0x70 != DIVIDER_OK {
                    self.unsupported += 1;
                }
                self.cmos[usize::from(REG_A)] = new;
            }
            REG_B => {
                if v & 0x08 != 0 {
                    self.unsupported += 1; // square wave
                }
                // Switching format keeps the time: nothing is re-encoded (the registers are derived).
                self.cmos[usize::from(REG_B)] = v;
                if v & B_SET != 0 {
                    let t = self.secs_at(now);
                    self.frozen.get_or_insert(t);
                } else if let Some(t) = self.frozen.take() {
                    self.sec_base = t;
                    self.ns_base = now;
                }
            }
            REG_C | REG_D => {} // read-only
            _ => self.cmos[usize::from(reg)] = v,
        }
    }

    fn set_field(&mut self, reg: u8, v: u8, now: u64) {
        let mut t = self.datetime(now);
        let century = (t.year / 100) as u8;
        match reg {
            REG_SECONDS => t.second = self.decode(v).min(59),
            REG_MINUTES => t.minute = self.decode(v).min(59),
            REG_HOURS => t.hour = self.decode_hour(v).min(23),
            REG_DAY => t.day = self.decode(v).clamp(1, 31),
            REG_MONTH => t.month = self.decode(v).clamp(1, 12),
            REG_YEAR => t.year = u32::from(century) * 100 + u32::from(self.decode(v) % 100),
            _ => t.year = u32::from(self.decode(v)) * 100 + t.year % 100,
        }
        let secs = t.to_unix();
        if self.frozen.is_some() {
            self.frozen = Some(secs);
        } else {
            self.sec_base = secs;
            self.ns_base = now;
        }
    }

    /// IN from port 0x70 or 0x71; None if `port` is not the RTC's.
    pub fn read(&mut self, port: u16, now: u64) -> Option<u8> {
        match port {
            PORT_INDEX => Some(0xFF),
            PORT_DATA => {
                self.sync(now);
                Some(self.read_reg(self.index & 0x7F, now))
            }
            _ => None,
        }
    }

    fn read_reg(&mut self, reg: u8, now: u64) -> u8 {
        let t = self.datetime(now);
        match reg {
            REG_SECONDS => self.encode(t.second),
            REG_MINUTES => self.encode(t.minute),
            REG_HOURS => self.encode_hour(t.hour),
            REG_WEEKDAY => t.weekday(),
            REG_DAY => self.encode(t.day),
            REG_MONTH => self.encode(t.month),
            REG_YEAR => self.encode((t.year % 100) as u8),
            REG_CENTURY => self.encode((t.year / 100) as u8),
            REG_A => {
                let uip =
                    self.frozen.is_none() && NS - now.saturating_sub(self.ns_base) % NS <= UIP_NS;
                self.cmos[usize::from(REG_A)] & 0x7F | if uip { A_UIP } else { 0 }
            }
            REG_C => {
                let v = self.status_c();
                self.flags = 0;
                v
            }
            _ => self.cmos[usize::from(reg)],
        }
    }
}
