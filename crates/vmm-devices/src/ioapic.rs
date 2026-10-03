//! The I/O APIC (82093AA-style, version 0x20, 24 pins) at 0xFEC00000: the
//! index/data window (IOREGSEL at +0x00, IOWIN at +0x10), the EOI register at
//! +0x40, the ID/version/arbitration registers and the 24 64-bit redirection
//! entries. Devices drive pins with [`IoApic::set_irq`]; the interrupts the
//! chip decides to send come out of [`IoApic::take_message`] as bus messages
//! for a local APIC, so the VMM stays the one who delivers them.
//!
//! Semantics modeled: edge pins (a rising edge is remembered while the pin is
//! masked and sent once it is unmasked), level pins (the message is sent while
//! the line is asserted, the entry's remote-IRR bit blocks the next one until
//! EOI), polarity, delivery status (set while a request waits), the rule that
//! switching an entry to edge clears its remote IRR, and EOI by vector (the
//! register at +0x40 or [`IoApic::eoi`]). Not modeled: arbitration between
//! several I/O APICs (one chip) and the bus itself (messages never fail).

pub const DEFAULT_BASE: u64 = 0xFEC0_0000;
pub const PINS: usize = 24;
pub const VERSION: u32 = 0x20;
/// IOREGSEL.
pub const OFF_INDEX: u64 = 0x00;
/// IOWIN.
pub const OFF_WINDOW: u64 = 0x10;
/// EOI register (IOAPIC version 0x20 and later).
pub const OFF_EOI: u64 = 0x40;

pub const REG_ID: u8 = 0x00;
pub const REG_VERSION: u8 = 0x01;
pub const REG_ARBITRATION: u8 = 0x02;
pub const REG_REDIRECTION: u8 = 0x10;

const MASKED: u64 = 1 << 16;
const LEVEL: u64 = 1 << 15;
const REMOTE_IRR: u64 = 1 << 14;
const ACTIVE_LOW: u64 = 1 << 13;
const DELIVERY_STATUS: u64 = 1 << 12;
/// Bits software may write: all but delivery status (12), remote IRR (14) and the reserved 55:17.
const WRITABLE: u64 = 0xFF00_0000_0001_FFFF & !(DELIVERY_STATUS | REMOTE_IRR);
const MESSAGES: usize = 32;

/// One interrupt sent to a local APIC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    pub vector: u8,
    /// Delivery mode, bits 10:8 of the entry (0 fixed, 1 lowest priority, 2 SMI, 4 NMI, 5 INIT, 7 ExtINT).
    pub mode: u8,
    /// Destination mode: false physical, true logical.
    pub logical: bool,
    pub dest: u8,
    pub level: bool,
    /// The pin it came from.
    pub pin: u8,
}

#[derive(Clone, Debug)]
pub struct IoApic {
    index: u8,
    id: u8,
    /// Redirection entries; reset state is masked, edge, physical, vector 0.
    entry: [u64; PINS],
    /// The electrical level of each pin as the device drives it (low until driven).
    raw: u32,
    /// Edge requests waiting (masked, or not yet taken).
    edge_pending: u32,
    out: [Message; MESSAGES],
    out_len: usize,
    pub dropped: u64,
    pub eois: u64,
}

impl Default for IoApic {
    fn default() -> Self {
        Self::new()
    }
}

impl IoApic {
    pub const fn new() -> Self {
        Self {
            index: 0,
            id: 0,
            entry: [MASKED; PINS],
            raw: 0,
            edge_pending: 0,
            out: [Message {
                vector: 0,
                mode: 0,
                logical: false,
                dest: 0,
                level: false,
                pin: 0,
            }; MESSAGES],
            out_len: 0,
            dropped: 0,
            eois: 0,
        }
    }

    pub fn owns(offset: u64) -> bool {
        matches!(offset, OFF_INDEX | OFF_WINDOW | OFF_EOI)
    }

    fn level_triggered(&self, pin: usize) -> bool {
        self.entry[pin] & LEVEL != 0
    }

    /// Does the pin have a request that has not been sent (held back by the mask)?
    fn waiting(&self, pin: usize) -> bool {
        if self.level_triggered(pin) {
            self.active(pin) && self.entry[pin] & MASKED != 0
        } else {
            self.edge_pending & (1 << pin) != 0
        }
    }

    fn push(&mut self, m: Message) {
        if self.out_len == MESSAGES {
            self.dropped += 1;
            return;
        }
        self.out[self.out_len] = m;
        self.out_len += 1;
    }

    fn message(&self, pin: usize) -> Message {
        let e = self.entry[pin];
        Message {
            vector: e as u8,
            mode: (e >> 8) as u8 & 7,
            logical: e & (1 << 11) != 0,
            dest: (e >> 56) as u8,
            level: e & LEVEL != 0,
            pin: pin as u8,
        }
    }

    /// Sends whatever the entry now allows.
    fn pump(&mut self, pin: usize) {
        let e = self.entry[pin];
        if e & MASKED != 0 {
            return;
        }
        if self.level_triggered(pin) {
            if self.active(pin) && e & REMOTE_IRR == 0 {
                self.entry[pin] |= REMOTE_IRR;
                let m = self.message(pin);
                self.push(m);
            }
        } else if self.edge_pending & (1 << pin) != 0 {
            self.edge_pending &= !(1 << pin);
            let m = self.message(pin);
            self.push(m);
        }
    }

    /// A device drives `pin` (0..24); `level` is the electrical level, polarity is applied here.
    pub fn set_irq(&mut self, pin: u8, level: bool) {
        let p = usize::from(pin);
        if p >= PINS {
            return;
        }
        let was = self.active(p);
        if level {
            self.raw |= 1 << p;
        } else {
            self.raw &= !(1 << p);
        }
        if !was && self.active(p) && !self.level_triggered(p) {
            self.edge_pending |= 1 << p;
        }
        self.pump(p);
    }

    /// Is the pin asserted, that is, at its active level?
    fn active(&self, pin: usize) -> bool {
        (self.raw & (1 << pin) != 0) != (self.entry[pin] & ACTIVE_LOW != 0)
    }

    /// The next message for a local APIC, oldest first.
    pub fn take_message(&mut self) -> Option<Message> {
        if self.out_len == 0 {
            return None;
        }
        let m = self.out[0];
        self.out.copy_within(1..self.out_len, 0);
        self.out_len -= 1;
        Some(m)
    }

    /// A local APIC acknowledged level-triggered `vector`: its entries' remote IRR clears, and a line that is still asserted sends again.
    pub fn eoi(&mut self, vector: u8) {
        self.eois += 1;
        for p in 0..PINS {
            let e = self.entry[p];
            if e & LEVEL != 0 && e & REMOTE_IRR != 0 && e as u8 == vector {
                self.entry[p] &= !REMOTE_IRR;
                self.pump(p);
            }
        }
    }

    fn delivery_status(&self, pin: usize) -> u64 {
        if self.waiting(pin) {
            DELIVERY_STATUS
        } else {
            0
        }
    }

    fn read_reg(&self) -> u32 {
        match self.index {
            REG_ID => u32::from(self.id) << 24,
            REG_VERSION => ((PINS as u32 - 1) << 16) | VERSION,
            REG_ARBITRATION => u32::from(self.id) << 24,
            i @ REG_REDIRECTION..=0x3F => {
                // Indexes 0x10..=0x3F name pins 0..=23, the last one.
                let pin = usize::from(i - REG_REDIRECTION) / 2;
                let e = self.entry[pin] | self.delivery_status(pin);
                if i & 1 == 0 {
                    e as u32
                } else {
                    (e >> 32) as u32
                }
            }
            _ => 0,
        }
    }

    fn write_reg(&mut self, v: u32) {
        match self.index {
            REG_ID => self.id = (v >> 24) as u8 & 0x0F,
            i @ REG_REDIRECTION..=0x3F => {
                let pin = usize::from(i - REG_REDIRECTION) / 2;
                let old = self.entry[pin];
                let new = if i & 1 == 0 {
                    (old & !0xFFFF_FFFF) | u64::from(v)
                } else {
                    (old & 0xFFFF_FFFF) | u64::from(v) << 32
                };
                // Read-only bits keep their value; reserved bits 55:17 read zero.
                let mut e = (new & WRITABLE) | (old & REMOTE_IRR);
                // Switching to edge clears the remote IRR.
                if e & LEVEL == 0 {
                    e &= !REMOTE_IRR;
                }
                // Reconfiguring the trigger mode or polarity discards a held edge; a line that
                // becomes active through the change counts as an edge.
                let was = self.active(pin);
                if (old ^ e) & (ACTIVE_LOW | LEVEL) != 0 {
                    self.edge_pending &= !(1 << pin);
                }
                self.entry[pin] = e;
                if !was && self.active(pin) && e & LEVEL == 0 {
                    self.edge_pending |= 1 << pin;
                }
                self.pump(pin);
            }
            _ => {}
        }
    }

    /// Write of a 32-bit register at `offset` into the chip's window; false if `offset` is not one of its registers.
    pub fn write(&mut self, offset: u64, value: u32) -> bool {
        match offset {
            OFF_INDEX => self.index = value as u8,
            OFF_WINDOW => self.write_reg(value),
            OFF_EOI => self.eoi(value as u8),
            _ => return false,
        }
        true
    }

    /// Read of a 32-bit register; None if `offset` is not one of its registers.
    pub fn read(&mut self, offset: u64) -> Option<u32> {
        match offset {
            OFF_INDEX => Some(u32::from(self.index)),
            OFF_WINDOW => Some(self.read_reg()),
            OFF_EOI => Some(0),
            _ => None,
        }
    }
}
