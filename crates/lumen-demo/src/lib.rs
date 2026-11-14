//! The logic behind the shell's demos, kept free of kernel dependencies so
//! it can be tested on the host:
//!
//! - [`mandel`]: the Mandelbrot set in fixed-point integer arithmetic (the
//!   kernel is built without SSE, so no floating point), and its colours.
//! - [`life`]: Conway's Game of Life on a bounded grid (cells beyond the
//!   edge are dead) and its seed patterns.

#![no_std]

extern crate alloc;

pub mod life;
pub mod mandel;

#[cfg(test)]
mod tests;
