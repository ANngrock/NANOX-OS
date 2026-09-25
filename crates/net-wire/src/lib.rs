//! Allocation-free wire codecs for the M5 network path: Ethernet II, ARP for
//! IPv4 over Ethernet, IPv4, ICMP echo and UDP (docs/specs/M5-NETWORK.md).
//!
//! Every parser checks lengths before it reads a field. Variable-length data
//! (payloads, IPv4 options, ICMP and UDP data) is returned as slices of the
//! caller's buffer; fixed-size fields, including the whole `ArpPacket`, are
//! copied. Malformed input yields `WireError`, never a panic. Emitters write into a caller-supplied buffer and report the number of
//! bytes written. The crate holds no state: ARP caches, routing, sockets and
//! timers belong to the network service, which needs M1-M4 first.

#![no_std]
#![forbid(unsafe_code)]

pub mod dns;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    /// Input is shorter than the structure or its own length fields require.
    Truncated,
    /// Output buffer cannot hold the encoded structure.
    BufferTooSmall,
    /// An encoded length would exceed its 16-bit field.
    Overflow,
    BadVersion,
    BadHeaderLength,
    /// A length field is smaller than the header it must cover.
    BadLength,
    BadChecksum,
    /// The reserved IPv4 flag bit is set.
    ReservedFlag,
    /// IPv4 fragment (MF set or non-zero offset); M5-1 does not reassemble.
    Fragmented,
    /// 802.3 length frames, VLAN tags or other framing outside Ethernet II.
    UnsupportedFrame,
    /// ARP for anything other than Ethernet/IPv4, or an unknown operation.
    UnsupportedArp,
    /// ICMP other than echo request/reply with code 0.
    UnsupportedIcmp,
    /// Multicast or broadcast source address.
    BadAddress,
    /// UDP destination port 0.
    BadPort,
    /// A field value the emitter refuses to produce (for example TTL 0).
    InvalidField,
}

pub type Result<T> = core::result::Result<T, WireError>;

fn slice(bytes: &[u8], at: usize, len: usize) -> Result<&[u8]> {
    let end = at.checked_add(len).ok_or(WireError::Overflow)?;
    bytes.get(at..end).ok_or(WireError::Truncated)
}

fn be16(bytes: &[u8], at: usize) -> Result<u16> {
    let b = slice(bytes, at, 2)?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

fn array<const N: usize>(bytes: &[u8], at: usize) -> Result<[u8; N]> {
    let mut out = [0; N];
    out.copy_from_slice(slice(bytes, at, N)?);
    Ok(out)
}

fn output(out: &mut [u8], len: usize) -> Result<&mut [u8]> {
    out.get_mut(..len).ok_or(WireError::BufferTooSmall)
}

/// RFC 1071 ones'-complement sum over data that may arrive in several pieces
/// of any length, including odd ones.
#[derive(Clone, Copy, Debug, Default)]
pub struct Checksum {
    sum: u32,
    odd: Option<u8>,
}

impl Checksum {
    pub const fn new() -> Self {
        Self { sum: 0, odd: None }
    }

    pub fn add(&mut self, mut bytes: &[u8]) {
        if let Some(high) = self.odd.take() {
            match bytes.split_first() {
                Some((&low, rest)) => {
                    self.add_word(u16::from_be_bytes([high, low]));
                    bytes = rest;
                }
                None => {
                    self.odd = Some(high);
                    return;
                }
            }
        }
        let mut words = bytes.chunks_exact(2);
        for word in &mut words {
            self.add_word(u16::from_be_bytes([word[0], word[1]]));
        }
        if let [last] = words.remainder() {
            self.odd = Some(*last);
        }
    }

    fn add_word(&mut self, word: u16) {
        // Folding after every word keeps the sum within 16 bits plus carry.
        let sum = self.sum + word as u32;
        self.sum = (sum & 0xffff) + (sum >> 16);
    }

    /// The checksum field value; zero when the data already contains a valid
    /// checksum.
    pub fn finish(mut self) -> u16 {
        if let Some(high) = self.odd.take() {
            self.add_word(u16::from_be_bytes([high, 0]));
        }
        !(self.sum as u16)
    }
}

pub fn checksum(bytes: &[u8]) -> u16 {
    let mut sum = Checksum::new();
    sum.add(bytes);
    sum.finish()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MacAddr(pub [u8; 6]);

impl MacAddr {
    pub const BROADCAST: Self = Self([0xff; 6]);

    /// Group bit; broadcast is also a group address.
    pub fn is_multicast(&self) -> bool {
        self.0[0] & 1 != 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    pub const UNSPECIFIED: Self = Self([0; 4]);
    pub const BROADCAST: Self = Self([255; 4]);

    pub fn is_multicast(&self) -> bool {
        (224..=239).contains(&self.0[0])
    }

    /// Never valid as the source of a datagram.
    pub fn is_group(&self) -> bool {
        self.is_multicast() || *self == Self::BROADCAST
    }
}

pub const ETHERNET_HEADER_LEN: usize = 14;
pub const ETHERTYPE_IPV4: u16 = 0x0800;
pub const ETHERTYPE_ARP: u16 = 0x0806;
const ETHERTYPE_MIN: u16 = 0x0600;
const ETHERTYPE_VLAN: u16 = 0x8100;
const ETHERTYPE_QINQ: u16 = 0x88a8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EthernetFrame<'a> {
    pub dst: MacAddr,
    pub src: MacAddr,
    pub ethertype: u16,
    /// Everything after the header, including any minimum-size padding; the
    /// next layer bounds itself by its own length field.
    pub payload: &'a [u8],
}

fn check_ethertype(ethertype: u16) -> Result<()> {
    if ethertype < ETHERTYPE_MIN || ethertype == ETHERTYPE_VLAN || ethertype == ETHERTYPE_QINQ {
        return Err(WireError::UnsupportedFrame);
    }
    Ok(())
}

pub fn parse_ethernet(frame: &[u8]) -> Result<EthernetFrame<'_>> {
    if frame.len() < ETHERNET_HEADER_LEN {
        return Err(WireError::Truncated);
    }
    let ethertype = be16(frame, 12)?;
    check_ethertype(ethertype)?;
    let src = MacAddr(array(frame, 6)?);
    if src.is_multicast() {
        return Err(WireError::BadAddress);
    }
    Ok(EthernetFrame {
        dst: MacAddr(array(frame, 0)?),
        src,
        ethertype,
        payload: &frame[ETHERNET_HEADER_LEN..],
    })
}

/// Writes header and payload; frames are not padded to the 60-byte minimum.
pub fn emit_ethernet(
    out: &mut [u8],
    dst: MacAddr,
    src: MacAddr,
    ethertype: u16,
    payload: &[u8],
) -> Result<usize> {
    check_ethertype(ethertype)?;
    if src.is_multicast() {
        return Err(WireError::BadAddress);
    }
    let len = ETHERNET_HEADER_LEN
        .checked_add(payload.len())
        .ok_or(WireError::Overflow)?;
    let out = output(out, len)?;
    out[0..6].copy_from_slice(&dst.0);
    out[6..12].copy_from_slice(&src.0);
    out[12..14].copy_from_slice(&ethertype.to_be_bytes());
    out[ETHERNET_HEADER_LEN..].copy_from_slice(payload);
    Ok(len)
}

pub const ARP_LEN: usize = 28;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArpOp {
    Request,
    Reply,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArpPacket {
    pub op: ArpOp,
    pub sender_mac: MacAddr,
    pub sender_ip: Ipv4Addr,
    pub target_mac: MacAddr,
    pub target_ip: Ipv4Addr,
}

/// Ethernet/IPv4 ARP only; bytes after the 28-byte packet are frame padding.
pub fn parse_arp(packet: &[u8]) -> Result<ArpPacket> {
    if packet.len() < ARP_LEN {
        return Err(WireError::Truncated);
    }
    if be16(packet, 0)? != 1
        || be16(packet, 2)? != ETHERTYPE_IPV4
        || packet[4] != 6
        || packet[5] != 4
    {
        return Err(WireError::UnsupportedArp);
    }
    let op = match be16(packet, 6)? {
        1 => ArpOp::Request,
        2 => ArpOp::Reply,
        _ => return Err(WireError::UnsupportedArp),
    };
    let sender_mac = MacAddr(array(packet, 8)?);
    let sender_ip = Ipv4Addr(array(packet, 14)?);
    if sender_mac.is_multicast() || sender_ip.is_group() {
        return Err(WireError::BadAddress);
    }
    Ok(ArpPacket {
        op,
        sender_mac,
        sender_ip,
        target_mac: MacAddr(array(packet, 18)?),
        target_ip: Ipv4Addr(array(packet, 24)?),
    })
}

pub fn emit_arp(out: &mut [u8], packet: &ArpPacket) -> Result<usize> {
    if packet.sender_mac.is_multicast() || packet.sender_ip.is_group() {
        return Err(WireError::BadAddress);
    }
    let out = output(out, ARP_LEN)?;
    out[0..2].copy_from_slice(&1u16.to_be_bytes());
    out[2..4].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
    out[4] = 6;
    out[5] = 4;
    let op: u16 = match packet.op {
        ArpOp::Request => 1,
        ArpOp::Reply => 2,
    };
    out[6..8].copy_from_slice(&op.to_be_bytes());
    out[8..14].copy_from_slice(&packet.sender_mac.0);
    out[14..18].copy_from_slice(&packet.sender_ip.0);
    out[18..24].copy_from_slice(&packet.target_mac.0);
    out[24..28].copy_from_slice(&packet.target_ip.0);
    Ok(ARP_LEN)
}

pub const IPV4_MIN_HEADER_LEN: usize = 20;
pub const IPPROTO_ICMP: u8 = 1;
pub const IPPROTO_TCP: u8 = 6;
pub const IPPROTO_UDP: u8 = 17;
const IPV4_FLAG_RESERVED: u16 = 0x8000;
const IPV4_FLAG_DF: u16 = 0x4000;
const IPV4_FLAG_MF: u16 = 0x2000;
const IPV4_OFFSET_MASK: u16 = 0x1fff;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Packet<'a> {
    pub dscp_ecn: u8,
    pub ident: u16,
    pub dont_fragment: bool,
    pub ttl: u8,
    pub protocol: u8,
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    /// Raw options, length-checked but not interpreted (M5-NETWORK.md).
    pub options: &'a [u8],
    /// Exactly `total_length - header_length` bytes; link padding is dropped.
    pub payload: &'a [u8],
}

pub fn parse_ipv4(packet: &[u8]) -> Result<Ipv4Packet<'_>> {
    if packet.len() < IPV4_MIN_HEADER_LEN {
        return Err(WireError::Truncated);
    }
    if packet[0] >> 4 != 4 {
        return Err(WireError::BadVersion);
    }
    let header_len = (packet[0] & 0x0f) as usize * 4;
    if header_len < IPV4_MIN_HEADER_LEN {
        return Err(WireError::BadHeaderLength);
    }
    let header = slice(packet, 0, header_len)?;
    let total_len = be16(packet, 2)? as usize;
    if total_len < header_len {
        return Err(WireError::BadLength);
    }
    let datagram = slice(packet, 0, total_len)?;
    if checksum(header) != 0 {
        return Err(WireError::BadChecksum);
    }
    let flags = be16(header, 6)?;
    if flags & IPV4_FLAG_RESERVED != 0 {
        return Err(WireError::ReservedFlag);
    }
    if flags & IPV4_FLAG_MF != 0 || flags & IPV4_OFFSET_MASK != 0 {
        return Err(WireError::Fragmented);
    }
    let src = Ipv4Addr(array(header, 12)?);
    if src.is_group() {
        return Err(WireError::BadAddress);
    }
    Ok(Ipv4Packet {
        dscp_ecn: header[1],
        ident: be16(header, 4)?,
        dont_fragment: flags & IPV4_FLAG_DF != 0,
        ttl: header[8],
        protocol: header[9],
        src,
        dst: Ipv4Addr(array(header, 16)?),
        options: &header[IPV4_MIN_HEADER_LEN..],
        payload: &datagram[header_len..],
    })
}

/// Header fields for an emitted datagram: no options, DF set, never fragmented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ipv4Emit {
    pub src: Ipv4Addr,
    pub dst: Ipv4Addr,
    pub protocol: u8,
    pub ttl: u8,
    pub ident: u16,
}

pub fn emit_ipv4(out: &mut [u8], header: &Ipv4Emit, payload: &[u8]) -> Result<usize> {
    if header.ttl == 0 {
        return Err(WireError::InvalidField);
    }
    if header.src.is_group() {
        return Err(WireError::BadAddress);
    }
    let len = IPV4_MIN_HEADER_LEN
        .checked_add(payload.len())
        .filter(|len| *len <= u16::MAX as usize)
        .ok_or(WireError::Overflow)?;
    let out = output(out, len)?;
    out[0] = 0x45;
    out[1] = 0;
    out[2..4].copy_from_slice(&(len as u16).to_be_bytes());
    out[4..6].copy_from_slice(&header.ident.to_be_bytes());
    out[6..8].copy_from_slice(&IPV4_FLAG_DF.to_be_bytes());
    out[8] = header.ttl;
    out[9] = header.protocol;
    out[10..12].copy_from_slice(&[0, 0]);
    out[12..16].copy_from_slice(&header.src.0);
    out[16..20].copy_from_slice(&header.dst.0);
    let sum = checksum(&out[..IPV4_MIN_HEADER_LEN]);
    out[10..12].copy_from_slice(&sum.to_be_bytes());
    out[IPV4_MIN_HEADER_LEN..].copy_from_slice(payload);
    Ok(len)
}

pub const ICMP_ECHO_HEADER_LEN: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IcmpEcho<'a> {
    pub reply: bool,
    pub ident: u16,
    pub seq: u16,
    pub data: &'a [u8],
}

/// `message` is the IPv4 payload; the checksum covers all of it.
pub fn parse_icmp_echo(message: &[u8]) -> Result<IcmpEcho<'_>> {
    if message.len() < ICMP_ECHO_HEADER_LEN {
        return Err(WireError::Truncated);
    }
    if checksum(message) != 0 {
        return Err(WireError::BadChecksum);
    }
    let reply = match (message[0], message[1]) {
        (8, 0) => false,
        (0, 0) => true,
        _ => return Err(WireError::UnsupportedIcmp),
    };
    Ok(IcmpEcho {
        reply,
        ident: be16(message, 4)?,
        seq: be16(message, 6)?,
        data: &message[ICMP_ECHO_HEADER_LEN..],
    })
}

pub fn emit_icmp_echo(out: &mut [u8], echo: &IcmpEcho<'_>) -> Result<usize> {
    let len = ICMP_ECHO_HEADER_LEN
        .checked_add(echo.data.len())
        .ok_or(WireError::Overflow)?;
    let out = output(out, len)?;
    out[0] = if echo.reply { 0 } else { 8 };
    out[1] = 0;
    out[2..4].copy_from_slice(&[0, 0]);
    out[4..6].copy_from_slice(&echo.ident.to_be_bytes());
    out[6..8].copy_from_slice(&echo.seq.to_be_bytes());
    out[ICMP_ECHO_HEADER_LEN..].copy_from_slice(echo.data);
    let sum = checksum(out);
    out[2..4].copy_from_slice(&sum.to_be_bytes());
    Ok(len)
}

pub const UDP_HEADER_LEN: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UdpDatagram<'a> {
    pub src_port: u16,
    pub dst_port: u16,
    /// False when the sender transmitted checksum 0 (allowed over IPv4).
    pub checksum_present: bool,
    pub payload: &'a [u8],
}

fn udp_pseudo_header(src: Ipv4Addr, dst: Ipv4Addr, len: u16) -> Checksum {
    let mut sum = Checksum::new();
    sum.add(&src.0);
    sum.add(&dst.0);
    sum.add(&[0, IPPROTO_UDP]);
    sum.add(&len.to_be_bytes());
    sum
}

/// A computed UDP checksum of zero is transmitted as 0xffff (RFC 768), since
/// zero on the wire means "no checksum".
pub fn udp_checksum_field(computed: u16) -> u16 {
    if computed == 0 {
        0xffff
    } else {
        computed
    }
}

/// `segment` is the IPv4 payload; `src`/`dst` come from its IPv4 header.
pub fn parse_udp(src: Ipv4Addr, dst: Ipv4Addr, segment: &[u8]) -> Result<UdpDatagram<'_>> {
    if segment.len() < UDP_HEADER_LEN {
        return Err(WireError::Truncated);
    }
    let len = be16(segment, 4)?;
    if (len as usize) < UDP_HEADER_LEN {
        return Err(WireError::BadLength);
    }
    let datagram = slice(segment, 0, len as usize)?;
    let wire_sum = be16(datagram, 6)?;
    if wire_sum != 0 {
        let mut sum = udp_pseudo_header(src, dst, len);
        sum.add(datagram);
        if sum.finish() != 0 {
            return Err(WireError::BadChecksum);
        }
    }
    let dst_port = be16(datagram, 2)?;
    if dst_port == 0 {
        return Err(WireError::BadPort);
    }
    Ok(UdpDatagram {
        src_port: be16(datagram, 0)?,
        dst_port,
        checksum_present: wire_sum != 0,
        payload: &datagram[UDP_HEADER_LEN..],
    })
}

/// Always emits a checksum.
pub fn emit_udp(
    out: &mut [u8],
    src: Ipv4Addr,
    dst: Ipv4Addr,
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> Result<usize> {
    if dst_port == 0 {
        return Err(WireError::BadPort);
    }
    let len = UDP_HEADER_LEN
        .checked_add(payload.len())
        .filter(|len| *len <= u16::MAX as usize)
        .ok_or(WireError::Overflow)?;
    let out = output(out, len)?;
    out[0..2].copy_from_slice(&src_port.to_be_bytes());
    out[2..4].copy_from_slice(&dst_port.to_be_bytes());
    out[4..6].copy_from_slice(&(len as u16).to_be_bytes());
    out[6..8].copy_from_slice(&[0, 0]);
    out[UDP_HEADER_LEN..].copy_from_slice(payload);
    let mut sum = udp_pseudo_header(src, dst, len as u16);
    sum.add(out);
    out[6..8].copy_from_slice(&udp_checksum_field(sum.finish()).to_be_bytes());
    Ok(len)
}
