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
//! - `window` (#206; the desktop started): `window_open`'s checks in order
//!   (a title exactly at `TITLE_BYTES` accepted, a byte past refused), a
//!   second window `Limit`; the first event `Theme`, kept in hand across
//!   `BufferTooSmall`; `window_wait`'s argument errors and its 0 at a
//!   deadline; `window_commit`'s canvas and rectangle checks (unaligned,
//!   a stride below the width, stride × height overflowing, rows past the
//!   memory, a size unlike the window's, 0 or too many rects, exactly
//!   `COMMIT_RECTS` accepted); the text calls' `px` range, caps and UTF-8;
//!   `text_draw` without a window `Unavailable`, drawing only inside its
//!   clip, and a call whose text lies in its own canvas drawing what one
//!   with a separate copy does; a closed window's handle `BadHandle`, a new
//!   window a new handle. `calls` checks `window_open` without a desktop
//!   (`Unavailable`).

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::vec;
use testkit::api::event::Event;
use testkit::api::layout::{Canvas, Rect, TextDraw};
use testkit::api::limits::{COMMIT_RECTS, EVENT_BYTES, TEXT_BYTES, TITLE_BYTES};
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

/// `text_draw` with fixed structures at the given pointers.
///
/// # Safety
///
/// The canvas they name is the caller's to write (or refused).
unsafe fn text_draw(h: i32, canvas: i32, args: i32, text: i32, len: i32) -> i32 {
    // SAFETY: the caller's contract is `host::text_draw`'s.
    unsafe { host::text_draw(h, canvas, args, text, len) }
}

/// A canvas over `px`, `w` × `h` with `stride`.
fn canvas(px: &[u32], stride: u32, w: u32, h: u32) -> [u8; 16] {
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
    let r = unsafe { text_draw(desk, ptr(ca.as_ptr()), ptr(args.as_ptr()), t, tn) };
    c.check("text_draw without a window", r, Err(Error::Unavailable));

    // window_open: the handle, its kind, the title's range, the size, the
    // title's cap and text.
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
    c.check("window_wait with one in hand", host::window_wait(w, -1, 0), Ok(()));
    c.holds("window_wait with one in hand is 1", host::window_wait(w, -1, 0) == 1);
    let n = next_event(w, &mut buf);
    c.check("next_event", n, Ok(()));
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
    let mut rects = [0u8; 16 * COMMIT_RECTS];
    for (i, r) in rects.as_chunks_mut::<16>().0.iter_mut().enumerate() {
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
    let commit_with = |cv: [u8; 16]| host::window_commit(w, testkit::ptr(cv.as_ptr()), fp, 1);
    c.check("commit: an unaligned canvas", commit_with(bad(base + 1, W, W, H)), Err(Error::BadPointer));
    c.check("commit: a stride below the width", commit_with(bad(base, W - 1, W, H)), Err(Error::BadArgument));
    c.check("commit: stride × height overflowing", commit_with(bad(base, 0x4000_0000, W, H)), Err(Error::BadPointer));
    c.check("commit: rows past the memory", commit_with(bad(end as u32 - 4, W, W, H)), Err(Error::BadPointer));
    c.check("commit: a canvas unlike the window", commit_with(bad(base, W, W - 1, H)), Err(Error::BadArgument));

    // Text: px, caps, UTF-8, ranges.
    c.check("line_height at 16", host::text_line_height(desk, 16), Ok(()));
    c.holds("a line is some pixels high", host::text_line_height(desk, 16) > 0);
    c.check("line_height px 7", host::text_line_height(desk, 7), Err(Error::BadArgument));
    c.check("line_height px 97", host::text_line_height(desk, 97), Err(Error::BadArgument));
    c.check("line_height on the window", host::text_line_height(w, 16), Err(Error::WrongKind));
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
        unsafe { text_draw(h, canvas, args, text, len) }
    };
    let px7 = draw(0, 7, Rect::new(0, 0, W, H));
    c.check("text_draw: px 7", td(desk, cp, ptr(px7.as_ptr()), t, tn), Err(Error::BadArgument));
    let args = draw(0, 16, Rect::new(0, 0, W, H));
    let ap = ptr(args.as_ptr());
    let unaligned = bad(base + 2, W, W, H);
    c.check("text_draw: an unaligned canvas", td(desk, ptr(unaligned.as_ptr()), ap, t, tn), Err(Error::BadPointer));
    c.check("text_draw: wrapping args", td(desk, cp, WRAPPING, t, tn), Err(Error::BadPointer));
    c.check("text_draw on the log handle", td(log, cp, ap, t, tn), Err(Error::WrongKind));
    a.fill(0);
    // Drawn only inside the clip: the left half.
    let left = draw(0, 16, Rect::new(0, 0, W / 2, H));
    let r = td(desk, cp, ptr(left.as_ptr()), t, tn);
    c.holds("text_draw's advance", r > 0);
    let inside = (0..H).any(|y| a[(y * W) as usize..(y * W + W / 2) as usize].iter().any(|&p| p != 0));
    let outside = (0..H).all(|y| a[(y * W + W / 2) as usize..((y + 1) * W) as usize].iter().all(|&p| p == 0));
    c.holds("text_draw drew inside its clip", inside);
    c.holds("text_draw drew nothing outside its clip", outside);
    // Its text inside its own canvas draws what a separate copy does.
    let mut b = vec![0u32; (W * H) as usize];
    a.fill(0);
    a[0] = u32::from_le_bytes(*b"Hi\0\0");
    b[0] = a[0];
    let below = draw(0, 16, Rect::new(0, 1, W, H - 1));
    let ra = td(desk, cp, ptr(below.as_ptr()), cp_text(&a), 2);
    let cb = canvas(&b, W, W, H);
    let rb = td(desk, ptr(cb.as_ptr()), ptr(below.as_ptr()), ptr(b"Hi".as_ptr()), 2);
    c.holds("an overlapping text_draw's advance", ra > 0 && ra == rb);
    c.holds("an overlapping text_draw draws what a separate one does", a == b);

    // A closed window's handle is gone; a new window gets a new handle.
    c.check("close the window", host::handle_close(w), Ok(()));
    c.check("commit on the closed window", host::window_commit(w, cp, fp, 1), Err(Error::BadHandle));
    let again = host::window_open(desk, t, tn, W as i32, H as i32);
    c.check("a window again", again, Ok(()));
    c.holds("a new window gets a new handle", again > w);
    c.check("close it", host::handle_close(again), Ok(()));
}

/// The bytes `a, 0xff, b`.
fn bad_utf8() -> [u8; 3] {
    [b'a', 0xff, b'b']
}

/// The address of `px`'s first pixel's bytes (text written into it).
fn cp_text(px: &[u32]) -> i32 {
    ptr(px.as_ptr())
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
