//! Internet socket identity, readiness and protocol dispatch (M9 B3, #474).
//! Protocol syscall semantics are supplied by C3 (UDP), C4 (TCP), C6 (ICMP).

use super::sock::{RecvDone, RecvIo, SendIo, ShutdownHow, SockKind, SockOps};
use super::stack::{NetStack, now};
use super::virtio_net::VirtioNet;
use crate::linux::errno;
use crate::sync::{SleepMutex, SpinLock};
use crate::vfs::KernelPollHead;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use lumen_net::{BufSizes, Readiness, SockId};
use lumen_sockabi::{Family, OptVal, SockAddr, SockOpt};
use lumen_vfs::{AccessMode, PollGuard, PollMask, PollWaker, Pollable};

use super::icmp::IcmpState;
pub use super::tcp::TcpState;
use super::udp::UdpState;
#[allow(dead_code, reason = "C3/C4/C6 add the syscall constructors and protocol state")]
pub enum Proto {
    Tcp(TcpState),
    Udp(UdpState),
    Icmp(IcmpState),
}
impl Proto {
    fn transport(&self) -> lumen_net::Proto {
        match self {
            Self::Tcp(_) => lumen_net::Proto::Tcp,
            Self::Udp(_) => lumen_net::Proto::Udp,
            Self::Icmp(state) => lumen_net::Proto::IcmpEcho { raw: state.raw },
        }
    }
}

/// Buffer capacities inherited by a listener's prepared children. C5 adds
/// timeouts and option values here, under the level-10c leaf lock.
#[cfg_attr(not(feature = "ktest"), allow(dead_code, reason = "C3/C4/C6 read options during preparation"))]
pub struct InetOpts {
    pub sizes: BufSizes,
}

pub struct InetSocket {
    id: SockId,
    family: Family,
    proto: Proto,
    #[allow(dead_code, reason = "Lock 0 send/receive serialization for C3/C4/C6")]
    // Serializes senders across prepare/copy/commit chunks; no user copy under inner.
    pub(super) send_lock: SleepMutex<()>,
    #[allow(dead_code, reason = "Lock 0 send/receive serialization for C3/C4/C6")]
    // Serializes receivers across peek/copy/consume chunks.
    pub(super) recv_lock: SleepMutex<()>,
    #[cfg_attr(not(feature = "ktest"), allow(dead_code, reason = "C5 option access"))]
    opts: SpinLock<InetOpts>,
    poll: KernelPollHead,
    released: AtomicBool,
    #[cfg(feature = "ktest")]
    test_readiness: SpinLock<Option<Readiness>>,
}
impl InetSocket {
    #[allow(dead_code, reason = "family constructors are added by C3/C4/C6")]
    pub fn new(family: Family, proto: Proto) -> Result<Arc<Self>, u32> {
        let transport_family = match family {
            Family::Inet => lumen_net::Family::V4,
            Family::Inet6 => lumen_net::Family::V6,
            _ => return Err(errno::EAFNOSUPPORT),
        };
        let net = NetStack::try_get().ok_or(errno::ENETDOWN)?;
        let id = net.with(|s| s.open(transport_family, proto.transport())).map_err(|e| {
            let errno = e.to_errno();
            debug_assert!(errno.is_some(), "open returned a non-errno error: {e:?}");
            errno.unwrap_or(errno::EIO)
        })?;
        Ok(Self::from_id(id, family, proto, BufSizes::default()))
    }
    pub(super) fn from_id(id: SockId, family: Family, proto: Proto, sizes: BufSizes) -> Arc<Self> {
        let sock = Arc::new(Self {
            id,
            family,
            proto,
            send_lock: SleepMutex::new(()),
            recv_lock: SleepMutex::new(()),
            opts: SpinLock::new(InetOpts { sizes }),
            poll: KernelPollHead::new(),
            released: AtomicBool::new(false),
            #[cfg(feature = "ktest")]
            test_readiness: SpinLock::new(None),
        });
        NetStack::get().register(id, &sock);
        sock
    }
    pub fn id(&self) -> SockId {
        self.id
    }
    #[cfg_attr(not(feature = "ktest"), allow(dead_code, reason = "C3/C4/C6 preparations"))]
    pub fn buffer_sizes(&self) -> BufSizes {
        self.opts.lock().sizes
    }
    /// Inline mutations notify only after releasing inner. No interface is
    /// polled here; changed events stay listed for the net thread too.
    /// Never drop an InetSocket or call from_id inside this closure.
    #[allow(dead_code, reason = "inline family operations added by C3/C4/C6")]
    pub fn with<R>(&self, f: impl FnOnce(&mut lumen_net::Stack<VirtioNet>, SockId) -> R) -> R {
        let net = NetStack::get();
        let result = net.with(|stack| f(stack, self.id));
        self.notify();
        net.kick();
        result
    }
    /// Reads stack state without notifying or kicking the worker.
    /// Never drop an InetSocket or call from_id inside this closure.
    pub fn query<R>(&self, f: impl FnOnce(&lumen_net::Stack<VirtioNet>, SockId) -> R) -> R {
        NetStack::get().with(|stack| f(stack, self.id))
    }
    /// Event hints include every possibly changed direction; poll reads the
    /// actual level again. Never called under the stack lock.
    pub fn notify(&self) {
        self.poll.notify(PollMask::ALWAYS_READY | PollMask::RDHUP | PollMask::HUP | PollMask::ERR);
    }
    #[cfg(feature = "ktest")]
    pub fn set_test_readiness(&self, readiness: Readiness) {
        *self.test_readiness.lock() = Some(readiness);
    }
}

/// Linux v6.12 net/ipv4/tcp.c:tcp_poll, lines 558–611: normal read/write
/// bits accompany IN/OUT. lumen-net guarantees readable alongside rdhup;
/// this mapping preserves both flags without inventing transport state.
pub fn readiness_mask(r: Readiness) -> PollMask {
    let mut mask = PollMask::EMPTY;
    if r.readable {
        mask |= PollMask::IN | PollMask::RDNORM;
    }
    if r.writable {
        mask |= PollMask::OUT | PollMask::WRNORM;
    }
    if r.rdhup {
        mask |= PollMask::RDHUP;
    }
    if r.hup {
        mask |= PollMask::HUP;
    }
    if r.error {
        mask |= PollMask::ERR;
    }
    mask
}
impl Pollable for InetSocket {
    fn poll(&self, _mode: AccessMode, waker: Option<&Arc<dyn PollWaker>>) -> (PollMask, Option<PollGuard>) {
        let guard = waker.map(|w| self.poll.subscribe(Arc::clone(w)));
        let readiness = self.query(|s, id| s.readiness(id));
        #[cfg(feature = "ktest")]
        let readiness = self.test_readiness.lock().unwrap_or(readiness);
        (readiness_mask(readiness), guard)
    }
}
impl SockOps for InetSocket {
    fn family(&self) -> Family {
        self.family
    }
    fn kind(&self) -> SockKind {
        match &self.proto {
            Proto::Tcp(_) => SockKind::Stream,
            Proto::Udp(_) => SockKind::Dgram,
            Proto::Icmp(state) => {
                if state.raw {
                    SockKind::Raw
                } else {
                    SockKind::Dgram
                }
            }
        }
    }
    fn bind(&self, addr: &SockAddr) -> Result<(), u32> {
        match &self.proto {
            Proto::Tcp(state) => state.bind(self, addr),
            Proto::Udp(state) => state.bind(self, addr),
            Proto::Icmp(state) => state.bind(self, addr),
        }
    }
    fn listen(&self, backlog: i32) -> Result<(), u32> {
        match &self.proto {
            Proto::Tcp(state) => state.listen(self, backlog),
            Proto::Udp(state) => state.listen(self, backlog),
            Proto::Icmp(state) => state.listen(self, backlog),
        }
    }
    fn accept(&self, nonblock: bool) -> Result<(Arc<dyn SockOps>, Option<SockAddr>), u32> {
        match &self.proto {
            Proto::Tcp(state) => state.accept(self, nonblock),
            Proto::Udp(state) => state.accept(self, nonblock),
            Proto::Icmp(state) => state.accept(self, nonblock),
        }
    }
    fn connect(&self, addr: &SockAddr, nonblock: bool) -> Result<(), u32> {
        match &self.proto {
            Proto::Tcp(state) => state.connect(self, addr, nonblock),
            Proto::Udp(state) => state.connect(self, addr, nonblock),
            Proto::Icmp(state) => state.connect(self, addr, nonblock),
        }
    }
    fn disconnect(&self) -> Result<(), u32> {
        match &self.proto {
            Proto::Tcp(state) => state.disconnect(self),
            Proto::Udp(state) => state.disconnect(self),
            Proto::Icmp(state) => state.disconnect(self),
        }
    }
    fn send(&self, io: &mut SendIo<'_>) -> Result<usize, u32> {
        match &self.proto {
            Proto::Tcp(state) => state.send(self, io),
            Proto::Udp(state) => state.send(self, io),
            Proto::Icmp(state) => state.send(self, io),
        }
    }
    fn recv(&self, io: &mut RecvIo<'_>) -> Result<RecvDone, u32> {
        match &self.proto {
            Proto::Tcp(state) => state.recv(self, io),
            Proto::Udp(state) => state.recv(self, io),
            Proto::Icmp(state) => state.recv(self, io),
        }
    }
    fn shutdown(&self, how: ShutdownHow) -> Result<(), u32> {
        match &self.proto {
            Proto::Tcp(state) => state.shutdown(self, how),
            Proto::Udp(state) => state.shutdown(self, how),
            Proto::Icmp(state) => state.shutdown(self, how),
        }
    }
    fn local_name(&self) -> Result<SockAddr, u32> {
        match &self.proto {
            Proto::Tcp(state) => state.local_name(self),
            Proto::Udp(state) => state.local_name(self),
            Proto::Icmp(state) => state.local_name(self),
        }
    }
    fn peer_name(&self) -> Result<SockAddr, u32> {
        match &self.proto {
            Proto::Tcp(state) => state.peer_name(self),
            Proto::Udp(state) => state.peer_name(self),
            Proto::Icmp(state) => state.peer_name(self),
        }
    }
    fn set_opt(&self, opt: SockOpt, val: &OptVal) -> Result<(), u32> {
        match &self.proto {
            Proto::Tcp(state) => state.set_opt(self, opt, val),
            Proto::Udp(state) => state.set_opt(self, opt, val),
            Proto::Icmp(state) => state.set_opt(self, opt, val),
        }
    }
    fn get_opt(&self, opt: SockOpt) -> Result<OptVal, u32> {
        match &self.proto {
            Proto::Tcp(state) => state.get_opt(self, opt),
            Proto::Udp(state) => state.get_opt(self, opt),
            Proto::Icmp(state) => state.get_opt(self, opt),
        }
    }
    fn fionread(&self) -> Result<usize, u32> {
        match &self.proto {
            Proto::Tcp(state) => state.fionread(self),
            Proto::Udp(state) => state.fionread(self),
            Proto::Icmp(state) => state.fionread(self),
        }
    }
    fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            let net = NetStack::get();
            net.with(|s| s.close(self.id, false, now()));
            net.unregister(self.id);
            self.notify();
            net.kick();
        }
    }
}
impl Drop for InetSocket {
    fn drop(&mut self) {
        self.release();
    }
}
