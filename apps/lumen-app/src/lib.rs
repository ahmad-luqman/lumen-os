//! Lumen's app SDK (#206; ARCHITECTURE section 8, "Apps: WASM modules in
//! the kernel"): what an app needs besides its own logic.
//!
//! - [`main!`] names the app's entry point (`lumen_main`), [`manifest!`]
//!   writes its manifest (`lumen_app::manifest!(time, desktop)`);
//! - a global allocator over the app's fixed memory (`heap`), and a panic
//!   handler that reports the message through `panic` (the runtime ends the
//!   app with it: one WARN);
//! - safe wrappers over the host functions (`args_len` is `args`' own, and
//!   `handle_close` is reached by dropping a [`Window`]): [`log`]'s
//!   functions, [`Time`],
//!   [`Desktop`] (opening a window, measuring text), [`Window`] (its canvas,
//!   commits, events, text drawn into the canvas), [`Trace`] (X-ray's
//!   reads), [`args`] and [`exit`];
//! - [`ui`] (#207): `lumen-ui` inside the app, the kernel's toolkit: its
//!   text through the host's text calls ([`ui::HostText`]), one step per
//!   event ([`ui::Toolkit`]: painted, committed, its tree published, then a
//!   request answered), so the app's window shows in `ui tree` and `ui
//!   invoke` reaches its widgets as a click does;
//! - [`api`] (`lumen-app-api`): the error codes, the limits, the fixed
//!   layouts, [`Event`], the tree and the answer.
//!
//! An app is a `no_std` `cdylib` (apps/template is one to copy: `make app
//! NAME=foo`). Float functions such as `f32::sqrt` aren't in `core`, and
//! `libm` isn't a dependency yet: apps avoid them (ARCHITECTURE section 8).

#![no_std]

extern crate alloc;

mod heap;
pub mod ui;

/// The `lumen` host functions as the module imports them (declared once,
/// in `lumen-app-api`); apps use the wrappers here.
pub use lumen_app_api::sys;

use alloc::string::String;
use alloc::vec::Vec;
use core::num::NonZeroU32;

pub use lumen_app_api as api;
pub use lumen_app_api::event::{Button, Buttons, Event, KeyAction, KeyCode, Mods, Palette, ThemeId, mods};
pub use lumen_app_api::layout::Rect;
pub use lumen_app_api::{Error, Grant, LogLevel};
pub use lumen_gfx::Surface;

use lumen_app_api::answer::AppAnswer;
use lumen_app_api::layout::{CANVAS_SIZE, Canvas, RECT_SIZE, TextDraw};
use lumen_app_api::limits::{ARGS_BYTES, COMMIT_RECTS, EVENT_BYTES, TEXT_BYTES, WINDOW_PIXELS};
use lumen_app_api::tree::{Tree, TreeError};
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
        Handle::from_result(sys::handle_find(grant as i32))
    }

    /// The handle a host call returned (`0` is never one).
    fn from_result(r: i32) -> Result<Handle> {
        NonZeroU32::new(check(r)?).map(Handle).ok_or(Error::BadHandle)
    }

    /// Its number, as the host functions take it.
    pub fn raw(self) -> i32 {
        self.0.get() as i32
    }
}

/// The kernel log: lines tagged with the app's name (`INFO  app: <name>:
/// <text>`). The kernel takes at most `TEXT_BYTES` a line (refusing more)
/// and shows at most `LOG_BYTES` of it, sanitised; these functions cut a
/// longer text to `TEXT_BYTES` on a char boundary first, so its start is
/// logged rather than nothing.
pub mod log {
    use super::{Error, Grant, Handle, LogLevel, TEXT_BYTES, check, ptr, sys};

    /// Log `text` (cut to `TEXT_BYTES`) at `level`.
    pub fn log(level: LogLevel, text: &str) -> Result<(), Error> {
        let h = Handle::find(Grant::Log)?;
        let text = lumen_app_api::text::prefix(text, TEXT_BYTES);
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

/// `run`'s arguments after the app's name; `BadArgument` if one isn't
/// UTF-8 (the shell only passes UTF-8, but a word isn't dropped silently).
pub fn args() -> Result<Vec<String>> {
    let mut buf = [0u8; ARGS_BYTES];
    // SAFETY: `buf` is ours to write.
    let n = check(unsafe { sys::args_read(ptr(buf.as_mut_ptr()), buf.len() as i32) })?;
    lumen_app_api::args::words(&buf[..n as usize])
        .map(|w| core::str::from_utf8(w).map(String::from).map_err(|_| Error::BadArgument))
        .collect()
}

/// An app's exit code from its body's result: 0, or 1 once `<what>: <why>`
/// is logged as an ERROR (`fn run() -> i32 { lumen_app::report(app()) }`,
/// each failing step naming itself with `map_err(|e| ("no window", e))`).
pub fn report(result: core::result::Result<(), (&str, Error)>) -> i32 {
    match result {
        Ok(()) => 0,
        Err((what, e)) => {
            log::error(&alloc::format!("{what}: {e}"));
            1
        }
    }
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

/// The per-CPU trace rings, read only (`caps` `trace`; X-ray, #208).
#[derive(Clone, Copy, Debug)]
pub struct Trace(Handle);

impl Trace {
    pub fn get() -> Result<Trace> {
        Handle::find(Grant::Trace).map(Trace)
    }

    /// One past the highest online CPU's index (an index below it may
    /// still be offline).
    pub fn cpus(&self) -> Result<u32> {
        check(sys::trace_cpus(self.0.raw()))
    }

    /// CPU `cpu`'s ring `stream` (`Main` or `Xray`) from sequence number
    /// `since` into `buf`: the header (`api::trace::decode_header`), then
    /// the events copied (`api::trace::decode_event`), whose count is the
    /// result.
    pub fn read(&self, stream: api::kind::Stream, cpu: u32, since: u64, buf: &mut [u8]) -> Result<u32> {
        let since = i64::try_from(since).map_err(|_| Error::BadArgument)?;
        let len = i32::try_from(buf.len()).unwrap_or(i32::MAX);
        // SAFETY: `buf` is ours to write, and the host writes at most `len`
        // bytes of it.
        let r = unsafe {
            sys::trace_read(self.0.raw(), stream as u32 as i32, cpu as i32, since, ptr(buf.as_mut_ptr()), len)
        };
        check(r)
    }
}

/// The desktop (`caps` `desktop`): opening a window, and the UI font.
#[derive(Clone, Copy, Debug)]
pub struct Desktop(Handle);

impl Desktop {
    pub fn get() -> Result<Desktop> {
        Handle::find(Grant::Desktop).map(Desktop)
    }

    /// A line's height at `px` (`TEXT_PX_MIN..=TEXT_PX_MAX`).
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
        // The host's size check, made before the canvas is reserved, so a
        // bad size is `BadArgument` and not `OutOfMemory`.
        let pixels = u64::from(w) * u64::from(h);
        if pixels == 0 || pixels > WINDOW_PIXELS {
            return Err(Error::BadArgument);
        }
        let canvas = Surface::with_stride(w, h, w as usize).ok_or(Error::OutOfMemory)?;
        let handle = Handle::from_result(sys::window_open(
            self.0.raw(),
            ptr(title.as_ptr()),
            title.len() as i32,
            w as i32,
            h as i32,
        ))?;
        Ok(Window { handle, desktop: *self, canvas })
    }
}

/// Why a tree wasn't published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishError {
    /// It doesn't encode within `TREE_BYTES` (or breaks a rule).
    Tree(TreeError),
    /// The host refused it.
    Host(Error),
}

impl core::fmt::Display for PublishError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PublishError::Tree(e) => write!(f, "the tree: {e}"),
            PublishError::Host(e) => write!(f, "the host: {e}"),
        }
    }
}

/// A surface as the host's commit call takes a canvas (read only).
fn canvas_of(s: &Surface) -> [u8; CANVAS_SIZE] {
    Canvas { ptr: ptr(s.pixels().as_ptr()) as u32, stride: s.stride() as u32, w: s.width(), h: s.height() }.encode()
}

/// A surface as `text_draw` takes a canvas it writes: the pointer from a
/// mutable borrow (the host writes the pixels).
fn canvas_of_mut(s: &mut Surface) -> [u8; CANVAS_SIZE] {
    let (stride, w, h) = (s.stride() as u32, s.width(), s.height());
    Canvas { ptr: ptr(s.pixels_mut().as_mut_ptr()) as u32, stride, w, h }.encode()
}

/// The app's window: its canvas (`w` × `h` pixels, `0x00RRGGBB`, row after
/// row, a `lumen_gfx::Surface`) is the app's to draw in; [`Window::commit`]
/// shows parts of it. Dropping it closes the window.
pub struct Window {
    handle: Handle,
    desktop: Desktop,
    canvas: Surface,
}

impl Window {
    pub fn width(&self) -> u32 {
        self.canvas.width()
    }

    pub fn height(&self) -> u32 {
        self.canvas.height()
    }

    /// The whole client area.
    pub fn bounds(&self) -> Rect {
        Rect::new(0, 0, self.width(), self.height())
    }

    /// The desktop it opened on (its text calls).
    pub fn desktop(&self) -> Desktop {
        self.desktop
    }

    /// The canvas's pixels, row after row.
    pub fn pixels_mut(&mut self) -> &mut [u32] {
        self.canvas.pixels_mut()
    }

    /// The canvas, as a surface to draw on (lumen-gfx's fills and blits,
    /// lumen-ui's painting).
    pub fn surface_mut(&mut self) -> &mut Surface {
        &mut self.canvas
    }

    /// The canvas as the host functions take it.
    fn layout(&self) -> [u8; CANVAS_SIZE] {
        canvas_of(&self.canvas)
    }

    /// Fill `r` (clipped to the canvas) with `rgb`.
    pub fn fill(&mut self, r: Rect, rgb: u32) {
        self.canvas.fill_rect(r.into(), rgb);
    }

    /// Draw `text` at `px` in `rgb` with its line box's top left at (`x`,
    /// `y`), touching only `clip`'s pixels: its advance.
    pub fn text_in(&mut self, x: i32, y: i32, px: u32, rgb: u32, text: &str, clip: Rect) -> Result<u32> {
        let args = TextDraw { x, y, px, rgb, clip }.encode();
        let canvas = canvas_of_mut(&mut self.canvas);
        // SAFETY: the canvas's pixels are ours (`self.canvas`, borrowed
        // mutably for the pointer).
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
            let mut raw = [0u8; COMMIT_RECTS * RECT_SIZE];
            for (slot, r) in raw.as_chunks_mut::<RECT_SIZE>().0.iter_mut().zip(chunk) {
                *slot = r.encode();
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

    /// Publish the window's automation tree: `ui tree` shows it from now
    /// on. A refused one (too big to encode, or refused by the host) leaves
    /// the last.
    pub fn publish(&self, tree: &Tree) -> core::result::Result<(), PublishError> {
        let bytes = tree.encode().map_err(PublishError::Tree)?;
        check(sys::window_publish(self.handle.raw(), ptr(bytes.as_ptr()), bytes.len() as i32))
            .map(|_| ())
            .map_err(PublishError::Host)
    }

    /// Its handle (for a test app's raw calls).
    pub fn handle(&self) -> Handle {
        self.handle
    }

    /// Answer the window's running request (the last `Event::Request`
    /// read); `BadArgument` with none running.
    pub fn answer(&self, answer: &AppAnswer) -> Result<()> {
        let bytes = answer.encode().map_err(|_| Error::BadArgument)?;
        check(sys::window_answer(self.handle.raw(), ptr(bytes.as_ptr()), bytes.len() as i32)).map(|_| ())
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

/// The most bytes of a panic's report: what the kernel shows of it.
const PANIC_BYTES: usize = lumen_app_api::limits::LOG_BYTES;

/// Text formatted into `bytes`, up to `cap` of them, cut on a char
/// boundary (the rest dropped): no stray bytes for the kernel to show as
/// U+FFFD, which would push the end of the report past what it shows.
struct Buf {
    bytes: [u8; PANIC_BYTES],
    len: usize,
    cap: usize,
}

impl core::fmt::Write for Buf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let mut n = s.len().min(self.cap - self.len);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        self.bytes[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// The app's panic: its message (cut so it fits) and ` at <file>:<line>`,
/// through `panic` (the runtime ends the app with it: one WARN). Formatted
/// into fixed buffers: a panic can come from the allocator running out.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    use core::fmt::Write;
    let mut place = Buf { bytes: [0; PANIC_BYTES], len: 0, cap: PANIC_BYTES / 2 };
    if let Some(l) = info.location() {
        let _ = write!(place, " at {}:{}", l.file(), l.line());
    }
    let mut out = Buf { bytes: [0; PANIC_BYTES], len: 0, cap: PANIC_BYTES - place.len };
    let _ = write!(out, "{}", info.message());
    out.cap = PANIC_BYTES;
    let _ = out.write_str(core::str::from_utf8(&place.bytes[..place.len]).unwrap_or(""));
    sys::panic(ptr(out.bytes.as_ptr()), out.len as i32)
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
