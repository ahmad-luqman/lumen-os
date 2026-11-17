//! From what the window manager reports to what a window's queue holds:
//! #153 calls [`deliver`] for each [`Delivery`] in an [`Outcome`], on the
//! queue of [`Delivery::window`].
//!
//! [`Outcome`]: crate::Outcome

use crate::queue::{Entry, EventQueue, KeyLike, Pushed};
use crate::wm::{Button, ButtonSet, Delivery};

/// A pointer event as a window's queue holds it (client coordinates).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerEntry {
    /// `button` went down; `buttons` is everything held now (with it).
    Down {
        x: i32,
        y: i32,
        button: Button,
        buttons: ButtonSet,
    },
    Move {
        x: i32,
        y: i32,
        buttons: ButtonSet,
    },
    /// `button` came up; `buttons` is what is still held (without it).
    Up {
        x: i32,
        y: i32,
        button: Button,
        buttons: ButtonSet,
    },
}

/// Applies one delivery to its window's queue: a pointer event is pushed
/// (what the push did is returned), a focus change, a loss or a close
/// sets that flag (`None`).
#[must_use = "a dropped event is reported here (`Pushed::Dropped`), for the burst's WARN"]
pub fn deliver<K: KeyLike, R>(q: &mut EventQueue<K, R>, d: &Delivery) -> Option<Pushed> {
    let entry = match *d {
        Delivery::PointerDown { x, y, button, buttons, .. } => PointerEntry::Down { x, y, button, buttons },
        Delivery::PointerMove { x, y, buttons, .. } => PointerEntry::Move { x, y, buttons },
        Delivery::PointerUp { x, y, button, buttons, .. } => PointerEntry::Up { x, y, button, buttons },
        Delivery::FocusGained(_) => {
            q.set_focus(true);
            return None;
        }
        Delivery::FocusLost(_) => {
            q.set_focus(false);
            return None;
        }
        Delivery::Lost(_) => {
            q.set_lost();
            return None;
        }
        Delivery::Close(_) => {
            q.request_close();
            return None;
        }
    };
    Some(q.push(Entry::Pointer(entry)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::Next;
    use crate::wm::WindowId;

    /// No keys in these tests.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct NoKey;

    impl KeyLike for NoKey {
        type Code = ();
        fn code(&self) {}
        fn is_press(&self) -> bool {
            true
        }
    }

    type Q = EventQueue<NoKey, ()>;

    fn drain(q: &mut Q) -> alloc::vec::Vec<Next<Entry<NoKey, ()>>> {
        core::iter::from_fn(|| q.next_event()).collect()
    }

    #[test]
    fn every_delivery_lands_on_the_queue_exactly() {
        let w = WindowId::first();
        let (l, lr) = (Button::Left, ButtonSet::LEFT.with(ButtonSet::RIGHT));
        let mut q = Q::new();
        // Pointer events are pushed whole (coordinates and buttons kept).
        let down = Delivery::PointerDown { window: w, x: 3, y: -7, button: l, buttons: lr };
        let mv = Delivery::PointerMove { window: w, x: -2, y: 9, buttons: lr };
        let up = Delivery::PointerUp { window: w, x: 5, y: 6, button: l, buttons: ButtonSet::RIGHT };
        assert_eq!(deliver(&mut q, &down), Some(Pushed::Queued));
        assert_eq!(deliver(&mut q, &mv), Some(Pushed::Queued));
        assert_eq!(deliver(&mut q, &up), Some(Pushed::Queued));
        assert_eq!(
            drain(&mut q),
            [
                Next::Event(Entry::Pointer(PointerEntry::Down { x: 3, y: -7, button: l, buttons: lr })),
                Next::Event(Entry::Pointer(PointerEntry::Move { x: -2, y: 9, buttons: lr })),
                Next::Event(Entry::Pointer(PointerEntry::Up { x: 5, y: 6, button: l, buttons: ButtonSet::RIGHT })),
            ]
        );
        // Each of the rest sets its own flag and pushes nothing.
        for (d, want) in [
            (Delivery::FocusGained(w), Next::Focus(true)),
            (Delivery::FocusLost(w), Next::Focus(false)),
            (Delivery::Lost(w), Next::Lost),
            (Delivery::Close(w), Next::Close),
        ] {
            assert_eq!(deliver(&mut q, &d), None, "{d:?}");
            assert!(q.is_empty(), "{d:?} pushed an entry");
            assert_eq!(drain(&mut q), [want], "{d:?}");
        }
        // What a push did comes back.
        q.kill();
        assert_eq!(deliver(&mut q, &mv), Some(Pushed::Dead));
    }

    #[test]
    fn a_delivery_names_its_window() {
        let w = WindowId::first();
        for d in [
            Delivery::PointerDown { window: w, x: 0, y: 0, button: Button::Left, buttons: ButtonSet::LEFT },
            Delivery::PointerMove { window: w, x: 0, y: 0, buttons: ButtonSet::NONE },
            Delivery::PointerUp { window: w, x: 0, y: 0, button: Button::Left, buttons: ButtonSet::NONE },
            Delivery::FocusGained(w),
            Delivery::FocusLost(w),
            Delivery::Lost(w),
            Delivery::Close(w),
        ] {
            assert_eq!(d.window(), w, "{d:?}");
        }
    }
}
