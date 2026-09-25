use net_tcp::segment::{ACK, FIN, PSH, RST, SYN};
use net_tcp::{emit_tcp, parse_tcp, seq, SegmentError, TcpHeader};
use net_wire::{Checksum, Ipv4Addr, IPPROTO_TCP};

const A: Ipv4Addr = Ipv4Addr([10, 0, 2, 15]);
const B: Ipv4Addr = Ipv4Addr([10, 0, 2, 2]);

fn header(flags: u8) -> TcpHeader {
    TcpHeader {
        src_port: 40000,
        dst_port: 443,
        seq: 0x0102_0304,
        ack: 0x0a0b_0c0d,
        flags,
        window: 4096,
        mss: None,
    }
}

fn emit(h: &TcpHeader, payload: &[u8]) -> Vec<u8> {
    let mut buf = vec![0u8; 128 + payload.len()];
    let len = emit_tcp(&mut buf, A, B, h, payload).unwrap();
    buf.truncate(len);
    buf
}

/// Recomputes the checksum after a deliberate header edit, so the test
/// reaches the check it targets.
fn fix_checksum(bytes: &mut [u8]) {
    bytes[16] = 0;
    bytes[17] = 0;
    let mut sum = Checksum::new();
    sum.add(&A.0);
    sum.add(&B.0);
    sum.add(&[0, IPPROTO_TCP]);
    sum.add(&(bytes.len() as u16).to_be_bytes());
    sum.add(bytes);
    let value = sum.finish();
    bytes[16..18].copy_from_slice(&value.to_be_bytes());
}

#[test]
fn round_trip_with_and_without_mss() {
    let syn = TcpHeader {
        mss: Some(1460),
        ..header(SYN)
    };
    let bytes = emit(&syn, &[]);
    assert_eq!(bytes.len(), 24);
    assert_eq!(bytes[12] >> 4, 6);
    let parsed = parse_tcp(A, B, &bytes).unwrap();
    assert_eq!(parsed.header, syn);
    assert!(parsed.payload.is_empty());
    assert_eq!(parsed.seq_len(), 1);

    let data = header(ACK | PSH | FIN);
    let bytes = emit(&data, b"odd");
    let parsed = parse_tcp(A, B, &bytes).unwrap();
    assert_eq!((parsed.header, parsed.payload), (data, &b"odd"[..]));
    assert_eq!(parsed.seq_len(), 4);
}

#[test]
fn checksum_covers_pseudo_header_and_payload() {
    let bytes = emit(&header(ACK), b"payload");
    assert_eq!(
        parse_tcp(A, Ipv4Addr([10, 0, 2, 3]), &bytes).err(),
        Some(SegmentError::BadChecksum)
    );
    let mut corrupt = bytes.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    assert_eq!(
        parse_tcp(A, B, &corrupt).err(),
        Some(SegmentError::BadChecksum)
    );
}

#[test]
fn header_length_and_truncation() {
    let bytes = emit(&header(ACK), &[]);
    assert_eq!(
        parse_tcp(A, B, &bytes[..19]).err(),
        Some(SegmentError::Truncated)
    );
    let mut short = bytes.clone();
    short[12] = 0x40;
    assert_eq!(
        parse_tcp(A, B, &short).err(),
        Some(SegmentError::BadHeaderLength)
    );
    let mut long = emit(&header(ACK), &[0; 4]);
    long[12] = 0xf0;
    assert_eq!(parse_tcp(A, B, &long).err(), Some(SegmentError::Truncated));
}

#[test]
fn option_rules() {
    let syn = TcpHeader {
        mss: Some(1460),
        ..header(SYN)
    };
    let base = emit(&syn, &[]);
    let with_options = |options: [u8; 4]| {
        let mut bytes = base.clone();
        bytes[20..24].copy_from_slice(&options);
        fix_checksum(&mut bytes);
        parse_tcp(A, B, &bytes).map(|s| s.header.mss)
    };
    assert_eq!(with_options([2, 4, 0x05, 0xb4]), Ok(Some(1460)));
    assert_eq!(
        with_options([2, 3, 0x05, 0xb4]),
        Err(SegmentError::BadOption)
    );
    assert_eq!(with_options([2, 4, 0, 0]), Err(SegmentError::BadOption));
    assert_eq!(with_options([8, 0, 0, 0]), Err(SegmentError::BadOption));
    assert_eq!(with_options([8, 10, 0, 0]), Err(SegmentError::BadOption));
    assert_eq!(with_options([1, 8, 1, 0]), Err(SegmentError::BadOption));
    // Unknown well-formed options are skipped; EOL ends the list.
    assert_eq!(with_options([8, 4, 0, 0]), Ok(None));
    assert_eq!(with_options([1, 1, 0, 0]), Ok(None));
    assert_eq!(with_options([1, 0, 2, 4]), Ok(None));
    // MSS outside SYN is ignored.
    let ack_with_mss = TcpHeader {
        mss: Some(1000),
        ..header(ACK)
    };
    let bytes = emit(&ack_with_mss, &[]);
    assert_eq!(parse_tcp(A, B, &bytes).map(|s| s.header.mss), Ok(None));
}

#[test]
fn flag_and_port_rules() {
    for bad in [SYN | FIN, SYN | RST] {
        let mut buf = [0u8; 64];
        assert_eq!(
            emit_tcp(&mut buf, A, B, &header(bad), &[]),
            Err(SegmentError::BadFlags)
        );
        let mut bytes = emit(&header(ACK), &[]);
        bytes[13] = bad;
        fix_checksum(&mut bytes);
        assert_eq!(parse_tcp(A, B, &bytes).err(), Some(SegmentError::BadFlags));
    }
    let mut buf = [0u8; 64];
    let zero_port = TcpHeader {
        dst_port: 0,
        ..header(ACK)
    };
    assert_eq!(
        emit_tcp(&mut buf, A, B, &zero_port, &[]),
        Err(SegmentError::BadPort)
    );
    let mut bytes = emit(&header(ACK), &[]);
    bytes[0..2].copy_from_slice(&[0, 0]);
    fix_checksum(&mut bytes);
    assert_eq!(parse_tcp(A, B, &bytes).err(), Some(SegmentError::BadPort));
    // ECN bits are not reported as flags.
    let mut ecn = emit(&header(ACK), &[]);
    ecn[13] |= 0xc0;
    fix_checksum(&mut ecn);
    assert_eq!(parse_tcp(A, B, &ecn).map(|s| s.header.flags), Ok(ACK));
    assert_eq!(
        emit_tcp(&mut buf[..23], A, B, &header(ACK), &[1, 2, 3, 4]),
        Err(SegmentError::BufferTooSmall)
    );
}

#[test]
fn sequence_arithmetic_wraps() {
    assert!(seq::lt(u32::MAX, 0));
    assert!(seq::lt(u32::MAX - 10, 5));
    assert!(seq::gt(5, u32::MAX - 10));
    assert!(seq::le(7, 7) && seq::ge(7, 7));
    assert!(!seq::lt(0, u32::MAX));
    assert!(seq::in_window(2, u32::MAX - 1, 10));
    assert!(!seq::in_window(8, u32::MAX - 1, 10));
    assert!(!seq::in_window(5, 5, 0));
}

#[test]
fn arbitrary_bytes_never_panic() {
    let mut state = 0x853c_49e6_748f_ea9bu64;
    let mut buf = [0u8; 80];
    for _ in 0..20_000 {
        for byte in buf.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        let len = (state as usize) % buf.len();
        let _ = parse_tcp(A, B, &buf[..len]);
        // Also with a valid checksum so option parsing is reached.
        if len >= 20 {
            let mut bytes = buf[..len].to_vec();
            bytes[12] = (5 + (state as u8 % 11)) << 4;
            fix_checksum(&mut bytes);
            let _ = parse_tcp(A, B, &bytes);
        }
    }
}
