//! How an app ends: one [`Outcome`], the first recorded ([`EndCell`]),
//! deciding its one end line (ARCHITECTURE section 8, "One end reason,
//! first writer wins"). The app's own thread is the only writer, through
//! #207: an end decided elsewhere (a `kill`, later the close grace's
//! `Close`) sets a flag the thread acts on at its next host call or slice
//! end. The cell still keeps the first record, so an app never gets two
//! end lines.

use crate::limits::CLOSE_GRACE_SLICES;
use crate::text::LogText;
use alloc::format;
use alloc::string::String;

/// Why an app ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// `lumen_main` returned this code, or the app called `exit` with it.
    Exited(i32),
    /// `kill`.
    Killed,
    /// Given `Close`, it held its window for [`CLOSE_GRACE_SLICES`] more
    /// slice ends.
    CloseGrace,
    /// `panic`, with its message (cut and sanitised as log text).
    Panicked(LogText),
    /// A wasm trap, with wasmi's message.
    Trapped(LogText),
}

/// How an app's life went: it never started (why), or it ended (why).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Loading failed: the module was refused, or there wasn't the memory.
    NotStarted(LogText),
    Ended(EndReason),
}

impl From<EndReason> for Outcome {
    fn from(reason: EndReason) -> Outcome {
        Outcome::Ended(reason)
    }
}

impl Outcome {
    /// What its line says after the app's name and id (`exited with 0`,
    /// `didn't start: ...`).
    pub fn what(&self) -> String {
        match self {
            Outcome::NotStarted(why) => format!("didn't start: {why}"),
            Outcome::Ended(reason) => reason.what(),
        }
    }

    /// Its line's level: a load failure is a fault, as a panic or trap is.
    pub fn level(&self) -> Level {
        match self {
            Outcome::NotStarted(_) => Level::Warn,
            Outcome::Ended(reason) => reason.level(),
        }
    }

    /// Its line's text (target `app`): `<name> (app <id>) <what>`.
    pub fn line(&self, name: &str, id: u64) -> String {
        format!("{name} (app {id}) {}", self.what())
    }
}

/// An end line's level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
}

impl EndReason {
    /// What the end line says after the app's name and id
    /// (`exited with 0`, `ended: trap: ...`).
    pub fn what(&self) -> String {
        match self {
            EndReason::Exited(code) => format!("exited with {code}"),
            EndReason::Killed => String::from("killed"),
            EndReason::CloseGrace => String::from("ended: didn't close its window"),
            EndReason::Panicked(msg) => format!("ended: panicked: {msg}"),
            EndReason::Trapped(msg) => format!("ended: trap: {msg}"),
        }
    }

    /// The end line's level: a panic or a trap is a fault (one WARN), the
    /// rest an app ending as asked.
    pub fn level(&self) -> Level {
        match self {
            EndReason::Panicked(_) | EndReason::Trapped(_) => Level::Warn,
            EndReason::Exited(_) | EndReason::Killed | EndReason::CloseGrace => Level::Info,
        }
    }
}

/// An app's outcome: empty until the first is recorded, which no later one
/// replaces.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndCell(Option<Outcome>);

impl EndCell {
    pub const fn new() -> EndCell {
        EndCell(None)
    }

    /// Record `outcome` unless one is already: whether it was this one.
    pub fn record(&mut self, outcome: impl Into<Outcome>) -> bool {
        if self.0.is_some() {
            return false;
        }
        self.0 = Some(outcome.into());
        true
    }

    /// The outcome recorded, if any.
    pub fn outcome(&self) -> Option<&Outcome> {
        self.0.as_ref()
    }
}

/// The close grace's count: once an app has been given `Close` (and still
/// holds its window), each slice end counts, and the
/// [`CLOSE_GRACE_SLICES`]th more ends it. An app given `Close` that closes
/// its window is done with it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloseGrace {
    /// Slice ends since `Close`, once it was given.
    since_close: Option<u32>,
}

impl CloseGrace {
    /// The app was given `Close` (the first time counts).
    pub fn given(&mut self) {
        self.since_close.get_or_insert(0);
    }

    /// It closed its window: no grace runs any more.
    pub fn closed(&mut self) {
        self.since_close = None;
    }

    /// A slice ended: whether the app has used up its grace.
    pub fn slice_end(&mut self) -> bool {
        match &mut self.since_close {
            Some(n) => {
                *n = n.saturating_add(1);
                *n >= CLOSE_GRACE_SLICES
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text;

    #[test]
    fn the_first_reason_recorded_wins() {
        let mut cell = EndCell::new();
        assert_eq!(cell.outcome(), None);
        assert!(cell.record(EndReason::Killed));
        assert!(!cell.record(EndReason::Exited(0)));
        assert!(!cell.record(EndReason::Trapped(text::log_line("unreachable"))));
        assert!(!cell.record(Outcome::NotStarted(text::log_line("x"))));
        assert_eq!(cell.outcome(), Some(&Outcome::Ended(EndReason::Killed)));
    }

    #[test]
    fn end_lines_and_their_levels() {
        let cases = [
            (EndReason::Exited(0), Level::Info, "hello (app 3) exited with 0"),
            (EndReason::Exited(-2), Level::Info, "hello (app 3) exited with -2"),
            (EndReason::Killed, Level::Info, "hello (app 3) killed"),
            (EndReason::CloseGrace, Level::Info, "hello (app 3) ended: didn't close its window"),
            (EndReason::Panicked(text::log_line("oops")), Level::Warn, "hello (app 3) ended: panicked: oops"),
            (
                EndReason::Trapped(text::log_line("wasm `unreachable` instruction executed")),
                Level::Warn,
                "hello (app 3) ended: trap: wasm `unreachable` instruction executed",
            ),
        ];
        for (reason, level, line) in cases {
            assert_eq!(reason.level(), level);
            assert_eq!(Outcome::from(reason).line("hello", 3), line);
        }
    }

    #[test]
    fn a_panic_message_cant_forge_a_line() {
        let r = EndReason::Panicked(text::log_line("bad\nWARN  app: fake"));
        assert_eq!(r.what(), "ended: panicked: bad\u{fffd}WARN  app: fake");
    }

    #[test]
    fn a_load_failure_is_a_warning_of_its_own() {
        let o = Outcome::NotStarted(text::log_line("memory: not fixed"));
        assert_eq!(o.level(), Level::Warn);
        assert_eq!(o.line("hello", 4), "hello (app 4) didn't start: memory: not fixed");
        let o: Outcome = EndReason::Exited(3).into();
        assert_eq!((o.level(), o.what()), (Level::Info, String::from("exited with 3")));
    }

    #[test]
    fn the_close_grace_counts_slice_ends_after_close() {
        let mut g = CloseGrace::default();
        // No Close yet: slices don't count.
        for _ in 0..10 {
            assert!(!g.slice_end());
        }
        g.given();
        for _ in 1..CLOSE_GRACE_SLICES {
            assert!(!g.slice_end());
        }
        assert!(g.slice_end());
        // Given again later doesn't restart a running grace.
        let mut g = CloseGrace::default();
        g.given();
        assert!(!g.slice_end());
        g.given();
        for _ in 2..CLOSE_GRACE_SLICES {
            assert!(!g.slice_end());
        }
        assert!(g.slice_end());
        // Closing the window ends the grace.
        let mut g = CloseGrace::default();
        g.given();
        g.closed();
        for _ in 0..10 {
            assert!(!g.slice_end());
        }
        // The count saturates rather than wrapping back under the grace.
        let mut g = CloseGrace { since_close: Some(u32::MAX - 1) };
        assert!(g.slice_end());
        assert!(g.slice_end());
    }
}
