//! `busy` (#199): loops forever without a host call, so only the end of a
//! fuel slice (or the timer, preempting its thread) can stop it. With
//! `sleep`: blocks in `time_sleep` for far longer than any test runs, so
//! only a kill ends it.

#![no_std]

use core::hint::black_box;
use testkit::{Grant, host};

testkit::manifest!(b"caps=time");

#[unsafe(no_mangle)]
pub extern "C" fn lumen_main() -> i32 {
    if testkit::args() == "sleep" {
        let time = host::handle_find(Grant::Time as i32);
        testkit::info("busy: sleeping");
        host::time_sleep(time, i64::MAX);
        return 1;
    }
    let mut n: u64 = 0;
    loop {
        n = black_box(n.wrapping_add(1));
    }
}
