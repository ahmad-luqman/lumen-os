//! Pointer input for the Lumen kernel (#98), key presses counted in PS/2
//! bytes (#149), and the desktop's input events in rings (#151),
//! host-tested:
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
//! - **Input devices as rings** ([`ring`], #151): a device keeps its
//!   newest events numbered in order ([`Ring`], [`Seq`]); a [`Reader`] asks
//!   for the first after the last it saw and counts a gap as lost
//!   ([`InputDevice`], [`Read`]). [`AfterOverrun`] (with the tablet)
//!   marks the frames that may follow frames lost at the device.
//! - **Desktop events** ([`keys`], #151): [`PointerEvent`], [`KeyEvent`]
//!   (press or release, with the modifiers after it, [`Mods`]) and
//!   [`InputEvent`]; [`KeyDecoder`] turns set 1 bytes into key events (one
//!   press per key, as [`Presses`] counts them) through `pc-keyboard`.

#![cfg_attr(not(test), no_std)]

pub mod cursor;
pub mod keys;
pub mod ring;
pub mod scancode;
pub mod tablet;

pub use cursor::{Cursor, Surface};
pub use keys::{AfterDrop, InputEvent, KeyAction, KeyCode, KeyDecoder, KeyEvent, Mods, PointerEvent};
pub use ring::{InputDevice, Read, Reader, Ring, Seq};
pub use scancode::Presses;
pub use tablet::{
    AbsAxis, AfterOverrun, AxisError, Buttons, Event, FRAME_TYPES, PointerState, Tablet, evdev, frame_bound,
    has_abs_xy, may_have_lost_frames,
};
