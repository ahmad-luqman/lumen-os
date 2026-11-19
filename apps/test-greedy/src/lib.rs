//! `greedy` (#199): its memory is fixed, so `memory.grow` gives -1; it
//! logs its page count and that, then panics, as an app out of memory
//! would.

#![no_std]

use core::hint::black_box;

testkit::manifest!(b"caps=");

#[unsafe(no_mangle)]
pub extern "C" fn lumen_main() -> i32 {
    let pages = core::arch::wasm32::memory_size(0);
    let grown = core::arch::wasm32::memory_grow(0, black_box(1)) as isize;
    testkit::info(&alloc::format!("greedy: {pages} pages, memory.grow(1) gave {grown}"));
    panic!("out of memory");
}

extern crate alloc;
