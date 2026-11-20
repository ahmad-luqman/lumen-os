//! `run clock`'s arguments: none (live, from the RTC) or `--at
//! H[H]:MM[:SS]` (frozen at that time); anything else an error naming the
//! argument, with the usage.

use alloc::string::{String, ToString};
use core::fmt;

use crate::TimeOfDay;

/// What `run clock [args]` takes.
pub const USAGE: &str = "usage: clock [--at H[H]:MM[:SS]]";

/// What the arguments ask for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// From the RTC.
    Live,
    /// At this time, with no ticks.
    Frozen(TimeOfDay),
}

/// Why the arguments were refused: what was wrong, then the usage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgError {
    /// `--at`'s value isn't a time of day.
    NotATime(String),
    /// `--at` with nothing after it.
    NoTime,
    /// An argument it doesn't take.
    Unknown(String),
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArgError::NotATime(t) => write!(f, "'{t}' is not a time of day ({USAGE})"),
            ArgError::NoTime => write!(f, "--at needs a time ({USAGE})"),
            ArgError::Unknown(a) => write!(f, "'{a}' is not an argument it takes ({USAGE})"),
        }
    }
}

/// `run clock`'s arguments (after the app's name) read.
pub fn mode<S: AsRef<str>>(args: &[S]) -> Result<Mode, ArgError> {
    let mut words = args.iter().map(AsRef::as_ref);
    let mode = match words.next() {
        None => return Ok(Mode::Live),
        Some("--at") => {
            let t = words.next().ok_or(ArgError::NoTime)?;
            Mode::Frozen(TimeOfDay::parse(t).ok_or_else(|| ArgError::NotATime(t.to_string()))?)
        }
        Some(other) => return Err(ArgError::Unknown(other.to_string())),
    };
    match words.next() {
        None => Ok(mode),
        Some(extra) => Err(ArgError::Unknown(extra.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn none_is_live_and_at_freezes() {
        assert_eq!(mode::<&str>(&[]), Ok(Mode::Live));
        assert_eq!(mode(&["--at", "10:09"]), Ok(Mode::Frozen(TimeOfDay::new(10, 9, 0).unwrap())));
        assert_eq!(mode(&["--at", "7:05:30"]), Ok(Mode::Frozen(TimeOfDay::new(7, 5, 30).unwrap())));
    }

    #[test]
    fn anything_else_names_what_was_wrong() {
        assert_eq!(mode(&["--at", "25:00"]), Err(ArgError::NotATime("25:00".into())));
        assert_eq!(mode(&["--at"]), Err(ArgError::NoTime));
        assert_eq!(mode(&["foo"]), Err(ArgError::Unknown("foo".into())));
        assert_eq!(mode(&["10:09"]), Err(ArgError::Unknown("10:09".into())));
        assert_eq!(mode(&["--at", "10:09", "x"]), Err(ArgError::Unknown("x".into())));
        assert_eq!(mode(&["--AT", "10:09"]), Err(ArgError::Unknown("--AT".into())));
    }

    #[test]
    fn its_messages() {
        assert_eq!(
            format!("{}", ArgError::NotATime("25:00".into())),
            "'25:00' is not a time of day (usage: clock [--at H[H]:MM[:SS]])"
        );
        assert_eq!(format!("{}", ArgError::NoTime), "--at needs a time (usage: clock [--at H[H]:MM[:SS]])");
        assert_eq!(
            format!("{}", ArgError::Unknown("foo".into())),
            "'foo' is not an argument it takes (usage: clock [--at H[H]:MM[:SS]])"
        );
    }
}
