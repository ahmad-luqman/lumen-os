//! `virtio-blk` PCI driver (`BlockDevice` implementation over `VirtQueue` + MSI-X).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use log::{info, warn};
use lumen_bcache::{
    BLOCK_SIZE, BlockCache, BlockCacheStats, BlockDevice, BlockError, BlockInfo, DEFAULT_CAPACITY, SECTOR_SIZE,
};
use lumen_pci::PciAddr;
use virtio_drivers::queue::VirtQueue;
use virtio_drivers::transport::{DeviceStatus, Transport};
use zerocopy::IntoBytes;

use crate::drivers::Driver;
use crate::pci::{self, DeviceInfo, DeviceResources};
use crate::sync::{SleepMutex, SpinLock};
use crate::virtio::{self, Doorbell, ProbeError, StatusHandle, VirtioHal, VirtioTransport, WaitError};

/// Maximum number of bound `virtio-blk` devices (each holds one static MSI-X [`Doorbell`]).
#[allow(dead_code)]
pub const MAX_BLK_DEVICES: usize = 4;

/// Driver table row for [`crate::drivers::DRIVERS`].
pub const DRIVER: Driver = Driver { name: "virtio-blk", matches, probe };

const VIRTIO_BLK: u16 = 2;
const QUEUE: u16 = 0;
const QUEUE_SIZE: usize = 16;
const BLK_REQUEST_TIMEOUT_NS: u64 = 5_000_000_000;

// virtio 1.2 §5.2.3 & §6 feature bits.
const VIRTIO_BLK_F_RO: u64 = 1 << 5;
const VIRTIO_BLK_F_BLK_SIZE: u64 = 1 << 6;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

// virtio 1.2 §5.2.4 config space byte offsets.
const CFG_CAPACITY_LOW: usize = 0;
const CFG_CAPACITY_HIGH: usize = 4;
const CFG_BLK_SIZE: usize = 20;

// virtio 1.2 §5.2.6 request types and status bytes.
const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;

const VIRTIO_BLK_S_OK: u8 = 0;
const VIRTIO_BLK_S_NOT_READY: u8 = 3;

#[repr(C)]
#[derive(Clone, Copy, zerocopy::Immutable, zerocopy::IntoBytes)]
struct BlkReqHeader {
    req_type: u32,
    reserved: u32,
    sector: u64,
}

const _: () = assert!(core::mem::size_of::<BlkReqHeader>() == 16);

static BELLS: [Doorbell; MAX_BLK_DEVICES] = [const { Doorbell::new() }; MAX_BLK_DEVICES];

fn on_irq<const I: usize>() {
    BELLS[I].ring();
}

const IRQ_HANDLERS: [fn(); MAX_BLK_DEVICES] = [on_irq::<0>, on_irq::<1>, on_irq::<2>, on_irq::<3>];

/// Registered block device entry (driver + its 4 KiB [`BlockCache`]).
#[derive(Clone)]
pub struct BlkEntry {
    pub id: usize,
    pub dev: Arc<VirtioBlkDevice>,
    /// Reached only through [`Self::stats`] and [`Self::into_cache`]: a
    /// volume mounted on it reaches it through its own seam (`ext4`'s
    /// `LatchedCache`, #349), never through the registry.
    cache: Arc<SleepMutex<BlockCache<Arc<VirtioBlkDevice>>>>,
}

impl BlkEntry {
    /// Its block cache's statistics, for `blk`.
    pub fn stats(&self) -> BlockCacheStats {
        self.cache.lock().stats()
    }

    /// Its block cache, for `vfs::init` alone, which mounts it behind the
    /// volume's seam.
    pub fn into_cache(self) -> Arc<SleepMutex<BlockCache<Arc<VirtioBlkDevice>>>> {
        self.cache
    }
}

#[derive(Clone)]
enum SlotState {
    Free,
    Probing,
    Bound(BlkEntry),
}

static SLOTS: SpinLock<[SlotState; MAX_BLK_DEVICES]> = SpinLock::new([const { SlotState::Free }; MAX_BLK_DEVICES]);

struct SlotGuard {
    slot: Option<usize>,
}

impl SlotGuard {
    fn defuse(&mut self) {
        self.slot = None;
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            let mut slots = SLOTS.lock();
            if matches!(slots[slot], SlotState::Probing) {
                slots[slot] = SlotState::Free;
            }
        }
    }
}

/// Snapshot of all bound `virtio-blk` devices in slot order.
pub fn devices() -> Vec<BlkEntry> {
    let slots = SLOTS.lock();
    slots
        .iter()
        .filter_map(|s| match s {
            SlotState::Bound(entry) => Some(entry.clone()),
            _ => None,
        })
        .collect()
}

/// Look up a bound `virtio-blk` device by logical id (`0..MAX_BLK_DEVICES`).
pub fn device(id: usize) -> Option<Arc<VirtioBlkDevice>> {
    let slots = SLOTS.lock();
    match slots.get(id) {
        Some(SlotState::Bound(entry)) => Some(entry.dev.clone()),
        _ => None,
    }
}

fn matches(d: &DeviceInfo) -> bool {
    d.virtio_type() == Some(VIRTIO_BLK)
}

fn probe(res: DeviceResources) -> Result<(), ProbeError> {
    let slot = {
        let mut slots = SLOTS.lock();
        match slots.iter().position(|s| matches!(s, SlotState::Free)) {
            Some(idx) => {
                slots[idx] = SlotState::Probing;
                Some(idx)
            }
            None => None,
        }
    };
    let Some(slot) = slot else {
        info!(
            target: "blk",
            "{}: already {MAX_BLK_DEVICES} block devices bound; declining",
            res.info().addr
        );
        return Err(ProbeError::NotMine);
    };
    let mut guard = SlotGuard { slot: Some(slot) };
    let entry = bring_up(slot, res)?;
    {
        let mut slots = SLOTS.lock();
        slots[slot] = SlotState::Bound(entry);
    }
    guard.defuse();
    Ok(())
}

fn bring_up(slot: usize, res: DeviceResources) -> Result<BlkEntry, ProbeError> {
    let mut transport = virtio::transport(res, IRQ_HANDLERS[slot])?;
    let status = transport.status();
    let addr = transport.info().addr;

    transport.set_status(DeviceStatus::empty());
    transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);

    let offered = transport.read_device_features();
    let supported = VIRTIO_F_VERSION_1 | VIRTIO_BLK_F_RO | VIRTIO_BLK_F_BLK_SIZE | VIRTIO_BLK_F_FLUSH;
    let negotiated = offered & supported;
    transport.write_driver_features(negotiated);
    transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::FEATURES_OK);
    if !status.get().contains(DeviceStatus::FEATURES_OK) {
        return Err(ProbeError::NotReady { status: status.raw() });
    }
    transport.set_guest_page_size(lumen_mm::FRAME_SIZE as u32);

    let sector_count = transport
        .read_consistent(|| {
            let lo: u32 = transport.read_config_space(CFG_CAPACITY_LOW)?;
            let hi: u32 = transport.read_config_space(CFG_CAPACITY_HIGH)?;
            Ok((u64::from(hi) << 32) | u64::from(lo))
        })
        .map_err(ProbeError::Device)?;
    if sector_count == 0 {
        return Err(ProbeError::Unusable(String::from("zero block capacity")));
    }

    let sector_size = if negotiated & VIRTIO_BLK_F_BLK_SIZE != 0 {
        transport.read_config_space::<u32>(CFG_BLK_SIZE).map_err(ProbeError::Device)?
    } else {
        SECTOR_SIZE as u32
    };
    if sector_size != SECTOR_SIZE as u32 {
        return Err(ProbeError::Unusable(format!("unsupported sector size {sector_size} (expected {SECTOR_SIZE})")));
    }

    if transport.max_queue_size(QUEUE) < QUEUE_SIZE as u32 {
        return Err(ProbeError::Unusable(format!(
            "virtqueue 0 max size {} < {QUEUE_SIZE}",
            transport.max_queue_size(QUEUE)
        )));
    }

    let queue =
        VirtQueue::<VirtioHal, QUEUE_SIZE>::new(&mut transport, QUEUE, false, false).map_err(ProbeError::Device)?;
    // Wrap `(transport, queue)` in `Inner` BEFORE `finish_init()` sets
    // `DRIVER_OK` so any subsequent `?` resets the transport before freeing
    // queue DMA frames.
    let mut inner = Inner { transport, queue };
    inner.transport.finish_init();
    virtio::check_device(&status)?;

    let info = BlockInfo {
        sector_size,
        sector_count,
        read_only: (negotiated & VIRTIO_BLK_F_RO) != 0,
        flush_supported: (negotiated & VIRTIO_BLK_F_FLUSH) != 0,
    };

    let dev = Arc::new(VirtioBlkDevice {
        slot,
        addr,
        info,
        status,
        failed: AtomicBool::new(false),
        reads: AtomicU64::new(0),
        writes: AtomicU64::new(0),
        flushes: AtomicU64::new(0),
        state: SleepMutex::new(Some(inner)),
    });
    let cache = BlockCache::new(dev.clone(), DEFAULT_CAPACITY)
        .map_err(|e| ProbeError::Unusable(format!("block cache init: {e}")))?;
    let cache = Arc::new(SleepMutex::new(cache));

    let total_bytes = sector_count * u64::from(sector_size);
    info!(
        target: "blk",
        "{addr}: blk{slot} virtio-blk, {} sectors ({} KiB), ro={}, flush={}",
        info.sector_count,
        total_bytes / 1024,
        info.read_only,
        info.flush_supported
    );

    Ok(BlkEntry { id: slot, dev, cache })
}

/// Field order matters: `transport` drops (resetting the device and freeing
/// `DeviceResources`) before `queue` frees its DMA rings.
struct Inner {
    transport: VirtioTransport,
    queue: VirtQueue<VirtioHal, QUEUE_SIZE>,
}

/// Bound `virtio-blk` device implementing [`BlockDevice`].
pub struct VirtioBlkDevice {
    slot: usize,
    addr: PciAddr,
    info: BlockInfo,
    status: StatusHandle,
    failed: AtomicBool,
    reads: AtomicU64,
    writes: AtomicU64,
    flushes: AtomicU64,
    state: SleepMutex<Option<Inner>>,
}

impl VirtioBlkDevice {
    /// PCI address of this block device.
    pub fn addr(&self) -> PciAddr {
        self.addr
    }

    /// Logical block device index (`0..MAX_BLK_DEVICES`).
    #[allow(dead_code)]
    pub fn id(&self) -> usize {
        self.slot
    }

    /// Whether the device has been retired after a fatal error or timeout.
    pub fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    /// Completed request counters `(reads, writes, flushes)`.
    #[allow(dead_code)]
    pub fn io_counts(&self) -> (u64, u64, u64) {
        (self.reads.load(Ordering::Relaxed), self.writes.load(Ordering::Relaxed), self.flushes.load(Ordering::Relaxed))
    }

    fn retire(&self, guard: &mut Option<Inner>, leaked: usize, why: &str) {
        // Drop `VirtioTransport` (resetting device & releasing DeviceResources)
        // BEFORE calling `pci::failed_after_bind`.
        drop(guard.take());
        self.failed.store(true, Ordering::Release);
        let msg = format!("{why} ({leaked} DMA runs leaked)");
        warn!(target: "blk", "{}: {msg}; device retired", self.addr);
        let _ = pci::failed_after_bind(self.addr, DRIVER.name, msg);
    }

    fn validate_range(&self, lba: u64, len: usize) -> Result<(), BlockError> {
        if len == 0 || !len.is_multiple_of(SECTOR_SIZE) {
            return Err(BlockError::Unaligned);
        }
        let sectors = (len / SECTOR_SIZE) as u64;
        let end = lba.checked_add(sectors).ok_or(BlockError::OutOfRange)?;
        if end > self.info.sector_count {
            return Err(BlockError::OutOfRange);
        }
        Ok(())
    }
}

impl BlockDevice for VirtioBlkDevice {
    fn info(&self) -> BlockInfo {
        self.info
    }

    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        self.validate_range(lba, buf.len())?;
        let mut guard = self.state.lock();
        if guard.is_none() {
            return Err(BlockError::DeviceFailed);
        }

        let mut cur_lba = lba;
        for chunk in buf.chunks_mut(BLOCK_SIZE) {
            let hdr = BlkReqHeader { req_type: VIRTIO_BLK_T_IN.to_le(), reserved: 0, sector: cur_lba.to_le() };
            let mut status_byte = [VIRTIO_BLK_S_NOT_READY; 1];
            let expected_len = chunk.len() + 1;

            let inner = guard.as_mut().ok_or(BlockError::DeviceFailed)?;
            let inputs = [hdr.as_bytes()];
            let mut outputs = [&mut *chunk, &mut status_byte[..]];
            // SAFETY: `inputs` and `outputs` stay alive and untouched until
            // `pop_used` succeeds with `token`, or the device is retired
            // (resetting it before these buffers go out of scope).
            let token = unsafe { inner.queue.add(&inputs, &mut outputs) }.map_err(|_| BlockError::Io)?;
            if inner.queue.should_notify() {
                inner.transport.notify(QUEUE);
            }

            let wait_res = {
                let queue = &inner.queue;
                self.status.wait_for(&BELLS[self.slot], BLK_REQUEST_TIMEOUT_NS, || queue.can_pop())
            };
            if let Err(e) = wait_res {
                let why = format!("{e}");
                self.retire(&mut guard, 3, &why);
                return Err(match e {
                    WaitError::Timeout { .. } => BlockError::Timeout,
                    WaitError::NeedsReset { .. } | WaitError::Failed { .. } => BlockError::DeviceFailed,
                });
            }

            // SAFETY: same buffers passed to `add` for `token`.
            let used_len = match unsafe { inner.queue.pop_used(token, &inputs, &mut outputs) } {
                Ok(n) => n,
                Err(e) => {
                    let why = format!("virtqueue pop_used: {e}");
                    self.retire(&mut guard, 3, &why);
                    return Err(BlockError::Io);
                }
            };

            if used_len as usize != expected_len || status_byte[0] != VIRTIO_BLK_S_OK {
                return Err(BlockError::Io);
            }
            cur_lba += (chunk.len() / SECTOR_SIZE) as u64;
        }

        self.reads.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        if self.info.read_only {
            return Err(BlockError::ReadOnly);
        }
        self.validate_range(lba, buf.len())?;
        let mut guard = self.state.lock();
        if guard.is_none() {
            return Err(BlockError::DeviceFailed);
        }

        let mut cur_lba = lba;
        for chunk in buf.chunks(BLOCK_SIZE) {
            let hdr = BlkReqHeader { req_type: VIRTIO_BLK_T_OUT.to_le(), reserved: 0, sector: cur_lba.to_le() };
            let mut status_byte = [VIRTIO_BLK_S_NOT_READY; 1];

            let inner = guard.as_mut().ok_or(BlockError::DeviceFailed)?;
            let inputs = [hdr.as_bytes(), chunk];
            let mut outputs = [&mut status_byte[..]];
            // SAFETY: `inputs` and `outputs` stay alive and untouched until
            // `pop_used` succeeds with `token`, or the device is retired
            // (resetting it before these buffers go out of scope).
            let token = unsafe { inner.queue.add(&inputs, &mut outputs) }.map_err(|_| BlockError::Io)?;
            if inner.queue.should_notify() {
                inner.transport.notify(QUEUE);
            }

            let wait_res = {
                let queue = &inner.queue;
                self.status.wait_for(&BELLS[self.slot], BLK_REQUEST_TIMEOUT_NS, || queue.can_pop())
            };
            if let Err(e) = wait_res {
                let why = format!("{e}");
                self.retire(&mut guard, 3, &why);
                return Err(match e {
                    WaitError::Timeout { .. } => BlockError::Timeout,
                    WaitError::NeedsReset { .. } | WaitError::Failed { .. } => BlockError::DeviceFailed,
                });
            }

            // SAFETY: same buffers passed to `add` for `token`.
            let used_len = match unsafe { inner.queue.pop_used(token, &inputs, &mut outputs) } {
                Ok(n) => n,
                Err(e) => {
                    let why = format!("virtqueue pop_used: {e}");
                    self.retire(&mut guard, 3, &why);
                    return Err(BlockError::Io);
                }
            };

            if used_len < 1 || status_byte[0] != VIRTIO_BLK_S_OK {
                return Err(BlockError::Io);
            }
            cur_lba += (chunk.len() / SECTOR_SIZE) as u64;
        }

        self.writes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn flush(&self) -> Result<(), BlockError> {
        if self.failed.load(Ordering::Acquire) {
            return Err(BlockError::DeviceFailed);
        }
        if self.info.read_only || !self.info.flush_supported {
            return Ok(());
        }
        let mut guard = self.state.lock();
        let inner = guard.as_mut().ok_or(BlockError::DeviceFailed)?;

        let hdr = BlkReqHeader { req_type: VIRTIO_BLK_T_FLUSH.to_le(), reserved: 0, sector: 0 };
        let mut status_byte = [VIRTIO_BLK_S_NOT_READY; 1];
        let inputs = [hdr.as_bytes()];
        let mut outputs = [&mut status_byte[..]];
        // SAFETY: `inputs` and `outputs` stay alive and untouched until `pop_used`
        // succeeds or the device is retired (reset).
        let token = unsafe { inner.queue.add(&inputs, &mut outputs) }.map_err(|_| BlockError::Io)?;
        if inner.queue.should_notify() {
            inner.transport.notify(QUEUE);
        }

        let wait_res = {
            let queue = &inner.queue;
            self.status.wait_for(&BELLS[self.slot], BLK_REQUEST_TIMEOUT_NS, || queue.can_pop())
        };
        if let Err(e) = wait_res {
            let why = format!("{e}");
            self.retire(&mut guard, 2, &why);
            return Err(match e {
                WaitError::Timeout { .. } => BlockError::Timeout,
                WaitError::NeedsReset { .. } | WaitError::Failed { .. } => BlockError::DeviceFailed,
            });
        }

        // SAFETY: same buffers passed to `add` for `token`.
        let used_len = match unsafe { inner.queue.pop_used(token, &inputs, &mut outputs) } {
            Ok(n) => n,
            Err(e) => {
                let why = format!("virtqueue pop_used: {e}");
                self.retire(&mut guard, 2, &why);
                return Err(BlockError::Io);
            }
        };

        if used_len < 1 || status_byte[0] != VIRTIO_BLK_S_OK {
            return Err(BlockError::Io);
        }
        self.flushes.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
