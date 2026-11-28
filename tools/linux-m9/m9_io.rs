//! `m9_io`: M9's ring-3 checks of `epoll`, `eventfd` and `timerfd`
//! (`tests/scenarios/test_linux_io.py`; ARCHITECTURE section 15, "M9's tests").
//!
//! B0 (#471) builds and stages the empty skeleton; B5, B6 and C8 add their
//! modes and checks.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

#[macro_use]
mod common;

use core::ffi::c_int;

const USAGE: &str = "usage: m9_io <mode>";

#[unsafe(no_mangle)]
pub extern "C" fn main() -> c_int {
    say!("{USAGE}");
    common::USAGE_EXIT
}
