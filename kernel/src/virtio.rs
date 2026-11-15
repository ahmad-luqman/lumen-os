//! The adapter for `virtio-drivers` (docs/ARCHITECTURE.md, section 7):
//! [`transport`] turns a claimed virtio PCI function into a
//! [`VirtioTransport`] the crate's device drivers run on, and [`VirtioHal`]
//! gives the crate DMA memory, owned by the frame allocator (hard rule 2).
//!
//! - **DMA runs.** Every contiguous run handed to the crate (`dma_alloc`,
//!   and each bounce buffer `share` makes) is an [`OwnedRun`] recorded in a
//!   fixed table of outstanding runs ([`RUNS`] entries, no heap, its own
//!   lock, always taken before the frame allocator's). `dma_dealloc` and
//!   `unshare` free only a run the table recorded with that address and
//!   length: `free_contiguous` alone can't catch a double free after the
//!   frames were reused, or a wrong length.
//! - **Bounce buffers, not address translation.** The crate shares
//!   whatever buffer a request uses (a caller's slice, a driver's `Box`, a
//!   stack local), and the device needs one physical address for it, but
//!   the heap and the stacks are windows mapped one page at a time: a
//!   buffer crossing a page there isn't physically contiguous. So `share`
//!   copies into (or zeroes) a fresh run and `unshare` copies back and
//!   frees it. Known costs: an allocation and a copy per buffer (each
//!   posted 8-byte input event holds a 4 KiB frame); buffers still posted
//!   when their queue is dropped are never unshared (`VirtQueue` has no
//!   `Drop` that does), so a probe failing partway leaks frames and table
//!   entries (`drivers::probe_all` logs the change).
//! - **MMIO.** `mmio_phys_to_virt` only looks up the BAR mappings of the
//!   device [`transport`] is probing (the claim mapped them); it never maps,
//!   and panics outside a probe or outside that device's BARs, which the
//!   check before the probe makes unreachable.
//! - **Waiting.** The crate's device structs don't expose their transport,
//!   so [`VirtioTransport::status`] hands out a [`StatusHandle`] on the
//!   device status register: a driver's bounded wait
//!   ([`StatusHandle::wait_until`]) gives up when the device reports
//!   `DEVICE_NEEDS_RESET` or `FAILED`. Known limits: `VirtIORng::
//!   request_entropy` and sound's control requests spin unbounded in the
//!   crate (#97 drives its own queue; #99's control path is done once at
//!   setup), and `PciTransport`'s `Drop` spins until the status reads 0.
//!   Drivers poll until MSI-X (#96).
//!
//! Locks: the run table before the frame allocator; the probe scope and
//! ECAM are each taken alone.

// The first driver on this adapter is rng (#97): until then only the ktests
// use it.
#![cfg_attr(not(feature = "ktest"), allow(dead_code))]

use alloc::vec::Vec;
use core::fmt;
use core::ptr::NonNull;

use lumen_mm::{FRAME_SIZE, FreeError, PhysAddr, PhysFrame};
use lumen_pci::cap;
use lumen_pci::virtio::{self as caps, CapProblem, VirtioCap};
use virtio_drivers::transport::pci::PciTransport;
use virtio_drivers::transport::pci::bus::{DeviceFunction, PciRoot};
use virtio_drivers::transport::{DeviceStatus, DeviceType, InterruptStatus, Transport};
use virtio_drivers::{BufferDirection, Hal};

use crate::mm::frame;
use crate::pci::{BarMapping, DeviceInfo, DeviceResources};
use crate::sync::SpinLock;

/// Entries in the table of outstanding DMA runs. From the counts in
/// section 7: input 36 (32 posted events and its rings), sound 40 idle plus
/// up to 96 buffers in flight and 3 for a control request, rng about 3:
/// about 78 at boot, about 180 while sound streams.
pub const RUNS: usize = 256;

/// Each outstanding run's first physical address and frame count.
static TABLE: SpinLock<[Option<(u64, usize)>; RUNS]> = SpinLock::new([None; RUNS]);

/// How many DMA runs are outstanding (handed to the crate, not yet freed).
pub fn outstanding_runs() -> usize {
    TABLE.lock().iter().flatten().count()
}

/// Contiguous frames from the frame allocator, freed when dropped.
#[derive(Debug)]
#[must_use = "dropping a run frees it"]
pub struct OwnedRun {
    first: PhysFrame,
    count: usize,
}

impl OwnedRun {
    /// `count` (at least 1) contiguous frames, or `None` if no such run is
    /// free.
    pub fn alloc(count: usize) -> Option<OwnedRun> {
        frame::alloc_contiguous(count, 1).map(|first| OwnedRun { first, count })
    }

    pub fn phys(&self) -> u64 {
        self.first.as_u64()
    }

    pub fn len(&self) -> usize {
        self.count * FRAME_SIZE as usize
    }

    /// The run through the HHDM (usable RAM, which the HHDM maps).
    fn ptr(&self) -> *mut u8 {
        crate::mm::phys_to_virt(self.first.start()).as_mut_ptr()
    }

    /// Give up ownership without freeing (the table records it raw).
    fn into_raw(self) -> (PhysFrame, usize) {
        let raw = (self.first, self.count);
        core::mem::forget(self);
        raw
    }

    /// Take ownership of a run the table recorded.
    ///
    /// # Safety
    /// `first` and `count` are a run [`into_raw`](Self::into_raw) gave up,
    /// just removed from the table, so nothing else owns it.
    unsafe fn from_raw(first: PhysFrame, count: usize) -> OwnedRun {
        OwnedRun { first, count }
    }

    /// Free the run now, reporting a failure (a kernel bug) to the caller.
    pub fn free(self) -> Result<(), FreeError> {
        let (first, count) = self.into_raw();
        // SAFETY: as in `drop`.
        unsafe { frame::free_contiguous(first, count) }
    }
}

impl Drop for OwnedRun {
    fn drop(&mut self) {
        // SAFETY: a live `OwnedRun` owns its frames (from `alloc`, or
        // `from_raw` after the table gave the run up); a device it was
        // shared with has finished with it, because only `dma_dealloc` and
        // `unshare` turn a recorded run back into an `OwnedRun`.
        unsafe { frame::free_contiguous(self.first, self.count) }.unwrap_or_else(|e| {
            panic!("virtio: freeing the DMA run at {:#x} ({} frames): {e:?}", self.phys(), self.count)
        });
    }
}

/// Record `run` in the table: its physical address, or the run back if the
/// table is full.
fn record(table: &mut [Option<(u64, usize)>; RUNS], run: OwnedRun) -> Result<u64, OwnedRun> {
    let Some(entry) = table.iter_mut().find(|e| e.is_none()) else { return Err(run) };
    let (first, count) = run.into_raw();
    *entry = Some((first.as_u64(), count));
    Ok(first.as_u64())
}

/// Remove the recorded run at `paddr` of `count` frames, if there is one.
fn take(table: &mut [Option<(u64, usize)>; RUNS], paddr: u64, count: usize) -> Option<OwnedRun> {
    let entry = table.iter_mut().find(|e| **e == Some((paddr, count)))?;
    *entry = None;
    let first = PhysFrame::from_u64(paddr).expect("the table records frame-aligned runs");
    // SAFETY: `record` gave this run up and it is no longer in the table.
    Some(unsafe { OwnedRun::from_raw(first, count) })
}

/// Frames for `len` bytes (at least one).
fn frames_for(len: usize) -> usize {
    len.div_ceil(FRAME_SIZE as usize).max(1)
}

/// The BAR mappings of the device being probed, while [`transport`] runs
/// `PciTransport::new`.
static PROBING: SpinLock<Option<[Option<BarMapping>; 6]>> = SpinLock::new(None);

/// `virtio-drivers`' hardware abstraction: DMA from the frame allocator,
/// bounce buffers for shared buffers, and MMIO through the probed device's
/// BAR mappings.
#[derive(Debug)]
pub struct VirtioHal;

// SAFETY: `dma_alloc` returns zeroed, physically contiguous runs the
// allocator gave only to the crate (recorded until `dma_dealloc` takes them
// back); `mmio_phys_to_virt` returns addresses inside a BAR the registry
// mapped uncached for the kernel's life; `share` returns a fresh run holding
// the buffer's contents (or zeros) that nothing else uses until `unshare`.
unsafe impl Hal for VirtioHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (u64, NonNull<u8>) {
        if pages == 0 {
            return (0, NonNull::dangling());
        }
        let mut table = TABLE.lock();
        let Some(run) = OwnedRun::alloc(pages) else { return (0, NonNull::dangling()) };
        // SAFETY: a fresh run nothing else uses, in the HHDM.
        unsafe { core::ptr::write_bytes(run.ptr(), 0, run.len()) };
        let ptr = NonNull::new(run.ptr()).expect("HHDM addresses aren't null");
        match record(&mut table, run) {
            Ok(paddr) => (paddr, ptr),
            // Out of table entries: freed again (the frame allocator's
            // lock nests inside the table's).
            Err(run) => {
                drop(run);
                (0, NonNull::dangling())
            }
        }
    }

    unsafe fn dma_dealloc(paddr: u64, vaddr: NonNull<u8>, pages: usize) -> i32 {
        let Some(phys) = PhysAddr::new(paddr) else { return -1 };
        if vaddr.as_ptr() != crate::mm::phys_to_virt(phys).as_mut_ptr::<u8>() {
            return -1;
        }
        let mut table = TABLE.lock();
        match take(&mut table, paddr, pages) {
            Some(run) => match run.free() {
                Ok(()) => 0,
                Err(_) => -2,
            },
            None => -1,
        }
    }

    unsafe fn mmio_phys_to_virt(paddr: u64, size: usize) -> NonNull<u8> {
        let scope = PROBING.lock();
        let bars = scope.as_ref().unwrap_or_else(|| panic!("virtio: mmio_phys_to_virt({paddr:#x}) outside a probe"));
        let virt = bars.iter().flatten().find_map(|m| m.virt_of(paddr, size as u64));
        let virt = virt.unwrap_or_else(|| panic!("virtio: {paddr:#x} (+{size:#x}) isn't in the probed device's BARs"));
        NonNull::new(virt.as_mut_ptr()).expect("mapped addresses aren't null")
    }

    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> u64 {
        let len = buffer.len();
        let mut table = TABLE.lock();
        let run = OwnedRun::alloc(frames_for(len))
            .unwrap_or_else(|| panic!("virtio: out of frames bouncing a {len}-byte buffer"));
        match direction {
            // SAFETY: the crate passes a buffer valid for reads of `len`
            // bytes; the run is fresh and at least `len` long.
            BufferDirection::DriverToDevice => unsafe {
                core::ptr::copy_nonoverlapping(buffer.as_ptr().cast::<u8>(), run.ptr(), len)
            },
            // SAFETY: the run is fresh and at least `len` long. Zeroed, so
            // stale memory never reaches the caller.
            BufferDirection::DeviceToDriver => unsafe { core::ptr::write_bytes(run.ptr(), 0, len) },
            BufferDirection::Both => {
                unreachable!("virtio: share with BufferDirection::Both, which the crate never asks for")
            }
        }
        record(&mut table, run)
            .unwrap_or_else(|_| panic!("virtio: the outstanding DMA run table is full ({RUNS} entries)"))
    }

    unsafe fn unshare(paddr: u64, buffer: NonNull<[u8]>, direction: BufferDirection) {
        let (len, pages) = (buffer.len(), frames_for(buffer.len()));
        let mut table = TABLE.lock();
        let run = take(&mut table, paddr, pages)
            .unwrap_or_else(|| panic!("virtio: unshare of {paddr:#x} ({pages} pages): no such run was shared"));
        if direction == BufferDirection::DeviceToDriver {
            // SAFETY: the crate passes the buffer it shared, valid for writes
            // of `len` bytes; the device has finished with the run.
            unsafe { core::ptr::copy_nonoverlapping(run.ptr(), buffer.as_ptr().cast::<u8>(), len) };
        }
        run.free().unwrap_or_else(|e| panic!("virtio: freeing the bounce run at {paddr:#x} ({pages} pages): {e:?}"));
    }
}

/// Why a device couldn't be brought up.
#[derive(Debug)]
pub enum ProbeError {
    /// A virtio capability `PciTransport::new` would use isn't inside a
    /// mapped memory BAR, or can't be read inside the header.
    Capability(CapProblem),
    /// `PciTransport::new` refused the device.
    Transport(virtio_drivers::transport::pci::VirtioPciError),
    /// The device driver's setup failed.
    #[cfg_attr(feature = "ktest", allow(dead_code))] // first user: rng (#97)
    Device(virtio_drivers::Error),
    /// A bounded wait gave up.
    Wait(WaitError),
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProbeError::Capability(p) => write!(f, "{p}"),
            ProbeError::Transport(e) => write!(f, "virtio transport: {e}"),
            ProbeError::Device(e) => write!(f, "virtio: {e}"),
            ProbeError::Wait(e) => write!(f, "{e}"),
        }
    }
}

/// Bring up the virtio transport of a claimed function: the one entry
/// point. It takes the resources by value, so it runs once per device, and
/// the transport owns them. First it checks what `PciTransport::new` will
/// look up (the first virtio capability of each type 1-4 inside a mapped
/// memory BAR: `lumen_pci::virtio::check`); then it runs it with the
/// function's configuration space and, for `mmio_phys_to_virt`, its BAR
/// mappings. Not reentrant: one probe at a time (panics otherwise).
pub fn transport(res: DeviceResources) -> Result<VirtioTransport, ProbeError> {
    let info = res.info();
    let addr = info.addr;
    let mut config = res.config();
    let found: Vec<VirtioCap> =
        info.caps.iter().filter(|c| c.id == cap::id::VENDOR).map(|c| VirtioCap::read(&mut config, addr, c)).collect();
    let bars: [Option<BarMapping>; 6] = core::array::from_fn(|i| res.bar(i));
    let used = caps::check(&found, |i| bars[usize::from(i)].map(|m| m.size())).map_err(ProbeError::Capability)?;
    {
        let mut scope = PROBING.lock();
        assert!(scope.is_none(), "virtio: two probes at once");
        *scope = Some(bars);
    }
    let df = DeviceFunction { bus: addr.bus(), device: addr.device(), function: addr.function() };
    let made = PciTransport::new::<VirtioHal, _>(&mut PciRoot::new(res.config()), df);
    *PROBING.lock() = None;
    let inner = made.map_err(ProbeError::Transport)?;
    let common = used.common.expect("PciTransport::new found the common configuration");
    let bar = bars[usize::from(common.bar)].expect("checked: a mapped memory BAR");
    let status = bar
        .virt_of(bar.phys().as_u64() + u64::from(common.offset) + caps::DEVICE_STATUS_OFFSET, 1)
        .expect("checked: the common configuration is inside its BAR");
    let reg = NonNull::new(status.as_mut_ptr::<u8>()).expect("mapped addresses aren't null");
    Ok(VirtioTransport { inner, status: StatusHandle { reg }, res })
}

/// A virtio PCI device's transport: `virtio-drivers`' `PciTransport`, which
/// it delegates to, plus what the crate leaves out (a [`StatusHandle`];
/// #96 adds the MSI-X vectors). Owns the device's resources.
#[derive(Debug)]
pub struct VirtioTransport {
    inner: PciTransport,
    status: StatusHandle,
    res: DeviceResources,
}

impl VirtioTransport {
    /// A handle on the device status register, for a driver's bounded
    /// waits once the crate's device struct owns this transport.
    pub fn status(&self) -> StatusHandle {
        self.status
    }

    pub fn info(&self) -> &'static DeviceInfo {
        self.res.info()
    }
}

impl Transport for VirtioTransport {
    fn device_type(&self) -> DeviceType {
        self.inner.device_type()
    }

    fn read_device_features(&mut self) -> u64 {
        self.inner.read_device_features()
    }

    fn write_driver_features(&mut self, driver_features: u64) {
        self.inner.write_driver_features(driver_features)
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        self.inner.max_queue_size(queue)
    }

    fn notify(&mut self, queue: u16) {
        self.inner.notify(queue)
    }

    fn get_status(&self) -> DeviceStatus {
        self.inner.get_status()
    }

    fn set_status(&mut self, status: DeviceStatus) {
        self.inner.set_status(status)
    }

    fn set_guest_page_size(&mut self, guest_page_size: u32) {
        self.inner.set_guest_page_size(guest_page_size)
    }

    fn requires_legacy_layout(&self) -> bool {
        self.inner.requires_legacy_layout()
    }

    fn queue_set(&mut self, queue: u16, size: u32, descriptors: u64, driver_area: u64, device_area: u64) {
        self.inner.queue_set(queue, size, descriptors, driver_area, device_area)
    }

    fn queue_unset(&mut self, queue: u16) {
        self.inner.queue_unset(queue)
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        self.inner.queue_used(queue)
    }

    fn ack_interrupt(&mut self) -> InterruptStatus {
        self.inner.ack_interrupt()
    }

    fn read_config_generation(&self) -> u32 {
        self.inner.read_config_generation()
    }

    fn read_config_space<T: zerocopy::FromBytes + zerocopy::IntoBytes>(
        &self,
        offset: usize,
    ) -> virtio_drivers::Result<T> {
        self.inner.read_config_space(offset)
    }

    fn write_config_space<T: zerocopy::IntoBytes + zerocopy::Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> virtio_drivers::Result<()> {
        self.inner.write_config_space(offset, value)
    }
}

/// The device status register of a virtio device, readable while the
/// crate's device struct owns its transport. (A second pointer to the
/// common configuration: both sides make only volatile accesses, and this
/// one only reads a byte.)
#[derive(Clone, Copy, Debug)]
pub struct StatusHandle {
    reg: NonNull<u8>,
}

// SAFETY: it only reads a device register through a mapping that lasts the
// kernel's life (MMIO reads are fine from any CPU).
unsafe impl Send for StatusHandle {}
// SAFETY: as for `Send`.
unsafe impl Sync for StatusHandle {}

/// Why a bounded wait gave up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitError {
    Timeout,
    /// The device set `DEVICE_NEEDS_RESET`.
    NeedsReset,
    /// The device status reads `FAILED`.
    Failed,
}

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            WaitError::Timeout => "the device didn't answer in time",
            WaitError::NeedsReset => "the device needs a reset",
            WaitError::Failed => "the device status reads FAILED",
        })
    }
}

impl StatusHandle {
    pub fn get(&self) -> DeviceStatus {
        // SAFETY: the register is in a BAR mapped uncached for the kernel's
        // life (checked by `transport`); a byte read, volatile.
        DeviceStatus::from_bits_truncate(u32::from(unsafe { self.reg.read_volatile() }))
    }

    /// Wait until `ready()` (checked first, and then after each look at the
    /// status), at most `timeout_ns` nanoseconds; give up early if the
    /// device reports `DEVICE_NEEDS_RESET` or `FAILED`. Busy-waits: call it
    /// without a lock held (after `time::init`).
    pub fn wait_until(&self, timeout_ns: u64, mut ready: impl FnMut() -> bool) -> Result<(), WaitError> {
        let deadline = crate::time::now_ns().saturating_add(timeout_ns);
        loop {
            if ready() {
                return Ok(());
            }
            let status = self.get();
            if status.contains(DeviceStatus::FAILED) {
                return Err(WaitError::Failed);
            }
            if status.contains(DeviceStatus::DEVICE_NEEDS_RESET) {
                return Err(WaitError::NeedsReset);
            }
            if crate::time::now_ns() >= deadline {
                return Err(WaitError::Timeout);
            }
            core::hint::spin_loop();
        }
    }
}

/// The ktest machine's virtio device of `kind` (virtio 1.2 §5 device type:
/// 1 network, 2 block, 9 9p; no driver takes those in M3).
#[cfg(feature = "ktest")]
fn ktest_device(kind: u16) -> Result<&'static DeviceInfo, &'static str> {
    crate::pci::devices()
        .iter()
        .find(|d| lumen_pci::names::virtio_type(d.vendor, d.device, d.subsystem.1) == Some(kind))
        .ok_or("the ktest machine lacks a virtio device (tests/scenarios/test_ktest.py attaches them)")
}

crate::ktest!(
    fn dma_frames_are_owned_and_freed() {
        use crate::{kassert, kassert_eq};
        let before = outstanding_runs();
        let (paddr, vaddr) = VirtioHal::dma_alloc(3, BufferDirection::DriverToDevice);
        kassert!(paddr != 0);
        // Allocated from the allocator (owned by it, so not reserved: hard
        // rule 2), recorded, zeroed and reached through the HHDM.
        let frames = || (0..3).map(|i| PhysFrame::from_u64(paddr + i * FRAME_SIZE).expect("aligned"));
        kassert!(frames().all(frame::is_allocated));
        kassert_eq!(outstanding_runs(), before + 1);
        let hhdm = crate::mm::phys_to_virt(PhysAddr::new(paddr).ok_or("not physical")?).as_mut_ptr::<u8>();
        kassert_eq!(vaddr.as_ptr(), hhdm);
        // SAFETY: the run is ours until `dma_dealloc`, 3 frames long.
        let bytes = unsafe { core::slice::from_raw_parts(hhdm, 3 * FRAME_SIZE as usize) };
        kassert!(bytes.iter().all(|&b| b == 0));
        // A wrong length, or another address, is refused and frees nothing.
        // SAFETY: refused calls touch nothing.
        unsafe {
            kassert!(VirtioHal::dma_dealloc(paddr, vaddr, 2) != 0);
            kassert!(VirtioHal::dma_dealloc(paddr + FRAME_SIZE, vaddr, 2) != 0);
            kassert!(VirtioHal::dma_dealloc(paddr, NonNull::dangling(), 3) != 0);
        }
        kassert!(frames().all(frame::is_allocated));
        // SAFETY: the run this test allocated, no longer used.
        kassert_eq!(unsafe { VirtioHal::dma_dealloc(paddr, vaddr, 3) }, 0);
        kassert!(frames().all(|f| !frame::is_allocated(f)));
        kassert_eq!(outstanding_runs(), before);
        // Freed once: a second free is refused, not a double free.
        // SAFETY: refused.
        kassert!(unsafe { VirtioHal::dma_dealloc(paddr, vaddr, 3) } != 0);
        Ok(())
    }
);

crate::ktest!(
    fn share_bounces_a_buffer_across_pages() {
        use alloc::vec;

        use crate::{kassert, kassert_eq};
        // 600 bytes straddling a page boundary of a heap buffer.
        let mut heap = vec![0u8; 3 * FRAME_SIZE as usize];
        let base = heap.as_ptr() as usize;
        let start = base.next_multiple_of(FRAME_SIZE as usize) + FRAME_SIZE as usize - 300 - base;
        let buf = &mut heap[start..start + 600];
        kassert!((buf.as_ptr() as usize + 300).is_multiple_of(FRAME_SIZE as usize));
        for (i, b) in buf.iter_mut().enumerate() {
            *b = i as u8 ^ 0x5a;
        }
        let before = outstanding_runs();
        let hhdm = |paddr: u64| crate::mm::phys_to_virt(PhysAddr::new(paddr).expect("physical")).as_mut_ptr::<u8>();

        // To the device: its copy is one contiguous run holding the bytes.
        let shared = NonNull::from(&mut *buf);
        // SAFETY: `buf` stays alive and untouched until `unshare`.
        let paddr = unsafe { VirtioHal::share(shared, BufferDirection::DriverToDevice) };
        kassert!(frame::is_allocated(PhysFrame::from_u64(paddr).ok_or("not a frame")?));
        kassert_eq!(outstanding_runs(), before + 1);
        // SAFETY: the bounce run, 1 frame, ours until `unshare`.
        let copy = unsafe { core::slice::from_raw_parts(hhdm(paddr), 600) };
        kassert!(copy.iter().enumerate().all(|(i, &b)| b == i as u8 ^ 0x5a));
        // SAFETY: the buffer and address `share` returned.
        unsafe { VirtioHal::unshare(paddr, shared, BufferDirection::DriverToDevice) };
        kassert!(!frame::is_allocated(PhysFrame::from_u64(paddr).ok_or("not a frame")?));
        kassert_eq!(outstanding_runs(), before);

        // From the device: zeroed (no stale bytes), what the device wrote
        // copied back.
        // SAFETY: as above.
        let paddr = unsafe { VirtioHal::share(shared, BufferDirection::DeviceToDriver) };
        // SAFETY: the bounce run, ours until `unshare`; the "device" writes.
        let dev = unsafe { core::slice::from_raw_parts_mut(hhdm(paddr), 600) };
        kassert!(dev.iter().all(|&b| b == 0));
        for (i, b) in dev.iter_mut().enumerate() {
            *b = (i * 7) as u8;
        }
        // SAFETY: as above.
        unsafe { VirtioHal::unshare(paddr, shared, BufferDirection::DeviceToDriver) };
        kassert!(buf.iter().enumerate().all(|(i, &b)| b == (i * 7) as u8));
        kassert!(!frame::is_allocated(PhysFrame::from_u64(paddr).ok_or("not a frame")?));
        kassert_eq!(outstanding_runs(), before);

        // A buffer of two pages and a byte takes three frames.
        let big = vec![1u8; 2 * FRAME_SIZE as usize + 1];
        let shared = NonNull::from(&big[..]);
        // SAFETY: `big` outlives the share and isn't written meanwhile.
        let paddr = unsafe { VirtioHal::share(shared, BufferDirection::DriverToDevice) };
        kassert!((0..3).all(|i| frame::is_allocated(PhysFrame::from_u64(paddr + i * FRAME_SIZE).expect("aligned"))));
        // SAFETY: as above.
        unsafe { VirtioHal::unshare(paddr, shared, BufferDirection::DriverToDevice) };
        kassert_eq!(outstanding_runs(), before);
        Ok(())
    }
);

crate::ktest!(
    fn a_claimed_device_has_memory_space_and_bus_master_on_and_io_off() {
        use lumen_pci::{ConfigSpace, command, reg};

        use crate::pci::{self, Binding, ClaimError};
        use crate::{kassert, kassert_eq};
        let d = ktest_device(1)?;
        let res = pci::claim(d.addr, "ktest").map_err(|_| "the claim failed")?;
        kassert_eq!(pci::binding(d.addr), Some(Binding::Probing("ktest")));
        let cmd = res.config().read16(d.addr, reg::COMMAND);
        kassert_eq!(
            cmd & (command::MEMORY_SPACE | command::BUS_MASTER | command::IO_SPACE),
            command::MEMORY_SPACE | command::BUS_MASTER
        );
        // Every memory BAR is mapped, nothing else.
        for (i, bar) in d.bars.iter().enumerate() {
            kassert_eq!(res.bar(i).is_some(), matches!(bar, Some(pci::Bar::Memory { .. })));
        }
        // Once per function, whoever asks.
        kassert_eq!(pci::claim(d.addr, "ktest-again").map(|_| ()), Err(ClaimError::Taken(Binding::Probing("ktest"))));
        pci::probed(d.addr, "ktest", Ok(()));
        kassert_eq!(pci::binding(d.addr), Some(Binding::Bound("ktest")));
        kassert!(matches!(pci::claim(d.addr, "ktest"), Err(ClaimError::Taken(Binding::Bound("ktest")))));
        Ok(())
    }
);

crate::ktest!(
    fn a_claim_that_cant_map_a_bar_is_refused_and_recorded() {
        // QEMU's VGA (1234:1111): its BAR 0 holds the framebuffer, which the
        // HHDM maps, so `map_mmio` refuses it.
        use crate::pci::{self, BarWhy, Binding, ClaimError};
        use crate::{kassert, kassert_eq};
        let vga = pci::devices().iter().find(|d| (d.vendor, d.device) == (0x1234, 0x1111)).ok_or("no VGA")?;
        let Err(ClaimError::Bar(e)) = pci::claim(vga.addr, "ktest") else { return Err("the VGA claim wasn't refused") };
        kassert_eq!((e.addr, e.index), (vga.addr, 0));
        kassert!(matches!(e.why, BarWhy::Mmio { error: crate::mm::paging::MmioError::Ram { .. }, .. }));
        let Some(Binding::NotProbed(why)) = pci::binding(vga.addr) else { return Err("not recorded as not probed") };
        kassert_eq!(why, alloc::format!("{e}"));
        kassert!(why.starts_with("0000:00:01.0 BAR 0: mapping 0x"));
        Ok(())
    }
);

crate::ktest!(
    fn transport_brings_up_a_virtio_device_through_the_hal() {
        use crate::pci;
        use crate::{kassert, kassert_eq};
        let d = ktest_device(2)?;
        let res = pci::claim(d.addr, "ktest").map_err(|_| "the claim failed")?;
        let runs = outstanding_runs();
        let mut t = transport(res).map_err(|e| {
            crate::ktest::diag(format_args!("{e}"));
            "transport failed"
        })?;
        kassert!(PROBING.lock().is_none());
        kassert_eq!(t.device_type(), DeviceType::Block);
        kassert_eq!(t.info().addr, d.addr);
        // The status handle reads what the transport does, and a bounded
        // wait gives up: on time, or when the device reports FAILED.
        let status = t.status();
        kassert_eq!(status.get(), t.get_status());
        t.set_status(DeviceStatus::ACKNOWLEDGE);
        kassert_eq!(status.get(), DeviceStatus::ACKNOWLEDGE);
        kassert_eq!(status.wait_until(1_000_000, || false), Err(WaitError::Timeout));
        kassert_eq!(status.wait_until(1_000_000, || true), Ok(()));
        t.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::FAILED);
        kassert_eq!(status.wait_until(10_000_000_000, || false), Err(WaitError::Failed));
        // A reset; the queues the crate would set up use `dma_alloc`.
        t.set_status(DeviceStatus::empty());
        kassert_eq!(t.get_status(), DeviceStatus::empty());
        kassert_eq!(outstanding_runs(), runs);
        pci::probed(d.addr, "ktest", Ok(()));
        // Dropping it resets the device (`PciTransport`'s `Drop` writes 0 and
        // spins until the status reads 0).
        drop(t);
        Ok(())
    }
);
