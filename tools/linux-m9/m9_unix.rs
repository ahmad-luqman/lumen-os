//! `m9_unix`: M9's ring-3 checks of `AF_UNIX`, `SCM_RIGHTS` and
//! `SCM_CREDENTIALS` (`tests/scenarios/test_net.py`; ARCHITECTURE section 15,
//! "M9's tests").
//!
//! B0 (#471) builds and stages the empty skeleton; C1 and C2 add their modes
//! and checks.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

#[macro_use]
mod common;

use core::ffi::c_int;

const USAGE: &str = "usage: m9_unix <mode>";

#[unsafe(no_mangle)]
pub unsafe extern "C" fn main(argc: c_int, argv: *const *const u8) -> c_int {
    // SAFETY: `main`'s `argc`/`argv` come from musl's startup over the kernel's
    // initial stack and stay valid for the life of the program.
    let (args, n) = unsafe { common::args(argc, argv) };
    let _ = &args[..n];
    say!("{USAGE}");
    common::USAGE_EXIT
}
