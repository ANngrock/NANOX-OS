//! virtio-input (device type 18): a keyboard, or an absolute pointer (a
//! "tablet": the host's pointer position in screen coordinates), one device
//! each. Queue 0 (eventq) carries events to the guest, queue 1 (statusq)
//! events from it (the keyboard's LEDs). An event is eight bytes, `le16 type,
//! le16 code, le32 value`, in Linux's input-event codes; the device writes one
//! event into each buffer the driver posts and ends every report with
//! EV_SYN / SYN_REPORT. No features beyond VERSION_1 are offered.
//!
//! The device configuration (136 bytes) is a window the driver points with
//! `select` (byte 0) and `subsel` (byte 1), the only bytes it writes; `size`
//! (byte 2) and the union (from byte 8) answer: the name, the serial, the ids,
//! the input properties (none), the codes of an event type, an axis's range.
//! What the device does not have answers size 0 and a union of zeros. The
//! transport does not take writes to the device configuration, so
//! [`VirtioInput::mmio_write`] takes them before it.
//!
//! The host makes reports ([`VirtioInput::key`], [`VirtioInput::button`],
//! [`VirtioInput::move_to`]); the device keeps up to [`QUEUE`] of them until
//! the driver posts buffers. A full queue drops its oldest report, whole, but
//! never one the guest has begun to receive (then the next oldest goes): the
//! guest only ever sees complete reports, and the newest state survives.
//! Reports made while the driver is not running are ignored, and a reset of
//! the device empties the queue, as for a real device the driver sets up anew.

use core::ops::Range;

use crate::virtio::{Chain, GuestMemory, VirtioPci, DEVICE, VENDOR};

pub const DEVICE_TYPE: u16 = 18;
/// Input device controller, other.
pub const CLASS: u32 = 0x09_8000;
/// The device configuration: select, subsel, size, five reserved bytes and the 128-byte union.
pub const CONFIG_LEN: usize = 136;
/// Reports the device holds for the guest.
pub const QUEUE: usize = 32;
/// Bytes of one event.
pub const EVENT_LEN: usize = 8;
const EVENTQ: usize = 0;
const STATUSQ: usize = 1;
/// Events of the longest report: ABS_X, ABS_Y, SYN_REPORT.
const MAX_REPORT: usize = 3;
const SELECT: u64 = DEVICE;
const SUBSEL: u64 = DEVICE + 1;
const UNION: usize = 8;

/// What the driver selects in the device configuration.
pub mod cfg {
    pub const UNSET: u8 = 0;
    pub const ID_NAME: u8 = 1;
    pub const ID_SERIAL: u8 = 2;
    pub const ID_DEVIDS: u8 = 3;
    pub const PROP_BITS: u8 = 0x10;
    pub const EV_BITS: u8 = 0x11;
    pub const ABS_INFO: u8 = 0x12;
}

// Linux input-event types and codes.
pub const EV_SYN: u16 = 0;
pub const EV_KEY: u16 = 1;
pub const EV_ABS: u16 = 3;
pub const EV_LED: u16 = 0x11;
pub const EV_REP: u16 = 0x14;
pub const SYN_REPORT: u16 = 0;
pub const ABS_X: u16 = 0;
pub const ABS_Y: u16 = 1;
pub const BTN_LEFT: u16 = 0x110;
pub const BTN_RIGHT: u16 = 0x111;
pub const BTN_MIDDLE: u16 = 0x112;
pub const LED_NUML: u16 = 0;
pub const LED_CAPSL: u16 = 1;
pub const LED_SCROLLL: u16 = 2;
const REP_DELAY: u16 = 0;
const REP_PERIOD: u16 = 1;
/// The ids the devices report: BUS_VIRTUAL, the virtio vendor, a product per device, version 1.0.
pub const BUS_VIRTUAL: u16 = 6;
pub const VERSION: u16 = 0x0100;

/// The keys of a 105-key PC keyboard: Esc to keypad `.` (1..=83), 102nd, F11, F12 (86..=88),
/// keypad Enter, right Ctrl, keypad `/`, SysRq, right Alt (96..=100), the navigation block
/// (102..=111), Pause (119), the two Meta keys and Compose (125..=127).
const KEYBOARD_KEYS: &[(u16, u16)] = &[
    (1, 83),
    (86, 88),
    (96, 100),
    (102, 111),
    (119, 119),
    (125, 127),
];

/// What the guest is told about a device.
#[derive(Debug)]
struct Caps {
    name: &'static str,
    serial: &'static str,
    product: u16,
    /// For each event type the device has, its codes as inclusive ranges.
    events: &'static [(u16, &'static [(u16, u16)])],
}

/// Autorepeat is the guest's (EV_REP; the host sends no repeats); the LEDs come back on the status queue.
static KEYBOARD: Caps = Caps {
    name: "NANOX keyboard",
    serial: "nanox-kbd-0",
    product: 1,
    events: &[
        (EV_KEY, KEYBOARD_KEYS),
        (EV_REP, &[(REP_DELAY, REP_PERIOD)]),
        (EV_LED, &[(LED_NUML, LED_SCROLLL)]),
    ],
};

/// No input properties: udev and libinput take absolute axes with BTN_LEFT for an absolute mouse,
/// which the host's pointer is (INPUT_PROP_DIRECT would ask for a touchscreen, POINTER for a touchpad).
static TABLET: Caps = Caps {
    name: "NANOX tablet",
    serial: "nanox-tablet-0",
    product: 2,
    events: &[
        (EV_KEY, &[(BTN_LEFT, BTN_MIDDLE)]),
        (EV_ABS, &[(ABS_X, ABS_Y)]),
    ],
};

fn event(kind: u16, code: u16, value: u32) -> [u8; EVENT_LEN] {
    let mut e = [0u8; EVENT_LEN];
    e[..2].copy_from_slice(&kind.to_le_bytes());
    e[2..4].copy_from_slice(&code.to_le_bytes());
    e[4..].copy_from_slice(&value.to_le_bytes());
    e
}

/// The events of one report; the one after the last the host gave is EV_SYN / SYN_REPORT / 0,
/// an event of zeros.
#[derive(Clone, Copy, Debug, Default)]
struct Report {
    events: [[u8; EVENT_LEN]; MAX_REPORT],
    len: usize,
}

#[derive(Clone, Debug)]
pub struct VirtioInput {
    pub t: VirtioPci,
    caps: &'static Caps,
    /// The largest ABS_X and ABS_Y (the keyboard has no axes).
    abs_max: [u32; 2],
    select: u8,
    subsel: u8,
    queue: [Report; QUEUE],
    /// Where the oldest report is, how many there are, and how many events of the oldest the guest has.
    head: usize,
    len: usize,
    sent: usize,
    /// The transport's reset count at the last look.
    seen_resets: u32,
    /// Events written into the guest's buffers.
    pub delivered: u64,
    /// Reports dropped because the queue was full.
    pub dropped: u64,
    /// Reports made while the driver was not running, thrown away.
    pub ignored: u64,
    /// Event buffers the device could not fill (not device-writable, shorter than an event, not guest memory): returned empty.
    pub bad_buffers: u64,
    /// Events the guest sent on the status queue, and status buffers that did not hold one.
    pub status_events: u64,
    pub status_bad: u64,
    /// The LEDs the guest has turned on (bit = LED code: Num Lock, Caps Lock, Scroll Lock).
    pub leds: u8,
}

/// Visits the first [`EVENT_LEN`] bytes of a chain's buffers: `f(guest address, the part of the event
/// there)`. False if a buffer is of the wrong kind (`writable`: the device writes them), the buffers
/// hold less than an event, or `f` says no. Empty buffers are skipped.
fn walk(chain: &Chain, writable: bool, mut f: impl FnMut(u64, Range<usize>) -> bool) -> bool {
    if chain.descs().iter().any(|d| d.write != writable) {
        return false;
    }
    let mut done = 0;
    for d in chain.descs() {
        let n = (d.len as usize).min(EVENT_LEN - done);
        if n != 0 && !f(d.addr, done..done + n) {
            return false;
        }
        done += n;
    }
    done == EVENT_LEN
}

/// Puts a string into the union; its length is the size.
fn text(u: &mut [u8], s: &str) -> u8 {
    u[..s.len()].copy_from_slice(s.as_bytes());
    s.len() as u8
}

/// Sets the bits of `codes` in the union; the size is the bytes up to the last one set (0: none).
fn bitmap(u: &mut [u8], codes: &[(u16, u16)]) -> u8 {
    let mut size = 0;
    for &(lo, hi) in codes {
        for c in lo..=hi {
            u[usize::from(c / 8)] |= 1 << (c % 8);
            size = size.max(c / 8 + 1);
        }
    }
    size as u8
}

impl VirtioInput {
    /// A keyboard on 8259 line `line`.
    pub fn keyboard(line: u8) -> Self {
        Self::new(&KEYBOARD, [0, 0], line)
    }

    /// An absolute pointer over a `width` x `height` screen: ABS_X 0..=width - 1, ABS_Y 0..=height - 1,
    /// the left, right and middle buttons.
    pub fn tablet(line: u8, width: u32, height: u32) -> Self {
        Self::new(
            &TABLET,
            [width.saturating_sub(1), height.saturating_sub(1)],
            line,
        )
    }

    fn new(caps: &'static Caps, abs_max: [u32; 2], line: u8) -> Self {
        let mut d = Self {
            t: VirtioPci::with_config_len(DEVICE_TYPE, CLASS, 2, 0, line, CONFIG_LEN),
            caps,
            abs_max,
            select: cfg::UNSET,
            subsel: 0,
            queue: [Report::default(); QUEUE],
            head: 0,
            len: 0,
            sent: 0,
            seen_resets: 0,
            delivered: 0,
            dropped: 0,
            ignored: 0,
            bad_buffers: 0,
            status_events: 0,
            status_bad: 0,
            leds: 0,
        };
        d.show_config();
        d
    }

    /// Reports waiting for the guest (one it has begun to receive included).
    pub fn queued(&self) -> usize {
        self.len
    }

    // ------------------------------------------------------------ the host

    /// A key goes down or up (a pointer button is a key too): EV_KEY and SYN_REPORT.
    /// False, and nothing happens, if the device has no such key.
    pub fn key(&mut self, code: u16, pressed: bool) -> bool {
        if !self.has(EV_KEY, code) {
            return false;
        }
        self.report(&[event(EV_KEY, code, u32::from(pressed))]);
        true
    }

    /// A pointer button (BTN_LEFT, BTN_RIGHT, BTN_MIDDLE) goes down or up: the same as [`VirtioInput::key`].
    pub fn button(&mut self, code: u16, pressed: bool) -> bool {
        self.key(code, pressed)
    }

    /// The pointer is at (`x`, `y`), each clamped to its axis: ABS_X, ABS_Y and SYN_REPORT.
    /// False, and nothing happens, on a device without axes.
    pub fn move_to(&mut self, x: u32, y: u32) -> bool {
        if !self.has(EV_ABS, ABS_X) {
            return false;
        }
        self.report(&[
            event(EV_ABS, ABS_X, x.min(self.abs_max[0])),
            event(EV_ABS, ABS_Y, y.min(self.abs_max[1])),
        ]);
        true
    }

    fn report(&mut self, events: &[[u8; EVENT_LEN]]) {
        self.notice_reset();
        if !self.t.driver_ok() {
            self.ignored += 1;
            return;
        }
        let mut r = Report {
            len: events.len() + 1,
            ..Report::default()
        };
        r.events[..events.len()].copy_from_slice(events);
        if self.len == QUEUE {
            // The oldest report goes; if the guest has begun to receive it, the one after it does,
            // and the oldest moves into its place.
            let victim = (self.head + usize::from(self.sent != 0)) % QUEUE;
            self.queue[victim] = self.queue[self.head];
            self.head = (self.head + 1) % QUEUE;
            self.len -= 1;
            self.dropped += 1;
        }
        self.queue[(self.head + self.len) % QUEUE] = r;
        self.len += 1;
    }

    /// A reset since the last look empties the queue: its reports were for a driver that is gone.
    fn notice_reset(&mut self) {
        if self.seen_resets != self.t.resets {
            self.seen_resets = self.t.resets;
            self.len = 0;
            self.sent = 0;
        }
    }

    fn codes(&self, kind: u16) -> &'static [(u16, u16)] {
        self.caps
            .events
            .iter()
            .find(|e| e.0 == kind)
            .map_or(&[], |e| e.1)
    }

    fn has(&self, kind: u16, code: u16) -> bool {
        self.codes(kind)
            .iter()
            .any(|&(lo, hi)| (lo..=hi).contains(&code))
    }

    // ----------------------------------------------------------- the guest

    /// A write of `size` (1, 2 or 4) bytes at `offset` of BAR 0. `select` and `subsel`, the first two
    /// bytes of the device configuration, are taken here and change what the configuration shows;
    /// every other write is the transport's.
    pub fn mmio_write(&mut self, offset: u64, size: u8, value: u32) {
        match (offset, size) {
            (SELECT, 1) => self.select = value as u8,
            (SUBSEL, 1) => self.subsel = value as u8,
            (SELECT, 2 | 4) => [self.select, self.subsel] = [value as u8, (value >> 8) as u8],
            _ => {
                self.t.mmio_write(offset, size, value);
                return;
            }
        }
        self.show_config();
    }

    /// Rebuilds the device configuration for the current `select` and `subsel`.
    fn show_config(&mut self) {
        let mut c = [0u8; CONFIG_LEN];
        c[0] = self.select;
        c[1] = self.subsel;
        let u = &mut c[UNION..];
        let size = match self.select {
            cfg::ID_NAME if self.subsel == 0 => text(u, self.caps.name),
            cfg::ID_SERIAL if self.subsel == 0 => text(u, self.caps.serial),
            cfg::ID_DEVIDS if self.subsel == 0 => {
                let ids = [BUS_VIRTUAL, VENDOR, self.caps.product, VERSION];
                for (i, v) in ids.iter().enumerate() {
                    u[2 * i..2 * i + 2].copy_from_slice(&v.to_le_bytes());
                }
                8
            }
            cfg::EV_BITS => bitmap(u, self.codes(u16::from(self.subsel))),
            cfg::ABS_INFO if self.has(EV_ABS, u16::from(self.subsel)) => {
                // min, max, fuzz, flat, resolution: all 0 but max
                u[4..8].copy_from_slice(&self.abs_max[usize::from(self.subsel)].to_le_bytes());
                20
            }
            // UNSET, PROP_BITS (no properties) and anything else.
            _ => 0,
        };
        c[2] = size;
        self.t.set_device_config(0, &c);
    }

    /// Writes queued events into the buffers the driver posted and takes what it sent on the status
    /// queue; returns how many event buffers and how many status buffers it completed.
    pub fn service(&mut self, mem: &mut dyn GuestMemory) -> (u32, u32) {
        self.t.take_kicks();
        self.notice_reset();
        if !self.t.dma_allowed() {
            return (0, 0);
        }
        (self.deliver(mem), self.take_status(mem))
    }

    fn deliver(&mut self, mem: &mut dyn GuestMemory) -> u32 {
        let mut n = 0;
        // A buffer is only taken when there is an event to put in it.
        while self.len != 0 {
            let Some(chain) = self.t.pop(mem, EVENTQ) else {
                break;
            };
            let r = self.queue[self.head];
            let e = r.events[self.sent];
            let written = if walk(&chain, true, |gpa, part| mem.write(gpa, &e[part])) {
                self.delivered += 1;
                self.sent += 1;
                if self.sent == r.len {
                    self.sent = 0;
                    self.head = (self.head + 1) % QUEUE;
                    self.len -= 1;
                }
                EVENT_LEN as u32
            } else {
                // The event stays for the next buffer.
                self.bad_buffers += 1;
                0
            };
            self.t.push_used(mem, EVENTQ, chain.head, written);
            n += 1;
        }
        n
    }

    fn take_status(&mut self, mem: &mut dyn GuestMemory) -> u32 {
        let mut n = 0;
        while let Some(chain) = self.t.pop(mem, STATUSQ) {
            let mut e = [0u8; EVENT_LEN];
            if walk(&chain, false, |gpa, part| mem.read(gpa, &mut e[part])) {
                self.status_events += 1;
                let kind = u16::from_le_bytes([e[0], e[1]]);
                let code = u16::from_le_bytes([e[2], e[3]]);
                // Only the LEDs the device has; anything else the guest sends is taken and ignored.
                if kind == EV_LED && self.has(EV_LED, code) {
                    let bit = 1u8 << code;
                    if u32::from_le_bytes([e[4], e[5], e[6], e[7]]) != 0 {
                        self.leds |= bit;
                    } else {
                        self.leds &= !bit;
                    }
                }
            } else {
                self.status_bad += 1;
            }
            self.t.push_used(mem, STATUSQ, chain.head, 0);
            n += 1;
        }
        n
    }
}
