//! PNG decoding (#154: the desktop's wallpaper), with `zune-png` built
//! without its `std` and `sse` features: any PNG (greyscale, RGB, palette,
//! with or without alpha, 1 to 16 bits a sample, interlaced or not) into
//! `0x00RRGGBB` pixels, as `lumen-gfx` draws them.
//!
//! A file that isn't a PNG, is cut short or corrupt (each chunk's CRC is
//! checked, and the decoder runs in its strict mode), or is bigger than the
//! caller allows is an error, never a panic: [`DecodeError`]. Alpha is
//! dropped (a wallpaper has nothing behind it), and 16-bit samples keep
//! their top 8 bits.
//!
//! Memory: the decoder's own buffers (the compressed data gathered, the
//! inflated rows) are its `Vec`s, an allocation failure there aborting as
//! any `Vec` does; the decoded samples' buffer and the pixels handed back
//! are taken with `try_reserve_exact`, so those refuse
//! ([`DecodeError::OutOfMemory`]).

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt;

use zune_core::bytestream::ZCursor;
use zune_core::colorspace::ColorSpace;
use zune_core::options::DecoderOptions;
use zune_png::PngDecoder;

/// A decoded image: `width` x `height` pixels, `0x00RRGGBB`, row-major
/// with no padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    width: u32,
    height: u32,
    pixels: Vec<u32>,
}

impl Image {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// Row `y` (`width` pixels). Panics past the last row.
    pub fn row(&self, y: u32) -> &[u32] {
        let w = self.width as usize;
        &self.pixels[y as usize * w..][..w]
    }

    /// Every pixel, row after row.
    pub fn pixels(&self) -> &[u32] {
        &self.pixels
    }
}

/// Why a PNG didn't decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The decoder refused it: not a PNG, cut short, a bad CRC, a broken
    /// zlib stream, an unknown colour type... (its message).
    Png(String),
    /// Wider or taller than the caller allows.
    TooBig { width: u32, height: u32, max_width: u32, max_height: u32 },
    /// No memory for the samples or the pixels.
    OutOfMemory,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Png(why) => write!(f, "not a PNG it can read: {why}"),
            DecodeError::TooBig { width, height, max_width, max_height } => {
                write!(f, "{width}x{height} pixels, more than the {max_width}x{max_height} allowed")
            }
            DecodeError::OutOfMemory => f.write_str("no memory for its pixels"),
        }
    }
}

/// The decoder's message, as one line (its `Display` ends in a newline and
/// quotes some messages).
fn message(e: impl fmt::Display) -> DecodeError {
    let text = e.to_string();
    DecodeError::Png(text.trim().trim_matches('"').to_string())
}

/// The size IHDR gives, if `png` starts with the signature and an IHDR.
fn header_size(png: &[u8]) -> Option<(u32, u32)> {
    let be = |at: usize| png.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    (png.get(..8)? == crate::encode::SIGNATURE && png.get(12..16)? == b"IHDR").then_some(())?;
    Some((be(16)?, be(20)?))
}

/// Decode `png`, refusing an image wider than `max_width` or taller than
/// `max_height` before anything is inflated.
pub fn decode(png: &[u8], max_width: u32, max_height: u32) -> Result<Image, DecodeError> {
    if let Some((width, height)) = header_size(png)
        && (width > max_width || height > max_height)
    {
        return Err(DecodeError::TooBig { width, height, max_width, max_height });
    }
    let options = DecoderOptions::default()
        .set_strict_mode(true)
        .png_set_confirm_crc(true)
        .png_set_strip_to_8bit(true)
        .png_set_decode_animated(false)
        // The header's size was checked against the caller's limits above;
        // the decoder checks them too (a header it reads differently).
        .set_max_width(max_width.max(1) as usize)
        .set_max_height(max_height.max(1) as usize);
    let mut decoder = PngDecoder::new_with_options(ZCursor::new(png), options);
    decoder.decode_headers().map_err(message)?;
    let (w, h) = decoder.dimensions().ok_or_else(|| DecodeError::Png("no header".into()))?;
    let (width, height) = (u32::try_from(w).unwrap_or(u32::MAX), u32::try_from(h).unwrap_or(u32::MAX));
    if width > max_width || height > max_height {
        return Err(DecodeError::TooBig { width, height, max_width, max_height });
    }
    let channels = match decoder.colorspace() {
        Some(ColorSpace::Luma) => 1,
        Some(ColorSpace::LumaA) => 2,
        Some(ColorSpace::RGB) => 3,
        Some(ColorSpace::RGBA) => 4,
        other => return Err(DecodeError::Png(alloc::format!("an unexpected colour space {other:?}"))),
    };
    // Within the limits both are far inside `usize` (16384 squared times 4
    // is 2^30); checked all the same.
    let samples = (w.checked_mul(h).and_then(|n| n.checked_mul(channels))).ok_or(DecodeError::TooBig {
        width,
        height,
        max_width,
        max_height,
    })?;
    // The decoder writes 8 bits a sample (16-bit images stripped): its own
    // size for the buffer must agree.
    if decoder.output_buffer_size() != Some(samples) {
        return Err(DecodeError::Png("its size and its samples disagree".into()));
    }
    let mut raw = Vec::new();
    raw.try_reserve_exact(samples).map_err(|_| DecodeError::OutOfMemory)?;
    raw.resize(samples, 0u8);
    decoder.decode_into(&mut raw).map_err(message)?;
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(w * h).map_err(|_| DecodeError::OutOfMemory)?;
    pixels.extend(raw.chunks_exact(channels).map(|p| {
        let (r, g, b) = match channels {
            1 | 2 => (p[0], p[0], p[0]),
            _ => (p[0], p[1], p[2]),
        };
        (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)
    }));
    Ok(Image { width, height, pixels })
}
