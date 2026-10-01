//! The executable statement of the window rules: every event sequence of a
//! bounded length from a fresh window, with the state invariants and the
//! transition rules checked after every step. Used by the tests and by the
//! bounded exhaustive checks of `crates/proofs`.

use crate::{
    Effect, Event, GuestState, Key, KeyRoute, Mode, Presentation, Screen, ServerWindow, Step,
};

pub const MAX_DEPTH: usize = 8;

/// The events the checks combine: every kind of event, with values that reach
/// the interesting cases (clamping, timeouts, tiny screens).
pub const ALPHABET: [Event; 33] = [
    Event::Switch,
    Event::Expand,
    Event::Maximize,
    Event::Restore,
    Event::Collapse,
    Event::Close,
    Event::Focus,
    Event::Move { dx: 40, dy: 25 },
    Event::Move { dx: -5000, dy: 0 },
    Event::Resize { w: 1000, h: 700 },
    Event::Resize { w: 10, h: 10 },
    Event::ScreenChanged(Screen { w: 1280, h: 800 }),
    Event::ScreenChanged(Screen { w: 200, h: 100 }),
    Event::GoFullServer,
    Event::Tick { now_ms: 60_000 },
    Event::Tick { now_ms: 200_000 },
    Event::ModeDenied,
    Event::Key {
        key: Key::Ctrl,
        down: true,
    },
    Event::Key {
        key: Key::Ctrl,
        down: false,
    },
    Event::Key {
        key: Key::Alt,
        down: true,
    },
    Event::Key {
        key: Key::Alt,
        down: false,
    },
    Event::Key {
        key: Key::Shift,
        down: true,
    },
    Event::Key {
        key: Key::G,
        down: true,
    },
    Event::Key {
        key: Key::G,
        down: false,
    },
    Event::Key {
        key: Key::Other,
        down: true,
    },
    Event::Guest(GuestState::Off),
    Event::Guest(GuestState::Starting),
    Event::Guest(GuestState::Running),
    Event::Guest(GuestState::Stopping),
    Event::Guest(GuestState::Crashed),
    Event::ModeGranted(Mode::Desktop),
    Event::ModeGranted(Mode::Hybrid),
    Event::ModeGranted(Mode::Server),
];

#[derive(Clone, Copy, Debug)]
pub struct Failure {
    pub why: &'static str,
    pub trail: [Event; MAX_DEPTH],
    pub len: usize,
}

impl Failure {
    pub fn events(&self) -> &[Event] {
        &self.trail[..self.len]
    }
}

pub fn fresh() -> ServerWindow {
    ServerWindow::new(Screen { w: 1280, h: 800 }, Mode::Hybrid)
}

/// The rules about one transition (the state invariants are checked too).
pub fn check(
    prev: &ServerWindow,
    ev: Event,
    now: &ServerWindow,
    step: &Step,
) -> Result<(), &'static str> {
    now.invariants()?;
    for e in step.effects() {
        match e {
            Effect::StartGuest => {
                if !matches!(now.guest(), GuestState::Off | GuestState::Crashed)
                    || now.mode() == Mode::Desktop
                {
                    return Err("StartGuest with the guest up or on a desktop machine");
                }
            }
            Effect::RequestMode(Mode::Server) if prev.guest() != GuestState::Running => {
                return Err("full server requested without a running guest");
            }
            Effect::ReleaseGuestKeys(0) => return Err("an empty key release"),
            _ => {}
        }
    }
    if let Event::Key {
        key: Key::G,
        down: true,
    } = ev
    {
        if step.route == Some(KeyRoute::Swallowed) {
            if now.grabbed()
                || now.presentation() == Presentation::Fullscreen
                || now.wanted_mode() == Mode::Server
            {
                return Err("the release chord did not hand control back");
            }
            if now.held_modifiers() != 0 {
                return Err("modifiers left held after the chord");
            }
        }
    }
    if step.route == Some(KeyRoute::Guest) && !prev.grabbed() {
        return Err("a key reached the guest without a grab");
    }
    Ok(())
}

/// All sequences of `depth` events from a fresh window. Returns how many.
/// The error is large because it carries the failing trail; it is only built on failure.
#[allow(clippy::result_large_err)]
pub fn run(depth: usize) -> Result<u64, Failure> {
    assert!(depth <= MAX_DEPTH);
    let mut count = 0u64;
    let mut idx = [0usize; MAX_DEPTH];
    loop {
        let mut w = fresh();
        let mut trail = [ALPHABET[0]; MAX_DEPTH];
        for (k, &i) in idx[..depth].iter().enumerate() {
            let (prev, ev) = (w, ALPHABET[i]);
            let step = w.step(ev);
            trail[k] = ev;
            if let Err(why) = check(&prev, ev, &w, &step) {
                return Err(Failure {
                    why,
                    trail,
                    len: k + 1,
                });
            }
        }
        count += 1;
        let mut k = depth;
        loop {
            if k == 0 {
                return Ok(count);
            }
            k -= 1;
            idx[k] += 1;
            if idx[k] < ALPHABET.len() {
                break;
            }
            idx[k] = 0;
        }
    }
}

/// From every state reachable by three events (on a desktop or a hybrid
/// machine), make the guest run, show the window and grab the keyboard: the
/// release chord then always hands control back. Returns how many states had a
/// grab to release.
#[allow(clippy::result_large_err)]
pub fn chord_from_every_state() -> Result<u64, Failure> {
    let mut checked = 0;
    for a in ALPHABET {
        for b in ALPHABET {
            for c in ALPHABET {
                for start in [Mode::Desktop, Mode::Hybrid] {
                    let mut w = ServerWindow::new(Screen { w: 1280, h: 800 }, start);
                    let mut trail = [a, b, c, a, a, a, a, a];
                    for ev in [a, b, c] {
                        w.step(ev);
                    }
                    for ev in [
                        Event::Guest(GuestState::Running),
                        Event::ModeGranted(Mode::Hybrid),
                        Event::Expand,
                        Event::Focus,
                    ] {
                        w.step(ev);
                    }
                    if !w.grabbed() {
                        continue;
                    }
                    for key in [Key::Ctrl, Key::Alt] {
                        w.step(Event::Key { key, down: true });
                    }
                    let prev = w;
                    let ev = Event::Key {
                        key: Key::G,
                        down: true,
                    };
                    let step = w.step(ev);
                    trail[3] = ev;
                    let bad = if step.route != Some(KeyRoute::Swallowed) {
                        Some("the chord reached the guest")
                    } else {
                        check(&prev, ev, &w, &step).err()
                    };
                    if let Some(why) = bad {
                        return Err(Failure { why, trail, len: 4 });
                    }
                    checked += 1;
                }
            }
        }
    }
    Ok(checked)
}
