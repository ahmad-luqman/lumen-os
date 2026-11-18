//! A window's app ([`App`]: its tree, and what it does with what the tree
//! reports) and running a request on it ([`serve`]). The kernel's
//! `ui::Toolkit` drives both; host tests run the same code.

use alloc::string::String;

use lumen_gfx::{Rect, Surface};

use crate::node::{Action, AutomationNode, InvokeError};
use crate::text::Text;
use crate::ui::{Ui, UiEvent};

/// What a window does after an event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Then {
    /// Carry on (repaint if its tree changed).
    Carry,
    /// Close the window (its owner drops the handle).
    Close,
    /// Carry on, but what the event asked for failed, and why (#157: a
    /// launcher item whose window didn't open): a request that reached
    /// it answers [`Served::Failed`].
    Failed(String),
}

/// A window's app.
pub trait App {
    fn ui_mut(&mut self) -> &mut Ui;
    /// Act on what the tree reported.
    fn apply(&mut self, events: &[UiEvent]) -> Then;
    /// Drawn over the painted tree (an animation's frame); nothing by
    /// default.
    fn paint_over(&mut self, _surface: &mut Surface) {}
}

/// What running a request did: the answer its asker gets, once the owner
/// has published its tree (so `ui tree` shows the effect) or, for
/// [`Served::ClosedItsWindow`], once the window is gone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Served {
    /// Done: the node as it is after the action (client coordinates).
    Row { depth: u32, node: AutomationNode },
    /// Refused: it changed nothing.
    Error(InvokeError),
    /// Done, and the node is gone after it (it ran: a retry would act
    /// twice).
    RanNodeGone,
    /// Done, and the action closed its window (a Quit button).
    ClosedItsWindow,
    /// The node took the action, but what it was for failed, and why
    /// ([`Then::Failed`]).
    Failed(String),
}

/// Run `action` on node `node` of `app`'s tree, through the handler a
/// click reaches, then let the app act on what happened; the tree laid out
/// again in `area` for the node's row.
pub fn serve(app: &mut dyn App, text: &mut dyn Text, area: Rect, node: u64, action: &Action) -> Served {
    let events = match app.ui_mut().invoke(node, action) {
        Ok(events) => events,
        Err(e) => return Served::Error(e),
    };
    let then = app.apply(&events);
    app.ui_mut().layout(area, text);
    match then {
        Then::Close => return Served::ClosedItsWindow,
        Then::Failed(why) => return Served::Failed(why),
        Then::Carry => {}
    }
    match app.ui_mut().node(node) {
        Some((depth, row)) => Served::Row { depth, node: row },
        None => Served::RanNodeGone,
    }
}
