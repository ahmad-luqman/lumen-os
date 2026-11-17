//! Surfaces: owned xRGB8888 pixel buffers, what they're presented to, and
//! coverage-mask blending.
//!
//! A pixel is a `u32`, `0xXXRRGGBB`: blue in the low byte, the top byte
//! unused by the display (the framebuffer's format on q35's GOP). A
//! [`Surface`]'s rows are `stride` pixels apart, at least `width`; the
//! pixels between a row's end and the next row are never written.
//!
//! Drawing is clipped: any part of a rectangle, blit or mask outside the
//! surface is skipped, so callers can draw at negative or oversized
//! positions (a window dragged half off the screen).

use alloc::vec;
use alloc::vec::Vec;

use crate::rect::Rect;

/// Something pixels are presented to, a horizontal span at a time: a
/// [`Surface`], or the kernel's front buffer (the framebuffer, which is
/// write-combined, so whole spans are what it wants).
pub trait Target {
    /// Width and height in pixels.
    fn size(&self) -> (u32, u32);

    /// Write `pixels` in row `y` from column `x` on. This crate only calls
    /// it with spans inside [`Target::size`]; an implementation must not
    /// write outside itself for one that isn't (it may panic).
    fn write_span(&mut self, x: u32, y: u32, pixels: &[u32]);
}

/// An 8-bit coverage mask, `width` x `height`, row by row with no padding:
/// 0 leaves a pixel alone, 255 paints it, anything between blends (an
/// anti-aliased glyph from the font rasterizer).
#[derive(Clone, Copy, Debug)]
pub struct Mask<'a> {
    width: u32,
    height: u32,
    coverage: &'a [u8],
}

impl<'a> Mask<'a> {
    /// The mask over `coverage`, or `None` unless it holds exactly
    /// `width * height` bytes.
    pub fn new(width: u32, height: u32, coverage: &'a [u8]) -> Option<Mask<'a>> {
        let len = (width as usize).checked_mul(height as usize)?;
        (coverage.len() == len).then_some(Mask { width, height, coverage })
    }

    /// Width in pixels.
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub const fn height(&self) -> u32 {
        self.height
    }
}

/// One 8-bit channel of `src` drawn over `dst` with coverage `alpha`:
/// `(src * alpha + dst * (255 - alpha)) / 255`, rounded to the nearest
/// integer (a tie can't happen: 255 is odd). So `alpha` 0 gives exactly
/// `dst`, 255 exactly `src`, and `blend_channel(s, d, a) ==
/// blend_channel(d, s, 255 - a)`.
pub const fn blend_channel(src: u8, dst: u8, alpha: u8) -> u8 {
    let a = alpha as u32;
    let sum = src as u32 * a + dst as u32 * (255 - a);
    // sum <= 255 * 255, so the quotient fits a u8.
    ((sum + 127) / 255) as u8
}

/// The pixel `src` drawn over `dst` with coverage `alpha`: each of the four
/// bytes (the unused top one too) through [`blend_channel`].
pub const fn blend(src: u32, dst: u32, alpha: u8) -> u32 {
    let s = src.to_le_bytes();
    let d = dst.to_le_bytes();
    u32::from_le_bytes([
        blend_channel(s[0], d[0], alpha),
        blend_channel(s[1], d[1], alpha),
        blend_channel(s[2], d[2], alpha),
        blend_channel(s[3], d[3], alpha),
    ])
}

/// An owned `width` x `height` xRGB8888 buffer, rows `stride` pixels
/// apart.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Surface {
    width: u32,
    height: u32,
    stride: usize,
    pixels: Vec<u32>,
}

impl Surface {
    /// A `width` x `height` surface of black (0) pixels, rows packed.
    pub fn new(width: u32, height: u32) -> Surface {
        Surface::with_stride(width, height, width as usize).expect("a stride of the width fits")
    }

    /// A `width` x `height` surface of black pixels with rows `stride`
    /// pixels apart, or `None` if `stride < width` or the buffer's size
    /// overflows.
    pub fn with_stride(width: u32, height: u32, stride: usize) -> Option<Surface> {
        if stride < width as usize {
            return None;
        }
        let len = stride.checked_mul(height as usize)?;
        Some(Surface { width, height, stride, pixels: vec![0; len] })
    }

    /// Width in pixels.
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Pixels from one row's start to the next's.
    pub const fn stride(&self) -> usize {
        self.stride
    }

    /// The whole surface as a rectangle at the origin.
    pub const fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width, self.height)
    }

    /// Row `y`'s `width` pixels. Panics if `y` is outside the surface.
    pub fn row(&self, y: u32) -> &[u32] {
        assert!(y < self.height, "row {y} of {}", self.height);
        let start = y as usize * self.stride;
        &self.pixels[start..start + self.width as usize]
    }

    /// Row `y`'s `width` pixels, to change. Panics if `y` is outside.
    pub fn row_mut(&mut self, y: u32) -> &mut [u32] {
        assert!(y < self.height, "row {y} of {}", self.height);
        let start = y as usize * self.stride;
        &mut self.pixels[start..start + self.width as usize]
    }

    /// The pixel at (`x`, `y`), or `None` outside the surface.
    pub fn pixel(&self, x: i32, y: i32) -> Option<u32> {
        self.bounds().contains(x, y).then(|| self.pixels[y as usize * self.stride + x as usize])
    }

    /// Set the pixel at (`x`, `y`); nothing happens outside the surface.
    pub fn set_pixel(&mut self, x: i32, y: i32, color: u32) {
        if self.bounds().contains(x, y) {
            self.pixels[y as usize * self.stride + x as usize] = color;
        }
    }

    /// Paint the part of `r` inside the surface `color`.
    pub fn fill_rect(&mut self, r: Rect, color: u32) {
        let Some(r) = r.clip(self.width, self.height) else { return };
        let (x0, x1) = (r.x() as usize, r.right() as usize);
        for y in r.y() as u32..r.bottom() as u32 {
            self.row_mut(y)[x0..x1].fill(color);
        }
    }

    /// Paint the whole surface `color`.
    pub fn fill(&mut self, color: u32) {
        self.fill_rect(self.bounds(), color);
    }

    /// Copy the pixels of `src_rect` (in this surface's coordinates) to
    /// `dst`, its top-left corner landing at (`dst_x`, `dst_y`). Clipped on
    /// both sides: the part of `src_rect` outside this surface is skipped,
    /// and so is whatever would land outside `dst`; every pixel copied
    /// keeps its offset from `src_rect`'s corner.
    pub fn copy_to(&self, src_rect: Rect, dst: &mut impl Target, dst_x: i32, dst_y: i32) {
        let Some(s) = src_rect.intersect(&self.bounds()) else { return };
        // A source pixel (x, y) lands at (x + ox, y + oy).
        let ox = dst_x as i64 - src_rect.x() as i64;
        let oy = dst_y as i64 - src_rect.y() as i64;
        let (tw, th) = dst.size();
        let x0 = (s.x() as i64 + ox).max(0);
        let x1 = (s.right() as i64 + ox).min(tw as i64);
        let y0 = (s.y() as i64 + oy).max(0);
        let y1 = (s.bottom() as i64 + oy).min(th as i64);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let sx0 = (x0 - ox) as usize;
        let len = (x1 - x0) as usize;
        for y in y0..y1 {
            let row = self.row((y - oy) as u32);
            dst.write_span(x0 as u32, y as u32, &row[sx0..sx0 + len]);
        }
    }

    /// Copy `src_rect` of `src` into this surface at (`dst_x`, `dst_y`),
    /// clipped on both sides ([`Surface::copy_to`]).
    pub fn blit(&mut self, src: &Surface, src_rect: Rect, dst_x: i32, dst_y: i32) {
        src.copy_to(src_rect, self, dst_x, dst_y);
    }

    /// Present the damaged rectangles of this surface to `dst` at the same
    /// coordinates (a back buffer to the front buffer), each clipped to
    /// both.
    pub fn present(&self, damage: &[Rect], dst: &mut impl Target) {
        for r in damage {
            self.copy_to(*r, dst, r.x(), r.y());
        }
    }

    /// Draw `mask` in `color` with its top-left corner at (`x`, `y`):
    /// each pixel becomes [`blend`]`(color, pixel, coverage)`. The part of
    /// the mask outside the surface is skipped.
    pub fn blend_mask(&mut self, mask: &Mask<'_>, x: i32, y: i32, color: u32) {
        let at = Rect::new(x, y, mask.width, mask.height);
        let Some(c) = at.clip(self.width, self.height) else { return };
        // Mask column and row of the clipped part's corner.
        let mx0 = (c.x() as i64 - x as i64) as usize;
        let my0 = (c.y() as i64 - y as i64) as usize;
        let mw = mask.width as usize;
        let (dx0, len) = (c.x() as usize, c.w() as usize);
        for row in 0..c.h() as usize {
            let start = (my0 + row) * mw + mx0;
            let cov = &mask.coverage[start..start + len];
            let px = &mut self.row_mut(c.y() as u32 + row as u32)[dx0..dx0 + len];
            for (p, &a) in px.iter_mut().zip(cov) {
                match a {
                    0 => {}
                    255 => *p = color,
                    a => *p = blend(color, *p, a),
                }
            }
        }
    }
}

impl Target for Surface {
    fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn write_span(&mut self, x: u32, y: u32, pixels: &[u32]) {
        let x = x as usize;
        self.row_mut(y)[x..x + pixels.len()].copy_from_slice(pixels);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A surface whose every pixel is distinct: `seed` plus its index.
    fn numbered(w: u32, h: u32, seed: u32) -> Surface {
        let mut s = Surface::new(w, h);
        for y in 0..h {
            for x in 0..w {
                s.set_pixel(x as i32, y as i32, seed + y * w + x);
            }
        }
        s
    }

    #[test]
    fn blend_channel_is_exactly_rounded_everywhere() {
        for a in 0..=255u32 {
            for s in 0..=255u32 {
                for d in 0..=255u32 {
                    let got = blend_channel(s as u8, d as u8, a as u8) as u32;
                    let sum = s * a + d * (255 - a);
                    // Within half a step of the exact value, |255*got - sum|
                    // <= 127: the nearest integer (unique, as 255 is odd).
                    assert!((got * 255).abs_diff(sum) <= 127, "s={s} d={d} a={a}: {got}");
                }
            }
        }
    }

    #[test]
    fn blend_rounding_at_the_ends_and_midpoints() {
        for v in [0u8, 1, 127, 128, 254, 255] {
            for w in [0u8, 1, 127, 128, 254, 255] {
                assert_eq!(blend_channel(v, w, 0), w);
                assert_eq!(blend_channel(v, w, 255), v);
                for a in [1u8, 127, 128, 254] {
                    assert_eq!(blend_channel(v, w, a), blend_channel(w, v, 255 - a));
                }
            }
        }
        // White over black at the midpoints: 128*255/255 = 128 exactly,
        // 127 likewise.
        assert_eq!(blend_channel(255, 0, 128), 128);
        assert_eq!(blend_channel(255, 0, 127), 127);
        assert_eq!(blend_channel(0, 255, 128), 127);
        assert_eq!(blend_channel(0, 255, 127), 128);
        // 1 over 0: 128/255 = 0.502 rounds up, 127/255 = 0.498 down.
        assert_eq!(blend_channel(1, 0, 128), 1);
        assert_eq!(blend_channel(1, 0, 127), 0);
        // 200*128 + 100*127 = 38300; /255 = 150.196.
        assert_eq!(blend_channel(200, 100, 128), 150);
        // 254 over 0 at 128: 32512/255 = 127.498, down.
        assert_eq!(blend_channel(254, 0, 128), 127);
        // 255 over 1 at 1: (255 + 254)/255 = 1.996, up.
        assert_eq!(blend_channel(255, 1, 1), 2);
    }

    #[test]
    fn blend_works_per_byte() {
        let (src, dst) = (0x40_ff_80_00, 0x00_00_80_ff);
        assert_eq!(blend(src, dst, 0), dst);
        assert_eq!(blend(src, dst, 255), src);
        let mid = blend(src, dst, 128);
        let expect = u32::from_le_bytes([
            blend_channel(0x00, 0xff, 128),
            blend_channel(0x80, 0x80, 128),
            blend_channel(0xff, 0x00, 128),
            blend_channel(0x40, 0x00, 128),
        ]);
        assert_eq!(mid, expect);
        assert_eq!(mid, 0x20_80_80_7f);
    }

    #[test]
    fn new_with_stride_and_bounds() {
        let s = Surface::new(3, 2);
        assert_eq!((s.width(), s.height(), s.stride()), (3, 2, 3));
        assert_eq!(s.bounds(), Rect::new(0, 0, 3, 2));
        assert_eq!(s.size(), (3, 2));
        assert!(Surface::with_stride(4, 2, 3).is_none());
        assert!(Surface::with_stride(4, 2, usize::MAX).is_none());
        let p = Surface::with_stride(3, 2, 5).unwrap();
        assert_eq!(p.stride(), 5);
        assert_eq!(p.row(1).len(), 3);
        let empty = Surface::new(0, 0);
        assert!(empty.bounds().is_empty());
        assert_eq!(empty.pixel(0, 0), None);
    }

    #[test]
    fn pixels_inside_and_outside() {
        let mut s = numbered(4, 3, 100);
        assert_eq!(s.pixel(0, 0), Some(100));
        assert_eq!(s.pixel(3, 2), Some(111));
        for (x, y) in [(-1, 0), (0, -1), (4, 0), (0, 3), (i32::MIN, i32::MAX)] {
            assert_eq!(s.pixel(x, y), None);
            let before = s.clone();
            s.set_pixel(x, y, 7);
            assert_eq!(s, before);
        }
        s.set_pixel(2, 1, 7);
        assert_eq!(s.pixel(2, 1), Some(7));
    }

    #[test]
    #[should_panic(expected = "row 3 of 3")]
    fn row_outside_panics() {
        Surface::new(2, 3).row(3);
    }

    /// `fill_rect` against a per-pixel model, for rectangles hanging over
    /// every edge and corner, outside, and empty.
    #[test]
    fn fill_rect_is_clipped_at_every_edge() {
        let (w, h) = (6u32, 5u32);
        for x in -8..9 {
            for y in -7..8 {
                for (rw, rh) in [(0, 3), (3, 0), (1, 1), (3, 2), (7, 6), (20, 20)] {
                    let r = Rect::new(x, y, rw, rh);
                    let mut s = numbered(w, h, 1000);
                    let model = numbered(w, h, 1000);
                    s.fill_rect(r, 0xdead);
                    for py in 0..h as i32 {
                        for px in 0..w as i32 {
                            let want = if r.contains(px, py) { 0xdead } else { model.pixel(px, py).unwrap() };
                            assert_eq!(s.pixel(px, py), Some(want), "{r:?} at {px},{py}");
                        }
                    }
                }
            }
        }
        let mut s = Surface::new(2, 2);
        s.fill(5);
        assert!((0..2).all(|y| s.row(y) == [5, 5]));
    }

    /// Clipped blits from every offset around a small surface, with source
    /// rectangles inside, overlapping and outside the source: every
    /// destination pixel is either the source pixel at the same offset
    /// from `src_rect`'s corner (when that pixel is in both `src_rect` and
    /// the source) or untouched.
    #[test]
    fn blit_is_clipped_at_every_edge() {
        let src = numbered(5, 4, 1);
        let (dw, dh) = (6u32, 5u32);
        let rects = [
            src.bounds(),
            Rect::new(1, 1, 2, 2),
            Rect::new(-2, -1, 4, 3),
            Rect::new(3, 2, 10, 10),
            Rect::new(-3, -3, 20, 20),
            Rect::new(5, 0, 2, 2),
            Rect::new(0, 0, 0, 3),
        ];
        for sr in rects {
            for dx in -8..10 {
                for dy in -7..9 {
                    let mut dst = numbered(dw, dh, 5000);
                    dst.blit(&src, sr, dx, dy);
                    for py in 0..dh as i32 {
                        for px in 0..dw as i32 {
                            let (sx, sy) = (px - dx + sr.x(), py - dy + sr.y());
                            let want = if sr.contains(sx, sy) && src.bounds().contains(sx, sy) {
                                src.pixel(sx, sy).unwrap()
                            } else {
                                5000 + py as u32 * dw + px as u32
                            };
                            assert_eq!(dst.pixel(px, py), Some(want), "{sr:?} to {dx},{dy}: at {px},{py}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn padding_past_the_width_is_never_written() {
        let mut s = Surface::with_stride(3, 3, 5).unwrap();
        s.fill(9);
        s.fill_rect(Rect::new(-10, -10, 100, 100), 8);
        let src = numbered(4, 4, 1);
        s.blit(&src, src.bounds(), -1, 0);
        let coverage = [255u8; 16];
        s.blend_mask(&Mask::new(4, 4, &coverage).unwrap(), 0, -1, 3);
        for y in 0..3 {
            assert_eq!(&s.pixels[y * 5 + 3..y * 5 + 5], &[0, 0], "row {y}'s padding");
        }
        assert_eq!(s.row(0), &[3, 3, 3]);
    }

    /// A target that records every span and panics on one outside it.
    struct Spans {
        w: u32,
        h: u32,
        spans: Vec<(u32, u32, Vec<u32>)>,
    }

    impl Target for Spans {
        fn size(&self) -> (u32, u32) {
            (self.w, self.h)
        }
        fn write_span(&mut self, x: u32, y: u32, pixels: &[u32]) {
            assert!(y < self.h && x as usize + pixels.len() <= self.w as usize, "span {x},{y}+{}", pixels.len());
            assert!(!pixels.is_empty(), "an empty span");
            self.spans.push((x, y, pixels.to_vec()));
        }
    }

    #[test]
    fn present_writes_damaged_spans_at_the_same_place() {
        let back = numbered(8, 6, 0);
        let mut front = Spans { w: 8, h: 6, spans: Vec::new() };
        back.present(&[Rect::new(1, 2, 3, 2), Rect::new(-2, 5, 4, 9), Rect::new(8, 0, 1, 1)], &mut front);
        assert_eq!(front.spans, vec![(1, 2, vec![17, 18, 19]), (1, 3, vec![25, 26, 27]), (0, 5, vec![40, 41])]);
        // A smaller target clips the spans, never overruns it.
        let mut small = Spans { w: 3, h: 3, spans: Vec::new() };
        back.present(&[back.bounds()], &mut small);
        assert_eq!(small.spans, vec![(0, 0, vec![0, 1, 2]), (0, 1, vec![8, 9, 10]), (0, 2, vec![16, 17, 18])]);
    }

    #[test]
    fn mask_rejects_the_wrong_length() {
        assert!(Mask::new(3, 2, &[0; 6]).is_some());
        assert!(Mask::new(3, 2, &[0; 5]).is_none());
        assert!(Mask::new(3, 2, &[0; 7]).is_none());
        assert!(Mask::new(0, 9, &[]).is_some());
        let m = Mask::new(3, 2, &[0; 6]).unwrap();
        assert_eq!((m.width(), m.height()), (3, 2));
    }

    /// `blend_mask` against a per-pixel model at every offset around the
    /// surface: inside, each pixel is `blend(color, old, coverage)` for
    /// the mask byte at its offset; 0 leaves it and 255 paints it.
    #[test]
    fn blend_mask_is_clipped_and_blends_each_pixel() {
        let (mw, mh) = (4u32, 3u32);
        let coverage: Vec<u8> = vec![0, 1, 127, 128, 254, 255, 64, 0, 255, 200, 0, 32];
        let mask = Mask::new(mw, mh, &coverage).unwrap();
        let color = 0x00_ff_80_10;
        let (w, h) = (5u32, 4u32);
        for x in -6..7 {
            for y in -5..6 {
                let model = numbered(w, h, 0x0040_2000);
                let mut s = model.clone();
                s.blend_mask(&mask, x, y, color);
                for py in 0..h as i32 {
                    for px in 0..w as i32 {
                        let old = model.pixel(px, py).unwrap();
                        let (mx, my) = (px - x, py - y);
                        let want = if (0..mw as i32).contains(&mx) && (0..mh as i32).contains(&my) {
                            blend(color, old, coverage[(my as u32 * mw + mx as u32) as usize])
                        } else {
                            old
                        };
                        assert_eq!(s.pixel(px, py), Some(want), "mask at {x},{y}: pixel {px},{py}");
                    }
                }
            }
        }
    }
}
