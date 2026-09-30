//! Properties of the virtual switch (docs/specs/M11-SERVER.md).

use vswitch::{Config, Decision, Port, Reason, Switch, MAX_PORTS};

use crate::{Proof, Property};

pub fn properties() -> Vec<Property> {
    vec![Property {
        id: "vswitch.forwarding-matches-reference-model",
        version: 1,
        statement: "For every port configuration and every short frame sequence, the \
                    switch makes the same decision, or drops for the same reason, as a \
                    reference model written from the specification: VLAN classification \
                    on access and trunk ports, learning and flap damping, known-unicast \
                    and flooding, isolation, port state, tagging on egress, never back \
                    out of the ingress port.",
        bound: "3 ports, each one of {access 10, access 20, trunk native 10, trunk \
                without native} x isolated x up (4096 configurations), trunks carrying \
                VLANs 10 and 20; every single frame over 5 ingress ports x 5 destinations \
                x 3 sources x 5 tags, plus runt, cut-short-tag and oversized frames; \
                every pair of frames over 3 ingress ports x 3 destinations x 2 sources \
                x 3 tags; one clock tick (no aging)",
        component: &["crates/vswitch/src/lib.rs"],
        checker: "crates/proofs/src/props/vswitch.rs",
        run: forwarding,
    }]
}

const P: usize = 3;
const MAC_A: [u8; 6] = [2, 0, 0, 0, 0, 0x0A];
const MAC_B: [u8; 6] = [2, 0, 0, 0, 0, 0x0B];
const BCAST: [u8; 6] = [0xFF; 6];
const MCAST: [u8; 6] = [1, 0, 0x5E, 0, 0, 1];
const RESERVED: [u8; 6] = [1, 0x80, 0xC2, 0, 0, 0];
const BADSRC: [u8; 6] = [1, 0, 0x5E, 0, 0, 2];

#[derive(Clone, Copy)]
struct Pc {
    /// 0 access 10, 1 access 20, 2 trunk native 10, 3 trunk without native.
    kind: u8,
    isolated: bool,
    up: bool,
}

fn port_of(c: Pc) -> Port {
    let mut p = match c.kind {
        0 => Port::access(10),
        1 => Port::access(20),
        2 => Port::trunk(Some(10)),
        _ => Port::trunk(None),
    };
    p.isolated = c.isolated;
    p.up = c.up;
    p
}

fn build(cfg: &[Pc; P]) -> Switch<16> {
    let mut sw = Switch::<16>::new(Config::default());
    for (p, c) in cfg.iter().enumerate() {
        sw.set_port(p, port_of(*c)).unwrap();
        if c.kind >= 2 {
            sw.allow_vlan(p, 10, true).unwrap();
            sw.allow_vlan(p, 20, true).unwrap();
        }
    }
    sw
}

fn frame(dst: [u8; 6], src: [u8; 6], tag: Option<u16>, len: usize) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&dst);
    f.extend_from_slice(&src);
    if let Some(v) = tag {
        f.extend_from_slice(&[0x81, 0x00, (v >> 8) as u8, v as u8]);
    }
    f.extend_from_slice(&[0x08, 0x00]);
    f.resize(len.max(if tag.is_some() { 18 } else { 14 }), 0);
    if len < f.len() {
        f.truncate(len);
    }
    f
}

/// The specification, as a model: the table is a list, time never passes.
struct Model {
    cfg: [Pc; P],
    table: Vec<(u16, [u8; 6], usize)>,
}

impl Model {
    fn carries(&self, p: usize, vid: u16) -> bool {
        match self.cfg[p].kind {
            0 => vid == 10,
            1 => vid == 20,
            _ => vid == 10 || vid == 20,
        }
    }

    fn receive(&mut self, in_port: usize, f: &[u8]) -> Result<Decision, Reason> {
        if in_port >= MAX_PORTS || in_port >= P || !self.cfg[in_port].up {
            return Err(Reason::PortDown);
        }
        let c = self.cfg[in_port];
        if f.len() < 14 {
            return Err(Reason::Runt);
        }
        if f.len() > 1522 {
            return Err(Reason::Oversized);
        }
        let (dst, src) = (
            <[u8; 6]>::try_from(&f[..6]).unwrap(),
            <[u8; 6]>::try_from(&f[6..12]).unwrap(),
        );
        if src[0] & 1 != 0 {
            return Err(Reason::BadSource);
        }
        if dst[..5] == [1, 0x80, 0xC2, 0, 0] && dst[5] < 0x10 {
            return Err(Reason::Reserved);
        }
        let tagged = f[12] == 0x81 && f[13] == 0x00;
        let mut tag_vid = 0;
        if tagged {
            if f.len() < 18 {
                return Err(Reason::Runt);
            }
            tag_vid = (u16::from(f[14]) << 8 | u16::from(f[15])) & 0x0FFF;
        }
        let vid = if c.kind < 2 {
            if tagged && tag_vid != 0 {
                return Err(Reason::TaggedOnAccess);
            }
            if c.kind == 0 {
                10
            } else {
                20
            }
        } else {
            let v = if tagged && tag_vid != 0 {
                tag_vid
            } else if c.kind == 2 {
                10
            } else {
                return Err(Reason::UntaggedOnTrunk);
            };
            if v != 10 && v != 20 {
                return Err(Reason::VlanNotAllowed);
            }
            v
        };
        match self.table.iter().find(|e| e.0 == vid && e.1 == src) {
            Some(e) if e.2 == in_port => {}
            Some(_) => return Err(Reason::MacFlap),
            None => self.table.push((vid, src, in_port)),
        }
        let known = if dst[0] & 1 != 0 {
            None
        } else {
            self.table
                .iter()
                .find(|e| e.0 == vid && e.1 == dst)
                .map(|e| e.2)
        };
        let (mut out, mut tagged_mask) = (0u16, 0u16);
        for p in 0..P {
            if known.is_some_and(|k| k != p) || p == in_port {
                continue;
            }
            let q = self.cfg[p];
            if !q.up || (q.isolated && c.isolated) || !self.carries(p, vid) {
                continue;
            }
            out |= 1 << p;
            if q.kind >= 2 && !(q.kind == 2 && vid == 10) {
                tagged_mask |= 1 << p;
            }
        }
        if out == 0 {
            return Err(Reason::NoEgress);
        }
        Ok(Decision {
            vid,
            ports: out,
            tagged: tagged_mask,
        })
    }
}

fn configs() -> Vec<[Pc; P]> {
    let one: Vec<Pc> = (0..4u8)
        .flat_map(|kind| {
            [false, true].into_iter().flat_map(move |isolated| {
                [false, true]
                    .into_iter()
                    .map(move |up| Pc { kind, isolated, up })
            })
        })
        .collect();
    let mut v = Vec::new();
    for a in &one {
        for b in &one {
            for c in &one {
                v.push([*a, *b, *c]);
            }
        }
    }
    v
}

/// Runs `frames` through a fresh switch and a fresh model; the first
/// difference is returned.
fn compare(cfg: &[Pc; P], frames: &[(usize, Vec<u8>)]) -> Result<(), String> {
    let mut sw = build(cfg);
    let mut model = Model {
        cfg: *cfg,
        table: Vec::new(),
    };
    for (i, (port, f)) in frames.iter().enumerate() {
        let (real, want) = (sw.receive(0, *port, f), model.receive(*port, f));
        if real != want {
            return Err(format!(
                "frame {i} on port {port}: switch {real:?}, model {want:?}"
            ));
        }
    }
    if sw.learned() != model.table.len() {
        return Err(format!(
            "learned {} entries, model {}",
            sw.learned(),
            model.table.len()
        ));
    }
    sw.check().map_err(str::to_string)
}

fn forwarding() -> Proof {
    let mut cases = 0;
    let fail = |cases, cfg: &[Pc; P], frames: &[(usize, Vec<u8>)], why: String| {
        let desc: Vec<String> = cfg
            .iter()
            .map(|c| format!("kind {} isolated {} up {}", c.kind, c.isolated, c.up))
            .collect();
        Proof::failed(
            cases,
            format!("ports [{}], frames {frames:02x?}: {why}", desc.join("; ")),
        )
    };
    let tags = [None, Some(0), Some(10), Some(20), Some(30)];
    let all_configs = configs();
    // Single frames, including malformed ones and ports that do not exist.
    let mut singles: Vec<(usize, Vec<u8>)> = Vec::new();
    for port in [0usize, 1, 2, 3, 16] {
        for dst in [BCAST, MCAST, MAC_A, MAC_B, RESERVED] {
            for src in [MAC_A, MAC_B, BADSRC] {
                for tag in tags {
                    singles.push((port, frame(dst, src, tag, 64)));
                }
            }
        }
        for tag in tags {
            for len in [13, 16, 2000] {
                singles.push((port, frame(MAC_B, MAC_A, tag, len)));
            }
        }
    }
    for cfg in &all_configs {
        for f in &singles {
            cases += 1;
            if let Err(why) = compare(cfg, std::slice::from_ref(f)) {
                return fail(cases, cfg, std::slice::from_ref(f), why);
            }
        }
    }
    // Pairs: the first teaches the switch something, the second uses it.
    let mut pairs: Vec<(usize, Vec<u8>)> = Vec::new();
    for port in 0..P {
        for dst in [BCAST, MAC_A, MAC_B] {
            for src in [MAC_A, MAC_B] {
                for tag in [None, Some(10), Some(20)] {
                    pairs.push((port, frame(dst, src, tag, 64)));
                }
            }
        }
    }
    for cfg in &all_configs {
        for first in &pairs {
            for second in &pairs {
                cases += 1;
                let seq = [first.clone(), second.clone()];
                if let Err(why) = compare(cfg, &seq) {
                    return fail(cases, cfg, &seq, why);
                }
            }
        }
    }
    Proof::held(cases)
}
