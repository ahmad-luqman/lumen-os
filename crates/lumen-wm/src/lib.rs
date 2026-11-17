//! The desktop's window-manager logic (#152), host-tested, with no pixels
//! and no locks: #153's compositor holds a [`Wm`] behind its lock, feeds
//! it pointer events and draws what it reports.
//!
//! - **Geometry** ([`geom`]): a window's frame, title bar, close box,
//!   client area and borders from [`Metrics`], and which [`Part`] a point
//!   is in.
//! - **Stacking, focus and the pointer** ([`wm`]): [`Wm`] keeps the windows
//!   back to front and which one is focused, and turns pointer events into
//!   a drag (a left press on a title bar, until the left release), the
//!   implicit grab (a press on a client area: the moves, other buttons and
//!   releases go to that window, in client coordinates, until the last
//!   button is up or it loses the focus), a close-box click (a left press
//!   and release on it), or nothing (a press on the desktop or a border).
//!   A click focuses and raises a window; closing the focused one focuses
//!   the one in front of the rest. Every operation returns the damage it
//!   caused and what to tell windows ([`Outcome`]). A pause or a gap in
//!   pointer input, or a gap in key input ([`Interruption`]), tells the
//!   focused window and ends the grab, so no window is left holding a
//!   button or key; while paused, focus changes wait for the resume. Moves with no button held aren't
//!   delivered (no hover in M4).
//! - **A window's event queue** ([`queue`]): [`EventQueue`] of [`Entry`]s
//!   (key events, pointer events, requests), with `Lost`, focus and
//!   `Close` as flags beside it that are never dropped and can't be
//!   pushed, presses, moves and requests rationed past
//!   [`queue::RATIONED`] entries, and a release queued only after its press
//!   was; [`deliver`] puts a [`Delivery`] on it.
//! - **Themes** ([`theme`]): the light and dark palettes.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod geom;
pub mod queue;
pub mod route;
pub mod theme;
pub mod wm;

#[cfg(test)]
mod model;

pub use geom::{Metrics, Part};
pub use queue::{Entry, EventQueue, Held, KeyLike, Next, Pushed, QueueCounts, Refused};
pub use route::{PointerEntry, deliver};
pub use theme::Theme;
pub use wm::{Delivery, Hit, Interruption, Outcome, Pointer, Window, WindowId, Wm, WmCounts, WmDamage};
