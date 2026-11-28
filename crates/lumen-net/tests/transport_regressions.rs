mod support;
use lumen_net::*;
use std::sync::atomic::Ordering;
use support::*;
fn at(ms: i64) -> Instant {
    Instant::from_millis(ms)
}
fn echo(id: u16) -> [u8; 12] {
    let mut data = [8, 0, 0, 0, 0, 0, 0, 1, 1, 2, 3, 4];
    data[4..6].copy_from_slice(&id.to_be_bytes());
    smoltcp::wire::Icmpv4Packet::new_unchecked(&mut data[..]).fill_checksum();
    data
}
fn ping(s: &mut Stack<Wire>, b: &'static Budget, raw: bool, dst: IpAddress, ident: u16) -> SockId {
    let id = s.open(Family::V4, Proto::IcmpEcho { raw }).unwrap();
    let preps: Vec<_> = (0..s.preps_needed(id, PrepFor::Send(Some(IpEndpoint::new(dst, 0)))))
        .map(|_| prepare_icmp(b).unwrap())
        .collect();
    s.send(id, &echo(ident), Some(IpEndpoint::new(dst, 0)), Preps::Icmp(&mut preps.into_iter()), at(0)).unwrap();
    id
}
#[test]
fn probe_ping_lo_while_dhcp_pending() {
    // Linux v6.12 net/ipv4/ping.c, ping_v4_sendmsg:816 uses the socket
    // identifier. Lack of an Ethernet lease must not affect loopback ping.
    let (wire, _) = Wire::pair();
    let mut s = stack();
    let b = budget();
    s.attach_eth(wire, EthConfig::Dhcp, at(0), 1).unwrap();
    let id = ping(&mut s, b, false, addr(false), 17);
    let mut now = 0;
    steps(&mut s, &mut now, 12);
    let mut data = [0; 32];
    assert_eq!(s.recv(id, &mut data, false).unwrap().full_len, 12);
    assert_eq!(data[0], 0);
    assert_eq!(u16::from_be_bytes([data[4], data[5]]), s.local_endpoint(id).unwrap().port);
}
#[test]
fn probe_ping_survives_address_loss() {
    let (mut s, _, _, _) = wire_stacks();
    let b = budget();
    let id = ping(&mut s, b, false, addr(false), 17);
    s.configure_eth(None, None).unwrap();
    let mut now = 0;
    steps(&mut s, &mut now, 12);
    assert_eq!(s.recv(id, &mut [0; 32], false).unwrap().full_len, 12);
    assert_eq!(s.take_error(id), None);
}
#[test]
fn raw_ping_headers_follow_each_received_destination_and_identifiers_are_shared() {
    // Linux v6.12 net/ipv4/ping.c, ping_get_port:78-103 reserves ping
    // identifiers; net/ipv4/raw.c, raw_v4_match:119-129 and raw_v4_input:
    // 174-193 deliver to every matching raw socket without identifier ownership.
    // raw_rcv:311-322 retains the received IP header, including its destination.
    let mut s = stack();
    let b = budget();
    let p = ping(&mut s, b, false, addr(false), 100);
    let ident = s.local_endpoint(p).unwrap().port;
    let alias = IpAddress::v4(127, 0, 0, 2);
    let r = ping(&mut s, b, true, alias, ident);
    let r2 = ping(&mut s, b, true, alias, ident);
    assert_eq!(s.port_count(), 1, "raw sockets do not reserve identifiers");
    let mut now = 0;
    steps(&mut s, &mut now, 16);
    for id in [r, r2] {
        let mut requests = 0;
        let mut replies = 0;
        let mut data = [0; 64];
        while let Ok(received) = s.recv(id, &mut data, false) {
            assert_eq!(received.full_len, 32);
            let header = smoltcp::wire::Ipv4Packet::new_checked(&data[..received.full_len]).unwrap();
            assert!(header.verify_checksum());
            if data[20] == 8 && header.dst_addr() == Ipv4Address::new(127, 0, 0, 2) {
                requests += 1;
            }
            if data[20] == 0 && header.src_addr() == Ipv4Address::new(127, 0, 0, 2) {
                replies += 1;
            }
        }
        assert_eq!((requests, replies), (2, 2));
    }
}
#[test]
fn closing_syn_sent_or_closed_releases_all_resources_without_arp() {
    // Linux v6.12 net/ipv4/tcp.c, new_state:2950 and __tcp_close:3076-3078.
    for timeout in [false, true] {
        let (mut s, _, control, _) = wire_stacks();
        control.drop_all.store(true, Ordering::Relaxed);
        let b = budget();
        let id = connect(&mut s, b, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), 0);
        s.poll_round(at(0));
        if timeout {
            s.poll_round(at(127_000));
            assert_eq!(s.take_error(id), Some(NetError::HostUnreach));
        }
        s.close(id, false, at(127_001));
        assert_eq!((s.socket_count(), s.port_count(), s.entry_count(), b.used()), (0, 0, 0, 0));
    }
}
#[test]
fn deadline_aborted_orphan_does_not_wait_forever_for_arp_or_tx_capacity() {
    // Lumen deviation: bounded orphan cleanup, including an unsent reset.
    for (blocked, reset) in [(false, false), (true, false), (false, true), (true, true)] {
        let (mut s, mut peer, control, _) = wire_stacks();
        let b = budget();
        let pb = budget();
        let l = listener(&mut peer, pb, None, false, false, 1234, 0);
        let c = connect(&mut s, b, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), 0);
        let mut now = 0;
        wire_steps(&mut s, &mut peer, &mut now, 30);
        peer.accept(l).unwrap();
        control.drop_all.store(true, Ordering::Relaxed);
        s.send(c, b"pending", None, Preps::None, at(now)).unwrap();
        s.close(c, reset, at(now));
        control.block.store(blocked, Ordering::Relaxed);
        s.poll_round(at(now + 60_001));
        s.poll_round(at(now + 60_002));
        assert_eq!((s.socket_count(), s.port_count(), s.entry_count(), b.used()), (0, 0, 0, 0));
    }
}
#[test]
fn half_close_then_idle_or_streaming_close_starts_a_fresh_orphan_deadline() {
    // Linux v6.12 net/ipv4/tcp.c, __tcp_close:3164-3180 starts the orphan
    // FIN-WAIT-2 timer at close; tcp_input.c tcp_rcv_state_process:6888-6915
    // only arms it on entry for an already orphaned socket.
    for streaming in [false, true] {
        let (mut s, _, _, c, peer, mut now) = pair(false);
        s.shutdown(c, Shut::Wr, at(now)).unwrap();
        steps(&mut s, &mut now, 20);
        for _ in 0..70 {
            now += 1000;
            if streaming {
                s.send(peer, b"stream", None, Preps::None, at(now)).unwrap();
            }
            steps(&mut s, &mut now, 8);
            if streaming {
                assert_eq!(s.recv(c, &mut [0; 8], false).unwrap().copied, 6);
            }
        }
        s.close(c, false, at(now));
        steps(&mut s, &mut now, 8);
        assert_eq!(s.recv(peer, &mut [0; 8], false).unwrap().copied, 0);
        assert_eq!(s.take_error(peer), None);
        now += 5000;
        s.close(peer, false, at(now));
        steps(&mut s, &mut now, 20);
    }
}
#[test]
fn slow_reader_receives_all_64k_after_close() {
    // Linux v6.12 net/ipv4/tcp_timer.c, tcp_orphan_retries:148-161:
    // retransmission failure, not time since close, ends FIN-WAIT-1.
    let mut s = stack();
    let b = budget();
    let l = listener(&mut s, b, Some(addr(false)), false, false, 1234, 0);
    let c = s.open(Family::V4, Proto::Tcp).unwrap();
    let prep = prepare_tcp(b, BufSizes { rx: 4096, tx: 65536 }).unwrap();
    assert_eq!(
        s.connect(c, IpEndpoint::new(addr(false), 1234), Some(ConnectPrep::Tcp(prep)), at(0)),
        Err(NetError::InProgress)
    );
    let mut now = 0;
    steps(&mut s, &mut now, 20);
    let (peer, _) = s.accept(l).unwrap();
    let payload: Vec<_> = (0..65536).map(|i| (i % 251) as u8).collect();
    assert_eq!(s.send(c, &payload, None, Preps::None, at(now)), Ok(payload.len()));
    s.close(c, false, at(now));
    let mut got = Vec::new();
    let mut buf = [0; 1024];
    let mut eof = false;
    for _ in 0..100 {
        now += 1000;
        steps(&mut s, &mut now, 8);
        match s.recv(peer, &mut buf, false) {
            Ok(r) if r.copied == 0 => {
                eof = true;
                break;
            }
            Ok(r) => got.extend_from_slice(&buf[..r.copied]),
            Err(NetError::WouldBlock) => (),
            Err(e) => panic!("slow reader failed at {} bytes: {e:?}", got.len()),
        }
    }
    assert_eq!(got, payload);
    assert!(eof);
    assert_eq!(s.take_error(peer), None);
}
fn keep(s: &mut Stack<Wire>, c: SockId, idle: u64, intvl: u64, count: u32, now: i64) {
    for opt in [
        NetOpt::KeepIdle(Duration::from_secs(idle)),
        NetOpt::KeepIntvl(Duration::from_secs(intvl)),
        NetOpt::KeepCnt(count),
        NetOpt::KeepAlive(true),
    ] {
        s.set_opt(c, opt, at(now)).unwrap();
    }
}
#[test]
fn late_keepalive_poll_probes_a_live_peer_instead_of_timing_out() {
    // Linux v6.12 net/ipv4/tcp_timer.c, tcp_keepalive_timer:806-820:
    // failure counts actual unanswered probes, not elapsed intervals.
    let (mut s, _, _, c, peer, mut now) = pair(false);
    keep(&mut s, c, 1, 1, 1, now);
    now += 10_000;
    assert!(s.poll_round(at(now)).more, "late poll must emit the first probe");
    steps(&mut s, &mut now, 10);
    assert_eq!(s.take_error(c), None);
    s.send(peer, b"alive", None, Preps::None, at(now)).unwrap();
    steps(&mut s, &mut now, 10);
    assert_eq!(s.recv(c, &mut [0; 16], false).unwrap().copied, 5);
}
#[test]
fn probe_keepalive_retuned_after_enable() {
    // Linux v6.12 net/ipv4/tcp.c, tcp_sock_set_keepidle_locked:3612-3631.
    // smoltcp cannot re-arm an existing timer: Lumen waits for real probes
    // on that timer, instead of timing out at a fictional earlier deadline.
    for (idle, intvl, count) in [(60, 10, 3), (1, 1, 1)] {
        let (mut s, _, _, c, peer, mut now) = pair(false);
        s.set_opt(c, NetOpt::KeepAlive(true), at(now)).unwrap();
        steps(&mut s, &mut now, 8);
        now += 100_000;
        keep(&mut s, c, idle, intvl, count, now);
        for _ in 0..800 {
            now += 10_000;
            steps(&mut s, &mut now, 8);
            assert_eq!(s.take_error(c), None);
            assert!(!s.readiness(c).hup);
        }
        s.send(peer, b"ok", None, Preps::None, at(now)).unwrap();
        steps(&mut s, &mut now, 8);
        assert_eq!(s.recv(c, &mut [0; 8], false).unwrap().copied, 2);
    }
}
#[test]
fn keepalive_timeout_preserves_unread_data_error_eof_and_wake() {
    // Linux v6.12 net/ipv4/tcp.c, tcp_recvmsg_locked:2626-2669;
    // tcp_timer.c, tcp_keepalive_timer:806-820 (count sent probes).
    let (mut s, mut peer, control, _) = wire_stacks();
    let b = budget();
    let l = listener(&mut peer, b, None, false, false, 1234, 0);
    let c = connect(&mut s, b, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), 0);
    let mut now = 0;
    wire_steps(&mut s, &mut peer, &mut now, 20);
    let (p, _) = peer.accept(l).unwrap();
    peer.send(p, b"unread", None, Preps::None, at(now)).unwrap();
    wire_steps(&mut s, &mut peer, &mut now, 20);
    control.drop_all.store(true, Ordering::Relaxed);
    keep(&mut s, c, 1, 1, 3, now);
    s.changed();
    for _ in 0..5 {
        s.poll_round(at(now));
        now += 1000;
    }
    assert!(s.changed().contains(&c));
    assert!(s.readiness(c).error);
    let mut data = [0; 8];
    assert_eq!(s.recv(c, &mut data, false).unwrap().copied, 6);
    assert_eq!(&data[..6], b"unread");
    assert_eq!(s.recv(c, &mut data, false), Err(NetError::TimedOut));
    assert_eq!(s.recv(c, &mut data, false).unwrap().copied, 0);
    s.poll_round(at(now + 10_000));
    assert_eq!(s.take_error(c), None);
    assert!(s.poll_at(at(now + 10_000)).is_none_or(|d| d > at(now + 10_000)));
}
#[test]
fn keepalive_after_reset_or_time_wait_has_no_second_error_or_poll_spin() {
    for reset in [false, true] {
        let (mut s, _, _, c, peer, mut now) = pair(false);
        s.send(peer, b"preserved", None, Preps::None, at(now)).unwrap();
        steps(&mut s, &mut now, 8);
        if reset {
            s.close(peer, true, at(now));
        } else {
            s.shutdown(c, Shut::Wr, at(now)).unwrap();
            steps(&mut s, &mut now, 8);
            s.close(peer, false, at(now));
        }
        steps(&mut s, &mut now, 8);
        assert_eq!(s.take_error(c), if reset { Some(NetError::ConnReset) } else { None });
        keep(&mut s, c, 1, 1, 1, now);
        now += 20_000;
        steps(&mut s, &mut now, 8);
        assert_eq!(s.take_error(c), None);
        let mut buf = [0; 16];
        assert_eq!(s.recv(c, &mut buf, false).unwrap().copied, 9);
        assert_eq!(&buf[..9], b"preserved");
        assert_eq!(s.recv(c, &mut buf, false).unwrap().copied, 0);
        steps(&mut s, &mut now, 2);
        assert!(s.poll_at(at(now)).is_none_or(|d| d > at(now)));
        if reset {
            assert_eq!(s.connect(c, IpEndpoint::new(addr(false), 1234), None, at(now)), Err(NetError::ConnAborted));
            steps(&mut s, &mut now, 2);
            assert!(s.poll_at(at(now)).is_none_or(|d| d > at(now)));
        }
    }
}
#[test]
fn received_fin_suppresses_later_reset_on_recv_but_preserves_so_error() {
    // Linux v6.12 net/ipv4/tcp.c, tcp_recvmsg_locked:2664-2669 checks
    // SOCK_DONE before sk_err; tcp_input.c, tcp_reset:4541-4543 sets EPIPE.
    let (mut s, _, _, c, peer, mut now) = pair(false);
    s.send(peer, b"tail", None, Preps::None, at(now)).unwrap();
    s.shutdown(peer, Shut::Wr, at(now)).unwrap();
    steps(&mut s, &mut now, 10);
    s.close(peer, true, at(now));
    steps(&mut s, &mut now, 10);
    assert_eq!(s.recv(c, &mut [0; 8], false).unwrap().copied, 4);
    assert_eq!(s.recv(c, &mut [0; 8], false).unwrap().copied, 0);
    assert_eq!(s.take_error(c), Some(NetError::Pipe));
}
#[test]
fn tcp_ticket_prefix_consumes_only_successfully_copied_bytes() {
    // Linux v6.12 net/ipv4/tcp.c, tcp_recvmsg_locked:2740-2782 advances
    // copied/seq only for bytes copied; UDP consumes the complete skb.
    let (mut s, _, _, c, peer, mut now) = pair(false);
    s.send(peer, b"0123456789", None, Preps::None, at(now)).unwrap();
    steps(&mut s, &mut now, 8);
    let ticket = s.recv(c, &mut [0; 10], true).unwrap().ticket;
    s.consume(c, ticket.prefix(4)).unwrap();
    let mut data = [0; 16];
    assert_eq!(s.recv(c, &mut data, false).unwrap().copied, 6);
    assert_eq!(&data[..6], b"456789");
    assert_eq!(s.consume(c, ticket), Err(NetError::StaleTicket));
    assert_eq!(NetError::StaleTicket.to_errno(), None);
    assert_eq!(NetError::Pipe.to_errno(), None);
}
#[test]
fn explicit_bind_preserves_v6only_and_connected_send_needs_no_preparation() {
    // Linux v6.12 net/ipv6/af_inet6.c, __inet6_bind:325 and 402-418
    // reads the socket's ipv6only flag, not a bind argument.
    let mut s = stack();
    let b = budget();
    let u = s.open(Family::V6, Proto::Udp).unwrap();
    s.set_opt(u, NetOpt::V6Only(true), at(0)).unwrap();
    s.bind(u, BindReq { port: 4000, ..BindReq::default() }, Preps::Udp(&mut [udp(b)].into_iter()), at(0)).unwrap();
    assert_eq!(s.get_opt(u, OptQuery::V6Only), Ok(NetOpt::V6Only(true)));
    assert_eq!(s.connect(u, IpEndpoint::new(addr(false), 4001), None, at(0)), Err(NetError::NetUnreach));
    s.connect(u, IpEndpoint::new(addr(true), 4001), None, at(0)).unwrap();
    assert_eq!(s.preps_needed(u, PrepFor::Send(None)), 0);
    assert_eq!(s.send(u, b"one", None, Preps::None, at(0)), Ok(3));
    let c = s.open(Family::V4, Proto::Tcp).unwrap();
    assert_eq!(
        s.connect(c, IpEndpoint::new(addr(false), 1234), Some(ConnectPrep::Udp(udp(b))), at(0)),
        Err(NetError::Inval)
    );
    assert_eq!(s.socket_count(), 1);
}
#[test]
fn v4_and_v6only_wildcard_listeners_coexist_in_both_orders() {
    // Linux v6.12 net/ipv4/inet_connection_sock.c, ipv6_rcv_saddr_equal:58-64
    // and ipv4_rcv_saddr_equal:78-88 allow this disjoint pair.
    for v6_first in [false, true] {
        let mut s = stack();
        let b = budget();
        let first = listener(&mut s, b, None, v6_first, v6_first, 1234, 4);
        let second = listener(&mut s, b, None, !v6_first, !v6_first, 1234, 4);
        let (v4, v6) = if v6_first { (second, first) } else { (first, second) };
        let clients: Vec<_> = [addr(false), addr(true), IpAddress::v4(127, 0, 0, 2)]
            .into_iter()
            .map(|dst| connect(&mut s, b, IpEndpoint::new(dst, 1234), 0))
            .collect();
        let mut now = 0;
        steps(&mut s, &mut now, 30);
        for c in clients {
            assert_eq!(s.take_error(c), None);
            assert!(s.readiness(c).writable);
        }
        assert!(s.accept(v4).is_ok());
        assert!(s.accept(v4).is_ok());
        assert_eq!(s.accept(v4), Err(NetError::WouldBlock));
        assert!(s.accept(v6).is_ok());
        assert_eq!(s.accept(v6), Err(NetError::WouldBlock));
    }
}
#[test]
fn accepted_connect_udp_write_shutdown_and_listener_shutdown() {
    // Linux v6.12 net/ipv4/af_inet.c, __inet_stream_connect:658-660
    // (SS_CONNECTED -> EISCONN), inet_shutdown:917-939.
    let (mut s, b, l, _, peer, now) = pair(false);
    assert_eq!(s.connect(peer, IpEndpoint::new(addr(false), 1234), None, at(now)), Err(NetError::IsConn));
    refill(&mut s, b);
    let count = s.socket_count();
    s.shutdown(l, Shut::Rd, at(now)).unwrap();
    assert_eq!(s.socket_count(), count - 1);
    let u = bind_udp(&mut s, b, None, 4000, false);
    assert_eq!(s.shutdown(u, Shut::Wr, at(now)), Err(NetError::NotConn));
    assert_eq!(s.send(u, b"x", Some(IpEndpoint::new(addr(false), 4001)), Preps::None, at(now)), Err(NetError::Pipe));
}
#[test]
fn reconnect_after_time_wait_clears_old_connection_deadlines() {
    // Linux v6.12 net/ipv4/af_inet.c, __inet_stream_connect:718-739
    // disconnects a failed CONNECTING socket before allowing another try.
    let (mut s, b, l, c, peer, mut now) = pair(false);
    s.send(peer, b"old", None, Preps::None, at(now)).unwrap();
    steps(&mut s, &mut now, 8);
    let old_ticket = s.recv(c, &mut [0; 3], true).unwrap().ticket;
    s.shutdown(c, Shut::Wr, at(now)).unwrap();
    steps(&mut s, &mut now, 8);
    s.close(peer, false, at(now));
    steps(&mut s, &mut now, 8);
    now += 11_000;
    steps(&mut s, &mut now, 8);
    assert_eq!(s.connect(c, IpEndpoint::new(addr(false), 1234), None, at(now)), Err(NetError::ConnAborted));
    refill(&mut s, b);
    assert_eq!(s.connect(c, IpEndpoint::new(addr(false), 1234), Some(prep(b)), at(now)), Err(NetError::InProgress));
    steps(&mut s, &mut now, 8);
    let (peer, _) = s.accept(l).unwrap();
    s.send(peer, b"new", None, Preps::None, at(now)).unwrap();
    steps(&mut s, &mut now, 8);
    assert_eq!(s.consume(c, old_ticket), Err(NetError::StaleTicket));
    let mut data = [0; 3];
    assert_eq!(s.recv(c, &mut data, false).unwrap().copied, 3);
    assert_eq!(&data, b"new");
    let port = s.local_endpoint(c).unwrap().port;
    s.close(c, false, at(now));
    steps(&mut s, &mut now, 8);
    s.close(peer, false, at(now));
    steps(&mut s, &mut now, 8);
    let deadline = s.time_wait_ports().find(|(p, _)| *p == port).unwrap().1;
    assert!(deadline > at(now) && deadline <= at(now + 10_000));
    s.poll_round(deadline - Duration::from_millis(1));
    assert!(s.time_wait_ports().any(|(p, _)| p == port));
    s.poll_round(deadline);
    assert!(!s.time_wait_ports().any(|(p, _)| p == port));
}

#[test]
fn datagram_ticket_prefix_still_discards_the_whole_message() {
    // Linux v6.12 net/ipv4/udp.c, udp_recvmsg:1869-1876 consumes the skb
    // even when the user copy fails; only a TCP consume can be partial.
    let mut s = stack();
    let b = budget();
    let u = bind_udp(&mut s, b, None, 5555, false);
    for data in [b"first".as_slice(), b"second"] {
        s.send(u, data, Some(IpEndpoint::new(addr(false), 5555)), Preps::None, at(0)).unwrap();
    }
    let mut now = 0;
    steps(&mut s, &mut now, 8);
    let (_, ticket) = s.datagram_len(u).unwrap();
    s.consume(u, ticket.prefix(0)).unwrap();
    let mut data = [0; 16];
    assert_eq!(s.recv(u, &mut data, false).unwrap().copied, 6);
    assert_eq!(&data[..6], b"second");
    assert_eq!(s.consume(u, ticket), Err(NetError::StaleTicket));
}

#[test]
fn reset_error_precedes_local_read_shutdown_without_peer_fin() {
    // Linux v6.12 net/ipv4/tcp.c, tcp_recvmsg_locked:2664-2673 checks
    // SOCK_DONE, sk_err, then RCV_SHUTDOWN. Local shutdown is not peer FIN.
    let (mut s, _, _, c, peer, mut now) = pair(false);
    s.close(peer, true, at(now));
    steps(&mut s, &mut now, 8);
    assert_eq!(s.shutdown(c, Shut::Rd, at(now)), Err(NetError::NotConn));
    assert_eq!(s.recv(c, &mut [0; 8], false), Err(NetError::ConnReset));
    assert_eq!(s.recv(c, &mut [0; 8], false).unwrap().copied, 0);
}

#[test]
fn keepalive_failed_transmissions_do_not_count_as_unanswered_probes() {
    // Linux v6.12 net/ipv4/tcp_timer.c, tcp_keepalive_timer:818-825 only
    // increments icsk_probes_out when tcp_write_wakeup succeeds.
    let (mut s, mut peer, control, _) = wire_stacks();
    let b = budget();
    let l = listener(&mut peer, b, None, false, false, 1234, 0);
    let c = connect(&mut s, b, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), 0);
    let mut now = 0;
    wire_steps(&mut s, &mut peer, &mut now, 30);
    peer.accept(l).unwrap();
    control.fail.store(true, Ordering::Relaxed);
    keep(&mut s, c, 1, 1, 1, now);
    for _ in 0..10 {
        now += 1000;
        s.poll_round(at(now));
        assert_eq!(s.take_error(c), None);
        assert!(!s.readiness(c).hup);
    }
    assert!(s.failed_transmits() >= 10);
    control.fail.store(false, Ordering::Relaxed);
    control.drop_all.store(true, Ordering::Relaxed);
    now += 1000;
    s.poll_round(at(now)); // the first probe actually leaves the device
    assert_eq!(s.take_error(c), None);
    s.changed();
    now += 1000;
    s.poll_round(at(now));
    assert!(s.changed().contains(&c));
    assert_eq!(s.take_error(c), Some(NetError::TimedOut));
}

#[test]
fn udp_read_shutdown_returns_eof_consistent_with_readiness() {
    // Linux v6.12 net/ipv4/udp.c, udp_recvmsg:1832-1834 and __skb_recv_udp:
    // 1775-1781; net/core/datagram.c, __skb_wait_for_more_packets:104-106.
    // Lumen exposes this blocking-path EOF through its unified recv API;
    // Linux MSG_DONTWAIT can instead return EAGAIN (udp.c:1725,1733,1775).
    for v6 in [false, true] {
        for how in [Shut::Rd, Shut::RdWr] {
            let mut s = stack();
            let b = budget();
            let receiver = bind_udp(&mut s, b, Some(addr(v6)), 4000, v6);
            let sender = bind_udp(&mut s, b, Some(addr(v6)), 4001, v6);
            s.connect(receiver, IpEndpoint::new(addr(v6), 4001), None, at(0)).unwrap();
            s.send(sender, b"queued", Some(IpEndpoint::new(addr(v6), 4000)), Preps::None, at(0)).unwrap();
            let mut now = 0;
            steps(&mut s, &mut now, 8);
            assert_eq!(s.datagram_len(receiver).unwrap().0, 6);
            s.shutdown(receiver, how, at(now)).unwrap();
            assert_eq!(s.datagram_len(receiver), Err(NetError::WouldBlock), "shutdown discards queued packets");
            for peek in [false, true] {
                let r = s.recv(receiver, &mut [0; 8], peek).unwrap();
                assert_eq!((r.copied, r.full_len, r.from), (0, 0, None));
                assert!(s.readiness(receiver).readable);
            }
            s.send(sender, b"later", Some(IpEndpoint::new(addr(v6), 4000)), Preps::None, at(now)).unwrap();
            steps(&mut s, &mut now, 8);
            assert_eq!(s.recv(receiver, &mut [0; 8], false).unwrap().copied, 0);
            assert_eq!(s.datagram_len(receiver), Err(NetError::WouldBlock));
        }
    }
    let mut s = stack();
    let u = s.open(Family::V4, Proto::Udp).unwrap();
    assert_eq!(s.shutdown(u, Shut::Rd, at(0)), Err(NetError::NotConn));
    assert_eq!(s.recv(u, &mut [0; 8], false).unwrap().copied, 0, "no transport slot is required for EOF");
}

#[test]
fn tcp_maxseg_reports_user_mss_before_connect_and_clamps_after() {
    // Linux v6.12 net/ipv4/tcp.c, do_tcp_setsockopt:3809-3818 stores
    // user_mss; do_tcp_getsockopt:4314-4318 selects it in CLOSE/LISTEN.
    // Deviation: smoltcp has no user-MSS setter; only readback is clamped.
    let (mut s, mut peer, _, _) = wire_stacks();
    let b = budget();
    let l = listener(&mut peer, b, None, false, false, 1234, 0);
    peer.set_opt(l, NetOpt::MaxSeg(5000), at(0)).unwrap();
    assert_eq!(peer.get_opt(l, OptQuery::MaxSeg), Ok(NetOpt::MaxSeg(5000)));
    let c = s.open(Family::V4, Proto::Tcp).unwrap();
    s.set_opt(c, NetOpt::MaxSeg(5000), at(0)).unwrap();
    assert_eq!(s.get_opt(c, OptQuery::MaxSeg), Ok(NetOpt::MaxSeg(5000)));
    assert_eq!(
        s.connect(c, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), Some(prep(b)), at(0)),
        Err(NetError::InProgress)
    );
    let mut now = 0;
    wire_steps(&mut s, &mut peer, &mut now, 20);
    let (child, _) = peer.accept(l).unwrap();
    assert_eq!(s.get_opt(c, OptQuery::MaxSeg), Ok(NetOpt::MaxSeg(1460)));
    assert_eq!(peer.get_opt(child, OptQuery::MaxSeg), Ok(NetOpt::MaxSeg(1460)));
    for (requested, expected) in [(1000, 1000), (88, 88), (32767, 1460), (0, 1460)] {
        s.set_opt(c, NetOpt::MaxSeg(requested), at(now)).unwrap();
        assert_eq!(s.get_opt(c, OptQuery::MaxSeg), Ok(NetOpt::MaxSeg(expected)));
    }
    for invalid in [1, 87, 32768] {
        assert_eq!(s.set_opt(c, NetOpt::MaxSeg(invalid), at(now)), Err(NetError::Inval));
        assert_eq!(s.get_opt(c, OptQuery::MaxSeg), Ok(NetOpt::MaxSeg(1460)));
    }
}

#[test]
fn udp_zero_length_datagrams_free_metadata_and_wake_writers() {
    // Linux v6.12 net/core/datagram.c, datagram_poll:919-923 uses available
    // send space; net/core/sock.c, sock_def_write_space:3454-3472 wakes writers.
    let mut s = stack();
    let b = budget();
    let sender = bind_udp(&mut s, b, Some(addr(false)), 4000, false);
    let receiver = bind_udp(&mut s, b, Some(addr(false)), 4001, false);
    for _ in 0..32 {
        assert_eq!(s.send(sender, &[], Some(IpEndpoint::new(addr(false), 4001)), Preps::None, at(0)), Ok(0));
    }
    assert!(!s.readiness(sender).writable);
    assert_eq!(
        s.send(sender, &[], Some(IpEndpoint::new(addr(false), 4001)), Preps::None, at(0)),
        Err(NetError::WouldBlock)
    );
    s.changed();
    s.poll_round(at(0));
    assert!(s.readiness(sender).writable);
    assert!(s.changed().contains(&sender), "payload count stays zero, but a metadata slot became free");
    let mut now = 1;
    steps(&mut s, &mut now, 40);
    for _ in 0..32 {
        let r = s.recv(receiver, &mut [], false).unwrap();
        assert_eq!(r.full_len, 0);
        assert!(r.from.is_some(), "zero-length datagram is distinct from shutdown EOF");
    }
    assert_eq!(s.recv(receiver, &mut [], false), Err(NetError::WouldBlock));
    s.changed();
    s.poll_round(at(now));
    assert!(!s.changed().contains(&sender), "idle writable state is not a new event");
}

#[test]
fn udp_wildcard_recipients_are_selected_by_family_in_both_bind_orders() {
    // Linux v6.12 net/ipv4/udp.c, compute_score:378-381 excludes v6only;
    // net/ipv6/udp.c, compute_score:126-129 excludes AF_INET before delivery.
    for v6_first in [false, true] {
        let mut s = stack();
        let b = budget();
        let mut receivers = Vec::new();
        for v6 in [v6_first, !v6_first] {
            let id = s.open(if v6 { Family::V6 } else { Family::V4 }, Proto::Udp).unwrap();
            if v6 {
                s.set_opt(id, NetOpt::V6Only(true), at(0)).unwrap();
            }
            s.bind(id, BindReq { port: 4000, ..BindReq::default() }, Preps::Udp(&mut [udp(b)].into_iter()), at(0))
                .unwrap();
            receivers.push((v6, id));
        }
        for (v6, _) in &receivers {
            let sender = bind_udp(&mut s, b, Some(addr(*v6)), 4001, *v6);
            s.send(sender, if *v6 { b"v6" } else { b"v4" }, Some(IpEndpoint::new(addr(*v6), 4000)), Preps::None, at(0))
                .unwrap();
        }
        let mut now = 0;
        steps(&mut s, &mut now, 12);
        for (v6, receiver) in receivers {
            let mut data = [0; 8];
            let r = s.recv(receiver, &mut data, false).unwrap();
            assert_eq!(r.copied, 2);
            assert_eq!(&data[..2], if v6 { b"v6" } else { b"v4" });
            assert_eq!(r.from.unwrap().addr, addr(v6));
            assert_eq!(s.recv(receiver, &mut data, false), Err(NetError::WouldBlock));
        }
    }
}

fn reuse_udp(s: &mut Stack<Wire>, b: &'static Budget, v6: bool, local: Option<IpAddress>) -> SockId {
    let id = s.open(if v6 { Family::V6 } else { Family::V4 }, Proto::Udp).unwrap();
    s.bind(
        id,
        BindReq { addr: local, port: 4000, reuse_addr: true, ..BindReq::default() },
        Preps::Udp(&mut [udp(b)].into_iter()),
        at(0),
    )
    .unwrap();
    id
}

#[test]
fn udp_specific_local_binding_wins_in_both_orders() {
    // Linux v6.12 net/ipv4/udp.c, __udp4_lib_lookup:497-527, and
    // net/ipv6/udp.c, __udp6_lib_lookup:235-265 try the specific local
    // address before the wildcard bucket, regardless of bind order.
    for v6 in [false, true] {
        for specific_first in [false, true] {
            let mut s = stack();
            let b = budget();
            let mut receivers = Vec::new();
            for specific in [specific_first, !specific_first] {
                receivers.push((specific, reuse_udp(&mut s, b, v6, specific.then_some(addr(v6)))));
            }
            let sender = bind_udp(&mut s, b, Some(addr(v6)), 4001, v6);
            s.send(sender, b"specific", Some(IpEndpoint::new(addr(v6), 4000)), Preps::None, at(0)).unwrap();
            steps(&mut s, &mut 0, 12);
            for (specific, receiver) in receivers {
                let mut data = [0; 16];
                if specific {
                    assert_eq!(s.recv(receiver, &mut data, false).unwrap().copied, 8);
                    assert_eq!(&data[..8], b"specific");
                }
                assert_eq!(s.recv(receiver, &mut data, false), Err(NetError::WouldBlock));
            }
        }
    }
}

#[test]
fn udp_connected_peer_wins_on_shared_binding() {
    // Linux v6.12 net/ipv4/udp.c, compute_score:389-401 and
    // udp4_lib_lookup2:438-449; net/ipv6/udp.c, compute_score:137-147
    // and udp6_lib_lookup2:181-192 prefer a matching peer address/port.
    for v6 in [false, true] {
        for connected_first in [false, true] {
            for specific in [false, true] {
                let mut s = stack();
                let b = budget();
                let mut receivers = Vec::new();
                for connected in [connected_first, !connected_first] {
                    let id = reuse_udp(&mut s, b, v6, specific.then_some(addr(v6)));
                    if connected {
                        s.connect(id, IpEndpoint::new(addr(v6), 4001), None, at(0)).unwrap();
                    }
                    receivers.push((connected, id));
                }
                let mut now = 0;
                for (port, payload) in [(4001, b"peer"), (4002, b"else")] {
                    let sender = bind_udp(&mut s, b, Some(addr(v6)), port, v6);
                    s.send(sender, payload, Some(IpEndpoint::new(addr(v6), 4000)), Preps::None, at(now)).unwrap();
                }
                steps(&mut s, &mut now, 12);
                for (connected, receiver) in receivers {
                    let mut data = [0; 16];
                    assert_eq!(s.recv(receiver, &mut data, false).unwrap().copied, 4);
                    assert_eq!(&data[..4], if connected { b"peer" } else { b"else" });
                    assert_eq!(s.recv(receiver, &mut data, false), Err(NetError::WouldBlock));
                }
            }
        }
    }
}

#[test]
fn udp_equal_specificity_uses_bind_order_not_slot_order() {
    // Linux v6.12 net/ipv4/udp.c, udp4_lib_lookup2:438-441 and
    // net/ipv6/udp.c, udp6_lib_lookup2:181-184 replace only on a higher
    // score. Lumen's deterministic tie policy is earliest bind.
    for v6 in [false, true] {
        let mut s = stack();
        let b = budget();
        let earlier_slot = s.open(if v6 { Family::V6 } else { Family::V4 }, Proto::Udp).unwrap();
        let earlier_bind = reuse_udp(&mut s, b, v6, Some(addr(v6)));
        s.bind(
            earlier_slot,
            BindReq { addr: Some(addr(v6)), port: 4000, reuse_addr: true, ..BindReq::default() },
            Preps::Udp(&mut [udp(b)].into_iter()),
            at(0),
        )
        .unwrap();
        let sender = bind_udp(&mut s, b, Some(addr(v6)), 4001, v6);
        s.send(sender, b"tie", Some(IpEndpoint::new(addr(v6), 4000)), Preps::None, at(0)).unwrap();
        steps(&mut s, &mut 0, 12);
        let mut data = [0; 3];
        assert_eq!(s.recv(earlier_bind, &mut data, false).unwrap().copied, 3);
        assert_eq!(&data, b"tie");
        assert_eq!(s.recv(earlier_slot, &mut data, false), Err(NetError::WouldBlock));
    }
}

#[test]
fn connected_udp_writability_ignores_unused_interface() {
    // Linux v6.12 net/core/datagram.c, datagram_poll:919-923 reports
    // this socket's send capacity. Lumen's two transport queues must not
    // let an unused interface conceal exhaustion on the connected route.
    let (mut s, mut peer, control, _) = wire_stacks();
    let b = budget();
    let receiver = bind_udp(&mut peer, budget(), None, 4000, false);
    let id = bind_udp(&mut s, b, None, 4001, false);
    assert_eq!(s.socket_count(), 2);
    // A queued sendto on lo will drain after connect fixes the route to eth0.
    s.send(id, b"unused", Some(IpEndpoint::new(addr(false), 4999)), Preps::None, at(0)).unwrap();
    s.connect(id, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 4000), None, at(0)).unwrap();
    assert_eq!(s.iface_of(id), Some(IfaceId::Eth0));
    control.block.store(true, Ordering::Relaxed);
    for _ in 0..32 {
        s.send(id, &[], None, Preps::None, at(0)).unwrap();
    }
    assert_eq!(s.send(id, &[], None, Preps::None, at(0)), Err(NetError::WouldBlock));
    assert!(!s.readiness(id).writable);
    s.changed();
    s.poll_round(at(0));
    assert!(!s.readiness(id).writable);
    assert!(!s.changed().contains(&id), "unused lo transmit space is not a writable event");
    control.block.store(false, Ordering::Relaxed);
    let mut now = 1;
    wire_steps(&mut s, &mut peer, &mut now, 48);
    assert!(s.readiness(id).writable);
    assert!(s.changed().contains(&id), "selected eth0 transmit space wakes the sender");
    assert_eq!(peer.recv(receiver, &mut [], false).unwrap().full_len, 0);
}

#[test]
fn raw_icmp_connect_or_bind_before_send_selects_request_identifier() {
    // Linux v6.12 net/ipv4/raw.c, raw_v4_match:119-129 demultiplexes
    // protocol/addresses, not an echo identifier claimed during connect.
    // raw_sendmsg:651-654 preserves the caller's ICMP payload. Lumen's
    // echo-only transport learns its identifier from the first send.
    for v6 in [false, true] {
        for bind_first in [false, true] {
            let mut s = stack();
            let b = budget();
            let id = s.open(if v6 { Family::V6 } else { Family::V4 }, Proto::IcmpEcho { raw: true }).unwrap();
            if bind_first {
                s.bind(
                    id,
                    BindReq { addr: Some(addr(v6)), ..BindReq::default() },
                    Preps::Icmp(&mut [prepare_icmp(b).unwrap()].into_iter()),
                    at(0),
                )
                .unwrap();
            }
            let prep = (!bind_first).then(|| ConnectPrep::Icmp(prepare_icmp(b).unwrap()));
            s.connect(id, IpEndpoint::new(addr(v6), 0), prep, at(0)).unwrap();
            assert_eq!(s.port_count(), 0);
            // A rejected send must not latch an identifier.
            assert_eq!(s.send(id, &[8; 4097], None, Preps::None, at(0)), Err(NetError::MsgSize));
            let mut request = echo(0x459);
            if v6 {
                request[0] = 128;
                smoltcp::wire::Icmpv6Packet::new_unchecked(&mut request[..])
                    .fill_checksum(&Ipv6Address::LOCALHOST, &Ipv6Address::LOCALHOST);
            }
            s.send(id, &request, None, Preps::None, at(0)).unwrap();
            assert_eq!(s.local_endpoint(id).unwrap().port, 0x459);
            assert_eq!(s.port_count(), 0);
            steps(&mut s, &mut 0, 12);
            let mut data = [0; 64];
            let mut replies = 0;
            while let Ok(r) = s.recv(id, &mut data, false) {
                let offset = if v6 { 0 } else { 20 };
                assert_eq!(r.full_len, offset + request.len());
                assert_eq!(u16::from_be_bytes([data[offset + 4], data[offset + 5]]), 0x459);
                replies += usize::from(data[offset] == if v6 { 129 } else { 0 });
            }
            assert_eq!(replies, 1);
        }
    }
}

#[test]
fn icmp_bound_loopback_alias_is_used_as_the_request_source() {
    // Linux v6.12 net/ipv4/ping.c, ping_v4_sendmsg:737,761-762 uses
    // inet_saddr via include/net/ip.h, ipcm_init_sk:101; raw_sendmsg in
    // net/ipv4/raw.c:540 uses the same bound source. Test wire-visible
    // round trips, including two same-identifier sockets and queued sends.
    for raw in [false, true] {
        let mut s = stack();
        let b = budget();
        let mut sockets = Vec::new();
        for last in [1, 2] {
            let local = IpAddress::v4(127, 0, 0, last);
            let id = s.open(Family::V4, Proto::IcmpEcho { raw }).unwrap();
            s.bind(
                id,
                BindReq { addr: Some(local), port: 0x459, ..BindReq::default() },
                Preps::Icmp(&mut [prepare_icmp(b).unwrap()].into_iter()),
                at(0),
            )
            .unwrap();
            for sequence in [last, last + 2] {
                let mut data = echo(0x459);
                data[7] = sequence;
                smoltcp::wire::Icmpv4Packet::new_unchecked(&mut data[..]).fill_checksum();
                s.send(id, &data, Some(IpEndpoint::new(IpAddress::v4(127, 0, 0, 9), 0)), Preps::None, at(0)).unwrap();
            }
            sockets.push((last, local, id));
        }
        steps(&mut s, &mut 0, 16);
        for (last, local, id) in sockets {
            for sequence in [last, last + 2] {
                let mut data = [0; 64];
                let r = s.recv(id, &mut data, false).unwrap();
                let offset = if raw { 20 } else { 0 };
                assert_eq!(r.from.unwrap().addr, IpAddress::v4(127, 0, 0, 9));
                assert_eq!(data[offset], 0);
                assert_eq!(data[offset + 7], sequence);
                if raw {
                    let ip = smoltcp::wire::Ipv4Packet::new_checked(&data[..r.full_len]).unwrap();
                    assert_eq!(IpAddress::Ipv4(ip.dst_addr()), local);
                }
            }
            assert_eq!(s.recv(id, &mut [0; 64], false), Err(NetError::WouldBlock));
        }
    }
}

#[test]
fn raw_icmp_late_interface_inherits_first_identifier() {
    // Linux v6.12 net/ipv4/raw.c, raw_v4_match:119-129 has no ping-port
    // reservation. Lumen's first-request identifier must follow every
    // transport, including one installed after the original send.
    for ident in [0, 0x459] {
        let mut s = stack();
        let b = budget();
        let id = s.open(Family::V4, Proto::IcmpEcho { raw: true }).unwrap();
        let request = echo(ident);
        s.send(
            id,
            &request,
            Some(IpEndpoint::new(addr(false), 0)),
            Preps::Icmp(&mut [prepare_icmp(b).unwrap()].into_iter()),
            at(0),
        )
        .unwrap();
        let mut now = 0;
        steps(&mut s, &mut now, 8);
        while s.recv(id, &mut [0; 64], false).is_ok() {}
        let (wire, other) = Wire::pair();
        let mut peer = stack();
        s.attach_eth(
            wire,
            EthConfig::Static { addr: Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 1), 24), router: None },
            at(now),
            459,
        )
        .unwrap();
        peer.attach_eth(
            other,
            EthConfig::Static { addr: Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 2), 24), router: None },
            at(now),
            460,
        )
        .unwrap();
        s.connect(
            id,
            IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 0),
            Some(ConnectPrep::Icmp(prepare_icmp(b).unwrap())),
            at(now),
        )
        .unwrap();
        s.send(id, &request, None, Preps::None, at(now)).unwrap();
        wire_steps(&mut s, &mut peer, &mut now, 16);
        let mut data = [0; 64];
        assert_eq!(s.recv(id, &mut data, false).unwrap().full_len, 32);
        assert_eq!(data[20], 0);
        assert_eq!(u16::from_be_bytes([data[24], data[25]]), ident);
        assert_eq!(s.port_count(), 0);
    }
}

#[test]
fn bound_icmp_unresolved_neighbor_keeps_poll_deadline() {
    // Source selection must preserve smoltcp's neighbor retry metadata:
    // a missing ARP response must not turn poll_at into a busy loop.
    let (mut s, _unpolled_peer, _, _) = wire_stacks();
    let id = s.open(Family::V4, Proto::IcmpEcho { raw: true }).unwrap();
    s.connect(
        id,
        IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 0),
        Some(ConnectPrep::Icmp(prepare_icmp(budget()).unwrap())),
        at(0),
    )
    .unwrap();
    s.send(id, &echo(0x459), None, Preps::None, at(0)).unwrap();
    for now in [0, 1] {
        s.poll_round(at(now));
        assert!(s.poll_at(at(now)).is_some_and(|deadline| deadline > at(now)));
    }
}
