mod support;
use lumen_net::*;
use support::*;
fn at(ms: i64) -> Instant {
    Instant::from_millis(ms)
}

#[test]
fn idle_close_preserves_data_then_eof() {
    // Linux v6.12 net/ipv4/tcp.c, __tcp_close:3069-3130: queued writes
    // precede the FIN; an idle connection does not acquire an idle timeout.
    for idle in [61_000, 3_600_000] {
        for polled in [false, true] {
            for sent in [false, true] {
                let (mut s, _, _, c, a, mut now) = pair(false);
                if sent {
                    s.send(c, b"bye", None, Preps::None, at(now)).unwrap();
                    steps(&mut s, &mut now, 8);
                }
                now += idle;
                if polled {
                    steps(&mut s, &mut now, 2);
                }
                if !sent {
                    s.send(c, b"bye", None, Preps::None, at(now)).unwrap();
                }
                s.close(c, false, at(now));
                steps(&mut s, &mut now, 10000);
                let mut data = [0; 8];
                assert_eq!(s.recv(a, &mut data, false).unwrap().copied, 3);
                assert_eq!(&data[..3], b"bye");
                assert_eq!(
                    s.recv(a, &mut data, false).map(|r| r.copied),
                    Ok(0),
                    "idle={idle} polled={polled} sent={sent} readiness={:?}",
                    s.readiness(a)
                );
                assert_eq!(s.take_error(a), None);
            }
        }
    }
}
#[test]
fn keepalive_enabled_after_idle_probes_a_live_peer() {
    // Linux v6.12 net/ipv4/tcp_timer.c, tcp_keepalive_timer:797-830:
    // idle expiry sends a probe before giving the peer up.
    let (mut s, _, _, c, a, mut now) = pair(false);
    now += 30_000;
    s.set_opt(c, NetOpt::KeepIdle(Duration::from_secs(1)), at(now)).unwrap();
    s.set_opt(c, NetOpt::KeepIntvl(Duration::from_secs(1)), at(now)).unwrap();
    s.set_opt(c, NetOpt::KeepCnt(1), at(now)).unwrap();
    s.set_opt(c, NetOpt::KeepAlive(true), at(now)).unwrap();
    assert!(s.poll_round(at(now)).more, "keepalive probe was transmitted");
    steps(&mut s, &mut now, 8);
    now += 1_001;
    steps(&mut s, &mut now, 8);
    assert_eq!(s.take_error(c), None);
    assert!(!s.readiness(c).hup);
    s.send(a, b"alive", None, Preps::None, at(now)).unwrap();
    steps(&mut s, &mut now, 8);
    assert_eq!(s.recv(c, &mut [0; 8], false).unwrap().copied, 5);
}
#[test]
fn reuseaddr_listen_checks_other_listeners_and_allows_restart() {
    // Linux v6.12 net/ipv4/inet_connection_sock.c, inet_bind_conflict:185-191:
    // only sk2 (the existing socket) being TCP_LISTEN defeats REUSEADDR.
    let mut s = stack();
    let b = budget();
    let req = BindReq { port: 1234, reuse_addr: true, ..BindReq::default() };
    let first = s.open(Family::V4, Proto::Tcp).unwrap();
    let second = s.open(Family::V4, Proto::Tcp).unwrap();
    for id in [first, second] {
        s.bind(id, req, Preps::None, at(0)).unwrap();
    }
    s.listen(first, 0, sizes(), &mut [prepare_tcp(b, sizes()).unwrap()].into_iter(), at(0)).unwrap();
    assert_eq!(
        s.listen(second, 0, sizes(), &mut [prepare_tcp(b, sizes()).unwrap()].into_iter(), at(0)),
        Err(NetError::AddrInUse)
    );
    let _c = connect(&mut s, b, IpEndpoint::new(addr(false), 1234), 0);
    let mut now = 0;
    steps(&mut s, &mut now, 8);
    let (a, _) = s.accept(first).unwrap();
    s.close(first, false, at(now));
    s.listen(second, 0, sizes(), &mut [prepare_tcp(b, sizes()).unwrap()].into_iter(), at(now)).unwrap();
    s.close(second, false, at(now));
    s.close(a, false, at(now));
    steps(&mut s, &mut now, 8);
    let third = s.open(Family::V4, Proto::Tcp).unwrap();
    s.bind(third, req, Preps::None, at(now)).unwrap();
    s.listen(third, 0, sizes(), &mut [prepare_tcp(b, sizes()).unwrap()].into_iter(), at(now)).unwrap();
}
#[test]
fn receive_queue_precedes_reset_error_but_send_reports_error_first() {
    // Linux v6.12 net/ipv4/tcp.c, tcp_recvmsg_locked:2626-2669 reads the
    // receive queue before sock_error; tcp_sendmsg_locked:1143-1144 checks sk_err.
    for send_first in [false, true] {
        let (mut s, _, _, c, a, mut now) = pair(false);
        s.send(a, b"queued", None, Preps::None, at(now)).unwrap();
        steps(&mut s, &mut now, 8);
        s.close(a, true, at(now));
        steps(&mut s, &mut now, 8);
        if send_first {
            assert_eq!(s.send(c, b"x", None, Preps::None, at(now)), Err(NetError::ConnReset));
        }
        let mut data = [0; 8];
        assert_eq!(s.recv(c, &mut data, true).unwrap().copied, 6);
        assert_eq!(&data[..6], b"queued");
        let r = s.recv(c, &mut data, true).unwrap();
        s.consume(c, r.ticket).unwrap();
        if !send_first {
            assert_eq!(s.recv(c, &mut data, false), Err(NetError::ConnReset));
        }
        assert_eq!(s.recv(c, &mut data, false).unwrap().copied, 0);
    }
}
#[test]
fn graceful_closed_states_preserve_unread_data_without_error() {
    // Linux v6.12 net/ipv4/tcp.c, tcp_recvmsg_locked:2626-2669: receive
    // data remains ahead of the close condition, including passive close.
    for active in [false, true] {
        let (mut s, _, _, c, a, mut now) = pair(false);
        s.send(a, b"queued", None, Preps::None, at(now)).unwrap();
        steps(&mut s, &mut now, 8);
        if active {
            s.shutdown(c, Shut::Wr, at(now)).unwrap();
            steps(&mut s, &mut now, 8);
            s.close(a, false, at(now));
        } else {
            s.close(a, false, at(now));
            steps(&mut s, &mut now, 8);
            s.shutdown(c, Shut::Wr, at(now)).unwrap();
        }
        steps(&mut s, &mut now, 8);
        now += 11_000;
        steps(&mut s, &mut now, 8);
        assert_eq!(s.take_error(c), None);
        assert!(s.readiness(c).readable);
        let mut data = [0; 8];
        assert_eq!(s.recv(c, &mut data, false).unwrap().copied, 6);
        assert_eq!(&data[..6], b"queued");
        assert_eq!(s.recv(c, &mut data, false).unwrap().copied, 0);
    }
}
#[test]
fn reset_in_close_wait_reports_epipe() {
    // Linux v6.12 net/ipv4/tcp_input.c, tcp_reset:4538-4549.
    let (mut s, _, _, c, a, mut now) = pair(false);
    s.shutdown(a, Shut::Wr, at(now)).unwrap();
    steps(&mut s, &mut now, 8);
    s.close(a, true, at(now));
    steps(&mut s, &mut now, 8);
    assert_eq!(s.take_error(c), Some(NetError::Pipe));
}
#[test]
fn completed_then_reset_connect_reports_error_or_aborted() {
    // Linux v6.12 net/ipv4/af_inet.c, __inet_stream_connect:718-738:
    // SS_CONNECTING plus TCP_CLOSE cannot complete successfully.
    for drained in [false, true] {
        let (mut s, _, _, c, a, mut now) = pair(false);
        s.close(a, true, at(now));
        steps(&mut s, &mut now, 8);
        if drained {
            assert_eq!(s.take_error(c), Some(NetError::ConnReset));
        }
        assert_eq!(
            s.connect(c, IpEndpoint::new(addr(false), 1234), None, at(now)),
            Err(if drained { NetError::ConnAborted } else { NetError::ConnReset })
        );
    }
}
#[test]
fn changed_lists_connect_completion_listener_and_syn_timeout() {
    // Linux v6.12 net/ipv4/tcp_input.c, tcp_finish_connect:6315-6335
    // changes state and wakes the connecting socket (section 7 event contract).
    let mut s = stack();
    let b = budget();
    let l = listener(&mut s, b, Some(addr(false)), false, false, 1234, 0);
    let c = connect(&mut s, b, IpEndpoint::new(addr(false), 1234), 0);
    s.changed();
    let mut now = 0;
    steps(&mut s, &mut now, 8);
    let changed = s.changed();
    assert!(changed.contains(&c));
    assert!(changed.contains(&l));
    let (mut a, _, _, _) = wire_stacks();
    let c = connect(&mut a, b, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), 0);
    a.poll_round(at(0));
    a.changed();
    a.poll_round(at(127_000));
    assert!(a.changed().contains(&c));
}
#[test]
fn changed_lists_tcp_send_space_and_survives_inline_recv_consume() {
    let (mut s, _, _, c, a, mut now) = pair(false);
    s.send(c, &[1; 4096], None, Preps::None, at(now)).unwrap();
    steps(&mut s, &mut now, 1000);
    s.send(c, &[2; 4096], None, Preps::None, at(now)).unwrap();
    steps(&mut s, &mut now, 8);
    s.changed();
    s.recv(a, &mut [0; 4096], false).unwrap();
    steps(&mut s, &mut now, 1000);
    assert!(s.changed().contains(&c));
    let ticket = s.recv(a, &mut [0; 4096], true).unwrap().ticket;
    s.consume(a, ticket).unwrap();
    assert!(s.changed().contains(&a));
    // Clear the inline-read listing, then isolate an arrival and drain it
    // without another poll. A recv mutant clearing dirty loses this wake.
    s.changed();
    s.send(c, b"x", None, Preps::None, at(now)).unwrap();
    steps(&mut s, &mut now, 8);
    s.recv(a, &mut [0; 8], false).unwrap();
    assert!(s.changed().contains(&a));
}
#[test]
fn changed_lists_udp_and_icmp_send_space_and_survives_consume() {
    for proto in [Proto::Udp, Proto::IcmpEcho { raw: false }] {
        let mut s = stack();
        let b = budget();
        let id = s.open(Family::V4, proto).unwrap();
        let req = BindReq { addr: Some(addr(false)), port: 1000, ..BindReq::default() };
        if proto == Proto::Udp {
            s.bind(id, req, Preps::Udp(&mut [udp(b)].into_iter()), at(0)).unwrap();
        } else {
            s.bind(id, req, Preps::Icmp(&mut [prepare_icmp(b).unwrap()].into_iter()), at(0)).unwrap();
        }
        let packet = [8, 0, 0, 0, 0, 0, 0, 1];
        let data = if proto == Proto::Udp { &b"data"[..] } else { &packet[..] };
        s.send(id, data, Some(IpEndpoint::new(addr(false), 1000)), Preps::None, at(0)).unwrap();
        s.changed();
        s.poll_round(at(0));
        assert!(s.changed().contains(&id), "{proto:?} send space");
        let mut now = 1;
        steps(&mut s, &mut now, 8);
        let (_, t) = s.datagram_len(id).unwrap();
        s.consume(id, t).unwrap();
        assert!(s.changed().contains(&id));
        assert_eq!(s.consume(id, t), Err(NetError::StaleTicket));
    }
}
fn echo(ident: u16) -> [u8; 12] {
    let mut data = [8, 0, 0, 0, 0, 0, 0, 1, 1, 2, 3, 4];
    data[4..6].copy_from_slice(&ident.to_be_bytes());
    smoltcp::wire::Icmpv4Packet::new_unchecked(&mut data[..]).fill_checksum();
    data
}
#[test]
fn icmp_echo_roundtrip_ping_rewrites_identifier_raw_preserves_it() {
    // Linux v6.12 net/ipv4/ping.c, ping_get_port:78-103 (1..65535),
    // ping_v4_sendmsg:816 overwrites the caller's identifier.
    for raw in [false, true] {
        let mut s = stack();
        let b = budget();
        let id = s.open(Family::V4, Proto::IcmpEcho { raw }).unwrap();
        s.send(
            id,
            &echo(0x1234),
            Some(IpEndpoint::new(addr(false), 0)),
            Preps::Icmp(&mut [prepare_icmp(b).unwrap()].into_iter()),
            at(0),
        )
        .unwrap();
        let ident = s.local_endpoint(id).unwrap().port;
        assert_eq!(ident, if raw { 0x1234 } else { 1 });
        let mut now = 0;
        steps(&mut s, &mut now, 8);
        let mut data = [0; 64];
        let mut reply = false;
        for _ in 0..2 {
            if let Ok(r) = s.recv(id, &mut data, false) {
                let h = if raw { 20 } else { 0 };
                assert_eq!(&data[h + 4..h + 6], &ident.to_be_bytes());
                reply |= data[h] == 0;
                assert_eq!(r.full_len, 12 + h);
            }
        }
        assert!(reply);
        assert_eq!(
            s.send(id, &[8; 4097], Some(IpEndpoint::new(addr(false), 0)), Preps::None, at(now)),
            Err(NetError::MsgSize)
        );
    }
}
#[test]
fn raw_icmp_ethernet_header_uses_local_address() {
    let (mut a, mut b, _, _) = wire_stacks();
    let budget = budget();
    let id = a.open(Family::V4, Proto::IcmpEcho { raw: true }).unwrap();
    let preps = [prepare_icmp(budget).unwrap(), prepare_icmp(budget).unwrap()];
    a.send(
        id,
        &echo(2345),
        Some(IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 0)),
        Preps::Icmp(&mut preps.into_iter()),
        at(0),
    )
    .unwrap();
    let mut now = 0;
    wire_steps(&mut a, &mut b, &mut now, 16);
    let mut data = [0; 64];
    let r = a.recv(id, &mut data, false).unwrap();
    assert_eq!(r.full_len, 32);
    assert_eq!(&data[12..16], &[10, 0, 0, 2]);
    assert_eq!(&data[16..20], &[10, 0, 0, 1]);
    assert_eq!(data[20], 0);
}
#[test]
fn preparations_match_operations_and_late_ethernet_attachment() {
    let (wire, _) = Wire::pair();
    let mut s = stack();
    let b = budget();
    let u = s.open(Family::V4, Proto::Udp).unwrap();
    let req = BindReq { port: 2000, ..BindReq::default() };
    let n = s.preps_needed(u, PrepFor::Bind(&req));
    assert_eq!(n, 1);
    let mut ps = (0..n).map(|_| udp(b)).collect::<Vec<_>>().into_iter();
    s.bind(u, req, Preps::Udp(&mut ps), at(0)).unwrap();
    assert_eq!(ps.len(), 0);
    s.attach_eth(
        wire,
        EthConfig::Static { addr: Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 1), 24), router: None },
        at(0),
        73,
    )
    .unwrap();
    let ep = IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 2000);
    assert_eq!(s.preps_needed(u, PrepFor::Connect(ep)), 1);
    s.connect(u, ep, Some(ConnectPrep::Udp(udp(b))), at(0)).unwrap();
    assert_eq!(s.preps_needed(u, PrepFor::Connect(ep)), 0);
    let u = s.open(Family::V4, Proto::Udp).unwrap();
    assert_eq!(s.preps_needed(u, PrepFor::Connect(ep)), 1);
    s.connect(u, ep, Some(ConnectPrep::Udp(udp(b))), at(0)).unwrap();
    let u = s.open(Family::V4, Proto::Udp).unwrap();
    let req = BindReq { addr: Some(addr(false)), port: 2001, ..req };
    assert_eq!(s.preps_needed(u, PrepFor::Bind(&req)), 1);
    let mut ps = [udp(b)].into_iter();
    s.bind(u, req, Preps::Udp(&mut ps), at(0)).unwrap();
    assert_eq!(ps.len(), 0);
    let l = s.open(Family::V4, Proto::Tcp).unwrap();
    let n = s.preps_needed(l, PrepFor::Listen(2));
    assert_eq!(n, 6);
    let mut ps = (0..n).map(|_| prepare_tcp(b, sizes()).unwrap()).collect::<Vec<_>>().into_iter();
    s.listen(l, 2, sizes(), &mut ps, at(0)).unwrap();
    assert_eq!(ps.len(), 0);
}
#[test]
fn specific_listener_address_and_refill_noop_stale_budget() {
    let mut s = stack();
    let b = budget();
    let first = listener(&mut s, b, Some(addr(false)), false, false, 1234, 0);
    let second = listener(&mut s, b, Some(IpAddress::v4(127, 0, 0, 2)), false, false, 1234, 0);
    connect(&mut s, b, IpEndpoint::new(IpAddress::v4(127, 0, 0, 2), 1234), 0);
    let mut now = 0;
    steps(&mut s, &mut now, 8);
    assert_eq!(s.accept(first), Err(NetError::WouldBlock));
    assert!(s.accept(second).is_ok());
    let count = s.socket_count();
    let used = b.used();
    assert_eq!(s.refill(first, IfaceId::Lo, prepare_tcp(b, sizes()).unwrap(), at(now)), Ok(false));
    assert_eq!(s.socket_count(), count);
    assert_eq!(b.used(), used);
    s.close(first, false, at(now));
    let count = s.socket_count();
    let used = b.used();
    assert_eq!(s.refill(first, IfaceId::Lo, prepare_tcp(b, sizes()).unwrap(), at(now)), Ok(false));
    assert_eq!(s.socket_count(), count);
    assert_eq!(b.used(), used);
}
#[test]
fn udp_ipv6_errno_order_zero_port_broadcast_ttl_and_eth_mtu() {
    // Linux v6.12 net/ipv6/udp.c, udpv6_sendmsg:1409,1436,1465;
    // net/ipv4/udp.c, udp_sendmsg:1247-1250 enforces SO_BROADCAST.
    let (mut s, _, _, _) = wire_stacks();
    let b = budget();
    let u = s.open(Family::V6, Proto::Udp).unwrap();
    assert_eq!(s.send(u, &[0; 65536], None, Preps::None, at(0)), Err(NetError::DestAddrReq));
    assert_eq!(s.send(u, b"x", Some(IpEndpoint::new(addr(true), 0)), Preps::None, at(0)), Err(NetError::Inval));
    let u = s.open(Family::V4, Proto::Udp).unwrap();
    let ep = IpEndpoint::new(IpAddress::v4(10, 0, 0, 255), 9);
    assert_eq!(s.send(u, b"x", Some(ep), Preps::None, at(0)), Err(NetError::Acces));
    s.set_opt(u, NetOpt::Broadcast(true), at(0)).unwrap();
    s.set_opt(u, NetOpt::Ttl(42), at(0)).unwrap();
    assert_eq!(
        s.send(u, &[0; 1473], Some(ep), Preps::Udp(&mut [udp(b), udp(b)].into_iter()), at(0)),
        Err(NetError::MsgSize)
    );
    assert_eq!(s.send(u, &[0; 1472], Some(ep), Preps::Udp(&mut [udp(b), udp(b)].into_iter()), at(0)), Ok(1472));
}
#[test]
fn keepalive_defaults_ranges_and_shutdown_states() {
    // Linux v6.12 include/net/tcp.h:163-169 defines the defaults/limits.
    // net/ipv4/tcp.c, tcp_sock_set_keepidle_locked:3610-3634,
    // tcp_sock_set_keepintvl:3644-3654, tcp_sock_set_keepcnt:3660-3670;
    // net/ipv4/af_inet.c, inet_shutdown:917-939.
    let mut s = stack();
    let b = budget();
    let id = s.open(Family::V4, Proto::Tcp).unwrap();
    for (q, v) in [
        (OptQuery::KeepIdle, NetOpt::KeepIdle(Duration::from_secs(7200))),
        (OptQuery::KeepIntvl, NetOpt::KeepIntvl(Duration::from_secs(75))),
        (OptQuery::KeepCnt, NetOpt::KeepCnt(9)),
    ] {
        assert_eq!(s.get_opt(id, q), Ok(v));
    }
    for v in [
        NetOpt::KeepIdle(Duration::ZERO),
        NetOpt::KeepIdle(Duration::from_secs(32768)),
        NetOpt::KeepIntvl(Duration::ZERO),
        NetOpt::KeepIntvl(Duration::from_secs(32768)),
        NetOpt::KeepCnt(0),
        NetOpt::KeepCnt(128),
    ] {
        assert_eq!(s.set_opt(id, v, at(0)), Err(NetError::Inval));
    }
    assert_eq!(s.shutdown(id, Shut::RdWr, at(0)), Err(NetError::NotConn));
    assert!(s.readiness(id).hup);
    assert_eq!(s.send(id, b"x", None, Preps::None, at(0)), Err(NetError::Pipe));
    let l = listener(&mut s, b, Some(addr(false)), false, false, 1234, 0);
    assert_eq!(s.shutdown(l, Shut::Wr, at(0)), Ok(()));
    assert_eq!(s.shutdown(l, Shut::Rd, at(0)), Ok(()));
    assert_eq!(s.accept(l), Err(NetError::Inval));
    let c = connect(&mut s, b, IpEndpoint::new(addr(false), 9999), 0);
    assert_eq!(s.shutdown(c, Shut::Wr, at(0)), Ok(()));
    let (mut s, _, _, c, a, mut now) = pair(false);
    s.close(a, true, at(now));
    steps(&mut s, &mut now, 8);
    assert_eq!(s.shutdown(c, Shut::Wr, at(now)), Err(NetError::NotConn));
}

#[test]
fn fin_wait_2_timeout_starts_at_acknowledged_fin() {
    // Lumen's bounded orphan cleanup restarts at FIN-WAIT-2 entry.
    // Deviation: Linux v6.12 net/ipv4/tcp_input.c,
    // tcp_rcv_state_process:6888-6915 uses a timewait mini-socket, not RST.
    let (mut a, mut b, _, _) = wire_stacks();
    let ba = budget();
    let bb = budget();
    let l = listener(&mut b, bb, Some(IpAddress::v4(10, 0, 0, 2)), false, false, 1234, 0);
    let c = connect(&mut a, ba, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), 0);
    let mut now = 0;
    wire_steps(&mut a, &mut b, &mut now, 16);
    let (peer, _) = b.accept(l).unwrap();
    a.close(c, false, at(1000));
    a.poll_round(at(1000));
    assert_eq!(ba.used(), 8192);
    // The peer has not processed the FIN yet. Advance its input explicitly.
    now = 5000;
    wire_steps(&mut a, &mut b, &mut now, 1000);
    let deadline = a.poll_at(at(now)).unwrap();
    assert!(deadline >= at(65_000));
    a.poll_round(at(61_000));
    assert_eq!(ba.used(), 8192);
    a.poll_round(deadline - Duration::from_millis(1));
    assert_eq!(ba.used(), 8192);
    a.poll_round(deadline);
    // ARP has expired. The deadline abort gets one egress attempt; cleanup
    // must release the slot even when the reset cannot leave.
    now = deadline.total_millis();
    wire_steps(&mut a, &mut b, &mut now, 2000);
    assert_eq!(ba.used(), 0);
    assert_eq!(b.take_error(peer), None);
}
#[test]
fn wildcard_udp_send_installs_socket_after_ethernet_attachment() {
    let (wire, other) = Wire::pair();
    let b = budget();
    let mut a = stack();
    let mut peer = stack();
    let u = bind_udp(&mut a, b, None, 2222, false);
    for (s, dev, octet) in [(&mut a, wire, 1), (&mut peer, other, 2)] {
        s.attach_eth(
            dev,
            EthConfig::Static { addr: Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, octet), 24), router: None },
            at(0),
            octet as u64,
        )
        .unwrap();
    }
    let receiver = bind_udp(&mut peer, b, Some(IpAddress::v4(10, 0, 0, 2)), 2222, false);
    let ep = IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 2222);
    assert_eq!(a.preps_needed(u, PrepFor::Send(Some(ep))), 1);
    let mut preps = [udp(b)].into_iter();
    a.send(u, b"late", Some(ep), Preps::Udp(&mut preps), at(0)).unwrap();
    assert_eq!(preps.len(), 0);
    let mut now = 0;
    wire_steps(&mut a, &mut peer, &mut now, 16);
    assert_eq!(peer.recv(receiver, &mut [0; 8], false).unwrap().copied, 4);
}
#[test]
fn ethernet_v4_cannot_enter_v6only_tcp_or_udp_wildcards() {
    let (mut a, mut b, _, _) = wire_stacks();
    let budget = budget();
    let l = listener(&mut b, budget, None, true, true, 1234, 0);
    let c = connect(&mut a, budget, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), 0);
    let u = b.open(Family::V6, Proto::Udp).unwrap();
    b.set_opt(u, NetOpt::V6Only(true), at(0)).unwrap();
    b.bind(
        u,
        BindReq { port: 2345, ..BindReq::default() },
        Preps::Udp(&mut [udp(budget), udp(budget)].into_iter()),
        at(0),
    )
    .unwrap();
    let sender = bind_udp(&mut a, budget, None, 2346, false);
    a.send(sender, b"wrong", Some(IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 2345)), Preps::None, at(0)).unwrap();
    let mut now = 0;
    wire_steps(&mut a, &mut b, &mut now, 16);
    assert_eq!(a.take_error(c), Some(NetError::ConnRefused));
    assert_eq!(b.accept(l), Err(NetError::WouldBlock));
    assert_eq!(b.recv(u, &mut [0; 16], false), Err(NetError::WouldBlock));
    let v4 = bind_udp(&mut b, budget, None, 2347, false);
    a.send(sender, b"ok", Some(IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 2347)), Preps::None, at(now)).unwrap();
    wire_steps(&mut a, &mut b, &mut now, 16);
    assert_eq!(b.recv(v4, &mut [0; 16], false).unwrap().copied, 2);
}

#[test]
fn icmp_ipv6_echo_and_unaddressable_error() {
    // Linux v6.12 net/ipv4/ping.c, ping_get_port:78-103 reserves zero
    // for automatic ping allocation; raw echo identifiers are packet data.
    for raw in [false, true] {
        let mut s = stack();
        let b = budget();
        let id = s.open(Family::V6, Proto::IcmpEcho { raw }).unwrap();
        let mut request = echo(0);
        request[0] = 128;
        smoltcp::wire::Icmpv6Packet::new_unchecked(&mut request[..])
            .fill_checksum(&Ipv6Address::LOCALHOST, &Ipv6Address::LOCALHOST);
        s.send(
            id,
            &request,
            Some(IpEndpoint::new(addr(true), 0)),
            Preps::Icmp(&mut [prepare_icmp(b).unwrap()].into_iter()),
            at(0),
        )
        .unwrap();
        assert_eq!(s.local_endpoint(id).unwrap().port, if raw { 0 } else { 1 });
        let mut now = 0;
        steps(&mut s, &mut now, 8);
        let mut data = [0; 32];
        let mut reply = false;
        for _ in 0..2 {
            if let Ok(r) = s.recv(id, &mut data, false) {
                assert_eq!(r.full_len, 12);
                reply |= data[0] == 129;
            }
        }
        assert!(reply);
        assert_eq!(
            s.send(
                id,
                &request,
                Some(IpEndpoint::new(IpAddress::Ipv6(Ipv6Address::UNSPECIFIED), 0)),
                Preps::None,
                at(now)
            ),
            Err(NetError::AddrNotAvail)
        );
    }
}
#[test]
fn open_family_does_not_follow_the_first_destination() {
    let mut s = stack();
    let b = budget();
    let u = s.open(Family::V4, Proto::Udp).unwrap();
    assert_eq!(s.set_opt(u, NetOpt::V6Only(false), at(0)), Err(NetError::Inval));
    assert_eq!(
        s.connect(u, IpEndpoint::new(addr(true), 80), Some(ConnectPrep::Udp(udp(b))), at(0)),
        Err(NetError::NetUnreach)
    );
    assert_eq!(
        s.bind(
            u,
            BindReq { addr: Some(addr(true)), ..BindReq::default() },
            Preps::Udp(&mut [udp(b)].into_iter()),
            at(0)
        ),
        Err(NetError::Inval)
    );
    let (s, _, _, _) = wire_stacks();
    let mut s = s;
    let u = s.open(Family::V4, Proto::Udp).unwrap();
    assert_eq!(
        s.preps_needed(
            u,
            PrepFor::Bind(&BindReq { addr: Some(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)), ..BindReq::default() })
        ),
        2
    );
}

#[test]
fn shutdown_pending_connect_releases_transport_and_can_reconnect() {
    // Linux v6.12 net/ipv4/af_inet.c, inet_shutdown:930-939 disconnects
    // SYN-SENT and returns the socket to SS_UNCONNECTED.
    let mut s = stack();
    let b = budget();
    let remote = IpEndpoint::new(addr(false), 1234);
    let c = connect(&mut s, b, remote, 0);
    s.shutdown(c, Shut::Wr, at(0)).unwrap();
    assert_eq!(s.socket_count(), 0);
    assert_eq!(b.used(), 0);
    let l = listener(&mut s, b, Some(addr(false)), false, false, 1234, 0);
    assert_eq!(s.preps_needed(c, PrepFor::Connect(remote)), 1);
    assert_eq!(s.connect(c, remote, Some(prep(b)), at(0)), Err(NetError::InProgress));
    let mut now = 0;
    steps(&mut s, &mut now, 8);
    let (peer, _) = s.accept(l).unwrap();
    s.send(c, b"retry", None, Preps::None, at(now)).unwrap();
    steps(&mut s, &mut now, 8);
    assert_eq!(s.recv(peer, &mut [0; 8], false).unwrap().copied, 5);
}
