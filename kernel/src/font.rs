//! The desktop's anti-aliased text (`lumen-font`, #148) in the kernel: the
//! UI font, parsed once and shared ([`face`]). The desktop doesn't draw
//! with it yet (M4's compositor will); for now it renders the golden
//! sample for the shell's `font` and its ktest, which show how long
//! `fontdue`'s soft-float parsing and rasterizing take here and that the
//! kernel's mask hashes to the host golden's value.

use crate::shell::Ctx;
use crate::time;
use alloc::boxed::Box;
use alloc::string::String;
use core::ptr;
use core::sync::atomic::{AtomicPtr, Ordering};
use lumen_font::{Face, Font, Px, Stats, sample};
use lumen_time::Millis;

/// The glyph cache's bound for the sample: far more than its glyphs take,
/// so nothing is evicted and the second render hits for every glyph.
const CACHE_BYTES: usize = 256 * 1024;

/// The widest line the sample's mask may take.
const MAX_WIDTH: u32 = 4096;

const PX: Px = match Px::new(sample::PX) {
    Some(px) => px,
    None => panic!("the sample's size is out of range"),
};

/// The parsed UI font and how long parsing it took.
pub struct Loaded {
    pub face: Face,
    pub parse_ns: u64,
}

/// Null until the first [`face`] publishes the parsed font; never freed
/// after (a `Box` leaked on purpose: the font lives as long as the kernel).
static LOADED: AtomicPtr<Loaded> = AtomicPtr::new(ptr::null_mut());

/// The UI font, parsed on first use and shared by every caller after.
/// Parsing takes about 60 ms under QEMU's TCG (soft-float), with no lock
/// held, so call it from a thread, not an interrupt handler. Two threads
/// that both find it unparsed both parse it; the first to publish wins and
/// the other's copy is dropped.
pub fn face() -> Result<&'static Loaded, lumen_font::Error> {
    let current = LOADED.load(Ordering::Acquire);
    if !current.is_null() {
        // SAFETY: a non-null pointer was published (Release) from
        // `Box::into_raw` below and is never freed or written again.
        return Ok(unsafe { &*current });
    }
    let start = time::now_ns();
    let face = Face::ui()?;
    let parse_ns = time::now_ns() - start;
    let ours = Box::into_raw(Box::new(Loaded { face, parse_ns }));
    match LOADED.compare_exchange(ptr::null_mut(), ours, Ordering::AcqRel, Ordering::Acquire) {
        // SAFETY: `ours` came from `Box::into_raw` and is now published,
        // never to be freed.
        Ok(_) => Ok(unsafe { &*ours }),
        Err(theirs) => {
            // SAFETY: `ours` was never published, so this is its only
            // owner.
            drop(unsafe { Box::from_raw(ours) });
            // SAFETY: `theirs` was published by the winner from
            // `Box::into_raw` and is never freed, as above.
            Ok(unsafe { &*theirs })
        }
    }
}

/// What rendering the sample twice found.
pub struct Sample {
    pub loaded: &'static Loaded,
    /// The first render, rasterizing each distinct glyph.
    pub first_ns: u64,
    pub first: Stats,
    /// The second, from the cache.
    pub second_ns: u64,
    pub second: Stats,
    pub width: u32,
    pub height: u32,
    pub hash: u64,
    /// Whether the second render's mask was the first's.
    pub same_again: bool,
    pub cached_bytes: usize,
}

/// Render [`sample::TEXT`] twice with a fresh cache, timing each render.
pub fn render_sample() -> Result<Sample, &'static str> {
    let loaded = face().map_err(|e| e.0)?;
    let mut font = Font::new(&loaded.face, CACHE_BYTES);
    let start = time::now_ns();
    let mask = font.render_mask(sample::TEXT, PX, MAX_WIDTH).ok_or("the sample is wider than its limit")?;
    let first_done = time::now_ns();
    let first = font.stats();
    let again = font.render_mask(sample::TEXT, PX, MAX_WIDTH);
    let second_done = time::now_ns();
    Ok(Sample {
        loaded,
        first_ns: first_done - start,
        first,
        second_ns: second_done - first_done,
        second: font.stats(),
        width: mask.width(),
        height: mask.height(),
        hash: mask.fnv1a(),
        same_again: again.as_ref() == Some(&mask),
        cached_bytes: font.cached_bytes(),
    })
}

/// `font`: the sample rendered twice, in three lines:
///
/// ```text
/// font: Atkinson Hyperlegible Regular, 369 glyphs, parsed in 61.854 ms on first use
/// font: "Lumen: AVATAR fj 0123" at 24 px: 251x30, rendered in 6.531 ms (17 rasterized), again in 0.337 ms (0 rasterized)
/// font: cache 25 hits, 17 misses, 0 evictions, 4096 of 262144 bytes; mask fnv1a 0xcaef385be76cda2b (the golden)
/// ```
///
/// A mask that isn't the golden ends the last line with `(not the golden
/// 0x...)` instead, and a second render that differs from the first adds
/// a line saying so; either fails the command.
pub fn command(ctx: &mut Ctx<'_>, _: &[String]) {
    let s = match render_sample() {
        Ok(s) => s,
        Err(e) => return ctx.fail(format_args!("font: {e}")),
    };
    let face = &s.loaded.face;
    let out = &mut *ctx.out;
    let _ = writeln!(
        out,
        "font: {}, {} glyphs, parsed in {} on first use",
        face.name().unwrap_or("(unnamed)"),
        face.glyph_count(),
        Millis(s.loaded.parse_ns)
    );
    let _ = writeln!(
        out,
        "font: {:?} at {} px: {}x{}, rendered in {} ({} rasterized), again in {} ({} rasterized)",
        sample::TEXT,
        sample::PX,
        s.width,
        s.height,
        Millis(s.first_ns),
        s.first.misses,
        Millis(s.second_ns),
        s.second.misses - s.first.misses,
    );
    let _ = write!(
        out,
        "font: cache {} hits, {} misses, {} evictions, {} of {CACHE_BYTES} bytes; mask fnv1a {:#018x}",
        s.second.hits, s.second.misses, s.second.evictions, s.cached_bytes, s.hash
    );
    let golden = s.hash == sample::HASH;
    if golden {
        let _ = writeln!(out, " (the golden)");
    } else {
        let _ = writeln!(out, " (not the golden {:#018x})", sample::HASH);
    }
    if !s.same_again {
        ctx.fail("font: the second render's mask differs from the first's");
    } else if !golden {
        ctx.fail("font: the mask isn't the golden");
    }
}

crate::ktest!(
    fn renders_and_caches() {
        // The embedded font parses, the sample's mask hashes to the host
        // golden's value, and the second render rasterizes nothing: every
        // glyph is a hit.
        use crate::{kassert, kassert_eq};
        let s = match render_sample() {
            Ok(s) => s,
            Err(e) => {
                crate::ktest::diag(format_args!("{e}"));
                return Err("the sample didn't render");
            }
        };
        log::info!(
            target: "font",
            "sample: parsed {} glyphs in {} (first use), rendered in {} ({} rasterized), again in {} ({} rasterized)",
            s.loaded.face.glyph_count(),
            Millis(s.loaded.parse_ns),
            Millis(s.first_ns),
            s.first.misses,
            Millis(s.second_ns),
            s.second.misses - s.first.misses,
        );
        kassert_eq!(s.loaded.face.name(), Some("Atkinson Hyperlegible Regular"));
        kassert_eq!(s.hash, sample::HASH);
        kassert!(s.same_again);
        kassert!(s.first.misses > 0);
        kassert_eq!(s.second.misses, s.first.misses);
        let chars = sample::TEXT.chars().count() as u64;
        kassert_eq!(s.second.hits, s.first.hits + chars);
        kassert_eq!(s.second.evictions, 0);
        kassert!(s.cached_bytes > 0 && s.cached_bytes <= CACHE_BYTES);
        // Parsed once: the next use is the same face.
        let again = match face() {
            Ok(loaded) => loaded,
            Err(_) => return Err("the font didn't parse the second time"),
        };
        kassert!(core::ptr::eq(again, s.loaded));
        Ok(())
    }
);
