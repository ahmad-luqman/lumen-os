//! The kernel heap: the `#[global_allocator]` behind `Box`, `Vec` and the
//! rest of `alloc`.
//!
//! The algorithm (an address-ordered free list with coalescing) is
//! [`lumen_heap::Heap`], host-tested. Here it manages the heap window from
//! [`paging::HEAP_BASE`] up: when no free block fits, [`grow`] maps fresh
//! frames at the heap's end (at least [`MIN_GROWTH`] at a time) and appends
//! them. The heap never shrinks and stops at [`paging::HEAP_LIMIT`].
//!
//! When the heap runs out (no frames, or the window is full) the allocation
//! returns null. Callers that can cope use `try_reserve` and friends;
//! everything else ends in `alloc`'s default allocation error handler, which
//! panics ("memory allocation of N bytes failed"). Any other growth failure
//! is a kernel bug and panics. The crash report shows the heap's state,
//! including the last failed growth ([`write_crash_stats`]). A free the heap
//! refuses (outside the heap, misaligned, or overlapping free memory: a
//! double free while the block is still free, or a too-large `Layout`) is a
//! panic too.
//!
//! Lock order: the heap, then the page tables (`paging`), then the frame
//! allocator. Code holding either of the last two must not allocate. The
//! heap logs only after releasing its lock. Not for interrupt handlers yet:
//! the lock is not IRQ-safe until #7.

use core::alloc::{GlobalAlloc, Layout};
use core::fmt::{self, Write};
use core::ops::Range;
use core::ptr::{self, NonNull};

use lumen_heap::{CheckError, Heap};
use lumen_mm::paging::{Attrs, MapError};
use lumen_mm::{FRAME_SIZE, VirtAddr};

use super::{frame, paging};
use crate::sync::SpinLock;

/// The heap grows by at least this much at a time: fewer, larger trips to
/// the frame allocator and the page tables.
const MIN_GROWTH: u64 = 64 * 1024;

/// How many free blocks the crash report walks before giving up, so it
/// finishes quickly even with a huge, fragmented free list.
const CRASH_WALK_LIMIT: usize = 4096;

struct State {
    heap: Heap,
    /// The most recent growth, if it failed: why, and for what request.
    last_failure: Option<(GrowError, Layout)>,
}

static STATE: SpinLock<State> = SpinLock::new(State { heap: Heap::empty(), last_failure: None });

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
    /// Is this the heap running out (the allocation returns null), rather
    /// than a kernel bug (a panic)? Only a page table that couldn't get a
    /// frame is an expected mapping failure.
    fn is_exhaustion(self) -> bool {
        !matches!(self, GrowError::Map(e) if e != paging::VmError::Map(MapError::OutOfMemory))
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
/// by `mm::init` once the kernel's page tables are active; allocations
/// before that return null.
pub fn init() {
    let base = NonNull::new(paging::HEAP_BASE as *mut u8).expect("the heap base is not null");
    // SAFETY: by convention only the heap maps `[HEAP_BASE, HEAP_LIMIT)`
    // (the paging ktests' SCRATCH page is at HEAP_LIMIT, just outside). It
    // starts empty; `grow` maps memory before handing it over.
    unsafe { STATE.lock().heap.init(base, 0) };
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
        let grown = grow(&mut state, layout);
        let p = if grown.is_ok() { state.heap.alloc(layout) } else { None };
        drop(state); // log and panic without the lock (the crash report shows the heap)
        let (size, align) = (layout.size(), layout.align());
        match grown {
            // `growth_for` guarantees the fit (host-tested): failing is a bug.
            Ok(end) => {
                log::debug!(target: "heap", "grew to {end:#x}");
                p.unwrap_or_else(|| panic!("heap: grew for {size} bytes (align {align}) but they still don't fit"))
                    .as_ptr()
            }
            Err(e) if e.is_exhaustion() => {
                log::debug!(target: "heap", "growth for {size} bytes (align {align}) failed: {e}");
                ptr::null_mut()
            }
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
        // The lock is released, so the crash report can show the heap.
        if let Err(e) = result {
            panic!("heap: bad free of {ptr:p} ({} bytes, align {}): {e}", layout.size(), layout.align());
        }
    }
}

/// Map enough frames at the heap's end for `layout` to fit and add them to
/// the heap; returns the new end. Records the outcome in `last_failure`.
fn grow(state: &mut State, layout: Layout) -> Result<u64, GrowError> {
    let result = try_grow(&mut state.heap, layout);
    state.last_failure = result.err().map(|e| (e, layout));
    result
}

/// [`grow`] without the bookkeeping. All or nothing: on failure every page
/// mapped so far is unmapped and its frame freed (page tables created on the
/// way stay; tables are never freed).
fn try_grow(heap: &mut Heap, layout: Layout) -> Result<u64, GrowError> {
    let end = heap.end().ok_or(GrowError::NotReady)? as u64;
    // `None` only for sizes near `isize::MAX`: far past the window anyway.
    let need = lumen_heap::growth_for(layout).ok_or(GrowError::WindowFull)? as u64;
    let bytes = need.max(MIN_GROWTH).checked_next_multiple_of(FRAME_SIZE).ok_or(GrowError::WindowFull)?;
    if bytes > paging::HEAP_LIMIT - end {
        return Err(GrowError::WindowFull);
    }
    for i in 0..bytes / FRAME_SIZE {
        if let Err(e) = map_one(end + i * FRAME_SIZE) {
            release(end, i, e);
            return Err(e);
        }
    }
    // SAFETY: `[end, end + bytes)` is now mapped read-write to frames
    // allocated for the heap, and nothing else uses it.
    unsafe { heap.extend(bytes as usize) };
    Ok(end + bytes)
}

/// Map a fresh frame at `virt`, just above the heap's end.
fn map_one(virt: u64) -> Result<(), GrowError> {
    assert!((paging::HEAP_BASE..paging::HEAP_LIMIT).contains(&virt), "heap page {virt:#x} outside the heap");
    let virt = VirtAddr::new(virt).expect("heap addresses are canonical");
    let frame = frame::alloc().ok_or(GrowError::OutOfFrames)?;
    // SAFETY: the frame was just allocated for the heap, and pages above the
    // heap's end (below HEAP_LIMIT) are the heap's and unused.
    let mapped = unsafe { paging::map(virt, frame, Attrs::DATA) };
    mapped.map_err(|e| {
        frame::free(frame).unwrap_or_else(|fe| panic!("heap: freeing {frame} after `{e}` failed: {fe:?}"));
        GrowError::Map(e)
    })
}

/// Undo the first `pages` pages a [`grow`] that failed with `cause` mapped
/// above `end`.
fn release(end: u64, pages: u64, cause: GrowError) {
    for i in 0..pages {
        let virt = VirtAddr::new(end + i * FRAME_SIZE).expect("heap addresses are canonical");
        // SAFETY: `grow` mapped this page a moment ago and nothing refers to
        // it: the heap never got it.
        let m = unsafe { paging::unmap(virt) }
            .unwrap_or_else(|e| panic!("heap: rollback after `{cause}` failed to unmap {virt}: {e}"));
        frame::free(m.phys)
            .unwrap_or_else(|e| panic!("heap: rollback after `{cause}` failed to free {}: {e:?}", m.phys));
    }
}

/// The heap's counters (O(1)).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests; X-ray (M5)
pub fn stats() -> lumen_heap::Stats {
    STATE.lock().heap.stats()
}

/// Walk the free list and check its invariants.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
pub fn check() -> Result<lumen_heap::FreeList, CheckError> {
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
    STATE.lock().last_failure.map(|(e, _)| e)
}

/// One line for humans (kmon `heap`, crash reports; parsed by `HEAP_LINE`
/// in tests/scenarios/test_mm.py): sizes, the free list (walking at most
/// `max_blocks` blocks) and the last failed growth.
fn write_state(w: &mut dyn Write, label: &str, state: &State, max_blocks: usize) -> fmt::Result {
    let s = state.heap.stats();
    write!(
        w,
        "{label}: {} bytes mapped at {:#x}, {} used in {} allocations, {} free",
        s.size,
        paging::HEAP_BASE,
        s.used,
        s.live,
        s.free()
    )?;
    match state.heap.check_bounded(max_blocks) {
        Ok(list) => write!(w, " in {} blocks (largest {})", list.blocks, list.largest)?,
        Err(CheckError::TooManyBlocks) => write!(w, " in more than {max_blocks} blocks (walk stopped)")?,
        Err(e) => write!(w, " (free list broken: {e})")?,
    }
    if let Some((e, layout)) = state.last_failure {
        write!(w, "; last growth failed for {} bytes (align {}): {e}", layout.size(), layout.align())?;
    }
    writeln!(w)
}

/// The heap line for kmon's `heap` command.
#[cfg_attr(feature = "ktest", allow(dead_code))] // kmon, the only caller, never runs in the test kernel
pub fn write_stats(w: &mut dyn Write) -> fmt::Result {
    write_state(w, "heap", &STATE.lock(), usize::MAX)
}

/// The heap line for a crash report. Never blocks: if the heap's lock is
/// held (the crash happened while the heap was in use), it reads the state
/// anyway and says so. Reading is safe even mid-update: the counters are
/// plain numbers and the bounded walk never reads outside the heap.
///
/// # Safety
/// Nothing else may use the heap while this runs: call it from the crash
/// reporter (this CPU runs alone and halts next), or while holding the lock
/// without using it.
pub unsafe fn write_crash_stats(w: &mut dyn Write) -> fmt::Result {
    match STATE.try_lock() {
        Some(state) => write_state(w, "heap", &state, CRASH_WALK_LIMIT),
        None => {
            // SAFETY: the caller guarantees nothing uses the heap meanwhile;
            // we only read.
            let state = unsafe { STATE.force_get() };
            write_state(w, "heap (lock held; may be mid-update)", state, CRASH_WALK_LIMIT)
        }
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
        let blocks = check().map_err(|_| "free list broken before the test")?.blocks;
        let mut filler: Vec<(*mut u8, Layout)> = Vec::with_capacity(2 * blocks + 16);
        loop {
            let list = check().map_err(|_| "free list broken while filling")?;
            if list.largest == 0 {
                break;
            }
            kassert!(filler.len() < filler.capacity());
            let layout = Layout::from_size_align(list.largest, 16).map_err(|_| "layout")?;
            // SAFETY: non-zero size.
            let p = unsafe { alloc(layout) };
            kassert!(!p.is_null());
            filler.push((p, layout));
        }
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
        let list = check().map_err(|_| "free list broken after the test")?;
        kassert_eq!(list, lumen_heap::FreeList { blocks: 1, bytes: grown, largest: grown });
        let big = Layout::from_size_align(grown, 16).map_err(|_| "layout")?;
        // SAFETY: non-zero size.
        let p = unsafe { alloc(big) };
        kassert!(!p.is_null());
        kassert_eq!(stats().size, after.size);
        // SAFETY: allocated just above with this layout.
        unsafe { dealloc(p, big) };
        for (p, layout) in filler {
            // SAFETY: allocated above with this layout.
            unsafe { dealloc(p, layout) };
        }
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
        let tables = || paging::for_each_active_table(|_| {}).map_err(|_| "unreadable table");
        let (free0, tables0, size0) = (frame::free_frames(), tables()?, stats().size);
        // `black_box`: LLVM may assume an allocation it can see through
        // succeeds, and skip it.
        let mut owned = Vec::<u8>::new();
        let v = core::hint::black_box(&mut owned);
        kassert!(v.try_reserve_exact((free0 + 1024) * FRAME_SIZE as usize).is_err());
        let no_table_frame = GrowError::Map(paging::VmError::Map(MapError::OutOfMemory));
        kassert!(matches!(last_error(), Some(e) if e == GrowError::OutOfFrames || e == no_table_frame));
        let (free1, tables1) = (frame::free_frames(), tables()?);
        kassert_eq!(stats().size, size0);
        kassert_eq!(free1 + (tables1 - tables0), free0);
        let end = mapped().end;
        let stray = (end..end + (free0 as u64 + 1) * FRAME_SIZE)
            .step_by(FRAME_SIZE as usize)
            .find(|&page| paging::translate(VirtAddr::new(page).expect("canonical")).is_some());
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
