//! A parsed face: `fontdue`'s font behind integer metrics.

use crate::Px;
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

/// One rasterized glyph: its coverage mask, where the mask sits against
/// the pen (`left` pixels right of it, its top row `top` pixels above the
/// baseline) and how far the pen moves after it, in 64ths of a pixel.
#[derive(Clone, PartialEq, Eq)]
pub struct Glyph {
    pub width: u16,
    pub height: u16,
    pub left: i16,
    pub top: i16,
    pub advance: i32,
    coverage: Vec<u8>,
}

impl Glyph {
    /// `width * height` coverage bytes, row-major (empty for a glyph
    /// without ink, such as a space).
    pub fn coverage(&self) -> &[u8] {
        &self.coverage
    }
}

impl fmt::Debug for Glyph {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Glyph")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("left", &self.left)
            .field("top", &self.top)
            .field("advance", &self.advance)
            .finish_non_exhaustive()
    }
}

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
    /// the slow part of loading a font (soft-float in the kernel).
    pub fn parse(data: &[u8]) -> Result<Face, Error> {
        let settings = FontSettings {
            collection_index: 0,
            // The size outlines are flattened for: text on the desktop is
            // 12 to 32 pixels.
            scale: 24.0,
            // No GSUB substitutions: nothing shapes text yet.
            load_substitutions: false,
        };
        Fontdue::from_bytes(data, settings).map(|font| Face { font }).map_err(Error)
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

    /// How far the pen moves after `glyph` at `px`, in 64ths of a pixel,
    /// from the metrics alone (nothing rasterized).
    pub fn advance(&self, glyph: u16, px: Px) -> i32 {
        to_64ths(self.font.metrics_indexed(glyph, f32::from(px.get())).advance_width)
    }

    /// The kerning between `left` and `right` at `px`, in 64ths of a pixel
    /// (0 if the face's `kern` table has no pair for them).
    pub fn kern(&self, left: u16, right: u16, px: Px) -> i32 {
        self.font.horizontal_kern_indexed(left, right, f32::from(px.get())).map_or(0, to_64ths)
    }

    /// Rasterize `glyph` at `px` (the slow path the cache saves).
    pub fn rasterize(&self, glyph: u16, px: Px) -> Glyph {
        let (metrics, coverage) = self.font.rasterize_indexed(glyph, f32::from(px.get()));
        // A glyph is at most a few em squares wide, and an em is at most
        // MAX_PX pixels, so each of these fits; saturate rather than wrap
        // if a broken face says otherwise.
        let width = u16::try_from(metrics.width).unwrap_or(0);
        let height = u16::try_from(metrics.height).unwrap_or(0);
        let coverage = if usize::from(width) * usize::from(height) == coverage.len() { coverage } else { Vec::new() };
        let (width, height) = if coverage.is_empty() { (0, 0) } else { (width, height) };
        Glyph {
            width,
            height,
            left: saturate_i16(metrics.xmin),
            // `ymin` is the mask's bottom row's offset above the baseline.
            top: saturate_i16(metrics.ymin.saturating_add(i32::from(height))),
            advance: to_64ths(metrics.advance_width),
            coverage,
        }
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
