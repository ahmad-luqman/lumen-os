//! The `lumen` host functions without a window (ARCHITECTURE section 8,
//! "The host interface"), defined into an app's `Linker`. Every call goes
//! through [`entry`] (the kill flag, a slice end if too little fuel is
//! left, `CALL_FUEL`), then checks its arguments in section 8's order
//! ("Every call, in this order"): a refused call writes nothing, and no
//! guest pointer is held across anything that can run the app. The window
//! and text calls come with #206.

use super::runtime::{self, AppCtx, AppState};
use crate::{rtc, sched, time, trace};
use alloc::vec;
use alloc::vec::Vec;
use lumen_app_api::end::EndReason;
use lumen_app_api::limits::{BYTE_FUEL, CALL_FUEL, EVENT_FUEL, LOG_FUEL, TEXT_BYTES, TRACE_EVENTS_PER_READ};
use lumen_app_api::text::{self, LogText};
use lumen_app_api::trace::{EVENT_SIZE, HEADER_SIZE, events_that_fit, header};
use lumen_app_api::{Error, Grant, Kind, LogLevel, range, returned, returned64};
use lumen_trace::{Event, Stream};
use wasmi::{Caller, Linker};

type C<'a> = Caller<'a, AppCtx>;
type Res<T> = Result<T, wasmi::Error>;

/// The app stops with `reason` (recorded, then the host error wasmi never
/// resumes).
fn stop(c: &mut C<'_>, reason: EndReason) -> wasmi::Error {
    c.data_mut().stop(reason)
}

/// Every call's entry: the kill flag; then, if the slice has less than
/// `CALL_FUEL` left, the slice ends right here (counted, a kill or the
/// close grace ending the app, the CPU given up, a fresh slice), since
/// wasmi checks fuel only where a block starts and a run of calls in one
/// block would otherwise be free; then `CALL_FUEL`, refused call or not;
/// then the call itself.
fn entry<T>(c: &mut C<'_>, call: impl FnOnce(&mut C<'_>) -> Res<T>) -> Res<T> {
    if c.data().state.kill_asked() {
        return Err(stop(c, EndReason::Killed));
    }
    if runtime::fuel_left(&*c) < CALL_FUEL {
        runtime::end_slice(&mut *c).map_err(|reason| stop(c, reason))?;
        sched::yield_now();
        runtime::begin_slice(&mut *c, 0).map_err(|reason| stop(c, reason))?;
    }
    runtime::charge(&mut *c, CALL_FUEL);
    call(c)
}

/// A call that ends the app (`exit`, `panic`): its entry, then the end
/// `reason` gives. Never `Ok`.
fn ending(c: &mut C<'_>, reason: impl FnOnce(&mut C<'_>) -> EndReason) -> Res<()> {
    entry(c, |c| {
        let reason = reason(c);
        Err(stop(c, reason))
    })
}

/// A blocking call's wait (`time_sleep`; #206's `window_wait`): the slice
/// ends (counted; a kill or the close grace ends the app), the app is
/// `waiting` in `call` while `wait` runs, a kill meanwhile ends it, then a
/// fresh slice.
fn blocking<T>(c: &mut C<'_>, call: &'static str, wait: impl FnOnce(&AppState) -> T) -> Res<T> {
    runtime::end_slice(&mut *c).map_err(|reason| stop(c, reason))?;
    let state = c.data().state.clone();
    let out = state.waiting(call, || wait(&state));
    if state.kill_asked() {
        return Err(stop(c, EndReason::Killed));
    }
    runtime::begin_slice(&mut *c, 0).map_err(|reason| stop(c, reason))?;
    Ok(out)
}

/// The app's memory's current size.
fn memory_len(c: &C<'_>) -> usize {
    c.data().memory.map_or(0, |m| m.data(c).len())
}

/// A copy of `[ptr, ptr + len)`, already checked by [`range`].
fn copy_in(c: &C<'_>, r: core::ops::Range<usize>) -> Vec<u8> {
    c.data().memory.map_or_else(Vec::new, |m| m.data(c)[r].to_vec())
}

/// `bytes` written at `at` (checked: within a range [`range`] accepted).
fn write_out(c: &mut C<'_>, at: usize, bytes: &[u8]) {
    if let Some(m) = c.data().memory {
        m.data_mut(c)[at..at + bytes.len()].copy_from_slice(bytes);
    }
}

pub(super) fn define(linker: &mut Linker<AppCtx>) -> Result<(), wasmi::Error> {
    let m = lumen_app_api::host::MODULE;
    linker.func_wrap(m, "handle_find", |mut c: C<'_>, kind: i32| -> Res<i32> {
        entry(&mut c, |c| {
            let grant = Grant::from_number(kind).ok_or(Error::BadArgument);
            Ok(returned(grant.and_then(|g| c.data_mut().find_grant(g))))
        })
    })?;
    linker.func_wrap(m, "handle_close", |mut c: C<'_>, h: i32| -> Res<i32> {
        entry(&mut c, |c| Ok(returned(c.data_mut().close(h).map(|()| 0))))
    })?;
    linker
        .func_wrap(m, "args_len", |mut c: C<'_>| -> Res<i32> { entry(&mut c, |c| Ok(c.data().args.len() as i32)) })?;
    linker.func_wrap(m, "args_read", |mut c: C<'_>, buf: i32, len: i32| -> Res<i32> {
        entry(&mut c, |c| Ok(returned(args_read(c, buf, len))))
    })?;
    linker
        .func_wrap(m, "exit", |mut c: C<'_>, code: i32| -> Res<()> { ending(&mut c, |_| EndReason::Exited(code)) })?;
    linker.func_wrap(m, "panic", |mut c: C<'_>, msg: i32, len: i32| -> Res<()> {
        ending(&mut c, |c| EndReason::Panicked(panic_message(c, msg, len)))
    })?;
    linker.func_wrap(m, "log", |mut c: C<'_>, h: i32, level: i32, msg: i32, len: i32| -> Res<i32> {
        entry(&mut c, |c| Ok(returned(log(c, h, level, msg, len))))
    })?;
    linker.func_wrap(m, "time_now", |mut c: C<'_>, h: i32| -> Res<i64> {
        entry(&mut c, |c| {
            let now = c.data().grant_of(h, Kind::Time).and_then(|()| time::since_boot_ns().ok_or(Error::Unavailable));
            Ok(returned64(now.map(|ns| ns as i64)))
        })
    })?;
    linker.func_wrap(m, "time_wall", |mut c: C<'_>, h: i32| -> Res<i64> {
        entry(&mut c, |c| {
            let wall = c.data().grant_of(h, Kind::Time).and_then(|()| {
                let now = rtc::now_quiet().map_err(|_| Error::Unavailable)?;
                now.unix_seconds().ok_or(Error::Unavailable)
            });
            Ok(returned64(wall.map(|s| s as i64)))
        })
    })?;
    linker.func_wrap(m, "time_sleep", |mut c: C<'_>, h: i32, ns: i64| -> Res<i32> {
        entry(&mut c, |c| {
            if let Err(e) = c.data().grant_of(h, Kind::Time) {
                return Ok(e.returned());
            }
            let Ok(ns) = u64::try_from(ns) else { return Ok(Error::BadArgument.returned()) };
            let deadline = time::now_ns().checked_add(ns);
            blocking(c, "time_sleep", |state| state.wait_until(deadline))?;
            Ok(0)
        })
    })?;
    linker.func_wrap(m, "trace_cpus", |mut c: C<'_>, h: i32| -> Res<i32> {
        entry(&mut c, |c| Ok(returned(c.data().grant_of(h, Kind::Trace).map(|()| trace::cpus() as i32))))
    })?;
    linker.func_wrap(
        m,
        "trace_read",
        |mut c: C<'_>, h: i32, stream: i32, cpu: i32, since: i64, buf: i32, len: i32| -> Res<i32> {
            entry(&mut c, |c| Ok(returned(trace_read(c, h, stream, cpu, since, buf, len))))
        },
    )?;
    Ok(())
}

/// `panic`'s message, however bad: at most `TEXT_BYTES` of it read (a
/// range past the memory, or a negative `len`, is `<a bad pointer>`),
/// invalid UTF-8 shown as U+FFFD, cut and sanitised as log text.
fn panic_message(c: &C<'_>, msg: i32, len: i32) -> LogText {
    if len < 0 {
        return text::log_line("<a bad pointer>");
    }
    match range::range(msg, len.min(TEXT_BYTES as i32), memory_len(c)) {
        Ok(r) => text::log_line_lossy(&copy_in(c, r)),
        Err(_) => text::log_line("<a bad pointer>"),
    }
}

fn args_read(c: &mut C<'_>, buf: i32, len: i32) -> Result<i32, Error> {
    let out = range::range(buf, len, memory_len(c))?;
    let n = c.data().args.len();
    if n > out.len() {
        return Err(Error::BufferTooSmall);
    }
    let args = c.data().args.clone();
    write_out(c, out.start, &args);
    Ok(n as i32)
}

fn log(c: &mut C<'_>, h: i32, level: i32, msg: i32, len: i32) -> Result<i32, Error> {
    c.data().grant_of(h, Kind::Log)?;
    let r = range::range(msg, len, memory_len(c))?;
    let level = match LogLevel::from_number(level).ok_or(Error::BadArgument)? {
        LogLevel::Error => log::Level::Error,
        LogLevel::Warn => log::Level::Warn,
        LogLevel::Info => log::Level::Info,
        LogLevel::Debug => log::Level::Debug,
        LogLevel::Trace => log::Level::Trace,
    };
    // The cap, checked before the text is copied (or decoded).
    if r.len() > TEXT_BYTES {
        return Err(Error::BadArgument);
    }
    let bytes = copy_in(c, r);
    // The copy is paid for even if the text is then refused.
    runtime::charge(&mut *c, BYTE_FUEL * bytes.len() as u64);
    let line = text::log_line(text::decode(&bytes, TEXT_BYTES)?);
    runtime::charge(&mut *c, LOG_FUEL);
    let name = c.data().state.name;
    log::log!(target: "app", level, "{name}: {line}");
    Ok(0)
}

#[allow(clippy::too_many_arguments, reason = "the host call's own arguments")]
fn trace_read(c: &mut C<'_>, h: i32, stream: i32, cpu: i32, since: i64, buf: i32, len: i32) -> Result<i32, Error> {
    c.data().grant_of(h, Kind::Trace)?;
    let out = range::range(buf, len, memory_len(c))?;
    let bytes = read_trace(stream, cpu, since, out.len())?;
    let copied = (bytes.len() - HEADER_SIZE) / EVENT_SIZE;
    runtime::charge(&mut *c, EVENT_FUEL * copied as u64);
    write_out(c, out.start, &bytes);
    Ok(copied as i32)
}

/// `trace_read`'s work, apart from the app's memory: CPU `cpu`'s ring
/// `stream` read from `since` into what a buffer of `len` bytes holds, as
/// the bytes to write (the header, then each event copied). Refuses a
/// negative or unknown stream, a negative or offline CPU, a negative
/// `since` or one past the ring's next sequence number (`BadArgument`), and
/// a buffer below a header and one event (`BufferTooSmall`).
pub(super) fn read_trace(stream: i32, cpu: i32, since: i64, len: usize) -> Result<Vec<u8>, Error> {
    let stream = u32::try_from(stream).ok().and_then(|s| Stream::try_from(s).ok()).ok_or(Error::BadArgument)?;
    let cpu = usize::try_from(cpu).map_err(|_| Error::BadArgument)?;
    let since = u64::try_from(since).map_err(|_| Error::BadArgument)?;
    let fit = events_that_fit(len)?;
    debug_assert!(fit <= TRACE_EVENTS_PER_READ);
    let mut events = vec![Event::default(); fit];
    let r = trace::read(stream, cpu, since, &mut events).map_err(|_| Error::BadArgument)?;
    let mut bytes = Vec::with_capacity(HEADER_SIZE + r.copied * EVENT_SIZE);
    bytes.extend_from_slice(&header(r.next, r.missed));
    for e in &events[..r.copied] {
        bytes.extend_from_slice(&lumen_app_api::trace::event(e));
    }
    Ok(bytes)
}
