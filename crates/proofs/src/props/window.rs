//! Properties of the server window (docs/specs/M11-WINDOW.md).

use serverwin::verify;

use crate::{Proof, Property};

pub fn properties() -> Vec<Property> {
    vec![Property {
        id: "serverwin.window-rules",
        version: 1,
        statement: "On every event sequence the window keeps its invariants (input is \
                    grabbed only with a shown window and a running guest, modifiers are \
                    held only with a grab, server mode is wanted only with a running \
                    guest on the whole screen, the display rate follows the presentation, \
                    the window stays reachable) and obeys the transition rules (the guest \
                    is started only when it is down on a non-desktop machine, full server \
                    is requested only for a running guest, a key reaches the guest only \
                    with a grab). The release chord, pressed from any reachable state with \
                    the keyboard grabbed, hands control back: no grab, no full screen, no \
                    wish for server mode, no held modifiers.",
        bound: "every sequence of 5 events over a 33-event alphabet from a fresh hybrid \
                machine, invariants and rules checked after every step; the release chord \
                from every state reached by 3 events on a desktop or a hybrid machine",
        component: &[
            "crates/serverwin/src/lib.rs",
            "crates/serverwin/src/verify.rs",
        ],
        checker: "crates/proofs/src/props/window.rs",
        run: window_rules,
    }]
}

fn window_rules() -> Proof {
    let mut cases = 0;
    match verify::run(5) {
        Ok(n) => cases += n,
        Err(f) => return Proof::failed(cases, format!("{} after {:?}", f.why, f.events())),
    }
    match verify::chord_from_every_state() {
        Ok(n) => cases += n,
        Err(f) => return Proof::failed(cases, format!("{} after {:?}", f.why, f.events())),
    }
    Proof::held(cases)
}
