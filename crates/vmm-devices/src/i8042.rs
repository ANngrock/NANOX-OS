//! The PS/2 controller (i8042, data port 0x60, status/command port 0x64)
//! with the keyboard behind it: what Linux's i8042 and atkbd drivers do at
//! boot (controller self-test, interface test, command byte, enabling the
//! port, resetting and identifying the keyboard, selecting the scancode set)
//! and what the host feeds it afterwards ([`I8042::push_scancode`], set 2,
//! translated to set 1 when the command byte asks for it, as a PC does).
//!
//! The auxiliary port has a PS/2 mouse (what Linux's psmouse meets: reset,
//! identify, rate, resolution, scaling, status, stream and remote mode,
//! reporting on and off, and the IntelliMouse wheel, switched on by the rates
//! 200, 100, 80), fed by the host ([`I8042::push_mouse`]); the controller's
//! loopback (0xD3) and aux test (0xA9) answer as on a PC. The keyboard's and
//! the mouse's bytes share the output buffer in order; the status says whose
//! the next one is (AUXB). The controller is instantaneous: the input buffer
//! is never full. Interrupts: [`I8042::irq1`] is high while the output buffer
//! holds a keyboard byte, [`I8042::irq12`] while it holds a mouse byte, each
//! when the command byte enables it; a PC's controller drops the line when its
//! buffer is read and raises it again with the next byte, so a read that
//! brings the next queued byte in is an edge of its own
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
/// The byte in the output buffer is the mouse's.
const STATUS_AUXB: u8 = 0x20;
const CMD_INT: u8 = 1;
const CMD_INT2: u8 = 2;
const CMD_SYS: u8 = 4;
const CMD_KBD_OFF: u8 = 0x10;
const CMD_AUX_OFF: u8 = 0x20;
const CMD_XLAT: u8 = 0x40;
/// Mouse buttons for [`I8042::push_mouse`].
pub const MOUSE_LEFT: u8 = 1;
pub const MOUSE_RIGHT: u8 = 2;
pub const MOUSE_MIDDLE: u8 = 4;
/// The mouse's identity: a plain PS/2 mouse, or an IntelliMouse with a wheel.
pub const MOUSE_ID: u8 = 0;
pub const MOUSE_ID_WHEEL: u8 = 3;
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

/// The PS/2 mouse behind the auxiliary port.
#[derive(Clone, Debug)]
struct Mouse {
    reporting: bool,
    remote: bool,
    scaling2: bool,
    rate: u8,
    resolution: u8,
    id: u8,
    /// The last three sample rates set (the IntelliMouse knock).
    rates: [u8; 3],
    /// The argument of command 0xF3 or 0xE8 comes next.
    arg: Option<u8>,
    buttons: u8,
}

impl Mouse {
    const fn new() -> Self {
        Self {
            reporting: false,
            remote: false,
            scaling2: false,
            rate: 100,
            resolution: 2,
            id: MOUSE_ID,
            rates: [0; 3],
            arg: None,
            buttons: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct I8042 {
    command: u8,
    output_port: u8,
    queue: [u8; QUEUE],
    /// Whether each queued byte is the mouse's.
    from_aux: [bool; QUEUE],
    len: usize,
    last_was_command: bool,
    mouse: Mouse,
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
            from_aux: [false; QUEUE],
            len: 0,
            last_was_command: false,
            mouse: Mouse::new(),
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

    /// The keyboard IRQ line: a keyboard byte waits and the command byte enables it.
    pub fn irq1(&self) -> bool {
        self.len > 0 && !self.from_aux[0] && self.command & CMD_INT != 0
    }

    /// The mouse IRQ line: a mouse byte waits and the command byte enables it.
    pub fn irq12(&self) -> bool {
        self.len > 0 && self.from_aux[0] && self.command & CMD_INT2 != 0
    }

    /// The mouse's identity as the guest last set it ([`MOUSE_ID`] or [`MOUSE_ID_WHEEL`]).
    pub fn mouse_id(&self) -> u8 {
        self.mouse.id
    }

    /// The host moves the mouse by (`dx`, `dy`) — right and up are positive,
    /// as PS/2 counts them — with `buttons` held ([`MOUSE_LEFT`] and the
    /// others) and the wheel turned `wheel` notches (positive: towards the
    /// user). Becomes one packet (three bytes, four with the wheel) if the
    /// guest has reporting on in stream mode and the port enabled, and the
    /// whole packet fits the buffer; else it is counted in `dropped`.
    /// Movements beyond ±255 (±8 notches) are cut to the range.
    pub fn push_mouse(&mut self, dx: i32, dy: i32, buttons: u8, wheel: i32) {
        let m = &self.mouse;
        let size = if m.id == MOUSE_ID_WHEEL { 4 } else { 3 };
        if !m.reporting || m.remote || self.command & CMD_AUX_OFF != 0 || QUEUE - self.len < size {
            self.dropped += 1;
            return;
        }
        let (dx, dy) = (dx.clamp(-255, 255), dy.clamp(-255, 255));
        let buttons = buttons & 7;
        self.mouse.buttons = buttons;
        let first = 0x08 | buttons | u8::from(dx < 0) << 4 | u8::from(dy < 0) << 5;
        for b in [first, dx as u8, dy as u8] {
            self.enqueue_aux(b);
        }
        if size == 4 {
            self.enqueue_aux(wheel.clamp(-8, 7) as u8 & 0x0F);
        }
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
        self.enqueue_from(b, false);
    }

    fn enqueue_aux(&mut self, b: u8) {
        self.enqueue_from(b, true);
    }

    fn enqueue_from(&mut self, b: u8, aux: bool) {
        if self.len == QUEUE {
            self.dropped += 1;
            return;
        }
        self.queue[self.len] = b;
        self.from_aux[self.len] = aux;
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
                    if self.from_aux[0] {
                        s |= STATUS_AUXB;
                    }
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
                self.from_aux.copy_within(1..self.len, 0);
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
            0xA7 => self.command |= CMD_AUX_OFF,
            0xA8 => self.command &= !CMD_AUX_OFF,
            0xA9 => self.enqueue(0x00), // aux interface test passed
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
            // Loopback: as if the mouse had sent it.
            0xD3 => self.enqueue_aux(v),
            0xD4 => self.mouse_command(v),
            _ => self.unsupported += 1,
        }
    }

    /// A byte the guest sends the mouse (controller command 0xD4).
    fn mouse_command(&mut self, v: u8) {
        if let Some(c) = self.mouse.arg.take() {
            match c {
                0xF3 => {
                    let m = &mut self.mouse;
                    m.rate = v;
                    m.rates = [m.rates[1], m.rates[2], v];
                    if m.id == MOUSE_ID && m.rates == [200, 100, 80] {
                        m.id = MOUSE_ID_WHEEL;
                    }
                }
                _ => self.mouse.resolution = v & 3,
            }
            self.enqueue_aux(ACK);
            return;
        }
        match v {
            0xFF => {
                self.mouse = Mouse::new();
                for b in [ACK, 0xAA, MOUSE_ID] {
                    self.enqueue_aux(b);
                }
            }
            0xF6 => {
                let id = self.mouse.id;
                self.mouse = Mouse { id, ..Mouse::new() };
                self.enqueue_aux(ACK);
            }
            0xF5 | 0xF4 => {
                self.mouse.reporting = v == 0xF4;
                self.enqueue_aux(ACK);
            }
            0xF3 | 0xE8 => {
                self.mouse.arg = Some(v);
                self.enqueue_aux(ACK);
            }
            0xF2 => {
                let id = self.mouse.id;
                self.enqueue_aux(ACK);
                self.enqueue_aux(id);
            }
            0xF0 | 0xEA => {
                self.mouse.remote = v == 0xF0;
                self.enqueue_aux(ACK);
            }
            0xE6 | 0xE7 => {
                self.mouse.scaling2 = v == 0xE7;
                self.enqueue_aux(ACK);
            }
            0xE9 => {
                let m = &self.mouse;
                let flags = u8::from(m.remote) << 6
                    | u8::from(m.reporting) << 5
                    | u8::from(m.scaling2) << 4
                    | (m.buttons & 1) << 2
                    | (m.buttons & 4) >> 1
                    | (m.buttons & 2) >> 1;
                let (resolution, rate) = (m.resolution, m.rate);
                for b in [ACK, flags, resolution, rate] {
                    self.enqueue_aux(b);
                }
            }
            0xEB => {
                // Read data (remote mode): no movement since the last packet.
                let first = 0x08 | self.mouse.buttons;
                self.enqueue_aux(ACK);
                for b in [first, 0, 0] {
                    self.enqueue_aux(b);
                }
                if self.mouse.id == MOUSE_ID_WHEEL {
                    self.enqueue_aux(0);
                }
            }
            // Wrap mode is not modeled; set and reset are acknowledged.
            0xEC | 0xEE => self.enqueue_aux(ACK),
            _ => {
                self.unsupported += 1;
                self.enqueue_aux(RESEND);
            }
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
