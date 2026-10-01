//! Every event sequence of a bounded length keeps the invariants, and the
//! release chord hands the keyboard back from every reachable state.

use serverwin::verify::{self, ALPHABET};
use serverwin::{Event, Key, KeyRoute};

#[test]
fn every_sequence_of_four_events_keeps_the_invariants() {
    match verify::run(4) {
        Ok(n) => assert_eq!(n, (ALPHABET.len() as u64).pow(4)),
        Err(f) => panic!("{} after {:?}", f.why, f.events()),
    }
}

#[test]
fn the_chord_works_from_every_reachable_state_in_three_steps() {
    match verify::chord_from_every_state() {
        Ok(n) => assert!(n > 5_000, "{n}"),
        Err(f) => panic!("{} after {:?}", f.why, f.events()),
    }
}

#[test]
fn an_ordinary_key_without_a_grab_goes_to_the_os() {
    let mut w = verify::fresh();
    let prev = w;
    let ev = Event::Key {
        key: Key::Other,
        down: true,
    };
    let step = w.step(ev);
    assert_eq!(step.route, Some(KeyRoute::Os));
    assert!(verify::check(&prev, ev, &w, &step).is_ok());
}
