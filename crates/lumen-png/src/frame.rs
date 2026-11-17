//! The frame a PNG travels in over serial (ARCHITECTURE section 11,
//! "Screenshot"; `tools/harness/lumen_harness/screenshot.py` reads it):
//!
//! ```text
//! @@png-begin 1280x800 bytes=31337 crc32=0a1b2c3d lines=349
//! @@png iVBORw0KGgoAAAANSUhEUgAABQAAAAMgCAIAAADK...   (base64, LINE_CHARS a line)
//! ...
//! @@png-end
//! ```
//!
//! - The begin line gives the image's size in pixels, the PNG's length in
//!   bytes, its CRC-32 ([`crate::crc32`], eight lowercase hex digits) and
//!   how many body lines follow.
//! - Each body line is [`BODY`] then at most [`LINE_CHARS`] characters of
//!   the PNG in base64 (every line but the last exactly that many, so no
//!   line ends inside a 4-character group; padding only at the very end).
//! - The end line is [`END`] alone.
//!
//! Lines end in `\n` (CR LF on serial). Every line starts with `@@png`,
//! which no log line does (they start with a timestamp), so a reader keeps
//! a frame's lines by their prefix and skips log lines that land between
//! them: the kernel writes each line whole, so a log line never lands
//! inside one.

use crate::{base64, crc32::crc32};
use alloc::string::String;
use core::fmt::Write;

/// Starts the begin line.
pub const BEGIN: &str = "@@png-begin ";
/// Starts each body line.
pub const BODY: &str = "@@png ";
/// The end line (without its newline).
pub const END: &str = "@@png-end";
/// Base64 characters on each body line but the last (a multiple of 4: 90
/// bytes of PNG).
pub const LINE_CHARS: usize = 120;
const _: () = assert!(LINE_CHARS.is_multiple_of(4));

/// Bytes of PNG on each full body line.
const LINE_BYTES: usize = LINE_CHARS / 4 * 3;

/// Body lines for a PNG of `bytes` bytes.
pub const fn body_lines(bytes: usize) -> usize {
    bytes.div_ceil(LINE_BYTES)
}

/// The length of [`frame`]'s text for a PNG of `bytes` bytes, at most (the
/// begin line's numbers taken at their longest).
pub const fn max_len(bytes: usize) -> usize {
    // "@@png-begin " + "4294967295x4294967295 bytes=<20> crc32=<8> lines=<20>\n"
    const BEGIN_MAX: usize = 12 + 21 + 7 + 20 + 7 + 8 + 7 + 20 + 1;
    BEGIN_MAX + body_lines(bytes) * (BODY.len() + 1) + base64::encoded_len(bytes) + END.len() + 1
}

/// `png` (a `width` x `height` image) framed, each line ending in `\n`.
pub fn frame(png: &[u8], width: u32, height: u32) -> String {
    let mut out = String::new();
    out.reserve_exact(max_len(png.len()));
    let _ = writeln!(
        out,
        "{BEGIN}{width}x{height} bytes={} crc32={:08x} lines={}",
        png.len(),
        crc32(png),
        body_lines(png.len())
    );
    for piece in png.chunks(LINE_BYTES) {
        out.push_str(BODY);
        base64::encode_into(piece, &mut out);
        out.push('\n');
    }
    out.push_str(END);
    out.push('\n');
    out
}
