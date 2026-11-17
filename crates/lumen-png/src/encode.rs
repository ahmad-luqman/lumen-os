//! The PNG encoder: an RGB8 image ([`Rows`]: 8 bits a channel, no alpha)
//! into a PNG file ([`Png`]): the signature, `IHDR`, one `IDAT` (the rows,
//! unfiltered, zlib at [`LEVEL`] from `miniz_oxide`), `IEND`.
//!
//! **Memory.** Every buffer the encoder needs is taken with `try_reserve`:
//! the rows ([`Rows::new`], 3 MiB at 1280x800, allocated up front so the
//! caller can fill them where it mustn't allocate) and the file, which the
//! compressor writes into directly and which grows by doubling. An
//! allocator that says no gives [`Error::OutOfMemory`], not an abort. The
//! exceptions are the compressor's own few fixed boxes (about 170 KiB; its
//! hash tables), which `miniz_oxide` allocates infallibly.
//!
//! **Stack.** The compressor keeps a 64 KiB buffer inline, so
//! [`Rows::encode`] needs about 75 KiB of stack: more than a kernel thread's
//! default (the kernel runs it on a bigger stack, `screenshot.rs`).
//!
//! **No filter.** Every row is written as it is (filter type 0). On the
//! text console (the boot log, `colors`) that compressed 16 to 20% smaller
//! than the `Sub` or `Up` filters: anti-aliased glyphs' differences repeat
//! less than their pixels, and deflate's matches find the repeats.

use crate::crc32;
use alloc::vec::Vec;
use core::fmt;
use miniz_oxide::DataFormat;
use miniz_oxide::deflate::core::{CompressorOxide, TDEFLFlush, TDEFLStatus, compress};

/// The eight bytes every PNG file starts with.
pub const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];

/// Bytes a pixel: red, green, blue.
pub const BYTES_PER_PIXEL: usize = 3;

/// PNG's limit on either dimension, and on a chunk's length (2^31 - 1).
pub const MAX_DIMENSION: u32 = i32::MAX as u32;

/// zlib's default level (of miniz's 0, stored, to 10).
pub const LEVEL: u8 = 6;

/// Bytes before the compressed data: the signature, the `IHDR` chunk (12
/// bytes of framing, 13 of data), the `IDAT` chunk's length and type.
const HEADER: usize = SIGNATURE.len() + 12 + 13 + 8;
/// Bytes after it: the `IDAT` chunk's CRC and the `IEND` chunk.
const TRAILER: usize = 4 + 12;
/// The compressor is handed at least this much room each time.
const MIN_ROOM: usize = 64;

/// Why there is no PNG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// A width or height of 0: PNG has no empty images.
    Empty,
    /// A dimension past [`MAX_DIMENSION`], rows of more bytes than fit in
    /// memory's address space, or compressed data longer than a chunk may
    /// be.
    TooLarge,
    /// The allocator refused a buffer of this many bytes.
    OutOfMemory(usize),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Empty => write!(f, "an image with no pixels"),
            Error::TooLarge => write!(f, "an image too large for PNG"),
            Error::OutOfMemory(bytes) => write!(f, "no memory for a buffer of {bytes} bytes"),
        }
    }
}

/// An RGB8 image laid out as PNG's uncompressed rows: each row one filter
/// byte (0: none), then `width` pixels.
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
    /// A black `width` x `height` image. All of its bytes are allocated
    /// here (nothing in [`Rows::row_mut`]).
    pub fn new(width: u32, height: u32) -> Result<Rows, Error> {
        if width == 0 || height == 0 {
            return Err(Error::Empty);
        }
        if width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(Error::TooLarge);
        }
        let len = (width as usize)
            .checked_mul(BYTES_PER_PIXEL)
            .and_then(|pixels| pixels.checked_add(1))
            .and_then(|stride| stride.checked_mul(height as usize))
            .filter(|&len| len <= isize::MAX as usize)
            .ok_or(Error::TooLarge)?;
        let mut data = Vec::new();
        data.try_reserve_exact(len).map_err(|_| Error::OutOfMemory(len))?;
        data.resize(len, 0);
        Ok(Rows { width, height, data })
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

    /// Row `y`'s pixels, left to right, to fill in. Panics if `y` is past
    /// the last row.
    pub fn row_mut(&mut self, y: u32) -> &mut [[u8; BYTES_PER_PIXEL]] {
        assert!(y < self.height, "row {y} of a {}-row image", self.height);
        let stride = self.stride();
        let start = y as usize * stride;
        self.data[start + 1..start + stride].as_chunks_mut().0
    }

    /// Row `y`'s pixels as filled in. Panics if `y` is past the last row.
    pub fn row(&self, y: u32) -> &[[u8; BYTES_PER_PIXEL]] {
        assert!(y < self.height, "row {y} of a {}-row image", self.height);
        let stride = self.stride();
        let start = y as usize * stride;
        self.data[start + 1..start + stride].as_chunks().0
    }

    /// The PNG file. Needs about 75 KiB of stack (see the module comment).
    pub fn encode(self) -> Result<Png, Error> {
        let (width, height) = (self.width, self.height);
        let mut png = Vec::new();
        reserve(&mut png, HEADER + self.data.len() / 16 + TRAILER + MIN_ROOM)?;
        png.extend_from_slice(&SIGNATURE);
        let mut ihdr = [0u8; 13];
        ihdr[0..4].copy_from_slice(&width.to_be_bytes());
        ihdr[4..8].copy_from_slice(&height.to_be_bytes());
        // Bit depth 8, colour type 2 (RGB), deflate, adaptive filtering
        // (method 0: a type byte per row), no interlace.
        ihdr[8..13].copy_from_slice(&[8, 2, 0, 0, 0]);
        let start = png.len();
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&ihdr);
        let crc = crc32(&png[start + 4..]);
        png.extend_from_slice(&crc.to_be_bytes());
        // IDAT: its length is filled in once the data is compressed.
        png.extend_from_slice(&[0; 4]);
        png.extend_from_slice(b"IDAT");
        let data = png.len();
        debug_assert_eq!(data, HEADER);

        let mut compressor = CompressorOxide::default();
        compressor.set_format_and_level(DataFormat::Zlib, LEVEL);
        let mut input = &self.data[..];
        loop {
            if png.capacity() - png.len() < MIN_ROOM {
                let double = png.capacity();
                reserve(&mut png, double)?;
            }
            let at = png.len();
            // Within the capacity: no allocation.
            png.resize(png.capacity(), 0);
            let (status, read, written) = compress(&mut compressor, input, &mut png[at..], TDEFLFlush::Finish);
            png.truncate(at + written);
            input = &input[read..];
            match status {
                TDEFLStatus::Done => break,
                TDEFLStatus::Okay => {}
                other => panic!("miniz_oxide refused to compress ({other:?}): a bug"),
            }
        }

        let len = u32::try_from(png.len() - data).ok().filter(|&l| l <= MAX_DIMENSION);
        png[data - 8..data - 4].copy_from_slice(&len.ok_or(Error::TooLarge)?.to_be_bytes());
        let crc = crc32(&png[data - 4..]);
        reserve(&mut png, TRAILER)?;
        png.extend_from_slice(&crc.to_be_bytes());
        png.extend_from_slice(&[0, 0, 0, 0]);
        png.extend_from_slice(b"IEND");
        png.extend_from_slice(&crc32(b"IEND").to_be_bytes());
        Ok(Png { width, height, bytes: png })
    }
}

/// Room for `extra` more bytes in `buf`, or the size refused.
fn reserve(buf: &mut Vec<u8>, extra: usize) -> Result<(), Error> {
    buf.try_reserve(extra).map_err(|_| Error::OutOfMemory(buf.len().saturating_add(extra)))
}

/// A PNG file and the size of the image in it.
pub struct Png {
    width: u32,
    height: u32,
    bytes: Vec<u8>,
}

impl fmt::Debug for Png {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Png({}x{}, {} bytes)", self.width, self.height, self.bytes.len())
    }
}

impl Png {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The file.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Any bytes, labelled as a `width` x `height` PNG (the frame's tests).
    #[cfg(test)]
    pub(crate) fn from_parts(width: u32, height: u32, bytes: Vec<u8>) -> Png {
        Png { width, height, bytes }
    }
}
