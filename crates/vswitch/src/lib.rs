//! Virtual L2 switch for the services NANOX hosts (docs/specs/M11-SERVER.md
//! §5): what a Linux bridge (`vmbr0`) is to Proxmox guests, between service
//! ports and an uplink.
//!
//! Per frame the switch decides, on a caller-supplied millisecond clock:
//! learning (VLAN, source MAC) → port with aging; forwarding of known
//! unicast to one port and flooding of unknown unicast, broadcast and
//! multicast to the others; 802.1Q VLANs with access and trunk ports (the
//! frame is rewritten by [`emit`]); isolated ports that only talk to
//! non-isolated ones; a per-port limit on learned MACs; damping of a MAC
//! that moves between ports too fast (a loop or a spoofer); and dropping of
//! runts, oversized frames, multicast sources and the reserved link-local
//! group (STP, LLDP, pause) which a bridge must not forward.
//!
//! There is no spanning tree: a loop in the wiring is not prevented, only
//! limited by the flap damping, and this is not claimed to be loop-free.
//! `no_std`, no allocation: the table is a fixed hash table with bounded
//! probing and eviction of the stalest entry, so a MAC flood costs the
//! attacker's own traffic its learning but never memory or time.

#![no_std]
#![forbid(unsafe_code)]

pub mod endpoint;

pub const MAX_PORTS: usize = 16;
pub const MIN_FRAME: usize = 14;
pub const VLAN_TAG: usize = 4;
const PROBE: usize = 4;
const TPID: u16 = 0x8100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// Shorter than an Ethernet header, or a tag cut short.
    Runt,
    Oversized,
    /// Source address is a multicast/broadcast address.
    BadSource,
    /// Destination in 01-80-C2-00-00-0x: consumed, never forwarded.
    Reserved,
    /// A tagged frame on an access port.
    TaggedOnAccess,
    /// Untagged frame on a trunk without a native VLAN.
    UntaggedOnTrunk,
    /// VLAN not allowed on the ingress port.
    VlanNotAllowed,
    PortDown,
    /// The port already knows its maximum of MAC addresses.
    PortSecurity,
    /// The MAC moved between ports faster than `flap_hold_ms` allows.
    MacFlap,
    /// Nowhere to send it (everything filtered).
    NoEgress,
}

const REASONS: usize = 11;

impl Reason {
    fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PortKind {
    /// Untagged member of one VLAN.
    Access(u16),
    /// Tagged member of the allowed VLANs; `native` travels untagged.
    Trunk { native: Option<u16> },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Port {
    pub kind: PortKind,
    pub up: bool,
    /// Isolated ports do not exchange frames with each other.
    pub isolated: bool,
    /// Learned MACs allowed on this port (0: unlimited).
    pub max_macs: u16,
}

impl Port {
    pub const fn access(vid: u16) -> Self {
        Self {
            kind: PortKind::Access(vid),
            up: true,
            isolated: false,
            max_macs: 0,
        }
    }

    pub const fn trunk(native: Option<u16>) -> Self {
        Self {
            kind: PortKind::Trunk { native },
            up: true,
            isolated: false,
            max_macs: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigError {
    NoSuchPort,
    /// VLAN ids are 1..=4094.
    Vlan,
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Learned entries not refreshed for this long are forgotten.
    pub age_ms: u64,
    /// A MAC may change port at most this often.
    pub flap_hold_ms: u64,
    /// Longest accepted frame, tag included.
    pub max_frame: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            age_ms: 300_000,
            flap_hold_ms: 1_000,
            max_frame: 1_522,
        }
    }
}

/// Where a frame goes: VLAN, output ports and which of them get a tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub vid: u16,
    pub ports: u16,
    pub tagged: u16,
}

impl Decision {
    pub fn to(&self, port: usize) -> bool {
        self.ports >> port & 1 != 0
    }

    pub fn tagged_on(&self, port: usize) -> bool {
        self.tagged >> port & 1 != 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PortStats {
    pub rx_frames: u64,
    pub rx_bytes: u64,
    pub tx_frames: u64,
    pub tx_bytes: u64,
    pub drops: [u64; REASONS],
}

#[derive(Clone, Copy)]
struct Entry {
    valid: bool,
    mac: [u8; 6],
    vid: u16,
    port: u8,
    seen: u64,
    moved: u64,
}

const EMPTY: Entry = Entry {
    valid: false,
    mac: [0; 6],
    vid: 0,
    port: 0,
    seen: 0,
    moved: 0,
};

/// Bit set over VLAN ids 0..4096.
#[derive(Clone, Copy)]
struct VlanSet([u64; 64]);

impl VlanSet {
    fn has(&self, v: u16) -> bool {
        self.0[usize::from(v >> 6)] >> (v & 63) & 1 != 0
    }
    fn set(&mut self, v: u16, on: bool) {
        let w = &mut self.0[usize::from(v >> 6)];
        if on {
            *w |= 1 << (v & 63);
        } else {
            *w &= !(1 << (v & 63));
        }
    }
}

pub struct Switch<const C: usize = 256> {
    cfg: Config,
    ports: [Port; MAX_PORTS],
    allowed: [VlanSet; MAX_PORTS],
    macs: [u16; MAX_PORTS],
    stats: [PortStats; MAX_PORTS],
    table: [Entry; C],
    count: usize,
}

fn hash(vid: u16, mac: &[u8; 6]) -> usize {
    let mut h: u32 = 0x811C_9DC5;
    for b in vid.to_le_bytes().iter().chain(mac) {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h as usize
}

impl<const C: usize> Switch<C> {
    /// All ports down and unconfigured until [`Switch::set_port`].
    pub fn new(cfg: Config) -> Self {
        assert!(C >= PROBE, "table too small");
        let down = Port {
            up: false,
            ..Port::access(1)
        };
        Self {
            cfg,
            ports: [down; MAX_PORTS],
            allowed: [VlanSet([0; 64]); MAX_PORTS],
            macs: [0; MAX_PORTS],
            stats: [PortStats::default(); MAX_PORTS],
            table: [EMPTY; C],
            count: 0,
        }
    }

    pub fn set_port(&mut self, p: usize, port: Port) -> Result<(), ConfigError> {
        if p >= MAX_PORTS {
            return Err(ConfigError::NoSuchPort);
        }
        let check = |v: u16| {
            if (1..=4094).contains(&v) {
                Ok(())
            } else {
                Err(ConfigError::Vlan)
            }
        };
        match port.kind {
            PortKind::Access(v) => check(v)?,
            PortKind::Trunk { native: Some(v) } => check(v)?,
            PortKind::Trunk { native: None } => {}
        }
        self.ports[p] = port;
        // A reconfigured port forgets what it learned and starts with only
        // its own VLAN allowed (trunks add more with `allow_vlan`).
        self.flush_port(p);
        self.allowed[p] = VlanSet([0; 64]);
        match port.kind {
            PortKind::Access(v) | PortKind::Trunk { native: Some(v) } => {
                self.allowed[p].set(v, true)
            }
            PortKind::Trunk { native: None } => {}
        }
        Ok(())
    }

    /// Allows or forbids a VLAN on a trunk port.
    pub fn allow_vlan(&mut self, p: usize, vid: u16, on: bool) -> Result<(), ConfigError> {
        if p >= MAX_PORTS {
            return Err(ConfigError::NoSuchPort);
        }
        if !(1..=4094).contains(&vid) {
            return Err(ConfigError::Vlan);
        }
        self.allowed[p].set(vid, on);
        Ok(())
    }

    pub fn flush_port(&mut self, p: usize) {
        for e in self.table.iter_mut() {
            if e.valid && usize::from(e.port) == p {
                e.valid = false;
                self.count -= 1;
            }
        }
        self.macs[p] = 0;
    }

    pub fn learned(&self) -> usize {
        self.count
    }

    /// Consistency of the table and the per-port counters (for tests).
    pub fn check(&self) -> Result<(), &'static str> {
        let valid = self.table.iter().filter(|e| e.valid).count();
        if valid != self.count {
            return Err("entry count out of step with the table");
        }
        for p in 0..MAX_PORTS {
            let n = self
                .table
                .iter()
                .filter(|e| e.valid && usize::from(e.port) == p)
                .count();
            if n != usize::from(self.macs[p]) {
                return Err("per-port MAC count out of step with the table");
            }
        }
        Ok(())
    }

    pub fn stats(&self, p: usize) -> &PortStats {
        &self.stats[p]
    }

    /// The port a MAC is learned on in a VLAN (None: unknown or aged out).
    pub fn lookup(&self, now: u64, vid: u16, mac: [u8; 6]) -> Option<usize> {
        let base = hash(vid, &mac);
        (0..PROBE.min(C)).find_map(|k| {
            let e = &self.table[(base + k) % C];
            (e.valid && e.vid == vid && e.mac == mac && !self.expired(e, now))
                .then_some(usize::from(e.port))
        })
    }

    fn expired(&self, e: &Entry, now: u64) -> bool {
        now.saturating_sub(e.seen) >= self.cfg.age_ms
    }

    fn drop_frame(&mut self, port: usize, why: Reason) -> Reason {
        self.stats[port].drops[why.index()] += 1;
        why
    }

    /// Classifies a frame arriving on `in_port` and decides where it goes.
    pub fn receive(&mut self, now: u64, in_port: usize, frame: &[u8]) -> Result<Decision, Reason> {
        if in_port >= MAX_PORTS {
            return Err(Reason::PortDown);
        }
        self.stats[in_port].rx_frames += 1;
        self.stats[in_port].rx_bytes += frame.len() as u64;
        let port = self.ports[in_port];
        if !port.up {
            return Err(self.drop_frame(in_port, Reason::PortDown));
        }
        if frame.len() < MIN_FRAME {
            return Err(self.drop_frame(in_port, Reason::Runt));
        }
        if frame.len() > self.cfg.max_frame {
            return Err(self.drop_frame(in_port, Reason::Oversized));
        }
        let dst: [u8; 6] = frame[0..6].try_into().unwrap_or([0; 6]);
        let src: [u8; 6] = frame[6..12].try_into().unwrap_or([0; 6]);
        if src[0] & 1 != 0 {
            return Err(self.drop_frame(in_port, Reason::BadSource));
        }
        if dst[..5] == [0x01, 0x80, 0xC2, 0x00, 0x00] && dst[5] < 0x10 {
            return Err(self.drop_frame(in_port, Reason::Reserved));
        }
        // VLAN classification.
        let tagged = u16::from_be_bytes([frame[12], frame[13]]) == TPID;
        let tag_vid = if tagged {
            if frame.len() < MIN_FRAME + VLAN_TAG {
                return Err(self.drop_frame(in_port, Reason::Runt));
            }
            u16::from_be_bytes([frame[14], frame[15]]) & 0x0FFF
        } else {
            0
        };
        let vid = match port.kind {
            PortKind::Access(v) => {
                if tagged && tag_vid != 0 {
                    return Err(self.drop_frame(in_port, Reason::TaggedOnAccess));
                }
                v
            }
            PortKind::Trunk { native } => {
                let v = if tagged && tag_vid != 0 {
                    tag_vid
                } else {
                    match native {
                        Some(n) => n,
                        None => return Err(self.drop_frame(in_port, Reason::UntaggedOnTrunk)),
                    }
                };
                if !self.allowed[in_port].has(v) {
                    return Err(self.drop_frame(in_port, Reason::VlanNotAllowed));
                }
                v
            }
        };
        if let Err(why) = self.learn(now, vid, src, in_port) {
            return Err(self.drop_frame(in_port, why));
        }
        // Where to send it.
        let mut mask: u16 = 0;
        let multicast = dst[0] & 1 != 0;
        let known = if multicast {
            None
        } else {
            self.lookup(now, vid, dst)
        };
        match known {
            Some(p) => mask = 1 << p,
            None => {
                for p in 0..MAX_PORTS {
                    mask |= 1 << p;
                }
            }
        }
        let mut tagged_mask = 0u16;
        let mut out = 0u16;
        for p in 0..MAX_PORTS {
            if mask >> p & 1 == 0 || p == in_port {
                continue;
            }
            let q = self.ports[p];
            if !q.up || (q.isolated && port.isolated) || !self.carries(p, vid) {
                continue;
            }
            out |= 1 << p;
            if let PortKind::Trunk { native } = q.kind {
                if native != Some(vid) {
                    tagged_mask |= 1 << p;
                }
            }
        }
        if out == 0 {
            return Err(self.drop_frame(in_port, Reason::NoEgress));
        }
        for p in 0..MAX_PORTS {
            if out >> p & 1 != 0 {
                self.stats[p].tx_frames += 1;
                self.stats[p].tx_bytes += frame.len() as u64;
            }
        }
        Ok(Decision {
            vid,
            ports: out,
            tagged: tagged_mask,
        })
    }

    fn carries(&self, p: usize, vid: u16) -> bool {
        match self.ports[p].kind {
            PortKind::Access(v) => v == vid,
            PortKind::Trunk { .. } => self.allowed[p].has(vid),
        }
    }

    fn learn(&mut self, now: u64, vid: u16, mac: [u8; 6], port: usize) -> Result<(), Reason> {
        let base = hash(vid, &mac);
        let window = PROBE.min(C);
        // Same address already here?
        for k in 0..window {
            let i = (base + k) % C;
            let e = self.table[i];
            if e.valid && e.vid == vid && e.mac == mac {
                if usize::from(e.port) == port {
                    self.table[i].seen = now;
                    return Ok(());
                }
                // Moved. Damp fast moves; an aged entry may be replaced.
                if !self.expired(&e, now) && now.saturating_sub(e.moved) < self.cfg.flap_hold_ms {
                    return Err(Reason::MacFlap);
                }
                let limit = self.ports[port].max_macs;
                if limit != 0 && self.macs[port] >= limit {
                    return Err(Reason::PortSecurity);
                }
                self.macs[usize::from(e.port)] -= 1;
                self.macs[port] += 1;
                self.table[i].port = port as u8;
                self.table[i].seen = now;
                self.table[i].moved = now;
                return Ok(());
            }
        }
        let limit = self.ports[port].max_macs;
        if limit != 0 && self.macs[port] >= limit {
            return Err(Reason::PortSecurity);
        }
        // A free or aged slot, else evict the stalest in the window.
        let mut slot = None;
        let mut stalest = (u64::MAX, 0);
        for k in 0..window {
            let i = (base + k) % C;
            let e = self.table[i];
            if !e.valid || self.expired(&e, now) {
                slot = Some(i);
                break;
            }
            if e.seen < stalest.0 {
                stalest = (e.seen, i);
            }
        }
        let i = slot.unwrap_or(stalest.1);
        if self.table[i].valid {
            self.macs[usize::from(self.table[i].port)] -= 1;
        } else {
            self.count += 1;
        }
        self.table[i] = Entry {
            valid: true,
            mac,
            vid,
            port: port as u8,
            seen: now,
            moved: now,
        };
        self.macs[port] += 1;
        Ok(())
    }
}

/// Writes the frame as it leaves a port: `tag` is the VLAN to tag it with
/// (None: untagged). Returns the length, or None if `out` is too small or
/// the frame is malformed.
pub fn emit(frame: &[u8], tag: Option<u16>, out: &mut [u8]) -> Option<usize> {
    if frame.len() < MIN_FRAME {
        return None;
    }
    let tagged = u16::from_be_bytes([frame[12], frame[13]]) == TPID;
    // Payload after the addresses and the (optional) tag.
    let (ethertype_at, rest_at) = if tagged {
        if frame.len() < MIN_FRAME + VLAN_TAG {
            return None;
        }
        (16, 18)
    } else {
        (12, 14)
    };
    let pcp_dei = if tagged { frame[14] & 0xF0 } else { 0 };
    let need = 12 + if tag.is_some() { 4 } else { 0 } + 2 + (frame.len() - rest_at);
    let o = out.get_mut(..need)?;
    o[..12].copy_from_slice(&frame[..12]);
    let mut n = 12;
    if let Some(v) = tag {
        o[n..n + 2].copy_from_slice(&TPID.to_be_bytes());
        o[n + 2] = pcp_dei | (v >> 8) as u8 & 0x0F;
        o[n + 3] = v as u8;
        n += 4;
    }
    o[n..n + 2].copy_from_slice(&frame[ethertype_at..ethertype_at + 2]);
    n += 2;
    o[n..].copy_from_slice(&frame[rest_at..]);
    Some(need)
}
