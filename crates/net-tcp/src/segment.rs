//! TCP segment codec (RFC 9293 section 3.1) with the IPv4 pseudo-header
//! checksum. Options: EOL, NOP and MSS are understood; other well-formed
//! options are skipped.

use net_wire::{Checksum, Ipv4Addr, IPPROTO_TCP};

pub const TCP_HEADER_LEN: usize = 20;
pub const FIN: u8 = 0x01;
pub const SYN: u8 = 0x02;
pub const RST: u8 = 0x04;
pub const PSH: u8 = 0x08;
pub const ACK: u8 = 0x10;
pub const URG: u8 = 0x20;
const FLAG_MASK: u8 = 0x3f;
const OPT_EOL: u8 = 0;
const OPT_NOP: u8 = 1;
const OPT_MSS: u8 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegmentError {
    Truncated,
    BufferTooSmall,
    /// Segment length does not fit the 16-bit pseudo-header length.
    Overflow,
    /// Data offset below 5 words.
    BadHeaderLength,
    BadChecksum,
    /// Option with a bad length, running past the header, or MSS of zero.
    BadOption,
    /// SYN combined with FIN or RST.
    BadFlags,
    /// Source or destination port 0.
    BadPort,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TcpHeader {
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    /// FIN, SYN, RST, PSH, ACK, URG bits; ECN bits are not reported.
    pub flags: u8,
    pub window: u16,
    /// MSS option; parsed only on SYN segments, emitted when present.
    pub mss: Option<u16>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TcpSegment<'a> {
    pub header: TcpHeader,
    pub payload: &'a [u8],
}

impl TcpSegment<'_> {
    pub fn has(&self, flag: u8) -> bool {
        self.header.flags & flag != 0
    }

    /// Sequence space occupied: payload plus one each for SYN and FIN.
    pub fn seq_len(&self) -> u32 {
        self.payload.len() as u32 + self.has(SYN) as u32 + self.has(FIN) as u32
    }
}

fn pseudo_header(src: Ipv4Addr, dst: Ipv4Addr, len: u16) -> Checksum {
    let mut sum = Checksum::new();
    sum.add(&src.0);
    sum.add(&dst.0);
    sum.add(&[0, IPPROTO_TCP]);
    sum.add(&len.to_be_bytes());
    sum
}

fn be16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn parse_options(options: &[u8], syn: bool) -> Result<Option<u16>, SegmentError> {
    let mut mss = None;
    let mut at = 0;
    while at < options.len() {
        match options[at] {
            OPT_EOL => break,
            OPT_NOP => at += 1,
            kind => {
                let len = *options.get(at + 1).ok_or(SegmentError::BadOption)? as usize;
                if len < 2 || at + len > options.len() {
                    return Err(SegmentError::BadOption);
                }
                if kind == OPT_MSS {
                    if len != 4 {
                        return Err(SegmentError::BadOption);
                    }
                    let value = be16(options, at + 2);
                    if value == 0 {
                        return Err(SegmentError::BadOption);
                    }
                    // MSS is meaningful only on SYN (RFC 9293 section 3.7.1).
                    if syn {
                        mss = Some(value);
                    }
                }
                at += len;
            }
        }
    }
    Ok(mss)
}

/// `segment` is the IPv4 payload; `src`/`dst` come from its IPv4 header.
pub fn parse_tcp(
    src: Ipv4Addr,
    dst: Ipv4Addr,
    segment: &[u8],
) -> Result<TcpSegment<'_>, SegmentError> {
    if segment.len() < TCP_HEADER_LEN {
        return Err(SegmentError::Truncated);
    }
    let len = u16::try_from(segment.len()).map_err(|_| SegmentError::Overflow)?;
    let header_len = (segment[12] >> 4) as usize * 4;
    if header_len < TCP_HEADER_LEN {
        return Err(SegmentError::BadHeaderLength);
    }
    if header_len > segment.len() {
        return Err(SegmentError::Truncated);
    }
    let mut sum = pseudo_header(src, dst, len);
    sum.add(segment);
    if sum.finish() != 0 {
        return Err(SegmentError::BadChecksum);
    }
    let flags = segment[13] & FLAG_MASK;
    if flags & SYN != 0 && flags & (FIN | RST) != 0 {
        return Err(SegmentError::BadFlags);
    }
    let src_port = be16(segment, 0);
    let dst_port = be16(segment, 2);
    if src_port == 0 || dst_port == 0 {
        return Err(SegmentError::BadPort);
    }
    let mss = parse_options(&segment[TCP_HEADER_LEN..header_len], flags & SYN != 0)?;
    Ok(TcpSegment {
        header: TcpHeader {
            src_port,
            dst_port,
            seq: be32(segment, 4),
            ack: be32(segment, 8),
            flags,
            window: be16(segment, 14),
            mss,
        },
        payload: &segment[header_len..],
    })
}

/// Length `emit_tcp` needs for `header` and `payload_len` bytes of data.
pub fn encoded_len(header: &TcpHeader, payload_len: usize) -> usize {
    TCP_HEADER_LEN + if header.mss.is_some() { 4 } else { 0 } + payload_len
}

pub fn emit_tcp(
    out: &mut [u8],
    src: Ipv4Addr,
    dst: Ipv4Addr,
    header: &TcpHeader,
    payload: &[u8],
) -> Result<usize, SegmentError> {
    let header_len = encoded_len(header, 0);
    check_emit(out, header, payload.len())?;
    out[header_len..header_len + payload.len()].copy_from_slice(payload);
    emit_in_place(out, src, dst, header, payload.len())
}

fn check_emit(out: &[u8], header: &TcpHeader, payload_len: usize) -> Result<usize, SegmentError> {
    if header.src_port == 0 || header.dst_port == 0 {
        return Err(SegmentError::BadPort);
    }
    if header.flags & SYN != 0 && header.flags & (FIN | RST) != 0 {
        return Err(SegmentError::BadFlags);
    }
    if header.mss == Some(0) {
        return Err(SegmentError::BadOption);
    }
    let len = encoded_len(header, payload_len);
    u16::try_from(len).map_err(|_| SegmentError::Overflow)?;
    if out.len() < len {
        return Err(SegmentError::BufferTooSmall);
    }
    Ok(len)
}

/// Like `emit_tcp`, but the `payload_len` payload bytes are already in place
/// right after the header, so large payloads need no intermediate copy.
pub(crate) fn emit_in_place(
    out: &mut [u8],
    src: Ipv4Addr,
    dst: Ipv4Addr,
    header: &TcpHeader,
    payload_len: usize,
) -> Result<usize, SegmentError> {
    let len = check_emit(out, header, payload_len)?;
    let header_len = encoded_len(header, 0);
    let out = &mut out[..len];
    out[0..2].copy_from_slice(&header.src_port.to_be_bytes());
    out[2..4].copy_from_slice(&header.dst_port.to_be_bytes());
    out[4..8].copy_from_slice(&header.seq.to_be_bytes());
    out[8..12].copy_from_slice(&header.ack.to_be_bytes());
    out[12] = ((header_len / 4) as u8) << 4;
    out[13] = header.flags & FLAG_MASK;
    out[14..16].copy_from_slice(&header.window.to_be_bytes());
    out[16..20].copy_from_slice(&[0; 4]);
    if let Some(mss) = header.mss {
        out[20] = OPT_MSS;
        out[21] = 4;
        out[22..24].copy_from_slice(&mss.to_be_bytes());
    }
    let mut sum = pseudo_header(src, dst, len as u16);
    sum.add(out);
    out[16..18].copy_from_slice(&sum.finish().to_be_bytes());
    Ok(len)
}
