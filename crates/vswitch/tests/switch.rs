use std::collections::HashMap;
use vswitch::{emit, Config, ConfigError, Decision, Port, PortKind, Reason, Switch};

fn mac(n: u8) -> [u8; 6] {
    [0x02, 0, 0, 0, 0, n]
}

const BCAST: [u8; 6] = [0xFF; 6];

fn frame(dst: [u8; 6], src: [u8; 6], vid: Option<u16>, payload: usize) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&dst);
    f.extend_from_slice(&src);
    if let Some(v) = vid {
        f.extend_from_slice(&[0x81, 0x00, 0x60 | (v >> 8) as u8, v as u8]);
    }
    f.extend_from_slice(&[0x08, 0x00]);
    f.extend((0..payload).map(|i| i as u8));
    f
}

fn ports(d: &Decision) -> Vec<usize> {
    (0..16).filter(|&p| d.to(p)).collect()
}

/// Four access ports on VLAN 1 and an uplink trunk.
fn small() -> Switch<64> {
    let mut s: Switch<64> = Switch::new(Config::default());
    for p in 0..4 {
        s.set_port(p, Port::access(1)).unwrap();
    }
    s.set_port(4, Port::trunk(Some(1))).unwrap();
    s
}

#[test]
fn learns_floods_then_forwards_to_one_port() {
    let mut s = small();
    let (a, b, c) = (mac(1), mac(2), mac(3));
    // Unknown destination: flooded to every other port.
    let d = s.receive(0, 0, &frame(b, a, None, 46)).unwrap();
    assert_eq!(ports(&d), [1, 2, 3, 4]);
    // B answers from port 1: A is now known, one port only.
    let d = s.receive(10, 1, &frame(a, b, None, 46)).unwrap();
    assert_eq!(ports(&d), [0]);
    // And now B is known too.
    let d = s.receive(20, 0, &frame(b, a, None, 46)).unwrap();
    assert_eq!(ports(&d), [1]);
    // C has not spoken: still flooded.
    let d = s.receive(30, 0, &frame(c, a, None, 46)).unwrap();
    assert_eq!(ports(&d), [1, 2, 3, 4]);
    assert_eq!(s.lookup(30, 1, a), Some(0));
    assert_eq!(s.lookup(30, 1, b), Some(1));
    assert_eq!(s.lookup(30, 1, c), None);
    assert_eq!(s.learned(), 2);
    s.check().unwrap();
}

#[test]
fn broadcast_and_multicast_flood_but_never_back_and_never_to_down_ports() {
    let mut s = small();
    s.set_port(
        2,
        Port {
            up: false,
            ..Port::access(1)
        },
    )
    .unwrap();
    let d = s.receive(0, 1, &frame(BCAST, mac(9), None, 46)).unwrap();
    assert_eq!(ports(&d), [0, 3, 4]);
    let d = s
        .receive(1, 1, &frame([0x01, 0, 0x5E, 1, 2, 3], mac(9), None, 46))
        .unwrap();
    assert_eq!(ports(&d), [0, 3, 4], "multicast floods too");
    // A frame for a host behind the ingress port itself goes nowhere.
    s.receive(2, 1, &frame(BCAST, mac(5), None, 46)).unwrap();
    assert_eq!(
        s.receive(3, 1, &frame(mac(5), mac(6), None, 46)),
        Err(Reason::NoEgress)
    );
    // A frame arriving on a down port is dropped.
    assert_eq!(
        s.receive(4, 2, &frame(BCAST, mac(7), None, 46)),
        Err(Reason::PortDown)
    );
}

#[test]
fn malformed_and_reserved_frames_are_dropped_with_a_reason() {
    let mut s = small();
    let ok = frame(BCAST, mac(1), None, 46);
    assert_eq!(s.receive(0, 0, &ok[..13]), Err(Reason::Runt));
    assert_eq!(s.receive(0, 0, &[]), Err(Reason::Runt));
    assert_eq!(
        s.receive(0, 0, &frame(BCAST, mac(1), None, 2000)),
        Err(Reason::Oversized)
    );
    assert_eq!(
        s.receive(0, 0, &frame(mac(2), BCAST, None, 46)),
        Err(Reason::BadSource)
    );
    assert_eq!(
        s.receive(0, 0, &frame(mac(2), [0x01, 0, 0x5E, 0, 0, 1], None, 46)),
        Err(Reason::BadSource)
    );
    // STP, LLDP, pause: never forwarded.
    for last in [0x00, 0x01, 0x02, 0x0E, 0x0F] {
        let d = [0x01, 0x80, 0xC2, 0x00, 0x00, last];
        assert_eq!(
            s.receive(0, 0, &frame(d, mac(1), None, 46)),
            Err(Reason::Reserved),
            "{last:#x}"
        );
    }
    // 01-80-C2-00-00-10 is not in the reserved block.
    assert!(s
        .receive(
            0,
            0,
            &frame([0x01, 0x80, 0xC2, 0, 0, 0x10], mac(1), None, 46)
        )
        .is_ok());
    // A tag cut short.
    let mut cut = frame(BCAST, mac(1), Some(1), 0);
    cut.truncate(16);
    assert_eq!(s.receive(0, 4, &cut), Err(Reason::Runt));
    assert_eq!(s.stats(0).drops[Reason::Runt as usize], 2);
    assert_eq!(s.stats(0).rx_frames, 11);
}

#[test]
fn vlans_isolate_access_ports_and_trunks_carry_tags() {
    let mut s: Switch<64> = Switch::new(Config::default());
    s.set_port(0, Port::access(10)).unwrap();
    s.set_port(1, Port::access(10)).unwrap();
    s.set_port(2, Port::access(20)).unwrap();
    s.set_port(3, Port::trunk(Some(10))).unwrap();
    s.allow_vlan(3, 20, true).unwrap();
    s.allow_vlan(3, 30, true).unwrap();
    // VLAN 10 broadcast: the other access port in 10, the trunk untagged
    // (native), never VLAN 20.
    let d = s.receive(0, 0, &frame(BCAST, mac(1), None, 46)).unwrap();
    assert_eq!((d.vid, ports(&d), d.tagged), (10, vec![1, 3], 0));
    // VLAN 20: the trunk tags it.
    let d = s.receive(0, 2, &frame(BCAST, mac(2), None, 46)).unwrap();
    assert_eq!((d.vid, ports(&d), d.tagged), (20, vec![3], 1 << 3));
    // Tagged frame from the trunk for 20 reaches only the access port in 20.
    let d = s
        .receive(0, 3, &frame(BCAST, mac(3), Some(20), 46))
        .unwrap();
    assert_eq!((d.vid, ports(&d), d.tagged), (20, vec![2], 0));
    // Untagged on the trunk is native VLAN 10.
    let d = s.receive(0, 3, &frame(BCAST, mac(4), None, 46)).unwrap();
    assert_eq!((d.vid, ports(&d)), (10, vec![0, 1]));
    // Forbidden VLAN, tag on an access port, no native VLAN.
    assert_eq!(
        s.receive(0, 3, &frame(BCAST, mac(5), Some(99), 46)),
        Err(Reason::VlanNotAllowed)
    );
    assert_eq!(
        s.receive(0, 0, &frame(BCAST, mac(6), Some(10), 46)),
        Err(Reason::TaggedOnAccess)
    );
    // Priority tag (VID 0) is accepted on an access port.
    assert!(s.receive(0, 0, &frame(BCAST, mac(7), Some(0), 46)).is_ok());
    s.set_port(3, Port::trunk(None)).unwrap();
    s.allow_vlan(3, 10, true).unwrap();
    s.allow_vlan(3, 20, true).unwrap();
    assert_eq!(
        s.receive(0, 3, &frame(BCAST, mac(8), None, 46)),
        Err(Reason::UntaggedOnTrunk)
    );
    // The same MAC in two VLANs is two stations.
    s.receive(0, 3, &frame(BCAST, mac(9), Some(10), 46))
        .unwrap();
    s.receive(0, 2, &frame(BCAST, mac(9), None, 46)).unwrap();
    assert_eq!(s.lookup(0, 10, mac(9)), Some(3));
    assert_eq!(s.lookup(0, 20, mac(9)), Some(2));
    // Bad VLAN ids and ports are refused.
    assert_eq!(s.set_port(0, Port::access(0)), Err(ConfigError::Vlan));
    assert_eq!(s.set_port(0, Port::access(4095)), Err(ConfigError::Vlan));
    assert_eq!(
        s.set_port(16, Port::access(1)),
        Err(ConfigError::NoSuchPort)
    );
    assert_eq!(s.allow_vlan(3, 4095, true), Err(ConfigError::Vlan));
}

#[test]
fn emit_adds_removes_and_keeps_the_priority_bits() {
    let untagged = frame(mac(1), mac(2), None, 30);
    let tagged = frame(mac(1), mac(2), Some(0x123), 30);
    let mut out = [0u8; 128];
    // Untagged -> tagged.
    let n = emit(&untagged, Some(0x123), &mut out).unwrap();
    let mut want = tagged.clone();
    want[14] &= 0x0F; // a new tag has no priority
    assert_eq!(&out[..n], &want[..]);
    // Tagged -> untagged.
    let n = emit(&tagged, None, &mut out).unwrap();
    assert_eq!(&out[..n], &untagged[..]);
    // Tagged -> re-tagged keeps priority and DEI, changes the VID.
    let n = emit(&tagged, Some(0x456), &mut out).unwrap();
    assert_eq!(&out[..n], &frame(mac(1), mac(2), Some(0x456), 30)[..]);
    assert_eq!(out[14] & 0xF0, 0x60);
    // Unchanged when the tag state already matches.
    let n = emit(&untagged, None, &mut out).unwrap();
    assert_eq!(&out[..n], &untagged[..]);
    // Too small an output buffer, and malformed input.
    assert_eq!(emit(&untagged, Some(1), &mut out[..40]), None);
    assert_eq!(emit(&untagged[..10], None, &mut out), None);
    assert_eq!(emit(&tagged[..16], None, &mut out), None);
}

#[test]
fn entries_age_out_and_traffic_refreshes_them() {
    let mut s = small();
    s.receive(0, 0, &frame(BCAST, mac(1), None, 46)).unwrap();
    assert_eq!(s.lookup(299_999, 1, mac(1)), Some(0));
    assert_eq!(s.lookup(300_000, 1, mac(1)), None, "aged out");
    // Traffic from the host keeps it alive.
    s.receive(200_000, 0, &frame(BCAST, mac(1), None, 46))
        .unwrap();
    assert_eq!(s.lookup(400_000, 1, mac(1)), Some(0));
    // An aged entry may move at once (no flap damping for a stale one).
    s.receive(600_000, 2, &frame(BCAST, mac(1), None, 46))
        .unwrap();
    assert_eq!(s.lookup(600_000, 1, mac(1)), Some(2));
    s.check().unwrap();
}

#[test]
fn a_mac_that_moves_too_fast_is_damped() {
    let mut s = small();
    s.receive(0, 0, &frame(BCAST, mac(1), None, 46)).unwrap();
    // Seen on another port within the hold time: dropped, entry unchanged.
    assert_eq!(
        s.receive(500, 1, &frame(BCAST, mac(1), None, 46)),
        Err(Reason::MacFlap)
    );
    assert_eq!(s.lookup(500, 1, mac(1)), Some(0));
    // After the hold time a real move is accepted.
    s.receive(1_000, 1, &frame(BCAST, mac(1), None, 46))
        .unwrap();
    assert_eq!(s.lookup(1_000, 1, mac(1)), Some(1));
    // The move restarts the hold.
    assert_eq!(
        s.receive(1_500, 0, &frame(BCAST, mac(1), None, 46)),
        Err(Reason::MacFlap)
    );
    assert_eq!(s.stats(1).drops[Reason::MacFlap as usize], 1);
    assert_eq!(s.stats(0).drops[Reason::MacFlap as usize], 1);
    s.check().unwrap();
}

#[test]
fn port_security_limits_learned_addresses_per_port() {
    let mut s = small();
    s.set_port(
        0,
        Port {
            max_macs: 3,
            ..Port::access(1)
        },
    )
    .unwrap();
    for n in 1..=3 {
        s.receive(0, 0, &frame(BCAST, mac(n), None, 46)).unwrap();
    }
    // A fourth address on that port is dropped and not learned.
    assert_eq!(
        s.receive(0, 0, &frame(BCAST, mac(4), None, 46)),
        Err(Reason::PortSecurity)
    );
    assert_eq!(s.lookup(0, 1, mac(4)), None);
    // Known ones keep working; other ports are unaffected.
    s.receive(1, 0, &frame(BCAST, mac(2), None, 46)).unwrap();
    for n in 10..40 {
        s.receive(0, 1, &frame(BCAST, mac(n), None, 46)).unwrap();
    }
    // A move onto the limited port does not bypass the limit.
    assert_eq!(
        s.receive(5_000, 0, &frame(BCAST, mac(10), None, 46)),
        Err(Reason::PortSecurity)
    );
    // Reconfiguring the port forgets its addresses.
    s.set_port(
        0,
        Port {
            max_macs: 3,
            ..Port::access(1)
        },
    )
    .unwrap();
    assert_eq!(s.lookup(0, 1, mac(1)), None);
    s.receive(6_000, 0, &frame(BCAST, mac(4), None, 46))
        .unwrap();
    s.check().unwrap();
}

#[test]
fn isolated_ports_only_reach_the_uplink() {
    let mut s = small();
    for p in 0..3 {
        s.set_port(
            p,
            Port {
                isolated: true,
                ..Port::access(1)
            },
        )
        .unwrap();
    }
    // Port 3 (not isolated) and the trunk are promiscuous.
    let d = s.receive(0, 0, &frame(BCAST, mac(1), None, 46)).unwrap();
    assert_eq!(ports(&d), [3, 4]);
    // A known isolated peer is still unreachable.
    s.receive(1, 1, &frame(BCAST, mac(2), None, 46)).unwrap();
    assert_eq!(
        s.receive(2, 0, &frame(mac(2), mac(1), None, 46)),
        Err(Reason::NoEgress)
    );
    // The uplink reaches them.
    s.receive(3, 4, &frame(BCAST, mac(3), None, 46)).unwrap();
    let d = s.receive(4, 4, &frame(mac(2), mac(3), None, 46)).unwrap();
    assert_eq!(ports(&d), [1]);
}

#[test]
fn a_mac_flood_never_grows_the_table_and_known_hosts_relearn() {
    let mut s: Switch<64> = Switch::new(Config::default());
    s.set_port(0, Port::access(1)).unwrap();
    s.set_port(1, Port::access(1)).unwrap();
    s.set_port(2, Port::trunk(Some(1))).unwrap();
    for i in 0..30_000u32 {
        let m = [0x02, 0, (i >> 16) as u8, (i >> 8) as u8, i as u8, 7];
        s.receive(u64::from(i), 2, &frame(BCAST, m, None, 46))
            .unwrap();
        if i % 1000 == 0 {
            s.check().unwrap();
        }
    }
    assert!(s.learned() <= 64);
    s.check().unwrap();
    // A legitimate host is flooded to at worst, and learned again on speaking.
    let d = s
        .receive(40_000, 0, &frame(mac(50), mac(60), None, 46))
        .unwrap();
    assert_eq!(ports(&d), [1, 2]);
    s.receive(40_001, 1, &frame(mac(60), mac(50), None, 46))
        .unwrap();
    let d = s
        .receive(40_002, 0, &frame(mac(50), mac(60), None, 46))
        .unwrap();
    assert_eq!(ports(&d), [1]);
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// A plain reference: a hash map, the same VLAN rules, no aging, no limits.
struct Model {
    table: HashMap<(u16, [u8; 6]), usize>,
}

const KINDS: [PortKind; 6] = [
    PortKind::Access(1),
    PortKind::Access(1),
    PortKind::Access(2),
    PortKind::Access(2),
    PortKind::Trunk { native: Some(1) },
    PortKind::Trunk { native: None },
];

fn model_vlans(p: usize) -> Vec<u16> {
    match p {
        0 | 1 => vec![1],
        2 | 3 => vec![2],
        4 => vec![1, 2, 3],
        _ => vec![2, 3],
    }
}

impl Model {
    fn receive(&mut self, in_port: usize, f: &[u8]) -> Option<(u16, Vec<usize>)> {
        let tagged = f[12] == 0x81 && f[13] == 0x00;
        let tag = if tagged {
            u16::from_be_bytes([f[14], f[15]]) & 0xFFF
        } else {
            0
        };
        let vid = match KINDS[in_port] {
            PortKind::Access(v) => {
                if tagged && tag != 0 {
                    return None;
                }
                v
            }
            PortKind::Trunk { native } => {
                let v = if tagged && tag != 0 { tag } else { native? };
                if !model_vlans(in_port).contains(&v) {
                    return None;
                }
                v
            }
        };
        let (dst, src): ([u8; 6], [u8; 6]) =
            (f[..6].try_into().unwrap(), f[6..12].try_into().unwrap());
        self.table.insert((vid, src), in_port);
        let known = if dst[0] & 1 == 0 {
            self.table.get(&(vid, dst)).copied()
        } else {
            None
        };
        let out: Vec<usize> = (0..6)
            .filter(|&p| p != in_port && model_vlans(p).contains(&vid))
            .filter(|&p| known.is_none_or(|k| k == p))
            .collect();
        (!out.is_empty()).then_some((vid, out))
    }
}

#[test]
fn matches_a_simple_reference_model_on_random_traffic() {
    let cfg = Config {
        age_ms: u64::MAX,
        flap_hold_ms: 0,
        max_frame: 1522,
    };
    let mut sw: Switch<4096> = Switch::new(cfg);
    for (p, k) in KINDS.iter().enumerate() {
        sw.set_port(
            p,
            Port {
                kind: *k,
                up: true,
                isolated: false,
                max_macs: 0,
            },
        )
        .unwrap();
    }
    sw.allow_vlan(4, 2, true).unwrap();
    sw.allow_vlan(4, 3, true).unwrap();
    sw.allow_vlan(5, 2, true).unwrap();
    sw.allow_vlan(5, 3, true).unwrap();
    let mut model = Model {
        table: HashMap::new(),
    };
    let mut rng = Rng(0xFEED_FACE_CAFE_BEEF);
    let (mut accepted, mut dropped) = (0, 0);
    for t in 0..200_000u64 {
        let in_port = rng.below(6) as usize;
        let src = mac(1 + rng.below(40) as u8);
        let dst = match rng.below(10) {
            0 => BCAST,
            1 => [0x01, 0, 0x5E, 0, 0, 1],
            _ => mac(1 + rng.below(40) as u8),
        };
        let vid = match rng.below(4) {
            0 => None,
            _ => Some(1 + rng.below(3) as u16),
        };
        let f = frame(dst, src, vid, rng.below(60) as usize);
        let want = model.receive(in_port, &f);
        // The switch may drop frames the model has no rule for (down
        // ports, reserved groups, multicast sources); none occur here.
        match (sw.receive(t, in_port, &f), want) {
            (Ok(d), Some((wv, wp))) => {
                assert_eq!((d.vid, ports(&d)), (wv, wp), "frame {t}");
                accepted += 1;
            }
            (Err(_), None) => dropped += 1,
            (got, want) => panic!("frame {t} on port {in_port}: switch {got:?}, model {want:?}"),
        }
        if t % 20_000 == 0 {
            sw.check().unwrap();
        }
    }
    // Many random frames carry a tag the port refuses; both sides matter.
    assert!(
        accepted > 40_000 && dropped > 10_000,
        "{accepted} accepted, {dropped} dropped"
    );
}

#[test]
fn every_decision_respects_the_invariants_under_random_configs() {
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    for round in 0..300 {
        let mut sw: Switch<128> = Switch::new(Config::default());
        let mut kinds = Vec::new();
        let mut allow: Vec<Vec<u16>> = Vec::new();
        for p in 0..8 {
            let k = if rng.below(3) == 0 {
                PortKind::Trunk {
                    native: (rng.below(2) == 0).then(|| 1 + rng.below(4) as u16),
                }
            } else {
                PortKind::Access(1 + rng.below(4) as u16)
            };
            sw.set_port(
                p,
                Port {
                    kind: k,
                    up: rng.below(8) != 0,
                    isolated: rng.below(3) == 0,
                    max_macs: if rng.below(4) == 0 { 2 } else { 0 },
                },
            )
            .unwrap();
            let mut vids = Vec::new();
            match k {
                PortKind::Access(v) => vids.push(v),
                PortKind::Trunk { native } => {
                    vids.extend(native);
                    for v in 1..=4 {
                        if rng.below(2) == 0 {
                            sw.allow_vlan(p, v, true).unwrap();
                            if !vids.contains(&v) {
                                vids.push(v);
                            }
                        }
                    }
                }
            }
            allow.push(vids);
            kinds.push(k);
        }
        let mut last: HashMap<(u16, [u8; 6]), usize> = HashMap::new();
        for t in 0..2_000u64 {
            let in_port = rng.below(8) as usize;
            let src = mac(1 + rng.below(12) as u8);
            let dst = if rng.below(6) == 0 {
                BCAST
            } else {
                mac(1 + rng.below(12) as u8)
            };
            let vid = (rng.below(2) == 0).then(|| 1 + rng.below(4) as u16);
            let f = frame(dst, src, vid, rng.below(40) as usize);
            let Ok(d) = sw.receive(t * 100, in_port, &f) else {
                continue;
            };
            assert_eq!(
                d.ports & (1 << in_port),
                0,
                "sent back out the ingress port"
            );
            assert_eq!(d.ports >> 8, 0, "round {round}: nonexistent port");
            assert_eq!(d.tagged & !d.ports, 0, "tag on a port that gets nothing");
            for (p, k) in kinds.iter().enumerate() {
                if !d.to(p) {
                    continue;
                }
                // Egress ports carry the VLAN and tag exactly when they are
                // trunks with another native VLAN.
                assert!(
                    allow[p].contains(&d.vid),
                    "VLAN {} leaked to port {p}",
                    d.vid
                );
                let tagged = matches!(k, PortKind::Trunk { native } if *native != Some(d.vid));
                assert_eq!(d.tagged_on(p), tagged);
                if let PortKind::Access(v) = k {
                    assert_eq!(*v, d.vid, "VLAN leak into an access port");
                }
            }
            // Egress bytes: same addresses and payload, tag as decided.
            let mut out = [0u8; 256];
            for p in (0..8).filter(|&p| d.to(p)) {
                let n = emit(&f, d.tagged_on(p).then_some(d.vid), &mut out).unwrap();
                assert_eq!(&out[..6], &f[..6]);
                assert_eq!(&out[6..12], &f[6..12]);
                let tagged = out[12] == 0x81 && out[13] == 0;
                assert_eq!(tagged, d.tagged_on(p));
                let payload_len = f.len() - if vid.is_some() { 4 } else { 0 } - 14;
                assert_eq!(n, 14 + payload_len + if tagged { 4 } else { 0 });
            }
            last.insert((d.vid, src), in_port);
            if t % 500 == 0 {
                sw.check().unwrap_or_else(|m| panic!("round {round}: {m}"));
            }
        }
        sw.check().unwrap();
    }
}
