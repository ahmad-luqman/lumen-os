mod support;
use lumen_net::*;
use support::*;

#[test]
fn listener_refills_keep_first_listen_buffer_capacities() {
    let mut s = stack();
    let b = budget();
    let sizes = BufSizes { rx: 4096, tx: 8192 };
    let l = s.open(Family::V4, Proto::Tcp).unwrap();
    s.bind(l, BindReq { addr: Some(addr(false)), port: 1234, ..BindReq::default() }, Preps::None, Instant::ZERO)
        .unwrap();
    let mut initial = [prepare_tcp(b, sizes).unwrap()].into_iter();
    s.listen(l, 0, sizes, &mut initial, Instant::ZERO).unwrap();
    assert_eq!(s.listener_sizes(l), Some(sizes));
    let client = connect(&mut s, b, IpEndpoint::new(addr(false), 1234), 0);
    let mut now = 0;
    steps(&mut s, &mut now, 16);
    let (child, _) = s.accept(l).unwrap();
    assert_eq!(s.shortfalls(), &[(l, IfaceId::Lo, 1)]);
    let used = b.used();
    assert_eq!(
        s.refill(l, IfaceId::Lo, prepare_tcp(b, BufSizes { rx: 1, tx: 1 }).unwrap(), Instant::ZERO),
        Err(NetError::Inval)
    );
    assert_eq!(b.used(), used);
    assert!(s.refill(l, IfaceId::Lo, prepare_tcp(b, s.listener_sizes(l).unwrap()).unwrap(), Instant::ZERO).unwrap());
    assert_eq!(b.used(), used + sizes.rx + sizes.tx);
    // Wrong preparations still fail even with an explicit sizes argument.
    // Linux v6.12 net/ipv4/af_inet.c:199-203 updates an existing listener's
    // backlog. C4 must prepare from listener_sizes, not changed SO_*BUF.
    let mut wrong = [prepare_tcp(b, BufSizes { rx: 8, tx: 8 }).unwrap()].into_iter();
    assert_eq!(s.listen(l, 1, sizes, &mut wrong, Instant::ZERO), Err(NetError::Inval));
    assert_eq!(s.listener_sizes(l), Some(sizes));
    let changed_options = BufSizes { rx: 8, tx: 8 };
    let mut additional = [prepare_tcp(b, s.listener_sizes(l).unwrap()).unwrap()].into_iter();
    s.listen(l, 1, changed_options, &mut additional, Instant::ZERO).unwrap();
    assert_eq!(s.listener_sizes(l), Some(sizes));
    assert_eq!(s.preps_needed(l, PrepFor::Listen(1)), 0);
    assert_eq!(b.used(), used + 2 * (sizes.rx + sizes.tx));
    for id in [client, child, l] {
        s.close(id, true, Instant::from_millis(now));
    }
    steps(&mut s, &mut now, 16);
    assert_eq!(b.used(), 0);
}
