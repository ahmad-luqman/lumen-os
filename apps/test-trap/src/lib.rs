//! `trap` (#199): ends by a fault. `unreachable`: a wasm trap. `panic`: a
//! panic whose message holds a newline and a control character (the end
//! line shows U+FFFD for each). `depth`: recursion past the call depth.

#![no_std]

use core::hint::black_box;

testkit::manifest!(b"caps=");

#[inline(never)]
#[allow(unconditional_recursion, reason = "recursing past the call depth is the point")]
fn deeper(n: u64) -> u64 {
    black_box(deeper(black_box(n + 1)) + 1)
}

#[unsafe(no_mangle)]
pub extern "C" fn lumen_main() -> i32 {
    match testkit::args().as_str() {
        "unreachable" => core::arch::wasm32::unreachable(),
        "panic" => panic!("boom\nWARN  app: forged\x07"),
        "depth" => black_box(deeper(0)) as i32,
        _ => {
            testkit::info("trap: unreachable, panic or depth?");
            2
        }
    }
}
