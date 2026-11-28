use crate::memory::filled;
use crate::{BindReq, EPHEMERAL, Instant, IpFamily, NetError, Proto, SockId, canonical};
use alloc::vec::Vec;

/// Family policy is captured from the socket, never from a bind request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Bound {
    pub addr: Option<crate::IpAddress>,
    pub port: u16,
    pub v6: bool,
    pub v6only: bool,
    pub reuse_addr: bool,
    pub reuse_port: bool,
}
impl Bound {
    pub fn new(req: BindReq, v6: bool, v6only: bool) -> Self {
        Self { addr: req.addr, port: req.port, reuse_addr: req.reuse_addr, reuse_port: req.reuse_port, v6, v6only }
    }
    pub fn request(self) -> BindReq {
        BindReq { addr: self.addr, port: self.port, reuse_addr: self.reuse_addr, reuse_port: self.reuse_port }
    }
}
#[derive(Clone, Copy, Debug)]
struct Binding {
    id: SockId,
    group: SockId,
    proto: Proto,
    req: Bound,
    listening: bool,
    until: Option<Instant>,
    tuple: Option<crate::packet::Tuple>,
}

/// Fixed storage shared by IPv4 and IPv6, with TCP, UDP and ping identifier cursors.
pub(crate) struct PortTable {
    slots: Vec<Option<Binding>>,
    time_wait: Vec<Option<Binding>>,
    next: [u16; 3],
    uses: Vec<[usize; 3]>,
}
impl PortTable {
    pub fn try_new(capacity: usize) -> Result<Self, NetError> {
        Ok(Self {
            slots: filled(capacity, || None)?,
            time_wait: filled(capacity, || None)?,
            next: [*EPHEMERAL.start(), *EPHEMERAL.start(), 1],
            uses: filled(65536, || [0; 3])?,
        })
    }
    fn protocol(proto: Proto) -> usize {
        match proto {
            Proto::Tcp => 0,
            Proto::Udp => 1,
            Proto::IcmpEcho { .. } => 2,
        }
    }
    pub fn bind(&mut self, id: SockId, proto: Proto, mut req: Bound) -> Result<Bound, NetError> {
        // Raw sockets demultiplex by identifier but do not reserve ping ports.
        if proto == (Proto::IcmpEcho { raw: true }) {
            return Ok(req);
        }
        if self.slots.iter().flatten().any(|b| b.id == id) {
            return Err(NetError::Inval);
        }
        let slot = self.slots.iter().position(Option::is_none).ok_or(NetError::NoBufs)?;
        req.addr = req.addr.map(canonical).filter(|a| !a.is_unspecified());
        if req.port == 0 && proto != (Proto::IcmpEcho { raw: true }) {
            req.port = self.pick_ephemeral(proto)?;
        } else if self.conflict(id, proto, req) {
            return Err(NetError::AddrInUse);
        }
        self.slots[slot] = Some(Binding { id, group: id, proto, req, listening: false, until: None, tuple: None });
        self.uses[req.port as usize][Self::protocol(proto)] += 1;
        Ok(req)
    }
    fn conflict(&self, id: SockId, proto: Proto, req: Bound) -> bool {
        self.slots.iter().chain(&self.time_wait).flatten().any(|b| {
            if b.id == id || b.group == id || Self::protocol(b.proto) != Self::protocol(proto) || b.req.port != req.port
            {
                return false;
            }
            if !overlap(req, b.req) {
                return false;
            }
            !(req.reuse_port && b.req.reuse_port
                || req.reuse_addr && b.req.reuse_addr && (proto != Proto::Tcp || !b.listening))
        })
    }
    pub fn unlisten(&mut self, id: SockId) {
        if let Some(b) = self.slots.iter_mut().flatten().find(|b| b.id == id) {
            b.listening = false;
        }
    }
    pub fn listen(&mut self, id: SockId) -> Result<(), NetError> {
        let index = self.slots.iter().position(|b| b.is_some_and(|b| b.id == id)).ok_or(NetError::Inval)?;
        let b = self.slots[index].unwrap();
        if self.conflict(id, b.proto, b.req) {
            return Err(NetError::AddrInUse);
        }
        self.slots[index].as_mut().unwrap().listening = true;
        Ok(())
    }
    pub(crate) fn inherit(&mut self, parent: SockId, id: SockId) -> Result<(), NetError> {
        let b = *self.slots.iter().flatten().find(|b| b.id == parent).ok_or(NetError::Inval)?;
        let slot = self.slots.iter_mut().find(|s| s.is_none()).ok_or(NetError::NoBufs)?;
        *slot = Some(Binding { id, listening: false, ..b });
        self.uses[b.req.port as usize][Self::protocol(b.proto)] += 1;
        Ok(())
    }
    pub fn release(&mut self, id: SockId) {
        Self::drop_where(&mut self.slots, &mut self.uses, |b| b.id == id);
    }
    pub fn time_wait(&mut self, id: SockId, until: Instant, tuple: Option<crate::packet::Tuple>) {
        if let Some(index) = self.slots.iter().position(|b| b.is_some_and(|b| b.id == id)) {
            // TIME-WAIT has an independent fixed pool. When it is full, evict
            // the oldest reservation. Linux refuses the new TW bucket instead.
            let target =
                self.time_wait.iter().position(Option::is_none).unwrap_or_else(|| {
                    self.time_wait.iter().enumerate().min_by_key(|(_, b)| b.unwrap().until).unwrap().0
                });
            if let Some(old) = self.time_wait[target].take() {
                Self::forget(&mut self.uses, old);
            }
            let mut b = self.slots[index].take().unwrap();
            b.until = Some(until);
            b.tuple = tuple;
            b.listening = false;
            self.time_wait[target] = Some(b);
        }
    }
    pub fn expire(&mut self, now: Instant) {
        Self::drop_where(&mut self.time_wait, &mut self.uses, |b| b.until.is_some_and(|t| now >= t));
    }
    pub fn tuple_reserved(&self, tuple: crate::packet::Tuple, now: Instant) -> bool {
        self.time_wait.iter().flatten().any(|b| b.tuple == Some(tuple) && b.until.is_some_and(|t| now < t))
    }
    fn pick_ephemeral(&mut self, proto: Proto) -> Result<u16, NetError> {
        let p = Self::protocol(proto);
        let range = if matches!(proto, Proto::IcmpEcho { .. }) { 1..=65535 } else { EPHEMERAL };
        for _ in range.clone() {
            let port = self.next[p];
            self.next[p] = if port == *range.end() { *range.start() } else { port + 1 };
            if self.uses[port as usize][p] == 0 {
                return Ok(port);
            }
        }
        Err(NetError::AddrNotAvail)
    }
    fn forget(uses: &mut [[usize; 3]], b: Binding) {
        uses[b.req.port as usize][Self::protocol(b.proto)] -= 1;
    }
    fn drop_where(slots: &mut [Option<Binding>], uses: &mut [[usize; 3]], pred: impl Fn(Binding) -> bool) {
        for slot in slots {
            if slot.is_some_and(&pred) {
                let b = slot.take().unwrap();
                Self::forget(uses, b);
            }
        }
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.time_wait.iter().flatten().filter_map(|b| b.until).min()
    }
    pub fn len(&self) -> usize {
        self.slots.iter().chain(&self.time_wait).flatten().count()
    }

    pub(crate) fn remaining(&self) -> usize {
        self.slots.iter().filter(|b| b.is_none()).count()
    }
    pub fn time_wait_ports(&self) -> impl Iterator<Item = (u16, Instant)> + '_ {
        self.time_wait.iter().flatten().filter_map(|b| b.until.map(|t| (b.req.port, t)))
    }
}

fn covers_v4(req: Bound) -> bool {
    !req.v6 || !req.v6only
}
fn wildcard_covers(req: Bound, addr: crate::IpAddress) -> bool {
    if addr.is_ipv4() { covers_v4(req) } else { req.v6 }
}
fn overlap(a: Bound, b: Bound) -> bool {
    match (a.addr, b.addr) {
        (Some(a), Some(b)) => a == b,
        (None, Some(addr)) => wildcard_covers(a, addr),
        (Some(addr), None) => wildcard_covers(b, addr),
        (None, None) => a.v6 && b.v6 || covers_v4(a) && covers_v4(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IpAddress, Ipv6Address};
    fn id(idx: u32) -> SockId {
        SockId { idx, generation: 1 }
    }
    #[test]
    fn port_table_conflict_matrix() {
        let v4 = Bound { addr: Some(IpAddress::v4(127, 0, 0, 1)), port: 1234, ..Bound::default() };
        let other = Bound { addr: Some(IpAddress::v4(127, 0, 0, 2)), ..v4 };
        let any4 = Bound { addr: None, ..v4 };
        let any6 = Bound { v6: true, ..any4 };
        let only6 = Bound { v6only: true, ..any6 };
        let v6 = Bound { addr: Some(IpAddress::Ipv6(Ipv6Address::LOCALHOST)), ..only6 };
        for (a, b, overlap) in [
            (v4, v4, true),
            (v4, other, false),
            (v4, any4, true),
            (any4, any4, true),
            (v4, any6, true),
            (any4, any6, true),
            (v4, only6, false),
            (any4, only6, false),
            (v6, any6, true),
            (v6, any4, false),
        ]
        .into_iter()
        .flat_map(|(a, b, overlap)| [(a, b, overlap), (b, a, overlap)])
        {
            for proto in [Proto::Tcp, Proto::Udp] {
                for reuse_addr in [false, true] {
                    for reuse_port in [false, true] {
                        for both in [false, true] {
                            let mut t = PortTable::try_new(4).unwrap();
                            t.bind(
                                id(0),
                                proto,
                                Bound { reuse_addr: reuse_addr && both, reuse_port: reuse_port && both, ..a },
                            )
                            .unwrap();
                            let got = t.bind(id(1), proto, Bound { reuse_addr, reuse_port, ..b });
                            let allowed = !overlap || both && (reuse_addr || reuse_port);
                            assert_eq!(
                                got.is_ok(),
                                allowed,
                                "{a:?} {b:?} {proto:?} addr={reuse_addr} port={reuse_port} both={both}"
                            );
                        }
                    }
                }
            }
        }
        let mut t = PortTable::try_new(8).unwrap();
        t.bind(id(0), Proto::Tcp, Bound { reuse_addr: true, ..any4 }).unwrap();
        t.listen(id(0)).unwrap();
        assert_eq!(t.bind(id(1), Proto::Tcp, Bound { reuse_addr: true, ..v4 }), Err(NetError::AddrInUse));
        assert!(t.bind(id(1), Proto::Udp, v4).is_ok());
        let mut t = PortTable::try_new(4).unwrap();
        t.bind(id(0), Proto::Tcp, Bound { reuse_port: true, ..any4 }).unwrap();
        t.listen(id(0)).unwrap();
        t.bind(id(1), Proto::Tcp, Bound { reuse_port: true, ..v4 }).unwrap();
        t.listen(id(1)).unwrap();
    }
    #[test]
    fn ephemeral_exhaustion_is_eaddrnotavail_and_wrap_reuses_only_freed_ports() {
        let count = EPHEMERAL.clone().count();
        let mut t = PortTable::try_new(count + 1).unwrap();
        // Populate occupancy directly so this exhaustively checks all 28,232
        // candidate ports without an O(N^2) conflict-matrix setup.
        for (i, port) in EPHEMERAL.enumerate() {
            t.slots[i] = Some(Binding {
                id: id(i as u32),
                group: id(i as u32),
                proto: Proto::Tcp,
                req: Bound { port, ..Bound::default() },
                listening: false,
                until: None,
                tuple: None,
            });
            t.uses[port as usize][0] = 1;
        }
        assert_eq!(t.bind(id(count as u32), Proto::Tcp, Bound::default()), Err(NetError::AddrNotAvail));
        t.release(id(100));
        assert_eq!(t.bind(id(count as u32), Proto::Tcp, Bound::default()).unwrap().port, 32868);
        t.release(id(count as u32));
        t.next[0] = 60999;
        t.release(id(0));
        assert_eq!(t.bind(id(count as u32), Proto::Tcp, Bound::default()).unwrap().port, 32768);
        let udp = t.bind(id((count + 1) as u32), Proto::Udp, Bound::default()).unwrap();
        assert_eq!(udp.port, 32768);
    }
    #[test]
    fn time_wait_holds_port_until_deadline_or_reuseaddr() {
        let mut t = PortTable::try_new(4).unwrap();
        let req = Bound { port: 1234, reuse_addr: true, ..Bound::default() };
        t.bind(id(0), Proto::Tcp, req).unwrap();
        t.time_wait(id(0), Instant::from_millis(10000), None);
        t.expire(Instant::from_millis(9999));
        assert_eq!(t.bind(id(1), Proto::Tcp, Bound { reuse_addr: false, ..req }), Err(NetError::AddrInUse));
        t.bind(id(1), Proto::Tcp, Bound { reuse_addr: true, ..req }).unwrap();
        t.release(id(1));
        t.expire(Instant::from_millis(10000));
        assert!(t.bind(id(1), Proto::Tcp, req).is_ok());
    }
}

#[cfg(test)]
mod round1_tests {
    use super::*;
    fn id(idx: u32) -> SockId {
        SockId { idx, generation: 1 }
    }
    #[test]
    fn time_wait_reuse_requires_both_owners_and_has_separate_capacity() {
        // Linux v6.12 net/ipv4/inet_timewait_sock.c,
        // inet_twsk_alloc:205-206 preserves both reuse flags.
        for old_addr in [false, true] {
            for old_port in [false, true] {
                for new_addr in [false, true] {
                    for new_port in [false, true] {
                        let mut t = PortTable::try_new(1).unwrap();
                        let req = Bound { port: 1234, reuse_addr: old_addr, reuse_port: old_port, ..Bound::default() };
                        t.bind(id(0), Proto::Tcp, req).unwrap();
                        t.time_wait(id(0), Instant::from_secs(10), None);
                        assert_eq!(t.remaining(), 1);
                        let result =
                            t.bind(id(1), Proto::Tcp, Bound { reuse_addr: new_addr, reuse_port: new_port, ..req });
                        assert_eq!(result.is_ok(), old_addr && new_addr || old_port && new_port);
                    }
                }
            }
        }
        let mut t = PortTable::try_new(1).unwrap();
        for n in 1..=2 {
            t.bind(id(n), Proto::Tcp, Bound { port: n as u16, ..Bound::default() }).unwrap();
            t.time_wait(id(n), Instant::from_secs(n as i64), None);
        }
        assert_eq!(t.len(), 1);
        assert_eq!(t.time_wait_ports().next(), Some((2, Instant::from_secs(2))));
        t.bind(id(3), Proto::Tcp, Bound { port: 1, ..Bound::default() }).unwrap();
    }
    #[test]
    fn reversed_bind_overlap_and_v6only_then_dual_listener() {
        // Linux v6.12 net/ipv4/inet_connection_sock.c,
        // inet_bind_conflict:172-199 and inet_rcv_saddr_equal family overlap.
        let v4 = Bound { addr: Some(crate::IpAddress::v4(127, 0, 0, 1)), port: 1234, ..Bound::default() };
        let v6 = Bound { addr: Some(crate::IpAddress::Ipv6(crate::Ipv6Address::LOCALHOST)), v6: true, ..v4 };
        let any4 = Bound { addr: None, ..v4 };
        let dual = Bound { v6: true, ..any4 };
        let only = Bound { v6only: true, ..dual };
        for (a, b, expected) in [
            (any4, v4, true),
            (dual, v4, true),
            (only, v4, false),
            (any4, v6, false),
            (dual, v6, true),
            (only, only, true),
            (only, dual, true),
            (any4, only, false),
        ] {
            for (a, b) in [(a, b), (b, a)] {
                let mut t = PortTable::try_new(2).unwrap();
                t.bind(id(0), Proto::Tcp, a).unwrap();
                assert_eq!(t.bind(id(1), Proto::Tcp, b).is_err(), expected);
            }
        }
        let mut t = PortTable::try_new(2).unwrap();
        t.bind(id(0), Proto::Tcp, only).unwrap();
        t.listen(id(0)).unwrap();
        assert_eq!(t.bind(id(1), Proto::Tcp, dual), Err(NetError::AddrInUse));
    }
    #[test]
    fn ping_identifiers_cover_one_through_65535() {
        // Linux v6.12 net/ipv4/ping.c, ping_get_port:86-104 skips zero.
        let mut t = PortTable::try_new(3).unwrap();
        let p = Proto::IcmpEcho { raw: false };
        assert_eq!(t.bind(id(0), p, Bound::default()).unwrap().port, 1);
        t.next[2] = 65535;
        assert_eq!(t.bind(id(1), p, Bound::default()).unwrap().port, 65535);
        assert_eq!(t.bind(id(2), p, Bound::default()).unwrap().port, 2);
    }
}
