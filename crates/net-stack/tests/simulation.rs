//! Deterministic host-only link simulation. The client uses the real
//! `net_tcp::Connection` and both endpoints receive frames through `demux_frame`.

use net_stack::{
    arp::{ArpCache, ArpConfig, LearnOutcome, Resolve},
    demux::{demux_frame, InboundPacket, Interface},
};
use net_tcp::segment::{emit_tcp, TcpHeader, ACK, FIN, SYN};
use net_tcp::{Config, Connection, Endpoint, State};
use net_wire::{
    emit_arp, emit_ethernet, emit_ipv4, ArpOp, ArpPacket, Ipv4Addr, Ipv4Emit, MacAddr,
    ETHERTYPE_ARP, ETHERTYPE_IPV4, IPPROTO_TCP,
};
use std::vec::Vec;

const C_IP: Ipv4Addr = Ipv4Addr([10, 9, 0, 1]);
const S_IP: Ipv4Addr = Ipv4Addr([10, 9, 0, 2]);
const C_MAC: MacAddr = MacAddr([2, 0, 0, 0, 9, 1]);
const S_MAC: MacAddr = MacAddr([2, 0, 0, 0, 9, 2]);
const C_PORT: u16 = 40000;
const S_PORT: u16 = 443;
const C_ISN: u32 = 0xffff_fffd;
const S_ISN: u32 = 7000;

#[derive(Clone)]
struct QueuedFrame {
    due: u64,
    order: u64,
    bytes: Vec<u8>,
}

struct SimLink {
    queue: Vec<QueuedFrame>,
    seed: u32,
    next_order: u64,
    delivered_high: u64,
    dropped_syn: bool,
    duplicated_data: bool,
    reordered: bool,
    faults: bool,
}

impl SimLink {
    fn new(faults: bool) -> Self {
        Self {
            queue: Vec::new(),
            seed: 0x5eed_1234,
            next_order: 0,
            delivered_high: 0,
            dropped_syn: false,
            duplicated_data: false,
            reordered: false,
            faults,
        }
    }

    fn send(&mut self, bytes: &[u8], now: u64) {
        let frame = net_wire::parse_ethernet(bytes).expect("simulated frame must be valid");
        let tcp = if frame.ethertype == ETHERTYPE_IPV4 {
            net_wire::parse_ipv4(frame.payload)
                .ok()
                .filter(|ip| ip.protocol == IPPROTO_TCP)
                .and_then(|ip| net_tcp::parse_tcp(ip.src, ip.dst, ip.payload).ok())
        } else {
            None
        };
        if self.faults && !self.dropped_syn {
            if let Some(segment) = tcp {
                if segment.header.flags & (SYN | ACK) == SYN {
                    self.dropped_syn = true;
                    return;
                }
            }
        }

        let mut delay = self.next_jitter();
        if self.faults {
            if let Some(segment) = tcp {
                let pure_ack = segment.header.flags == ACK && segment.payload.is_empty();
                if pure_ack {
                    // Hold the handshake ACK while the following data packet
                    // is allowed through, deterministically reordering them.
                    delay = 5;
                }
                if segment.header.flags & SYN != 0 && segment.header.flags & ACK == 0 {
                    self.enqueue(bytes, now + delay);
                    self.enqueue(bytes, now + delay + 1);
                    return;
                }
                if !self.duplicated_data && !segment.payload.is_empty() {
                    self.duplicated_data = true;
                    delay = 0;
                    self.enqueue(bytes, now + delay);
                    self.enqueue(bytes, now + delay + 1);
                    return;
                }
            }
        }
        self.enqueue(bytes, now + delay);
    }

    fn next_jitter(&mut self) -> u64 {
        self.seed = self
            .seed
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        (self.seed % 3) as u64
    }

    fn enqueue(&mut self, bytes: &[u8], due: u64) {
        let order = self.next_order;
        self.next_order += 1;
        self.queue.push(QueuedFrame {
            due,
            order,
            bytes: Vec::from(bytes),
        });
    }

    fn pop_due(&mut self, now: u64) -> Option<Vec<u8>> {
        let index = self
            .queue
            .iter()
            .enumerate()
            .filter(|(_, packet)| packet.due <= now)
            .min_by_key(|(_, packet)| (packet.due, packet.order))
            .map(|(index, _)| index)?;
        let packet = self.queue.remove(index);
        if packet.order < self.delivered_high {
            self.reordered = true;
        }
        self.delivered_high = self.delivered_high.max(packet.order);
        Some(packet.bytes)
    }
}

struct Peer {
    rcv_nxt: u32,
    snd_nxt: u32,
    received: Vec<u8>,
    arp: ArpCache<4>,
}

impl Peer {
    fn new() -> Self {
        Self {
            rcv_nxt: 0,
            snd_nxt: S_ISN,
            received: Vec::new(),
            arp: ArpCache::new(ArpConfig::default()).unwrap(),
        }
    }
}

fn client_interface() -> Interface {
    Interface {
        mac: C_MAC,
        ip: C_IP,
    }
}
fn server_interface() -> Interface {
    Interface {
        mac: S_MAC,
        ip: S_IP,
    }
}

fn tcp_frame(
    src_mac: MacAddr,
    dst_mac: MacAddr,
    src: Ipv4Addr,
    dst: Ipv4Addr,
    header: &TcpHeader,
    payload: &[u8],
    ident: u16,
) -> Vec<u8> {
    let mut tcp = [0u8; 1600];
    let tcp_len = emit_tcp(&mut tcp, src, dst, header, payload).unwrap();
    ipv4_frame(
        src_mac,
        dst_mac,
        src,
        dst,
        IPPROTO_TCP,
        &tcp[..tcp_len],
        ident,
    )
}

fn ipv4_frame(
    src_mac: MacAddr,
    dst_mac: MacAddr,
    src: Ipv4Addr,
    dst: Ipv4Addr,
    proto: u8,
    payload: &[u8],
    ident: u16,
) -> Vec<u8> {
    let mut ip = [0u8; 1600];
    let ip_len = emit_ipv4(
        &mut ip,
        &Ipv4Emit {
            src,
            dst,
            protocol: proto,
            ttl: 64,
            ident,
        },
        payload,
    )
    .unwrap();
    let mut frame = [0u8; 1700];
    let frame_len =
        emit_ethernet(&mut frame, dst_mac, src_mac, ETHERTYPE_IPV4, &ip[..ip_len]).unwrap();
    Vec::from(&frame[..frame_len])
}

fn arp_frame(dst_mac: MacAddr, src_mac: MacAddr, packet: &ArpPacket) -> Vec<u8> {
    let mut arp = [0u8; 28];
    let arp_len = emit_arp(&mut arp, packet).unwrap();
    let mut frame = [0u8; 64];
    let len = emit_ethernet(&mut frame, dst_mac, src_mac, ETHERTYPE_ARP, &arp[..arp_len]).unwrap();
    Vec::from(&frame[..len])
}

fn peer_receive(frame: &[u8], peer: &mut Peer, now: u64) -> Vec<Vec<u8>> {
    let mut replies = Vec::new();
    match demux_frame(server_interface(), frame).unwrap() {
        Some(InboundPacket::Arp { packet, .. }) => {
            assert_eq!(
                peer.arp.learn(packet.sender_ip, packet.sender_mac, now),
                Ok(LearnOutcome::Inserted)
            );
            if packet.op == ArpOp::Request {
                replies.push(arp_frame(
                    packet.sender_mac,
                    S_MAC,
                    &ArpPacket {
                        op: ArpOp::Reply,
                        sender_mac: S_MAC,
                        sender_ip: S_IP,
                        target_mac: packet.sender_mac,
                        target_ip: packet.sender_ip,
                    },
                ));
            }
        }
        Some(InboundPacket::Tcp { packet, .. }) => {
            let h = packet.header;
            if h.flags & SYN != 0 && h.flags & ACK == 0 {
                peer.rcv_nxt = h.seq.wrapping_add(1);
                peer.snd_nxt = S_ISN.wrapping_add(1);
                replies.push(tcp_frame(
                    S_MAC,
                    C_MAC,
                    S_IP,
                    C_IP,
                    &TcpHeader {
                        src_port: S_PORT,
                        dst_port: C_PORT,
                        seq: S_ISN,
                        ack: peer.rcv_nxt,
                        flags: SYN | ACK,
                        window: 4096,
                        mss: Some(536),
                    },
                    &[],
                    1,
                ));
            } else {
                if h.seq == peer.rcv_nxt {
                    peer.received.extend_from_slice(packet.payload);
                    peer.rcv_nxt = peer.rcv_nxt.wrapping_add(packet.payload.len() as u32);
                    if h.flags & FIN != 0 {
                        peer.rcv_nxt = peer.rcv_nxt.wrapping_add(1);
                    }
                }
                if h.flags & FIN != 0 {
                    replies.push(tcp_frame(
                        S_MAC,
                        C_MAC,
                        S_IP,
                        C_IP,
                        &TcpHeader {
                            src_port: S_PORT,
                            dst_port: C_PORT,
                            seq: peer.snd_nxt,
                            ack: peer.rcv_nxt,
                            flags: ACK | FIN,
                            window: 4096,
                            mss: None,
                        },
                        &[],
                        2,
                    ));
                    peer.snd_nxt = peer.snd_nxt.wrapping_add(1);
                } else if !packet.payload.is_empty() {
                    replies.push(tcp_frame(
                        S_MAC,
                        C_MAC,
                        S_IP,
                        C_IP,
                        &TcpHeader {
                            src_port: S_PORT,
                            dst_port: C_PORT,
                            seq: peer.snd_nxt,
                            ack: peer.rcv_nxt,
                            flags: ACK,
                            window: 4096,
                            mss: None,
                        },
                        &[],
                        3,
                    ));
                }
            }
        }
        _ => {}
    }
    replies
}

fn client_receive(frame: &[u8], conn: &mut Connection<'_>, arp: &mut ArpCache<4>, now: u64) {
    if let Some(packet) = demux_frame(client_interface(), frame).unwrap() {
        match packet {
            InboundPacket::Arp { packet, .. } => {
                arp.learn(packet.sender_ip, packet.sender_mac, now).unwrap();
            }
            InboundPacket::Tcp { packet, .. } => conn.on_segment(&packet, now),
            _ => {}
        }
    }
}

fn pump(
    link: &mut SimLink,
    peer: &mut Peer,
    conn: &mut Connection<'_>,
    arp: &mut ArpCache<4>,
    now: u64,
) {
    let mut delivered = 0;
    while let Some(frame) = link.pop_due(now) {
        let eth = net_wire::parse_ethernet(&frame).unwrap();
        if eth.dst == S_MAC || (eth.dst == MacAddr::BROADCAST && eth.src == C_MAC) {
            for reply in peer_receive(&frame, peer, now) {
                link.send(&reply, now);
            }
        } else if eth.dst == C_MAC || eth.dst == MacAddr::BROADCAST {
            client_receive(&frame, conn, arp, now);
        }
        delivered += 1;
        assert!(delivered < 128, "simulated link failed to settle");
    }
}

fn transmit_client(conn: &mut Connection<'_>, link: &mut SimLink, now: u64) -> usize {
    let mut raw = [0u8; 1600];
    let Some(len) = conn.poll_transmit(now, &mut raw).unwrap() else {
        return 0;
    };
    let frame = ipv4_frame(
        C_MAC,
        S_MAC,
        C_IP,
        S_IP,
        IPPROTO_TCP,
        &raw[..len],
        now as u16,
    );
    link.send(&frame, now);
    1
}

fn send_peer_data(peer: &mut Peer, link: &mut SimLink, payload: &[u8], now: u64) {
    let frame = tcp_frame(
        S_MAC,
        C_MAC,
        S_IP,
        C_IP,
        &TcpHeader {
            src_port: S_PORT,
            dst_port: C_PORT,
            seq: peer.snd_nxt,
            ack: peer.rcv_nxt,
            flags: ACK,
            window: 4096,
            mss: None,
        },
        payload,
        now as u16,
    );
    peer.snd_nxt = peer.snd_nxt.wrapping_add(payload.len() as u32);
    link.send(&frame, now);
}

fn run_exchange(faults: bool) {
    let mut link = SimLink::new(faults);
    let mut peer = Peer::new();
    let mut arp = ArpCache::<4>::new(ArpConfig::default()).unwrap();
    assert_eq!(
        arp.resolve(S_IP, 0),
        Ok(Resolve::SendRequest { attempt: 1 })
    );
    let request = ArpPacket {
        op: ArpOp::Request,
        sender_mac: C_MAC,
        sender_ip: C_IP,
        target_mac: MacAddr([0; 6]),
        target_ip: S_IP,
    };
    let frame = arp_frame(MacAddr::BROADCAST, C_MAC, &request);
    link.send(&frame, 0);
    let (mut tx, mut rx) = ([0u8; 2048], [0u8; 2048]);
    let mut conn = Connection::connect(
        Config {
            rto_initial_ms: 50,
            rto_min_ms: 50,
            rto_max_ms: 100,
            time_wait_ms: 20,
            ..Config::default()
        },
        Endpoint {
            addr: C_IP,
            port: C_PORT,
        },
        Endpoint {
            addr: S_IP,
            port: S_PORT,
        },
        C_ISN,
        &mut tx,
        &mut rx,
    )
    .unwrap();
    pump(&mut link, &mut peer, &mut conn, &mut arp, 10);
    pump(&mut link, &mut peer, &mut conn, &mut arp, 20);
    assert_eq!(arp.resolve(S_IP, 20), Ok(Resolve::Resolved(S_MAC)));

    assert_eq!(transmit_client(&mut conn, &mut link, 30), 1);
    if faults {
        // The first SYN was dropped; retransmission occurs at its RTO.
        assert_eq!(transmit_client(&mut conn, &mut link, 79), 0);
        assert_eq!(transmit_client(&mut conn, &mut link, 80), 1);
        pump(&mut link, &mut peer, &mut conn, &mut arp, 90);
    } else {
        pump(&mut link, &mut peer, &mut conn, &mut arp, 40);
    }
    assert_eq!(conn.state(), State::Established);

    // Queue the final handshake ACK, then data. In fault mode the deterministic
    // channel delays the ACK and duplicates data, so data overtakes the ACK.
    assert_eq!(transmit_client(&mut conn, &mut link, 100), 1);
    assert_eq!(conn.send(b"request-data").unwrap(), 12);
    assert_eq!(transmit_client(&mut conn, &mut link, 100), 1);
    pump(&mut link, &mut peer, &mut conn, &mut arp, 105);
    assert_eq!(peer.received, b"request-data");
    pump(&mut link, &mut peer, &mut conn, &mut arp, 110);
    assert_eq!(conn.send_buffered(), 0);
    if faults {
        assert!(
            link.reordered,
            "fault profile did not reorder the queued ACK and data"
        );
        assert!(link.duplicated_data);
    }

    send_peer_data(&mut peer, &mut link, b"response", 120);
    pump(&mut link, &mut peer, &mut conn, &mut arp, 125);
    let mut received = [0u8; 16];
    assert_eq!(conn.recv(&mut received), 8);
    assert_eq!(&received[..8], b"response");
    transmit_client(&mut conn, &mut link, 126);
    pump(&mut link, &mut peer, &mut conn, &mut arp, 130);

    conn.close();
    assert_eq!(transmit_client(&mut conn, &mut link, 131), 1);
    pump(&mut link, &mut peer, &mut conn, &mut arp, 140);
    pump(&mut link, &mut peer, &mut conn, &mut arp, 145);
    assert_eq!(conn.state(), State::TimeWait);
    assert_eq!(transmit_client(&mut conn, &mut link, 146), 1);
    pump(&mut link, &mut peer, &mut conn, &mut arp, 150);
}

#[test]
fn arp_tcp_data_and_active_close_complete_over_clean_link() {
    run_exchange(false);
}

#[test]
fn arp_tcp_data_and_active_close_survive_deterministic_faults() {
    run_exchange(true);
}
