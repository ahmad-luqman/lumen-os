//! The frame a PNG travels in over serial (ARCHITECTURE section 11,
//! "Screenshot"; `tools/harness/lumen_harness/screenshot.py` reads it):
//!
//! ```text
//! @@png-begin 1280x800 bytes=46726 crc32=0a1b2c3d lines=520
//! @@png iVBORw0KGgoAAAANSUhEUgAABQAAAAMgCAIAAADz...   (base64, LINE_CHARS a line)
//! ...
//! @@png-end
//! ```
//!
//! - The begin line gives the image's size in pixels, the PNG file's
//!   length in bytes, the CRC-32 of the whole file ([`crate::crc32`], the
//!   algorithm `zlib.crc32` uses; eight lowercase hex digits) and how many
//!   body lines follow.
//! - Each body line is [`BODY`] then at most [`LINE_CHARS`] characters of
//!   the file in base64 (every line but the last exactly that many, so no
//!   line ends inside a 4-character group; padding only at the very end).
//! - The end line is [`END`] alone.
//!
//! Every line starts with `@@png`, which no log line does (they start with
//! a timestamp), so a reader keeps a frame's lines by their prefix and
//! skips log lines that land between them. [`write`] hands out each line
//! whole, from buffers on the stack (nothing allocates), for the caller to
//! write in one piece, so a log line never lands inside one.

use crate::{base64, crc32, encode::Png};
use core::fmt::{self, Write};

/// Starts the begin line.
pub const BEGIN: &str = "@@png-begin ";
/// Starts each body line.
pub const BODY: &str = "@@png ";
/// The end line (without its newline).
pub const END: &str = "@@png-end";
/// The end line as [`write`] hands it out.
const END_LINE: &str = "@@png-end\n";
/// Base64 characters on each body line but the last (a multiple of 4: 90
/// bytes of PNG).
pub const LINE_CHARS: usize = 120;
const _: () = assert!(LINE_CHARS.is_multiple_of(4));

/// Bytes of PNG on each full body line.
const LINE_BYTES: usize = LINE_CHARS / 4 * 3;

/// The longest begin line: "@@png-begin 4294967295x4294967295
/// bytes=<20 digits> crc32=<8> lines=<20>" and its newline.
const BEGIN_MAX: usize = BEGIN.len() + 21 + 7 + 20 + 7 + 8 + 7 + 20 + 1;

/// Body lines for a PNG of `bytes` bytes.
pub const fn body_lines(bytes: usize) -> usize {
    bytes.div_ceil(LINE_BYTES)
}

/// A line formatted on the stack.
struct Line<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> Write for Line<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        self.buf.get_mut(self.len..end).ok_or(fmt::Error)?.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

impl<const N: usize> Line<N> {
    fn as_str(&self) -> Result<&str, fmt::Error> {
        core::str::from_utf8(&self.buf[..self.len]).map_err(|_| fmt::Error)
    }
}

/// Hand `png`'s frame to `line`, a line at a time, each with its `\n`;
/// stops at the first error `line` returns.
pub fn write(png: &Png, mut line: impl FnMut(&str) -> fmt::Result) -> fmt::Result {
    let bytes = png.bytes();
    let mut begin = Line { buf: [0; BEGIN_MAX], len: 0 };
    writeln!(
        begin,
        "{BEGIN}{}x{} bytes={} crc32={:08x} lines={}",
        png.width(),
        png.height(),
        bytes.len(),
        crc32(bytes),
        body_lines(bytes.len())
    )?;
    line(begin.as_str()?)?;
    let mut body = Line { buf: [0; BODY.len() + LINE_CHARS + 1], len: BODY.len() };
    body.buf[..BODY.len()].copy_from_slice(BODY.as_bytes());
    for piece in bytes.chunks(LINE_BYTES) {
        let chars = base64::encode_to_slice(piece, &mut body.buf[BODY.len()..]);
        body.buf[BODY.len() + chars] = b'\n';
        body.len = BODY.len() + chars + 1;
        line(body.as_str()?)?;
    }
    line(END_LINE)
}
