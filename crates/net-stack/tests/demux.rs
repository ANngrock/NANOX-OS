use net_stack::demux::{demux_frame, InboundPacket, Interface, StackError};
use net_tcp::segment::{emit_tcp, TcpHeader, ACK};
use net_wire::{
    emit_arp, emit_ethernet, emit_icmp_echo, emit_ipv4, emit_udp, parse_arp, ArpOp, ArpPacket,
    IcmpEcho, Ipv4Addr, Ipv4Emit, MacAddr, WireError, ETHERTYPE_ARP, ETHERTYPE_IPV4, IPPROTO_ICMP,
    IPPROTO_TCP, IPPROTO_UDP,
};

const LOCAL_MAC: MacAddr = MacAddr([2, 0, 0, 0, 0, 1]);
const REMOTE_MAC: MacAddr = MacAddr([2, 0, 0, 0, 0, 2]);
const LOCAL_IP: Ipv4Addr = Ipv4Addr([10, 0, 0, 1]);
const REMOTE_IP: Ipv4Addr = Ipv4Addr([10, 0, 0, 2]);

fn iface() -> Interface {
    Interface {
        mac: LOCAL_MAC,
        ip: LOCAL_IP,
    }
}

fn ipv4_frame(
    protocol: u8,
    payload: &[u8],
    ttl: u8,
    dst_mac: MacAddr,
    dst_ip: Ipv4Addr,
) -> Vec<u8> {
    let mut ip = [0u8; 1600];
    let ip_len = emit_ipv4(
        &mut ip,
        &Ipv4Emit {
            src: REMOTE_IP,
            dst: dst_ip,
            protocol,
            ttl,
            ident: 7,
        },
        payload,
    )
    .unwrap();
    let mut frame = vec![0u8; ip_len + 14];
    let len = emit_ethernet(
        &mut frame,
        dst_mac,
        REMOTE_MAC,
        ETHERTYPE_IPV4,
        &ip[..ip_len],
    )
    .unwrap();
    frame.truncate(len);
    frame
}

#[test]
fn demultiplexes_valid_icmp_udp_and_tcp() {
    let mut icmp = [0u8; 32];
    let icmp_len = emit_icmp_echo(
        &mut icmp,
        &IcmpEcho {
            reply: false,
            ident: 2,
            seq: 4,
            data: b"ping",
        },
    )
    .unwrap();
    let frame = ipv4_frame(IPPROTO_ICMP, &icmp[..icmp_len], 64, LOCAL_MAC, LOCAL_IP);
    match demux_frame(iface(), &frame).unwrap().unwrap() {
        InboundPacket::Icmp { source, packet, .. } => {
            assert_eq!(source, REMOTE_IP);
            assert_eq!(packet.data, b"ping");
            assert!(!packet.reply);
        }
        other => panic!("unexpected packet: {other:?}"),
    }

    let mut udp = [0u8; 64];
    let udp_len = emit_udp(&mut udp, REMOTE_IP, LOCAL_IP, 53, 53000, b"dns").unwrap();
    let frame = ipv4_frame(IPPROTO_UDP, &udp[..udp_len], 64, LOCAL_MAC, LOCAL_IP);
    match demux_frame(iface(), &frame).unwrap().unwrap() {
        InboundPacket::Udp { packet, .. } => {
            assert_eq!(packet.src_port, 53);
            assert_eq!(packet.dst_port, 53000);
            assert_eq!(packet.payload, b"dns");
            assert!(packet.checksum_present);
        }
        other => panic!("unexpected packet: {other:?}"),
    }

    let mut tcp = [0u8; 64];
    let tcp_len = emit_tcp(
        &mut tcp,
        REMOTE_IP,
        LOCAL_IP,
        &TcpHeader {
            src_port: 443,
            dst_port: 40000,
            seq: 10,
            ack: 20,
            flags: ACK,
            window: 4096,
            ..TcpHeader::default()
        },
        b"tls",
    )
    .unwrap();
    let frame = ipv4_frame(IPPROTO_TCP, &tcp[..tcp_len], 64, LOCAL_MAC, LOCAL_IP);
    match demux_frame(iface(), &frame).unwrap().unwrap() {
        InboundPacket::Tcp { packet, .. } => {
            assert_eq!(packet.header.src_port, 443);
            assert_eq!(packet.payload, b"tls");
        }
        other => panic!("unexpected packet: {other:?}"),
    }
}

#[test]
fn arp_is_local_and_ethernet_sender_must_match() {
    let request = ArpPacket {
        op: ArpOp::Request,
        sender_mac: REMOTE_MAC,
        sender_ip: REMOTE_IP,
        target_mac: MacAddr([0; 6]),
        target_ip: LOCAL_IP,
    };
    let mut arp = [0u8; 28];
    let arp_len = emit_arp(&mut arp, &request).unwrap();
    let mut frame = [0u8; 64];
    let len = emit_ethernet(
        &mut frame,
        MacAddr::BROADCAST,
        REMOTE_MAC,
        ETHERTYPE_ARP,
        &arp[..arp_len],
    )
    .unwrap();
    match demux_frame(iface(), &frame[..len]).unwrap().unwrap() {
        InboundPacket::Arp { packet, .. } => assert_eq!(packet.target_ip, LOCAL_IP),
        other => panic!("unexpected packet: {other:?}"),
    }
    let mut wrong = [0u8; 64];
    let wrong_len = emit_ethernet(
        &mut wrong,
        MacAddr::BROADCAST,
        LOCAL_MAC,
        ETHERTYPE_ARP,
        &arp[..arp_len],
    )
    .unwrap();
    assert_eq!(
        demux_frame(iface(), &wrong[..wrong_len]),
        Err(StackError::AddressMismatch)
    );
    let mut other_target = request;
    other_target.target_ip = Ipv4Addr([10, 0, 0, 9]);
    let other_len = emit_arp(&mut arp, &other_target).unwrap();
    let frame_len = emit_ethernet(
        &mut frame,
        MacAddr::BROADCAST,
        REMOTE_MAC,
        ETHERTYPE_ARP,
        &arp[..other_len],
    )
    .unwrap();
    assert_eq!(demux_frame(iface(), &frame[..frame_len]).unwrap(), None);
    assert_eq!(parse_arp(&arp[..other_len]).unwrap().op, ArpOp::Request);
}

#[test]
fn filters_nonlocal_frames_and_rejects_bad_ttl_or_wire_checksums() {
    let mut udp = [0u8; 32];
    let udp_len = emit_udp(&mut udp, REMOTE_IP, LOCAL_IP, 9000, 9001, b"x").unwrap();
    let remote_mac_frame = ipv4_frame(IPPROTO_UDP, &udp[..udp_len], 64, REMOTE_MAC, LOCAL_IP);
    assert_eq!(demux_frame(iface(), &remote_mac_frame).unwrap(), None);
    let remote_ip_frame = ipv4_frame(
        IPPROTO_UDP,
        &udp[..udp_len],
        64,
        LOCAL_MAC,
        Ipv4Addr([10, 0, 0, 9]),
    );
    assert_eq!(demux_frame(iface(), &remote_ip_frame).unwrap(), None);
    let mut zero_ttl = ipv4_frame(IPPROTO_UDP, &udp[..udp_len], 1, LOCAL_MAC, LOCAL_IP);
    let ip_header = 14;
    zero_ttl[ip_header + 8] = 0;
    zero_ttl[ip_header + 10] = 0;
    zero_ttl[ip_header + 11] = 0;
    let sum = net_wire::checksum(&zero_ttl[ip_header..ip_header + 20]);
    zero_ttl[ip_header + 10..ip_header + 12].copy_from_slice(&sum.to_be_bytes());
    assert_eq!(demux_frame(iface(), &zero_ttl), Err(StackError::InvalidTtl));
    let mut bad_sum = ipv4_frame(IPPROTO_UDP, &udp[..udp_len], 64, LOCAL_MAC, LOCAL_IP);
    *bad_sum.last_mut().unwrap() ^= 0x80;
    assert_eq!(
        demux_frame(iface(), &bad_sum),
        Err(StackError::Wire(WireError::BadChecksum))
    );
}

#[test]
fn unknown_ip_protocol_is_ignored_but_fragmented_ipv4_is_rejected() {
    let unknown = ipv4_frame(99, b"opaque", 64, LOCAL_MAC, LOCAL_IP);
    assert_eq!(demux_frame(iface(), &unknown).unwrap(), None);
    let mut ip = [0u8; 32];
    let ip_len = emit_ipv4(
        &mut ip,
        &Ipv4Emit {
            src: REMOTE_IP,
            dst: LOCAL_IP,
            protocol: IPPROTO_UDP,
            ttl: 64,
            ident: 1,
        },
        b"",
    )
    .unwrap();
    ip[6..8].copy_from_slice(&0x2000u16.to_be_bytes());
    ip[10..12].copy_from_slice(&[0, 0]);
    let sum = net_wire::checksum(&ip[..20]);
    ip[10..12].copy_from_slice(&sum.to_be_bytes());
    let mut frame = [0u8; 64];
    let len = emit_ethernet(
        &mut frame,
        LOCAL_MAC,
        REMOTE_MAC,
        ETHERTYPE_IPV4,
        &ip[..ip_len],
    )
    .unwrap();
    assert_eq!(
        demux_frame(iface(), &frame[..len]),
        Err(StackError::Wire(WireError::Fragmented))
    );
}

#[test]
fn arbitrary_frame_bytes_never_panic() {
    let mut state = 0x6d2b_79f5u32;
    let mut bytes = [0u8; 320];
    for len in 0..20_000usize {
        let size = len % bytes.len();
        for byte in &mut bytes[..size] {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            *byte = state as u8;
        }
        let _ = demux_frame(iface(), &bytes[..size]);
    }
}
