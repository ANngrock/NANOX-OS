use net_tcp::segment::{ACK, FIN, PSH, RST, SYN};
use net_tcp::{
    emit_tcp, parse_tcp, Config, Connection, Endpoint, SegmentError, State, TcpError, TcpHeader,
};
use net_wire::Ipv4Addr;

const CLIENT: Ipv4Addr = Ipv4Addr([10, 0, 2, 15]);
const SERVER: Ipv4Addr = Ipv4Addr([10, 0, 2, 2]);
const CPORT: u16 = 40000;
const SPORT: u16 = 443;
const ISS: u32 = 1000;
const IRS: u32 = 5000;

fn config() -> Config {
    Config {
        rto_min_ms: 100,
        ..Config::default()
    }
}

fn endpoints() -> (Endpoint, Endpoint) {
    (
        Endpoint {
            addr: CLIENT,
            port: CPORT,
        },
        Endpoint {
            addr: SERVER,
            port: SPORT,
        },
    )
}

#[derive(Clone, Debug)]
struct Seg {
    h: TcpHeader,
    data: Vec<u8>,
}

impl Seg {
    fn has(&self, flag: u8) -> bool {
        self.h.flags & flag != 0
    }
}

/// Drains every segment the connection wants to send at `now`.
fn pull(conn: &mut Connection<'_>, now: u64) -> Vec<Seg> {
    let mut out = Vec::new();
    let mut buf = [0u8; 2048];
    while let Some(len) = conn.poll_transmit(now, &mut buf).unwrap() {
        let seg = parse_tcp(CLIENT, SERVER, &buf[..len]).unwrap();
        out.push(Seg {
            h: seg.header,
            data: seg.payload.to_vec(),
        });
        assert!(out.len() < 1000, "poll_transmit did not settle");
    }
    out
}

fn deliver_header(conn: &mut Connection<'_>, h: TcpHeader, data: &[u8], now: u64) {
    let mut buf = vec![0u8; 64 + data.len()];
    let len = emit_tcp(&mut buf, SERVER, CLIENT, &h, data).unwrap();
    let seg = parse_tcp(SERVER, CLIENT, &buf[..len]).unwrap();
    conn.on_segment(&seg, now);
}

fn deliver(
    conn: &mut Connection<'_>,
    seq: u32,
    ack: u32,
    flags: u8,
    window: u16,
    data: &[u8],
    now: u64,
) {
    let h = TcpHeader {
        src_port: SPORT,
        dst_port: CPORT,
        seq,
        ack,
        flags,
        window,
        mss: None,
    };
    deliver_header(conn, h, data, now);
}

fn syn_ack(conn: &mut Connection<'_>, iss: u32, mss: u16, window: u16, now: u64) {
    let h = TcpHeader {
        src_port: SPORT,
        dst_port: CPORT,
        seq: IRS,
        ack: iss.wrapping_add(1),
        flags: SYN | ACK,
        window,
        mss: Some(mss),
    };
    deliver_header(conn, h, &[], now);
}

/// Handshake at time 0 (RTT sample 0, so RTO settles at rto_min = 100 ms).
fn open<'a>(tx: &'a mut [u8], rx: &'a mut [u8], iss: u32, mss: u16, window: u16) -> Connection<'a> {
    let (local, remote) = endpoints();
    let mut conn = Connection::connect(config(), local, remote, iss, tx, rx).unwrap();
    assert_eq!(pull(&mut conn, 0).len(), 1);
    syn_ack(&mut conn, iss, mss, window, 0);
    assert_eq!(conn.state(), State::Established);
    let ack = pull(&mut conn, 0);
    assert_eq!(ack.len(), 1);
    assert_eq!((ack[0].h.flags, ack[0].h.ack), (ACK, IRS + 1));
    conn
}

#[test]
fn invalid_configuration_is_rejected() {
    let (local, remote) = endpoints();
    let (mut tx, mut rx) = ([0u8; 8], [0u8; 8]);
    let bad_configs = [
        Config {
            mss: 10,
            ..config()
        },
        Config {
            max_retries: 0,
            ..config()
        },
        Config {
            rto_min_ms: 0,
            ..config()
        },
        Config {
            rto_min_ms: 2000,
            ..config()
        },
        Config {
            rto_max_ms: 500,
            ..config()
        },
        Config {
            time_wait_ms: 0,
            ..config()
        },
    ];
    for bad in bad_configs {
        assert_eq!(
            Connection::connect(bad, local, remote, ISS, &mut tx, &mut rx).err(),
            Some(TcpError::InvalidConfig)
        );
    }
    let bad_remote = Endpoint {
        addr: Ipv4Addr::BROADCAST,
        ..remote
    };
    assert!(Connection::connect(config(), local, bad_remote, ISS, &mut tx, &mut rx).is_err());
    let zero_port = Endpoint { port: 0, ..local };
    assert!(Connection::connect(config(), zero_port, remote, ISS, &mut tx, &mut rx).is_err());
    let mut empty = [0u8; 0];
    assert!(Connection::connect(config(), local, remote, ISS, &mut tx, &mut empty).is_err());
}

#[test]
fn handshake_sends_mss_and_window() {
    let (mut tx, mut rx) = ([0u8; 64], [0u8; 3000]);
    let (local, remote) = endpoints();
    let mut conn = Connection::connect(config(), local, remote, ISS, &mut tx, &mut rx).unwrap();
    // BufferTooSmall does not consume the pending SYN.
    assert_eq!(
        conn.poll_transmit(0, &mut [0u8; 23]),
        Err(SegmentError::BufferTooSmall)
    );
    let syn = pull(&mut conn, 0);
    assert_eq!(syn.len(), 1);
    assert_eq!(
        (syn[0].h.flags, syn[0].h.seq, syn[0].h.mss, syn[0].h.window),
        (SYN, ISS, Some(1460), 3000)
    );
    assert!(pull(&mut conn, 50).is_empty());
    syn_ack(&mut conn, ISS, 1000, 8000, 0);
    assert_eq!(conn.state(), State::Established);
    let ack = pull(&mut conn, 0);
    assert_eq!((ack[0].h.seq, ack[0].h.ack), (ISS + 1, IRS + 1));
}

#[test]
fn handshake_and_data_across_sequence_wrap() {
    let iss = u32::MAX - 1;
    let (mut tx, mut rx) = ([0u8; 64], [0u8; 64]);
    let mut conn = open(&mut tx, &mut rx, iss, 1000, 8000);
    assert_eq!(conn.send(b"wrap!"), Ok(5));
    let segs = pull(&mut conn, 10);
    assert_eq!(segs.len(), 1);
    assert_eq!(
        (segs[0].h.seq, segs[0].data.as_slice()),
        (u32::MAX, &b"wrap!"[..])
    );
    deliver(&mut conn, IRS + 1, iss.wrapping_add(6), ACK, 8000, &[], 20);
    assert_eq!((conn.send_buffered(), conn.bytes_in_flight()), (0, 0));
}

#[test]
fn syn_retransmits_with_backoff_then_times_out() {
    let (mut tx, mut rx) = ([0u8; 8], [0u8; 8]);
    let (local, remote) = endpoints();
    let cfg = Config {
        max_retries: 3,
        ..config()
    };
    let mut conn = Connection::connect(cfg, local, remote, ISS, &mut tx, &mut rx).unwrap();
    let mut sent_at = vec![];
    let mut now = 0;
    for _ in 0..10 {
        for seg in pull(&mut conn, now) {
            assert_eq!((seg.h.flags, seg.h.seq), (SYN, ISS));
            sent_at.push(now);
        }
        match conn.next_deadline() {
            Some(deadline) => now = deadline,
            None => break,
        }
    }
    // Initial SYN plus three retransmissions at 1 s, 2 s, 4 s intervals.
    assert_eq!(sent_at, vec![0, 1000, 3000, 7000]);
    assert_eq!(
        (conn.state(), conn.error()),
        (State::Closed, Some(TcpError::TimedOut))
    );
    assert_eq!(conn.send(b"x"), Err(TcpError::TimedOut));
}

#[test]
fn syn_sent_ack_and_rst_rules() {
    let (mut tx, mut rx) = ([0u8; 8], [0u8; 8]);
    let (local, remote) = endpoints();
    let mut conn = Connection::connect(config(), local, remote, ISS, &mut tx, &mut rx).unwrap();
    pull(&mut conn, 0);
    // Unacceptable ACK: answer with RST carrying SEQ = SEG.ACK.
    deliver(&mut conn, IRS, ISS + 5, ACK, 100, &[], 1);
    let rst = pull(&mut conn, 1);
    assert_eq!(rst.len(), 1);
    assert_eq!((rst[0].h.flags, rst[0].h.seq), (RST, ISS + 5));
    assert_eq!(conn.state(), State::SynSent);
    // RST with an unacceptable ACK is ignored; SYN without ACK too.
    deliver(&mut conn, IRS, ISS + 7, RST | ACK, 0, &[], 2);
    deliver(&mut conn, IRS, 0, SYN, 100, &[], 2);
    assert_eq!(conn.state(), State::SynSent);
    assert!(pull(&mut conn, 2).is_empty());
    // RST acknowledging our SYN: connection refused.
    deliver(&mut conn, 0, ISS + 1, RST | ACK, 0, &[], 3);
    assert_eq!(
        (conn.state(), conn.error()),
        (State::Closed, Some(TcpError::Refused))
    );
}

#[test]
fn segmentation_follows_mss_cwnd_and_peer_window() {
    let (mut tx, mut rx) = ([0u8; 20000], [0u8; 4096]);
    let mut conn = open(&mut tx, &mut rx, ISS, 1000, 65535);
    assert_eq!(conn.send(&[7u8; 10000]), Ok(10000));
    // RFC 5681 initial window for MSS 1000: min(4000, max(2000, 4380)).
    assert_eq!(conn.congestion_window(), 4000);
    let first = pull(&mut conn, 10);
    assert_eq!(first.len(), 4);
    for (i, seg) in first.iter().enumerate() {
        assert_eq!(
            (seg.h.seq, seg.data.len()),
            (ISS + 1 + 1000 * i as u32, 1000)
        );
        assert!(seg.has(ACK) && seg.has(PSH));
    }
    deliver(&mut conn, IRS + 1, ISS + 1 + 4000, ACK, 65535, &[], 20);
    assert_eq!(conn.congestion_window(), 5000);
    assert_eq!(pull(&mut conn, 20).len(), 5);
    // Peer window 500: at most 500 bytes in flight regardless of cwnd.
    deliver(&mut conn, IRS + 1, ISS + 1 + 9000, ACK, 500, &[], 30);
    let limited = pull(&mut conn, 30);
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].data.len(), 500);
    assert!(pull(&mut conn, 31).is_empty());
}

#[test]
fn receive_in_order_drops_out_of_order_and_trims_overlap() {
    let (mut tx, mut rx) = ([0u8; 64], [0u8; 4096]);
    let mut conn = open(&mut tx, &mut rx, ISS, 1000, 8000);
    deliver(&mut conn, IRS + 1, ISS + 1, ACK | PSH, 8000, b"hello", 1);
    assert_eq!(pull(&mut conn, 1)[0].h.ack, IRS + 6);
    // Future segment: dropped, duplicate ACK for the gap.
    deliver(&mut conn, IRS + 20, ISS + 1, ACK, 8000, b"WORLD", 2);
    let dup = pull(&mut conn, 2);
    assert_eq!((dup.len(), dup[0].h.ack), (1, IRS + 6));
    // Overlapping retransmission: only the new tail is appended.
    deliver(&mut conn, IRS + 4, ISS + 1, ACK, 8000, b"lo wor", 3);
    assert_eq!(pull(&mut conn, 3)[0].h.ack, IRS + 10);
    let mut out = [0u8; 32];
    let n = conn.recv(&mut out);
    assert_eq!(&out[..n], b"hello wor");
    // Entirely old data only re-ACKs.
    deliver(&mut conn, IRS + 1, ISS + 1, ACK, 8000, b"hel", 4);
    assert_eq!(pull(&mut conn, 4)[0].h.ack, IRS + 10);
    assert_eq!(conn.recv(&mut out), 0);
}

#[test]
fn full_receive_buffer_advertises_zero_then_updates() {
    let (mut tx, mut rx) = ([0u8; 64], [0u8; 8]);
    let mut conn = open(&mut tx, &mut rx, ISS, 1000, 8000);
    deliver(&mut conn, IRS + 1, ISS + 1, ACK, 8000, b"0123456789", 1);
    let ack = pull(&mut conn, 1);
    assert_eq!((ack[0].h.ack, ack[0].h.window), (IRS + 9, 0));
    // Zero window: the segment at RCV.NXT is processed but not buffered.
    deliver(&mut conn, IRS + 9, ISS + 1, ACK, 8000, b"zz", 2);
    let ack = pull(&mut conn, 2);
    assert_eq!((ack[0].h.ack, ack[0].h.window), (IRS + 9, 0));
    let mut out = [0u8; 16];
    assert_eq!(conn.recv(&mut out), 8);
    assert_eq!(&out[..8], b"01234567");
    let update = pull(&mut conn, 3);
    assert_eq!(update.len(), 1);
    assert_eq!(update[0].h.window, 8);
}

#[test]
fn retransmission_backoff_karn_and_loss_window() {
    let (mut tx, mut rx) = ([0u8; 8192], [0u8; 4096]);
    let mut conn = open(&mut tx, &mut rx, ISS, 1000, 65535);
    assert_eq!(conn.rto_ms(), 100);
    conn.send(&[1u8; 3000]).unwrap();
    assert_eq!(pull(&mut conn, 1000).len(), 3);
    assert!(pull(&mut conn, 1099).is_empty());
    // Timeout: go back to SND.UNA with a one-segment loss window.
    let retransmit = pull(&mut conn, 1100);
    assert_eq!(retransmit.len(), 1);
    assert_eq!(
        (retransmit[0].h.seq, retransmit[0].data.len()),
        (ISS + 1, 1000)
    );
    assert_eq!((conn.rto_ms(), conn.congestion_window()), (200, 1000));
    assert!(pull(&mut conn, 1299).is_empty());
    assert_eq!(pull(&mut conn, 1300).len(), 1);
    assert_eq!(conn.rto_ms(), 400);
    // ACK after retransmissions: no RTT sample (Karn), backed-off RTO kept.
    deliver(&mut conn, IRS + 1, ISS + 3001, ACK, 65535, &[], 1350);
    assert_eq!((conn.rto_ms(), conn.bytes_in_flight()), (400, 0));
    // Slow start below ssthresh = max(3000 / 2, 2 * MSS) = 2000.
    assert_eq!(conn.congestion_window(), 2000);
    assert_eq!(conn.next_deadline(), None);
    // At ssthresh: congestion avoidance adds MSS * MSS / cwnd per ACK.
    conn.send(&[2u8; 2000]).unwrap();
    assert_eq!(pull(&mut conn, 2000).len(), 2);
    deliver(&mut conn, IRS + 1, ISS + 5001, ACK, 65535, &[], 2050);
    assert_eq!(conn.congestion_window(), 2500);
}

#[test]
fn rtt_estimation_follows_rfc6298() {
    let (mut tx, mut rx) = ([0u8; 256], [0u8; 256]);
    let (local, remote) = endpoints();
    let mut conn = Connection::connect(config(), local, remote, ISS, &mut tx, &mut rx).unwrap();
    pull(&mut conn, 0);
    syn_ack(&mut conn, ISS, 1000, 8000, 200);
    // First sample R = 200: SRTT 200, RTTVAR 100, RTO = 200 + 4 * 100.
    assert_eq!(conn.rto_ms(), 600);
    pull(&mut conn, 200);
    conn.send(b"x").unwrap();
    pull(&mut conn, 1000);
    deliver(&mut conn, IRS + 1, ISS + 2, ACK, 8000, &[], 1100);
    // R = 100: RTTVAR = (3 * 100 + 100) / 4 = 100, SRTT = (7 * 200 + 100) / 8.
    assert_eq!(conn.rto_ms(), 187 + 400);
}

#[test]
fn retry_limit_in_established_state() {
    let (mut tx, mut rx) = ([0u8; 64], [0u8; 64]);
    let (local, remote) = endpoints();
    let cfg = Config {
        max_retries: 2,
        ..config()
    };
    let mut conn = Connection::connect(cfg, local, remote, ISS, &mut tx, &mut rx).unwrap();
    pull(&mut conn, 0);
    syn_ack(&mut conn, ISS, 1000, 8000, 0);
    pull(&mut conn, 0);
    conn.send(b"data").unwrap();
    let mut now = 10;
    let mut sends = 0;
    while conn.state() != State::Closed {
        sends += pull(&mut conn, now)
            .iter()
            .filter(|s| !s.data.is_empty())
            .count();
        now = conn.next_deadline().unwrap_or(now);
    }
    assert_eq!(sends, 3);
    assert_eq!(conn.error(), Some(TcpError::TimedOut));
}

#[test]
fn zero_window_probes_never_time_out() {
    let (mut tx, mut rx) = ([0u8; 256], [0u8; 256]);
    let mut conn = open(&mut tx, &mut rx, ISS, 1000, 0);
    conn.send(&[9u8; 100]).unwrap();
    assert!(pull(&mut conn, 10).is_empty());
    assert_eq!(conn.next_deadline(), Some(110));
    let mut now = 110;
    let mut intervals = vec![];
    for _ in 0..12 {
        let probe = pull(&mut conn, now);
        assert_eq!(probe.len(), 1);
        assert_eq!((probe[0].h.seq, probe[0].data.len()), (ISS + 1, 1));
        // Peer still has no room: acknowledges nothing, window stays 0.
        deliver(&mut conn, IRS + 1, ISS + 1, ACK, 0, &[], now);
        let next = conn.next_deadline().unwrap();
        intervals.push(next - now);
        now = next;
    }
    assert_eq!(conn.state(), State::Established);
    assert_eq!(&intervals[..4], &[200, 400, 800, 1600]);
    assert_eq!(*intervals.last().unwrap(), 60_000);
    // Window opens without taking the probe byte: data resumes at SND.UNA+1.
    deliver(&mut conn, IRS + 1, ISS + 1, ACK, 4000, &[], now);
    let resumed = pull(&mut conn, now);
    let total: usize = resumed.iter().map(|s| s.data.len()).sum();
    assert_eq!((resumed[0].h.seq, total), (ISS + 2, 99));
    deliver(&mut conn, IRS + 1, ISS + 101, ACK, 4000, &[], now + 5);
    assert_eq!(conn.send_buffered(), 0);
}

#[test]
fn rst_syn_and_ack_validation_in_synchronized_states() {
    let (mut tx, mut rx) = ([0u8; 64], [0u8; 4096]);
    let mut conn = open(&mut tx, &mut rx, ISS, 1000, 8000);
    // In-window but not exact RST: challenge ACK (RFC 5961 3.2).
    deliver(&mut conn, IRS + 6, 0, RST, 0, &[], 1);
    let challenge = pull(&mut conn, 1);
    assert_eq!((challenge.len(), challenge[0].h.ack), (1, IRS + 1));
    assert_eq!(conn.state(), State::Established);
    // Out-of-window RST: silently dropped.
    deliver(&mut conn, IRS + 100_000, 0, RST, 0, &[], 2);
    assert!(pull(&mut conn, 2).is_empty());
    // SYN in a synchronized state: challenge ACK (RFC 5961 4.2).
    deliver(&mut conn, IRS + 1, 0, SYN, 8000, &[], 3);
    assert_eq!(pull(&mut conn, 3).len(), 1);
    assert_eq!(conn.state(), State::Established);
    // ACK for data never sent: ACK back, segment (and its data) dropped.
    deliver(&mut conn, IRS + 1, ISS + 50, ACK, 8000, b"x", 4);
    assert_eq!(pull(&mut conn, 4).len(), 1);
    assert_eq!(conn.recv_buffered(), 0);
    // Segment without ACK is ignored.
    deliver(&mut conn, IRS + 1, 0, PSH, 8000, b"y", 5);
    assert_eq!(conn.recv_buffered(), 0);
    // Exact RST resets.
    deliver(&mut conn, IRS + 1, 0, RST, 0, &[], 6);
    assert_eq!(
        (conn.state(), conn.error()),
        (State::Closed, Some(TcpError::Reset))
    );
    assert_eq!(conn.send(b"z"), Err(TcpError::Reset));
    assert!(pull(&mut conn, 7).is_empty());
}

#[test]
fn active_close_through_time_wait() {
    let (mut tx, mut rx) = ([0u8; 64], [0u8; 64]);
    let mut conn = open(&mut tx, &mut rx, ISS, 1000, 8000);
    conn.send(b"0123456789").unwrap();
    conn.close();
    assert_eq!(conn.state(), State::FinWait1);
    assert_eq!(conn.send(b"late"), Err(TcpError::SendClosed));
    let fin = pull(&mut conn, 1);
    assert_eq!(fin.len(), 1);
    assert!(fin[0].has(FIN));
    assert_eq!((fin[0].h.seq, fin[0].data.len()), (ISS + 1, 10));
    // FIN is retransmitted with the data until acknowledged.
    let again = pull(&mut conn, 101);
    assert!(again[0].has(FIN));
    deliver(&mut conn, IRS + 1, ISS + 12, ACK, 8000, &[], 150);
    assert_eq!(conn.state(), State::FinWait2);
    assert_eq!(conn.next_deadline(), None);
    deliver(&mut conn, IRS + 1, ISS + 12, ACK | FIN, 8000, &[], 200);
    assert_eq!(conn.state(), State::TimeWait);
    // The final ACK carries no FIN: ours is already acknowledged.
    let last_ack = pull(&mut conn, 200);
    assert_eq!((last_ack.len(), last_ack[0].h.flags), (1, ACK));
    assert_eq!((last_ack[0].h.seq, last_ack[0].h.ack), (ISS + 12, IRS + 2));
    // A retransmitted FIN is re-acknowledged and restarts 2*MSL.
    deliver(&mut conn, IRS + 1, ISS + 12, ACK | FIN, 8000, &[], 1000);
    assert_eq!(pull(&mut conn, 1000).len(), 1);
    assert_eq!(conn.next_deadline(), Some(61_000));
    assert!(pull(&mut conn, 60_999).is_empty());
    pull(&mut conn, 61_000);
    assert_eq!((conn.state(), conn.error()), (State::Closed, None));
}

#[test]
fn passive_close_and_simultaneous_close() {
    let (mut tx, mut rx) = ([0u8; 64], [0u8; 64]);
    let mut conn = open(&mut tx, &mut rx, ISS, 1000, 8000);
    deliver(&mut conn, IRS + 1, ISS + 1, ACK | FIN, 8000, b"bye", 1);
    assert_eq!(conn.state(), State::CloseWait);
    assert_eq!(pull(&mut conn, 1)[0].h.ack, IRS + 5);
    assert!(!conn.peer_closed());
    let mut out = [0u8; 8];
    assert_eq!(conn.recv(&mut out), 3);
    assert!(conn.peer_closed());
    conn.close();
    assert_eq!(conn.state(), State::LastAck);
    let fin = pull(&mut conn, 2);
    assert_eq!((fin[0].h.flags & FIN, fin[0].h.seq), (FIN, ISS + 1));
    deliver(&mut conn, IRS + 5, ISS + 2, ACK, 8000, &[], 3);
    assert_eq!((conn.state(), conn.error()), (State::Closed, None));

    let (mut tx, mut rx) = ([0u8; 64], [0u8; 64]);
    let mut conn = open(&mut tx, &mut rx, ISS, 1000, 8000);
    conn.close();
    assert!(pull(&mut conn, 1)[0].has(FIN));
    // Peer's FIN crosses ours without acknowledging it.
    deliver(&mut conn, IRS + 1, ISS + 1, ACK | FIN, 8000, &[], 2);
    assert_eq!(conn.state(), State::Closing);
    pull(&mut conn, 2);
    deliver(&mut conn, IRS + 2, ISS + 2, ACK, 8000, &[], 3);
    assert_eq!(conn.state(), State::TimeWait);
}

#[test]
fn abort_sends_rst() {
    let (mut tx, mut rx) = ([0u8; 64], [0u8; 64]);
    let mut conn = open(&mut tx, &mut rx, ISS, 1000, 8000);
    conn.send(b"abc").unwrap();
    pull(&mut conn, 1);
    conn.abort();
    let rst = pull(&mut conn, 2);
    assert_eq!(rst.len(), 1);
    assert_eq!((rst[0].h.flags, rst[0].h.seq), (RST, ISS + 4));
    assert_eq!((conn.state(), conn.error()), (State::Closed, None));
}

#[test]
fn port_mismatch_is_ignored() {
    let (mut tx, mut rx) = ([0u8; 64], [0u8; 64]);
    let mut conn = open(&mut tx, &mut rx, ISS, 1000, 8000);
    let h = TcpHeader {
        src_port: SPORT + 1,
        dst_port: CPORT,
        seq: IRS + 1,
        ack: ISS + 1,
        flags: RST,
        window: 0,
        mss: None,
    };
    deliver_header(&mut conn, h, &[], 1);
    assert_eq!(conn.state(), State::Established);
}

#[test]
fn arbitrary_segments_keep_invariants() {
    let (mut tx, mut rx) = ([0u8; 4096], [0u8; 1024]);
    let mut conn = open(&mut tx, &mut rx, ISS, 536, 2000);
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut now = 1;
    let mut sink = [0u8; 256];
    for round in 0..20_000u32 {
        if conn.state() == State::Closed {
            break;
        }
        let r = next();
        let flags = [ACK, ACK | PSH, ACK | FIN, RST, SYN | ACK, ACK | RST, PSH][r as usize % 7];
        let seq = IRS
            .wrapping_add(1)
            .wrapping_add((next() % 3000) as u32)
            .wrapping_sub(1000);
        let ack = ISS.wrapping_add((next() % 6000) as u32).wrapping_sub(1000);
        let len = (next() % 64) as usize;
        let data = vec![round as u8; len];
        deliver(
            &mut conn,
            seq,
            ack,
            flags,
            (next() % 4000) as u16,
            &data,
            now,
        );
        if round % 7 == 0 {
            let _ = conn.send(&[1u8; 300]);
        }
        if round % 5 == 0 {
            conn.recv(&mut sink);
        }
        now += next() % 150;
        pull(&mut conn, now);
        assert!(conn.bytes_in_flight() < 1 << 20);
        assert!(conn.send_buffered() <= 4096 && conn.recv_buffered() <= 1024);
    }
}
