//! The desktop's window-manager logic (#152), host-tested, with no pixels
//! and no locks: #153's compositor holds a [`Wm`] behind its lock, feeds
//! it pointer events and draws what it reports.
//!
//! - **Geometry** ([`geom`]): a window's frame, title bar, close box,
//!   client area and borders from [`Metrics`], and which [`Part`] a point
//!   is in.
//! - **Stacking, focus and the pointer** ([`wm`]): [`Wm`] keeps the windows
//!   back to front and which one is focused, and turns pointer events into
//!   a drag (a press on a title bar), the implicit grab (a press on a
//!   client area: that button's moves and release go to that window, in
//!   client coordinates), a close-box click, or nothing (a press on the
//!   desktop or a border). A click focuses and raises a window; closing
//!   the focused one focuses the one in front of the rest. Every operation
//!   returns the damage it caused and what to tell windows ([`Outcome`]).
//!   A pause or a gap in pointer input ([`Interruption`]) tells the focused
//!   window and a grabbing one, and ends the gesture, so no window is left
//!   holding a button. Moves with no button held aren't delivered (no
//!   hover in M4).
//! - **A window's event queue** ([`queue`]): [`EventQueue`], with `Lost`,
//!   focus and `Close` as flags beside it that are never dropped, presses,
//!   moves and requests rationed past [`queue::RATIONED`] entries, and a
//!   release queued only after its press was.
//! - **Themes** ([`theme`]): the light and dark palettes.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod geom;
pub mod queue;
pub mod theme;
pub mod wm;

#[cfg(test)]
mod model;

pub use geom::{Metrics, Part};
pub use queue::{Class, EventQueue, Next, Pushed, QueueCounts, QueueEvent};
pub use theme::Theme;
pub use wm::{ButtonSet, Delivery, Hit, Interruption, Outcome, Pointer, Window, WindowId, Wm, WmCounts, WmDamage};
