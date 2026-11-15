//! The adapter for `virtio-drivers` (docs/ARCHITECTURE.md, section 7):
//! [`transport`] turns a claimed virtio PCI function into a
//! [`VirtioTransport`] the crate's device drivers run on, and [`VirtioHal`]
//! gives the crate DMA memory, owned by the frame allocator (hard rule 2).
//!
//! - **DMA runs.** Every contiguous run handed to the crate (`dma_alloc`,
//!   and each bounce buffer `share` makes) is an `OwnedRun` the table of
//!   outstanding runs owns until it comes back ([`RUNS`] entries, no heap,
//!   its own lock, taken before the frame allocator's: a run is freed with
//!   the table's lock held). Each entry records its kind (a DMA buffer, or a
//!   bounce buffer of a given length) and the device whose probe made it.
//!   `dma_dealloc` and `unshare` free only a run recorded at that address
//!   with that kind and length, so the table catches a wrong length or
//!   kind, an address never handed out, and a free of a run not
//!   outstanding (not a stale free after the frames were handed out again
//!   at the same address). A refusal is logged with what was recorded.
//! - **Bounce buffers, not address translation.** The crate shares
//!   whatever buffer a request uses (a caller's slice, a driver's `Box`, a
//!   stack local), and the device needs one physical address for it, but
//!   the heap and the stacks are windows mapped one page at a time: a
//!   buffer crossing a page there isn't physically contiguous. So `share`
//!   copies into (or zeroes) a fresh run and `unshare` copies back and
//!   frees it. Known costs and limits: an allocation and a copy per buffer
//!   (each posted 8-byte input event holds a 4 KiB frame); `share` has no
//!   error path, so out of frames or table entries it panics (saying how
//!   many runs are outstanding); buffers still posted when their queue is
//!   dropped are never unshared (`VirtQueue` has no `Drop` that does), so a
//!   probe failing partway leaks frames and table entries
//!   (`drivers::probe_all` reports the runs its probe left).
//! - **Drop order.** DMA memory must not be freed under a live device:
//!   reset the transport before any queue is dropped. A driver keeps its
//!   queues in a struct field declared after the transport (fields drop in
//!   order), or drops the transport first; the crate's device structs own
//!   both and reset in their `Drop`. (#97 drives its own `VirtQueue`.)
//! - **MMIO.** `mmio_phys_to_virt` only looks up the BAR mappings of the
//!   device [`transport`] is probing (the claim mapped them); it never maps,
//!   and panics outside a probe or outside that device's BARs, naming it
//!   (the check before the probe makes that unreachable).
//! - **Setup and waiting.** The crate's device structs don't expose their
//!   transport, so [`VirtioTransport::status`] hands out a [`StatusHandle`]
//!   on the device status register. Every driver calls
//!   [`StatusHandle::check_ready`] right after the crate's `new()`: the
//!   crate writes `FEATURES_OK` without reading it back (virtio 1.2 §3.1.1
//!   step 6), so a device refusing the features would otherwise end up
//!   "bound". A driver's bounded wait ([`StatusHandle::wait_until`]) gives
//!   up on time, or when the device sets `DEVICE_NEEDS_RESET` (or a driver
//!   set `FAILED`). Known limits: `VirtIORng::request_entropy` and sound's
//!   control requests spin unbounded in the crate (#97 drives its own
//!   queue; #99's control path is done once at setup); `PciTransport`'s
//!   `Drop` spins until the status reads 0 (the [`VirtioTransport`]'s
//!   `Drop` waits a bounded time first and logs a device that doesn't
//!   reset); [`transport`] must be called from the probe itself (it runs
//!   under the probe lock, one device at a time). Drivers poll until MSI-X
//!   (#96).
//! - **One aliasing exception.** The crate holds the common configuration
//!   as a `UniqueMmioPointer` (safe-mmio: nothing else may access it), and
//!   the `Hal` promises `mmio_phys_to_virt`'s memory isn't aliased. The
//!   [`VirtioTransport`] keeps a second pointer to it anyway, for the
//!   status handle and the notify check here (and #96's vectors). That is
//!   benign: both sides make only volatile accesses through raw pointers,
//!   `device_status` is a plain byte register that doesn't depend on
//!   `queue_select`, and the wrapper writes `queue_select` only through
//!   `&mut self`, when the crate isn't in the middle of an access (it
//!   rewrites `queue_select` before each queue access of its own).
//!
//! Locks: the probe lock, then the probe scope or ECAM; the run table
//! before the frame allocator.

// The first driver on this adapter is rng (#97): until then only the ktests
// and `probe_all`'s bookkeeping use it.
#![cfg_attr(not(feature = "ktest"), allow(dead_code))]

use alloc::vec::Vec;
use core::fmt;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

use log::{error, warn};
use lumen_mm::{FRAME_SIZE, FreeError, PhysAddr, PhysFrame};
use lumen_pci::virtio::{self as caps, CapProblem, VirtioCap, common};
use lumen_pci::{ConfigSpace, PciAddr, cap};
use virtio_drivers::transport::pci::PciTransport;
use virtio_drivers::transport::pci::bus::{DeviceFunction, PciRoot};
use virtio_drivers::transport::{DeviceStatus, DeviceType, InterruptStatus, Transport};
use virtio_drivers::{BufferDirection, Hal};

use crate::mm::frame;
use crate::pci::{BarMapping, DeviceInfo, DeviceResources};
use crate::sync::SpinLock;

/// Entries in the table of outstanding DMA runs. From the counts in
/// section 7: input 36 (32 posted events and its rings) plus sound 40 idle
/// plus rng about 3 is about 79 at boot; with sound's 96 buffers in flight
/// and 3 for a control request, about 178 while it streams.
const RUNS: usize = 256;

/// What a run in the table was handed out as: `dma_dealloc` frees only a
/// `Dma` run of that many frames, `unshare` only a `Bounce` run of that
/// buffer length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Dma,
    Bounce { len: usize },
}

/// An outstanding run: owned here until it comes back.
#[derive(Debug)]
struct Entry {
    run: OwnedRun,
    kind: Kind,
    /// Whose probe was running when it was handed out.
    owner: Option<PciAddr>,
}

type Runs = [Option<Entry>; RUNS];

static TABLE: SpinLock<Runs> = SpinLock::new([const { None }; RUNS]);

/// The function whose probe is running (`drivers::probe_all`): runs handed
/// out meanwhile are recorded as its. `NO_OWNER` when none.
static OWNER: AtomicU64 = AtomicU64::new(NO_OWNER);
const NO_OWNER: u64 = u64::MAX;

fn pack(a: PciAddr) -> u64 {
    u64::from(a.segment()) << 16 | u64::from(a.bus()) << 8 | u64::from(a.device()) << 3 | u64::from(a.function())
}

fn unpack(v: u64) -> Option<PciAddr> {
    (v != NO_OWNER).then(|| PciAddr::new((v >> 16) as u16, (v >> 8) as u8, (v >> 3) as u8 & 31, v as u8 & 7)).flatten()
}

/// Record runs handed out from now on as `addr`'s (or nobody's).
pub fn set_probe_owner(addr: Option<PciAddr>) {
    OWNER.store(addr.map_or(NO_OWNER, pack), Ordering::Relaxed);
}

/// How many DMA runs are outstanding (handed to the crate, not yet freed).
pub fn outstanding_runs() -> usize {
    TABLE.lock().iter().flatten().count()
}

/// How many of them were handed out while `addr`'s probe ran.
pub fn outstanding_runs_of(addr: PciAddr) -> usize {
    TABLE.lock().iter().flatten().filter(|e| e.owner == Some(addr)).count()
}

/// Contiguous frames from the frame allocator, freed when dropped.
#[derive(Debug)]
#[must_use = "dropping a run frees it"]
struct OwnedRun {
    first: PhysFrame,
    count: usize,
}

impl OwnedRun {
    /// `count` contiguous frames, or `None` if no such run is free (or
    /// `count` is 0).
    fn alloc(count: usize) -> Option<OwnedRun> {
        if count == 0 {
            return None;
        }
        frame::alloc_contiguous(count, 1).map(|first| OwnedRun { first, count })
    }

    fn phys(&self) -> u64 {
        self.first.as_u64()
    }

    fn len(&self) -> usize {
        self.count * FRAME_SIZE as usize
    }

    /// The run through the HHDM (usable RAM, which the HHDM maps).
    fn ptr(&self) -> *mut u8 {
        crate::mm::phys_to_virt(self.first.start()).as_mut_ptr()
    }

    /// Free the run now, reporting a failure (a kernel bug) to the caller.
    fn free(self) -> Result<(), FreeError> {
        let run = core::mem::ManuallyDrop::new(self);
        // SAFETY: as in `drop` (and `ManuallyDrop` keeps it from running).
        unsafe { frame::free_contiguous(run.first, run.count) }
    }
}

impl Drop for OwnedRun {
    fn drop(&mut self) {
        // SAFETY: a live `OwnedRun` owns its frames (from `alloc`); a device
        // it was shared with has finished with it, because only
        // `dma_dealloc` and `unshare` take a recorded run out of the table.
        unsafe { frame::free_contiguous(self.first, self.count) }.unwrap_or_else(|e| {
            panic!("virtio: freeing the DMA run at {:#x} ({} frames): {e:?}", self.phys(), self.count)
        });
    }
}

/// Whether the table has room for one more run.
fn has_room(table: &Runs) -> bool {
    table.iter().any(|e| e.is_none())
}

/// Record `run` (the caller checked [`has_room`]): its physical address.
fn record(table: &mut Runs, run: OwnedRun, kind: Kind) -> u64 {
    let entry = table.iter_mut().find(|e| e.is_none()).expect("has_room was checked");
    let owner = unpack(OWNER.load(Ordering::Relaxed));
    entry.insert(Entry { run, kind, owner }).run.phys()
}

/// Why [`take`] found no run.
#[derive(Debug)]
enum Miss {
    /// Nothing outstanding starts at that address.
    NotOutstanding,
    /// A run starts there, but of another kind or length.
    Differs { kind: Kind, frames: usize },
}

/// Take out the run at `paddr` of `kind` and `frames` frames.
fn take(table: &mut Runs, paddr: u64, kind: Kind, frames: usize) -> Result<OwnedRun, Miss> {
    let entry =
        table.iter_mut().find(|e| e.as_ref().is_some_and(|e| e.run.phys() == paddr)).ok_or(Miss::NotOutstanding)?;
    let e = entry.as_ref().expect("found");
    if (e.kind, e.run.count) != (kind, frames) {
        return Err(Miss::Differs { kind: e.kind, frames: e.run.count });
    }
    Ok(entry.take().expect("found").run)
}

/// Frames for `len` bytes (at least one).
fn frames_for(len: usize) -> usize {
    len.div_ceil(FRAME_SIZE as usize).max(1)
}

/// The function being probed and its BAR mappings, while [`transport`] runs
/// `PciTransport::new`.
static PROBING: SpinLock<Option<(PciAddr, [Option<BarMapping>; 6])>> = SpinLock::new(None);

/// Held across a probe's `PciTransport::new`: one device at a time.
static PROBE: SpinLock<()> = SpinLock::new(());

/// `virtio-drivers`' hardware abstraction: DMA from the frame allocator,
/// bounce buffers for shared buffers, and MMIO through the probed device's
/// BAR mappings.
#[derive(Debug)]
pub struct VirtioHal;

// SAFETY: `dma_alloc` returns zeroed, page-aligned, physically contiguous
// runs the allocator gave only to the crate (owned by the table until
// `dma_dealloc` takes them back); `mmio_phys_to_virt` returns addresses
// inside a BAR the registry mapped uncached for the kernel's life (the
// common configuration is aliased once more, by `VirtioTransport`, a
// deliberate and benign exception: see the module doc); `share` returns a
// fresh run holding the buffer's contents (or zeros) that nothing else uses
// until `unshare`.
unsafe impl Hal for VirtioHal {
    fn dma_alloc(pages: usize, _direction: BufferDirection) -> (u64, NonNull<u8>) {
        let mut table = TABLE.lock();
        let outstanding = table.iter().flatten().count();
        let run = if pages == 0 || !has_room(&table) { None } else { OwnedRun::alloc(pages) };
        let Some(run) = run else {
            drop(table);
            if pages == 0 {
                warn!(target: "virtio", "dma_alloc of 0 pages refused");
            } else if outstanding == RUNS {
                warn!(target: "virtio", "dma_alloc of {pages} pages: the run table is full ({RUNS} outstanding)");
            } else {
                warn!(target: "virtio", "dma_alloc: no {pages} contiguous free frames ({outstanding} runs outstanding)");
            }
            return (0, NonNull::dangling());
        };
        // SAFETY: a fresh run nothing else uses, in the HHDM.
        unsafe { core::ptr::write_bytes(run.ptr(), 0, run.len()) };
        let ptr = NonNull::new(run.ptr()).expect("HHDM addresses aren't null");
        (record(&mut table, run, Kind::Dma), ptr)
    }

    unsafe fn dma_dealloc(paddr: u64, vaddr: NonNull<u8>, pages: usize) -> i32 {
        let refused = |why: fmt::Arguments<'_>| {
            error!(target: "virtio", "dma_dealloc({paddr:#x}, {:p}, {pages} pages) refused: {why}", vaddr.as_ptr());
            -1
        };
        let Some(phys) = PhysAddr::new(paddr) else { return refused(format_args!("not a physical address")) };
        let hhdm = crate::mm::phys_to_virt(phys).as_mut_ptr::<u8>();
        if vaddr.as_ptr() != hhdm {
            return refused(format_args!("dma_alloc returned {hhdm:p} for it"));
        }
        let mut table = TABLE.lock();
        let taken = take(&mut table, paddr, Kind::Dma, pages);
        let freed = taken.map(|run| run.free());
        drop(table);
        match freed {
            Ok(Ok(())) => 0,
            Ok(Err(e)) => {
                error!(target: "virtio", "dma_dealloc({paddr:#x}, {pages} pages): the frame allocator refused: {e:?}");
                -2
            }
            Err(Miss::NotOutstanding) => refused(format_args!("no run is outstanding there")),
            Err(Miss::Differs { kind, frames }) => {
                refused(format_args!("the run there is {kind:?} of {frames} frames"))
            }
        }
    }

    unsafe fn mmio_phys_to_virt(paddr: u64, size: usize) -> NonNull<u8> {
        let scope = PROBING.lock();
        let (addr, bars) =
            scope.as_ref().unwrap_or_else(|| panic!("virtio: mmio_phys_to_virt({paddr:#x}) outside a probe"));
        let virt = bars.iter().flatten().find_map(|m| m.virt_of(paddr, size as u64));
        let virt = virt.unwrap_or_else(|| panic!("virtio: {paddr:#x} (+{size:#x}) isn't in {addr}'s BARs"));
        NonNull::new(virt.as_mut_ptr()).expect("mapped addresses aren't null")
    }

    unsafe fn share(buffer: NonNull<[u8]>, direction: BufferDirection) -> u64 {
        let len = buffer.len();
        let mut table = TABLE.lock();
        let outstanding = table.iter().flatten().count();
        if !has_room(&table) {
            panic!("virtio: sharing a {len}-byte buffer: the run table is full ({RUNS} outstanding)");
        }
        let run = OwnedRun::alloc(frames_for(len)).unwrap_or_else(|| {
            panic!("virtio: out of frames bouncing a {len}-byte buffer ({outstanding} runs outstanding)")
        });
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
        record(&mut table, run, Kind::Bounce { len })
    }

    unsafe fn unshare(paddr: u64, buffer: NonNull<[u8]>, direction: BufferDirection) {
        let (len, frames) = (buffer.len(), frames_for(buffer.len()));
        let mut table = TABLE.lock();
        let run = take(&mut table, paddr, Kind::Bounce { len }, frames).unwrap_or_else(|miss| {
            let outstanding = table.iter().flatten().count();
            panic!("virtio: unshare of {paddr:#x} ({len} bytes): {miss:?} ({outstanding} runs outstanding)")
        });
        if direction == BufferDirection::DeviceToDriver {
            // SAFETY: the crate passes the buffer it shared, valid for writes
            // of `len` bytes; the device has finished with the run.
            unsafe { core::ptr::copy_nonoverlapping(run.ptr(), buffer.as_ptr().cast::<u8>(), len) };
        }
        run.free().unwrap_or_else(|e| panic!("virtio: freeing the bounce run at {paddr:#x} ({frames} frames): {e:?}"));
    }
}

/// Why a device couldn't be brought up.
#[derive(Debug)]
pub enum ProbeError {
    /// A virtio structure `PciTransport::new` would use isn't inside a
    /// mapped memory BAR, is too short, can't be read inside the header, or
    /// a queue's notify register is outside the notify structure.
    Capability(CapProblem),
    /// `PciTransport::new` refused the device.
    Transport(virtio_drivers::transport::pci::VirtioPciError),
    /// The device driver's setup failed.
    #[cfg_attr(feature = "ktest", allow(dead_code))] // first user: rng (#97)
    Device(virtio_drivers::Error),
    /// The device status after setup isn't `FEATURES_OK` and `DRIVER_OK`
    /// (or shows `DEVICE_NEEDS_RESET` or `FAILED`): the raw byte.
    NotReady { status: u8 },
    /// A bounded wait gave up.
    Wait(WaitError),
    /// Not this driver's device after all (virtio's keyboard and tablet
    /// share an id, told apart only by their configuration): `probe_all`
    /// frees the function and offers it to the next driver row.
    NotMine,
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProbeError::Capability(p) => write!(f, "{p}"),
            ProbeError::Transport(e) => write!(f, "virtio transport: {e}"),
            ProbeError::Device(e) => write!(f, "virtio: {e}"),
            ProbeError::NotReady { status } => write!(
                f,
                "the device status reads {status:#04x} after setup (FEATURES_OK and DRIVER_OK wanted){}",
                gone(*status)
            ),
            ProbeError::Wait(e) => write!(f, "{e}"),
            ProbeError::NotMine => f.write_str("not this driver's device"),
        }
    }
}

/// What an all-ones status byte means, for messages.
fn gone(status: u8) -> &'static str {
    if status == 0xff { ": all ones, the device is gone or not decoding" } else { "" }
}

/// Bring up the virtio transport of a claimed function: the one entry
/// point, called from the driver's probe. It takes the resources by value,
/// so it runs once per device, and the transport owns them. First it checks
/// what `PciTransport::new` will look up (`lumen_pci::virtio::check`: the
/// structures `used` keeps inside mapped memory BARs and long enough, every
/// capability the crate reads inside the header); then it runs it, under
/// the probe lock, with the function's configuration space and, for
/// `mmio_phys_to_virt`, its BAR mappings; then it checks each queue's
/// notify register is inside the notify structure.
pub fn transport(res: DeviceResources) -> Result<VirtioTransport, ProbeError> {
    let info = res.info();
    let addr = info.addr;
    let found: Vec<VirtioCap> = {
        let mut config = res.config();
        info.caps.iter().filter(|c| c.id == cap::id::VENDOR).map(|c| VirtioCap::read(&mut config, addr, c)).collect()
    };
    let bars: [Option<BarMapping>; 6] = core::array::from_fn(|i| res.bar(i));
    let used = caps::check(&found, |i| bars[usize::from(i)].map(|m| m.size())).map_err(ProbeError::Capability)?;
    let made = {
        let _one_at_a_time = PROBE.lock();
        *PROBING.lock() = Some((addr, bars));
        let df = DeviceFunction { bus: addr.bus(), device: addr.device(), function: addr.function() };
        let made = PciTransport::new::<VirtioHal, _>(&mut PciRoot::new(res.config()), df);
        *PROBING.lock() = None;
        made
    };
    let inner = made.map_err(ProbeError::Transport)?;
    let cfg = used.common.expect("PciTransport::new found the common configuration");
    let bar = bars[usize::from(cfg.bar)].expect("checked: a mapped memory BAR");
    let base = bar
        .virt_of(bar.phys().as_u64() + u64::from(cfg.offset), u64::from(common::LEN))
        .expect("checked: the common configuration is inside its BAR");
    let common = NonNull::new(base.as_mut_ptr::<u8>()).expect("mapped addresses aren't null");
    let mut t = VirtioTransport { inner, common, res };
    let notify = used.notify.expect("PciTransport::new found the notify structure");
    let multiplier = t.res.config().read(addr, u16::from(notify.at + caps::NOTIFY_MULTIPLIER_AT));
    for queue in 0..t.common_read16(common::NUM_QUEUES) {
        t.common_write16(common::QUEUE_SELECT, queue);
        let off = t.common_read16(common::QUEUE_NOTIFY_OFF);
        // On failure the transport drops here, resetting the device.
        caps::notify_fits(notify, multiplier, queue, off).map_err(ProbeError::Capability)?;
    }
    Ok(t)
}

/// A virtio PCI device's transport: `virtio-drivers`' `PciTransport`, which
/// it delegates to, plus what the crate leaves out: a [`StatusHandle`], and
/// the checked common configuration's base (#96 writes its MSI-X vectors
/// there; the per-transport error slot for a refused vector is #96's).
/// Owns the device's resources. Dropping it resets the device, waiting a
/// bounded time and logging a device that doesn't reset before
/// `PciTransport`'s own `Drop` spins on it.
#[derive(Debug)]
pub struct VirtioTransport {
    inner: PciTransport,
    common: NonNull<u8>,
    res: DeviceResources,
}

// SAFETY: `common` points into a BAR mapped for the kernel's life, reached
// only through `&self`/`&mut self` (as `PciTransport` itself is Send/Sync).
unsafe impl Send for VirtioTransport {}
// SAFETY: as for `Send`; `&self` only reads.
unsafe impl Sync for VirtioTransport {}

impl VirtioTransport {
    /// A handle on the device status register, for [`StatusHandle::
    /// check_ready`] and a driver's bounded waits once the crate's device
    /// struct owns this transport.
    pub fn status(&self) -> StatusHandle {
        // SAFETY: `common` is the checked base of the 56-byte common
        // configuration, so +20 is inside it.
        let reg = unsafe { self.common.add(common::DEVICE_STATUS as usize) };
        StatusHandle { reg }
    }

    pub fn info(&self) -> &'static DeviceInfo {
        self.res.info()
    }

    fn common_read16(&self, offset: u64) -> u16 {
        // SAFETY: a 16-bit field of the checked common configuration
        // (2-byte aligned: the structure is in a BAR at a 4-byte aligned
        // offset), mapped uncached; a volatile read (the aliasing exception
        // in the module doc).
        unsafe { self.common.add(offset as usize).cast::<u16>().read_volatile() }
    }

    fn common_write16(&mut self, offset: u64, value: u16) {
        // SAFETY: as in `common_read16`; through `&mut self`, so the crate's
        // transport isn't in the middle of an access.
        unsafe { self.common.add(offset as usize).cast::<u16>().write_volatile(value) }
    }
}

/// How long [`VirtioTransport`]'s `Drop` waits for the device to reset.
const RESET_WAIT_NS: u64 = 100_000_000;

impl Drop for VirtioTransport {
    fn drop(&mut self) {
        self.inner.set_status(DeviceStatus::empty());
        let status = self.status();
        let reset = status.wait_until(RESET_WAIT_NS, || status.raw() == 0);
        if reset.is_err() {
            error!(
                target: "virtio",
                "{}: the device status still reads {:#04x} {} ms after a reset{}: waiting forever in PciTransport::drop",
                self.res.info().addr,
                status.raw(),
                RESET_WAIT_NS / 1_000_000,
                gone(status.raw())
            );
        }
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
/// crate's device struct owns its transport. It reads through a second
/// pointer into the common configuration, which the crate holds as unique:
/// the deliberate, benign exception the module doc describes (single-byte
/// volatile accesses on both sides, to a register no selector affects).
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
    /// `ready()` wasn't true after `waited_ns`; the status byte then.
    Timeout { waited_ns: u64, status: u8 },
    /// The device set `DEVICE_NEEDS_RESET`.
    NeedsReset { status: u8 },
    /// The status reads `FAILED` (a driver gave up on the device, or all
    /// ones: it is gone).
    Failed { status: u8 },
}

impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            WaitError::Timeout { waited_ns, status } => write!(
                f,
                "the device didn't answer in {} ms (status {status:#04x}{})",
                waited_ns / 1_000_000,
                gone(status)
            ),
            WaitError::NeedsReset { status } => write!(f, "the device needs a reset (status {status:#04x})"),
            WaitError::Failed { status } => {
                write!(f, "the device status reads FAILED ({status:#04x}{})", gone(status))
            }
        }
    }
}

impl StatusHandle {
    /// The raw status byte.
    pub fn raw(&self) -> u8 {
        // SAFETY: the register is in a BAR mapped uncached for the kernel's
        // life (checked by `transport`); a byte read, volatile, aliasing the
        // crate's unique pointer as the type's doc explains.
        unsafe { self.reg.read_volatile() }
    }

    pub fn get(&self) -> DeviceStatus {
        DeviceStatus::from_bits_truncate(u32::from(self.raw()))
    }

    /// After the crate's `new()`: the device accepted the features and is
    /// running (`FEATURES_OK` and `DRIVER_OK`, neither `DEVICE_NEEDS_RESET`
    /// nor `FAILED`). The crate writes `FEATURES_OK` without reading it
    /// back, so every driver calls this before it reports the device bound.
    pub fn check_ready(&self) -> Result<(), ProbeError> {
        let status = self.raw();
        let s = DeviceStatus::from_bits_truncate(u32::from(status));
        let ready = s.contains(DeviceStatus::FEATURES_OK | DeviceStatus::DRIVER_OK)
            && !s.intersects(DeviceStatus::DEVICE_NEEDS_RESET | DeviceStatus::FAILED);
        if ready { Ok(()) } else { Err(ProbeError::NotReady { status }) }
    }

    /// Wait until `ready()`, at most `timeout_ns` nanoseconds: checked first,
    /// after each look at the status, and once more before giving up (the
    /// thread may have been preempted past the deadline). Gives up early if
    /// the status shows `DEVICE_NEEDS_RESET` or `FAILED`. Busy-waits: call it
    /// without a lock held (after `time::init`).
    pub fn wait_until(&self, timeout_ns: u64, mut ready: impl FnMut() -> bool) -> Result<(), WaitError> {
        let start = crate::time::now_ns();
        let deadline = start.saturating_add(timeout_ns);
        loop {
            if ready() {
                return Ok(());
            }
            let status = self.raw();
            let s = DeviceStatus::from_bits_truncate(u32::from(status));
            if s.contains(DeviceStatus::FAILED) {
                return Err(WaitError::Failed { status });
            }
            if s.contains(DeviceStatus::DEVICE_NEEDS_RESET) {
                return Err(WaitError::NeedsReset { status });
            }
            let now = crate::time::now_ns();
            if now >= deadline {
                return if ready() { Ok(()) } else { Err(WaitError::Timeout { waited_ns: now - start, status }) };
            }
            core::hint::spin_loop();
        }
    }
}

/// The ktest machine's virtio device of `kind` (virtio 1.2 §5 device type:
/// 1 network, 2 block, 5 balloon, 9 9p; no driver takes those in M3).
#[cfg(feature = "ktest")]
fn ktest_device(kind: u16) -> Result<&'static DeviceInfo, &'static str> {
    crate::pci::devices()
        .iter()
        .find(|d| d.virtio_type() == Some(kind))
        .ok_or("the ktest machine lacks a virtio device (tests/scenarios/test_ktest.py attaches them)")
}

#[cfg(feature = "ktest")]
fn hhdm(paddr: u64) -> *mut u8 {
    crate::mm::phys_to_virt(PhysAddr::new(paddr).expect("physical")).as_mut_ptr::<u8>()
}

crate::ktest!(
    fn dma_frames_are_owned_and_freed() {
        use crate::{kassert, kassert_eq};
        let before = outstanding_runs();
        // Dirty a run and free it: the allocator hands the same frames out
        // next, so `dma_alloc` must zero them.
        let dirty = OwnedRun::alloc(3).ok_or("no run")?;
        // SAFETY: our fresh run, 3 frames.
        unsafe { core::ptr::write_bytes(dirty.ptr(), 0xa5, dirty.len()) };
        let dirty_phys = dirty.phys();
        dirty.free().map_err(|_| "free")?;
        let (paddr, vaddr) = VirtioHal::dma_alloc(3, BufferDirection::DriverToDevice);
        kassert_eq!(paddr, dirty_phys);
        // Allocated from the allocator (owned by it, so not reserved: hard
        // rule 2), recorded, zeroed and reached through the HHDM.
        let frames = || (0..3).map(|i| PhysFrame::from_u64(paddr + i * FRAME_SIZE).expect("aligned"));
        kassert!(frames().all(frame::is_allocated));
        kassert_eq!(outstanding_runs(), before + 1);
        kassert_eq!(vaddr.as_ptr(), hhdm(paddr));
        // SAFETY: the run is ours until `dma_dealloc`, 3 frames long.
        let bytes = unsafe { core::slice::from_raw_parts(hhdm(paddr), 3 * FRAME_SIZE as usize) };
        kassert!(bytes.iter().all(|&b| b == 0));
        // A wrong length, another address (with its own HHDM address, so the
        // table is asked), a wrong virtual address: refused, freeing nothing.
        let other = paddr + FRAME_SIZE;
        let other_v = NonNull::new(hhdm(other)).ok_or("null")?;
        // SAFETY: refused calls touch nothing.
        unsafe {
            kassert_eq!(VirtioHal::dma_dealloc(paddr, vaddr, 2), -1);
            kassert_eq!(VirtioHal::dma_dealloc(other, other_v, 3), -1);
            kassert_eq!(VirtioHal::dma_dealloc(paddr, NonNull::dangling(), 3), -1);
        }
        kassert_eq!(outstanding_runs(), before + 1);
        kassert!(frames().all(frame::is_allocated));
        // SAFETY: the run this test allocated, no longer used.
        kassert_eq!(unsafe { VirtioHal::dma_dealloc(paddr, vaddr, 3) }, 0);
        kassert!(frames().all(|f| !frame::is_allocated(f)));
        kassert_eq!(outstanding_runs(), before);
        // Freed once: a second free is refused, not a double free.
        // SAFETY: refused.
        kassert_eq!(unsafe { VirtioHal::dma_dealloc(paddr, vaddr, 3) }, -1);
        // Nothing for 0 pages.
        kassert_eq!(VirtioHal::dma_alloc(0, BufferDirection::DriverToDevice).0, 0);
        kassert_eq!(outstanding_runs(), before);
        Ok(())
    }
);

crate::ktest!(
    fn a_run_comes_back_only_as_what_it_was_handed_out_as() {
        use crate::{kassert, kassert_eq};
        let before = outstanding_runs();
        // A one-frame DMA run and a one-frame bounce run: each refuses the
        // other's way back.
        let (dma, dma_v) = VirtioHal::dma_alloc(1, BufferDirection::DriverToDevice);
        kassert!(dma != 0);
        let buf = [7u8; 100];
        let shared = NonNull::from(&buf[..]);
        // SAFETY: `buf` (a stack local, as the crate shares some) outlives
        // the share and isn't written meanwhile.
        let bounce = unsafe { VirtioHal::share(shared, BufferDirection::DriverToDevice) };
        let bounce_v = NonNull::new(hhdm(bounce)).ok_or("null")?;
        // SAFETY: refused: frees nothing.
        kassert_eq!(unsafe { VirtioHal::dma_dealloc(bounce, bounce_v, 1) }, -1);
        kassert_eq!(outstanding_runs(), before + 2);
        // The table matches the bounce run by its length too.
        {
            let mut table = TABLE.lock();
            kassert!(matches!(
                take(&mut table, dma, Kind::Bounce { len: 100 }, 1),
                Err(Miss::Differs { kind: Kind::Dma, frames: 1 })
            ));
            kassert!(matches!(
                take(&mut table, bounce, Kind::Bounce { len: 99 }, 1),
                Err(Miss::Differs { kind: Kind::Bounce { len: 100 }, frames: 1 })
            ));
            kassert!(matches!(take(&mut table, 0x1000, Kind::Dma, 1), Err(Miss::NotOutstanding)));
        }
        // SAFETY: the buffer and address `share` returned.
        unsafe { VirtioHal::unshare(bounce, shared, BufferDirection::DriverToDevice) };
        // SAFETY: the run this test allocated.
        kassert_eq!(unsafe { VirtioHal::dma_dealloc(dma, dma_v, 1) }, 0);
        kassert_eq!(outstanding_runs(), before);
        // A run handed out during a probe is that device's.
        let owner = lumen_pci::PciAddr::new(0, 0, 3, 0).ok_or("addr")?;
        set_probe_owner(Some(owner));
        let (p, v) = VirtioHal::dma_alloc(1, BufferDirection::DriverToDevice);
        set_probe_owner(None);
        kassert_eq!(outstanding_runs_of(owner), 1);
        // SAFETY: the run this test allocated.
        kassert_eq!(unsafe { VirtioHal::dma_dealloc(p, v, 1) }, 0);
        kassert_eq!(outstanding_runs_of(owner), 0);
        kassert_eq!(unpack(pack(owner)), Some(owner));
        kassert_eq!(unpack(NO_OWNER), None);
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
        let shared = NonNull::from(&mut *buf);

        // To the device: its copy is one contiguous run holding the bytes;
        // whatever the device does to it isn't copied back.
        // SAFETY: `buf` stays alive and untouched until `unshare`.
        let paddr = unsafe { VirtioHal::share(shared, BufferDirection::DriverToDevice) };
        kassert!(frame::is_allocated(PhysFrame::from_u64(paddr).ok_or("not a frame")?));
        kassert_eq!(outstanding_runs(), before + 1);
        // SAFETY: the bounce run, 1 frame, ours until `unshare`.
        let copy = unsafe { core::slice::from_raw_parts_mut(hhdm(paddr), 600) };
        kassert!(copy.iter().enumerate().all(|(i, &b)| b == i as u8 ^ 0x5a));
        copy.fill(0xee);
        // SAFETY: the buffer and address `share` returned.
        unsafe { VirtioHal::unshare(paddr, shared, BufferDirection::DriverToDevice) };
        kassert!(buf.iter().enumerate().all(|(i, &b)| b == i as u8 ^ 0x5a));
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

        // Exactly a page takes one frame, a page and a byte two, two pages
        // and a byte three; an empty buffer one.
        kassert_eq!((frames_for(0), frames_for(4096), frames_for(4097)), (1, 1, 2));
        let big = vec![1u8; 2 * FRAME_SIZE as usize + 1];
        let shared = NonNull::from(&big[..]);
        // SAFETY: `big` outlives the share and isn't written meanwhile.
        let paddr = unsafe { VirtioHal::share(shared, BufferDirection::DriverToDevice) };
        kassert!((0..3).all(|i| frame::is_allocated(PhysFrame::from_u64(paddr + i * FRAME_SIZE).expect("aligned"))));
        // SAFETY: as above.
        unsafe { VirtioHal::unshare(paddr, shared, BufferDirection::DriverToDevice) };
        let empty: &[u8] = &[];
        let shared = NonNull::from(empty);
        // SAFETY: an empty buffer: nothing read or written.
        let paddr = unsafe { VirtioHal::share(shared, BufferDirection::DeviceToDriver) };
        kassert!(frame::is_allocated(PhysFrame::from_u64(paddr).ok_or("not a frame")?));
        // SAFETY: as above.
        unsafe { VirtioHal::unshare(paddr, shared, BufferDirection::DeviceToDriver) };
        kassert_eq!(outstanding_runs(), before);
        Ok(())
    }
);

crate::ktest!(
    fn a_claimed_device_has_memory_space_and_bus_master_on_and_io_off() {
        use lumen_pci::command::{BUS_MASTER, IO_SPACE, MEMORY_SPACE};
        use lumen_pci::reg;

        use crate::pci::{self, Binding, ClaimError, Probed, ktest_command};
        use crate::{kassert, kassert_eq};
        let d = ktest_device(1)?;
        let bits = MEMORY_SPACE | BUS_MASTER | IO_SPACE;
        // Firmware left it decoding I/O only, mastering nothing.
        kassert_eq!(ktest_command(d.addr, Some(IO_SPACE)) & bits, IO_SPACE);
        let res = pci::claim(d.addr, "ktest").map_err(|_| "the claim failed")?;
        kassert_eq!(pci::binding(d.addr), Some(Binding::Probing("ktest")));
        kassert_eq!(res.config().read16(d.addr, reg::COMMAND) & bits, MEMORY_SPACE | BUS_MASTER);
        // Every memory BAR is mapped, nothing else.
        for (i, bar) in d.bars.iter().enumerate() {
            kassert_eq!(res.bar(i).is_some(), matches!(bar, Some(pci::Bar::Memory { .. })));
        }
        kassert_eq!(res.bar(6), None);
        // Once per function, whoever asks.
        kassert_eq!(pci::claim(d.addr, "ktest-again").map(|_| ()), Err(ClaimError::Taken(Binding::Probing("ktest"))));
        pci::probed(d.addr, "ktest", Probed::Bound);
        kassert_eq!(pci::binding(d.addr), Some(Binding::Bound("ktest")));
        kassert!(matches!(pci::claim(d.addr, "ktest"), Err(ClaimError::Taken(Binding::Bound("ktest")))));
        // How `lspci` shows each binding.
        let shown = [
            (Binding::Free, "no driver"),
            (Binding::NotProbed("rng", "why".into()), "rng: not probed: why"),
            (Binding::Probing("rng"), "rng: probing"),
            (Binding::Bound("rng"), "rng: bound"),
            (Binding::Failed("rng", "why".into()), "rng: failed: why"),
        ];
        for (b, text) in shown {
            kassert_eq!(alloc::format!("{b}"), text);
        }
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
        let Some(Binding::NotProbed(driver, why)) = pci::binding(vga.addr) else {
            return Err("not recorded as not probed");
        };
        kassert_eq!((driver, why.clone()), ("ktest", alloc::format!("{e}")));
        kassert!(why.starts_with("0000:00:01.0 BAR 0: mapping 0x"));
        // `lspci`'s driver column.
        let row = crate::shell::sysinfo::pci_rows().into_iter().find(|(a, _)| *a == vga.addr).ok_or("no row")?;
        kassert_eq!(row.1, alloc::format!("ktest: not probed: {why}"));
        Ok(())
    }
);

crate::ktest!(
    fn transport_brings_up_a_virtio_device_through_the_hal() {
        use crate::pci::{self, Probed};
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
        // The status handle reads what the transport does.
        let status = t.status();
        kassert_eq!(status.get(), t.get_status());
        t.set_status(DeviceStatus::ACKNOWLEDGE);
        kassert_eq!(status.raw(), 0x01);
        // Not ready until FEATURES_OK and DRIVER_OK, without NEEDS_RESET or
        // FAILED (values written here as the device would show them).
        kassert!(matches!(status.check_ready(), Err(ProbeError::NotReady { status: 0x01 })));
        let ok = DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::FEATURES_OK | DeviceStatus::DRIVER_OK;
        t.set_status(ok);
        kassert!(status.check_ready().is_ok());
        t.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::DRIVER_OK);
        kassert!(matches!(status.check_ready(), Err(ProbeError::NotReady { status: 0x07 })));
        t.set_status(ok | DeviceStatus::DEVICE_NEEDS_RESET);
        kassert!(status.check_ready().is_err());
        kassert_eq!(
            alloc::format!("{}", ProbeError::NotReady { status: 0xff }),
            "the device status reads 0xff after setup (FEATURES_OK and DRIVER_OK wanted): all ones, the device is \
             gone or not decoding"
        );
        // A bounded wait gives up on time (at least the timeout, well under
        // a second), re-polling `ready`; or when the status shows
        // DEVICE_NEEDS_RESET (which the device sets) or FAILED (a driver).
        t.set_status(DeviceStatus::ACKNOWLEDGE);
        let t0 = crate::time::now_ns();
        let Err(WaitError::Timeout { waited_ns, status: 0x01 }) = status.wait_until(2_000_000, || false) else {
            return Err("no timeout");
        };
        let took = crate::time::now_ns() - t0;
        kassert!(waited_ns >= 2_000_000 && (2_000_000..1_000_000_000).contains(&took));
        let mut polls = 0;
        kassert_eq!(
            status.wait_until(1_000_000_000, || {
                polls += 1;
                polls >= 3
            }),
            Ok(())
        );
        kassert_eq!(polls, 3);
        kassert_eq!(status.wait_until(1_000_000, || true), Ok(()));
        t.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DEVICE_NEEDS_RESET);
        kassert_eq!(status.wait_until(10_000_000_000, || false), Err(WaitError::NeedsReset { status: 0x41 }));
        t.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::FAILED);
        kassert_eq!(status.wait_until(10_000_000_000, || false), Err(WaitError::Failed { status: 0x81 }));
        // A reset; nothing was handed out for DMA.
        t.set_status(DeviceStatus::empty());
        kassert_eq!(t.get_status(), DeviceStatus::empty());
        kassert_eq!(outstanding_runs(), runs);
        pci::probed(d.addr, "ktest", Probed::Bound);
        // Dropping it resets the device: a bounded wait, then `PciTransport`'s
        // `Drop` (writes 0, spins until the status reads 0).
        drop(t);
        Ok(())
    }
);
