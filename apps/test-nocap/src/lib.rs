//! `nocap` (#199): asks for `time`, which its manifest lists but `run
//! --without time` withholds, and calls `log` with a handle it made up;
//! logs what each gave, then exits 0 (an error code, never a trap).

#![no_std]

extern crate alloc;

use alloc::format;
use testkit::{Grant, host, info, named};

testkit::manifest!(b"caps=time");

#[unsafe(no_mangle)]
pub extern "C" fn lumen_main() -> i32 {
    let time = host::handle_find(Grant::Time as i32);
    let time = if time > 0 { String::from("granted") } else { named(time.into()) };
    info(&format!("nocap: time: {time}"));
    let text = "made up";
    let r = host::log(999, 3, testkit::ptr(text.as_ptr()), text.len() as i32);
    info(&format!("nocap: made-up handle: {}", named(r.into())));
    0
}

use alloc::string::String;
