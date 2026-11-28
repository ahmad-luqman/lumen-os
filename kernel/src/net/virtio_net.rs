//! Nonblocking virtio Ethernet with fixed DMA buffers (M9 B2, #473).
//! The device has one serialized caller; its IRQ only wakes that caller.
//! B3 moves it from the temporary shell owner into `NetStack::inner`.

use alloc::string::String;
use alloc::vec::Vec;
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
const SELFTEST_TIMEOUT_NS: u64 = 5_000_000_000;
type Raw = VirtIONetRaw<VirtioHal, VirtioTransport, QUEUE_SIZE>;

pub const DRIVER: Driver = Driver { name: "virtio-net", matches, probe };

/// RX and TX completions wake the owner. B3's net thread can wait here,
/// and ring the same bell for work queued by syscalls.
pub static BELL: Doorbell = Doorbell::new();

// B2 alone drives the device from the shell. No lock lives in the driver.
static DEVICE: SleepMutex<Option<VirtioNetDevice>> = SleepMutex::new(None);

fn on_irq() {
    BELL.ring();
}

fn matches(info: &DeviceInfo) -> bool {
    info.virtio_type() == Some(1)
}

fn probe(res: DeviceResources) -> Result<(), ProbeError> {
    // Probes run serially. Decline a second NIC before making its transport.
    if DEVICE.lock().is_some() {
        return Err(ProbeError::NotMine);
    }
    let transport = virtio::transport(res, on_irq)?;
    let status = transport.status();
    let raw = Raw::new(transport).map_err(ProbeError::Device)?;
    let mut device = VirtioNetDevice {
        raw,
        rx: buffers(BufferDirection::DeviceToDriver).map_err(ProbeError::Device)?,
        tx: buffers(BufferDirection::DriverToDevice).map_err(ProbeError::Device)?,
        rx_tokens: [None; QUEUE_SIZE],
        tx_pending: [None; QUEUE_SIZE],
        status,
        failed: false,
        warned: false,
        broken: false,
        counts: Counts::default(),
    };
    for slot in 0..QUEUE_SIZE {
        device.post_receive(slot).map_err(ProbeError::Device)?;
    }
    virtio::check_device(&status)?;
    *DEVICE.lock() = Some(device);
    Ok(())
}

/// Transfers the boot-probed device to B3's stack, once.
#[allow(dead_code, reason = "B3 takes ownership when the net thread lands")]
pub fn take_device() -> Option<VirtioNetDevice> {
    DEVICE.lock().take()
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
        // SAFETY: these are the allocation's original values. The raw
        // device is the first field and resets before any Buffer is dropped.
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
    // Drop order is essential: reset hardware before releasing its buffers.
    raw: Raw,
    rx: [Buffer; QUEUE_SIZE],
    tx: [Buffer; QUEUE_SIZE],
    rx_tokens: [Option<u16>; QUEUE_SIZE],
    // Tokens are descriptor ids, not buffer indices; completion order may vary.
    tx_pending: [Option<(u16, usize)>; QUEUE_SIZE],
    status: StatusHandle,
    failed: bool,
    warned: bool,
    // A corrupt completion cannot safely be retried or reuse its buffer.
    broken: bool,
    counts: Counts,
}

impl VirtioNetDevice {
    pub fn counts(&self) -> Counts {
        self.counts
    }

    pub fn tx_free(&self) -> usize {
        self.tx_pending.iter().filter(|slot| slot.is_none()).count()
    }

    fn fail(&mut self) {
        self.failed = true;
        if !self.warned {
            self.warned = true;
            log::warn!(target: "net", "virtio-net device failed; link down (said once)");
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
            link_up: !self.failed && !self.broken && self.status.check_ready().is_ok(),
        }
    }

    fn tx_ready(&self) -> bool {
        // Raw::can_send requires two descriptors, but we use exactly one.
        !self.broken && self.status.check_ready().is_ok() && self.tx_free() != 0
    }

    fn receive(&mut self, out: &mut [u8; ETH_FRAME_MAX]) -> Option<usize> {
        if self.broken || self.status.check_ready().is_err() {
            return None;
        }
        let token = self.raw.poll_receive()?;
        let Some(slot) = self.rx_tokens.iter().position(|t| *t == Some(token)) else {
            self.counts.rx_drops += 1;
            self.broken = true;
            self.fail();
            return None;
        };
        // SAFETY: poll_receive returned this slot's posted token. The exact
        // original range is returned to the raw API, and read only afterwards.
        let completed = unsafe { self.raw.receive_complete(token, self.rx[slot].bytes()) };
        self.rx_tokens[slot] = None;
        let Ok((header, len)) = completed else {
            self.counts.rx_drops += 1;
            self.broken = true;
            self.fail();
            return None;
        };
        let valid = len <= ETH_FRAME_MAX && header <= BUFFER_SIZE && len <= BUFFER_SIZE - header;
        if valid {
            // SAFETY: receive_complete returned ownership of this range.
            out[..len].copy_from_slice(&unsafe { self.rx[slot].bytes() }[header..header + len]);
            self.counts.received += 1;
        } else {
            self.counts.rx_drops += 1;
        }
        if self.post_receive(slot).is_err() {
            self.broken = true;
            self.fail();
        }
        valid.then_some(len)
    }

    fn transmit<R>(&mut self, len: usize, f: impl FnOnce(&mut [u8]) -> R) -> Result<R, TxError> {
        if self.broken || self.status.check_ready().is_err() || len > ETH_FRAME_MAX {
            self.counts.tx_drops += 1;
            self.fail();
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
                self.failed = false;
                Ok(result)
            }
            Err(virtio_drivers::Error::QueueFull) => {
                self.counts.tx_drops += 1;
                Err(TxError::Full)
            }
            Err(_) => {
                self.counts.tx_drops += 1;
                self.fail();
                Err(TxError::DeviceFailed)
            }
        }
    }

    fn reclaim(&mut self) -> usize {
        if self.broken || self.status.check_ready().is_err() {
            return 0;
        }
        let mut reclaimed = 0;
        // Bounded even if the device misbehaves. No new sends in this call.
        for _ in 0..QUEUE_SIZE {
            let Some(token) = self.raw.poll_transmit() else { break };
            let Some(slot) = self.tx_pending.iter().position(|p| p.is_some_and(|(t, _)| t == token)) else {
                self.broken = true;
                self.fail();
                break;
            };
            let (_, size) = self.tx_pending[slot].expect("found token");
            // SAFETY: this completed token owns this exact address and
            // length, unchanged since transmit_begin.
            if unsafe { self.raw.transmit_complete(token, &self.tx[slot].bytes()[..size]) }.is_err() {
                self.counts.tx_drops += 1;
                self.broken = true;
                self.fail();
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

fn selftest(device: &mut VirtioNetDevice) -> Result<[u8; 6], &'static str> {
    let mac = device.info().mac;
    device.reclaim();
    device.transmit(ARP_FRAME_LEN, |out| arp_request(out, mac)).map_err(|_| "transmit failed")?;
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
            false
        })
        .map_err(|_| "no ARP reply from 10.0.2.2")?;
    reply.ok_or("no ARP reply from 10.0.2.2")
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
            Ok([a, b, c, d, e, f]) => {
                let _ = writeln!(
                    ctx.out,
                    "net selftest: ARP reply 10.0.2.2 is {a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}"
                );
            }
            Err(why) => ctx.fail(alloc::format!("net selftest: {why}")),
        }
        return;
    }
    device.reclaim();
    let info = device.info();
    let counts = device.counts();
    let free = device.tx_free();
    drop(guard);
    let [a, b, c, d, e, f] = info.mac;
    let link = if info.link_up { "up" } else { "down" };
    let _ =
        writeln!(ctx.out, "net: mac {a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{f:02x}, link {link}, mtu {}", info.mtu);
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

crate::ktest!(
    fn virtio_net_fills_and_reclaims_all_slots_without_dma_allocation() {
        use crate::{kassert, kassert_eq};
        let mut guard = DEVICE.lock();
        let dev = guard.as_mut().ok_or("no net device")?;
        let mac = dev.info().mac;
        let runs = virtio::outstanding_runs();
        let status = dev.status;
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
            status
                .wait_for(&BELL, SELFTEST_TIMEOUT_NS, || {
                    dev.reclaim();
                    dev.tx_free() == QUEUE_SIZE
                })
                .map_err(|_| "TX did not complete")?;
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
        actual
            .wait_for(&BELL, SELFTEST_TIMEOUT_NS, || {
                dev.reclaim();
                dev.tx_free() == QUEUE_SIZE
            })
            .map_err(|_| "recovery TX did not complete")?;
        drop(guard);
        let mut log = alloc::vec![0; crate::klog::RING_BYTES];
        let (len, _) = crate::klog::copy_recent(&mut log);
        let log = core::str::from_utf8(&log[..len]).map_err(|_| "invalid log")?;
        kassert_eq!(log.matches("WARN  net: virtio-net device failed; link down (said once)\n").count(), 1);
        Ok(())
    }
);
