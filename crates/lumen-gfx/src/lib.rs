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
//!   draw an 8-bit coverage [`Mask`] (an anti-aliased glyph) in one colour,
//!   optionally clipped to a rectangle such as one damaged area
//!   ([`Surface::blend_mask`], [`Surface::blend_mask_in`]; rounding as
//!   [`blend_channel`] states).
//! - **Targets** ([`Target`]): what pixels are presented to, a span of
//!   xRGB8888 pixels at a time. A [`Surface`] is one; the kernel's front
//!   buffer is meant to implement it too (the compositor, #153), so
//!   presenting a back buffer's damage is one path ([`Surface::present`]).
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

/// Deterministic randomness for the randomized tests, without a
/// dependency.
#[cfg(test)]
pub(crate) mod test_rng {
    /// xorshift64.
    pub struct Rng(pub u64);

    impl Rng {
        pub fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        /// A number in `lo..hi`.
        pub fn range(&mut self, lo: i32, hi: i32) -> i32 {
            lo + (self.next() % (hi - lo) as u64) as i32
        }
    }
}
