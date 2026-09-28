//! Boot keyboard decoding: report validation, event order, rollover,
//! locks and LEDs, US and Russian typing, layout switching, keypad,
//! typematic repeat and a randomized press/release balance check.

use std::collections::HashMap;

use hid_keyboard::{
    led, Action, BootReport, Config, Key, KeyEvent, Keyboard, Layout, Modifiers, Named, ReportError,
};

const SHIFT: u8 = Modifiers::LEFT_SHIFT;
const ALT: u8 = Modifiers::LEFT_ALT;

fn r(mods: u8, keys: &[u8]) -> BootReport {
    let mut b = [0u8; 8];
    b[0] = mods;
    b[2..2 + keys.len()].copy_from_slice(keys);
    BootReport::parse(&b).unwrap()
}

fn feed(kb: &mut Keyboard, reports: &[BootReport]) -> Vec<KeyEvent> {
    let mut ev = Vec::new();
    for rep in reports {
        kb.feed(rep, 0, &mut |e| ev.push(e));
    }
    ev
}

fn text(events: &[KeyEvent]) -> String {
    events
        .iter()
        .filter(|e| e.action != Action::Release)
        .filter_map(|e| match e.key {
            Key::Char(c) => Some(c),
            _ => None,
        })
        .collect()
}

/// (usage, shift) that produces `c` in `layout`, found through the public
/// API only.
fn find_key(layout: Layout, c: char) -> (u8, bool) {
    for shift in [false, true] {
        for usage in 0x04..=0x38u8 {
            let mut kb = Keyboard::new(Config {
                layouts: [layout, layout],
                ..Config::default()
            });
            let ev = feed(&mut kb, &[r(if shift { SHIFT } else { 0 }, &[usage])]);
            if ev.iter().any(|e| e.usage == usage && e.key == Key::Char(c)) {
                return (usage, shift);
            }
        }
    }
    panic!("{c:?} not on the {layout:?} layout");
}

/// Reports typing `s` key by key (press, release).
pub fn type_reports(layout: Layout, s: &str) -> Vec<BootReport> {
    let mut out = Vec::new();
    for c in s.chars() {
        let (u, shift) = find_key(layout, c);
        let m = if shift { SHIFT } else { 0 };
        out.push(r(m, &[u]));
        out.push(r(m, &[]));
    }
    out.push(r(0, &[]));
    out
}

#[test]
fn reports_are_validated() {
    assert_eq!(BootReport::parse(&[0; 7]), Err(ReportError::TooShort));
    let rep = BootReport::parse(&[SHIFT, 0xFF, 4, 5, 0, 0, 0, 0, 9, 9]).unwrap();
    assert_eq!(rep.modifiers, Modifiers(SHIFT));
    assert_eq!(rep.keys, [4, 5, 0, 0, 0, 0]);
    assert!(r(0, &[1, 1, 1, 1, 1, 1]).is_rollover());
    assert!(!r(0, &[1, 1, 1, 1, 1, 4]).is_rollover());
}

#[test]
fn us_typing() {
    let mut kb = Keyboard::new(Config::default());
    let s = "Hello, World! 1+1=2 [ok] {\"q\"} a_b\\c|d";
    let ev = feed(&mut kb, &type_reports(Layout::Us, s));
    assert_eq!(text(&ev), s);
    assert_eq!(kb.state(), BootReport::RELEASED);
}

#[test]
fn events_are_ordered_release_modifiers_press() {
    let mut kb = Keyboard::new(Config::default());
    feed(&mut kb, &[r(0, &[0x04])]); // 'a'
    let ev = feed(&mut kb, &[r(SHIFT, &[0x05])]); // shift + 'b'
    let summary: Vec<(u8, Action, Key)> = ev.iter().map(|e| (e.usage, e.action, e.key)).collect();
    assert_eq!(
        summary,
        vec![
            (0x04, Action::Release, Key::Char('a')),
            (0xE1, Action::Press, Key::Named(Named::LeftShift)),
            (0x05, Action::Press, Key::Char('B')),
        ]
    );
    assert_eq!(
        ev[0].modifiers,
        Modifiers(0),
        "release carries the old modifiers"
    );
    assert_eq!(ev[2].modifiers, Modifiers(SHIFT));
}

#[test]
fn rollover_and_error_codes_change_nothing() {
    let mut kb = Keyboard::new(Config::default());
    feed(&mut kb, &[r(0, &[0x04, 0x05])]);
    assert!(feed(&mut kb, &[r(0, &[1, 1, 1, 1, 1, 1])]).is_empty());
    assert_eq!(kb.state().keys[..2], [0x04, 0x05]);
    // Error codes 02h/03h and duplicates are not keys.
    let ev = feed(&mut kb, &[r(0, &[0x04, 0x02, 0x03, 0x04, 0x05])]);
    assert!(ev.is_empty(), "{ev:?}");
    let ev = feed(&mut kb, &[r(0, &[])]);
    assert_eq!(ev.len(), 2, "each key released once: {ev:?}");
}

#[test]
fn caps_lock_and_led_report() {
    let mut kb = Keyboard::new(Config::default());
    assert_eq!(kb.take_led_update(), None);
    feed(&mut kb, &[r(0, &[0x39]), r(0, &[])]);
    assert_eq!(kb.leds(), led::CAPS_LOCK);
    assert_eq!(kb.take_led_update(), Some(led::CAPS_LOCK));
    assert_eq!(kb.take_led_update(), None, "only once per change");
    let ev = feed(&mut kb, &type_reports(Layout::Us, "a1;"));
    assert_eq!(text(&ev), "A1;", "caps affects letters only");
    let ev = feed(&mut kb, &[r(SHIFT, &[0x04]), r(0, &[])]);
    assert_eq!(text(&ev), "a", "shift inverts caps");
    feed(&mut kb, &[r(0, &[0x39]), r(0, &[])]);
    assert_eq!(kb.leds(), 0);
    assert_eq!(kb.take_led_update(), Some(0));
    // Scroll Lock and Num Lock bits.
    feed(&mut kb, &[r(0, &[0x47, 0x53]), r(0, &[])]);
    assert_eq!(kb.leds(), led::SCROLL_LOCK | led::NUM_LOCK);
}

#[test]
fn russian_typing_and_layout_switch() {
    let mut kb = Keyboard::new(Config::default());
    assert_eq!(kb.layout(), Layout::Us);
    // Alt+Shift: pressing Shift while Alt is held forms the chord.
    feed(&mut kb, &[r(ALT, &[]), r(ALT | SHIFT, &[]), r(0, &[])]);
    assert_eq!(kb.layout(), Layout::Russian);
    let s = "Привет, мир! Ёж съел 3 блюда №5; эхо: \"ю\".";
    let ev = feed(&mut kb, &type_reports(Layout::Russian, s));
    assert_eq!(text(&ev), s);
    assert!(ev.iter().all(|e| e.layout == Layout::Russian));
    // Caps Lock works for Cyrillic letters too.
    feed(&mut kb, &[r(0, &[0x39]), r(0, &[])]);
    let ev = feed(&mut kb, &type_reports(Layout::Russian, "ж."));
    assert_eq!(text(&ev), "Ж.");
    // Switch back.
    feed(&mut kb, &[r(SHIFT, &[]), r(SHIFT | ALT, &[]), r(0, &[])]);
    assert_eq!(kb.layout(), Layout::Us);
}

#[test]
fn layout_switch_rules() {
    let mut kb = Keyboard::new(Config::default());
    // With an ordinary key held the chord is a shortcut, not a switch.
    feed(
        &mut kb,
        &[r(0, &[0x04]), r(ALT | SHIFT, &[0x04]), r(0, &[])],
    );
    assert_eq!(kb.layout(), Layout::Us);
    // Holding the chord switches once, not per report.
    feed(
        &mut kb,
        &[
            r(ALT | SHIFT, &[]),
            r(ALT | SHIFT, &[]),
            r(ALT | SHIFT, &[]),
        ],
    );
    assert_eq!(kb.layout(), Layout::Russian);
    feed(&mut kb, &[r(0, &[])]);
    // Right-hand modifiers count as well.
    feed(
        &mut kb,
        &[
            r(Modifiers::RIGHT_ALT | Modifiers::RIGHT_SHIFT, &[]),
            r(0, &[]),
        ],
    );
    assert_eq!(kb.layout(), Layout::Us);
}

#[test]
fn keypad_follows_num_lock() {
    let mut kb = Keyboard::new(Config::default());
    let keys = |kb: &mut Keyboard| -> Vec<Key> {
        feed(kb, &[r(0, &[0x59, 0x62, 0x63]), r(0, &[])])
            .into_iter()
            .filter(|e| e.action == Action::Press)
            .map(|e| e.key)
            .collect()
    };
    assert_eq!(
        keys(&mut kb),
        vec![
            Key::Named(Named::End),
            Key::Named(Named::Insert),
            Key::Named(Named::Delete)
        ]
    );
    feed(&mut kb, &[r(0, &[0x53]), r(0, &[])]);
    assert_eq!(
        keys(&mut kb),
        vec![Key::Char('1'), Key::Char('0'), Key::Char('.')]
    );
    let ev = feed(&mut kb, &[r(0, &[0x55, 0x58]), r(0, &[])]);
    assert_eq!(ev[0].key, Key::Char('*'));
    assert_eq!(ev[1].key, Key::Named(Named::Enter));
}

#[test]
fn typematic_repeat() {
    let mut kb = Keyboard::new(Config::default());
    let mut ev = Vec::new();
    kb.feed(&r(0, &[0x04]), 0, &mut |e| ev.push(e));
    assert!(!kb.tick(499, &mut |e| ev.push(e)));
    assert!(kb.tick(500, &mut |e| ev.push(e)));
    assert!(!kb.tick(520, &mut |e| ev.push(e)));
    assert!(kb.tick(533, &mut |e| ev.push(e)));
    assert_eq!(ev.iter().filter(|e| e.action == Action::Repeat).count(), 2);
    // A newer key takes over the repeat.
    kb.feed(&r(0, &[0x04, 0x05]), 600, &mut |e| ev.push(e));
    assert!(!kb.tick(1000, &mut |e| ev.push(e)));
    assert!(kb.tick(1100, &mut |e| ev.push(e)));
    assert_eq!(ev.last().unwrap().usage, 0x05);
    // A late tick yields one repeat, not a burst.
    ev.clear();
    let mut n = 0;
    while kb.tick(100_000, &mut |e| ev.push(e)) {
        n += 1;
        assert!(n < 2, "burst");
    }
    assert_eq!(n, 1);
    assert!(kb.tick(100_033, &mut |e| ev.push(e)));
    // Releasing the repeating key stops the repeat even if another is held.
    kb.feed(&r(0, &[0x04]), 100_040, &mut |e| ev.push(e));
    assert!(!kb.tick(200_000, &mut |e| ev.push(e)));
    // Lock keys and modifiers never repeat.
    kb.feed(&r(SHIFT, &[0x39]), 200_001, &mut |e| ev.push(e));
    assert!(!kb.tick(300_000, &mut |e| ev.push(e)));
    // Repeat disabled.
    let mut kb = Keyboard::new(Config {
        repeat_interval_ms: 0,
        ..Config::default()
    });
    kb.feed(&r(0, &[0x04]), 0, &mut |_| {});
    assert!(!kb.tick(10_000, &mut |_| {}));
}

#[test]
fn randomized_reports_keep_presses_and_releases_balanced() {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut kb = Keyboard::new(Config::default());
    let mut held: HashMap<u8, i32> = HashMap::new();
    let count = |e: KeyEvent, held: &mut HashMap<u8, i32>| match e.action {
        Action::Press => {
            let v = held.entry(e.usage).or_default();
            *v += 1;
            assert_eq!(*v, 1, "usage {:#x} pressed twice", e.usage);
        }
        Action::Release => {
            let v = held.entry(e.usage).or_default();
            *v -= 1;
            assert_eq!(*v, 0, "usage {:#x} released while up", e.usage);
        }
        Action::Repeat => assert_eq!(held.get(&e.usage), Some(&1), "repeat of a key that is up"),
    };
    for t in 0..20_000u64 {
        let mut b = [0u8; 8];
        let v = next();
        b[0] = v as u8;
        for (i, k) in b[2..].iter_mut().enumerate() {
            let w = (v >> (8 + 8 * i)) as u8;
            *k = if w < 64 { 0 } else { w };
        }
        if v % 97 == 0 {
            b[2..].fill(1);
        }
        let rep = BootReport::parse(&b).unwrap();
        let mut ev = Vec::new();
        kb.feed(&rep, t * 10, &mut |e| ev.push(e));
        kb.tick(t * 10 + 5, &mut |e| ev.push(e));
        for e in ev {
            count(e, &mut held);
        }
    }
    let mut ev = Vec::new();
    kb.feed(&BootReport::RELEASED, 1 << 40, &mut |e| ev.push(e));
    for e in ev {
        count(e, &mut held);
    }
    assert!(held.values().all(|&v| v == 0), "keys left down: {held:?}");
}
