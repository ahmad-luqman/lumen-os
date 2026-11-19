//! The types of Lumen's WASM app interface (M5; ARCHITECTURE section 8,
//! "Apps: WASM modules in the kernel"), shared by the kernel's runtime
//! (`kernel/src/app`) and the apps (through `apps/`'s path dependency), so
//! the two can't disagree on a number:
//!
//! - [`Error`]: the codes a host call returns, negated;
//! - [`Kind`], [`Grant`], [`Listable`] and [`Grants`]: capability handles'
//!   kinds, what `handle_find` takes, what a manifest and `run --without`
//!   name;
//! - [`manifest`]: the `lumen.manifest` custom section, found and parsed;
//! - [`limits`]: every limit, and each app's worst case checked against the
//!   budget at compile time;
//! - [`handles`]: an app's handle table (opaque ids, never reused);
//! - [`range`]: a guest (pointer, length) range checked against its memory;
//! - [`text`]: log text cut and sanitised, so it can't forge a line;
//! - [`end`]: why an app ended (one reason, the first recorded) and its end
//!   line;
//! - [`host`]: the `lumen` functions a module may import, with their
//!   signatures;
//! - [`trace`]: `trace_read`'s header and event layouts;
//! - [`args`]: `run`'s arguments as the app reads them.
//!
//! Wire formats for events, trees and answers (postcard) come with the
//! window calls (#206, #207).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod args;
pub mod end;
pub mod handles;
pub mod host;
pub mod limits;
pub mod manifest;
pub mod range;
pub mod text;
pub mod trace;

pub use lumen_trace::kind;

use core::fmt;

/// What a host call returns when it fails, negated: `-1` is
/// [`BadHandle`](Error::BadHandle). Stable, never reused (pinned by a test).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum Error {
    /// No such open handle.
    BadHandle = 1,
    /// A handle of a kind the call doesn't take.
    WrongKind = 2,
    /// `handle_find` for a grant the app doesn't hold.
    NotGranted = 3,
    /// A range outside the app's memory, an overflow, or an unaligned
    /// canvas.
    BadPointer = 4,
    /// Bad UTF-8, a value out of range, text past its cap, an encoding that
    /// doesn't decode or breaks its rules.
    BadArgument = 5,
    /// The output didn't fit: nothing written.
    BufferTooSmall = 6,
    /// A count reached: windows, handles.
    Limit = 7,
    /// The object can't answer now: no desktop, no window, the RTC failed.
    Unavailable = 8,
    /// The heap refused although the worst case fits (a bug).
    OutOfMemory = 9,
}

impl Error {
    /// Every code, in order.
    pub const ALL: [Error; 9] = [
        Error::BadHandle,
        Error::WrongKind,
        Error::NotGranted,
        Error::BadPointer,
        Error::BadArgument,
        Error::BufferTooSmall,
        Error::Limit,
        Error::Unavailable,
        Error::OutOfMemory,
    ];

    /// Its code (positive).
    pub const fn code(self) -> i32 {
        self as i32
    }

    /// What a host call returns for it: the code negated.
    pub const fn returned(self) -> i32 {
        -(self as i32)
    }

    /// The error a host call's negative result names, if any.
    pub fn from_returned(result: i64) -> Option<Error> {
        Error::ALL.into_iter().find(|e| i64::from(e.returned()) == result)
    }

    /// Its name, as apps log it (`NotGranted`).
    pub const fn name(self) -> &'static str {
        match self {
            Error::BadHandle => "BadHandle",
            Error::WrongKind => "WrongKind",
            Error::NotGranted => "NotGranted",
            Error::BadPointer => "BadPointer",
            Error::BadArgument => "BadArgument",
            Error::BufferTooSmall => "BufferTooSmall",
            Error::Limit => "Limit",
            Error::Unavailable => "Unavailable",
            Error::OutOfMemory => "OutOfMemory",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A host call's result as the app gets it: the value, or the error
/// negated.
pub fn returned(result: Result<i32, Error>) -> i32 {
    match result {
        Ok(v) => v,
        Err(e) => e.returned(),
    }
}

/// [`returned`] for the calls returning `i64`.
pub fn returned64(result: Result<i64, Error>) -> i64 {
    match result {
        Ok(v) => v,
        Err(e) => i64::from(e.returned()),
    }
}

/// A capability handle's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum Kind {
    /// The kernel log, lines tagged with the app's name: always granted.
    Log = 1,
    /// The clocks: `caps` `time`.
    Time = 2,
    /// Opening a window, the shared UI font: `caps` `desktop`.
    Desktop = 3,
    /// The app's window: only by `window_open`.
    Window = 4,
    /// The per-CPU trace rings, read only: `caps` `trace`.
    Trace = 5,
}

/// What `handle_find` takes: the kinds an app is granted (not `Window`,
/// which `window_open` makes). Its numbers are the kinds'.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum Grant {
    Log = 1,
    Time = 2,
    Desktop = 3,
    Trace = 5,
}

impl Grant {
    pub const ALL: [Grant; 4] = [Grant::Log, Grant::Time, Grant::Desktop, Grant::Trace];

    /// The grant `handle_find(number)` asks for: `None` for `Window` (4)
    /// and any number that names no grant (both `BadArgument`).
    pub fn from_number(number: i32) -> Option<Grant> {
        Grant::ALL.into_iter().find(|g| *g as i32 == number)
    }

    /// The kind of handle it is.
    pub const fn kind(self) -> Kind {
        match self {
            Grant::Log => Kind::Log,
            Grant::Time => Kind::Time,
            Grant::Desktop => Kind::Desktop,
            Grant::Trace => Kind::Trace,
        }
    }

    /// Its bit in [`Grants`].
    const fn bit(self) -> u8 {
        1 << (self as i32 as u8)
    }
}

/// What a manifest's `caps=` and `run --without` name: the grants that
/// aren't always there (`Log` is).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Listable {
    Time,
    Desktop,
    Trace,
}

impl Listable {
    pub const ALL: [Listable; 3] = [Listable::Time, Listable::Desktop, Listable::Trace];

    /// Its name, as `caps=` and `run --without` write it.
    pub const fn name(self) -> &'static str {
        match self {
            Listable::Time => "time",
            Listable::Desktop => "desktop",
            Listable::Trace => "trace",
        }
    }

    /// The listable grant `name` names (`log` isn't one).
    pub fn from_name(name: &str) -> Option<Listable> {
        Listable::ALL.into_iter().find(|l| l.name() == name)
    }

    pub const fn grant(self) -> Grant {
        match self {
            Listable::Time => Grant::Time,
            Listable::Desktop => Grant::Desktop,
            Listable::Trace => Grant::Trace,
        }
    }
}

/// A set of grants.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Grants(u8);

impl Grants {
    pub const NONE: Grants = Grants(0);

    /// `Log` alone: what every app starts with.
    pub const fn log() -> Grants {
        Grants(Grant::Log.bit())
    }

    pub const fn with(self, g: Grant) -> Grants {
        Grants(self.0 | g.bit())
    }

    pub const fn without(self, g: Grant) -> Grants {
        Grants(self.0 & !g.bit())
    }

    pub const fn has(self, g: Grant) -> bool {
        self.0 & g.bit() != 0
    }

    /// The grants in it, in number order.
    pub fn iter(self) -> impl Iterator<Item = Grant> {
        Grant::ALL.into_iter().filter(move |g| self.has(*g))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_are_pinned() {
        let codes: Vec<(i32, &str)> = Error::ALL.iter().map(|e| (e.code(), e.name())).collect();
        assert_eq!(
            codes,
            [
                (1, "BadHandle"),
                (2, "WrongKind"),
                (3, "NotGranted"),
                (4, "BadPointer"),
                (5, "BadArgument"),
                (6, "BufferTooSmall"),
                (7, "Limit"),
                (8, "Unavailable"),
                (9, "OutOfMemory"),
            ]
        );
        for e in Error::ALL {
            assert_eq!(e.returned(), -e.code());
            assert_eq!(Error::from_returned(i64::from(e.returned())), Some(e));
        }
        assert_eq!(Error::from_returned(0), None);
        assert_eq!(Error::from_returned(-10), None);
        assert_eq!(returned(Ok(7)), 7);
        assert_eq!(returned(Err(Error::Limit)), -7);
        assert_eq!(returned64(Err(Error::Unavailable)), -8);
    }

    #[test]
    fn kind_and_grant_numbers_are_pinned() {
        let kinds = [Kind::Log, Kind::Time, Kind::Desktop, Kind::Window, Kind::Trace];
        assert_eq!(kinds.map(|k| k as i32), [1, 2, 3, 4, 5]);
        assert_eq!(Grant::ALL.map(|g| g as i32), [1, 2, 3, 5]);
        for g in Grant::ALL {
            assert_eq!(g.kind() as i32, g as i32);
            assert_eq!(Grant::from_number(g as i32), Some(g));
        }
        // Window (4) and numbers naming no grant.
        for n in [-1, 0, 4, 6, i32::MAX] {
            assert_eq!(Grant::from_number(n), None, "{n}");
        }
    }

    #[test]
    fn listable_names_and_grant_sets() {
        assert_eq!(Listable::ALL.map(|l| l.name()), ["time", "desktop", "trace"]);
        assert_eq!(Listable::from_name("time"), Some(Listable::Time));
        assert_eq!(Listable::from_name("log"), None);
        assert_eq!(Listable::from_name("Time"), None);
        let g = Grants::log().with(Grant::Trace).with(Grant::Time);
        assert_eq!(g.iter().collect::<Vec<_>>(), [Grant::Log, Grant::Time, Grant::Trace]);
        assert!(!g.without(Grant::Time).has(Grant::Time));
        assert!(!Grants::NONE.has(Grant::Log));
    }
}
