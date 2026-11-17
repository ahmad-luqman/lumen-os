//! A request's reply slot (ARCHITECTURE section 11, "A request is a reply
//! slot"): each transition made once, checked against the state it starts
//! from. The kernel keeps a [`Reply`] behind its lock, shared by the asker
//! (`ui invoke`) and the request in the window's queue, and wakes the asker
//! after each answer; this is the state machine, its state private so it
//! moves only by these steps:
//!
//! - `Pending` to `Running`: the owner takes the request ([`Reply::start`]).
//! - `Pending` to `Cancelled`: the asker's 1 s deadline passed first
//!   ([`Reply::cancel`]); the owner then skips it ([`Reply::start`] says
//!   so), so a retry can't press twice.
//! - `Running` to `Done`: the owner's result ([`Reply::finish`]), or the
//!   request dropped while running ([`Reply::answer_dropped`]).
//! - `Pending` to `Done`: unanswered when its window closed or the request
//!   was dropped before it ran ([`Reply::answer_unrun`]).
//! - `Done` to `Taken`: the asker takes the answer ([`Reply::take`]).
//!
//! The asker's steps after its deadlines are [`Reply::after_first_deadline`]
//! ([`First`]) and [`Reply::after_second_deadline`] ([`Second`]).

/// A reply slot: see the module's doc. Not `Clone` (one slot per request).
#[derive(Debug, PartialEq, Eq)]
pub struct Reply<T> {
    state: State<T>,
}

#[derive(Debug, PartialEq, Eq)]
enum State<T> {
    Pending,
    Running,
    Cancelled,
    Done(T),
    Taken,
}

/// Where a reply is, without its answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Asked, not taken by the owner yet.
    Pending,
    /// The owner is running it.
    Running,
    /// The asker gave up before the owner took it: never run.
    Cancelled,
    /// Answered, not yet taken by the asker.
    Done,
    /// The asker took the answer.
    Taken,
}

/// Why the owner may not run a request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotStarted {
    /// Cancelled at the asker's deadline: skip it.
    Cancelled,
    /// Already taken, or answered.
    NotPending,
}

/// What the asker makes of a reply at its first deadline (or woken
/// answered).
#[derive(Debug, PartialEq, Eq)]
pub enum First<T> {
    /// Answered in time.
    Answered(T),
    /// Not taken in time: cancelled, it will never run.
    Cancelled,
    /// The owner is running it: wait a second deadline.
    Running,
}

/// What the asker makes of a running reply at its second deadline (or
/// woken answered).
#[derive(Debug, PartialEq, Eq)]
pub enum Second<T> {
    /// Answered after the first deadline, before the second: "ran late".
    Late(T),
    /// Still running.
    StillRunning,
}

impl<T> Reply<T> {
    #[allow(clippy::new_without_default, reason = "a fresh reply is made for a request, not a default")]
    pub fn new() -> Reply<T> {
        Reply { state: State::Pending }
    }

    pub fn phase(&self) -> Phase {
        match self.state {
            State::Pending => Phase::Pending,
            State::Running => Phase::Running,
            State::Cancelled => Phase::Cancelled,
            State::Done(_) => Phase::Done,
            State::Taken => Phase::Taken,
        }
    }

    /// The owner takes the request: `Pending` to `Running`, else why not.
    pub fn start(&mut self) -> Result<(), NotStarted> {
        match self.state {
            State::Pending => {
                self.state = State::Running;
                Ok(())
            }
            State::Cancelled => Err(NotStarted::Cancelled),
            _ => Err(NotStarted::NotPending),
        }
    }

    /// The asker's deadline: `Pending` to `Cancelled`; whether it was
    /// cancelled (false: the owner took it first, or it is answered).
    pub fn cancel(&mut self) -> bool {
        self.step(Phase::Pending, || State::Cancelled)
    }

    /// The owner's result: `Running` to `Done`; whether it was written.
    pub fn finish(&mut self, result: T) -> bool {
        self.step(Phase::Running, || State::Done(result))
    }

    /// Dropped while running, unanswered: `Running` to `Done`; whether it
    /// was written.
    pub fn answer_dropped(&mut self, result: T) -> bool {
        self.step(Phase::Running, || State::Done(result))
    }

    /// Answered without running: its window closed, or the request was
    /// dropped, before the owner took it. `Pending` to `Done`; whether it
    /// was written.
    pub fn answer_unrun(&mut self, result: T) -> bool {
        self.step(Phase::Pending, || State::Done(result))
    }

    /// Move to `to()` if in phase `from`; whether it moved.
    fn step(&mut self, from: Phase, to: impl FnOnce() -> State<T>) -> bool {
        if self.phase() == from {
            self.state = to();
            true
        } else {
            false
        }
    }

    /// Whether it is answered (an asker waits for this, or a deadline).
    pub fn is_done(&self) -> bool {
        self.phase() == Phase::Done
    }

    /// The asker takes the answer, if there is one: `Done` to `Taken`.
    pub fn take(&mut self) -> Option<T> {
        match core::mem::replace(&mut self.state, State::Taken) {
            State::Done(t) => Some(t),
            other => {
                self.state = other;
                None
            }
        }
    }

    /// The asker's first deadline passed (or it was woken answered): the
    /// answer, else cancel it if the owner hasn't taken it, else wait more.
    pub fn after_first_deadline(&mut self) -> First<T> {
        if let Some(t) = self.take() {
            First::Answered(t)
        } else if self.cancel() {
            First::Cancelled
        } else {
            First::Running
        }
    }

    /// The asker's second deadline passed (or it was woken answered): ran
    /// late, or still running.
    pub fn after_second_deadline(&mut self) -> Second<T> {
        match self.take() {
            Some(t) => Second::Late(t),
            None => Second::StillRunning,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answered_in_time() {
        let mut r = Reply::new();
        assert_eq!(r.start(), Ok(()));
        assert!(r.finish(7));
        assert!(r.is_done());
        assert_eq!(r.after_first_deadline(), First::Answered(7));
        assert_eq!(r.phase(), Phase::Taken);
    }

    #[test]
    fn cancelled_at_the_deadline_is_never_run() {
        // The asker's deadline passes before the owner takes it: cancelled,
        // and the owner, reaching it later, is told to skip it.
        let mut r: Reply<u32> = Reply::new();
        assert_eq!(r.after_first_deadline(), First::Cancelled);
        assert_eq!(r.start(), Err(NotStarted::Cancelled));
        assert!(!r.finish(1));
        assert!(!r.answer_unrun(2), "a cancelled request isn't answered later");
        assert!(!r.answer_dropped(3));
        assert_eq!(r.phase(), Phase::Cancelled);
    }

    #[test]
    fn taken_before_the_deadline_then_answered_late() {
        // The owner took it before the deadline: the cancel fails, the
        // asker waits a second deadline, and an answer then is "ran late".
        let mut r = Reply::new();
        assert_eq!(r.start(), Ok(()));
        assert_eq!(r.after_first_deadline(), First::Running);
        assert!(r.finish("row"));
        assert_eq!(r.after_second_deadline(), Second::Late("row"));
    }

    #[test]
    fn still_running_at_the_second_deadline() {
        let mut r: Reply<u32> = Reply::new();
        assert_eq!(r.start(), Ok(()));
        assert_eq!(r.after_first_deadline(), First::Running);
        assert_eq!(r.after_second_deadline(), Second::StillRunning);
        // Its answer still lands (for nobody), once.
        assert!(r.finish(3));
        assert!(!r.finish(4));
    }

    #[test]
    fn a_closed_window_or_a_dropped_request_answers_it() {
        // Pending (its window closed before the owner took it): answered
        // unrun; running (dropped by the owner without an answer): answered
        // as dropped. Each once, and each only from its own state.
        let mut pending = Reply::new();
        assert!(!pending.answer_dropped("dropped"), "not running");
        assert!(pending.answer_unrun("window closed"));
        assert_eq!(pending.after_first_deadline(), First::Answered("window closed"));
        let mut running = Reply::new();
        assert_eq!(running.start(), Ok(()));
        assert!(!running.answer_unrun("unrun"), "it ran");
        assert!(running.answer_dropped("dropped"));
        assert!(!running.answer_dropped("again"));
        assert_eq!(running.start(), Err(NotStarted::NotPending));
        assert_eq!(running.take(), Some("dropped"));
        assert_eq!(running.take(), None);
    }

    #[test]
    fn an_answered_request_isn_t_started_or_cancelled() {
        let mut r = Reply::new();
        assert!(r.answer_unrun(1));
        assert_eq!(r.start(), Err(NotStarted::NotPending));
        assert!(!r.cancel());
        assert_eq!(r.phase(), Phase::Done);
    }
}
