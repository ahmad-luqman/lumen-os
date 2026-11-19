//! `trap` (#199): ends by a fault. `unreachable`: a wasm trap. `panic`: a
//! panic whose message holds a newline and a control character (the end
//! line shows U+FFFD for each). `depth`: recursion past the call depth.
//! `badptr`: `panic` with a message running past the memory's end (the
//! end line says `<a bad pointer>`). `badtext`: `panic` with 2000 bytes
//! claimed near the memory's end, invalid UTF-8 among them: only
//! `TEXT_BYTES` are read (so the range fits), the bad byte shown as U+FFFD,
//! the line cut at `LOG_BYTES`.

#![no_std]

use core::hint::black_box;
use testkit::{host, memory_bytes};

testkit::manifest!(b"caps=");

#[inline(never)]
#[allow(unconditional_recursion, reason = "recursing past the call depth is the point")]
fn deeper(n: u64) -> u64 {
    black_box(deeper(black_box(n + 1)) + 1)
}

/// `panic` with `bad\xff` then `y`s, 1500 bytes before the memory's end,
/// claiming 2000.
fn bad_text() -> ! {
    let at = memory_bytes() - 1500;
    // SAFETY: the memory's last 1500 bytes lie past the stack, the data and
    // the bump heap (which is in the data): nothing of the app uses them.
    unsafe {
        let p = at as *mut u8;
        core::ptr::copy_nonoverlapping(b"bad\xff".as_ptr(), p, 4);
        core::ptr::write_bytes(p.add(4), b'y', 1496);
    }
    host::panic(at as i32, 2000)
}

#[unsafe(no_mangle)]
pub extern "C" fn lumen_main() -> i32 {
    match testkit::args().as_str() {
        "unreachable" => core::arch::wasm32::unreachable(),
        "panic" => panic!("boom\nWARN  app: forged\x07"),
        "depth" => black_box(deeper(0)) as i32,
        "badptr" => host::panic(memory_bytes() as i32 - 10, 100),
        "badtext" => bad_text(),
        _ => {
            testkit::info("trap: unreachable, panic, depth, badptr or badtext?");
            2
        }
    }
}
