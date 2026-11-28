mod support;
use lumen_net::*;
use smoltcp::phy::{Device, TxToken};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use support::*;

struct Counting;
thread_local! { static COUNT: Cell<Option<usize>> = const { Cell::new(None) }; }
fn record() {
    let _ = COUNT.try_with(|n| {
        if let Some(v) = n.get() {
            n.set(Some(v + 1));
        }
    });
}
// SAFETY: allocations and deallocations retain System's layouts and pointers;
// the thread-local counter neither allocates nor alters the allocation.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: the caller supplied this valid Layout.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record();
        // SAFETY: the caller supplied this valid Layout.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record();
        // SAFETY: GlobalAlloc's caller guarantees the pointer/layout and new size.
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller returns a live System allocation with its original layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;
fn counted(f: impl FnOnce()) -> usize {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            COUNT.with(|n| n.set(None));
        }
    }
    COUNT.with(|n| n.set(Some(0)));
    let _reset = Reset;
    f();
    COUNT.with(|n| n.get().unwrap())
}

#[test]
fn all_stack_operations_after_preparation_allocate_nothing() {
    let (mut s, b, l, c, a, mut now) = pair(false);
    let udp_preps: Vec<_> = (0..2).map(|_| udp(b)).collect();
    let replacement = prepare_tcp(b, sizes()).unwrap();
    assert_eq!(
        counted(|| {
            let u = s.open(Family::V4, Proto::Udp).unwrap();
            s.bind(
                u,
                BindReq { port: 2000, ..BindReq::default() },
                Preps::Udp(&mut udp_preps.into_iter()),
                Instant::from_millis(now),
            )
            .unwrap();
            s.connect(u, IpEndpoint::new(addr(false), 2000), None, Instant::from_millis(now)).unwrap();
            s.send(u, b"datagram", None, Preps::None, Instant::from_millis(now)).unwrap();
            s.send(c, b"stream", None, Preps::None, Instant::from_millis(now)).unwrap();
            steps(&mut s, &mut now, 8);
            let (_, t) = s.datagram_len(u).unwrap();
            s.recv_ticket(u, t, &mut [0; 8], true).unwrap();
            s.consume(u, t).unwrap();
            let r = s.recv(a, &mut [0; 8], true).unwrap();
            s.consume(a, r.ticket).unwrap();
            s.set_opt(c, NetOpt::NoDelay(true), Instant::from_millis(now)).unwrap();
            s.set_opt(c, NetOpt::KeepAlive(true), Instant::from_millis(now)).unwrap();
            s.get_opt(c, OptQuery::KeepAlive).unwrap();
            s.readiness(c);
            s.local_endpoint(c);
            s.remote_endpoint(c);
            s.iface_of(c);
            s.poll_at(Instant::from_millis(now));
            s.take_error(c);
            s.changed();
            s.lease_event();
            s.preps_needed(c, PrepFor::Bind(&BindReq::default()));
            s.refill(l, IfaceId::Lo, replacement, Instant::from_millis(now)).unwrap();
            s.disconnect(u).unwrap();
            s.close(u, false, Instant::from_millis(now));
            s.shutdown(c, Shut::RdWr, Instant::from_millis(now)).unwrap();
            s.close(c, true, Instant::from_millis(now));
            s.close(a, true, Instant::from_millis(now));
            s.close(l, false, Instant::from_millis(now));
            steps(&mut s, &mut now, 8);
        }),
        0
    );
    assert_eq!(b.used(), 0);
    assert!(
        counted(|| {
            let v = vec![42; 128];
            std::hint::black_box(v);
        }) > 0,
        "allocator positive control"
    );
}
#[test]
fn connect_listen_accept_and_ethernet_poll_allocate_nothing() {
    let (wire_a, wire_b) = Wire::pair();
    let (mut a, mut b) = (stack(), stack());
    let budget = budget();
    let pool = vec![prepare_tcp(budget, sizes()).unwrap()];
    let client = prep(budget);
    assert_eq!(
        counted(|| {
            a.attach_eth(
                wire_a,
                EthConfig::Static { addr: Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 1), 24), router: None },
                Instant::ZERO,
                459,
            )
            .unwrap();
            b.attach_eth(
                wire_b,
                EthConfig::Static { addr: Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 2), 24), router: None },
                Instant::ZERO,
                459,
            )
            .unwrap();
            let l = b.open(Family::V4, Proto::Tcp).unwrap();
            b.bind(
                l,
                BindReq { addr: Some(IpAddress::v4(10, 0, 0, 2)), port: 1234, ..BindReq::default() },
                Preps::None,
                Instant::ZERO,
            )
            .unwrap();
            b.listen(l, 0, &mut pool.into_iter(), Instant::ZERO).unwrap();
            let c = a.open(Family::V4, Proto::Tcp).unwrap();
            assert_eq!(
                a.connect(c, IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 1234), Some(client), Instant::ZERO),
                Err(NetError::InProgress)
            );
            let mut now = 0;
            wire_steps(&mut a, &mut b, &mut now, 16);
            let (server, _) = b.accept(l).unwrap();
            a.send(c, b"wire", None, Preps::None, Instant::from_millis(now)).unwrap();
            wire_steps(&mut a, &mut b, &mut now, 8);
            assert_eq!(b.recv(server, &mut [0; 8], false).unwrap().copied, 4);
            a.close(c, true, Instant::from_millis(now));
            b.close(server, true, Instant::from_millis(now));
            b.close(l, false, Instant::from_millis(now));
            wire_steps(&mut a, &mut b, &mut now, 8);
        }),
        0
    );
    assert_eq!(budget.used(), 0);
    let (wire, _) = Wire::pair();
    let mut dhcp = stack();
    assert_eq!(
        counted(|| {
            dhcp.attach_eth(wire, EthConfig::Dhcp, Instant::ZERO, 459).unwrap();
            dhcp.poll_round(Instant::ZERO);
        }),
        0
    );
}
#[test]
fn full_loopback_ring_can_receive_and_reply_without_allocation() {
    let mut s = stack();
    let b = budget();
    let u = bind_udp(&mut s, b, Some(addr(false)), 1234, false);
    let senders: Vec<_> = (0..16).map(|n| bind_udp(&mut s, b, Some(addr(false)), 2000 + n, false)).collect();
    assert_eq!(
        counted(|| {
            for &sender in &senders {
                s.send(sender, b"ring", Some(IpEndpoint::new(addr(false), 1234)), Preps::None, Instant::ZERO).unwrap();
            }
            assert!(s.poll_round(Instant::ZERO).more);
            s.poll_round(Instant::ZERO);
            for _ in 0..16 {
                assert_eq!(s.recv(u, &mut [0; 4], false).unwrap().copied, 4);
            }
            assert_eq!(s.recv(u, &mut [0; 4], false), Err(NetError::WouldBlock));
        }),
        0
    );
    let mut original = smoltcp::phy::Loopback::new(smoltcp::phy::Medium::Ip);
    assert!(counted(|| original.transmit(Instant::ZERO).unwrap().consume(1, |b| b[0] = 1)) > 0);
}
#[test]
fn charges_refund_on_drop_and_failed_preparation() {
    let b = Box::leak(Box::new(Budget::new(128)));
    let charge = b.try_charge(100).unwrap();
    assert_eq!(b.used(), 100);
    assert!(matches!(b.try_charge(93), Err(NetError::NoBufs)));
    assert_eq!(b.used(), 100);
    drop(charge);
    assert_eq!(b.used(), 0);
    let p = prepare_tcp(b, BufSizes { rx: 64, tx: 64 }).unwrap();
    assert_eq!(b.used(), 128);
    drop(p);
    assert_eq!(b.used(), 0);
    let big = Box::leak(Box::new(Budget::new(usize::MAX)));
    assert!(matches!(prepare_tcp(big, BufSizes { rx: 1, tx: usize::MAX - 2 }), Err(NetError::NoMem)));
    assert_eq!(big.used(), 0);
    assert_eq!(Budget::for_usable_memory(128 * 1024 * 1024).limit(), 16 * 1024 * 1024);
    assert_eq!(Budget::for_usable_memory(1024 * 1024 * 1024).limit(), NET_MEM_CAP);
}
#[test]
fn entry_and_socket_capacity_return_enobufs_without_growing_storage() {
    let mut s = stack();
    let b = budget();
    let preps: Vec<_> = (0..NET_SOCKETS + 1).map(|_| prepare_udp(b, BufSizes { rx: 1, tx: 1 }).unwrap()).collect();
    assert_eq!(
        counted(|| {
            let mut preps = preps.into_iter();
            for port in 1..=NET_SOCKETS {
                let id = s.open(Family::V4, Proto::Udp).unwrap();
                s.bind(
                    id,
                    BindReq { addr: Some(addr(false)), port: port as u16, ..BindReq::default() },
                    Preps::Udp(&mut preps),
                    Instant::ZERO,
                )
                .unwrap();
            }
            let id = s.open(Family::V4, Proto::Udp).unwrap();
            assert_eq!(
                s.bind(
                    id,
                    BindReq { addr: Some(addr(false)), port: 2000, ..BindReq::default() },
                    Preps::Udp(&mut preps),
                    Instant::ZERO
                ),
                Err(NetError::NoBufs)
            );
            assert_eq!(s.socket_count(), NET_SOCKETS);
            for _ in NET_SOCKETS + 1..NET_ENTRIES {
                s.open(Family::V4, Proto::Tcp).unwrap();
            }
            assert_eq!(s.open(Family::V4, Proto::Tcp), Err(NetError::NoBufs));
        }),
        0
    );
    drop(s);
    assert_eq!(b.used(), 0);
}

#[test]
fn retired_tcp_queues_need_no_backup_allocation_and_refund_exactly_once() {
    let (mut s, b, l, c, peer, mut now) = pair(false);
    s.send(peer, b"retained", None, Preps::None, Instant::from_millis(now)).unwrap();
    steps(&mut s, &mut now, 8);
    s.shutdown(c, Shut::Wr, Instant::from_millis(now)).unwrap();
    steps(&mut s, &mut now, 8);
    s.close(peer, false, Instant::from_millis(now));
    steps(&mut s, &mut now, 8);
    s.close(l, false, Instant::from_millis(now));
    assert_eq!(b.used(), sizes().rx + sizes().tx);
    assert_eq!(s.socket_count(), 1);
    assert_eq!(
        counted(|| {
            now += 11_000;
            s.poll_round(Instant::from_millis(now));
            assert_eq!(s.socket_count(), 1, "retired sockets keep their slot reservation");
            let mut bytes = [0; 8];
            let ticket = s.recv(c, &mut bytes, true).unwrap().ticket;
            assert_eq!(&bytes, b"retained");
            s.consume(c, ticket.prefix(3)).unwrap();
            assert_eq!(s.recv(c, &mut bytes, false).unwrap().copied, 5);
            assert_eq!(&bytes[..5], b"ained");
            s.close(c, false, Instant::from_millis(now));
            assert_eq!((b.used(), s.socket_count(), s.entry_count()), (0, 0, 0));
        }),
        0
    );
}

#[test]
fn bound_icmp_egress_and_identifier_selection_allocate_nothing() {
    // The per-socket ICMP egress path must retain the stack's no-allocation
    // guarantee, including Ethernet neighbor discovery before the echo.
    for (raw, alias) in [(false, false), (true, false), (false, true), (true, true)] {
        let (mut s, mut peer, _, _) = wire_stacks();
        let b = budget();
        let prep = prepare_icmp(b).unwrap();
        assert_eq!(
            counted(|| {
                let id = s.open(Family::V4, Proto::IcmpEcho { raw }).unwrap();
                if alias {
                    s.bind(
                        id,
                        BindReq { addr: Some(IpAddress::v4(127, 0, 0, 2)), ..BindReq::default() },
                        Preps::Icmp(&mut [prep].into_iter()),
                        Instant::ZERO,
                    )
                    .unwrap();
                    s.connect(id, IpEndpoint::new(IpAddress::v4(127, 0, 0, 9), 0), None, Instant::ZERO).unwrap();
                } else {
                    s.connect(
                        id,
                        IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 0),
                        Some(ConnectPrep::Icmp(prep)),
                        Instant::ZERO,
                    )
                    .unwrap();
                }
                let mut request = [8, 0, 0, 0, 4, 89, 0, 1];
                smoltcp::wire::Icmpv4Packet::new_unchecked(&mut request[..]).fill_checksum();
                s.send(id, &request, None, Preps::None, Instant::ZERO).unwrap();
                let mut now = 0;
                wire_steps(&mut s, &mut peer, &mut now, 16);
                let mut data = [0; 32];
                let received = s.recv(id, &mut data, false).unwrap();
                let offset = if raw { 20 } else { 0 };
                assert_eq!(received.full_len, offset + 8);
                assert_eq!(data[offset], 0);
                s.close(id, false, Instant::from_millis(now));
            }),
            0
        );
        assert_eq!(b.used(), 0);
    }
}

#[test]
fn udp_broadcast_fanout_preserves_charges_wakes_and_unicast_without_allocation() {
    // Linux v6.12 net/ipv4/udp.c, __udp4_lib_mcast_deliver:2277-2315
    // queues a copy to each eligible socket; __udp4_lib_rcv:2443-2446
    // sends both broadcast and multicast through that path.
    let (mut sender, mut receiver, _, _) = wire_stacks();
    let b = budget();
    let source = bind_udp(&mut sender, budget(), None, 4001, false);
    sender.set_opt(source, NetOpt::Broadcast(true), Instant::ZERO).unwrap();
    let mut ids = Vec::new();
    for _ in 0..3 {
        let id = receiver.open(Family::V4, Proto::Udp).unwrap();
        receiver
            .bind(
                id,
                BindReq { port: 4000, reuse_addr: true, ..BindReq::default() },
                Preps::Udp(&mut [udp(b), udp(b)].into_iter()),
                Instant::ZERO,
            )
            .unwrap();
        ids.push(id);
    }
    let connected = receiver.open(Family::V4, Proto::Udp).unwrap();
    receiver
        .bind(
            connected,
            BindReq { port: 4000, reuse_addr: true, ..BindReq::default() },
            Preps::Udp(&mut [udp(b), udp(b)].into_iter()),
            Instant::ZERO,
        )
        .unwrap();
    receiver.connect(connected, IpEndpoint::new(IpAddress::v4(10, 0, 0, 1), 4001), None, Instant::ZERO).unwrap();
    let used = b.used();
    let sockets = receiver.socket_count();
    let mut now = 0;
    assert_eq!(
        counted(|| {
            for destination in [IpAddress::Ipv4(Ipv4Address::BROADCAST), IpAddress::v4(10, 0, 0, 255)] {
                receiver.changed();
                sender
                    .send(
                        source,
                        b"all",
                        Some(IpEndpoint::new(destination, 4000)),
                        Preps::None,
                        Instant::from_millis(now),
                    )
                    .unwrap();
                wire_steps(&mut sender, &mut receiver, &mut now, 8);
                let changed = receiver.changed();
                for &id in &ids {
                    assert!(changed.contains(&id));
                }
                assert!(!changed.contains(&connected), "connected local unicast address excludes broadcast");
                assert_eq!(receiver.recv(connected, &mut [0; 8], false), Err(NetError::WouldBlock));
                for &id in &ids {
                    let mut data = [0; 8];
                    assert_eq!(receiver.recv(id, &mut data, false).unwrap().copied, 3);
                    assert_eq!(&data[..3], b"all");
                    assert_eq!(receiver.recv(id, &mut data, false), Err(NetError::WouldBlock));
                }
                assert_eq!((receiver.socket_count(), b.used()), (sockets, used));
            }
            receiver.disconnect(connected).unwrap();
            receiver.changed();
            sender
                .send(
                    source,
                    b"one",
                    Some(IpEndpoint::new(IpAddress::v4(10, 0, 0, 2), 4000)),
                    Preps::None,
                    Instant::from_millis(now),
                )
                .unwrap();
            wire_steps(&mut sender, &mut receiver, &mut now, 16);
            let mut data = [0; 8];
            assert_eq!(receiver.recv(ids[0], &mut data, false).unwrap().copied, 3);
            assert_eq!(&data[..3], b"one");
            for &id in &ids[1..] {
                assert_eq!(receiver.recv(id, &mut data, false), Err(NetError::WouldBlock));
            }
            assert_eq!(receiver.recv(connected, &mut data, false), Err(NetError::WouldBlock));
            for &id in &ids {
                receiver.close(id, false, Instant::from_millis(now));
            }
            receiver.close(connected, false, Instant::from_millis(now));
            assert_eq!(b.used(), 0);
        }),
        0
    );
}
