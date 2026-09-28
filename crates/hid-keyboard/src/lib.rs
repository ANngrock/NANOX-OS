//! USB HID boot keyboard decoding for NANOX M9 (docs/specs/M9-HARDWARE.md).
//!
//! The target laptop has no PS/2 path the OS can rely on and no serial
//! port; a USB keyboard behind the xHCI driver (`hw-xhci`) is the first input
//! device. This crate turns 8-byte boot protocol reports (HID 1.11
//! Appendix B.1) into key events:
//!
//! * [`BootReport::parse`] validates a report; the ErrorRollOver report
//!   (every key slot 01h) is recognised and ignored, so a phantom state
//!   never releases or presses keys.
//! * [`Keyboard::feed`] compares a report with the previous one and emits
//!   events in a fixed order: releases of ordinary keys (with the old
//!   modifiers), modifier changes, then presses (with the new modifiers).
//! * [`Keyboard::tick`] generates typematic repeats from the caller's
//!   millisecond clock; only the most recently pressed ordinary key repeats.
//! * Text uses a US or Russian (ЙЦУКЕН) layout, switched with Alt+Shift;
//!   Caps Lock and Num Lock are tracked and [`Keyboard::take_led_update`]
//!   yields the LED output report byte for SET_REPORT when it changes.
//!
//! Usage IDs are from the HID Usage Tables, Keyboard/Keypad page (07h).
//! No input makes the crate panic; it keeps no heap state.

#![no_std]
#![forbid(unsafe_code)]

mod layout;

pub use layout::Layout;

/// Length of a boot protocol keyboard input report.
pub const BOOT_REPORT_LEN: usize = 8;
/// Usage ID reported in every slot when too many keys are pressed.
pub const ERROR_ROLL_OVER: u8 = 0x01;

/// Modifier byte of a boot report (byte 0).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const LEFT_CTRL: u8 = 1 << 0;
    pub const LEFT_SHIFT: u8 = 1 << 1;
    pub const LEFT_ALT: u8 = 1 << 2;
    pub const LEFT_GUI: u8 = 1 << 3;
    pub const RIGHT_CTRL: u8 = 1 << 4;
    pub const RIGHT_SHIFT: u8 = 1 << 5;
    pub const RIGHT_ALT: u8 = 1 << 6;
    pub const RIGHT_GUI: u8 = 1 << 7;

    pub const fn ctrl(self) -> bool {
        self.0 & (Self::LEFT_CTRL | Self::RIGHT_CTRL) != 0
    }
    pub const fn shift(self) -> bool {
        self.0 & (Self::LEFT_SHIFT | Self::RIGHT_SHIFT) != 0
    }
    pub const fn alt(self) -> bool {
        self.0 & (Self::LEFT_ALT | Self::RIGHT_ALT) != 0
    }
    pub const fn gui(self) -> bool {
        self.0 & (Self::LEFT_GUI | Self::RIGHT_GUI) != 0
    }
}

/// Why a report was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportError {
    /// Fewer than [`BOOT_REPORT_LEN`] bytes.
    TooShort,
}

/// A validated boot keyboard report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootReport {
    pub modifiers: Modifiers,
    /// Key usage slots (bytes 2..8); 0 = empty.
    pub keys: [u8; 6],
}

impl BootReport {
    /// An empty report: nothing pressed.
    pub const RELEASED: Self = Self {
        modifiers: Modifiers(0),
        keys: [0; 6],
    };

    /// Parses a report. Bytes after the eighth (some devices pad) are
    /// ignored; byte 1 is reserved and ignored.
    pub fn parse(b: &[u8]) -> Result<Self, ReportError> {
        if b.len() < BOOT_REPORT_LEN {
            return Err(ReportError::TooShort);
        }
        let mut keys = [0u8; 6];
        keys.copy_from_slice(&b[2..8]);
        Ok(Self {
            modifiers: Modifiers(b[0]),
            keys,
        })
    }

    /// True for the ErrorRollOver (phantom) report.
    pub fn is_rollover(&self) -> bool {
        self.keys.iter().all(|&k| k == ERROR_ROLL_OVER)
    }

    /// True if `usage` is an ordinary key pressed in this report. Error
    /// codes 01h..03h and modifier usages in the array are not keys.
    fn has(&self, usage: u8) -> bool {
        is_key(usage) && self.keys.contains(&usage)
    }
}

/// Ordinary (non-modifier) key usages that can appear in the key array.
const fn is_key(usage: u8) -> bool {
    usage >= 0x04 && usage < 0xE0
}

/// Keys without text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Named {
    Enter,
    Escape,
    Backspace,
    Tab,
    CapsLock,
    NumLock,
    ScrollLock,
    F(u8),
    PrintScreen,
    Pause,
    Insert,
    Delete,
    Home,
    End,
    PageUp,
    PageDown,
    Left,
    Right,
    Up,
    Down,
    Application,
    LeftCtrl,
    LeftShift,
    LeftAlt,
    LeftGui,
    RightCtrl,
    RightShift,
    RightAlt,
    RightGui,
}

/// What a key means in the current state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// Produces text.
    Char(char),
    /// A function, navigation, lock or modifier key.
    Named(Named),
    /// A usage this crate does not map.
    Unknown(u8),
}

/// Kind of a key event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    Press,
    Release,
    Repeat,
}

/// A key event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    /// HID usage ID (07h page); modifiers use E0h..E7h.
    pub usage: u8,
    pub action: Action,
    /// Modifiers in effect for this event.
    pub modifiers: Modifiers,
    pub key: Key,
    /// Layout used to resolve `key`.
    pub layout: Layout,
}

/// Typematic and layout switching settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// Delay before the first repeat.
    pub repeat_delay_ms: u64,
    /// Interval between repeats (0 disables repeat).
    pub repeat_interval_ms: u64,
    /// Layouts cycled by Alt+Shift, first one active initially.
    pub layouts: [Layout; 2],
}

impl Default for Config {
    fn default() -> Self {
        Self {
            repeat_delay_ms: 500,
            repeat_interval_ms: 33,
            layouts: [Layout::Us, Layout::Russian],
        }
    }
}

/// LED bits of the boot keyboard output report (HID 1.11 B.1).
pub mod led {
    pub const NUM_LOCK: u8 = 1 << 0;
    pub const CAPS_LOCK: u8 = 1 << 1;
    pub const SCROLL_LOCK: u8 = 1 << 2;
}

#[derive(Clone, Copy, Debug)]
struct Repeat {
    usage: u8,
    next_ms: u64,
}

/// Decoder state of one keyboard.
#[derive(Clone, Debug)]
pub struct Keyboard {
    cfg: Config,
    prev: BootReport,
    layout: usize,
    leds: u8,
    led_dirty: bool,
    repeat: Option<Repeat>,
}

const CAPS: u8 = 0x39;
const SCROLL: u8 = 0x47;
const NUM: u8 = 0x53;

impl Keyboard {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            prev: BootReport::RELEASED,
            layout: 0,
            leds: 0,
            led_dirty: false,
            repeat: None,
        }
    }

    /// Active layout.
    pub fn layout(&self) -> Layout {
        self.cfg.layouts[self.layout]
    }

    /// Current LED bits ([`led`]).
    pub fn leds(&self) -> u8 {
        self.leds
    }

    /// The LED output report byte, once after each change.
    pub fn take_led_update(&mut self) -> Option<u8> {
        core::mem::take(&mut self.led_dirty).then_some(self.leds)
    }

    /// Currently pressed ordinary keys and modifiers as last reported.
    pub fn state(&self) -> BootReport {
        self.prev
    }

    /// Processes one report received at `now_ms`, calling `out` for every
    /// event. A rollover report changes nothing.
    pub fn feed(&mut self, report: &BootReport, now_ms: u64, out: &mut impl FnMut(KeyEvent)) {
        if report.is_rollover() {
            return;
        }
        let old = self.prev;
        // 1. Releases of ordinary keys, with the old modifiers.
        for u in distinct_keys(&old.keys) {
            if !report.has(u) {
                if self.repeat.is_some_and(|r| r.usage == u) {
                    self.repeat = None;
                }
                out(self.event(u, Action::Release, old.modifiers));
            }
        }
        // 2. Modifier releases, then presses.
        let (was, now) = (old.modifiers.0, report.modifiers.0);
        for bit in 0..8u8 {
            if was & (1 << bit) != 0 && now & (1 << bit) == 0 {
                out(self.event(0xE0 + bit, Action::Release, report.modifiers));
            }
        }
        let alt_shift_before = old.modifiers.alt() && old.modifiers.shift();
        for bit in 0..8u8 {
            if was & (1 << bit) == 0 && now & (1 << bit) != 0 {
                out(self.event(0xE0 + bit, Action::Press, report.modifiers));
            }
        }
        // Alt+Shift switches the layout when the chord forms with no
        // ordinary key held.
        if !alt_shift_before
            && report.modifiers.alt()
            && report.modifiers.shift()
            && !report.keys.iter().any(|&k| is_key(k))
        {
            self.layout = (self.layout + 1) % self.cfg.layouts.len();
        }
        // 3. Presses, in report order, with the new modifiers.
        for u in distinct_keys(&report.keys) {
            if !old.has(u) {
                self.toggle_lock(u);
                out(self.event(u, Action::Press, report.modifiers));
                if repeats(u) && self.cfg.repeat_interval_ms > 0 {
                    self.repeat = Some(Repeat {
                        usage: u,
                        next_ms: now_ms.saturating_add(self.cfg.repeat_delay_ms),
                    });
                }
            }
        }
        self.prev = *report;
    }

    /// Emits at most one typematic repeat due at `now_ms`. Returns true if
    /// an event was emitted.
    pub fn tick(&mut self, now_ms: u64, out: &mut impl FnMut(KeyEvent)) -> bool {
        let Some(r) = self.repeat else {
            return false;
        };
        if now_ms < r.next_ms || !self.prev.has(r.usage) {
            return false;
        }
        // A late tick does not produce a burst: the next repeat is one
        // interval after this one.
        self.repeat = Some(Repeat {
            usage: r.usage,
            next_ms: now_ms.saturating_add(self.cfg.repeat_interval_ms),
        });
        out(self.event(r.usage, Action::Repeat, self.prev.modifiers));
        true
    }

    fn toggle_lock(&mut self, usage: u8) {
        let bit = match usage {
            CAPS => led::CAPS_LOCK,
            NUM => led::NUM_LOCK,
            SCROLL => led::SCROLL_LOCK,
            _ => return,
        };
        self.leds ^= bit;
        self.led_dirty = true;
    }

    fn event(&self, usage: u8, action: Action, modifiers: Modifiers) -> KeyEvent {
        let layout = self.layout();
        KeyEvent {
            usage,
            action,
            modifiers,
            key: layout::resolve(
                layout,
                usage,
                modifiers.shift(),
                self.leds & led::CAPS_LOCK != 0,
                self.leds & led::NUM_LOCK != 0,
            ),
            layout,
        }
    }
}

/// Ordinary keys of a report, each usage once, in slot order (a usage
/// repeated in one report counts once).
fn distinct_keys(keys: &[u8; 6]) -> impl Iterator<Item = u8> + '_ {
    keys.iter()
        .enumerate()
        .filter(|&(i, &u)| is_key(u) && !keys[..i].contains(&u))
        .map(|(_, &u)| u)
}

/// Lock keys do not repeat; everything else in the key array does.
const fn repeats(usage: u8) -> bool {
    !matches!(usage, CAPS | NUM | SCROLL)
}
