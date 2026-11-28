use super::*;
use alloc::boxed::Box;
struct TestDevice {
    frames: usize,
    received: usize,
    ready: bool,
    last: [u8; ETH_FRAME_MAX],
    last_len: usize,
}
impl NetDevice for TestDevice {
    fn info(&self) -> NetInfo {
        NetInfo { mac: [2, 0, 0, 0, 0, 1], mtu: 1200, link_up: true }
    }
    fn tx_ready(&self) -> bool {
        self.ready
    }
    fn receive(&mut self, out: &mut [u8; ETH_FRAME_MAX]) -> Option<usize> {
        if self.frames == 0 {
            return None;
        }
        self.frames -= 1;
        self.received += 1;
        out[..14].fill(0);
        Some(14)
    }
    fn reclaim(&mut self) -> usize {
        0
    }
    fn transmit<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, TxError> {
        if !self.ready {
            return Err(TxError::Full);
        }
        self.last_len = len;
        Ok(f(&mut self.last[..len]))
    }
}
fn stack() -> Stack<TestDevice> {
    Stack::try_new(Loopback::try_new(65535, 16).unwrap(), Instant::ZERO, 459).unwrap()
}
fn budget() -> &'static Budget {
    Box::leak(Box::new(Budget::new(32 * 1024 * 1024)))
}
fn tcp(b: &'static Budget) -> TcpPrep {
    prepare_tcp(b, BufSizes { rx: 64, tx: 64 }).unwrap()
}
fn eth(s: &mut Stack<TestDevice>, frames: usize) {
    s.attach_eth(
        TestDevice { frames, received: 0, ready: true, last: [0; ETH_FRAME_MAX], last_len: 0 },
        EthConfig::Static { addr: Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 1), 24), router: None },
        Instant::ZERO,
        73,
    )
    .unwrap();
}
#[test]
fn ethernet_receive_budget_requests_another_round_for_seventeenth_frame() {
    let mut s = stack();
    eth(&mut s, 17);
    assert!(s.poll_round(Instant::ZERO).more);
    assert_eq!(s.eth.as_ref().unwrap().device.received, 16);
    assert!(!s.poll_round(Instant::ZERO).more);
    assert_eq!(s.eth.as_ref().unwrap().device.received, 17);
}
#[test]
fn socket_generation_wrap_and_live_list_removal() {
    let mut s = stack();
    s.generations[0] = u32::MAX;
    let id = s.open(Family::V4, Proto::Tcp).unwrap();
    assert_eq!(id.generation, 0);
    assert_eq!(s.live, &[0]);
    s.close(id, false, Instant::ZERO);
    assert!(s.live.is_empty());
    s.changed();
    assert_eq!(s.open(Family::V4, Proto::Tcp).unwrap().generation, 1);
}
#[test]
fn bind_and_pool_installation_roll_back_on_failure() {
    let mut s = stack();
    eth(&mut s, 0);
    let b = budget();
    let id = s.open(Family::V4, Proto::Udp).unwrap();
    let first = prepare_udp(b, BufSizes::default()).unwrap();
    let mut bad = prepare_udp(b, BufSizes::default()).unwrap();
    bad.socket.bind(7777).unwrap();
    assert_eq!(
        s.bind(
            id,
            BindReq { port: 1234, ..BindReq::default() },
            Preps::Udp(&mut [first, bad].into_iter()),
            Instant::ZERO
        ),
        Err(NetError::Inval)
    );
    assert_eq!(s.port_count(), 0);
    assert_eq!(s.socket_count(), 0);
    assert_eq!(b.used(), 0);
    assert!(s.entry(id).unwrap().bound.is_none());
    let l = s.open(Family::V4, Proto::Tcp).unwrap();
    s.bind(l, BindReq { port: 1234, ..BindReq::default() }, Preps::None, Instant::ZERO).unwrap();
    s.ports = PortTable::try_new(1).unwrap();
    s.ports.bind(l, Proto::Tcp, Bound { port: 1234, ..Bound::default() }).unwrap();
    let entries = s.entry_count();
    assert_eq!(s.install_pool(l, IfaceId::Lo, tcp(b), Instant::ZERO), Err(NetError::NoBufs));
    assert_eq!(s.entry_count(), entries);
    assert_eq!(s.socket_count(), 0);
    assert_eq!(s.live.len(), entries);
    assert_eq!(b.used(), 0);
}
#[test]
fn accept_failure_keeps_child_queued() {
    let mut s = stack();
    let b = budget();
    let l = s.open(Family::V4, Proto::Tcp).unwrap();
    s.listen(l, 0, &mut [tcp(b)].into_iter(), Instant::ZERO).unwrap();
    let idx = *s.live.last().unwrap();
    let c = s.entries[idx].as_mut().unwrap();
    c.role = Role::Queued(l, 1);
    let id = c.id;
    assert_eq!(s.accept(l), Err(NetError::NotConn));
    assert_eq!(s.entry(id).unwrap().role, Role::Queued(l, 1));
}
#[test]
fn somaxconn_plus_one_clamps_and_refill_reports_capacity_failure() {
    // Linux v6.12 net/socket.c, __sys_listen_socket:1883-1891 uses an unsigned
    // backlog comparison. The public helper also covers negative inputs.
    let mut s = stack();
    let b = budget();
    let l = s.open(Family::V4, Proto::Tcp).unwrap();
    let pool: Vec<_> = (0..8).map(|_| tcp(b)).collect();
    s.listen(l, SOMAXCONN + 1, &mut pool.into_iter(), Instant::ZERO).unwrap();
    assert_eq!(s.entry(l).unwrap().role, Role::Listener(SOMAXCONN));
    assert_eq!(s.preps_needed(l, PrepFor::Listen(SOMAXCONN + 1)), 0);
    assert_eq!(clamp_backlog((SOMAXCONN + 1) as i32), SOMAXCONN);
    let child = s.entries[*s.live.last().unwrap()].as_ref().unwrap().id;
    s.remove(child, false, Instant::ZERO);
    s.interfaces[0].as_mut().unwrap().count = NET_SOCKETS;
    let used = b.used();
    assert_eq!(s.refill(l, IfaceId::Lo, tcp(b), Instant::ZERO), Err(NetError::NoBufs));
    assert_eq!(b.used(), used);
}
#[test]
fn configure_eth_reports_route_capacity_error() {
    let mut s = stack();
    eth(&mut s, 0);
    s.interfaces[1].as_mut().unwrap().iface.routes_mut().update(|r| {
        for n in [1, 2] {
            r.push(smoltcp::iface::Route {
                cidr: IpCidr::new(IpAddress::v4(192, 0, n, 0), 24),
                via_router: IpAddress::v4(10, 0, 0, 2),
                preferred_until: None,
                expires_at: None,
            })
            .unwrap();
        }
    });
    assert_eq!(
        s.configure_eth(Some(Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 1), 24)), Some(Ipv4Address::new(10, 0, 0, 2))),
        Err(NetError::NoBufs)
    );
}
#[test]
fn time_wait_deadline_is_poll_at_and_bind_expires_without_poll() {
    // Linux v6.12 net/ipv4/inet_timewait_sock.c, inet_twsk_alloc:205-206
    // preserves creator reuse flags; expiry releases the reservation.
    let mut s = stack();
    let id = s.open(Family::V4, Proto::Tcp).unwrap();
    let req = BindReq { port: 1234, ..BindReq::default() };
    s.bind(id, req, Preps::None, Instant::ZERO).unwrap();
    s.entries[id.index()].as_mut().unwrap().time_wait_until = Some(Instant::from_secs(10));
    s.remove(id, true, Instant::ZERO);
    s.poll_round(Instant::ZERO);
    assert_eq!(s.poll_at(Instant::ZERO), Some(Instant::from_secs(10)));
    let n = s.open(Family::V4, Proto::Tcp).unwrap();
    assert_eq!(s.bind(n, req, Preps::None, Instant::from_millis(9999)), Err(NetError::AddrInUse));
    s.bind(n, req, Preps::None, Instant::from_secs(10)).unwrap();
}
#[test]
fn orphan_removal_waits_for_egress_capacity() {
    let mut s = stack();
    eth(&mut s, 0);
    let b = budget();
    let id = s.open(Family::V4, Proto::Tcp).unwrap();
    let (h, charge) = s.interfaces[1].as_mut().unwrap().add(ConnectPrep::Tcp(tcp(b))).unwrap();
    let e = s.entry_mut(id).unwrap();
    e.handles[1] = Some(h);
    e.charges[1] = Some(charge);
    e.iface = Some(IfaceId::Eth0);
    s.entry_mut(id).unwrap().role = Role::Orphan;
    s.eth.as_mut().unwrap().device.ready = false;
    s.poll_round(Instant::ZERO);
    assert_eq!(s.socket_count(), 1);
    assert_eq!(b.used(), 128);
    s.eth.as_mut().unwrap().device.ready = true;
    s.poll_round(Instant::ZERO);
    assert_eq!(s.socket_count(), 0);
    assert_eq!(b.used(), 0);
}
#[test]
fn udp_ttl_and_maxseg_use_the_device_ip_mtu() {
    let mut s = stack();
    eth(&mut s, 0);
    let b = budget();
    let u = s.open(Family::V4, Proto::Udp).unwrap();
    s.set_opt(u, NetOpt::Ttl(42), Instant::ZERO).unwrap();
    let req = BindReq { addr: Some(IpAddress::v4(10, 0, 0, 1)), port: 1234, ..BindReq::default() };
    s.bind(u, req, Preps::Udp(&mut [prepare_udp(b, BufSizes::default()).unwrap()].into_iter()), Instant::ZERO).unwrap();
    let h = s.entry(u).unwrap().handles[1].unwrap();
    assert_eq!(s.interfaces[1].as_ref().unwrap().sockets.get::<udp::Socket>(h).hop_limit(), Some(42));
    s.set_opt(u, NetOpt::Ttl(37), Instant::from_secs(1)).unwrap();
    assert_eq!(s.interfaces[1].as_ref().unwrap().sockets.get::<udp::Socket>(h).hop_limit(), Some(37));
    let to = Some(IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234));
    assert_eq!(s.send(u, &[0; 1173], to, Preps::None, Instant::ZERO), Err(NetError::MsgSize));
    assert_eq!(s.send(u, &[0; 1172], to, Preps::None, Instant::ZERO), Ok(1172));
    let c = s.open(Family::V4, Proto::Tcp).unwrap();
    s.entry_mut(c).unwrap().iface = Some(IfaceId::Eth0);
    s.entry_mut(c).unwrap().remote_mss = 1500;
    assert_eq!(s.get_opt(c, OptQuery::MaxSeg), Ok(NetOpt::MaxSeg(1160)));
}

#[test]
fn hop_by_hop_ipv6_syn_cannot_steal_an_ipv4_listen_slot() {
    use smoltcp::phy::TxToken;
    let mut lo = Loopback::try_new(65535, 16).unwrap();
    let addr = IpAddress::Ipv6(Ipv6Address::LOCALHOST);
    lo.inner.transmit(Instant::ZERO).unwrap().consume(68, |p| {
        p.fill(0);
        p[0] = 0x60;
        p[5] = 28;
        p[6] = 0;
        p[7] = 64;
        p[23] = 1;
        p[39] = 1;
        p[40] = 6;
        p[48..50].copy_from_slice(&3000u16.to_be_bytes());
        p[50..52].copy_from_slice(&1234u16.to_be_bytes());
        p[60] = 0x50;
        p[61] = 2;
        p[62] = 0x10;
        smoltcp::wire::TcpPacket::new_unchecked(&mut p[48..]).fill_checksum(&addr, &addr);
    });
    let mut s: Stack<TestDevice> = Stack::try_new(lo, Instant::ZERO, 459).unwrap();
    let b = budget();
    let l = s.open(Family::V4, Proto::Tcp).unwrap();
    s.bind(l, BindReq { port: 1234, ..BindReq::default() }, Preps::None, Instant::ZERO).unwrap();
    s.listen(l, 0, &mut [tcp(b)].into_iter(), Instant::ZERO).unwrap();
    let c = s.open(Family::V4, Proto::Tcp).unwrap();
    assert_eq!(
        s.connect(c, IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), 1234), Some(ConnectPrep::Tcp(tcp(b))), Instant::ZERO),
        Err(NetError::InProgress)
    );
    for n in 0..16 {
        s.poll_round(Instant::from_millis(n));
    }
    let (_, peer) = s.accept(l).unwrap();
    assert!(peer.addr.is_ipv4());
    assert!(s.readiness(c).writable);
}
#[test]
fn loopback_full_ring_frees_a_slot_for_reply_and_preserves_fifo() {
    use smoltcp::phy::{RxToken, TxToken};
    let mut lo = LoopDevice::try_new(64, 2).unwrap();
    for b in [1, 2] {
        lo.transmit(Instant::ZERO).unwrap().consume(1, |out| out[0] = b);
    }
    assert!(lo.full());
    assert!(lo.transmit(Instant::ZERO).is_none());
    let (rx, tx) = lo.receive(Instant::ZERO).unwrap();
    rx.consume(|data| assert_eq!(data, &[1]));
    tx.consume(1, |out| out[0] = 3);
    for b in [2, 3] {
        let (rx, _) = lo.receive(Instant::ZERO).unwrap();
        rx.consume(|data| assert_eq!(data, &[b]));
    }
    assert_eq!(lo.queued(), 0);
}

#[test]
fn ethernet_ipv6_syn_and_udp_filters_do_not_depend_on_loopback_pinning() {
    use smoltcp::wire::{TcpPacket, UdpPacket};
    let mut s = stack();
    eth(&mut s, 0);
    let b = budget();
    // Test-only IPv6 interface address lets real Ethernet ingress reach the
    // family filter; production eth0 configuration remains IPv4-only.
    let src = IpAddress::Ipv6(Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2));
    let dst = IpAddress::Ipv6(Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
    s.interfaces[1].as_mut().unwrap().iface.update_ip_addrs(|a| a.push(IpCidr::new(dst, 64)).unwrap());
    let l = s.open(Family::V4, Proto::Tcp).unwrap();
    s.bind(l, BindReq { port: 1234, ..BindReq::default() }, Preps::None, Instant::ZERO).unwrap();
    s.listen(l, 0, &mut [tcp(b), tcp(b)].into_iter(), Instant::ZERO).unwrap();
    let mut p = [0u8; 74];
    p[..6].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
    p[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 2]);
    p[12..14].copy_from_slice(&[0x86, 0xdd]);
    p[14] = 0x60;
    p[19] = 20;
    p[20] = 6;
    p[21] = 64;
    if let (IpAddress::Ipv6(src), IpAddress::Ipv6(dst)) = (src, dst) {
        p[22..38].copy_from_slice(&src.octets());
        p[38..54].copy_from_slice(&dst.octets());
    }
    p[54..56].copy_from_slice(&3000u16.to_be_bytes());
    p[56..58].copy_from_slice(&1234u16.to_be_bytes());
    p[66] = 0x50;
    p[67] = 2;
    p[68] = 0x10;
    TcpPacket::new_unchecked(&mut p[54..]).fill_checksum(&src, &dst);
    s.frame[..74].copy_from_slice(&p);
    s.ingress_frame(1, 74, Instant::ZERO);
    assert!(
        s.live_entries()
            .filter(|e| e.role == Role::Pool(l))
            .all(|e| s.tcp_ref(e.id).unwrap().state() == tcp::State::Listen)
    );
    let u = s.open(Family::V4, Proto::Udp).unwrap();
    let ps: Vec<_> = (0..2).map(|_| prepare_udp(b, BufSizes { rx: 64, tx: 64 }).unwrap()).collect();
    s.bind(u, BindReq { port: 2345, ..BindReq::default() }, Preps::Udp(&mut ps.into_iter()), Instant::ZERO).unwrap();
    p[19] = 12;
    p[20] = 17;
    p[54..66].fill(0);
    p[54..56].copy_from_slice(&3000u16.to_be_bytes());
    p[56..58].copy_from_slice(&2345u16.to_be_bytes());
    p[58..60].copy_from_slice(&12u16.to_be_bytes());
    p[62..66].copy_from_slice(b"data");
    UdpPacket::new_unchecked(&mut p[54..66]).fill_checksum(&src, &dst);
    s.frame[..66].copy_from_slice(&p[..66]);
    s.ingress_frame(1, 66, Instant::ZERO);
    assert_eq!(s.recv(u, &mut [0; 8], false), Err(NetError::WouldBlock));
    let v = s.open(Family::V6, Proto::Udp).unwrap();
    let ps: Vec<_> = (0..2).map(|_| prepare_udp(b, BufSizes { rx: 64, tx: 64 }).unwrap()).collect();
    s.bind(v, BindReq { port: 2346, ..BindReq::default() }, Preps::Udp(&mut ps.into_iter()), Instant::ZERO).unwrap();
    p[56..58].copy_from_slice(&2346u16.to_be_bytes());
    UdpPacket::new_unchecked(&mut p[54..66]).fill_checksum(&src, &dst);
    s.frame[..66].copy_from_slice(&p[..66]);
    s.ingress_frame(1, 66, Instant::ZERO);
    assert_eq!(s.recv(v, &mut [0; 8], false).unwrap().copied, 4);
}

#[test]
fn supplied_seeds_control_tcp_isns_and_dhcp_transaction_ids() {
    fn syn(seed: u64) -> [u8; 4] {
        let mut s: Stack<TestDevice> =
            Stack::try_new(Loopback::try_new(65535, 16).unwrap(), Instant::ZERO, seed).unwrap();
        let id = s.open(Family::V4, Proto::Tcp).unwrap();
        assert_eq!(
            s.connect(
                id,
                IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), 1234),
                Some(ConnectPrep::Tcp(tcp(budget()))),
                Instant::ZERO
            ),
            Err(NetError::InProgress)
        );
        s.poll_round(Instant::ZERO);
        let (rx, _) = s.lo.receive(Instant::ZERO).unwrap();
        rx.consume(|p| p[24..28].try_into().unwrap())
    }
    fn xid(seed: u64) -> [u8; 4] {
        let mut s = stack();
        s.attach_eth(
            TestDevice { frames: 0, received: 0, ready: true, last: [0; ETH_FRAME_MAX], last_len: 0 },
            EthConfig::Dhcp,
            Instant::ZERO,
            seed,
        )
        .unwrap();
        s.poll_round(Instant::ZERO);
        let dev = &s.eth.as_ref().unwrap().device;
        assert!(dev.last_len >= 50);
        assert_eq!(&dev.last[34..38], &[0, 68, 0, 67]);
        dev.last[46..50].try_into().unwrap()
    }
    assert_eq!(syn(459), syn(459));
    assert_ne!(syn(459), syn(460));
    assert_eq!(xid(459), xid(459));
    assert_ne!(xid(459), xid(460));
}

fn connected_pair(s: &mut Stack<TestDevice>, b: &'static Budget) -> (SockId, SockId) {
    let l = s.open(Family::V4, Proto::Tcp).unwrap();
    s.bind(l, BindReq { port: 1234, ..BindReq::default() }, Preps::None, Instant::ZERO).unwrap();
    s.listen(l, 0, &mut [tcp(b)].into_iter(), Instant::ZERO).unwrap();
    let c = s.open(Family::V4, Proto::Tcp).unwrap();
    assert_eq!(
        s.connect(c, IpEndpoint::new(loopback_addr(false), 1234), Some(ConnectPrep::Tcp(tcp(b))), Instant::ZERO),
        Err(NetError::InProgress)
    );
    for n in 0..12 {
        s.poll_round(Instant::from_millis(n));
    }
    (c, s.accept(l).unwrap().0)
}
#[test]
fn receive_queue_is_moved_only_before_a_destructive_reset() {
    let mut s = stack();
    let b = budget();
    let (c, peer) = connected_pair(&mut s, b);
    let payload = [0x61; 64];
    s.send(peer, &payload, None, Preps::None, Instant::from_secs(1)).unwrap();
    for n in 1000..1012 {
        s.poll_round(Instant::from_millis(n));
    }
    assert_eq!(s.tcp_ref(c).unwrap().recv_queue(), 64);
    assert!(s.entry(c).unwrap().retired.is_none(), "ordinary ingress does not copy the queue");
    s.shutdown(c, Shut::Wr, Instant::from_secs(2)).unwrap();
    for n in 2000..2012 {
        s.poll_round(Instant::from_millis(n));
    }
    s.close(peer, false, Instant::from_secs(3));
    for n in 3000..3012 {
        s.poll_round(Instant::from_millis(n));
    }
    assert_eq!(s.tcp_ref(c).unwrap().state(), tcp::State::TimeWait);
    assert!(s.entry(c).unwrap().retired.is_none());
    s.changed();
    s.poll_round(Instant::from_secs(14));
    assert!(s.changed().contains(&c));
    assert_eq!(s.tcp_ref(c).unwrap().state(), tcp::State::Closed);
    assert!(s.entry(c).unwrap().retired.is_some(), "preserve the socket before TIME-WAIT reset");
    assert_eq!(s.tcp_ref(c).unwrap().recv_queue(), 64);
    let mut got = [0; 64];
    assert_eq!(s.recv(c, &mut got, false).unwrap().copied, 64);
    assert_eq!(got, payload);
    assert_eq!(s.recv(c, &mut got, false).unwrap().copied, 0);
    assert_eq!(s.take_error(c), None);
}
#[test]
fn rst_really_delivered_in_time_wait_is_not_a_socket_error() {
    // Linux v6.12 net/ipv4/tcp_minisocks.c, tcp_timewait_state_process:
    // 187-200 retires the mini-socket, without setting a user socket error.
    use smoltcp::phy::{RxToken, TxToken};
    use smoltcp::wire::{Ipv4Packet, TcpPacket};
    let mut s = stack();
    let b = budget();
    let (c, peer) = connected_pair(&mut s, b);
    s.shutdown(c, Shut::Wr, Instant::from_secs(1)).unwrap();
    for n in 1000..1012 {
        s.poll_round(Instant::from_millis(n));
    }
    s.shutdown(peer, Shut::Wr, Instant::from_secs(2)).unwrap();
    s.poll_round(Instant::from_secs(2));
    let mut fin = Vec::new();
    let mut frames = Vec::new();
    while let Some((rx, _)) = s.lo.receive(Instant::from_secs(2)) {
        let bytes = rx.consume(|d| d.to_vec());
        let ip = Ipv4Packet::new_checked(&bytes[..]).unwrap();
        let t = TcpPacket::new_checked(ip.payload()).unwrap();
        if t.fin() {
            fin = bytes.clone();
        }
        frames.push(bytes);
    }
    assert!(!fin.is_empty(), "capture the peer FIN's actual sequence number");
    for frame in frames {
        s.lo.transmit(Instant::from_secs(2)).unwrap().consume(frame.len(), |out| out.copy_from_slice(&frame));
    }
    for n in 2001..2012 {
        s.poll_round(Instant::from_millis(n));
    }
    assert_eq!(s.tcp_ref(c).unwrap().state(), tcp::State::TimeWait);
    let ip = Ipv4Packet::new_checked(&fin[..]).unwrap();
    let (src, dst, offset) = (IpAddress::Ipv4(ip.src_addr()), IpAddress::Ipv4(ip.dst_addr()), ip.header_len() as usize);
    let mut packet = TcpPacket::new_unchecked(&mut fin[offset..]);
    packet.set_seq_number(packet.seq_number() + 1);
    packet.set_fin(false);
    packet.set_rst(true);
    packet.fill_checksum(&src, &dst);
    s.lo.transmit(Instant::from_secs(3)).unwrap().consume(fin.len(), |out| out.copy_from_slice(&fin));
    s.poll_round(Instant::from_secs(3));
    assert_eq!(s.tcp_ref(c).unwrap().state(), tcp::State::Closed, "RST reached TIME-WAIT socket");
    assert_eq!(s.take_error(c), None);
    assert_eq!(s.recv(c, &mut [0; 8], false).unwrap().copied, 0);
}
#[test]
fn keepalive_counts_transmissions_and_retuning_preserves_armed_probe_time() {
    // Linux v6.12 net/ipv4/tcp_timer.c, tcp_keepalive_timer:806-820.
    let mut s = stack();
    let b = budget();
    let (c, _) = connected_pair(&mut s, b);
    s.set_opt(c, NetOpt::KeepAlive(true), Instant::from_secs(1)).unwrap();
    for n in 1000..1010 {
        s.poll_round(Instant::from_millis(n));
    }
    let armed = s.entry(c).unwrap().keep_armed.unwrap();
    s.set_opt(c, NetOpt::KeepIdle(Duration::from_secs(1)), Instant::from_secs(2)).unwrap();
    s.set_opt(c, NetOpt::KeepIntvl(Duration::from_secs(1)), Instant::from_secs(2)).unwrap();
    s.set_opt(c, NetOpt::KeepCnt(1), Instant::from_secs(2)).unwrap();
    assert_eq!(s.entry(c).unwrap().keep_armed, Some(armed));
    assert_eq!(s.entry(c).unwrap().keep_probes, 0);
    s.poll_round(Instant::from_secs(10));
    assert_eq!(s.entry(c).unwrap().keep_deadline, None);
    assert_eq!(s.take_error(c), None);
}
#[test]
fn failed_dhcp_configuration_is_retained_for_retry() {
    let mut s = stack();
    s.attach_eth(
        TestDevice { frames: 0, received: 0, ready: true, last: [0; ETH_FRAME_MAX], last_len: 0 },
        EthConfig::Dhcp,
        Instant::ZERO,
        77,
    )
    .unwrap();
    s.poll_dhcp(); // consume smoltcp's initial Deconfigured event
    s.lease_event();
    s.interfaces[1].as_mut().unwrap().iface.routes_mut().update(|r| {
        for n in [1, 2] {
            r.push(smoltcp::iface::Route {
                cidr: IpCidr::new(IpAddress::v4(192, 0, n, 0), 24),
                via_router: IpAddress::v4(10, 0, 0, 2),
                preferred_until: None,
                expires_at: None,
            })
            .unwrap();
        }
    });
    let event = Lease::Configured {
        addr: Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 1), 24),
        router: Some(Ipv4Address::new(10, 0, 0, 2)),
        dns: [None; 3],
        server: Ipv4Address::new(10, 0, 0, 2),
    };
    s.pending_lease = Some(event);
    s.poll_dhcp();
    assert_eq!(s.pending_lease, Some(event));
    assert_eq!(s.lease_event(), None);
    s.interfaces[1].as_mut().unwrap().iface.routes_mut().update(|r| r.clear());
    s.poll_dhcp();
    assert_eq!(s.pending_lease, None);
    assert_eq!(s.lease_event(), Some(event));
    assert_eq!(s.iface_addr(IfaceId::Eth0, false), Some(IpAddress::v4(10, 0, 0, 1)));
}

#[test]
fn icmp_zero_payload_metadata_release_lists_writable() {
    // Public echo sends require an eight-byte header. Exercise the underlying
    // smoltcp zero-byte queue directly: discarding an invalid packet must also
    // release metadata and notify a blocked writer, despite zero payload use.
    // Linux v6.12 net/core/sock.c, sock_def_write_space:3454-3472.
    let mut s = stack();
    let b = budget();
    let id = s.open(Family::V4, Proto::IcmpEcho { raw: true }).unwrap();
    s.bind(
        id,
        BindReq { port: 1234, ..BindReq::default() },
        Preps::Icmp(&mut [prepare_icmp(b).unwrap()].into_iter()),
        Instant::ZERO,
    )
    .unwrap();
    let h = s.entry(id).unwrap().handles[0].unwrap();
    let socket = s.interfaces[0].as_mut().unwrap().sockets.get_mut::<icmp::Socket>(h);
    for _ in 0..8 {
        socket.send_slice(&[], loopback_addr(false)).unwrap();
    }
    assert_eq!(socket.send_queue(), 0);
    assert!(!socket.can_send());
    s.changed();
    s.poll_round(Instant::ZERO);
    assert!(s.readiness(id).writable);
    assert!(s.changed().contains(&id));
    for _ in 0..8 {
        s.poll_round(Instant::ZERO);
    }
    s.changed();
    s.poll_round(Instant::ZERO);
    assert!(!s.changed().contains(&id), "an idle writable socket is not a new writable event");
}

#[test]
fn listener_option_changes_update_prefilled_children_before_handshake() {
    // Linux v6.12 net/ipv4/inet_connection_sock.c, inet_csk_clone_lock:
    // 1245-1254 clones current listener state when creating the child.
    let mut s = stack();
    let b = budget();
    let l = s.open(Family::V4, Proto::Tcp).unwrap();
    s.bind(l, BindReq { port: 1234, ..BindReq::default() }, Preps::None, Instant::ZERO).unwrap();
    s.listen(l, 0, &mut [tcp(b)].into_iter(), Instant::ZERO).unwrap();
    let options = [
        (OptQuery::NoDelay, NetOpt::NoDelay(true)),
        (OptQuery::KeepIdle, NetOpt::KeepIdle(Duration::from_secs(2))),
        (OptQuery::KeepIntvl, NetOpt::KeepIntvl(Duration::from_secs(1))),
        (OptQuery::KeepCnt, NetOpt::KeepCnt(2)),
        (OptQuery::KeepAlive, NetOpt::KeepAlive(true)),
        (OptQuery::Ttl, NetOpt::Ttl(37)),
        (OptQuery::Tos, NetOpt::Tos(16)),
        (OptQuery::Cork, NetOpt::Cork(true)),
        (OptQuery::Linger, NetOpt::Linger(Some(Duration::from_secs(7)))),
        (OptQuery::MaxSeg, NetOpt::MaxSeg(1000)),
    ];
    for (_, option) in options {
        s.set_opt(l, option, Instant::ZERO).unwrap();
    }
    let c = s.open(Family::V4, Proto::Tcp).unwrap();
    assert_eq!(
        s.connect(c, IpEndpoint::new(loopback_addr(false), 1234), Some(ConnectPrep::Tcp(tcp(b))), Instant::ZERO),
        Err(NetError::InProgress)
    );
    for now in 0..12 {
        s.poll_round(Instant::from_millis(now));
    }
    // A completed child is independent, even before userspace accepts it.
    s.set_opt(l, NetOpt::NoDelay(false), Instant::from_millis(12)).unwrap();
    s.set_opt(l, NetOpt::KeepAlive(false), Instant::from_millis(12)).unwrap();
    let (child, _) = s.accept(l).unwrap();
    for (query, expected) in options {
        assert_eq!(s.get_opt(child, query), Ok(expected));
    }
    let transport = s.tcp_ref(child).unwrap();
    assert!(!transport.nagle_enabled(), "NoDelay must reach smoltcp, not just readback");
    assert_eq!(transport.hop_limit(), Some(37));
    assert_eq!(transport.keep_alive(), Some(Duration::from_secs(2)));
    assert!(s.entry(child).unwrap().keep_armed.is_some());
}

#[test]
fn udp_family_demux_preserves_dhcp_dispatch() {
    // smoltcp 0.14 iface/interface/ipv4.rs processes DHCP before UDP. Keep
    // its DHCP socket visible rather than using the one-UDP-socket scratch set.
    let mut s = stack();
    s.attach_eth(
        TestDevice { frames: 0, received: 0, ready: true, last: [0; ETH_FRAME_MAX], last_len: 0 },
        EthConfig::Dhcp,
        Instant::ZERO,
        99,
    )
    .unwrap();
    let packet = Tuple {
        local: IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 67),
        remote: IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::BROADCAST), 68),
    };
    assert!(matches!(s.udp_action(1, packet), Syn::Deliver(None)));
    assert!(matches!(s.udp_action(0, packet), Syn::NoOwner));
    let ordinary = Tuple { local: IpEndpoint::new(packet.local.addr, 1234), ..packet };
    assert!(matches!(s.udp_action(1, ordinary), Syn::NoOwner));
}

#[test]
fn icmp_ttl_before_and_after_bind_reaches_all_transports_and_wire() {
    // Linux v6.12 net/ipv4/ping.c, ping_v4_sendmsg:822-828 and
    // net/ipv4/raw.c, raw_sendmsg:651-662 use ip_append_data/push;
    // net/ipv4/ip_output.c, __ip_make_skb:1437-1449 selects the socket's
    // TTL for the emitted header. Binding does not freeze that option.
    use smoltcp::wire::{Icmpv4Packet, Ipv4Packet};
    for raw in [false, true] {
        let mut s = stack();
        eth(&mut s, 0);
        let b = budget();
        let id = s.open(Family::V4, Proto::IcmpEcho { raw }).unwrap();
        s.set_opt(id, NetOpt::Ttl(37), Instant::ZERO).unwrap();
        s.bind(
            id,
            BindReq { port: 1234, ..BindReq::default() },
            Preps::Icmp(&mut [prepare_icmp(b).unwrap(), prepare_icmp(b).unwrap()].into_iter()),
            Instant::ZERO,
        )
        .unwrap();
        for ttl in [37, 91] {
            if ttl == 91 {
                s.set_opt(id, NetOpt::Ttl(ttl), Instant::ZERO).unwrap();
            }
            for i in 0..2 {
                let h = s.entry(id).unwrap().handles[i].unwrap();
                assert_eq!(s.interfaces[i].as_ref().unwrap().sockets.get::<icmp::Socket>(h).hop_limit(), Some(ttl));
            }
            let mut request = [8, 0, 0, 0, 0x04, 0xd2, 0, 1];
            Icmpv4Packet::new_unchecked(&mut request[..]).fill_checksum();
            s.send(id, &request, Some(IpEndpoint::new(loopback_addr(false), 0)), Preps::None, Instant::ZERO).unwrap();
            s.poll_round(Instant::ZERO);
            let (rx, _) = s.lo.receive(Instant::ZERO).expect("echo request was transmitted");
            rx.consume(|data| {
                let packet = Ipv4Packet::new_checked(data).unwrap();
                assert_eq!(packet.hop_limit(), ttl);
                assert_eq!(packet.next_header(), smoltcp::wire::IpProtocol::Icmp);
                assert_eq!(packet.payload()[0], 8);
            });
        }
    }
}
