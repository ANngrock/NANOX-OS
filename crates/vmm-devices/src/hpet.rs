//! The High Precision Event Timer at 0xFED00000: three comparators on a
//! 10 MHz 64-bit main counter, as QEMU's q35 model has them and as Linux
//! reads them (it uses the counter as a clocksource and timer 0 or 1 as a
//! clock-event device, in legacy replacement mode it takes IRQ0 and IRQ8 from
//! the PIT and the RTC).
//!
//! Time is virtual: every access carries `now` in nanoseconds. Interrupts come
//! out in two forms. An edge-triggered timer queues a [`Fire`] for each
//! expiry ([`Hpet::take_fire`]); a level-triggered one sets its bit in the
//! general interrupt status register, and [`Hpet::line`] is that line until the
//! guest clears the bit. [`Hpet::next_event`] says when the VMM must call
//! [`Hpet::sync`] next.
//!
//! In legacy replacement mode timer 0 is ISA IRQ0 and timer 1 is IRQ8; the
//! VMM wires IRQ0 to I/O APIC pin 2, as PC chipsets do. Not modeled: the FSB
//! (MSI) delivery path beyond storing its registers, and counter writes while
//! the counter runs (they are ignored, which the specification allows).

pub const DEFAULT_BASE: u64 = 0xFED0_0000;
pub const SIZE: u64 = 0x400;
pub const TIMERS: usize = 3;
/// 10 MHz: one tick every 100 ns.
pub const TICK_NS: u64 = 100;
/// The counter period in femtoseconds, as GCAP_ID reports it.
pub const PERIOD_FS: u64 = 100_000_000;
const VENDOR: u64 = 0x8086;

pub const REG_ID: u64 = 0x000;
/// What the general capabilities and ID register (`REG_ID`) reads: revision 1, three timers, a
/// 64-bit counter, legacy replacement routing, the vendor id and the counter period.
pub const CAPABILITIES: u64 =
    1 | ((TIMERS as u64 - 1) << 8) | (1 << 13) | (1 << 15) | (VENDOR << 16) | (PERIOD_FS << 32);
pub const REG_CONFIG: u64 = 0x010;
pub const REG_STATUS: u64 = 0x020;
pub const REG_COUNTER: u64 = 0x0F0;
pub const REG_TIMER0: u64 = 0x100;
pub const TIMER_STRIDE: u64 = 0x20;

const CONF_ENABLE: u64 = 1;
const CONF_LEGACY: u64 = 2;
const TN_LEVEL: u64 = 1 << 1;
const TN_ENABLE: u64 = 1 << 2;
const TN_PERIODIC: u64 = 1 << 3;
const TN_PERIODIC_CAP: u64 = 1 << 4;
const TN_SIZE_CAP: u64 = 1 << 5;
const TN_SETVAL: u64 = 1 << 6;
const TN_32BIT: u64 = 1 << 8;
const TN_ROUTE_SHIFT: u32 = 9;
const TN_FSB_ENABLE: u64 = 1 << 14;
/// Interrupt routes a timer can use (bits 63:32 of its configuration): IRQ 2, 8, 11 and 20..23.
const ROUTE_CAP: u64 = 0x00F0_0904;
const WRITABLE: u64 = TN_LEVEL
    | TN_ENABLE
    | TN_PERIODIC
    | TN_SETVAL
    | TN_32BIT
    | (0x1F << TN_ROUTE_SHIFT)
    | TN_FSB_ENABLE;
const FIRES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fire {
    pub timer: u8,
    /// The ISA IRQ the pulse is on: 0 or 8 in legacy replacement mode, else the timer's route.
    pub irq: u8,
}

#[derive(Clone, Copy, Debug)]
struct Timer {
    config: u64,
    comparator: u64,
    period: u64,
    fsb: u64,
}

#[derive(Clone, Debug)]
pub struct Hpet {
    config: u64,
    status: u64,
    /// Counter value at `since` (or frozen, when disabled).
    base: u64,
    since: u64,
    synced: u64,
    timer: [Timer; TIMERS],
    fires: [Fire; FIRES],
    fires_len: usize,
    pub dropped: u64,
}

impl Default for Hpet {
    fn default() -> Self {
        Self::new()
    }
}

impl Hpet {
    pub const fn new() -> Self {
        let t = Timer {
            config: TN_PERIODIC_CAP | TN_SIZE_CAP | (ROUTE_CAP << 32),
            comparator: u64::MAX,
            period: 0,
            fsb: 0,
        };
        Self {
            config: 0,
            status: 0,
            base: 0,
            since: 0,
            synced: 0,
            timer: [t; TIMERS],
            fires: [Fire { timer: 0, irq: 0 }; FIRES],
            fires_len: 0,
            dropped: 0,
        }
    }

    pub fn owns(offset: u64) -> bool {
        offset < SIZE
    }

    fn enabled(&self) -> bool {
        self.config & CONF_ENABLE != 0
    }

    /// The main counter at `now`.
    pub fn counter(&self, now: u64) -> u64 {
        if self.enabled() {
            self.base
                .wrapping_add(now.saturating_sub(self.since) / TICK_NS)
        } else {
            self.base
        }
    }

    fn irq_of(&self, t: usize) -> u8 {
        if self.config & CONF_LEGACY != 0 && t < 2 {
            [0, 8][t]
        } else {
            ((self.timer[t].config >> TN_ROUTE_SHIFT) & 0x1F) as u8
        }
    }

    fn mask(&self, t: usize) -> u64 {
        if self.timer[t].config & TN_32BIT != 0 {
            0xFFFF_FFFF
        } else {
            u64::MAX
        }
    }

    fn push(&mut self, f: Fire) {
        if self.fires_len == FIRES {
            self.dropped += 1;
        } else {
            self.fires[self.fires_len] = f;
            self.fires_len += 1;
        }
    }

    /// Accounts for everything that happened up to `now`: comparators crossed, interrupts raised.
    pub fn sync(&mut self, now: u64) {
        let (from, to) = (self.counter(self.synced), self.counter(now));
        self.synced = self.synced.max(now);
        if !self.enabled() || to == from {
            return;
        }
        for t in 0..TIMERS {
            let m = self.mask(t);
            let tm = self.timer[t];
            // Distance from the old counter to the comparator, within the counter's width.
            let first = tm.comparator.wrapping_sub(from) & m;
            let span = to.wrapping_sub(from) & u64::MAX;
            if first == 0 || first > span {
                continue;
            }
            let mut count = 1;
            if tm.config & TN_PERIODIC != 0 && tm.period & m != 0 {
                count += (span - first) / (tm.period & m);
                self.timer[t].comparator =
                    tm.comparator.wrapping_add(count * (tm.period & m)) & m_all(m);
            }
            if tm.config & TN_ENABLE == 0 {
                continue;
            }
            if tm.config & TN_LEVEL != 0 {
                self.status |= 1 << t;
            } else {
                let irq = self.irq_of(t);
                for _ in 0..count.min(FIRES as u64) {
                    self.push(Fire {
                        timer: t as u8,
                        irq,
                    });
                }
            }
        }
    }

    /// The configuration register of timer `t` (0 for a timer that does not exist).
    pub fn timer_config(&self, t: usize) -> u64 {
        self.timer.get(t).map_or(0, |tm| tm.config)
    }

    /// The next edge-triggered interrupt, oldest first.
    pub fn take_fire(&mut self) -> Option<Fire> {
        if self.fires_len == 0 {
            return None;
        }
        let f = self.fires[0];
        self.fires.copy_within(1..self.fires_len, 0);
        self.fires_len -= 1;
        Some(f)
    }

    /// The level of timer `t`'s interrupt line at `now` (level-triggered timers only).
    pub fn line(&mut self, t: usize, now: u64) -> bool {
        self.sync(now);
        t < TIMERS && self.status & (1 << t) != 0 && self.timer[t].config & TN_ENABLE != 0
    }

    /// When the VMM must call `sync` next to catch the next enabled comparator.
    pub fn next_event(&self, now: u64) -> Option<u64> {
        if !self.enabled() {
            return None;
        }
        let c = self.counter(now);
        let mut best: Option<u64> = None;
        for t in 0..TIMERS {
            let tm = self.timer[t];
            if tm.config & TN_ENABLE == 0 {
                continue;
            }
            let ticks = tm.comparator.wrapping_sub(c) & self.mask(t);
            if ticks == 0 {
                continue;
            }
            // A comparator that is behind the counter is a wrap away: no deadline worth waiting for.
            let Some(at) = ticks.checked_mul(TICK_NS).and_then(|d| now.checked_add(d)) else {
                continue;
            };
            best = Some(best.map_or(at, |b| b.min(at)));
        }
        best
    }

    fn read_reg(&mut self, offset: u64, now: u64) -> u64 {
        match offset {
            REG_ID => CAPABILITIES,
            REG_CONFIG => self.config,
            REG_STATUS => self.status,
            REG_COUNTER => self.counter(now),
            o if (REG_TIMER0..REG_TIMER0 + TIMER_STRIDE * TIMERS as u64).contains(&o) => {
                let t = ((o - REG_TIMER0) / TIMER_STRIDE) as usize;
                let tm = &self.timer[t];
                match (o - REG_TIMER0) % TIMER_STRIDE {
                    0 => tm.config,
                    8 => tm.comparator,
                    16 => tm.fsb,
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    fn write_reg(&mut self, offset: u64, v: u64, now: u64) {
        match offset {
            REG_CONFIG => {
                let was = self.enabled();
                self.config = v & (CONF_ENABLE | CONF_LEGACY);
                if !was && self.enabled() {
                    self.since = now;
                    self.synced = now;
                } else if was && !self.enabled() {
                    self.base = self
                        .base
                        .wrapping_add(now.saturating_sub(self.since) / TICK_NS);
                }
            }
            REG_STATUS => self.status &= !v,
            REG_COUNTER => {
                if !self.enabled() {
                    self.base = v;
                }
            }
            o if (REG_TIMER0..REG_TIMER0 + TIMER_STRIDE * TIMERS as u64).contains(&o) => {
                let t = ((o - REG_TIMER0) / TIMER_STRIDE) as usize;
                match (o - REG_TIMER0) % TIMER_STRIDE {
                    0 => {
                        let tm = &mut self.timer[t];
                        tm.config = (tm.config & !WRITABLE) | (v & WRITABLE);
                        if tm.config & TN_LEVEL == 0 {
                            self.status &= !(1 << t);
                        }
                    }
                    8 => {
                        let tm = &mut self.timer[t];
                        let periodic = tm.config & TN_PERIODIC != 0;
                        if !periodic || tm.config & TN_SETVAL != 0 {
                            tm.comparator = v;
                        }
                        if periodic {
                            tm.period = v;
                        }
                        tm.config &= !TN_SETVAL;
                    }
                    16 => self.timer[t].fsb = v,
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// Register write of `size` bytes (4 or 8) at `offset`; false for any other access.
    pub fn write(&mut self, offset: u64, size: u8, value: u64, now: u64) -> bool {
        if !Self::owns(offset) || !matches!(size, 4 | 8) {
            return false;
        }
        self.sync(now);
        let reg = offset & !7;
        if size == 8 {
            self.write_reg(reg, value, now);
        } else {
            let old = self.read_reg(reg, now);
            let new = if offset & 4 == 0 {
                (old & !0xFFFF_FFFF) | (value & 0xFFFF_FFFF)
            } else {
                (old & 0xFFFF_FFFF) | (value << 32)
            };
            // Writing the status register half-wise must clear only what the half names.
            let v = if reg == REG_STATUS {
                value << (32 * ((offset & 4) >> 2))
            } else {
                new
            };
            self.write_reg(reg, v, now);
        }
        true
    }

    /// Register read of `size` bytes (4 or 8); None for any other access.
    pub fn read(&mut self, offset: u64, size: u8, now: u64) -> Option<u64> {
        if !Self::owns(offset) || !matches!(size, 4 | 8) {
            return None;
        }
        self.sync(now);
        let v = self.read_reg(offset & !7, now);
        Some(match (size, offset & 4) {
            (8, _) => v,
            (_, 0) => v & 0xFFFF_FFFF,
            _ => v >> 32,
        })
    }
}

/// Comparator wrap for a counter of the given width.
fn m_all(m: u64) -> u64 {
    m
}
