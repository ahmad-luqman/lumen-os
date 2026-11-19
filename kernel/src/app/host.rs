//! The `lumen` host functions (ARCHITECTURE section 8, "The host interface"),
//! defined into an app's `Linker`. Every call, in this order:
//!
//! 1. checks the kill flag (set: the app ends, as a [`Stop`] host error);
//! 2. checks the handle, then its kind;
//! 3. checks every (pointer, length) range against the memory's current
//!    size (`lumen_app_api::range`: unsigned, no overflow);
//! 4. copies its inputs out of the app's memory and checks their content
//!    (text within its cap, UTF-8; values in range);
//! 5. does its work, charging fuel for it (saturating: a charge past what
//!    is left ends the slice at the next fuel check, resumably);
//! 6. only then writes its output whole, or nothing.
//!
//! So a refused call writes nothing, overlapping ranges are harmless, and
//! no guest pointer is held across anything that can run the app. The
//! window and text calls come with #206.

use super::runtime::{AppCtx, Obj, Stop};
use crate::{rtc, time, trace};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use lumen_app_api::limits::{
    BYTE_FUEL, CALL_FUEL, EVENT_FUEL, LOG_FUEL, SLICE_FUEL, TEXT_BYTES, TRACE_EVENTS_PER_READ,
};
use lumen_app_api::{Error, Grant, Kind, range, returned, returned64, text};
use lumen_trace::{Event, Stream};
use wasmi::{Caller, Linker};

type C<'a> = Caller<'a, AppCtx>;
type Res<T> = Result<T, wasmi::Error>;

/// Step 1: the kill flag.
fn not_killed(c: &C<'_>) -> Res<()> {
    if c.data().state.kill_asked() { Err(wasmi::Error::host(Stop::Killed)) } else { Ok(()) }
}

/// Step 5's charge: `fuel` off what the slice has left, saturating.
fn charge(c: &mut C<'_>, fuel: u64) {
    if let Ok(left) = c.get_fuel() {
        let _ = c.set_fuel(left.saturating_sub(fuel));
    }
}

/// Step 2: handle `h` names a grant of `kind`.
fn grant_of(c: &C<'_>, h: i32, kind: Kind) -> Result<(), Error> {
    match c.data().handles.get(h)? {
        Obj::Grant(g) if g.kind() == kind => Ok(()),
        Obj::Grant(_) => Err(Error::WrongKind),
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
    linker.func_wrap(m, "handle_find", |c: C<'_>, kind: i32| -> Res<i32> {
        not_killed(&c)?;
        Ok(returned(handle_find(c, kind)))
    })?;
    linker.func_wrap(m, "handle_close", |mut c: C<'_>, h: i32| -> Res<i32> {
        not_killed(&c)?;
        charge(&mut c, CALL_FUEL);
        let ctx = c.data_mut();
        Ok(returned(ctx.handles.remove(h).map(|Obj::Grant(g)| {
            // A closed grant is gone for the rest of the app's life.
            ctx.grants = ctx.grants.without(g);
            0
        })))
    })?;
    linker.func_wrap(m, "args_len", |mut c: C<'_>| -> Res<i32> {
        not_killed(&c)?;
        charge(&mut c, CALL_FUEL);
        Ok(c.data().args.len() as i32)
    })?;
    linker.func_wrap(m, "args_read", |mut c: C<'_>, buf: i32, len: i32| -> Res<i32> {
        not_killed(&c)?;
        Ok(returned(args_read(&mut c, buf, len)))
    })?;
    linker.func_wrap(m, "exit", |c: C<'_>, code: i32| -> Res<()> {
        not_killed(&c)?;
        Err(wasmi::Error::host(Stop::Exit(code)))
    })?;
    linker.func_wrap(m, "panic", |c: C<'_>, msg: i32, len: i32| -> Res<()> {
        not_killed(&c)?;
        // However bad the message, the app ends with it.
        let message = match range::range(msg, len.min(TEXT_BYTES as i32), memory_len(&c)) {
            Ok(r) if len >= 0 => text::log_line_lossy(&copy_in(&c, r)),
            _ => String::from("<a bad pointer>"),
        };
        Err(wasmi::Error::host(Stop::Panic(message)))
    })?;
    linker.func_wrap(m, "log", |mut c: C<'_>, h: i32, level: i32, msg: i32, len: i32| -> Res<i32> {
        not_killed(&c)?;
        Ok(returned(log(&mut c, h, level, msg, len)))
    })?;
    linker.func_wrap(m, "time_now", |mut c: C<'_>, h: i32| -> Res<i64> {
        not_killed(&c)?;
        let now = grant_of(&c, h, Kind::Time).and_then(|()| time::since_boot_ns().ok_or(Error::Unavailable));
        charge(&mut c, CALL_FUEL);
        Ok(returned64(now.map(|ns| ns as i64)))
    })?;
    linker.func_wrap(m, "time_wall", |mut c: C<'_>, h: i32| -> Res<i64> {
        not_killed(&c)?;
        let wall = grant_of(&c, h, Kind::Time).and_then(|()| {
            let now = rtc::now_quiet().map_err(|_| Error::Unavailable)?;
            now.unix_seconds().ok_or(Error::Unavailable)
        });
        charge(&mut c, CALL_FUEL);
        Ok(returned64(wall.map(|s| s as i64)))
    })?;
    linker.func_wrap(m, "time_sleep", |mut c: C<'_>, h: i32, ns: i64| -> Res<i32> {
        not_killed(&c)?;
        if let Err(e) = grant_of(&c, h, Kind::Time) {
            return Ok(e.returned());
        }
        if ns < 0 {
            return Ok(Error::BadArgument.returned());
        }
        charge(&mut c, CALL_FUEL);
        sleep(&mut c, ns as u64)?;
        Ok(0)
    })?;
    linker.func_wrap(m, "trace_cpus", |mut c: C<'_>, h: i32| -> Res<i32> {
        not_killed(&c)?;
        let cpus = grant_of(&c, h, Kind::Trace).map(|()| trace::cpus() as i32);
        charge(&mut c, CALL_FUEL);
        Ok(returned(cpus))
    })?;
    linker.func_wrap(
        m,
        "trace_read",
        |mut c: C<'_>, h: i32, stream: i32, cpu: i32, since: i64, buf: i32, len: i32| -> Res<i32> {
            not_killed(&c)?;
            Ok(returned(trace_read(&mut c, h, stream, cpu, since, buf, len)))
        },
    )?;
    Ok(())
}

fn handle_find(mut c: C<'_>, kind: i32) -> Result<i32, Error> {
    charge(&mut c, CALL_FUEL);
    let grant = Grant::from_number(kind).ok_or(Error::BadArgument)?;
    let ctx = c.data_mut();
    if !ctx.grants.has(grant) {
        return Err(Error::NotGranted);
    }
    // The same handle every time.
    match ctx.handles.find(|o| *o == Obj::Grant(grant)) {
        Some(h) => Ok(h),
        None => ctx.handles.insert(Obj::Grant(grant)),
    }
}

fn args_read(c: &mut C<'_>, buf: i32, len: i32) -> Result<i32, Error> {
    let out = range::range(buf, len, memory_len(c))?;
    charge(c, CALL_FUEL);
    let args = c.data().args.clone();
    if args.len() > out.len() {
        return Err(Error::BufferTooSmall);
    }
    write_out(c, out.start, &args);
    Ok(args.len() as i32)
}

fn log(c: &mut C<'_>, h: i32, level: i32, msg: i32, len: i32) -> Result<i32, Error> {
    grant_of(c, h, Kind::Log)?;
    let r = range::range(msg, len, memory_len(c))?;
    let level = match level {
        1 => log::Level::Error,
        2 => log::Level::Warn,
        3 => log::Level::Info,
        4 => log::Level::Debug,
        5 => log::Level::Trace,
        _ => return Err(Error::BadArgument),
    };
    // Checked against the cap before it is copied or decoded.
    if r.len() > TEXT_BYTES {
        return Err(Error::BadArgument);
    }
    let bytes = copy_in(c, r);
    let line = text::log_line(text::decode(&bytes, TEXT_BYTES)?);
    charge(c, CALL_FUEL + LOG_FUEL + BYTE_FUEL * bytes.len() as u64);
    let name = c.data().state.name;
    log::log!(target: "app", level, "{name}: {line}");
    Ok(0)
}

/// `time_sleep`'s wait: a slice end (counted, the fuel used added), then
/// blocking (`waiting`) on the app's own wait queue until `ns` pass or a
/// kill, then a fresh slice. A kill ends the app.
fn sleep(c: &mut C<'_>, ns: u64) -> Res<()> {
    let used = c.data().slice_given.saturating_sub(c.get_fuel().unwrap_or(0));
    let state = c.data().state.clone();
    state.slice_ended(used);
    let deadline = time::now_ns().checked_add(ns);
    state.block("time_sleep", deadline);
    if state.kill_asked() {
        return Err(wasmi::Error::host(Stop::Killed));
    }
    let _ = c.set_fuel(SLICE_FUEL);
    c.data_mut().slice_given = SLICE_FUEL;
    Ok(())
}

#[allow(clippy::too_many_arguments, reason = "the host call's own arguments")]
fn trace_read(c: &mut C<'_>, h: i32, stream: i32, cpu: i32, since: i64, buf: i32, len: i32) -> Result<i32, Error> {
    grant_of(c, h, Kind::Trace)?;
    let out = range::range(buf, len, memory_len(c))?;
    let bytes = read_trace(stream, cpu, since, out.len())?;
    let copied = (bytes.len() - lumen_app_api::trace::HEADER_SIZE) / lumen_app_api::trace::EVENT_SIZE;
    charge(c, CALL_FUEL + EVENT_FUEL * copied as u64);
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
