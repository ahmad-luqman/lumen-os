//! Serialized network stack and its sole polling thread (M9 B3, #474).

use super::inet::InetSocket;
use super::virtio_net::{self, VirtioNet};
use crate::sched::{self, WaitQueue};
use crate::sync::{SleepMutex, SleepMutexGuard};
use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};
#[cfg(feature = "ktest")]
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicUsize, Ordering};
use lumen_net::BufSizes;
use lumen_net::{
    Budget, EthConfig, IfaceId, Instant, Loopback, NET_ENTRIES, NET_SOCKETS, NetError, SockId, Stack, prepare_tcp,
};

static STACK: AtomicPtr<NetStack> = AtomicPtr::new(core::ptr::null_mut());
#[repr(u8)]
enum Startup {
    Waiting,
    Failed,
    Up,
}
static STARTUP: AtomicU8 = AtomicU8::new(Startup::Waiting as u8);
static STARTED: WaitQueue = WaitQueue::new();

pub(super) fn startup_status() -> &'static str {
    match STARTUP.load(Ordering::Acquire) {
        n if n == Startup::Up as u8 => "up",
        n if n == Startup::Failed as u8 => "failed",
        _ => "waiting for entropy",
    }
}

type Notifications = Vec<(Arc<InetSocket>, bool)>;
type Shortfalls = Vec<(SockId, IfaceId, usize, BufSizes)>;
#[cfg(feature = "ktest")]
static TEST_NOW: AtomicU64 = AtomicU64::new(u64::MAX);

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
    #[cfg(feature = "ktest")]
    rounds: AtomicU64,
}

struct NetInner {
    stack: Stack<VirtioNet>,
    entries: Box<[Option<Entry>]>,
}

struct Entry {
    id: SockId,
    sock: Weak<InetSocket>,
    warned: bool,
}

const IDLE_POLL_NS: u64 = 100_000_000;

fn idle_deadline(now_ns: u64, time: Instant, next: Option<Instant>) -> u64 {
    let delta = next.map_or(u64::MAX, |next| {
        (next.total_micros().saturating_sub(time.total_micros()).max(0) as u64).saturating_mul(1000)
    });
    now_ns.saturating_add(delta.min(IDLE_POLL_NS))
}

fn reserved<T>(capacity: usize) -> Result<Vec<T>, NetError> {
    let mut v = Vec::new();
    v.try_reserve_exact(capacity).map_err(|_| NetError::NoMem)?;
    Ok(v)
}

fn boxed<T>(value: T) -> Result<Box<T>, NetError> {
    let mut storage = reserved(1)?;
    storage.push(value);
    let ptr = Box::into_raw(storage.into_boxed_slice()).cast::<T>();
    // SAFETY: the boxed slice has exactly one initialized T; its allocation
    // has the same layout and ownership as Box<T> and is consumed once.
    Ok(unsafe { Box::from_raw(ptr) })
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
    #[cfg(feature = "ktest")]
    {
        let instant = TEST_NOW.load(Ordering::SeqCst);
        if instant != u64::MAX {
            return Instant::from_micros(instant as i64);
        }
    }
    Instant::from_micros((crate::time::now_ns() / 1000) as i64)
}

impl NetStack {
    fn new(budget: &'static Budget, seed: u64) -> Result<Self, NetError> {
        let stack = Stack::try_new(Loopback::try_new(65535, 16)?, now(), seed)?;
        let mut entries = reserved(NET_ENTRIES)?;
        entries.resize_with(NET_ENTRIES, || None);
        Ok(Self {
            inner: SleepMutex::new(NetInner { stack, entries: entries.into_boxed_slice() }),
            kicked: AtomicBool::new(true),
            wake: WaitQueue::new(),
            budget,
            refill_failures: AtomicUsize::new(0),
            #[cfg(feature = "ktest")]
            owner: AtomicU64::new(0),
            #[cfg(feature = "ktest")]
            rounds: AtomicU64::new(0),
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
        #[cfg(feature = "ktest")]
        assert!(!self.held_by_me(), "recursive NetStack::inner acquisition");
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
    /// Never drop an InetSocket or call InetSocket::from_id inside this closure.
    pub(super) fn with<R>(&self, f: impl FnOnce(&mut Stack<VirtioNet>) -> R) -> R {
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
    /// Registers a live identity once; stale callers cannot replace its generation.
    pub fn register(&self, id: SockId, sock: &Arc<InetSocket>) {
        if !self.register_current(id, sock) {
            log::warn!(target: "net", "register called with absent socket {id:?}");
            debug_assert!(false, "register requires a live stack identity");
        }
    }
    // The rejection is separately testable without invoking the public
    // contract's debug assertion (kernel tests cannot catch a panic).
    fn register_current(&self, id: SockId, sock: &Arc<InetSocket>) -> bool {
        let mut inner = self.lock();
        assert_eq!(id, sock.id());
        if !inner.stack.contains(id) {
            return false;
        }
        if let Some(entry) = &inner.entries[id.index()] {
            assert!(entry.id != id || entry.sock.strong_count() == 0, "socket registered twice");
        }
        inner.entries[id.index()] = Some(Entry { id, sock: Arc::downgrade(sock), warned: false });
        true
    }
    /// Removes a released socket without disturbing a recycled generation.
    pub fn unregister(&self, id: SockId) {
        let mut inner = self.lock();
        if inner.entries[id.index()].as_ref().is_some_and(|entry| entry.id == id) {
            inner.entries[id.index()] = None;
        }
    }
    #[cfg(feature = "ktest")]
    pub fn held_by_me(&self) -> bool {
        let me = sched::running_id_here().map_or(0, |(id, _)| id);
        me != 0 && self.owner.load(Ordering::Acquire) == me
    }
    fn refill_failed(&self, id: SockId, error: NetError) {
        let warn = {
            let mut inner = self.lock();
            match inner.entries[id.index()].as_mut().filter(|entry| entry.id == id) {
                Some(entry) => !core::mem::replace(&mut entry.warned, true),
                // The listener may have closed while its preparation failed.
                // Still report that failure, without touching a recycled slot.
                None => true,
            }
        };
        if warn {
            self.refill_failures.fetch_add(1, Ordering::Relaxed);
            log::warn!(target: "net", "listener {id:?}: pool refill failed: {error:?} (once per episode)");
        }
    }
    fn refill_succeeded(&self, id: SockId) {
        let mut inner = self.lock();
        if inner.stack.shortfalls().iter().any(|(listener, _, _)| *listener == id) {
            return;
        }
        if let Some(entry) = inner.entries[id.index()].as_mut().filter(|entry| entry.id == id) {
            entry.warned = false;
        }
    }
    fn run(&self, mut notifications: Notifications, mut shortfalls: Shortfalls) -> ! {
        // Reserved before spawn; neither vector grows while inner is held.
        let mut seen = virtio_net::BELL.rings();
        let mut deadline = 0;
        loop {
            let started = crate::time::now_ns();
            let woke = self.wake.wait_until_or_deadline(deadline, || {
                self.kicked.load(Ordering::Acquire) || virtio_net::BELL.rings() != seen
            });
            // Clear BEFORE the round: concurrent inline work/IRQs remain
            // visible at the next wait. B2 also rings after 16 bad RX entries.
            self.kicked.store(false, Ordering::Release);
            let silent_deadline = !woke && started < deadline;
            let (more, next, time, now_ns, answered, before_poll, after_poll) = {
                let mut inner = self.lock();
                // Sample immediately before looking at the device, including
                // IRQs that arrived while acquisition of inner was blocked.
                let before_poll = virtio_net::BELL.rings();
                let before = inner.stack.device_mut().map(|device| device.completions());
                if let Some(device) = inner.stack.device_mut() {
                    device.selftest_step();
                }
                let now_ns = crate::time::now_ns();
                let time = now();
                let round = inner.stack.poll_round(time);
                let (usable, full) = inner.stack.device_mut().map_or((true, false), |d| (d.usable(), d.tx_full()));
                let next = lumen_net::worker_deadline(
                    usable,
                    full,
                    inner.stack.poll_at(time),
                    inner.stack.poll_at_while_eth_blocked(time),
                );
                let answered = inner.stack.device_mut().map(|device| device.completions()) != before;
                let NetInner { stack, entries } = &mut *inner;
                for id in stack.changed() {
                    if let Some(sock) = entries[id.index()].as_ref().and_then(|entry| entry.sock.upgrade()) {
                        let current = sock.id() == *id;
                        // Even a mismatched last Arc must be dropped outside
                        // inner: InetSocket::drop takes this lock again.
                        debug_assert!(notifications.len() < notifications.capacity());
                        notifications.push((sock, current));
                    }
                }
                for &(id, iface, count) in stack.shortfalls() {
                    if let Some(sizes) = stack.listener_sizes(id) {
                        debug_assert!(shortfalls.len() < shortfalls.capacity());
                        shortfalls.push((id, iface, count, sizes));
                    }
                }
                // A ring during the round is an answered interrupt, not a
                // missed one. Keep before_poll as seen so it prompts a recheck.
                let after_poll = virtio_net::BELL.rings();
                (round.more, next, time, now_ns, answered, before_poll, after_poll)
            };
            if silent_deadline && before_poll == seen && after_poll == seen && answered {
                // The IRQ can still be in flight after the final sample. This
                // recovery observation must not make a healthy boot warn.
                virtio_net::BELL.note_missed_info(None);
            }
            seen = before_poll;
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
                        Ok(true) => {
                            installed = true;
                            self.refill_succeeded(id);
                        }
                        Ok(false) => break,
                        Err(error) => {
                            self.refill_failed(id, error);
                            break;
                        }
                    }
                }
            }
            #[cfg(feature = "ktest")]
            self.rounds.fetch_add(1, Ordering::SeqCst);
            // Immediate receive work wins over Ethernet backpressure. An
            // expired egress deadline on a full TX queue must not busy-loop.
            deadline = if more || installed { 0 } else { idle_deadline(now_ns, time, next) };
        }
    }
}

/// IRQs before publication only ring BELL; the first round observes them.
pub fn kick() {
    if let Some(stack) = NetStack::try_get() {
        stack.kick();
    }
}

fn seeds() -> Result<(u64, u64), crate::rng::NoSource> {
    Ok((crate::rng::u64()?, crate::rng::u64()?))
}

pub fn init() {
    match seeds() {
        Ok(seeds) => start(seeds),
        Err(why) => {
            log::info!(target: "net", "stack waiting for entropy: {why}");
            if !crate::rng::can_retry_seed() {
                STARTUP.store(Startup::Failed as u8, Ordering::Release);
                log::info!(target: "net", "stack disabled: no entropy source can answer later; raw diagnostics retained");
                return;
            }
            if sched::spawn("net-init", || {
                loop {
                    let _ = sched::sleep(1_000_000_000);
                    if let Ok(seeds) = seeds() {
                        start(seeds);
                        return;
                    }
                    if !crate::rng::can_retry_seed() {
                        STARTUP.store(Startup::Failed as u8, Ordering::Release);
                        log::info!(target: "net", "stack disabled: no entropy source can answer later; raw diagnostics retained");
                        return;
                    }
                }
            })
            .is_err()
            {
                STARTUP.store(Startup::Failed as u8, Ordering::Release);
                log::error!(target: "net", "cannot start entropy retry thread");
            }
        }
    }
}

fn start((lo_seed, eth_seed): (u64, u64)) {
    let before = crate::mm::heap::stats().used;
    let result = (|| {
        let budget = boxed(Budget::for_usable_memory(crate::mm::frame::counts().owned * 4096))?;
        let budget = Box::leak(budget);
        let net = boxed(NetStack::new(budget, lo_seed)?)?;
        Ok::<_, NetError>((net, reserved(NET_ENTRIES)?, reserved(2 * NET_ENTRIES)?))
    })();
    let (net, notifications, shortfalls) = match result {
        Ok(net) => net,
        Err(error) => {
            STARTUP.store(Startup::Failed as u8, Ordering::Release);
            log::error!(target: "net", "network fixed storage unavailable: {error:?}; raw diagnostics retained");
            return;
        }
    };
    // Deferred startup can overlap frees by other threads; this diagnostic
    // delta must never turn successful allocation into an overflow panic.
    let fixed_bytes = crate::mm::heap::stats().used.saturating_sub(before);
    log::info!(target: "net", "fixed boot storage {fixed_bytes} bytes (including worker vectors; excluding thread and NIC DMA)");
    // The closure owns the stack and vectors. If spawn fails it is dropped,
    // leaving DEVICE untouched. Only the successfully spawned owner takes
    // the device and publishes the stack; init waits for that publication.
    if let Err(error) = sched::spawn("net", move || {
        if let Some(device) = virtio_net::take_device() {
            // B4 chooses DHCP/static/off. Until then attach an unconfigured NIC;
            // the ARP diagnostic remains independent of address configuration.
            // attach_eth cannot fail here: this fresh stack owns both reserved
            // sets, one IPv4 address fits, and router=None never adds a route.
            // configure_eth(None, None) cannot fail after the successful attach.
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
        let net = Box::leak(net);
        assert!(STACK.compare_exchange(core::ptr::null_mut(), net, Ordering::Release, Ordering::Relaxed).is_ok());
        let net: &'static NetStack = net;
        STARTUP.store(Startup::Up as u8, Ordering::Release);
        STARTED.wake_all();
        log::info!(target: "net", "lo 127.0.0.1/8 ::1/128; socket sets {SET_STORAGE_BYTES} bytes; memory limit {}", net.budget.limit());
        net.run(notifications, shortfalls);
    }) {
        STARTUP.store(Startup::Failed as u8, Ordering::Release);
        log::error!(target: "net", "cannot spawn net thread: {error:?}; raw diagnostics retained");
        return;
    }
    STARTED.wait_until(|| STARTUP.load(Ordering::Acquire) == Startup::Up as u8);
}

/// Snapshot diagnostics under inner; formatting and output may allocate and
/// take console locks, so they happen after the snapshot's guard is gone.
pub(super) fn command(net: &NetStack, ctx: &mut crate::shell::Ctx<'_>) {
    let mut ports = Vec::with_capacity(NET_ENTRIES);
    let (device, sockets, entries, addresses) = {
        let mut inner = net.lock();
        let entries = inner.entries.iter().filter(|entry| entry.is_some()).count();
        let stack = &mut inner.stack;
        for port in stack.time_wait_ports() {
            debug_assert!(ports.len() < ports.capacity());
            ports.push(port);
        }
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
pub(super) fn device_test_guard() -> Option<DeviceTestGuard> {
    NetStack::try_get().map(|net| DeviceTestGuard(net.lock()))
}

#[cfg(feature = "ktest")]
mod test_support {
    use super::super::inet::{Proto, TcpState};
    use super::*;
    use lumen_net::{BindReq, ConnectPrep, IpAddress, IpEndpoint, Preps};
    use lumen_sockabi::Family;
    use lumen_vfs::{AccessMode, PollMask, PollWaker, Pollable};

    pub struct FrozenClock;
    impl FrozenClock {
        pub fn new() -> Self {
            let instant = now().total_micros() as u64;
            assert_eq!(TEST_NOW.swap(instant, Ordering::SeqCst), u64::MAX);
            Self
        }
        pub fn advance(&self, to: Instant) {
            TEST_NOW.store(to.total_micros() as u64, Ordering::SeqCst);
        }
    }
    impl Drop for FrozenClock {
        fn drop(&mut self) {
            TEST_NOW.store(u64::MAX, Ordering::SeqCst);
            NetStack::get().kick();
        }
    }
    pub fn family(v6: bool) -> Family {
        if v6 { Family::Inet6 } else { Family::Inet }
    }
    pub fn readiness(sock: &InetSocket) -> lumen_net::Readiness {
        sock.query(|s, id| s.readiness(id))
    }
    pub fn subscribe(sock: &InetSocket, waiter: &Arc<Waker>) -> Option<lumen_vfs::PollGuard> {
        let waker: Arc<dyn PollWaker> = waiter.clone();
        sock.poll(AccessMode::ReadWrite, Some(&waker)).1
    }
    pub fn resources() -> (usize, usize) {
        let net = NetStack::get();
        net.with(|s| (net.budget().used(), s.socket_count()))
    }
    pub fn resources_return_to(expected: (usize, usize)) {
        log::info!(target: "net-test", "waiting for released buffers and transport slots: {expected:?}");
        while resources() != expected {
            let _ = sched::sleep(1_000_000);
        }
    }
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
        fn wake(&self, _ready: PollMask, _defer: &mut lumen_vfs::PollDefer) {
            assert!(!NetStack::get().held_by_me(), "readiness notified while holding NetStack::inner");
            if sched::running_id_here().unwrap().0 != self.caller {
                self.worker_wakes.fetch_add(1, Ordering::SeqCst);
            } else {
                self.caller_wakes.fetch_add(1, Ordering::SeqCst);
            }
            self.sequence.fetch_add(1, Ordering::SeqCst);
            self.wait.wake_all();
        }
    }
    pub fn wait(step: &str, sock: &InetSocket, mut ready: impl FnMut() -> bool) {
        let waiter = Waker::new();
        let _guard = subscribe(sock, &waiter);
        loop {
            let seen = waiter.sequence.load(Ordering::SeqCst);
            if ready() {
                return;
            }
            log::info!(target: "net-test", "waiting for {step}");
            waiter.wait.wait_until(|| waiter.sequence.load(Ordering::SeqCst) != seen);
        }
    }
    pub fn socket(v6: bool) -> Result<Arc<InetSocket>, &'static str> {
        InetSocket::new(family(v6), Proto::Tcp(TcpState::new())).map_err(|_| "open TCP")
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
        let sizes = sock.buffer_sizes();
        sock.with(|s, id| s.listen(id, 3, sizes, &mut pool, now())).map_err(|_| "listen")?;
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
        wait("accept", listener, || readiness(listener).readable);
        let (id, _) = listener.with(|s, id| s.accept(id)).map_err(|_| "accept")?;
        Ok(InetSocket::from_id(
            id,
            family(v6),
            Proto::Tcp(TcpState::new()),
            listener.query(|s, id| s.listener_sizes(id)).ok_or("listener sizes")?,
        ))
    }
    pub fn echo(from: &InetSocket, to: &InetSocket, tag: u8) -> crate::ktest::TestResult {
        let mut sent = 0;
        let mut received = 0;
        let data: [u8; 4096] = core::array::from_fn(|i| (i as u8).wrapping_add(tag));
        let mut out = [0; 4096];
        while received < 65536 {
            if sent < 65536 {
                wait("send capacity", from, || readiness(from).writable);
                let n = from
                    .with(|s, id| s.send(id, &data[..(65536 - sent).min(data.len())], None, Preps::None, now()))
                    .map_err(|_| "send bytes")?;
                crate::kassert!(n != 0);
                sent += n;
            }
            wait("echo data", to, || readiness(to).readable);
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
        resources_return_to((0, 0));
        let baseline = resources();
        for v6 in [false, true] {
            let listener = listener(v6)?;
            let client = connect(&listener, v6)?;
            let accepted = accept(&listener, v6)?;
            echo(&client, &accepted, 17)?;
            echo(&accepted, &client, 193)?;
        }
        resources_return_to(baseline);
        crate::kassert_eq!(resources(), baseline);
        Ok(())
    }
);

crate::ktest!(
    fn readiness_notifies_after_the_stack_lock() {
        use crate::{kassert, kassert_eq};
        use test_support::*;
        let net = NetStack::get();
        let listener = listener(false)?;
        let waiter = Waker::new();
        let _guard = subscribe(&listener, &waiter);
        // Positive control: held_by_me observes the actual acquiring thread.
        net.with(|_| {
            assert!(net.held_by_me());
        });
        kassert!(!net.held_by_me());
        let client = connect(&listener, false)?;
        let accepted = accept(&listener, false)?;
        log::info!(target: "net-test", "waiting for worker readiness notification");
        waiter.wait.wait_until(|| waiter.worker_wakes.load(Ordering::SeqCst) != 0);
        kassert!(waiter.sequence.load(Ordering::SeqCst) != 0);
        // Inline shutdown must notify in this caller after dropping inner too.
        let inline = Waker::new();
        let _guard = subscribe(&client, &inline);
        let before_query = inline.caller_wakes.load(Ordering::SeqCst);
        client.query(|s, id| s.readiness(id));
        kassert_eq!(inline.caller_wakes.load(Ordering::SeqCst), before_query);
        kassert_eq!(client.with(|s, id| s.shutdown(id, lumen_net::Shut::Wr, now())), Ok(()));
        kassert_eq!(inline.caller_wakes.load(Ordering::SeqCst), 1);
        wait("peer FIN", &accepted, || readiness(&accepted).rdhup);
        let before = inline.caller_wakes.load(Ordering::SeqCst);
        use crate::net::sock::SockOps;
        client.release();
        kassert!(!net.held_by_me());
        kassert_eq!(inline.caller_wakes.load(Ordering::SeqCst), before + 1);
        client.release();
        kassert_eq!(inline.caller_wakes.load(Ordering::SeqCst), before + 1);
        Ok(())
    }
);

crate::ktest!(
    fn listen_pool_refills_between_polls() {
        use crate::{kassert, kassert_eq};
        use test_support::*;
        resources_return_to((0, 0));
        let baseline = resources();
        let listener = listener(false)?;
        let mut accepted_count = 0;
        for _ in 0..3 {
            let clients = (0..4).map(|_| connect(&listener, false)).collect::<Result<Vec<_>, _>>()?;
            let accepted = (0..4).map(|_| accept(&listener, false)).collect::<Result<Vec<_>, _>>()?;
            accepted_count += accepted.len();
            for (client, child) in clients.iter().zip(&accepted) {
                wait("connect", client, || readiness(client).writable);
                kassert_eq!(NetStack::get().with(|s| s.take_error(client.id())), None);
                kassert_eq!(
                    NetStack::get().with(|s| s.remote_endpoint(child.id())),
                    NetStack::get().with(|s| s.local_endpoint(client.id()))
                );
            }
            kassert!(!NetStack::get().with(|s| s.readiness(listener.id()).readable));
        }
        kassert_eq!(accepted_count, 12);
        drop(listener);
        resources_return_to(baseline);
        kassert_eq!(resources(), baseline);
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

crate::ktest!(
    fn recycled_socket_registration_preserves_the_new_generation() {
        use super::sock::SockOps;
        use crate::{kassert, kassert_eq};
        use test_support::*;
        let net = NetStack::get();
        let old = socket(false)?;
        let old_id = old.id();
        old.release();
        net.with(|s| {
            s.changed();
        });
        let new = socket(false)?;
        kassert_eq!(old_id.index(), new.id().index());
        kassert!(old_id != new.id());
        net.unregister(old_id);
        kassert!(!net.register_current(old_id, &old));
        net.refill_failed(old_id, NetError::NoBufs);
        let inner = net.lock();
        let entry = inner.entries[new.id().index()].as_ref().ok_or("lost new entry")?;
        kassert_eq!(entry.id, new.id());
        kassert!(!entry.warned);
        kassert_eq!(entry.sock.strong_count(), 1);
        drop(inner);
        Ok(())
    }
);

crate::ktest!(
    fn refill_failures_warn_once_per_episode_and_recover() {
        use crate::{kassert, kassert_eq};
        use lumen_net::ConnectPrep;
        use test_support::*;
        resources_return_to((0, 0));
        let net = NetStack::get();
        let listener = listener(false)?;
        let start = net.refill_failures.load(Ordering::SeqCst);
        let warning = alloc::format!("WARN  net: listener {:?}: pool refill failed:", listener.id());
        let mut log = alloc::vec![0; crate::klog::RING_BYTES];
        for episode in 1..=2 {
            let client = socket(false)?;
            let prep = prepare_tcp(net.budget(), client.buffer_sizes()).map_err(|_| "client prep")?;
            let held =
                net.budget().try_charge(net.budget().limit() - net.budget().used()).map_err(|_| "fill budget")?;
            let remote = listener.query(|s, id| s.local_endpoint(id)).ok_or("listener address")?;
            kassert_eq!(
                client.with(|s, id| s.connect(id, remote, Some(ConnectPrep::Tcp(prep)), now())),
                Err(NetError::InProgress)
            );
            let child = accept(&listener, false)?;
            log::info!(target: "net-test", "waiting for refill failure episode {episode}");
            loop {
                let (n, _) = crate::klog::copy_recent(&mut log);
                let text = core::str::from_utf8(&log[..n]).map_err(|_| "log UTF-8")?;
                if text.matches(&warning).count() == episode {
                    break;
                }
                let _ = sched::sleep(1_000_000);
            }
            let rounds = net.rounds.load(Ordering::SeqCst);
            log::info!(target: "net-test", "waiting for repeated failed refills");
            while net.rounds.load(Ordering::SeqCst) < rounds + 2 {
                let _ = sched::sleep(1_000_000);
            }
            kassert_eq!(net.refill_failures.load(Ordering::SeqCst), start + episode);
            let (n, _) = crate::klog::copy_recent(&mut log);
            kassert_eq!(core::str::from_utf8(&log[..n]).map_err(|_| "log UTF-8")?.matches(&warning).count(), episode);
            drop(held);
            net.kick();
            log::info!(target: "net-test", "waiting for successful refill after episode {episode}");
            loop {
                let inner = net.lock();
                let recovered = inner.entries[listener.id().index()].as_ref().is_some_and(|entry| !entry.warned)
                    && inner.stack.shortfalls().is_empty();
                drop(inner);
                if recovered {
                    break;
                }
                let _ = sched::sleep(1_000_000);
            }
            kassert!(net.budget().used() < net.budget().limit());
            drop((client, child));
        }
        drop(listener);
        resources_return_to((0, 0));
        Ok(())
    }
);

crate::ktest!(
    fn loopback_time_wait_expires_with_unusable_ethernet() {
        use crate::{kassert, kassert_eq};
        use test_support::*;
        resources_return_to((0, 0));
        let net = NetStack::get();
        // Freeze caller/worker time so a busy host cannot expire the
        // reservation before the failed-device phase observes it.
        let clock = FrozenClock::new();
        let listener = listener(false)?;
        let client = connect(&listener, false)?;
        let child = accept(&listener, false)?;
        let port = client.query(|s, id| s.local_endpoint(id)).ok_or("client port")?.port;
        client.with(|s, id| s.shutdown(id, lumen_net::Shut::Wr, now())).map_err(|_| "client FIN")?;
        wait("peer FIN before TIME-WAIT", &child, || readiness(&child).rdhup);
        drop((child, client, listener));
        resources_return_to((0, 0));
        let deadline = net
            .with(|s| s.time_wait_ports().find(|(p, _)| *p == port).map(|(_, until)| until))
            .ok_or("missing TIME-WAIT reservation")?;
        let _failed = virtio_net::FailedDevice::new()?;
        let rounds = net.rounds.load(Ordering::SeqCst);
        log::info!(target: "net-test", "waiting for a poll with failed Ethernet");
        while net.rounds.load(Ordering::SeqCst) == rounds {
            let _ = sched::sleep(1_000_000);
        }
        clock.advance(deadline);
        // No kick or synthetic interrupt: advancing this frozen clock does
        // not reschedule the real wait. The 100 ms idle cap must recheck it.
        log::info!(target: "net-test", "waiting for loopback TIME-WAIT expiry with failed Ethernet");
        while net.with(|s| s.time_wait_ports().any(|(p, _)| p == port)) {
            let _ = sched::sleep(10_000_000);
        }
        kassert!(net.rounds.load(Ordering::SeqCst) > rounds);
        kassert_eq!(resources(), (0, 0));
        Ok(())
    }
);

crate::ktest!(
    fn idle_wait_preserves_earlier_timers_and_bounds_silent_devices() {
        use crate::kassert_eq;
        // Deliberately unrelated clock origins: a frozen stack clock must
        // not turn a future timer into a past-due real-time busy loop.
        let now = 90_000_000_000;
        let time = Instant::from_secs(1);
        kassert_eq!(idle_deadline(now, time, None), now + IDLE_POLL_NS);
        kassert_eq!(idle_deadline(now, time, Some(Instant::from_millis(1001))), now + 1_000_000);
        kassert_eq!(idle_deadline(now, time, Some(Instant::from_secs(5))), now + IDLE_POLL_NS);
        kassert_eq!(idle_deadline(now, time, Some(Instant::ZERO)), now);
        kassert_eq!(idle_deadline(u64::MAX - 1, time, None), u64::MAX);
        Ok(())
    }
);

crate::ktest!(
    fn net_diagnostics_report_open_listener_and_memory() {
        use crate::shell::{Captured, Ctx};
        use crate::{kassert, kassert_eq};
        use test_support::*;
        resources_return_to((0, 0));
        let listener = listener(false)?;
        let net = NetStack::get();
        let mut out = Captured::default();
        let history = lumen_shell::History::new(1);
        command(net, &mut Ctx::new(&mut out, false, &history));
        let used = net.budget().used();
        kassert!(used > 0);
        kassert_eq!(net.with(|s| s.socket_count()), 4);
        kassert!(out.text.contains(&alloc::format!(
            "net: open sockets 1, transport slots 4, refill failures {}, socket memory {}/{}",
            net.refill_failures.load(Ordering::Relaxed),
            used,
            net.budget().limit()
        )));
        drop(listener);
        resources_return_to((0, 0));
        Ok(())
    }
);

crate::ktest!(
    fn partial_listener_refill_keeps_the_failure_episode() {
        use crate::{kassert, kassert_eq};
        use lumen_net::{BindReq, Family, Preps, Proto};
        static BUDGET: Budget = Budget::new(1024 * 1024);
        let net = NetStack::new(&BUDGET, 501).map_err(|_| "test stack")?;
        let sizes = BufSizes { rx: 64, tx: 64 };
        let prep = prepare_tcp(&BUDGET, sizes).map_err(|_| "initial prep")?;
        let id = net
            .with(|s| {
                let id = s.open(Family::V4, Proto::Tcp)?;
                s.bind(
                    id,
                    BindReq { addr: Some(test_support::address(false)), ..BindReq::default() },
                    Preps::None,
                    now(),
                )?;
                s.listen(id, 0, sizes, &mut [prep].into_iter(), now())?;
                Ok::<_, NetError>(id)
            })
            .map_err(|_| "listen")?;
        // Reserve a two-slot target before consuming the pool through
        // connections. All mutations use real listener bookkeeping.
        let additional = prepare_tcp(&BUDGET, sizes).map_err(|_| "additional prep")?;
        net.with(|s| s.listen(id, 1, sizes, &mut [additional].into_iter(), now())).map_err(|_| "relisten")?;
        // With no kernel socket registered, install only the bookkeeping
        // entry needed by the warning latch; its generation is the live ID.
        net.lock().entries[id.index()] = Some(Entry { id, sock: Weak::new(), warned: true });
        // A live shortfall is produced by two real connections and accepts.
        let mut clients = Vec::new();
        for _ in 0..2 {
            let prep = prepare_tcp(&BUDGET, sizes).map_err(|_| "client prep")?;
            let client = net
                .with(|s| {
                    let client = s.open(Family::V4, Proto::Tcp)?;
                    let remote = s.local_endpoint(id).ok_or(NetError::Inval)?;
                    assert_eq!(
                        s.connect(client, remote, Some(lumen_net::ConnectPrep::Tcp(prep)), now()),
                        Err(NetError::InProgress)
                    );
                    Ok::<_, NetError>(client)
                })
                .map_err(|_| "client")?;
            clients.push(client);
        }
        for _ in 0..16 {
            net.with(|s| {
                s.poll_round(now());
            });
        }
        for _ in 0..2 {
            net.with(|s| s.accept(id)).map_err(|_| "accept")?;
        }
        kassert_eq!(net.with(|s| s.shortfalls().iter().map(|(_, _, n)| n).sum::<usize>()), 2);
        for remaining in [1, 0] {
            let prep = prepare_tcp(&BUDGET, sizes).map_err(|_| "refill prep")?;
            kassert_eq!(net.with(|s| s.refill(id, IfaceId::Lo, prep, now())), Ok(true));
            net.refill_succeeded(id);
            kassert_eq!(net.lock().entries[id.index()].as_ref().unwrap().warned, remaining != 0);
        }
        kassert!(net.with(|s| s.shortfalls().is_empty()));
        drop(net);
        kassert_eq!(BUDGET.used(), 0);
        Ok(())
    }
);

crate::ktest!(
    fn worker_deadline_selection_preserves_timers_for_every_device_state() {
        use crate::kassert_eq;
        let egress = Instant::from_secs(4);
        let timer = Instant::from_secs(6);
        for (usable, full, expected) in [
            (true, true, timer),   // usable but full
            (false, false, timer), // broken
            (false, true, timer),  // not ready
            (true, false, egress), // usable with a free TX slot
        ] {
            kassert_eq!(lumen_net::worker_deadline(usable, full, Some(egress), Some(timer)), Some(expected));
        }
        Ok(())
    }
);
