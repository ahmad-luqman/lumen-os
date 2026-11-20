//! The viewer's memory plan (`lumen_view::HEAP_BYTES`), checked by running
//! the app's allocations through the app's own allocator logic
//! (`lumen-heap`, as the SDK's global allocator does) over a region of
//! exactly that size, in a test binary of its own with one test, so nothing
//! else allocates meanwhile:
//!
//! - the canvas the SDK reserves for its window, and a block standing for
//!   the toolkit's own (its widgets, its strings), held throughout;
//! - then, image after image as the app shows them (the old one and its
//!   fitted copy dropped first): the PNG decoded by `lumen-png` (its copy of
//!   the image's chunks, the decoder's buffers, the samples, the pixels) and
//!   its fitted copy;
//! - every bundled image, the corrupt one, and the costliest PNGs the
//!   viewer takes to decode (`lumen_view::admit`; made by
//!   tools/viewer-images.py, both interlaced): `worst.png`, its biggest size
//!   in 8-bit RGB, and `worst16.png`, 16-bit RGBA at the most pixels that
//!   allows; each several times, so the free list's fragmentation after a
//!   round of them is part of what must fit.
//!
//! An allocation the region can't hold fails the test (it is served from
//! the system meanwhile, so the run finishes and says how much more it
//! needed).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::UnsafeCell;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lumen_heap::Heap;
use lumen_view::{HEAP_BYTES, MAX_IMAGE_H, MAX_IMAGE_W, VIEW, WINDOW_H, WINDOW_W, fitted, scale};

/// Standing for the toolkit's allocations (the widgets, their strings, the
/// tree published), held throughout: generous (a few KiB in practice).
const TOOLKIT_BYTES: usize = 256 * 1024;

#[repr(C, align(4096))]
struct Region(UnsafeCell<[u8; HEAP_BYTES]>);

// SAFETY: the region is only reached through `Sim`'s heap, under its lock.
unsafe impl Sync for Region {}

static REGION: Region = Region(UnsafeCell::new([0; HEAP_BYTES]));

/// The global allocator: while armed, every allocation comes from a
/// `lumen_heap::Heap` over `REGION`; frees of its blocks go back to it
/// whenever they come; everything else is the system's.
struct Sim {
    heap: UnsafeCell<Option<Heap>>,
    lock: AtomicBool,
}

// SAFETY: the heap is only reached under `lock`.
unsafe impl Sync for Sim {}

static ARMED: AtomicBool = AtomicBool::new(false);
/// Bytes the region's blocks take now, and at most (`lumen_heap`'s block
/// sizes).
static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
/// The first allocation the region couldn't hold (its size), 0 if none.
static REFUSED: AtomicUsize = AtomicUsize::new(0);

impl Sim {
    fn with<R>(&self, f: impl FnOnce(&mut Heap) -> R) -> R {
        while self.lock.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            std::hint::spin_loop();
        }
        // SAFETY: under the lock, nothing else holds the heap.
        let slot = unsafe { &mut *self.heap.get() };
        let heap = slot.get_or_insert_with(|| {
            let mut heap = Heap::empty();
            let base = NonNull::new(REGION.0.get().cast::<u8>()).expect("a static's address");
            // SAFETY: the region is a static only this heap uses.
            unsafe { heap.init(base, HEAP_BYTES) };
            heap
        });
        let r = f(heap);
        self.lock.store(false, Ordering::Release);
        r
    }

    fn in_region(ptr: *mut u8) -> bool {
        let start = REGION.0.get() as usize;
        (start..start + HEAP_BYTES).contains(&(ptr as usize))
    }
}

// SAFETY: blocks come from `lumen_heap::Heap` (disjoint, aligned, inside the
// region) or the system, and each goes back to where it came from.
unsafe impl GlobalAlloc for Sim {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ARMED.load(Ordering::SeqCst) {
            if let Some(p) = self.with(|h| h.alloc(layout)) {
                let size = lumen_heap::block_size(layout).unwrap_or(layout.size());
                let now = NOW.fetch_add(size, Ordering::SeqCst) + size;
                PEAK.fetch_max(now, Ordering::SeqCst);
                return p.as_ptr();
            }
            let _ = REFUSED.compare_exchange(0, layout.size().max(1), Ordering::SeqCst, Ordering::SeqCst);
        }
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if Sim::in_region(ptr) {
            let p = NonNull::new(ptr).expect("in the region, so not null");
            // SAFETY: `ptr` came from this heap with `layout`.
            self.with(|h| unsafe { h.dealloc(p, layout) }).expect("a block the heap handed out");
            NOW.fetch_sub(lumen_heap::block_size(layout).unwrap_or(layout.size()), Ordering::SeqCst);
        } else {
            // SAFETY: the caller's contract, passed on.
            unsafe { System.dealloc(ptr, layout) }
        }
    }
}

#[global_allocator]
static A: Sim = Sim { heap: UnsafeCell::new(None), lock: AtomicBool::new(false) };

const WALLPAPER: &[u8] = include_bytes!("../../../assets/wallpaper/lumen.png");
const TILES: &[u8] = include_bytes!("../../../apps/viewer/images/tiles.png");
const GREY16: &[u8] = include_bytes!("../../../apps/viewer/images/grey16.png");
const BROKEN: &[u8] = include_bytes!("../../../apps/viewer/images/broken.png");
const WORST: &[u8] = include_bytes!("worst.png");
const WORST16: &[u8] = include_bytes!("worst16.png");

/// What the app holds for the image shown: its pixels and its fitted copy.
struct Shown {
    _pixels: Vec<u32>,
    _fitted: Option<Vec<u32>>,
}

/// The app's work for one image: decode it, then fit it (as `apps/viewer`
/// does; a refused image holds nothing).
fn show(png: &[u8]) -> Option<Shown> {
    lumen_view::admit(png).ok()?;
    let image = lumen_png::decode::decode(png, MAX_IMAGE_W, MAX_IMAGE_H).ok()?;
    let at = fitted(image.width(), image.height(), VIEW);
    let fitted = scale(image.pixels(), image.width(), image.height(), at.w, at.h);
    Some(Shown { _pixels: image.into_pixels(), _fitted: fitted })
}

#[test]
fn the_viewer_s_allocations_fit_its_heap() {
    // Each image decodes outside the region first (the decoder works).
    for (name, png) in
        [("wallpaper", WALLPAPER), ("tiles", TILES), ("grey16", GREY16), ("worst", WORST), ("worst16", WORST16)]
    {
        assert!(lumen_view::admit(png).is_ok(), "{name} isn't admitted");
        assert!(lumen_png::decode::decode(png, MAX_IMAGE_W, MAX_IMAGE_H).is_ok(), "{name} doesn't decode");
    }
    assert!(lumen_png::decode::decode(BROKEN, MAX_IMAGE_W, MAX_IMAGE_H).is_err(), "broken decodes");

    ARMED.store(true, Ordering::SeqCst);
    let canvas: Vec<u32> = vec![0; (WINDOW_W * WINDOW_H) as usize];
    let toolkit: Vec<u8> = vec![0; TOOLKIT_BYTES];
    let mut shown: Option<Shown> = None;
    let order = [WALLPAPER, TILES, GREY16, BROKEN, WORST, WALLPAPER, WORST16, TILES, WORST, WORST16, WALLPAPER];
    for _round in 0..3 {
        for png in order {
            // The old image goes before the new one is decoded.
            drop(shown.take());
            shown = show(png);
        }
    }
    drop((shown, canvas, toolkit));
    ARMED.store(false, Ordering::SeqCst);

    let (peak, refused) = (PEAK.load(Ordering::SeqCst), REFUSED.load(Ordering::SeqCst));
    eprintln!("peak {peak} bytes of the heap's {HEAP_BYTES} ({} KiB left)", (HEAP_BYTES - peak) / 1024);
    assert_eq!(refused, 0, "an allocation of {refused} bytes didn't fit (peak {peak} of {HEAP_BYTES})");
    assert_eq!(NOW.load(Ordering::SeqCst), 0, "everything allocated was freed");
}
