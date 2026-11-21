//! Minimal `#![no_std]` dynamic shared library (`libm7.so`) for Lumen M7/M8 tests.
//!
//! Cross-compiled with `rustc --crate-type cdylib --target x86_64-unknown-linux-musl`
//! and `rust-lld` against Alpine's `ld-musl-x86_64.so.1`, exporting C ABI symbols
//! exercised via `dlopen`/`dlsym`.

#![no_std]

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn m7_abi_version() -> u32 {
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn m7_add(a: i32, b: i32) -> i32 {
    a.wrapping_add(b)
}
