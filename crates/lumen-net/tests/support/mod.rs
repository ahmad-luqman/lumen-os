#![allow(dead_code)]
use lumen_net::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub fn budget() -> &'static Budget {
    Box::leak(Box::new(Budget::new(32 * 1024 * 1024)))
}
pub fn sizes() -> BufSizes {
    BufSizes { rx: 4096, tx: 4096 }
}
pub fn prep(b: &'static Budget) -> ConnectPrep {
    ConnectPrep::Tcp(prepare_tcp(b, sizes()).unwrap())
}
pub fn udp(b: &'static Budget) -> UdpPrep {
    prepare_udp(b, sizes()).unwrap()
}
pub fn addr(v6: bool) -> IpAddress {
    if v6 { IpAddress::Ipv6(Ipv6Address::LOCALHOST) } else { IpAddress::v4(127, 0, 0, 1) }
}
pub fn stack() -> Stack<Wire> {
    Stack::try_new(Loopback::try_new(65535, 16).unwrap(), Instant::ZERO, 459).unwrap()
}
pub fn steps(s: &mut Stack<Wire>, now: &mut i64, n: usize) {
    for _ in 0..n {
        s.poll_round(Instant::from_millis(*now));
        *now += 1;
    }
}
pub fn listener(
    s: &mut Stack<Wire>,
    b: &'static Budget,
    addr: Option<IpAddress>,
    v6: bool,
    only: bool,
    port: u16,
    backlog: usize,
) -> SockId {
    let id = s.open(if v6 { Family::V6 } else { Family::V4 }, Proto::Tcp).unwrap();
    if v6 {
        s.set_opt(id, NetOpt::V6Only(only), Instant::ZERO).unwrap();
    }
    s.bind(id, BindReq { addr, port, ..BindReq::default() }, Preps::None, Instant::ZERO).unwrap();
    let n = s.preps_needed(id, PrepFor::Listen(backlog));
    let pool: Vec<_> = (0..n).map(|_| prepare_tcp(b, sizes()).unwrap()).collect();
    s.listen(id, backlog, sizes(), &mut pool.into_iter(), Instant::ZERO).unwrap();
    id
}
pub fn connect(s: &mut Stack<Wire>, b: &'static Budget, remote: IpEndpoint, now: i64) -> SockId {
    let id = s.open(Family::of(remote.addr), Proto::Tcp).unwrap();
    assert_eq!(s.connect(id, remote, Some(prep(b)), Instant::from_millis(now)), Err(NetError::InProgress));
    id
}
pub fn pair(v6: bool) -> (Stack<Wire>, &'static Budget, SockId, SockId, SockId, i64) {
    let mut s = stack();
    let b = budget();
    let l = listener(&mut s, b, Some(addr(v6)), v6, false, 1234, 0);
    let c = connect(&mut s, b, IpEndpoint::new(addr(v6), 1234), 0);
    let mut now = 0;
    steps(&mut s, &mut now, 12);
    assert!(s.readiness(c).writable);
    assert_eq!(s.take_error(c), None);
    let (a, _) = s.accept(l).unwrap();
    (s, b, l, c, a, now)
}
pub fn bind_udp(s: &mut Stack<Wire>, b: &'static Budget, address: Option<IpAddress>, port: u16, v6: bool) -> SockId {
    let id = s.open(if v6 { Family::V6 } else { Family::V4 }, Proto::Udp).unwrap();
    let preps: Vec<_> = (0..2).map(|_| udp(b)).collect();
    s.bind(
        id,
        BindReq { addr: address, port, ..BindReq::default() },
        Preps::Udp(&mut preps.into_iter()),
        Instant::ZERO,
    )
    .unwrap();
    id
}
pub fn refill(s: &mut Stack<Wire>, b: &'static Budget) {
    let needs = s.shortfalls().to_vec();
    for (id, iface, n) in needs {
        for _ in 0..n {
            s.refill(id, iface, prepare_tcp(b, sizes()).unwrap(), Instant::ZERO).unwrap();
        }
    }
}
struct Frames {
    slots: Vec<[u8; ETH_FRAME_MAX]>,
    lengths: Vec<usize>,
    head: usize,
    len: usize,
}
impl Frames {
    fn new() -> Self {
        Self { slots: vec![[0; ETH_FRAME_MAX]; 64], lengths: vec![0; 64], head: 0, len: 0 }
    }
    fn push(&mut self, data: &[u8]) {
        let i = (self.head + self.len) % 64;
        self.slots[i][..data.len()].copy_from_slice(data);
        self.lengths[i] = data.len();
        self.len += 1;
    }
    fn pop(&mut self, out: &mut [u8; ETH_FRAME_MAX]) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let n = self.lengths[self.head];
        out[..n].copy_from_slice(&self.slots[self.head][..n]);
        self.head = (self.head + 1) % 64;
        self.len -= 1;
        Some(n)
    }
}
#[derive(Default)]
pub struct Control {
    pub block: AtomicBool,
    pub fail: AtomicBool,
    pub drop_ip: AtomicBool,
    pub drop_all: AtomicBool,
}
pub struct Wire {
    rx: Arc<Mutex<Frames>>,
    tx: Arc<Mutex<Frames>>,
    pub control: Arc<Control>,
    mac: [u8; 6],
}
impl Wire {
    pub fn pair() -> (Self, Self) {
        let a = Arc::new(Mutex::new(Frames::new()));
        let b = Arc::new(Mutex::new(Frames::new()));
        // Darwin's std mutex allocates its native mutex lazily. Prepare the
        // host fixture's locks before counting the stack's allocations.
        drop(a.lock().unwrap());
        drop(b.lock().unwrap());
        (
            Self { rx: a.clone(), tx: b.clone(), control: Arc::default(), mac: [2, 0, 0, 0, 0, 1] },
            Self { rx: b, tx: a, control: Arc::default(), mac: [2, 0, 0, 0, 0, 2] },
        )
    }
}
impl NetDevice for Wire {
    fn info(&self) -> NetInfo {
        NetInfo { mac: self.mac, mtu: 1500, link_up: true }
    }
    fn tx_ready(&self) -> bool {
        !self.control.block.load(Ordering::Relaxed) && self.tx.lock().unwrap().len < 64
    }
    fn receive(&mut self, buf: &mut [u8; ETH_FRAME_MAX]) -> Option<usize> {
        self.rx.lock().unwrap().pop(buf)
    }
    fn transmit<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, TxError> {
        if self.control.fail.load(Ordering::Relaxed) {
            return Err(TxError::DeviceFailed);
        }
        if !self.tx_ready() {
            return Err(TxError::Full);
        }
        let mut frame = [0; ETH_FRAME_MAX];
        let result = f(&mut frame[..len]);
        if !self.control.drop_all.load(Ordering::Relaxed)
            && !(self.control.drop_ip.load(Ordering::Relaxed) && frame[12..14] == [8, 0])
        {
            self.tx.lock().unwrap().push(&frame[..len]);
        }
        Ok(result)
    }
    fn reclaim(&mut self) -> usize {
        0
    }
}
pub fn wire_stacks() -> (Stack<Wire>, Stack<Wire>, Arc<Control>, Arc<Control>) {
    let (a, b) = Wire::pair();
    let ca = a.control.clone();
    let cb = b.control.clone();
    let mut sa = stack();
    let mut sb = stack();
    sa.attach_eth(
        a,
        EthConfig::Static { addr: Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 1), 24), router: None },
        Instant::ZERO,
        459,
    )
    .unwrap();
    sb.attach_eth(
        b,
        EthConfig::Static { addr: Ipv4Cidr::new(Ipv4Address::new(10, 0, 0, 2), 24), router: None },
        Instant::ZERO,
        459,
    )
    .unwrap();
    (sa, sb, ca, cb)
}
pub fn wire_steps(a: &mut Stack<Wire>, b: &mut Stack<Wire>, now: &mut i64, n: usize) {
    for _ in 0..n {
        a.poll_round(Instant::from_millis(*now));
        b.poll_round(Instant::from_millis(*now));
        *now += 1;
    }
}
