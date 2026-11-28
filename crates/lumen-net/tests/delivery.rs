mod support;
use lumen_net::*;
use support::*;

fn at(ms: i64) -> Instant {
    Instant::from_millis(ms)
}
fn reuse_tcp(s: &mut Stack<Wire>, v6: bool, local: Option<IpAddress>, port: u16) -> SockId {
    let id = s.open(if v6 { Family::V6 } else { Family::V4 }, Proto::Tcp).unwrap();
    s.bind(id, BindReq { addr: local, port, reuse_addr: true, ..BindReq::default() }, Preps::None, at(0)).unwrap();
    id
}

#[test]
fn tcp_duplicate_explicit_four_tuple_is_rejected_before_installation() {
    // Linux v6.12 net/ipv4/inet_hashtables.c, __inet_hash_connect:1014-1019
    // checks explicit ports; __inet_check_established:565-573,600-602
    // refuses a duplicate with EADDRNOTAVAIL. The IPv6 counterpart is
    // net/ipv6/inet6_hashtables.c, __inet6_check_established:290-298,325-327.
    for v6 in [false, true] {
        for wildcard in [false, true] {
            let mut s = stack();
            let b = budget();
            let l = listener(&mut s, b, Some(addr(v6)), v6, false, 1234, 2);
            let local = (!wildcard).then_some(addr(v6));
            let c = reuse_tcp(&mut s, v6, local, 4000);
            let duplicate = reuse_tcp(&mut s, v6, local, 4000);
            let remote = IpEndpoint::new(addr(v6), 1234);
            assert_eq!(s.connect(c, remote, Some(prep(b)), at(0)), Err(NetError::InProgress));
            let counts = (s.socket_count(), s.port_count(), b.used());
            // SYN-SENT must reserve the tuple, before any poll or packet.
            assert_eq!(s.connect(duplicate, remote, Some(prep(b)), at(0)), Err(NetError::AddrNotAvail));
            assert_eq!((s.socket_count(), s.port_count(), b.used()), counts);
            assert_eq!(s.remote_endpoint(duplicate), None);
            let mut now = 0;
            steps(&mut s, &mut now, 12);
            let peer = s.accept(l).unwrap().0;
            assert_eq!(s.connect(duplicate, remote, Some(prep(b)), at(now)), Err(NetError::AddrNotAvail));
            s.send(c, b"original", None, Preps::None, at(now)).unwrap();
            steps(&mut s, &mut now, 8);
            let mut bytes = [0; 8];
            assert_eq!(s.recv(peer, &mut bytes, false).unwrap().copied, 8);
            assert_eq!(&bytes, b"original");
            assert_eq!(s.accept(l), Err(NetError::WouldBlock));
            // Same local endpoint with a different peer tuple remains legal.
            assert_eq!(
                s.connect(duplicate, IpEndpoint::new(addr(v6), 1235), Some(prep(b)), at(now)),
                Err(NetError::InProgress)
            );
        }
    }
}

#[test]
fn tcp_ephemeral_connect_skips_an_occupied_tuple() {
    // Linux v6.12 net/ipv4/inet_hashtables.c, __inet_hash_connect:1053-1065 searches
    // candidate ports through check_established. Lumen's allocator is
    // stricter: it skips every occupied TCP port, not just this tuple.
    for v6 in [false, true] {
        let mut s = stack();
        let b = budget();
        let l = listener(&mut s, b, Some(addr(v6)), v6, false, 1234, 2);
        let first = reuse_tcp(&mut s, v6, Some(addr(v6)), *EPHEMERAL.start());
        let remote = IpEndpoint::new(addr(v6), 1234);
        assert_eq!(s.connect(first, remote, Some(prep(b)), at(0)), Err(NetError::InProgress));
        let second = connect(&mut s, b, remote, 0);
        assert_ne!(s.local_endpoint(first), s.local_endpoint(second));
        let mut now = 0;
        steps(&mut s, &mut now, 12);
        let a = s.accept(l).unwrap();
        let b = s.accept(l).unwrap();
        assert_ne!(a.1, b.1);
    }
}

#[test]
fn tcp_specific_listener_precedes_an_earlier_wildcard() {
    // Linux v6.12 net/ipv4/inet_hashtables.c, __inet_lookup_listener:
    // 447-462 searches exact address before INADDR_ANY; IPv6 uses
    // net/ipv6/inet6_hashtables.c, inet6_lookup_listener:223-238.
    for v6 in [false, true] {
        let mut s = stack();
        let b = budget();
        let mut listeners = Vec::new();
        for local in [None, Some(addr(v6))] {
            let id = s.open(if v6 { Family::V6 } else { Family::V4 }, Proto::Tcp).unwrap();
            s.bind(id, BindReq { addr: local, port: 1234, reuse_port: true, ..BindReq::default() }, Preps::None, at(0))
                .unwrap();
            s.listen(id, 0, &mut [prepare_tcp(b, sizes()).unwrap()].into_iter(), at(0)).unwrap();
            listeners.push(id);
        }
        let c = connect(&mut s, b, IpEndpoint::new(addr(v6), 1234), 0);
        let mut now = 0;
        steps(&mut s, &mut now, 12);
        assert_eq!(s.accept(listeners[0]), Err(NetError::WouldBlock));
        let peer = s.accept(listeners[1]).unwrap().0;
        s.send(c, b"owned", None, Preps::None, at(now)).unwrap();
        steps(&mut s, &mut now, 8);
        assert_eq!(s.recv(peer, &mut [0; 8], false).unwrap().copied, 5);
    }
}

#[test]
fn connected_udp_wildcard_acquires_a_specific_local_address() {
    // Linux v6.12 net/ipv4/datagram.c, __ip4_datagram_connect:64-70
    // fills inet_rcv_saddr on connect. A matching peer is not sufficient
    // when the incoming destination is another address on that interface.
    let mut s = stack();
    let b = budget();
    let mut ids = Vec::new();
    for _ in 0..2 {
        let id = s.open(Family::V4, Proto::Udp).unwrap();
        s.bind(
            id,
            BindReq { port: 4000, reuse_addr: true, ..BindReq::default() },
            Preps::Udp(&mut [udp(b)].into_iter()),
            at(0),
        )
        .unwrap();
        ids.push(id);
    }
    let peer = IpEndpoint::new(addr(false), 4001);
    s.connect(ids[1], peer, None, at(0)).unwrap();
    let sender = bind_udp(&mut s, b, Some(peer.addr), peer.port, false);
    for (dst, data) in [(addr(false), b"here"), (IpAddress::v4(127, 0, 0, 2), b"else")] {
        s.send(sender, data, Some(IpEndpoint::new(dst, 4000)), Preps::None, at(0)).unwrap();
    }
    steps(&mut s, &mut 0, 12);
    for (id, expected) in [(ids[1], b"here"), (ids[0], b"else")] {
        let mut data = [0; 4];
        assert_eq!(s.recv(id, &mut data, false).unwrap().copied, 4);
        assert_eq!(&data, expected);
        assert_eq!(s.recv(id, &mut data, false), Err(NetError::WouldBlock));
    }
}

#[test]
fn tcp_time_wait_tuple_is_reserved_until_expiry_without_timestamps() {
    // Linux v6.12 net/ipv4/inet_hashtables.c, __inet_check_established:
    // 565-573 checks TIME-WAIT too; net/ipv4/tcp_ipv4.c, tcp_twsk_unique:
    // 164-167,198 requires a timestamp for early reuse. Lumen does not
    // enable smoltcp timestamps, so reuse waits for its 10-second expiry.
    for v6 in [false, true] {
        for orphan in [false, true] {
            let mut s = stack();
            let b = budget();
            let l = listener(&mut s, b, Some(addr(v6)), v6, false, 1234, 0);
            let c = reuse_tcp(&mut s, v6, Some(addr(v6)), 4000);
            let retry = reuse_tcp(&mut s, v6, Some(addr(v6)), 4000);
            let remote = IpEndpoint::new(addr(v6), 1234);
            assert_eq!(s.connect(c, remote, Some(prep(b)), at(0)), Err(NetError::InProgress));
            let mut now = 0;
            steps(&mut s, &mut now, 12);
            let peer = s.accept(l).unwrap().0;
            s.shutdown(c, Shut::Wr, at(now)).unwrap();
            steps(&mut s, &mut now, 8);
            s.shutdown(peer, Shut::Wr, at(now)).unwrap();
            steps(&mut s, &mut now, 8);
            if orphan {
                s.close(c, false, at(now));
                steps(&mut s, &mut now, 4);
                assert_eq!(s.entry_count(), 3); // listener, peer, retry
            }
            let count = s.socket_count();
            let used = b.used();
            assert_eq!(s.connect(retry, remote, Some(prep(b)), at(now)), Err(NetError::AddrNotAvail));
            assert_eq!(s.socket_count(), count);
            assert_eq!(b.used(), used);
            // A different remote tuple on the same port is still available.
            let other = reuse_tcp(&mut s, v6, Some(addr(v6)), 4000);
            assert_eq!(
                s.connect(other, IpEndpoint::new(addr(v6), 1235), Some(prep(b)), at(now)),
                Err(NetError::InProgress)
            );
            now += 11_000;
            s.poll_round(at(now));
            assert_eq!(s.connect(retry, remote, Some(prep(b)), at(now)), Err(NetError::InProgress));
        }
    }
}
