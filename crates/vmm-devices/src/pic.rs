//! The pair of 8259A interrupt controllers of a PC (master 0x20/0x21, slave
//! 0xA0/0xA1, the slave cascaded on the master's IRQ2) and the edge/level
//! control registers ELCR (0x4D0/0x4D1). A Linux guest with an IOAPIC masks
//! the PICs and probes them; one without uses them for IRQ0..15.
//!
//! Modeled: ICW1..ICW4 (cascade, 8086 mode, automatic EOI), OCW1 mask,
//! non-specific and specific EOI, OCW3 register reads and poll mode, fixed
//! priority with the in-service rule (a request is delivered only if it
//! outranks every in-service one), edge and level triggering per ELCR, the
//! cascade (the slave's INT is the master's IRQ2, and drops while the slave
//! is serving and rises again after its EOI if more is pending), and the
//! spurious vector (IRQ7 of the chip that had nothing). Not modeled, and
//! counted in `unsupported`: rotation, special mask mode, single mode,
//! non-8086 mode, level-sense through ICW1.
//!
//! Interface towards the CPU: [`Pic::int_pending`] is the INT output;
//! [`Pic::acknowledge`] is the interrupt-acknowledge cycle and returns the
//! vector.

pub const MASTER_COMMAND: u16 = 0x20;
pub const MASTER_DATA: u16 = 0x21;
pub const SLAVE_COMMAND: u16 = 0xA0;
pub const SLAVE_DATA: u16 = 0xA1;
pub const ELCR_MASTER: u16 = 0x4D0;
pub const ELCR_SLAVE: u16 = 0x4D1;

/// ELCR bits that are fixed to edge: IRQ0, 1, 2 and IRQ8, 13.
const ELCR_MASTER_FIXED: u8 = 0b0000_0111;
const ELCR_SLAVE_FIXED: u8 = 0b0010_0001;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Init {
    Ready,
    Icw2,
    Icw3,
    Icw4,
}

#[derive(Clone, Copy, Debug)]
struct Chip {
    irr: u8,
    isr: u8,
    imr: u8,
    /// Current input levels.
    lines: u8,
    base: u8,
    init: Init,
    icw4_needed: bool,
    icw3: u8,
    auto_eoi: bool,
    read_isr: bool,
    poll: bool,
}

impl Chip {
    const fn new() -> Self {
        Self {
            irr: 0,
            isr: 0,
            imr: 0xFF,
            lines: 0,
            base: 0,
            init: Init::Ready,
            icw4_needed: false,
            icw3: 0,
            auto_eoi: false,
            read_isr: false,
            poll: false,
        }
    }

    /// The request that would be delivered now: unmasked, and outranking everything in service.
    fn winner(&self) -> Option<u8> {
        let ready = self.irr & !self.imr;
        if ready == 0 {
            return None;
        }
        let top = ready.trailing_zeros() as u8;
        if self.isr == 0 || top < self.isr.trailing_zeros() as u8 {
            Some(top)
        } else {
            None
        }
    }

    /// Takes the request: it goes in service (or, with automatic EOI, straight through).
    fn take(&mut self, irq: u8, level: bool) {
        if !level {
            self.irr &= !(1 << irq);
        }
        if !self.auto_eoi {
            self.isr |= 1 << irq;
        }
    }
}

#[derive(Clone, Debug)]
pub struct Pic {
    chip: [Chip; 2],
    elcr: [u8; 2],
    pub unsupported: u32,
    pub spurious: u32,
}

impl Default for Pic {
    fn default() -> Self {
        Self::new()
    }
}

impl Pic {
    pub const fn new() -> Self {
        Self {
            chip: [Chip::new(), Chip::new()],
            elcr: [0, 0],
            unsupported: 0,
            spurious: 0,
        }
    }

    pub fn owns(port: u16) -> bool {
        matches!(
            port,
            MASTER_COMMAND | MASTER_DATA | SLAVE_COMMAND | SLAVE_DATA | ELCR_MASTER | ELCR_SLAVE
        )
    }

    fn level_triggered(&self, chip: usize, irq: u8) -> bool {
        self.elcr[chip] & (1 << irq) != 0
    }

    /// A device drives IRQ line `irq` (0..16) high or low.
    pub fn set_irq(&mut self, irq: u8, level: bool) {
        debug_assert!(irq < 16);
        let (c, n) = ((irq >> 3) as usize, irq & 7);
        let bit = 1u8 << n;
        let was = self.chip[c].lines & bit != 0;
        if level {
            self.chip[c].lines |= bit;
            if self.level_triggered(c, n) || !was {
                self.chip[c].irr |= bit;
            }
        } else {
            self.chip[c].lines &= !bit;
            if self.level_triggered(c, n) {
                self.chip[c].irr &= !bit;
            }
        }
        self.cascade();
    }

    /// The slave's INT output is the master's IRQ2 (an edge input).
    fn cascade(&mut self) {
        let up = self.chip[1].winner().is_some();
        let m = &mut self.chip[0];
        let was = m.lines & 4 != 0;
        if up {
            m.lines |= 4;
            if !was {
                m.irr |= 4;
            }
        } else {
            m.lines &= !4;
        }
    }

    /// The INT output towards the CPU.
    pub fn int_pending(&self) -> bool {
        self.chip[0].winner().is_some()
    }

    /// An interrupt-acknowledge cycle: the vector of the request taken, or the
    /// spurious vector (IRQ7 of the chip that had none).
    pub fn acknowledge(&mut self) -> u8 {
        let Some(irq) = self.chip[0].winner() else {
            self.spurious += 1;
            return self.chip[0].base.wrapping_add(7);
        };
        let level = self.level_triggered(0, irq);
        self.chip[0].take(irq, level);
        let vector = if irq == 2 && self.chip[0].icw3 & 4 != 0 {
            match self.chip[1].winner() {
                Some(s) => {
                    let level = self.level_triggered(1, s);
                    self.chip[1].take(s, level);
                    self.chip[1].base.wrapping_add(s)
                }
                None => {
                    self.spurious += 1;
                    self.chip[1].base.wrapping_add(7)
                }
            }
        } else {
            self.chip[0].base.wrapping_add(irq)
        };
        self.cascade();
        vector
    }

    /// OUT to a PIC or ELCR port; false if the port is not modeled here.
    pub fn write(&mut self, port: u16, value: u8) -> bool {
        let c = match port {
            MASTER_COMMAND | MASTER_DATA => 0,
            SLAVE_COMMAND | SLAVE_DATA => 1,
            ELCR_MASTER => {
                self.elcr[0] = value & !ELCR_MASTER_FIXED;
                return true;
            }
            ELCR_SLAVE => {
                self.elcr[1] = value & !ELCR_SLAVE_FIXED;
                return true;
            }
            _ => return false,
        };
        if port & 1 == 0 {
            self.command(c, value);
        } else {
            self.data(c, value);
        }
        self.cascade();
        true
    }

    fn command(&mut self, c: usize, v: u8) {
        if v & 0x10 != 0 {
            // ICW1: reset the chip and start the initialisation sequence.
            let ch = &mut self.chip[c];
            ch.irr = 0;
            ch.isr = 0;
            ch.imr = 0;
            ch.lines = 0;
            ch.auto_eoi = false;
            ch.read_isr = false;
            ch.poll = false;
            ch.icw4_needed = v & 1 != 0;
            ch.init = Init::Icw2;
            if v & 2 != 0 || v & 8 != 0 {
                self.unsupported += 1; // single mode, level-sense by ICW1
            }
            return;
        }
        if v & 8 != 0 {
            // OCW3.
            let ch = &mut self.chip[c];
            if v & 0x60 == 0x60 {
                self.unsupported += 1; // special mask mode
            }
            if v & 4 != 0 {
                ch.poll = true;
            }
            if v & 2 != 0 {
                ch.read_isr = v & 1 != 0;
            }
            return;
        }
        // OCW2.
        match v >> 5 {
            0b001 => {
                // Non-specific EOI: the highest-priority in-service request.
                let ch = &mut self.chip[c];
                if ch.isr != 0 {
                    ch.isr &= !(1 << ch.isr.trailing_zeros());
                }
            }
            0b011 => self.chip[c].isr &= !(1 << (v & 7)), // specific EOI
            0b010 => {}                                   // no operation
            _ => self.unsupported += 1,                   // rotation, set priority
        }
    }

    fn data(&mut self, c: usize, v: u8) {
        let ch = &mut self.chip[c];
        match ch.init {
            Init::Ready => ch.imr = v,
            Init::Icw2 => {
                ch.base = v & 0xF8;
                ch.init = Init::Icw3;
            }
            Init::Icw3 => {
                ch.icw3 = v;
                ch.init = if ch.icw4_needed {
                    Init::Icw4
                } else {
                    Init::Ready
                };
            }
            Init::Icw4 => {
                ch.auto_eoi = v & 2 != 0;
                if v & 1 == 0 {
                    self.unsupported += 1; // MCS-80/85 mode
                }
                ch.init = Init::Ready;
            }
        }
    }

    /// IN from a PIC or ELCR port; None if the port is not modeled here.
    pub fn read(&mut self, port: u16) -> Option<u8> {
        let c = match port {
            MASTER_COMMAND | MASTER_DATA => 0,
            SLAVE_COMMAND | SLAVE_DATA => 1,
            ELCR_MASTER => return Some(self.elcr[0]),
            ELCR_SLAVE => return Some(self.elcr[1]),
            _ => return None,
        };
        if port & 1 == 1 {
            return Some(self.chip[c].imr);
        }
        if self.chip[c].poll {
            // Poll: acknowledge like an INTA, and report the request with bit 7.
            self.chip[c].poll = false;
            return Some(match self.chip[c].winner() {
                Some(irq) => {
                    let level = self.level_triggered(c, irq);
                    self.chip[c].take(irq, level);
                    self.cascade();
                    0x80 | irq
                }
                None => 0,
            });
        }
        Some(if self.chip[c].read_isr {
            self.chip[c].isr
        } else {
            self.chip[c].irr
        })
    }
}
