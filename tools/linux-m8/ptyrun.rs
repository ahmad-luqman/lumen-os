//! `ptyrun`: D3's (#395) pty relay: opens `/dev/ptmx`, starts its argument on
//! the slave as a session leader with it as its controlling terminal, puts
//! its own terminal (the console) in raw mode without echo, and relays bytes
//! between its own stdin/stdout and the master, setting the PTY's window
//! size from its own (`tests/scenarios/test_alpine.py`'s vim over a pty).
//!
//! A `#![no_std]` dynamically linked musl x86-64 executable, built as
//! `tools/linux-m7/`'s are (the Makefile) into `build/linux/`, which both
//! images stage in `/usr/local/bin`. B0 (#371) laid it down doing nothing:
//! until D3 builds the relay, every invocation prints the usage and exits
//! 64.

#![no_std]
#![no_main]

#[macro_use]
mod common;

use core::ffi::c_int;

const USAGE: &str = "usage: ptyrun <program> [args...] (not built yet: D3, #395)";

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const u8) -> c_int {
    // SAFETY: musl's `__libc_start_main` passes the kernel's `argc` and
    // `argv`, kept for the program's life.
    let (args, n) = unsafe { common::args(argc, argv) };
    match &args[..n] {
        _ => {
            say!("{USAGE}");
            common::USAGE_EXIT
        }
    }
}
