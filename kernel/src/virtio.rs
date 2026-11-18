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
//!   bounce buffer of a given length) and the probe attempt that made it.
//!   `dma_dealloc` and `unshare` free only a run recorded at that address
//!   with that kind and length, so the table catches a wrong length or
//!   kind, an address never handed out, and a free of a run not
//!   outstanding (not a stale free after the frames were handed out again
//!   at the same address). `dma_dealloc` logs a refusal with what was
//!   recorded and returns non-zero (the crate's `Dma::drop` then panics);
//!   `unshare` panics with it.
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
//!   probe failing partway leaks frames and table entries, and so does a
//!   driver dropping a device with a buffer still posted (rng after a
//!   request that gave up: it reports the runs it leaked; the tablet's,
//!   when its device fails) (`drivers::probe_all` reports the runs that
//!   probe attempt left: only those of the probing thread and threads made
//!   during the attempt are charged to it, not those an older driver's
//!   thread hands out meanwhile).
//! - **Drop order.** DMA memory must not be freed under a live device:
//!   reset the transport before any queue is dropped. A driver keeps its
//!   queues in a struct field declared after the transport (fields drop in
//!   order), or drops the transport first. The crate's device structs get
//!   this from their field order: `transport` is their first field (their
//!   own `Drop`s only call `queue_unset`, a no-op on PCI; `VirtIOSound` has
//!   none). A crate constructor that fails partway (`?`) drops queues it
//!   made before the transport resets the device: allowed, because the
//!   device was never set `DRIVER_OK`. (#97 drives its own `VirtQueue`.)
//! - **MMIO.** `mmio_phys_to_virt` only looks up the BAR mappings of the
//!   device [`transport`] is probing (the claim mapped them); it never maps,
//!   and panics outside a probe or outside that device's BARs, naming it
//!   (the check before the probe makes that unreachable).
//! - **Setup and waiting.** The crate's device structs don't expose their
//!   transport, so [`VirtioTransport::status`] hands out a [`StatusHandle`]
//!   on the device status register. The crate writes `FEATURES_OK` without
//!   reading it back (virtio 1.2 §3.1.1 step 6), so a device refusing the
//!   features would otherwise end up "bound": [`transport`] records the
//!   probing attempt's status handle, and `drivers::probe_all` runs
//!   [`BroughtUp::check`] when the probe returns `Ok` (no MSI-X vector
//!   problem was latched, and [`StatusHandle::check_ready`]), turning a
//!   device that isn't ready into a failed probe (drivers call
//!   [`check_device`], the same checks, earlier). A driver's bounded wait
//!   ([`StatusHandle::wait_for`]; `wait_until`, spinning, only for the
//!   ktests' made-up device) gives up on time, or when the device sets
//!   `DEVICE_NEEDS_RESET` (or a driver set `FAILED`; all ones, a device
//!   gone, shows as `FAILED`; either is seen at the next wake or the
//!   deadline). Known limits:
//!   `VirtIORng::request_entropy` and sound's control requests spin
//!   unbounded in the crate (#97 drives its own queue; #99's control
//!   requests run in its probe and on its own thread around each sound,
//!   outside the bounded waits for periods; the crate's `pcm_xfer_ok`
//!   doesn't read a period's status reply, so a period the device refused
//!   counts as played); `PciTransport`'s `Drop` spins until the status
//!   reads 0, so the [`VirtioTransport`]'s `Drop` waits a bounded time
//!   first and, for a device that doesn't reset, logs it, turns its bus
//!   mastering off and leaks the `PciTransport` instead. [`transport`] must
//!   be called from the probe itself: runs and the status handle are
//!   charged to the running probe attempt, and it runs under the probe lock,
//!   one device at a time.
//! - **Interrupts: MSI-X.** A driver passes [`transport`] its handler; at
//!   the first `queue_set` the transport gets the function's vector
//!   (`DeviceResources::msix`), writes its table entry to `msix_config`,
//!   reads it back and unmasks the entry; then, for that queue and each
//!   later one, it writes the entry to `queue_msix_vector` (which the crate
//!   never writes) and reads it back. `NO_VECTOR` read back, or no vector
//!   at all, is latched for the probe (`queue_set` returns `()`):
//!   [`check_device`] and `probe_all` turn it into a failed probe, with no
//!   polling fallback.
//!   The handler rings the driver's [`Doorbell`], which its thread waits on
//!   ([`StatusHandle::wait_for`]: a deadline, and the status checks).
//! - **One aliasing exception.** The crate holds the common configuration
//!   as a `UniqueMmioPointer` (safe-mmio: nothing else may access it), and
//!   the `Hal` promises `mmio_phys_to_virt`'s memory isn't aliased. The
//!   [`VirtioTransport`] keeps a second pointer to it anyway, for the
//!   status handle, the notify check and the MSI-X vectors here. That is
//!   benign: both sides make only volatile accesses through raw pointers,
//!   `device_status` is a plain byte register that doesn't depend on
//!   `queue_select`, and the wrapper writes `queue_select` only through
//!   `&mut self`, when the crate isn't in the middle of an access (it
//!   rewrites `queue_select` before each queue access of its own).
//!
//! Locks: the probe lock, then the probe scope or ECAM (each taken last);
//! the run table before the probe owner and the frame allocator.

// Parts of the adapter (run counts, the status bits) are used only by the
// ktests until more drivers use them.
#![cfg_attr(not(feature = "ktest"), allow(dead_code))]

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU64, Ordering};

use log::{debug, error, warn};
use lumen_mm::{FRAME_SIZE, FreeError, PhysAddr, PhysFrame};
use lumen_pci::virtio::{self as caps, CapProblem, VirtioCap, common};
use lumen_pci::{ConfigSpace, PciAddr, cap};
use virtio_drivers::transport::pci::PciTransport;
use virtio_drivers::transport::pci::bus::{DeviceFunction, PciRoot};
use virtio_drivers::transport::{DeviceStatus, DeviceType, InterruptStatus, Transport};
use virtio_drivers::{BufferDirection, Hal};

use crate::mm::frame;
use crate::pci::{BarMapping, DeviceInfo, DeviceResources, MsiError, MsiVector};
use crate::sched::WaitQueue;
use crate::sync::SpinLock;

/// Entries in the table of outstanding DMA runs. From the counts in
/// section 7: input 36 (32 posted events and its rings) plus sound 40 idle
/// plus rng about 3 is about 79 at boot; with sound's 96 buffers in flight
/// and 3 for a control request, about 178 while it streams.
const RUNS: usize = 256;

/// What a run in the table was handed out as: `dma_dealloc` frees only a
/// `Dma` run of that many pages, `unshare` only a `Bounce` run of that
/// buffer length (its frames follow from the length).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Dma { pages: usize },
    Bounce { len: usize },
}

/// An outstanding run: owned here until it comes back.
#[derive(Debug)]
struct Entry {
    run: OwnedRun,
    kind: Kind,
    /// The probe attempt that was running when it was handed out.
    owner: Option<Attempt>,
}

type Runs = [Option<Entry>; RUNS];

static TABLE: SpinLock<Runs> = SpinLock::new([const { None }; RUNS]);

/// One probe of one function by one driver row (`drivers::probe_all`):
/// runs the probing thread, or a thread made during the attempt (one the
/// probe spawned), hands out meanwhile are its, so a declined row's
/// leftovers aren't charged to the next row. Runs older threads hand out
/// meanwhile (a bound driver's thread re-posting its buffers, like the
/// tablet's) aren't charged to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub addr: PciAddr,
    id: u64,
    /// The probing thread's id (`None` before this CPU's threads run).
    thread: Option<u64>,
    /// Threads with at least this id were made during the attempt.
    watermark: u64,
}

impl Attempt {
    /// Whether a run handed out now by this thread is the attempt's.
    fn charges_this_thread(&self) -> bool {
        let me = this_thread();
        me == self.thread || me.is_some_and(|id| id >= self.watermark)
    }
}

/// The running thread's id, lock-free (`None` before this CPU's threads
/// are set up).
fn this_thread() -> Option<u64> {
    crate::sched::running_here().map(|(id, _)| id)
}

/// The probe attempt running now, the last attempt's id, the status
/// handle of the transport the running attempt brought up, and the first
/// MSI-X vector problem its transport latched (`queue_set` returns `()`).
struct Probe {
    attempt: Option<Attempt>,
    last_id: u64,
    status: Option<StatusHandle>,
    vectors: Option<VectorError>,
}

static OWNER: SpinLock<Probe> = SpinLock::new(Probe { attempt: None, last_id: 0, status: None, vectors: None });

/// Start a probe attempt of `addr`: runs handed out until [`end_probe`]
/// are recorded as its.
pub fn begin_probe(addr: PciAddr) -> Attempt {
    let mut p = OWNER.lock();
    p.last_id += 1;
    let attempt = Attempt { addr, id: p.last_id, thread: this_thread(), watermark: crate::sched::next_thread_id() };
    p.attempt = Some(attempt);
    p.status = None;
    p.vectors = None;
    attempt
}

/// What the probe attempt's transport left for `probe_all` to check.
#[derive(Clone, Copy, Debug)]
pub struct BroughtUp {
    status: StatusHandle,
    vectors: Option<VectorError>,
}

impl BroughtUp {
    /// No MSI-X vector problem was latched, and the device is ready
    /// ([`check_device`]'s checks).
    pub fn check(&self) -> Result<(), ProbeError> {
        both_checks(self.vectors, &self.status)
    }

    /// The MSI-X vector problem the transport latched, if any (kept in a
    /// failed probe's reason when the probe failed some other way).
    pub fn vector_error(&self) -> Option<VectorError> {
        self.vectors
    }
}

fn both_checks(vectors: Option<VectorError>, status: &StatusHandle) -> Result<(), ProbeError> {
    match vectors {
        Some(e) => Err(ProbeError::Vector(e)),
        None => status.check_ready(),
    }
}

/// End the running probe attempt: what the transport it brought up left
/// to check, if it brought one up.
pub fn end_probe() -> Option<BroughtUp> {
    let mut p = OWNER.lock();
    p.attempt = None;
    let vectors = p.vectors.take();
    p.status.take().map(|status| BroughtUp { status, vectors })
}

/// From a probe, after the crate's device constructor (or the driver's own
/// queue setup): no MSI-X vector problem was latched (for configuration
/// changes or any queue set up so far: else that problem), and the device
/// is ready ([`StatusHandle::check_ready`]). A driver checks this before it
/// waits for an interrupt, publishes the device or starts a thread;
/// `probe_all` checks it again when the probe returns.
pub fn check_device(status: &StatusHandle) -> Result<(), ProbeError> {
    let p = OWNER.lock();
    debug_assert!(p.attempt.is_some(), "check_device outside a probe: no vector problem could be latched");
    let vectors = p.vectors;
    drop(p);
    both_checks(vectors, status)
}

/// Why a device has no working MSI-X vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorError {
    /// `DeviceResources::msix` refused.
    Msi(MsiError),
    /// The device read back `NO_VECTOR` (virtio 1.2 §4.1.5.1.2.1).
    Refused(VectorFor),
    /// The device read back neither the entry written nor `NO_VECTOR`.
    Mismatch { what: VectorFor, wrote: u16, read: u16 },
}

/// What a vector was being mapped for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorFor {
    /// Configuration changes (`msix_config`).
    Config,
    /// That queue (`queue_msix_vector`).
    Queue(u16),
}

impl fmt::Display for VectorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VectorError::Msi(e) => write!(f, "MSI-X: {e}"),
            VectorError::Refused(VectorFor::Queue(q)) => {
                write!(f, "MSI-X: the device refused its vector for queue {q} (NO_VECTOR read back)")
            }
            VectorError::Refused(VectorFor::Config) => {
                f.write_str("MSI-X: the device refused its vector for configuration changes (NO_VECTOR read back)")
            }
            VectorError::Mismatch { what, wrote, read } => {
                write!(f, "MSI-X: the device read back entry {read:#06x} for ")?;
                match what {
                    VectorFor::Config => f.write_str("configuration changes")?,
                    VectorFor::Queue(q) => write!(f, "queue {q}")?,
                }
                write!(f, " after entry {wrote} was written")
            }
        }
    }
}

/// Record the transport's vector problem for the probe attempt of `addr`
/// that is running, if one is (the first problem wins), with a warning.
fn latch(addr: PciAddr, e: VectorError) {
    let mut p = OWNER.lock();
    if p.attempt.is_some_and(|a| a.addr == addr) {
        if p.vectors.is_none() {
            p.vectors = Some(e);
        }
        drop(p);
        warn!(target: "virtio", "{addr}: {e}");
    } else {
        drop(p);
        // Only a probe sets queues up in M3: nothing will check this one.
        error!(target: "virtio", "{addr}: {e} (outside its probe: the device runs without its interrupt)");
    }
}

/// How many runs `attempt` left outstanding.
pub fn outstanding_runs_of(attempt: Attempt) -> usize {
    TABLE.lock().iter().flatten().filter(|e| e.owner == Some(attempt)).count()
}

/// How many runs any probe of `addr` left outstanding.
pub fn outstanding_runs_of_function(addr: PciAddr) -> usize {
    TABLE.lock().iter().flatten().filter(|e| e.owner.is_some_and(|a| a.addr == addr)).count()
}

/// How many DMA runs are outstanding (handed to the crate, not yet freed).
pub fn outstanding_runs() -> usize {
    count(&TABLE.lock())
}

fn count(table: &Runs) -> usize {
    table.iter().flatten().count()
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
        // SAFETY: a live `OwnedRun` owns its frames (from `alloc`). A device
        // it was shared with has finished with it: only `dma_dealloc` and
        // `unshare` take a recorded run out of the table, and the crate
        // calls them once the device is done with the buffer or reset (the
        // drop-order rule in the module doc).
        unsafe { frame::free_contiguous(self.first, self.count) }.unwrap_or_else(|e| {
            panic!("virtio: freeing the DMA run at {:#x} ({} frames): {e:?}", self.phys(), self.count)
        });
    }
}

/// Record `run` (the caller checked the table isn't full): its physical
/// address.
fn record(table: &mut Runs, run: OwnedRun, kind: Kind) -> u64 {
    let entry = table.iter_mut().find(|e| e.is_none()).expect("the table isn't full");
    let owner = OWNER.lock().attempt.filter(Attempt::charges_this_thread);
    entry.insert(Entry { run, kind, owner }).run.phys()
}

/// Why [`take`] found no run.
#[derive(Debug)]
enum Miss {
    /// Nothing outstanding starts at that address.
    NotOutstanding,
    /// A run starts there, but of another kind or length.
    Differs(Kind),
}

/// Take out the run at `paddr` handed out as `kind`.
fn take(table: &mut Runs, paddr: u64, kind: Kind) -> Result<OwnedRun, Miss> {
    let entry =
        table.iter_mut().find(|e| e.as_ref().is_some_and(|e| e.run.phys() == paddr)).ok_or(Miss::NotOutstanding)?;
    let recorded = entry.as_ref().expect("found").kind;
    if recorded != kind {
        return Err(Miss::Differs(recorded));
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
        let outstanding = count(&table);
        // (`OwnedRun::alloc` refuses 0 pages.)
        let run = if outstanding < RUNS { OwnedRun::alloc(pages) } else { None };
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
        (record(&mut table, run, Kind::Dma { pages }), ptr)
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
        let taken = take(&mut table, paddr, Kind::Dma { pages });
        let freed = taken.map(|run| run.free());
        drop(table);
        match freed {
            Ok(Ok(())) => 0,
            Ok(Err(e)) => {
                error!(target: "virtio", "dma_dealloc({paddr:#x}, {pages} pages): the frame allocator refused: {e:?}");
                -2
            }
            Err(Miss::NotOutstanding) => refused(format_args!("no run is outstanding there")),
            Err(Miss::Differs(kind)) => refused(format_args!("the run there is {kind:?}")),
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
        let outstanding = count(&table);
        if outstanding == RUNS {
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
        let len = buffer.len();
        let mut table = TABLE.lock();
        let run = take(&mut table, paddr, Kind::Bounce { len }).unwrap_or_else(|miss| {
            let outstanding = count(&table);
            panic!("virtio: unshare of {paddr:#x} ({len} bytes): {miss:?} ({outstanding} runs outstanding)")
        });
        if direction == BufferDirection::DeviceToDriver {
            // SAFETY: the crate passes the buffer it shared, valid for writes
            // of `len` bytes; the device has finished with the run.
            unsafe { core::ptr::copy_nonoverlapping(run.ptr(), buffer.as_ptr().cast::<u8>(), len) };
        }
        let frames = run.count;
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
    Device(virtio_drivers::Error),
    /// The device status after setup isn't `FEATURES_OK` and `DRIVER_OK`
    /// (or shows `DEVICE_NEEDS_RESET` or `FAILED`): the raw byte.
    NotReady { status: u8 },
    /// A bounded wait gave up.
    Wait(WaitError),
    /// Not this driver's device after all (one its row's `matches` can't
    /// rule out: a second tablet, a virtio input device of the tablet's
    /// class without its axes), or one the driver doesn't take (rng and
    /// sound: a second device): `probe_all` frees the function and offers
    /// it to the next driver row.
    NotMine,
    /// The device can't be used by this driver: its configuration or
    /// behaviour.
    Unusable(String),
    /// The driver's owner thread couldn't be started.
    Thread(crate::sched::SpawnError),
    /// The device has no working MSI-X vector.
    Vector(VectorError),
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProbeError::Capability(p) => write!(f, "{p}"),
            ProbeError::Transport(e) => write!(f, "virtio transport: {e}"),
            ProbeError::Device(e) => write!(f, "virtio: {e}"),
            ProbeError::NotReady { status } => {
                write!(f, "the device status reads {status:#04x} after setup: {}", not_ready_why(*status))
            }
            ProbeError::Wait(e) => write!(f, "{e}"),
            ProbeError::NotMine => f.write_str("not this driver's device"),
            ProbeError::Unusable(why) => write!(f, "unusable device: {why}"),
            ProbeError::Thread(e) => write!(f, "no thread for the driver: {e}"),
            ProbeError::Vector(e) => write!(f, "{e}"),
        }
    }
}

/// Why a status byte after setup isn't ready, for messages.
pub fn not_ready_why(status: u8) -> &'static str {
    let s = bits(status);
    if status == 0xff {
        "all ones, the device is gone or not decoding"
    } else if s.contains(DeviceStatus::FAILED) {
        "FAILED set"
    } else if s.contains(DeviceStatus::DEVICE_NEEDS_RESET) {
        "DEVICE_NEEDS_RESET set: the device needs a reset"
    } else if !s.contains(DeviceStatus::FEATURES_OK) {
        "FEATURES_OK clear: the device refused the negotiated features"
    } else if !s.contains(DeviceStatus::DRIVER_OK) {
        "DRIVER_OK clear"
    } else {
        "ready"
    }
}

/// What an all-ones status byte means, for messages.
fn gone(status: u8) -> &'static str {
    if status == 0xff { ": all ones, the device is gone or not decoding" } else { "" }
}

/// Bring up the virtio transport of a claimed function: the one entry
/// point, called from the driver's probe (the running probe attempt is
/// charged its runs, and given its status handle for `probe_all`'s
/// readiness check). It takes the resources by value, so it runs once per
/// claim (a function is claimed again only after its driver declines it),
/// and the transport owns them. First it checks
/// what `PciTransport::new` will look up (`lumen_pci::virtio::check`: the
/// structures `used` keeps inside mapped memory BARs and long enough, every
/// capability the crate reads inside the header); then it runs it, under
/// the probe lock, with the function's configuration space and, for
/// `mmio_phys_to_virt`, its BAR mappings; then it checks each queue's
/// notify register is inside the notify structure.
///
/// The device interrupts through MSI-X, running `handler`: the transport
/// asks for the function's vector (`DeviceResources::msix`, so a vector
/// can't belong to another device) at the first `queue_set`, inside the
/// crate's device constructor, not here: a driver may still decline the
/// device before that, and a vector allocated now would be stranded.
/// `handler` runs in the interrupt (keep it short: ring a [`Doorbell`]).
pub fn transport(res: DeviceResources, handler: fn()) -> Result<VirtioTransport, ProbeError> {
    let info = res.info();
    let addr = info.addr;
    let found: Vec<VirtioCap> = {
        let mut config = res.config();
        info.caps.iter().filter(|c| c.id == cap::id::VENDOR).map(|c| VirtioCap::read(&mut config, addr, c)).collect()
    };
    let bars: [Option<BarMapping>; 6] = core::array::from_fn(|i| res.bar(i));
    // The MSI-X table and pending bit array, which no virtio structure may
    // overlap (the kernel programs them itself). A capability that can't
    // be read is `msix`'s to report.
    let msix = info
        .caps
        .find(cap::id::MSIX)
        .and_then(|c| lumen_pci::msix::Capability::read(&mut res.config(), addr, c).ok())
        .map(|m| m.regions());
    let used = caps::check(&found, |i| bars[usize::from(i)].map(|m| m.size()), msix.as_ref().map_or(&[], |r| &r[..]))
        .map_err(ProbeError::Capability)?;
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
    let mut t = VirtioTransport { inner: ManuallyDrop::new(inner), common, res, vectors: Vectors::Wanted(handler) };
    let notify = used.notify.expect("PciTransport::new found the notify structure");
    let multiplier = t.res.config().read(addr, u16::from(notify.at + caps::NOTIFY_MULTIPLIER_AT));
    for queue in 0..t.common_read16(common::NUM_QUEUES) {
        t.common_write16(common::QUEUE_SELECT, queue);
        let off = t.common_read16(common::QUEUE_NOTIFY_OFF);
        // On failure the transport drops here, resetting the device.
        caps::notify_fits(notify, multiplier, queue, off).map_err(ProbeError::Capability)?;
    }
    // For `probe_all`, which checks the device is ready once the probe
    // returns (a probe brings up at most one transport).
    let mut p = OWNER.lock();
    if p.attempt.is_some_and(|a| a.addr == addr) {
        p.status = Some(t.status());
    }
    drop(p);
    Ok(t)
}

/// A virtio PCI device's transport: `virtio-drivers`' `PciTransport`, which
/// it delegates to, plus what the crate leaves out: a [`StatusHandle`], and
/// the device's MSI-X vector, written to the checked common configuration
/// (`msix_config` and each `queue_msix_vector`, which the crate never
/// writes) in `queue_set`.
/// Owns the device's resources. Dropping it resets the device: it writes 0
/// and waits up to [`RESET_WAIT_NS`] for the status to read 0, then drops
/// the `PciTransport` (whose own `Drop` writes 0 and spins until it reads
/// 0, now at once). A device that doesn't reset is logged as an error, its
/// bus mastering turned off, and its `PciTransport` leaked rather than
/// dropped, so boot doesn't hang in that spin.
///
/// One initialization per transport: the vector is mapped in the first
/// `queue_set` after `begin_init`'s reset, and a second `begin_init` (a
/// reset, which unmaps `msix_config`) would leave configuration changes
/// without it. No driver re-initializes a device in M3.
#[derive(Debug)]
pub struct VirtioTransport {
    inner: ManuallyDrop<PciTransport>,
    common: NonNull<u8>,
    res: DeviceResources,
    vectors: Vectors,
}

/// Where a transport's MSI-X vector is.
#[derive(Debug)]
enum Vectors {
    /// Asked for, with this handler: allocated at the first `queue_set`.
    Wanted(fn()),
    /// Allocated, enabled for configuration changes, unmasked: each queue
    /// set up from now on is given it.
    Mapped(MsiVector),
    /// It couldn't be had, or the device refused it (latched for the
    /// probe, and kept here, in the transport's `Debug`, for when no probe
    /// was running): no more tries.
    Failed(#[allow(dead_code)] VectorError),
}

/// Ktests: make this function's transport map an entry 8 past its own for
/// this (configuration changes, or that queue), so the device refuses it:
/// QEMU reads back `NO_VECTOR` for an entry past its table. `None`: every
/// transport maps its own entry.
#[cfg(feature = "ktest")]
pub static KTEST_REFUSE: SpinLock<Option<(PciAddr, VectorFor)>> = SpinLock::new(None);

/// virtio's `VIRTIO_F_EVENT_IDX` (1.2 §6), the crate's `RING_EVENT_IDX`.
const RING_EVENT_IDX: u64 = 1 << 29;

/// The entry to write for `what` on `addr`'s device: the vector's own
/// (ktests may push it past the table, [`KTEST_REFUSE`]).
fn entry_for(addr: PciAddr, v: &MsiVector, what: VectorFor) -> u16 {
    #[cfg(feature = "ktest")]
    if *KTEST_REFUSE.lock() == Some((addr, what)) {
        return v.table_entry() + 8;
    }
    let _ = (addr, what);
    v.table_entry()
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
        StatusHandle { reg, addr: Some(self.res.info().addr) }
    }

    pub fn info(&self) -> &'static DeviceInfo {
        self.res.info()
    }

    fn common_read16(&self, offset: u64) -> u16 {
        // SAFETY: a 16-bit field of the checked common configuration, mapped
        // uncached; aligned, because `PciTransport::new` refused a common
        // configuration not 8-byte aligned (its `Misaligned` check) and the
        // offset is even. A volatile read (the aliasing exception in the
        // module doc).
        unsafe { self.common.add(offset as usize).cast::<u16>().read_volatile() }
    }

    fn common_write16(&mut self, offset: u64, value: u16) {
        // SAFETY: as in `common_read16`; through `&mut self`, so the crate's
        // transport isn't in the middle of an access.
        unsafe { self.common.add(offset as usize).cast::<u16>().write_volatile(value) }
    }

    /// Write `entry` to the 16-bit vector field at `offset` and read it
    /// back (virtio 1.2 §4.1.5.1.2.2): the device must read back exactly
    /// the entry written (`NO_VECTOR`: refused; anything else: a device
    /// that won't deliver what was asked).
    fn map_vector(&mut self, offset: u64, entry: u16, what: VectorFor) -> Result<(), VectorError> {
        self.common_write16(offset, entry);
        match self.common_read16(offset) {
            read if read == entry => Ok(()),
            common::NO_VECTOR => Err(VectorError::Refused(what)),
            read => Err(VectorError::Mismatch { what, wrote: entry, read }),
        }
    }

    /// Before the crate sets queue `queue` up (so before its
    /// `queue_enable`, virtio 1.2 §4.1.4.3.2): the first time, get the
    /// function's MSI-X vector and map configuration changes to it, then
    /// unmask it; each time, map the queue to it. A failure is latched for
    /// the probe (and warned about), and the transport stops trying.
    fn give_vector(&mut self, queue: u16) {
        let addr = self.res.info().addr;
        if let Vectors::Wanted(handler) = self.vectors {
            // After `begin_init`'s reset (which unmaps the vectors), and
            // `msix` has enabled MSI-X by the time the device is told.
            let v = match self.res.msix(handler) {
                Ok(v) => v,
                Err(e) => return self.fail_vector(addr, VectorError::Msi(e)),
            };
            let entry = entry_for(addr, &v, VectorFor::Config);
            if let Err(e) = self.map_vector(common::MSIX_CONFIG, entry, VectorFor::Config) {
                return self.fail_vector(addr, e);
            }
            if let Err(e) = v.unmask() {
                return self.fail_vector(addr, VectorError::Msi(e));
            }
            debug!(target: "virtio", "{addr}: MSI-X vector {:#x} (entry {})", v.idt_vector(), v.table_entry());
            self.vectors = Vectors::Mapped(v);
        }
        let Vectors::Mapped(v) = &self.vectors else { return };
        let what = VectorFor::Queue(queue);
        let entry = entry_for(addr, v, what);
        self.common_write16(common::QUEUE_SELECT, queue);
        if let Err(e) = self.map_vector(common::QUEUE_MSIX_VECTOR, entry, what) {
            self.fail_vector(addr, e);
        }
    }

    /// Ktests: the device's features as the crate's transport reads them
    /// (before [`Transport::read_device_features`] hides any).
    #[cfg(feature = "ktest")]
    pub fn ktest_inner_features(&mut self) -> u64 {
        self.inner.read_device_features()
    }

    /// Ktests: what `msix_config` and queue 0's `queue_msix_vector` read
    /// now.
    #[cfg(feature = "ktest")]
    pub fn ktest_vectors(&mut self) -> (u16, u16) {
        self.common_write16(common::QUEUE_SELECT, 0);
        (self.common_read16(common::MSIX_CONFIG), self.common_read16(common::QUEUE_MSIX_VECTOR))
    }

    fn fail_vector(&mut self, addr: PciAddr, e: VectorError) {
        self.vectors = Vectors::Failed(e);
        latch(addr, e);
    }
}

/// How long [`VirtioTransport`]'s `Drop` waits for the device to reset.
const RESET_WAIT_NS: u64 = 100_000_000;

impl Drop for VirtioTransport {
    fn drop(&mut self) {
        let addr = self.res.info().addr;
        self.inner.set_status(DeviceStatus::empty());
        // Not `wait_until`: the status before the reset (FAILED, say) must
        // not end the wait.
        let status = self.status();
        let start = crate::time::now_ns();
        let deadline = start.saturating_add(RESET_WAIT_NS);
        let mut last = status.raw();
        while last != 0 && crate::time::now_ns() < deadline {
            core::hint::spin_loop();
            last = status.raw();
        }
        let took = crate::time::now_ns().saturating_sub(start);
        if last == 0 {
            debug!(target: "virtio", "{addr}: reset in {} us", took / 1000);
            // SAFETY: dropped once, here; the field isn't used afterwards.
            unsafe { ManuallyDrop::drop(&mut self.inner) };
            return;
        }
        let mut config = self.res.config();
        let command = config.read16(addr, lumen_pci::reg::COMMAND);
        config.write_command(addr, command & !lumen_pci::command::BUS_MASTER);
        error!(
            target: "virtio",
            "{addr}: the device status still reads {last:#04x} {} ms after a reset{}: bus mastering turned off, \
             its transport leaked (PciTransport::drop would wait forever)",
            took / 1_000_000,
            gone(last)
        );
    }
}

impl Transport for VirtioTransport {
    fn device_type(&self) -> DeviceType {
        self.inner.device_type()
    }

    /// The device's features without `RING_EVENT_IDX` (bit 29), so no
    /// driver negotiates it: with it, the crate stores `used_event` and then
    /// loads `used.idx` with no full fence between them, and x86 may perform
    /// that load before the store is visible: the device may read the old
    /// `used_event` and skip the interrupt, while the driver, having read no
    /// new buffer, sleeps. Without it the device interrupts for every used
    /// buffer (nothing calls the crate's `set_dev_notify(false)`, so the
    /// queues' `NO_INTERRUPT` flag stays clear).
    fn read_device_features(&mut self) -> u64 {
        self.inner.read_device_features() & !RING_EVENT_IDX
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
        // One initialization per transport: a reset after the vector was
        // mapped would unmap it (see the type's doc).
        debug_assert!(
            !(status.is_empty() && matches!(self.vectors, Vectors::Mapped(_))),
            "a second initialization of a virtio transport: its MSI-X vector would be unmapped"
        );
        self.inner.set_status(status)
    }

    fn set_guest_page_size(&mut self, guest_page_size: u32) {
        self.inner.set_guest_page_size(guest_page_size)
    }

    fn requires_legacy_layout(&self) -> bool {
        self.inner.requires_legacy_layout()
    }

    fn queue_set(&mut self, queue: u16, size: u32, descriptors: u64, driver_area: u64, device_area: u64) {
        self.give_vector(queue);
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
    /// The device's function, for warnings (`None`: a ktest's made-up one).
    addr: Option<PciAddr>,
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
            // (All ones never times out: it shows FAILED.)
            WaitError::Timeout { waited_ns, status } => {
                write!(f, "the device didn't answer in {} ms (status {status:#04x})", waited_ns / 1_000_000)
            }
            WaitError::NeedsReset { status } => write!(f, "the device needs a reset (status {status:#04x})"),
            WaitError::Failed { status } => {
                write!(f, "the device status reads FAILED ({status:#04x}{})", gone(status))
            }
        }
    }
}

impl StatusHandle {
    /// Ktests: a handle reading `byte` instead of a device's register (a
    /// made-up device whose status the test sets).
    #[cfg(feature = "ktest")]
    pub fn ktest_reading(byte: &'static core::sync::atomic::AtomicU8, addr: Option<PciAddr>) -> StatusHandle {
        StatusHandle { reg: NonNull::from(byte).cast(), addr }
    }

    /// The raw status byte.
    pub fn raw(&self) -> u8 {
        // SAFETY: the register is in a BAR mapped uncached for the kernel's
        // life (checked by `transport`); a byte read, volatile, aliasing the
        // crate's unique pointer as the type's doc explains.
        unsafe { self.reg.read_volatile() }
    }

    pub fn get(&self) -> DeviceStatus {
        bits(self.raw())
    }

    /// After the crate's `new()`: the device accepted the features and is
    /// running (`FEATURES_OK` and `DRIVER_OK`, neither `DEVICE_NEEDS_RESET`
    /// nor `FAILED`). The crate writes `FEATURES_OK` without reading it
    /// back, so every driver calls this before it reports the device bound.
    pub fn check_ready(&self) -> Result<(), ProbeError> {
        let status = self.raw();
        let s = bits(status);
        let ready = s.contains(DeviceStatus::FEATURES_OK | DeviceStatus::DRIVER_OK)
            && !s.intersects(DeviceStatus::DEVICE_NEEDS_RESET | DeviceStatus::FAILED);
        if ready { Ok(()) } else { Err(ProbeError::NotReady { status }) }
    }

    /// Wait until `ready()`, at most `timeout_ns` nanoseconds: checked first,
    /// after each look at the status, and once more before giving up (the
    /// thread may have been preempted past the deadline). Gives up early if
    /// the status shows `DEVICE_NEEDS_RESET` or `FAILED`. Busy-waits: for a
    /// made-up device the ktests drive without an interrupt.
    #[cfg(feature = "ktest")]
    pub fn wait_until(&self, timeout_ns: u64, ready: impl FnMut() -> bool) -> Result<(), WaitError> {
        self.wait(timeout_ns, None, ready)
    }

    /// Wait until `ready()`, at most `timeout_ns` nanoseconds, for a device
    /// whose MSI-X handler rings `bell`: `ready()` is checked first, after
    /// each ring, and once more at the deadline (the thread may have been
    /// preempted past it); between looks the thread blocks until the bell
    /// rings (a used buffer on any of the device's queues, or a
    /// configuration change) or the deadline passes, so it costs no CPU.
    /// Gives up early if the status shows `DEVICE_NEEDS_RESET` or `FAILED`
    /// (looked at on each wake: a device announces `DEVICE_NEEDS_RESET` with
    /// a configuration-change interrupt, `FAILED` is a driver's or all ones
    /// and comes with none, so it is seen at the next wake or the
    /// deadline). Where the thread can't block (a lock held, interrupts
    /// off, before the scheduler) it spins instead. A wait that blocked to
    /// its deadline without a ring, after which `ready()` held with the
    /// ring count still unchanged, means the device answered without its
    /// interrupt ([`MissedRule`]): counted, and warned about once
    /// ([`Doorbell::note_missed`]).
    pub fn wait_for(&self, bell: &Doorbell, timeout_ns: u64, ready: impl FnMut() -> bool) -> Result<(), WaitError> {
        self.wait(timeout_ns, Some(bell), ready)
    }

    fn wait(&self, timeout_ns: u64, bell: Option<&Doorbell>, mut ready: impl FnMut() -> bool) -> Result<(), WaitError> {
        let start = crate::time::now_ns();
        let deadline = start.saturating_add(timeout_ns);
        let mut rule = MissedRule::default();
        let answered = |rule: &mut MissedRule, seen: Option<u64>| {
            if let (Some(bell), Some(seen)) = (bell, seen)
                && rule.answered(seen)
            {
                bell.note_missed(self.addr);
            }
            Ok(())
        };
        loop {
            // Read before looking, so a ring after the look ends the wait.
            let seen = bell.map(Doorbell::rings);
            if ready() {
                return answered(&mut rule, seen);
            }
            let status = self.raw();
            let s = bits(status);
            if s.contains(DeviceStatus::FAILED) {
                return Err(WaitError::Failed { status });
            }
            if s.contains(DeviceStatus::DEVICE_NEEDS_RESET) {
                return Err(WaitError::NeedsReset { status });
            }
            let now = crate::time::now_ns();
            if now >= deadline {
                let seen = bell.map(Doorbell::rings);
                return if ready() {
                    answered(&mut rule, seen)
                } else {
                    Err(WaitError::Timeout { waited_ns: now - start, status })
                };
            }
            match (bell, seen) {
                (Some(bell), Some(seen)) => match bell.wait_after(seen, deadline) {
                    Wake::CannotBlock => core::hint::spin_loop(),
                    wake => rule.blocked(seen, wake),
                },
                _ => core::hint::spin_loop(),
            }
        }
    }
}

/// What a driver's MSI-X handler rings, and its thread waits for: a count
/// of rings and a [`WaitQueue`]. Read [`rings`](Self::rings) before looking
/// at the device, then [`wait_after`](Self::wait_after) that count: a ring
/// in between ends the wait at once, so none is lost. The handler rings it
/// for anything the device signals (a used buffer on any queue, or a
/// configuration change: one vector per device). A driver may also ring
/// its own bell to wake its thread (`mouse::hold` does, so the thread
/// parks); the missed-interrupt rule ([`MissedRule`]) counts such a ring as
/// an interrupt that came.
pub struct Doorbell {
    rings: AtomicU64,
    queue: WaitQueue,
    /// Waits that ran to their deadline with no ring though the device had
    /// answered ([`note_missed`](Self::note_missed)).
    missed: AtomicU64,
}

/// The missed-interrupt rule every wait for a [`Doorbell`] uses: the
/// device answered (a buffer came back) without its interrupt if the last
/// block ended at its deadline with no ring, and the ring count, read just
/// before the look that found the answer, is still the one that block
/// waited after. A ring in between (an MSI taken just after the block's
/// last check, or just past the deadline) is an interrupt that came.
#[derive(Debug, Default)]
pub struct MissedRule {
    /// The ring count the last block waited after, if it ended at its
    /// deadline with no ring.
    silent: Option<u64>,
}

impl MissedRule {
    /// A block that waited after ring count `seen` ended with `wake`.
    pub fn blocked(&mut self, seen: u64, wake: Wake) {
        self.silent = (wake == Wake::Deadline).then_some(seen);
    }

    /// The device turned out to have answered, at a look made after
    /// reading ring count `seen`: whether the answer came without its
    /// interrupt. Starts over (one answer counts once).
    pub fn answered(&mut self, seen: u64) -> bool {
        self.silent.take() == Some(seen)
    }
}

/// How [`Doorbell::wait_after`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// It rang.
    Rang,
    /// The deadline passed without a ring.
    Deadline,
    /// The thread can't block here: it returned at once (the caller spins).
    CannotBlock,
}

impl Doorbell {
    pub const fn new() -> Doorbell {
        Doorbell { rings: AtomicU64::new(0), queue: WaitQueue::new(), missed: AtomicU64::new(0) }
    }

    /// From the interrupt handler: count the ring and wake every waiter.
    pub fn ring(&self) {
        self.rings.fetch_add(1, Ordering::AcqRel);
        self.queue.wake_all();
    }

    /// How many times it has rung.
    pub fn rings(&self) -> u64 {
        self.rings.load(Ordering::Acquire)
    }

    /// Block until it has rung more than `seen` times, at most until
    /// `deadline` (`time::now_ns`). Returns at once where the thread can't
    /// block.
    #[must_use = "a deadline or no block at all means nothing rang"]
    pub fn wait_after(&self, seen: u64, deadline: u64) -> Wake {
        if !crate::sched::can_block_now() {
            return Wake::CannotBlock;
        }
        if self.queue.wait_until_or_deadline(deadline, || self.rings() != seen) { Wake::Rang } else { Wake::Deadline }
    }

    /// The device at `addr` answered though a wait for it ran to its
    /// deadline with no ring ([`MissedRule`]): its interrupt never came.
    /// Counted; the first time, a warning naming the device.
    pub fn note_missed(&self, addr: Option<PciAddr>) {
        if self.missed.fetch_add(1, Ordering::Relaxed) != 0 {
            return;
        }
        match addr {
            Some(addr) => warn!(
                target: "virtio",
                "{addr}: the device answered but its MSI-X interrupt never came (said once)"
            ),
            None => warn!(target: "virtio", "a device answered but its MSI-X interrupt never came (said once)"),
        }
    }

    /// How many waits ran to their deadline though the device had answered.
    #[cfg(feature = "ktest")]
    pub fn missed(&self) -> u64 {
        self.missed.load(Ordering::Relaxed)
    }
}

impl Default for Doorbell {
    fn default() -> Doorbell {
        Doorbell::new()
    }
}

/// A status byte as flags (bits `DeviceStatus` doesn't name dropped).
fn bits(status: u8) -> DeviceStatus {
    DeviceStatus::from_bits_truncate(u32::from(status))
}

/// The ktest machine's virtio devices of `kind`, in address order.
#[cfg(feature = "ktest")]
pub fn ktest_devices(kind: u16) -> Vec<&'static DeviceInfo> {
    crate::pci::devices().iter().filter(|d| d.virtio_type() == Some(kind)).collect()
}

crate::ktest!(
    fn a_doorbell_wait_where_the_thread_cant_block_spins_to_its_deadline() {
        // Under a `SpinLock`, or with interrupts off, `wait_after` returns at
        // once (it can't block) and `wait_for` spins: a timeout, not a panic.
        use core::sync::atomic::AtomicU8;

        use crate::{kassert, kassert_eq};
        static BYTE: AtomicU8 = AtomicU8::new(0x01);
        static BELL: Doorbell = Doorbell::new();
        let status = StatusHandle::ktest_reading(&BYTE, None);
        let lock = SpinLock::new(());
        let (wake, waited) = {
            let _held = lock.lock();
            let now = crate::time::now_ns();
            (BELL.wait_after(BELL.rings(), now + 1_000_000), status.wait_for(&BELL, 2_000_000, || false))
        };
        kassert_eq!(wake, Wake::CannotBlock);
        kassert!(matches!(waited, Err(WaitError::Timeout { status: 0x01, .. })));
        let were_on = crate::arch::irq::disable();
        let waited = status.wait_for(&BELL, 2_000_000, || false);
        crate::arch::irq::restore(were_on);
        kassert!(matches!(waited, Err(WaitError::Timeout { status: 0x01, .. })));
        kassert_eq!(BELL.missed(), 0);
        Ok(())
    }
);

/// The ktest machine's first virtio device of `kind` (virtio 1.2 §5 device
/// type: 1 network, 2 block, 3 console, 5 balloon, 8 SCSI, 9 9p, 18 input:
/// keyboards and a mouse; no driver takes those in M3).
#[cfg(feature = "ktest")]
pub fn ktest_device(kind: u16) -> Result<&'static DeviceInfo, &'static str> {
    crate::pci::devices()
        .iter()
        .find(|d| d.virtio_type() == Some(kind))
        .ok_or("the ktest machine lacks a virtio device (tests/scenarios/test_ktest.py attaches them)")
}

#[cfg(feature = "ktest")]
fn hhdm(paddr: u64) -> *mut u8 {
    crate::mm::phys_to_virt(PhysAddr::new(paddr).expect("physical")).as_mut_ptr::<u8>()
}

/// Whether the `frames` frames from `paddr` are all allocated (`true`) or
/// all free (`false`).
#[cfg(feature = "ktest")]
fn all_allocated(paddr: u64, frames: u64, allocated: bool) -> bool {
    (0..frames)
        .all(|i| PhysFrame::from_u64(paddr + i * FRAME_SIZE).is_some_and(|f| frame::is_allocated(f) == allocated))
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
        kassert!(all_allocated(paddr, 3, true));
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
        kassert!(all_allocated(paddr, 3, true));
        // SAFETY: the run this test allocated, no longer used.
        kassert_eq!(unsafe { VirtioHal::dma_dealloc(paddr, vaddr, 3) }, 0);
        kassert!(all_allocated(paddr, 3, false));
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
                take(&mut table, dma, Kind::Bounce { len: 100 }),
                Err(Miss::Differs(Kind::Dma { pages: 1 }))
            ));
            kassert!(matches!(
                take(&mut table, bounce, Kind::Bounce { len: 99 }),
                Err(Miss::Differs(Kind::Bounce { len: 100 }))
            ));
            kassert!(matches!(take(&mut table, 0x1000, Kind::Dma { pages: 1 }), Err(Miss::NotOutstanding)));
        }
        // SAFETY: the buffer and address `share` returned.
        unsafe { VirtioHal::unshare(bounce, shared, BufferDirection::DriverToDevice) };
        // SAFETY: the run this test allocated.
        kassert_eq!(unsafe { VirtioHal::dma_dealloc(dma, dma_v, 1) }, 0);
        kassert_eq!(outstanding_runs(), before);
        // A run handed out during a probe is that device's.
        // A run handed out during a probe attempt is that attempt's (and its
        // function's), not the next attempt's.
        let owner = lumen_pci::PciAddr::new(0, 0x80, 31, 7).ok_or("addr")?; // no function there
        let first = begin_probe(owner);
        let (p, v) = VirtioHal::dma_alloc(1, BufferDirection::DriverToDevice);
        kassert!(end_probe().is_none());
        let second = begin_probe(owner);
        end_probe();
        kassert_eq!((outstanding_runs_of(first), outstanding_runs_of(second)), (1, 0));
        kassert_eq!(outstanding_runs_of_function(owner), 1);
        // SAFETY: the run this test allocated.
        kassert_eq!(unsafe { VirtioHal::dma_dealloc(p, v, 1) }, 0);
        kassert_eq!(outstanding_runs_of(first), 0);
        Ok(())
    }
);

crate::ktest!(
    fn runs_are_charged_to_a_probe_only_from_its_own_threads() {
        // While a function is probed: a run from the probing thread, or from
        // a thread made during the attempt (one a probe spawns), is the
        // attempt's; a run from a thread made before it (a bound driver's,
        // like the tablet's re-posting an event buffer) isn't, so the probe
        // running meanwhile isn't blamed for it.
        use crate::kassert_eq;
        use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
        static GO: AtomicBool = AtomicBool::new(false);
        static OLD_RUN: AtomicU64 = AtomicU64::new(0);
        static NEW_RUN: AtomicU64 = AtomicU64::new(0);
        let alloc_into = |slot: &'static AtomicU64| {
            let (p, _) = VirtioHal::dma_alloc(1, BufferDirection::DriverToDevice);
            slot.store(p, Ordering::Relaxed);
        };
        GO.store(false, Ordering::Relaxed);
        // Made before the attempt; hands out its run during it.
        let old = crate::sched::spawn("ktest-old", move || {
            while !GO.load(Ordering::Acquire) {
                crate::sched::yield_now();
            }
            alloc_into(&OLD_RUN);
        })
        .map_err(|_| "no thread")?;
        let owner = lumen_pci::PciAddr::new(0, 0x80, 31, 7).ok_or("addr")?; // no function there
        let attempt = begin_probe(owner);
        // Everything that can fail runs before `end_probe`, which runs on
        // every path.
        let during = (|| {
            let new = crate::sched::spawn("ktest-new", move || alloc_into(&NEW_RUN)).map_err(|_| "no thread")?;
            new.join();
            GO.store(true, Ordering::Release);
            old.join();
            Ok::<_, &'static str>(VirtioHal::dma_alloc(1, BufferDirection::DriverToDevice))
        })();
        end_probe();
        let (mine, mine_v) = during?;
        kassert_eq!(outstanding_runs_of(attempt), 2);
        for p in [OLD_RUN.load(Ordering::Relaxed), NEW_RUN.load(Ordering::Relaxed)] {
            let v = NonNull::new(hhdm(p)).ok_or("null")?;
            // SAFETY: the runs this test's threads allocated, each freed once.
            kassert_eq!(unsafe { VirtioHal::dma_dealloc(p, v, 1) }, 0);
        }
        // SAFETY: as above.
        kassert_eq!(unsafe { VirtioHal::dma_dealloc(mine, mine_v, 1) }, 0);
        kassert_eq!(outstanding_runs_of(attempt), 0);
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
        kassert!(all_allocated(paddr, 1, true));
        kassert_eq!(outstanding_runs(), before + 1);
        // SAFETY: the bounce run, 1 frame, ours until `unshare`.
        let copy = unsafe { core::slice::from_raw_parts_mut(hhdm(paddr), 600) };
        kassert!(copy.iter().enumerate().all(|(i, &b)| b == i as u8 ^ 0x5a));
        copy.fill(0xee);
        // SAFETY: the buffer and address `share` returned.
        unsafe { VirtioHal::unshare(paddr, shared, BufferDirection::DriverToDevice) };
        kassert!(buf.iter().enumerate().all(|(i, &b)| b == i as u8 ^ 0x5a));
        kassert!(all_allocated(paddr, 1, false));
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
        kassert!(all_allocated(paddr, 1, false));
        kassert_eq!(outstanding_runs(), before);

        // Exactly a page takes one frame, a page and a byte two, two pages
        // and a byte three; an empty buffer one.
        kassert_eq!((frames_for(0), frames_for(4096), frames_for(4097)), (1, 1, 2));
        let big = vec![1u8; 2 * FRAME_SIZE as usize + 1];
        let shared = NonNull::from(&big[..]);
        // SAFETY: `big` outlives the share and isn't written meanwhile.
        let paddr = unsafe { VirtioHal::share(shared, BufferDirection::DriverToDevice) };
        kassert!(all_allocated(paddr, 3, true));
        // SAFETY: as above.
        unsafe { VirtioHal::unshare(paddr, shared, BufferDirection::DriverToDevice) };
        let empty: &[u8] = &[];
        let shared = NonNull::from(empty);
        // SAFETY: an empty buffer (a dangling but aligned, non-null pointer
        // of length 0): nothing is read or written through it.
        let paddr = unsafe { VirtioHal::share(shared, BufferDirection::DeviceToDriver) };
        kassert!(all_allocated(paddr, 1, true));
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
        let nowhere = lumen_pci::PciAddr::new(0, 0x80, 31, 7).ok_or("addr")?;
        kassert_eq!(pci::claim(nowhere, "ktest").map(|_| ()), Err(ClaimError::NoSuchFunction));
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
            (Binding::Free { declined_by: None }, "no driver"),
            (Binding::Free { declined_by: Some("kbd") }, "no driver (kbd declined it)"),
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
        kassert_eq!(crate::shell::sysinfo::driver_text(vga.addr), alloc::format!("ktest: not probed: {why}"));
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
        let mut t = transport(res, || {}).map_err(|e| {
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
        t.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::FEATURES_OK);
        kassert!(matches!(status.check_ready(), Err(ProbeError::NotReady { status: 0x0b })));
        t.set_status(ok | DeviceStatus::FAILED);
        kassert!(matches!(status.check_ready(), Err(ProbeError::NotReady { status: 0x8f })));
        // The message names what is missing.
        let why = |status| alloc::format!("{}", ProbeError::NotReady { status });
        kassert_eq!(
            why(0xff),
            "the device status reads 0xff after setup: all ones, the device is gone or not decoding"
        );
        kassert_eq!(why(0x8f), "the device status reads 0x8f after setup: FAILED set");
        kassert_eq!(
            why(0x4f),
            "the device status reads 0x4f after setup: DEVICE_NEEDS_RESET set: the device needs a reset"
        );
        kassert_eq!(
            why(0x07),
            "the device status reads 0x07 after setup: FEATURES_OK clear: the device refused the negotiated features"
        );
        kassert_eq!(why(0x0b), "the device status reads 0x0b after setup: DRIVER_OK clear");
        // A bounded wait gives up on time (at least the timeout: no bound on
        // how much more, which a host stalling the vCPU decides), re-polling
        // `ready`; or when the status shows
        // DEVICE_NEEDS_RESET (which the device sets) or FAILED (a driver).
        t.set_status(DeviceStatus::ACKNOWLEDGE);
        let t0 = crate::time::now_ns();
        let Err(WaitError::Timeout { waited_ns, status: 0x01 }) = status.wait_until(2_000_000, || false) else {
            return Err("no timeout");
        };
        let took = crate::time::now_ns() - t0;
        kassert!(waited_ns >= 2_000_000 && took >= 2_000_000);
        let mut polls = 0;
        kassert_eq!(
            status.wait_until(1_000_000_000, || {
                polls += 1;
                polls >= 3
            }),
            Ok(())
        );
        kassert_eq!(polls, 3);
        // `wait_for` gives up the same way, blocking between looks until its
        // bell rings: a 20 ms wait nobody rings looks a few times, not
        // thousands (and isn't a missed interrupt: the device never
        // answered); a ring from a thread on another CPU ends a wait blocked
        // on it; a wait whose device answers only at the deadline, with no
        // ring, counts a missed interrupt.
        static BELL: Doorbell = Doorbell::new();
        let (t0, mut looks, wakes) = (crate::time::now_ns(), 0, crate::sched::deadline_wakes());
        let Err(WaitError::Timeout { waited_ns, status: 0x01 }) = status.wait_for(&BELL, 20_000_000, || {
            looks += 1;
            false
        }) else {
            return Err("no timeout blocking");
        };
        let took = crate::time::now_ns() - t0;
        kassert!(waited_ns >= 20_000_000 && took >= 20_000_000);
        // Looks come at the start, after each wake and once more at the
        // deadline (normally 3), not with time passing: a stall can only skip
        // the block, never add a look.
        kassert!((2..=4).contains(&looks));
        // The timer ended its block (and so counts the ones below).
        kassert_eq!((BELL.missed(), crate::sched::deadline_wakes()), (0, wakes + 1));
        let rung = alloc::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
        let r = rung.clone();
        let other = if crate::arch::cpu_index() == 0 { 1 } else { 0 };
        let ringer = crate::sched::spawn_on(other, "ringer", move || {
            let _ = crate::sched::sleep(2_000_000);
            r.store(true, Ordering::Release);
            BELL.ring();
        })
        .map_err(|_| "no ringer thread")?;
        // The ring ended it: the timer ended no wait at its 5 s deadline
        // (`sched::deadline_wakes`; a lost wake would still return Ok,
        // looking again there).
        let wakes = crate::sched::deadline_wakes();
        kassert_eq!(status.wait_for(&BELL, 5_000_000_000, || rung.load(Ordering::Acquire)), Ok(()));
        ringer.join();
        kassert_eq!((BELL.missed(), crate::sched::deadline_wakes()), (0, wakes));
        let t1 = crate::time::now_ns();
        kassert_eq!(status.wait_for(&BELL, 5_000_000, || crate::time::now_ns() >= t1 + 5_000_000), Ok(()));
        kassert_eq!(BELL.missed(), 1);
        // A ring during the first look (rung by `ready` itself): the wait
        // read the ring count before looking, so the timer ends no wait at
        // its deadline, and `ready` is asked twice, with no miss.
        static EARLY: Doorbell = Doorbell::new();
        let (mut calls, wakes) = (0, crate::sched::deadline_wakes());
        let early = status.wait_for(&EARLY, 5_000_000_000, || {
            calls += 1;
            if calls == 1 {
                EARLY.ring();
            }
            calls > 1
        });
        kassert_eq!(early, Ok(()));
        kassert_eq!((calls, EARLY.missed(), crate::sched::deadline_wakes()), (2, 0, wakes));
        kassert_eq!(status.wait_until(1_000_000, || true), Ok(()));
        // `ready` is asked once more at the deadline.
        let t1 = crate::time::now_ns();
        kassert_eq!(status.wait_until(2_000_000, || crate::time::now_ns() >= t1 + 2_000_000), Ok(()));
        t.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DEVICE_NEEDS_RESET);
        kassert_eq!(status.wait_until(10_000_000_000, || false), Err(WaitError::NeedsReset { status: 0x41 }));
        t.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::FAILED);
        kassert_eq!(status.wait_until(10_000_000_000, || false), Err(WaitError::Failed { status: 0x81 }));
        // A reset; nothing was handed out for DMA.
        t.set_status(DeviceStatus::empty());
        kassert_eq!(t.get_status(), DeviceStatus::empty());
        kassert_eq!(outstanding_runs(), runs);
        pci::probed(d.addr, "ktest", Probed::Bound);
        // Dropping it resets a running device: writes 0, waits for the status
        // to read 0, then `PciTransport`'s `Drop` (test_ktest.py checks no
        // reset failure is logged).
        t.set_status(DeviceStatus::ACKNOWLEDGE);
        drop(t);
        kassert_eq!(status.raw(), 0);
        Ok(())
    }
);

crate::ktest!(
    fn a_missed_interrupt_is_an_answer_after_a_silent_deadline_with_no_ring() {
        // The rule (`MissedRule`): a block that ended at its deadline with no
        // ring, then an answer with the ring count unchanged, is a miss; a
        // ring in between, or a block a ring ended, isn't; an answer counts
        // once. A miss warns once, naming the device (read back from the
        // log), and counts every time.
        use crate::{kassert, kassert_eq};
        let mut rule = MissedRule::default();
        rule.blocked(5, Wake::Deadline);
        kassert!(!rule.answered(6)); // a ring after the silent deadline
        rule.blocked(5, Wake::Deadline);
        kassert!(rule.answered(5)); // an answer after a silent deadline, no ring
        kassert!(!rule.answered(5)); // counted once
        rule.blocked(5, Wake::Rang);
        kassert!(!rule.answered(5)); // the block was rung
        kassert!(!MissedRule::default().answered(0)); // never blocked

        static BELL: Doorbell = Doorbell::new();
        let addr = PciAddr::new(0, 0x80, 31, 6).ok_or("addr")?;
        BELL.note_missed(Some(addr));
        BELL.note_missed(Some(addr));
        kassert_eq!(BELL.missed(), 2);
        let mut log = alloc::vec![0u8; crate::klog::RING_BYTES];
        let (n, _) = crate::klog::copy_recent(&mut log);
        let log = core::str::from_utf8(&log[..n]).map_err(|_| "the log isn't UTF-8")?;
        let line = alloc::format!(
            "WARN  virtio: {addr}: the device answered but its MSI-X interrupt never came (said once)\n"
        );
        kassert_eq!(log.matches(line.as_str()).count(), 1);
        Ok(())
    }
);
