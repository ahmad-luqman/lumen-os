//! Pointer input for the Lumen kernel (#98), and key presses counted in
//! PS/2 bytes (#149), host-tested:
//!
//! - **Events into pointer states** ([`tablet`]): virtio-input delivers
//!   Linux evdev events ([`Event`]: a type, a code and a value); a
//!   [`Tablet`] turns an absolute pointer's into [`PointerState`]s, one per
//!   `SYN_REPORT` (a frame), each axis scaled to the screen
//!   ([`AbsAxis::scale`]).
//! - **The pointer on the screen** ([`cursor`]): an arrow sprite and a
//!   save-under ([`Cursor`]) that keeps the pixels below the arrow and puts
//!   them back when it moves or hides, over any [`Surface`] (the kernel's
//!   framebuffer, a test's array). Whoever draws there takes the arrow off
//!   first ([`Cursor::overlaps`], [`Cursor::hide`]); the kernel's terminal
//!   does (`kernel/src/term.rs`).
//! - **Key presses in PS/2 bytes** ([`scancode`]): [`Presses`] counts the
//!   keys pressed in a stream of scancode set 1 bytes (prefixes, fake
//!   shifts and Pause each taken as one key or none), for `kbd`'s
//!   `presses` (#149).

#![cfg_attr(not(test), no_std)]

pub mod cursor;
pub mod scancode;
pub mod tablet;

pub use cursor::{Cursor, Surface};
pub use scancode::Presses;
pub use tablet::{
    AbsAxis, AxisError, Buttons, Event, FRAME_TYPES, PointerState, Tablet, evdev, frame_bound, has_abs_xy,
    may_have_lost_frames,
};
