//! Anti-aliased TrueType text for the desktop (#148), kept free of kernel
//! dependencies so it can be tested on the host.
//!
//! - [`Font`]: a parsed face plus its [`cache`](mod@cache) of rasterized
//!   glyphs. [`Font::render`] lays out one line and hands out each glyph's
//!   coverage mask with its position; [`Font::render_mask`] composites a
//!   line into one [`Mask`]; [`Font::measure`] sizes a line without
//!   rasterizing anything.
//! - [`UI_FONT`]: the embedded UI font, Atkinson Hyperlegible Regular (SIL
//!   Open Font License 1.1, `assets/fonts/`).
//!
//! Rasterizing is `fontdue`'s, built without its `simd` feature: the kernel
//! has no SSE, so its floating point is soft-float library calls, slow but
//! IEEE 754 exact, which makes a glyph's mask the same bytes on the host and
//! in the kernel ([`sample`] pins one). That slowness is why the cache
//! exists: a glyph is rasterized once per pixel size, then reused for as
//! long as it stays cached.
//!
//! Positions are integers. Advances and kerning are kept in 64ths of a
//! pixel while a line is laid out (kerning from the face's `kern` table,
//! if it has one), and each glyph is placed on the whole
//! pixel nearest its pen position, so one cached mask serves every
//! occurrence of a glyph at a size (no subpixel positioning).
//!
//! The coverage masks are plain bytes, one per pixel, 0 (none) to 255
//! (full), row-major with no padding: what `lumen-gfx`'s `blend_mask` draws.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

pub mod cache;
mod face;

pub use cache::Stats;
pub use face::{Error, Face, Glyph, LineMetrics};

use alloc::vec;
use alloc::vec::Vec;
use cache::GlyphCache;

/// The embedded UI font: Atkinson Hyperlegible Regular 1.006, by the
/// Braille Institute of America, under the SIL Open Font License 1.1
/// (`assets/fonts/AtkinsonHyperlegible-OFL.txt`). Picked over Inter and
/// Noto Sans for its size: 54 KiB and about 250 glyphs, where those are
/// hundreds of KiB and thousands of glyphs, and `fontdue` parses every
/// glyph's outline when the face loads (soft-float in the kernel). It
/// kerns only in its `GPOS` table, which `fontdue` doesn't read (it reads
/// `kern`), so its text is unkerned.
///
/// Source: <https://raw.githubusercontent.com/google/fonts/1b22086d13bdbbf5ec63d01a39a49246e4ac355f/ofl/atkinsonhyperlegible/AtkinsonHyperlegible-Regular.ttf>
/// sha256: 7fb917c89019896d0b52ee84b7cbb3304c18cb90b19a62f5e32712bd23e97669
pub static UI_FONT: &[u8] = include_bytes!("../../../assets/fonts/AtkinsonHyperlegible-Regular.ttf");

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
/// baseline is `ascent` pixels down; see [`Font::line_metrics`]). A mask
/// may stick out of the line box (an accent above, a descent below its
/// rounding, a glyph left of the pen at the start).
#[derive(Clone, Copy, Debug)]
pub struct Placed<'a> {
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
    /// `width * height` bytes, row-major.
    pub coverage: &'a [u8],
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
pub struct Mask {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// `width * height` bytes, row-major.
    pub coverage: Vec<u8>,
}

impl Mask {
    /// 64-bit FNV-1a over the position, the size and the coverage: a
    /// golden that host tests and the kernel's ktest compare.
    pub fn fnv1a(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        let header = [self.x.to_le_bytes(), self.y.to_le_bytes(), self.width.to_le_bytes(), self.height.to_le_bytes()];
        for byte in header.iter().flatten().chain(&self.coverage) {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }
}

/// The string the golden pins, on the host (`tests/golden/sample.pgm`) and
/// in the kernel (`ktest:lumen_kernel::font::renders_and_caches`): the
/// mask of [`TEXT`](sample::TEXT) in [`UI_FONT`] at [`PX`](sample::PX)
/// hashes to [`HASH`](sample::HASH).
pub mod sample {
    pub const TEXT: &str = "Lumen: AVATAR fj 0123";
    pub const PX: u16 = 24;
    pub const HASH: u64 = 0x02cb_17f5_4193_ee6d;
}

/// A face and its glyph cache. The cache belongs to the face (glyph
/// numbers mean nothing in another face), so they come together.
pub struct Font {
    face: Face,
    cache: GlyphCache,
}

impl core::fmt::Debug for Font {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Font").field("face", &self.face).field("cache", &self.cache).finish()
    }
}

impl Font {
    /// Parse a TrueType or OpenType face, with a cache of at most
    /// `cache_bytes` (see [`GlyphCache`] for what counts).
    pub fn parse(data: &[u8], cache_bytes: usize) -> Result<Font, Error> {
        Ok(Font { face: Face::parse(data)?, cache: GlyphCache::new(cache_bytes) })
    }

    /// [`UI_FONT`], with a cache of at most `cache_bytes`.
    pub fn ui(cache_bytes: usize) -> Result<Font, Error> {
        Font::parse(UI_FONT, cache_bytes)
    }

    pub fn face(&self) -> &Face {
        &self.face
    }

    /// The cache's counters.
    pub fn stats(&self) -> Stats {
        self.cache.stats()
    }

    /// Bytes the cache holds now (never more than its bound).
    pub fn cached_bytes(&self) -> usize {
        self.cache.used()
    }

    /// The face's line metrics at `px`.
    pub fn line_metrics(&self, px: Px) -> LineMetrics {
        self.face.line_metrics(px)
    }

    /// Lay out `text` as one line at `px` and call `draw` with each glyph
    /// that has ink, left to right (a space advances the pen and draws
    /// nothing). Characters the face lacks draw its missing-glyph box.
    /// Glyphs come from the cache, rasterized only on a miss.
    pub fn render(&mut self, text: &str, px: Px, mut draw: impl FnMut(Placed<'_>)) {
        let ascent = self.face.line_metrics(px).ascent;
        let Font { face, cache } = self;
        let mut pen = Pen::default();
        for c in text.chars() {
            let index = face.glyph_index(c);
            let at = pen.kern_to(face, index, px);
            let advance = cache.with(face, index, px, |glyph| {
                if !glyph.coverage().is_empty() {
                    draw(Placed {
                        x: at.saturating_add(i32::from(glyph.left)),
                        y: ascent - i32::from(glyph.top),
                        width: glyph.width,
                        height: glyph.height,
                        coverage: glyph.coverage(),
                    });
                }
                glyph.advance
            });
            pen.advance(advance);
        }
    }

    /// `text`'s size as one line at `px`, from the face's metrics alone:
    /// nothing is rasterized and the cache isn't touched.
    pub fn measure(&self, text: &str, px: Px) -> Measure {
        let metrics = self.face.line_metrics(px);
        let mut pen = Pen::default();
        for c in text.chars() {
            let index = self.face.glyph_index(c);
            pen.kern_to(&self.face, index, px);
            pen.advance(self.face.advance(index, px));
        }
        Measure { width: pen.width(), ascent: metrics.ascent, descent: metrics.descent }
    }

    /// `text` at `px` composited into one mask: each pixel the highest
    /// coverage any glyph gives it (overlapping glyphs, as kerning can
    /// make, don't darken where they meet).
    pub fn render_mask(&mut self, text: &str, px: Px) -> Mask {
        let measure = self.measure(text, px);
        let line_height = measure.ascent + measure.descent;
        // The bounding box of the line box and every glyph's mask.
        let (mut x0, mut y0) = (0_i32, 0_i32);
        let (mut x1, mut y1) = (i32::try_from(measure.width).unwrap_or(i32::MAX), line_height);
        let mut placed = Vec::new();
        self.render(text, px, |g| {
            x0 = x0.min(g.x);
            y0 = y0.min(g.y);
            x1 = x1.max(g.x.saturating_add(i32::from(g.width)));
            y1 = y1.max(g.y + i32::from(g.height));
            placed.push((g.x, g.y, g.width, g.height, Vec::from(g.coverage)));
        });
        let width = (x1 - x0) as u32;
        let height = (y1 - y0) as u32;
        let mut coverage = vec![0_u8; width as usize * height as usize];
        for (gx, gy, gw, gh, mask) in placed {
            for row in 0..usize::from(gh) {
                let dst_row = (gy - y0) as usize + row;
                let dst = dst_row * width as usize + (gx - x0) as usize;
                let src = &mask[row * usize::from(gw)..][..usize::from(gw)];
                for (d, s) in coverage[dst..dst + usize::from(gw)].iter_mut().zip(src) {
                    *d = (*d).max(*s);
                }
            }
        }
        Mask { x: x0, y: y0, width, height, coverage }
    }
}

/// The pen moving along a line, in 64ths of a pixel: [`Font::render`] and
/// [`Font::measure`] share it, so a line measures as wide as it renders.
#[derive(Clone, Copy, Debug, Default)]
struct Pen {
    at: i64,
    previous: Option<u16>,
}

impl Pen {
    /// Kern from the previous glyph to `glyph`, and return the whole pixel
    /// `glyph`'s origin goes on.
    fn kern_to(&mut self, face: &Face, glyph: u16, px: Px) -> i32 {
        let kern = self.previous.map_or(0, |left| face.kern(left, glyph, px));
        self.previous = Some(glyph);
        self.kern(kern)
    }

    /// Move by `kern` 64ths (before a glyph) and return the whole pixel
    /// nearest the pen (halves round up).
    fn kern(&mut self, kern: i32) -> i32 {
        self.at += i64::from(kern);
        i32::try_from((self.at + 32).div_euclid(64)).unwrap_or(i32::MAX)
    }

    /// Move by a glyph's advance, in 64ths.
    fn advance(&mut self, advance: i32) {
        self.at += i64::from(advance);
    }

    /// Where the pen ended, in whole pixels, rounded up (0 if kerning took
    /// it back past the start).
    fn width(&self) -> u32 {
        u32::try_from((self.at.max(0) + 63) / 64).unwrap_or(u32::MAX)
    }
}

#[cfg(test)]
mod tests;
