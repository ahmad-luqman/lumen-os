//! PNG for Lumen: the encoder the kernel's `screenshot` uses ([`encode`])
//! and the decoder the desktop's wallpaper uses ([`decode`]). Kept free of
//! kernel dependencies so it can be tested on the host:
//!
//! - [`decode`]: any PNG into `0x00RRGGBB` pixels ([`decode::Image`]),
//!   with `zune-png`; a file that isn't a PNG, is cut short or corrupt, or
//!   is bigger than allowed is an error ([`decode::DecodeError`]), never a
//!   panic.
//! - [`encode`]: an RGB8 image ([`encode::Rows`]) into a PNG file
//!   ([`encode::Png`]), zlib from `miniz_oxide`; the rows' and the file's
//!   buffers are refused rather than aborted when the allocator says no
//!   (the compressor's own fixed boxes excepted).
//! - [`crc32`]: the CRC PNG's chunks carry (and the frame's check).
//! - [`base64`] and [`frame`]: how a PNG travels over serial, as lines the
//!   harness picks out from between log lines.
//!
//! Integers only in this crate: nothing here is floating point (the kernel
//! has no SSE).

#![no_std]

extern crate alloc;

pub mod base64;
pub mod decode;
pub mod encode;
pub mod frame;

/// CRC-32 as PNG computes it (and zlib's `crc32`, and Python's
/// `zlib.crc32`): the reflected polynomial 0xEDB88320, starting from all
/// ones and inverted at the end; a byte at a time from a table built at
/// compile time.
pub fn crc32(bytes: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut n = 0;
        while n < 256 {
            let mut c = n as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
                k += 1;
            }
            table[n] = c;
            n += 1;
        }
        table
    };
    !bytes.iter().fold(!0u32, |c, &b| TABLE[((c ^ u32::from(b)) & 0xff) as usize] ^ (c >> 8))
}

#[cfg(test)]
mod tests;
