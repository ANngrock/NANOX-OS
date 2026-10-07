//! A host address on the switch, as the host's own IPv4 address on a Proxmox
//! bridge: it answers ARP requests for that address and ICMP echo requests
//! sent to it, and ignores every other frame. Untagged Ethernet II frames
//! only; the answer is written into the caller's buffer, nothing is kept.
//!
//! What is checked before answering: the destination MAC (broadcast or ours
//! for ARP, ours for IPv4), an ARP request for Ethernet and IPv4 asking for
//! our address, an IPv4 header that is complete, has a valid checksum, is not
//! a fragment and is addressed to us, and an ICMP echo request (code 0) with a
//! valid checksum. The echo reply carries the request's identifier, sequence
//! number and data; its IPv4 header has no options, TTL 64.

pub const ETHERTYPE_ARP: u16 = 0x0806;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const BROADCAST: [u8; 6] = [0xFF; 6];
/// Ethernet header, then an ARP packet for Ethernet and IPv4.
pub const ARP_FRAME: usize = 14 + 28;
pub const TTL: u8 = 64;
const ETH: usize = 14;
const IP_PROTO_ICMP: u8 = 1;
const ICMP_ECHO_REPLY: u8 = 0;
const ICMP_ECHO_REQUEST: u8 = 8;

#[derive(Clone, Debug)]
pub struct Endpoint {
    pub mac: [u8; 6],
    pub ip: [u8; 4],
    pub arp_replies: u64,
    pub echo_replies: u64,
    /// Frames that got no answer.
    pub ignored: u64,
}

impl Endpoint {
    pub fn new(mac: [u8; 6], ip: [u8; 4]) -> Self {
        Self {
            mac,
            ip,
            arp_replies: 0,
            echo_replies: 0,
            ignored: 0,
        }
    }

    /// The answer to `frame` in `out`: its length, or None (nothing to answer,
    /// or `out` is too small for the answer).
    pub fn answer(&mut self, frame: &[u8], out: &mut [u8]) -> Option<usize> {
        let r = match frame.get(12..14).map(|t| u16::from_be_bytes([t[0], t[1]])) {
            Some(ETHERTYPE_ARP) => self.arp(frame, out),
            Some(ETHERTYPE_IPV4) => self.echo(frame, out),
            _ => None,
        };
        if r.is_none() {
            self.ignored += 1;
        }
        r
    }

    fn arp(&mut self, frame: &[u8], out: &mut [u8]) -> Option<usize> {
        let a = frame.get(ETH..ARP_FRAME)?;
        let to_us = frame[..6] == BROADCAST || frame[..6] == self.mac;
        // Ethernet, IPv4, 6- and 4-byte addresses, a request, for our address.
        if !to_us || a[..8] != [0, 1, 8, 0, 6, 4, 0, 1] || a[24..28] != self.ip {
            return None;
        }
        let out = out.get_mut(..ARP_FRAME)?;
        let (sha, spa) = (&a[8..14], &a[14..18]);
        out[..6].copy_from_slice(sha);
        out[6..12].copy_from_slice(&self.mac);
        out[12..14].copy_from_slice(&ETHERTYPE_ARP.to_be_bytes());
        let p = &mut out[ETH..];
        p[..8].copy_from_slice(&[0, 1, 8, 0, 6, 4, 0, 2]);
        p[8..14].copy_from_slice(&self.mac);
        p[14..18].copy_from_slice(&self.ip);
        p[18..24].copy_from_slice(sha);
        p[24..28].copy_from_slice(spa);
        self.arp_replies += 1;
        Some(ARP_FRAME)
    }

    fn echo(&mut self, frame: &[u8], out: &mut [u8]) -> Option<usize> {
        if frame[..6] != self.mac {
            return None;
        }
        let ip = &frame[ETH..];
        let ihl = usize::from(*ip.first()? & 0x0F) * 4;
        let total = usize::from(u16::from_be_bytes([*ip.get(2)?, *ip.get(3)?]));
        let fragment = u16::from_be_bytes([*ip.get(6)?, *ip.get(7)?]) & 0x3FFF;
        if ip[0] >> 4 != 4
            || ihl < 20
            || total < ihl + 8
            || total > ip.len()
            || checksum(&ip[..ihl]) != 0
            || fragment != 0
            || ip[9] != IP_PROTO_ICMP
            || ip[16..20] != self.ip
        {
            return None;
        }
        let icmp = &ip[ihl..total];
        if icmp[0] != ICMP_ECHO_REQUEST || icmp[1] != 0 || checksum(icmp) != 0 {
            return None;
        }
        let len = ETH + 20 + icmp.len();
        let out = out.get_mut(..len)?;
        out[..6].copy_from_slice(&frame[6..12]);
        out[6..12].copy_from_slice(&self.mac);
        out[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        let (h, r) = out[ETH..].split_at_mut(20);
        h.fill(0);
        h[0] = 0x45;
        h[2..4].copy_from_slice(&((20 + icmp.len()) as u16).to_be_bytes());
        h[8] = TTL;
        h[9] = IP_PROTO_ICMP;
        h[12..16].copy_from_slice(&self.ip);
        h[16..20].copy_from_slice(&ip[12..16]);
        let sum = checksum(h);
        h[10..12].copy_from_slice(&sum.to_be_bytes());
        r.copy_from_slice(icmp);
        r[0] = ICMP_ECHO_REPLY;
        r[2..4].fill(0);
        let sum = checksum(r);
        r[2..4].copy_from_slice(&sum.to_be_bytes());
        self.echo_replies += 1;
        Some(len)
    }
}

/// The Internet checksum (RFC 1071) of `data`: zero over data that carries a
/// correct one.
pub fn checksum(data: &[u8]) -> u16 {
    let mut sum = 0u64;
    for pair in data.chunks(2) {
        let word = u16::from_be_bytes([pair[0], pair.get(1).copied().unwrap_or(0)]);
        sum += u64::from(word);
    }
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}
