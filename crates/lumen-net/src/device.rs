//! Nonblocking frame devices and a fixed-capacity IP loopback ring.
use crate::{NetError, memory::filled};
use alloc::vec::Vec;
use smoltcp::phy::{self, Device, DeviceCapabilities, Medium};
use smoltcp::time::Instant;

/// Ethernet bytes excluding FCS and VLAN tags.
pub const ETH_FRAME_MAX: usize = 1514;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Hardware information sampled at the start of a poll round.
pub struct NetInfo {
    /// Unicast Ethernet hardware address.
    pub mac: [u8; 6],
    /// IP MTU, excluding the Ethernet header.
    pub mtu: usize,
    /// Current link indication, adjusted for device transmit failure.
    pub link_up: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// A frame could not be committed after acquiring its transmit token.
pub enum TxError {
    /// The acquired token could not be committed; counted as a dropped frame.
    Full,
    /// Hardware failure; count the drop and mark the link down.
    DeviceFailed,
}
/// A frame device with one serialized caller. No operation may block.
pub trait NetDevice: Send {
    /// Current hardware address, IP MTU and link indication.
    fn info(&self) -> NetInfo;
    /// Whether a transmit slot is available without waiting.
    fn tx_ready(&self) -> bool;
    /// Copies a frame and reposts the receive buffer before returning.
    fn receive(&mut self, buf: &mut [u8; ETH_FRAME_MAX]) -> Option<usize>;
    /// Writes one frame into a free slot; never waits for completion.
    fn transmit<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, TxError>;
    /// Reclaims completed transmissions and returns their count.
    fn reclaim(&mut self) -> usize;
}

struct Ring {
    data: Vec<u8>,
    lengths: Vec<usize>,
    head: usize,
    len: usize,
    mtu: usize,
}
impl Ring {
    fn is_full(&self) -> bool {
        self.len == self.lengths.len()
    }
    fn pop(&mut self, out: &mut [u8]) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let n = self.lengths[self.head];
        let start = self.head * self.mtu;
        out[..n].copy_from_slice(&self.data[start..start + n]);
        self.head = (self.head + 1) % self.lengths.len();
        self.len -= 1;
        Some(n)
    }
    fn push<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> R {
        assert!(self.len < self.lengths.len() && len <= self.mtu);
        let index = (self.head + self.len) % self.lengths.len();
        let result = f(&mut self.data[index * self.mtu..index * self.mtu + len]);
        self.lengths[index] = len;
        self.len += 1;
        result
    }
}
/// FIFO loopback whose receive token always leaves room for a reply.
pub struct Loopback {
    pub(crate) inner: LoopDevice,
}
impl Loopback {
    /// Allocates a fixed ring with the specified IP MTU and frame capacity.
    pub fn try_new(mtu: usize, slots: usize) -> Result<Self, NetError> {
        Ok(Self { inner: LoopDevice::try_new(mtu, slots)? })
    }
    /// Number of queued loopback frames.
    pub fn queued(&self) -> usize {
        self.inner.queued()
    }
    /// Whether the fixed loopback ring has no free frame slot.
    pub fn full(&self) -> bool {
        self.inner.full()
    }
}
pub(crate) struct LoopDevice {
    ring: Ring,
    rx: Vec<u8>,
    was_full: bool,
    pub(crate) sent: Vec<crate::packet::TxTrace>,
}
impl LoopDevice {
    /// Allocates a fixed ring with the specified IP MTU and frame capacity.
    pub fn try_new(mtu: usize, slots: usize) -> Result<Self, NetError> {
        if mtu == 0 || mtu > 65535 || slots == 0 {
            return Err(NetError::Inval);
        }
        Ok(Self {
            ring: Ring {
                data: filled(mtu.checked_mul(slots).ok_or(NetError::NoMem)?, || 0)?,
                lengths: filled(slots, || 0)?,
                head: 0,
                len: 0,
                mtu,
            },
            rx: filled(mtu, || 0)?,
            was_full: false,
            sent: crate::memory::reserved(crate::NET_SOCKETS)?,
        })
    }
    /// Number of queued loopback frames.
    pub fn queued(&self) -> usize {
        self.ring.len
    }
    /// Whether the fixed loopback ring has no free frame slot.
    pub fn full(&self) -> bool {
        self.ring.is_full()
    }
    pub(crate) fn take_full(&mut self) -> bool {
        core::mem::take(&mut self.was_full)
    }
}
pub(crate) struct Rx<'a> {
    data: &'a [u8],
}
impl phy::RxToken for Rx<'_> {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        f(self.data)
    }
}
pub(crate) struct LoopTx<'a> {
    ring: &'a mut Ring,
    sent: &'a mut Vec<crate::packet::TxTrace>,
}
impl phy::TxToken for LoopTx<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        self.ring.push(len, |out| {
            let result = f(out);
            if let Some(p) = crate::packet::tcp_packet(out, false)
                && self.sent.len() < self.sent.capacity()
                && !self.sent.iter().any(|t| t.tuple == p.tuple && t.probe == p.probe)
            {
                self.sent.push(crate::packet::TxTrace { tuple: p.tuple, probe: p.probe });
            }
            result
        })
    }
}
impl Device for LoopDevice {
    type RxToken<'a> = Rx<'a>;
    type TxToken<'a> = LoopTx<'a>;
    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ip;
        caps.max_transmission_unit = self.ring.mtu;
        caps
    }
    fn receive(&mut self, _: Instant) -> Option<(Rx<'_>, LoopTx<'_>)> {
        if self.ring.len == 0 {
            return None;
        }
        self.was_full |= self.ring.is_full();
        let n = self.ring.pop(&mut self.rx)?;
        Some((Rx { data: &self.rx[..n] }, LoopTx { ring: &mut self.ring, sent: &mut self.sent }))
    }
    fn transmit(&mut self, _: Instant) -> Option<LoopTx<'_>> {
        if self.ring.is_full() {
            self.was_full = true;
            None
        } else {
            Some(LoopTx { ring: &mut self.ring, sent: &mut self.sent })
        }
    }
}

/// Ethernet token adapter; refreshes device information once per poll round.
pub(crate) struct Adapter<D: NetDevice> {
    pub device: D,
    info: NetInfo,
    rx: [u8; ETH_FRAME_MAX],
    pub failed_transmits: usize,
    failed: bool,
    pub(crate) sent: Vec<crate::packet::TxTrace>,
}
impl<D: NetDevice> Adapter<D> {
    #[cfg(test)]
    /// Allocates a fixed ring with the specified IP MTU and frame capacity.
    pub fn try_new(device: D) -> Result<Self, NetError> {
        Ok(Self::new(device, crate::memory::reserved(crate::NET_SOCKETS)?))
    }
    pub(crate) fn new(device: D, sent: Vec<crate::packet::TxTrace>) -> Self {
        let info = device.info();
        Self { device, info, rx: [0; ETH_FRAME_MAX], failed_transmits: 0, failed: false, sent }
    }
    pub fn refresh(&mut self) {
        self.info = self.device.info();
        if self.info.link_up {
            self.failed = false;
        }
        self.device.reclaim();
        self.sent.clear();
    }
    pub fn info(&self) -> NetInfo {
        NetInfo { link_up: self.info.link_up && !self.failed, ..self.info }
    }
    pub fn tx_ready(&self) -> bool {
        self.device.tx_ready()
    }
}
pub(crate) struct EthTx<'a, D> {
    device: &'a mut D,
    drops: &'a mut usize,
    failed: &'a mut bool,
    link_up: &'a mut bool,
    sent: &'a mut Vec<crate::packet::TxTrace>,
}
impl<D: NetDevice> phy::TxToken for EthTx<'_, D> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        // Device failure can occur before invoking f. Run it into a bounded
        // scratch buffer first, so smoltcp's token callback still runs once.
        let mut frame = [0; ETH_FRAME_MAX];
        let result = f(&mut frame[..len]);
        match self.device.transmit(len, |out| out.copy_from_slice(&frame[..len])) {
            Ok(()) => {
                *self.failed = false;
                *self.link_up = true;
                if let Some(p) = crate::packet::tcp_packet(&frame[..len], true)
                    && self.sent.len() < self.sent.capacity()
                    && !self.sent.iter().any(|t| t.tuple == p.tuple && t.probe == p.probe)
                {
                    self.sent.push(crate::packet::TxTrace { tuple: p.tuple, probe: p.probe });
                }
            }
            Err(TxError::Full) => *self.drops += 1,
            Err(TxError::DeviceFailed) => {
                *self.drops += 1;
                *self.failed = true;
            }
        }
        result
    }
}
impl<D: NetDevice> Device for Adapter<D> {
    type RxToken<'a>
        = Rx<'a>
    where
        D: 'a;
    type TxToken<'a>
        = EthTx<'a, D>
    where
        D: 'a;
    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = self.info.mtu.min(1500) + 14;
        caps
    }
    fn receive(&mut self, _: Instant) -> Option<(Rx<'_>, EthTx<'_, D>)> {
        if !self.tx_ready() {
            return None;
        }
        let len = self.device.receive(&mut self.rx)?;
        if len > ETH_FRAME_MAX {
            return None;
        }
        Some((
            Rx { data: &self.rx[..len] },
            EthTx {
                device: &mut self.device,
                drops: &mut self.failed_transmits,
                failed: &mut self.failed,
                link_up: &mut self.info.link_up,
                sent: &mut self.sent,
            },
        ))
    }
    fn transmit(&mut self, _: Instant) -> Option<EthTx<'_, D>> {
        if !self.tx_ready() {
            return None;
        }
        Some(EthTx {
            device: &mut self.device,
            drops: &mut self.failed_transmits,
            failed: &mut self.failed,
            link_up: &mut self.info.link_up,
            sent: &mut self.sent,
        })
    }
}

/// Supplies an already classified frame to smoltcp, with the real TX token.
pub(crate) struct OneFrame<'a, D> {
    pub device: &'a mut D,
    pub frame: Option<&'a [u8]>,
}
impl<D: Device> Device for OneFrame<'_, D> {
    type RxToken<'a>
        = Rx<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = D::TxToken<'a>
    where
        Self: 'a;
    fn capabilities(&self) -> DeviceCapabilities {
        self.device.capabilities()
    }
    fn receive(&mut self, now: Instant) -> Option<(Rx<'_>, D::TxToken<'_>)> {
        let data = self.frame.take()?;
        let tx = self.device.transmit(now)?;
        Some((Rx { data }, tx))
    }
    fn transmit(&mut self, now: Instant) -> Option<D::TxToken<'_>> {
        self.device.transmit(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smoltcp::phy::TxToken;
    struct DeviceStub {
        up: bool,
        error: Option<TxError>,
        ready: bool,
    }
    impl NetDevice for DeviceStub {
        fn info(&self) -> NetInfo {
            NetInfo { mac: [2, 0, 0, 0, 0, 1], mtu: 1400, link_up: self.up }
        }
        fn tx_ready(&self) -> bool {
            self.ready
        }
        fn receive(&mut self, _: &mut [u8; ETH_FRAME_MAX]) -> Option<usize> {
            None
        }
        fn reclaim(&mut self) -> usize {
            0
        }
        fn transmit<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, TxError> {
            if let Some(e) = self.error {
                return Err(e);
            };
            Ok(f(&mut [0; ETH_FRAME_MAX][..len]))
        }
    }
    #[test]
    fn device_backpressure_and_failure_are_bounded_and_recoverable() {
        let mut a = Adapter::try_new(DeviceStub { up: true, error: None, ready: false }).unwrap();
        assert_eq!(a.capabilities().max_transmission_unit, 1414);
        assert!(a.transmit(Instant::ZERO).is_none());
        assert!(a.receive(Instant::ZERO).is_none());
        a.device.ready = true;
        a.device.error = Some(TxError::Full);
        let mut calls = 0;
        a.transmit(Instant::ZERO).unwrap().consume(14, |_| calls += 1);
        assert_eq!(calls, 1);
        assert!(a.info().link_up);
        assert_eq!(a.failed_transmits, 1);
        a.device.error = Some(TxError::DeviceFailed);
        a.transmit(Instant::ZERO).unwrap().consume(14, |_| ());
        assert!(!a.info().link_up);
        assert_eq!(a.failed_transmits, 2);
        a.device.up = false;
        a.refresh();
        assert!(!a.info().link_up);
        a.device.error = None;
        a.transmit(Instant::ZERO).unwrap().consume(14, |_| ());
        assert!(!a.failed);
        assert!(a.info().link_up);
        a.device.up = true;
        a.refresh();
        assert!(a.info().link_up);
    }
}
