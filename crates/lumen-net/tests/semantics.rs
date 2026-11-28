mod support;
use lumen_net::*;
use std::sync::atomic::Ordering;
use support::*;

#[test]
fn tcp_echo_over_ipv4_and_ipv6() {
    for v6 in [false, true] {
        let (mut s, _, _, c, a, mut now) = pair(v6);
        let payload = b"echo #459, both directions";
        assert_eq!(s.send(c, payload, None, Preps::None, Instant::from_millis(now)), Ok(payload.len()));
        steps(&mut s, &mut now, 8);
        let mut buf = [0; 64];
        assert_eq!(s.recv(a, &mut buf, true).unwrap().copied, payload.len());
        assert_eq!(&buf[..payload.len()], payload);
        assert_eq!(s.recv(a, &mut buf, false).unwrap().copied, payload.len());
        assert_eq!(s.send(a, &buf[..payload.len()], None, Preps::None, Instant::from_millis(now)), Ok(payload.len()));
        steps(&mut s, &mut now, 8);
        assert_eq!(s.recv(c, &mut buf, false).unwrap().copied, payload.len());
        assert_eq!(&buf[..payload.len()], payload);
        assert_eq!(s.recv(c, &mut buf, false), Err(NetError::WouldBlock));
    }
}
#[test]
fn n_simultaneous_connects_to_one_listener_all_accepted() {
    let mut s = stack();
    let b = budget();
    let l = listener(&mut s, b, Some(addr(false)), false, false, 1234, 3);
    let mut now = 0;
    for _ in 0..3 {
        let clients: Vec<_> = (0..4).map(|_| connect(&mut s, b, IpEndpoint::new(addr(false), 1234), now)).collect();
        steps(&mut s, &mut now, 16);
        let accepted: Vec<_> = (0..4).map(|_| s.accept(l).unwrap()).collect();
        for c in clients {
            assert!(s.readiness(c).writable);
            assert_eq!(s.take_error(c), None);
            s.close(c, true, Instant::from_millis(now));
        }
        for (a, _) in accepted {
            s.close(a, true, Instant::from_millis(now));
        }
        steps(&mut s, &mut now, 8);
        s.changed();
        refill(&mut s, b);
    }
    s.close(l, false, Instant::from_millis(now));
    steps(&mut s, &mut now, 8);
    assert_eq!(b.used(), 0);
    assert_eq!(s.socket_count(), 0);
    assert!(s.port_count() == 0);
}
#[test]
fn connects_beyond_the_backlog_are_dropped_until_refill() {
    // Design #469 supersedes the issue's "refused": Linux drops excess SYNs.
    let mut s = stack();
    let b = budget();
    let l = listener(&mut s, b, Some(addr(false)), false, false, 1234, 0);
    let mut now = 0;
    let first = connect(&mut s, b, IpEndpoint::new(addr(false), 1234), now);
    let second = connect(&mut s, b, IpEndpoint::new(addr(false), 1234), now);
    steps(&mut s, &mut now, 16);
    assert!(s.readiness(first).writable);
    assert!(!s.readiness(second).writable);
    assert_eq!(s.take_error(second), None);
    assert!(s.shortfalls().is_empty());
    s.accept(l).unwrap();
    refill(&mut s, b);
    now = 2000;
    steps(&mut s, &mut now, 16);
    assert!(s.readiness(second).writable);
    assert!(s.accept(l).is_ok());
}
#[test]
fn nonblocking_connect_closed_port_records_econnrefused() {
    // Linux v6.12 net/ipv4/af_inet.c, __inet_stream_connect:654-693: EISCONN, EALREADY, EINPROGRESS.
    let mut s = stack();
    let b = budget();
    let remote = IpEndpoint::new(addr(false), 9999);
    let c = connect(&mut s, b, remote, 0);
    assert_eq!(s.connect(c, remote, None, Instant::ZERO), Err(NetError::Already));
    let mut now = 0;
    steps(&mut s, &mut now, 8);
    assert!(s.readiness(c).error);
    assert_eq!(s.take_error(c), Some(NetError::ConnRefused));
    assert_eq!(s.take_error(c), None);
    assert_eq!(s.recv(c, &mut [0; 1], false).unwrap().copied, 0);
    assert_eq!(s.send(c, b"x", None, Preps::None, Instant::from_millis(now)), Err(NetError::Pipe));
    // Linux v6.12 net/ipv4/af_inet.c, __inet_stream_connect:654-730: SS_CONNECTING with an
    // established transport completes one connect with 0; SS_CONNECTED
    // then gives EISCONN. SO_ERROR does not change the socket's SS state.
    // This is more precise than section 7's "once connected" shorthand.
    let (mut s, _, _, c, _, _) = pair(false);
    assert_eq!(s.connect(c, remote, None, Instant::ZERO), Ok(()));
    assert_eq!(s.connect(c, remote, None, Instant::ZERO), Err(NetError::IsConn));
}
#[test]
fn half_close_reads_eof_while_peer_still_sends() {
    // Linux v6.12 net/ipv4/tcp.c, tcp_shutdown:2978-2994: shutdown only sends FIN for SEND_SHUTDOWN.
    let (mut s, _, _, c, a, mut now) = pair(false);
    s.shutdown(c, Shut::Wr, Instant::from_millis(now)).unwrap();
    steps(&mut s, &mut now, 8);
    assert!(s.readiness(a).rdhup);
    assert!(!s.readiness(a).hup);
    assert_eq!(s.recv(a, &mut [0; 8], false).unwrap().copied, 0);
    assert_eq!(s.send(a, b"reply", None, Preps::None, Instant::from_millis(now)), Ok(5));
    steps(&mut s, &mut now, 8);
    let mut buf = [0; 8];
    assert_eq!(s.recv(c, &mut buf, false).unwrap().copied, 5);
    assert_eq!(&buf[..5], b"reply");
    assert_eq!(s.send(c, b"x", None, Preps::None, Instant::from_millis(now)), Err(NetError::Pipe));
}
#[test]
fn rst_after_close_with_unread_data() {
    // Linux v6.12 net/ipv4/tcp.c, __tcp_close:3085-3101: unread data and zero linger cause RST.
    for unread in [false, true] {
        let (mut s, _, _, c, a, mut now) = pair(false);
        if unread {
            s.send(c, b"unread", None, Preps::None, Instant::from_millis(now)).unwrap();
            steps(&mut s, &mut now, 8);
            assert!(s.readiness(a).readable);
        }
        s.close(a, !unread, Instant::from_millis(now));
        steps(&mut s, &mut now, 8);
        assert!(s.readiness(c).error);
        assert_eq!(s.send(c, b"x", None, Preps::None, Instant::from_millis(now)), Err(NetError::ConnReset));
        assert!(!s.readiness(c).error);
        assert_eq!(s.recv(c, &mut [0; 8], false).unwrap().copied, 0);
        assert_eq!(s.send(c, b"x", None, Preps::None, Instant::from_millis(now)), Err(NetError::Pipe));
    }
}
#[test]
fn udp_connected_filtering_send_to_without_address_and_truncation_lengths() {
    // Linux v6.12 net/ipv4/udp.c, udp_sendmsg:1111-1129: explicit destination wins; absent needs a peer.
    for v6 in [false, true] {
        let mut s = stack();
        let b = budget();
        let a = bind_udp(&mut s, b, Some(addr(v6)), 1001, v6);
        let c = bind_udp(&mut s, b, Some(addr(v6)), 1002, v6);
        let stranger = bind_udp(&mut s, b, Some(addr(v6)), 1003, v6);
        assert_eq!(s.send(a, b"x", None, Preps::None, Instant::ZERO), Err(NetError::DestAddrReq));
        s.connect(a, IpEndpoint::new(addr(v6), 1002), None, Instant::ZERO).unwrap();
        s.send(stranger, b"bad", Some(IpEndpoint::new(addr(v6), 1001)), Preps::None, Instant::ZERO).unwrap();
        s.send(c, b"good packet", Some(IpEndpoint::new(addr(v6), 1001)), Preps::None, Instant::ZERO).unwrap();
        let mut now = 0;
        steps(&mut s, &mut now, 8);
        let (len, ticket) = s.datagram_len(a).unwrap();
        assert_eq!(len, 11);
        let mut buf = [0; 4];
        let r = s.recv_ticket(a, ticket, &mut buf, true).unwrap();
        assert_eq!((r.copied, r.full_len), (4, 11));
        assert_eq!(&buf, b"good");
        assert_eq!(s.datagram_len(a).unwrap().1, ticket);
        s.consume(a, ticket).unwrap();
        assert_eq!(s.recv_ticket(a, ticket, &mut buf, false), Err(NetError::StaleTicket));
        assert_eq!(s.recv(a, &mut buf, false), Err(NetError::WouldBlock));
        s.disconnect(a).unwrap();
        s.send(stranger, b"ok", Some(IpEndpoint::new(addr(v6), 1001)), Preps::None, Instant::from_millis(now)).unwrap();
        steps(&mut s, &mut now, 8);
        assert_eq!(s.recv(a, &mut buf, false).unwrap().full_len, 2);
        assert_eq!(s.send(a, b"x", None, Preps::None, Instant::from_millis(now)), Err(NetError::DestAddrReq));
    }
}
#[test]
fn loopback_and_ethernet_like_interface_side_by_side() {
    let (mut a, mut b, _, _) = wire_stacks();
    let budget = budget();
    let la = listener(&mut a, budget, Some(addr(false)), false, false, 1234, 0);
    let lb = listener(&mut b, budget, Some(IpAddress::v4(10, 0, 0, 2)), false, false, 1234, 0);
    let local = connect(&mut a, budget, IpEndpoint::new(addr(false), 1234), 0);
    let remote = connect(&mut a, budget, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), 0);
    let mut now = 0;
    wire_steps(&mut a, &mut b, &mut now, 20);
    let (sa, _) = a.accept(la).unwrap();
    let (sb, _) = b.accept(lb).unwrap();
    assert_eq!(a.iface_of(local), Some(IfaceId::Lo));
    assert_eq!(a.iface_of(remote), Some(IfaceId::Eth0));
    a.send(local, b"local", None, Preps::None, Instant::from_millis(now)).unwrap();
    a.send(remote, b"wire", None, Preps::None, Instant::from_millis(now)).unwrap();
    wire_steps(&mut a, &mut b, &mut now, 20);
    let mut buf = [0; 8];
    assert_eq!(a.recv(sa, &mut buf, false).unwrap().copied, 5);
    assert_eq!(&buf[..5], b"local");
    assert_eq!(b.recv(sb, &mut buf, false).unwrap().copied, 4);
    assert_eq!(&buf[..4], b"wire");
    b.send(sb, b"back", None, Preps::None, Instant::from_millis(now)).unwrap();
    wire_steps(&mut a, &mut b, &mut now, 20);
    assert_eq!(a.recv(remote, &mut buf, false).unwrap().copied, 4);
    assert_eq!(&buf[..4], b"back");
}
#[test]
fn syn_timeouts_distinguish_transmitted_tcp_and_unanswered_arp() {
    for arp in [false, true] {
        let (mut a, mut b, ca, cb) = wire_stacks();
        let budget = budget();
        if arp {
            cb.drop_all.store(true, Ordering::Relaxed);
        } else {
            ca.drop_ip.store(true, Ordering::Relaxed);
        }
        let c = connect(&mut a, budget, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), 0);
        let mut now = 0;
        wire_steps(&mut a, &mut b, &mut now, 20);
        a.poll_round(Instant::from_millis(126999));
        assert_eq!(a.take_error(c), None);
        a.poll_round(Instant::from_millis(127000));
        assert_eq!(a.take_error(c), Some(if arp { NetError::HostUnreach } else { NetError::TimedOut }));
    }
}
#[test]
fn established_connection_idle_for_200_seconds_stays_established() {
    let (mut s, _, _, c, a, mut now) = pair(false);
    now += 200000;
    steps(&mut s, &mut now, 8);
    assert_eq!(s.take_error(c), None);
    assert_eq!(s.take_error(a), None);
    assert!(!s.readiness(c).hup);
    assert_eq!(s.send(c, b"alive", None, Preps::None, Instant::from_millis(now)), Ok(5));
    steps(&mut s, &mut now, 8);
    assert_eq!(s.recv(a, &mut [0; 8], false).unwrap().copied, 5);
}
#[test]
fn anyip_accepts_127_0_0_2_and_dual_stack_reports_mapped_names() {
    let mut s = stack();
    let b = budget();
    let l = listener(&mut s, b, None, true, false, 1234, 0);
    let c = connect(&mut s, b, IpEndpoint::new(IpAddress::v4(127, 0, 0, 2), 1234), 0);
    let mut now = 0;
    steps(&mut s, &mut now, 16);
    let (a, peer) = s.accept(l).unwrap();
    assert_eq!(peer.addr, IpAddress::Ipv6(Ipv4Address::LOCALHOST.to_ipv6_mapped()));
    assert_eq!(s.local_endpoint(a).unwrap().addr, IpAddress::Ipv6(Ipv4Address::new(127, 0, 0, 2).to_ipv6_mapped()));
    assert!(s.readiness(c).writable);
}
#[test]
fn syn_family_filter_and_udp_family_filter() {
    let mut s = stack();
    let b = budget();
    let l = listener(&mut s, b, None, true, true, 1234, 0);
    let c = connect(&mut s, b, IpEndpoint::new(addr(false), 1234), 0);
    let mut now = 0;
    steps(&mut s, &mut now, 16);
    assert_eq!(s.take_error(c), Some(NetError::ConnRefused));
    assert_eq!(s.accept(l), Err(NetError::WouldBlock));
    let valid = connect(&mut s, b, IpEndpoint::new(addr(true), 1234), now);
    steps(&mut s, &mut now, 16);
    assert!(s.accept(l).is_ok());
    assert!(s.readiness(valid).writable);
    let v4 = bind_udp(&mut s, b, None, 2000, false);
    let v6 = bind_udp(&mut s, b, Some(addr(true)), 2001, true);
    s.send(v6, b"wrong family", Some(IpEndpoint::new(addr(true), 2000)), Preps::None, Instant::from_millis(now))
        .unwrap();
    steps(&mut s, &mut now, 8);
    assert_eq!(s.recv(v4, &mut [0; 32], false), Err(NetError::WouldBlock));
    let sender = bind_udp(&mut s, b, Some(addr(false)), 2002, false);
    s.send(sender, b"ok", Some(IpEndpoint::new(addr(false), 2000)), Preps::None, Instant::from_millis(now)).unwrap();
    steps(&mut s, &mut now, 8);
    assert_eq!(s.recv(v4, &mut [0; 8], false).unwrap().copied, 2);
}
#[test]
fn changed_lists_arrival_after_inline_drain() {
    let (mut s, _, _, c, a, mut now) = pair(false);
    s.changed();
    for _ in 0..2 {
        s.send(c, b"x", None, Preps::None, Instant::from_millis(now)).unwrap();
        steps(&mut s, &mut now, 8);
        assert!(s.changed().contains(&a));
        s.recv(a, &mut [0; 1], false).unwrap();
        s.changed();
    }
}
#[test]
fn changed_survives_inline_send_readiness_and_poll() {
    let (mut s, _, _, c, a, mut now) = pair(false);
    s.changed();
    s.send(c, b"x", None, Preps::None, Instant::from_millis(now)).unwrap();
    steps(&mut s, &mut now, 8);
    s.send(a, b"y", None, Preps::None, Instant::from_millis(now)).unwrap();
    assert!(s.readiness(a).readable);
    steps(&mut s, &mut now, 8);
    assert!(s.changed().contains(&a));
    assert!(s.changed().is_empty());
}

#[test]
fn randomized_connection_churn_has_no_port_socket_or_charge_leaks() {
    let mut s = stack();
    let b = budget();
    let l = listener(&mut s, b, Some(addr(false)), false, false, 1234, 0);
    let mut now = 0;
    let mut random = 459u64;
    let baseline = b.used();
    let baseline_ports = s.port_count();
    for _ in 0..128 {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        let c = connect(&mut s, b, IpEndpoint::new(addr(false), 1234), now);
        steps(&mut s, &mut now, 8);
        let (a, _) = s.accept(l).unwrap();
        let bytes = [random as u8; 128];
        let n = (random as usize % 128) + 1;
        s.send(c, &bytes[..n], None, Preps::None, Instant::from_millis(now)).unwrap();
        steps(&mut s, &mut now, 8);
        if random & 1 != 0 {
            let mut out = [0; 128];
            assert_eq!(s.recv(a, &mut out, false).unwrap().copied, n);
            assert_eq!(&out[..n], &bytes[..n]);
        }
        s.close(a, true, Instant::from_millis(now));
        s.close(c, true, Instant::from_millis(now));
        steps(&mut s, &mut now, 8);
        s.changed();
        refill(&mut s, b);
        assert_eq!(b.used(), baseline);
        assert_eq!(s.socket_count(), 1);
        assert_eq!(s.entry_count(), 2);
        assert_eq!(s.port_count(), baseline_ports);
    }
    s.close(l, false, Instant::from_millis(now));
    assert_eq!(b.used(), 0);
    assert_eq!(s.socket_count(), 0);
    assert_eq!(s.entry_count(), 0);
    assert!(s.port_count() == 0);
}
#[test]
fn time_wait_releases_buffers_but_preserves_the_port() {
    let (mut s, b, l, c, a, mut now) = pair(false);
    let port = s.local_endpoint(c).unwrap().port;
    s.shutdown(c, Shut::Wr, Instant::from_millis(now)).unwrap();
    steps(&mut s, &mut now, 8);
    s.close(a, false, Instant::from_millis(now));
    steps(&mut s, &mut now, 8);
    s.close(c, false, Instant::from_millis(now));
    steps(&mut s, &mut now, 8);
    assert_eq!(b.used(), 0);
    assert_eq!(s.socket_count(), 0);
    let deadline = s.time_wait_ports().find(|(p, _)| *p == port).unwrap().1;
    let n = s.open(Family::V4, Proto::Tcp).unwrap();
    let req = BindReq { port, ..BindReq::default() };
    assert_eq!(s.bind(n, req, Preps::None, Instant::from_millis(now)), Err(NetError::AddrInUse));
    assert_eq!(
        s.bind(n, BindReq { reuse_addr: true, ..req }, Preps::None, Instant::from_millis(now)),
        Err(NetError::AddrInUse)
    );
    s.close(n, false, Instant::from_millis(now));
    s.poll_round(deadline - Duration::from_millis(1));
    let n = s.open(Family::V4, Proto::Tcp).unwrap();
    assert_eq!(s.bind(n, req, Preps::None, Instant::from_millis(now)), Err(NetError::AddrInUse));
    s.poll_round(deadline);
    s.bind(n, req, Preps::None, Instant::from_millis(now)).unwrap();
    s.close(n, false, Instant::from_millis(now));
    s.close(l, false, Instant::from_millis(now));
    assert!(s.port_count() == 0);
}
#[test]
fn orphan_fin_wait_2_is_aborted_at_sixty_seconds() {
    let (mut s, b, _, c, a, mut now) = pair(false);
    let before = b.used();
    s.close(c, false, Instant::from_millis(now));
    steps(&mut s, &mut now, 1000);
    let deadline = s.poll_at(Instant::from_millis(now)).unwrap();
    assert_eq!(b.used(), before);
    s.poll_round(deadline - Duration::from_millis(1));
    assert_eq!(b.used(), before);
    s.poll_round(deadline);
    assert_eq!(b.used(), before / 2);
    now = deadline.total_millis() + 1;
    steps(&mut s, &mut now, 8);
    assert_eq!(s.take_error(a), Some(NetError::Pipe));
}
#[test]
fn keepalive_times_out_and_disabling_it_clears_the_timeout() {
    let (mut a, mut b, _, cb) = wire_stacks();
    let budget = budget();
    let l = listener(&mut b, budget, Some(IpAddress::v4(10, 0, 0, 2)), false, false, 1234, 0);
    let c = connect(&mut a, budget, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), 0);
    let mut now = 0;
    wire_steps(&mut a, &mut b, &mut now, 20);
    b.accept(l).unwrap();
    a.set_opt(c, NetOpt::KeepIdle(Duration::from_secs(1)), Instant::from_millis(now)).unwrap();
    a.set_opt(c, NetOpt::KeepIntvl(Duration::from_secs(1)), Instant::from_millis(now)).unwrap();
    a.set_opt(c, NetOpt::KeepCnt(1), Instant::from_millis(now)).unwrap();
    a.set_opt(c, NetOpt::KeepAlive(true), Instant::from_millis(now)).unwrap();
    cb.drop_ip.store(true, Ordering::Relaxed);
    now = 1100;
    wire_steps(&mut a, &mut b, &mut now, 8);
    assert_eq!(a.take_error(c), None);
    now = 2100;
    wire_steps(&mut a, &mut b, &mut now, 8);
    assert_eq!(a.take_error(c), Some(NetError::TimedOut));
    let (mut s, _, _, c, _, _) = pair(false);
    s.set_opt(c, NetOpt::KeepAlive(true), Instant::from_millis(now)).unwrap();
    s.set_opt(c, NetOpt::KeepAlive(false), Instant::from_millis(now)).unwrap();
    s.poll_round(Instant::from_millis(8_000_000));
    assert_eq!(s.take_error(c), None);
    assert!(!s.readiness(c).hup);
}
#[test]
fn syn_received_times_out_and_requests_a_replacement_pool_socket() {
    let (mut a, mut b, ca, _) = wire_stacks();
    let budget = budget();
    let l = listener(&mut b, budget, Some(IpAddress::v4(10, 0, 0, 2)), false, false, 1234, 0);
    connect(&mut a, budget, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), 0);
    let mut now = 0;
    wire_steps(&mut a, &mut b, &mut now, 2);
    ca.drop_ip.store(true, Ordering::Relaxed);
    wire_steps(&mut a, &mut b, &mut now, 8);
    assert_eq!(b.accept(l), Err(NetError::WouldBlock));
    assert!(b.shortfalls().is_empty());
    b.poll_round(Instant::from_millis(63002));
    assert_eq!(b.shortfalls(), &[(l, IfaceId::Eth0, 1)]);
    refill(&mut b, budget);
    assert!(b.shortfalls().is_empty());
}
#[test]
fn udp_wildcard_fairness_and_tickets_across_both_interfaces() {
    let (mut a, mut b, _, _) = wire_stacks();
    let budget = budget();
    let receiver = bind_udp(&mut a, budget, None, 1000, false);
    let local = bind_udp(&mut a, budget, Some(addr(false)), 1001, false);
    let remote = bind_udp(&mut b, budget, Some(IpAddress::v4(10, 0, 0, 2)), 1002, false);
    for _ in 0..2 {
        a.send(local, b"lo", Some(IpEndpoint::new(addr(false), 1000)), Preps::None, Instant::ZERO).unwrap();
        b.send(remote, b"eth", Some(IpEndpoint::new(IpAddress::v4(10, 0, 0, 1), 1000)), Preps::None, Instant::ZERO)
            .unwrap();
    }
    let mut now = 0;
    wire_steps(&mut a, &mut b, &mut now, 16);
    let ticket = a.datagram_len(receiver).unwrap().1;
    let mut buf = [0; 8];
    assert_eq!(a.recv_ticket(receiver, ticket, &mut buf, true).unwrap().full_len, 2);
    assert_eq!(a.datagram_len(receiver).unwrap().1, ticket);
    for expected in [2, 3, 2, 3] {
        assert_eq!(a.recv(receiver, &mut buf, false).unwrap().full_len, expected);
    }
    assert_eq!(a.datagram_len(receiver), Err(NetError::WouldBlock));
}
#[test]
fn socket_buffer_sizes_backlogs_and_errno_values() {
    assert_eq!(clamp_backlog(-1), 4096);
    assert_eq!(clamp_backlog(0), 0);
    assert_eq!(clamp_backlog(7), 7);
    assert_eq!(buffer_size(0, true), (2304, 1152));
    assert_eq!(buffer_size(0, false), (4608, 2304));
    assert_eq!(buffer_size(8192, true), (16384, 8192));
    assert_eq!(buffer_size(i32::MAX, false), (425984, 212992));
    for (e, n) in [
        (NetError::WouldBlock, 11),
        (NetError::InProgress, 115),
        (NetError::Already, 114),
        (NetError::IsConn, 106),
        (NetError::NotConn, 107),
        (NetError::AddrInUse, 98),
        (NetError::AddrNotAvail, 99),
        (NetError::ConnRefused, 111),
        (NetError::ConnAborted, 103),
        (NetError::ConnReset, 104),
        (NetError::TimedOut, 110),
        (NetError::HostUnreach, 113),
        (NetError::NetUnreach, 101),
        (NetError::MsgSize, 90),
        (NetError::NoBufs, 105),
        (NetError::NoMem, 12),
        (NetError::Inval, 22),
        (NetError::OpNotSupp, 95),
        (NetError::DestAddrReq, 89),
    ] {
        assert_eq!(e.to_errno(), Some(n));
    }
}
#[test]
fn stale_socket_ids_and_bind_route_errors() {
    let mut s = stack();
    let b = budget();
    let old = s.open(Family::V4, Proto::Tcp).unwrap();
    s.close(old, false, Instant::ZERO);
    s.changed();
    let new = s.open(Family::V4, Proto::Tcp).unwrap();
    assert_eq!(s.preps_needed(new, PrepFor::Bind(&BindReq::default())), 0);
    assert_eq!(s.preps_needed(new, PrepFor::Connect(IpEndpoint::new(addr(false), 80))), 1);
    assert_eq!(old.index(), new.index());
    assert_ne!(old, new);
    assert_eq!(s.bind(old, BindReq::default(), Preps::None, Instant::ZERO), Err(NetError::Inval));
    assert!(s.readiness(old).hup && s.readiness(old).error);
    assert_eq!(s.shutdown(new, Shut::Wr, Instant::ZERO), Err(NetError::NotConn));
    assert_eq!(
        s.bind(
            new,
            BindReq { addr: Some(IpAddress::v4(192, 0, 2, 1)), port: 80, ..BindReq::default() },
            Preps::None,
            Instant::ZERO
        ),
        Err(NetError::AddrNotAvail)
    );
    assert_eq!(
        s.connect(new, IpEndpoint::new(IpAddress::v4(192, 0, 2, 1), 80), Some(prep(b)), Instant::ZERO),
        Err(NetError::NetUnreach)
    );
    s.bind(new, BindReq::default(), Preps::None, Instant::ZERO).unwrap();
    assert!(EPHEMERAL.contains(&s.local_endpoint(new).unwrap().port));
    assert_eq!(s.set_opt(new, NetOpt::V6Only(true), Instant::ZERO), Err(NetError::Inval));
    let (mut s, _, _, _) = wire_stacks();
    let id = s.open(Family::V4, Proto::Tcp).unwrap();
    let remote = IpEndpoint::new(IpAddress::v4(192, 0, 2, 1), 80);
    assert_eq!(s.connect(id, remote, Some(prep(b)), Instant::ZERO), Err(NetError::NetUnreach));
    s.configure_eth(Some(Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 1), 24)), Some(Ipv4Address::new(10, 0, 0, 254)))
        .unwrap();
    assert_eq!(s.connect(id, remote, Some(prep(b)), Instant::ZERO), Err(NetError::InProgress));
}
#[test]
fn udp_mtu_and_send_buffer_limits_and_empty_datagrams() {
    for v6 in [false, true] {
        let mut s = stack();
        let b = budget();
        let a = s.open(Family::of(addr(v6)), Proto::Udp).unwrap();
        let r = s.open(Family::of(addr(v6)), Proto::Udp).unwrap();
        for (id, port) in [(a, 1000), (r, 1001)] {
            s.bind(
                id,
                BindReq { addr: Some(addr(v6)), port, ..BindReq::default() },
                Preps::Udp(&mut [prepare_udp(b, BufSizes::default()).unwrap()].into_iter()),
                Instant::ZERO,
            )
            .unwrap();
        }
        let max = if v6 { 65487 } else { 65507 };
        let data = vec![0x5a; max];
        assert_eq!(s.send(a, &data, Some(IpEndpoint::new(addr(v6), 1001)), Preps::None, Instant::ZERO), Ok(max));
        assert_eq!(
            s.send(a, &vec![0; max + 1], Some(IpEndpoint::new(addr(v6), 1001)), Preps::None, Instant::ZERO),
            Err(NetError::MsgSize)
        );
        let mut now = 0;
        steps(&mut s, &mut now, 8);
        assert_eq!(s.recv(r, &mut [], false).unwrap().full_len, max);
        s.changed();
        s.send(a, &[], Some(IpEndpoint::new(addr(v6), 1001)), Preps::None, Instant::from_millis(now)).unwrap();
        steps(&mut s, &mut now, 8);
        assert!(s.changed().contains(&r));
        assert!(s.readiness(r).readable);
        assert_eq!(s.recv(r, &mut [], false).unwrap().full_len, 0);
        assert!(!s.readiness(r).readable);
    }
}

#[test]
fn reuseport_selects_first_bound_listener_and_never_spreads_excess_syns() {
    let mut s = stack();
    let b = budget();
    let later = s.open(Family::V4, Proto::Tcp).unwrap();
    let first = s.open(Family::V4, Proto::Tcp).unwrap();
    let req = BindReq { port: 1234, reuse_port: true, ..BindReq::default() };
    for id in [first, later] {
        s.bind(id, req, Preps::None, Instant::ZERO).unwrap();
    }
    // Deliberately reverse both SockId order and smoltcp slot order.
    for id in [later, first] {
        s.listen(id, 0, &mut [prepare_tcp(b, sizes()).unwrap()].into_iter(), Instant::ZERO).unwrap();
    }
    let c = connect(&mut s, b, IpEndpoint::new(addr(false), 1234), 0);
    let mut now = 0;
    steps(&mut s, &mut now, 16);
    assert!(s.readiness(c).writable);
    assert_eq!(s.accept(later), Err(NetError::WouldBlock));
    let extra = connect(&mut s, b, IpEndpoint::new(addr(false), 1234), now);
    steps(&mut s, &mut now, 16);
    assert!(!s.readiness(extra).writable);
    assert_eq!(s.take_error(extra), None);
    assert_eq!(s.accept(later), Err(NetError::WouldBlock));
    assert!(s.accept(first).is_ok());
}
#[test]
fn listen_pool_cap_relisten_and_independent_family_overlap() {
    let mut s = stack();
    let b = budget();
    let l = listener(&mut s, b, None, false, false, 1234, SOMAXCONN);
    assert_eq!(s.socket_count(), LISTEN_POOL_MAX);
    assert_eq!(b.used(), LISTEN_POOL_MAX * 8192);
    s.listen(l, 0, &mut std::iter::empty(), Instant::ZERO).unwrap();
    assert_eq!(s.socket_count(), 1);
    assert_eq!(b.used(), 8192);
    let v6 = s.open(Family::V6, Proto::Tcp).unwrap();
    s.set_opt(v6, NetOpt::V6Only(true), Instant::ZERO).unwrap();
    assert_eq!(
        s.bind(
            v6,
            BindReq { port: 1234, reuse_addr: true, reuse_port: true, ..BindReq::default() },
            Preps::None,
            Instant::ZERO
        ),
        Ok(())
    );
    s.close(l, false, Instant::ZERO);
    assert_eq!(b.used(), 0);
}
#[test]
fn tcp_readiness_uses_one_third_free_and_read_shutdown_drops_data() {
    let (mut s, _, _, c, a, mut now) = pair(false);
    s.send(c, &[7; 2730], None, Preps::None, Instant::from_millis(now)).unwrap();
    assert!(s.readiness(c).writable);
    s.send(c, &[7], None, Preps::None, Instant::from_millis(now)).unwrap();
    assert!(!s.readiness(c).writable);
    s.shutdown(a, Shut::Rd, Instant::from_millis(now)).unwrap();
    steps(&mut s, &mut now, 8);
    assert_eq!(s.recv(a, &mut [0; 8], false).unwrap().copied, 0);
    assert!(s.readiness(a).readable);
    assert!(s.readiness(a).rdhup);
    assert!(!s.readiness(a).hup);
    s.close(a, false, Instant::from_millis(now));
    steps(&mut s, &mut now, 8);
    assert_eq!(s.take_error(c), None, "read-shutdown data was drained, so close must not reset");
}
#[test]
fn closing_listener_resets_queued_connections() {
    let mut s = stack();
    let b = budget();
    let l = listener(&mut s, b, Some(addr(false)), false, false, 1234, 0);
    let c = connect(&mut s, b, IpEndpoint::new(addr(false), 1234), 0);
    let mut now = 0;
    steps(&mut s, &mut now, 8);
    assert!(s.readiness(l).readable);
    s.close(l, false, Instant::from_millis(now));
    steps(&mut s, &mut now, 8);
    assert_eq!(s.recv(c, &mut [0; 8], false), Err(NetError::ConnReset));
    assert_eq!(s.recv(c, &mut [0; 8], false).unwrap().copied, 0);
    s.close(c, false, Instant::from_millis(now));
    steps(&mut s, &mut now, 8);
    assert_eq!(b.used(), 0);
}
#[test]
fn udp_first_send_autobinds_and_explicit_destination_overrides_peer() {
    let (mut s, _, _, _) = wire_stacks();
    let b = budget();
    let a = s.open(Family::V4, Proto::Udp).unwrap();
    let receiver = bind_udp(&mut s, b, Some(addr(false)), 2000, false);
    let n = s.preps_needed(a, PrepFor::Send(Some(IpEndpoint::new(addr(false), 2000))));
    assert_eq!(n, 2);
    let preps: Vec<_> = (0..n).map(|_| udp(b)).collect();
    s.send(a, b"first", Some(IpEndpoint::new(addr(false), 2000)), Preps::Udp(&mut preps.into_iter()), Instant::ZERO)
        .unwrap();
    assert!(EPHEMERAL.contains(&s.local_endpoint(a).unwrap().port));
    s.connect(a, IpEndpoint::new(addr(false), 9999), None, Instant::ZERO).unwrap();
    s.send(a, b"override", Some(IpEndpoint::new(addr(false), 2000)), Preps::None, Instant::ZERO).unwrap();
    let mut now = 0;
    steps(&mut s, &mut now, 8);
    assert_eq!(s.recv(receiver, &mut [0; 16], false).unwrap().full_len, 5);
    assert_eq!(s.recv(receiver, &mut [0; 16], false).unwrap().full_len, 8);
}
#[test]
fn ethernet_address_loss_is_reset_and_loopback_survives() {
    let (mut a, mut b, _, _) = wire_stacks();
    let budget = budget();
    let l = listener(&mut b, budget, Some(IpAddress::v4(10, 0, 0, 2)), false, false, 1234, 0);
    let c = connect(&mut a, budget, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), 0);
    let mut now = 0;
    wire_steps(&mut a, &mut b, &mut now, 16);
    let (peer, _) = b.accept(l).unwrap();
    let payload = [0x5a; 4096];
    b.send(peer, &payload, None, Preps::None, Instant::from_millis(now)).unwrap();
    wire_steps(&mut a, &mut b, &mut now, 16);
    assert_eq!(a.get_opt(c, OptQuery::MaxSeg), Ok(NetOpt::MaxSeg(1460)));
    a.set_opt(c, NetOpt::MaxSeg(1000), Instant::from_millis(now)).unwrap();
    assert_eq!(a.get_opt(c, OptQuery::MaxSeg), Ok(NetOpt::MaxSeg(1000)));
    a.configure_eth(None, None).unwrap();
    a.poll_round(Instant::from_millis(now));
    // Linux v6.12 net/ipv4/tcp.c, tcp_recvmsg_locked:2626-2669 returns
    // queued bytes before an error, including when a local route disappears.
    let mut received = [0; 4096];
    assert_eq!(a.recv(c, &mut received, false).unwrap().copied, payload.len());
    assert_eq!(received, payload);
    assert_eq!(a.recv(c, &mut received, false), Err(NetError::ConnReset));
    assert_eq!(a.recv(c, &mut received, false).unwrap().copied, 0);
    let l = listener(&mut a, budget, Some(addr(false)), false, false, 4321, 0);
    let local = connect(&mut a, budget, IpEndpoint::new(addr(false), 4321), now);
    steps(&mut a, &mut now, 16);
    assert!(a.accept(l).is_ok());
    assert!(a.readiness(local).writable);
}

#[test]
fn concurrent_handshakes_cannot_overfill_a_partly_full_accept_queue() {
    let mut s = stack();
    let b = budget();
    let l = listener(&mut s, b, Some(addr(false)), false, false, 1234, 8);
    let mut now = 0;
    for _ in 0..7 {
        connect(&mut s, b, IpEndpoint::new(addr(false), 1234), now);
    }
    steps(&mut s, &mut now, 16);
    refill(&mut s, b);
    let second: Vec<_> = (0..8).map(|_| connect(&mut s, b, IpEndpoint::new(addr(false), 1234), now)).collect();
    steps(&mut s, &mut now, 24);
    assert_eq!(second.iter().filter(|id| s.readiness(**id).writable).count(), 2);
    for _ in 0..9 {
        s.accept(l).unwrap();
    }
    assert_eq!(s.accept(l), Err(NetError::WouldBlock));
    refill(&mut s, b);
    now = 2000;
    steps(&mut s, &mut now, 24);
    for _ in 0..6 {
        s.accept(l).unwrap();
    }
}

#[test]
fn loopback_egress_requests_another_round_without_a_timer() {
    let mut s = stack();
    let b = budget();
    let a = bind_udp(&mut s, b, Some(addr(false)), 1000, false);
    let r = bind_udp(&mut s, b, Some(addr(false)), 1001, false);
    s.send(a, b"wake", Some(IpEndpoint::new(addr(false), 1001)), Preps::None, Instant::ZERO).unwrap();
    assert!(s.poll_round(Instant::ZERO).more);
    assert_eq!(s.poll_at(Instant::ZERO), Some(Instant::ZERO));
    assert!(!s.poll_round(Instant::ZERO).more);
    assert!(s.readiness(r).readable);
    assert_eq!(s.recv(r, &mut [0; 4], false).unwrap().copied, 4);
}
#[test]
fn udp_length_errors_precede_missing_destination_only_at_linux_outer_limit() {
    // Linux v6.12 net/ipv4/udp.c, udp_sendmsg:1081 precedes EDESTADDRREQ at 1128.
    let mut s = stack();
    let b = budget();
    let u = s.open(Family::V4, Proto::Udp).unwrap();
    let data = vec![0; 65536];
    assert_eq!(s.send(u, &data, None, Preps::None, Instant::ZERO), Err(NetError::MsgSize));
    assert_eq!(s.send(u, &data[..65535], None, Preps::None, Instant::ZERO), Err(NetError::DestAddrReq));
    assert_eq!(s.listen(u, 0, &mut std::iter::empty(), Instant::ZERO), Err(NetError::OpNotSupp));
    let u = bind_udp(&mut s, b, Some(addr(false)), 2000, false);
    assert_eq!(
        s.send(u, &data[..4097], Some(IpEndpoint::new(addr(false), 2001)), Preps::None, Instant::ZERO),
        Err(NetError::MsgSize)
    );
}

#[test]
fn udp_v6only_survives_implicit_bind_and_connected_names_are_local() {
    for connected in [false, true] {
        let mut s = stack();
        let b = budget();
        let u = s.open(Family::V6, Proto::Udp).unwrap();
        s.set_opt(u, NetOpt::V6Only(true), Instant::ZERO).unwrap();
        if connected {
            s.connect(u, IpEndpoint::new(addr(true), 2000), Some(ConnectPrep::Udp(udp(b))), Instant::ZERO).unwrap();
        } else {
            s.send(
                u,
                b"v6",
                Some(IpEndpoint::new(addr(true), 2000)),
                Preps::Udp(&mut [udp(b)].into_iter()),
                Instant::ZERO,
            )
            .unwrap();
        }
        assert_eq!(s.get_opt(u, OptQuery::V6Only), Ok(NetOpt::V6Only(true)));
        assert_eq!(
            s.send(u, b"v4", Some(IpEndpoint::new(addr(false), 2000)), Preps::None, Instant::ZERO),
            Err(NetError::NetUnreach)
        );
    }
    let mut s = stack();
    let b = budget();
    let u = bind_udp(&mut s, b, None, 2000, false);
    s.connect(u, IpEndpoint::new(addr(false), 2001), None, Instant::ZERO).unwrap();
    assert_eq!(s.local_endpoint(u), Some(IpEndpoint::new(addr(false), 2000)));
}

#[test]
fn time_wait_keeps_buffers_until_final_ack_reaches_the_device() {
    let (mut a, mut b, _, cb) = wire_stacks();
    let ba = budget();
    let bb = budget();
    let l = listener(&mut b, bb, Some(IpAddress::v4(10, 0, 0, 2)), false, false, 1234, 0);
    let c = connect(&mut a, ba, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), 0);
    let port = a.local_endpoint(c).unwrap().port;
    let mut now = 0;
    wire_steps(&mut a, &mut b, &mut now, 16);
    let (server, _) = b.accept(l).unwrap();
    a.close(c, false, Instant::from_millis(now));
    wire_steps(&mut a, &mut b, &mut now, 8);
    // Updating the same address clears the neighbor cache without resetting
    // the connection. The final ACK must now wait for a new ARP answer.
    a.configure_eth(Some(Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 1), 24)), None).unwrap();
    b.close(server, false, Instant::from_millis(now));
    b.poll_round(Instant::from_millis(now));
    cb.drop_all.store(true, Ordering::Relaxed);
    a.poll_round(Instant::from_millis(now));
    let entered = now;
    assert_eq!(ba.used(), 8192);
    assert!(!a.time_wait_ports().any(|(p, _)| p == port));
    cb.drop_all.store(false, Ordering::Relaxed);
    now += 1;
    wire_steps(&mut a, &mut b, &mut now, 16);
    // smoltcp's immediate reply was lost to neighbor discovery; the peer's
    // retransmitted FIN elicits the ACK once ARP has completed.
    now = 2000;
    wire_steps(&mut a, &mut b, &mut now, 16);
    assert_eq!(ba.used(), 0);
    assert_eq!(a.time_wait_ports().find(|(p, _)| *p == port).unwrap().1, Instant::from_millis(entered + 10000));
}

#[test]
fn failed_connect_consumes_the_error_and_allows_a_fresh_attempt() {
    // Linux v6.12 net/ipv4/af_inet.c, __inet_stream_connect:734-739: sock_error or ECONNABORTED,
    // followed by SS_UNCONNECTED and the protocol's disconnect operation.
    for read_so_error in [false, true] {
        let mut s = stack();
        let b = budget();
        let remote = IpEndpoint::new(addr(false), 1234);
        let c = connect(&mut s, b, remote, 0);
        let mut now = 0;
        steps(&mut s, &mut now, 8);
        if read_so_error {
            assert_eq!(s.take_error(c), Some(NetError::ConnRefused));
        }
        assert_eq!(
            s.connect(c, remote, None, Instant::from_millis(now)),
            Err(if read_so_error { NetError::ConnAborted } else { NetError::ConnRefused })
        );
        assert_eq!(b.used(), 0);
        assert!(s.port_count() == 0);
        let l = listener(&mut s, b, Some(addr(false)), false, false, 1234, 0);
        assert_eq!(s.connect(c, remote, Some(prep(b)), Instant::from_millis(now)), Err(NetError::InProgress));
        steps(&mut s, &mut now, 8);
        assert!(s.accept(l).is_ok());
        assert_eq!(s.take_error(c), None);
        assert!(s.readiness(c).writable);
    }
}
