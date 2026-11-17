//! A parsed face: `fontdue`'s font behind integer metrics, and the pen that
//! lays a line out on it.

use crate::{Measure, Px};
use alloc::vec::Vec;
use core::fmt;
use fontdue::{Font as Fontdue, FontSettings};

/// A face couldn't be parsed (`fontdue`'s reason).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error(pub &'static str);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "can't parse the font: {}", self.0)
    }
}

/// A face's vertical metrics at one size, in whole pixels: `ascent` above
/// the baseline and `descent` below it (both positive, rounded up so a
/// line box holds them), and `line_gap` between lines.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineMetrics {
    pub ascent: i32,
    pub descent: i32,
    pub line_gap: i32,
}

/// One rasterized glyph: its coverage mask (`width * height` bytes,
/// row-major; none for a glyph without ink, such as a space) and where it
/// sits against the pen: `left` pixels right of it, its top row `top`
/// pixels above the baseline. Only the cache and the layout see these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Glyph {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub(crate) left: i16,
    pub(crate) top: i16,
    pub(crate) coverage: Vec<u8>,
}

/// Where a glyph's ink goes against the pen, from the metrics alone: the
/// same box [`Face::rasterize`] fills (a host test checks every glyph).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InkBox {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

// How many glyphs this thread has rasterized: the tests' proof that a cache
// hit rasterizes nothing.
#[cfg(test)]
std::thread_local! {
    pub(crate) static RASTERIZED: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
}

/// A parsed face. Immutable once parsed, so one face can be shared (behind
/// a `&`) by every thread drawing text, each with its own glyph cache
/// ([`crate::Font`]).
pub struct Face {
    font: Fontdue,
}

impl fmt::Debug for Face {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Face").field("name", &self.name()).field("glyphs", &self.glyph_count()).finish()
    }
}

impl Face {
    /// Parse a face. `fontdue` reads every glyph's outline now, so this is
    /// the slow part of loading a font (soft-float in the kernel: about
    /// 60 ms for [`crate::UI_FONT`] under QEMU's TCG).
    pub fn parse(data: &[u8]) -> Result<Face, Error> {
        let settings = FontSettings {
            collection_index: 0,
            // The size outlines are flattened for: the top of the desktop's
            // 12 to 32 pixels, so the largest text keeps smooth curves.
            scale: 32.0,
            // No GSUB substitutions: nothing shapes text yet.
            load_substitutions: false,
        };
        Fontdue::from_bytes(data, settings).map(|font| Face { font }).map_err(Error)
    }

    /// [`crate::UI_FONT`].
    pub fn ui() -> Result<Face, Error> {
        Face::parse(crate::UI_FONT)
    }

    /// The face's full name, if it has one.
    pub fn name(&self) -> Option<&str> {
        self.font.name()
    }

    pub fn glyph_count(&self) -> u16 {
        self.font.glyph_count()
    }

    /// The glyph for `c`: 0, the missing-glyph box, if the face lacks it.
    pub fn glyph_index(&self, c: char) -> u16 {
        self.font.lookup_glyph_index(c)
    }

    pub fn line_metrics(&self, px: Px) -> LineMetrics {
        match self.font.horizontal_line_metrics(f32::from(px.get())) {
            Some(m) => LineMetrics { ascent: ceil(m.ascent), descent: ceil(-m.descent), line_gap: ceil(m.line_gap) },
            // Every face with horizontal text has an `hhea` table; without
            // one, the em square above the baseline.
            None => LineMetrics { ascent: i32::from(px.get()), descent: 0, line_gap: 0 },
        }
    }

    /// `text`'s size as one line at `px`, from the metrics alone (nothing
    /// is rasterized, and no cache is needed). It agrees with
    /// [`crate::Font::render`]: both lay the line out with [`Face::layout`].
    pub fn measure(&self, text: &str, px: Px) -> Measure {
        let metrics = self.line_metrics(px);
        let pen = self.layout(text, px, |_, _| ());
        Measure { width: pen.width(), ascent: metrics.ascent, descent: metrics.descent }
    }

    /// Lay `text` out as one line at `px`: call `each` with every
    /// character's glyph and the whole pixel its origin goes on, left to
    /// right, and return the pen where the line ends. The one layout that
    /// measuring, rendering and compositing share.
    pub(crate) fn layout(&self, text: &str, px: Px, mut each: impl FnMut(u16, i32)) -> Pen {
        let mut pen = Pen::default();
        for c in text.chars() {
            let glyph = self.glyph_index(c);
            let at = pen.kern_to(self, glyph, px);
            each(glyph, at);
            pen.advance(self.advance(glyph, px));
        }
        pen
    }

    /// `glyph` if the face has it, else 0 (the missing-glyph box): fontdue
    /// indexes its glyphs without checking, and panics past the end.
    fn checked(&self, glyph: u16) -> u16 {
        if glyph < self.glyph_count() { glyph } else { 0 }
    }

    /// How far the pen moves after `glyph` at `px`, in 64ths of a pixel,
    /// from the metrics alone (nothing rasterized).
    pub(crate) fn advance(&self, glyph: u16, px: Px) -> i32 {
        to_64ths(self.font.metrics_indexed(self.checked(glyph), f32::from(px.get())).advance_width)
    }

    /// Where `glyph`'s ink goes at `px`, from the metrics alone.
    pub(crate) fn ink_box(&self, glyph: u16, px: Px) -> InkBox {
        let m = self.font.metrics_indexed(self.checked(glyph), f32::from(px.get()));
        let (width, height) = (m.width as u32, m.height as u32);
        if width == 0 || height == 0 || width > u32::from(u16::MAX) || height > u32::from(u16::MAX) {
            return InkBox { left: m.xmin, top: m.ymin, width: 0, height: 0 };
        }
        InkBox { left: m.xmin, top: m.ymin.saturating_add(height as i32), width, height }
    }

    /// The kerning between `left` and `right` at `px`, in 64ths of a pixel
    /// (0 if the face's `kern` table has no pair for them, or it has no
    /// `kern` table: `fontdue` doesn't read `GPOS`).
    pub(crate) fn kern(&self, left: u16, right: u16, px: Px) -> i32 {
        self.font.horizontal_kern_indexed(left, right, f32::from(px.get())).map_or(0, to_64ths)
    }

    /// Rasterize `glyph` at `px` (the slow path the cache saves).
    pub(crate) fn rasterize(&self, glyph: u16, px: Px) -> Glyph {
        #[cfg(test)]
        RASTERIZED.with(|n| n.set(n.get() + 1));
        let (metrics, coverage) = self.font.rasterize_indexed(self.checked(glyph), f32::from(px.get()));
        // fontdue's mask is width * height bytes. A glyph is at most a few
        // em squares wide and an em at most MAX_PX pixels, so both fit u16;
        // if a broken face says otherwise, the glyph draws without ink.
        debug_assert_eq!(metrics.width * metrics.height, coverage.len());
        let (width, height, coverage) = match (u16::try_from(metrics.width), u16::try_from(metrics.height)) {
            (Ok(w), Ok(h)) if w > 0 && h > 0 => (w, h, coverage),
            _ => (0, 0, Vec::new()),
        };
        Glyph {
            width,
            height,
            left: saturate_i16(metrics.xmin),
            // `ymin` is the mask's bottom row's offset above the baseline.
            top: saturate_i16(metrics.ymin.saturating_add(i32::from(height))),
            coverage,
        }
    }
}

/// The pen moving along a line, in 64ths of a pixel.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Pen {
    at: i64,
    previous: Option<u16>,
}

impl Pen {
    /// Kern from the previous glyph to `glyph`, and return the whole pixel
    /// `glyph`'s origin goes on.
    pub(crate) fn kern_to(&mut self, face: &Face, glyph: u16, px: Px) -> i32 {
        let kern = self.previous.map_or(0, |left| face.kern(left, glyph, px));
        self.previous = Some(glyph);
        self.kern(kern)
    }

    /// Move by `kern` 64ths (before a glyph) and return the whole pixel
    /// nearest the pen (halves round up).
    pub(crate) fn kern(&mut self, kern: i32) -> i32 {
        self.at += i64::from(kern);
        i32::try_from((self.at + 32).div_euclid(64)).unwrap_or(i32::MAX)
    }

    /// Move by a glyph's advance, in 64ths.
    pub(crate) fn advance(&mut self, advance: i32) {
        self.at += i64::from(advance);
    }

    /// Where the pen is, in 64ths of a pixel.
    pub(crate) fn at(&self) -> i64 {
        self.at
    }

    /// Where the pen ended, in whole pixels, rounded up (0 if kerning took
    /// it back past the start).
    pub(crate) fn width(&self) -> u32 {
        u32::try_from((self.at.max(0) + 63) / 64).unwrap_or(u32::MAX)
    }
}

fn saturate_i16(v: i32) -> i16 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// `x` pixels in 64ths of a pixel, the nearest (halves away from zero).
/// `core` has no `f32::round`; the casts saturate, and NaN becomes 0.
pub(crate) fn to_64ths(x: f32) -> i32 {
    let scaled = x * 64.0;
    if scaled >= 0.0 { (scaled + 0.5) as i32 } else { -((-scaled + 0.5) as i32) }
}

/// The smallest whole number at or above `x` (`core` has no `f32::ceil`).
pub(crate) fn ceil(x: f32) -> i32 {
    let truncated = x as i32;
    if (truncated as f32) < x { truncated.saturating_add(1) } else { truncated }
}
