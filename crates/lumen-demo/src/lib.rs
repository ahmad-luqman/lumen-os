//! The logic behind the shell's demos, kept free of kernel dependencies so
//! it can be tested on the host:
//!
//! - [`mandel`]: the Mandelbrot set in fixed-point integer arithmetic (the
//!   kernel is built without SSE, so no floating point), and its colours.
//! - [`bands`]: how an image's rows are dealt out to the CPUs drawing it.
//! - [`life`]: Conway's Game of Life on a bounded grid (cells beyond the
//!   edge are dead) and its seed patterns.

#![no_std]

extern crate alloc;

pub mod life;
pub mod mandel;

/// Dealing an image's rows out in bands: band `i` (rows `i * band` to
/// `(i + 1) * band`, the last one cut short at the height) goes to slot
/// `i % slots`. Interleaving spreads costly rows (the middle of the set)
/// over every slot.
pub mod bands {
    use core::num::NonZeroUsize;
    use core::ops::Range;

    /// The rows slot `slot` of `slots` draws in a `height`-row image cut
    /// into bands of `band` rows, band by band.
    pub fn rows(
        slot: usize,
        slots: NonZeroUsize,
        height: usize,
        band: NonZeroUsize,
    ) -> impl Iterator<Item = Range<usize>> {
        let band = band.get();
        (slot..height.div_ceil(band)).step_by(slots.get()).map(move |i| i * band..((i + 1) * band).min(height))
    }
}

#[cfg(test)]
mod tests;
