//! `abuse` (#199): calls the host functions wrong and checks each error
//! code itself, logging a line per failed check and a summary
//! (`abuse: <mode>: <n> checks, <f> failed`); its exit code is the number
//! failed. The kernel must answer every one with an error code: a trap or
//! a WARN would be the runtime's failure.
//!
//! - `calls`: the argument checks' order (handle, kind, ranges, content),
//!   wrapping and out-of-range pointers, sentinel-filled outputs a refused
//!   call leaves alone, the text cap (exactly at it accepted, a byte past
//!   refused), bad UTF-8, log levels, the clocks (`time_wall` past 2020),
//!   a negative sleep, `trace_read`'s refusals.
//! - `handles`: the same handle from `handle_find` every time, no reuse
//!   after a close, a closed grant `NotGranted`, the wrong kind, made-up
//!   handles and kind numbers.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::vec;
use testkit::api::limits::TEXT_BYTES;
use testkit::api::trace::{EVENT_SIZE, HEADER_SIZE, decode_event, decode_header};
use testkit::{Error, Grant, find, host, info, log_handle, named, ptr};

testkit::manifest!(b"caps=time,trace");

/// A pointer whose range wraps past 4 GiB.
const WRAPPING: i32 = 0xffff_fff0_u32 as i32;

struct Checks {
    run: u32,
    failed: u32,
}

impl Checks {
    fn check(&mut self, what: &str, got: impl Into<i64>, want: Result<(), Error>) {
        let got = got.into();
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

fn log_raw(h: i32, level: i32, msg: i32, len: i32) -> i64 {
    i64::from(host::log(h, level, msg, len))
}

/// `args_read`, as the checks call it.
///
/// # Safety
///
/// `[buf, buf + len)` is inside a buffer the caller may write, or a range
/// the host refuses (wrapping, past the memory), into which it writes
/// nothing.
unsafe fn args_read(buf: i32, len: i32) -> i32 {
    // SAFETY: the caller's contract is `host::args_read`'s.
    unsafe { host::args_read(buf, len) }
}

/// `trace_read`, as the checks call it.
///
/// # Safety
///
/// As [`args_read`]'s.
unsafe fn trace_read(h: i32, stream: i32, cpu: i32, since: i64, buf: i32, len: i32) -> i32 {
    // SAFETY: the caller's contract is `host::trace_read`'s.
    unsafe { host::trace_read(h, stream, cpu, since, buf, len) }
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
    let (b, blen) = (ptr(buf.as_mut_ptr()), buf.len() as i32);
    let args = host::args_len();
    c.check("args_len", args, Ok(()));
    // SAFETY: `buf` is ours; a wrapping range is refused, written nothing.
    let (small, wrapping, whole) = unsafe { (args_read(b, args - 1), args_read(WRAPPING, 64), args_read(b, blen)) };
    c.check("args_read too small", small, Err(Error::BufferTooSmall));
    c.check("args_read at a wrapping range", wrapping, Err(Error::BadPointer));
    c.check("args_read", whole, Ok(()));
    c.holds("args_read wrote the arguments", buf[..whole.max(0) as usize] == *b"calls\0");
    c.holds("args_read wrote only the arguments", buf[whole.max(0) as usize..].iter().all(|&b| b == 0xa5));

    // Time.
    c.check("time_now", host::time_now(time), Ok(()));
    c.check("time_now on the log handle", host::time_now(log), Err(Error::WrongKind));
    // The RTC's Unix time: past 2020-01-01 (QEMU's clock is the host's).
    c.holds("time_wall is past 2020", host::time_wall(time) > 1_577_836_800);
    c.check("time_wall on the log handle", host::time_wall(log), Err(Error::WrongKind));
    c.check("time_wall on a made-up handle", host::time_wall(999), Err(Error::BadHandle));
    c.check("a negative sleep", host::time_sleep(time, -1), Err(Error::BadArgument));
    c.check("a zero sleep", host::time_sleep(time, 0), Ok(()));

    // Trace: the header and one event (72 bytes) is the least.
    let mut out = [0xa5u8; 16 + 56 * 2 + 8];
    let o = ptr(out.as_mut_ptr());
    let two = (HEADER_SIZE + 2 * EVENT_SIZE) as i32;
    let cpus = host::trace_cpus(trace);
    c.holds("trace_cpus is at least 1", cpus >= 1);
    // SAFETY: every range below is inside `out`, or refused (wrapping).
    let read = |h: i32, stream: i32, cpu: i32, since: i64, buf: i32, len: i32| unsafe {
        trace_read(h, stream, cpu, since, buf, len)
    };
    c.check("trace_read below one event", read(trace, 0, 0, 0, o, 71), Err(Error::BufferTooSmall));
    c.holds("trace_read too small wrote nothing", out.iter().all(|&b| b == 0xa5));
    c.check("trace_read stream 2", read(trace, 2, 0, 0, o, 72), Err(Error::BadArgument));
    c.check("trace_read stream -1", read(trace, -1, 0, 0, o, 72), Err(Error::BadArgument));
    c.check("trace_read cpu -1", read(trace, 0, -1, 0, o, 72), Err(Error::BadArgument));
    c.check("trace_read cpu 99", read(trace, 0, 99, 0, o, 72), Err(Error::BadArgument));
    c.check("trace_read since -1", read(trace, 0, 0, -1, o, 72), Err(Error::BadArgument));
    c.check("trace_read since ahead", read(trace, 0, 0, i64::MAX, o, 72), Err(Error::BadArgument));
    c.check("trace_read at a wrapping range", read(trace, 0, 0, 0, WRAPPING, 72), Err(Error::BadPointer));
    c.check("trace_read on the time handle", read(time, 0, 0, 0, o, 72), Err(Error::WrongKind));
    c.holds("refused trace_reads wrote nothing", out.iter().all(|&b| b == 0xa5));
    // Room for two events: the header, then `r` events numbered on from
    // `since + missed` (since 0), `next` just past them, and the bytes past
    // the last event untouched.
    let r = read(trace, 0, 0, 0, o, two);
    c.check("trace_read", r, Ok(()));
    c.holds("trace_read copied at most what fits", (0..=2).contains(&r));
    let r = r.clamp(0, 2) as usize;
    let (next, missed) = decode_header(out[..HEADER_SIZE].try_into().expect("a header"));
    c.holds("trace_read's next follows what it copied", next == missed + r as u64);
    for i in 0..r {
        let at = HEADER_SIZE + i * EVENT_SIZE;
        let e = decode_event(out[at..at + EVENT_SIZE].try_into().expect("an event"));
        c.holds("trace_read's events are numbered on from since + missed", e.seq == missed + i as u64);
    }
    c.holds("trace_read wrote nothing past its events", out[HEADER_SIZE + r * EVENT_SIZE..].iter().all(|&b| b == 0xa5));
}

fn handles(c: &mut Checks) {
    let log = log_handle();
    c.holds("a log handle", log > 0);
    c.holds("the same log handle", find(Grant::Log) == log);
    let time = find(Grant::Time);
    c.holds("a time handle", time > 0 && time != log);
    c.check("close time", host::handle_close(time), Ok(()));
    c.check("close time again", host::handle_close(time), Err(Error::BadHandle));
    c.check("time_now on the closed handle", host::time_now(time), Err(Error::BadHandle));
    c.check("time after its close", find(Grant::Time), Err(Error::NotGranted));
    let trace = find(Grant::Trace);
    c.holds("a new handle never takes a closed one's id", trace > time);
    c.check("Window (4) is no grant", host::handle_find(4), Err(Error::BadArgument));
    c.check("kind 0", host::handle_find(0), Err(Error::BadArgument));
    c.check("kind 99", host::handle_find(99), Err(Error::BadArgument));
    c.check("the wrong kind", host::trace_cpus(log), Err(Error::WrongKind));
    c.check("a made-up handle", host::trace_cpus(12345), Err(Error::BadHandle));
    c.check("handle 0", host::trace_cpus(0), Err(Error::BadHandle));
    c.check("a negative handle", host::handle_close(-3), Err(Error::BadHandle));
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
