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

// Filled by the protocol owners; state stays separate from the shared ID,
// I/O serialization, options and readiness subscription lifetime.
pub struct TcpState;
pub struct UdpState;
pub struct IcmpState {
    pub raw: bool,
}
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
    fn unsupported<T>(&self) -> Result<T, u32> {
        match self {
            Self::Tcp(_) => Err(errno::EOPNOTSUPP),  // C4
            Self::Udp(_) => Err(errno::EOPNOTSUPP),  // C3
            Self::Icmp(_) => Err(errno::EOPNOTSUPP), // C6
        }
    }
}

/// Buffer capacities inherited by a listener's prepared children. C5 adds
/// timeouts and option values here, under the level-10c leaf lock.
pub struct InetOpts {
    pub sizes: BufSizes,
}

pub struct InetSocket {
    id: SockId,
    family: Family,
    proto: Proto,
    #[allow(dead_code, reason = "Lock 0 send/receive serialization for C3/C4/C6")]
    pub(super) send_lock: SleepMutex<()>,
    #[allow(dead_code, reason = "Lock 0 send/receive serialization for C3/C4/C6")]
    pub(super) recv_lock: SleepMutex<()>,
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
        let id = net.with(|s| s.open(transport_family, proto.transport())).map_err(|e| e.to_errno().unwrap())?;
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
    pub fn buffer_sizes(&self) -> BufSizes {
        self.opts.lock().sizes
    }
    /// Inline mutations notify only after releasing inner. No interface is
    /// polled here; changed events stay listed for the net thread too.
    #[allow(dead_code, reason = "inline family operations added by C3/C4/C6")]
    pub fn with<R>(&self, f: impl FnOnce(&mut lumen_net::Stack<VirtioNet>, SockId) -> R) -> R {
        let net = NetStack::get();
        let result = net.with(|stack| f(stack, self.id));
        self.notify();
        net.kick();
        result
    }
    /// Event hints include every possibly changed direction; poll reads the
    /// actual level again. Never called under the stack lock.
    pub fn notify(&self) {
        self.poll.notify(
            PollMask::IN
                | PollMask::RDNORM
                | PollMask::OUT
                | PollMask::WRNORM
                | PollMask::RDHUP
                | PollMask::HUP
                | PollMask::ERR,
        );
    }
    #[cfg(feature = "ktest")]
    pub fn set_test_readiness(&self, readiness: Readiness) {
        *self.test_readiness.lock() = Some(readiness);
    }
}

/// Linux v6.12 net/ipv4/tcp.c:tcp_poll, lines 558–611: normal read/write
/// bits accompany IN/OUT; FIN, close and pending errors are independent.
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
        let readiness = NetStack::get().with(|s| s.readiness(self.id));
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
    fn bind(&self, _addr: &SockAddr) -> Result<(), u32> {
        self.proto.unsupported()
    }
    fn listen(&self, _backlog: i32) -> Result<(), u32> {
        self.proto.unsupported()
    }
    fn accept(&self, _nonblock: bool) -> Result<(Arc<dyn SockOps>, Option<SockAddr>), u32> {
        self.proto.unsupported()
    }
    fn connect(&self, _addr: &SockAddr, _nonblock: bool) -> Result<(), u32> {
        self.proto.unsupported()
    }
    fn disconnect(&self) -> Result<(), u32> {
        self.proto.unsupported()
    }
    fn send(&self, _io: &mut SendIo<'_>) -> Result<usize, u32> {
        self.proto.unsupported()
    }
    fn recv(&self, _io: &mut RecvIo<'_>) -> Result<RecvDone, u32> {
        self.proto.unsupported()
    }
    fn shutdown(&self, _how: ShutdownHow) -> Result<(), u32> {
        self.proto.unsupported()
    }
    fn local_name(&self) -> Result<SockAddr, u32> {
        self.proto.unsupported()
    }
    fn peer_name(&self) -> Result<SockAddr, u32> {
        self.proto.unsupported()
    }
    fn set_opt(&self, _opt: SockOpt, _val: &OptVal) -> Result<(), u32> {
        self.proto.unsupported()
    }
    fn get_opt(&self, _opt: SockOpt) -> Result<OptVal, u32> {
        self.proto.unsupported()
    }
    fn fionread(&self) -> Result<usize, u32> {
        self.proto.unsupported()
    }
    fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            NetStack::get().with(|s| s.close(self.id, false, now()));
            NetStack::get().unregister(self.id);
            self.notify();
            NetStack::get().kick();
        }
    }
}
impl Drop for InetSocket {
    fn drop(&mut self) {
        self.release();
    }
}
