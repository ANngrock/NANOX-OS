//! Properties of the qga crate (docs/specs/M11-SERVER.md).

use qga::frame::{Channel, Feed};

use crate::{Proof, Property};

pub fn properties() -> Vec<Property> {
    vec![Property {
        id: "qga.frame.matches-reference-model",
        version: 1,
        statement: "The agent channel framer reports exactly the events of a reference \
                    model that recomputes bracket depth and string state from scratch \
                    after every byte: message boundaries, oversized messages, stray \
                    characters once per run, and reset on 0xFF.",
        bound: "buffer of 4 bytes; every byte sequence of length 1..=6 over 16 symbols \
                ({ } [ ] \" \\ a x 1 , : space tab CR LF 0xFF)",
        component: &["crates/qga/src/frame.rs"],
        checker: "crates/proofs/src/props/qga.rs",
        run: frame_model,
    }]
}

const N: usize = 4;
const ALPHABET: [u8; 16] = [
    b'{', b'}', b'[', b']', b'"', b'\\', b'a', b'x', b'1', b',', b':', b' ', b'\t', b'\r', b'\n',
    0xFF,
];

#[derive(Debug, PartialEq, Eq)]
enum Event {
    Need,
    Message(Vec<u8>),
    TooLarge,
    Stray(u8),
}

/// The specification, written to be obviously right rather than fast: it
/// keeps the whole message and scans it from the start after every byte.
#[derive(Default)]
struct Model {
    active: bool,
    msg: Vec<u8>,
    stray_reported: bool,
}

impl Model {
    fn reset(&mut self) {
        *self = Model::default();
    }

    /// Depth of brackets outside strings after `msg`.
    fn depth(&self) -> usize {
        let (mut depth, mut in_string, mut escape) = (0usize, false, false);
        for &b in &self.msg {
            if in_string {
                if escape {
                    escape = false;
                } else if b == b'\\' {
                    escape = true;
                } else if b == b'"' {
                    in_string = false;
                }
            } else if b == b'"' {
                in_string = true;
            } else if b == b'{' || b == b'[' {
                depth += 1;
            } else if b == b'}' || b == b']' {
                depth -= 1;
            }
        }
        depth
    }

    fn feed(&mut self, b: u8) -> Event {
        if b == 0xFF {
            self.reset();
            return Event::Need;
        }
        if !self.active {
            return match b {
                b'{' | b'[' => {
                    self.reset();
                    self.active = true;
                    self.msg.push(b);
                    Event::Need
                }
                b' ' | b'\t' | b'\r' => Event::Need,
                b'\n' => {
                    self.stray_reported = false;
                    Event::Need
                }
                _ if self.stray_reported => Event::Need,
                _ => {
                    self.stray_reported = true;
                    Event::Stray(b)
                }
            };
        }
        self.msg.push(b);
        if self.depth() != 0 {
            return Event::Need;
        }
        if self.msg.len() > N {
            self.reset();
            Event::TooLarge
        } else {
            Event::Message(self.msg.clone())
        }
    }
}

fn frame_model() -> Proof {
    let mut cases = 0;
    for len in 1..=6usize {
        let total = ALPHABET.len().pow(len as u32);
        for code in 0..total {
            let mut seq = [0u8; 6];
            let mut c = code;
            for s in seq.iter_mut().take(len) {
                *s = ALPHABET[c % ALPHABET.len()];
                c /= ALPHABET.len();
            }
            cases += 1;
            let mut real = Channel::<N>::new();
            let mut model = Model::default();
            for (i, &b) in seq[..len].iter().enumerate() {
                let want = model.feed(b);
                let got = match real.feed(b) {
                    Feed::Need => Event::Need,
                    Feed::Message => {
                        let m = real.message().to_vec();
                        real.reset();
                        model.reset();
                        Event::Message(m)
                    }
                    Feed::TooLarge => Event::TooLarge,
                    Feed::Stray(s) => Event::Stray(s),
                };
                let want = match want {
                    Event::Message(m) => {
                        model.reset();
                        Event::Message(m)
                    }
                    other => other,
                };
                if got != want {
                    return Proof::failed(
                        cases,
                        format!("after {:?}: real {got:?}, model {want:?}", &seq[..=i]),
                    );
                }
            }
        }
    }
    Proof::held(cases)
}
