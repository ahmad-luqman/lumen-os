//! Pointer input for the Lumen kernel (#98), host-tested:
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

#![cfg_attr(not(test), no_std)]

pub mod cursor;
pub mod tablet;

pub use cursor::{Cursor, Surface};
pub use tablet::{
    AbsAxis, AxisError, Buttons, Event, MAX_FRAME, PointerState, Tablet, evdev, has_abs_xy, may_have_lost_frames,
};
