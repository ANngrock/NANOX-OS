//! 16550A UART on COM1 (ports 0x3F8..=0x3FF): what a Linux guest does to its
//! serial console: probe the chip (scratch register, loopback, FIFO
//! detection through IIR bits 7:6), program the divisor, send bytes and
//! take interrupts. The line is infinitely fast: a byte written to THR is in
//! the host-side output buffer at once and THR is empty again, so the guest
//! never waits; receive bytes are pushed by the host into a 16-byte FIFO.
//!
//! Interrupt causes follow the datasheet priority: receiver line status,
//! received data, THR empty, modem status. [`Uart::irq`] is the INTR line.
//! Not modeled (counted in `unsupported`): break, parity and framing
//! errors, FIFO trigger levels other than 1 byte, modem-control outputs
//! other than loopback.

pub const BASE: u16 = 0x3F8;
pub const PORTS: u16 = 8;
pub const FIFO_DEPTH: usize = 16;
/// Bytes of output kept for the host between calls to [`Uart::take_tx`].
pub const TX_BUFFER: usize = 256;

const IER_RX: u8 = 1;
const IER_THRE: u8 = 2;
const IER_LSR: u8 = 4;
const IER_MSR: u8 = 8;
const LCR_DLAB: u8 = 0x80;
const MCR_LOOP: u8 = 0x10;
const LSR_DR: u8 = 1;
const LSR_OE: u8 = 2;
const LSR_THRE: u8 = 0x20;
const LSR_TEMT: u8 = 0x40;
const FCR_ENABLE: u8 = 1;

#[derive(Clone, Debug)]
pub struct Uart {
    ier: u8,
    fcr: u8,
    lcr: u8,
    mcr: u8,
    lsr: u8,
    /// Modem input levels, bits 7:4 of MSR (all low unless loopback drives them).
    msr_in: u8,
    /// Delta bits, 3:0 of MSR, cleared by reading it.
    msr_delta: u8,
    scr: u8,
    divisor: u16,
    rx: [u8; FIFO_DEPTH],
    rx_len: usize,
    /// THR-empty interrupt is pending (set by an IER write or a THR write, cleared by an IIR read that reported it or a THR write).
    thre_pending: bool,
    tx: [u8; TX_BUFFER],
    tx_len: usize,
    pub dropped_tx: u64,
    pub overruns: u64,
    pub unsupported: u32,
}

impl Default for Uart {
    fn default() -> Self {
        Self::new()
    }
}

impl Uart {
    pub const fn new() -> Self {
        Self {
            ier: 0,
            fcr: 0,
            lcr: 0,
            mcr: 0,
            lsr: LSR_THRE | LSR_TEMT,
            msr_in: 0,
            msr_delta: 0,
            scr: 0,
            divisor: 0,
            rx: [0; FIFO_DEPTH],
            rx_len: 0,
            thre_pending: false,
            tx: [0; TX_BUFFER],
            tx_len: 0,
            dropped_tx: 0,
            overruns: 0,
            unsupported: 0,
        }
    }

    pub fn divisor(&self) -> u16 {
        self.divisor
    }

    pub fn owns(port: u16) -> bool {
        (BASE..BASE + PORTS).contains(&port)
    }

    /// The host sends one byte to the guest. A full FIFO sets the overrun bit and drops it.
    pub fn push_rx(&mut self, byte: u8) {
        if self.rx_len == FIFO_DEPTH {
            self.lsr |= LSR_OE;
            self.overruns += 1;
            return;
        }
        self.rx[self.rx_len] = byte;
        self.rx_len += 1;
        self.lsr |= LSR_DR;
    }

    /// Moves what the guest wrote into `out`; how many bytes.
    pub fn take_tx(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.tx_len);
        out[..n].copy_from_slice(&self.tx[..n]);
        self.tx.copy_within(n..self.tx_len, 0);
        self.tx_len -= n;
        n
    }

    fn pop_rx(&mut self) -> u8 {
        let b = self.rx[0];
        self.rx.copy_within(1..self.rx_len, 0);
        self.rx_len -= 1;
        if self.rx_len == 0 {
            self.lsr &= !LSR_DR;
        }
        b
    }

    fn send(&mut self, byte: u8) {
        if self.mcr & MCR_LOOP != 0 {
            self.push_rx(byte);
            return;
        }
        if self.tx_len == TX_BUFFER {
            self.dropped_tx += 1;
        } else {
            self.tx[self.tx_len] = byte;
            self.tx_len += 1;
        }
    }

    /// In loopback the modem inputs are the modem outputs: CTS=RTS, DSR=DTR, RI=OUT1, DCD=OUT2.
    fn modem_inputs(&self) -> u8 {
        if self.mcr & MCR_LOOP == 0 {
            return 0;
        }
        let m = self.mcr;
        (u8::from(m & 2 != 0) << 4)
            | (u8::from(m & 1 != 0) << 5)
            | (u8::from(m & 4 != 0) << 6)
            | (u8::from(m & 8 != 0) << 7)
    }

    /// Takes the new modem inputs and records what changed: DCTS, DDSR, DDCD
    /// on any change, TERI when RI falls.
    fn update_modem(&mut self) {
        let new = self.modem_inputs();
        let old = self.msr_in;
        let changed = old ^ new;
        self.msr_delta |= (changed >> 4) & 0b1011;
        if old & 0x40 != 0 && new & 0x40 == 0 {
            self.msr_delta |= 0b0100;
        }
        self.msr_in = new;
    }

    /// The highest-priority pending cause as IIR bits 3:0 (bit 0 set: none).
    fn cause(&self) -> u8 {
        if self.ier & IER_LSR != 0 && self.lsr & LSR_OE != 0 {
            0b0110
        } else if self.ier & IER_RX != 0 && self.lsr & LSR_DR != 0 {
            0b0100
        } else if self.thre_pending {
            0b0010
        } else if self.ier & IER_MSR != 0 && self.msr_delta != 0 {
            0b0000
        } else {
            1
        }
    }

    /// The INTR output (before any 8259 or IOAPIC).
    pub fn irq(&self) -> bool {
        self.cause() & 1 == 0
    }

    /// OUT to one of the eight ports; false if `port` is not COM1's.
    pub fn write(&mut self, port: u16, value: u8) -> bool {
        if !Self::owns(port) {
            return false;
        }
        let dlab = self.lcr & LCR_DLAB != 0;
        match (port - BASE, dlab) {
            (0, true) => self.divisor = (self.divisor & 0xFF00) | u16::from(value),
            (1, true) => self.divisor = (self.divisor & 0x00FF) | u16::from(value) << 8,
            (0, false) => {
                self.thre_pending = false;
                self.send(value);
                // Transmission is instantaneous: THR is empty again at once and says so.
                if self.ier & IER_THRE != 0 {
                    self.thre_pending = true;
                }
            }
            (1, false) => {
                let was = self.ier & IER_THRE != 0;
                self.ier = value & 0x0F;
                // Enabling the THR-empty interrupt while THR is empty raises it.
                if !was && self.ier & IER_THRE != 0 && self.lsr & LSR_THRE != 0 {
                    self.thre_pending = true;
                }
                if self.ier & IER_THRE == 0 {
                    self.thre_pending = false;
                }
            }
            (2, _) => {
                if value & 0x06 != 0 {
                    // Reset receive and/or transmit FIFO.
                    if value & 2 != 0 {
                        self.rx_len = 0;
                        self.lsr &= !LSR_DR;
                    }
                }
                if value & 0xC0 != 0 {
                    self.unsupported += 1; // trigger levels above one byte
                }
                self.fcr = value & FCR_ENABLE;
            }
            (3, _) => self.lcr = value,
            (4, _) => {
                if value & 0x20 != 0 {
                    self.unsupported += 1; // auto flow control (16750)
                }
                // OUT1/OUT2 drive nothing outside loopback; Linux sets OUT2 to enable the IRQ line.
                self.mcr = value & 0x1F;
                self.update_modem();
            }
            (5, _) | (6, _) => self.unsupported += 1, // LSR and MSR are read-only
            (7, _) => self.scr = value,
            _ => unreachable!("port - BASE is below 8"),
        }
        true
    }

    /// IN from one of the eight ports; None if `port` is not COM1's.
    pub fn read(&mut self, port: u16) -> Option<u8> {
        if !Self::owns(port) {
            return None;
        }
        let dlab = self.lcr & LCR_DLAB != 0;
        Some(match (port - BASE, dlab) {
            (0, true) => (self.divisor & 0xFF) as u8,
            (1, true) => (self.divisor >> 8) as u8,
            (0, false) => {
                if self.rx_len == 0 {
                    0
                } else {
                    self.pop_rx()
                }
            }
            (1, false) => self.ier,
            (2, _) => {
                let c = self.cause();
                if c == 0b0010 {
                    // Reading IIR reports the THR-empty cause and clears it.
                    self.thre_pending = false;
                }
                c | if self.fcr & FCR_ENABLE != 0 { 0xC0 } else { 0 }
            }
            (3, _) => self.lcr,
            (4, _) => self.mcr,
            (5, _) => {
                let v = self.lsr;
                self.lsr &= !LSR_OE;
                v
            }
            (6, _) => {
                let v = self.msr_in | self.msr_delta;
                self.msr_delta = 0;
                v
            }
            (7, _) => self.scr,
            _ => unreachable!("port - BASE is below 8"),
        })
    }
}
