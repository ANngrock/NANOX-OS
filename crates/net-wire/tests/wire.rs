use net_wire::*;

const MAC_A: MacAddr = MacAddr([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
const MAC_B: MacAddr = MacAddr([0x52, 0x54, 0x00, 0xab, 0xcd, 0xef]);
const IP_A: Ipv4Addr = Ipv4Addr([10, 0, 2, 15]);
const IP_B: Ipv4Addr = Ipv4Addr([10, 0, 2, 2]);

/// Well-known valid header (checksum 0xb861) used in many RFC 1071 walkthroughs.
const REFERENCE_IPV4_HEADER: [u8; 20] = [
    0x45, 0x00, 0x00, 0x73, 0x00, 0x00, 0x40, 0x00, 0x40, 0x11, 0xb8, 0x61, 0xc0, 0xa8, 0x00, 0x01,
    0xc0, 0xa8, 0x00, 0xc7,
];

fn ipv4(payload: &[u8], protocol: u8) -> ([u8; 1500], usize) {
    let mut buf = [0u8; 1500];
    let header = Ipv4Emit {
        src: IP_A,
        dst: IP_B,
        protocol,
        ttl: 64,
        ident: 7,
    };
    let len = emit_ipv4(&mut buf, &header, payload).unwrap();
    (buf, len)
}

fn refresh_ipv4_checksum(packet: &mut [u8]) {
    let header_len = (packet[0] & 0x0f) as usize * 4;
    packet[10] = 0;
    packet[11] = 0;
    let sum = checksum(&packet[..header_len]);
    packet[10..12].copy_from_slice(&sum.to_be_bytes());
}

#[test]
fn rfc1071_checksum_example_and_split_input() {
    // RFC 1071 section 3 example: sum ddf2, checksum is its complement.
    let data = [0x00, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
    assert_eq!(checksum(&data), !0xddf2);
    // Odd-length pieces must give the same result as one contiguous buffer.
    let mut split = Checksum::new();
    split.add(&data[..3]);
    split.add(&[]);
    split.add(&data[3..5]);
    split.add(&data[5..]);
    assert_eq!(split.finish(), checksum(&data));
    assert_eq!(checksum(&REFERENCE_IPV4_HEADER), 0);
}

#[test]
fn ethernet_round_trip_and_rejections() {
    let mut buf = [0u8; 64];
    let len = emit_ethernet(
        &mut buf,
        MacAddr::BROADCAST,
        MAC_A,
        ETHERTYPE_ARP,
        &[1, 2, 3],
    )
    .unwrap();
    assert_eq!(len, 17);
    let frame = parse_ethernet(&buf[..len]).unwrap();
    assert_eq!(
        (frame.dst, frame.src, frame.ethertype),
        (MacAddr::BROADCAST, MAC_A, ETHERTYPE_ARP)
    );
    assert_eq!(frame.payload, &[1, 2, 3]);

    assert_eq!(parse_ethernet(&buf[..13]), Err(WireError::Truncated));
    for tag in [0x8100u16, 0x88a8] {
        let mut vlan = buf;
        vlan[12..14].copy_from_slice(&tag.to_be_bytes());
        assert_eq!(
            parse_ethernet(&vlan[..len]),
            Err(WireError::UnsupportedFrame),
            "tag {tag:#06x}"
        );
        assert_eq!(
            emit_ethernet(&mut [0u8; 64], MAC_B, MAC_A, tag, &[]),
            Err(WireError::UnsupportedFrame),
            "tag {tag:#06x}"
        );
    }
    let mut ieee8023 = buf;
    ieee8023[12..14].copy_from_slice(&0x05dcu16.to_be_bytes());
    assert_eq!(
        parse_ethernet(&ieee8023[..len]),
        Err(WireError::UnsupportedFrame)
    );
    let mut group_src = buf;
    group_src[6] |= 1;
    assert_eq!(
        parse_ethernet(&group_src[..len]),
        Err(WireError::BadAddress)
    );

    assert_eq!(
        emit_ethernet(&mut buf[..16], MAC_B, MAC_A, ETHERTYPE_IPV4, &[1, 2, 3]),
        Err(WireError::BufferTooSmall)
    );
    assert_eq!(
        emit_ethernet(&mut buf, MAC_B, MacAddr::BROADCAST, ETHERTYPE_IPV4, &[]),
        Err(WireError::BadAddress)
    );
}

#[test]
fn arp_round_trip_padding_and_rejections() {
    let request = ArpPacket {
        op: ArpOp::Request,
        sender_mac: MAC_A,
        sender_ip: IP_A,
        target_mac: MacAddr([0; 6]),
        target_ip: IP_B,
    };
    let mut buf = [0u8; 46]; // Ethernet minimum payload: 28 bytes of ARP plus padding.
    assert_eq!(emit_arp(&mut buf, &request), Ok(ARP_LEN));
    assert_eq!(parse_arp(&buf), Ok(request));
    assert_eq!(parse_arp(&buf[..ARP_LEN - 1]), Err(WireError::Truncated));

    for (offset, value) in [(1, 6u8), (3, 0xdd), (4, 8), (5, 16), (7, 3)] {
        let mut bad = buf;
        bad[offset] = value;
        assert_eq!(
            parse_arp(&bad),
            Err(WireError::UnsupportedArp),
            "byte {offset}"
        );
    }
    for group_ip in [Ipv4Addr::BROADCAST, Ipv4Addr([224, 0, 0, 1])] {
        let mut group_sender = buf;
        group_sender[14..18].copy_from_slice(&group_ip.0);
        assert_eq!(parse_arp(&group_sender), Err(WireError::BadAddress));
        let bad = ArpPacket {
            sender_ip: group_ip,
            ..request
        };
        assert_eq!(emit_arp(&mut [0u8; 28], &bad), Err(WireError::BadAddress));
    }
    for group_mac in [MacAddr::BROADCAST, MacAddr([0x01, 0x00, 0x5e, 0, 0, 1])] {
        let mut group_sender = buf;
        group_sender[8..14].copy_from_slice(&group_mac.0);
        assert_eq!(parse_arp(&group_sender), Err(WireError::BadAddress));
        let bad = ArpPacket {
            sender_mac: group_mac,
            ..request
        };
        assert_eq!(emit_arp(&mut [0u8; 28], &bad), Err(WireError::BadAddress));
    }
    // A broadcast target is normal for requests and stays accepted.
    let mut broadcast_target = buf;
    broadcast_target[18..24].copy_from_slice(&MacAddr::BROADCAST.0);
    assert_eq!(
        parse_arp(&broadcast_target).map(|p| p.target_mac),
        Ok(MacAddr::BROADCAST)
    );
    assert_eq!(
        emit_arp(&mut buf[..27], &request),
        Err(WireError::BufferTooSmall)
    );
}

#[test]
fn ipv4_round_trip_drops_link_padding() {
    let (mut buf, len) = ipv4(&[0xaa; 5], IPPROTO_UDP);
    assert_eq!(len, 25);
    // Bytes beyond total length (Ethernet padding) are not payload.
    buf[len..len + 3].copy_from_slice(&[0xee; 3]);
    let packet = parse_ipv4(&buf[..len + 3]).unwrap();
    assert_eq!(packet.payload, &[0xaa; 5]);
    assert_eq!(
        (packet.src, packet.dst, packet.protocol, packet.ttl),
        (IP_A, IP_B, IPPROTO_UDP, 64)
    );
    assert!(packet.dont_fragment);
    assert!(packet.options.is_empty());
    let mut reference_packet = [0u8; 0x73];
    reference_packet[..20].copy_from_slice(&REFERENCE_IPV4_HEADER);
    let reference = parse_ipv4(&reference_packet).unwrap();
    assert_eq!(reference.payload.len(), 0x73 - 20);
}

#[test]
fn ipv4_header_field_rejections() {
    let (buf, len) = ipv4(&[1, 2, 3, 4], IPPROTO_UDP);
    let good = &buf[..len];
    assert_eq!(parse_ipv4(&good[..19]), Err(WireError::Truncated));

    let mut v6 = [0u8; 24];
    v6.copy_from_slice(good);
    v6[0] = 0x65;
    assert_eq!(parse_ipv4(&v6), Err(WireError::BadVersion));

    for ihl in 0..5u8 {
        let mut short_ihl = [0u8; 24];
        short_ihl.copy_from_slice(good);
        short_ihl[0] = 0x40 | ihl;
        assert_eq!(
            parse_ipv4(&short_ihl),
            Err(WireError::BadHeaderLength),
            "ihl {ihl}"
        );
    }

    // IHL claims 60 bytes of header in a 24-byte packet.
    let mut long_ihl = [0u8; 24];
    long_ihl.copy_from_slice(good);
    long_ihl[0] = 0x4f;
    assert_eq!(parse_ipv4(&long_ihl), Err(WireError::Truncated));

    let mut short_total = [0u8; 24];
    short_total.copy_from_slice(good);
    short_total[2..4].copy_from_slice(&19u16.to_be_bytes());
    refresh_ipv4_checksum(&mut short_total);
    assert_eq!(parse_ipv4(&short_total), Err(WireError::BadLength));

    let mut long_total = [0u8; 24];
    long_total.copy_from_slice(good);
    long_total[2..4].copy_from_slice(&25u16.to_be_bytes());
    refresh_ipv4_checksum(&mut long_total);
    assert_eq!(parse_ipv4(&long_total), Err(WireError::Truncated));

    let mut corrupt = [0u8; 24];
    corrupt.copy_from_slice(good);
    corrupt[8] ^= 1;
    assert_eq!(parse_ipv4(&corrupt), Err(WireError::BadChecksum));

    let mut group_src = [0u8; 24];
    group_src.copy_from_slice(good);
    group_src[12..16].copy_from_slice(&[224, 0, 0, 1]);
    refresh_ipv4_checksum(&mut group_src);
    assert_eq!(parse_ipv4(&group_src), Err(WireError::BadAddress));

    let mut broadcast_src = [0u8; 24];
    broadcast_src.copy_from_slice(good);
    broadcast_src[12..16].copy_from_slice(&Ipv4Addr::BROADCAST.0);
    refresh_ipv4_checksum(&mut broadcast_src);
    assert_eq!(parse_ipv4(&broadcast_src), Err(WireError::BadAddress));
    // Broadcast and multicast remain valid destinations.
    let mut broadcast_dst = [0u8; 24];
    broadcast_dst.copy_from_slice(good);
    broadcast_dst[16..20].copy_from_slice(&Ipv4Addr::BROADCAST.0);
    refresh_ipv4_checksum(&mut broadcast_dst);
    assert_eq!(
        parse_ipv4(&broadcast_dst).map(|p| p.dst),
        Ok(Ipv4Addr::BROADCAST)
    );
}

#[test]
fn ipv4_fragment_and_flag_policy() {
    let (buf, len) = ipv4(&[1, 2, 3, 4], IPPROTO_UDP);
    let with_flags = |flags: u16| {
        let mut packet = [0u8; 24];
        packet.copy_from_slice(&buf[..len]);
        packet[6..8].copy_from_slice(&flags.to_be_bytes());
        refresh_ipv4_checksum(&mut packet);
        parse_ipv4(&packet).map(|p| p.dont_fragment)
    };
    assert_eq!(with_flags(0x0000), Ok(false));
    assert_eq!(with_flags(0x4000), Ok(true));
    assert_eq!(with_flags(0x2000), Err(WireError::Fragmented)); // first fragment
    assert_eq!(with_flags(0x0001), Err(WireError::Fragmented)); // last fragment
    assert_eq!(with_flags(0x1fff), Err(WireError::Fragmented));
    assert_eq!(with_flags(0x8000), Err(WireError::ReservedFlag));
    assert_eq!(with_flags(0xa000), Err(WireError::ReservedFlag));
}

#[test]
fn ipv4_options_are_exposed_uninterpreted() {
    // IHL 6: one 4-byte option word (NOP, NOP, NOP, EOL) before the payload.
    let mut packet = [0u8; 28];
    packet[..20].copy_from_slice(&REFERENCE_IPV4_HEADER);
    packet[0] = 0x46;
    packet[2..4].copy_from_slice(&28u16.to_be_bytes());
    packet[20..24].copy_from_slice(&[1, 1, 1, 0]);
    packet[24..28].copy_from_slice(&[9, 8, 7, 6]);
    refresh_ipv4_checksum(&mut packet);
    let parsed = parse_ipv4(&packet).unwrap();
    assert_eq!(parsed.options, &[1, 1, 1, 0]);
    assert_eq!(parsed.payload, &[9, 8, 7, 6]);
    // Option bytes are covered by the header checksum.
    packet[21] = 0x44;
    assert_eq!(parse_ipv4(&packet), Err(WireError::BadChecksum));
}

#[test]
fn ipv4_emit_limits() {
    let header = Ipv4Emit {
        src: IP_A,
        dst: IP_B,
        protocol: IPPROTO_UDP,
        ttl: 64,
        ident: 0,
    };
    let mut buf = [0u8; 32];
    assert_eq!(
        emit_ipv4(&mut buf[..23], &header, &[0; 4]),
        Err(WireError::BufferTooSmall)
    );
    assert_eq!(
        emit_ipv4(&mut buf, &Ipv4Emit { ttl: 0, ..header }, &[]),
        Err(WireError::InvalidField)
    );
    assert_eq!(
        emit_ipv4(
            &mut buf,
            &Ipv4Emit {
                src: Ipv4Addr::BROADCAST,
                ..header
            },
            &[]
        ),
        Err(WireError::BadAddress)
    );
    let oversized = [0u8; 65516]; // 20 + 65516 > 65535
    let mut big = [0u8; 70000];
    assert_eq!(
        emit_ipv4(&mut big, &header, &oversized),
        Err(WireError::Overflow)
    );
    assert_eq!(emit_ipv4(&mut big, &header, &oversized[..65515]), Ok(65535));
}

#[test]
fn icmp_echo_round_trip_and_rejections() {
    let request = IcmpEcho {
        reply: false,
        ident: 0x1234,
        seq: 9,
        data: b"nanox",
    };
    let mut buf = [0u8; 32];
    let len = emit_icmp_echo(&mut buf, &request).unwrap();
    assert_eq!(len, 13); // odd length exercises the trailing checksum byte
    assert_eq!(parse_icmp_echo(&buf[..len]), Ok(request));
    assert_eq!(buf[0], 8);
    assert_eq!(parse_icmp_echo(&buf[..7]), Err(WireError::Truncated));

    // Reply echoes ident, sequence and data; type 0 on the wire.
    let reply = IcmpEcho {
        reply: true,
        ..request
    };
    let mut reply_buf = [0u8; 32];
    let reply_len = emit_icmp_echo(&mut reply_buf, &reply).unwrap();
    assert_eq!((reply_len, reply_buf[0], reply_buf[1]), (13, 0, 0));
    assert_eq!(parse_icmp_echo(&reply_buf[..reply_len]), Ok(reply));
    let empty_reply = IcmpEcho {
        reply: true,
        ident: 1,
        seq: 2,
        data: &[],
    };
    let empty_len = emit_icmp_echo(&mut reply_buf, &empty_reply).unwrap();
    assert_eq!(parse_icmp_echo(&reply_buf[..empty_len]), Ok(empty_reply));

    let mut corrupt = buf;
    corrupt[len - 1] ^= 0x80;
    assert_eq!(
        parse_icmp_echo(&corrupt[..len]),
        Err(WireError::BadChecksum)
    );

    // Destination unreachable with a valid checksum is well-formed but unsupported.
    let mut unreachable = [3u8, 1, 0, 0, 0, 0, 0, 0];
    let sum = checksum(&unreachable);
    unreachable[2..4].copy_from_slice(&sum.to_be_bytes());
    assert_eq!(
        parse_icmp_echo(&unreachable),
        Err(WireError::UnsupportedIcmp)
    );
    let mut echo_bad_code = [8u8, 1, 0, 0, 0, 0, 0, 0];
    let sum = checksum(&echo_bad_code);
    echo_bad_code[2..4].copy_from_slice(&sum.to_be_bytes());
    assert_eq!(
        parse_icmp_echo(&echo_bad_code),
        Err(WireError::UnsupportedIcmp)
    );
    assert_eq!(
        emit_icmp_echo(&mut buf[..12], &request),
        Err(WireError::BufferTooSmall)
    );
}

#[test]
fn udp_round_trip_through_all_layers() {
    let mut udp = [0u8; 64];
    let udp_len = emit_udp(&mut udp, IP_A, IP_B, 5000, 53, b"query").unwrap();
    let (ip, ip_len) = ipv4(&udp[..udp_len], IPPROTO_UDP);
    let mut frame = [0u8; 1514];
    let frame_len = emit_ethernet(&mut frame, MAC_B, MAC_A, ETHERTYPE_IPV4, &ip[..ip_len]).unwrap();

    let eth = parse_ethernet(&frame[..frame_len]).unwrap();
    assert_eq!(eth.ethertype, ETHERTYPE_IPV4);
    let packet = parse_ipv4(eth.payload).unwrap();
    assert_eq!(packet.protocol, IPPROTO_UDP);
    let datagram = parse_udp(packet.src, packet.dst, packet.payload).unwrap();
    assert_eq!((datagram.src_port, datagram.dst_port), (5000, 53));
    assert!(datagram.checksum_present);
    assert_eq!(datagram.payload, b"query");
}

#[test]
fn udp_length_checksum_and_port_rejections() {
    let mut buf = [0u8; 32];
    let len = emit_udp(&mut buf, IP_A, IP_B, 1, 2, &[1, 2, 3]).unwrap();
    let good = buf;

    assert_eq!(parse_udp(IP_A, IP_B, &good[..7]), Err(WireError::Truncated));
    // The pseudo-header binds the checksum to the IPv4 addresses.
    assert!(parse_udp(IP_A, IP_B, &good[..len]).is_ok());
    assert_eq!(
        parse_udp(IP_A, Ipv4Addr([10, 0, 2, 3]), &good[..len]),
        Err(WireError::BadChecksum)
    );

    let mut short_len = good;
    short_len[4..6].copy_from_slice(&7u16.to_be_bytes());
    assert_eq!(
        parse_udp(IP_A, IP_B, &short_len[..len]),
        Err(WireError::BadLength)
    );
    let mut long_len = good;
    long_len[4..6].copy_from_slice(&12u16.to_be_bytes());
    assert_eq!(
        parse_udp(IP_A, IP_B, &long_len[..len]),
        Err(WireError::Truncated)
    );

    let mut corrupt = good;
    corrupt[len - 1] ^= 0xff;
    assert_eq!(
        parse_udp(IP_A, IP_B, &corrupt[..len]),
        Err(WireError::BadChecksum)
    );
    // Checksum 0 means "not computed" over IPv4 and is accepted.
    corrupt[6..8].copy_from_slice(&[0, 0]);
    let unchecked = parse_udp(IP_A, IP_B, &corrupt[..len]).unwrap();
    assert!(!unchecked.checksum_present);

    let mut port_zero = [0u8; 8];
    port_zero[4..6].copy_from_slice(&8u16.to_be_bytes());
    assert_eq!(parse_udp(IP_A, IP_B, &port_zero), Err(WireError::BadPort));
    assert_eq!(
        emit_udp(&mut buf, IP_A, IP_B, 1, 0, &[]),
        Err(WireError::BadPort)
    );
    assert_eq!(
        emit_udp(&mut buf[..10], IP_A, IP_B, 1, 2, &[1, 2, 3]),
        Err(WireError::BufferTooSmall)
    );
    let oversized = [0u8; 65528];
    let mut big = [0u8; 70000];
    assert_eq!(
        emit_udp(&mut big, IP_A, IP_B, 1, 2, &oversized),
        Err(WireError::Overflow)
    );
}

#[test]
fn udp_zero_checksum_is_sent_as_all_ones() {
    assert_eq!(udp_checksum_field(0), 0xffff);
    assert_eq!(udp_checksum_field(0x1234), 0x1234);
    // Find a payload whose computed checksum is zero and confirm the wire value
    // is 0xffff and still verifies.
    let mut buf = [0u8; 16];
    for word in 0..=u16::MAX {
        let len = emit_udp(&mut buf, IP_A, IP_B, 1, 2, &word.to_be_bytes()).unwrap();
        if buf[6..8] == [0xff, 0xff] {
            assert!(parse_udp(IP_A, IP_B, &buf[..len]).unwrap().checksum_present);
            return;
        }
    }
    panic!("no payload produced a zero UDP checksum");
}

#[test]
fn every_truncation_is_an_error_not_a_panic() {
    let mut udp = [0u8; 64];
    let udp_len = emit_udp(&mut udp, IP_A, IP_B, 5000, 53, b"abc").unwrap();
    let (ip, ip_len) = ipv4(&udp[..udp_len], IPPROTO_UDP);
    for cut in 0..ip_len {
        assert!(parse_ipv4(&ip[..cut]).is_err(), "ipv4 cut {cut}");
    }
    for cut in 0..udp_len {
        assert!(parse_udp(IP_A, IP_B, &udp[..cut]).is_err(), "udp cut {cut}");
    }
    for cut in 0..ARP_LEN {
        assert!(parse_arp(&[0u8; ARP_LEN][..cut]).is_err());
    }
    for cut in 0..ICMP_ECHO_HEADER_LEN {
        assert!(parse_icmp_echo(&[0u8; 8][..cut]).is_err());
    }
    for cut in 0..ETHERNET_HEADER_LEN {
        assert!(parse_ethernet(&[0u8; 14][..cut]).is_err());
    }
}

#[test]
fn arbitrary_bytes_never_panic() {
    // Deterministic xorshift input; checks absence of panics on any shape.
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut buf = [0u8; 96];
    for round in 0..20_000 {
        for byte in buf.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state as u8;
        }
        // Make IPv4-looking input common so deeper checks are reached.
        if round % 2 == 0 {
            buf[0] = 0x45 + (round as u8 % 11);
        }
        let len = (state as usize) % buf.len();
        let input = &buf[..len];
        let _ = parse_ethernet(input);
        let _ = parse_arp(input);
        let _ = parse_ipv4(input);
        let _ = parse_icmp_echo(input);
        let _ = parse_udp(IP_A, IP_B, input);
    }
}
