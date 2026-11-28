use crate::{IpAddress, IpEndpoint};
use smoltcp::wire::{
    EthernetFrame, EthernetProtocol, IpProtocol, Ipv4Packet, Ipv6Packet, TcpPacket, TcpRepr, UdpPacket, UdpRepr,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Tuple {
    pub local: IpEndpoint,
    pub remote: IpEndpoint,
}
impl Tuple {
    pub fn reversed(self) -> Self {
        Self { local: self.remote, remote: self.local }
    }
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct TxTrace {
    pub tuple: Tuple,
    pub probe: bool,
}
pub(crate) struct TcpPacketInfo {
    pub tuple: Tuple,
    pub syn: bool,
    pub rst: bool,
    pub mss: Option<u16>,
    pub probe: bool,
}

/// Only checksum-valid, unfragmented TCP headers participate in filtering.
/// The returned tuple is in outgoing orientation: local is the source.
pub(crate) fn tcp_packet(data: &[u8], ethernet: bool) -> Option<TcpPacketInfo> {
    let (src, dst, payload) = transport_packet(data, ethernet, IpProtocol::Tcp)?;
    let tcp = TcpPacket::new_checked(payload).ok()?;
    if !tcp.verify_checksum(&src, &dst) {
        return None;
    }
    let repr = TcpRepr::parse(&tcp, &src, &dst, &smoltcp::phy::ChecksumCapabilities::default()).ok()?;
    Some(TcpPacketInfo {
        tuple: Tuple { local: IpEndpoint::new(src, tcp.src_port()), remote: IpEndpoint::new(dst, tcp.dst_port()) },
        probe: tcp.ack() && !tcp.syn() && !tcp.fin() && !tcp.rst() && repr.payload == [0],
        syn: tcp.syn() && !tcp.ack(),
        rst: tcp.rst(),
        mss: if tcp.syn() { Some(repr.max_seg_size.unwrap_or(536).max(48)) } else { None },
    })
}

/// Incoming destination for ICMP's source-only packet metadata. smoltcp
/// validates the packet before its receive-queue growth commits this value.
pub(crate) fn packet_destination(data: &[u8], ethernet: bool) -> Option<IpAddress> {
    let data = if ethernet { data.get(14..)? } else { data };
    match data.first()? >> 4 {
        4 => Some(IpAddress::Ipv4(Ipv4Packet::new_checked(data).ok()?.dst_addr())),
        6 => Some(IpAddress::Ipv6(Ipv6Packet::new_checked(data).ok()?.dst_addr())),
        _ => None,
    }
}

/// Return only valid UDP tuples; checksum/extension handling matches smoltcp.
pub(crate) fn udp_packet(data: &[u8], ethernet: bool) -> Option<Tuple> {
    let (src, dst, payload) = transport_packet(data, ethernet, IpProtocol::Udp)?;
    let packet = UdpPacket::new_checked(payload).ok()?;
    let repr = UdpRepr::parse(&packet, &src, &dst, &smoltcp::phy::ChecksumCapabilities::default()).ok()?;
    Some(Tuple { local: IpEndpoint::new(src, repr.src_port), remote: IpEndpoint::new(dst, repr.dst_port) })
}
fn transport_packet(data: &[u8], ethernet: bool, protocol: IpProtocol) -> Option<(IpAddress, IpAddress, &[u8])> {
    let data = if ethernet {
        let frame = EthernetFrame::new_checked(data).ok()?;
        if !matches!(frame.ethertype(), EthernetProtocol::Ipv4 | EthernetProtocol::Ipv6) {
            return None;
        }
        &data[14..]
    } else {
        data
    };
    match data.first()? >> 4 {
        4 => {
            let ip = Ipv4Packet::new_checked(data).ok()?;
            if ip.next_header() != protocol || !ip.verify_checksum() || ip.more_frags() || ip.frag_offset() != 0 {
                return None;
            }
            let start = ip.header_len() as usize;
            Some((
                IpAddress::Ipv4(ip.src_addr()),
                IpAddress::Ipv4(ip.dst_addr()),
                &data[start..ip.total_len() as usize],
            ))
        }
        6 => {
            let ip = Ipv6Packet::new_checked(data).ok()?;
            let mut next = ip.next_header();
            let mut start = 40;
            let end = 40 + ip.payload_len() as usize;
            // The pinned smoltcp accepts one leading Hop-by-Hop header.
            if next == IpProtocol::HopByHop {
                next = IpProtocol::from(*data.get(start)?);
                start += (usize::from(*data.get(start + 1)?) + 1) * 8;
            }
            if next != protocol || start > end {
                return None;
            }
            Some((IpAddress::Ipv6(ip.src_addr()), IpAddress::Ipv6(ip.dst_addr()), &data[start..end]))
        }
        _ => None,
    }
}

/// Correct only emitted ICMP source addresses and dependent checksums.
/// Neighbor discovery/ARP and other protocols keep their original headers.
pub(crate) fn icmp_source(frame: &mut [u8], ethernet: bool, source: IpAddress) {
    let data = if ethernet {
        let Ok(eth) = EthernetFrame::new_checked(&*frame) else { return };
        if !matches!(eth.ethertype(), EthernetProtocol::Ipv4 | EthernetProtocol::Ipv6) {
            return;
        }
        &mut frame[14..]
    } else {
        frame
    };
    match source {
        IpAddress::Ipv4(source) => {
            let Ok(mut ip) = Ipv4Packet::new_checked(data) else { return };
            if ip.version() != 4 || ip.next_header() != IpProtocol::Icmp || ip.payload_mut().first() != Some(&8) {
                return;
            }
            ip.set_src_addr(source);
            ip.fill_checksum();
        }
        IpAddress::Ipv6(source) => {
            let Ok(mut ip) = Ipv6Packet::new_checked(data) else { return };
            if ip.version() != 6 || ip.next_header() != IpProtocol::Icmpv6 || ip.payload_mut().first() != Some(&128) {
                return;
            }
            let destination = ip.dst_addr();
            ip.set_src_addr(source);
            if let Ok(mut icmp) = smoltcp::wire::Icmpv6Packet::new_checked(ip.payload_mut()) {
                icmp.fill_checksum(&source, &destination);
            }
        }
    }
}
