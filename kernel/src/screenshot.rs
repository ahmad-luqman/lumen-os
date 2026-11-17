//! `screenshot`: the screen as a PNG, out over serial in a text frame
//! (`lumen_png::frame`; ARCHITECTURE section 11, "Screenshot"). To disk
//! once there are files (M6).
//!
//! The pixels come straight from the framebuffer, read once under the
//! screen's lock ([`term::read_pixels`]): no terminal output, cursor flip
//! or pointer move lands halfway through, so the image is one moment of
//! the screen, the mouse pointer included where it is drawn (QEMU's
//! `screendump` shows it too). Only a command drawing on a `term::Canvas`
//! (it draws without the lock) can still change pixels mid-read. The
//! framebuffer is write-combined: reads are slow there, but this reads it
//! once. Desktop mode (#153) will read the compositor's back buffer
//! instead.
//!
//! The rows are encoded unfiltered at zlib level 6 ([`FILTER`], [`LEVEL`])
//! after the lock is dropped, then written in chunks of whole
//! lines (`shell::write_in_chunks`), so log lines may land between the
//! frame's lines, never inside one; the harness skips them by prefix.

use crate::shell::Ctx;
use crate::term;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use lumen_png::{Filter, Rows, frame};
use lumen_time::Millis;

/// No filter: on the text console (boot log, then `colors`) the rows
/// compressed to about 20% less unfiltered than with `Sub` or `Up`: the
/// anti-aliased glyphs' differences repeat less than their pixels.
const FILTER: Filter = Filter::None;
/// zlib's default level.
const LEVEL: u8 = 6;

/// A screen captured and encoded.
pub struct Shot {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
    /// How long reading the pixels (under the screen's lock) and encoding
    /// them took.
    pub read_ns: u64,
    pub encode_ns: u64,
}

/// Why there is no screenshot.
pub enum Error {
    /// No terminal on a framebuffer (none usable, or one too small), or a
    /// crash report took the screen.
    NoScreen,
    Png(lumen_png::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoScreen => write!(f, "no screen to capture"),
            Error::Png(e) => write!(f, "{e}"),
        }
    }
}

/// Capture the screen and encode it as PNG.
pub fn capture() -> Result<Shot, Error> {
    let (w, h) = term::read_pixels(|fb| (fb.width(), fb.height())).ok_or(Error::NoScreen)?;
    let size = |n: usize| u32::try_from(n).map_err(|_| Error::Png(lumen_png::Error::TooLarge));
    // Allocated before taking the lock (nothing allocates under it); the
    // framebuffer's size never changes.
    let mut rows = Rows::new(size(w)?, size(h)?).map_err(Error::Png)?;
    let start = crate::time::now_ns();
    term::read_pixels(|fb| {
        for y in 0..rows.height() {
            fb.rgb_row(y as usize, rows.row_mut(y));
        }
    })
    .ok_or(Error::NoScreen)?;
    let read = crate::time::now_ns();
    let png = rows.encode(FILTER, LEVEL);
    let encoded = crate::time::now_ns();
    Ok(Shot {
        width: size(w)?,
        height: size(h)?,
        png,
        read_ns: read.saturating_sub(start),
        encode_ns: encoded.saturating_sub(read),
    })
}

/// The shell's `screenshot`: the frame, then a summary line.
pub fn command(ctx: &mut Ctx<'_>, args: &[String]) {
    if !args.is_empty() {
        return ctx.fail("usage: screenshot");
    }
    if ctx.out.vt().is_some() {
        // A screen full of base64 is no use; the harness reads serial.
        return ctx.fail("screenshot: run it on serial (the PNG comes out as text)");
    }
    let shot = match capture() {
        Ok(shot) => shot,
        Err(e) => return ctx.fail(format_args!("screenshot: {e}")),
    };
    let start = crate::time::now_ns();
    let text = frame::frame(&shot.png, shot.width, shot.height);
    crate::shell::write_in_chunks(ctx.out, &text);
    let sent = crate::time::now_ns().saturating_sub(start);
    let _ = writeln!(
        ctx.out,
        "screenshot: {}x{}, {} bytes of PNG (read {}, encoded {}, sent {})",
        shot.width,
        shot.height,
        shot.png.len(),
        Millis(shot.read_ns),
        Millis(shot.encode_ns),
        Millis(sent)
    );
}
