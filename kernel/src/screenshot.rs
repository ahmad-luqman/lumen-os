//! `screenshot`: the screen as a PNG, out over serial in a text frame
//! (`lumen_png::frame`; ARCHITECTURE section 11, "Screenshot"). To disk
//! once there are files (M6).
//!
//! Two steps, kept apart so the desktop can bring its own source:
//!
//! - **Capture** ([`capture`]): the desktop's back buffer while it is
//!   shown, else [`capture_framebuffer`]: the framebuffer's pixels into
//!   `lumen_png` rows allocated beforehand, read once under the screen's
//!   lock ([`term::read_pixels`]), so terminal output, a cursor flip or a
//!   pointer move can't land halfway through (by design; not tested). A
//!   command holding a `term::Canvas` draws without that lock: the capture
//!   then says a command held the screen, and the summary line warns that
//!   the image may be torn. The framebuffer is write-combined: reads are slow there, but
//!   this reads it once. While the desktop is shown (#153) the source is a
//!   copy the compositor makes between passes into rows the command
//!   allocated (`desktop::capture`), so it never shows the pointer.
//! - **Encoding** ([`encode`]): on a thread with an [`ENCODE_STACK`] stack
//!   (the compressor needs about 72 KiB, more than a thread's default),
//!   the rows' and the file's buffers refused rather than aborted when the
//!   heap is out, the compressor's own fixed boxes excepted
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
/// needs about 72 KiB in all (as `script.rs` runs rhai on a bigger one).
const ENCODE_STACK: u64 = 256 * 1024;

/// Why there is no screenshot.
pub enum Error {
    /// No terminal on a framebuffer (none usable, or one too small), or a
    /// crash report took the screen.
    NoScreen,
    Png(png::Error),
    /// No thread to encode on.
    NoThread(crate::sched::SpawnError),
    /// The desktop's compositor didn't copy its back buffer (why).
    Desktop(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoScreen => write!(f, "no screen to capture"),
            Error::Png(e) => write!(f, "{e}"),
            Error::NoThread(e) => write!(f, "no thread to encode on: {e}"),
            Error::Desktop(why) => write!(f, "the desktop's back buffer: {why}"),
        }
    }
}

/// The screen's pixels, captured.
pub struct Capture {
    pub rows: Rows,
    /// A command held the screen (a canvas) during the capture: it draws
    /// without the screen's lock, so the image may be torn.
    pub canvas_held: bool,
    /// How long filling the rows took (under the screen's lock).
    pub read_ns: u64,
}

/// Capture the screen: the desktop's back buffer while it is shown (the
/// compositor copies it into the rows between passes, so the pointer is
/// never in it), the framebuffer otherwise (paused, or no desktop).
pub fn capture() -> Result<Capture, Error> {
    if !crate::desktop::is_shown() {
        return capture_framebuffer();
    }
    let (w, h) = term::screen_size().ok_or(Error::NoScreen)?;
    let dimension = |n: usize| u32::try_from(n).map_err(|_| Error::Png(png::Error::TooLarge));
    let rows = Rows::new(dimension(w)?, dimension(h)?).map_err(Error::Png)?;
    match crate::desktop::capture(rows) {
        Some(Ok((rows, read_ns))) => Ok(Capture { rows, canvas_held: false, read_ns }),
        Some(Err(why)) => Err(Error::Desktop(why)),
        // Left meanwhile: the text screen it is.
        None => capture_framebuffer(),
    }
}

/// Capture the framebuffer (the text console's screen).
pub fn capture_framebuffer() -> Result<Capture, Error> {
    let (w, h) = term::screen_size().ok_or(Error::NoScreen)?;
    let dimension = |n: usize| u32::try_from(n).map_err(|_| Error::Png(png::Error::TooLarge));
    // Allocated before taking the lock (nothing allocates under it); the
    // framebuffer's size never changes.
    let mut rows = Rows::new(dimension(w)?, dimension(h)?).map_err(Error::Png)?;
    let start = crate::time::now_ns();
    let canvas_held = term::read_pixels(|fb| {
        for y in 0..rows.height() {
            fb.rgb_row(y as usize, rows.row_mut(y));
        }
    })
    .ok_or(Error::NoScreen)?;
    let read_ns = crate::time::now_ns().saturating_sub(start);
    Ok(Capture { rows, canvas_held, read_ns })
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
    let capture = match capture() {
        Ok(capture) => capture,
        Err(e) => return ctx.fail(format_args!("screenshot: {e}")),
    };
    let (png, encode_ns) = match encode(capture.rows) {
        Ok(encoded) => encoded,
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
        Millis(capture.read_ns),
        Millis(encode_ns),
        Millis(sent_ns),
        if capture.canvas_held { ", a command held the screen (a canvas): may be torn" } else { "" }
    );
}

crate::ktest!(
    fn encodes_from_a_thread_with_the_default_stack() {
        use crate::kassert;
        use core::sync::atomic::{AtomicBool, Ordering};
        // The compressor needs about 72 KiB of stack, more than a thread's
        // default (`mm::stack::STACK_SIZE`, 64 KiB): `encode` runs it on a
        // thread of its own. Called from a thread with the default stack,
        // as a shell's command is, it must still come back with a PNG
        // (without the bigger thread it would run into the guard page: a
        // crash report).
        static ENCODED: AtomicBool = AtomicBool::new(false);
        let rows = Rows::new(1280, 800).map_err(|_| "no memory for the rows")?;
        let h = crate::sched::spawn("screenshot-test", move || {
            let ok = encode(rows).is_ok_and(|(png, _)| {
                (png.width(), png.height()) == (1280, 800) && png.bytes().starts_with(&png::SIGNATURE)
            });
            ENCODED.store(ok, Ordering::Release);
        })
        .map_err(|_| "no thread for the test")?;
        crate::sched::join_bounded(h)?;
        kassert!(ENCODED.load(Ordering::Acquire));
        Ok(())
    }
);
