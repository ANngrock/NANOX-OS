//! Guest side of the QEMU Guest Agent protocol for NANOX (docs/specs/
//! M11-SERVER.md §4): what Proxmox VE talks to over the virtio-serial
//! channel `org.qemu.guest_agent.0` when a VM has `agent: 1`.
//!
//! * [`frame`] — messages on the byte channel;
//! * [`json`] — allocation-free JSON reader with QEMU's error wording;
//! * [`out`] — JSON output in QEMU's exact format;
//! * [`Agent`] — command dispatch over a [`Host`] trait the kernel (or a
//!   test) implements.
//!
//! Replies are byte-compared with the reference `qemu-ga` 9.2.4 for the
//! deterministic commands and structure-compared for the others
//! (tests/fixtures, captured by tools/proxmox/capture_qga.py). Behaviour
//! that could not be checked against the reference is marked "(unverified)".
//! Dangerous commands (exec, file access, ...) are refused with the
//! "disabled" answer `qemu-ga --block-rpcs` gives; implemented commands can
//! be disabled per instance ([`Config::disable`]).

#![no_std]
#![forbid(unsafe_code)]

pub mod frame;
pub mod json;
pub mod out;

use frame::{Channel, Feed};
use json::{Kind, Val};
use out::Out;

/// Commands this agent implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    SyncDelimited,
    Sync,
    Ping,
    Info,
    GetTime,
    GetHostName,
    GetOsinfo,
    Shutdown,
    FsfreezeStatus,
    FsfreezeFreeze,
    FsfreezeThaw,
    NetworkGetInterfaces,
}

pub const COMMANDS: [Command; 12] = [
    Command::SyncDelimited,
    Command::Sync,
    Command::Ping,
    Command::Info,
    Command::GetTime,
    Command::GetHostName,
    Command::GetOsinfo,
    Command::Shutdown,
    Command::FsfreezeStatus,
    Command::FsfreezeFreeze,
    Command::FsfreezeThaw,
    Command::NetworkGetInterfaces,
];

impl Command {
    pub fn name(self) -> &'static str {
        match self {
            Command::SyncDelimited => "guest-sync-delimited",
            Command::Sync => "guest-sync",
            Command::Ping => "guest-ping",
            Command::Info => "guest-info",
            Command::GetTime => "guest-get-time",
            Command::GetHostName => "guest-get-host-name",
            Command::GetOsinfo => "guest-get-osinfo",
            Command::Shutdown => "guest-shutdown",
            Command::FsfreezeStatus => "guest-fsfreeze-status",
            Command::FsfreezeFreeze => "guest-fsfreeze-freeze",
            Command::FsfreezeThaw => "guest-fsfreeze-thaw",
            Command::NetworkGetInterfaces => "guest-network-get-interfaces",
        }
    }

    /// `guest-shutdown` never replies on success (the guest is going away).
    pub fn success_response(self) -> bool {
        self != Command::Shutdown
    }

    fn bit(self) -> u16 {
        1 << self as u16
    }

    /// Commands allowed while the filesystems are frozen: those that touch
    /// no disk (unverified: the reference's list, from memory).
    fn allowed_while_frozen(self) -> bool {
        matches!(
            self,
            Command::SyncDelimited
                | Command::Sync
                | Command::Ping
                | Command::Info
                | Command::FsfreezeStatus
                | Command::FsfreezeThaw
        )
    }
}

/// Known dangerous commands this agent does not implement: they get the
/// same "disabled" answer as a blocked command in `qemu-ga`.
pub const BLOCKED: [&str; 23] = [
    "guest-exec",
    "guest-exec-status",
    "guest-file-open",
    "guest-file-close",
    "guest-file-read",
    "guest-file-write",
    "guest-file-seek",
    "guest-file-flush",
    "guest-set-user-password",
    "guest-ssh-add-authorized-keys",
    "guest-ssh-remove-authorized-keys",
    "guest-ssh-get-authorized-keys",
    "guest-set-vcpus",
    "guest-set-memory-blocks",
    "guest-set-time",
    "guest-suspend-disk",
    "guest-suspend-ram",
    "guest-suspend-hybrid",
    "guest-fstrim",
    "guest-fsfreeze-freeze-list",
    "guest-get-fsinfo",
    "guest-get-users",
    "guest-get-memory-blocks",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    enabled: u16,
    pub version: &'static str,
}

impl Config {
    /// Every implemented command enabled.
    pub const fn new(version: &'static str) -> Self {
        Self {
            enabled: (1 << COMMANDS.len()) - 1,
            version,
        }
    }

    pub const fn disable(mut self, c: Command) -> Self {
        self.enabled &= !(1 << c as u16);
        self
    }

    pub fn enabled(&self, c: Command) -> bool {
        self.enabled & c.bit() != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownMode {
    Powerdown,
    Halt,
    Reboot,
}

/// A failure the host reports; its text goes into a GenericError reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostError(pub &'static str);

#[derive(Clone, Copy, Debug, Default)]
pub struct OsInfo<'a> {
    pub name: &'a str,
    pub kernel_release: &'a str,
    pub version: &'a str,
    pub pretty_name: &'a str,
    pub version_id: &'a str,
    pub kernel_version: &'a str,
    pub machine: &'a str,
    pub id: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IpAddr {
    V4([u8; 4]),
    /// Eight 16-bit groups, most significant first.
    V6([u16; 8]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Addr {
    pub ip: IpAddr,
    pub prefix: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errs: u64,
    pub rx_dropped: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errs: u64,
    pub tx_dropped: u64,
}

#[derive(Clone, Copy, Debug)]
pub struct Iface<'a> {
    pub name: &'a str,
    pub mac: Option<[u8; 6]>,
    pub addrs: &'a [Addr],
    pub stats: Option<Stats>,
}

/// What the agent needs from the system it runs in.
pub trait Host {
    /// Nanoseconds since the Unix epoch.
    fn time_ns(&mut self) -> i64;
    fn host_name(&mut self) -> &str;
    fn os_info(&mut self) -> OsInfo<'_>;
    /// Starts the shutdown; on success the agent sends no reply.
    fn shutdown(&mut self, mode: ShutdownMode) -> Result<(), HostError>;
    /// Freezes (true) or thaws (false) the filesystems; returns how many.
    fn fsfreeze(&mut self, freeze: bool) -> Result<u64, HostError>;
    /// Interface `index` (0-based), None past the last.
    fn interface(&mut self, index: usize) -> Option<Iface<'_>>;
}

/// Longest `id` (raw JSON bytes) that is echoed; longer ones are dropped.
const MAX_ID: usize = 256;

pub struct Agent<H: Host, const IN: usize = 4096, const OUT: usize = 16384> {
    host: H,
    cfg: Config,
    frozen: bool,
    channel: Channel<IN>,
    reply: [u8; OUT],
}

struct Fail<'a> {
    class: &'static str,
    pieces: [&'a [u8]; 4],
}

const GENERIC: &str = "GenericError";
const NOT_FOUND: &str = "CommandNotFound";

fn err<'a>(class: &'static str, pieces: &[&'a [u8]]) -> Fail<'a> {
    let mut p: [&[u8]; 4] = [b""; 4];
    p[..pieces.len()].copy_from_slice(pieces);
    Fail { class, pieces: p }
}

impl<H: Host, const IN: usize, const OUT: usize> Agent<H, IN, OUT> {
    pub fn new(host: H, cfg: Config) -> Self {
        Self {
            host,
            cfg,
            frozen: false,
            channel: Channel::new(),
            reply: [0; OUT],
        }
    }

    pub fn host(&mut self) -> &mut H {
        &mut self.host
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// Feeds bytes read from the channel; every reply is passed to `send`.
    pub fn receive(&mut self, input: &[u8], mut send: impl FnMut(&[u8])) {
        for &b in input {
            match self.channel.feed(b) {
                Feed::Need => {}
                Feed::Message => {
                    let n = Self::dispatch(
                        &mut self.host,
                        &self.cfg,
                        &mut self.frozen,
                        self.channel.message(),
                        &mut self.reply,
                    );
                    self.channel.reset();
                    if n > 0 {
                        send(&self.reply[..n]);
                    }
                }
                Feed::TooLarge => {
                    let n = Self::error_reply(
                        &mut self.reply,
                        None,
                        &err(GENERIC, &[b"JSON parse error, message too large"]),
                    );
                    send(&self.reply[..n]);
                }
                Feed::Stray(c) => {
                    let ch = [c];
                    let stray: &[u8] = if (0x20..0x7F).contains(&c) { &ch } else { b"?" };
                    let n = Self::error_reply(
                        &mut self.reply,
                        None,
                        &err(GENERIC, &[b"JSON parse error, stray '", stray, b"'"]),
                    );
                    send(&self.reply[..n]);
                }
            }
        }
    }

    /// Handles one complete message; returns the reply length (0: none).
    pub fn handle(&mut self, msg: &[u8], out: &mut [u8]) -> usize {
        Self::dispatch(&mut self.host, &self.cfg, &mut self.frozen, msg, out)
    }

    fn error_reply(out: &mut [u8], id: Option<(&[u8], Val)>, e: &Fail<'_>) -> usize {
        let mut o = Out::new(out);
        o.raw(b"{");
        if let Some((s, v)) = id {
            o.raw(b"\"id\": ");
            o.value(s, v);
            o.raw(b", ");
        }
        o.raw(b"\"error\": {\"class\": \"");
        o.str(e.class);
        o.raw(b"\", \"desc\": ");
        o.string(&e.pieces);
        o.raw(b"}}\n");
        if o.overflowed() {
            0
        } else {
            o.len()
        }
    }

    fn dispatch(
        host: &mut H,
        cfg: &Config,
        frozen: &mut bool,
        msg: &[u8],
        out: &mut [u8],
    ) -> usize {
        let top = match json::parse(msg) {
            Ok(v) => v,
            Err(e) => {
                let text = e.text();
                return Self::error_reply(
                    out,
                    None,
                    &err(GENERIC, &[b"JSON parse error, ", text.as_bytes()]),
                );
            }
        };
        if top.kind != Kind::Object {
            return Self::error_reply(
                out,
                None,
                &err(GENERIC, &[b"QMP input must be a JSON object"]),
            );
        }
        // The `id` is echoed in every reply, errors included.
        let id = json::members(msg, top)
            .find(|(k, _)| key_is(msg, *k, b"id"))
            .map(|(_, v)| v)
            .filter(|v| v.end - v.start <= MAX_ID)
            .map(|v| (msg, v));
        match Self::run(host, cfg, frozen, msg, top, out, id) {
            Ok(n) => n,
            Err(e) => Self::error_reply(out, id, &e),
        }
    }

    fn run<'m>(
        host: &mut H,
        cfg: &Config,
        frozen: &mut bool,
        msg: &'m [u8],
        top: Val,
        out: &mut [u8],
        id: Option<(&[u8], Val)>,
    ) -> Result<usize, Fail<'m>> {
        let mut execute = None;
        let mut arguments = None;
        for (k, v) in json::members(msg, top) {
            if key_is(msg, k, b"execute") {
                if v.kind != Kind::String {
                    return Err(err(
                        GENERIC,
                        &[b"QMP input member 'execute' must be a string"],
                    ));
                }
                execute = Some(v);
            } else if key_is(msg, k, b"arguments") {
                if v.kind != Kind::Object {
                    return Err(err(
                        GENERIC,
                        &[b"QMP input member 'arguments' must be an object"],
                    ));
                }
                arguments = Some(v);
            } else if !key_is(msg, k, b"id") {
                return Err(unexpected(
                    msg,
                    k,
                    b"QMP input member '",
                    b"' is unexpected",
                ));
            }
        }
        let Some(execute) = execute else {
            return Err(err(GENERIC, &[b"QMP input lacks member 'execute'"]));
        };
        // A name longer than the buffer cannot match any command.
        let mut name = [0u8; 64];
        let n = json::unescape(msg, execute, &mut name).unwrap_or(0);
        let name = &name[..n];
        let cmd = COMMANDS
            .iter()
            .copied()
            .find(|c| c.name().as_bytes() == name);
        let Some(cmd) = cmd else {
            return Err(if BLOCKED.iter().any(|b| b.as_bytes() == name) {
                disabled_error(msg, execute)
            } else {
                err(
                    NOT_FOUND,
                    &[
                        b"The command ",
                        raw_string(msg, execute),
                        b" has not been found",
                    ],
                )
            });
        };
        if !cfg.enabled(cmd) || (*frozen && !cmd.allowed_while_frozen()) {
            return Err(disabled_error(msg, execute));
        }
        let args = check_args(msg, cmd, arguments)?;
        let mut o = Out::new(out);
        if cmd == Command::SyncDelimited {
            o.raw(&[0xFF]);
        }
        o.raw(b"{\"return\": ");
        match cmd {
            Command::SyncDelimited | Command::Sync => o.int(args.id),
            Command::Ping => o.raw(b"{}"),
            Command::Info => {
                o.raw(b"{\"version\": ");
                o.string(&[cfg.version.as_bytes()]);
                o.raw(b", \"supported_commands\": [");
                for (i, c) in COMMANDS.iter().enumerate() {
                    if i > 0 {
                        o.raw(b", ");
                    }
                    o.raw(b"{\"enabled\": ");
                    o.bool(cfg.enabled(*c));
                    o.raw(b", \"name\": ");
                    o.string(&[c.name().as_bytes()]);
                    o.raw(b", \"success-response\": ");
                    o.bool(c.success_response());
                    o.raw(b"}");
                }
                o.raw(b"]}");
            }
            Command::GetTime => o.int(host.time_ns()),
            Command::GetHostName => {
                o.raw(b"{\"host-name\": ");
                o.string(&[host.host_name().as_bytes()]);
                o.raw(b"}");
            }
            Command::GetOsinfo => {
                let i = host.os_info();
                o.raw(b"{");
                for (n, (key, v)) in [
                    ("name", i.name),
                    ("kernel-release", i.kernel_release),
                    ("version", i.version),
                    ("pretty-name", i.pretty_name),
                    ("version-id", i.version_id),
                    ("kernel-version", i.kernel_version),
                    ("machine", i.machine),
                    ("id", i.id),
                ]
                .iter()
                .enumerate()
                {
                    if n > 0 {
                        o.raw(b", ");
                    }
                    o.raw(b"\"");
                    o.str(key);
                    o.raw(b"\": ");
                    o.string(&[v.as_bytes()]);
                }
                o.raw(b"}");
            }
            Command::Shutdown => {
                return match host.shutdown(args.mode) {
                    Ok(()) => Ok(0),
                    Err(HostError(m)) => Err(err(GENERIC, &[m.as_bytes()])),
                };
            }
            Command::FsfreezeStatus => o.raw(if *frozen {
                b"\"frozen\""
            } else {
                b"\"thawed\""
            }),
            Command::FsfreezeFreeze => match host.fsfreeze(true) {
                Ok(n) => {
                    *frozen = true;
                    o.uint(n);
                }
                Err(HostError(m)) => return Err(err(GENERIC, &[m.as_bytes()])),
            },
            Command::FsfreezeThaw => {
                if *frozen {
                    match host.fsfreeze(false) {
                        Ok(n) => {
                            *frozen = false;
                            o.uint(n);
                        }
                        Err(HostError(m)) => return Err(err(GENERIC, &[m.as_bytes()])),
                    }
                } else {
                    o.uint(0);
                }
            }
            Command::NetworkGetInterfaces => {
                o.raw(b"[");
                let mut i = 0;
                while let Some(f) = host.interface(i) {
                    if i > 0 {
                        o.raw(b", ");
                    }
                    iface_json(&mut o, &f);
                    i += 1;
                }
                o.raw(b"]");
            }
        }
        if let Some((s, v)) = id {
            o.raw(b", \"id\": ");
            o.value(s, v);
        }
        o.raw(b"}\n");
        if o.overflowed() {
            return Err(err(GENERIC, &[b"response too large"]));
        }
        Ok(o.len())
    }
}

fn key_is(msg: &[u8], k: Val, want: &[u8]) -> bool {
    let mut t = [0u8; 40];
    matches!(json::unescape(msg, k, &mut t), Some(n) if &t[..n] == want)
}

fn raw_string(msg: &[u8], v: Val) -> &[u8] {
    // The escaped text between the quotes; for names/keys without escapes
    // this is the text itself.
    &msg[v.start + 1..v.end - 1]
}

fn unexpected<'m>(msg: &'m [u8], k: Val, pre: &'static [u8], post: &'static [u8]) -> Fail<'m> {
    err(GENERIC, &[pre, raw_string(msg, k), post])
}

fn disabled_error<'m>(msg: &'m [u8], name: Val) -> Fail<'m> {
    err(
        NOT_FOUND,
        &[
            b"Command ",
            raw_string(msg, name),
            b" has been disabled: the command is not allowed",
        ],
    )
}

struct Args {
    id: i64,
    mode: ShutdownMode,
}

fn check_args<'m>(msg: &'m [u8], cmd: Command, args: Option<Val>) -> Result<Args, Fail<'m>> {
    let mut out = Args {
        id: 0,
        mode: ShutdownMode::Powerdown,
    };
    let member = |key: &[u8]| -> Option<Val> {
        json::members(msg, args?)
            .find(|(k, _)| key_is(msg, *k, key))
            .map(|(_, v)| v)
    };
    let known: &[&[u8]] = match cmd {
        Command::SyncDelimited | Command::Sync => &[b"id"],
        Command::Shutdown => &[b"mode"],
        _ => &[],
    };
    match cmd {
        Command::SyncDelimited | Command::Sync => match member(b"id") {
            None => return Err(err(GENERIC, &[b"Parameter 'id' is missing"])),
            Some(v) => match json::as_i64(msg, v) {
                Some(n) => out.id = n,
                None => {
                    return Err(err(
                        GENERIC,
                        &[b"Invalid parameter type for 'id', expected: integer"],
                    ))
                }
            },
        },
        Command::Shutdown => {
            if let Some(v) = member(b"mode") {
                if v.kind != Kind::String {
                    return Err(err(
                        GENERIC,
                        &[b"Invalid parameter type for 'mode', expected: string"],
                    ));
                }
                let mut m = [0u8; 16];
                let n = json::unescape(msg, v, &mut m).unwrap_or(0);
                out.mode = match &m[..n] {
                    b"powerdown" => ShutdownMode::Powerdown,
                    b"halt" => ShutdownMode::Halt,
                    b"reboot" => ShutdownMode::Reboot,
                    // (unverified) the reference's text, from memory.
                    _ => {
                        return Err(err(
                            GENERIC,
                            &[b"mode is invalid (valid values are: halt|powerdown|reboot"],
                        ))
                    }
                };
            }
        }
        _ => {}
    }
    if let Some(a) = args {
        for (k, _) in json::members(msg, a) {
            if !known.iter().any(|w| key_is(msg, k, w)) {
                return Err(unexpected(msg, k, b"Parameter '", b"' is unexpected"));
            }
        }
    }
    Ok(out)
}

fn iface_json(o: &mut Out<'_>, f: &Iface<'_>) {
    o.raw(b"{\"name\": ");
    o.string(&[f.name.as_bytes()]);
    if !f.addrs.is_empty() {
        o.raw(b", \"ip-addresses\": [");
        for (i, a) in f.addrs.iter().enumerate() {
            if i > 0 {
                o.raw(b", ");
            }
            o.raw(b"{\"ip-address-type\": \"");
            o.str(match a.ip {
                IpAddr::V4(_) => "ipv4",
                IpAddr::V6(_) => "ipv6",
            });
            o.raw(b"\", \"ip-address\": \"");
            ip_text(o, &a.ip);
            o.raw(b"\", \"prefix\": ");
            o.uint(u64::from(a.prefix));
            o.raw(b"}");
        }
        o.raw(b"]");
    }
    if let Some(s) = f.stats {
        o.raw(b", \"statistics\": {");
        for (i, (k, v)) in [
            ("tx-packets", s.tx_packets),
            ("tx-errs", s.tx_errs),
            ("rx-bytes", s.rx_bytes),
            ("rx-dropped", s.rx_dropped),
            ("rx-packets", s.rx_packets),
            ("rx-errs", s.rx_errs),
            ("tx-bytes", s.tx_bytes),
            ("tx-dropped", s.tx_dropped),
        ]
        .iter()
        .enumerate()
        {
            if i > 0 {
                o.raw(b", ");
            }
            o.raw(b"\"");
            o.str(k);
            o.raw(b"\": ");
            o.uint(*v);
        }
        o.raw(b"}");
    }
    if let Some(m) = f.mac {
        o.raw(b", \"hardware-address\": \"");
        for (i, b) in m.iter().enumerate() {
            if i > 0 {
                o.raw(b":");
            }
            o.raw(&[
                b"0123456789abcdef"[usize::from(b >> 4)],
                b"0123456789abcdef"[usize::from(b & 15)],
            ]);
        }
        o.raw(b"\"");
    }
    o.raw(b"}");
}

/// IPv4 dotted decimal; IPv6 in the canonical compressed form (RFC 5952):
/// lowercase hex, the longest run of two or more zero groups (the first if
/// equal) written as `::`.
fn ip_text(o: &mut Out<'_>, ip: &IpAddr) {
    match ip {
        IpAddr::V4(b) => {
            for (i, x) in b.iter().enumerate() {
                if i > 0 {
                    o.raw(b".");
                }
                o.uint(u64::from(*x));
            }
        }
        IpAddr::V6(g) => {
            let (mut best_at, mut best_len) = (0, 0);
            let mut i = 0;
            while i < 8 {
                if g[i] == 0 {
                    let start = i;
                    while i < 8 && g[i] == 0 {
                        i += 1;
                    }
                    if i - start > best_len {
                        best_at = start;
                        best_len = i - start;
                    }
                } else {
                    i += 1;
                }
            }
            if best_len < 2 {
                best_len = 0;
            }
            let mut i = 0;
            while i < 8 {
                if best_len > 0 && i == best_at {
                    o.raw(b"::");
                    i += best_len;
                    continue;
                }
                if i > 0 && !(best_len > 0 && i == best_at + best_len) {
                    o.raw(b":");
                }
                hex(o, g[i]);
                i += 1;
            }
        }
    }
}

fn hex(o: &mut Out<'_>, v: u16) {
    let mut started = false;
    for shift in [12, 8, 4, 0] {
        let d = (v >> shift & 15) as u8;
        if d != 0 || started || shift == 0 {
            started = true;
            o.raw(&[b"0123456789abcdef"[usize::from(d)]]);
        }
    }
}
