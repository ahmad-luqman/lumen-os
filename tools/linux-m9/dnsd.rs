//! `dnsd`: loopback UDP DNS responder (A and AAAA for one name given on the
//! command line; `tests/scenarios/test_net.py`, ARCHITECTURE section 15).
//!
//! B0 (#471) builds and stages the empty skeleton; C3 implements the responder.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

#[macro_use]
mod common;

use core::ffi::c_int;

const USAGE: &str = "usage: dnsd <name> <ipv4> [ipv6]";

#[unsafe(no_mangle)]
pub extern "C" fn main() -> c_int {
    say!("{USAGE}");
    common::USAGE_EXIT
}
