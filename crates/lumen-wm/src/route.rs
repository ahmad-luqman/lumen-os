//! From what the window manager reports to what a window's queue holds:
//! #153 calls [`deliver`] for each [`Delivery`] in an [`Outcome`], on the
//! queue of [`Delivery::window`].
//!
//! [`Outcome`]: crate::Outcome

use crate::queue::{EventQueue, Pushed, QueueEvent};
use crate::wm::{ButtonSet, Delivery};

/// A pointer delivery as a window's queue entry takes it (client
/// coordinates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerEntry {
    Down { x: i32, y: i32, button: ButtonSet, buttons: ButtonSet },
    Move { x: i32, y: i32, buttons: ButtonSet },
    Up { x: i32, y: i32, button: ButtonSet, buttons: ButtonSet },
}

/// Applies one delivery to its window's queue: a pointer event is pushed
/// (what the push did is returned), a focus change, a loss or a close
/// sets that flag (`None`).
pub fn deliver<E: QueueEvent + From<PointerEntry>>(q: &mut EventQueue<E>, d: &Delivery) -> Option<Pushed> {
    let mut push = |entry| Some(q.push(E::from(entry)));
    match *d {
        Delivery::PointerDown { x, y, button, buttons, .. } => push(PointerEntry::Down { x, y, button, buttons }),
        Delivery::PointerMove { x, y, buttons, .. } => push(PointerEntry::Move { x, y, buttons }),
        Delivery::PointerUp { x, y, button, buttons, .. } => push(PointerEntry::Up { x, y, button, buttons }),
        Delivery::FocusGained(_) => {
            q.set_focus(true);
            None
        }
        Delivery::FocusLost(_) => {
            q.set_focus(false);
            None
        }
        Delivery::Lost(_) => {
            q.set_lost();
            None
        }
        Delivery::Close(_) => {
            q.request_close();
            None
        }
    }
}
