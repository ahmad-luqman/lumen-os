//! The viewer's memory plan (`lumen_view::HEAP_BYTES`), checked by running
//! the app's allocations through the app's own allocator logic
//! (`lumen-heap`, as the SDK's global allocator does) over a region of
//! exactly that size, in a test binary of its own with one test, so nothing
//! else allocates meanwhile:
//!
//! - the canvas the SDK reserves for its window, and a block standing for
//!   the toolkit's own (its widgets, its strings), held throughout;
//! - then, image after image as the app shows them (the old one and its
//!   fitted copy dropped first): `lumen_view::prepare`, the app's own call
//!   (`lumen-png`'s copy of the image's chunks, the decoder's buffers, the
//!   samples, the pixels, then the fitted copy);
//! - every bundled image (the corrupt one and the one `admit` refuses
//!   included), and the costliest PNGs the
//!   viewer takes to decode (`lumen_view::admit`; made by
//!   tools/viewer-images.py, both interlaced): `worst.png`, its biggest size
//!   in 8-bit RGB, and `worst16.png`, 16-bit RGBA at the most pixels that
//!   allows; and a file exactly `MAX_FILE_BYTES` long whose image data is
//!   incompressible (built here: the most the file's copies can take); and
//!   the costliest header and the longest file at once, `worst16.png`'s
//!   stream padded to `MAX_FILE_BYTES` with empty stored deflate blocks
//!   (built here: `admit` bounds each alone); each several times.
//!
//! It passes if nothing is refused in three rounds of them, so the free
//! list's fragmentation after a round is part of what must fit, not only
//! the peak (12.7 MiB of the 14.5 MiB heap, 1.8 MiB to spare, when last
//! measured: 1.4 MiB of canvas and toolkit, and 11.3 MiB while the padded
//! `worst16.png` decodes). An
//! allocation the region can't hold fails the test (it is served from the
//! system meanwhile, so the run finishes and says how much more it needed).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::UnsafeCell;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lumen_heap::Heap;
use lumen_view::{HEAP_BYTES, Prepared, WINDOW_H, WINDOW_W, prepare};

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
const COSTLY: &[u8] = include_bytes!("../../../apps/viewer/images/costly.png");
const WORST: &[u8] = include_bytes!("worst.png");
const WORST16: &[u8] = include_bytes!("worst16.png");

/// `png`'s chunks after the signature: (type, data).
fn chunks(png: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut at = 8;
    while at + 8 <= png.len() {
        let len = u32::from_be_bytes(png[at..at + 4].try_into().expect("4 bytes")) as usize;
        out.push((png[at + 4..at + 8].try_into().expect("4 bytes"), &png[at + 8..at + 8 + len]));
        at += 12 + len;
    }
    out
}

/// One PNG chunk: its length, type, data and CRC.
fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = lumen_png::crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// `png` (one IDAT) padded to exactly `lumen_view::MAX_FILE_BYTES` inside
/// its image data: empty stored deflate blocks (5 bytes each, inflating to
/// nothing) between the zlib header and the stream, so `admit` takes the
/// costliest header and the longest file at once and the decoder gathers
/// and walks every byte (an ancillary chunk takes the last 0 to 4 bytes).
fn padded(png: &[u8]) -> Vec<u8> {
    let all = chunks(png);
    let ihdr = all.iter().find(|(k, _)| k == b"IHDR").expect("an IHDR").1;
    let idat = all.iter().find(|(k, _)| k == b"IDAT").expect("an IDAT").1;
    // The fixed parts: the signature, IHDR, IDAT's framing and zlib header
    // and the stream after it, IEND.
    let fixed = 8 + (12 + ihdr.len()) + 12 + idat.len() + 12;
    let room = lumen_view::MAX_FILE_BYTES - fixed;
    let (blocks, pad) =
        if room.is_multiple_of(5) { (room / 5, None) } else { ((room - 12) / 5, Some((room - 12) % 5)) };
    let mut data = idat[..2].to_vec();
    for _ in 0..blocks {
        // BFINAL 0, BTYPE 00 (stored), padded to the byte; LEN 0, NLEN !0.
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0xff, 0xff]);
    }
    data.extend_from_slice(&idat[2..]);
    let mut out = png[..8].to_vec();
    chunk(&mut out, b"IHDR", ihdr);
    chunk(&mut out, b"IDAT", &data);
    if let Some(k) = pad {
        chunk(&mut out, b"zzPd", &vec![0; k]);
    }
    chunk(&mut out, b"IEND", &[]);
    assert_eq!(out.len(), lumen_view::MAX_FILE_BYTES);
    out
}

/// A PNG exactly `lumen_view::MAX_FILE_BYTES` long whose image data is
/// incompressible (zlib stored blocks of pseudo-random rows): the most the
/// file's copies can take. 8-bit RGB 1280 pixels wide, as many rows as the
/// length allows, padded to the cap with an ancillary chunk.
fn longest() -> Vec<u8> {
    const W: usize = 1280;
    const ROW: usize = 1 + 3 * W;
    // The chunks around the image data: the signature, IHDR, IDAT's and
    // the pad's framing, IEND; zlib's header and Adler-32; a stored
    // block's 5 bytes a 65,535.
    let budget = lumen_view::MAX_FILE_BYTES - (8 + 25 + 12 + 12 + 12 + 2 + 4);
    let mut rows = budget / ROW;
    while rows * ROW + 5 * (rows * ROW).div_ceil(65_535) > budget {
        rows -= 1;
    }
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut noise = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed as u8
    };
    let mut raw = Vec::with_capacity(rows * ROW);
    for _ in 0..rows {
        // Each row: its filter byte (none), then its samples.
        raw.extend(std::iter::once(0).chain((0..3 * W).map(|_| noise())));
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    let mut zlib = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, block) in blocks.iter().enumerate() {
        zlib.push(u8::from(i + 1 == blocks.len()));
        zlib.extend_from_slice(&(block.len() as u16).to_le_bytes());
        zlib.extend_from_slice(&(!(block.len() as u16)).to_le_bytes());
        zlib.extend_from_slice(block);
    }
    zlib.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut png = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(W as u32).to_be_bytes());
    ihdr.extend_from_slice(&(rows as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &zlib);
    let pad = lumen_view::MAX_FILE_BYTES - png.len() - 12 - 12;
    chunk(&mut png, b"zzPd", &vec![0; pad]);
    chunk(&mut png, b"IEND", &[]);
    assert_eq!(png.len(), lumen_view::MAX_FILE_BYTES);
    png
}

#[test]
fn the_viewer_s_allocations_fit_its_heap() {
    // Each image is shown outside the region first (admitted and decoded).
    for (name, png) in
        [("wallpaper", WALLPAPER), ("tiles", TILES), ("grey16", GREY16), ("worst", WORST), ("worst16", WORST16)]
    {
        assert!(prepare(png).is_ok(), "{name} isn't shown");
    }
    assert!(prepare(BROKEN).is_err(), "broken is shown");
    assert!(prepare(COSTLY).is_err(), "costly is shown");
    let longest = longest();
    assert!(prepare(&longest).is_ok(), "the longest file isn't shown");
    let costliest = padded(WORST16);
    assert!(prepare(&costliest).is_ok(), "worst16.png padded to the longest file isn't shown");
    let mut too_long = longest.clone();
    too_long.push(0);
    assert!(prepare(&too_long).is_err(), "a file past the cap is shown");

    ARMED.store(true, Ordering::SeqCst);
    let canvas: Vec<u32> = vec![0; (WINDOW_W * WINDOW_H) as usize];
    let toolkit: Vec<u8> = vec![0; TOOLKIT_BYTES];
    let mut shown: Option<Prepared> = None;
    let order: [&[u8]; 15] = [
        WALLPAPER, TILES, GREY16, BROKEN, WORST, WALLPAPER, WORST16, TILES, &longest, COSTLY, &costliest, WORST,
        WORST16, &costliest, WALLPAPER,
    ];
    for _round in 0..3 {
        for png in order {
            // The old image goes before the new one is decoded.
            drop(shown.take());
            shown = prepare(png).ok();
        }
    }
    drop((shown, canvas, toolkit));
    ARMED.store(false, Ordering::SeqCst);

    let (peak, refused) = (PEAK.load(Ordering::SeqCst), REFUSED.load(Ordering::SeqCst));
    let mib = |bytes: usize| bytes as f64 / (1024.0 * 1024.0);
    eprintln!(
        "peak {:.1} MiB of the heap's {:.1} MiB ({:.1} MiB to spare at the peak)",
        mib(peak),
        mib(HEAP_BYTES),
        mib(HEAP_BYTES - peak)
    );
    assert_eq!(
        refused, 0,
        "an allocation of {refused} bytes found no room (the peak so far {peak} of {HEAP_BYTES} bytes)"
    );
    assert_eq!(NOW.load(Ordering::SeqCst), 0, "everything allocated was freed");
}
