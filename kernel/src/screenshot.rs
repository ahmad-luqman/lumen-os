//! `screenshot`: the screen as a PNG, out over serial in a text frame
//! (`lumen_png::frame`; ARCHITECTURE section 11, "Screenshot"). To disk
//! once there are files (M6).
//!
//! Two steps, kept apart so the desktop can bring its own source:
//!
//! - **Capture** ([`capture_framebuffer`]): the framebuffer's pixels into
//!   `lumen_png` rows allocated beforehand, read once under the screen's
//!   lock ([`term::read_pixels`]), so terminal output, a cursor flip or a
//!   pointer move can't land halfway through. A command drawing on a
//!   `term::Canvas` draws without that lock: the capture then says a
//!   canvas was drawing, and the summary line warns that the image may be
//!   torn. The framebuffer is write-combined: reads are slow there, but
//!   this reads it once. In desktop mode (#153) the source is a copy the
//!   compositor makes between passes into a buffer the command allocated,
//!   so it never shows the pointer; the compositor fills the rows directly.
//! - **Encoding** ([`encode`]): on a thread with an [`ENCODE_STACK`] stack
//!   (the compressor needs about 75 KiB, more than a thread's default),
//!   every buffer refused rather than aborted when the heap is out
//!   (`lumen_png::encode`).
//!
//! The frame is then written a line at a time, each line whole, so log
//! lines may land between the frame's lines, never inside one; the harness
//! skips them by prefix.

use crate::shell::Ctx;
use crate::term;
use alloc::string::String;
use core::fmt;
use lumen_png::encode::{self as png, Png, Rows};
use lumen_time::Millis;

/// The encoding thread's stack: the compressor keeps 64 KiB inline and
/// needs about 75 KiB in all (as `script.rs` runs rhai on a bigger one).
const ENCODE_STACK: u64 = 256 * 1024;

/// Why there is no screenshot.
pub enum Error {
    /// No terminal on a framebuffer (none usable, or one too small), or a
    /// crash report took the screen.
    NoScreen,
    Png(png::Error),
    /// No thread to encode on.
    NoThread(crate::sched::SpawnError),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoScreen => write!(f, "no screen to capture"),
            Error::Png(e) => write!(f, "{e}"),
            Error::NoThread(e) => write!(f, "no thread to encode on: {e}"),
        }
    }
}

/// The screen's pixels, captured.
pub struct Capture {
    pub rows: Rows,
    /// A canvas drew meanwhile: the image may be torn.
    pub canvas_drawing: bool,
    /// How long reading them took (under the screen's lock).
    pub read_ns: u64,
}

/// Capture the framebuffer (the text console's screen).
pub fn capture_framebuffer() -> Result<Capture, Error> {
    let size = term::read_pixels(|fb| (fb.width(), fb.height())).ok_or(Error::NoScreen)?.value;
    let dimension = |n: usize| u32::try_from(n).map_err(|_| Error::Png(png::Error::TooLarge));
    // Allocated before taking the lock (nothing allocates under it); the
    // framebuffer's size never changes.
    let mut rows = Rows::new(dimension(size.0)?, dimension(size.1)?).map_err(Error::Png)?;
    let start = crate::time::now_ns();
    let read = term::read_pixels(|fb| {
        for y in 0..rows.height() {
            fb.rgb_row(y as usize, rows.row_mut(y));
        }
    })
    .ok_or(Error::NoScreen)?;
    let read_ns = crate::time::now_ns().saturating_sub(start);
    Ok(Capture { rows, canvas_drawing: read.canvas_drawing, read_ns })
}

/// Encode `rows` as PNG on a thread with an [`ENCODE_STACK`] stack; with
/// how long it took.
pub fn encode(rows: Rows) -> Result<(Png, u64), Error> {
    let mut result = None;
    let start = crate::time::now_ns();
    crate::sched::run_sized("screenshot", ENCODE_STACK, || result = Some(rows.encode())).map_err(Error::NoThread)?;
    let ns = crate::time::now_ns().saturating_sub(start);
    // `run_sized` returned Ok: the closure ran to the end.
    let png = result.expect("run_sized ran the encoder").map_err(Error::Png)?;
    Ok((png, ns))
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
    let shot = capture_framebuffer().and_then(|c| Ok((encode(c.rows)?, c.canvas_drawing, c.read_ns)));
    let ((png, encode_ns), canvas_drawing, read_ns) = match shot {
        Ok(shot) => shot,
        Err(e) => return ctx.fail(format_args!("screenshot: {e}")),
    };
    let start = crate::time::now_ns();
    if lumen_png::frame::write(&png, |line| ctx.out.write_str(line)).is_err() {
        return ctx.fail("screenshot: the frame couldn't be written");
    }
    let sent_ns = crate::time::now_ns().saturating_sub(start);
    let _ = writeln!(
        ctx.out,
        "screenshot: {}x{}, {} bytes of PNG (read {}, encoded {}, sent {}){}",
        png.width(),
        png.height(),
        png.bytes().len(),
        Millis(read_ns),
        Millis(encode_ns),
        Millis(sent_ns),
        if canvas_drawing { ", a canvas was drawing: may be torn" } else { "" }
    );
}
