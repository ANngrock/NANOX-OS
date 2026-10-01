//! The server window: what the interface switch opens (docs/specs/M11-WINDOW.md).
//!
//! A window that is collapsed to a tile, a normal window, or the whole screen,
//! showing a guest (the Proxmox VE server) that runs in parallel with the
//! interactive part of the system. This crate is the logic behind it, with no
//! drawing and no devices: a state machine that takes events (the switch, the
//! window buttons, keys, the supervisor and the guest reporting back) and
//! returns effects (ask for a mode, start the guest, set how often the display
//! is refreshed, grab or release input, release keys the guest still thinks
//! are held).
//!
//! Three things are kept apart on purpose:
//!
//! * **presentation** of the window (hidden, collapsed, windowed, fullscreen);
//! * the **guest** (off, starting, running, stopping, crashed), whose life is
//!   never ended by anything done to the window;
//! * the resource **mode** of the machine (`svc::Mode`), changed only by the
//!   supervisor, which can refuse.
//!
//! The rules that make a full-screen server safe to use are properties of the
//! machine, checked on every event sequence of a bounded length: the release
//! chord always returns control to the interactive part, keys the guest was
//! holding are released, input goes to the guest only while it runs, and a
//! machine in server mode never stays there without a server to look at.
//!
//! `no_std`, no allocation, safe Rust.

#![no_std]
#![forbid(unsafe_code)]

pub use svc::Mode;

#[doc(hidden)]
pub mod verify;

pub const MIN_W: u32 = 320;
pub const MIN_H: u32 = 200;
/// How much of the title bar must stay on screen (width, height).
pub const GRAB_W: i32 = 64;
pub const GRAB_H: i32 = 32;
/// A guest that has been starting for this long is reported.
pub const START_TIMEOUT_MS: u64 = 120_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presentation {
    Hidden,
    Collapsed,
    Windowed,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuestState {
    Off,
    Starting,
    Running,
    Stopping,
    Crashed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Screen {
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Ctrl,
    Alt,
    Shift,
    /// The key of the release chord (Ctrl + Alt + G).
    G,
    Other,
}

pub const MOD_CTRL: u8 = 1;
pub const MOD_ALT: u8 = 2;
pub const MOD_SHIFT: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The interface switch: opens the window collapsed, or hides it.
    Switch,
    Expand,
    Maximize,
    /// Leave full screen for a window.
    Restore,
    Collapse,
    /// Hide the window. The guest keeps running.
    Close,
    /// The user clicked into the guest.
    Focus,
    Move {
        dx: i32,
        dy: i32,
    },
    Resize {
        w: u32,
        h: u32,
    },
    ScreenChanged(Screen),
    /// Hand the whole machine to the server: ask for server mode, full screen.
    GoFullServer,
    Key {
        key: Key,
        down: bool,
    },
    Tick {
        now_ms: u64,
    },
    /// The guest changed state (reported by whatever runs it).
    Guest(GuestState),
    /// The supervisor switched the mode.
    ModeGranted(Mode),
    /// The supervisor refused the mode that was asked for.
    ModeDenied,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    ModeDenied,
    GuestCrashed,
    StartTimeout,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    /// Ask the supervisor for this mode; it answers with `ModeGranted` or `ModeDenied`.
    RequestMode(Mode),
    StartGuest,
    /// Refresh rate of the guest display: 0, 1, 30 or 60 Hz.
    SetDisplayHz(u8),
    /// Input now goes to the guest (true) or to the interactive part (false).
    GrabInput(bool),
    /// Send key-up to the guest for these modifiers (a `MOD_` mask).
    ReleaseGuestKeys(u8),
    Notice(Notice),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyRoute {
    Os,
    Guest,
    /// Consumed by the window (the release chord).
    Swallowed,
}

/// What one event did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    effects: [Effect; 8],
    n: usize,
    pub route: Option<KeyRoute>,
}

impl Step {
    pub fn effects(&self) -> &[Effect] {
        &self.effects[..self.n]
    }
}

struct Out {
    effects: [Effect; 8],
    n: usize,
    route: Option<KeyRoute>,
}

impl Out {
    fn new() -> Self {
        Self {
            effects: [Effect::StartGuest; 8],
            n: 0,
            route: None,
        }
    }
    fn push(&mut self, e: Effect) {
        // Eight is more than any single event can produce.
        if self.n < self.effects.len() {
            self.effects[self.n] = e;
            self.n += 1;
        }
    }
    fn done(self) -> Step {
        Step {
            effects: self.effects,
            n: self.n,
            route: self.route,
        }
    }
}

/// How often the guest display is refreshed for a presentation.
pub fn display_hz(p: Presentation, guest: GuestState) -> u8 {
    match (p, guest) {
        (Presentation::Hidden, _) => 0,
        (Presentation::Collapsed, _)
        | (_, GuestState::Off | GuestState::Stopping | GuestState::Crashed) => 1,
        (Presentation::Windowed, _) => 30,
        (Presentation::Fullscreen, _) => 60,
    }
}

fn clamp_rect(r: Rect, s: Screen) -> Rect {
    let w = r.w.clamp(MIN_W.min(s.w), s.w);
    let h = r.h.clamp(MIN_H.min(s.h), s.h);
    let lo_x = GRAB_W - w as i32;
    let hi_x = (s.w as i32 - GRAB_W).max(lo_x);
    let hi_y = (s.h as i32 - GRAB_H).max(0);
    Rect {
        x: r.x.clamp(lo_x, hi_x),
        y: r.y.clamp(0, hi_y),
        w,
        h,
    }
}

fn default_rect(s: Screen) -> Rect {
    let (w, h) = (s.w / 5 * 4, s.h / 5 * 4);
    clamp_rect(
        Rect {
            x: ((s.w - w.min(s.w)) / 2) as i32,
            y: ((s.h - h.min(s.h)) / 2) as i32,
            w,
            h,
        },
        s,
    )
}

#[derive(Clone, Copy, Debug)]
pub struct ServerWindow {
    screen: Screen,
    pres: Presentation,
    windowed: Rect,
    guest: GuestState,
    mode: Mode,
    want: Mode,
    start_after_mode: bool,
    start_sent: bool,
    go_server: bool,
    grab: bool,
    mods: u8,
    swallow_g: bool,
    hz: u8,
    now: u64,
    starting_since: Option<u64>,
    timeout_reported: bool,
    retry_at: Option<u64>,
}

impl ServerWindow {
    pub fn new(screen: Screen, mode: Mode) -> Self {
        Self {
            screen,
            pres: Presentation::Hidden,
            windowed: default_rect(screen),
            guest: GuestState::Off,
            mode,
            want: mode,
            start_after_mode: false,
            start_sent: false,
            go_server: false,
            grab: false,
            mods: 0,
            swallow_g: false,
            hz: 0,
            now: 0,
            starting_since: None,
            timeout_reported: false,
            retry_at: None,
        }
    }

    pub fn presentation(&self) -> Presentation {
        self.pres
    }
    pub fn guest(&self) -> GuestState {
        self.guest
    }
    /// The mode the supervisor last granted.
    pub fn mode(&self) -> Mode {
        self.mode
    }
    /// The mode last asked for (differs from [`Self::mode`] while a request is out).
    pub fn wanted_mode(&self) -> Mode {
        self.want
    }
    pub fn grabbed(&self) -> bool {
        self.grab
    }
    pub fn held_modifiers(&self) -> u8 {
        self.mods
    }
    pub fn display_hz(&self) -> u8 {
        self.hz
    }
    /// The window rectangle when it has one.
    pub fn rect(&self) -> Option<Rect> {
        match self.pres {
            Presentation::Windowed => Some(self.windowed),
            Presentation::Fullscreen => Some(Rect {
                x: 0,
                y: 0,
                w: self.screen.w,
                h: self.screen.h,
            }),
            _ => None,
        }
    }

    /// True while a request for full-server mode is in flight.
    pub fn pending_full_server(&self) -> bool {
        self.go_server
    }

    /// The state invariants, for debug assertions and for the exhaustive
    /// checks: input only reaches a running guest through a shown window,
    /// held modifiers imply a grab, server mode only with a running guest on
    /// the whole screen (or on its way there), the display rate follows the
    /// presentation, and the window stays reachable on the screen.
    pub fn invariants(&self) -> Result<(), &'static str> {
        let shown = matches!(self.pres, Presentation::Windowed | Presentation::Fullscreen);
        if self.grab && !(shown && self.guest == GuestState::Running) {
            return Err("input grabbed without a shown window and a running guest");
        }
        if self.mods != 0 && !self.grab {
            return Err("modifiers held for the guest without a grab");
        }
        if self.want == Mode::Server
            && !self.go_server
            && !(self.pres == Presentation::Fullscreen && self.guest == GuestState::Running)
        {
            return Err("server mode wanted without a running guest on the whole screen");
        }
        if self.hz != display_hz(self.pres, self.guest) {
            return Err("display rate does not follow the presentation");
        }
        let (s, r) = (self.screen, self.windowed);
        let in_range = r.w >= MIN_W.min(s.w)
            && r.w <= s.w
            && r.h >= MIN_H.min(s.h)
            && r.h <= s.h
            && r.x >= GRAB_W - r.w as i32
            && r.x <= (s.w as i32 - GRAB_W).max(GRAB_W - r.w as i32)
            && r.y >= 0
            && r.y <= (s.h as i32 - GRAB_H).max(0);
        if !in_range {
            return Err("window outside the reachable area of the screen");
        }
        Ok(())
    }

    fn drop_grab(&mut self, out: &mut Out) {
        if self.mods != 0 {
            out.push(Effect::ReleaseGuestKeys(self.mods));
            self.mods = 0;
        }
        if self.grab {
            self.grab = false;
            out.push(Effect::GrabInput(false));
        }
    }

    /// The machine must not be left in server mode with nothing to look at,
    /// and the release chord must always lead back to the interactive part.
    fn leave_server(&mut self, out: &mut Out) {
        self.go_server = false;
        if self.want == Mode::Server {
            self.want = Mode::Hybrid;
            out.push(Effect::RequestMode(Mode::Hybrid));
        }
    }

    fn ensure_guest(&mut self, out: &mut Out) {
        let down = matches!(self.guest, GuestState::Off | GuestState::Crashed);
        if !down || self.start_sent {
            return;
        }
        if self.mode != Mode::Desktop {
            self.start_sent = true;
            out.push(Effect::StartGuest);
        } else if !self.start_after_mode {
            self.start_after_mode = true;
            if self.want == Mode::Desktop {
                self.want = Mode::Hybrid;
                out.push(Effect::RequestMode(Mode::Hybrid));
            }
        }
    }

    /// Re-establishes the invariants after any change.
    fn settle(&mut self, out: &mut Out) {
        let shown = matches!(self.pres, Presentation::Windowed | Presentation::Fullscreen);
        let running = self.guest == GuestState::Running;
        // Server mode only makes sense with a running guest on the whole screen.
        if self.want == Mode::Server
            && !self.go_server
            && !(self.pres == Presentation::Fullscreen && running)
        {
            self.leave_server(out);
        }
        if self.grab && !(shown && running) {
            self.drop_grab(out);
        }
        if !self.grab && self.pres == Presentation::Fullscreen && running {
            self.grab = true;
            out.push(Effect::GrabInput(true));
        }
        let hz = display_hz(self.pres, self.guest);
        if hz != self.hz {
            self.hz = hz;
            out.push(Effect::SetDisplayHz(hz));
        }
    }
}

impl ServerWindow {
    fn hide(&mut self, out: &mut Out) {
        self.pres = Presentation::Hidden;
        self.leave_server(out);
    }

    fn key(&mut self, key: Key, down: bool, out: &mut Out) {
        // The key-up that ends a swallowed chord is swallowed too.
        if self.swallow_g && key == Key::G && !down {
            self.swallow_g = false;
            out.route = Some(KeyRoute::Swallowed);
            return;
        }
        if !self.grab {
            out.route = Some(KeyRoute::Os);
            return;
        }
        let bit = match key {
            Key::Ctrl => MOD_CTRL,
            Key::Alt => MOD_ALT,
            Key::Shift => MOD_SHIFT,
            _ => 0,
        };
        if bit != 0 {
            if down {
                self.mods |= bit;
            } else {
                self.mods &= !bit;
            }
        }
        if key == Key::G && down && self.mods & (MOD_CTRL | MOD_ALT) == MOD_CTRL | MOD_ALT {
            // Release chord: give the keyboard back, whatever the guest is doing.
            self.swallow_g = true;
            out.route = Some(KeyRoute::Swallowed);
            self.drop_grab(out);
            if self.pres == Presentation::Fullscreen {
                self.pres = Presentation::Windowed;
            }
            self.leave_server(out);
        } else {
            out.route = Some(KeyRoute::Guest);
        }
    }

    /// Applies one event.
    pub fn step(&mut self, ev: Event) -> Step {
        let mut out = Out::new();
        let visible = self.pres != Presentation::Hidden;
        match ev {
            Event::Switch => {
                if visible {
                    self.hide(&mut out);
                } else {
                    self.pres = Presentation::Collapsed;
                    self.ensure_guest(&mut out);
                }
            }
            Event::Expand if self.pres == Presentation::Collapsed => {
                self.pres = Presentation::Windowed;
            }
            Event::Maximize
                if matches!(self.pres, Presentation::Collapsed | Presentation::Windowed) =>
            {
                self.pres = Presentation::Fullscreen;
            }
            Event::Restore if self.pres == Presentation::Fullscreen => {
                self.pres = Presentation::Windowed;
            }
            Event::Collapse
                if matches!(self.pres, Presentation::Windowed | Presentation::Fullscreen) =>
            {
                self.pres = Presentation::Collapsed;
                self.leave_server(&mut out);
            }
            Event::Close if visible => self.hide(&mut out),
            Event::Focus => {
                let shown = matches!(self.pres, Presentation::Windowed | Presentation::Fullscreen);
                if shown && self.guest == GuestState::Running && !self.grab {
                    self.grab = true;
                    out.push(Effect::GrabInput(true));
                }
            }
            Event::Move { dx, dy } if self.pres == Presentation::Windowed => {
                let r = self.windowed;
                self.windowed = clamp_rect(
                    Rect {
                        x: r.x.saturating_add(dx),
                        y: r.y.saturating_add(dy),
                        ..r
                    },
                    self.screen,
                );
            }
            Event::Resize { w, h } if self.pres == Presentation::Windowed => {
                self.windowed = clamp_rect(
                    Rect {
                        w,
                        h,
                        ..self.windowed
                    },
                    self.screen,
                );
            }
            Event::ScreenChanged(s) => {
                self.screen = s;
                self.windowed = clamp_rect(self.windowed, s);
            }
            Event::GoFullServer => {
                if self.guest == GuestState::Running && self.want != Mode::Server {
                    if self.pres == Presentation::Hidden {
                        self.pres = Presentation::Collapsed;
                    }
                    self.want = Mode::Server;
                    self.go_server = true;
                    out.push(Effect::RequestMode(Mode::Server));
                }
            }
            Event::Key { key, down } => self.key(key, down, &mut out),
            Event::Tick { now_ms } => {
                self.now = self.now.max(now_ms);
                if let Some(since) = self.starting_since {
                    if !self.timeout_reported && self.now.saturating_sub(since) >= START_TIMEOUT_MS
                    {
                        self.timeout_reported = true;
                        out.push(Effect::Notice(Notice::StartTimeout));
                    }
                }
                if let Some(at) = self.retry_at {
                    if self.now >= at && self.mode == Mode::Server && self.want == Mode::Hybrid {
                        self.retry_at = Some(self.now + 5_000);
                        out.push(Effect::RequestMode(Mode::Hybrid));
                    }
                }
            }
            Event::Guest(g) => {
                self.guest = g;
                self.start_sent = false;
                self.starting_since = (g == GuestState::Starting).then_some(self.now);
                self.timeout_reported = false;
                if g == GuestState::Crashed {
                    out.push(Effect::Notice(Notice::GuestCrashed));
                }
            }
            Event::ModeGranted(m) => {
                self.mode = m;
                if m == self.want {
                    self.retry_at = None;
                }
                if self.start_after_mode && m != Mode::Desktop {
                    self.start_after_mode = false;
                    self.ensure_guest(&mut out);
                }
                if self.go_server && m == Mode::Server && self.want == Mode::Server {
                    self.go_server = false;
                    if self.guest == GuestState::Running {
                        self.pres = Presentation::Fullscreen;
                    } else {
                        self.leave_server(&mut out);
                    }
                }
            }
            Event::ModeDenied => {
                self.start_after_mode = false;
                self.go_server = false;
                if self.want == Mode::Hybrid && self.mode == Mode::Server {
                    // Leaving server mode must not be given up on: ask again later.
                    self.retry_at = Some(self.now + 5_000);
                } else {
                    self.want = self.mode;
                }
                out.push(Effect::Notice(Notice::ModeDenied));
            }
            // Events that do nothing in the current presentation.
            _ => {}
        }
        self.settle(&mut out);
        out.done()
    }
}
