//! Single-interface Ethernet/IPv4 protocol demultiplexer.

use net_tcp::segment::{parse_tcp, SegmentError, TcpSegment};
use net_wire::{
    parse_arp, parse_ethernet, parse_icmp_echo, parse_ipv4, parse_udp, ArpPacket, EthernetFrame,
    IcmpEcho, Ipv4Addr, Ipv4Packet, MacAddr, UdpDatagram, WireError, ETHERTYPE_ARP, ETHERTYPE_IPV4,
    IPPROTO_ICMP, IPPROTO_TCP, IPPROTO_UDP,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackError {
    Wire(WireError),
    Tcp(SegmentError),
    InvalidTtl,
    AddressMismatch,
}

impl From<WireError> for StackError {
    fn from(value: WireError) -> Self {
        Self::Wire(value)
    }
}

impl From<SegmentError> for StackError {
    fn from(value: SegmentError) -> Self {
        Self::Tcp(value)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboundPacket<'a> {
    Arp {
        ethernet_source: MacAddr,
        packet: ArpPacket,
    },
    Icmp {
        source: Ipv4Addr,
        destination: Ipv4Addr,
        packet: IcmpEcho<'a>,
    },
    Udp {
        source: Ipv4Addr,
        destination: Ipv4Addr,
        packet: UdpDatagram<'a>,
    },
    Tcp {
        source: Ipv4Addr,
        destination: Ipv4Addr,
        packet: TcpSegment<'a>,
    },
}

#[derive(Clone, Copy)]
pub struct Interface {
    pub mac: MacAddr,
    pub ip: Ipv4Addr,
}

/// Parse and classify one frame for a single IPv4 interface. Frames for other
/// hosts and unknown EtherTypes/IP protocols are ignored. Structural errors
/// in traffic addressed to this interface are returned to the caller.
pub fn demux_frame<'a>(
    interface: Interface,
    frame: &'a [u8],
) -> Result<Option<InboundPacket<'a>>, StackError> {
    let eth = parse_ethernet(frame)?;
    match eth.ethertype {
        ETHERTYPE_ARP => demux_arp(interface, eth).map(|packet| {
            packet.map(|(ethernet_source, packet)| InboundPacket::Arp {
                ethernet_source,
                packet,
            })
        }),
        ETHERTYPE_IPV4 => demux_ipv4(interface, eth),
        _ => Ok(None),
    }
}

fn demux_arp(
    interface: Interface,
    eth: EthernetFrame<'_>,
) -> Result<Option<(MacAddr, ArpPacket)>, StackError> {
    if eth.dst != interface.mac && eth.dst != MacAddr::BROADCAST {
        return Ok(None);
    }
    let packet = parse_arp(eth.payload)?;
    if packet.sender_mac != eth.src {
        return Err(StackError::AddressMismatch);
    }
    if packet.target_ip != interface.ip {
        return Ok(None);
    }
    if matches!(packet.op, net_wire::ArpOp::Reply) && packet.target_mac != interface.mac {
        return Ok(None);
    }
    Ok(Some((eth.src, packet)))
}

fn demux_ipv4<'a>(
    interface: Interface,
    eth: EthernetFrame<'a>,
) -> Result<Option<InboundPacket<'a>>, StackError> {
    if eth.dst != interface.mac {
        return Ok(None);
    }
    let ip = parse_ipv4(eth.payload)?;
    if ip.dst != interface.ip {
        return Ok(None);
    }
    if ip.ttl == 0 {
        return Err(StackError::InvalidTtl);
    }
    match ip.protocol {
        IPPROTO_ICMP => Ok(Some(InboundPacket::Icmp {
            source: ip.src,
            destination: ip.dst,
            packet: parse_icmp_echo(ip.payload)?,
        })),
        IPPROTO_UDP => Ok(Some(InboundPacket::Udp {
            source: ip.src,
            destination: ip.dst,
            packet: parse_udp(ip.src, ip.dst, ip.payload)?,
        })),
        IPPROTO_TCP => Ok(Some(InboundPacket::Tcp {
            source: ip.src,
            destination: ip.dst,
            packet: parse_tcp(ip.src, ip.dst, ip.payload)?,
        })),
        _ => Ok(None),
    }
}

/// Useful to consumers that need the parsed IPv4 metadata alongside a
/// protocol payload without reparsing the packet.
pub fn parse_local_ipv4<'a>(
    interface: Interface,
    frame: &'a [u8],
) -> Result<Option<Ipv4Packet<'a>>, StackError> {
    let eth = parse_ethernet(frame)?;
    if eth.ethertype != ETHERTYPE_IPV4 || eth.dst != interface.mac {
        return Ok(None);
    }
    let packet = parse_ipv4(eth.payload)?;
    if packet.dst != interface.ip {
        return Ok(None);
    }
    if packet.ttl == 0 {
        return Err(StackError::InvalidTtl);
    }
    Ok(Some(packet))
}
