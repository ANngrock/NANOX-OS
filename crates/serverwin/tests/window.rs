//! Scenarios: the switch, the window states, the release chord, full-server
//! mode, refusals and crashes.

use serverwin::{
    display_hz, Effect, Event, GuestState, Key, KeyRoute, Mode, Notice, Presentation, Rect, Screen,
    ServerWindow, GRAB_H, GRAB_W, MIN_H, MIN_W, MOD_ALT, MOD_CTRL, MOD_SHIFT, START_TIMEOUT_MS,
};

const SCREEN: Screen = Screen { w: 1280, h: 800 };

fn win(mode: Mode) -> ServerWindow {
    ServerWindow::new(SCREEN, mode)
}

fn go(w: &mut ServerWindow, ev: Event) -> Vec<Effect> {
    let s = w.step(ev);
    w.invariants()
        .unwrap_or_else(|e| panic!("{e} after {ev:?}"));
    s.effects().to_vec()
}

fn key(w: &mut ServerWindow, k: Key, down: bool) -> (Vec<Effect>, Option<KeyRoute>) {
    let s = w.step(Event::Key { key: k, down });
    w.invariants().unwrap();
    (s.effects().to_vec(), s.route)
}

/// A window with a running guest on a hybrid machine, shown as a window.
fn running_window() -> ServerWindow {
    let mut w = win(Mode::Hybrid);
    go(&mut w, Event::Switch);
    go(&mut w, Event::Guest(GuestState::Starting));
    go(&mut w, Event::Guest(GuestState::Running));
    go(&mut w, Event::Expand);
    w
}

#[test]
fn the_switch_opens_the_window_and_starts_the_server_through_the_supervisor() {
    let mut w = win(Mode::Desktop);
    assert_eq!(w.presentation(), Presentation::Hidden);
    assert_eq!(w.display_hz(), 0);
    // Desktop mode has no room for a server: ask first, start only when granted.
    let e = go(&mut w, Event::Switch);
    assert_eq!(
        e,
        [Effect::RequestMode(Mode::Hybrid), Effect::SetDisplayHz(1)]
    );
    assert_eq!(w.presentation(), Presentation::Collapsed);
    assert_eq!(w.wanted_mode(), Mode::Hybrid);
    assert_eq!(w.mode(), Mode::Desktop);
    let e = go(&mut w, Event::ModeGranted(Mode::Hybrid));
    assert_eq!(e, [Effect::StartGuest]);
    // Asking twice does not start twice.
    assert!(go(&mut w, Event::ModeGranted(Mode::Hybrid)).is_empty());
    go(&mut w, Event::Guest(GuestState::Starting));
    go(&mut w, Event::Guest(GuestState::Running));
    assert_eq!(go(&mut w, Event::Expand), [Effect::SetDisplayHz(30)]);
    assert_eq!(w.presentation(), Presentation::Windowed);
}

#[test]
fn on_a_hybrid_machine_the_guest_starts_at_once() {
    let mut w = win(Mode::Hybrid);
    assert_eq!(
        go(&mut w, Event::Switch),
        [Effect::StartGuest, Effect::SetDisplayHz(1)]
    );
    // Pressing the switch again while it has not reported yet does not start it twice.
    go(&mut w, Event::Switch);
    assert!(go(&mut w, Event::Switch)
        .iter()
        .all(|e| *e != Effect::StartGuest));
}

#[test]
fn a_refusal_leaves_the_window_open_and_the_guest_off() {
    let mut w = win(Mode::Desktop);
    go(&mut w, Event::Switch);
    let e = go(&mut w, Event::ModeDenied);
    assert_eq!(e, [Effect::Notice(Notice::ModeDenied)]);
    assert_eq!(w.wanted_mode(), Mode::Desktop, "the wish is withdrawn");
    assert_eq!(w.presentation(), Presentation::Collapsed);
    assert_eq!(w.guest(), GuestState::Off);
    // A later grant of something nobody asked for does not start the guest.
    assert!(go(&mut w, Event::ModeGranted(Mode::Hybrid)).is_empty());
}

#[test]
fn closing_or_collapsing_never_touches_the_guest() {
    let mut w = running_window();
    for ev in [
        Event::Collapse,
        Event::Expand,
        Event::Close,
        Event::Switch,
        Event::Close,
    ] {
        let e = go(&mut w, ev);
        assert!(
            e.iter()
                .all(|x| matches!(x, Effect::SetDisplayHz(_) | Effect::GrabInput(false))),
            "{ev:?}: {e:?}"
        );
        assert_eq!(w.guest(), GuestState::Running, "{ev:?}");
    }
    assert_eq!(w.presentation(), Presentation::Hidden);
    // Reopening a running server starts nothing.
    let e = go(&mut w, Event::Switch);
    assert_eq!(e, [Effect::SetDisplayHz(1)]);
}

#[test]
fn the_display_is_refreshed_only_as_often_as_it_is_watched() {
    let mut w = running_window();
    assert_eq!(w.display_hz(), 30);
    assert_eq!(
        go(&mut w, Event::Maximize),
        [Effect::GrabInput(true), Effect::SetDisplayHz(60)]
    );
    assert_eq!(
        go(&mut w, Event::Collapse),
        [Effect::GrabInput(false), Effect::SetDisplayHz(1)]
    );
    assert_eq!(go(&mut w, Event::Close), [Effect::SetDisplayHz(0)]);
    assert_eq!(display_hz(Presentation::Windowed, GuestState::Crashed), 1);
    // A starting guest shows its boot console, so it is watched like a running one.
    assert_eq!(
        display_hz(Presentation::Fullscreen, GuestState::Starting),
        60
    );
    assert_eq!(
        display_hz(Presentation::Fullscreen, GuestState::Stopping),
        1
    );
}

#[test]
fn the_release_chord_returns_the_keyboard_from_full_screen() {
    let mut w = running_window();
    go(&mut w, Event::Maximize);
    assert!(w.grabbed());
    assert_eq!(
        key(&mut w, Key::Ctrl, true),
        (vec![], Some(KeyRoute::Guest))
    );
    key(&mut w, Key::Alt, true);
    key(&mut w, Key::Shift, true);
    assert_eq!(w.held_modifiers(), MOD_CTRL | MOD_ALT | MOD_SHIFT);
    let (e, route) = key(&mut w, Key::G, true);
    assert_eq!(
        route,
        Some(KeyRoute::Swallowed),
        "the chord never reaches the guest"
    );
    assert_eq!(
        e,
        [
            Effect::ReleaseGuestKeys(MOD_CTRL | MOD_ALT | MOD_SHIFT),
            Effect::GrabInput(false),
            Effect::SetDisplayHz(30),
        ]
    );
    assert!(!w.grabbed());
    assert_eq!(w.presentation(), Presentation::Windowed);
    assert_eq!(w.held_modifiers(), 0, "no stuck keys in the guest");
    // The key-up of G is swallowed too; everything after goes to the OS.
    assert_eq!(key(&mut w, Key::G, false).1, Some(KeyRoute::Swallowed));
    assert_eq!(key(&mut w, Key::Other, true).1, Some(KeyRoute::Os));
    assert_eq!(
        key(&mut w, Key::G, false).1,
        Some(KeyRoute::Os),
        "only one key-up is swallowed"
    );
}

#[test]
fn keys_reach_the_guest_only_while_it_is_grabbed_and_a_lone_g_is_an_ordinary_key() {
    let mut w = running_window();
    assert_eq!(
        key(&mut w, Key::Other, true).1,
        Some(KeyRoute::Os),
        "not grabbed yet"
    );
    assert_eq!(go(&mut w, Event::Focus), [Effect::GrabInput(true)]);
    assert_eq!(go(&mut w, Event::Focus), vec![], "already grabbed");
    assert_eq!(key(&mut w, Key::Other, true).1, Some(KeyRoute::Guest));
    assert_eq!(key(&mut w, Key::G, true).1, Some(KeyRoute::Guest));
    // Ctrl alone, Alt alone, Shift+Alt: none of them is the chord.
    key(&mut w, Key::Ctrl, true);
    assert_eq!(key(&mut w, Key::G, true).1, Some(KeyRoute::Guest));
    key(&mut w, Key::Ctrl, false);
    key(&mut w, Key::Alt, true);
    key(&mut w, Key::Shift, true);
    assert_eq!(key(&mut w, Key::G, true).1, Some(KeyRoute::Guest));
    assert!(w.grabbed());
    // Released modifiers are not held any more, so the chord needs them again.
    key(&mut w, Key::Alt, false);
    key(&mut w, Key::Shift, false);
    assert_eq!(w.held_modifiers(), 0);
    // In a window (not full screen) the chord only gives the keyboard back.
    key(&mut w, Key::Ctrl, true);
    key(&mut w, Key::Alt, true);
    let (e, r) = key(&mut w, Key::G, true);
    assert_eq!(r, Some(KeyRoute::Swallowed));
    assert_eq!(
        e,
        [
            Effect::ReleaseGuestKeys(MOD_CTRL | MOD_ALT),
            Effect::GrabInput(false)
        ]
    );
    assert_eq!(w.presentation(), Presentation::Windowed);
}

#[test]
fn full_server_mode_takes_the_whole_machine_and_the_chord_gives_it_back() {
    let mut w = running_window();
    let e = go(&mut w, Event::GoFullServer);
    assert_eq!(e, [Effect::RequestMode(Mode::Server)]);
    assert!(w.pending_full_server());
    assert_eq!(
        w.presentation(),
        Presentation::Windowed,
        "not yet: the supervisor may refuse"
    );
    let e = go(&mut w, Event::ModeGranted(Mode::Server));
    assert_eq!(e, [Effect::GrabInput(true), Effect::SetDisplayHz(60)]);
    assert_eq!(
        (w.presentation(), w.mode()),
        (Presentation::Fullscreen, Mode::Server)
    );
    key(&mut w, Key::Ctrl, true);
    key(&mut w, Key::Alt, true);
    let (e, r) = key(&mut w, Key::G, true);
    assert_eq!(r, Some(KeyRoute::Swallowed));
    // The desktop is suspended in server mode: the chord must bring it back.
    assert!(e.contains(&Effect::RequestMode(Mode::Hybrid)), "{e:?}");
    assert_eq!(w.wanted_mode(), Mode::Hybrid);
    assert_eq!(w.presentation(), Presentation::Windowed);
    assert!(go(&mut w, Event::ModeGranted(Mode::Hybrid)).is_empty());
}

#[test]
fn full_server_refused_changes_nothing_and_a_hidden_window_opens_for_it() {
    let mut w = running_window();
    go(&mut w, Event::GoFullServer);
    let e = go(&mut w, Event::ModeDenied);
    assert_eq!(e, [Effect::Notice(Notice::ModeDenied)]);
    assert_eq!(
        (w.wanted_mode(), w.presentation()),
        (Mode::Hybrid, Presentation::Windowed)
    );
    // Without a running guest there is nothing to hand the machine to.
    let mut off = win(Mode::Hybrid);
    assert!(go(&mut off, Event::GoFullServer).is_empty());
    // From a hidden window it opens collapsed first.
    let mut h = running_window();
    go(&mut h, Event::Close);
    go(&mut h, Event::GoFullServer);
    assert_eq!(h.presentation(), Presentation::Collapsed);
}

#[test]
fn leaving_server_mode_is_never_given_up_on() {
    let mut w = running_window();
    go(&mut w, Event::GoFullServer);
    go(&mut w, Event::ModeGranted(Mode::Server));
    let e = go(&mut w, Event::Close);
    assert!(e.contains(&Effect::RequestMode(Mode::Hybrid)));
    // The supervisor says no: the window keeps asking.
    let e = go(&mut w, Event::ModeDenied);
    assert_eq!(e, [Effect::Notice(Notice::ModeDenied)]);
    assert_eq!(w.wanted_mode(), Mode::Hybrid, "the wish to leave stays");
    assert!(
        go(&mut w, Event::Tick { now_ms: 1_000 }).is_empty(),
        "not yet"
    );
    let e = go(&mut w, Event::Tick { now_ms: 5_000 });
    assert_eq!(e, [Effect::RequestMode(Mode::Hybrid)]);
    assert!(go(&mut w, Event::Tick { now_ms: 6_000 }).is_empty());
    assert_eq!(
        go(&mut w, Event::Tick { now_ms: 10_000 }),
        [Effect::RequestMode(Mode::Hybrid)]
    );
    go(&mut w, Event::ModeGranted(Mode::Hybrid));
    assert!(
        go(&mut w, Event::Tick { now_ms: 99_000 }).is_empty(),
        "granted: no more asking"
    );
}

#[test]
fn a_crash_in_full_server_mode_brings_the_desktop_back() {
    let mut w = running_window();
    go(&mut w, Event::GoFullServer);
    go(&mut w, Event::ModeGranted(Mode::Server));
    key(&mut w, Key::Ctrl, true);
    let e = go(&mut w, Event::Guest(GuestState::Crashed));
    assert!(e.contains(&Effect::Notice(Notice::GuestCrashed)));
    assert!(
        e.contains(&Effect::ReleaseGuestKeys(MOD_CTRL)),
        "the dead guest has no stuck keys"
    );
    assert!(e.contains(&Effect::GrabInput(false)));
    assert!(
        e.contains(&Effect::RequestMode(Mode::Hybrid)),
        "no server mode without a server"
    );
    assert!(!w.grabbed());
    // The window stays; the guest can be started again from it.
    go(&mut w, Event::ModeGranted(Mode::Hybrid));
    go(&mut w, Event::Close);
    let e = go(&mut w, Event::Switch);
    assert!(e.contains(&Effect::StartGuest), "{e:?}");
}

#[test]
fn a_guest_that_does_not_come_up_is_reported_once() {
    let mut w = win(Mode::Hybrid);
    go(&mut w, Event::Switch);
    go(&mut w, Event::Tick { now_ms: 10_000 });
    go(&mut w, Event::Guest(GuestState::Starting));
    assert!(go(
        &mut w,
        Event::Tick {
            now_ms: 10_000 + START_TIMEOUT_MS - 1
        }
    )
    .is_empty());
    let e = go(
        &mut w,
        Event::Tick {
            now_ms: 10_000 + START_TIMEOUT_MS,
        },
    );
    assert_eq!(e, [Effect::Notice(Notice::StartTimeout)]);
    assert!(go(
        &mut w,
        Event::Tick {
            now_ms: 10_000 + 2 * START_TIMEOUT_MS
        }
    )
    .is_empty());
    // A new attempt gets a new clock.
    go(&mut w, Event::Guest(GuestState::Crashed));
    go(&mut w, Event::Guest(GuestState::Starting));
    let e = go(
        &mut w,
        Event::Tick {
            now_ms: 10_000 + 3 * START_TIMEOUT_MS,
        },
    );
    assert_eq!(e, [Effect::Notice(Notice::StartTimeout)]);
    // Time never runs backwards inside the window.
    assert!(go(&mut w, Event::Tick { now_ms: 0 }).is_empty());
}

#[test]
fn the_window_stays_reachable_on_any_screen() {
    let mut w = running_window();
    assert_eq!(
        w.rect(),
        Some(Rect {
            x: 128,
            y: 80,
            w: 1024,
            h: 640
        }),
        "centered, four fifths"
    );
    go(
        &mut w,
        Event::Move {
            dx: -100_000,
            dy: -100_000,
        },
    );
    let r = w.rect().unwrap();
    assert_eq!(
        (r.x, r.y),
        (GRAB_W - r.w as i32, 0),
        "the title bar can not leave the screen"
    );
    go(
        &mut w,
        Event::Move {
            dx: i32::MAX,
            dy: i32::MAX,
        },
    );
    let r = w.rect().unwrap();
    assert_eq!(
        (r.x, r.y),
        (SCREEN.w as i32 - GRAB_W, SCREEN.h as i32 - GRAB_H)
    );
    go(&mut w, Event::Resize { w: 10, h: 10 });
    let r = w.rect().unwrap();
    assert_eq!((r.w, r.h), (MIN_W, MIN_H));
    go(
        &mut w,
        Event::Resize {
            w: 90_000,
            h: 90_000,
        },
    );
    let r = w.rect().unwrap();
    assert_eq!((r.w, r.h), (SCREEN.w, SCREEN.h));
    // The screen shrinks (a resolution change): the window follows.
    go(&mut w, Event::ScreenChanged(Screen { w: 800, h: 600 }));
    let r = w.rect().unwrap();
    assert!(r.w <= 800 && r.h <= 600 && r.x <= 800 - GRAB_W && r.y <= 600 - GRAB_H);
    // Smaller than the minimum window: the window is the screen.
    go(&mut w, Event::ScreenChanged(Screen { w: 200, h: 100 }));
    let r = w.rect().unwrap();
    assert_eq!((r.w, r.h), (200, 100));
    // Move and Resize only apply to a window, not to a tile or full screen.
    go(&mut w, Event::Maximize);
    assert_eq!(
        w.rect(),
        Some(Rect {
            x: 0,
            y: 0,
            w: 200,
            h: 100
        })
    );
    go(&mut w, Event::Move { dx: 5, dy: 5 });
    assert_eq!(
        w.rect(),
        Some(Rect {
            x: 0,
            y: 0,
            w: 200,
            h: 100
        })
    );
    go(&mut w, Event::Collapse);
    assert_eq!(w.rect(), None);
}

#[test]
fn focus_does_nothing_without_a_running_guest() {
    let mut w = win(Mode::Hybrid);
    go(&mut w, Event::Switch);
    go(&mut w, Event::Expand);
    assert_eq!(w.guest(), GuestState::Off);
    assert!(
        go(&mut w, Event::Focus).is_empty(),
        "nothing to give the keyboard to"
    );
    assert!(!w.grabbed());
    go(&mut w, Event::Guest(GuestState::Starting));
    assert!(go(&mut w, Event::Focus).is_empty(), "still starting");
}

#[test]
fn a_late_grant_of_server_mode_does_not_pull_the_window_to_full_screen() {
    let mut w = running_window();
    go(&mut w, Event::GoFullServer);
    // The user changes their mind before the supervisor answers.
    let e = go(&mut w, Event::Close);
    assert!(e.contains(&Effect::RequestMode(Mode::Hybrid)));
    // The supervisor answers the first request after all.
    assert!(go(&mut w, Event::ModeGranted(Mode::Server)).is_empty());
    assert_eq!(
        w.presentation(),
        Presentation::Hidden,
        "the late grant is not obeyed"
    );
    assert_eq!(w.wanted_mode(), Mode::Hybrid);
    assert!(go(&mut w, Event::ModeGranted(Mode::Hybrid)).is_empty());
}
