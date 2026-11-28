use crate::device::{Adapter, IcmpSource, LoopDevice, OneFrame};
use crate::memory::{filled, reserved};
use crate::packet::{Tuple, TxTrace, packet_destination, tcp_packet, udp_packet};
use crate::port::Bound;
use crate::*;
use alloc::vec::Vec;
use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet, SocketStorage};
use smoltcp::phy::{Device, RxToken};
use smoltcp::socket::{dhcpv4, icmp, tcp, udp};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpCidr, IpListenEndpoint};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Fresh,
    Active,
    Listener(usize),
    Pool(SockId),
    Queued(SockId, u64),
    Orphan,
}
#[derive(Clone, Copy)]
struct Options {
    nodelay: bool,
    cork: bool,
    keepalive: bool,
    idle: Duration,
    interval: Duration,
    count: u32,
    linger: Option<Duration>,
    ttl: u8,
    tos: u8,
    broadcast: bool,
    v6: bool,
    v6only: bool,
    maxseg: u16,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            nodelay: false,
            cork: false,
            keepalive: false,
            idle: Duration::from_secs(7200),
            interval: Duration::from_secs(75),
            count: 9,
            linger: None,
            ttl: 64,
            tos: 0,
            broadcast: false,
            v6: false,
            v6only: false,
            maxseg: 0,
        }
    }
}
#[derive(Clone, Copy, Default)]
struct Head {
    len: usize,
    from: Option<IpEndpoint>,
    present: bool,
}
#[derive(Clone, Copy, Default)]
struct Snapshot {
    state: Option<tcp::State>,
    rx: usize,
    tx: usize,
    can_send: bool,
    tuple: Option<Tuple>,
}
struct Entry {
    id: SockId,
    proto: Proto,
    role: Role,
    bound: Option<Bound>,
    options: Options,
    handles: [Option<SocketHandle>; 2],
    charges: [Option<Charge>; 2],
    peer: Option<IpEndpoint>,
    error: Option<NetError>,
    rd: bool,
    wr: bool,
    connected: bool,
    iface: Option<IfaceId>,
    deadline: Option<Instant>,
    sent: bool,
    heads: [Head; 2],
    sequence: [u64; 2],
    next_rx: usize,
    icmp_head: [Option<Vec<u8>>; 2],
    connect_reported: bool,
    auto_bound: bool,
    bind_order: u64,
    remote_mss: u16,
    time_wait_until: Option<Instant>,
    time_wait_acked: bool,
    last_arrival: Instant,
    keep_deadline: Option<Instant>,
    // Closed sockets removed before a destructive smoltcp reset keep their
    // original buffers here. Storage for this enum is reserved with entries.
    retired: Option<tcp::Socket<'static>>,
    fin_wait_since: Option<Instant>,
    fin_received: bool,
    orphan_aborted: bool,
    keep_armed: Option<Instant>,
    keep_probes: u32,
    icmp_destinations: [Destinations; 2],
    raw_ident: Option<u16>,
}
// ICMP metadata in smoltcp contains only the source. Keep one destination
// alongside each of its eight receive slots, including queued alias traffic.
#[derive(Clone, Copy, Default)]
struct Destinations {
    slots: [Option<(IpAddress, bool)>; 8],
    head: usize,
    len: usize,
}
impl Destinations {
    fn push(&mut self, addr: IpAddress, deliver: bool) {
        assert!(self.len < self.slots.len());
        self.slots[(self.head + self.len) % self.slots.len()] = Some((addr, deliver));
        self.len += 1;
    }
    fn pop(&mut self) -> Option<(IpAddress, bool)> {
        if self.len == 0 {
            return None;
        }
        let addr = self.slots[self.head].take();
        self.head = (self.head + 1) % self.slots.len();
        self.len -= 1;
        addr
    }
}

// Linux v6.12 include/net/tcp.h:111-118 (TCP_SYN_RETRIES),
// 126-128 (TCP_TIMEWAIT_LEN/TCP_FIN_TIMEOUT). Orphan cleanup is bounded.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(127);
const ORPHAN_TIMEOUT: Duration = Duration::from_secs(60);
// Pinned smoltcp 0.14 socket/tcp.rs, Timer::CLOSE_DELAY (Linux uses 60 s).
const TIME_WAIT: Duration = Duration::from_secs(10);
const SYN_RECEIVED_TIMEOUT: Duration = Duration::from_secs(63);

impl Entry {
    // Connected datagrams report send capacity only on their default route.
    fn datagram_tx_iface(&self, i: usize) -> bool {
        self.peer.is_none() || self.iface == Some(IfaceId::from_index(i))
    }
    fn new(id: SockId, proto: Proto) -> Self {
        Self {
            id,
            proto,
            role: Role::Fresh,
            bound: None,
            options: Options::default(),
            handles: [None; 2],
            charges: [None, None],
            peer: None,
            error: None,
            rd: false,
            wr: false,
            connected: false,
            iface: None,
            deadline: None,
            sent: false,
            heads: [Head::default(); 2],
            sequence: [0; 2],
            next_rx: 0,
            icmp_head: [None, None],
            connect_reported: false,
            auto_bound: false,
            bind_order: 0,
            remote_mss: 536,
            time_wait_until: None,
            time_wait_acked: false,
            last_arrival: Instant::ZERO,
            keep_deadline: None,
            retired: None,
            fin_wait_since: None,
            fin_received: false,
            orphan_aborted: false,
            keep_armed: None,
            keep_probes: 0,
            icmp_destinations: [Destinations::default(); 2],
            raw_ident: None,
        }
    }
}
struct InterfaceState {
    iface: Interface,
    sockets: SocketSet<'static>,
    count: usize,
}
impl InterfaceState {
    fn add(&mut self, prep: ConnectPrep) -> Result<(SocketHandle, Charge), NetError> {
        if self.count == NET_SOCKETS {
            return Err(NetError::NoBufs);
        }
        let (handle, charge) = match prep {
            ConnectPrep::Tcp(p) => (self.sockets.add(p.socket), p.charge),
            ConnectPrep::Udp(p) => (self.sockets.add(p.socket), p.charge),
            ConnectPrep::Icmp(p) => (self.sockets.add(p.socket), p.charge),
        };
        self.count += 1;
        Ok((handle, charge))
    }
    fn remove(&mut self, handle: SocketHandle) {
        self.sockets.remove(handle);
        self.count -= 1;
    }
}

/// Two independently polled interfaces sharing Linux port and socket state.
pub struct Stack<D: NetDevice> {
    lo: LoopDevice,
    eth: Option<Adapter<D>>,
    interfaces: [Option<InterfaceState>; 2],
    eth_storage: Option<Vec<SocketStorage<'static>>>,
    trace_storage: Option<Vec<TxTrace>>,
    entries: Vec<Option<Entry>>,
    generations: Vec<u32>,
    live: Vec<usize>,
    ports: PortTable,
    dirty: Vec<Option<SockId>>,
    changes: Vec<SockId>,
    shortages: Vec<(SockId, IfaceId, usize)>,
    frame: Vec<u8>,
    empty: SocketSet<'static>,
    // Loopback alias ICMP sockets need a source override during egress.
    held_icmp: Vec<(usize, icmp::Socket<'static>)>,
    now: Instant,
    next_poll: Option<Instant>,
    needs_poll: bool,
    queue_order: u64,
    lease: Option<Lease>,
    dhcp: Option<SocketHandle>,
    pending_lease: Option<Lease>,
    gateway: Option<Ipv4Address>,
    snapshots: Vec<Snapshot>,
    bind_order: u64,
}
impl<D: NetDevice> Stack<D> {
    /// Allocates all fixed bookkeeping and loopback interface state before the stack lock is taken.
    pub fn try_new(lo: Loopback, now: Instant, seed: u64) -> Result<Self, NetError> {
        let mut lo = lo.inner;
        let mut config = Config::new(HardwareAddress::Ip);
        config.random_seed = seed;
        let mut iface = Interface::new(config, &mut lo, now);
        iface.update_ip_addrs(|a| {
            a.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).unwrap();
            a.push(IpCidr::new(IpAddress::Ipv6(Ipv6Address::LOCALHOST), 128)).unwrap();
        });
        iface.set_any_ip(true);
        let state =
            InterfaceState { iface, sockets: SocketSet::new(filled(NET_SOCKETS, || SocketStorage::EMPTY)?), count: 0 };
        Ok(Self {
            lo,
            eth: None,
            interfaces: [Some(state), None],
            eth_storage: Some(filled(NET_SOCKETS, || SocketStorage::EMPTY)?),
            trace_storage: Some(reserved(NET_SOCKETS)?),
            entries: filled(NET_ENTRIES, || None)?,
            generations: filled(NET_ENTRIES, || 0)?,
            live: reserved(NET_ENTRIES)?,
            ports: PortTable::try_new(NET_ENTRIES)?,
            dirty: filled(NET_ENTRIES, || None)?,
            changes: reserved(NET_ENTRIES)?,
            shortages: reserved(2 * NET_ENTRIES)?,
            frame: filled(65535, || 0)?,
            empty: SocketSet::new(filled(1, || SocketStorage::EMPTY)?),
            held_icmp: reserved(NET_SOCKETS)?,
            now,
            next_poll: None,
            needs_poll: false,
            queue_order: 0,
            lease: None,
            dhcp: None,
            pending_lease: None,
            gateway: None,
            snapshots: filled(NET_ENTRIES, Snapshot::default)?,
            bind_order: 0,
        })
    }
    /// Attaches the single Ethernet device using storage reserved at boot.
    pub fn attach_eth(&mut self, dev: D, config: EthConfig, now: Instant, seed: u64) -> Result<(), NetError> {
        assert!(self.eth.is_none(), "only one Ethernet device is supported");
        let mut adapter = Adapter::new(dev, self.trace_storage.take().unwrap());
        let mut cfg = Config::new(HardwareAddress::Ethernet(EthernetAddress(adapter.info().mac)));
        cfg.random_seed = seed;
        let iface = Interface::new(cfg, &mut adapter, now);
        self.interfaces[1] =
            Some(InterfaceState { iface, sockets: SocketSet::new(self.eth_storage.take().unwrap()), count: 0 });
        self.eth = Some(adapter);
        match config {
            EthConfig::Static { addr, router } => self.configure_eth(Some(addr), router)?,
            EthConfig::Dhcp => {
                let s = self.interfaces[1].as_mut().unwrap();
                self.dhcp = Some(s.sockets.add(dhcpv4::Socket::new()));
                s.count += 1;
            }
        }
        self.needs_poll = true;
        Ok(())
    }
    /// Applies a local address and default route, preserving unread TCP data on address changes.
    pub fn configure_eth(&mut self, addr: Option<Ipv4Cidr>, router: Option<Ipv4Address>) -> Result<(), NetError> {
        let s = self.interfaces[1].as_mut().ok_or(NetError::NetUnreach)?;
        let changed = s.iface.ip_addrs().first().copied() != addr.map(IpCidr::Ipv4);
        s.iface.routes_mut().remove_default_ipv4_route();
        if let Some(router) = router {
            s.iface.routes_mut().add_default_ipv4_route(router).map_err(|_| NetError::NoBufs)?;
        }
        if changed {
            self.retain_rx(1, false);
        }
        self.interfaces[1].as_mut().unwrap().iface.update_ip_addrs(|a| {
            a.clear();
            if let Some(addr) = addr {
                a.push(IpCidr::Ipv4(addr)).unwrap();
            }
        });
        self.gateway = router;
        self.needs_poll = true;
        Ok(())
    }
    fn entry(&self, id: SockId) -> Result<&Entry, NetError> {
        self.entries
            .get(id.index())
            .and_then(Option::as_ref)
            .filter(|e| e.id == id && e.role != Role::Orphan)
            .ok_or(NetError::Inval)
    }
    fn entry_mut(&mut self, id: SockId) -> Result<&mut Entry, NetError> {
        self.entries
            .get_mut(id.index())
            .and_then(Option::as_mut)
            .filter(|e| e.id == id && e.role != Role::Orphan)
            .ok_or(NetError::Inval)
    }
    fn mark(&mut self, id: SockId) {
        if self.dirty[id.index()].is_none() {
            self.dirty[id.index()] = Some(id);
        }
    }
    fn touch(&mut self, id: SockId) {
        self.mark(id);
        self.needs_poll = true;
    }
    /// Creates bookkeeping for the supplied address family and protocol.
    pub fn open(&mut self, family: Family, proto: Proto) -> Result<SockId, NetError> {
        let idx = self.entries.iter().enumerate().position(|(i, _)| self.slot_free(i)).ok_or(NetError::NoBufs)?;
        self.generations[idx] = self.generations[idx].wrapping_add(1);
        let id = SockId { idx: idx as u32, generation: self.generations[idx] };
        let mut e = Entry::new(id, proto);
        e.options.v6 = family == Family::V6;
        self.entries[idx] = Some(e);
        self.live.push(idx);
        Ok(id)
    }
    fn live_entries(&self) -> impl Iterator<Item = &Entry> {
        self.live.iter().map(|&i| self.entries[i].as_ref().unwrap())
    }
    fn unbound_req(&self, id: SockId) -> Result<Bound, NetError> {
        let e = self.entry(id)?;
        Ok(Bound::new(BindReq::default(), e.options.v6, e.options.v6only))
    }
    fn ip_mtu(&self, iface: IfaceId) -> usize {
        match iface {
            IfaceId::Lo => self.lo.capabilities().max_transmission_unit,
            IfaceId::Eth0 => self.eth.as_ref().unwrap().info().mtu.min(ETH_FRAME_MAX - 14),
        }
    }
    fn broadcast(&self, addr: IpAddress, iface: IfaceId) -> bool {
        let IpAddress::Ipv4(addr) = addr else {
            return false;
        };
        addr == Ipv4Address::BROADCAST
            || self.interfaces[iface.index()]
                .as_ref()
                .unwrap()
                .iface
                .ip_addrs()
                .iter()
                .any(|cidr| matches!(cidr, IpCidr::Ipv4(c) if c.broadcast() == Some(addr)))
    }
    fn resolve(&self, id: SockId, mut remote: IpEndpoint) -> Result<(IpEndpoint, IfaceId), NetError> {
        let e = self.entry(id)?;
        if remote.port == 0 && !matches!(e.proto, Proto::IcmpEcho { .. }) {
            return Err(NetError::Inval);
        }
        remote.addr = canonical(remote.addr);
        if remote.addr.is_unspecified() {
            return Err(NetError::AddrNotAvail);
        }
        if !family_ok(e.options.v6, e.options.v6only, remote.addr) {
            return Err(NetError::NetUnreach);
        }
        let iface = self.route(remote.addr)?;
        if e.bound.is_some_and(|b| b.addr.is_some()) && e.iface.is_some_and(|f| f != iface) {
            return Err(NetError::NetUnreach);
        }
        Ok((remote, iface))
    }
    fn route(&self, addr: IpAddress) -> Result<IfaceId, NetError> {
        if loopback(addr) {
            return Ok(IfaceId::Lo);
        }
        if !canonical(addr).is_ipv4() || self.interfaces[1].as_ref().is_none_or(|s| s.iface.ip_addrs().is_empty()) {
            return Err(NetError::NetUnreach);
        }
        if self.gateway.is_none()
            && !self.interfaces[1]
                .as_ref()
                .unwrap()
                .iface
                .ip_addrs()
                .iter()
                .any(|cidr| cidr.contains_addr(&canonical(addr)))
            && canonical(addr) != IpAddress::Ipv4(Ipv4Address::BROADCAST)
        {
            return Err(NetError::NetUnreach);
        }
        Ok(IfaceId::Eth0)
    }
    fn local(&self, addr: IpAddress) -> bool {
        loopback(addr)
            || self.interfaces[1]
                .as_ref()
                .is_some_and(|s| s.iface.ip_addrs().iter().any(|c| c.address() == canonical(addr)))
    }
    fn coverage(&self, addr: Option<IpAddress>) -> [bool; 2] {
        match addr.map(canonical).filter(|a| !a.is_unspecified()) {
            Some(addr) => [loopback(addr), !loopback(addr)],
            None => [true, self.eth.is_some()],
        }
    }
    fn coverage_count(&self, addr: Option<IpAddress>) -> usize {
        self.coverage(addr).into_iter().filter(|v| *v).count()
    }
    fn iface_addr(&self, iface: IfaceId, v6: bool) -> Option<IpAddress> {
        if iface == IfaceId::Lo {
            Some(loopback_addr(v6))
        } else {
            self.interfaces[iface.index()].as_ref()?.iface.ip_addrs().first().map(|c| c.address())
        }
    }
    fn slot_free(&self, idx: usize) -> bool {
        self.entries[idx].is_none() && self.dirty[idx].is_none()
    }
    /// Exact preparations consumed on success. Re-call under the same stack
    /// lock hold as the operation after preparing buffers outside the lock;
    /// another caller may have changed coverage or filled a listener pool.
    pub fn preps_needed(&self, id: SockId, operation: PrepFor<'_>) -> usize {
        let Ok(e) = self.entry(id) else {
            return 0;
        };
        match operation {
            PrepFor::Bind(req) => {
                if e.proto == Proto::Tcp || e.bound.is_some() {
                    0
                } else {
                    self.coverage_count(req.addr)
                }
            }
            PrepFor::Listen(backlog) => {
                if e.proto != Proto::Tcp {
                    0
                } else {
                    self.pool_needed(id, backlog).iter().sum()
                }
            }
            PrepFor::Connect(ep) => {
                self.resolve(id, ep).ok().map_or(0, |(_, i)| usize::from(e.handles[i.index()].is_none()))
            }
            PrepFor::Send(ep) => {
                let Some(ep) = ep.or(e.peer) else {
                    return 0;
                };
                if e.proto == Proto::Tcp {
                    return 0;
                }
                if self.resolve(id, ep).is_err() {
                    return 0;
                }
                if e.bound.is_none() {
                    self.coverage_count(None)
                } else {
                    self.resolve(id, ep).ok().map_or(0, |(_, i)| usize::from(e.handles[i.index()].is_none()))
                }
            }
        }
    }
    /// Reserves a local port and installs prepared datagram slots atomically; expires TIME-WAIT using now.
    pub fn bind(&mut self, id: SockId, req: BindReq, mut preps: Preps<'_>, now: Instant) -> Result<(), NetError> {
        self.ports.expire(now);
        let e = self.entry(id)?;
        let mut req = Bound::new(req, e.options.v6, e.options.v6only);
        if e.bound.is_some() {
            return Err(NetError::Inval);
        }
        if let Some(addr) = req.addr {
            if req.v6only && canonical(addr).is_ipv4() || !req.v6 && canonical(addr).is_ipv6() {
                return Err(NetError::Inval);
            }
            if !addr.is_unspecified() && !self.local(addr) {
                return Err(NetError::AddrNotAvail);
            }
            req.addr = Some(canonical(addr)).filter(|a| !a.is_unspecified());
        }
        let proto = e.proto;
        let coverage = self.coverage(req.addr);
        let mut prepared: [Option<ConnectPrep>; 2] = [None, None];
        if proto != Proto::Tcp {
            for i in 0..2 {
                if coverage[i] {
                    if self.interfaces[i].as_ref().is_none_or(|s| s.count == NET_SOCKETS) {
                        return Err(NetError::NoBufs);
                    }
                    let p = preps.next().ok_or(NetError::NoBufs)?;
                    if !matches!(
                        (&p, proto),
                        (ConnectPrep::Udp(_), Proto::Udp) | (ConnectPrep::Icmp(_), Proto::IcmpEcho { .. })
                    ) {
                        return Err(NetError::Inval);
                    }
                    prepared[i] = Some(p);
                }
            }
        }
        let bound = self.ports.bind(id, proto, req)?;
        for (i, p) in prepared.iter_mut().enumerate() {
            if let Some(p) = p {
                let ok = configure_datagram(p, proto, bound, i, self.entry(id)?.options.ttl).is_ok();
                if !ok {
                    self.ports.release(id);
                    return Err(NetError::Inval);
                }
            }
        }
        for (i, p) in prepared.iter_mut().enumerate() {
            if let Some(p) = p.take() {
                self.install_datagram(id, i, p)?;
            }
        }
        self.bind_order += 1;
        let order = self.bind_order;
        let e = self.entry_mut(id)?;
        e.bound = Some(bound);
        e.bind_order = order;
        e.options.v6 = bound.v6;
        e.options.v6only = bound.v6only;
        e.iface = bound_iface(Some(bound));
        self.touch(id);
        Ok(())
    }
    fn install_datagram(&mut self, id: SockId, i: usize, mut prep: ConnectPrep) -> Result<(), NetError> {
        if self.interfaces[i].as_ref().is_none_or(|s| s.count == NET_SOCKETS) {
            return Err(NetError::NoBufs);
        }
        let head = if let ConnectPrep::Icmp(ref mut p) = prep {
            if let Some(ident) = self.entry(id)?.raw_ident {
                p.socket.bind(icmp::Endpoint::Ident(ident)).map_err(|_| NetError::Inval)?;
            }
            Some(core::mem::take(&mut p.head))
        } else {
            None
        };
        let (h, c) = self.interfaces[i].as_mut().unwrap().add(prep)?;
        let e = self.entry_mut(id)?;
        e.handles[i] = Some(h);
        e.charges[i] = Some(c);
        e.icmp_head[i] = head;
        Ok(())
    }
    fn missing_datagram(&mut self, id: SockId, i: usize, mut prep: ConnectPrep) -> Result<(), NetError> {
        let e = self.entry(id)?;
        let req = e.bound.ok_or(NetError::Inval)?;
        configure_datagram(&mut prep, e.proto, req, i, e.options.ttl)?;
        self.install_datagram(id, i, prep)
    }
    /// Starts TCP nonblocking connection or selects a datagram peer; wrong preparation kinds give Inval.
    pub fn connect(
        &mut self,
        id: SockId,
        mut remote: IpEndpoint,
        prep: Option<ConnectPrep>,
        now: Instant,
    ) -> Result<(), NetError> {
        self.ports.expire(now);
        let e = self.entry(id)?;
        if e.proto == Proto::Tcp {
            if e.connected && (e.connect_reported || self.tcp_ref(id)?.state() != tcp::State::Closed) {
                if e.connect_reported {
                    return Err(NetError::IsConn);
                }
                self.entry_mut(id)?.connect_reported = true;
                return Ok(());
            }
            if e.peer.is_some() {
                if self.tcp_ref(id)?.state() == tcp::State::Closed {
                    let error = e.error.unwrap_or(NetError::ConnAborted);
                    self.drop_tcp_slot(id)?;
                    self.touch(id);
                    return Err(error);
                }
                return Err(NetError::Already);
            }
            if matches!(e.role, Role::Listener(_)) {
                return Err(NetError::IsConn);
            }
        }
        if prep.as_ref().is_some_and(|p| {
            !matches!(
                (p, e.proto),
                (ConnectPrep::Tcp(_), Proto::Tcp)
                    | (ConnectPrep::Udp(_), Proto::Udp)
                    | (ConnectPrep::Icmp(_), Proto::IcmpEcho { .. })
            )
        }) {
            return Err(NetError::Inval);
        }
        let (resolved, iface) = self.resolve(id, remote)?;
        remote = resolved;
        let i = iface.index();
        let v6 = e.options.v6;
        let proto = e.proto;
        let bound = e.bound;
        if proto == Proto::Tcp {
            let mut p = match prep {
                Some(ConnectPrep::Tcp(p)) => p,
                Some(_) => return Err(NetError::Inval),
                None => return Err(NetError::NoBufs),
            };
            if self.interfaces[i].as_ref().unwrap().count == NET_SOCKETS {
                return Err(NetError::NoBufs);
            }
            let req = match bound {
                Some(b) => b,
                None => self.ports.bind(id, proto, Bound::new(BindReq::default(), v6, e.options.v6only))?,
            };
            let s = self.interfaces[i].as_mut().unwrap();
            let endpoint = IpListenEndpoint { addr: req.addr, port: req.port };
            if p.socket.connect(s.iface.context(), remote, endpoint).is_err() {
                if bound.is_none() {
                    self.ports.release(id);
                }
                return Err(NetError::AddrNotAvail);
            }
            // Resolve the actual local address before checking the tuple, but
            // do not install or transmit the prepared SYN until it is unique.
            let tuple = Tuple { local: p.socket.local_endpoint().unwrap(), remote };
            if self.tcp_tuple_in_use(i, tuple, now) {
                if bound.is_none() {
                    self.ports.release(id);
                }
                return Err(NetError::AddrNotAvail);
            }
            let (h, c) = self.interfaces[i].as_mut().unwrap().add(ConnectPrep::Tcp(p))?;
            let e = self.entry_mut(id)?;
            e.handles[i] = Some(h);
            e.charges[i] = Some(c);
            e.bound = Some(req);
            e.last_arrival = now;
            e.rd = false;
            e.wr = false;
            e.error = None;
            e.auto_bound = bound.is_none();
            e.peer = Some(remote);
            e.iface = Some(iface);
            e.options.v6 = v6;
            e.role = Role::Active;
            e.deadline = Some(now + CONNECT_TIMEOUT);
            self.apply_tcp_options(id);
            self.touch(id);
            return Err(NetError::InProgress);
        }
        if bound.is_none() {
            // Connecting selects one interface, unlike a wildcard bind.
            let local = self.iface_addr(iface, remote.addr.is_ipv6()).ok_or(NetError::AddrNotAvail)?;
            let req = BindReq { addr: Some(local), ..BindReq::default() };
            match prep {
                Some(ConnectPrep::Udp(p)) => self.bind(id, req, Preps::Udp(&mut core::iter::once(p)), now)?,
                Some(ConnectPrep::Icmp(p)) => self.bind(id, req, Preps::Icmp(&mut core::iter::once(p)), now)?,
                Some(_) => return Err(NetError::Inval),
                None => return Err(NetError::NoBufs),
            }
        } else if self.entry(id)?.handles[i].is_none() {
            self.missing_datagram(id, i, prep.ok_or(NetError::NoBufs)?)?;
        }
        let e = self.entry_mut(id)?;
        e.peer = Some(remote);
        e.iface = Some(iface);
        e.connected = true;
        e.options.v6 = v6;
        self.filter_datagrams(id);
        self.touch(id);
        Ok(())
    }
    fn tcp_tuple_in_use(&self, i: usize, tuple: Tuple, now: Instant) -> bool {
        self.ports.tuple_reserved(tuple, now)
            || self.live_entries().any(|e| {
                e.proto == Proto::Tcp
                    && e.handles[i].is_some_and(|h| {
                        let s = self.interfaces[i].as_ref().unwrap().sockets.get::<tcp::Socket>(h);
                        !matches!(s.state(), tcp::State::Closed | tcp::State::Listen)
                            && !(s.state() == tcp::State::TimeWait && e.time_wait_until.is_some_and(|t| now >= t))
                            && s.local_endpoint() == Some(tuple.local)
                            && s.remote_endpoint() == Some(tuple.remote)
                    })
            })
    }
    /// Clears a UDP peer filter while retaining its local binding.
    pub fn disconnect(&mut self, id: SockId) -> Result<(), NetError> {
        let e = self.entry_mut(id)?;
        if e.proto != Proto::Udp {
            return Err(NetError::OpNotSupp);
        }
        e.peer = None;
        e.connected = false;
        e.iface = bound_iface(e.bound);
        self.touch(id);
        Ok(())
    }
}

fn listen_endpoint(req: Bound, iface: usize) -> IpListenEndpoint {
    IpListenEndpoint {
        addr: req
            .addr
            .or_else(|| (iface == 0 && req.v6 && req.v6only).then_some(IpAddress::Ipv6(Ipv6Address::LOCALHOST))),
        port: req.port,
    }
}

impl<D: NetDevice> Stack<D> {
    fn snapshot(&mut self, i: usize) {
        let Some(s) = self.interfaces[i].as_mut() else {
            return;
        };
        for &idx in &self.live {
            self.snapshots[idx] = Snapshot::default();
            let e = self.entries[idx].as_mut().unwrap();
            let Some(h) = e.handles[i] else {
                continue;
            };
            if e.proto == Proto::Tcp {
                let t = s.sockets.get_mut::<tcp::Socket>(h);
                self.snapshots[idx] = Snapshot {
                    state: Some(t.state()),
                    rx: t.recv_queue(),
                    tx: t.send_queue(),
                    can_send: t.can_send(),
                    tuple: t.local_endpoint().zip(t.remote_endpoint()).map(|(local, remote)| Tuple { local, remote }),
                };
            } else if e.proto == Proto::Udp {
                let u = s.sockets.get::<udp::Socket>(h);
                self.snapshots[idx].rx = u.recv_queue();
                self.snapshots[idx].tx = u.send_queue();
                self.snapshots[idx].can_send = u.can_send();
            } else {
                let socket = s.sockets.get::<icmp::Socket>(h);
                self.snapshots[idx].tx = socket.send_queue();
                self.snapshots[idx].can_send = socket.can_send();
                self.snapshots[idx].rx = socket.recv_queue();
            }
        }
    }
    fn observe(
        &mut self,
        i: usize,
        packet: Option<&crate::packet::TcpPacketInfo>,
        destination: Option<IpAddress>,
        arrival: bool,
    ) {
        // Select one ping owner at arrival, before any cache drains a queue.
        // Raw sockets remain independent recipients. Record the decision with
        // the destination so later binds cannot steal already queued replies.
        let ping_owner = destination.and_then(|dst| self.ping_recipient(i, dst));
        for pos in (0..self.live.len()).rev() {
            let idx = self.live[pos];
            let e = self.entries[idx].as_ref().unwrap();
            let Some(h) = e.handles[i] else {
                continue;
            };
            let id = e.id;
            if e.proto != Proto::Tcp {
                if matches!(e.proto, Proto::IcmpEcho { .. })
                    && let Some(dst) = destination
                    && self.interfaces[i].as_ref().unwrap().sockets.get::<icmp::Socket>(h).recv_queue()
                        > self.snapshots[idx].rx
                {
                    let deliver = e.proto == (Proto::IcmpEcho { raw: true }) || ping_owner == Some(id);
                    self.entries[idx].as_mut().unwrap().icmp_destinations[i].push(dst, deliver);
                }
                self.filter_datagrams(id);
                let e = self.entries[idx].as_ref().unwrap();
                let sockets = &self.interfaces[i].as_ref().unwrap().sockets;
                let (queued, can_send) = if e.proto == Proto::Udp {
                    let socket = sockets.get::<udp::Socket>(h);
                    (socket.send_queue(), socket.can_send())
                } else {
                    let socket = sockets.get::<icmp::Socket>(h);
                    (socket.send_queue(), socket.can_send())
                };
                // Payload occupancy alone misses zero-length datagrams that
                // exhaust the packet metadata ring.
                let writable = e.datagram_tx_iface(i)
                    && (queued < self.snapshots[idx].tx || can_send && !self.snapshots[idx].can_send);
                if writable || arrival && e.heads[i].present {
                    self.mark(id);
                }
                continue;
            }
            let before = self.snapshots[idx];
            let e = self.entries[idx].as_mut().unwrap();
            let s = self.interfaces[i].as_mut().unwrap().sockets.get_mut::<tcp::Socket>(h);
            let state = s.state();
            if state == tcp::State::TimeWait && e.time_wait_until.is_none() {
                e.time_wait_until = Some(self.now + TIME_WAIT);
            }
            let changed = before.state != Some(state) || s.recv_queue() > before.rx || s.send_queue() < before.tx;
            let arrived = packet.is_some_and(|p| Some(p.tuple.reversed()) == before.tuple);
            let reset = arrived && packet.unwrap().rst;
            if arrived {
                e.last_arrival = self.now;
                e.keep_deadline = None;
                e.keep_probes = 0;
                e.keep_armed = s.keep_alive().map(|d| self.now + d);
                if e.role == Role::Orphan
                    && s.send_queue() < before.tx
                    && matches!(state, tcp::State::FinWait1 | tcp::State::Closing | tcp::State::LastAck)
                {
                    e.deadline = Some(self.now + ORPHAN_TIMEOUT);
                }
            }
            e.fin_received |= matches!(
                state,
                tcp::State::CloseWait | tcp::State::LastAck | tcp::State::Closing | tcp::State::TimeWait
            );
            if state == tcp::State::FinWait2 && before.state != Some(state) {
                e.fin_wait_since = Some(self.now);
                if e.role == Role::Orphan {
                    e.deadline = Some(self.now + ORPHAN_TIMEOUT);
                }
            }
            if state == tcp::State::Closed
                && before.state != Some(tcp::State::Closed)
                && e.role != Role::Orphan
                && !matches!(e.role, Role::Pool(_))
            {
                e.error = match (reset, before.state) {
                    (_, Some(tcp::State::TimeWait)) | (false, Some(tcp::State::LastAck)) => None,
                    (true, Some(tcp::State::SynSent)) => Some(NetError::ConnRefused),
                    (true, Some(tcp::State::CloseWait)) => Some(NetError::Pipe),
                    _ => Some(NetError::ConnReset),
                };
                e.connected = false;
                e.keep_armed = None;
                e.keep_probes = 0;
                s.set_keep_alive(None);
                e.wr = true;
                e.deadline = None;
                e.keep_deadline = None;
            }
            if matches!(state, tcp::State::Established | tcp::State::CloseWait) && e.role != Role::Orphan {
                e.connected = true;
                e.deadline = None;
                e.peer = s.remote_endpoint();
                s.set_timeout(None);
                if before.state != Some(state) {
                    e.last_arrival = self.now;
                    s.set_keep_alive(e.options.keepalive.then_some(e.options.idle));
                }
                if e.options.keepalive {
                    if arrived || before.state != Some(state) {
                        s.set_keep_alive(Some(e.options.idle));
                    }
                    if e.keep_armed.is_none() {
                        e.keep_armed = Some(self.now);
                    }
                }
                if let Role::Pool(parent) = e.role {
                    self.queue_order = self.queue_order.wrapping_add(1);
                    e.role = Role::Queued(parent, self.queue_order);
                    self.dirty[parent.index()].get_or_insert(parent);
                }
            }
            if e.rd {
                discard_rx(s);
            }
            let failed_pool = matches!(e.role, Role::Pool(_))
                && (state == tcp::State::Closed
                    || state == tcp::State::Listen && before.state != Some(tcp::State::Listen));
            if changed {
                self.mark(id);
            }
            if failed_pool {
                self.remove(id, false, self.now);
            }
        }
    }
    fn ping_recipient(&self, i: usize, destination: IpAddress) -> Option<SockId> {
        let sockets = &self.interfaces[i].as_ref()?.sockets;
        // Only checksum-validated echo delivery grows an Ident socket's RX.
        // Derive the identifier from such a socket rather than parsing twice.
        let ident = self
            .live_entries()
            .find(|e| {
                e.proto == (Proto::IcmpEcho { raw: false })
                    && e.handles[i]
                        .is_some_and(|h| sockets.get::<icmp::Socket>(h).recv_queue() > self.snapshots[e.id.index()].rx)
            })?
            .bound?
            .port;
        self.live_entries()
            .filter(|e| {
                e.proto == (Proto::IcmpEcho { raw: false })
                    && e.handles[i].is_some()
                    && e.bound.is_some_and(|b| b.port == ident && b.addr.is_none_or(|a| a == destination))
                    && family_ok(e.options.v6, e.options.v6only, destination)
            })
            .max_by_key(|e| e.bind_order)
            .map(|e| e.id)
    }
    /// Select a TCP owner before smoltcp's first-match demultiplexing. The
    /// one-slot set prevents a wildcard pool stealing a tuple or a later
    /// SO_REUSEPORT listener receiving a first-bound listener's excess SYN.
    fn syn_action(&self, i: usize, packet: Option<&crate::packet::TcpPacketInfo>) -> Syn {
        let Some(p) = packet else {
            return Syn::Deliver(None);
        };
        let tuple = p.tuple.reversed();
        let sockets = &self.interfaces[i].as_ref().unwrap().sockets;
        if let Some(e) = self.live_entries().find(|e| {
            e.proto == Proto::Tcp
                && e.handles[i].is_some_and(|h| {
                    let s = sockets.get::<tcp::Socket>(h);
                    !matches!(s.state(), tcp::State::Closed | tcp::State::Listen)
                        && s.local_endpoint() == Some(tuple.local)
                        && s.remote_endpoint() == Some(tuple.remote)
                })
        }) {
            return Syn::Deliver(Some(e.id.index()));
        }
        if !p.syn {
            return Syn::Deliver(None);
        }
        let listener = self
            .live_entries()
            .filter(|e| {
                if !matches!(e.role, Role::Listener(_)) {
                    return false;
                }
                let req = e.bound.unwrap();
                req.port == tuple.local.port
                    && self.coverage(req.addr)[i]
                    && req.addr.is_none_or(|a| a == tuple.local.addr)
                    && family_ok(req.v6, req.v6only, tuple.local.addr)
            })
            .min_by_key(|e| (e.bound.unwrap().addr.is_none(), e.bind_order));
        let Some(e) = listener else {
            return Syn::NoOwner;
        };
        let Role::Listener(backlog) = e.role else {
            unreachable!();
        };
        let in_flight = self
            .live_entries()
            .filter(|child| {
                child.role == Role::Pool(e.id)
                    && child.handles.iter().enumerate().any(|(j, h)| {
                        h.is_some_and(|h| {
                            self.interfaces[j].as_ref().unwrap().sockets.get::<tcp::Socket>(h).state()
                                == tcp::State::SynReceived
                        })
                    })
            })
            .count();
        // Reserve an accept slot for each admitted handshake, across both
        // interfaces, so simultaneous final ACKs cannot overfill the queue.
        if self.queued(e.id) + in_flight > backlog {
            return Syn::Drop;
        }
        let free = self.live_entries().find(|child| {
            child.role == Role::Pool(e.id)
                && child.handles[i].is_some_and(|h| sockets.get::<tcp::Socket>(h).state() == tcp::State::Listen)
        });
        free.map_or(Syn::Drop, |e| Syn::Deliver(Some(e.id.index())))
    }
    fn deadlines(&mut self) {
        for e in self.entries.iter_mut().flatten() {
            let expired = e.deadline.is_some_and(|d| self.now >= d);
            let keep_expired = e.keep_deadline.is_some_and(|d| self.now >= d);
            if expired {
                e.deadline = None;
            }
            if keep_expired {
                e.keep_deadline = None;
            }
            let Some(i) = e.iface.map(IfaceId::index) else {
                continue;
            };
            let Some(h) = e.handles[i] else {
                continue;
            };
            if e.proto != Proto::Tcp {
                continue;
            }
            let s = self.interfaces[i].as_mut().unwrap().sockets.get_mut::<tcp::Socket>(h);
            if expired && e.role == Role::Orphan {
                if s.state() != tcp::State::Closed {
                    s.abort();
                }
                e.orphan_aborted = true;
                e.keep_deadline = None;
                e.keep_armed = None;
                continue;
            }
            if s.state() == tcp::State::Closed {
                e.keep_deadline = None;
                e.keep_armed = None;
                continue;
            }
            let keep_state = matches!(s.state(), tcp::State::Established | tcp::State::CloseWait);
            if !keep_state {
                e.keep_deadline = None;
                e.keep_armed = None;
            }
            if keep_expired && keep_state && e.keep_probes >= e.options.count {
                // abort() preserves smoltcp's receive buffer.
                s.abort();
                s.set_keep_alive(None);
                e.connected = false;
                e.keep_armed = None;
                e.error = Some(NetError::TimedOut);
                e.wr = true;
                self.dirty[e.id.index()].get_or_insert(e.id);
            } else if expired && s.state() == tcp::State::SynSent {
                s.abort();
                e.error = Some(if i == 1 && !e.sent { NetError::HostUnreach } else { NetError::TimedOut });
                e.rd = true;
                e.wr = true;
                self.dirty[e.id.index()].get_or_insert(e.id);
            }
            // set_keep_alive cannot re-arm an existing timer. Change the
            // interval before its armed probe leaves so dispatch rewinds it
            // using KEEPINTVL, without inventing probes during a late poll.
            if e.options.keepalive && keep_state && e.keep_armed.is_some_and(|d| self.now >= d) {
                s.set_keep_alive(Some(e.options.interval));
            }
        }
    }
    /// Move a socket out of polling before smoltcp would erase its RX queue.
    /// abort() changes its state without clearing either buffer; because the
    /// socket is no longer polled, no reset is emitted. Its charge and slot
    /// reservation remain until close or reconnect. No receive bytes copy.
    fn retain_rx(&mut self, i: usize, expiring_only: bool) {
        if self.interfaces[i].is_none() {
            return;
        }
        for pos in (0..self.live.len()).rev() {
            let idx = self.live[pos];
            let e = self.entries[idx].as_ref().unwrap();
            if e.proto != Proto::Tcp || expiring_only && !e.time_wait_until.is_some_and(|d| self.now >= d) {
                continue;
            }
            let Some(h) = e.handles[i] else {
                continue;
            };
            let state = self.interfaces[i].as_mut().unwrap();
            let socket = state.sockets.get::<tcp::Socket>(h);
            let before = socket.state();
            if expiring_only && before != tcp::State::TimeWait || before == tcp::State::Listen {
                continue;
            }
            if e.role == Role::Orphan || matches!(e.role, Role::Pool(_)) {
                let id = e.id;
                self.remove(id, false, self.now);
                continue;
            }
            let smoltcp::socket::Socket::Tcp(mut socket) = state.sockets.remove(h) else { unreachable!() };
            socket.abort();
            socket.set_keep_alive(None);
            let e = self.entries[idx].as_mut().unwrap();
            e.retired = Some(socket);
            e.handles[i] = None;
            e.connected = false;
            e.wr = true;
            e.deadline = None;
            e.keep_deadline = None;
            e.keep_armed = None;
            e.keep_probes = 0;
            if !matches!(before, tcp::State::Closed | tcp::State::TimeWait | tcp::State::LastAck) {
                e.error = Some(NetError::ConnReset);
            }
            self.dirty[idx].get_or_insert(e.id);
        }
    }
    /// Effective local receive address, including a wildcard connect's source.
    fn udp_local_addr(&self, e: &Entry) -> Option<IpAddress> {
        e.bound
            .and_then(|b| b.addr)
            .or_else(|| e.peer.and_then(|p| e.iface.and_then(|i| self.iface_addr(i, p.addr.is_ipv6()))))
    }
    fn udp_accepts(&self, e: &Entry, i: usize, tuple: Tuple) -> bool {
        e.proto == Proto::Udp
            && e.handles[i].is_some()
            && e.bound.is_some_and(|b| b.port == tuple.local.port)
            && self.udp_local_addr(e).is_none_or(|a| a == tuple.local.addr)
            && family_ok(e.options.v6, e.options.v6only, tuple.local.addr)
            && e.peer.is_none_or(|p| p == tuple.remote)
            && (e.peer.is_none() || e.iface.is_none_or(|iface| iface.index() == i))
    }
    fn udp_action(&self, i: usize, incoming: Tuple) -> Syn {
        let tuple = incoming.reversed();
        if i == 1 && self.dhcp.is_some() && incoming.local.port == 67 && incoming.remote.port == 68 {
            return Syn::Deliver(None); // smoltcp handles DHCP before UDP sockets
        }
        self.live_entries()
            .filter(|e| self.udp_accepts(e, i, tuple))
            // Linux v6.12 __udp{4,6}_lib_lookup tries the exact local
            // address before the wildcard bucket; compute_score then
            // prefers a matching connected peer within that bucket.
            .min_by_key(|e| (self.udp_local_addr(e).is_none(), e.peer.is_none(), e.bind_order))
            .map_or(Syn::NoOwner, |e| Syn::Deliver(Some(e.id.index())))
    }
    fn ingress_frame(&mut self, i: usize, len: usize, now: Instant) {
        let packet = tcp_packet(&self.frame[..len], i == 1);
        let destination = packet_destination(&self.frame[..len], i == 1);
        let action = if let Some(tuple) = udp_packet(&self.frame[..len], i == 1) {
            let action = self.udp_action(i, tuple);
            if i == 1 && self.broadcast(tuple.remote.addr, IfaceId::Eth0) && !matches!(action, Syn::Deliver(None)) {
                // smoltcp returns after its first UDP match, even for a
                // broadcast. Replay through each eligible transport's
                // one-slot set; no new buffers or receive charges are needed.
                self.snapshot(i);
                for pos in 0..self.live.len() {
                    let idx = self.live[pos];
                    if self.udp_accepts(self.entries[idx].as_ref().unwrap(), i, tuple.reversed()) {
                        self.deliver_transport(i, len, now, Syn::Deliver(Some(idx)), None);
                    }
                }
                self.observe(i, None, destination, true);
                self.note_sent(i);
                return;
            }
            action
        } else {
            self.syn_action(i, packet.as_ref())
        };
        if matches!(action, Syn::Drop) {
            return;
        }
        self.snapshot(i);
        self.deliver_transport(i, len, now, action, packet.as_ref());
        self.observe(i, packet.as_ref(), destination, true);
        self.note_sent(i);
    }
    fn deliver_transport(
        &mut self,
        i: usize,
        len: usize,
        now: Instant,
        action: Syn,
        packet: Option<&crate::packet::TcpPacketInfo>,
    ) {
        let selected = match action {
            Syn::Deliver(s) => s,
            Syn::NoOwner => None,
            Syn::Drop => return,
        };
        let state = self.interfaces[i].as_mut().unwrap();
        let temporary = selected.map(|idx| {
            let h = self.entries[idx].as_ref().unwrap().handles[i].unwrap();
            self.empty.add(state.sockets.remove(h))
        });
        let sockets =
            if matches!(action, Syn::NoOwner) || temporary.is_some() { &mut self.empty } else { &mut state.sockets };
        if i == 0 {
            let mut device = OneFrame { device: &mut self.lo, frame: Some(&self.frame[..len]) };
            state.iface.poll_ingress_single(now, &mut device, sockets);
        } else {
            let mut device = OneFrame { device: self.eth.as_mut().unwrap(), frame: Some(&self.frame[..len]) };
            state.iface.poll_ingress_single(now, &mut device, sockets);
        }
        if let Some(h) = temporary {
            let h = state.sockets.add(self.empty.remove(h));
            self.entries[selected.unwrap()].as_mut().unwrap().handles[i] = Some(h);
            if let Some(mss) = packet.as_ref().and_then(|p| p.mss)
                && matches!(self.snapshots[selected.unwrap()].state, Some(tcp::State::Listen | tcp::State::SynSent))
                && matches!(
                    state.sockets.get::<tcp::Socket>(h).state(),
                    tcp::State::SynReceived | tcp::State::Established
                )
            {
                self.entries[selected.unwrap()].as_mut().unwrap().remote_mss = mss;
            }
        }
    }
    fn reap_orphans(&mut self, i: usize, now: Instant) {
        let egress_ok = if i == 0 { !self.lo.full() } else { self.eth.as_ref().unwrap().tx_ready() };
        {
            for idx in 0..self.entries.len() {
                let Some(e) = self.entries[idx].as_ref() else {
                    continue;
                };
                if e.role != Role::Orphan {
                    continue;
                }
                let Some(h) = e.handles[i] else {
                    continue;
                };
                let socket = self.interfaces[i].as_ref().unwrap().sockets.get::<tcp::Socket>(h);
                let state = socket.state();
                if e.orphan_aborted
                    || egress_ok
                        && (state == tcp::State::Closed && socket.remote_endpoint().is_none()
                            || state == tcp::State::TimeWait && e.time_wait_acked)
                {
                    self.remove(e.id, state == tcp::State::TimeWait, now);
                }
            }
        }
    }
    fn poll_dhcp(&mut self) {
        if let Some(h) = self.dhcp {
            let event =
                self.interfaces[1].as_mut().unwrap().sockets.get_mut::<dhcpv4::Socket>(h).poll().map(|e| match e {
                    dhcpv4::Event::Deconfigured => Lease::Deconfigured,
                    dhcpv4::Event::Configured(c) => Lease::Configured {
                        addr: c.address,
                        router: c.router,
                        dns: core::array::from_fn(|i| c.dns_servers.get(i).copied()),
                        server: c.server.address,
                    },
                });
            if let Some(event) = event {
                self.pending_lease = Some(event);
            }
            if let Some(event) = self.pending_lease {
                let result = match event {
                    Lease::Deconfigured => self.configure_eth(None, None),
                    Lease::Configured { addr, router, .. } => self.configure_eth(Some(addr), router),
                };
                if result.is_ok() {
                    self.lease = Some(event);
                    self.pending_lease = None;
                }
            }
        }
    }
    /// Processes bounded ingress, egress and timers at the supplied monotonic instant.
    pub fn poll_round(&mut self, now: Instant) -> Round {
        self.now = now;
        self.needs_poll = false;
        self.ports.expire(now);
        self.deadlines();
        self.lo.sent.clear();
        if let Some(eth) = self.eth.as_mut() {
            eth.refresh();
        }
        let mut more = false;
        for i in 0..2 {
            let Some(state) = self.interfaces[i].as_mut() else {
                continue;
            };
            state.iface.poll_maintenance(now);
            for n in 0..RX_BUDGET {
                let len = if i == 0 {
                    receive_frame(&mut self.lo, &mut self.frame, now)
                } else {
                    receive_frame(self.eth.as_mut().unwrap(), &mut self.frame, now)
                };
                let Some(len) = len else {
                    break;
                };
                more |= n + 1 == RX_BUDGET;
                self.ingress_frame(i, len, now);
            }
            self.retain_rx(i, true);
            self.snapshot(i);
            self.poll_egress(i, now);
            self.observe(i, None, None, false);
            self.note_sent(i);
            self.reap_orphans(i, now);
        }
        self.poll_dhcp();
        self.update_shortfalls();
        let socket_deadline = self.live_entries().flat_map(|e| e.deadline.into_iter().chain(e.keep_deadline)).min();
        self.next_poll = self
            .interfaces
            .iter_mut()
            .flatten()
            .filter_map(|s| s.iface.poll_at(now, &s.sockets))
            .chain(socket_deadline)
            .min();
        // Egress can enqueue loopback packets after the ingress budget ran.
        // Unlike hardware, this device has no interrupt to wake the owner.
        more |= self.lo.take_full() || self.lo.queued() != 0;
        // Work queued by inline calls is now represented in smoltcp's deadline.
        self.needs_poll = false;
        Round { more }
    }
    fn poll_egress(&mut self, i: usize, now: Instant) {
        let state = self.interfaces[i].as_mut().unwrap();
        // eth0 has one configured IPv4 address, already selected by smoltcp.
        // Keep its sockets in place so neighbor-discovery retry metadata stays
        // intact. Only loopback's AnyIP aliases need the source override.
        if i == 1 {
            state.iface.poll_egress(now, self.eth.as_mut().unwrap(), &mut state.sockets);
            return;
        }
        for &idx in &self.live {
            let e = self.entries[idx].as_ref().unwrap();
            if matches!(e.proto, Proto::IcmpEcho { .. })
                && e.bound.and_then(|b| b.addr).is_some_and(|a| a != loopback_addr(a.is_ipv6()))
                && let Some(h) = e.handles[i]
            {
                let smoltcp::socket::Socket::Icmp(socket) = state.sockets.remove(h) else { unreachable!() };
                self.held_icmp.push((idx, socket));
            }
        }
        state.iface.poll_egress(now, &mut self.lo, &mut state.sockets);
        while let Some((idx, socket)) = self.held_icmp.pop() {
            let e = self.entries[idx].as_mut().unwrap();
            let source = e.bound.unwrap().addr.unwrap();
            let h = self.empty.add(socket);
            state.iface.poll_egress(now, &mut IcmpSource { device: &mut self.lo, source }, &mut self.empty);
            e.handles[i] = Some(state.sockets.add(self.empty.remove(h)));
        }
    }
    fn note_sent(&mut self, i: usize) {
        let sent = if i == 0 { &mut self.lo.sent } else { &mut self.eth.as_mut().unwrap().sent };
        for &idx in &self.live {
            let e = self.entries[idx].as_mut().unwrap();
            if let Some(h) = e.handles[i]
                && e.proto == Proto::Tcp
            {
                let s = self.interfaces[i].as_ref().unwrap().sockets.get::<tcp::Socket>(h);
                if let Some((local, remote)) = s.local_endpoint().zip(s.remote_endpoint())
                    && let Some(trace) = sent.iter().find(|t| t.tuple.local == local && t.tuple.remote == remote)
                {
                    e.sent = true;
                    e.time_wait_acked |= s.state() == tcp::State::TimeWait;
                    if e.options.keepalive && matches!(s.state(), tcp::State::Established | tcp::State::CloseWait) {
                        e.keep_armed = s.keep_alive().map(|d| self.now + d);
                        // A real one-byte data segment stays in send_queue until
                        // ACKed; the probe's synthetic byte never enters it.
                        if trace.probe && s.send_queue() == 0 {
                            e.keep_probes = e.keep_probes.saturating_add(1);
                            if e.keep_probes >= e.options.count {
                                e.keep_deadline = Some(self.now + e.options.interval);
                            }
                        }
                    }
                }
            }
        }
        sent.clear();
    }
}

impl<D: NetDevice> Stack<D> {
    /// Records shutdown directions and disconnects listeners or SYN-SENT sockets as Linux does.
    pub fn shutdown(&mut self, id: SockId, how: Shut, now: Instant) -> Result<(), NetError> {
        self.now = now;
        let e = self.entry(id)?;
        let state = if e.proto == Proto::Tcp { self.tcp_ref(id).ok().map(tcp::Socket::state) } else { None };
        if matches!(e.role, Role::Listener(_)) || state == Some(tcp::State::SynSent) {
            if matches!(e.role, Role::Listener(_)) && how == Shut::Wr {
                return Ok(());
            }
            self.close_children(id, now);
            if state == Some(tcp::State::SynSent) {
                self.drop_tcp_slot(id)?;
            }
            let e = self.entry_mut(id)?;
            e.role = Role::Fresh;
            e.peer = None;
            e.connected = false;
            e.deadline = None;
            e.keep_deadline = None;
            self.ports.unlisten(id);
            self.touch(id);
            self.update_shortfalls();
            return Ok(());
        }
        let e = self.entry_mut(id)?;
        let connected = e.connected && state != Some(tcp::State::Closed);
        if matches!(how, Shut::Rd | Shut::RdWr) {
            e.rd = true;
        }
        if matches!(how, Shut::Wr | Shut::RdWr) {
            e.wr = true;
        }
        let (rd, wr, proto) = (e.rd, e.wr, e.proto);
        if proto == Proto::Tcp {
            if let Ok(s) = self.tcp_mut(id) {
                if rd {
                    discard_rx(s);
                }
                if wr {
                    s.close();
                }
            }
        } else {
            self.filter_datagrams(id);
        }
        self.touch(id);
        if connected { Ok(()) } else { Err(NetError::NotConn) }
    }
    /// Aborts a TCP connection, retaining its slot until its RST can leave.
    pub fn abort(&mut self, id: SockId, now: Instant) -> Result<(), NetError> {
        self.now = now;
        self.tcp_mut(id)?.abort();
        self.entry_mut(id)?.rd = true;
        self.entry_mut(id)?.wr = true;
        self.touch(id);
        Ok(())
    }
    /// Closes an identity or transfers its TCP transport to bounded orphan cleanup; stale IDs do nothing.
    pub fn close(&mut self, id: SockId, linger_zero: bool, now: Instant) {
        self.now = now;
        let Ok(e) = self.entry(id) else {
            return;
        };
        if matches!(e.role, Role::Listener(_)) {
            self.close_children(id, now);
            self.remove(id, false, self.now);
        } else if e.proto == Proto::Tcp && e.handles.iter().any(Option::is_some) {
            if self.tcp_ref(id).is_ok_and(|s| matches!(s.state(), tcp::State::SynSent | tcp::State::Closed)) {
                self.remove(id, false, now);
                return;
            }
            let zero = linger_zero || e.options.linger == Some(Duration::ZERO);
            let s = self.tcp_mut(id).unwrap();
            if zero || s.recv_queue() > 0 {
                s.abort();
            } else {
                s.close();
            }
            s.set_ack_delay(None);
            s.set_timeout(None);
            s.set_keep_alive(None);
            let e = self.entry_mut(id).unwrap();
            e.role = Role::Orphan;
            e.keep_deadline = None;
            e.keep_armed = None;
            e.deadline = Some(now + ORPHAN_TIMEOUT);
            self.touch(id);
        } else {
            self.remove(id, false, self.now);
        }
        self.update_shortfalls();
    }
    fn close_children(&mut self, id: SockId, now: Instant) {
        for idx in 0..self.entries.len() {
            let child = self.entries[idx]
                .as_ref()
                .filter(|e| matches!(e.role, Role::Pool(p) | Role::Queued(p, _) if p == id))
                .map(|e| e.id);
            if let Some(child) = child {
                if self.tcp_ref(child).is_ok_and(|s| s.state() == tcp::State::Listen) {
                    self.remove(child, false, now);
                } else {
                    self.close(child, true, now);
                }
            }
        }
    }
    fn remove(&mut self, id: SockId, time_wait: bool, now: Instant) {
        let Some(e) = self.entries[id.index()].take() else {
            return;
        };
        self.live.retain(|&idx| idx != id.index());
        let tuple = if time_wait {
            let i = e.iface.unwrap().index();
            let s = self.interfaces[i].as_ref().unwrap().sockets.get::<tcp::Socket>(e.handles[i].unwrap());
            s.local_endpoint().zip(s.remote_endpoint()).map(|(local, remote)| Tuple { local, remote })
        } else {
            None
        };
        for i in 0..2 {
            if let Some(h) = e.handles[i] {
                self.interfaces[i].as_mut().unwrap().remove(h);
            }
        }
        if e.retired.is_some() {
            self.interfaces[e.iface.unwrap().index()].as_mut().unwrap().count -= 1;
        }
        if time_wait {
            self.ports.time_wait(id, e.time_wait_until.unwrap_or(now + TIME_WAIT), tuple);
        } else {
            self.ports.release(id);
        }
        self.touch(id);
    }
    /// Returns current readiness; stale identities report error and hangup.
    pub fn readiness(&self, id: SockId) -> Readiness {
        let Ok(e) = self.entry(id) else {
            return Readiness { hup: true, error: true, ..Readiness::default() };
        };
        let mut r = Readiness { error: e.error.is_some(), ..Readiness::default() };
        if let Role::Listener(_) = e.role {
            r.readable = self.queued(id) != 0;
            return r;
        }
        if e.proto != Proto::Tcp {
            r.readable = e.rd || e.heads.iter().any(|h| h.present);
            r.writable = e.handles.iter().all(Option::is_none)
                || e.handles.iter().enumerate().any(|(i, h)| {
                    e.datagram_tx_iface(i)
                        && h.is_some_and(|h| {
                            let s = &self.interfaces[i].as_ref().unwrap().sockets;
                            if e.proto == Proto::Udp {
                                s.get::<udp::Socket>(h).can_send()
                            } else {
                                s.get::<icmp::Socket>(h).can_send()
                            }
                        })
                });
            r.hup = e.rd && e.wr;
            r.rdhup = e.rd;
            return r;
        }
        let Ok(s) = self.tcp_ref(id) else {
            r.writable = true;
            r.hup = true;
            return r;
        };
        let state = s.state();
        if matches!(state, tcp::State::SynSent | tcp::State::SynReceived) {
            return r;
        }
        let closed = state == tcp::State::Closed;
        let fin =
            matches!(state, tcp::State::CloseWait | tcp::State::Closing | tcp::State::LastAck | tcp::State::TimeWait);
        r.readable = s.can_recv() || e.rd || fin || closed;
        r.rdhup = e.rd || fin || closed;
        r.writable = closed
            || e.wr
            || s.may_send() && s.send_capacity().saturating_sub(s.send_queue()) >= s.send_capacity().div_ceil(3);
        r.hup = closed || (e.wr && r.rdhup);
        r
    }
    /// Takes the pending SO_ERROR once, or None for a stale identity.
    pub fn take_error(&mut self, id: SockId) -> Option<NetError> {
        self.entry_mut(id).ok()?.error.take()
    }
    /// Returns the local endpoint in the socket’s reported family, or None for a stale identity.
    pub fn local_endpoint(&self, id: SockId) -> Option<IpEndpoint> {
        let e = self.entry(id).ok()?;
        let ep = if e.proto == Proto::Tcp { self.tcp_ref(id).ok().and_then(tcp::Socket::local_endpoint) } else { None };
        ep.or_else(|| {
            e.bound.map(|b| {
                let connected_addr =
                    e.peer.and_then(|p| e.iface.and_then(|iface| self.iface_addr(iface, p.addr.is_ipv6())));
                IpEndpoint::new(
                    b.addr.or(connected_addr).unwrap_or(if b.v6 {
                        IpAddress::Ipv6(Ipv6Address::UNSPECIFIED)
                    } else {
                        IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)
                    }),
                    b.port,
                )
            })
        })
        .map(|p| reported(p, e.options.v6))
    }
    /// Returns the peer in the socket’s reported family, or None if absent/stale.
    pub fn remote_endpoint(&self, id: SockId) -> Option<IpEndpoint> {
        let e = self.entry(id).ok()?;
        e.peer
            .or_else(|| self.tcp_ref(id).ok().and_then(tcp::Socket::remote_endpoint))
            .map(|p| reported(p, e.options.v6))
    }
    /// Returns the selected interface, if any, for a live identity.
    pub fn iface_of(&self, id: SockId) -> Option<IfaceId> {
        self.entry(id).ok()?.iface
    }
    /// Sets a supported option; timer changes use now and respect already armed probes.
    pub fn set_opt(&mut self, id: SockId, opt: NetOpt, now: Instant) -> Result<(), NetError> {
        let e = self.entry_mut(id)?;
        match opt {
            NetOpt::NoDelay(v) => e.options.nodelay = v,
            NetOpt::Cork(v) => e.options.cork = v,
            NetOpt::KeepAlive(v) => e.options.keepalive = v,
            NetOpt::KeepIdle(v) if v > Duration::ZERO && v <= Duration::from_secs(32767) => e.options.idle = v,
            NetOpt::KeepIntvl(v) if v > Duration::ZERO && v <= Duration::from_secs(32767) => e.options.interval = v,
            NetOpt::KeepCnt(v) if (1..=127).contains(&v) => e.options.count = v,
            NetOpt::Linger(v) => e.options.linger = v,
            NetOpt::Ttl(v) if v != 0 => e.options.ttl = v,
            NetOpt::Tos(v) => e.options.tos = v,
            NetOpt::Broadcast(v) => e.options.broadcast = v,
            NetOpt::V6Only(v) if e.bound.is_none() && e.options.v6 => {
                e.options.v6only = v;
            }
            NetOpt::MaxSeg(v) if v == 0 || (88..=32767).contains(&v) => e.options.maxseg = v,
            _ => return Err(NetError::Inval),
        }
        if e.proto == Proto::Tcp {
            if matches!(opt, NetOpt::KeepAlive(_) | NetOpt::KeepIdle(_) | NetOpt::KeepIntvl(_) | NetOpt::KeepCnt(_)) {
                self.apply_keepalive(id, now);
            }
            self.apply_tcp_options(id);
        } else {
            let handles = e.handles;
            let ttl = e.options.ttl;
            let proto = e.proto;
            for (i, h) in handles.into_iter().enumerate() {
                if let Some(h) = h {
                    let sockets = &mut self.interfaces[i].as_mut().unwrap().sockets;
                    if proto == Proto::Udp {
                        sockets.get_mut::<udp::Socket>(h).set_hop_limit(Some(ttl));
                    } else {
                        sockets.get_mut::<icmp::Socket>(h).set_hop_limit(Some(ttl));
                    }
                }
            }
        }
        let e = self.entry(id)?;
        if matches!(e.role, Role::Listener(_)) {
            let options = e.options;
            for pos in 0..self.live.len() {
                let child = self.entries[self.live[pos]].as_mut().unwrap();
                if child.role != Role::Pool(id) {
                    continue;
                }
                child.options = options;
                let child_id = child.id;
                self.apply_tcp_options(child_id);
                if matches!(opt, NetOpt::KeepAlive(_) | NetOpt::KeepIdle(_) | NetOpt::KeepIntvl(_) | NetOpt::KeepCnt(_))
                {
                    self.apply_keepalive(child_id, now);
                }
            }
        }
        self.touch(id);
        Ok(())
    }
    /// Reads the effective or recorded value of a supported option.
    pub fn get_opt(&self, id: SockId, q: OptQuery) -> Result<NetOpt, NetError> {
        let e = self.entry(id)?;
        let o = e.options;
        Ok(match q {
            OptQuery::NoDelay => NetOpt::NoDelay(o.nodelay),
            OptQuery::Cork => NetOpt::Cork(o.cork),
            OptQuery::KeepAlive => NetOpt::KeepAlive(o.keepalive),
            OptQuery::KeepIdle => NetOpt::KeepIdle(o.idle),
            OptQuery::KeepIntvl => NetOpt::KeepIntvl(o.interval),
            OptQuery::KeepCnt => NetOpt::KeepCnt(o.count),
            OptQuery::Linger => NetOpt::Linger(o.linger),
            OptQuery::Ttl => NetOpt::Ttl(o.ttl),
            OptQuery::Tos => NetOpt::Tos(o.tos),
            OptQuery::Broadcast => NetOpt::Broadcast(o.broadcast),
            OptQuery::V6Only => NetOpt::V6Only(o.v6only),
            OptQuery::MaxSeg => {
                // smoltcp 0.14 has no per-socket user-MSS setter. Report
                // Linux's user_mss before connect and its effective clamp
                // afterwards; advertised MSS/actual segmentation remain a
                // documented deviation until the transport supports a clamp.
                if o.maxseg != 0
                    && self.tcp_ref(id).map_or(true, |s| matches!(s.state(), tcp::State::Closed | tcp::State::Listen))
                {
                    NetOpt::MaxSeg(o.maxseg)
                } else {
                    let mtu = self.ip_mtu(e.iface.unwrap_or(IfaceId::Lo));
                    let effective = mtu
                        .saturating_sub(if e.peer.is_some_and(|p| p.addr.is_ipv6()) { 60 } else { 40 })
                        .min(e.remote_mss as usize) as u16;
                    NetOpt::MaxSeg(if o.maxseg == 0 { effective } else { o.maxseg.min(effective) })
                }
            }
        })
    }
    fn apply_tcp_options(&mut self, id: SockId) {
        let Ok(e) = self.entry(id) else {
            return;
        };
        let o = e.options;
        let Ok(s) = self.tcp_mut(id) else {
            return;
        };
        s.set_nagle_enabled(!o.nodelay);
        s.set_hop_limit(Some(o.ttl));
    }
    fn apply_keepalive(&mut self, id: SockId, now: Instant) {
        let active =
            self.tcp_ref(id).is_ok_and(|s| matches!(s.state(), tcp::State::Established | tcp::State::CloseWait));
        let e = self.entry_mut(id).unwrap();
        let o = e.options;
        e.keep_deadline = None;
        e.keep_probes = 0;
        if active && o.keepalive {
            // Existing smoltcp probe time is not changed by set_keep_alive.
            e.keep_armed.get_or_insert(now);
        } else {
            e.keep_armed = None;
        }
        if let Ok(s) = self.tcp_mut(id) {
            s.set_keep_alive((active && o.keepalive).then_some(o.idle));
        }
    }
    /// Only this method clears event flags; inline I/O cannot erase a wake.
    pub fn changed(&mut self) -> &[SockId] {
        self.changes.clear();
        for id in &mut self.dirty {
            if let Some(id) = id.take() {
                self.changes.push(id);
            }
        }
        &self.changes
    }
    /// Takes the most recent successfully applied DHCP change.
    pub fn lease_event(&mut self) -> Option<Lease> {
        self.lease.take()
    }
    /// Earliest pending work or timer instant; None means the owner may wait for an event.
    pub fn poll_at(&self, now: Instant) -> Option<Instant> {
        if self.needs_poll || self.lo.queued() != 0 {
            Some(now)
        } else {
            self.next_poll.into_iter().chain(self.ports.deadline()).min()
        }
    }
    /// Number of live port claims plus TIME-WAIT reservations; raw ICMP claims no identifier.
    pub fn port_count(&self) -> usize {
        self.ports.len()
    }
    /// Held TIME-WAIT ports and their absolute expiration instants.
    pub fn time_wait_ports(&self) -> impl Iterator<Item = (u16, Instant)> + '_ {
        self.ports.time_wait_ports()
    }
    /// Transport reservations across both interfaces, including retained closed transports.
    pub fn socket_count(&self) -> usize {
        self.interfaces.iter().flatten().map(|s| s.count).sum()
    }
    /// Number of live socket identities, including listener pools and orphans.
    pub fn entry_count(&self) -> usize {
        self.live.len()
    }
    /// Whether the single Ethernet device has been attached.
    pub fn has_eth(&self) -> bool {
        self.eth.is_some()
    }
    /// Last sampled Ethernet information, with transmit failures reflected in link state.
    pub fn device_info(&self) -> Option<NetInfo> {
        self.eth.as_ref().map(Adapter::info)
    }
    /// Cumulative dropped transmissions (Full and DeviceFailed); the kernel
    /// can report this counter without logging under the stack lock.
    pub fn failed_transmits(&self) -> usize {
        self.eth.as_ref().map_or(0, |d| d.failed_transmits)
    }
    /// Whether an Ethernet transmit token can currently be acquired.
    pub fn eth_tx_ready(&self) -> bool {
        self.eth.as_ref().is_some_and(Adapter::tx_ready)
    }

    /// Borrows the attached device with exclusive access to the stack.
    /// The owner must serialize diagnostics with polling, just as socket I/O.
    pub fn device_mut(&mut self) -> Option<&mut D> {
        self.eth.as_mut().map(|adapter| &mut adapter.device)
    }

    /// Configured addresses of one interface, or an empty slice if absent.
    pub fn interface_addresses(&self, iface: IfaceId) -> &[IpCidr] {
        self.interfaces[iface.index()].as_ref().map_or(&[], |state| state.iface.ip_addrs())
    }
}

impl<D: NetDevice> Stack<D> {
    fn pool_needed(&self, id: SockId, backlog: usize) -> [usize; 2] {
        let e = self.entry(id).unwrap();
        let coverage = self.coverage(e.bound.and_then(|b| b.addr));
        core::array::from_fn(
            |i| if coverage[i] { pool_target(backlog).saturating_sub(self.pool_count(id, i)) } else { 0 },
        )
    }
    fn pool_count(&self, id: SockId, iface: usize) -> usize {
        self.live_entries().filter(|e| e.role == Role::Pool(id) && e.handles[iface].is_some()).count()
    }
    fn queued(&self, id: SockId) -> usize {
        self.live_entries().filter(|e| matches!(e.role, Role::Queued(p, _) if p == id)).count()
    }
    /// Creates or resizes a bounded LISTEN pool using the supplied prepared TCP buffers.
    pub fn listen(
        &mut self,
        id: SockId,
        backlog: usize,
        pool: &mut dyn Iterator<Item = TcpPrep>,
        now: Instant,
    ) -> Result<(), NetError> {
        let e = self.entry(id)?;
        if e.proto != Proto::Tcp {
            return Err(NetError::OpNotSupp);
        }
        if !matches!(e.role, Role::Fresh | Role::Listener(_)) {
            return Err(NetError::Inval);
        }
        let req = e.bound.unwrap_or(self.unbound_req(id)?);
        let backlog = backlog.min(SOMAXCONN);
        let target = pool_target(backlog);
        let needed = self.pool_needed(id, backlog);
        if self.ports.remaining() < needed.iter().sum::<usize>() + usize::from(e.bound.is_none()) {
            return Err(NetError::NoBufs);
        }
        if (0..self.entries.len()).filter(|&i| self.slot_free(i)).count() < needed.iter().sum() {
            return Err(NetError::NoBufs);
        }
        let mut prepared: [Option<TcpPrep>; 2 * LISTEN_POOL_MAX] = core::array::from_fn(|_| None);
        let mut n = 0;
        for (i, needed) in needed.iter().copied().enumerate() {
            if needed == 0 {
                continue;
            }
            if self.interfaces[i].as_ref().is_none_or(|s| s.count + needed > NET_SOCKETS) {
                return Err(NetError::NoBufs);
            }
            for _ in 0..needed {
                prepared[n] = Some(pool.next().ok_or(NetError::NoBufs)?);
                n += 1;
            }
        }
        let autobind = self.entry(id)?.bound.is_none();
        if autobind {
            self.bind(id, req.request(), Preps::None, now)?;
        }
        if let Err(err) = self.ports.listen(id) {
            if autobind {
                self.ports.release(id);
                self.entry_mut(id)?.bound = None;
            }
            return Err(err);
        }
        self.entry_mut(id)?.role = Role::Listener(backlog);
        let mut n = 0;
        for (i, iface) in IfaceId::ALL.into_iter().enumerate() {
            for _ in 0..needed[i] {
                self.install_pool(id, iface, prepared[n].take().unwrap(), now)?;
                n += 1;
            }
            for idx in 0..self.entries.len() {
                if self.pool_count(id, i) <= target {
                    break;
                }
                if let Some(e) = self.entries[idx].as_ref()
                    && e.role == Role::Pool(id)
                    && e.handles[i].is_some_and(|h| {
                        self.interfaces[i].as_ref().unwrap().sockets.get::<tcp::Socket>(h).state() == tcp::State::Listen
                    })
                {
                    self.remove(e.id, false, now);
                }
            }
        }
        self.update_shortfalls();
        self.touch(id);
        Ok(())
    }
    fn install_pool(&mut self, id: SockId, iface: IfaceId, mut prep: TcpPrep, now: Instant) -> Result<(), NetError> {
        let parent = self.entry(id)?;
        let req = parent.bound.ok_or(NetError::Inval)?;
        let options = parent.options;
        let i = iface.index();
        if self.interfaces[i].as_ref().is_none_or(|s| s.count == NET_SOCKETS) {
            return Err(NetError::NoBufs);
        }
        prep.socket.listen(listen_endpoint(req, i)).map_err(|_| NetError::Inval)?;
        prep.socket.set_timeout(Some(SYN_RECEIVED_TIMEOUT));
        let child = self.open(if req.v6 { Family::V6 } else { Family::V4 }, Proto::Tcp)?;
        if let Err(err) = self.ports.inherit(id, child) {
            self.entries[child.index()] = None;
            self.live.retain(|&idx| idx != child.index());
            return Err(err);
        }
        let (h, c) = match self.interfaces[i].as_mut().unwrap().add(ConnectPrep::Tcp(prep)) {
            Ok(v) => v,
            Err(err) => {
                self.remove(child, false, now);
                return Err(err);
            }
        };
        let e = self.entry_mut(child)?;
        e.bound = Some(req);
        e.options = options;
        e.last_arrival = now;
        e.role = Role::Pool(id);
        e.iface = Some(iface);
        e.handles[i] = Some(h);
        e.charges[i] = Some(c);
        self.apply_tcp_options(child);
        Ok(())
    }
    /// At most `2 * NET_ENTRIES` listener/interface requests, valid until
    /// the next mutation. Copy before releasing the stack lock.
    pub fn shortfalls(&self) -> &[(SockId, IfaceId, usize)] {
        &self.shortages
    }
    fn shortfall(&self, id: SockId, iface: IfaceId) -> usize {
        let Ok(e) = self.entry(id) else {
            return 0;
        };
        let Role::Listener(backlog) = e.role else {
            return 0;
        };
        if self.queued(id) > backlog || !self.coverage(e.bound.unwrap().addr)[iface.index()] {
            return 0;
        }
        pool_target(backlog).saturating_sub(self.pool_count(id, iface.index()))
    }
    fn update_shortfalls(&mut self) {
        self.shortages.clear();
        for pos in 0..self.live.len() {
            let id = self.entries[self.live[pos]].as_ref().unwrap().id;
            for iface in IfaceId::ALL {
                let count = self.shortfall(id, iface);
                if count != 0 {
                    self.shortages.push((id, iface, count));
                }
            }
        }
    }
    /// Installs one missing pool socket; returns false for excess preparations or a closed listener.
    pub fn refill(&mut self, id: SockId, iface: IfaceId, prep: TcpPrep, now: Instant) -> Result<bool, NetError> {
        if self.shortfall(id, iface) == 0 {
            return Ok(false);
        }
        self.install_pool(id, iface, prep, now)?;
        self.needs_poll = true;
        self.update_shortfalls();
        Ok(true)
    }
    /// Detaches the oldest completed child only after its peer endpoint is available.
    pub fn accept(&mut self, id: SockId) -> Result<(SockId, IpEndpoint), NetError> {
        if !matches!(self.entry(id)?.role, Role::Listener(_)) {
            return Err(NetError::Inval);
        }
        let child = self
            .live_entries()
            .filter_map(|e| match e.role {
                Role::Queued(p, n) if p == id => Some((n, e.id)),
                _ => None,
            })
            .min_by_key(|(n, _)| *n)
            .map(|(_, id)| id)
            .ok_or(NetError::WouldBlock)?;
        let remote = self.remote_endpoint(child).ok_or(NetError::NotConn)?;
        self.entry_mut(child)?.role = Role::Active;
        self.entry_mut(child)?.connect_reported = true;
        self.touch(id);
        self.update_shortfalls();
        Ok((child, remote))
    }
    /// Discards one connection while preserving explicit bind/options.
    fn drop_tcp_slot(&mut self, id: SockId) -> Result<(), NetError> {
        let e = self.entry(id)?;
        let i = e.iface.ok_or(NetError::NotConn)?.index();
        if let Some(h) = e.handles[i] {
            self.interfaces[i].as_mut().unwrap().remove(h);
        } else if e.retired.is_some() {
            self.interfaces[i].as_mut().unwrap().count -= 1;
        }
        let e = self.entry_mut(id)?;
        let bound = (!e.auto_bound).then_some(e.bound).flatten();
        let options = e.options;
        let order = e.bind_order;
        let sequence = e.sequence.map(|n| n.wrapping_add(1));
        *e = Entry::new(id, Proto::Tcp);
        e.sequence = sequence;
        e.options = options;
        e.bound = bound;
        e.bind_order = order;
        e.iface = bound_iface(bound);
        if bound.is_none() {
            self.ports.release(id);
        }
        Ok(())
    }
    fn tcp_slot(&self, id: SockId) -> Result<(usize, SocketHandle), NetError> {
        let e = self.entry(id)?;
        if e.proto != Proto::Tcp {
            return Err(NetError::OpNotSupp);
        }
        let i = e.iface.ok_or(NetError::NotConn)?.index();
        Ok((i, e.handles[i].ok_or(NetError::NotConn)?))
    }
    fn tcp_ref(&self, id: SockId) -> Result<&tcp::Socket<'static>, NetError> {
        if let Some(s) = self.entry(id)?.retired.as_ref() {
            return Ok(s);
        }
        let (i, h) = self.tcp_slot(id)?;
        Ok(self.interfaces[i].as_ref().unwrap().sockets.get::<tcp::Socket>(h))
    }
    fn tcp_mut(&mut self, id: SockId) -> Result<&mut tcp::Socket<'static>, NetError> {
        if self.entry(id)?.retired.is_some() {
            return Ok(self.entry_mut(id)?.retired.as_mut().unwrap());
        }
        let (i, h) = self.tcp_slot(id)?;
        Ok(self.interfaces[i].as_mut().unwrap().sockets.get_mut::<tcp::Socket>(h))
    }
    /// Sends stream bytes or one datagram, applying pending errors and shutdown policy first.
    pub fn send(
        &mut self,
        id: SockId,
        data: &[u8],
        to: Option<IpEndpoint>,
        mut preps: Preps<'_>,
        now: Instant,
    ) -> Result<usize, NetError> {
        let e = self.entry_mut(id)?;
        if let Some(err) = e.error.take() {
            return Err(err);
        }
        if e.wr {
            return Err(NetError::Pipe);
        }
        if e.proto == Proto::Tcp {
            let connected = e.connected;
            let s = self.tcp_mut(id).map_err(|_| NetError::Pipe)?;
            if s.state() == tcp::State::Closed {
                return Err(NetError::Pipe);
            }
            if !connected {
                return Err(NetError::WouldBlock);
            }
            if !s.may_send() {
                return Err(NetError::Pipe);
            }
            let n = s.send_slice(data).map_err(|_| NetError::Pipe)?;
            if n == 0 && !data.is_empty() {
                return Err(NetError::WouldBlock);
            }
            self.touch(id);
            return Ok(n);
        }
        // Linux net/ipv4/udp.c:1059 checks the absolute IPv4 length before
        // looking for a destination (the IPv6 path has a larger outer limit).
        if e.proto == Proto::Udp && !e.options.v6 && data.len() > 65535 {
            return Err(NetError::MsgSize);
        }
        let remote = to.or(e.peer).ok_or(NetError::DestAddrReq)?;
        let proto = e.proto;
        let (remote, iface) = self.resolve(id, remote)?;
        let i = iface.index();
        if proto == Proto::Udp && self.broadcast(remote.addr, iface) && !self.entry(id)?.options.broadcast {
            return Err(NetError::Acces);
        }
        let header = if remote.addr.is_ipv4() { 20 } else { 40 } + if proto == Proto::Udp { 8 } else { 0 };
        if data.len() > self.ip_mtu(iface).saturating_sub(header) {
            return Err(NetError::MsgSize);
        }
        if matches!(proto, Proto::IcmpEcho { .. }) && (data.len() < 8 || !matches!(data[0], 8 | 128)) {
            return Err(NetError::Inval);
        }
        if self.entry(id)?.bound.is_none() {
            let mut req = self.unbound_req(id)?;
            if proto == (Proto::IcmpEcho { raw: true }) {
                req.port = u16::from_be_bytes([data[4], data[5]]);
            }
            self.bind(id, req.request(), preps, now)?;
        } else if self.entry(id)?.handles[i].is_none() {
            self.missing_datagram(id, i, preps.next().ok_or(NetError::NoBufs)?)?;
        }
        let e = self.entry(id)?;
        let h = e.handles[i].ok_or(NetError::NoBufs)?;
        let ident = e.bound.unwrap().port;
        let local = self
            .local_endpoint(id)
            .map(|p| canonical(p.addr))
            .filter(|a| !a.is_unspecified())
            .or_else(|| self.iface_addr(iface, remote.addr.is_ipv6()))
            .ok_or(NetError::AddrNotAvail)?;
        match e.proto {
            Proto::Udp => {
                let s = self.interfaces[i].as_mut().unwrap().sockets.get_mut::<udp::Socket>(h);
                if data.len() > s.payload_send_capacity() {
                    return Err(NetError::MsgSize);
                }
                s.send_slice(data, remote).map_err(|err| match err {
                    udp::SendError::BufferFull => NetError::WouldBlock,
                    udp::SendError::Unaddressable => NetError::AddrNotAvail,
                })?;
            }
            Proto::IcmpEcho { raw } => {
                let s = self.interfaces[i].as_mut().unwrap().sockets.get_mut::<icmp::Socket>(h);
                if data.len() > s.payload_send_capacity() {
                    return Err(NetError::MsgSize);
                }
                let out = s.send(data.len(), remote.addr).map_err(|err| match err {
                    icmp::SendError::BufferFull => NetError::WouldBlock,
                    icmp::SendError::Unaddressable => NetError::AddrNotAvail,
                })?;
                out.copy_from_slice(data);
                if !raw {
                    out[4..6].copy_from_slice(&ident.to_be_bytes());
                    if remote.addr.is_ipv4() {
                        smoltcp::wire::Icmpv4Packet::new_unchecked(out).fill_checksum();
                    } else if let (IpAddress::Ipv6(src), IpAddress::Ipv6(dst)) = (local, remote.addr) {
                        smoltcp::wire::Icmpv6Packet::new_unchecked(out).fill_checksum(&src, &dst);
                    }
                }
            }
            Proto::Tcp => unreachable!(),
        }
        if proto == (Proto::IcmpEcho { raw: true }) && self.entry(id)?.raw_ident.is_none() {
            let ident = u16::from_be_bytes([data[4], data[5]]);
            let handles = self.entry(id)?.handles;
            for (i, h) in handles.into_iter().enumerate() {
                if let Some(h) = h {
                    // Raw preparations stay unbound until this first successful
                    // enqueue. All installed handles therefore accept the bind.
                    self.interfaces[i]
                        .as_mut()
                        .unwrap()
                        .sockets
                        .get_mut::<icmp::Socket>(h)
                        .bind(icmp::Endpoint::Ident(ident))
                        .expect("unbound raw ICMP transport");
                }
            }
            let e = self.entry_mut(id)?;
            e.raw_ident = Some(ident);
            e.bound.as_mut().unwrap().port = ident;
        }
        self.touch(id);
        Ok(data.len())
    }
    fn filter_datagrams(&mut self, id: SockId) {
        match self.entries[id.index()].as_ref().map(|e| e.proto) {
            Some(Proto::Udp) => self.filter_udp(id),
            Some(Proto::IcmpEcho { raw }) => self.filter_icmp(id, raw),
            _ => (),
        }
    }
    fn filter_udp(&mut self, id: SockId) {
        let e = self.entries[id.index()].as_mut().unwrap();
        for i in 0..2 {
            e.heads[i] = Head::default();
            let Some(h) = e.handles[i] else {
                continue;
            };
            let socket = self.interfaces[i].as_mut().unwrap().sockets.get_mut::<udp::Socket>(h);
            while let Ok((data, m)) = socket.peek() {
                let from = m.endpoint;
                if family_ok(e.options.v6, e.options.v6only, from.addr)
                    && e.peer.is_none_or(|p| p == from)
                    && (e.peer.is_none() || e.iface.is_none_or(|f| f.index() == i))
                    && !e.rd
                {
                    e.heads[i] = Head { len: data.len(), from: Some(from), present: true };
                    break;
                }
                let _ = socket.recv();
                e.sequence[i] = e.sequence[i].wrapping_add(1);
            }
        }
    }
    fn filter_icmp(&mut self, id: SockId, raw: bool) {
        let e = self.entries[id.index()].as_mut().unwrap();
        for i in 0..2 {
            if e.heads[i].present && !e.rd {
                continue;
            }
            e.heads[i] = Head::default();
            let Some(h) = e.handles[i] else {
                continue;
            };
            let state = self.interfaces[i].as_mut().unwrap();
            let socket = state.sockets.get_mut::<icmp::Socket>(h);
            while let Ok((data, from)) = socket.recv() {
                let destination = e.icmp_destinations[i].pop();
                let local = destination.map(|(addr, _)| addr);
                if e.rd
                    || destination.is_some_and(|(_, deliver)| !deliver)
                    || !matches!(data.first(), Some(0 | 8 | 128 | 129))
                    || !raw && matches!(data.first(), Some(8 | 128))
                    || !family_ok(e.options.v6, e.options.v6only, from)
                    || e.bound.and_then(|b| b.addr).is_some_and(|addr| local != Some(addr))
                    || raw && e.peer.is_some_and(|p| p.addr != from)
                {
                    continue;
                }
                let cache = e.icmp_head[i].as_mut().unwrap();
                let header = if raw && from.is_ipv4() { 20 } else { 0 };
                cache[header..header + data.len()].copy_from_slice(data);
                if header != 0 {
                    cache[..20].fill(0);
                    cache[0] = 0x45;
                    cache[2..4].copy_from_slice(&((data.len() + 20) as u16).to_be_bytes());
                    cache[8] = 64;
                    cache[9] = 1;
                    if let IpAddress::Ipv4(a) = from {
                        cache[12..16].copy_from_slice(&a.octets());
                    }
                    if let Some(IpAddress::Ipv4(a)) = local {
                        cache[16..20].copy_from_slice(&a.octets());
                    }
                    smoltcp::wire::Ipv4Packet::new_unchecked(&mut cache[..20]).fill_checksum();
                }
                e.heads[i] = Head { len: header + data.len(), from: Some(IpEndpoint::new(from, 0)), present: true };
                break;
            }
        }
    }
    /// Returns the next datagram’s true size and a generation-checked receive ticket.
    pub fn datagram_len(&self, id: SockId) -> Result<(usize, RecvTicket), NetError> {
        let e = self.entry(id)?;
        if e.proto == Proto::Tcp {
            return Err(NetError::OpNotSupp);
        }
        for i in [e.next_rx, 1 - e.next_rx] {
            if e.heads[i].present {
                return Ok((
                    e.heads[i].len,
                    RecvTicket { id, iface: IfaceId::from_index(i), sequence: e.sequence[i], bytes: e.heads[i].len },
                ));
            }
        }
        Err(NetError::WouldBlock)
    }
    /// Copies or peeks the ticket’s datagram; stale tickets must be retried from datagram_len.
    pub fn recv_ticket(
        &mut self,
        id: SockId,
        ticket: RecvTicket,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<Received, NetError> {
        let e = self.entry(id).map_err(|_| NetError::StaleTicket)?;
        let i = ticket.iface.index();
        if ticket.id != id || e.sequence[i] != ticket.sequence || !e.heads[i].present {
            return Err(NetError::StaleTicket);
        }
        let head = e.heads[i];
        let from = head.from.map(|p| reported(p, e.options.v6));
        let h = e.handles[i].ok_or(NetError::Inval)?;
        let copied = buf.len().min(head.len);
        if e.proto == Proto::Udp {
            let data = self.interfaces[i]
                .as_mut()
                .unwrap()
                .sockets
                .get_mut::<udp::Socket>(h)
                .peek()
                .map(|(d, _)| d)
                .map_err(|_| NetError::WouldBlock)?;
            buf[..copied].copy_from_slice(&data[..copied]);
        } else {
            buf[..copied].copy_from_slice(&e.icmp_head[i].as_ref().unwrap()[..copied]);
        }
        if !peek {
            self.consume(id, ticket)?;
        }
        Ok(Received { copied, full_len: head.len, from, ticket })
    }
    /// Reads or peeks queued bytes before errors; FIN preserves EOF priority after the queue drains.
    pub fn recv(&mut self, id: SockId, buf: &mut [u8], peek: bool) -> Result<Received, NetError> {
        let e = self.entry(id)?;
        if e.proto != Proto::Tcp {
            if let Some(err) = self.take_error(id) {
                return Err(err);
            }
            let e = self.entry(id)?;
            if e.proto == Proto::Udp && e.rd && !e.heads.iter().any(|h| h.present) {
                let iface = e.iface.unwrap_or(IfaceId::Lo);
                let ticket = RecvTicket { id, iface, sequence: e.sequence[iface.index()], bytes: 0 };
                return Ok(Received { copied: 0, full_len: 0, from: None, ticket });
            }
            let (_, ticket) = self.datagram_len(id)?;
            return self.recv_ticket(id, ticket, buf, peek);
        }
        let iface = e.iface.unwrap_or(IfaceId::Lo);
        let i = iface.index();
        let ticket = RecvTicket { id, iface, sequence: e.sequence[i], bytes: 0 };
        let n = if !e.rd && self.tcp_ref(id).is_ok_and(|s| s.can_recv()) {
            let s = self.tcp_mut(id)?;
            if peek { s.peek_slice(buf) } else { s.recv_slice(buf) }.map_err(|_| NetError::WouldBlock)?
        } else {
            if !e.fin_received
                && let Some(err) = self.take_error(id)
            {
                return Err(err);
            }
            let e = self.entry(id)?;
            let s = self.tcp_ref(id)?;
            if !e.rd && s.state() != tcp::State::Closed && s.may_recv() && !buf.is_empty()
                || matches!(s.state(), tcp::State::SynSent | tcp::State::SynReceived)
            {
                return Err(NetError::WouldBlock);
            }
            0
        };
        if !peek && n > 0 {
            self.entry_mut(id)?.sequence[i] = ticket.sequence.wrapping_add(1);
            self.touch(id);
        }
        Ok(Received { copied: n, full_len: n, from: None, ticket: RecvTicket { bytes: n, ..ticket } })
    }
    /// Consumes a TCP ticket’s prefix or an entire datagram; stale tickets return StaleTicket.
    pub fn consume(&mut self, id: SockId, ticket: RecvTicket) -> Result<(), NetError> {
        let e = self.entry(id).map_err(|_| NetError::StaleTicket)?;
        let i = ticket.iface.index();
        if ticket.id != id || e.sequence[i] != ticket.sequence {
            return Err(NetError::StaleTicket);
        }
        let h = e.handles[i];
        let proto = e.proto;
        match proto {
            Proto::Tcp => {
                let mut left = ticket.bytes;
                let s = self.tcp_mut(id).map_err(|_| NetError::StaleTicket)?;
                while left > 0 {
                    let Ok(n) = s.recv(|data| {
                        let n = data.len().min(left);
                        (n, n)
                    }) else {
                        break;
                    };
                    if n == 0 {
                        break;
                    }
                    left -= n;
                }
            }
            Proto::Udp => {
                let _ = self.interfaces[i]
                    .as_mut()
                    .unwrap()
                    .sockets
                    .get_mut::<udp::Socket>(h.ok_or(NetError::StaleTicket)?)
                    .recv();
            }
            Proto::IcmpEcho { .. } => {
                self.entry_mut(id).unwrap().heads[i] = Head::default();
            }
        }
        let e = self.entry_mut(id).unwrap();
        e.sequence[i] = e.sequence[i].wrapping_add(1);
        if proto != Proto::Tcp {
            e.next_rx = 1 - i;
        }
        self.filter_datagrams(id);
        self.touch(id);
        Ok(())
    }
}

fn pool_target(backlog: usize) -> usize {
    (backlog.min(SOMAXCONN) + 1).min(LISTEN_POOL_MAX)
}
enum Syn {
    Deliver(Option<usize>),
    NoOwner,
    Drop,
}

fn discard_rx(s: &mut tcp::Socket<'_>) {
    while s.can_recv() {
        let _ = s.recv(|d| (d.len(), ()));
    }
}

fn receive_frame<D: Device>(device: &mut D, frame: &mut [u8], now: Instant) -> Option<usize> {
    device.receive(now).map(|(rx, _)| {
        rx.consume(|data| {
            frame[..data.len()].copy_from_slice(data);
            data.len()
        })
    })
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

fn bound_iface(req: Option<Bound>) -> Option<IfaceId> {
    req.and_then(|b| b.addr.map(|a| if loopback(a) { IfaceId::Lo } else { IfaceId::Eth0 }))
}

fn configure_datagram(prep: &mut ConnectPrep, proto: Proto, req: Bound, i: usize, ttl: u8) -> Result<(), NetError> {
    match (prep, proto) {
        (ConnectPrep::Udp(p), Proto::Udp) => {
            p.socket.set_hop_limit(Some(ttl));
            p.socket.bind(listen_endpoint(req, i)).map_err(|_| NetError::Inval)
        }
        (ConnectPrep::Icmp(p), Proto::IcmpEcho { raw }) => {
            p.socket.set_hop_limit(Some(ttl));
            if raw { Ok(()) } else { p.socket.bind(icmp::Endpoint::Ident(req.port)).map_err(|_| NetError::Inval) }
        }
        _ => Err(NetError::Inval),
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;

    struct Device {
        ready: bool,
    }
    impl NetDevice for Device {
        fn info(&self) -> NetInfo {
            NetInfo { mac: [2, 0, 0, 0, 0, 1], mtu: 1500, link_up: true }
        }
        fn tx_ready(&self) -> bool {
            self.ready
        }
        fn receive(&mut self, _: &mut [u8; ETH_FRAME_MAX]) -> Option<usize> {
            None
        }
        fn transmit<R>(&mut self, _: usize, _: impl FnOnce(&mut [u8]) -> R) -> Result<R, TxError> {
            Err(TxError::Full)
        }
        fn reclaim(&mut self) -> usize {
            0
        }
    }
    #[test]
    fn diagnostics_follow_the_attached_device_and_address_configuration() {
        let mut stack = Stack::<Device>::try_new(Loopback::try_new(65535, 16).unwrap(), Instant::ZERO, 474).unwrap();
        assert!(stack.device_mut().is_none());
        assert!(stack.interface_addresses(IfaceId::Eth0).is_empty());
        assert_eq!(
            stack.interface_addresses(IfaceId::Lo),
            &[IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8), IpCidr::new(IpAddress::Ipv6(Ipv6Address::LOCALHOST), 128),]
        );
        let addr = Ipv4Cidr::new(Ipv4Address::new(10, 0, 2, 15), 24);
        stack
            .attach_eth(Device { ready: false }, EthConfig::Static { addr, router: None }, Instant::ZERO, 475)
            .unwrap();
        assert!(!stack.eth_tx_ready());
        stack.device_mut().unwrap().ready = true;
        assert!(stack.eth_tx_ready());
        assert_eq!(stack.interface_addresses(IfaceId::Eth0), &[IpCidr::Ipv4(addr)]);
        stack.configure_eth(None, None).unwrap();
        assert!(stack.interface_addresses(IfaceId::Eth0).is_empty());
        assert_eq!(stack.interface_addresses(IfaceId::Lo).len(), 2);
    }
}
