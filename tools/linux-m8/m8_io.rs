//! `m8_io`: M8's ring-3 checks of pipes, FIFOs, `poll`/`select` and PTYs
//! (`tests/scenarios/test_linux_io.py`).
//!
//! A `#![no_std]` dynamically linked musl x86-64 executable, built as
//! `tools/linux-m7/`'s are (the Makefile) into `build/linux/`, which both
//! images stage in `/usr/local/bin`. Its checks and their conventions:
//! `common.rs` and ARCHITECTURE section 15, "M8's tests". B0 (#371) laid it
//! down with no modes: until an issue adds one, every invocation prints the
//! usage and exits 64.

#![no_std]
#![no_main]

#[macro_use]
mod common;

use core::ffi::c_int;

const USAGE: &str = "usage: m8_io <mode> (no modes yet)";

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const u8) -> c_int {
    // SAFETY: musl's `__libc_start_main` passes the kernel's `argc` and
    // `argv`, kept for the program's life.
    let (args, n) = unsafe { common::args(argc, argv) };
    match &args[..n] {
        // Each mode an arm here, as `m7_vfs`'s.
        _ => {
            say!("{USAGE}");
            common::USAGE_EXIT
        }
    }
}
