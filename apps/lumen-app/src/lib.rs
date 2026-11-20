//! Lumen's app SDK (#206; ARCHITECTURE section 8, "Apps: WASM modules in
//! the kernel"): what an app needs besides its own logic.
//!
//! - [`main!`] names the app's entry point (`lumen_main`), [`manifest!`]
//!   writes its manifest (`lumen_app::manifest!(time, desktop)`);
//! - a global allocator over the app's fixed memory (`heap`), and a panic
//!   handler that reports the message through `panic` (the runtime ends the
//!   app with it: one WARN);
//! - safe wrappers over the host functions: [`log`]'s functions, [`Time`],
//!   [`Desktop`] (opening a window, measuring text), [`Window`] (its canvas,
//!   commits, events, text drawn into the canvas), [`args`];
//! - [`api`] (`lumen-app-api`): the error codes, the limits, the fixed
//!   layouts and [`Event`].
//!
//! An app is a `no_std` `cdylib` (apps/template is one to copy: `make app
//! NAME=foo`). Float functions such as `f32::sqrt` aren't in `core`, and
//! `libm` isn't a dependency yet: apps avoid them (ARCHITECTURE section 8).

#![no_std]

extern crate alloc;

mod heap;
pub mod sys;

use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU32;

pub use lumen_app_api as api;
pub use lumen_app_api::event::{Event, KeyAction, KeyCode, Palette, ThemeId};
pub use lumen_app_api::layout::Rect;
pub use lumen_app_api::{Error, Grant, LogLevel};

use lumen_app_api::layout::{Canvas, TextDraw};
use lumen_app_api::limits::{ARGS_BYTES, COMMIT_RECTS, EVENT_BYTES};
use sys::ptr;

#[global_allocator]
static HEAP: heap::AppHeap = heap::AppHeap::new();

/// A host call's result.
pub type Result<T> = core::result::Result<T, Error>;

/// A host call's `i32` result: the value, or the error it negates.
fn check(r: i32) -> Result<u32> {
    if r >= 0 { Ok(r as u32) } else { Err(Error::from_returned(i64::from(r)).unwrap_or(Error::BadArgument)) }
}

/// [`check`] for the calls returning `i64`.
fn check64(r: i64) -> Result<u64> {
    if r >= 0 { Ok(r as u64) } else { Err(Error::from_returned(r).unwrap_or(Error::BadArgument)) }
}

/// A capability handle: an opaque id the host gave (ARCHITECTURE section 8,
/// "Capabilities as handles").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handle(NonZeroU32);

impl Handle {
    /// The handle to `grant` (the same every time), or `NotGranted`.
    pub fn find(grant: Grant) -> Result<Handle> {
        let h = check(sys::handle_find(grant as i32))?;
        NonZeroU32::new(h).map(Handle).ok_or(Error::BadHandle)
    }

    /// Its number, as the host functions take it.
    pub fn raw(self) -> i32 {
        self.0.get() as i32
    }
}

/// The kernel log: lines tagged with the app's name (`INFO  app: <name>:
/// <text>`), at most `TEXT_BYTES` each, cut and sanitised by the kernel.
pub mod log {
    use super::{Error, Grant, Handle, LogLevel, check, ptr, sys};

    /// Log `text` at `level`.
    pub fn log(level: LogLevel, text: &str) -> Result<(), Error> {
        let h = Handle::find(Grant::Log)?;
        check(sys::log(h.raw(), level as i32, ptr(text.as_ptr()), text.len() as i32)).map(|_| ())
    }

    pub fn error(text: &str) {
        let _ = log(LogLevel::Error, text);
    }

    pub fn warn(text: &str) {
        let _ = log(LogLevel::Warn, text);
    }

    pub fn info(text: &str) {
        let _ = log(LogLevel::Info, text);
    }

    pub fn debug(text: &str) {
        let _ = log(LogLevel::Debug, text);
    }
}

/// `run`'s arguments after the app's name.
pub fn args() -> Vec<String> {
    let mut buf = [0u8; ARGS_BYTES];
    // SAFETY: `buf` is ours to write.
    let n = unsafe { sys::args_read(ptr(buf.as_mut_ptr()), buf.len() as i32) };
    let Ok(n) = check(n) else { return Vec::new() };
    lumen_app_api::args::words(&buf[..n as usize])
        .map(|w| String::from(core::str::from_utf8(w).unwrap_or("")))
        .collect()
}

/// End the app now, as if `lumen_main` returned `code`.
pub fn exit(code: i32) -> ! {
    sys::exit(code)
}

/// The clocks (`caps` `time`).
#[derive(Clone, Copy, Debug)]
pub struct Time(Handle);

impl Time {
    pub fn get() -> Result<Time> {
        Handle::find(Grant::Time).map(Time)
    }

    /// Nanoseconds since boot: the clock `Window::wait`'s deadline is on.
    pub fn now_ns(&self) -> Result<u64> {
        check64(sys::time_now(self.0.raw()))
    }

    /// Seconds since 1970-01-01 UTC (the RTC).
    pub fn wall(&self) -> Result<u64> {
        check64(sys::time_wall(self.0.raw()))
    }

    /// Wait `ns` nanoseconds (0 gives up the CPU).
    pub fn sleep(&self, ns: u64) -> Result<()> {
        check(sys::time_sleep(self.0.raw(), ns.min(i64::MAX as u64) as i64)).map(|_| ())
    }
}

/// The desktop (`caps` `desktop`): opening a window, and the UI font.
#[derive(Clone, Copy, Debug)]
pub struct Desktop(Handle);

impl Desktop {
    pub fn get() -> Result<Desktop> {
        Handle::find(Grant::Desktop).map(Desktop)
    }

    /// A line's height at `px` (8 to 96).
    pub fn line_height(&self, px: u32) -> Result<u32> {
        check(sys::text_line_height(self.0.raw(), px as i32))
    }

    /// `text`'s advance at `px`.
    pub fn measure(&self, px: u32, text: &str) -> Result<u32> {
        check(sys::text_measure(self.0.raw(), px as i32, ptr(text.as_ptr()), text.len() as i32))
    }

    /// Open the app's window (one per app): `w` × `h` client pixels titled
    /// `title`, in front and focused, with a canvas of its size to draw in.
    pub fn open_window(&self, title: &str, w: u32, h: u32) -> Result<Window> {
        let pixels = (w as usize).checked_mul(h as usize).ok_or(Error::BadArgument)?;
        let mut canvas = Vec::new();
        canvas.try_reserve_exact(pixels).map_err(|_| Error::OutOfMemory)?;
        canvas.resize(pixels, 0);
        let handle =
            check(sys::window_open(self.0.raw(), ptr(title.as_ptr()), title.len() as i32, w as i32, h as i32))?;
        let handle = NonZeroU32::new(handle).map(Handle).ok_or(Error::BadHandle)?;
        Ok(Window { handle, desktop: *self, canvas, w, h })
    }
}

/// The app's window: its canvas (`w` × `h` pixels, `0x00RRGGBB`, row after
/// row) is the app's to draw in; [`Window::commit`] shows parts of it.
/// Dropping it closes the window.
pub struct Window {
    handle: Handle,
    desktop: Desktop,
    canvas: Vec<u32>,
    w: u32,
    h: u32,
}

impl Window {
    pub fn width(&self) -> u32 {
        self.w
    }

    pub fn height(&self) -> u32 {
        self.h
    }

    /// The whole client area.
    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.w, self.h)
    }

    /// The canvas's pixels, row after row.
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        &mut self.canvas
    }

    /// The canvas as the host functions take it.
    fn layout(&self) -> [u8; 16] {
        Canvas { ptr: ptr(self.canvas.as_ptr()) as u32, stride: self.w, w: self.w, h: self.h }.encode()
    }

    /// Fill `r` (clipped to the canvas) with `rgb`.
    pub fn fill(&mut self, r: Rect, rgb: u32) {
        let Some(c) = r.clip(self.w, self.h) else { return };
        for y in c.y..c.y + c.h {
            let row = (y * self.w) as usize;
            self.canvas[row + c.x as usize..row + (c.x + c.w) as usize].fill(rgb);
        }
    }

    /// Draw `text` at `px` in `rgb` with its line box's top left at (`x`,
    /// `y`), touching only `clip`'s pixels: its advance.
    pub fn text_in(&mut self, x: i32, y: i32, px: u32, rgb: u32, text: &str, clip: Rect) -> Result<u32> {
        let args = TextDraw { x, y, px, rgb, clip }.encode();
        let canvas = self.layout();
        // SAFETY: the canvas's pixels are ours (`self.canvas`).
        let r = unsafe {
            sys::text_draw(
                self.desktop.0.raw(),
                ptr(canvas.as_ptr()),
                ptr(args.as_ptr()),
                ptr(text.as_ptr()),
                text.len() as i32,
            )
        };
        check(r)
    }

    /// [`Window::text_in`] with the whole canvas as the clip.
    pub fn text(&mut self, x: i32, y: i32, px: u32, rgb: u32, text: &str) -> Result<u32> {
        let all = self.bounds();
        self.text_in(x, y, px, rgb, text, all)
    }

    /// Show `rects` of the canvas (clipped to it; empty ones skipped), at
    /// most `COMMIT_RECTS` a call (more are committed in turns).
    pub fn commit(&self, rects: &[Rect]) -> Result<()> {
        let canvas = self.layout();
        for chunk in rects.chunks(COMMIT_RECTS) {
            let mut raw = [0u8; COMMIT_RECTS * 16];
            for (i, r) in chunk.iter().enumerate() {
                raw[i * 16..i * 16 + 16].copy_from_slice(&r.encode());
            }
            check(sys::window_commit(self.handle.raw(), ptr(canvas.as_ptr()), ptr(raw.as_ptr()), chunk.len() as i32))?;
        }
        Ok(())
    }

    /// Show the whole canvas.
    pub fn commit_all(&self) -> Result<()> {
        self.commit(&[self.bounds()])
    }

    /// Wait for an event until `deadline` (`Time::now_ns`'s clock; `None`:
    /// none): whether one is there to read. `animating`: the wait is part
    /// of an animation, so the desktop never counts the window idle meanwhile
    /// (it needs a deadline).
    pub fn wait(&self, deadline: Option<u64>, animating: bool) -> Result<bool> {
        let deadline = deadline.map_or(-1, |d| d.min(i64::MAX as u64) as i64);
        check(sys::window_wait(self.handle.raw(), deadline, i32::from(animating))).map(|r| r == 1)
    }

    /// The next event, if one is waiting (never blocks).
    pub fn next_event(&mut self) -> Result<Option<Event>> {
        let mut buf = [0u8; EVENT_BYTES];
        // SAFETY: `buf` is ours to write.
        let n = check(unsafe { sys::window_next_event(self.handle.raw(), ptr(buf.as_mut_ptr()), buf.len() as i32) })?;
        if n == 0 {
            return Ok(None);
        }
        Event::decode(&buf[..n as usize]).map(Some).map_err(|_| Error::BadArgument)
    }

    /// The next event, waiting for one as long as it takes.
    pub fn wait_event(&mut self) -> Result<Event> {
        loop {
            if let Some(e) = self.next_event()? {
                return Ok(e);
            }
            self.wait(None, false)?;
        }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        let _ = sys::handle_close(self.handle.raw());
    }
}

/// The app's panic: its message, through `panic` (the runtime ends the app
/// with it). Formatted into a fixed buffer: a panic can come from the
/// allocator running out.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    struct Buf {
        bytes: [u8; 256],
        len: usize,
    }
    impl core::fmt::Write for Buf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let n = s.len().min(self.bytes.len() - self.len);
            self.bytes[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
            self.len += n;
            Ok(())
        }
    }
    let mut buf = Buf { bytes: [0; 256], len: 0 };
    let _ = core::fmt::write(&mut buf, format_args!("{}", info.message()));
    sys::panic(ptr(buf.bytes.as_ptr()), buf.len as i32)
}

/// The app's entry point: `lumen_app::main!(run);` with `fn run() -> i32`
/// (its exit code; 0 is success).
#[macro_export]
macro_rules! main {
    ($main:path) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn lumen_main() -> i32 {
            $main()
        }
    };
}

/// The app's manifest (ARCHITECTURE section 8, "The manifest"):
/// `lumen_app::manifest!(time, desktop)` writes `caps=time,desktop` into the
/// `lumen.manifest` custom section; `manifest!()` writes `caps=`. Once per
/// app (a second one makes a section the kernel refuses).
#[macro_export]
macro_rules! manifest {
    () => {
        $crate::__manifest_section!("caps=");
    };
    ($first:ident $(, $rest:ident)* $(,)?) => {
        $crate::__manifest_section!(concat!("caps=", stringify!($first) $(, ",", stringify!($rest))*));
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __manifest_section {
    ($text:expr) => {
        #[unsafe(link_section = "lumen.manifest")]
        #[used]
        static LUMEN_MANIFEST: [u8; $text.len()] = {
            let text = $text.as_bytes();
            let mut out = [0u8; $text.len()];
            let mut i = 0;
            while i < out.len() {
                out[i] = text[i];
                i += 1;
            }
            out
        };
    };
}
