//! Anti-aliased TrueType text for the desktop (#148), kept free of kernel
//! dependencies so it can be tested on the host.
//!
//! - [`Face`]: a parsed face, immutable, so one can be shared by every
//!   thread that draws text. [`Face::measure`] sizes a line from the
//!   metrics alone.
//! - [`Font`]: a borrowed face plus a glyph cache of its own (one per
//!   drawing thread, so a miss never waits on another thread's lock).
//!   [`Font::render`] lays out one line and hands out each glyph's
//!   coverage mask with its position; [`Font::render_mask`] composites a
//!   line into one [`LineMask`].
//! - [`UI_FONT`]: the embedded UI font, Atkinson Hyperlegible Regular (SIL
//!   Open Font License 1.1, `assets/fonts/`).
//!
//! Rasterizing is `fontdue`'s, built without its `simd` feature: the kernel
//! has no SSE, so its floating point is soft-float library calls, slow but
//! IEEE 754 exact. The host and the kernel then make the same masks (the
//! [`sample`]'s hash pins it), because both run fontdue's portable
//! (non-`simd`) code: its `sqrt`, `atan2`, `floor` and the like are its
//! own pure-Rust functions (`platform::float`), not `libm` (`ttf-parser`
//! uses `core_maths` only for COLR transforms, which nothing here reads);
//! basic `f32` operations are correctly rounded on both; and Rust doesn't
//! contract `a * b + c` into a fused multiply-add. Turning `simd` on for an
//! x86-64 host (CI's) would swap in SSE versions of those functions and
//! break that agreement (it changes nothing on other hosts). Rasterizing
//! is slow, which is why the cache exists: a glyph is rasterized once per
//! pixel size, then reused for as long as it stays cached.
//!
//! Positions are integers. Advances and kerning are kept in 64ths of a
//! pixel while a line is laid out (kerning from the face's `kern` table, if
//! it has one), and each glyph is placed on the whole pixel nearest its pen
//! position, so one cached mask serves every occurrence of a glyph at a
//! size (no subpixel positioning).
//!
//! The coverage masks are plain bytes, one per pixel, 0 (none) to 255
//! (full), row-major with no padding: what `lumen-gfx`'s `blend_mask` (#147)
//! draws, given each mask's width, height and bytes.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

mod cache;
mod face;

pub use cache::Stats;
pub use face::{Error, Face, LineMetrics};

use alloc::vec;
use cache::GlyphCache;

/// The embedded UI font: Atkinson Hyperlegible Regular 1.006, by the
/// Braille Institute of America, under the SIL Open Font License 1.1
/// (`assets/fonts/AtkinsonHyperlegible-OFL.txt`). Picked over Inter and
/// Noto Sans for its size: 54 KB and 369 glyphs, where those are hundreds
/// of KB and thousands of glyphs, and `fontdue` parses every glyph's
/// outline when the face loads (soft-float in the kernel). It kerns only
/// in its `GPOS` table, which `fontdue` doesn't read (it reads `kern`), so
/// its text is unkerned.
///
/// Source: <https://raw.githubusercontent.com/google/fonts/1b22086d13bdbbf5ec63d01a39a49246e4ac355f/ofl/atkinsonhyperlegible/AtkinsonHyperlegible-Regular.ttf>
/// sha256: 7fb917c89019896d0b52ee84b7cbb3304c18cb90b19a62f5e32712bd23e97669
/// FNV-1a 64: [`UI_FONT_FNV1A`] (a host test checks it)
pub static UI_FONT: &[u8] = include_bytes!("../../../assets/fonts/AtkinsonHyperlegible-Regular.ttf");

/// [`fnv1a`] of [`UI_FONT`]'s bytes: a changed font file fails
/// `cargo test -p lumen-font`.
pub const UI_FONT_FNV1A: u64 = 0x323b_1baa_8763_5ac9;

/// The largest pixel size [`Px`] takes: it bounds a glyph's mask (an em
/// square of 200x200 is 40 KB) and keeps 64ths of a pixel far from `i32`'s
/// limits for any line a screen can show.
pub const MAX_PX: u16 = 200;

/// A text size in pixels per em, 1 to [`MAX_PX`]. Whole pixels only, so
/// the size can key the glyph cache.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Px(u16);

impl Px {
    /// `px` pixels per em, or `None` outside 1 to [`MAX_PX`].
    pub const fn new(px: u16) -> Option<Px> {
        if px == 0 || px > MAX_PX { None } else { Some(Px(px)) }
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

/// One glyph of a laid-out line: its coverage mask and where the mask's
/// top-left pixel goes, relative to the line box's top-left corner (the
/// baseline is `ascent` pixels down; see [`Face::line_metrics`]). A mask
/// may stick out of the line box (an accent above, a descent below its
/// rounding, a glyph left of the pen at the start).
#[derive(Clone, Copy, Debug)]
pub struct Placed<'a> {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    coverage: &'a [u8],
}

impl<'a> Placed<'a> {
    pub fn x(&self) -> i32 {
        self.x
    }

    pub fn y(&self) -> i32 {
        self.y
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Exactly `width * height` bytes, row-major.
    pub fn coverage(&self) -> &'a [u8] {
        self.coverage
    }
}

/// A line's size: the advance width (where the next text would start, in
/// whole pixels, rounded up) and the face's ascent and descent at that
/// size (above and below the baseline, both positive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Measure {
    pub width: u32,
    pub ascent: i32,
    pub descent: i32,
}

/// A whole line's coverage in one mask ([`Font::render_mask`]). Its
/// top-left pixel is at (`x`, `y`) relative to the line box's top-left
/// corner, as for [`Placed`]; it covers the line box and every glyph's
/// mask, so `x` and `y` are 0 or negative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineMask {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    coverage: alloc::vec::Vec<u8>,
}

impl LineMask {
    pub fn x(&self) -> i32 {
        self.x
    }

    pub fn y(&self) -> i32 {
        self.y
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Exactly `width * height` bytes, row-major.
    pub fn coverage(&self) -> &[u8] {
        &self.coverage
    }

    /// [`fnv1a`] over the position, the size (each little-endian) and the
    /// coverage: the golden that host tests and the kernel compare.
    pub fn fnv1a(&self) -> u64 {
        let header = [self.x.to_le_bytes(), self.y.to_le_bytes(), self.width.to_le_bytes(), self.height.to_le_bytes()];
        fnv1a(header.iter().flatten().chain(&self.coverage).copied())
    }
}

/// 64-bit FNV-1a over `bytes`.
pub fn fnv1a(bytes: impl IntoIterator<Item = u8>) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// The string the golden pins, on the host (`tests/golden/sample.pgm`) and
/// in the kernel (`ktest:lumen_kernel::font::renders_and_caches`): the
/// [`LineMask`] of [`TEXT`](sample::TEXT) in [`UI_FONT`] at
/// [`PX`](sample::PX) hashes to [`HASH`](sample::HASH) on both.
pub mod sample {
    pub const TEXT: &str = "Lumen: AVATAR fj 0123";
    pub const PX: u16 = 24;
    pub const HASH: u64 = 0xcaef_385b_e76c_da2b;
}

/// A face and a glyph cache for it. The face is borrowed (one parsed face
/// serves every `Font`); the cache is this `Font`'s own, and glyph numbers
/// mean nothing in another face, so they come together.
#[derive(Debug)]
pub struct Font<'f> {
    face: &'f Face,
    cache: GlyphCache,
}

impl<'f> Font<'f> {
    /// `face` with a cache of at most `cache_bytes` (coverage bytes plus a
    /// fixed 64 per glyph).
    pub fn new(face: &'f Face, cache_bytes: usize) -> Font<'f> {
        Font { face, cache: GlyphCache::new(cache_bytes) }
    }

    pub fn face(&self) -> &'f Face {
        self.face
    }

    /// The cache's counters.
    pub fn stats(&self) -> Stats {
        self.cache.stats()
    }

    /// Bytes the cache holds now (never more than its bound).
    pub fn cached_bytes(&self) -> usize {
        self.cache.used()
    }

    /// Lay out `text` as one line at `px` and call `draw` with each glyph
    /// that has ink, left to right (a space advances the pen and draws
    /// nothing). Characters the face lacks draw its missing-glyph box (and
    /// count in [`Stats::missing`]). Glyphs come from the cache, rasterized
    /// only on a miss.
    pub fn render(&mut self, text: &str, px: Px, mut draw: impl FnMut(Placed<'_>)) {
        let Font { face, cache } = self;
        let ascent = face.line_metrics(px).ascent;
        face.layout(text, px, |glyph, at| {
            if glyph == 0 {
                cache.count_missing();
            }
            cache.with(face, glyph, px, |g| {
                if !g.coverage.is_empty() {
                    draw(Placed {
                        x: at.saturating_add(g.ink.left),
                        y: ascent.saturating_sub(g.ink.top),
                        width: g.ink.width,
                        height: g.ink.height,
                        coverage: &g.coverage,
                    });
                }
            });
        });
    }

    /// `text` at `px` composited into one mask: each pixel the highest
    /// coverage any glyph gives it (overlapping glyphs, as kerning can
    /// make, don't darken where they meet). The mask's box comes from the
    /// metrics, so it is allocated once before anything is drawn, and a
    /// line whose mask would be wider than `max_width` pixels (counting any
    /// ink left of the line's start) is refused before anything is
    /// rasterized: [`TooWide`] says how wide it would have been.
    pub fn render_mask(&mut self, text: &str, px: Px, max_width: u32) -> Result<LineMask, TooWide> {
        let face = self.face;
        let metrics = face.line_metrics(px);
        // The bounding box of the line box and every glyph's ink, in i64 so
        // no line overflows it.
        let (mut x0, mut y0) = (0_i64, 0_i64);
        let mut y1 = i64::from(metrics.ascent + metrics.descent);
        let mut x1 = 0_i64;
        let pen = face.layout(text, px, |glyph, at| {
            let ink = face.ink_box(glyph, px);
            if ink.width > 0 {
                let x = i64::from(at) + i64::from(ink.left);
                let y = i64::from(metrics.ascent) - i64::from(ink.top);
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + i64::from(ink.width));
                y1 = y1.max(y + i64::from(ink.height));
            }
        });
        x1 = x1.max(i64::from(pen.width()));
        let too_wide = TooWide { width: u64::try_from(x1 - x0).unwrap_or(u64::MAX) };
        let width = u32::try_from(x1 - x0).ok().filter(|&w| w <= max_width).ok_or(too_wide)?;
        // A line box and glyphs a few ems tall: far inside u32 and i32.
        let height = u32::try_from(y1 - y0).map_err(|_| too_wide)?;
        let (x0, y0) = (i32::try_from(x0).map_err(|_| too_wide)?, i32::try_from(y0).map_err(|_| too_wide)?);
        let mut coverage = vec![0_u8; width as usize * height as usize];
        self.render(text, px, |g| {
            // Inside the box by construction (the ink box is the mask's);
            // clipped all the same, so a disagreement can't write outside.
            let (gx, gy) = (i64::from(g.x) - i64::from(x0), i64::from(g.y) - i64::from(y0));
            debug_assert!(
                gx >= 0
                    && gy >= 0
                    && gx + i64::from(g.width) <= i64::from(width)
                    && gy + i64::from(g.height) <= i64::from(height)
            );
            for row in 0..g.height as usize {
                let y = gy + row as i64;
                if y < 0 || y >= i64::from(height) {
                    continue;
                }
                let src = &g.coverage[row * g.width as usize..][..g.width as usize];
                for (col, s) in src.iter().enumerate() {
                    let x = gx + col as i64;
                    if x >= 0 && x < i64::from(width) {
                        let d = &mut coverage[y as usize * width as usize + x as usize];
                        *d = (*d).max(*s);
                    }
                }
            }
        });
        Ok(LineMask { x: x0, y: y0, width, height, coverage })
    }
}

/// [`Font::render_mask`] refused a line: its mask would be `width` pixels
/// wide, more than the limit it was given.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooWide {
    pub width: u64,
}

impl core::fmt::Display for TooWide {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "the line would be {} pixels wide, more than its limit", self.width)
    }
}

// One parsed face is shared by every thread that draws text (behind a
// `&'static`), and a `Font` or a mask may move to the thread that uses it:
// a change that made any of them unsafe to share fails to build here.
const _: () = {
    const fn send_sync<T: Send + Sync>() {}
    const fn send<T: Send>() {}
    send_sync::<Face>();
    send_sync::<LineMask>();
    send::<Font<'static>>();
};

#[cfg(test)]
mod tests;
