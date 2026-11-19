//! What Lumen's test apps share (#199): the `lumen` host functions as raw
//! imports, a log helper, [`manifest!`], a panic handler that reports the
//! message through `panic`, and a bump allocator in the app's fixed memory.
//! Not the SDK (that is #206's `lumen-app`): just enough for the runtime's
//! tests to call every host function, right and wrong.

#![no_std]

extern crate alloc;

use alloc::string::String;
use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicI32, AtomicUsize, Ordering};

pub use lumen_app_api as api;
pub use lumen_app_api::{Error, Grant};

/// The `lumen` host functions (ARCHITECTURE section 8), as the module
/// imports them. Pointers and lengths are wasm `i32`s.
pub mod sys {
    #[link(wasm_import_module = "lumen")]
    unsafe extern "C" {
        pub fn handle_find(kind: i32) -> i32;
        pub fn handle_close(h: i32) -> i32;
        pub fn args_len() -> i32;
        pub fn args_read(buf: i32, len: i32) -> i32;
        pub fn exit(code: i32) -> !;
        pub fn panic(msg: i32, len: i32) -> !;
        pub fn log(h: i32, level: i32, msg: i32, len: i32) -> i32;
        pub fn time_now(h: i32) -> i64;
        pub fn time_wall(h: i32) -> i64;
        pub fn time_sleep(h: i32, ns: i64) -> i32;
        pub fn trace_cpus(h: i32) -> i32;
        pub fn trace_read(h: i32, stream: i32, cpu: i32, since: i64, buf: i32, len: i32) -> i32;
    }
}

/// The host functions, safe to call: the host checks every pointer and
/// length against the app's memory (ARCHITECTURE section 8), so a wrong
/// one is an error code, never memory the app didn't mean to share.
pub mod host {
    use super::sys;

    macro_rules! wrap {
        ($(fn $name:ident($($arg:ident: $ty:ty),*) -> $ret:ty;)*) => {$(
            pub fn $name($($arg: $ty),*) -> $ret {
                // SAFETY: the host checks every range it is given before
                // it reads or writes a byte of the app's memory.
                unsafe { sys::$name($($arg),*) }
            }
        )*};
    }

    wrap! {
        fn handle_find(kind: i32) -> i32;
        fn handle_close(h: i32) -> i32;
        fn args_len() -> i32;
        fn args_read(buf: i32, len: i32) -> i32;
        fn exit(code: i32) -> !;
        fn panic(msg: i32, len: i32) -> !;
        fn log(h: i32, level: i32, msg: i32, len: i32) -> i32;
        fn time_now(h: i32) -> i64;
        fn time_wall(h: i32) -> i64;
        fn time_sleep(h: i32, ns: i64) -> i32;
        fn trace_cpus(h: i32) -> i32;
        fn trace_read(h: i32, stream: i32, cpu: i32, since: i64, buf: i32, len: i32) -> i32;
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

static LOG: AtomicI32 = AtomicI32::new(0);

/// The `Log` handle (found once).
pub fn log_handle() -> i32 {
    let h = LOG.load(Ordering::Relaxed);
    if h > 0 {
        return h;
    }
    let h = host::handle_find(Grant::Log as i32);
    LOG.store(h, Ordering::Relaxed);
    h
}

/// Log `text` at `level` (1 error .. 5 trace): what `log` returned.
pub fn log_at(level: i32, text: &str) -> i32 {
    host::log(log_handle(), level, ptr(text.as_ptr()), text.len() as i32)
}

/// Log `text` at INFO.
pub fn info(text: &str) -> i32 {
    log_at(3, text)
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
    let mut buf = [0u8; 256];
    let n = host::args_read(ptr(buf.as_mut_ptr()), buf.len() as i32);
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
