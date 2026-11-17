//! A PNG writer for the kernel's `screenshot`, kept free of kernel
//! dependencies so it can be tested on the host:
//!
//! - [`Rows`]: an RGB8 image (8 bits a channel, no alpha) as PNG's rows
//!   before compression, each a filter byte then its pixels. The caller
//!   fills the pixels ([`Rows::row_mut`]); [`Rows::encode`] filters them in
//!   place ([`Filter`]), compresses them (zlib, from `miniz_oxide`) and
//!   writes the file: the signature, `IHDR`, one `IDAT`, `IEND`.
//! - [`crc32`]: the CRC PNG's chunks carry (and the frame's check).
//! - [`base64`] and [`frame`]: how a PNG travels over serial, as lines
//!   the harness picks out from between log lines.
//!
//! Integers only: nothing here is floating point (the kernel has no SSE).

#![no_std]

extern crate alloc;

pub mod base64;
pub mod crc32;
pub mod frame;

use alloc::vec::Vec;
use core::fmt;

/// The eight bytes every PNG file starts with.
pub const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// Bytes a pixel: red, green, blue.
pub const BYTES_PER_PIXEL: usize = 3;

/// PNG's limit on either dimension (2^31 - 1).
pub const MAX_DIMENSION: u32 = i32::MAX as u32;

/// Why [`Rows::new`] can't make an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A width or height of 0: PNG has no empty images.
    Empty,
    /// A dimension past [`MAX_DIMENSION`], or more bytes than fit in memory's
    /// address space.
    TooLarge,
    /// The allocator refused the rows' bytes (this many).
    OutOfMemory(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Empty => write!(f, "an image with no pixels"),
            Error::TooLarge => write!(f, "an image too large for PNG"),
            Error::OutOfMemory(bytes) => write!(f, "no memory for {bytes} bytes of pixels"),
        }
    }
}

/// How each row is filtered before compression (PNG's filter types; one
/// for every row). The filter turns a row into differences, which compress
/// better where neighbouring pixels change smoothly (photos, gradients);
/// where the same pixels repeat exactly, as on a text screen, deflate's
/// matches do better unfiltered (the kernel's `screenshot` uses `None`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    /// Type 0: the pixels as they are.
    None,
    /// Type 1: each byte minus the same channel of the pixel to its left.
    Sub,
    /// Type 2: each byte minus the byte above it.
    Up,
}

impl Filter {
    /// The filter-type byte that starts each row.
    pub const fn byte(self) -> u8 {
        match self {
            Filter::None => 0,
            Filter::Sub => 1,
            Filter::Up => 2,
        }
    }
}

/// An RGB8 image laid out as PNG's uncompressed rows: each row one filter
/// byte, then `width` pixels of [`BYTES_PER_PIXEL`] bytes.
pub struct Rows {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl fmt::Debug for Rows {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Rows({}x{})", self.width, self.height)
    }
}

impl Rows {
    /// A black `width` x `height` image. Allocates all of its bytes here
    /// (and nothing in [`Rows::row_mut`]), so a caller can fill it where it
    /// mustn't allocate; refuses rather than aborts when the allocator
    /// can't give them.
    pub fn new(width: u32, height: u32) -> Result<Rows, Error> {
        if width == 0 || height == 0 {
            return Err(Error::Empty);
        }
        if width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(Error::TooLarge);
        }
        let len = Self::stride_of(width)
            .and_then(|stride| stride.checked_mul(height as usize))
            .filter(|&len| len <= isize::MAX as usize)
            .ok_or(Error::TooLarge)?;
        let mut data = Vec::new();
        data.try_reserve_exact(len).map_err(|_| Error::OutOfMemory(len))?;
        data.resize(len, 0);
        Ok(Rows { width, height, data })
    }

    /// One row's bytes, its filter byte included.
    fn stride_of(width: u32) -> Option<usize> {
        (width as usize).checked_mul(BYTES_PER_PIXEL)?.checked_add(1)
    }

    fn stride(&self) -> usize {
        // Checked in `new`.
        self.width as usize * BYTES_PER_PIXEL + 1
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Row `y`'s pixels (red, green, blue for each, left to right), to fill
    /// in. Panics if `y` is past the last row.
    pub fn row_mut(&mut self, y: u32) -> &mut [u8] {
        assert!(y < self.height, "row {y} of a {}-row image", self.height);
        let stride = self.stride();
        let start = y as usize * stride;
        &mut self.data[start + 1..start + stride]
    }

    /// Row `y`'s pixels as filled in. Panics if `y` is past the last row.
    pub fn row(&self, y: u32) -> &[u8] {
        assert!(y < self.height, "row {y} of a {}-row image", self.height);
        let stride = self.stride();
        let start = y as usize * stride;
        &self.data[start + 1..start + stride]
    }

    /// Filter every row with `filter` (in place: rows bottom to top for
    /// `Up`, so each still sees the unfiltered row above; bytes right to
    /// left for `Sub`), then return the image's uncompressed `IDAT` data.
    fn filtered(mut self, filter: Filter) -> Vec<u8> {
        let stride = self.stride();
        let rows = self.height as usize;
        match filter {
            Filter::None => {}
            Filter::Sub => {
                for row in self.data.chunks_exact_mut(stride) {
                    let pixels = &mut row[1..];
                    for i in (BYTES_PER_PIXEL..pixels.len()).rev() {
                        pixels[i] = pixels[i].wrapping_sub(pixels[i - BYTES_PER_PIXEL]);
                    }
                }
            }
            Filter::Up => {
                for y in (1..rows).rev() {
                    let (above, row) = self.data.split_at_mut(y * stride);
                    let above = &above[(y - 1) * stride + 1..];
                    for (byte, up) in row[1..stride].iter_mut().zip(above) {
                        *byte = byte.wrapping_sub(*up);
                    }
                }
            }
        }
        for row in self.data.chunks_exact_mut(stride) {
            row[0] = filter.byte();
        }
        self.data
    }

    /// The PNG file: the rows filtered with `filter`, compressed at zlib
    /// `level` (0, stored, to 10, miniz's slowest), in one `IDAT`.
    pub fn encode(self, filter: Filter, level: u8) -> Vec<u8> {
        let (width, height) = (self.width, self.height);
        let zlib = miniz_oxide::deflate::compress_to_vec_zlib(&self.filtered(filter), level);
        let mut png = Vec::with_capacity(SIGNATURE.len() + 3 * 12 + 13 + zlib.len());
        png.extend_from_slice(&SIGNATURE);
        let mut ihdr = [0u8; 13];
        ihdr[0..4].copy_from_slice(&width.to_be_bytes());
        ihdr[4..8].copy_from_slice(&height.to_be_bytes());
        // Bit depth 8, colour type 2 (RGB), deflate, adaptive filtering
        // (method 0: a type byte per row), no interlace.
        ihdr[8..13].copy_from_slice(&[8, 2, 0, 0, 0]);
        chunk(&mut png, *b"IHDR", &ihdr);
        chunk(&mut png, *b"IDAT", &zlib);
        chunk(&mut png, *b"IEND", &[]);
        png
    }
}

/// Append one chunk: its length, type, data and the CRC of type and data.
/// `data` must be shorter than 2^31 bytes (PNG's limit).
fn chunk(png: &mut Vec<u8>, kind: [u8; 4], data: &[u8]) {
    let len = u32::try_from(data.len()).ok().filter(|&l| l <= MAX_DIMENSION);
    let len = len.expect("a PNG chunk is shorter than 2^31 bytes");
    png.extend_from_slice(&len.to_be_bytes());
    png.extend_from_slice(&kind);
    png.extend_from_slice(data);
    let mut crc = crc32::Crc32::new();
    crc.update(&kind);
    crc.update(data);
    png.extend_from_slice(&crc.finish().to_be_bytes());
}

#[cfg(test)]
mod tests;
