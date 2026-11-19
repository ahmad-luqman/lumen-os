//! What Lumen's test apps share (#199): the `lumen` host functions as
//! imports, a log helper, [`manifest!`], a panic handler that reports the
//! message through `panic`, and a bump allocator in the app's fixed memory.
//! Not the SDK (that is #206's `lumen-app`): just enough for the runtime's
//! tests to call every host function, right and wrong.

#![no_std]

extern crate alloc;

use alloc::string::String;
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

pub use lumen_app_api as api;
pub use lumen_app_api::{Error, Grant, LogLevel};

/// The `lumen` host functions (ARCHITECTURE section 8), as the module
/// imports them. Pointers and lengths are wasm `i32`s. The host checks
/// every range against the app's memory before it touches a byte, so the
/// calls that only read the app's memory (or none) are safe: a wrong
/// pointer is an error code (or, for `panic`, which ends the app anyway,
/// the message `<a bad pointer>`). The two that write into it are
/// `unsafe`.
pub mod host {
    #[link(wasm_import_module = "lumen")]
    unsafe extern "C" {
        pub safe fn handle_find(kind: i32) -> i32;
        pub safe fn handle_close(h: i32) -> i32;
        pub safe fn args_len() -> i32;
        /// Writes the arguments into `[buf, buf + len)`.
        ///
        /// # Safety
        ///
        /// The caller owns that range (a buffer it may write), whatever the
        /// call returns: Rust knows nothing of the host's writes.
        pub unsafe fn args_read(buf: i32, len: i32) -> i32;
        pub safe fn exit(code: i32) -> !;
        pub safe fn panic(msg: i32, len: i32) -> !;
        pub safe fn log(h: i32, level: i32, msg: i32, len: i32) -> i32;
        pub safe fn time_now(h: i32) -> i64;
        pub safe fn time_wall(h: i32) -> i64;
        pub safe fn time_sleep(h: i32, ns: i64) -> i32;
        pub safe fn trace_cpus(h: i32) -> i32;
        /// Writes the trace header and events into `[buf, buf + len)`.
        ///
        /// # Safety
        ///
        /// As [`args_read`]: the caller owns that range.
        pub unsafe fn trace_read(h: i32, stream: i32, cpu: i32, since: i64, buf: i32, len: i32) -> i32;
    }
}

/// The app's manifest: `manifest!(b"caps=time")` puts the bytes in a
/// `lumen.manifest` custom section.
#[macro_export]
macro_rules! manifest {
    ($caps:literal) => {
        #[unsafe(link_section = "lumen.manifest")]
        #[used]
        static LUMEN_MANIFEST: [u8; $caps.len()] = *$caps;
    };
}

/// A pointer as the `i32` a host function takes.
pub fn ptr<T: ?Sized>(p: *const T) -> i32 {
    p as *const u8 as usize as i32
}

/// The linear memory's size in bytes.
pub fn memory_bytes() -> usize {
    core::arch::wasm32::memory_size(0) * 65536
}

/// The handle to `grant` (`handle_find` gives the same one every time), or
/// the error code.
pub fn find(grant: Grant) -> i32 {
    host::handle_find(grant as i32)
}

/// The `Log` handle.
pub fn log_handle() -> i32 {
    find(Grant::Log)
}

/// Log `text` at INFO: what `log` returned.
pub fn info(text: &str) -> i32 {
    host::log(log_handle(), LogLevel::Info as i32, ptr(text.as_ptr()), text.len() as i32)
}

/// An error code's name, or the value when it is none (`>= 0`).
pub fn named(result: i64) -> String {
    match Error::from_returned(result) {
        Some(e) => String::from(e.name()),
        None => alloc::format!("{result}"),
    }
}

/// `run`'s arguments after the app's name, as one string (words joined by
/// spaces), or empty.
pub fn args() -> String {
    let mut buf = [0u8; api::limits::ARGS_BYTES];
    // SAFETY: `buf` is ours to write.
    let n = unsafe { host::args_read(ptr(buf.as_mut_ptr()), buf.len() as i32) };
    if n <= 0 {
        return String::new();
    }
    let words = lumen_app_api::args::words(&buf[..n as usize]);
    let mut out = String::new();
    for w in words {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(core::str::from_utf8(w).unwrap_or("?"));
    }
    out
}

/// A test app's panic: the message, through `panic` (the runtime ends the
/// app with it).
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    let msg = alloc::format!("{}", info.message());
    host::panic(ptr(msg.as_ptr()), msg.len() as i32)
}

/// The heap the bump allocator hands out: inside the app's fixed memory.
const HEAP_BYTES: usize = 64 * 1024;

struct Bump {
    heap: UnsafeCell<[u8; HEAP_BYTES]>,
    used: AtomicUsize,
}

// SAFETY: a wasm app has one thread.
unsafe impl Sync for Bump {}

// SAFETY: hands out disjoint, aligned blocks inside `heap`; never frees
// (a test app is short-lived), and returns null when it is used up.
unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let base = self.heap.get() as usize;
        let used = self.used.load(Ordering::Relaxed);
        let start = (base + used).next_multiple_of(layout.align());
        let end = start + layout.size();
        if end > base + HEAP_BYTES {
            return core::ptr::null_mut();
        }
        self.used.store(end - base, Ordering::Relaxed);
        start as *mut u8
    }

    unsafe fn dealloc(&self, _: *mut u8, _: Layout) {}
}

#[global_allocator]
static ALLOC: Bump = Bump { heap: UnsafeCell::new([0; HEAP_BYTES]), used: AtomicUsize::new(0) };
