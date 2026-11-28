//! Serialized network stack and its sole polling thread (M9 B3, #474).

use super::inet::InetSocket;
use super::virtio_net::{self, VirtioNet};
use crate::sched::{self, WaitQueue};
use crate::sync::{SleepMutex, SleepMutexGuard};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};
#[cfg(feature = "ktest")]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use lumen_net::{
    Budget, BufSizes, EthConfig, IfaceId, Instant, Loopback, NET_ENTRIES, NET_SOCKETS, NetError, SockId, Stack,
    prepare_tcp,
};

static STACK: AtomicPtr<NetStack> = AtomicPtr::new(core::ptr::null_mut());

/// Both fixed SocketSets are reserved even without a NIC; packet buffers
/// and Lumen's bookkeeping are additional, separately reserved storage.
pub const SET_STORAGE_BYTES: usize = 2 * NET_SOCKETS * size_of::<smoltcp::iface::SocketStorage<'static>>();

pub struct NetStack {
    inner: SleepMutex<NetInner>,
    kicked: AtomicBool,
    wake: WaitQueue,
    budget: &'static Budget,
    refill_failures: AtomicUsize,
    #[cfg(feature = "ktest")]
    owner: AtomicU64,
}

struct NetInner {
    stack: Stack<VirtioNet>,
    socks: Box<[Weak<InetSocket>]>,
    // Generation-tagged listener buffer sizes are copied at registration;
    // refills never need to upgrade/drop a last Arc under the stack lock.
    sizes: Box<[Option<(SockId, BufSizes)>]>,
    warned: Box<[Option<SockId>]>,
}

struct Guard<'a> {
    inner: SleepMutexGuard<'a, NetInner>,
    #[cfg(feature = "ktest")]
    owner: &'a AtomicU64,
}
impl Deref for Guard<'_> {
    type Target = NetInner;
    fn deref(&self) -> &NetInner {
        &self.inner
    }
}
impl DerefMut for Guard<'_> {
    fn deref_mut(&mut self) -> &mut NetInner {
        &mut self.inner
    }
}
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        #[cfg(feature = "ktest")]
        self.owner.store(0, Ordering::Release);
    }
}

/// Monotonic clock converted to smoltcp's microsecond units.
pub fn now() -> Instant {
    Instant::from_micros((crate::time::now_ns() / 1000) as i64)
}

impl NetStack {
    fn new(budget: &'static Budget, seed: u64) -> Result<Self, NetError> {
        let stack = Stack::try_new(Loopback::try_new(65535, 16)?, now(), seed)?;
        Ok(Self {
            inner: SleepMutex::new(NetInner {
                stack,
                socks: (0..NET_ENTRIES).map(|_| Weak::new()).collect(),
                sizes: alloc::vec![None; NET_ENTRIES].into_boxed_slice(),
                warned: alloc::vec![None; NET_ENTRIES].into_boxed_slice(),
            }),
            kicked: AtomicBool::new(true),
            wake: WaitQueue::new(),
            budget,
            refill_failures: AtomicUsize::new(0),
            #[cfg(feature = "ktest")]
            owner: AtomicU64::new(0),
        })
    }
    pub fn get() -> &'static Self {
        Self::try_get().expect("network stack not initialized")
    }
    pub(super) fn try_get() -> Option<&'static Self> {
        let ptr = STACK.load(Ordering::Acquire);
        // SAFETY: init publishes a fully initialized, leaked Box once. It
        // is never removed; mutable state is behind inner or atomics.
        unsafe { ptr.as_ref() }
    }
    fn lock(&self) -> Guard<'_> {
        let inner = self.inner.lock();
        #[cfg(feature = "ktest")]
        self.owner.store(sched::running_id_here().map_or(0, |(id, _)| id), Ordering::Release);
        Guard {
            inner,
            #[cfg(feature = "ktest")]
            owner: &self.owner,
        }
    }
    /// Runs bounded, allocation-free work with exclusive stack access.
    /// Callers must not copy user memory or poll interfaces. After mutations,
    /// call kick; socket operations use InetSocket::with to also notify inline.
    pub fn with<R>(&self, f: impl FnOnce(&mut Stack<VirtioNet>) -> R) -> R {
        f(&mut self.lock().stack)
    }
    /// Wakes the polling thread; safe in the device interrupt handler.
    pub fn kick(&self) {
        self.kicked.store(true, Ordering::Release);
        self.wake.wake_one();
    }
    /// Shared budget for preparations made before taking the stack lock.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // C3/C4/C6 preparations
    pub fn budget(&self) -> &'static Budget {
        self.budget
    }
    /// Registers the exact generation and its inherited listener buffer sizes.
    pub fn register(&self, id: SockId, sock: &Arc<InetSocket>) {
        let sizes = sock.buffer_sizes();
        let mut inner = self.lock();
        inner.socks[id.index()] = Arc::downgrade(sock);
        if inner.sizes[id.index()].is_none_or(|(registered, _)| registered != id) {
            inner.warned[id.index()] = None;
        }
        inner.sizes[id.index()] = Some((id, sizes));
    }
    /// Removes a released socket without disturbing a recycled generation.
    pub fn unregister(&self, id: SockId) {
        let mut inner = self.lock();
        if inner.sizes[id.index()].is_some_and(|(registered, _)| registered == id) {
            inner.socks[id.index()] = Weak::new();
            inner.sizes[id.index()] = None;
            inner.warned[id.index()] = None;
        }
    }
    #[cfg(feature = "ktest")]
    pub fn held_by_me(&self) -> bool {
        let me = sched::running_id_here().map_or(0, |(id, _)| id);
        me != 0 && self.owner.load(Ordering::Acquire) == me
    }
    fn refill_failed(&self, id: SockId, error: NetError) {
        self.refill_failures.fetch_add(1, Ordering::Relaxed);
        let warn = {
            let mut inner = self.lock();
            let warn = inner.warned[id.index()] != Some(id);
            inner.warned[id.index()] = Some(id);
            warn
        };
        if warn {
            log::warn!(target: "net", "listener {id:?}: pool refill failed: {error:?} (once per listener)");
        }
    }
    fn run(&self) -> ! {
        // Each changed ID appears once, and there are at most two shortfalls
        // per ID. These vectors never grow while inner is held.
        let mut notifications = Vec::with_capacity(NET_ENTRIES);
        let mut shortfalls = Vec::with_capacity(2 * NET_ENTRIES);
        let mut seen = virtio_net::BELL.rings();
        let mut deadline = 0;
        loop {
            self.wake.wait_until_or_deadline(deadline, || {
                self.kicked.load(Ordering::Acquire) || virtio_net::BELL.rings() != seen
            });
            // Clear BEFORE the round: concurrent inline work/IRQs remain
            // visible at the next wait. B2 also rings after 16 bad RX entries.
            self.kicked.store(false, Ordering::Release);
            seen = virtio_net::BELL.rings();
            let (more, tx_full, next) = {
                let mut inner = self.lock();
                if let Some(device) = inner.stack.device_mut() {
                    device.selftest_step();
                }
                let time = now();
                let round = inner.stack.poll_round(time);
                let next = inner.stack.poll_at(time);
                let tx_full = inner.stack.has_eth() && !inner.stack.eth_tx_ready();
                let NetInner { stack, socks, sizes, .. } = &mut *inner;
                for id in stack.changed() {
                    if let Some(sock) = socks[id.index()].upgrade() {
                        // A recycled index must never notify a different ID.
                        let mask = sock.id() == *id;
                        notifications.push((sock, mask));
                    }
                }
                for &(id, iface, count) in stack.shortfalls() {
                    if let Some((registered, sizes)) = sizes[id.index()]
                        && registered == id
                    {
                        shortfalls.push((id, iface, count, sizes));
                    }
                }
                (round.more, tx_full, next)
            };
            for (sock, current) in notifications.drain(..) {
                if current {
                    sock.notify();
                }
            }
            let mut installed = false;
            for (id, iface, count, sizes) in shortfalls.drain(..) {
                for _ in 0..count {
                    let result = prepare_tcp(self.budget, sizes)
                        .and_then(|prep| self.with(|s| s.refill(id, iface, prep, now())));
                    match result {
                        Ok(true) => installed = true,
                        Ok(false) => break,
                        Err(error) => {
                            self.refill_failed(id, error);
                            break;
                        }
                    }
                }
            }
            // Immediate receive work wins over Ethernet backpressure. An
            // expired egress deadline on a full TX queue must not busy-loop.
            deadline = if more || installed {
                0
            } else if tx_full {
                u64::MAX
            } else {
                next.map_or(u64::MAX, |t| (t.total_micros().max(0) as u64).saturating_mul(1000))
            };
        }
    }
}

/// IRQs before publication only ring BELL; the first round observes them.
pub fn kick() {
    if let Some(stack) = NetStack::try_get() {
        stack.kick();
    }
}

pub fn init() {
    // ISNs must come from independently drawn pool output. Without a seeded
    // pool, leave the device with B2's raw diagnostics and reject inet opens.
    let (Ok(lo_seed), Ok(eth_seed)) = (crate::rng::u64(), crate::rng::u64()) else {
        log::warn!(target: "net", "stack disabled: no seeded RNG");
        return;
    };
    let budget = Box::leak(Box::new(Budget::for_usable_memory(crate::mm::frame::counts().owned * 4096)));
    let net = NetStack::new(budget, lo_seed).expect("network fixed storage");
    if let Some(device) = virtio_net::take_device() {
        // B4 chooses DHCP/static/off. Until then attach an unconfigured NIC;
        // the ARP diagnostic remains independent of address configuration.
        let mut inner = net.lock();
        inner
            .stack
            .attach_eth(
                VirtioNet::new(device),
                EthConfig::Static {
                    addr: lumen_net::Ipv4Cidr::new(lumen_net::Ipv4Address::UNSPECIFIED, 0),
                    router: None,
                },
                now(),
                eth_seed,
            )
            .expect("attach Ethernet");
        inner.stack.configure_eth(None, None).expect("unconfigured Ethernet");
    }
    let net = Box::leak(Box::new(net));
    assert!(STACK.compare_exchange(core::ptr::null_mut(), net, Ordering::Release, Ordering::Relaxed).is_ok());
    let _ = sched::spawn("net", move || net.run()).expect("spawn net thread");
    log::info!(target: "net", "lo 127.0.0.1/8 ::1/128; socket sets {SET_STORAGE_BYTES} bytes; memory limit {}", budget.limit());
}

/// Snapshot diagnostics under inner; formatting and output may allocate and
/// take console locks, so they happen after the snapshot's guard is gone.
pub fn command(ctx: &mut crate::shell::Ctx<'_>, args: &[String]) {
    let net = NetStack::get();
    if matches!(args, [arg] if arg == "selftest") {
        return virtio_net::stack_selftest(ctx);
    }
    if !args.is_empty() {
        return ctx.fail("usage: net [selftest]");
    }
    let mut ports = Vec::with_capacity(NET_ENTRIES);
    let (device, sockets, entries, addresses) = {
        let mut inner = net.lock();
        let entries = inner.sizes.iter().filter(|entry| entry.is_some()).count();
        let stack = &mut inner.stack;
        ports.extend(stack.time_wait_ports());
        let addresses = [IfaceId::Lo, IfaceId::Eth0]
            .map(|iface| core::array::from_fn::<_, 2, _>(|i| stack.interface_addresses(iface).get(i).copied()));
        (stack.device_mut().map(|d| d.snapshot()), stack.socket_count(), entries, addresses)
    };
    if let Some(snapshot) = device {
        virtio_net::print_snapshot(ctx, snapshot);
    } else {
        let _ = writeln!(ctx.out, "net: no device");
    }
    for (name, addresses) in ["lo", "eth0"].into_iter().zip(addresses) {
        let _ = write!(ctx.out, "net: {name}");
        if addresses.iter().all(Option::is_none) {
            let _ = write!(ctx.out, " unconfigured");
        }
        for address in addresses.into_iter().flatten() {
            let _ = write!(ctx.out, " {address}");
        }
        let _ = writeln!(ctx.out);
    }
    let _ = writeln!(
        ctx.out,
        "net: open sockets {entries}, transport slots {sockets}, refill failures {}, socket memory {}/{}",
        net.refill_failures.load(Ordering::Relaxed),
        net.budget.used(),
        net.budget.limit()
    );
    for (port, deadline) in ports {
        let _ = writeln!(ctx.out, "net: TIME-WAIT port {port} until {} us", deadline.total_micros());
    }
}

#[cfg(feature = "ktest")]
pub(super) struct DeviceTestGuard(Guard<'static>);
#[cfg(feature = "ktest")]
impl DeviceTestGuard {
    pub fn as_mut(&mut self) -> Option<&mut virtio_net::VirtioNetDevice> {
        self.0.stack.device_mut().map(|device| &mut device.device)
    }
}
#[cfg(feature = "ktest")]
pub(super) fn device_test_guard() -> DeviceTestGuard {
    DeviceTestGuard(NetStack::get().lock())
}

pub(super) fn initialized() -> bool {
    NetStack::try_get().is_some()
}

#[cfg(feature = "ktest")]
mod test_support {
    use super::super::inet::{Proto, TcpState};
    use super::*;
    use lumen_net::{BindReq, ConnectPrep, IpAddress, IpEndpoint, Preps};
    use lumen_sockabi::Family;
    use lumen_vfs::{AccessMode, PollMask, PollWaker, Pollable};

    pub struct Waker {
        pub sequence: AtomicU64,
        pub worker_wakes: AtomicUsize,
        pub caller_wakes: AtomicUsize,
        pub caller: u64,
        pub wait: WaitQueue,
    }
    impl Waker {
        pub fn new() -> Arc<Self> {
            Arc::new(Self {
                sequence: AtomicU64::new(0),
                worker_wakes: AtomicUsize::new(0),
                caller_wakes: AtomicUsize::new(0),
                caller: sched::running_id_here().unwrap().0,
                wait: WaitQueue::new(),
            })
        }
    }
    impl PollWaker for Waker {
        fn wake(&self, _ready: PollMask) {
            assert!(!NetStack::get().held_by_me(), "readiness notified while holding NetStack::inner");
            if sched::running_id_here().unwrap().0 != self.caller {
                self.worker_wakes.fetch_add(1, Ordering::Relaxed);
            } else {
                self.caller_wakes.fetch_add(1, Ordering::Release);
            }
            self.sequence.fetch_add(1, Ordering::Release);
            self.wait.wake_all();
        }
    }
    pub fn wait(sock: &InetSocket, mut ready: impl FnMut() -> bool) {
        let waiter = Waker::new();
        let waker: Arc<dyn PollWaker> = waiter.clone();
        let (_, _guard) = sock.poll(AccessMode::ReadWrite, Some(&waker));
        loop {
            let seen = waiter.sequence.load(Ordering::Acquire);
            if ready() {
                return;
            }
            waiter.wait.wait_until(|| waiter.sequence.load(Ordering::Acquire) != seen);
        }
    }
    pub fn socket(v6: bool) -> Result<Arc<InetSocket>, &'static str> {
        InetSocket::new(if v6 { Family::Inet6 } else { Family::Inet }, Proto::Tcp(TcpState)).map_err(|_| "open TCP")
    }
    pub fn address(v6: bool) -> IpAddress {
        if v6 { IpAddress::Ipv6(lumen_net::Ipv6Address::LOCALHOST) } else { IpAddress::v4(127, 0, 0, 1) }
    }
    pub fn listener(v6: bool) -> Result<Arc<InetSocket>, &'static str> {
        let sock = socket(v6)?;
        sock.with(|s, id| s.bind(id, BindReq { addr: Some(address(v6)), ..BindReq::default() }, Preps::None, now()))
            .map_err(|_| "bind listener")?;
        let mut pool = (0..4)
            .map(|_| prepare_tcp(NetStack::get().budget(), sock.buffer_sizes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "prepare pool")?
            .into_iter();
        sock.with(|s, id| s.listen(id, 3, &mut pool, now())).map_err(|_| "listen")?;
        Ok(sock)
    }
    pub fn connect(listener: &InetSocket, v6: bool) -> Result<Arc<InetSocket>, &'static str> {
        let sock = socket(v6)?;
        let endpoint = NetStack::get().with(|s| s.local_endpoint(listener.id())).ok_or("listener endpoint")?;
        let prep = prepare_tcp(NetStack::get().budget(), sock.buffer_sizes()).map_err(|_| "prepare client")?;
        crate::kassert_eq!(
            sock.with(|s, id| s.connect(id, endpoint, Some(ConnectPrep::Tcp(prep)), now())),
            Err(NetError::InProgress)
        );
        Ok(sock)
    }
    pub fn accept(listener: &InetSocket, v6: bool) -> Result<Arc<InetSocket>, &'static str> {
        wait(listener, || NetStack::get().with(|s| s.readiness(listener.id()).readable));
        let (id, _) = listener.with(|s, id| s.accept(id)).map_err(|_| "accept")?;
        Ok(InetSocket::from_id(
            id,
            if v6 { Family::Inet6 } else { Family::Inet },
            Proto::Tcp(TcpState),
            listener.buffer_sizes(),
        ))
    }
    pub fn echo(from: &InetSocket, to: &InetSocket, tag: u8) -> crate::ktest::TestResult {
        let mut sent = 0;
        let mut received = 0;
        let data: [u8; 4096] = core::array::from_fn(|i| (i as u8).wrapping_add(tag));
        let mut out = [0; 4096];
        while received < 65536 {
            if sent < 65536 {
                wait(from, || NetStack::get().with(|s| s.readiness(from.id()).writable));
                let n = from
                    .with(|s, id| s.send(id, &data[..(65536 - sent).min(data.len())], None, Preps::None, now()))
                    .map_err(|_| "send bytes")?;
                crate::kassert!(n != 0);
                sent += n;
            }
            wait(to, || NetStack::get().with(|s| s.readiness(to.id()).readable));
            let n = to.with(|s, id| s.recv(id, &mut out, false)).map_err(|_| "receive bytes")?.copied;
            crate::kassert!(n != 0);
            for (i, byte) in out[..n].iter().enumerate() {
                crate::kassert_eq!(*byte, ((received + i) as u8).wrapping_add(tag));
            }
            received += n;
        }
        crate::kassert_eq!(sent, 65536);
        crate::kassert_eq!(received, 65536);
        Ok(())
    }
    pub fn endpoint() -> IpEndpoint {
        IpEndpoint::new(address(false), 9)
    }
}

crate::ktest!(
    fn loopback_tcp_echo_through_the_net_thread() {
        use test_support::*;
        for v6 in [false, true] {
            let listener = listener(v6)?;
            let client = connect(&listener, v6)?;
            let accepted = accept(&listener, v6)?;
            echo(&client, &accepted, 17)?;
            echo(&accepted, &client, 193)?;
        }
        Ok(())
    }
);

crate::ktest!(
    fn readiness_notifies_after_the_stack_lock() {
        use crate::{kassert, kassert_eq};
        use lumen_vfs::{AccessMode, PollWaker, Pollable};
        use test_support::*;
        let listener = listener(false)?;
        let waiter = Waker::new();
        let waker: Arc<dyn PollWaker> = waiter.clone();
        let (_, _guard) = listener.poll(AccessMode::ReadWrite, Some(&waker));
        // Positive control: held_by_me observes the actual acquiring thread.
        NetStack::get().with(|_| {
            assert!(NetStack::get().held_by_me());
        });
        kassert!(!NetStack::get().held_by_me());
        let client = connect(&listener, false)?;
        let accepted = accept(&listener, false)?;
        waiter.wait.wait_until(|| waiter.worker_wakes.load(Ordering::Acquire) != 0);
        kassert!(waiter.sequence.load(Ordering::Acquire) != 0);
        // Inline shutdown must notify in this caller after dropping inner too.
        let inline = Waker::new();
        let waker: Arc<dyn PollWaker> = inline.clone();
        let (_, _guard) = client.poll(AccessMode::ReadWrite, Some(&waker));
        kassert_eq!(client.with(|s, id| s.shutdown(id, lumen_net::Shut::Wr, now())), Ok(()));
        kassert_eq!(inline.caller_wakes.load(Ordering::Acquire), 1);
        wait(&accepted, || NetStack::get().with(|s| s.readiness(accepted.id()).rdhup));
        Ok(())
    }
);

crate::ktest!(
    fn listen_pool_refills_between_polls() {
        use crate::{kassert, kassert_eq};
        use test_support::*;
        let listener = listener(false)?;
        let mut accepted_count = 0;
        for _ in 0..3 {
            let clients = (0..4).map(|_| connect(&listener, false)).collect::<Result<Vec<_>, _>>()?;
            let accepted = (0..4).map(|_| accept(&listener, false)).collect::<Result<Vec<_>, _>>()?;
            accepted_count += accepted.len();
            for (client, child) in clients.iter().zip(&accepted) {
                wait(client, || NetStack::get().with(|s| s.readiness(client.id()).writable));
                kassert_eq!(NetStack::get().with(|s| s.take_error(client.id())), None);
                kassert_eq!(
                    NetStack::get().with(|s| s.remote_endpoint(child.id())),
                    NetStack::get().with(|s| s.local_endpoint(client.id()))
                );
            }
            kassert!(!NetStack::get().with(|s| s.readiness(listener.id()).readable));
        }
        kassert_eq!(accepted_count, 12);
        Ok(())
    }
);

crate::ktest!(
    fn socket_limits_give_enobufs() {
        use crate::{kassert, kassert_eq};
        use lumen_net::{ConnectPrep, Family, Proto};
        static BUDGET: Budget = Budget::new(1024 * 1024);
        let net = NetStack::new(&BUDGET, 474).map_err(|_| "test stack")?;
        // Real transports consume eight 128-KiB preparations. The ninth fails
        // before installation, and preserves both the charge and slot count.
        for _ in 0..8 {
            let prep = prepare_tcp(net.budget(), BufSizes::default()).map_err(|_| "below memory cap")?;
            net.with(|s| -> crate::ktest::TestResult {
                let id = s.open(Family::V4, Proto::Tcp).map_err(|_| "open")?;
                kassert_eq!(
                    s.connect(id, test_support::endpoint(), Some(ConnectPrep::Tcp(prep)), now()),
                    Err(NetError::InProgress)
                );
                Ok(())
            })?;
        }
        kassert_eq!(BUDGET.used(), BUDGET.limit());
        let rejected = prepare_tcp(net.budget(), BufSizes::default()).err().ok_or("budget exceeded")?;
        kassert_eq!(rejected.to_errno(), Some(crate::linux::errno::ENOBUFS));
        kassert_eq!(BUDGET.used(), BUDGET.limit());
        kassert_eq!(net.with(|s| s.socket_count()), 8);
        drop(net);
        kassert_eq!(BUDGET.used(), 0);
        let net = NetStack::new(&BUDGET, 475).map_err(|_| "slot test stack")?;
        for i in 0..=NET_SOCKETS {
            // Tiny buffers reach the slot cap long before the memory cap.
            let prep = prepare_tcp(net.budget(), BufSizes { rx: 1, tx: 1 }).map_err(|_| "small prep")?;
            let result = net.with(|s| {
                let id = s.open(Family::V4, Proto::Tcp)?;
                s.connect(id, test_support::endpoint(), Some(ConnectPrep::Tcp(prep)), now())
            });
            if i < NET_SOCKETS {
                kassert_eq!(result, Err(NetError::InProgress));
            } else {
                kassert_eq!(result.err().and_then(NetError::to_errno), Some(crate::linux::errno::ENOBUFS));
            }
        }
        kassert_eq!(net.with(|s| s.socket_count()), NET_SOCKETS);
        kassert_eq!(BUDGET.used(), 2 * NET_SOCKETS);
        kassert!(BUDGET.used() < BUDGET.limit());
        drop(net);
        kassert_eq!(BUDGET.used(), 0);
        Ok(())
    }
);
