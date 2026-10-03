//! PCI configuration space of one function (type 0 header, 256 bytes) as
//! enumerating software sees it: identification, class code, command and
//! status registers, up to six base address registers with the standard
//! sizing protocol (write all ones, read back the size mask), the capability
//! list, the interrupt line and pin. The bus above it — configuration
//! mechanism #1 through ports 0xCF8/0xCFC and the memory-mapped ECAM window —
//! is in [`crate::machine`], which routes an access to the right function.
//!
//! Only what the platform here needs is writable: command bits (I/O, memory,
//! bus master, INTx disable), cache-line size, latency timer, the BARs and the
//! interrupt line. Everything else is read-only; writes to it are counted in
//! `ignored_writes`. Status bit 3 (interrupt pending) follows
//! [`Config::set_interrupt_status`] and bit 4 (capability list) is set when a
//! capability was added.

pub const VENDOR_ID: usize = 0x00;
pub const DEVICE_ID: usize = 0x02;
pub const COMMAND: usize = 0x04;
pub const STATUS: usize = 0x06;
pub const REVISION: usize = 0x08;
pub const CLASS_CODE: usize = 0x09;
pub const HEADER_TYPE: usize = 0x0E;
pub const BAR0: usize = 0x10;
pub const SUBSYSTEM_VENDOR: usize = 0x2C;
pub const SUBSYSTEM_ID: usize = 0x2E;
pub const CAPABILITIES: usize = 0x34;
pub const INTERRUPT_LINE: usize = 0x3C;
pub const INTERRUPT_PIN: usize = 0x3D;

pub const CMD_IO: u16 = 1;
pub const CMD_MEMORY: u16 = 2;
pub const CMD_BUS_MASTER: u16 = 4;
pub const CMD_INTX_DISABLE: u16 = 1 << 10;
const CMD_WRITABLE: u16 =
    CMD_IO | CMD_MEMORY | CMD_BUS_MASTER | (1 << 6) | (1 << 8) | CMD_INTX_DISABLE;
const STATUS_INTERRUPT: u16 = 1 << 3;
const STATUS_CAPABILITIES: u16 = 1 << 4;
/// First byte after the header where capabilities go.
const CAP_START: usize = 0x40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarKind {
    Unused,
    /// 32-bit memory BAR of this size (a power of two, at least 16).
    Memory(u32),
    /// I/O BAR of this size (a power of two, at least 4).
    Io(u32),
}

#[derive(Clone, Debug)]
pub struct Config {
    b: [u8; 256],
    bar_kind: [BarKind; 6],
    bar_value: [u32; 6],
    command: u16,
    interrupt_status: bool,
    cap_end: usize,
    pub ignored_writes: u32,
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

impl Config {
    /// A function with the given identification; `class` is class, subclass and programming interface (24 bits).
    pub fn new(
        vendor: u16,
        device: u16,
        class: u32,
        revision: u8,
        subsystem: (u16, u16),
        pin: u8,
    ) -> Self {
        let mut b = [0u8; 256];
        put16(&mut b, VENDOR_ID, vendor);
        put16(&mut b, DEVICE_ID, device);
        b[REVISION] = revision;
        b[CLASS_CODE] = class as u8;
        b[CLASS_CODE + 1] = (class >> 8) as u8;
        b[CLASS_CODE + 2] = (class >> 16) as u8;
        put16(&mut b, SUBSYSTEM_VENDOR, subsystem.0);
        put16(&mut b, SUBSYSTEM_ID, subsystem.1);
        b[INTERRUPT_PIN] = pin;
        Self {
            b,
            bar_kind: [BarKind::Unused; 6],
            bar_value: [0; 6],
            command: 0,
            interrupt_status: false,
            cap_end: CAP_START,
            ignored_writes: 0,
        }
    }

    /// Declares base address register `n` (0..6).
    pub fn define_bar(&mut self, n: usize, kind: BarKind) {
        self.bar_kind[n] = kind;
        self.bar_value[n] = match kind {
            BarKind::Io(_) => 1,
            _ => 0,
        };
    }

    /// Appends a capability (its first byte is the id; the next-pointer byte is filled in here). Returns its offset.
    pub fn add_capability(&mut self, cap: &[u8]) -> usize {
        let at = self.cap_end;
        assert!(
            cap.len() >= 2 && at + cap.len() <= 256,
            "capability does not fit"
        );
        self.b[at..at + cap.len()].copy_from_slice(cap);
        self.b[at + 1] = 0;
        if self.b[CAPABILITIES] == 0 {
            self.b[CAPABILITIES] = at as u8;
        } else {
            // Append to the end of the chain.
            let mut p = usize::from(self.b[CAPABILITIES]);
            while self.b[p + 1] != 0 {
                p = usize::from(self.b[p + 1]);
            }
            self.b[p + 1] = at as u8;
        }
        self.cap_end = (at + cap.len()).next_multiple_of(4);
        at
    }

    /// The device's interrupt-pending bit (status bit 3), set while its INTx line is asserted.
    pub fn set_interrupt_status(&mut self, on: bool) {
        self.interrupt_status = on;
    }

    pub fn command(&self) -> u16 {
        self.command
    }

    pub fn io_enabled(&self) -> bool {
        self.command & CMD_IO != 0
    }

    pub fn memory_enabled(&self) -> bool {
        self.command & CMD_MEMORY != 0
    }

    pub fn bus_master(&self) -> bool {
        self.command & CMD_BUS_MASTER != 0
    }

    /// Is the device's INTx output allowed to reach the interrupt controller?
    pub fn intx_enabled(&self) -> bool {
        self.command & CMD_INTX_DISABLE == 0
    }

    pub fn interrupt_line(&self) -> u8 {
        self.b[INTERRUPT_LINE]
    }

    fn status(&self) -> u16 {
        (if self.interrupt_status {
            STATUS_INTERRUPT
        } else {
            0
        }) | (if self.b[CAPABILITIES] != 0 {
            STATUS_CAPABILITIES
        } else {
            0
        })
    }

    /// The decoded base of BAR `n`, if it is defined and its space is enabled in the command register.
    pub fn bar_base(&self, n: usize) -> Option<u64> {
        match self.bar_kind[n] {
            BarKind::Unused => None,
            BarKind::Memory(_) if self.memory_enabled() => {
                Some(u64::from(self.bar_value[n] & !0xF))
            }
            BarKind::Io(_) if self.io_enabled() => Some(u64::from(self.bar_value[n] & !3)),
            _ => None,
        }
    }

    /// Which memory BAR and offset an address falls in, if the function decodes it.
    pub fn memory_hit(&self, addr: u64) -> Option<(usize, u64)> {
        (0..6).find_map(|n| match (self.bar_kind[n], self.bar_base(n)) {
            (BarKind::Memory(size), Some(base))
                if base != 0 && (base..base + u64::from(size)).contains(&addr) =>
            {
                Some((n, addr - base))
            }
            _ => None,
        })
    }

    /// Which I/O BAR and offset a port falls in, if the function decodes it.
    pub fn io_hit(&self, port: u16) -> Option<(usize, u64)> {
        (0..6).find_map(|n| match (self.bar_kind[n], self.bar_base(n)) {
            (BarKind::Io(size), Some(base))
                if base != 0 && (base..base + u64::from(size)).contains(&u64::from(port)) =>
            {
                Some((n, u64::from(port) - base))
            }
            _ => None,
        })
    }

    fn byte(&self, off: usize) -> u8 {
        match off {
            COMMAND => self.command as u8,
            o if o == COMMAND + 1 => (self.command >> 8) as u8,
            STATUS => self.status() as u8,
            o if o == STATUS + 1 => (self.status() >> 8) as u8,
            o @ 0x10..=0x27 => (self.bar_value[(o - BAR0) / 4] >> (8 * ((o - BAR0) % 4))) as u8,
            o => self.b[o],
        }
    }

    /// Reads `size` (1, 2 or 4) bytes at `off`; offsets past 255 read as ones.
    pub fn read(&self, off: usize, size: u8) -> u32 {
        let mut v = 0u32;
        for i in 0..usize::from(size) {
            let byte = if off + i < 256 {
                self.byte(off + i)
            } else {
                0xFF
            };
            v |= u32::from(byte) << (8 * i);
        }
        v
    }

    fn write_byte(&mut self, off: usize, v: u8) {
        match off {
            COMMAND => {
                self.command = (self.command & 0xFF00) | (u16::from(v) & (CMD_WRITABLE & 0xFF))
            }
            o if o == COMMAND + 1 => {
                self.command =
                    (self.command & 0x00FF) | ((u16::from(v) << 8) & (CMD_WRITABLE & 0xFF00))
            }
            0x0C | 0x0D | INTERRUPT_LINE => self.b[off] = v,
            _ => self.ignored_writes += 1,
        }
    }

    /// Writes `size` (1, 2 or 4) bytes at `off`. A BAR is written as a whole word (the sizing protocol).
    pub fn write(&mut self, off: usize, size: u8, value: u32) {
        if off >= 256 {
            return;
        }
        if (0x10..0x28).contains(&off) && size == 4 && off.is_multiple_of(4) {
            let n = (off - BAR0) / 4;
            self.bar_value[n] = match self.bar_kind[n] {
                BarKind::Unused => 0,
                BarKind::Memory(s) => value & !(s - 1) & !0xF,
                BarKind::Io(s) => (value & !(s - 1) & !3) | 1,
            };
            return;
        }
        for i in 0..usize::from(size) {
            if off + i < 256 {
                self.write_byte(off + i, (value >> (8 * i)) as u8);
            }
        }
    }
}
