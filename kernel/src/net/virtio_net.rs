//! Nonblocking virtio Ethernet with fixed DMA buffers (M9 B2, #473).
//! The device has one serialized caller; its IRQ only wakes that caller.
//! The shell owns it under `DEVICE` for B2; B3 will move it into
//! `NetStack::inner`, whose sleep mutex serializes the net thread.

use alloc::string::String;
use alloc::vec::Vec;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;

use lumen_net::{ETH_FRAME_MAX, NetDevice, NetInfo, TxError};
use smoltcp::wire::{ArpOperation, ArpPacket, ArpRepr, EthernetAddress, EthernetFrame, EthernetProtocol, Ipv4Address};
use virtio_drivers::device::net::VirtIONetRaw;
use virtio_drivers::{BufferDirection, Hal};

use crate::drivers::Driver;
use crate::pci::{DeviceInfo, DeviceResources};
use crate::sync::SleepMutex;
use crate::virtio::{self, Doorbell, ProbeError, StatusHandle, VirtioHal, VirtioTransport};

const QUEUE_SIZE: usize = 16;
const BUFFER_SIZE: usize = 2048;
/// A 30 s hang guard, not a completion-time requirement on a busy host.
const SELFTEST_TIMEOUT_NS: u64 = 30_000_000_000;
type Raw = VirtIONetRaw<VirtioHal, VirtioTransport, QUEUE_SIZE>;

pub const DRIVER: Driver = Driver { name: "virtio-net", matches, probe };

/// RX and TX completions wake the owner. B3's net thread can wait here,
/// and ring the same bell for work queued by syscalls.
pub static BELL: Doorbell = Doorbell::new();

// B2 shell calls hold this sleep mutex. The device itself has no lock.
static DEVICE: SleepMutex<DeviceSlot> = SleepMutex::new(DeviceSlot::Empty);

#[allow(clippy::large_enum_variant, reason = "one static slot; inline ownership needs no extra heap allocation")]
enum DeviceSlot {
    Empty,
    Probed(VirtioNetDevice),
    Taken,
}

impl DeviceSlot {
    fn as_mut(&mut self) -> Option<&mut VirtioNetDevice> {
        match self {
            Self::Probed(device) => Some(device),
            Self::Empty | Self::Taken => None,
        }
    }
}

fn on_irq() {
    BELL.ring();
}

fn matches(info: &DeviceInfo) -> bool {
    info.virtio_type() == Some(1)
}

fn probe(res: DeviceResources) -> Result<(), ProbeError> {
    // Probes run serially. Decline a second NIC before making its transport.
    if !matches!(*DEVICE.lock(), DeviceSlot::Empty) {
        return Err(ProbeError::NotMine);
    }
    let transport = virtio::transport(res, on_irq)?;
    let status = transport.status();
    let raw = Raw::new(transport).map_err(ProbeError::Device)?;
    let rx = buffers(BufferDirection::DeviceToDriver).map_err(ProbeError::Device)?;
    let tx = buffers(BufferDirection::DriverToDevice).map_err(ProbeError::Device)?;
    let mut device = VirtioNetDevice {
        raw: ManuallyDrop::new(raw),
        rx,
        tx,
        rx_tokens: [None; QUEUE_SIZE],
        tx_pending: [None; QUEUE_SIZE],
        status,
        health: Health::Up,
        counts: Counts::default(),
    };
    for slot in 0..QUEUE_SIZE {
        device.post_receive(slot).map_err(ProbeError::Device)?;
    }
    virtio::check_device(&status)?;
    let mut slot = DEVICE.lock();
    assert!(matches!(*slot, DeviceSlot::Empty));
    *slot = DeviceSlot::Probed(device);
    Ok(())
}

/// Transfers the boot-probed device to B3's stack, once.
#[allow(dead_code, reason = "B3 takes ownership when the net thread lands")]
pub fn take_device() -> Option<VirtioNetDevice> {
    let mut slot = DEVICE.lock();
    match core::mem::replace(&mut *slot, DeviceSlot::Taken) {
        DeviceSlot::Probed(device) => Some(device),
        DeviceSlot::Empty | DeviceSlot::Taken => None,
    }
}

/// One page owned through the Hal's DMA run table, of which 2048 bytes are
/// posted. Its containing device resets before this allocation is freed.
struct Buffer {
    phys: u64,
    ptr: NonNull<u8>,
}

// SAFETY: the allocation has one owning device, moved between threads only
// with exclusive access. No Rust references to a posted buffer are retained.
unsafe impl Send for Buffer {}

impl Buffer {
    fn new(direction: BufferDirection) -> virtio_drivers::Result<Self> {
        let (phys, ptr) = VirtioHal::dma_alloc(1, direction);
        if phys == 0 { Err(virtio_drivers::Error::DmaError) } else { Ok(Self { phys, ptr }) }
    }

    /// Releases a posted range after the owning device has been reset.
    fn unshare_posted(&self, size: Option<usize>, direction: BufferDirection) {
        if let Some(size) = size {
            // SAFETY: only VirtioNetDevice::drop calls this, after resetting
            // raw. The saved size and this buffer identify the posted range.
            unsafe {
                VirtioHal::unshare(self.phys, NonNull::slice_from_raw_parts(self.ptr, size), direction);
            }
        }
    }

    /// # Safety
    /// The device must have finished accessing this buffer, or the slice
    /// must be used only to return the original buffer to a completion call.
    unsafe fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: our allocation spans a page; the caller excludes device
        // access except for returning this same range to the raw queue API.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), BUFFER_SIZE) }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: these are the allocation's original values.
        // VirtioNetDevice::drop resets the raw device and unshares posted
        // buffers before its fields drop; fresh buffers were never posted.
        assert_eq!(unsafe { VirtioHal::dma_dealloc(self.phys, self.ptr, 1) }, 0);
    }
}

fn buffers(direction: BufferDirection) -> virtio_drivers::Result<[Buffer; QUEUE_SIZE]> {
    // Allocation only at probe, before any of these buffers are posted.
    let buffers: Vec<_> = (0..QUEUE_SIZE).map(|_| Buffer::new(direction)).collect::<Result<_, _>>()?;
    Ok(buffers.try_into().ok().expect("exactly QUEUE_SIZE buffers"))
}

#[derive(Clone, Copy, Default)]
pub struct Counts {
    pub received: u64,
    pub transmitted: u64,
    pub reclaimed: u64,
    pub rx_drops: u64,
    pub tx_drops: u64,
}

/// A single-descriptor-per-frame device with 16 RX and 16 TX buffers.
/// No operation allocates or waits for a device completion.
pub struct VirtioNetDevice {
    // Explicit Drop resets hardware and releases live shares before buffers.
    raw: ManuallyDrop<Raw>,
    rx: [Buffer; QUEUE_SIZE],
    tx: [Buffer; QUEUE_SIZE],
    rx_tokens: [Option<u16>; QUEUE_SIZE],
    // Tokens are descriptor ids, not buffer indices; completion order may vary.
    tx_pending: [Option<(u16, usize)>; QUEUE_SIZE],
    status: StatusHandle,
    health: Health,
    counts: Counts,
}

/// Failed recovers on a successful transmit; Broken is permanent because
/// a corrupt queue cannot safely reuse its buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Health {
    Up,
    Failed,
    Broken,
}

impl Drop for VirtioNetDevice {
    fn drop(&mut self) {
        // SAFETY: raw is dropped exactly once here, resetting the transport
        // before any buffers are freed. No method uses it after this point.
        unsafe { ManuallyDrop::drop(&mut self.raw) };
        // VirtQueue's drop does not unshare posted buffers. After reset no
        // device can access them; discharge those shares before Buffer::drop.
        for (buffer, token) in self.rx.iter().zip(self.rx_tokens) {
            buffer.unshare_posted(token.map(|_| BUFFER_SIZE), BufferDirection::DeviceToDriver);
        }
        for (buffer, pending) in self.tx.iter().zip(self.tx_pending) {
            buffer.unshare_posted(pending.map(|(_, size)| size), BufferDirection::DriverToDevice);
        }
    }
}

// A completed RX slot and the raw API's header/frame lengths.
type RxCompletion = (usize, virtio_drivers::Result<(usize, usize)>);

impl VirtioNetDevice {
    pub fn counts(&self) -> Counts {
        self.counts
    }

    pub fn tx_free(&self) -> usize {
        self.tx_pending.iter().filter(|slot| slot.is_none()).count()
    }

    fn fail(&mut self) {
        if self.health == Health::Up {
            self.health = Health::Failed;
            log::warn!(target: "net", "virtio-net device failed; link down (once per outage)");
        }
    }

    fn break_down(&mut self, reason: core::fmt::Arguments<'_>) {
        if self.health != Health::Broken {
            log::error!(target: "net", "virtio-net queue broken: {reason}");
            self.fail();
            self.health = Health::Broken;
        }
    }

    fn recover(&mut self) {
        debug_assert!(self.health != Health::Broken, "a broken queue cannot recover");
        self.health = Health::Up;
    }

    fn usable(&mut self) -> bool {
        if self.status.check_ready().is_err() {
            self.fail();
            return false;
        }
        self.health != Health::Broken
    }

    fn next_receive(&mut self) -> Option<RxCompletion> {
        let token = self.raw.poll_receive()?;
        let Some(slot) = self.rx_tokens.iter().position(|t| *t == Some(token)) else {
            self.counts.rx_drops += 1;
            self.break_down(format_args!("unknown RX token {token}"));
            return None;
        };
        // SAFETY: poll_receive returned this slot's posted token. Return the
        // exact original range; read it only after successful completion.
        let completed = unsafe { self.raw.receive_complete(token, self.rx[slot].bytes()) };
        self.finish_receive(slot, completed)
    }

    fn finish_receive(
        &mut self,
        slot: usize,
        completed: virtio_drivers::Result<(usize, usize)>,
    ) -> Option<RxCompletion> {
        // virtio-drivers 0.13 dev_raw.rs: receive_complete's IoError is a
        // short header AFTER pop_used unshares the buffer. Other errors have
        // not released it, so keep the token for teardown after reset.
        match &completed {
            Ok(_) | Err(virtio_drivers::Error::IoError) => {}
            Err(virtio_drivers::Error::WrongToken) => {
                self.counts.rx_drops += 1;
                self.break_down(format_args!("receive_complete slot {slot}: WrongToken"));
                return None;
            }
            // NotReady means the used ring is empty. Keep the posted token.
            Err(virtio_drivers::Error::NotReady) => return None,
            Err(_) => {
                self.fail();
                return None;
            }
        }
        self.rx_tokens[slot] = None;
        Some((slot, completed))
    }

    fn receive_with(
        &mut self,
        out: &mut [u8; ETH_FRAME_MAX],
        bell: &Doorbell,
        mut next: impl FnMut(&mut Self) -> Option<RxCompletion>,
    ) -> Option<usize> {
        if !self.usable() {
            return None;
        }
        // A malformed completion must not hide a later valid frame. Bound
        // the work even if hardware keeps filling the reposted buffers.
        for _ in 0..QUEUE_SIZE {
            let (slot, completed) = next(self)?;
            let valid = completed
                .ok()
                .filter(|&(header, len)| len <= ETH_FRAME_MAX && header <= BUFFER_SIZE && len <= BUFFER_SIZE - header);
            if let Some((header, len)) = valid {
                // SAFETY: next returned an unshared, completed RX buffer.
                out[..len].copy_from_slice(&unsafe { self.rx[slot].bytes() }[header..header + len]);
                self.counts.received += 1;
            } else {
                self.counts.rx_drops += 1;
            }
            if let Err(error) = self.post_receive(slot) {
                self.break_down(format_args!("RX repost slot {slot}: {error:?}"));
                return valid.map(|(_, len)| len);
            }
            if let Some((_, len)) = valid {
                return Some(len);
            }
        }
        // More completions may already be queued without another IRQ.
        // Schedule another poll instead of letting the owner sleep on them.
        bell.ring();
        None
    }

    // Separate from the debug assertion so ktests can exercise the release
    // rejection path without deliberately panicking the kernel.
    fn transmit_frame<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, TxError> {
        if len > ETH_FRAME_MAX {
            self.counts.tx_drops += 1;
            // TxError has no caller-error variant. DeviceFailed reports the
            // rejection, but a bad length must not change health or warnings.
            return Err(TxError::DeviceFailed);
        }
        if !self.usable() {
            self.counts.tx_drops += 1;
            return Err(TxError::DeviceFailed);
        }
        let Some(slot) = self.tx_pending.iter().position(Option::is_none) else {
            self.counts.tx_drops += 1;
            return Err(TxError::Full);
        };
        // SAFETY: only slots with no pending transmission may be written.
        let buffer = unsafe { self.tx[slot].bytes() };
        let header = self.raw.fill_buffer_header(buffer).expect("2048 bytes fit the virtio header");
        let result = f(&mut buffer[header..header + len]);
        let size = header + len;
        // SAFETY: the buffer remains alive and untouched until its token is
        // completed, or the raw device resets before dropping the buffers.
        match unsafe { self.raw.transmit_begin(&buffer[..size]) } {
            Ok(token) => {
                self.tx_pending[slot] = Some((token, size));
                self.counts.transmitted += 1;
                self.recover();
                Ok(result)
            }
            Err(virtio_drivers::Error::QueueFull) => {
                self.counts.tx_drops += 1;
                self.break_down(format_args!("transmit_begin QueueFull with free slot {slot}"));
                Err(TxError::DeviceFailed)
            }
            Err(_) => {
                self.counts.tx_drops += 1;
                self.fail();
                Err(TxError::DeviceFailed)
            }
        }
    }

    fn post_receive(&mut self, slot: usize) -> virtio_drivers::Result<()> {
        // SAFETY: this slot is fresh or completed; the allocation stays at
        // this address until completion or the raw device resets on drop.
        let token = unsafe { self.raw.receive_begin(self.rx[slot].bytes()) }?;
        self.rx_tokens[slot] = Some(token);
        Ok(())
    }
}

impl NetDevice for VirtioNetDevice {
    fn info(&self) -> NetInfo {
        NetInfo {
            mac: self.raw.mac_address(),
            mtu: 1500,
            link_up: self.health == Health::Up && self.status.check_ready().is_ok(),
        }
    }

    fn tx_ready(&self) -> bool {
        // Raw::can_send requires two descriptors, but we use exactly one.
        self.health != Health::Broken && self.status.check_ready().is_ok() && self.tx_free() != 0
    }

    /// Copies and reposts one valid frame. `None` also covers an unusable
    /// device or exhausting the QUEUE_SIZE malformed-frame skip budget;
    /// after that bound it does not mean the ring is empty. BELL is rung
    /// to request another poll before the owner sleeps.
    fn receive(&mut self, out: &mut [u8; ETH_FRAME_MAX]) -> Option<usize> {
        self.receive_with(out, &BELL, Self::next_receive)
    }

    fn transmit<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, TxError> {
        debug_assert!(len <= ETH_FRAME_MAX, "oversized Ethernet frame");
        self.transmit_frame(len, f)
    }

    fn reclaim(&mut self) -> usize {
        if !self.usable() {
            return 0;
        }
        let mut reclaimed = 0;
        // Bounded even if the device misbehaves. No new sends in this call.
        // Committed frames already count in transmitted. A completion error
        // cannot tell us whether they reached the wire, so neither error
        // branch counts them again as drops.
        for _ in 0..QUEUE_SIZE {
            let Some(token) = self.raw.poll_transmit() else { break };
            let Some(slot) = self.tx_pending.iter().position(|p| p.is_some_and(|(t, _)| t == token)) else {
                self.break_down(format_args!("unknown TX token {token}"));
                break;
            };
            let (_, size) = self.tx_pending[slot].expect("found token");
            // SAFETY: this completed token owns this exact address and
            // length, unchanged since transmit_begin.
            if let Err(error) = unsafe { self.raw.transmit_complete(token, &self.tx[slot].bytes()[..size]) } {
                self.break_down(format_args!("transmit_complete token {token}: {error:?}"));
                break;
            }
            self.tx_pending[slot] = None;
            reclaimed += 1;
        }
        self.counts.reclaimed += reclaimed as u64;
        reclaimed
    }
}

const SELF_IP: Ipv4Address = Ipv4Address::new(10, 0, 2, 15);
const GATEWAY: Ipv4Address = Ipv4Address::new(10, 0, 2, 2);
const ARP_FRAME_LEN: usize = 60; // Ethernet minimum, no FCS; zero-padded.

fn arp_request(out: &mut [u8], mac: [u8; 6]) {
    out.fill(0);
    let mut ethernet = EthernetFrame::new_unchecked(out);
    ethernet.set_dst_addr(EthernetAddress::BROADCAST);
    ethernet.set_src_addr(EthernetAddress(mac));
    ethernet.set_ethertype(EthernetProtocol::Arp);
    ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Request,
        source_hardware_addr: EthernetAddress(mac),
        source_protocol_addr: SELF_IP,
        target_hardware_addr: EthernetAddress::default(),
        target_protocol_addr: GATEWAY,
    }
    .emit(&mut ArpPacket::new_unchecked(ethernet.payload_mut()));
}

fn arp_reply(frame: &[u8], mac: [u8; 6]) -> Option<[u8; 6]> {
    let ethernet = EthernetFrame::new_checked(frame).ok()?;
    if ethernet.ethertype() != EthernetProtocol::Arp || ethernet.dst_addr() != EthernetAddress(mac) {
        return None;
    }
    let packet = ArpPacket::new_checked(ethernet.payload()).ok()?;
    let ArpRepr::EthernetIpv4 {
        operation: ArpOperation::Reply,
        source_hardware_addr,
        source_protocol_addr: GATEWAY,
        target_hardware_addr,
        target_protocol_addr: SELF_IP,
    } = ArpRepr::parse(&packet).ok()?
    else {
        return None;
    };
    (target_hardware_addr == EthernetAddress(mac) && source_hardware_addr == ethernet.src_addr())
        .then_some(source_hardware_addr.0)
}

const NO_REPLY: &str = "no ARP reply from 10.0.2.2";

fn mac_text([a, b, c, d, e, f]: [u8; 6]) -> String {
    alloc::format!("{a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}")
}

fn selftest(device: &mut VirtioNetDevice) -> Result<[u8; 6], String> {
    let mac = device.info().mac;
    device.reclaim();
    device
        .transmit(ARP_FRAME_LEN, |out| arp_request(out, mac))
        .map_err(|error| alloc::format!("transmit failed: {error:?}"))?;
    let mut reply = None;
    let mut frame = [0; ETH_FRAME_MAX];
    let status = device.status;
    status
        .wait_for(&BELL, SELFTEST_TIMEOUT_NS, || {
            device.reclaim();
            for _ in 0..QUEUE_SIZE {
                let Some(len) = device.receive(&mut frame) else { break };
                if let Some(mac) = arp_reply(&frame[..len], mac) {
                    reply = Some(mac);
                    return true;
                }
            }
            !device.info().link_up
        })
        .map_err(|error| alloc::format!("{NO_REPLY}: {error}"))?;
    if !device.info().link_up {
        return Err(match device.status.check_ready() {
            Err(error) => alloc::format!("{NO_REPLY}: link down ({error})"),
            Ok(()) => alloc::format!("{NO_REPLY}: link down ({:?})", device.health),
        });
    }
    reply.ok_or_else(|| NO_REPLY.into())
}

/// B2's device status and direct ARP selftest; shell output is written only
/// after releasing the device owner lock.
pub fn command(ctx: &mut crate::shell::Ctx<'_>, args: &[String]) {
    let test = match args {
        [] => false,
        [arg] if arg == "selftest" => true,
        _ => return ctx.fail("usage: net [selftest]"),
    };
    let mut guard = DEVICE.lock();
    let Some(device) = guard.as_mut() else {
        drop(guard);
        return ctx.fail("net: no device");
    };
    if test {
        let result = selftest(device);
        drop(guard);
        match result {
            Ok(mac) => {
                let _ = writeln!(ctx.out, "net selftest: ARP reply 10.0.2.2 is {}", mac_text(mac));
            }
            Err(why) => ctx.fail(alloc::format!("net selftest: {why}")),
        }
        return;
    }
    device.reclaim();
    let info = device.info();
    let counts = device.counts();
    let free = device.tx_free();
    let health = device.health;
    drop(guard);
    let mac = mac_text(info.mac);
    let link = if info.link_up { "up" } else { "down" };
    let _ = writeln!(ctx.out, "net: mac {mac}, link {link} ({health:?}), mtu {}", info.mtu);
    let _ = writeln!(
        ctx.out,
        "net: rx {}, tx {}, rx_drops {}, tx_drops {}, tx_free {free}/{QUEUE_SIZE}, reclaimed {}, irqs {}",
        counts.received,
        counts.transmitted,
        counts.rx_drops,
        counts.tx_drops,
        counts.reclaimed,
        BELL.rings()
    );
}

#[cfg(feature = "ktest")]
fn drain_tx(dev: &mut VirtioNetDevice) -> Result<(), &'static str> {
    let status = dev.status;
    status
        .wait_for(&BELL, SELFTEST_TIMEOUT_NS, || {
            dev.reclaim();
            dev.tx_free() == QUEUE_SIZE
        })
        .map_err(|_| "TX did not complete")
}

crate::ktest!(
    fn virtio_net_fills_and_reclaims_all_slots_without_dma_allocation() {
        use crate::{kassert, kassert_eq};
        let mut guard = DEVICE.lock();
        let dev = guard.as_mut().ok_or("no net device")?;
        let mac = dev.info().mac;
        let runs = virtio::outstanding_runs();
        let start = dev.counts();
        for _ in 0..2 {
            for _ in 0..QUEUE_SIZE {
                kassert!(dev.tx_ready());
                dev.transmit(ARP_FRAME_LEN, |out| arp_request(out, mac)).map_err(|_| "send failed")?;
                kassert_eq!(virtio::outstanding_runs(), runs);
            }
            kassert!(!dev.tx_ready());
            kassert_eq!(dev.tx_free(), 0);
            let mut called = false;
            kassert_eq!(dev.transmit(ARP_FRAME_LEN, |_| called = true), Err(TxError::Full));
            kassert!(!called && dev.info().link_up);
            drain_tx(dev)?;
            kassert!(dev.tx_ready());
        }
        kassert_eq!(dev.counts().transmitted - start.transmitted, 32);
        kassert_eq!(dev.counts().reclaimed - start.reclaimed, 32);
        kassert_eq!(dev.counts().tx_drops - start.tx_drops, 2);
        kassert_eq!(virtio::outstanding_runs(), runs);
        Ok(())
    }
);

crate::ktest!(
    fn virtio_net_failure_counts_drops_and_recovers_on_transmit() {
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::AtomicU8;
        // A status-register seam, not a reset of live hardware: posted DMA
        // stays owned until the real device completes it.
        static FAILED: AtomicU8 = AtomicU8::new(0x8f);
        let mut guard = DEVICE.lock();
        let dev = guard.as_mut().ok_or("no net device")?;
        let actual = dev.status;
        let start = dev.counts();
        dev.status = StatusHandle::ktest_reading(&FAILED, None);
        let mut calls = 0;
        let first = dev.transmit(ARP_FRAME_LEN, |_| calls += 1);
        let second = dev.transmit(ARP_FRAME_LEN, |_| calls += 1);
        let down = !dev.info().link_up && !dev.tx_ready();
        dev.status = actual;
        kassert_eq!(first, Err(TxError::DeviceFailed));
        kassert_eq!(second, Err(TxError::DeviceFailed));
        kassert_eq!(calls, 0);
        kassert!(down && !dev.info().link_up);
        kassert_eq!(dev.counts().tx_drops - start.tx_drops, 2);
        let mac = dev.info().mac;
        dev.reclaim();
        dev.transmit(ARP_FRAME_LEN, |out| arp_request(out, mac)).map_err(|_| "recovery send failed")?;
        kassert!(dev.info().link_up);
        drain_tx(dev)?;
        kassert_eq!(dev.health, Health::Up);
        // Each entry point independently detects a new outage. A repeated
        // call in that outage stays quiet; a successful TX returns to Up.
        for receive in [true, false] {
            dev.status = StatusHandle::ktest_reading(&FAILED, None);
            let mut out = [0; ETH_FRAME_MAX];
            let no_progress = if receive {
                dev.receive(&mut out).is_none() && dev.receive(&mut out).is_none()
            } else {
                dev.reclaim() == 0 && dev.reclaim() == 0
            };
            let failed = dev.health;
            dev.status = actual;
            kassert!(no_progress);
            kassert_eq!(failed, Health::Failed);
            kassert_eq!(dev.counts().tx_drops - start.tx_drops, 2);
            dev.transmit(ARP_FRAME_LEN, |out| arp_request(out, mac)).map_err(|_| "recovery send failed")?;
            kassert!(dev.info().link_up);
            kassert_eq!(dev.health, Health::Up);
            drain_tx(dev)?;
        }
        drop(guard);
        let mut log = alloc::vec![0; crate::klog::RING_BYTES];
        let (len, _) = crate::klog::copy_recent(&mut log);
        let log = core::str::from_utf8(&log[..len]).map_err(|_| "invalid log")?;
        kassert_eq!(log.matches("WARN  net: virtio-net device failed; link down (once per outage)\n").count(), 3);
        Ok(())
    }
);

crate::ktest!(
    fn virtio_net_oversized_transmit_preserves_health() {
        use crate::{kassert, kassert_eq};
        let mut guard = DEVICE.lock();
        let dev = guard.as_mut().ok_or("no net device")?;
        let start = dev.counts();
        let health = dev.health;
        let free = dev.tx_free();
        let mut called = false;
        // Exercise the runtime guard below transmit's debug assertion.
        kassert_eq!(dev.transmit_frame(ETH_FRAME_MAX + 1, |_| called = true), Err(TxError::DeviceFailed));
        kassert!(!called);
        kassert_eq!(dev.health, health);
        kassert_eq!(dev.tx_free(), free);
        kassert_eq!(dev.counts().transmitted, start.transmitted);
        kassert_eq!(dev.counts().tx_drops, start.tx_drops + 1);
        kassert!(dev.info().link_up);
        Ok(())
    }
);

crate::ktest!(
    fn virtio_net_receive_skips_bad_completions_and_reposts() {
        use crate::{kassert, kassert_eq};
        let mut guard = DEVICE.lock();
        let dev = guard.as_mut().ok_or("no net device")?;
        drain_tx(dev)?;
        let mac = dev.info().mac;
        let mut out = [0; ETH_FRAME_MAX];
        while dev.receive(&mut out).is_some() {}
        for bad in [
            Ok((12, ETH_FRAME_MAX + 1)),
            Ok((BUFFER_SIZE + 1, 0)),
            Ok((BUFFER_SIZE - 1, 2)),
            Err(virtio_drivers::Error::IoError),
        ] {
            // Get two real completed, unshared RX buffers. Only the returned
            // metadata and completed payload are seamed; no posted DMA or
            // ring contents are modified.
            for _ in 0..2 {
                dev.transmit(ARP_FRAME_LEN, |out| arp_request(out, mac)).map_err(|_| "send failed")?;
            }
            let mut completed = [None, None];
            let mut count = 0;
            let status = dev.status;
            status
                .wait_for(&BELL, SELFTEST_TIMEOUT_NS, || {
                    while count < completed.len() {
                        let Some(next) = dev.next_receive() else { break };
                        completed[count] = Some(next);
                        count += 1;
                    }
                    count == completed.len()
                })
                .map_err(|_| "RX did not complete")?;
            let (first_slot, _) = completed[0].take().ok_or("first RX missing")?;
            let (second_slot, good) = completed[1].take().ok_or("second RX missing")?;
            let (header, _) = good.map_err(|_| "good RX failed")?;
            let len = ETH_FRAME_MAX;
            let expected: [u8; ETH_FRAME_MAX] = core::array::from_fn(|i| i as u8);
            // SAFETY: next_receive completed and unshared this RX slot.
            // The maximum Ethernet frame and its header fit this buffer.
            (unsafe { dev.rx[second_slot].bytes() })[header..header + len].copy_from_slice(&expected);
            let start = dev.counts();
            let mut script = [(first_slot, bad), (second_slot, Ok((header, len)))].into_iter();
            kassert_eq!(
                dev.receive_with(&mut out, &BELL, |dev| {
                    let (slot, result) = script.next()?;
                    dev.finish_receive(slot, result)
                }),
                Some(len)
            );
            kassert!(script.next().is_none());
            kassert_eq!(&out[..len], &expected[..len]);
            kassert_eq!(dev.counts().rx_drops, start.rx_drops + 1);
            kassert_eq!(dev.counts().received, start.received + 1);
            kassert!(dev.rx_tokens[first_slot].is_some() && dev.rx_tokens[second_slot].is_some());
            kassert!(dev.info().link_up);
            drain_tx(dev)?;
        }
        Ok(())
    }
);

crate::ktest!(
    fn virtio_net_receive_budget_rings_for_another_poll() {
        use crate::{kassert, kassert_eq};
        let mut guard = DEVICE.lock();
        let dev = guard.as_mut().ok_or("no net device")?;
        drain_tx(dev)?;
        let mut out = [0; ETH_FRAME_MAX];
        while dev.receive(&mut out).is_some() {}
        let mac = dev.info().mac;
        for _ in 0..QUEUE_SIZE {
            dev.transmit(ARP_FRAME_LEN, |out| arp_request(out, mac)).map_err(|_| "send failed")?;
        }
        // Hold completed, unshared buffers for a deterministic run of bad
        // metadata. Never fabricate a completion for a posted DMA buffer.
        let mut slots = [0; QUEUE_SIZE];
        let mut count = 0;
        let status = dev.status;
        status
            .wait_for(&BELL, SELFTEST_TIMEOUT_NS, || {
                while count < QUEUE_SIZE {
                    let Some((slot, _)) = dev.next_receive() else { break };
                    slots[count] = slot;
                    count += 1;
                }
                count == QUEUE_SIZE
            })
            .map_err(|_| "RX did not complete")?;
        drain_tx(dev)?;
        let start = dev.counts();
        // Production passes BELL; a private bell excludes unrelated IRQs
        // from this exact wakeup assertion on a busy host.
        let bell = Doorbell::new();
        let mut script = slots.into_iter();
        let received = dev.receive_with(&mut out, &bell, |dev| {
            let slot = script.next()?;
            dev.finish_receive(slot, Err(virtio_drivers::Error::IoError))
        });
        kassert_eq!(received, None);
        kassert!(script.next().is_none());
        kassert_eq!(bell.rings(), 1);
        kassert_eq!(dev.counts().rx_drops, start.rx_drops + QUEUE_SIZE as u64);
        kassert_eq!(dev.counts().received, start.received);
        kassert!(dev.rx_tokens.iter().all(Option::is_some));
        kassert_eq!(dev.health, Health::Up);
        // An empty ring must not request another poll forever.
        kassert_eq!(dev.receive_with(&mut out, &bell, |_| None), None);
        kassert_eq!(bell.rings(), 1);
        Ok(())
    }
);
