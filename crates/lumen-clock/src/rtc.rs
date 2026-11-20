//! What the clock shows while its RTC reads fail, as the taskbar's clock
//! does (`kernel/src/taskbar.rs`'s `Clock`): the last good reading for a
//! while, then no time at all (`--:--:--`, no hands); the first failure of
//! a run warned about once, a recovery said once.

use crate::TimeOfDay;

/// Failed reads in a row after which the last good reading is no longer
/// shown (the taskbar's number).
pub const CLOCK_FAILURES: u32 = 3;

/// The clock's readings so far.
#[derive(Debug, Default)]
pub struct Rtc {
    /// The last good reading.
    last: Option<TimeOfDay>,
    /// Failed reads in a row.
    failures: u32,
}

/// What a reading asks to be logged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note<E> {
    Nothing,
    /// The first failure of a run (a WARN): why, and whether the last good
    /// reading is kept for now (else nothing is shown).
    Failing {
        why: E,
        kept: bool,
    },
    /// Reads work again after failing (an INFO).
    Recovered,
}

impl Rtc {
    pub fn new() -> Rtc {
        Rtc::default()
    }

    /// One reading of the RTC (`time_wall`'s seconds, or why not): the time
    /// to show (`None`: none) and what to log. After a failure the last
    /// good reading stays shown until [`CLOCK_FAILURES`] fail in a row;
    /// with none, nothing is shown at once.
    pub fn read<E>(&mut self, reading: Result<u64, E>) -> (Option<TimeOfDay>, Note<E>) {
        match reading {
            Ok(secs) => {
                let note = if self.failures > 0 { Note::Recovered } else { Note::Nothing };
                self.failures = 0;
                let t = TimeOfDay::from_unix(secs);
                self.last = Some(t);
                (Some(t), note)
            }
            Err(why) => {
                let note =
                    if self.failures == 0 { Note::Failing { why, kept: self.last.is_some() } } else { Note::Nothing };
                self.failures = self.failures.saturating_add(1);
                let shown = self.last.filter(|_| self.failures < CLOCK_FAILURES);
                (shown, note)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> Option<TimeOfDay> {
        Some(TimeOfDay::from_unix(secs))
    }

    #[test]
    fn good_reads_show_the_time_and_say_nothing() {
        let mut rtc = Rtc::new();
        assert_eq!(rtc.read::<()>(Ok(60)), (t(60), Note::Nothing));
        assert_eq!(rtc.read::<()>(Ok(61)), (t(61), Note::Nothing));
    }

    #[test]
    fn failures_keep_the_last_reading_then_show_none() {
        let mut rtc = Rtc::new();
        rtc.read::<&str>(Ok(100));
        assert_eq!(rtc.read(Err("e1")), (t(100), Note::Failing { why: "e1", kept: true }));
        assert_eq!(rtc.read(Err("e2")), (t(100), Note::Nothing));
        assert_eq!(rtc.read(Err("e3")), (None, Note::Nothing));
        assert_eq!(rtc.read(Err("e4")), (None, Note::Nothing));
    }

    #[test]
    fn a_recovery_is_said_once_and_a_second_outage_warns_again() {
        let mut rtc = Rtc::new();
        rtc.read::<&str>(Ok(100));
        for e in ["a", "b", "c", "d"] {
            rtc.read(Err(e));
        }
        assert_eq!(rtc.read::<&str>(Ok(200)), (t(200), Note::Recovered));
        assert_eq!(rtc.read::<&str>(Ok(201)), (t(201), Note::Nothing));
        assert_eq!(rtc.read(Err("again")), (t(201), Note::Failing { why: "again", kept: true }));
        assert_eq!(rtc.read::<&str>(Ok(300)), (t(300), Note::Recovered));
    }

    #[test]
    fn failing_from_the_start_shows_none_at_once() {
        let mut rtc = Rtc::new();
        assert_eq!(rtc.read(Err("boot")), (None, Note::Failing { why: "boot", kept: false }));
        assert_eq!(rtc.read(Err("again")), (None, Note::Nothing));
        assert_eq!(rtc.read::<&str>(Ok(5)), (t(5), Note::Recovered));
    }

    #[test]
    fn the_count_never_wraps() {
        let mut rtc = Rtc { last: t(1), failures: u32::MAX };
        assert_eq!(rtc.read(Err(())), (None, Note::Nothing));
        assert_eq!(rtc.failures, u32::MAX);
    }
}
