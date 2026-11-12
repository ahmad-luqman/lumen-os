//! The kernel heap: the `#[global_allocator]` behind `Box`, `Vec` and the
//! rest of `alloc`.
//!
//! The algorithm (an address-ordered free list with coalescing) is
//! [`lumen_heap::Heap`], host-tested. Here it manages the heap window from
//! [`paging::HEAP_BASE`] up: when no free block fits, [`grow`] maps fresh
//! frames at the heap's end and appends them. It grows by its own size,
//! between [`MIN_GROWTH`] and [`MAX_GROWTH`] (so a heap that reaches N
//! bytes took about log2(N) trips to the page tables, not N / 64 KiB), or by
//! what the request needs if that is more; when that much isn't available it
//! grows by just what the request needs. The heap never shrinks and stops at
//! [`paging::HEAP_LIMIT`].
//!
//! When the heap runs out (no frame for the heap or for a page table, or the
//! window is full) the allocation returns null. Callers that can cope use
//! `try_reserve` and friends; everything else ends in `alloc`'s default
//! allocation error handler, which panics ("memory allocation of N bytes
//! failed"). Any other growth failure is a kernel bug and panics, and so
//! does allocating before [`init`]. The crash report shows the heap's state,
//! including the last failed growth ([`write_crash_stats`]). A free the heap
//! refuses (outside the heap, misaligned, or overlapping free memory: a
//! double free while the block is still free, or a `Layout` reaching into
//! free memory) is a panic too.
//!
//! Lock order: the heap, then the page tables (`paging`), then the frame
//! allocator. Code holding either of the last two must not allocate. The
//! allocator never logs (a log argument that allocates would re-enter it
//! under the logger's lock), and formats nothing while holding its lock.
//! Panics in the allocator's own paths happen after the lock is released,
//! except inside the heap itself (a corrupt list, a failed rollback); the
//! crash line then says it read the heap under the lock. Not for interrupt
//! handlers yet: the lock is not IRQ-safe until #7.

use core::alloc::{GlobalAlloc, Layout};
use core::fmt::{self, Write};
use core::ops::Range;
use core::ptr::{self, NonNull};

use lumen_heap::{CheckError, FreeList, Heap, Stats};
use lumen_mm::FRAME_SIZE;
#[cfg(feature = "ktest")]
use lumen_mm::paging::MapError;

#[cfg(feature = "ktest")]
use super::frame;
use lumen_mm::window::Window;

use super::{paging, window};
use crate::sync::SpinLock;

/// The heap grows by its current size, at least this much when it can:
/// fewer, larger trips to the frame allocator and the page tables.
const MIN_GROWTH: u64 = 64 * 1024;

/// ... and at most this much (unless a request needs more), so a heap that
/// grew once and went quiet doesn't hold much it never uses.
const MAX_GROWTH: u64 = 4 * 1024 * 1024;

/// How many free blocks the crash report walks before giving up, so it
/// finishes quickly even with a huge, fragmented free list.
const CRASH_WALK_LIMIT: usize = 4096;

struct State {
    heap: Heap,
    /// The heap window's token (from [`init`]): what lets the heap map pages
    /// there.
    window: Option<Window>,
    /// The most recent growth, if it failed.
    last_failure: Option<GrowFailure>,
}

static STATE: SpinLock<State> = SpinLock::new(State { heap: Heap::empty(), window: None, last_failure: None });

/// A failed growth: why, and for what request.
#[derive(Clone, Copy, Debug)]
struct GrowFailure {
    error: GrowError,
    layout: Layout,
}

/// Why the heap could not grow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrowError {
    /// Before [`init`].
    NotReady,
    /// The request doesn't fit below [`paging::HEAP_LIMIT`].
    WindowFull,
    /// The frame allocator is empty.
    OutOfFrames,
    /// The page tables refused the mapping.
    Map(paging::VmError),
}

impl GrowError {
    /// Is this the heap running out, so the allocation returns null? Anything
    /// else is a kernel bug and panics. (No wildcard: a new variant must be
    /// decided here.)
    fn returns_null(self) -> bool {
        match self {
            GrowError::WindowFull | GrowError::OutOfFrames => true,
            // A page table that couldn't get a frame is running out too.
            GrowError::Map(e) => window::WindowError::Map(e).is_exhaustion(),
            // Allocating before `mm::init` is a bug.
            GrowError::NotReady => false,
        }
    }
}

impl fmt::Display for GrowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrowError::NotReady => f.write_str("heap not initialized"),
            GrowError::WindowFull => f.write_str("heap window full"),
            GrowError::OutOfFrames => f.write_str("out of physical frames"),
            GrowError::Map(e) => write!(f, "mapping failed: {e}"),
        }
    }
}

/// Hand the heap its window (empty: the first allocation grows it). Called
/// by `mm::init` once the kernel's page tables are active; allocating
/// before that panics.
pub fn init() {
    let window = paging::claim_heap_window();
    let base = NonNull::new(window.range().start as *mut u8).expect("the heap base is not null");
    let mut state = STATE.lock();
    // SAFETY: the heap holds the window's token, so nothing else maps there.
    // It starts empty; `grow` maps memory before handing it over.
    unsafe { state.heap.init(base, 0) };
    state.window = Some(window);
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator;

struct KernelAllocator;

// SAFETY: `Heap` hands out disjoint blocks of at least the requested size
// and alignment, inside memory mapped read-write for the heap alone. The
// lock serializes every access (interrupts are off until #7 makes
// `SpinLock` IRQ-safe), and panics abort, so nothing unwinds out of here.
unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut state = STATE.lock();
        if let Some(p) = state.heap.alloc(layout) {
            return p.as_ptr();
        }
        let grown = grow(&mut state, layout).map(|()| state.heap.alloc(layout));
        drop(state); // panic without the lock: the crash line reads a consistent heap
        let (size, align) = (layout.size(), layout.align());
        match grown {
            Ok(Some(p)) => p.as_ptr(),
            // `growth_for` guarantees the fit (host-tested): failing is a bug.
            Ok(None) => panic!("heap: grew for {size} bytes (align {align}) but they still don't fit"),
            Err(e) if e.returns_null() => ptr::null_mut(),
            Err(e) => panic!("heap: growth for {size} bytes (align {align}) failed: {e}"),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let result = match NonNull::new(ptr) {
            // SAFETY: the `GlobalAlloc` contract: `ptr` came from `alloc`
            // with this layout and is no longer used.
            Some(p) => unsafe { STATE.lock().heap.dealloc(p, layout) },
            None => Err(lumen_heap::FreeError::OutsideHeap),
        };
        // Panic without the lock: the crash line reads a consistent heap.
        if let Err(e) = result {
            panic!("heap: bad free of {ptr:p} ({} bytes, align {}): {e}", layout.size(), layout.align());
        }
    }
}

/// Map enough frames at the heap's end for `layout` to fit and add them to
/// the heap. Records the outcome in `last_failure` (cleared first, so a
/// panic part way doesn't leave an older failure on the crash line).
fn grow(state: &mut State, layout: Layout) -> Result<(), GrowError> {
    state.last_failure = None;
    let result = match state.window.as_mut() {
        Some(window) => try_grow(&mut state.heap, window, layout),
        None => Err(GrowError::NotReady),
    };
    state.last_failure = result.err().map(|error| GrowFailure { error, layout });
    result
}

/// How much the heap prefers to grow by when it is `size` bytes: its own
/// size, between [`MIN_GROWTH`] and [`MAX_GROWTH`] (whole pages, since the
/// heap's size always is).
fn preferred_growth(size: u64) -> u64 {
    size.clamp(MIN_GROWTH, MAX_GROWTH)
}

/// [`grow`] without the bookkeeping: [`preferred_growth`] (or what `layout`
/// needs, if more), falling back to just what it needs when the heap runs
/// out on the way.
fn try_grow(heap: &mut Heap, window: &mut Window, layout: Layout) -> Result<(), GrowError> {
    let end = heap.end().ok_or(GrowError::NotReady)? as u64;
    // `None` only for sizes near `isize::MAX`: far past the window anyway.
    let need = lumen_heap::growth_for(layout).ok_or(GrowError::WindowFull)? as u64;
    let exact = need.checked_next_multiple_of(FRAME_SIZE).ok_or(GrowError::WindowFull)?;
    let preferred = exact.max(preferred_growth(heap.stats().size as u64));
    let bytes = match map_span(window, end, preferred) {
        Err(e) if e.returns_null() && exact < preferred => map_span(window, end, exact).map(|()| exact)?,
        result => result.map(|()| preferred)?,
    };
    // SAFETY: `[end, end + bytes)` is now mapped read-write to frames
    // allocated for the heap, and nothing else uses it.
    unsafe { heap.extend(bytes as usize) };
    Ok(())
}

/// Map `bytes` (whole pages) of fresh frames at `end`, the heap's end, in
/// the heap's window. All or nothing ([`window::map_fresh`]).
fn map_span(window: &mut Window, end: u64, bytes: u64) -> Result<(), GrowError> {
    if bytes > window.range().end - end {
        return Err(GrowError::WindowFull);
    }
    window::map_fresh(window, end..end + bytes).map_err(GrowError::from)
}

impl From<window::WindowError> for GrowError {
    fn from(e: window::WindowError) -> GrowError {
        match e {
            window::WindowError::OutOfFrames => GrowError::OutOfFrames,
            window::WindowError::Map(e) => GrowError::Map(e),
        }
    }
}

/// The heap's counters (O(1)).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests; X-ray (M5)
pub fn stats() -> Stats {
    STATE.lock().heap.stats()
}

/// Walk the free list and check its invariants.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
pub fn check() -> Result<FreeList, CheckError> {
    STATE.lock().heap.check()
}

/// The heap's mapped span.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
pub fn mapped() -> Range<u64> {
    let end = STATE.lock().heap.end().map_or(paging::HEAP_BASE, |e| e as u64);
    paging::HEAP_BASE..end
}

/// Why the most recent growth failed, if it did.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
pub fn last_error() -> Option<GrowError> {
    STATE.lock().last_failure.map(|f| f.error)
}

/// What the heap line shows, copied out so it is formatted without the lock
/// (a writer that allocates would otherwise deadlock).
#[derive(Clone, Copy)]
struct Snapshot {
    stats: Stats,
    /// The free-list walk, stopped after `max_blocks` blocks.
    list: Result<FreeList, CheckError>,
    max_blocks: usize,
    last_failure: Option<GrowFailure>,
}

impl Snapshot {
    fn of(state: &State, max_blocks: usize) -> Snapshot {
        let list = state.heap.check_bounded(max_blocks);
        Snapshot { stats: state.heap.stats(), list, max_blocks, last_failure: state.last_failure }
    }

    /// One line for humans (kmon `heap`, crash reports; parsed by
    /// `HEAP_LINE` in tests/scenarios/test_mm.py): sizes, the free list and
    /// the last failed growth.
    fn write(&self, w: &mut dyn Write, label: &str) -> fmt::Result {
        let s = self.stats;
        write!(
            w,
            "{label}: {} bytes mapped at {:#x}, {} used in {} allocations, {} free",
            s.size,
            paging::HEAP_BASE,
            s.used,
            s.live,
            s.free()
        )?;
        match self.list {
            Ok(list) => write!(w, " in {} blocks (largest {})", list.blocks, list.largest)?,
            Err(CheckError::TooManyBlocks) => write!(w, " in more than {} blocks (walk stopped)", self.max_blocks)?,
            Err(e) if e.is_accounting() => write!(w, " (counters inconsistent: {e})")?,
            Err(e) => write!(w, " (free list broken: {e})")?,
        }
        if let Some(GrowFailure { error, layout }) = self.last_failure {
            write!(w, "; last growth failed for {} bytes (align {}): {error}", layout.size(), layout.align())?;
        }
        writeln!(w)
    }
}

/// The heap line for kmon's `heap` command.
#[cfg_attr(feature = "ktest", allow(dead_code))] // kmon, the only caller, never runs in the test kernel
pub fn write_stats(w: &mut dyn Write) -> fmt::Result {
    let snapshot = Snapshot::of(&STATE.lock(), usize::MAX);
    snapshot.write(w, "heap")
}

/// The heap line for a crash report. Never blocks: if the heap's lock is
/// held (the crash happened while the heap was in use), it reads the state
/// anyway and says so. Reading is safe even mid-update: the counters are
/// plain numbers, the bounded walk never reads outside the heap, and the
/// heap never unmaps what it has.
///
/// # Safety
/// Nothing may use the heap while this runs: call it from the crash reporter
/// (one CPU, interrupts off until #7, and it halts next, so the interrupted
/// lock holder never resumes), or from a test that holds the lock and
/// doesn't use the heap meanwhile.
pub unsafe fn write_crash_stats(w: &mut dyn Write) -> fmt::Result {
    let (snapshot, label) = match STATE.try_lock() {
        Some(state) => (Snapshot::of(&state, CRASH_WALK_LIMIT), "heap"),
        None => {
            // SAFETY: the caller guarantees nothing uses the heap meanwhile
            // (the interrupted holder, if any, never resumes); we only read.
            let state = unsafe { STATE.force_get() };
            (Snapshot::of(state, CRASH_WALK_LIMIT), "heap (lock held; may be mid-update)")
        }
    };
    snapshot.write(w, label)
}

/// [`check`] for the tests: a broken heap is a failure that says what broke.
#[cfg(feature = "ktest")]
fn checked() -> Result<FreeList, &'static str> {
    check().map_err(|e| {
        crate::ktest::diag(format_args!("heap check failed: {e}"));
        "heap check failed"
    })
}

/// Blocks a test allocated to use up every free block (see [`fill`]).
#[cfg(feature = "ktest")]
type Filler = alloc::vec::Vec<(*mut u8, Layout)>;

/// Allocate every free block whole, so the next allocation must grow the
/// heap, whatever earlier tests left. Free them with [`unfill`].
#[cfg(feature = "ktest")]
fn fill() -> Result<Filler, &'static str> {
    let mut filler = Filler::with_capacity(2 * checked()?.blocks + 16);
    loop {
        let largest = checked()?.largest;
        if largest == 0 {
            return Ok(filler);
        }
        if filler.len() == filler.capacity() {
            return Err("more free blocks than expected while filling");
        }
        let layout = Layout::from_size_align(largest, 16).map_err(|_| "layout")?;
        // SAFETY: non-zero size.
        let p = unsafe { alloc::alloc::alloc(layout) };
        if p.is_null() {
            return Err("filling a free block failed");
        }
        filler.push((p, layout));
    }
}

#[cfg(feature = "ktest")]
fn unfill(filler: Filler) {
    for (p, layout) in filler {
        // SAFETY: allocated by `fill` with this layout.
        unsafe { alloc::alloc::dealloc(p, layout) };
    }
}

/// Deterministic randomness for the stress test (xorshift64).
#[cfg(feature = "ktest")]
struct Rng(u64);

#[cfg(feature = "ktest")]
impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

crate::ktest!(
    fn reuse_after_free() {
        use crate::kassert_eq;
        use alloc::vec::Vec;
        let first = Vec::<u8>::with_capacity(256);
        let at = first.as_ptr();
        drop(first);
        let second = Vec::<u8>::with_capacity(256);
        kassert_eq!(second.as_ptr(), at);
        drop(second);
        // Growth can't hide a broken free: a thousand 64 KiB cycles reuse
        // the same memory, so the heap doesn't grow.
        // (`black_box`: LLVM may delete an allocation nobody looks at.)
        drop(core::hint::black_box(Vec::<u8>::with_capacity(64 * 1024)));
        let size = stats().size;
        for _ in 0..1000 {
            drop(core::hint::black_box(Vec::<u8>::with_capacity(64 * 1024)));
        }
        kassert_eq!(stats().size, size);
        Ok(())
    }
);

crate::ktest!(
    fn fragmentation_stress() {
        // First every free block is filled, so the stress runs only in
        // memory it makes the heap grow by, whatever earlier tests left.
        // Random sizes and alignments are then allocated and freed in random
        // order, each block filled and checked before it is freed. Freeing
        // the rest must coalesce everything the stress used back into one
        // block, so an allocation of all of it fits without growing.
        use crate::{kassert, kassert_eq};
        use alloc::alloc::{alloc, dealloc};
        use alloc::vec::Vec;
        let mut live: Vec<(*mut u8, Layout, u8)> = Vec::with_capacity(1024);
        let filler = fill()?;
        let before = stats();
        kassert_eq!(before.free(), 0);
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for step in 0..6000u32 {
            if live.len() < 1000 && (live.is_empty() || rng.below(100) < 55) {
                let max = if rng.below(10) == 0 { 64 * 1024 } else { 4096 };
                let size = 1 + rng.below(max) as usize;
                let layout = Layout::from_size_align(size, 1 << rng.below(13)).map_err(|_| "layout")?;
                // SAFETY: `layout` has a non-zero size.
                let p = unsafe { alloc(layout) };
                kassert!(!p.is_null());
                kassert_eq!(p as usize % layout.align(), 0);
                let tag = step as u8;
                // SAFETY: `p` is a fresh block of `size` bytes.
                unsafe { p.write_bytes(tag, size) };
                live.push((p, layout, tag));
            } else {
                let (p, layout, tag) = live.swap_remove(rng.below(live.len() as u64) as usize);
                // SAFETY: the block is ours and nothing else wrote to it.
                let bytes = unsafe { core::slice::from_raw_parts(p, layout.size()) };
                kassert!(bytes.iter().all(|&b| b == tag));
                // SAFETY: allocated above with this layout.
                unsafe { dealloc(p, layout) };
            }
        }
        for (p, layout, _) in live.drain(..) {
            // SAFETY: allocated above with this layout.
            unsafe { dealloc(p, layout) };
        }
        let after = stats();
        kassert_eq!((after.used, after.live), (before.used, before.live));
        let grown = after.size - before.size;
        kassert!(grown >= 1 << 20);
        kassert_eq!(check(), Ok(FreeList { blocks: 1, bytes: grown, largest: grown }));
        let big = Layout::from_size_align(grown, 16).map_err(|_| "layout")?;
        // SAFETY: non-zero size.
        let p = unsafe { alloc(big) };
        kassert!(!p.is_null());
        kassert_eq!(stats().size, after.size);
        // SAFETY: allocated just above with this layout.
        unsafe { dealloc(p, big) };
        unfill(filler);
        Ok(())
    }
);

crate::ktest!(
    fn crash_stats_never_block() {
        // The crash reporter may run while the heap's lock is held (a fault
        // inside the allocator): its heap line must still appear, saying so,
        // instead of deadlocking. (The buffer is on the stack: allocating
        // here would deadlock.)
        use crate::{kassert, kassert_eq};
        let mut buf = crate::ktest::Buf::new();
        let held = STATE.lock();
        // SAFETY: this test holds the lock and doesn't use the heap meanwhile.
        let written = unsafe { write_crash_stats(&mut buf) };
        drop(held);
        kassert_eq!(written, Ok(()));
        kassert!(buf.as_str().starts_with("heap (lock held; may be mid-update): "));
        kassert!(buf.as_str().contains(" bytes mapped at ") && !buf.as_str().contains("broken"));
        let mut buf = crate::ktest::Buf::new();
        // SAFETY: nothing uses the heap while this runs.
        kassert_eq!(unsafe { write_crash_stats(&mut buf) }, Ok(()));
        kassert!(buf.as_str().starts_with("heap: ") && buf.as_str().contains(" bytes mapped at "));
        Ok(())
    }
);

crate::ktest!(
    fn vec_and_box_work() {
        use crate::{kassert, kassert_eq};
        use alloc::boxed::Box;
        use alloc::collections::BTreeMap;
        use alloc::string::String;
        use alloc::vec::Vec;
        let before = stats();
        let boxed = Box::new(0x1234_5678_u64);
        kassert_eq!(*boxed, 0x1234_5678);
        kassert!(mapped().contains(&(&raw const *boxed as u64)));
        let mut v = Vec::new();
        for i in 0..10_000u64 {
            v.push(i);
        }
        kassert!(mapped().contains(&(v.as_ptr() as u64)));
        kassert_eq!(v.iter().sum::<u64>(), 10_000 * 9_999 / 2);
        let mut map = BTreeMap::new();
        for i in 0..500u32 {
            map.insert(i, alloc::format!("value {i}"));
        }
        map.retain(|k, _| k % 2 == 0);
        kassert_eq!(map.len(), 250);
        kassert_eq!(map.get(&42).map(String::as_str), Some("value 42"));
        let nested: Vec<Vec<u8>> = (0..64).map(|n| alloc::vec![n as u8; n * 17]).collect();
        kassert!(nested.iter().enumerate().all(|(n, b)| b.len() == n * 17 && b.iter().all(|&x| x == n as u8)));
        kassert!(stats().live > before.live); // the heap was really used
        drop((boxed, v, map, nested));
        let after = stats();
        kassert_eq!((after.used, after.live), (before.used, before.live));
        Ok(())
    }
);

crate::ktest!(
    fn growth_failure_rolls_back() {
        // Asking for more than all free RAM maps pages until frames run
        // out, then must undo all of it: no frame leaks (apart from page
        // tables, which are never freed), nothing stays mapped above the
        // heap, and the heap is unchanged. Asking for one page more than fits
        // below HEAP_LIMIT fails before touching anything.
        use crate::{kassert, kassert_eq};
        use alloc::vec::Vec;
        let tables = paging::active_table_count;
        let (free0, tables0, size0) = (frame::free_frames(), tables()?, stats().size);
        // `black_box`: LLVM may assume an allocation it can see through
        // succeeds, and skip it.
        let mut owned = Vec::<u8>::new();
        let v = core::hint::black_box(&mut owned);
        kassert!(v.try_reserve_exact((free0 + 1024) * FRAME_SIZE as usize).is_err());
        kassert!(matches!(
            last_error(),
            Some(GrowError::OutOfFrames | GrowError::Map(paging::VmError::Map(MapError::OutOfMemory)))
        ));
        let (free1, tables1) = (frame::free_frames(), tables()?);
        kassert!(tables1 > tables0); // it really mapped pages before running out
        kassert_eq!(stats().size, size0);
        kassert_eq!(free1 + (tables1 - tables0), free0);
        let end = mapped().end;
        let stray = (end..end + (free0 as u64 + 1) * FRAME_SIZE)
            .step_by(FRAME_SIZE as usize)
            .find(|&page| paging::translate(lumen_mm::VirtAddr::new(page).expect("canonical")).is_some());
        kassert_eq!(stray, None);
        let past_limit = (paging::HEAP_LIMIT - end + FRAME_SIZE) as usize;
        kassert!(v.try_reserve_exact(past_limit).is_err());
        kassert_eq!(last_error(), Some(GrowError::WindowFull));
        kassert_eq!(frame::free_frames(), free1);
        // The heap still works, and a successful growth (more than is free
        // forces one) clears the error.
        kassert!(v.try_reserve_exact(stats().free() + MIN_GROWTH as usize).is_ok());
        kassert_eq!(last_error(), None);
        Ok(())
    }
);

crate::ktest!(
    fn growth_errors_are_classified() {
        // Running out returns null; anything else is a kernel bug (a panic).
        use crate::kassert;
        use paging::VmError;
        kassert!(GrowError::OutOfFrames.returns_null());
        kassert!(GrowError::WindowFull.returns_null());
        kassert!(GrowError::Map(VmError::Map(MapError::OutOfMemory)).returns_null());
        kassert!(!GrowError::Map(VmError::Map(MapError::AlreadyMapped)).returns_null());
        kassert!(!GrowError::Map(VmError::NotWriteBack).returns_null());
        kassert!(!GrowError::Map(VmError::Executable).returns_null());
        let page = lumen_mm::VirtAddr::new(paging::HEAP_BASE).expect("canonical");
        kassert!(!GrowError::Map(VmError::NotOwner(page)).returns_null());
        kassert!(!GrowError::Map(VmError::OutsideWindow(page)).returns_null());
        let f = lumen_mm::PhysFrame::from_u64(0).expect("aligned");
        kassert!(!GrowError::Map(VmError::NotAllocated(f)).returns_null());
        kassert!(!GrowError::NotReady.returns_null());
        Ok(())
    }
);

crate::ktest!(
    fn growth_is_geometric() {
        // A small request that has to grow the heap grows it by its own size
        // (between MIN_GROWTH and MAX_GROWTH); a request bigger than that
        // grows it by what the request needs. (The growth policy is checked
        // as a function too: the heap's size when this runs depends on the
        // tests before it.)
        use crate::kassert_eq;
        kassert_eq!(preferred_growth(0), MIN_GROWTH);
        kassert_eq!(preferred_growth(MIN_GROWTH), MIN_GROWTH);
        kassert_eq!(preferred_growth(1 << 20), 1 << 20);
        kassert_eq!(preferred_growth(1 << 30), MAX_GROWTH);
        let filler = fill()?;
        let size = stats().size as u64;
        let small = Layout::from_size_align(64, 16).map_err(|_| "layout")?;
        // SAFETY: non-zero size.
        let p = core::hint::black_box(unsafe { alloc::alloc::alloc(small) });
        let grown_small = stats().size as u64 - size;
        // The heap is full again apart from the new block's tail; a request
        // bigger than MAX_GROWTH must grow it by (about) its own size.
        let filler2 = fill()?;
        let size2 = stats().size as u64;
        let big = Layout::from_size_align(MAX_GROWTH as usize + 1, 16).map_err(|_| "layout")?;
        // SAFETY: non-zero size.
        let q = core::hint::black_box(unsafe { alloc::alloc::alloc(big) });
        let grown_big = stats().size as u64 - size2;
        crate::kassert!(!p.is_null() && !q.is_null());
        kassert_eq!(grown_small, preferred_growth(size));
        kassert_eq!(grown_big, (MAX_GROWTH + 1).next_multiple_of(16).next_multiple_of(FRAME_SIZE));
        // SAFETY: allocated above with these layouts.
        unsafe {
            alloc::alloc::dealloc(q, big);
            alloc::alloc::dealloc(p, small);
        }
        unfill(filler2);
        unfill(filler);
        Ok(())
    }
);

crate::ktest!(
    fn small_growth_when_frames_are_scarce() {
        // With fewer free frames than the preferred growth needs, a one-page
        // request still succeeds: the heap grows by just that page.
        use crate::{kassert, kassert_eq};
        use alloc::vec::Vec;
        let mut held = Vec::with_capacity(frame::free_frames());
        let filler = fill()?;
        // 8 frames: half of MIN_GROWTH (the smallest preferred growth), but
        // enough for a page and its tables.
        while frame::free_frames() > 8 {
            held.push(frame::alloc_owned().ok_or("frames ran out")?);
        }
        let size = stats().size;
        let page = Layout::from_size_align(FRAME_SIZE as usize, 16).map_err(|_| "layout")?;
        // SAFETY: non-zero size.
        let p = core::hint::black_box(unsafe { alloc::alloc::alloc(page) });
        let grown = stats().size - size;
        drop(held); // frees them
        kassert!(!p.is_null());
        kassert_eq!(grown, FRAME_SIZE as usize);
        // SAFETY: allocated just above with this layout.
        unsafe { alloc::alloc::dealloc(p, page) };
        unfill(filler);
        Ok(())
    }
);

crate::ktest!(
    fn heap_maps_only_in_its_own_window() {
        // The heap holds the heap window's token, and that token can't map
        // a page just past the heap (the ktest page) or in the stack window.
        use crate::kassert_eq;
        use lumen_mm::VirtAddr;
        use lumen_mm::paging::Attrs;
        let v = |a| VirtAddr::new(a).expect("canonical");
        // (No allocation while STATE is held: it would deadlock. Asserting
        // only writes to serial.)
        let mut state = STATE.lock();
        let window = state.window.as_mut().ok_or("the heap holds no window")?;
        kassert_eq!(window.range(), paging::HEAP_BASE..paging::HEAP_LIMIT);
        let mut f = frame::alloc_owned().ok_or("out of frames")?;
        for page in [paging::HEAP_LIMIT, paging::STACKS_BASE] {
            let (got, back) = paging::map_refused(window, v(page), f, Attrs::DATA, "outside the heap's window")?;
            kassert_eq!(got, paging::VmError::NotOwner(v(page)));
            f = back;
        }
        Ok(())
    }
);
