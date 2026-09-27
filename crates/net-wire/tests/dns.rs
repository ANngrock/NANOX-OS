use net_wire::dns::*;
use net_wire::Ipv4Addr;

const ID: u16 = 0x1234;
const RESPONSE: u16 = 0x8180; // QR, RD, RA, NOERROR
const A: u16 = 1;
const CNAME: u16 = 5;
const IN: u16 = 1;
const QUESTION_PTR: [u8; 2] = [0xc0, 0x0c];

fn wire_name(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for label in text.split('.').filter(|l| !l.is_empty()) {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }
    out.push(0);
    out
}

/// Message builder: header, then one A/IN question for `qname`.
struct Msg(Vec<u8>);

impl Msg {
    fn new(flags: u16, qname: &str, counts: [u16; 3]) -> Self {
        let mut v = Vec::new();
        v.extend_from_slice(&ID.to_be_bytes());
        v.extend_from_slice(&flags.to_be_bytes());
        v.extend_from_slice(&1u16.to_be_bytes());
        for count in counts {
            v.extend_from_slice(&count.to_be_bytes());
        }
        v.extend_from_slice(&wire_name(qname));
        v.extend_from_slice(&A.to_be_bytes());
        v.extend_from_slice(&IN.to_be_bytes());
        Msg(v)
    }

    fn rr(mut self, owner: &[u8], rtype: u16, ttl: u32, rdata: &[u8]) -> Self {
        self.0.extend_from_slice(owner);
        self.0.extend_from_slice(&rtype.to_be_bytes());
        self.0.extend_from_slice(&IN.to_be_bytes());
        self.0.extend_from_slice(&ttl.to_be_bytes());
        self.0
            .extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        self.0.extend_from_slice(rdata);
        self
    }
}

fn name(text: &str) -> DnsName {
    DnsName::from_text(text).unwrap()
}

fn parse(msg: &[u8], qname: &str, out: &mut [Ipv4Addr]) -> Result<AResult> {
    parse_a_response(msg, ID, &name(qname), out)
}

fn reference() -> Vec<u8> {
    // RFC 1035 layout: the answer owner is a pointer to the question name.
    Msg::new(RESPONSE, "example.com", [1, 0, 0])
        .rr(&QUESTION_PTR, A, 300, &[93, 184, 216, 34])
        .0
}

#[test]
fn query_bytes_match_rfc1035_layout() {
    let mut buf = [0u8; 64];
    let len = emit_a_query(&mut buf, 0xbeef, &name("Example.COM.")).unwrap();
    let expected: &[u8] = &[
        0xbe, 0xef, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 7, b'e', b'x',
        b'a', b'm', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0x00, 0x01, 0x00, 0x01,
    ];
    assert_eq!(&buf[..len], expected);
    assert_eq!(
        emit_a_query(&mut buf[..len - 1], 0xbeef, &name("example.com")),
        Err(DnsError::BufferTooSmall)
    );
}

#[test]
fn query_name_validation() {
    assert_eq!(name("a.b-c.d1").wire(), wire_name("a.b-c.d1").as_slice());
    for bad in [
        "", ".", "a..b", "-a.com", "a-.com", "a_b.com", "a b.com", "é.com",
    ] {
        assert_eq!(
            DnsName::from_text(bad).err(),
            Some(DnsError::BadName),
            "{bad:?}"
        );
    }
    let long_label = "a".repeat(64);
    assert_eq!(
        DnsName::from_text(&long_label).err(),
        Some(DnsError::NameTooLong)
    );
    // 3 * (1 + 63) + (1 + 61) + root = 255 bytes: the largest valid name.
    let l63 = "a".repeat(63);
    let max = format!("{l63}.{l63}.{l63}.{}", "b".repeat(61));
    assert_eq!(name(&max).wire().len(), MAX_NAME_WIRE_LEN);
    let over = format!("{l63}.{l63}.{l63}.{}", "b".repeat(62));
    assert_eq!(DnsName::from_text(&over).err(), Some(DnsError::NameTooLong));
}

#[test]
fn reference_response_and_case_insensitive_question() {
    let mut out = [Ipv4Addr([0; 4]); 4];
    assert_eq!(
        parse(&reference(), "example.com", &mut out),
        Ok(AResult { count: 1, ttl: 300 })
    );
    assert_eq!(out[0], Ipv4Addr([93, 184, 216, 34]));
    let mixed_case = Msg::new(RESPONSE, "ExAmPlE.cOm", [1, 0, 0])
        .rr(&QUESTION_PTR, A, 60, &[1, 2, 3, 4])
        .0;
    assert_eq!(
        parse(&mixed_case, "example.com", &mut out).map(|r| r.count),
        Ok(1)
    );
}

#[test]
fn header_identity_and_rcode_rejections() {
    let good = reference();
    let mut out = [Ipv4Addr([0; 4]); 4];
    assert_eq!(
        parse_a_response(&good, ID ^ 1, &name("example.com"), &mut out),
        Err(DnsError::IdMismatch)
    );
    let with_flags = |flags: u16| {
        let mut msg = good.clone();
        msg[2..4].copy_from_slice(&flags.to_be_bytes());
        parse(&msg, "example.com", &mut [Ipv4Addr([0; 4]); 4])
    };
    assert_eq!(with_flags(0x0180), Err(DnsError::BadHeader)); // QR clear
    assert_eq!(with_flags(0x8980), Err(DnsError::BadHeader)); // opcode 1
    assert_eq!(with_flags(0x8380), Err(DnsError::TruncatedResponse));
    assert_eq!(with_flags(0x8182), Err(DnsError::ServerFailure));
    assert_eq!(with_flags(0x8183), Err(DnsError::NameError));
    assert_eq!(with_flags(0x8185), Err(DnsError::Refused));
    assert_eq!(with_flags(0x8184), Err(DnsError::OtherRcode(4)));
    for qdcount in [0u16, 2] {
        let mut msg = good.clone();
        msg[4..6].copy_from_slice(&qdcount.to_be_bytes());
        assert_eq!(
            parse(&msg, "example.com", &mut out),
            Err(DnsError::BadHeader)
        );
    }
    let mut many = good.clone();
    many[10..12].copy_from_slice(&64u16.to_be_bytes()); // 1 answer + 64 additional
    assert_eq!(
        parse(&many, "example.com", &mut out),
        Err(DnsError::TooManyRecords)
    );
}

#[test]
fn question_must_echo_the_query() {
    let mut out = [Ipv4Addr([0; 4]); 4];
    assert_eq!(
        parse(&reference(), "example.org", &mut out),
        Err(DnsError::QuestionMismatch)
    );
    let qtype_at = 12 + wire_name("example.com").len();
    for (offset, value) in [(qtype_at + 1, 28u8), (qtype_at + 3, 3)] {
        let mut msg = reference();
        msg[offset] = value;
        assert_eq!(
            parse(&msg, "example.com", &mut out),
            Err(DnsError::QuestionMismatch)
        );
    }
    // The error class must not be trusted before the question matches.
    let mut spoofed_nxdomain = Msg::new(0x8183, "other.com", [0, 0, 0]).0;
    assert_eq!(
        parse(&spoofed_nxdomain, "example.com", &mut out),
        Err(DnsError::QuestionMismatch)
    );
    spoofed_nxdomain.truncate(20);
    assert!(parse(&spoofed_nxdomain, "example.com", &mut out).is_err());
}

#[test]
fn only_answer_records_reachable_from_qname_are_results() {
    let mut out = [Ipv4Addr([0; 4]); 4];
    let authority_only = Msg::new(RESPONSE, "example.com", [0, 1, 1])
        .rr(&QUESTION_PTR, A, 30, &[6, 6, 6, 6])
        .rr(&QUESTION_PTR, A, 30, &[7, 7, 7, 7])
        .0;
    assert_eq!(
        parse(&authority_only, "example.com", &mut out),
        Ok(AResult { count: 0, ttl: 0 })
    );
    let unrelated = Msg::new(RESPONSE, "example.com", [2, 0, 0])
        .rr(&wire_name("evil.test"), A, 30, &[6, 6, 6, 6])
        .rr(&QUESTION_PTR, A, 30, &[1, 1, 1, 1])
        .0;
    assert_eq!(
        parse(&unrelated, "example.com", &mut out).map(|r| r.count),
        Ok(1)
    );
    assert_eq!(out[0], Ipv4Addr([1, 1, 1, 1]));
    // Non-IN classes and other types in Answer are ignored.
    let mut chaos = Msg::new(RESPONSE, "example.com", [1, 0, 0])
        .rr(&QUESTION_PTR, A, 30, &[6, 6, 6, 6])
        .0;
    let class_at = chaos.len() - 4 - 2 - 4 - 2;
    chaos[class_at..class_at + 2].copy_from_slice(&3u16.to_be_bytes());
    assert_eq!(
        parse(&chaos, "example.com", &mut out).map(|r| r.count),
        Ok(0)
    );
}

#[test]
fn cname_chain_and_ttl_minimum() {
    let mut out = [Ipv4Addr([0; 4]); 4];
    let chained = Msg::new(RESPONSE, "www.example.com", [3, 0, 0])
        .rr(&QUESTION_PTR, CNAME, 600, &wire_name("edge.example.net"))
        .rr(
            &wire_name("edge.example.net"),
            CNAME,
            90,
            &wire_name("host.cdn.test"),
        )
        .rr(&wire_name("host.cdn.test"), A, 300, &[10, 1, 2, 3])
        .0;
    assert_eq!(
        parse(&chained, "www.example.com", &mut out),
        Ok(AResult { count: 1, ttl: 90 })
    );
    assert_eq!(out[0], Ipv4Addr([10, 1, 2, 3]));
    // A CNAME to a name without A records is NODATA; its TTL still counts.
    let dangling = Msg::new(RESPONSE, "www.example.com", [1, 0, 0])
        .rr(&QUESTION_PTR, CNAME, 45, &wire_name("gone.test"))
        .0;
    assert_eq!(
        parse(&dangling, "www.example.com", &mut out),
        Ok(AResult { count: 0, ttl: 45 })
    );
    // TTL with the top bit set counts as zero (RFC 2181 section 8).
    let huge_ttl = Msg::new(RESPONSE, "example.com", [1, 0, 0])
        .rr(&QUESTION_PTR, A, 0x8000_0000, &[1, 2, 3, 4])
        .0;
    assert_eq!(
        parse(&huge_ttl, "example.com", &mut out).map(|r| r.ttl),
        Ok(0)
    );
}

#[test]
fn cname_loops_conflicts_and_length() {
    let mut out = [Ipv4Addr([0; 4]); 4];
    let looped = Msg::new(RESPONSE, "a.test", [2, 0, 0])
        .rr(&QUESTION_PTR, CNAME, 60, &wire_name("b.test"))
        .rr(&wire_name("b.test"), CNAME, 60, &wire_name("A.TEST"))
        .0;
    assert_eq!(parse(&looped, "a.test", &mut out), Err(DnsError::CnameLoop));

    let two_targets = Msg::new(RESPONSE, "a.test", [2, 0, 0])
        .rr(&QUESTION_PTR, CNAME, 60, &wire_name("b.test"))
        .rr(&QUESTION_PTR, CNAME, 60, &wire_name("c.test"))
        .0;
    assert_eq!(
        parse(&two_targets, "a.test", &mut out),
        Err(DnsError::CnameConflict)
    );
    let cname_and_a = Msg::new(RESPONSE, "a.test", [2, 0, 0])
        .rr(&QUESTION_PTR, A, 60, &[1, 2, 3, 4])
        .rr(&QUESTION_PTR, CNAME, 60, &wire_name("b.test"))
        .0;
    assert_eq!(
        parse(&cname_and_a, "a.test", &mut out),
        Err(DnsError::CnameConflict)
    );
    // Identical duplicate CNAMEs are not a conflict.
    let duplicate = Msg::new(RESPONSE, "a.test", [3, 0, 0])
        .rr(&QUESTION_PTR, CNAME, 60, &wire_name("b.test"))
        .rr(&QUESTION_PTR, CNAME, 30, &wire_name("b.test"))
        .rr(&wire_name("b.test"), A, 60, &[1, 2, 3, 4])
        .0;
    assert_eq!(
        parse(&duplicate, "a.test", &mut out),
        Ok(AResult { count: 1, ttl: 30 })
    );

    let chain = |links: usize| {
        let mut msg = Msg::new(RESPONSE, "n0.test", [links as u16 + 1, 0, 0]);
        for i in 0..links {
            let owner = wire_name(&format!("n{i}.test"));
            msg = msg.rr(&owner, CNAME, 60, &wire_name(&format!("n{}.test", i + 1)));
        }
        msg = msg.rr(&wire_name(&format!("n{links}.test")), A, 60, &[9, 9, 9, 9]);
        parse(&msg.0, "n0.test", &mut [Ipv4Addr([0; 4]); 1])
    };
    assert_eq!(chain(MAX_CNAME_CHAIN).map(|r| r.count), Ok(1));
    assert_eq!(chain(MAX_CNAME_CHAIN + 1), Err(DnsError::CnameChainTooLong));
}

#[test]
fn output_overflow_is_an_error_not_a_partial_result() {
    let three = Msg::new(RESPONSE, "example.com", [3, 0, 0])
        .rr(&QUESTION_PTR, A, 60, &[1, 1, 1, 1])
        .rr(&QUESTION_PTR, A, 60, &[2, 2, 2, 2])
        .rr(&QUESTION_PTR, A, 60, &[3, 3, 3, 3])
        .0;
    // On OutputFull the caller's buffer keeps its previous contents.
    let sentinel = Ipv4Addr([0xde, 0xad, 0xbe, 0xef]);
    let mut small = [sentinel; 2];
    assert_eq!(
        parse(&three, "example.com", &mut small),
        Err(DnsError::OutputFull)
    );
    assert_eq!(small, [sentinel; 2]);
    // The same holds when the addresses sit behind a CNAME.
    let behind_cname = Msg::new(RESPONSE, "www.example.com", [3, 0, 0])
        .rr(&QUESTION_PTR, CNAME, 60, &wire_name("host.test"))
        .rr(&wire_name("host.test"), A, 60, &[1, 1, 1, 1])
        .rr(&wire_name("host.test"), A, 60, &[2, 2, 2, 2])
        .0;
    let mut one = [sentinel; 1];
    assert_eq!(
        parse(&behind_cname, "www.example.com", &mut one),
        Err(DnsError::OutputFull)
    );
    assert_eq!(one, [sentinel]);
    let mut out = [Ipv4Addr([0; 4]); 3];
    assert_eq!(
        parse(&three, "example.com", &mut out).map(|r| r.count),
        Ok(3)
    );
    assert_eq!(out[2], Ipv4Addr([3, 3, 3, 3]));
}

#[test]
fn rdata_validation() {
    let mut out = [Ipv4Addr([0; 4]); 4];
    for rdata in [&[1u8, 2, 3][..], &[1, 2, 3, 4, 5]] {
        let bad_a = Msg::new(RESPONSE, "example.com", [1, 0, 0])
            .rr(&QUESTION_PTR, A, 60, rdata)
            .0;
        assert_eq!(
            parse(&bad_a, "example.com", &mut out),
            Err(DnsError::BadRdata)
        );
    }
    // CNAME RDATA must be exactly one name.
    let mut padded = wire_name("b.test");
    padded.push(0);
    let bad_cname = Msg::new(RESPONSE, "a.test", [1, 0, 0])
        .rr(&QUESTION_PTR, CNAME, 60, &padded)
        .0;
    assert_eq!(
        parse(&bad_cname, "a.test", &mut out),
        Err(DnsError::BadRdata)
    );
    // Authority and Additional get the same RDATA checks, even though their
    // records never become results.
    for counts in [[1u16, 1, 0], [1, 0, 1]] {
        let bad_a = Msg::new(RESPONSE, "example.com", counts)
            .rr(&QUESTION_PTR, A, 60, &[1, 2, 3, 4])
            .rr(&QUESTION_PTR, A, 60, &[1, 2, 3])
            .0;
        assert_eq!(
            parse(&bad_a, "example.com", &mut out),
            Err(DnsError::BadRdata),
            "counts {counts:?}"
        );
        let bad_cname = Msg::new(RESPONSE, "example.com", counts)
            .rr(&QUESTION_PTR, A, 60, &[1, 2, 3, 4])
            .rr(&wire_name("other.test"), CNAME, 60, &padded)
            .0;
        assert_eq!(
            parse(&bad_cname, "example.com", &mut out),
            Err(DnsError::BadRdata),
            "counts {counts:?}"
        );
        let bad_pointer = Msg::new(RESPONSE, "example.com", counts)
            .rr(&QUESTION_PTR, A, 60, &[1, 2, 3, 4])
            .rr(&wire_name("other.test"), CNAME, 60, &[0xc3, 0xff])
            .0;
        assert_eq!(
            parse(&bad_pointer, "example.com", &mut out),
            Err(DnsError::BadPointer),
            "counts {counts:?}"
        );
    }
    // Unknown types in those sections, such as OPT (41, class = UDP size),
    // are skipped without interpretation.
    let with_opt = Msg::new(RESPONSE, "example.com", [1, 0, 1])
        .rr(&QUESTION_PTR, A, 60, &[1, 2, 3, 4])
        .rr(&[0], 41, 0, &[])
        .0;
    assert_eq!(
        parse(&with_opt, "example.com", &mut out).map(|r| r.count),
        Ok(1)
    );
    // RDLENGTH beyond the message.
    let mut long_rdlen = reference();
    let at = long_rdlen.len() - 6;
    long_rdlen[at..at + 2].copy_from_slice(&5u16.to_be_bytes());
    assert_eq!(
        parse(&long_rdlen, "example.com", &mut out),
        Err(DnsError::Truncated)
    );
}

#[test]
fn compression_pointer_rules() {
    let mut out = [Ipv4Addr([0; 4]); 4];
    let with_owner = |owner: &[u8]| {
        Msg::new(RESPONSE, "example.com", [1, 0, 0])
            .rr(owner, A, 60, &[1, 2, 3, 4])
            .0
    };
    let answer_at = reference().len() - 16;
    // Forward pointer, self pointer and pointer past the message.
    for target in [answer_at as u16 + 4, answer_at as u16, 0x3fff] {
        let ptr = (0xc000 | target).to_be_bytes();
        assert_eq!(
            parse(&with_owner(&ptr), "example.com", &mut out),
            Err(DnsError::BadPointer),
            "target {target}"
        );
    }
    // Pointer into the middle of the question name: "com" suffix is legal.
    let suffix = [0xc0, 12 + 8];
    let msg = with_owner(&suffix);
    assert_eq!(parse(&msg, "example.com", &mut out).map(|r| r.count), Ok(0));
    // A label followed by a pointer back to that same label would loop.
    let mut looping = with_owner(&[1, b'x', 0xc0, 0]);
    // Owner (4 bytes) + type, class, TTL, RDLENGTH (10) + RDATA (4).
    let owner_at = looping.len() - 18;
    looping[owner_at + 3] = owner_at as u8;
    assert_eq!(
        parse(&looping, "example.com", &mut out),
        Err(DnsError::BadPointer)
    );
    // Reserved label types 0x40 and 0x80.
    for tag in [0x40u8, 0x80] {
        assert_eq!(
            parse(&with_owner(&[tag, 0]), "example.com", &mut out),
            Err(DnsError::BadName)
        );
    }
}

#[test]
fn compressed_names_are_limited_to_255_bytes() {
    // Runs of one 63-byte label, each pointing to the previous run, stored in
    // the RDATA of an ignored record type so the record walk skips them.
    let owner_x = wire_name("x");
    let rdata_at = (12 + wire_name("example.com").len() + 4 + owner_x.len() + 10) as u16;
    let mut runs = Vec::new();
    let mut previous = 12u16; // the question name
    for _ in 0..4 {
        let here = rdata_at + runs.len() as u16;
        runs.push(63);
        runs.extend_from_slice(&[b'z'; 63]);
        runs.extend_from_slice(&(0xc000 | previous).to_be_bytes());
        previous = here;
    }
    // Owner points at the last run: 4 * 64 + 13 bytes > 255.
    let owner = (0xc000 | previous).to_be_bytes();
    let msg = Msg::new(RESPONSE, "example.com", [2, 0, 0])
        .rr(&owner_x, 99, 60, &runs)
        .rr(&owner, A, 60, &[1, 2, 3, 4])
        .0;
    assert_eq!(
        parse(&msg, "example.com", &mut [Ipv4Addr([0; 4]); 1]),
        Err(DnsError::NameTooLong)
    );
}

#[test]
fn every_truncation_is_an_error() {
    let msg = Msg::new(RESPONSE, "www.example.com", [2, 1, 0])
        .rr(&QUESTION_PTR, CNAME, 60, &wire_name("host.test"))
        .rr(&wire_name("host.test"), A, 60, &[1, 2, 3, 4])
        .rr(&QUESTION_PTR, A, 60, &[5, 6, 7, 8])
        .0;
    let mut out = [Ipv4Addr([0; 4]); 4];
    assert!(parse(&msg, "www.example.com", &mut out).is_ok());
    for cut in 0..msg.len() {
        assert!(
            parse(&msg[..cut], "www.example.com", &mut out).is_err(),
            "cut {cut}"
        );
    }
}

#[test]
fn arbitrary_bytes_never_panic() {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let base = reference();
    let mut out = [Ipv4Addr([0; 4]); 2];
    let qname = name("example.com");
    for round in 0..20_000 {
        let mut msg = base.clone();
        msg.resize(96, 0);
        // Keep the valid header/question in half the rounds so record parsing
        // and resolution are exercised, not only the early checks.
        let from = if round % 2 == 0 { base.len() - 16 } else { 0 };
        for byte in &mut msg[from..] {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        let len = from + (state as usize) % (msg.len() - from);
        let _ = parse_a_response(&msg[..len], ID, &qname, &mut out);
    }
}
