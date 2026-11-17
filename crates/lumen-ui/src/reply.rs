//! A request's reply slot (ARCHITECTURE section 11, "A request is a reply
//! slot"): written once, by the asker or the owner, each write a
//! compare-and-swap on its state. The kernel keeps a [`Reply`] behind its
//! lock, shared by the asker (`ui invoke`) and the request in the window's
//! queue, and wakes the asker after each write; this is the state machine.
//!
//! - `Pending` to `Running`: the owner takes the request ([`Reply::start`]).
//! - `Pending` to `Cancelled`: the asker's 1 s deadline passed first
//!   ([`Reply::cancel`]); the owner then skips it ([`Reply::start`] says
//!   so), so a retry can't press twice.
//! - `Running` to `Done`: the owner's result ([`Reply::finish`]).
//! - `Pending` or `Running` to `Done`: unanswered when its window closed or
//!   the request was dropped ([`Reply::answer_unrun`]).
//!
//! The asker's steps after its deadlines are [`Reply::after_deadline`] and
//! [`Reply::after_second_deadline`].

/// Where a reply is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Reply<T> {
    /// Asked, not taken yet.
    #[default]
    Pending,
    /// The owner is running it.
    Running,
    /// The asker gave up before the owner took it: never run.
    Cancelled,
    /// Answered (and not yet taken by the asker, [`Reply::take`]).
    Done(T),
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

/// What the asker makes of a reply when its deadline passes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome<T> {
    /// Answered in time.
    Answered(T),
    /// Not taken in time: cancelled, it will never run.
    Cancelled,
    /// The owner is running it: wait a second deadline.
    Running,
    /// Answered after the first deadline, before the second: "ran late".
    Late(T),
    /// Still running at the second deadline.
    StillRunning,
}

impl<T> Reply<T> {
    pub fn new() -> Reply<T> {
        Reply::Pending
    }

    /// The owner takes the request: `Pending` to `Running`, else why not.
    pub fn start(&mut self) -> Result<(), NotStarted> {
        match self {
            Reply::Pending => {
                *self = Reply::Running;
                Ok(())
            }
            Reply::Cancelled => Err(NotStarted::Cancelled),
            _ => Err(NotStarted::NotPending),
        }
    }

    /// The asker's deadline: `Pending` to `Cancelled`; whether it was
    /// cancelled (false: the owner took it first, or it is answered).
    pub fn cancel(&mut self) -> bool {
        if matches!(self, Reply::Pending) {
            *self = Reply::Cancelled;
            true
        } else {
            false
        }
    }

    /// The owner's result: `Running` to `Done`; whether it was written.
    pub fn finish(&mut self, result: T) -> bool {
        if matches!(self, Reply::Running) {
            *self = Reply::Done(result);
            true
        } else {
            false
        }
    }

    /// Answered without running to its end: its window closed, or the
    /// request was dropped unanswered. `Pending` or `Running` to `Done`;
    /// whether it was written.
    pub fn answer_unrun(&mut self, result: T) -> bool {
        if matches!(self, Reply::Pending | Reply::Running) {
            *self = Reply::Done(result);
            true
        } else {
            false
        }
    }

    /// Whether it is answered (an asker waits for this, or a deadline).
    pub fn is_done(&self) -> bool {
        matches!(self, Reply::Done(_))
    }

    /// Whether nobody needs to write it any more (answered, taken or
    /// cancelled).
    pub fn is_settled(&self) -> bool {
        matches!(self, Reply::Done(_) | Reply::Taken | Reply::Cancelled)
    }

    /// The asker takes the answer, if there is one.
    pub fn take(&mut self) -> Option<T> {
        match core::mem::replace(self, Reply::Taken) {
            Reply::Done(t) => Some(t),
            other => {
                *self = other;
                None
            }
        }
    }

    /// The asker's first deadline passed (or it was woken answered): the
    /// answer, else cancel it if the owner hasn't taken it, else wait more.
    pub fn after_deadline(&mut self) -> Outcome<T> {
        if let Some(t) = self.take() {
            Outcome::Answered(t)
        } else if self.cancel() {
            Outcome::Cancelled
        } else {
            Outcome::Running
        }
    }

    /// The asker's second deadline passed (or it was woken answered): ran
    /// late, or still running.
    pub fn after_second_deadline(&mut self) -> Outcome<T> {
        match self.take() {
            Some(t) => Outcome::Late(t),
            None => Outcome::StillRunning,
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
        assert_eq!(r.after_deadline(), Outcome::Answered(7));
        assert!(r.is_settled() && !r.is_done());
    }

    #[test]
    fn cancelled_at_the_deadline_is_never_run() {
        // The asker's deadline passes before the owner takes it: cancelled,
        // and the owner, reaching it later, is told to skip it.
        let mut r: Reply<u32> = Reply::new();
        assert_eq!(r.after_deadline(), Outcome::Cancelled);
        assert_eq!(r.start(), Err(NotStarted::Cancelled));
        assert!(!r.finish(1));
        assert!(!r.answer_unrun(2), "a cancelled request isn't answered later");
        assert_eq!(r, Reply::Cancelled);
    }

    #[test]
    fn taken_before_the_deadline_then_answered_late() {
        // The owner took it before the deadline: the cancel fails, the
        // asker waits a second deadline, and an answer then is "ran late".
        let mut r = Reply::new();
        assert_eq!(r.start(), Ok(()));
        assert_eq!(r.after_deadline(), Outcome::Running);
        assert!(r.finish("row"));
        assert_eq!(r.after_second_deadline(), Outcome::Late("row"));
    }

    #[test]
    fn still_running_at_the_second_deadline() {
        let mut r: Reply<u32> = Reply::new();
        assert_eq!(r.start(), Ok(()));
        assert_eq!(r.after_deadline(), Outcome::Running);
        assert_eq!(r.after_second_deadline(), Outcome::StillRunning);
        // Its answer still lands (for nobody), once.
        assert!(r.finish(3));
        assert!(!r.finish(4));
    }

    #[test]
    fn a_closed_window_or_a_dropped_request_answers_it() {
        // Pending (its window closed before the owner took it) or running
        // (dropped by the owner without an answer): answered once.
        let mut pending = Reply::new();
        assert!(pending.answer_unrun("window closed"));
        assert_eq!(pending.after_deadline(), Outcome::Answered("window closed"));
        let mut running = Reply::new();
        assert_eq!(running.start(), Ok(()));
        assert!(running.answer_unrun("window closed"));
        assert!(!running.answer_unrun("again"));
        assert_eq!(running.start(), Err(NotStarted::NotPending));
        assert_eq!(running.take(), Some("window closed"));
        assert_eq!(running.take(), None);
    }

    #[test]
    fn an_answered_request_isn_t_started_or_cancelled() {
        let mut r = Reply::new();
        assert!(r.answer_unrun(1));
        assert_eq!(r.start(), Err(NotStarted::NotPending));
        assert!(!r.cancel());
    }
}
