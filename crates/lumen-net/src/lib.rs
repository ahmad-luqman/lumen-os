//! Linux TCP/UDP semantics with caller-supplied monotonic time.
//!
//! Allocate only in `try_new` and `prepare_*`, before taking the stack lock.
//! All other operations use bounded storage. No method sleeps or reads a clock.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod device;
mod memory;
mod packet;
mod port;
mod stack;

pub use device::{ETH_FRAME_MAX, Loopback, NetDevice, NetInfo, TxError};
pub use memory::{
    Budget, Charge, ConnectPrep, IcmpPrep, Preps, TcpPrep, UdpPrep, prepare_icmp, prepare_tcp, prepare_udp,
};
use port::PortTable;
pub use smoltcp::time::{Duration, Instant};
pub use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address, Ipv4Cidr, Ipv6Address};
pub use stack::Stack;

/// Maximum transport slots per interface, including retained closed transports.
pub const NET_SOCKETS: usize = 1024;
/// Socket IDs for live sockets, including sockets without a transport slot.
pub const NET_ENTRIES: usize = 2 * NET_SOCKETS;
/// Maximum prepared LISTEN sockets per listener and interface.
pub const LISTEN_POOL_MAX: usize = 8;
/// Maximum ingress frames processed per interface per poll round.
pub const RX_BUDGET: usize = 16;
/// Linux default automatic TCP/UDP port range, inclusive.
pub const EPHEMERAL: core::ops::RangeInclusive<u16> = 32768..=60999;
/// Linux maximum accept backlog.
pub const SOMAXCONN: usize = 4096;
/// Upper bound on the shared socket buffer budget.
pub const NET_MEM_CAP: usize = 32 * 1024 * 1024;

/// Linux's unsigned backlog comparison, including negative backlogs.
pub const fn clamp_backlog(backlog: i32) -> usize {
    if backlog as u32 > SOMAXCONN as u32 { SOMAXCONN } else { backlog as usize }
}

/// Socket payload bytes, independently allocated in each direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BufSizes {
    /// Receive payload capacity in bytes.
    pub rx: usize,
    /// Send payload capacity in bytes.
    pub tx: usize,
}
impl Default for BufSizes {
    fn default() -> Self {
        Self { rx: 65536, tx: 65536 }
    }
}

/// Reported SO_*BUF size and allocated payload size for an explicit option.
pub fn buffer_size(value: i32, receive: bool) -> (usize, usize) {
    let reported = (value as u32 as usize).min(212992).saturating_mul(2).max(if receive { 2304 } else { 4608 });
    (reported, reported / 2)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// The independently polled loopback and Ethernet interfaces.
pub enum IfaceId {
    /// IP loopback, including IPv4 127/8 and IPv6 ::1.
    Lo,
    /// The attached Ethernet interface.
    Eth0,
}
impl IfaceId {
    pub(crate) const ALL: [Self; 2] = [Self::Lo, Self::Eth0];
    pub(crate) fn from_index(i: usize) -> Self {
        Self::ALL[i]
    }
    pub(crate) fn index(self) -> usize {
        match self {
            Self::Lo => 0,
            Self::Eth0 => 1,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Socket identity including a wrapping generation to reject recycled slots.
pub struct SockId {
    idx: u32,
    generation: u32,
}
impl SockId {
    /// Stable slot index within this stack; the generation still determines identity.
    pub fn index(self) -> usize {
        self.idx as usize
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Address family fixed when the socket is opened.
pub enum Family {
    /// AF_INET.
    V4,
    /// AF_INET6, optionally IPv4-mapped unless V6Only is enabled.
    V6,
}
impl Family {
    /// Returns the address family of the supplied address, including mapped IPv6.
    pub fn of(addr: IpAddress) -> Self {
        if addr.is_ipv6() { Self::V6 } else { Self::V4 }
    }
}
#[derive(Clone, Copy, Debug)]
/// Operation whose preparation consumption is being queried.
pub enum PrepFor<'a> {
    /// An explicit local bind.
    Bind(&'a BindReq),
    /// A connection attempt to this peer.
    Connect(IpEndpoint),
    /// A send to the explicit destination or the connected peer.
    Send(Option<IpEndpoint>),
    /// A listen call with this requested backlog.
    Listen(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Supported transport and ICMP echo socket kinds.
pub enum Proto {
    /// TCP byte stream.
    Tcp,
    /// UDP datagrams.
    Udp,
    /// ICMP echo; raw sockets include a synthesized IPv4 header.
    IcmpEcho {
        /// Include raw echo requests and the synthesized IPv4 receive header.
        raw: bool,
    },
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Requested local address, port and sharing flags; family policy comes from the socket.
pub struct BindReq {
    /// Specific local address, or None for a wildcard.
    pub addr: Option<IpAddress>,
    /// Port or ping identifier; zero requests automatic allocation.
    pub port: u16,
    /// Permit compatible existing non-listening address reuse.
    pub reuse_addr: bool,
    /// Permit compatible port reuse on both owners.
    pub reuse_port: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Level readiness; event notifications are independently retained by `Stack::changed`.
pub struct Readiness {
    /// Reading, accepting or receiving EOF can complete.
    pub readable: bool,
    /// Sending or observing connect completion can complete.
    pub writable: bool,
    /// The connection is fully shut down.
    pub hup: bool,
    /// The peer’s write half is closed or reading was shut down.
    pub rdhup: bool,
    /// A pending socket error can be taken.
    pub error: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Directions disabled by socket shutdown.
pub enum Shut {
    /// Disable receives.
    Rd,
    /// Disable sends.
    Wr,
    /// Disable both directions.
    RdWr,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Supported socket option values; durations use caller-supplied monotonic time.
pub enum NetOpt {
    /// TCP Nagle disable flag.
    NoDelay(bool),
    /// Recorded and reported only; smoltcp has no cork implementation.
    Cork(bool),
    /// TCP keepalive enable flag.
    KeepAlive(bool),
    /// Idle duration before keepalive probes.
    KeepIdle(Duration),
    /// Interval between unanswered keepalive probes.
    KeepIntvl(Duration),
    /// Number of unanswered transmitted probes before failure.
    KeepCnt(u32),
    /// Close linger policy; zero requests a reset.
    Linger(Option<Duration>),
    /// IP hop limit, in the range 1–255.
    Ttl(u8),
    /// Recorded and reported only; smoltcp has no per-socket traffic class.
    Tos(u8),
    /// Permission to send UDP broadcast datagrams.
    Broadcast(bool),
    /// Restrict an IPv6 socket to IPv6 peers and bindings.
    V6Only(bool),
    /// User MSS: reported before connect, clamped to the path/peer afterwards.
    /// smoltcp cannot apply this limit to advertised MSS or actual segments.
    MaxSeg(u16),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Option selected for readback.
pub enum OptQuery {
    /// TCP Nagle disable flag.
    NoDelay,
    /// Recorded TCP cork value, with no transport effect.
    Cork,
    /// TCP keepalive enable flag.
    KeepAlive,
    /// Idle duration before keepalive probes.
    KeepIdle,
    /// Interval between unanswered keepalive probes.
    KeepIntvl,
    /// Number of unanswered transmitted probes before failure.
    KeepCnt,
    /// Close linger policy; zero requests a reset.
    Linger,
    /// IP hop limit, in the range 1–255.
    Ttl,
    /// Recorded traffic class, with no transport effect.
    Tos,
    /// Permission to send UDP broadcast datagrams.
    Broadcast,
    /// Restrict an IPv6 socket to IPv6 peers and bindings.
    V6Only,
    /// User MSS: reported before connect, clamped to the path/peer afterwards.
    /// smoltcp cannot apply this limit to advertised MSS or actual segments.
    MaxSeg,
}
/// A ticket is valid only for its socket generation and current queue head.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecvTicket {
    id: SockId,
    iface: IfaceId,
    sequence: u64,
    bytes: usize,
}
impl RecvTicket {
    /// Limits a TCP consume to the bytes successfully copied. Datagram
    /// tickets still consume the entire message, including on a copy fault.
    pub fn prefix(self, n: usize) -> Self {
        Self { bytes: self.bytes.min(n), ..self }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Copied length, original message length and a ticket for deferred consumption.
pub struct Received {
    /// Number of bytes placed in the supplied output buffer.
    pub copied: usize,
    /// Original datagram length, or copied stream length.
    pub full_len: usize,
    /// Datagram source in the socket’s family.
    pub from: Option<IpEndpoint>,
    /// Token for deferred consumption after a successful user copy.
    pub ticket: RecvTicket,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// DHCP change successfully applied to the Ethernet interface.
pub enum Lease {
    /// A newly applied lease, including at most three DNS servers.
    Configured {
        /// Leased local address and prefix.
        addr: Ipv4Cidr,
        /// Default gateway supplied by the server.
        router: Option<Ipv4Address>,
        /// Up to three resolver addresses.
        dns: [Option<Ipv4Address>; 3],
        /// Address of the DHCP server.
        server: Ipv4Address,
    },
    /// The previous lease has been removed.
    Deconfigured,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Initial Ethernet address configuration policy.
pub enum EthConfig {
    /// Acquire an IPv4 address through DHCP.
    Dhcp,
    /// Use an explicit address/prefix and optional default router.
    Static {
        /// Local IPv4 address and prefix.
        addr: Ipv4Cidr,
        /// Optional default gateway.
        router: Option<Ipv4Address>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Transport result; internal retry and signal cases have no direct errno mapping.
pub enum NetError {
    /// No progress is currently possible.
    WouldBlock,
    /// A TCP connect has started.
    InProgress,
    /// The TCP connect is still in progress.
    Already,
    /// The socket is already connected.
    IsConn,
    /// This operation requires an established connection.
    NotConn,
    /// The local port/address conflicts with another owner.
    AddrInUse,
    /// The local address or automatic port is unavailable.
    AddrNotAvail,
    /// A connection attempt was reset.
    ConnRefused,
    /// A completed attempt ended before connect completion was consumed.
    ConnAborted,
    /// The established transport was reset.
    ConnReset,
    /// The caller-driven connect or keepalive deadline expired.
    TimedOut,
    /// No TCP segment could leave before the connect deadline.
    HostUnreach,
    /// Send shutdown; the syscall layer must apply SIGPIPE policy and return EPIPE.
    Pipe,
    /// No supported route or family covers the destination.
    NetUnreach,
    /// The message exceeds the path or prepared-buffer bound.
    MsgSize,
    /// A fixed slot, preparation or memory budget is exhausted.
    NoBufs,
    /// The allocator could not prepare storage.
    NoMem,
    /// Invalid state, option, preparation kind or socket identity.
    Inval,
    /// The protocol does not support this operation.
    OpNotSupp,
    /// An unconnected datagram send has no destination.
    DestAddrReq,
    /// Broadcast permission is required.
    Acces,
    /// Internal retry result; restart from datagram_len rather than expose an errno.
    StaleTicket,
}
impl NetError {
    /// Linux errno, except internal retry tickets and SIGPIPE handling.
    /// Retry `StaleTicket` from `datagram_len`; handle `Pipe` by applying
    /// MSG_NOSIGNAL/SIGPIPE policy and returning EPIPE in the syscall layer.
    pub const fn to_errno(self) -> Option<u32> {
        Some(match self {
            Self::WouldBlock => 11,
            Self::InProgress => 115,
            Self::Already => 114,
            Self::IsConn => 106,
            Self::NotConn => 107,
            Self::AddrInUse => 98,
            Self::AddrNotAvail => 99,
            Self::ConnRefused => 111,
            Self::ConnAborted => 103,
            Self::ConnReset => 104,
            Self::TimedOut => 110,
            Self::HostUnreach => 113,
            Self::Pipe => return None,
            Self::NetUnreach => 101,
            Self::MsgSize => 90,
            Self::NoBufs => 105,
            Self::NoMem => 12,
            Self::Inval => 22,
            Self::OpNotSupp => 95,
            Self::DestAddrReq => 89,
            Self::Acces => 13,
            Self::StaleTicket => return None,
        })
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
/// Poll result telling the owner whether another round is immediately needed.
pub struct Round {
    /// Another immediate poll can process queued or budget-limited work.
    pub more: bool,
}

pub(crate) fn canonical(addr: IpAddress) -> IpAddress {
    match addr {
        IpAddress::Ipv6(v6) => v6.to_ipv4_mapped().map(IpAddress::Ipv4).unwrap_or(addr),
        _ => addr,
    }
}
pub(crate) trait IpFamily {
    fn is_ipv4(&self) -> bool;
    fn is_ipv6(&self) -> bool;
}
impl IpFamily for IpAddress {
    fn is_ipv4(&self) -> bool {
        matches!(self, Self::Ipv4(_))
    }
    fn is_ipv6(&self) -> bool {
        matches!(self, Self::Ipv6(_))
    }
}
pub(crate) fn loopback(addr: IpAddress) -> bool {
    match canonical(addr) {
        IpAddress::Ipv4(v4) => v4.octets()[0] == 127,
        IpAddress::Ipv6(v6) => v6 == Ipv6Address::LOCALHOST,
    }
}
pub(crate) fn reported(mut ep: IpEndpoint, v6: bool) -> IpEndpoint {
    if v6 && let IpAddress::Ipv4(v4) = ep.addr {
        ep.addr = IpAddress::Ipv6(v4.to_ipv6_mapped());
    }
    ep
}

pub(crate) fn family_ok(v6: bool, only: bool, addr: IpAddress) -> bool {
    if canonical(addr).is_ipv4() { !only } else { v6 }
}
pub(crate) fn loopback_addr(v6: bool) -> IpAddress {
    if v6 { IpAddress::Ipv6(Ipv6Address::LOCALHOST) } else { IpAddress::v4(127, 0, 0, 1) }
}

/// Selects the worker deadline from this round's device state. Unusable or
/// full Ethernet uses the deadline with past-due Ethernet egress filtered
/// out; that deadline still includes loopback and transport timers.
pub fn worker_deadline(usable: bool, full: bool, normal: Option<Instant>, blocked: Option<Instant>) -> Option<Instant> {
    if !usable || full { blocked } else { normal }
}
