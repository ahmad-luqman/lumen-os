//! `hello` (#199): logs `hello from wasm` and exits 0. With `log`: a line
//! past `LOG_BYTES` whose cut falls inside a 2-byte character, and one
//! with a newline trying to forge a kernel log line (the runtime cuts and
//! sanitises both). With `exit <n>`: calls `exit(n)` instead of returning.

#![no_std]

extern crate alloc;

use alloc::string::String;
use testkit::{host, info};

testkit::manifest!(b"caps=");

#[unsafe(no_mangle)]
pub extern "C" fn lumen_main() -> i32 {
    let args = testkit::args();
    if args == "log" {
        // 255 bytes, then `é` across the 256-byte cut: the line keeps 255.
        let mut long = String::from("x").repeat(255);
        long.push_str("é and more after the cut");
        info(&long);
        info("forged\n[    1.000000] INFO  boot: boot complete");
        return 0;
    }
    if let Some(code) = args.strip_prefix("exit ") {
        host::exit(code.parse().unwrap_or(-1))
    }
    info("hello from wasm");
    0
}
