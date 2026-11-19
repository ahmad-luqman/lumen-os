//! The `lumen` host functions (ARCHITECTURE section 8, "The host interface"),
//! defined into an app's `Linker`. Every call, in this order:
//!
//! 1. checks the kill flag (set: the app ends, as a [`Stop`] host error),
//!    then charges `CALL_FUEL`, refused or not ([`entry`], one wrapper for
//!    every call);
//! 2. checks the handle, then its kind;
//! 3. checks every (pointer, length) range against the memory's current
//!    size (`lumen_app_api::range`: unsigned, no overflow);
//! 4. checks its other arguments' values (`log`'s level, a negative sleep)
//!    and copies its inputs out of the app's memory and checks their
//!    content (text within its cap, UTF-8);
//! 5. does its work, charging fuel for what it did (`BYTE_FUEL` per byte
//!    copied in, `LOG_FUEL` per line logged, `EVENT_FUEL` per event
//!    copied out; saturating: a charge past what is left ends the slice
//!    at the next fuel check, resumably);
//! 6. only then writes its output whole, or nothing.
//!
//! So a refused call writes nothing, overlapping ranges are harmless, and
//! no guest pointer is held across anything that can run the app. The
//! window and text calls come with #206.

use super::runtime::{AppCtx, Stop};
use crate::{rtc, time, trace};
use alloc::vec;
use alloc::vec::Vec;
use lumen_app_api::end::EndReason;
use lumen_app_api::limits::{BYTE_FUEL, CALL_FUEL, EVENT_FUEL, LOG_FUEL, TEXT_BYTES, TRACE_EVENTS_PER_READ};
use lumen_app_api::text::{self, LogText};
use lumen_app_api::{Error, Grant, Kind, LogLevel, range, returned, returned64};
use lumen_trace::{Event, Stream};
use wasmi::{Caller, Linker};

type C<'a> = Caller<'a, AppCtx>;
type Res<T> = Result<T, wasmi::Error>;

/// The app ends with `reason` (a host error wasmi never resumes).
fn stop(reason: EndReason) -> wasmi::Error {
    wasmi::Error::host(Stop(reason))
}

/// Step 1, every call's entry: the kill flag, then `CALL_FUEL`; then the
/// call itself.
fn entry<T>(c: &mut C<'_>, call: impl FnOnce(&mut C<'_>) -> Res<T>) -> Res<T> {
    if c.data().state.kill_asked() {
        return Err(stop(EndReason::Killed));
    }
    charge(c, CALL_FUEL);
    call(c)
}

/// A charge: `fuel` off what the slice has left, saturating.
fn charge(c: &mut C<'_>, fuel: u64) {
    if let Ok(left) = c.get_fuel() {
        let _ = c.set_fuel(left.saturating_sub(fuel));
    }
}

/// The app's memory's current size.
fn memory_len(c: &C<'_>) -> usize {
    c.data().memory.map_or(0, |m| m.data(c).len())
}

/// Step 4: a copy of `[ptr, ptr + len)`, already checked by [`range`].
fn copy_in(c: &C<'_>, r: core::ops::Range<usize>) -> Vec<u8> {
    c.data().memory.map_or_else(Vec::new, |m| m.data(c)[r].to_vec())
}

/// Step 6: `bytes` written at `at` (checked: within a range [`range`]
/// accepted).
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
    linker.func_wrap(m, "exit", |mut c: C<'_>, code: i32| -> Res<()> {
        entry(&mut c, |_| Err(stop(EndReason::Exited(code))))
    })?;
    linker.func_wrap(m, "panic", |mut c: C<'_>, msg: i32, len: i32| -> Res<()> {
        entry(&mut c, |c| Err(stop(EndReason::Panicked(panic_message(c, msg, len)))))
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
            sleep(c, ns)?;
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
/// range past the memory, or a negative length, is `<a bad pointer>`),
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
    // Checked against the cap before it is copied or decoded.
    if r.len() > TEXT_BYTES {
        return Err(Error::BadArgument);
    }
    let bytes = copy_in(c, r);
    charge(c, BYTE_FUEL * bytes.len() as u64);
    let line = text::log_line(text::decode(&bytes, TEXT_BYTES)?);
    charge(c, LOG_FUEL);
    let name = c.data().state.name;
    log::log!(target: "app", level, "{name}: {line}");
    Ok(0)
}

/// `time_sleep`'s wait: a slice end (counted, the fuel used added, the
/// kill flag and close grace checked), then blocking (`waiting`) on the
/// app's own wait queue until `ns` pass or a kill (0, or a deadline
/// already reached: a yield), then a fresh slice. A kill ends the app.
fn sleep(c: &mut C<'_>, ns: u64) -> Res<()> {
    let left = c.get_fuel().unwrap_or(0);
    c.data_mut().end_slice(left).map_err(stop)?;
    let state = c.data().state.clone();
    state.block("time_sleep", time::now_ns().checked_add(ns));
    if state.kill_asked() {
        return Err(stop(EndReason::Killed));
    }
    let fuel = c.data_mut().begin_slice(0);
    let _ = c.set_fuel(fuel);
    Ok(())
}

#[allow(clippy::too_many_arguments, reason = "the host call's own arguments")]
fn trace_read(c: &mut C<'_>, h: i32, stream: i32, cpu: i32, since: i64, buf: i32, len: i32) -> Result<i32, Error> {
    c.data().grant_of(h, Kind::Trace)?;
    let out = range::range(buf, len, memory_len(c))?;
    let bytes = read_trace(stream, cpu, since, out.len())?;
    let copied = (bytes.len() - lumen_app_api::trace::HEADER_SIZE) / lumen_app_api::trace::EVENT_SIZE;
    charge(c, EVENT_FUEL * copied as u64);
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
    let fit = lumen_app_api::trace::events_that_fit(len)?;
    debug_assert!(fit <= TRACE_EVENTS_PER_READ);
    let mut events = vec![Event::default(); fit];
    let r = trace::read(stream, cpu, since, &mut events).map_err(|_| Error::BadArgument)?;
    let mut bytes = Vec::with_capacity(lumen_app_api::trace::HEADER_SIZE + r.copied * lumen_app_api::trace::EVENT_SIZE);
    bytes.extend_from_slice(&lumen_app_api::trace::header(r.next, r.missed));
    for e in &events[..r.copied] {
        bytes.extend_from_slice(&lumen_app_api::trace::event(e));
    }
    Ok(bytes)
}
