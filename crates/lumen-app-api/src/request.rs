//! An app window's automation request, the kernel's side as plain logic
//! (#207; ARCHITECTURE section 8, "Apps' automation trees"), so the host
//! tests drive what the runtime does: [`take`], what becomes of a request
//! the window's queue hands over, and [`Running`], the one started request
//! the app answers (a refused answer leaving it running).

use crate::answer::AppAnswer;
use crate::event::Event;
use crate::limits::ANSWER_BYTES;
use crate::wire::DecodeError;
use lumen_ui::{Action, InvokeError, Role};

/// What becomes of a request taken from the window's queue; `S` is the
/// kernel's started request (its reply slot).
#[derive(Debug, PartialEq, Eq)]
pub enum Taken<S> {
    /// Cancelled at its asker's deadline: dropped, the next event taken.
    Skipped,
    /// Answered now, without the app: its node isn't in the newest tree
    /// (`NoNode`), the node's role doesn't take the action (`NotAllowed`:
    /// a window's root actions never reach an app), or a `set_value` text
    /// past its cap (`Arguments`).
    Answer { started: S, error: InvokeError },
    /// Handed to the app as this event (its node the app's own id).
    Deliver { started: S, event: Event },
}

/// Take a request: `started` is `None` if it was cancelled first, `node`
/// the app's id and role for the request's (global) node in the newest tree
/// (`None`: not in it).
pub fn take<S>(started: Option<S>, node: Option<(u64, Role)>, action: &Action) -> Taken<S> {
    let Some(started) = started else { return Taken::Skipped };
    let Some((local, role)) = node else { return Taken::Answer { started, error: InvokeError::NoNode } };
    let kind = action.kind();
    if !role.actions().contains(&kind) {
        return Taken::Answer { started, error: InvokeError::NotAllowed(role, kind) };
    }
    match Event::request(local, action) {
        Ok(event) => Taken::Deliver { started, event },
        Err(_) => Taken::Answer { started, error: InvokeError::Arguments },
    }
}

/// Why `window_answer` refused an answer (to the app: `BadArgument`); the
/// request, if any, stays running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnswerError {
    /// No request is running (none delivered, or answered already).
    NoneStarted,
    /// Past [`ANSWER_BYTES`] (checked before decoding).
    TooBig(usize),
    Decode(DecodeError),
}

/// A window's one running request: delivered to the app, not yet answered.
#[derive(Debug, Default)]
pub struct Running<S>(Option<S>);

impl<S> Running<S> {
    pub const fn new() -> Running<S> {
        Running(None)
    }

    /// The request delivered (one at a time: the window's queue refuses a
    /// second while one is queued or running), or handed back if one is
    /// running already (a bug in that refusal: the caller answers it).
    pub fn start(&mut self, started: S) -> Result<(), S> {
        if self.0.is_some() {
            return Err(started);
        }
        self.0 = Some(started);
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.0.is_some()
    }

    pub fn get(&self) -> Option<&S> {
        self.0.as_ref()
    }

    /// The request, taken unanswered (its app ended or closed its window).
    pub fn take(&mut self) -> Option<S> {
        self.0.take()
    }

    /// The app's answer `bytes`: the request and its answer, or why not (the
    /// request stays running).
    pub fn answer(&mut self, bytes: &[u8]) -> Result<(S, AppAnswer), AnswerError> {
        if self.0.is_none() {
            return Err(AnswerError::NoneStarted);
        }
        if bytes.len() > ANSWER_BYTES {
            return Err(AnswerError::TooBig(bytes.len()));
        }
        let answer = AppAnswer::decode(bytes).map_err(AnswerError::Decode)?;
        let started = self.0.take().ok_or(AnswerError::NoneStarted)?;
        Ok((started, answer))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::answer::Refusal;
    use crate::event::{Action as WireAction, SetValue};
    use crate::limits::SET_VALUE_BYTES;
    use alloc::string::String;
    use lumen_ui::ActionKind;

    #[test]
    fn taking_a_request_skips_answers_or_delivers_it() {
        // Cancelled first: skipped.
        assert_eq!(take::<u8>(None, Some((5, Role::Button)), &Action::Press), Taken::Skipped);
        // Its node not in the newest tree: "no node", without the app.
        assert_eq!(take(Some(1), None, &Action::Press), Taken::Answer { started: 1, error: InvokeError::NoNode });
        // A role that doesn't take the action (a window's root actions never
        // reach an app's node).
        for action in [Action::Close, Action::Minimize, Action::Restore, Action::Select] {
            let kind = action.kind();
            assert_eq!(
                take(Some(1), Some((5, Role::Button)), &action),
                Taken::Answer { started: 1, error: InvokeError::NotAllowed(Role::Button, kind) }
            );
        }
        // `focus` where the role doesn't take it (a label, a list item).
        for role in [Role::Label, Role::ListItem] {
            assert_eq!(
                take(Some(1), Some((5, role)), &Action::Focus),
                Taken::Answer { started: 1, error: InvokeError::NotAllowed(role, ActionKind::Focus) }
            );
        }
        // A list item takes `select`.
        assert!(matches!(take(Some(1), Some((5, Role::ListItem)), &Action::Select), Taken::Deliver { .. }));
        // A set_value text past its cap.
        let long = Action::SetValue("v".repeat(SET_VALUE_BYTES + 1));
        assert_eq!(
            take(Some(1), Some((5, Role::TextField)), &long),
            Taken::Answer { started: 1, error: InvokeError::Arguments }
        );
        // Otherwise delivered, as the app's own id.
        let event = Event::Request { node: 5, action: WireAction::Press };
        assert_eq!(take(Some(1), Some((5, Role::Button)), &Action::Press), Taken::Deliver { started: 1, event });
        let set = Action::SetValue(String::from("hi"));
        let event = Event::Request { node: 7, action: WireAction::SetValue(SetValue::try_from("hi").unwrap()) };
        assert_eq!(take(Some(2), Some((7, Role::TextField)), &set), Taken::Deliver { started: 2, event });
        assert_eq!(ActionKind::Press, Action::Press.kind());
    }

    #[test]
    fn a_refused_answer_leaves_the_request_running() {
        let mut r: Running<u8> = Running::new();
        assert_eq!(r.answer(&[0]), Err(AnswerError::NoneStarted));
        assert_eq!(r.start(9), Ok(()));
        assert!(r.is_running());
        // A second while one runs is handed back, the first kept.
        assert_eq!(r.start(10), Err(10));
        // Too big (before decoding), malformed, bytes left over: refused,
        // still running.
        assert_eq!(r.answer(&[0; ANSWER_BYTES + 1]), Err(AnswerError::TooBig(ANSWER_BYTES + 1)));
        assert!(matches!(r.answer(&[0; ANSWER_BYTES]), Err(AnswerError::Decode(DecodeError::LeftOver(_)))));
        assert!(matches!(r.answer(&[9]), Err(AnswerError::Decode(_))));
        assert!(matches!(r.answer(&[0, 0]), Err(AnswerError::Decode(DecodeError::LeftOver(1)))));
        assert_eq!(r.get(), Some(&9));
        // Answered once.
        assert_eq!(r.answer(&[1, 1]), Ok((9, AppAnswer::Refused(Refusal::Disabled))));
        assert!(!r.is_running());
        assert_eq!(r.answer(&[0]), Err(AnswerError::NoneStarted));
        // Taken unanswered (the app ended).
        assert_eq!(r.start(3), Ok(()));
        assert_eq!(r.take(), Some(3));
        assert_eq!(r.take(), None);
    }
}
