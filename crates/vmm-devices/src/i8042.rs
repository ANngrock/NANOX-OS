//! The PS/2 controller (i8042, data port 0x60, status/command port 0x64)
//! with the keyboard behind it: what Linux's i8042 and atkbd drivers do at
//! boot (controller self-test, interface test, command byte, enabling the
//! port, resetting and identifying the keyboard, selecting the scancode set)
//! and what the host feeds it afterwards ([`I8042::push_scancode`], set 2,
//! translated to set 1 when the command byte asks for it, as a PC does).
//!
//! There is no mouse: the auxiliary port is disabled, commands sent to it
//! are counted and get no answer, so a driver's probe times out as on a
//! machine without one. The controller is instantaneous: the input buffer is
//! never full. Interrupt: [`I8042::irq1`] is high while the output buffer
//! holds a byte and the command byte enables it; a PC's controller drops the
//! line when its buffer is read and raises it again with the next byte, so a
//! read that brings the next queued byte in is an edge of its own
//! ([`I8042::take_reloaded`]). The output port carries the
//! A20 gate (bit 1, [`I8042::a20_enabled`]) and the reset line (a command
//! 0xFE or a write with bit 0 clear raises [`I8042::take_reset`]).

pub const PORT_DATA: u16 = 0x60;
pub const PORT_STATUS: u16 = 0x64;
pub const QUEUE: usize = 16;

const STATUS_OBF: u8 = 1;
const STATUS_SYS: u8 = 4;
const STATUS_COMMAND: u8 = 8;
const STATUS_UNLOCKED: u8 = 0x10;
const CMD_INT: u8 = 1;
const CMD_SYS: u8 = 4;
const CMD_KBD_OFF: u8 = 0x10;
const CMD_XLAT: u8 = 0x40;
const ACK: u8 = 0xFA;
const RESEND: u8 = 0xFE;

/// Set 2 to set 1, as the controller translates when bit 6 of the command byte is set (break = 0xF0 prefix sets bit 7).
const XLATE: [u8; 128] = [
    0xff, 0x43, 0x41, 0x3f, 0x3d, 0x3b, 0x3c, 0x58, 0x64, 0x44, 0x42, 0x40, 0x3e, 0x0f, 0x29, 0x59,
    0x65, 0x38, 0x2a, 0x70, 0x1d, 0x10, 0x02, 0x5a, 0x66, 0x71, 0x2c, 0x1f, 0x1e, 0x11, 0x03, 0x5b,
    0x67, 0x2e, 0x2d, 0x20, 0x12, 0x05, 0x04, 0x5c, 0x68, 0x39, 0x2f, 0x21, 0x14, 0x13, 0x06, 0x5d,
    0x69, 0x31, 0x30, 0x23, 0x22, 0x15, 0x07, 0x5e, 0x6a, 0x72, 0x32, 0x24, 0x16, 0x08, 0x09, 0x5f,
    0x6b, 0x33, 0x25, 0x17, 0x18, 0x0b, 0x0a, 0x60, 0x6c, 0x34, 0x35, 0x26, 0x27, 0x19, 0x0c, 0x61,
    0x6d, 0x73, 0x28, 0x74, 0x1a, 0x0d, 0x62, 0x6e, 0x3a, 0x36, 0x1c, 0x1b, 0x75, 0x2b, 0x63, 0x76,
    0x55, 0x56, 0x77, 0x78, 0x79, 0x7a, 0x0e, 0x7b, 0x7c, 0x4f, 0x7d, 0x4b, 0x47, 0x7e, 0x7f, 0x6f,
    0x52, 0x53, 0x50, 0x4c, 0x4d, 0x48, 0x01, 0x45, 0x57, 0x4e, 0x51, 0x4a, 0x37, 0x49, 0x46, 0x54,
];

/// The set-2 make code the controller's translation turns into the set-1 `code`
/// (below 0x80): the inverse of the translation, for a host that receives set-1
/// bytes from a translating controller of its own and hands them on to
/// [`I8042::push_scancode`] (a release is 0xF0 and this code). None for a code
/// no set-2 key produces.
pub fn set1_to_set2(code: u8) -> Option<u8> {
    (0u8..0x80).find(|&b| XLATE[usize::from(b)] == code)
}

/// What the next byte written to port 0x60 means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Next {
    /// A keyboard command (or nothing pending).
    Keyboard,
    /// The argument of the controller command 0x60..0x7F (the command byte), 0xD1 (output port), 0xD2, 0xD3, 0xD4.
    Controller(u8),
    /// The argument of keyboard command 0xED, 0xF0 or 0xF3.
    KeyboardArg(u8),
}

#[derive(Clone, Debug)]
pub struct I8042 {
    command: u8,
    output_port: u8,
    queue: [u8; QUEUE],
    len: usize,
    last_was_command: bool,
    next: Next,
    scanning: bool,
    scancode_set: u8,
    leds: u8,
    typematic: u8,
    last_sent: u8,
    /// Set-2 break prefix seen: the next byte is a release.
    break_pending: bool,
    reset_requested: bool,
    /// The output buffer was read and the next queued byte took its place.
    reloaded: bool,
    pub dropped: u64,
    pub unsupported: u32,
}

impl Default for I8042 {
    fn default() -> Self {
        Self::new()
    }
}

impl I8042 {
    pub const fn new() -> Self {
        Self {
            command: CMD_INT | CMD_SYS | CMD_XLAT,
            output_port: 0x03,
            queue: [0; QUEUE],
            len: 0,
            last_was_command: false,
            next: Next::Keyboard,
            scanning: true,
            scancode_set: 2,
            leds: 0,
            typematic: 0x2B,
            last_sent: 0,
            break_pending: false,
            reset_requested: false,
            reloaded: false,
            dropped: 0,
            unsupported: 0,
        }
    }

    pub fn owns(port: u16) -> bool {
        port == PORT_DATA || port == PORT_STATUS
    }

    /// The command byte (what Linux reads with 0x20).
    pub fn command_byte(&self) -> u8 {
        self.command
    }

    pub fn a20_enabled(&self) -> bool {
        self.output_port & 2 != 0
    }

    /// A reset request raised by command 0xFE or by clearing output-port bit 0; reading clears it.
    pub fn take_reset(&mut self) -> bool {
        core::mem::take(&mut self.reset_requested)
    }

    /// The keyboard IRQ line.
    pub fn irq1(&self) -> bool {
        self.len > 0 && self.command & CMD_INT != 0
    }

    /// Whether the output buffer was read and refilled from the queue since the
    /// last call: the IRQ line went low and high again, an edge that an
    /// edge-triggered interrupt controller must see (Linux's atkbd waits for
    /// each byte of a reply, such as the two ID bytes after the ACK, by its
    /// interrupt). Reading clears it.
    pub fn take_reloaded(&mut self) -> bool {
        core::mem::take(&mut self.reloaded)
    }

    /// Bytes waiting in the output buffer (the host feeds keys when it is empty).
    pub fn queued(&self) -> usize {
        self.len
    }

    pub fn leds(&self) -> u8 {
        self.leds
    }

    pub fn scancode_set(&self) -> u8 {
        self.scancode_set
    }

    fn enqueue(&mut self, b: u8) {
        if self.len == QUEUE {
            self.dropped += 1;
            return;
        }
        self.queue[self.len] = b;
        self.len += 1;
    }

    /// The host presses or releases a key: `byte` is a set-2 scancode byte (0xE0 and 0xF0 prefixes included, one call each).
    pub fn push_scancode(&mut self, byte: u8) {
        if !self.scanning || self.command & CMD_KBD_OFF != 0 {
            self.dropped += 1;
            return;
        }
        if self.command & CMD_XLAT == 0 || self.scancode_set != 2 {
            self.enqueue(byte);
            return;
        }
        match byte {
            0xF0 => self.break_pending = true,
            b => {
                // The upper half of the table is the identity except F7 (0x83) and Alt+SysRq (0x84).
                let t = match b {
                    0..=0x7F => XLATE[usize::from(b)],
                    0x83 => 0x41,
                    0x84 => 0x54,
                    _ => b,
                };
                let out = if core::mem::take(&mut self.break_pending) {
                    t | 0x80
                } else {
                    t
                };
                self.enqueue(out);
            }
        }
    }

    /// IN from port 0x60 or 0x64; None if `port` is not the controller's.
    pub fn read(&mut self, port: u16) -> Option<u8> {
        match port {
            PORT_STATUS => {
                let mut s = STATUS_UNLOCKED;
                if self.len > 0 {
                    s |= STATUS_OBF;
                }
                if self.command & CMD_SYS != 0 {
                    s |= STATUS_SYS;
                }
                if self.last_was_command {
                    s |= STATUS_COMMAND;
                }
                Some(s)
            }
            PORT_DATA => Some(if self.len == 0 {
                self.last_sent
            } else {
                let b = self.queue[0];
                self.queue.copy_within(1..self.len, 0);
                self.len -= 1;
                self.last_sent = b;
                self.reloaded = self.len > 0;
                b
            }),
            _ => None,
        }
    }

    /// OUT to port 0x60 or 0x64; false if `port` is not the controller's.
    pub fn write(&mut self, port: u16, value: u8) -> bool {
        match port {
            PORT_STATUS => {
                self.last_was_command = true;
                self.controller_command(value);
            }
            PORT_DATA => {
                self.last_was_command = false;
                match core::mem::replace(&mut self.next, Next::Keyboard) {
                    Next::Controller(c) => self.controller_argument(c, value),
                    Next::KeyboardArg(c) => self.keyboard_argument(c, value),
                    Next::Keyboard => self.keyboard_command(value),
                }
            }
            _ => return false,
        }
        true
    }

    fn controller_command(&mut self, c: u8) {
        self.next = Next::Keyboard;
        match c {
            0x20 => self.enqueue(self.command),
            0x21..=0x3F => self.enqueue(0),
            0x60..=0x7F => self.next = Next::Controller(c),
            0xA7 | 0xA8 => {} // the auxiliary port stays disabled: there is none
            0xA9 => self.enqueue(0x00), // "test passed": the port is not there to fail
            0xAA => {
                self.command = CMD_INT | CMD_SYS | CMD_XLAT;
                self.len = 0;
                self.enqueue(0x55);
            }
            0xAB => self.enqueue(0x00),
            0xAD => self.command |= CMD_KBD_OFF,
            0xAE => self.command &= !CMD_KBD_OFF,
            0xC0 => self.enqueue(0x80),
            0xD0 => self.enqueue(self.output_port),
            0xD1..=0xD4 => self.next = Next::Controller(c),
            0xF0..=0xFF => {
                // Pulse output lines: bit 0 low resets the machine.
                if c & 1 == 0 {
                    self.reset_requested = true;
                }
            }
            _ => self.unsupported += 1,
        }
    }

    fn controller_argument(&mut self, c: u8, v: u8) {
        match c {
            0x60..=0x7F => {
                if c == 0x60 {
                    self.command = v;
                }
            }
            0xD1 => {
                self.output_port = v;
                if v & 1 == 0 {
                    self.reset_requested = true;
                }
            }
            0xD2 => self.enqueue(v),
            0xD3 => {}                  // would appear as mouse data
            _ => self.unsupported += 1, // 0xD4: a byte for the mouse that is not there
        }
    }

    fn keyboard_command(&mut self, v: u8) {
        match v {
            0xED | 0xF0 | 0xF3 => {
                self.enqueue(ACK);
                self.next = Next::KeyboardArg(v);
            }
            0xEE => self.enqueue(0xEE),
            0xF2 => {
                self.enqueue(ACK);
                self.enqueue(0xAB);
                self.enqueue(0x83);
            }
            0xF4 => {
                self.scanning = true;
                self.enqueue(ACK);
            }
            0xF5 | 0xF6 => {
                self.scanning = v == 0xF6;
                self.scancode_set = 2;
                self.typematic = 0x2B;
                self.enqueue(ACK);
            }
            0xFE => {
                let b = self.last_sent;
                self.enqueue(b);
            }
            0xFF => {
                self.enqueue(ACK);
                self.enqueue(0xAA);
                self.scanning = true;
                self.scancode_set = 2;
                self.leds = 0;
                self.break_pending = false;
            }
            _ => self.enqueue(RESEND),
        }
    }

    fn keyboard_argument(&mut self, c: u8, v: u8) {
        match c {
            0xED => {
                self.leds = v & 7;
                self.enqueue(ACK);
            }
            0xF3 => {
                self.typematic = v;
                self.enqueue(ACK);
            }
            _ => match v {
                0 => {
                    self.enqueue(ACK);
                    let s = self.scancode_set;
                    self.enqueue(s);
                }
                1..=3 => {
                    self.scancode_set = v;
                    self.enqueue(ACK);
                }
                _ => self.enqueue(RESEND),
            },
        }
    }
}
