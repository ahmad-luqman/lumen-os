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
//! - `window` (#206; the desktop started):
//!   - `window_open`: a made-up handle, the wrong kind, a wrapping title
//!     (before a zero size), a zero, negative or past-`WINDOW_PIXELS` size,
//!     a title a byte past `TITLE_BYTES` or not UTF-8 refused, one exactly
//!     at it accepted; a second window `Limit`; exactly `WINDOW_PIXELS`
//!     (640 × 480) opening;
//!   - events: a made-up handle, the wrong kind, a wrapping range; the
//!     first event `Theme`, kept in hand across `BufferTooSmall` (nothing
//!     written; `window_wait` then 1 at once), taken by a buffer exactly its
//!     size; `window_wait`'s argument errors, its 0 at a deadline and never
//!     before it;
//!   - `window_commit`: a made-up handle, the wrong kind, a wrapping canvas
//!     or rects, 0, -1 or too many rects (a bad count before bad rects),
//!     exactly `COMMIT_RECTS`; an unaligned canvas, a stride below the width
//!     (before rows past the memory), stride × height past the memory or
//!     overflowing, rows past the memory, a canvas unlike the window;
//!   - text: made-up handles and the wrong kind for all three calls; `px` 0,
//!     7 and 97; text past `TEXT_BYTES`, not UTF-8 or at a wrapping range;
//!     `text_draw` with an unaligned or wrapping canvas or wrapping
//!     arguments, `Unavailable` without a window, drawing only inside its
//!     clip (from x = 0 and from x > 0), only within the window's size on a
//!     bigger canvas, and a call whose text lies in its own canvas drawing
//!     what one with a separate copy does;
//!   - `window_publish` (#207): a made-up handle, the wrong kind, a wrapping
//!     range; no nodes, a button, a label holding ESC, LF and U+2028, and
//!     a tree of exactly `TREE_BYTES` (labels within `TEXT_BYTES`) accepted; a byte past `TREE_BYTES` (before the copy), bytes that
//!     don't decode and an id twice refused; `window_answer`: a made-up
//!     handle, the wrong kind, a wrapping range (before "none running"),
//!     and no request running, refused;
//!   - a closed window's handle `BadHandle`, a new window a new handle.
//!
//!   `calls` checks `window_open` without a desktop (`Unavailable`).

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::vec;
use testkit::api::event::Event;
use testkit::api::layout::{CANVAS_SIZE, Canvas, RECT_SIZE, Rect, TextDraw};
use testkit::api::limits::{COMMIT_RECTS, EVENT_BYTES, TEXT_BYTES, TITLE_BYTES, TREE_BYTES};
use testkit::api::trace::{EVENT_SIZE, HEADER_SIZE, decode_event, decode_header};
use testkit::{Error, Grant, find, host, info, log_handle, named, ptr};

testkit::manifest!(b"caps=time,desktop,trace");

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
    // No desktop started: no window.
    c.check(
        "window_open without a desktop",
        host::window_open(find(Grant::Desktop), t, n, 40, 20),
        Err(Error::Unavailable),
    );

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

/// `window_next_event` into `buf`.
fn next_event(w: i32, buf: &mut [u8]) -> i32 {
    // SAFETY: `buf` is ours to write.
    unsafe { host::window_next_event(w, ptr(buf.as_mut_ptr()), buf.len() as i32) }
}

/// Whether any pixel of `px` (rows `stride` apart) in columns `xs` of
/// rows `ys` is drawn (not 0).
fn drawn(px: &[u32], stride: u32, xs: core::ops::Range<u32>, ys: core::ops::Range<u32>) -> bool {
    ys.into_iter().any(|y| px[(y * stride + xs.start) as usize..(y * stride + xs.end) as usize].iter().any(|&p| p != 0))
}

/// A canvas over `px`, `w` × `h` with `stride`.
fn canvas(px: &[u32], stride: u32, w: u32, h: u32) -> [u8; CANVAS_SIZE] {
    Canvas { ptr: ptr(px.as_ptr()) as u32, stride, w, h }.encode()
}

fn window(c: &mut Checks) {
    const W: u32 = 40;
    const H: u32 = 20;
    let log = log_handle();
    let desk = find(Grant::Desktop);
    let time = find(Grant::Time);
    c.holds("a desktop handle", desk > 0);
    let title = "abuse";
    let (t, tn) = (ptr(title.as_ptr()), title.len() as i32);
    let end = testkit::memory_bytes() as i32;
    let mut a = vec![0u32; (W * H) as usize];
    let full = Rect::new(0, 0, W, H).encode();
    let draw = |x, px, clip: Rect| TextDraw { x, y: 0, px, rgb: 0xffffff, clip }.encode();

    // Before any window: text_draw has nowhere to draw.
    let ca = canvas(&a, W, W, H);
    let args = draw(0, 16, Rect::new(0, 0, W, H));
    // SAFETY: `a` is ours.
    let r = unsafe { host::text_draw(desk, ptr(ca.as_ptr()), ptr(args.as_ptr()), t, tn) };
    c.check("text_draw without a window", r, Err(Error::Unavailable));

    // window_open: the handle, its kind, the title's range before its size,
    // the size, the title's cap and text (the last two both BadArgument:
    // their order can't be seen).
    c.check(
        "open: a made-up handle and a bad pointer",
        host::window_open(999, WRAPPING, 10, W as i32, H as i32),
        Err(Error::BadHandle),
    );
    c.check("open on the log handle", host::window_open(log, t, tn, W as i32, H as i32), Err(Error::WrongKind));
    c.check(
        "open: a wrapping title",
        host::window_open(desk, WRAPPING, 0x20, W as i32, H as i32),
        Err(Error::BadPointer),
    );
    c.check(
        "open: a wrapping title before a zero size",
        host::window_open(desk, WRAPPING, 0x20, 0, H as i32),
        Err(Error::BadPointer),
    );
    c.check("open: zero wide", host::window_open(desk, t, tn, 0, H as i32), Err(Error::BadArgument));
    c.check("open: a negative height", host::window_open(desk, t, tn, W as i32, -1), Err(Error::BadArgument));
    c.check("open: past WINDOW_PIXELS", host::window_open(desk, t, tn, 640, 481), Err(Error::BadArgument));
    let long = [b'x'; TITLE_BYTES + 1];
    c.check(
        "open: a title past TITLE_BYTES",
        host::window_open(desk, ptr(long.as_ptr()), long.len() as i32, W as i32, H as i32),
        Err(Error::BadArgument),
    );
    let bad = [b'a', 0xff];
    c.check(
        "open: a title not UTF-8",
        host::window_open(desk, ptr(bad.as_ptr()), 2, W as i32, H as i32),
        Err(Error::BadArgument),
    );
    let w = host::window_open(desk, ptr(long.as_ptr()), TITLE_BYTES as i32, W as i32, H as i32);
    c.check("open: a title at TITLE_BYTES", w, Ok(()));
    c.check("a second window", host::window_open(desk, t, tn, W as i32, H as i32), Err(Error::Limit));

    // Events: the first is the theme, kept in hand across BufferTooSmall
    // (window_wait then has it at once).
    let mut buf = [0xa5u8; EVENT_BYTES];
    c.check("next_event into 1 byte", next_event(w, &mut buf[..1]), Err(Error::BufferTooSmall));
    c.holds("a refused next_event wrote nothing", buf.iter().all(|&b| b == 0xa5));
    c.check("next_event on the log handle", next_event(log, &mut buf), Err(Error::WrongKind));
    c.check("next_event at a wrapping range", unsafe_next(w, WRAPPING, 64), Err(Error::BadPointer));
    c.check("next_event on a made-up handle", next_event(999, &mut buf), Err(Error::BadHandle));
    c.check("window_wait on a made-up handle", host::window_wait(999, -1, 0), Err(Error::BadHandle));
    c.check("window_wait with one in hand", host::window_wait(w, -1, 0), Ok(()));
    c.holds("window_wait with one in hand is 1", host::window_wait(w, -1, 0) == 1);
    // Grown a byte at a time from 2: refused while too small, then taken
    // with a buffer exactly its size (its length one more than the last
    // refused).
    let mut len = 2;
    let n = loop {
        let r = next_event(w, &mut buf[..len]);
        if r != Error::BufferTooSmall.returned() || len == EVENT_BYTES {
            break r;
        }
        len += 1;
    };
    c.check("next_event", n, Ok(()));
    c.holds("an event taken by a buffer exactly its size", n == len as i32);
    let first = Event::decode(&buf[..n.clamp(0, EVENT_BYTES as i32) as usize]);
    c.holds("the first event is the theme", matches!(first, Ok(Event::Theme { .. })));
    c.check("window_wait animating 2", host::window_wait(w, -1, 2), Err(Error::BadArgument));
    c.check("window_wait deadline -2", host::window_wait(w, -2, 0), Err(Error::BadArgument));
    c.check("window_wait animating without a deadline", host::window_wait(w, -1, 1), Err(Error::BadArgument));
    c.check("window_wait on the time handle", host::window_wait(time, -1, 0), Err(Error::WrongKind));
    // At a deadline with nothing to read: 0 (whatever came meanwhile, a
    // focus change, read and the wait tried again).
    let mut got = -1;
    for _ in 0..20 {
        while next_event(w, &mut buf) > 0 {}
        got = host::window_wait(w, host::time_now(time) + 1_000_000, 0);
        if got == 0 {
            break;
        }
    }
    c.holds("window_wait at its deadline is 0", got == 0);
    // And not before it: whatever it returns, a 0 comes at or after it.
    while next_event(w, &mut buf) > 0 {}
    let dl = host::time_now(time) + 20_000_000;
    let got = host::window_wait(w, dl, 0);
    c.holds("a deadline's 0 not before it", got != 0 || host::time_now(time) >= dl);

    // window_commit: the handle, the canvas's range, the count, the rects'
    // range, the canvas's pixels, its size.
    let ca = canvas(&a, W, W, H);
    let (cp, fp) = (ptr(ca.as_ptr()), ptr(full.as_ptr()));
    c.check("commit", host::window_commit(w, cp, fp, 1), Ok(()));
    c.check(
        "commit: a made-up handle and bad pointers",
        host::window_commit(999, WRAPPING, WRAPPING, 0),
        Err(Error::BadHandle),
    );
    c.check("commit on the desktop handle", host::window_commit(desk, cp, fp, 1), Err(Error::WrongKind));
    c.check("commit: a wrapping canvas", host::window_commit(w, WRAPPING, fp, 1), Err(Error::BadPointer));
    c.check("commit: 0 rects", host::window_commit(w, cp, fp, 0), Err(Error::BadArgument));
    c.check("commit: too many rects", host::window_commit(w, cp, fp, COMMIT_RECTS as i32 + 1), Err(Error::BadArgument));
    c.check("commit: -1 rects", host::window_commit(w, cp, fp, -1), Err(Error::BadArgument));
    c.check("commit: wrapping rects", host::window_commit(w, cp, WRAPPING, 1), Err(Error::BadPointer));
    c.check("commit: a bad count before bad rects", host::window_commit(w, cp, WRAPPING, 0), Err(Error::BadArgument));
    let mut rects = [0u8; RECT_SIZE * COMMIT_RECTS];
    for (i, r) in rects.as_chunks_mut::<RECT_SIZE>().0.iter_mut().enumerate() {
        // One whole, the rest outside or empty: skipped.
        *r = if i == 0 { Rect::new(0, 0, W, H) } else { Rect::new(-100 * i as i32, 5, 10, 10) }.encode();
    }
    c.check(
        "commit: exactly COMMIT_RECTS",
        host::window_commit(w, cp, ptr(rects.as_ptr()), COMMIT_RECTS as i32),
        Ok(()),
    );
    let base = ptr(a.as_ptr()) as u32;
    let bad = |ptr: u32, stride: u32, w: u32, h: u32| Canvas { ptr, stride, w, h }.encode();
    let commit_with = |cv: [u8; CANVAS_SIZE]| host::window_commit(w, ptr(cv.as_ptr()), fp, 1);
    c.check("commit: an unaligned canvas", commit_with(bad(base + 1, W, W, H)), Err(Error::BadPointer));
    c.check("commit: a stride below the width", commit_with(bad(base, W - 1, W, H)), Err(Error::BadArgument));
    c.check(
        "commit: stride × height past the memory",
        commit_with(bad(base, 0x4000_0000, W, H)),
        Err(Error::BadPointer),
    );
    c.check(
        "commit: stride × height overflowing",
        commit_with(bad(0, u32::MAX, u32::MAX, u32::MAX)),
        Err(Error::BadPointer),
    );
    c.check(
        "commit: a stride below the width before rows past the memory",
        commit_with(bad(end as u32 - 4, W - 1, W, H)),
        Err(Error::BadArgument),
    );
    c.check("commit: rows past the memory", commit_with(bad(end as u32 - 4, W, W, H)), Err(Error::BadPointer));
    c.check("commit: a canvas unlike the window", commit_with(bad(base, W, W - 1, H)), Err(Error::BadArgument));

    // Text: px, caps, UTF-8, ranges.
    c.check("line_height at 16", host::text_line_height(desk, 16), Ok(()));
    c.holds("a line is some pixels high", host::text_line_height(desk, 16) > 0);
    c.check("line_height px 7", host::text_line_height(desk, 7), Err(Error::BadArgument));
    c.check("line_height px 97", host::text_line_height(desk, 97), Err(Error::BadArgument));
    c.check("line_height on the window", host::text_line_height(w, 16), Err(Error::WrongKind));
    c.check("line_height on a made-up handle", host::text_line_height(999, 16), Err(Error::BadHandle));
    c.check("measure on the window", host::text_measure(w, 16, t, tn), Err(Error::WrongKind));
    c.check("measure on a made-up handle", host::text_measure(999, 16, t, tn), Err(Error::BadHandle));
    c.holds("measure", host::text_measure(desk, 16, t, tn) > 0);
    c.check("measure: not UTF-8", host::text_measure(desk, 16, ptr(bad_utf8().as_ptr()), 3), Err(Error::BadArgument));
    let past = vec![b'm'; TEXT_BYTES + 1];
    c.check(
        "measure: past TEXT_BYTES",
        host::text_measure(desk, 16, ptr(past.as_ptr()), past.len() as i32),
        Err(Error::BadArgument),
    );
    c.check("measure: a wrapping range", host::text_measure(desk, 16, WRAPPING, 0x20), Err(Error::BadPointer));
    c.check("measure: px 0", host::text_measure(desk, 0, t, tn), Err(Error::BadArgument));
    let ca = canvas(&a, W, W, H);
    let cp = ptr(ca.as_ptr());
    let td = |h: i32, canvas: i32, args: i32, text: i32, len: i32| {
        // SAFETY: every canvas passed below is `a` or `b`, ours to write,
        // or one the host refuses (unaligned, or at a wrapping range).
        unsafe { host::text_draw(h, canvas, args, text, len) }
    };
    let px7 = draw(0, 7, Rect::new(0, 0, W, H));
    c.check("text_draw: px 7", td(desk, cp, ptr(px7.as_ptr()), t, tn), Err(Error::BadArgument));
    let args = draw(0, 16, Rect::new(0, 0, W, H));
    let ap = ptr(args.as_ptr());
    let unaligned = bad(base + 2, W, W, H);
    c.check("text_draw: an unaligned canvas", td(desk, ptr(unaligned.as_ptr()), ap, t, tn), Err(Error::BadPointer));
    c.check("text_draw: wrapping args", td(desk, cp, WRAPPING, t, tn), Err(Error::BadPointer));
    c.check("text_draw on the log handle", td(log, cp, ap, t, tn), Err(Error::WrongKind));
    c.check("text_draw on a made-up handle", td(999, cp, ap, t, tn), Err(Error::BadHandle));
    c.check("text_draw: a wrapping canvas", td(desk, WRAPPING, ap, t, tn), Err(Error::BadPointer));
    c.check("text_draw: a wrapping text", td(desk, cp, ap, WRAPPING, 0x20), Err(Error::BadPointer));
    let long_text = vec![b't'; TEXT_BYTES + 1];
    c.check(
        "text_draw: text past TEXT_BYTES",
        td(desk, cp, ap, ptr(long_text.as_ptr()), long_text.len() as i32),
        Err(Error::BadArgument),
    );
    c.check("text_draw: text not UTF-8", td(desk, cp, ap, ptr(bad_utf8().as_ptr()), 3), Err(Error::BadArgument));
    a.fill(0);
    // Drawn only inside the clip: the left half.
    let left = draw(0, 16, Rect::new(0, 0, W / 2, H));
    let r = td(desk, cp, ptr(left.as_ptr()), t, tn);
    c.holds("text_draw's advance", r > 0);
    c.holds("text_draw drew inside its clip", drawn(&a, W, 0..W / 2, 0..H));
    c.holds("text_draw drew nothing outside its clip", !drawn(&a, W, W / 2..W, 0..H));
    // A clip from x > 0: the right half only.
    a.fill(0);
    let right = draw(0, 16, Rect::new((W / 2) as i32, 0, W / 2, H));
    let wide = "mmmmmmmm";
    let r = td(desk, cp, ptr(right.as_ptr()), ptr(wide.as_ptr()), wide.len() as i32);
    c.holds("text_draw's advance from x > 0", r > 0);
    c.holds("a clip from x > 0 drew inside it", drawn(&a, W, W / 2..W, 0..H));
    c.holds("a clip from x > 0 drew nothing left of it", !drawn(&a, W, 0..W / 2, 0..H));
    // Its text inside its own canvas draws what a separate copy does.
    let mut b = vec![0u32; (W * H) as usize];
    a.fill(0);
    a[0] = u32::from_le_bytes(*b"Hi\0\0");
    b[0] = a[0];
    let below = draw(0, 16, Rect::new(0, 1, W, H - 1));
    let ra = td(desk, cp, ptr(below.as_ptr()), ptr(a.as_ptr()), 2);
    let cb = canvas(&b, W, W, H);
    let rb = td(desk, ptr(cb.as_ptr()), ptr(below.as_ptr()), ptr(b"Hi".as_ptr()), 2);
    c.holds("an overlapping text_draw's advance", ra > 0 && ra == rb);
    c.holds("an overlapping text_draw draws what a separate one does", a == b);

    // A canvas bigger than the window: drawn only within the window's
    // size from the clip's corner (its scratch surface's).
    let big = vec![0u32; (4 * W * H) as usize];
    let cbig = canvas(&big, 2 * W, 2 * W, 2 * H);
    let wide = draw(0, 16, Rect::new(0, 0, 2 * W, 2 * H));
    let rbig = td(desk, ptr(cbig.as_ptr()), ptr(wide.as_ptr()), ptr(b"mmmmmmmm".as_ptr()), 8);
    c.holds("text_draw on a canvas bigger than the window", rbig > 0);
    let past = drawn(&big, 2 * W, W..2 * W, 0..2 * H) || drawn(&big, 2 * W, 0..W, H..2 * H);
    c.holds("nothing drawn past the window's size", !past);

    // window_publish and window_answer (#207): the handle, its kind, the
    // range, then the size (before the copy), then the encoding.
    let none = [0u8; 1];
    let np = ptr(none.as_ptr());
    c.check("publish on a made-up handle", host::window_publish(999, np, 0), Err(Error::BadHandle));
    c.check("publish on the desktop handle", host::window_publish(desk, np, 0), Err(Error::WrongKind));
    c.check("publish: a wrapping tree", host::window_publish(w, WRAPPING, 64), Err(Error::BadPointer));
    c.check("publish: no nodes", host::window_publish(w, np, 0), Ok(()));
    let junk = vec![0xffu8; TREE_BYTES + 1];
    let jp = ptr(junk.as_ptr());
    c.check(
        "publish: a byte past TREE_BYTES",
        host::window_publish(w, jp, TREE_BYTES as i32 + 1),
        Err(Error::BadArgument),
    );
    c.check(
        "publish: TREE_BYTES that don't decode",
        host::window_publish(w, jp, TREE_BYTES as i32),
        Err(Error::BadArgument),
    );
    // Labels making a tree of exactly TREE_BYTES (each text within
    // TEXT_BYTES: a longer one is refused).
    let tree = exact_tree(TREE_BYTES);
    c.check(
        "publish: a tree of exactly TREE_BYTES",
        host::window_publish(w, ptr(tree.as_ptr()), TREE_BYTES as i32),
        Ok(()),
    );
    // A label whose text holds ESC, LF and U+2028: accepted (`ui tree`
    // shows each as U+FFFD).
    let odd = "a\u{1b}b\nc\u{2028}d".as_bytes();
    let mut label = vec![1u8, 1, 0, 0, 0, 1, 1, 0, 0, odd.len() as u8];
    label.extend_from_slice(odd);
    c.check(
        "publish: a label with control characters",
        host::window_publish(w, ptr(label.as_ptr()), label.len() as i32),
        Ok(()),
    );
    // A button, app id 1: tag 2, id 1, no name, bounds, flags, not focused.
    let button = [2u8, 1, 0, 0, 0, 1, 1, 0, 0, 0];
    c.check("publish: a button", host::window_publish(w, ptr(button.as_ptr()), button.len() as i32), Ok(()));
    let twice = [button, button].concat();
    c.check(
        "publish: an id twice",
        host::window_publish(w, ptr(twice.as_ptr()), twice.len() as i32),
        Err(Error::BadArgument),
    );
    c.check("answer on a made-up handle", host::window_answer(999, np, 1), Err(Error::BadHandle));
    c.check("answer on the desktop handle", host::window_answer(desk, np, 1), Err(Error::WrongKind));
    c.check(
        "answer: a wrapping range (before none running)",
        host::window_answer(w, WRAPPING, 64),
        Err(Error::BadPointer),
    );
    c.check("answer with no request running", host::window_answer(w, np, 1), Err(Error::BadArgument));

    // A closed window's handle is gone; a new window gets a new handle,
    // and exactly WINDOW_PIXELS (640 × 480) opens.
    c.check("close the window", host::handle_close(w), Ok(()));
    c.check("commit on the closed window", host::window_commit(w, cp, fp, 1), Err(Error::BadHandle));
    let again = host::window_open(desk, t, tn, 640, 480);
    c.check("a window again, at exactly WINDOW_PIXELS", again, Ok(()));
    c.holds("a new window gets a new handle", again > w);
    c.check("close it", host::handle_close(again), Ok(()));
}

/// The bytes `a, 0xff, b`.
fn bad_utf8() -> [u8; 3] {
    [b'a', 0xff, b'b']
}

/// `window_next_event` at any range (refused ones write nothing).
fn unsafe_next(w: i32, buf: i32, len: i32) -> i32 {
    // SAFETY: only refused ranges are passed (a wrapping one).
    unsafe { host::window_next_event(w, buf, len) }
}

#[unsafe(no_mangle)]
pub extern "C" fn lumen_main() -> i32 {
    let mode = testkit::args();
    let mut c = Checks { run: 0, failed: 0 };
    match mode.as_str() {
        "calls" => calls(&mut c),
        "handles" => handles(&mut c),
        "window" => window(&mut c),
        _ => {
            info("abuse: calls, handles or window?");
            return 100;
        }
    }
    info(&format!("abuse: {mode}: {} checks, {} failed", c.run, c.failed));
    c.failed as i32
}

/// A label's encoded length with `n` characters of text (its length one
/// byte below 128, two from there).
fn label_len(n: usize) -> usize {
    9 + if n < 128 { 1 } else { 2 } + n
}

/// Push a label's bytes onto `out`: tag 1, app id `id` (below 128), no
/// name, bounds 0 0 1 1, not disabled or hidden, then `n` (at most
/// `TEXT_BYTES`) characters of text with their length.
fn push_label(out: &mut alloc::vec::Vec<u8>, id: u8, n: usize) {
    out.extend_from_slice(&[1u8, id, 0, 0, 0, 1, 1, 0, 0]);
    if n < 128 {
        out.push(n as u8);
    } else {
        out.extend_from_slice(&[(n as u8 & 0x7f) | 0x80, (n >> 7) as u8]);
    }
    out.resize(out.len() + n, b'x');
}

/// Labels whose encoding is exactly `len` bytes, in one allocation (the
/// bump heap never frees): whole ones of 1,000 characters, then two making
/// up the rest.
fn exact_tree(len: usize) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::with_capacity(len);
    let mut id = 1;
    while len - out.len() > 2 * label_len(1000) {
        push_label(&mut out, id, 1000);
        id += 1;
    }
    let half = (len - out.len()) / 2;
    push_label(&mut out, id, half - 12);
    let left = len - out.len();
    let n = (0..TEXT_BYTES).find(|&n| label_len(n) == left).unwrap_or(0);
    push_label(&mut out, id + 1, n);
    out
}
