//! [`AppAnswer`], what an app answers a started request with
//! (`window_answer`, #207; ARCHITECTURE section 8, "Wire formats" and
//! "Apps' automation trees"): what `lumen_ui::serve` said, without the row,
//! which the runtime builds from its own copy of the newest tree.

use crate::limits::TEXT_BYTES;
use crate::wire::{Capped, DecodeError};
use lumen_ui::{ActionKind, InvokeError, Role, Served};
use serde::{Deserialize, Serialize};

wire_enum! {
    /// Why the app's toolkit refused a request (`lumen_ui::InvokeError`,
    /// without what the runtime knows itself: the role and the action).
    pub enum Refusal {
        NoNode = 0,
        Disabled = 1,
        Hidden = 2,
        NotAllowed = 3,
        Arguments = 4,
    }
}

/// `Failed`'s text: at most [`TEXT_BYTES`] (then cut and sanitised as log
/// text by the runtime).
pub type FailText = Capped<TEXT_BYTES>;

/// An app's answer, in its numbered order (pinned by a test).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AppAnswer {
    /// Done: the runtime answers the node's row from the newest tree (or
    /// "ran; node gone" if it isn't in it).
    Ran,
    /// Refused: it changed nothing.
    Refused(Refusal),
    /// Done, and the action closed the app's window: the runtime closes it,
    /// then answers.
    ClosedItsWindow,
    /// The node took the action, but what it was for failed, and why.
    Failed(FailText),
}

impl AppAnswer {
    /// What `served` (`lumen_ui::serve`'s result, in the app) answers.
    pub fn from_served(served: &Served) -> AppAnswer {
        match served {
            Served::Row { .. } | Served::RanNodeGone => AppAnswer::Ran,
            Served::Error(e) => AppAnswer::Refused(match e {
                InvokeError::NoNode => Refusal::NoNode,
                InvokeError::Disabled => Refusal::Disabled,
                InvokeError::Hidden => Refusal::Hidden,
                InvokeError::NotAllowed(..) => Refusal::NotAllowed,
                // A wire action always names one: wrong arguments, if ever.
                InvokeError::NoSuchAction | InvokeError::Arguments => Refusal::Arguments,
            }),
            Served::ClosedItsWindow => AppAnswer::ClosedItsWindow,
            Served::Failed(why) => AppAnswer::Failed(FailText::cut(why)),
        }
    }

    /// Its encoding (never past [`crate::limits::ANSWER_BYTES`]: the
    /// largest, a `Failed` of `TEXT_BYTES`, is tested).
    pub fn encode(&self) -> Result<alloc::vec::Vec<u8>, postcard::Error> {
        postcard::to_allocvec(self)
    }

    /// The answer `bytes` hold, exactly (bytes left over refused). The
    /// caller checks `ANSWER_BYTES` first.
    pub fn decode(bytes: &[u8]) -> Result<AppAnswer, DecodeError> {
        crate::wire::decode_exact(bytes)
    }
}

impl Refusal {
    /// The asker's error, with the node's `role` and the request's `action`
    /// (what `NotAllowed` names).
    pub fn invoke_error(self, role: Role, action: ActionKind) -> InvokeError {
        match self {
            Refusal::NoNode => InvokeError::NoNode,
            Refusal::Disabled => InvokeError::Disabled,
            Refusal::Hidden => InvokeError::Hidden,
            Refusal::NotAllowed => InvokeError::NotAllowed(role, action),
            Refusal::Arguments => InvokeError::Arguments,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::ANSWER_BYTES;
    use crate::wire::Capped;
    use alloc::vec;
    use alloc::vec::Vec;
    use lumen_gfx::Rect;
    use lumen_ui::{AutomationNode, NodeId};
    use postcard::Error as P;

    /// How many variants `AppAnswer` has (see `tree`'s `NODE_VARIANTS`).
    const ANSWER_VARIANTS: u8 = 4;

    /// Each variant's tag, by an exhaustive match (see `tree`'s).
    fn tag(a: &AppAnswer) -> u8 {
        match a {
            AppAnswer::Ran => 0,
            AppAnswer::Refused(_) => 1,
            AppAnswer::ClosedItsWindow => 2,
            AppAnswer::Failed(_) => 3,
        }
    }

    #[test]
    fn every_answer_s_encoding_is_pinned() {
        let cases: Vec<(AppAnswer, Vec<u8>)> = vec![
            (AppAnswer::Ran, vec![0]),
            (AppAnswer::Refused(Refusal::Hidden), vec![1, 2]),
            (AppAnswer::ClosedItsWindow, vec![2]),
            (AppAnswer::Failed(Capped::try_from("no").unwrap()), vec![3, 2, b'n', b'o']),
        ];
        let mut tags: Vec<u8> = cases.iter().map(|(a, _)| tag(a)).collect();
        for (a, bytes) in &cases {
            assert_eq!(&a.encode().unwrap(), bytes, "{a:?}");
            assert_eq!(bytes[0], tag(a));
            assert_eq!(AppAnswer::decode(bytes).as_ref(), Ok(a));
        }
        tags.sort_unstable();
        assert_eq!(tags, (0..ANSWER_VARIANTS).collect::<Vec<u8>>(), "every variant pinned once");
        let names: Vec<&str> = Refusal::ALL.iter().map(|r| r.name()).collect();
        assert_eq!(names, ["NoNode", "Disabled", "Hidden", "NotAllowed", "Arguments"]);
        for (i, r) in Refusal::ALL.into_iter().enumerate() {
            assert_eq!(AppAnswer::Refused(r).encode().unwrap(), [1, i as u8]);
        }
    }

    #[test]
    fn answers_are_what_serve_said_without_the_row() {
        let node = AutomationNode::button(NodeId::fresh(), "Add", Rect::new(0, 0, 1, 1), false, false);
        let row = Served::Row { depth: 1, node };
        assert_eq!(AppAnswer::from_served(&row), AppAnswer::Ran);
        assert_eq!(AppAnswer::from_served(&Served::RanNodeGone), AppAnswer::Ran);
        assert_eq!(AppAnswer::from_served(&Served::ClosedItsWindow), AppAnswer::ClosedItsWindow);
        let table = [
            (InvokeError::NoNode, Refusal::NoNode),
            (InvokeError::Disabled, Refusal::Disabled),
            (InvokeError::Hidden, Refusal::Hidden),
            (InvokeError::NotAllowed(Role::Label, ActionKind::Press), Refusal::NotAllowed),
            (InvokeError::Arguments, Refusal::Arguments),
            (InvokeError::NoSuchAction, Refusal::Arguments),
        ];
        for (e, r) in table {
            assert_eq!(AppAnswer::from_served(&Served::Error(e)), AppAnswer::Refused(r), "{e:?}");
        }
        // And back, with the role and action the runtime knows.
        assert_eq!(
            Refusal::NotAllowed.invoke_error(Role::Label, ActionKind::Press),
            InvokeError::NotAllowed(Role::Label, ActionKind::Press)
        );
        for (e, r) in table.into_iter().take(5) {
            assert_eq!(r.invoke_error(Role::Label, ActionKind::Press), e);
        }
        // A failure's text is cut to TEXT_BYTES on a char boundary.
        let long = "é".repeat(TEXT_BYTES);
        let AppAnswer::Failed(text) = AppAnswer::from_served(&Served::Failed(long)) else { panic!("not Failed") };
        assert_eq!(text.as_str(), "é".repeat(TEXT_BYTES / 2));
    }

    #[test]
    fn the_largest_answer_fits_answer_bytes_and_decoding_is_exact() {
        let largest = AppAnswer::Failed(Capped::try_from("é".repeat(TEXT_BYTES / 2).as_str()).unwrap());
        let bytes = largest.encode().unwrap();
        assert!(bytes.len() <= ANSWER_BYTES, "{}", bytes.len());
        // Past TEXT_BYTES: refused when built, and when decoded.
        assert!(Capped::<TEXT_BYTES>::try_from("x".repeat(TEXT_BYTES + 1).as_str()).is_err());
        let mut past = vec![3, 0x81, 0x08];
        past.extend(core::iter::repeat_n(b'x', TEXT_BYTES + 1));
        assert_eq!(AppAnswer::decode(&past), Err(DecodeError::Postcard(P::SerdeDeCustom)));
        // A remainder is refused.
        assert_eq!(AppAnswer::decode(&[0, 0]), Err(DecodeError::LeftOver(1)));
        assert_eq!(AppAnswer::decode(&[2, 1, 2, 3]), Err(DecodeError::LeftOver(3)));
        // Malformed values: a variant past the last, a refusal past its
        // table, bad UTF-8, an overflowing length.
        assert_eq!(AppAnswer::decode(&[ANSWER_VARIANTS]), Err(DecodeError::Postcard(P::SerdeDeCustom)));
        let refusals = Refusal::ALL.len() as u8;
        assert_eq!(AppAnswer::decode(&[1, refusals]), Err(DecodeError::Postcard(P::SerdeDeCustom)));
        assert_eq!(AppAnswer::decode(&[3, 1, 0xff]), Err(DecodeError::Postcard(P::DeserializeBadUtf8)));
        assert_eq!(
            AppAnswer::decode(&[3, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]),
            Err(DecodeError::Postcard(P::DeserializeBadVarint))
        );
        assert_eq!(AppAnswer::decode(&[]), Err(DecodeError::Postcard(P::DeserializeUnexpectedEnd)));
        // No bool, Option or char in an answer: those cases are the tree's
        // and Event's.
    }

    #[test]
    fn decoding_never_panics_on_truncations_flips_or_trailing_bytes() {
        for good in [vec![0], vec![1, 3], vec![2], vec![3, 3, b'a', 0xc3, 0xa9]] {
            for n in 0..good.len() {
                let _ = AppAnswer::decode(&good[..n]);
            }
            for i in 0..good.len() {
                for v in 0..=255u8 {
                    let mut b = good.clone();
                    b[i] = v;
                    let _ = AppAnswer::decode(&b);
                }
            }
            assert!(matches!(AppAnswer::decode(&[&good[..], &[0]].concat()), Err(DecodeError::LeftOver(1))));
        }
    }
}
