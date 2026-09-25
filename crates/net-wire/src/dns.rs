//! DNS stub codec for IPv4 address lookups over UDP (RFC 1035, RFC 2181).
//!
//! `emit_a_query` builds a recursive A query; `parse_a_response` validates a
//! reply against that query and returns only the A records of the Answer
//! section reachable from the queried name through a bounded CNAME chain.
//! Authority and Additional records are checked for well-formedness and
//! skipped. No allocation: record offsets live in a fixed table and names are
//! decoded into fixed 255-byte buffers.

use crate::Ipv4Addr;

pub const DNS_HEADER_LEN: usize = 12;
/// Wire length limit of a name, including length bytes and the root label.
pub const MAX_NAME_WIRE_LEN: usize = 255;
pub const MAX_LABEL_LEN: usize = 63;
/// Maximum number of CNAME records followed from the queried name.
pub const MAX_CNAME_CHAIN: usize = 8;
/// Upper bound for answer + authority + additional records in one message.
pub const MAX_RECORDS: usize = 64;
const MAX_POINTER_HOPS: usize = 32;

const TYPE_A: u16 = 1;
const TYPE_CNAME: u16 = 5;
const CLASS_IN: u16 = 1;
const FLAG_QR: u16 = 0x8000;
const FLAG_TC: u16 = 0x0200;
const FLAG_RD: u16 = 0x0100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DnsError {
    /// Message ends inside a field, name, record or RDATA.
    Truncated,
    BufferTooSmall,
    /// Empty label, non-LDH query name, or reserved label type on the wire.
    BadName,
    /// Name longer than 255 bytes in wire form, or a label over 63 bytes.
    NameTooLong,
    /// Compression pointer that does not point strictly backwards.
    BadPointer,
    IdMismatch,
    /// QR clear, non-zero opcode, or QDCOUNT other than one.
    BadHeader,
    TooManyRecords,
    QuestionMismatch,
    /// TC set: the answer did not fit into UDP; retry over TCP is needed.
    TruncatedResponse,
    NameError,
    ServerFailure,
    Refused,
    OtherRcode(u8),
    /// RDATA length or content inconsistent with the record type.
    BadRdata,
    CnameLoop,
    /// Two different CNAME targets, or CNAME and A records, for one owner.
    CnameConflict,
    CnameChainTooLong,
    /// More reachable A records than the caller's output slice holds.
    OutputFull,
}

pub type Result<T> = core::result::Result<T, DnsError>;

/// A domain name in wire form with ASCII letters folded to lower case, so
/// equality is the case-insensitive comparison of RFC 1035 section 2.3.3.
#[derive(Clone, Copy)]
pub struct DnsName {
    bytes: [u8; MAX_NAME_WIRE_LEN],
    len: usize,
}

impl PartialEq for DnsName {
    fn eq(&self, other: &Self) -> bool {
        self.wire() == other.wire()
    }
}

impl Eq for DnsName {}

impl core::fmt::Debug for DnsName {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "DnsName({:?})", self.wire())
    }
}

impl DnsName {
    const fn empty() -> Self {
        Self {
            bytes: [0; MAX_NAME_WIRE_LEN],
            len: 0,
        }
    }

    /// Wire form including the terminating zero-length root label.
    pub fn wire(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// Parses a query name such as `example.com` or `example.com.`. Labels
    /// must be letters, digits and inner hyphens (RFC 1123 host names).
    pub fn from_text(text: &str) -> Result<Self> {
        let text = text.strip_suffix('.').unwrap_or(text);
        if text.is_empty() {
            return Err(DnsError::BadName);
        }
        let mut name = Self::empty();
        for label in text.split('.') {
            let bytes = label.as_bytes();
            let ldh = bytes
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || *b == b'-');
            if bytes.is_empty()
                || !ldh
                || bytes.first() == Some(&b'-')
                || bytes.last() == Some(&b'-')
            {
                return Err(DnsError::BadName);
            }
            name.push_label(bytes)?;
        }
        name.push_root()?;
        Ok(name)
    }

    fn push_label(&mut self, label: &[u8]) -> Result<()> {
        if label.len() > MAX_LABEL_LEN {
            return Err(DnsError::NameTooLong);
        }
        // Reserve one byte for the root label that must still follow.
        let end = self.len + 1 + label.len();
        if end + 1 > MAX_NAME_WIRE_LEN {
            return Err(DnsError::NameTooLong);
        }
        self.bytes[self.len] = label.len() as u8;
        for (dst, src) in self.bytes[self.len + 1..end].iter_mut().zip(label) {
            *dst = src.to_ascii_lowercase();
        }
        self.len = end;
        Ok(())
    }

    fn push_root(&mut self) -> Result<()> {
        if self.len >= MAX_NAME_WIRE_LEN {
            return Err(DnsError::NameTooLong);
        }
        self.bytes[self.len] = 0;
        self.len += 1;
        Ok(())
    }
}

fn be16(msg: &[u8], at: usize) -> Result<u16> {
    let bytes = msg.get(at..at + 2).ok_or(DnsError::Truncated)?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn be32(msg: &[u8], at: usize) -> Result<u32> {
    let bytes = msg.get(at..at + 4).ok_or(DnsError::Truncated)?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Decodes the (possibly compressed) name at `start`; returns it and the
/// offset just after its encoding at `start`. Every pointer must target an
/// offset below the start of the label run that contains it, so positions
/// strictly decrease across hops and decoding always terminates.
fn read_name(msg: &[u8], start: usize) -> Result<(DnsName, usize)> {
    let mut name = DnsName::empty();
    let mut pos = start;
    let mut run_start = start;
    let mut next = None;
    let mut hops = 0;
    loop {
        let len_byte = *msg.get(pos).ok_or(DnsError::Truncated)?;
        match len_byte & 0xc0 {
            0x00 if len_byte == 0 => {
                name.push_root()?;
                return Ok((name, next.unwrap_or(pos + 1)));
            }
            0x00 => {
                let len = len_byte as usize;
                let label = msg.get(pos + 1..pos + 1 + len).ok_or(DnsError::Truncated)?;
                name.push_label(label)?;
                pos += 1 + len;
            }
            0xc0 => {
                let low = *msg.get(pos + 1).ok_or(DnsError::Truncated)?;
                let target = ((len_byte as usize & 0x3f) << 8) | low as usize;
                hops += 1;
                if target >= run_start || hops > MAX_POINTER_HOPS {
                    return Err(DnsError::BadPointer);
                }
                next.get_or_insert(pos + 2);
                pos = target;
                run_start = target;
            }
            // 0x40 and 0x80 are extended/obsolete label types (RFC 6891).
            _ => return Err(DnsError::BadName),
        }
    }
}

/// Writes a recursive A/IN query for `name`.
pub fn emit_a_query(out: &mut [u8], id: u16, name: &DnsName) -> Result<usize> {
    let len = DNS_HEADER_LEN + name.len + 4;
    let out = out.get_mut(..len).ok_or(DnsError::BufferTooSmall)?;
    out[0..2].copy_from_slice(&id.to_be_bytes());
    out[2..4].copy_from_slice(&FLAG_RD.to_be_bytes());
    out[4..6].copy_from_slice(&1u16.to_be_bytes());
    out[6..12].copy_from_slice(&[0; 6]);
    let question = &mut out[DNS_HEADER_LEN..];
    question[..name.len].copy_from_slice(name.wire());
    question[name.len..name.len + 2].copy_from_slice(&TYPE_A.to_be_bytes());
    question[name.len + 2..name.len + 4].copy_from_slice(&CLASS_IN.to_be_bytes());
    Ok(len)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AResult {
    /// Number of addresses written to the front of the output slice. Zero
    /// with RCODE 0 means the name exists without A records (NODATA).
    pub count: usize,
    /// Minimum TTL over every CNAME followed and every A record returned;
    /// zero when nothing was used. TTLs with the top bit set count as zero
    /// (RFC 2181 section 8).
    pub ttl: u32,
}

#[derive(Clone, Copy, Default)]
struct Record {
    owner_at: usize,
    rtype: u16,
    class: u16,
    ttl: u32,
    rdata_at: usize,
    rdata_len: usize,
}

fn read_record(msg: &[u8], at: usize) -> Result<(Record, usize)> {
    let (_, fixed_at) = read_name(msg, at)?;
    let rdata_len = be16(msg, fixed_at + 8)? as usize;
    let rdata_at = fixed_at + 10;
    let end = rdata_at + rdata_len;
    if end > msg.len() {
        return Err(DnsError::Truncated);
    }
    let ttl = be32(msg, fixed_at + 4)?;
    let record = Record {
        owner_at: at,
        rtype: be16(msg, fixed_at)?,
        class: be16(msg, fixed_at + 2)?,
        ttl: if ttl > i32::MAX as u32 { 0 } else { ttl },
        rdata_at,
        rdata_len,
    };
    Ok((record, end))
}

/// Checks RDATA of the IN types this codec understands, in every section.
fn validate_rdata(msg: &[u8], record: &Record) -> Result<()> {
    if record.class != CLASS_IN {
        return Ok(());
    }
    match record.rtype {
        TYPE_A if record.rdata_len != 4 => Err(DnsError::BadRdata),
        TYPE_CNAME => {
            let (_, end) = read_name(msg, record.rdata_at)?;
            if end != record.rdata_at + record.rdata_len {
                return Err(DnsError::BadRdata);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Validates `msg` as the reply to the query `id`/`name` and writes the A
/// records reachable from `name` into `out`.
pub fn parse_a_response(
    msg: &[u8],
    id: u16,
    name: &DnsName,
    out: &mut [Ipv4Addr],
) -> Result<AResult> {
    if msg.len() < DNS_HEADER_LEN {
        return Err(DnsError::Truncated);
    }
    if be16(msg, 0)? != id {
        return Err(DnsError::IdMismatch);
    }
    let flags = be16(msg, 2)?;
    let opcode = (flags >> 11) & 0x0f;
    if flags & FLAG_QR == 0 || opcode != 0 || be16(msg, 4)? != 1 {
        return Err(DnsError::BadHeader);
    }
    let answers = be16(msg, 6)? as usize;
    let others = be16(msg, 8)? as usize + be16(msg, 10)? as usize;
    if answers + others > MAX_RECORDS {
        return Err(DnsError::TooManyRecords);
    }

    let (question, fixed_at) = read_name(msg, DNS_HEADER_LEN)?;
    if question != *name || be16(msg, fixed_at)? != TYPE_A || be16(msg, fixed_at + 2)? != CLASS_IN {
        return Err(DnsError::QuestionMismatch);
    }
    if flags & FLAG_TC != 0 {
        return Err(DnsError::TruncatedResponse);
    }
    match (flags & 0x0f) as u8 {
        0 => {}
        2 => return Err(DnsError::ServerFailure),
        3 => return Err(DnsError::NameError),
        5 => return Err(DnsError::Refused),
        other => return Err(DnsError::OtherRcode(other)),
    }

    let mut table = [Record::default(); MAX_RECORDS];
    let mut pos = fixed_at + 4;
    for slot in table.iter_mut().take(answers) {
        let (record, next) = read_record(msg, pos)?;
        validate_rdata(msg, &record)?;
        *slot = record;
        pos = next;
    }
    // Authority and Additional get the same RDATA checks but never supply
    // results.
    for _ in 0..others {
        let (record, next) = read_record(msg, pos)?;
        validate_rdata(msg, &record)?;
        pos = next;
    }
    resolve(msg, &table[..answers], name, out)
}

fn owner(msg: &[u8], record: &Record) -> Result<DnsName> {
    Ok(read_name(msg, record.owner_at)?.0)
}

fn resolve(
    msg: &[u8],
    answers: &[Record],
    name: &DnsName,
    out: &mut [Ipv4Addr],
) -> Result<AResult> {
    let mut visited = [DnsName::empty(); MAX_CNAME_CHAIN + 1];
    visited[0] = *name;
    let mut chain = 0;
    let mut ttl = u32::MAX;
    loop {
        let current = visited[chain];
        let mut target: Option<(DnsName, u32)> = None;
        let mut has_a = false;
        for record in answers.iter().filter(|r| r.class == CLASS_IN) {
            if owner(msg, record)? != current {
                continue;
            }
            match record.rtype {
                TYPE_CNAME => {
                    let (next, _) = read_name(msg, record.rdata_at)?;
                    match target {
                        Some((seen, _)) if seen != next => return Err(DnsError::CnameConflict),
                        Some((seen, seen_ttl)) => target = Some((seen, seen_ttl.min(record.ttl))),
                        None => target = Some((next, record.ttl)),
                    }
                }
                TYPE_A => has_a = true,
                _ => {}
            }
        }
        match target {
            Some(_) if has_a => return Err(DnsError::CnameConflict),
            Some((next, cname_ttl)) => {
                if chain == MAX_CNAME_CHAIN {
                    return Err(DnsError::CnameChainTooLong);
                }
                if visited[..=chain].contains(&next) {
                    return Err(DnsError::CnameLoop);
                }
                ttl = ttl.min(cname_ttl);
                chain += 1;
                visited[chain] = next;
            }
            None => {
                // Count first so `out` is untouched when it is too small.
                let mut total = 0;
                for record in answers
                    .iter()
                    .filter(|r| r.class == CLASS_IN && r.rtype == TYPE_A)
                {
                    if owner(msg, record)? == current {
                        total += 1;
                    }
                }
                if total > out.len() {
                    return Err(DnsError::OutputFull);
                }
                let mut count = 0;
                for record in answers
                    .iter()
                    .filter(|r| r.class == CLASS_IN && r.rtype == TYPE_A)
                {
                    if owner(msg, record)? != current {
                        continue;
                    }
                    // RDATA length 4 was checked by `validate_rdata`.
                    let rdata = &msg[record.rdata_at..record.rdata_at + 4];
                    out[count] = Ipv4Addr([rdata[0], rdata[1], rdata[2], rdata[3]]);
                    ttl = ttl.min(record.ttl);
                    count += 1;
                }
                let ttl = if ttl == u32::MAX { 0 } else { ttl };
                return Ok(AResult { count, ttl });
            }
        }
    }
}
