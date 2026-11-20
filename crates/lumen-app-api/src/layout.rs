//! The window calls' fixed layouts (ARCHITECTURE section 8, "Fixed
//! layouts"): [`Canvas`], [`Rect`] and [`TextDraw`], little-endian with no
//! alignment needed, their bytes pinned by tests; and the checks the host
//! makes on them before it touches a pixel ([`Canvas::pixels`],
//! [`commit_count`], [`Rect::clip`]), so the kernel and the tests share one
//! statement of each rule.

use crate::Error;
use crate::limits::{COMMIT_RECTS, TITLE_BYTES};
use core::ops::Range;

/// A [`Canvas`]'s encoded size.
pub const CANVAS_SIZE: usize = 16;
/// A [`Rect`]'s encoded size.
pub const RECT_SIZE: usize = 16;
/// A [`TextDraw`]'s encoded size.
pub const TEXT_DRAW_SIZE: usize = 32;
/// A pixel's bytes (xRGB8888).
pub const PIXEL_BYTES: u32 = 4;

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn i32_at(b: &[u8], at: usize) -> i32 {
    u32_at(b, at) as i32
}

/// Pixels in the app's memory: `w` × `h` xRGB8888 pixels from byte `ptr`,
/// row after row `stride` pixels apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Canvas {
    /// Its first pixel's byte in the app's memory: a multiple of 4.
    pub ptr: u32,
    /// Pixels from one row's start to the next's: at least `w`.
    pub stride: u32,
    pub w: u32,
    pub h: u32,
}

impl Canvas {
    pub fn encode(&self) -> [u8; CANVAS_SIZE] {
        let mut b = [0; CANVAS_SIZE];
        for (i, v) in [self.ptr, self.stride, self.w, self.h].into_iter().enumerate() {
            b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    pub fn decode(b: &[u8; CANVAS_SIZE]) -> Canvas {
        Canvas { ptr: u32_at(b, 0), stride: u32_at(b, 4), w: u32_at(b, 8), h: u32_at(b, 12) }
    }

    /// The bytes its pixels span in a memory `memory` bytes long, checked in
    /// this order: `ptr` a multiple of 4 (else [`Error::BadPointer`]),
    /// `stride` at least `w` ([`Error::BadArgument`]), then every row
    /// inside the memory, with checked arithmetic (an overflow or a byte
    /// past the end is [`Error::BadPointer`]). An empty canvas spans
    /// nothing.
    pub fn pixels(&self, memory: usize) -> Result<Range<usize>, Error> {
        if !self.ptr.is_multiple_of(PIXEL_BYTES) {
            return Err(Error::BadPointer);
        }
        if self.stride < self.w {
            return Err(Error::BadArgument);
        }
        let start = u64::from(self.ptr);
        if self.w == 0 || self.h == 0 {
            return if start <= memory as u64 { Ok(start as usize..start as usize) } else { Err(Error::BadPointer) };
        }
        // The last row's end: (stride × (h − 1) + w) pixels in.
        let pixels = u64::from(self.stride)
            .checked_mul(u64::from(self.h - 1))
            .and_then(|p| p.checked_add(u64::from(self.w)))
            .ok_or(Error::BadPointer)?;
        let end = pixels
            .checked_mul(u64::from(PIXEL_BYTES))
            .and_then(|bytes| start.checked_add(bytes))
            .ok_or(Error::BadPointer)?;
        if end > memory as u64 {
            return Err(Error::BadPointer);
        }
        Ok(start as usize..end as usize)
    }

    /// The byte offset of pixel (`x`, `y`) from [`Canvas::ptr`] (inside the
    /// canvas: no overflow once [`Canvas::pixels`] accepted it).
    pub fn offset(&self, x: u32, y: u32) -> usize {
        (y as usize * self.stride as usize + x as usize) * PIXEL_BYTES as usize
    }
}

/// A rectangle in a canvas's pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// A rectangle cut to a canvas: inside it, never empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clipped {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Clipped {
    /// Its pixels.
    pub fn area(&self) -> u64 {
        u64::from(self.w) * u64::from(self.h)
    }
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn encode(&self) -> [u8; RECT_SIZE] {
        let mut b = [0; RECT_SIZE];
        b[0..4].copy_from_slice(&self.x.to_le_bytes());
        b[4..8].copy_from_slice(&self.y.to_le_bytes());
        b[8..12].copy_from_slice(&self.w.to_le_bytes());
        b[12..16].copy_from_slice(&self.h.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8; RECT_SIZE]) -> Rect {
        Rect { x: i32_at(b, 0), y: i32_at(b, 4), w: u32_at(b, 8), h: u32_at(b, 12) }
    }

    /// The part of it inside a `w` × `h` canvas, or `None` if that is empty
    /// (skipped). Worked out in `i64`, so no corner overflows.
    pub fn clip(&self, w: u32, h: u32) -> Option<Clipped> {
        let x0 = i64::from(self.x).max(0);
        let y0 = i64::from(self.y).max(0);
        let x1 = (i64::from(self.x) + i64::from(self.w)).min(i64::from(w));
        let y1 = (i64::from(self.y) + i64::from(self.h)).min(i64::from(h));
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        Some(Clipped { x: x0 as u32, y: y0 as u32, w: (x1 - x0) as u32, h: (y1 - y0) as u32 })
    }
}

/// `text_draw`'s arguments: where the line box's top left goes, its size and
/// colour, and the pixels it may touch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TextDraw {
    pub x: i32,
    pub y: i32,
    /// Pixels per em: `TEXT_PX_MIN..=TEXT_PX_MAX`.
    pub px: u32,
    /// `0x00RRGGBB` (the top byte ignored).
    pub rgb: u32,
    pub clip: Rect,
}

impl TextDraw {
    pub fn encode(&self) -> [u8; TEXT_DRAW_SIZE] {
        let mut b = [0; TEXT_DRAW_SIZE];
        b[0..4].copy_from_slice(&self.x.to_le_bytes());
        b[4..8].copy_from_slice(&self.y.to_le_bytes());
        b[8..12].copy_from_slice(&self.px.to_le_bytes());
        b[12..16].copy_from_slice(&self.rgb.to_le_bytes());
        b[16..32].copy_from_slice(&self.clip.encode());
        b
    }

    pub fn decode(b: &[u8; TEXT_DRAW_SIZE]) -> TextDraw {
        let mut clip = [0; RECT_SIZE];
        clip.copy_from_slice(&b[16..32]);
        TextDraw { x: i32_at(b, 0), y: i32_at(b, 4), px: u32_at(b, 8), rgb: u32_at(b, 12), clip: Rect::decode(&clip) }
    }
}

/// `window_commit`'s rectangle count: 1..=[`COMMIT_RECTS`], else
/// [`Error::BadArgument`] (checked before the rectangles' range: the count
/// says how many bytes it has). Their bytes.
pub fn commit_count(n: i32) -> Result<usize, Error> {
    match usize::try_from(n) {
        Ok(n) if (1..=COMMIT_RECTS).contains(&n) => Ok(n * RECT_SIZE),
        _ => Err(Error::BadArgument),
    }
}

/// A window title's length: at most [`TITLE_BYTES`], else
/// [`Error::BadArgument`] (checked before the title is copied).
pub fn title_len(len: usize) -> Result<usize, Error> {
    if len > TITLE_BYTES { Err(Error::BadArgument) } else { Ok(len) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_are_pinned() {
        let c = Canvas { ptr: 0x0102_0304, stride: 640, w: 600, h: 0xaabb_ccdd };
        assert_eq!(c.encode(), [4, 3, 2, 1, 0x80, 2, 0, 0, 0x58, 2, 0, 0, 0xdd, 0xcc, 0xbb, 0xaa],);
        assert_eq!(Canvas::decode(&c.encode()), c);
        let r = Rect::new(-1, 2, 3, u32::MAX);
        assert_eq!(r.encode(), [0xff, 0xff, 0xff, 0xff, 2, 0, 0, 0, 3, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(Rect::decode(&r.encode()), r);
        let t = TextDraw { x: 5, y: -6, px: 16, rgb: 0xff12_3456, clip: r };
        let b = t.encode();
        assert_eq!(&b[..16], &[5, 0, 0, 0, 0xfa, 0xff, 0xff, 0xff, 16, 0, 0, 0, 0x56, 0x34, 0x12, 0xff]);
        assert_eq!(&b[16..], &r.encode());
        assert_eq!(TextDraw::decode(&b), t);
        assert_eq!((CANVAS_SIZE, RECT_SIZE, TEXT_DRAW_SIZE), (16, 16, 32));
    }

    #[test]
    fn a_canvas_inside_the_memory_spans_its_rows() {
        let c = Canvas { ptr: 64, stride: 10, w: 8, h: 3 };
        // Two whole strides, then the last row's 8 pixels.
        assert_eq!(c.pixels(1 << 20), Ok(64..64 + (2 * 10 + 8) * 4));
        // Ending exactly at the memory's end.
        assert_eq!(c.pixels(64 + 28 * 4), Ok(64..64 + 28 * 4));
        assert_eq!(c.pixels(64 + 28 * 4 - 1), Err(Error::BadPointer));
        assert_eq!(c.offset(2, 1), (10 + 2) * 4);
        // Stride equal to the width: rows back to back.
        assert_eq!(Canvas { ptr: 0, stride: 4, w: 4, h: 2 }.pixels(32), Ok(0..32));
    }

    #[test]
    fn canvas_checks_refuse_in_their_order() {
        // Unaligned, even with a stride below the width: a bad pointer.
        for ptr in [1, 2, 3, 0xffff_fffd] {
            assert_eq!(Canvas { ptr, stride: 1, w: 2, h: 1 }.pixels(1 << 20), Err(Error::BadPointer), "{ptr}");
        }
        // A stride below the width.
        assert_eq!(Canvas { ptr: 0, stride: 7, w: 8, h: 1 }.pixels(1 << 20), Err(Error::BadArgument));
        // Stride × height overflowing 64 bits' worth of bytes, and 32 bits'.
        let huge = Canvas { ptr: 0, stride: u32::MAX, w: u32::MAX, h: u32::MAX };
        assert_eq!(huge.pixels(usize::MAX), Err(Error::BadPointer));
        let wraps32 = Canvas { ptr: 0xffff_fff0, stride: 0x4000_0000, w: 1, h: 2 };
        assert_eq!(wraps32.pixels(1 << 32), Err(Error::BadPointer));
        // Past the end.
        assert_eq!(Canvas { ptr: 1 << 20, stride: 1, w: 1, h: 1 }.pixels(1 << 20), Err(Error::BadPointer));
        // Empty: spans nothing, but its start must be inside.
        assert_eq!(Canvas { ptr: 16, stride: 0, w: 0, h: 5 }.pixels(16), Ok(16..16));
        assert_eq!(Canvas { ptr: 20, stride: 9, w: 9, h: 0 }.pixels(16), Err(Error::BadPointer));
    }

    #[test]
    fn rects_are_clipped_to_the_canvas() {
        let (w, h) = (100, 50);
        assert_eq!(Rect::new(10, 5, 20, 10).clip(w, h), Some(Clipped { x: 10, y: 5, w: 20, h: 10 }));
        // Cut on every side.
        assert_eq!(Rect::new(-5, -5, 200, 200).clip(w, h), Some(Clipped { x: 0, y: 0, w: 100, h: 50 }));
        assert_eq!(Rect::new(90, 45, 20, 20).clip(w, h), Some(Clipped { x: 90, y: 45, w: 10, h: 5 }));
        // Empty, outside, or touching only an edge: skipped.
        for r in [
            Rect::new(0, 0, 0, 10),
            Rect::new(0, 0, 10, 0),
            Rect::new(100, 0, 10, 10),
            Rect::new(0, 50, 10, 10),
            Rect::new(-10, 0, 10, 10),
            Rect::new(i32::MIN, i32::MIN, u32::MAX, 1),
        ] {
            assert_eq!(r.clip(w, h), None, "{r:?}");
        }
        // Corners far past i32: no overflow.
        assert_eq!(Rect::new(i32::MAX, i32::MAX, u32::MAX, u32::MAX).clip(w, h), None);
        assert_eq!(Rect::new(i32::MIN, 0, u32::MAX, 1).clip(w, h).map(|c| c.w), Some(100));
        assert_eq!(Clipped { x: 0, y: 0, w: 3, h: 4 }.area(), 12);
    }

    #[test]
    fn commit_counts_and_titles_have_caps() {
        assert_eq!(commit_count(1), Ok(RECT_SIZE));
        assert_eq!(commit_count(COMMIT_RECTS as i32), Ok(COMMIT_RECTS * RECT_SIZE));
        for n in [0, -1, COMMIT_RECTS as i32 + 1, i32::MAX, i32::MIN] {
            assert_eq!(commit_count(n), Err(Error::BadArgument), "{n}");
        }
        assert_eq!(title_len(0), Ok(0));
        assert_eq!(title_len(TITLE_BYTES), Ok(TITLE_BYTES));
        assert_eq!(title_len(TITLE_BYTES + 1), Err(Error::BadArgument));
    }
}
