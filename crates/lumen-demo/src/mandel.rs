//! The Mandelbrot set in fixed point: numbers are `i64` with [`FRAC`]
//! fraction bits. Every value stays far from overflow: a point is iterated
//! only while `|z|² <= 4`, so `|re|, |im| <= 2` going into a step and below
//! 2^31 (in units of 2^-28) coming out, whose squares fit in an `i64`.
//! Pure integer math, so every machine draws the same pixels.

/// Fraction bits.
pub const FRAC: u32 = 28;
/// 1.0.
pub const ONE: i64 = 1 << FRAC;

/// The part of the complex plane an image shows: the point at its top-left
/// pixel and the distance between pixels (the same both ways).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    pub re0: i64,
    pub im0: i64,
    pub step: i64,
    /// Iterations before a point counts as inside the set.
    pub max_iter: u32,
}

impl View {
    /// The whole set (re -2.5 to 1, im -1.25 to 1.25), as large as fits a
    /// `width` x `height` image and centred in it.
    pub fn fit(width: usize, height: usize, max_iter: u32) -> View {
        let (width, height) = (width.max(1) as i64, height.max(1) as i64);
        // The region is 3.5 wide and 2.5 high: the larger step (rounded up)
        // fits both.
        let up = |span: i64, pixels: i64| (span + pixels - 1) / pixels;
        let step = up(7 * ONE / 2, width).max(up(5 * ONE / 2, height));
        // Centre (-0.75, 0).
        let re0 = -3 * ONE / 4 - step * width / 2;
        let im0 = -step * height / 2;
        View { re0, im0, step, max_iter }
    }

    /// The point at pixel (`x`, `y`).
    pub fn point(&self, x: usize, y: usize) -> (i64, i64) {
        (self.re0 + self.step * x as i64, self.im0 + self.step * y as i64)
    }

    /// The iterations pixel (`x`, `y`) escapes after (see [`escape`]).
    pub fn escape_at(&self, x: usize, y: usize) -> u32 {
        let (re, im) = self.point(x, y);
        escape(re, im, self.max_iter)
    }
}

/// How many iterations of `z = z² + c` from 0 (`c` = `re` + `im`·i) leave
/// `|z|² <= 4`: the first `n` whose `z` has `|z|² > 4`, or `max_iter` if none
/// does in that many (the point counts as inside the set).
pub fn escape(re: i64, im: i64, max_iter: u32) -> u32 {
    let (mut zr, mut zi) = (0i64, 0i64);
    for n in 0..max_iter {
        let (rr, ii) = ((zr * zr) >> FRAC, (zi * zi) >> FRAC);
        if rr + ii > 4 * ONE {
            return n;
        }
        zi = (((zr * zi) >> FRAC) << 1) + im;
        zr = rr - ii + re;
    }
    max_iter
}

/// Colours escaping points cycle through (dark blue through white and
/// orange to brown).
const PALETTE: [u32; 16] = [
    0x421e0f, 0x19071a, 0x09012f, 0x040449, 0x000764, 0x0c2c8a, 0x1852b1, 0x397dd1, 0x86b5e5, 0xd3ecf8, 0xf1e9bf,
    0xf8c95f, 0xffaa00, 0xcc8000, 0x995700, 0x6a3403,
];

/// The colour (`0xRRGGBB`) of a point that escaped after `n` of `max_iter`
/// iterations: black inside the set.
pub fn color(n: u32, max_iter: u32) -> u32 {
    if n >= max_iter { 0 } else { PALETTE[n as usize % PALETTE.len()] }
}
