//! Minimal `#![no_std]` dynamically linked musl x86-64 test executable for Lumen M7/M8.
//!
//! Cross-compiled with `rustc --target x86_64-unknown-linux-musl` + `rust-lld`
//! (`-C target-feature=-crt-static`) against Alpine's `ld-musl-x86_64.so.1`,
//! using musl's `Scrt1.o` (`__libc_start_main`) to enter `main`, and exercising
//! musl `write` plus `dlopen`/`dlsym`/`dlclose` against `libm7.so`.

#![no_std]
#![no_main]

use core::ffi::{c_char, c_int, c_void};

const RTLD_NOW: c_int = 2;

unsafe extern "C" {
    fn write(fd: c_int, buf: *const u8, count: usize) -> isize;
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn _exit(status: c_int) -> !;
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { _exit(127) }
}

fn write_all(fd: c_int, msg: &[u8]) {
    let mut off = 0;
    while off < msg.len() {
        let n = unsafe { write(fd, msg.as_ptr().add(off), msg.len() - off) };
        if n <= 0 {
            break;
        }
        off += n as usize;
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main(_argc: c_int, _argv: *const *const u8) -> c_int {
    write_all(1, b"m7_test: dynamic musl start\n");

    let handle = unsafe { dlopen(c"libm7.so".as_ptr(), RTLD_NOW) };
    if handle.is_null() {
        write_all(2, b"m7_test: dlopen(libm7.so) failed\n");
        return 1;
    }

    let add_sym = unsafe { dlsym(handle, c"m7_add".as_ptr()) };
    if add_sym.is_null() {
        write_all(2, b"m7_test: dlsym(m7_add) failed\n");
        unsafe {
            dlclose(handle);
        }
        return 2;
    }

    let ver_sym = unsafe { dlsym(handle, c"m7_abi_version".as_ptr()) };
    if ver_sym.is_null() {
        write_all(2, b"m7_test: dlsym(m7_abi_version) failed\n");
        unsafe {
            dlclose(handle);
        }
        return 3;
    }

    let m7_add: extern "C" fn(i32, i32) -> i32 = unsafe { core::mem::transmute(add_sym) };
    let m7_abi_version: extern "C" fn() -> u32 = unsafe { core::mem::transmute(ver_sym) };

    if m7_abi_version() != 1 || m7_add(20, 22) != 42 {
        write_all(2, b"m7_test: unexpected libm7.so result\n");
        unsafe {
            dlclose(handle);
        }
        return 4;
    }

    if unsafe { dlclose(handle) } != 0 {
        write_all(2, b"m7_test: dlclose failed\n");
        return 5;
    }

    write_all(1, b"m7_test: dlopen/dlsym ok\n");
    0
}
