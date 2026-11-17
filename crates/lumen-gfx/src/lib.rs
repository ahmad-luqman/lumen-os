//! The desktop's pixel core (#147), host-tested: one rectangle algebra,
//! one damage list and one surface type for the compositor, the fonts and
//! the toolkit.
//!
//! - **Rectangles** ([`rect`]): [`Rect`] has a signed origin and an
//!   unsigned size (a window may hang off the screen's top or left edge).
//!   Intersection, bounding box, containment, clipping to a surface, and
//!   subtraction into at most four disjoint pieces ([`Pieces`]).
//! - **Damage** ([`damage`]): [`Damage`] is a fixed-size list of the
//!   rectangles that changed. Adding one coalesces it with the rectangles
//!   it overlaps or touches when the merged box costs no more pixels than
//!   drawing both, and past the list's size everything collapses into one
//!   bounding box. Coverage is never lost: every pixel added stays inside
//!   some rectangle reported. It never allocates.
//! - **Surfaces** ([`surface`]): [`Surface`] is an owned xRGB8888 buffer
//!   (`0xXXRRGGBB`, one `u32` per pixel) with a stride. It can fill a
//!   rectangle, read and write a pixel, copy a rectangle to any [`Target`]
//!   (clipped on both sides: [`Surface::copy_to`], [`Surface::blit`]), and
//!   draw an 8-bit coverage [`Mask`] (an anti-aliased glyph) in one colour
//!   ([`Surface::blend_mask`], rounding as [`blend_channel`] states).
//! - **Targets** ([`Target`]): what pixels are presented to, a span of
//!   pixels at a time. A [`Surface`] is one; the kernel's front buffer
//!   implements it too, so presenting a back buffer's damage is one path
//!   ([`Surface::present`]).
//!
//! All arithmetic is integer: the kernel is built without SSE, so floating
//! point would be slow soft-float library calls.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod damage;
pub mod rect;
pub mod surface;

pub use damage::Damage;
pub use rect::{Pieces, Rect};
pub use surface::{Mask, Surface, Target, blend, blend_channel};
