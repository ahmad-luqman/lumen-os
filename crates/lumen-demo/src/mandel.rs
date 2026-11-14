//! The Mandelbrot set in fixed point: numbers are `i64` with [`FRAC`]
//! fraction bits, so every machine draws the same pixels (pure integer
//! math; the kernel has no floating point).
//!
//! No step can overflow. A point with `|re| > 2` or `|im| > 2` has
//! `|c| > 2`, so it escapes at the first check after one step (`z₁ = c`):
//! [`escape`] answers that at once, without iterating. Every other point
//! has `|c| < 2√2`, and is stepped only while `|z|² <= 4`, so going into a
//! step `|re z|, |im z| <= 2` and coming out they are below `4 + 2√2 < 7`,
//! that is below 2^31 in units of 2^-28: their squares and product stay
//! below 2^62.

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
    /// `width` x `height` image and centred in it (on -0.75).
    pub fn fit(width: usize, height: usize, max_iter: u32) -> View {
        let pixels = |n: usize| i64::try_from(n.max(1)).unwrap_or(i64::MAX);
        let (width, height) = (pixels(width), pixels(height));
        // The region is 3.5 wide and 2.5 high: the larger step (rounded up)
        // fits both.
        let up = |span: i64, pixels: i64| span / pixels + i64::from(span % pixels != 0);
        let step = up(7 * ONE / 2, width).max(up(5 * ONE / 2, height));
        // Centre (-0.75, 0).
        let re0 = (-3 * ONE / 4).saturating_sub(step.saturating_mul(width / 2));
        let im0 = -step.saturating_mul(height / 2);
        View { re0, im0, step, max_iter }
    }

    /// The point at pixel (`x`, `y`) (saturating far outside the image).
    pub fn point(&self, x: usize, y: usize) -> (i64, i64) {
        let at = |origin: i64, n: usize| {
            origin.saturating_add(self.step.saturating_mul(i64::try_from(n).unwrap_or(i64::MAX)))
        };
        (at(self.re0, x), at(self.im0, y))
    }

    /// The iterations pixel (`x`, `y`) escapes after (see [`escape`]).
    pub fn escape_at(&self, x: usize, y: usize) -> u32 {
        let (re, im) = self.point(x, y);
        escape(re, im, self.max_iter)
    }
}

/// Iterate `z = z² + c` from `z₀ = 0` (`c` = `re` + `im`·i): the index `n`
/// of the first `zₙ` with `|zₙ|² > 4`, or `max_iter` if none of `z₀` to
/// `z₍max_iter-1₎` has (the point counts as inside the set).
pub fn escape(re: i64, im: i64, max_iter: u32) -> u32 {
    // |c| > 2: z₁ = c is out (see the module comment).
    let near = -2 * ONE..=2 * ONE;
    if !near.contains(&re) || !near.contains(&im) {
        return max_iter.min(1);
    }
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

/// The colours escaping points cycle through, by escape count: brown, dark
/// purple, deepening blues to near-white, then yellows and oranges back
/// towards brown.
const PALETTE: [u32; 16] = [
    0x421e0f, 0x19071a, 0x09012f, 0x040449, 0x000764, 0x0c2c8a, 0x1852b1, 0x397dd1, 0x86b5e5, 0xd3ecf8, 0xf1e9bf,
    0xf8c95f, 0xffaa00, 0xcc8000, 0x995700, 0x6a3403,
];

/// The colour (`0xRRGGBB`) of a point that escaped after `n` of `max_iter`
/// iterations: black inside the set.
pub fn color(n: u32, max_iter: u32) -> u32 {
    if n >= max_iter { 0 } else { PALETTE[n as usize % PALETTE.len()] }
}
