use qga::{
    Addr, Agent, Command, Config, Host, HostError, Iface, IpAddr, OsInfo, ShutdownMode, Stats,
};
use serde_json::Value;

const FIXTURE: &str = include_str!("fixtures/qemu-ga-9.2.4.json");

#[derive(Default)]
struct TestHost {
    shutdowns: Vec<ShutdownMode>,
    freezes: Vec<bool>,
    fail_shutdown: bool,
    fail_freeze: bool,
    ifaces: Vec<(String, Option<[u8; 6]>, Vec<Addr>)>,
}

impl Host for TestHost {
    fn time_ns(&mut self) -> i64 {
        1_790_000_000_123_456_789
    }
    fn host_name(&mut self) -> &str {
        "nanox-test"
    }
    fn os_info(&mut self) -> OsInfo<'_> {
        OsInfo {
            name: "NANOX",
            kernel_release: "0.1.0",
            version: "0.1",
            pretty_name: "NANOX-OS 0.1",
            version_id: "0.1",
            kernel_version: "m11-test",
            machine: "x86_64",
            id: "nanox",
        }
    }
    fn shutdown(&mut self, mode: ShutdownMode) -> Result<(), HostError> {
        if self.fail_shutdown {
            return Err(HostError("power management is not available"));
        }
        self.shutdowns.push(mode);
        Ok(())
    }
    fn fsfreeze(&mut self, freeze: bool) -> Result<u64, HostError> {
        if self.fail_freeze {
            return Err(HostError("cannot freeze"));
        }
        self.freezes.push(freeze);
        Ok(2)
    }
    fn interface(&mut self, index: usize) -> Option<Iface<'_>> {
        let (name, mac, addrs) = self.ifaces.get(index)?;
        Some(Iface {
            name,
            mac: *mac,
            addrs,
            stats: Some(Stats {
                rx_bytes: 10,
                rx_packets: 1,
                tx_bytes: 20,
                tx_packets: 2,
                ..Stats::default()
            }),
        })
    }
}

fn host_with_interfaces() -> TestHost {
    TestHost {
        ifaces: vec![
            (
                "lo".into(),
                Some([0; 6]),
                vec![
                    Addr {
                        ip: IpAddr::V4([127, 0, 0, 1]),
                        prefix: 8,
                    },
                    Addr {
                        ip: IpAddr::V6([0, 0, 0, 0, 0, 0, 0, 1]),
                        prefix: 128,
                    },
                ],
            ),
            (
                "net0".into(),
                Some([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]),
                vec![Addr {
                    ip: IpAddr::V4([10, 0, 0, 5]),
                    prefix: 24,
                }],
            ),
        ],
        ..TestHost::default()
    }
}

type A = Agent<TestHost>;

fn agent() -> A {
    Agent::new(host_with_interfaces(), Config::new("nanox-0.1"))
}

/// What the reference had blocked when the fixtures were captured.
fn reference_like() -> A {
    Agent::new(
        host_with_interfaces(),
        Config::new("nanox-0.1")
            .disable(Command::Shutdown)
            .disable(Command::FsfreezeFreeze),
    )
}

fn send(a: &mut A, input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    a.receive(input, |r| out.extend_from_slice(r));
    out
}

fn shape(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.iter().map(|(k, x)| (k.clone(), shape(x))).collect()),
        Value::Array(a) => Value::Array(a.first().map(shape).into_iter().collect()),
        Value::Bool(_) => "bool".into(),
        Value::Number(n) if n.is_f64() => "float".into(),
        Value::Number(_) => "int".into(),
        Value::Null => "null".into(),
        Value::String(_) => "string".into(),
    }
}

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

#[test]
fn every_deterministic_reply_is_byte_identical_to_qemu_ga() {
    let f = fixture();
    let mut checked = 0;
    for r in f["records"].as_array().unwrap() {
        if r["mode"] != "exact" {
            continue;
        }
        let req = hex(r["request_hex"].as_str().unwrap());
        let want = hex(r["response_hex"].as_str().unwrap());
        let got = send(&mut reference_like(), &req);
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&want),
            "{}: request {:?}",
            r["name"],
            String::from_utf8_lossy(&req)
        );
        assert_eq!(got, want, "{} (bytes)", r["name"]);
        checked += 1;
    }
    assert_eq!(checked, 39, "the capture holds 39 exact records");
}

#[test]
fn structured_replies_have_the_reference_shape() {
    let f = fixture();
    let mut a = reference_like();
    let mut checked = 0;
    for r in f["records"].as_array().unwrap() {
        if r["mode"] != "shape" {
            continue;
        }
        let req = hex(r["request_hex"].as_str().unwrap());
        let got = send(&mut a, &req);
        let text = std::str::from_utf8(&got).unwrap();
        let v: Value = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(shape(&v), r["shape"], "{}", r["name"]);
        checked += 1;
    }
    assert_eq!(checked, 5);
}

fn hex(h: &str) -> Vec<u8> {
    (0..h.len() / 2)
        .map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).unwrap())
        .collect()
}

#[test]
fn recovers_after_a_bad_message() {
    let mut a = reference_like();
    let r = send(
        &mut a,
        b"{\"execute\" \"guest-ping\"}\n{\"execute\":\"guest-ping\"}\n",
    );
    let text = String::from_utf8(r).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("missing : in object pair"));
    assert_eq!(lines[1], "{\"return\": {}}");
    assert_eq!(fixture()["recovers_after_bad_json"], "{\"return\": {}}");
}

#[test]
fn framing_split_batched_reset_and_stray() {
    let mut a = agent();
    // Byte at a time.
    let mut got = Vec::new();
    for b in b"{\"execute\": \"guest-ping\", \"id\": 1}\n" {
        a.receive(&[*b], |r| got.extend_from_slice(r));
    }
    assert_eq!(got, b"{\"return\": {}, \"id\": 1}\n");
    // Two messages in one chunk.
    let both = send(
        &mut a,
        b"{\"execute\":\"guest-ping\"}{\"execute\":\"guest-sync\",\"arguments\":{\"id\":7}}",
    );
    assert_eq!(both, b"{\"return\": {}}\n{\"return\": 7}\n");
    // 0xFF discards a partial message.
    let r = send(
        &mut a,
        b"{\"execute\":\"guest-sy\xff{\"execute\":\"guest-ping\"}",
    );
    assert_eq!(r, b"{\"return\": {}}\n");
    // Braces inside strings do not count.
    let r = send(&mut a, b"{\"execute\":\"guest-ping\",\"id\":\"}}{{\"}");
    assert_eq!(r, b"{\"return\": {}, \"id\": \"}}{{\"}\n");
    // Stray text is reported once per run, then the next message works.
    let r = send(&mut a, b"hello world\n{\"execute\":\"guest-ping\"}");
    let text = String::from_utf8(r).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].contains("stray 'h'"));
    assert_eq!(lines[1], "{\"return\": {}}");
}

#[test]
fn oversized_messages_are_refused_and_skipped() {
    let mut a: Agent<TestHost, 64, 1024> = Agent::new(TestHost::default(), Config::new("t"));
    let mut req = b"{\"execute\":\"guest-ping\",\"id\":\"".to_vec();
    req.extend(std::iter::repeat_n(b'x', 200));
    req.extend_from_slice(b"\"}");
    let mut out = Vec::new();
    a.receive(&req, |r| out.extend_from_slice(r));
    assert!(String::from_utf8(out)
        .unwrap()
        .contains("message too large"));
    let mut out = Vec::new();
    a.receive(b"{\"execute\":\"guest-ping\"}", |r| {
        out.extend_from_slice(r)
    });
    assert_eq!(out, b"{\"return\": {}}\n");
}

#[test]
fn ipv6_text_matches_the_standard_library() {
    // Zero-heavy random addresses cover every compression case.
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for _ in 0..20_000 {
        let mut g = [0u16; 8];
        let r = next();
        for (i, v) in g.iter_mut().enumerate() {
            let pick = (r >> (2 * i)) & 3;
            *v = if pick < 2 {
                0
            } else {
                (next() & 0xFFFF) as u16
            };
            if pick == 3 && next() & 1 == 0 {
                *v &= 0xF;
            }
        }
        let mut host = TestHost::default();
        host.ifaces.push((
            "x".into(),
            None,
            vec![Addr {
                ip: IpAddr::V6(g),
                prefix: 64,
            }],
        ));
        let mut a: A = Agent::new(host, Config::new("t"));
        let r = send(&mut a, b"{\"execute\":\"guest-network-get-interfaces\"}");
        let v: Value = serde_json::from_slice(&r).unwrap();
        let got = v["return"][0]["ip-addresses"][0]["ip-address"]
            .as_str()
            .unwrap();
        assert_eq!(got, std::net::Ipv6Addr::from(g).to_string(), "{g:x?}");
    }
}

#[test]
fn strings_are_escaped_and_survive_a_round_trip() {
    struct Named(String);
    impl Host for Named {
        fn time_ns(&mut self) -> i64 {
            0
        }
        fn host_name(&mut self) -> &str {
            &self.0
        }
        fn os_info(&mut self) -> OsInfo<'_> {
            OsInfo::default()
        }
        fn shutdown(&mut self, _: ShutdownMode) -> Result<(), HostError> {
            Ok(())
        }
        fn fsfreeze(&mut self, _: bool) -> Result<u64, HostError> {
            Ok(0)
        }
        fn interface(&mut self, _: usize) -> Option<Iface<'_>> {
            None
        }
    }
    for name in [
        "plain",
        "quote\" back\\slash",
        "ctl\u{1}\u{1f}\n\r\t\u{8}\u{c}",
        "é ü 中文 🙂",
        "del\u{7f}",
        "",
    ] {
        let mut a: Agent<Named> = Agent::new(Named(name.into()), Config::new("t"));
        let mut out = Vec::new();
        a.receive(b"{\"execute\":\"guest-get-host-name\"}", |r| {
            out.extend_from_slice(r)
        });
        let text = std::str::from_utf8(&out).unwrap();
        assert!(text.is_ascii(), "non-ASCII must be escaped: {text}");
        let v: Value = serde_json::from_str(text).unwrap();
        assert_eq!(v["return"]["host-name"], name);
    }
    // Upper-case hex like the reference, surrogate pairs above the BMP.
    let mut a: Agent<Named> = Agent::new(Named("é🙂".into()), Config::new("t"));
    let mut out = Vec::new();
    a.receive(b"{\"execute\":\"guest-get-host-name\"}", |r| {
        out.extend_from_slice(r)
    });
    assert_eq!(
        out,
        b"{\"return\": {\"host-name\": \"\\u00E9\\uD83D\\uDE42\"}}\n"
    );
}

#[test]
fn ids_are_echoed_normalized() {
    let mut a = agent();
    for (id, want) in [
        ("1", "1"),
        ("-7", "-7"),
        ("null", "null"),
        ("true", "true"),
        ("\"\\u0041\\/\"", "\"A/\""),
        ("{ \"a\" :[ 1 ,2 , { } ] }", "{\"a\": [1, 2, {}]}"),
        ("[]", "[]"),
    ] {
        let req = format!("{{\"execute\":\"guest-ping\",\"id\":{id}}}");
        let r = String::from_utf8(send(&mut a, req.as_bytes())).unwrap();
        assert_eq!(r, format!("{{\"return\": {{}}, \"id\": {want}}}\n"), "{id}");
    }
    // Too long to echo: dropped, the command still runs.
    let long = format!(
        "{{\"execute\":\"guest-ping\",\"id\":\"{}\"}}",
        "y".repeat(300)
    );
    assert_eq!(send(&mut a, long.as_bytes()), b"{\"return\": {}}\n");
}

#[test]
fn shutdown_replies_only_on_failure() {
    let mut a = agent();
    assert!(send(&mut a, b"{\"execute\":\"guest-shutdown\"}").is_empty());
    assert!(send(
        &mut a,
        b"{\"execute\":\"guest-shutdown\",\"arguments\":{\"mode\":\"halt\"}}"
    )
    .is_empty());
    assert!(send(
        &mut a,
        b"{\"execute\":\"guest-shutdown\",\"arguments\":{\"mode\":\"reboot\"}}"
    )
    .is_empty());
    assert_eq!(
        a.host().shutdowns,
        [
            ShutdownMode::Powerdown,
            ShutdownMode::Halt,
            ShutdownMode::Reboot
        ]
    );
    let r = String::from_utf8(send(
        &mut a,
        b"{\"execute\":\"guest-shutdown\",\"arguments\":{\"mode\":\"sideways\"}}",
    ))
    .unwrap();
    assert!(r.contains("mode is invalid"), "{r}");
    let r = String::from_utf8(send(
        &mut a,
        b"{\"execute\":\"guest-shutdown\",\"arguments\":{\"mode\":5}}",
    ))
    .unwrap();
    assert_eq!(
        r,
        "{\"error\": {\"class\": \"GenericError\", \"desc\": \"Invalid parameter type for 'mode', expected: string\"}}\n"
    );
    assert_eq!(a.host().shutdowns.len(), 3, "invalid requests did nothing");
    a.host().fail_shutdown = true;
    let r = String::from_utf8(send(&mut a, b"{\"execute\":\"guest-shutdown\"}")).unwrap();
    assert!(r.contains("power management is not available"));
    // Disabled by configuration: refused like a blocked command.
    let mut b: A = Agent::new(
        TestHost::default(),
        Config::new("t").disable(Command::Shutdown),
    );
    let r = String::from_utf8(send(&mut b, b"{\"execute\":\"guest-shutdown\"}")).unwrap();
    assert!(r.contains("has been disabled: the command is not allowed"));
    assert!(b.host().shutdowns.is_empty());
}

#[test]
fn fsfreeze_state_machine_restricts_commands_while_frozen() {
    let mut a = agent();
    let t = |a: &mut A, req: &str| String::from_utf8(send(a, req.as_bytes())).unwrap();
    assert_eq!(
        t(&mut a, r#"{"execute":"guest-fsfreeze-status"}"#),
        "{\"return\": \"thawed\"}\n"
    );
    assert_eq!(
        t(&mut a, r#"{"execute":"guest-fsfreeze-thaw"}"#),
        "{\"return\": 0}\n"
    );
    assert!(
        a.host().freezes.is_empty(),
        "thaw when thawed touches nothing"
    );
    assert_eq!(
        t(&mut a, r#"{"execute":"guest-fsfreeze-freeze"}"#),
        "{\"return\": 2}\n"
    );
    assert!(a.is_frozen());
    assert_eq!(
        t(&mut a, r#"{"execute":"guest-fsfreeze-status"}"#),
        "{\"return\": \"frozen\"}\n"
    );
    assert_eq!(
        t(&mut a, r#"{"execute":"guest-ping"}"#),
        "{\"return\": {}}\n"
    );
    for cmd in [
        "guest-get-time",
        "guest-get-host-name",
        "guest-network-get-interfaces",
        "guest-shutdown",
        "guest-fsfreeze-freeze",
    ] {
        let r = t(&mut a, &format!("{{\"execute\":\"{cmd}\"}}"));
        assert!(r.contains("has been disabled"), "{cmd}: {r}");
    }
    assert_eq!(
        t(&mut a, r#"{"execute":"guest-fsfreeze-thaw"}"#),
        "{\"return\": 2}\n"
    );
    assert!(!a.is_frozen());
    assert!(t(&mut a, r#"{"execute":"guest-get-time"}"#).starts_with("{\"return\": 17900"));
    // A failed freeze leaves the state alone.
    a.host().fail_freeze = true;
    assert!(t(&mut a, r#"{"execute":"guest-fsfreeze-freeze"}"#).contains("cannot freeze"));
    assert!(!a.is_frozen());
    assert_eq!(a.host().freezes, [true, false]);
}

#[test]
fn guest_info_lists_exactly_the_implemented_commands() {
    let mut a: A = Agent::new(
        TestHost::default(),
        Config::new("nanox-0.1").disable(Command::Shutdown),
    );
    let r = send(&mut a, b"{\"execute\":\"guest-info\"}");
    let v: Value = serde_json::from_slice(&r).unwrap();
    assert_eq!(v["return"]["version"], "nanox-0.1");
    let list = v["return"]["supported_commands"].as_array().unwrap();
    assert_eq!(list.len(), qga::COMMANDS.len());
    for (c, e) in qga::COMMANDS.iter().zip(list) {
        assert_eq!(e["name"], c.name());
        assert_eq!(e["enabled"], *c != Command::Shutdown);
        assert_eq!(e["success-response"], *c != Command::Shutdown);
    }
    // Everything Proxmox VE needs for `agent: 1` is there.
    for need in [
        "guest-ping",
        "guest-network-get-interfaces",
        "guest-get-osinfo",
        "guest-fsfreeze-freeze",
        "guest-fsfreeze-thaw",
        "guest-fsfreeze-status",
        "guest-shutdown",
        "guest-get-time",
    ] {
        assert!(list.iter().any(|e| e["name"] == need), "{need}");
    }
    // Dangerous commands are answered as blocked, whatever the config.
    for c in qga::BLOCKED {
        let r = String::from_utf8(send(
            &mut agent(),
            format!("{{\"execute\":\"{c}\"}}").as_bytes(),
        ))
        .unwrap();
        assert!(
            r.contains("has been disabled: the command is not allowed"),
            "{c}: {r}"
        );
        assert!(!list.iter().any(|e| e["name"] == c), "{c} is listed");
    }
}

#[test]
fn a_reply_that_does_not_fit_becomes_an_error() {
    let mut a: Agent<TestHost, 4096, 100> = Agent::new(host_with_interfaces(), Config::new("t"));
    let mut out = Vec::new();
    a.receive(b"{\"execute\":\"guest-network-get-interfaces\"}", |r| {
        out.extend_from_slice(r)
    });
    assert_eq!(
        String::from_utf8(out).unwrap(),
        "{\"error\": {\"class\": \"GenericError\", \"desc\": \"response too large\"}}\n"
    );
}

#[test]
fn mutated_messages_never_panic_and_every_reply_is_valid_json() {
    let seeds: [&[u8]; 6] = [
        br#"{"execute":"guest-sync","arguments":{"id":42},"id":"a"}"#,
        br#"{"execute":"guest-shutdown","arguments":{"mode":"halt"}}"#,
        br#"{"execute":"guest-network-get-interfaces","id":{"k":[1,2,{"z":null}]}}"#,
        br#"{"execute":"guest-fsfreeze-freeze"}"#,
        br#"{"arguments":{"aA":1.5e3},"execute":"guest-ping"}"#,
        br#"[{"execute":"guest-ping"}]"#,
    ];
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut a = agent();
    let (mut replies, mut errors) = (0, 0);
    for round in 0..60_000 {
        let mut m = seeds[round % seeds.len()].to_vec();
        for _ in 0..(next() % 4) {
            let i = (next() % m.len() as u64) as usize;
            match next() % 5 {
                0 => m[i] = (next() & 0xFF) as u8,
                1 => m[i] = b"{}[]\",:\\ "[(next() % 9) as usize],
                2 => {
                    m.remove(i);
                }
                3 => m.insert(i, b"{}[]\",:\\ 0"[(next() % 10) as usize]),
                _ => m.truncate(i.max(1)),
            }
            if m.is_empty() {
                m.push(b'{');
            }
        }
        m.push(b'\n');
        let mut out = Vec::new();
        a.receive(&m, |r| out.extend_from_slice(r));
        for line in out.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            let line = line.strip_prefix(&[0xFF]).unwrap_or(line);
            let v: Value = serde_json::from_slice(line).unwrap_or_else(|e| {
                panic!(
                    "bad reply {:?} to {:?}: {e}",
                    String::from_utf8_lossy(line),
                    String::from_utf8_lossy(&m)
                )
            });
            assert!(v.get("return").is_some() != v.get("error").is_some());
            replies += 1;
            errors += usize::from(v.get("error").is_some());
        }
        // Keep the state machine from getting stuck frozen for all rounds.
        if round % 50 == 0 {
            let _ = send(&mut a, b"\xff{\"execute\":\"guest-fsfreeze-thaw\"}");
        }
    }
    // Most mutations leave a message incomplete (no reply); enough remain to
    // exercise both the error and the success paths.
    assert!(
        replies > 3_000 && errors > 1_000 && replies - errors > 200,
        "{replies} replies, {errors} errors"
    );
}
