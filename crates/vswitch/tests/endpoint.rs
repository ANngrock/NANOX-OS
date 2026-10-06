use vswitch::endpoint::*;

const HOST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x35, 0x02];
const HOST_IP: [u8; 4] = [10, 0, 2, 2];
const GUEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x4E, 0x58, 0x01];
const GUEST_IP: [u8; 4] = [10, 0, 2, 15];

fn host() -> Endpoint {
    Endpoint::new(HOST_MAC, HOST_IP)
}

fn arp(dst: [u8; 6], oper: u16, target: [u8; 4]) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(&dst);
    f.extend_from_slice(&GUEST_MAC);
    f.extend_from_slice(&[0x08, 0x06, 0, 1, 0x08, 0, 6, 4]);
    f.extend_from_slice(&oper.to_be_bytes());
    f.extend_from_slice(&GUEST_MAC);
    f.extend_from_slice(&GUEST_IP);
    f.extend_from_slice(&[0; 6]);
    f.extend_from_slice(&target);
    f
}

/// An echo request as Linux's ping sends it: IPv4 with DF, `options` extra
/// header words, ICMP id 0x1234, sequence 1, `data`.
fn ping(options: usize, data: &[u8]) -> Vec<u8> {
    let ihl = 5 + options;
    let mut icmp = vec![8, 0, 0, 0, 0x12, 0x34, 0, 1];
    icmp.extend_from_slice(data);
    let sum = checksum(&icmp);
    icmp[2..4].copy_from_slice(&sum.to_be_bytes());
    let total = 4 * ihl + icmp.len();
    let mut ip = vec![
        0x40 | ihl as u8,
        0,
        (total >> 8) as u8,
        total as u8,
        0xAB,
        0xCD,
        0x40,
        0,
    ];
    ip.extend_from_slice(&[64, 1, 0, 0]);
    ip.extend_from_slice(&GUEST_IP);
    ip.extend_from_slice(&HOST_IP);
    ip.extend(std::iter::repeat_n(1u8, 4 * options)); // NOP options
    let sum = checksum(&ip);
    ip[10..12].copy_from_slice(&sum.to_be_bytes());
    let mut f = Vec::new();
    f.extend_from_slice(&HOST_MAC);
    f.extend_from_slice(&GUEST_MAC);
    f.extend_from_slice(&[0x08, 0x00]);
    f.extend_from_slice(&ip);
    f.extend_from_slice(&icmp);
    f
}

fn answer(e: &mut Endpoint, frame: &[u8]) -> Option<Vec<u8>> {
    let mut out = [0u8; 2048];
    e.answer(frame, &mut out).map(|n| out[..n].to_vec())
}

#[test]
fn the_internet_checksum() {
    // A header from RFC 1071's usual example: its checksum is 0xB861.
    let mut h = [
        0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 0xC0, 0xA8, 0x00,
        0x01, 0xC0, 0xA8, 0x00, 0xC7,
    ];
    assert_eq!(checksum(&h), 0xB861);
    h[10..12].copy_from_slice(&[0xB8, 0x61]);
    assert_eq!(checksum(&h), 0);
    // an odd length is padded with a zero byte; carries fold back in
    assert_eq!(checksum(&[0x01]), !0x0100);
    assert_eq!(checksum(&[0xFF, 0xFF, 0x00, 0x02]), !0x0002);
    assert_eq!(checksum(&[]), 0xFFFF);
    // a sum of exactly 0x10000 folds to 1
    assert_eq!(checksum(&[0xFF, 0xFF, 0x00, 0x01]), 0xFFFE);
}

#[test]
fn an_arp_request_for_the_host_gets_its_mac() {
    let mut e = host();
    let reply = answer(&mut e, &arp(BROADCAST, 1, HOST_IP)).unwrap();
    let mut want = Vec::new();
    want.extend_from_slice(&GUEST_MAC);
    want.extend_from_slice(&HOST_MAC);
    want.extend_from_slice(&[0x08, 0x06, 0, 1, 0x08, 0, 6, 4, 0, 2]);
    want.extend_from_slice(&HOST_MAC);
    want.extend_from_slice(&HOST_IP);
    want.extend_from_slice(&GUEST_MAC);
    want.extend_from_slice(&GUEST_IP);
    assert_eq!(reply, want);
    assert_eq!(reply.len(), ARP_FRAME);
    // also when sent to the host's MAC (a refresh), and with padding after it
    let mut padded = arp(HOST_MAC, 1, HOST_IP);
    padded.resize(60, 0);
    assert_eq!(answer(&mut e, &padded), Some(want));
    assert_eq!((e.arp_replies, e.echo_replies, e.ignored), (2, 0, 0));
}

#[test]
fn other_arp_is_ignored() {
    let mut e = host();
    let mut cases = vec![
        ("for another address", arp(BROADCAST, 1, GUEST_IP)),
        ("a reply", arp(BROADCAST, 2, HOST_IP)),
        ("to another MAC", arp(GUEST_MAC, 1, HOST_IP)),
        ("cut short", arp(BROADCAST, 1, HOST_IP)[..41].to_vec()),
    ];
    for (i, what) in [
        (14, "not Ethernet"),
        (16, "not IPv4"),
        (18, "6-byte"),
        (19, "4-byte"),
    ] {
        let mut f = arp(BROADCAST, 1, HOST_IP);
        f[i + 1] ^= 1;
        cases.push((what, f));
    }
    for (what, f) in &cases {
        assert_eq!(answer(&mut e, f), None, "{what}");
    }
    assert_eq!(e.ignored, cases.len() as u64);
    assert_eq!(e.arp_replies, 0);
}

#[test]
fn an_echo_request_gets_an_echo_reply() {
    let mut e = host();
    for (options, data) in [(0, &b"NANOX ping"[..]), (2, &b"odd"[..]), (0, &[][..])] {
        let req = ping(options, data);
        let reply = answer(&mut e, &req).unwrap();
        assert_eq!(&reply[..6], &GUEST_MAC);
        assert_eq!(&reply[6..12], &HOST_MAC);
        assert_eq!(&reply[12..14], &[0x08, 0x00]);
        let ip = &reply[14..34];
        let total = 20 + 8 + data.len();
        assert_eq!(
            ip,
            {
                let mut h = vec![0x45, 0, (total >> 8) as u8, total as u8, 0, 0, 0, 0, TTL, 1];
                let sum_at = h.len();
                h.extend_from_slice(&[0, 0]);
                h.extend_from_slice(&HOST_IP);
                h.extend_from_slice(&GUEST_IP);
                let sum = checksum(&h);
                h[sum_at..sum_at + 2].copy_from_slice(&sum.to_be_bytes());
                h
            }
            .as_slice()
        );
        assert_eq!(checksum(ip), 0);
        let icmp = &reply[34..];
        assert_eq!(&icmp[..2], &[0, 0], "echo reply, code 0");
        assert_eq!(&icmp[4..8], &[0x12, 0x34, 0, 1], "id and sequence");
        assert_eq!(&icmp[8..], data);
        assert_eq!(checksum(icmp), 0);
        assert_eq!(reply.len(), 14 + total);
        assert_eq!(TTL, 64);
    }
    // Ethernet padding after the IP packet is not echoed
    let mut padded = ping(0, b"x");
    let unpadded = answer(&mut e, &padded).unwrap();
    padded.resize(60, 0);
    assert_eq!(answer(&mut e, &padded), Some(unpadded));
    assert_eq!((e.arp_replies, e.echo_replies, e.ignored), (0, 5, 0));
}

#[test]
fn other_ip_is_ignored() {
    let mut e = host();
    let good = ping(0, b"data");
    let edit = |f: &dyn Fn(&mut Vec<u8>)| {
        let mut p = good.clone();
        f(&mut p);
        p
    };
    // keeps the IPv4 header checksum right after an edit of the header
    let refix = |p: &mut Vec<u8>| {
        let ihl = usize::from(p[14] & 0x0F) * 4;
        p[24..26].fill(0);
        let sum = checksum(&p[14..14 + ihl]);
        p[24..26].copy_from_slice(&sum.to_be_bytes());
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "to another MAC",
            edit(&|p| p[..6].copy_from_slice(&BROADCAST)),
        ),
        (
            "not IPv4",
            edit(&|p| {
                p[14] = 0x65;
                refix(p)
            }),
        ),
        (
            "header too short",
            edit(&|p| {
                p[14] = 0x44;
            }),
        ),
        ("bad header checksum", edit(&|p| p[25] ^= 1)),
        (
            "a fragment",
            edit(&|p| {
                p[20] = 0x20;
                refix(p)
            }),
        ),
        (
            "a later fragment",
            edit(&|p| {
                p[21] = 0x01;
                refix(p)
            }),
        ),
        (
            "not ICMP",
            edit(&|p| {
                p[23] = 17;
                refix(p)
            }),
        ),
        (
            "to another address",
            edit(&|p| {
                p[33] = 3;
                refix(p)
            }),
        ),
        (
            "longer than the frame",
            edit(&|p| {
                p[17] += 1;
                refix(p)
            }),
        ),
        (
            "too short for ICMP",
            edit(&|p| {
                // seven bytes of ICMP, with a checksum right over them
                p.truncate(14 + 27);
                p[16..18].copy_from_slice(&27u16.to_be_bytes());
                refix(p);
                p[36..38].fill(0);
                let sum = checksum(&p[34..]);
                p[36..38].copy_from_slice(&sum.to_be_bytes());
            }),
        ),
        (
            "an echo reply",
            edit(&|p| {
                p[34] = 0;
                p[36] = p[36].wrapping_add(8)
            }),
        ),
        (
            "code 1",
            edit(&|p| {
                p[35] = 1;
                p[37] = p[37].wrapping_sub(1)
            }),
        ),
        ("bad ICMP checksum", edit(&|p| p[37] ^= 1)),
        ("cut in the header", good[..30].to_vec()),
        ("only Ethernet", good[..14].to_vec()),
        (
            "an IPv6 frame",
            edit(&|p| p[12..14].copy_from_slice(&[0x86, 0xDD])),
        ),
        ("too short for a type", good[..13].to_vec()),
    ];
    for (what, f) in &cases {
        assert_eq!(answer(&mut e, f), None, "{what}");
    }
    assert_eq!(e.ignored, cases.len() as u64);
    assert_eq!(e.echo_replies, 0);
    // the edits really were the only thing wrong
    assert!(answer(&mut e, &good).is_some());
}

#[test]
fn an_answer_that_does_not_fit_is_not_given() {
    let mut e = host();
    let req = ping(0, b"0123456789");
    let mut out = vec![0u8; 14 + 20 + 8 + 10 - 1];
    assert_eq!(e.answer(&req, &mut out), None);
    out.push(0);
    assert_eq!(e.answer(&req, &mut out), Some(out.len()));
    let mut out = [0u8; ARP_FRAME - 1];
    assert_eq!(e.answer(&arp(BROADCAST, 1, HOST_IP), &mut out), None);
    assert_eq!((e.arp_replies, e.echo_replies, e.ignored), (0, 1, 2));
}
