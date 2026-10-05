//! The 8254 programmable interval timer and port 0x61, wired as on a PC:
//! channel 0's OUT is IRQ0, the system tick; channel 1 (once the DRAM
//! refresh) is free; channel 2's gate is bit 0 of port 0x61 and its OUT reads
//! back in bit 5, the reference NANOX and Linux calibrate against. The gates
//! of channels 0 and 1 are tied high.
//!
//! Each channel follows the Intel 8254 data sheet:
//!
//! * control words: access by latch, LSB, MSB or LSB then MSB (with separate
//!   write and read flip-flops, so reads and writes may interleave), modes 0
//!   to 5 (6 and 7 are 2 and 3), binary or BCD counting; the counter latch
//!   command and the read-back command (count and/or status of any channels;
//!   a second latch before the read is ignored; a latched status reads before
//!   a latched count); the status byte: OUT, null count, access, mode, BCD;
//! * OUT and the count over virtual time. A count is loaded by the first clock
//!   after it is written (after the trigger in modes 1 and 5) and decremented
//!   from the next, so for a count N: mode 0 raises OUT after N + 1 clocks;
//!   mode 1 holds OUT low for N clocks from the trigger; mode 2 pulls it low
//!   for the one clock its counter is 1, every N clocks; mode 3 is high for
//!   (N + 1) / 2 clocks and low for N / 2, its counter going down by two; modes
//!   4 and 5 strobe it low for the one clock after N + 1. A count of 0 is 65536
//!   (10000 in BCD); a count of 1 in modes 2 and 3, which the chip forbids,
//!   never raises OUT;
//! * a count written while counting: modes 0 and 4 restart with it (in mode 0
//!   the first of two bytes already stops the count and drops OUT), modes 2
//!   and 3 take it at the end of the running period or half-period, modes 1
//!   and 5 at the next trigger. A control word resets its channel;
//! * the gate (channel 2): low, it pauses modes 0 and 4 and stops modes 2 and
//!   3 with OUT high; its rising edge triggers modes 1, 2, 3 and 5.
//!
//! The clock is relative, as the model always had it: it ticks one period
//! (838 ns) after a count is written or a gate opens, and a channel counts
//! only while its gate is open, the loading clock included. The chip loads on
//! the next edge of a free-running clock, gate or not; so channel 2 loaded
//! behind a closed gate raises OUT N + 1 clocks after the gate opens here and N
//! on the chip, the timing the NANOX calibration was written against.
//!
//! IRQ0 is an edge: [`Pit::take_edges`] counts OUT0's rising edges since its
//! previous call, by arithmetic however late the call, including the one a
//! control word makes when it ends mode 0's low OUT; [`Pit::next_event`] says
//! when the next comes. Not modeled: the speaker, the refresh toggle in bit 4
//! of port 0x61, the count before the loading clock (it reads as the new count,
//! the chip shows the old one), and BCD counts with a digit above 9: they count
//! with the digits' binary weights, read back as nothing a chip would show, and
//! are counted in `unsupported`, as are reads of the control port.

use crate::{events_in, ns_for_events};

/// Input clock of the 8254 on a PC.
pub const HZ: u64 = 1_193_182;
pub const PORT_CHANNEL0: u16 = 0x40;
pub const PORT_CHANNEL1: u16 = 0x41;
pub const PORT_CHANNEL2: u16 = 0x42;
pub const PORT_CONTROL: u16 = 0x43;
pub const PORT_SPEAKER: u16 = 0x61;
/// Channel 2, lobyte/hibyte, mode 0, binary.
pub const CONTROL_MODE0: u8 = 0xB0;

/// A deferred count waits for the next trigger (modes 1 and 5).
const AT_TRIGGER: i64 = i64::MAX;

#[derive(Clone, Copy, Debug)]
struct Channel {
    /// Bits 5:0 of its last control word: access (5:4), mode (3:1), BCD (0).
    ctrl: u8,
    gate: bool,
    /// The write flip-flop: the low byte of a two-byte count, the high byte awaited.
    low: Option<u8>,
    /// The read flip-flop of unlatched two-byte reads: the high byte is next.
    high_next: bool,
    /// A latched count, and whether its low byte was read.
    latch: Option<(u16, bool)>,
    status: Option<u8>,
    /// The count register: the last count written (1..=65536, 1..=10000 in BCD); 0 after a control word.
    cr: u32,
    /// The count the counting element runs with.
    n: u32,
    /// Clocks of the run counted before `since` (its first one loads the counting element).
    base: i64,
    /// Counting since this time.
    since: Option<u64>,
    /// A count written while counting: the clock of the run at which the counting element takes it
    /// (`AT_TRIGGER`: at the next trigger), and the clock of its own run it starts at there.
    defer: Option<(i64, i64)>,
}

/// The first rising edge of OUT in a run of count `n` (its clock) and the period of the next ones.
fn rise(mode: u8, n: i64) -> Option<(i64, Option<i64>)> {
    match mode {
        0 | 1 => Some((n + 1, None)),
        4 | 5 => Some((n + 2, None)),
        _ if n > 1 => Some((n + 1, Some(n))),
        _ => None,
    }
}

/// Rising edges of OUT at clocks up to `c` of a run of count `n`.
fn rises_to(mode: u8, n: i64, c: i64) -> i64 {
    match rise(mode, n) {
        Some((first, period)) if c >= first => 1 + period.map_or(0, |p| (c - first) / p),
        _ => 0,
    }
}

/// The clock of the first rising edge of OUT after clock `c` of a run of count `n`.
fn rise_after(mode: u8, n: i64, c: i64) -> Option<i64> {
    let (first, period) = rise(mode, n)?;
    if c < first {
        return Some(first);
    }
    period.map(|p| first + ((c - first) / p + 1) * p)
}

/// A BCD count as a number, and whether every digit was one.
fn from_bcd(raw: u16) -> (u32, bool) {
    [12, 8, 4, 0].iter().fold((0, true), |(n, ok), s| {
        let d = u32::from(raw >> s & 0xF);
        (n * 10 + d, ok && d < 10)
    })
}

fn to_bcd(v: i64) -> u16 {
    (0..4).fold(0, |acc, i| {
        acc | ((v / 10i64.pow(i) % 10) as u16) << (4 * i)
    })
}

impl Channel {
    const fn new(gate: bool) -> Self {
        Self {
            ctrl: 0x30,
            gate,
            low: None,
            high_next: false,
            latch: None,
            status: None,
            cr: 0,
            n: 0,
            base: 0,
            since: None,
            defer: None,
        }
    }

    fn access(&self) -> u8 {
        self.ctrl >> 4
    }

    fn mode(&self) -> u8 {
        let m = (self.ctrl >> 1) & 7;
        if m > 5 {
            m - 4
        } else {
            m
        }
    }

    fn bcd(&self) -> bool {
        self.ctrl & 1 != 0
    }

    fn modulus(&self) -> i64 {
        if self.bcd() {
            10_000
        } else {
            0x1_0000
        }
    }

    /// Clocks of the current run at `now`.
    fn clock(&self, now: u64) -> i64 {
        self.base
            + self
                .since
                .map_or(0, |s| events_in(now.saturating_sub(s), HZ) as i64)
    }

    /// The count being counted and the clock of its own run, at clock `c` of the current run.
    fn epoch(&self, c: i64) -> (i64, i64) {
        match self.defer {
            Some((at, start)) if c >= at => (i64::from(self.cr), c - at + start),
            _ => (i64::from(self.n), c),
        }
    }

    fn out(&self, now: u64) -> bool {
        let m = self.mode();
        if m == 0 && self.low.is_some() {
            return false;
        }
        if !self.gate && matches!(m, 2 | 3) {
            return true;
        }
        let (n, c) = self.epoch(self.clock(now));
        if c < 1 {
            return m != 0;
        }
        let d = c - 1;
        match m {
            0 | 1 => d >= n,
            2 => d % n != n - 1,
            3 => d % n < (n + 1) / 2,
            _ => d != n,
        }
    }

    /// The counting element at `now`, as the guest reads it.
    fn count(&self, now: u64) -> u16 {
        let (n, c) = self.epoch(self.clock(now));
        let m = self.modulus();
        let d = c - 1;
        let v = if c < 1 {
            i64::from(self.cr)
        } else {
            match self.mode() {
                2 => n - d % n,
                3 => {
                    let (h, p) = ((n + 1) / 2, d % n);
                    (n & !1) - 2 * if p < h { p } else { p - h }
                }
                _ => n - d % m + m,
            }
        } % m;
        if self.bcd() {
            to_bcd(v)
        } else {
            v as u16
        }
    }

    fn null_count(&self, now: u64) -> bool {
        let c = self.clock(now);
        self.cr == 0 || c < 1 || self.defer.is_some_and(|(at, _)| c < at)
    }

    fn status_byte(&self, now: u64) -> u8 {
        u8::from(self.out(now)) << 7 | u8::from(self.null_count(now)) << 6 | self.ctrl
    }

    /// Rising edges of OUT in the current run up to `now`.
    fn rises(&self, now: u64) -> u64 {
        let (m, c) = (self.mode(), self.clock(now));
        let r = match self.defer {
            Some((at, start)) if c >= at => {
                rises_to(m, i64::from(self.n), at) + rises_to(m, i64::from(self.cr), c - at + start)
            }
            _ => rises_to(m, i64::from(self.n), c),
        };
        r as u64
    }

    /// When OUT next rises after clock `c` of the run, if the channel counts towards it.
    fn next_rise(&self, c: i64) -> Option<u64> {
        let since = self.since?;
        let m = self.mode();
        let e = match self.defer {
            Some((at, start)) => match rise_after(m, i64::from(self.n), c) {
                Some(e) if c < at && e <= at => Some(e),
                _ => rise_after(m, i64::from(self.cr), c.max(at) - at + start)
                    .and_then(|e| e.checked_add(at - start)),
            },
            None => rise_after(m, i64::from(self.n), c),
        }?;
        since.checked_add(ns_for_events((e - self.base) as u64, HZ))
    }

    /// Takes a deferred count whose clock has come.
    fn fold(&mut self, now: u64) {
        if let Some((at, start)) = self.defer {
            if self.clock(now) >= at {
                self.n = self.cr;
                self.base += start - at;
                self.defer = None;
            }
        }
    }

    /// Counts the count register from its loading clock, if the gate lets it.
    fn restart(&mut self, now: u64) {
        self.n = self.cr;
        self.base = 0;
        self.defer = None;
        self.since = self.gate.then_some(now);
    }

    /// Where mode 2 or 3 takes a new count: the end of the period, or half-period, running at clock `c`.
    fn period_end(&self, c: i64) -> (i64, i64) {
        let n = i64::from(self.n);
        let (p, h) = ((c - 1) % n, (n + 1) / 2);
        if self.mode() == 3 && p < h {
            // into the low half of the new count
            (c + h - p, i64::from(self.cr + 1) / 2 + 1)
        } else {
            (c + n - p, 1)
        }
    }

    /// A complete count was written.
    fn load(&mut self, n: u32, now: u64) {
        let c = self.clock(now);
        self.cr = n;
        match self.mode() {
            1 | 5 if c > 0 => self.defer = Some((AT_TRIGGER, 1)),
            1 | 5 => self.n = n,
            2 | 3 if c > 0 => self.defer = Some(self.period_end(c)),
            _ => self.restart(now),
        }
    }

    /// A byte to the channel's port; false if it completed a count that is not BCD in BCD mode.
    fn write(&mut self, v: u8, now: u64) -> bool {
        let raw = match (self.access(), self.low.take()) {
            (1, _) => u16::from(v),
            (2, _) => u16::from(v) << 8,
            (_, Some(lo)) => u16::from(lo) | u16::from(v) << 8,
            (_, None) => {
                self.low = Some(v);
                if self.mode() == 0 {
                    self.base = self.clock(now);
                    self.since = None;
                }
                return true;
            }
        };
        let (n, ok) = if self.bcd() {
            from_bcd(raw)
        } else {
            (u32::from(raw), true)
        };
        self.load(if n == 0 { self.modulus() as u32 } else { n }, now);
        ok
    }

    fn read(&mut self, now: u64) -> u8 {
        if let Some(s) = self.status.take() {
            return s;
        }
        let a = self.access();
        let (v, high) = match self.latch {
            Some((v, low_read)) => {
                let high = a == 2 || low_read;
                self.latch = (a == 3 && !high).then_some((v, true));
                (v, high)
            }
            None => {
                let high = a == 2 || (a == 3 && self.high_next);
                self.high_next ^= a == 3;
                (self.count(now), high)
            }
        };
        if high {
            (v >> 8) as u8
        } else {
            v as u8
        }
    }

    fn latch_count(&mut self, now: u64) {
        let v = self.count(now);
        self.latch.get_or_insert((v, false));
    }

    fn latch_status(&mut self, now: u64) {
        let s = self.status_byte(now);
        self.status.get_or_insert(s);
    }

    /// The gate changes to `open`.
    fn set_gate(&mut self, open: bool, now: u64) {
        let c = self.clock(now);
        self.gate = open;
        match (self.mode(), open) {
            (0 | 4, true) => {
                if self.cr > 0 {
                    self.since = Some(now);
                }
            }
            (1 | 5, false) => {}
            (_, false) => {
                self.base = c;
                self.since = None;
            }
            (_, true) => {
                if self.cr > 0 {
                    self.restart(now);
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Pit {
    ch: [Channel; 3],
    /// Bits 3:0 of port 0x61 as last written (bit 0: gate 2).
    port61: u8,
    /// Rising edges of OUT0 not taken yet, and those of its current run already counted.
    edges: u64,
    seen: u64,
    pub unsupported: u32,
}

impl Default for Pit {
    fn default() -> Self {
        Self::new()
    }
}

impl Pit {
    /// Every channel as after a control word for LSB then MSB, mode 0, binary: no count, OUT low.
    pub const fn new() -> Self {
        Self {
            ch: [Channel::new(true), Channel::new(true), Channel::new(false)],
            port61: 0,
            edges: 0,
            seen: 0,
            unsupported: 0,
        }
    }

    pub fn owns(port: u16) -> bool {
        matches!(port, PORT_CHANNEL0..=PORT_CONTROL | PORT_SPEAKER)
    }

    /// OUT of channel `ch` (0, 1 or 2) at `now`.
    pub fn out(&self, ch: usize, now: u64) -> bool {
        self.ch[ch].out(now)
    }

    pub fn out2(&self, now: u64) -> bool {
        self.out(2, now)
    }

    /// When OUT2 first rises after its count last started or resumed (in mode 0: the terminal count).
    pub fn out2_deadline(&self) -> Option<u64> {
        let c = &self.ch[2];
        c.next_rise(c.base)
    }

    /// When OUT0 rises next after `now` (an IRQ0 edge), if it does.
    pub fn next_event(&self, now: u64) -> Option<u64> {
        let c = &self.ch[0];
        c.next_rise(c.clock(now))
    }

    /// OUT0's rising edges since the previous call: each is an edge on IRQ0.
    pub fn take_edges(&mut self, now: u64) -> u64 {
        self.settle(now);
        core::mem::take(&mut self.edges)
    }

    /// Counts OUT0's edges up to `now` and lets deferred counts whose clock came take effect.
    fn settle(&mut self, now: u64) {
        self.edges += self.ch[0].rises(now).saturating_sub(self.seen);
        for c in &mut self.ch {
            c.fold(now);
        }
        self.seen = self.ch[0].rises(now);
    }

    fn control(&mut self, v: u8, now: u64) {
        let sc = usize::from(v >> 6);
        if sc == 3 {
            // Read-back: bits 3:1 pick channels 2..0; bit 5 clear latches the count, bit 4 clear the status.
            for (i, c) in self.ch.iter_mut().enumerate() {
                if v & 2 << i != 0 {
                    if v & 0x20 == 0 {
                        c.latch_count(now);
                    }
                    if v & 0x10 == 0 {
                        c.latch_status(now);
                    }
                }
            }
        } else if v & 0x30 == 0 {
            self.ch[sc].latch_count(now);
        } else {
            let gate = self.ch[sc].gate;
            self.ch[sc] = Channel {
                ctrl: v & 0x3F,
                ..Channel::new(gate)
            };
        }
    }

    /// OUT to a PIT/port-0x61 port; false if the port is not modeled here.
    pub fn write(&mut self, port: u16, value: u8, now: u64) -> bool {
        if !Self::owns(port) {
            return false;
        }
        self.settle(now);
        let was = self.ch[0].out(now);
        match port {
            PORT_CONTROL => self.control(value, now),
            PORT_SPEAKER => {
                let open = value & 1 != 0;
                if open != self.ch[2].gate {
                    self.ch[2].set_gate(open, now);
                }
                self.port61 = value & 0x0F;
            }
            p => {
                if !self.ch[usize::from(p - PORT_CHANNEL0)].write(value, now) {
                    self.unsupported += 1;
                }
            }
        }
        // OUT0 raised by the access itself is an edge too.
        if !was && self.ch[0].out(now) {
            self.edges += 1;
        }
        self.seen = self.ch[0].rises(now);
        true
    }

    /// IN from a PIT/port-0x61 port; None if the port is not modeled here.
    pub fn read(&mut self, port: u16, now: u64) -> Option<u8> {
        match port {
            PORT_SPEAKER => Some(self.port61 | u8::from(self.out2(now)) << 5),
            PORT_CONTROL => {
                self.unsupported += 1;
                Some(0)
            }
            PORT_CHANNEL0..=PORT_CHANNEL2 => {
                Some(self.ch[usize::from(port - PORT_CHANNEL0)].read(now))
            }
            _ => None,
        }
    }
}
