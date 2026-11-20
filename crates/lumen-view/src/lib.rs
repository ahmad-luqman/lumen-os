//! The image viewer's logic (#204; `apps/viewer` is its window): where an
//! image goes in the view, at 1:1 (panned, or centred when it is smaller
//! than the view) or fitted to it (scaled by nearest neighbour, keeping its
//! shape, centred), and the app's memory plan. Host-tested, `no_std` with
//! `alloc`.
//!
//! - **The window:** [`WINDOW_W`] x [`WINDOW_H`] client pixels (the most
//!   `WINDOW_PIXELS` allows), a panel of widgets on the left
//!   ([`PANEL_W`] wide) and the view, [`VIEW`], the rest.
//! - **1:1** ([`actual`]): an axis on which the image is bigger than the
//!   view shows the part from the pan ([`Pan`], moved in steps of
//!   [`PAN_STEP`] and clamped so the view stays filled); an axis on which
//!   it is smaller shows all of it, centred.
//! - **Fit** ([`fitted`], [`scale`]): the biggest size with the image's
//!   shape that fits the view (smaller images are enlarged too), each pixel
//!   of it taken from the source pixel [`source`] names.
//! - **What it decodes** ([`admit`]): read from the PNG's header before
//!   anything is inflated, an image at most [`MAX_IMAGE_W`] x
//!   [`MAX_IMAGE_H`] whose inflated rows (the decoder's biggest buffer) are
//!   at most [`MAX_ROW_BYTES`] in all: 8-bit RGB (the wallpaper's format)
//!   at the biggest size, so an 8-bit RGBA image is taken at three quarters
//!   of that many pixels, a 16-bit RGBA one at three eighths. The decoder's
//!   work, and so the memory plan, is then bounded whatever a file says.
//! - **Showing one** ([`prepare`]): [`admit`], `lumen-png`'s decoder, and
//!   the fitted copy, as the app does it, both held as surfaces; or why not
//!   ([`Why`]: refused, the decoder's error, or out of memory, which is the
//!   plan failing, not the file).
//! - **Memory** ([`MEMORY_BYTES`]): the app's fixed memory, whose heap part
//!   ([`HEAP_BYTES`]) `tests/memory.rs` checks the app's allocations fit,
//!   running [`prepare`] through the app's own allocator logic
//!   (`lumen-heap`) over a region of that size, for the bundled images and
//!   the costliest ones [`admit`] takes.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

pub use lumen_gfx::{Rect, Surface};
pub use lumen_png::decode::DecodeError;
use lumen_png::decode::decode;

/// The window's client size.
pub const WINDOW_W: u32 = 640;
pub const WINDOW_H: u32 = 480;
/// The panel of widgets on the left (the image list, the buttons, the
/// label): the toolkit's area.
pub const PANEL_W: u32 = 200;
/// The panel: the client left of the view.
pub const PANEL: Rect = Rect::new(0, 0, PANEL_W, WINDOW_H);
/// The view: the client right of the panel.
pub const VIEW: Rect = Rect::new(PANEL_W as i32, 0, WINDOW_W - PANEL_W, WINDOW_H);
/// How far one arrow key moves a 1:1 view.
pub const PAN_STEP: u32 = 40;

/// The biggest image it decodes (an image's header asking for more is
/// refused before any of it is inflated): the wallpaper's size.
pub const MAX_IMAGE_W: u32 = 1280;
pub const MAX_IMAGE_H: u32 = 800;
/// The most bytes an image's inflated rows may take (each row its filter
/// byte and its samples): 8-bit RGB at [`MAX_IMAGE_W`] x [`MAX_IMAGE_H`]. The
/// decoder holds about three buffers of that size at once for an interlaced
/// image (the inflated rows, the deinterlaced samples, the samples handed
/// back), then the pixels (4 bytes each): `tests/memory.rs` measures it.
pub const MAX_ROW_BYTES: u64 = (1 + 3 * MAX_IMAGE_W as u64) * MAX_IMAGE_H as u64;

/// The longest file it decodes (`lumen-png` and the decoder each copy a
/// file's image data once): the module's data bound, so a bundled image is
/// always within it.
pub const MAX_FILE_BYTES: usize = DATA_BYTES;

/// The app's fixed memory (its `build.rs` sets it; the app checks it at
/// its start), a multiple of 64 KiB: the stack, the module's data, and the
/// heap.
pub const MEMORY_BYTES: usize = 16 * 1024 * 1024;
/// wasm-ld's stack, at the bottom of the memory.
pub const STACK_BYTES: usize = 1024 * 1024;
/// The most the module's data (the bundled images, the strings) may take,
/// after the stack (the app checks its heap starts below `STACK_BYTES +
/// DATA_BYTES`).
pub const DATA_BYTES: usize = 512 * 1024;
/// The heap: the rest, where the canvas, the decoder's buffers, the image
/// shown and its fitted copy live.
pub const HEAP_BYTES: usize = MEMORY_BYTES - STACK_BYTES - DATA_BYTES;

const _: () =
    assert!(MEMORY_BYTES.is_multiple_of(65536) && MEMORY_BYTES <= lumen_app_api::limits::MEMORY_PAGES as usize * 65536);
const _: () = assert!(WINDOW_W as u64 * WINDOW_H as u64 <= lumen_app_api::limits::WINDOW_PIXELS);

/// Why [`admit`] refused an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Longer than [`MAX_FILE_BYTES`] (checked first, before the header).
    TooLong { bytes: usize },
    /// No PNG signature and IHDR at its start (the decoder says the same).
    NoHeader,
    /// A colour type and bit depth PNG doesn't define.
    BadFormat { colour: u8, depth: u8 },
    /// No pixels: a width or height of 0.
    TooSmall { w: u32, h: u32 },
    /// Wider or taller than [`MAX_IMAGE_W`] x [`MAX_IMAGE_H`].
    TooBig { w: u32, h: u32 },
    /// Its inflated rows would take more than [`MAX_ROW_BYTES`].
    TooCostly { w: u32, h: u32, bytes: u64 },
}

impl core::fmt::Display for Refused {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Refused::TooLong { bytes } => write!(f, "{bytes} bytes, more than the {MAX_FILE_BYTES} it takes"),
            Refused::NoHeader => f.write_str("not a PNG (no header)"),
            Refused::BadFormat { colour, depth } => write!(f, "colour type {colour} at {depth} bits isn't PNG's"),
            Refused::TooSmall { w, h } => write!(f, "{w}x{h} pixels: no image"),
            Refused::TooBig { w, h } => {
                write!(f, "{w}x{h} pixels, more than the {MAX_IMAGE_W}x{MAX_IMAGE_H} it shows")
            }
            Refused::TooCostly { w, h, bytes } => {
                write!(f, "{w}x{h} pixels need {bytes} bytes inflated, more than the {MAX_ROW_BYTES} it takes")
            }
        }
    }
}

/// Samples a pixel has for PNG colour type `colour` at bit depth `depth`
/// (the combinations PNG defines), else `None`.
fn samples(colour: u8, depth: u8) -> Option<u64> {
    match (colour, depth) {
        (0, 1 | 2 | 4 | 8 | 16) => Some(1),
        (3, 1 | 2 | 4 | 8) => Some(1),
        (2, 8 | 16) => Some(3),
        (4, 8 | 16) => Some(2),
        (6, 8 | 16) => Some(4),
        _ => None,
    }
}

/// The bytes a `w` x `h` image's inflated rows take, `per_pixel` samples of
/// `depth` bits a pixel: each row a filter byte and its samples, rounded up
/// to a whole byte.
fn inflated(w: u32, h: u32, per_pixel: u64, depth: u8) -> u64 {
    (1 + (u64::from(w) * per_pixel * u64::from(depth)).div_ceil(8)) * u64::from(h)
}

/// Whether the viewer decodes `png`, from its length and its header alone
/// (before anything is inflated): its size, if so. Both bound the decoder's
/// work: the length its copies of the file's data (`lumen-png` copies the
/// image's chunks, and the decoder gathers the IDAT data again), the header
/// its buffers. The inflated rows are counted as for an
/// image that isn't interlaced (an interlaced image's seven passes add at
/// most a filter byte a row of each, which the memory test's interlaced
/// worst case includes).
pub fn admit(png: &[u8]) -> Result<(u32, u32), Refused> {
    if png.len() > MAX_FILE_BYTES {
        return Err(Refused::TooLong { bytes: png.len() });
    }
    let be32 = |at: usize| png.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    if png.get(..8) != Some(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'][..])
        || png.get(12..16) != Some(b"IHDR")
    {
        return Err(Refused::NoHeader);
    }
    let (Some(w), Some(h), Some(&depth), Some(&colour)) = (be32(16), be32(20), png.get(24), png.get(25)) else {
        return Err(Refused::NoHeader);
    };
    let per_pixel = samples(colour, depth).ok_or(Refused::BadFormat { colour, depth })?;
    if w == 0 || h == 0 {
        return Err(Refused::TooSmall { w, h });
    }
    if w > MAX_IMAGE_W || h > MAX_IMAGE_H {
        return Err(Refused::TooBig { w, h });
    }
    let bytes = inflated(w, h, per_pixel, depth);
    if bytes > MAX_ROW_BYTES {
        return Err(Refused::TooCostly { w, h, bytes });
    }
    Ok((w, h))
}

/// Where a 1:1 view is panned to: the image's pixel at the view's top left
/// on each axis on which the image is bigger than the view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pan {
    x: u32,
    y: u32,
}

impl Pan {
    /// Moved by (`dx`, `dy`) steps of [`PAN_STEP`], clamped to what an
    /// `image_w` x `image_h` image allows in `view` (the view stays
    /// filled): whether it moved.
    pub fn by(&mut self, dx: i32, dy: i32, image_w: u32, image_h: u32, view: Rect) -> bool {
        let before = *self;
        self.x = step(self.x, dx, image_w.saturating_sub(view.w()));
        self.y = step(self.y, dy, image_h.saturating_sub(view.h()));
        *self != before
    }
}

/// `at` moved by `steps` of [`PAN_STEP`], within `0..=max`.
fn step(at: u32, steps: i32, max: u32) -> u32 {
    let to = i64::from(at.min(max)) + i64::from(steps) * i64::from(PAN_STEP);
    to.clamp(0, i64::from(max)) as u32
}

/// What a 1:1 view shows: the view's part `dst` holds the image's pixels
/// from (`src_x`, `src_y`), `dst.w` x `dst.h` of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed {
    pub dst: Rect,
    pub src_x: u32,
    pub src_y: u32,
}

/// An `image_w` x `image_h` image at 1:1 in `view`, panned to `pan`
/// (clamped): per axis, the part from the pan when the image is bigger
/// than the view, all of it centred when it isn't.
pub fn actual(image_w: u32, image_h: u32, view: Rect, pan: Pan) -> Placed {
    let (x, src_x, w) = axis(image_w, view.w(), pan.x);
    let (y, src_y, h) = axis(image_h, view.h(), pan.y);
    Placed { dst: Rect::new(view.x() + x as i32, view.y() + y as i32, w, h), src_x, src_y }
}

/// One axis of [`actual`]: (the offset in the view, the first source
/// pixel, how many show).
fn axis(image: u32, view: u32, pan: u32) -> (u32, u32, u32) {
    if image <= view { ((view - image) / 2, 0, image) } else { (0, pan.min(image - view), view) }
}

/// The biggest `w` x `h` with an `image_w` x `image_h` image's shape that
/// fits `view_w` x `view_h` (at least 1 x 1; rounded down).
fn fit_size(image_w: u32, image_h: u32, view_w: u32, view_h: u32) -> (u32, u32) {
    let (iw, ih, vw, vh) = (u64::from(image_w.max(1)), u64::from(image_h.max(1)), u64::from(view_w), u64::from(view_h));
    let (w, h) = if iw * vh <= ih * vw {
        // The height decides.
        (iw * vh / ih, vh)
    } else {
        (vw, ih * vw / iw)
    };
    (w.max(1) as u32, h.max(1) as u32)
}

/// Where an `image_w` x `image_h` image fitted to `view` goes: centred
/// (nowhere, `Rect::EMPTY`, in an empty view).
pub fn fitted(image_w: u32, image_h: u32, view: Rect) -> Rect {
    if view.is_empty() {
        return Rect::EMPTY;
    }
    let (w, h) = fit_size(image_w, image_h, view.w(), view.h());
    Rect::new(view.x() + ((view.w() - w) / 2) as i32, view.y() + ((view.h() - h) / 2) as i32, w, h)
}

/// The source pixel (on one axis) that pixel `d` of a `dst_len` long scaled
/// copy of a `src_len` long axis takes: `d × src_len / dst_len`, rounded
/// down (nearest neighbour; the first pixel from the first, never past the
/// last).
fn source(d: u32, dst_len: u32, src_len: u32) -> u32 {
    (u64::from(d) * u64::from(src_len) / u64::from(dst_len.max(1))) as u32
}

/// `src` (`src_w` x `src_h` pixels, row after row) scaled to `dst_w` x
/// `dst_h` by [`source`]; `None` if the heap refuses the copy or its column
/// table, a size is zero or too big to count (wasm32's `usize` is 32
/// bits), or `src` is short.
pub fn scale(src: &[u32], src_w: u32, src_h: u32, dst_w: u32, dst_h: u32) -> Option<Vec<u32>> {
    let len = (dst_w as usize).checked_mul(dst_h as usize)?;
    let src_len = (src_w as usize).checked_mul(src_h as usize)?;
    if src_w == 0 || src_h == 0 || len == 0 || src.len() < src_len {
        return None;
    }
    let mut out = Vec::new();
    out.try_reserve_exact(len).ok()?;
    let mut columns: Vec<usize> = Vec::new();
    columns.try_reserve_exact(dst_w as usize).ok()?;
    columns.extend((0..dst_w).map(|x| source(x, dst_w, src_w) as usize));
    for y in 0..dst_h {
        let row = &src[source(y, dst_h, src_h) as usize * src_w as usize..][..src_w as usize];
        out.extend(columns.iter().map(|&x| row[x]));
    }
    Some(out)
}

/// Why an image can't be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Why {
    /// [`admit`] refused it from its header.
    Refused(Refused),
    /// The decoder refused it (a bad file), or ran out of memory
    /// ([`DecodeError::OutOfMemory`]).
    Decode(DecodeError),
    /// No memory for its fitted copy.
    NoMemory,
}

impl Why {
    /// Whether it is the memory plan failing (the heap refused what the
    /// plan says fits), not the file: the app warns about these.
    pub fn out_of_memory(&self) -> bool {
        matches!(self, Why::NoMemory | Why::Decode(DecodeError::OutOfMemory))
    }
}

impl core::fmt::Display for Why {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Why::Refused(r) => r.fmt(f),
            Why::Decode(e) => e.fmt(f),
            Why::NoMemory => f.write_str("no memory for its fitted copy"),
        }
    }
}

/// An image's copy fitted to [`VIEW`] and where it goes ([`fitted`]): the
/// place's size is the copy's.
pub struct Fitted {
    at: Rect,
    pixels: Surface,
}

impl Fitted {
    /// Where it goes in the client.
    pub fn at(&self) -> Rect {
        self.at
    }

    /// Its pixels, `at()`'s size.
    pub fn pixels(&self) -> &Surface {
        &self.pixels
    }
}

/// An image decoded and fitted to [`VIEW`]: what the app shows.
pub struct Prepared {
    image: Surface,
    fitted: Fitted,
}

impl Prepared {
    /// The image, 1:1.
    pub fn image(&self) -> &Surface {
        &self.image
    }

    /// Its copy fitted to [`VIEW`].
    pub fn fitted(&self) -> &Fitted {
        &self.fitted
    }
}

/// `png` [`admit`]ted, decoded and fitted to [`VIEW`], or why it can't be
/// shown: the app's one call for an image, which `tests/memory.rs` runs
/// through the app's allocator logic.
pub fn prepare(png: &[u8]) -> Result<Prepared, Why> {
    admit(png).map_err(Why::Refused)?;
    let image = decode(png, MAX_IMAGE_W, MAX_IMAGE_H).map_err(Why::Decode)?;
    let (w, h) = (image.width(), image.height());
    let at = fitted(w, h, VIEW);
    let pixels = scale(image.pixels(), w, h, at.w(), at.h())
        .and_then(|fit| Surface::from_pixels(at.w(), at.h(), fit))
        .ok_or(Why::NoMemory)?;
    // The decoder hands back exactly `w` x `h` pixels.
    let image = Surface::from_pixels(w, h, image.into_pixels())
        .ok_or_else(|| Why::Decode(DecodeError::Png(String::from("its pixels and its size disagree"))))?;
    Ok(Prepared { image, fitted: Fitted { at, pixels } })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_view_is_the_client_right_of_the_panel() {
        assert_eq!(VIEW, Rect::new(200, 0, 440, 480));
        assert_eq!(PANEL, Rect::new(0, 0, 200, 480));
    }

    #[test]
    fn the_wallpaper_at_1_1_shows_its_top_left_then_pans() {
        let mut pan = Pan::default();
        assert_eq!(actual(1280, 800, VIEW, pan), Placed { dst: VIEW, src_x: 0, src_y: 0 });
        assert!(pan.by(1, 1, 1280, 800, VIEW));
        assert_eq!(actual(1280, 800, VIEW, pan), Placed { dst: VIEW, src_x: PAN_STEP, src_y: PAN_STEP });
        // Back past the start: clamped at 0, and a move that changes
        // nothing says so.
        assert!(pan.by(-5, -5, 1280, 800, VIEW));
        assert_eq!(pan, Pan::default());
        assert!(!pan.by(-1, 0, 1280, 800, VIEW));
    }

    #[test]
    fn a_pan_stops_where_the_view_is_still_filled() {
        let mut pan = Pan::default();
        assert!(pan.by(1000, 1000, 1280, 800, VIEW));
        assert_eq!(pan, Pan { x: 1280 - 440, y: 800 - 480 });
        let placed = actual(1280, 800, VIEW, pan);
        assert_eq!((placed.src_x + placed.dst.w(), placed.src_y + placed.dst.h()), (1280, 800));
        assert!(!pan.by(1, 1, 1280, 800, VIEW));
    }

    #[test]
    fn a_smaller_image_is_centred_and_never_pans() {
        let mut pan = Pan::default();
        assert!(!pan.by(3, 3, 96, 64, VIEW));
        assert_eq!(
            actual(96, 64, VIEW, pan),
            Placed { dst: Rect::new(200 + (440 - 96) / 2, (480 - 64) / 2, 96, 64), src_x: 0, src_y: 0 }
        );
        // Bigger on one axis only: that one pans, the other is centred.
        assert!(pan.by(2, 2, 1000, 100, VIEW));
        let placed = actual(1000, 100, VIEW, pan);
        assert_eq!((placed.src_x, placed.src_y), (2 * PAN_STEP, 0));
        assert_eq!(placed.dst, Rect::new(200, (480 - 100) / 2, 440, 100));
    }

    #[test]
    fn a_pan_from_a_bigger_image_is_clamped_for_a_smaller_one() {
        let mut pan = Pan::default();
        pan.by(100, 100, 1280, 800, VIEW);
        // The same pan read for a smaller image: clamped, the view filled.
        let placed = actual(500, 500, VIEW, pan);
        assert_eq!((placed.src_x, placed.src_y), (60, 20));
        // And moving from there starts at the clamped place.
        assert!(pan.by(-1, 0, 500, 500, VIEW));
        assert_eq!(pan.x, 20);
    }

    #[test]
    fn fit_keeps_the_shape_and_fills_one_side() {
        // The wallpaper: the width decides.
        assert_eq!(fit_size(1280, 800, 440, 480), (440, 275));
        assert_eq!(fitted(1280, 800, VIEW), Rect::new(200, (480 - 275) / 2, 440, 275));
        // A tall image: the height decides, and it is centred across.
        assert_eq!(fit_size(100, 400, 440, 480), (120, 480));
        assert_eq!(fitted(100, 400, VIEW), Rect::new(200 + (440 - 120) / 2, 0, 120, 480));
        // A small one is enlarged.
        assert_eq!(fit_size(96, 64, 440, 480), (440, 293));
        // Exactly the view's shape: the whole view.
        assert_eq!(fit_size(880, 960, 440, 480), (440, 480));
        // A sliver never vanishes.
        assert_eq!(fit_size(10_000, 1, 440, 480), (440, 1));
        assert_eq!(fit_size(1, 10_000, 440, 480), (1, 480));
    }

    #[test]
    fn source_takes_the_first_pixel_first_and_never_passes_the_last() {
        assert_eq!(source(0, 440, 1280), 0);
        assert_eq!(source(439, 440, 1280), 439 * 1280 / 440);
        assert!(source(439, 440, 1280) < 1280);
        // Enlarging repeats pixels.
        assert_eq!((0..6).map(|d| source(d, 6, 3)).collect::<Vec<_>>(), [0, 0, 1, 1, 2, 2]);
        // Shrinking skips them.
        assert_eq!((0..3).map(|d| source(d, 3, 9)).collect::<Vec<_>>(), [0, 3, 6]);
    }

    #[test]
    fn scale_takes_each_pixel_from_its_source() {
        // A 4 x 2 image whose pixels are numbered.
        let src: Vec<u32> = (0..8).collect();
        assert_eq!(scale(&src, 4, 2, 2, 1).unwrap(), [0, 2]);
        assert_eq!(
            scale(&src, 4, 2, 8, 4).unwrap(),
            [0, 0, 1, 1, 2, 2, 3, 3, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 4, 4, 5, 5, 6, 6, 7, 7]
        );
        // Every pixel agrees with `source`, at the wallpaper's sizes.
        let (sw, sh, dw, dh) = (128, 80, 44, 27);
        let src: Vec<u32> = (0..sw * sh).collect();
        let out = scale(&src, sw, sh, dw, dh).unwrap();
        for y in 0..dh {
            for x in 0..dw {
                assert_eq!(out[(y * dw + x) as usize], source(y, dh, sh) * sw + source(x, dw, sw));
            }
        }
    }

    /// A PNG's signature and IHDR (and nothing after) for `admit`.
    fn header(w: u32, h: u32, depth: u8, colour: u8) -> Vec<u8> {
        let mut png = alloc::vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', 0, 0, 0, 13];
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&w.to_be_bytes());
        png.extend_from_slice(&h.to_be_bytes());
        png.extend_from_slice(&[depth, colour, 0, 0, 0]);
        png
    }

    #[test]
    fn admit_takes_up_to_8_bit_rgb_at_the_biggest_size() {
        assert_eq!(admit(&header(1280, 800, 8, 2)), Ok((1280, 800)));
        assert_eq!(admit(&header(1280, 800, 8, 3)), Ok((1280, 800)));
        assert_eq!(admit(&header(1280, 800, 16, 0)), Ok((1280, 800)));
        assert_eq!(admit(&header(1, 1, 1, 0)), Ok((1, 1)));
        // 8-bit RGBA: three quarters of the pixels; 16-bit RGBA three
        // eighths (480 x 800 is exactly the limit).
        assert_eq!(admit(&header(960, 800, 8, 6)), Ok((960, 800)));
        assert_eq!(admit(&header(480, 800, 16, 6)), Ok((480, 800)));
        assert_eq!(inflated(480, 800, 4, 16), MAX_ROW_BYTES);
        assert_eq!(
            admit(&header(481, 800, 16, 6)),
            Err(Refused::TooCostly { w: 481, h: 800, bytes: (1 + 8 * 481) * 800 })
        );
        assert_eq!(
            admit(&header(1280, 800, 8, 6)),
            Err(Refused::TooCostly { w: 1280, h: 800, bytes: (1 + 4 * 1280) * 800 })
        );
        assert_eq!(
            admit(&header(1280, 800, 16, 4)),
            Err(Refused::TooCostly { w: 1280, h: 800, bytes: (1 + 4 * 1280) * 800 })
        );
        assert_eq!((1 + 3 * 1280) * 800, MAX_ROW_BYTES);
    }

    #[test]
    fn admit_refuses_a_size_past_the_biggest_before_its_cost() {
        assert_eq!(admit(&header(1281, 1, 1, 0)), Err(Refused::TooBig { w: 1281, h: 1 }));
        assert_eq!(admit(&header(1, 801, 1, 0)), Err(Refused::TooBig { w: 1, h: 801 }));
        assert_eq!(admit(&header(u32::MAX, u32::MAX, 16, 6)), Err(Refused::TooBig { w: u32::MAX, h: u32::MAX }));
    }

    #[test]
    fn refusals_say_why() {
        assert_eq!(Refused::NoHeader.to_string(), "not a PNG (no header)");
        assert_eq!(Refused::BadFormat { colour: 5, depth: 8 }.to_string(), "colour type 5 at 8 bits isn't PNG's");
        assert_eq!(Refused::TooBig { w: 1281, h: 1 }.to_string(), "1281x1 pixels, more than the 1280x800 it shows");
        assert_eq!(
            Refused::TooCostly { w: 1280, h: 800, bytes: 4_096_800 }.to_string(),
            "1280x800 pixels need 4096800 bytes inflated, more than the 3072800 it takes"
        );
    }

    #[test]
    fn admit_refuses_an_image_with_no_pixels() {
        assert_eq!(admit(&header(0, 10, 8, 2)), Err(Refused::TooSmall { w: 0, h: 10 }));
        assert_eq!(admit(&header(10, 0, 8, 2)), Err(Refused::TooSmall { w: 10, h: 0 }));
        assert_eq!(admit(&header(0, 0, 8, 2)), Err(Refused::TooSmall { w: 0, h: 0 }));
        assert_eq!(Refused::TooSmall { w: 0, h: 10 }.to_string(), "0x10 pixels: no image");
    }

    #[test]
    fn admit_refuses_a_file_past_its_length_first() {
        let mut png = header(10, 10, 8, 2);
        png.resize(MAX_FILE_BYTES, 0);
        assert_eq!(admit(&png), Ok((10, 10)));
        png.push(0);
        assert_eq!(admit(&png), Err(Refused::TooLong { bytes: MAX_FILE_BYTES + 1 }));
        // Before the header: a long file with none is too long, not headless.
        assert_eq!(admit(&alloc::vec![0; MAX_FILE_BYTES + 1]), Err(Refused::TooLong { bytes: MAX_FILE_BYTES + 1 }));
        assert_eq!(
            Refused::TooLong { bytes: 9 }.to_string(),
            alloc::format!("9 bytes, more than the {MAX_FILE_BYTES} it takes")
        );
    }

    #[test]
    fn admit_refuses_what_png_doesnt_define_and_a_missing_header() {
        for (colour, depth) in [(2, 4), (3, 16), (6, 1), (5, 8), (0, 3), (4, 4)] {
            assert_eq!(admit(&header(10, 10, depth, colour)), Err(Refused::BadFormat { colour, depth }));
        }
        assert_eq!(admit(b""), Err(Refused::NoHeader));
        assert_eq!(admit(&header(10, 10, 8, 2)[..25]), Err(Refused::NoHeader));
        let mut png = header(10, 10, 8, 2);
        png[12] = b'X';
        assert_eq!(admit(&png), Err(Refused::NoHeader));
        png = header(10, 10, 8, 2);
        png[0] = 0;
        assert_eq!(admit(&png), Err(Refused::NoHeader));
    }

    #[test]
    fn inflated_rows_are_rounded_up_to_whole_bytes() {
        // 9 pixels at 1 bit: 2 bytes and the filter byte.
        assert_eq!(inflated(9, 1, 1, 1), 3);
        assert_eq!(inflated(8, 1, 1, 1), 2);
        assert_eq!(inflated(3, 2, 1, 2), 2 * 2);
        assert_eq!(inflated(1280, 800, 4, 16), (1 + 8 * 1280) * 800);
        assert_eq!(
            Refused::TooCostly { w: 9, h: 1, bytes: 3 }.to_string(),
            "9x1 pixels need 3 bytes inflated, more than the 3072800 it takes"
        );
    }

    #[test]
    fn only_running_out_of_memory_is_the_plan_failing() {
        assert!(Why::NoMemory.out_of_memory());
        assert!(Why::Decode(DecodeError::OutOfMemory).out_of_memory());
        assert!(!Why::Decode(DecodeError::Png(String::from("bad"))).out_of_memory());
        let big = DecodeError::TooBig { width: 2, height: 2, max_width: 1, max_height: 1 };
        assert!(!Why::Decode(big).out_of_memory());
        assert!(!Why::Refused(Refused::NoHeader).out_of_memory());
        assert!(!Why::Refused(Refused::TooCostly { w: 1, h: 1, bytes: 1 }).out_of_memory());
        assert_eq!(Why::NoMemory.to_string(), "no memory for its fitted copy");
        assert_eq!(Why::Refused(Refused::NoHeader).to_string(), Refused::NoHeader.to_string());
    }

    #[test]
    fn an_empty_view_fits_nothing() {
        assert_eq!(fitted(1280, 800, Rect::EMPTY), Rect::EMPTY);
        assert_eq!(fitted(1280, 800, Rect::new(5, 5, 0, 10)), Rect::EMPTY);
    }

    #[test]
    fn scale_refuses_sizes_too_big_to_count() {
        assert_eq!(scale(&[1], u32::MAX, u32::MAX, 1, 1), None);
    }

    #[test]
    fn scale_refuses_zero_sizes_and_short_sources() {
        assert_eq!(scale(&[1, 2, 3], 2, 2, 1, 1), None);
        assert_eq!(scale(&[1], 0, 1, 1, 1), None);
        assert_eq!(scale(&[1], 1, 1, 0, 1), None);
    }
}
