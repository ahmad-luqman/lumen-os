//! `prepare`'s answers when the heap refuses (a test binary of its own,
//! with one test, so nothing else allocates meanwhile): a global allocator
//! that refuses every allocation at or above a threshold while armed. The
//! app logs these as the memory plan failing (`Why::out_of_memory`), not as
//! a bad file, so which `Why` comes back is pinned here:
//!
//! - the fitted copy refused: `Why::NoMemory` (`tiles.png` at 400 KiB: its
//!   fitted copy is 440 × 293 pixels, 515 KB; everything before it smaller);
//! - the decoder's samples refused: `Why::Decode(DecodeError::OutOfMemory)`
//!   (the wallpaper at 1 MiB: `lumen-png` reserves its 3 MB of samples, fallibly,
//!   before the decoder inflates anything).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use lumen_view::{DecodeError, Why, prepare};

/// Allocations of at least this many bytes are refused (0: none are).
static REFUSE_FROM: AtomicUsize = AtomicUsize::new(0);

struct Refusing;

// SAFETY: every allocation it doesn't refuse is the system's; a refusal is
// a null pointer, which `GlobalAlloc` allows.
unsafe impl GlobalAlloc for Refusing {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let from = REFUSE_FROM.load(Ordering::SeqCst);
        if from != 0 && layout.size() >= from {
            return std::ptr::null_mut();
        }
        // SAFETY: the caller's contract, passed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static A: Refusing = Refusing;

const WALLPAPER: &[u8] = include_bytes!("../../../assets/wallpaper/lumen.png");
const TILES: &[u8] = include_bytes!("../../../apps/viewer/images/tiles.png");

/// `prepare(png)`'s error with allocations from `from` bytes refused.
fn refused_from(from: usize, png: &[u8]) -> Option<Why> {
    REFUSE_FROM.store(from, Ordering::SeqCst);
    let got = prepare(png).err();
    REFUSE_FROM.store(0, Ordering::SeqCst);
    got
}

#[test]
fn a_refusing_heap_is_out_of_memory_never_a_bad_file() {
    // Unrefused, both are shown.
    assert!(prepare(TILES).is_ok() && prepare(WALLPAPER).is_ok());
    let fit = refused_from(400 * 1024, TILES);
    assert_eq!(fit, Some(Why::NoMemory));
    let samples = refused_from(1024 * 1024, WALLPAPER);
    assert_eq!(samples, Some(Why::Decode(DecodeError::OutOfMemory)));
    assert!(fit.is_some_and(|w| w.out_of_memory()) && samples.is_some_and(|w| w.out_of_memory()));
}
