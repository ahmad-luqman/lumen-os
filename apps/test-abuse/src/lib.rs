//! `abuse` (#199): calls the host functions wrong and checks each error
//! code itself, logging a line per failed check and a summary
//! (`abuse: <mode>: <n> checks, <f> failed`); its exit code is the number
//! failed. The kernel must answer every one with an error code: a trap or
//! a WARN would be the runtime's failure.
//!
//! - `calls`: the argument checks' order (handle, kind, ranges, content),
//!   wrapping and out-of-range pointers, sentinel-filled outputs a refused
//!   call leaves alone, the text cap (exactly at it accepted, a byte past
//!   refused), bad UTF-8, log levels, a negative sleep, `trace_read`'s
//!   refusals.
//! - `handles`: the same handle from `handle_find` every time, no reuse
//!   after a close, a closed grant `NotGranted`, the wrong kind, made-up
//!   handles and kind numbers.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::vec;
use testkit::api::limits::TEXT_BYTES;
use testkit::{Error, Grant, host, info, log_handle, named, ptr};

testkit::manifest!(b"caps=time,trace");

/// A pointer whose range wraps past 4 GiB.
const WRAPPING: i32 = 0xffff_fff0_u32 as i32;

struct Checks {
    run: u32,
    failed: u32,
}

impl Checks {
    fn check(&mut self, what: &str, got: i64, want: Result<(), Error>) {
        self.run += 1;
        let ok = match want {
            Ok(()) => got >= 0,
            Err(e) => got == i64::from(e.returned()),
        };
        if !ok {
            self.failed += 1;
            let want = match want {
                Ok(()) => "a value".into(),
                Err(e) => alloc::string::String::from(e.name()),
            };
            info(&format!("abuse: FAILED {what}: got {}, want {want}", named(got)));
        }
    }

    fn holds(&mut self, what: &str, ok: bool) {
        self.check(what, if ok { 0 } else { -100 }, Ok(()));
    }
}

fn find(grant: Grant) -> i32 {
    host::handle_find(grant as i32)
}

fn log_raw(h: i32, level: i32, msg: i32, len: i32) -> i64 {
    i64::from(host::log(h, level, msg, len))
}

fn calls(c: &mut Checks) {
    let log = log_handle();
    let time = find(Grant::Time);
    let trace = find(Grant::Trace);
    let text = "abuse";
    let (t, n) = (ptr(text.as_ptr()), text.len() as i32);
    let end = testkit::memory_bytes() as i32;

    // The order: the handle, then its kind, then the ranges, then the text.
    c.check("bad handle and bad pointer", log_raw(999, 3, WRAPPING, 100), Err(Error::BadHandle));
    c.check("wrong kind and bad pointer", log_raw(time, 3, WRAPPING, 100), Err(Error::WrongKind));
    c.check("wrong kind", log_raw(time, 3, t, n), Err(Error::WrongKind));
    c.check("bad level and bad pointer", log_raw(log, 0, WRAPPING, 100), Err(Error::BadPointer));
    // Ranges.
    c.check("a wrapping range", log_raw(log, 3, WRAPPING, 0x20), Err(Error::BadPointer));
    c.check("a range past the end", log_raw(log, 3, end - 1, 2), Err(Error::BadPointer));
    c.check("a negative length", log_raw(log, 3, t, -1), Err(Error::BadPointer));
    c.check("empty at the end", log_raw(log, 3, end, 0), Ok(()));
    // Content.
    c.check("level 0", log_raw(log, 0, t, n), Err(Error::BadArgument));
    c.check("level 6", log_raw(log, 6, t, n), Err(Error::BadArgument));
    let bad_utf8 = [b'a', 0xff, b'b'];
    c.check("bad UTF-8", log_raw(log, 3, ptr(bad_utf8.as_ptr()), 3), Err(Error::BadArgument));
    let at_cap = vec![b'c'; TEXT_BYTES];
    c.check("text at the cap", log_raw(log, 5, ptr(at_cap.as_ptr()), TEXT_BYTES as i32), Ok(()));
    let past_cap = vec![b'c'; TEXT_BYTES + 1];
    c.check(
        "text past the cap",
        log_raw(log, 5, ptr(past_cap.as_ptr()), past_cap.len() as i32),
        Err(Error::BadArgument),
    );

    // Outputs: a refused call writes nothing.
    let mut buf = [0xa5u8; 128];
    let args = host::args_len();
    c.check("args_len", args.into(), Ok(()));
    let r = host::args_read(ptr(buf.as_mut_ptr()), args - 1);
    c.check("args_read too small", r.into(), Err(Error::BufferTooSmall));
    c.holds("args_read too small wrote nothing", buf.iter().all(|&b| b == 0xa5));
    let r = host::args_read(WRAPPING, 64);
    c.check("args_read at a wrapping range", r.into(), Err(Error::BadPointer));
    let r = host::args_read(ptr(buf.as_mut_ptr()), buf.len() as i32);
    c.check("args_read", r.into(), Ok(()));
    c.holds("args_read wrote the arguments", buf[..r.max(0) as usize] == *b"calls\0");

    // Time.
    c.check("time_now", host::time_now(time), Ok(()));
    c.check("time_now on the log handle", host::time_now(log), Err(Error::WrongKind));
    c.check("a negative sleep", host::time_sleep(time, -1).into(), Err(Error::BadArgument));
    c.check("a zero sleep", host::time_sleep(time, 0).into(), Ok(()));

    // Trace: the header and one event (72 bytes) is the least.
    let mut out = [0xa5u8; 16 + 56 * 2];
    let o = ptr(out.as_mut_ptr());
    let cpus = host::trace_cpus(trace);
    c.holds("trace_cpus is at least 1", cpus >= 1);
    let r = host::trace_read(trace, 0, 0, 0, o, 71);
    c.check("trace_read below one event", r.into(), Err(Error::BufferTooSmall));
    c.holds("trace_read too small wrote nothing", out.iter().all(|&b| b == 0xa5));
    c.check("trace_read stream 2", host::trace_read(trace, 2, 0, 0, o, 72).into(), Err(Error::BadArgument));
    c.check("trace_read stream -1", host::trace_read(trace, -1, 0, 0, o, 72).into(), Err(Error::BadArgument));
    c.check("trace_read cpu -1", host::trace_read(trace, 0, -1, 0, o, 72).into(), Err(Error::BadArgument));
    c.check("trace_read cpu 99", host::trace_read(trace, 0, 99, 0, o, 72).into(), Err(Error::BadArgument));
    c.check("trace_read since -1", host::trace_read(trace, 0, 0, -1, o, 72).into(), Err(Error::BadArgument));
    c.check("trace_read since ahead", host::trace_read(trace, 0, 0, i64::MAX, o, 72).into(), Err(Error::BadArgument));
    c.check(
        "trace_read at a wrapping range",
        host::trace_read(trace, 0, 0, 0, WRAPPING, 72).into(),
        Err(Error::BadPointer),
    );
    c.check("trace_read on the time handle", host::trace_read(time, 0, 0, 0, o, 72).into(), Err(Error::WrongKind));
    let r = host::trace_read(trace, 0, 0, 0, o, out.len() as i32);
    c.check("trace_read", r.into(), Ok(()));
    c.holds("trace_read copied at most what fits", (0..=2).contains(&r));
}

fn handles(c: &mut Checks) {
    let log = log_handle();
    c.holds("a log handle", log > 0);
    c.check("the same log handle again", (find(Grant::Log) - log).into(), Ok(()));
    c.holds("the same log handle", find(Grant::Log) == log);
    let time = find(Grant::Time);
    c.holds("a time handle", time > 0 && time != log);
    c.check("close time", host::handle_close(time).into(), Ok(()));
    c.check("close time again", host::handle_close(time).into(), Err(Error::BadHandle));
    c.check("time_now on the closed handle", host::time_now(time), Err(Error::BadHandle));
    c.check("time after its close", find(Grant::Time).into(), Err(Error::NotGranted));
    let trace = find(Grant::Trace);
    c.holds("a new handle never takes a closed one's id", trace > time);
    c.check("Window (4) is no grant", host::handle_find(4).into(), Err(Error::BadArgument));
    c.check("kind 0", host::handle_find(0).into(), Err(Error::BadArgument));
    c.check("kind 99", host::handle_find(99).into(), Err(Error::BadArgument));
    c.check("the wrong kind", host::trace_cpus(log).into(), Err(Error::WrongKind));
    c.check("a made-up handle", host::trace_cpus(12345).into(), Err(Error::BadHandle));
    c.check("handle 0", host::trace_cpus(0).into(), Err(Error::BadHandle));
    c.check("a negative handle", host::handle_close(-3).into(), Err(Error::BadHandle));
}

#[unsafe(no_mangle)]
pub extern "C" fn lumen_main() -> i32 {
    let mode = testkit::args();
    let mut c = Checks { run: 0, failed: 0 };
    match mode.as_str() {
        "calls" => calls(&mut c),
        "handles" => handles(&mut c),
        _ => {
            info("abuse: calls or handles?");
            return 100;
        }
    }
    info(&format!("abuse: {mode}: {} checks, {} failed", c.run, c.failed));
    c.failed as i32
}
