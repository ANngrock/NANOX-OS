//! PIT (8254) channel 2 and port 0x61 (gate, speaker data, OUT2 status):
//! the reference NANOX calibrates its local APIC timer against. Only what
//! the kernel programs is modeled — mode 0 (interrupt on terminal count),
//! lobyte/hibyte access, binary counting; other control words for channel
//! 2, counter reads and read-back commands are counted as unsupported, and
//! OUT2 then stays low so a caller notices instead of measuring nonsense.
//! Channels 0 and 1 are not modeled.

use crate::{events_in, ns_for_events};

/// Input clock of the 8254 on a PC.
pub const HZ: u64 = 1_193_182;
pub const PORT_CHANNEL2: u16 = 0x42;
pub const PORT_CONTROL: u16 = 0x43;
pub const PORT_SPEAKER: u16 = 0x61;
/// Channel 2, lobyte/hibyte, mode 0, binary.
pub const CONTROL_MODE0: u8 = 0xB0;

#[derive(Clone, Debug, Default)]
pub struct Pit2 {
    /// Bits 3:0 of port 0x61 as last written (bit 0: gate 2).
    port61: u8,
    /// Channel 2 was programmed with [`CONTROL_MODE0`].
    mode0: bool,
    /// Low byte written, high byte awaited.
    low: Option<u8>,
    /// Loaded count (0 means 65536).
    count: Option<u32>,
    /// PIT clocks counted while the gate was open, up to `since`.
    clocks: u64,
    /// Counting since this time (gate open and a count loaded).
    since: Option<u64>,
    pub unsupported: u32,
}

impl Pit2 {
    pub fn new() -> Self {
        Self::default()
    }

    fn gate(&self) -> bool {
        self.port61 & 1 != 0
    }

    fn clocks_at(&self, now: u64) -> u64 {
        self.clocks
            + self
                .since
                .map_or(0, |s| events_in(now.saturating_sub(s), HZ))
    }

    /// OUT2 at `now`. In mode 0 OUT goes low when the control word is
    /// written and high once the counter reaches zero: N + 1 clocks after
    /// the count N is loaded (the first clock loads the counter).
    pub fn out2(&self, now: u64) -> bool {
        match self.count {
            Some(n) if self.mode0 => self.clocks_at(now) > u64::from(n),
            _ => false,
        }
    }

    /// When OUT2 goes high, if it is counting towards that.
    pub fn out2_deadline(&self) -> Option<u64> {
        let (n, since) = (self.count?, self.since?);
        if !self.mode0 {
            return None;
        }
        let left = (u64::from(n) + 1).saturating_sub(self.clocks);
        since.checked_add(ns_for_events(left, HZ))
    }

    /// OUT to a PIT/port-0x61 port; false if the port is not modeled here.
    pub fn write(&mut self, port: u16, value: u8, now: u64) -> bool {
        match port {
            PORT_CONTROL => {
                if value >> 6 == 2 {
                    // Channel 2: stop, drop the count, OUT low.
                    self.clocks = 0;
                    self.since = None;
                    self.count = None;
                    self.low = None;
                    self.mode0 = value == CONTROL_MODE0;
                    if !self.mode0 {
                        self.unsupported += 1;
                    }
                } else {
                    self.unsupported += 1;
                }
            }
            PORT_CHANNEL2 => match self.low.take() {
                None => self.low = Some(value),
                Some(lo) => {
                    let n = u32::from(lo) | u32::from(value) << 8;
                    self.count = Some(if n == 0 { 0x1_0000 } else { n });
                    self.clocks = 0;
                    self.since = self.gate().then_some(now);
                }
            },
            PORT_SPEAKER => {
                let open = value & 1 != 0;
                if self.gate() && !open {
                    self.clocks = self.clocks_at(now);
                    self.since = None;
                } else if !self.gate() && open && self.count.is_some() {
                    self.since = Some(now);
                }
                self.port61 = value & 0x0F;
            }
            _ => return false,
        }
        true
    }

    /// IN from a PIT/port-0x61 port; None if the port is not modeled here.
    pub fn read(&mut self, port: u16, now: u64) -> Option<u8> {
        match port {
            PORT_SPEAKER => Some(self.port61 | u8::from(self.out2(now)) << 5),
            PORT_CHANNEL2 | PORT_CONTROL => {
                self.unsupported += 1;
                Some(0)
            }
            _ => None,
        }
    }
}
