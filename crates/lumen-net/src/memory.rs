use crate::{BufSizes, NetError};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use smoltcp::socket::{icmp, tcp, udp};

/// Shared socket-memory limit. Charges are refunded exactly once on drop.
#[derive(Debug)]
pub struct Budget {
    limit: usize,
    used: AtomicUsize,
}
impl Budget {
    /// Creates a budget with an explicit byte limit.
    pub const fn new(limit: usize) -> Self {
        Self { limit, used: AtomicUsize::new(0) }
    }
    /// Limits socket memory to one eighth of usable memory, capped by NET_MEM_CAP.
    pub const fn for_usable_memory(bytes: usize) -> Self {
        Self::new(if bytes / 8 < crate::NET_MEM_CAP { bytes / 8 } else { crate::NET_MEM_CAP })
    }
    /// Atomically reserves bytes or returns NoBufs; dropping the charge refunds them.
    pub fn try_charge(&'static self, bytes: usize) -> Result<Charge, NetError> {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|n| *n <= self.limit)
            })
            .map_err(|_| NetError::NoBufs)?;
        Ok(Charge { budget: self, bytes })
    }
    /// Refunds storage when its owning charge is dropped.
    pub(crate) fn credit(&self, bytes: usize) {
        self.used
            .try_update(Ordering::AcqRel, Ordering::Acquire, |used| used.checked_sub(bytes))
            .expect("credit exceeds charged bytes");
    }
    /// Currently charged buffer capacity in bytes.
    pub fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
    /// Maximum charged buffer capacity in bytes.
    pub fn limit(&self) -> usize {
        self.limit
    }
}
#[derive(Debug)]
/// An owned memory reservation automatically refunded on drop.
pub struct Charge {
    budget: &'static Budget,
    bytes: usize,
}
impl Charge {
    /// Number of bytes owned by this reservation.
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}
impl Drop for Charge {
    fn drop(&mut self) {
        self.budget.credit(self.bytes);
    }
}

pub(crate) fn filled<T>(len: usize, mut make: impl FnMut() -> T) -> Result<Vec<T>, NetError> {
    let mut v = Vec::new();
    v.try_reserve_exact(len).map_err(|_| NetError::NoMem)?;
    v.resize_with(len, &mut make);
    Ok(v)
}
pub(crate) fn reserved<T>(len: usize) -> Result<Vec<T>, NetError> {
    let mut v = Vec::new();
    v.try_reserve_exact(len).map_err(|_| NetError::NoMem)?;
    Ok(v)
}
/// Prepared TCP transport buffers and their budget reservation.
pub struct TcpPrep {
    pub(crate) socket: tcp::Socket<'static>,
    pub(crate) charge: Charge,
}
/// Prepared UDP packet storage and its budget reservation.
pub struct UdpPrep {
    pub(crate) socket: udp::Socket<'static>,
    pub(crate) charge: Charge,
}
/// Prepared ICMP packet storage, receive header cache and budget reservation.
pub struct IcmpPrep {
    pub(crate) socket: icmp::Socket<'static>,
    pub(crate) charge: Charge,
    pub(crate) head: Vec<u8>,
}
#[expect(
    clippy::large_enum_variant,
    reason = "prepared sockets move inline into fixed slots without another allocation"
)]
/// One typed transport preparation for connect.
pub enum ConnectPrep {
    /// TCP transport storage.
    Tcp(TcpPrep),
    /// UDP transport storage.
    Udp(UdpPrep),
    /// ICMP transport storage.
    Icmp(IcmpPrep),
}

/// Allocates exactly RX + TX payload bytes and charges their capacity before locking.
pub fn prepare_tcp(budget: &'static Budget, sizes: BufSizes) -> Result<TcpPrep, NetError> {
    if sizes.rx == 0 || sizes.tx == 0 {
        return Err(NetError::Inval);
    }
    let charge = budget.try_charge(sizes.rx.checked_add(sizes.tx).ok_or(NetError::NoBufs)?)?;
    Ok(TcpPrep {
        socket: tcp::Socket::new(
            tcp::SocketBuffer::new(filled(sizes.rx, || 0)?),
            tcp::SocketBuffer::new(filled(sizes.tx, || 0)?),
        ),
        charge,
    })
}
/// Allocates charged payload buffers and 32 packet metadata slots in each direction.
pub fn prepare_udp(budget: &'static Budget, sizes: BufSizes) -> Result<UdpPrep, NetError> {
    let bytes = sizes
        .rx
        .checked_add(sizes.tx)
        .and_then(|n| n.checked_add(64 * size_of::<udp::PacketMetadata>()))
        .ok_or(NetError::NoBufs)?;
    let charge = budget.try_charge(bytes)?;
    Ok(UdpPrep {
        socket: udp::Socket::new(
            udp::PacketBuffer::new(filled(32, || udp::PacketMetadata::EMPTY)?, filled(sizes.rx, || 0)?),
            udp::PacketBuffer::new(filled(32, || udp::PacketMetadata::EMPTY)?, filled(sizes.tx, || 0)?),
        ),
        charge,
    })
}
/// Allocates charged 4 KiB buffers, eight slots each way and one receive header cache.
pub fn prepare_icmp(budget: &'static Budget) -> Result<IcmpPrep, NetError> {
    let charge = budget.try_charge(8192 + 4116 + 16 * size_of::<icmp::PacketMetadata>())?;
    Ok(IcmpPrep {
        socket: icmp::Socket::new(
            icmp::PacketBuffer::new(filled(8, || icmp::PacketMetadata::EMPTY)?, filled(4096, || 0)?),
            icmp::PacketBuffer::new(filled(8, || icmp::PacketMetadata::EMPTY)?, filled(4096, || 0)?),
        ),
        charge,
        head: filled(4116, || 0)?,
    })
}

/// A homogeneous batch of prepared transport buffers. The variants require
/// the matching concrete preparation type; mixed/wrong buffer kinds do not compile.
///
/// ```compile_fail
/// use lumen_net::*;
/// fn wrong(p: UdpPrep) {
///     let mut buffers = [p].into_iter();
///     let _ = Preps::Tcp(&mut buffers);
/// }
/// ```
/// ```compile_fail
/// use lumen_net::*;
/// fn wrong(p: IcmpPrep) { let _ = ConnectPrep::Udp(p); }
/// ```
pub enum Preps<'a> {
    /// This operation needs no transport allocation.
    None,
    /// Prepared TCP buffers.
    Tcp(&'a mut dyn Iterator<Item = TcpPrep>),
    /// Prepared UDP buffers.
    Udp(&'a mut dyn Iterator<Item = UdpPrep>),
    /// Prepared ICMP buffers.
    Icmp(&'a mut dyn Iterator<Item = IcmpPrep>),
}
impl Preps<'_> {
    pub(crate) fn next(&mut self) -> Option<ConnectPrep> {
        match self {
            Self::None => None,
            Self::Tcp(p) => p.next().map(ConnectPrep::Tcp),
            Self::Udp(p) => p.next().map(ConnectPrep::Udp),
            Self::Icmp(p) => p.next().map(ConnectPrep::Icmp),
        }
    }
}
