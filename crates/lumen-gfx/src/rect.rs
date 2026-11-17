//! Rectangles: a signed origin and an unsigned size.
//!
//! A [`Rect`] covers the pixels `x..x + w` by `y..y + h` (its right and
//! bottom edges are exclusive). A rectangle with no width or no height is
//! empty: it covers no pixel, whatever its origin, and every operation
//! treats all empty rectangles alike. `==` compares the fields, so two
//! empty rectangles at different origins are unequal: [`Rect::is_empty`]
//! is the test for "no pixels".
//!
//! [`Rect::new`] shortens a size that would carry an edge past `i32::MAX`,
//! so every edge fits in an `i32` and the bounding box of any two
//! rectangles has a size that fits in a `u32`.

use core::ops::Deref;

/// The pixels `x..x + w` by `y..y + h`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct Rect {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

/// The largest size along one axis that keeps `origin + size` within `i32`.
const fn room(origin: i32) -> u32 {
    // i32::MAX - i32::MIN is u32::MAX, so this never overflows a u32.
    (i32::MAX as i64 - origin as i64) as u32
}

impl Rect {
    /// The empty rectangle at the origin.
    pub const EMPTY: Rect = Rect { x: 0, y: 0, w: 0, h: 0 };

    /// The `w` x `h` rectangle at (`x`, `y`), its size shortened where an
    /// edge would pass `i32::MAX`.
    pub const fn new(x: i32, y: i32, w: u32, h: u32) -> Rect {
        let rw = room(x);
        let rh = room(y);
        Rect { x, y, w: if w > rw { rw } else { w }, h: if h > rh { rh } else { h } }
    }

    /// The rectangle from (`x0`, `y0`) up to, not including, (`x1`, `y1`),
    /// or `None` if that's empty. The edges are within `i32` (callers
    /// derive them from rectangles).
    fn from_edges(x0: i64, y0: i64, x1: i64, y1: i64) -> Option<Rect> {
        debug_assert!(x0 >= i32::MIN as i64 && x1 <= i32::MAX as i64);
        debug_assert!(y0 >= i32::MIN as i64 && y1 <= i32::MAX as i64);
        (x1 > x0 && y1 > y0).then(|| Rect { x: x0 as i32, y: y0 as i32, w: (x1 - x0) as u32, h: (y1 - y0) as u32 })
    }

    /// The left edge.
    pub const fn x(&self) -> i32 {
        self.x
    }

    /// The top edge.
    pub const fn y(&self) -> i32 {
        self.y
    }

    /// The width.
    pub const fn w(&self) -> u32 {
        self.w
    }

    /// The height.
    pub const fn h(&self) -> u32 {
        self.h
    }

    /// The right edge, exclusive.
    pub const fn right(&self) -> i32 {
        // Within i32: `new` keeps x + w <= i32::MAX.
        (self.x as i64 + self.w as i64) as i32
    }

    /// The bottom edge, exclusive.
    pub const fn bottom(&self) -> i32 {
        (self.y as i64 + self.h as i64) as i32
    }

    /// Whether it covers no pixel.
    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// How many pixels it covers.
    pub const fn area(&self) -> u64 {
        self.w as u64 * self.h as u64
    }

    /// Whether it covers the pixel at (`x`, `y`).
    pub const fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }

    /// Whether every pixel of `other` is in this one (always, for an empty
    /// `other`).
    pub const fn contains_rect(&self, other: &Rect) -> bool {
        other.is_empty()
            || (!self.is_empty()
                && other.x >= self.x
                && other.y >= self.y
                && other.right() <= self.right()
                && other.bottom() <= self.bottom())
    }

    /// The pixels moved by (`dx`, `dy`), each edge clamped to `i32`'s
    /// range, so moving never overflows (client to screen coordinates,
    /// say) and never covers a pixel the moved rectangle wouldn't: a part
    /// pushed past either end is cut off. [`Rect::EMPTY`] if nothing is
    /// left, or the rectangle was empty.
    #[must_use]
    pub fn translate(&self, dx: i32, dy: i32) -> Rect {
        let clamp = |v: i64| v.clamp(i32::MIN as i64, i32::MAX as i64);
        Rect::from_edges(
            clamp(self.x as i64 + dx as i64),
            clamp(self.y as i64 + dy as i64),
            clamp(self.right() as i64 + dx as i64),
            clamp(self.bottom() as i64 + dy as i64),
        )
        .unwrap_or(Rect::EMPTY)
    }

    /// The pixels in both, or `None` if there are none.
    #[must_use]
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        if self.is_empty() || other.is_empty() {
            return None;
        }
        Rect::from_edges(
            self.x.max(other.x) as i64,
            self.y.max(other.y) as i64,
            self.right().min(other.right()) as i64,
            self.bottom().min(other.bottom()) as i64,
        )
    }

    /// The smallest rectangle covering both (the other one, if one is
    /// empty).
    #[must_use]
    pub fn union(&self, other: &Rect) -> Rect {
        if other.is_empty() {
            return *self;
        }
        if self.is_empty() {
            return *other;
        }
        // Edges from two valid rectangles: the size fits a u32.
        Rect::from_edges(
            self.x.min(other.x) as i64,
            self.y.min(other.y) as i64,
            self.right().max(other.right()) as i64,
            self.bottom().max(other.bottom()) as i64,
        )
        .expect("two non-empty rectangles have a non-empty bounding box")
    }

    /// The part inside a `w` x `h` surface at the origin, or `None`.
    #[must_use]
    pub fn clip(&self, w: u32, h: u32) -> Option<Rect> {
        self.intersect(&Rect::new(0, 0, w, h))
    }

    /// The pixels in this rectangle and not in `other`, as at most four
    /// disjoint, non-empty rectangles: the band above `other`, the band
    /// below it, and the parts left and right of it in between.
    #[must_use]
    pub fn subtract(&self, other: &Rect) -> Pieces {
        let mut out = Pieces { rects: [Rect::EMPTY; 4], len: 0 };
        let Some(hole) = self.intersect(other) else {
            if !self.is_empty() {
                out.push(*self);
            }
            return out;
        };
        let (x0, y0, x1, y1) = (self.x as i64, self.y as i64, self.right() as i64, self.bottom() as i64);
        let (hx0, hy0, hx1, hy1) = (hole.x as i64, hole.y as i64, hole.right() as i64, hole.bottom() as i64);
        for piece in [
            Rect::from_edges(x0, y0, x1, hy0),
            Rect::from_edges(x0, hy1, x1, y1),
            Rect::from_edges(x0, hy0, hx0, hy1),
            Rect::from_edges(hx1, hy0, x1, hy1),
        ]
        .into_iter()
        .flatten()
        {
            out.push(piece);
        }
        out
    }
}

/// Up to four rectangles: what [`Rect::subtract`] leaves. Derefs to a
/// slice of the ones there are.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pieces {
    rects: [Rect; 4],
    len: usize,
}

impl Pieces {
    fn push(&mut self, r: Rect) {
        self.rects[self.len] = r;
        self.len += 1;
    }
}

impl Deref for Pieces {
    type Target = [Rect];
    fn deref(&self) -> &[Rect] {
        &self.rects[..self.len]
    }
}

impl<'a> IntoIterator for &'a Pieces {
    type Item = &'a Rect;
    type IntoIter = core::slice::Iter<'a, Rect>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_rng::Rng;

    /// A rectangle in a small grid around the origin, sometimes empty.
    fn random_rect(rng: &mut Rng) -> Rect {
        Rect::new(rng.range(-8, 12), rng.range(-8, 12), rng.range(0, 10) as u32, rng.range(0, 10) as u32)
    }

    /// The grid the brute-force checks look at: wider than any rect `Rng`
    /// makes.
    fn grid() -> impl Iterator<Item = (i32, i32)> {
        (-10..25).flat_map(|y| (-10..25).map(move |x| (x, y)))
    }

    #[test]
    fn accessors_and_edges() {
        let r = Rect::new(-3, 4, 10, 2);
        assert_eq!((r.x(), r.y(), r.w(), r.h()), (-3, 4, 10, 2));
        assert_eq!((r.right(), r.bottom()), (7, 6));
        assert_eq!(r.area(), 20);
        assert!(!r.is_empty());
        assert!(Rect::new(5, 5, 0, 3).is_empty());
        assert!(Rect::new(5, 5, 3, 0).is_empty());
        assert!(Rect::EMPTY.is_empty());
        assert_eq!(Rect::default(), Rect::EMPTY);
    }

    #[test]
    fn new_keeps_edges_within_i32() {
        let r = Rect::new(i32::MAX - 5, i32::MAX, 100, 7);
        assert_eq!((r.w(), r.h()), (5, 0));
        assert_eq!(r.right(), i32::MAX);
        let whole = Rect::new(i32::MIN, i32::MIN, u32::MAX, u32::MAX);
        assert_eq!((whole.w(), whole.h()), (u32::MAX, u32::MAX));
        assert_eq!((whole.right(), whole.bottom()), (i32::MAX, i32::MAX));
        assert_eq!(whole.area(), u32::MAX as u64 * u32::MAX as u64);
        // The bounding box of the two corners of the plane still fits.
        let a = Rect::new(i32::MIN, i32::MIN, 1, 1);
        let b = Rect::new(i32::MAX - 1, i32::MAX - 1, 1, 1);
        assert_eq!(a.union(&b), whole);
    }

    #[test]
    fn translate_moves_and_clamps() {
        assert_eq!(Rect::new(1, 2, 3, 4).translate(-5, 10), Rect::new(-4, 12, 3, 4));
        assert_eq!(Rect::new(1, 2, 3, 4).translate(0, 0), Rect::new(1, 2, 3, 4));
        // Past i32::MAX the far edge stops there: the size shrinks.
        let r = Rect::new(i32::MAX - 10, 0, 8, 1).translate(5, 0);
        assert_eq!((r.x(), r.w(), r.right()), (i32::MAX - 5, 5, i32::MAX));
        let r = Rect::new(0, i32::MAX - 10, 4, 8).translate(0, 5);
        assert_eq!((r.y(), r.h(), r.bottom(), r.w()), (i32::MAX - 5, 5, i32::MAX, 4));
        // Past i32::MIN the near edge stops there: the size shrinks too,
        // so the result never covers a pixel the moved rect wouldn't.
        let r = Rect::new(i32::MIN + 2, 0, 8, 1).translate(-5, 0);
        assert_eq!((r.x(), r.w(), r.right()), (i32::MIN, 5, i32::MIN + 5));
        let r = Rect::new(0, i32::MIN + 1, 3, 4).translate(0, -3);
        assert_eq!((r.y(), r.h(), r.bottom(), r.w()), (i32::MIN, 2, i32::MIN + 2, 3));
        // Pushed entirely past either end, or empty: nothing.
        assert_eq!(Rect::new(0, 0, 8, 8).translate(i32::MAX, 0), Rect::EMPTY);
        assert_eq!(Rect::new(0, 0, 8, 8).translate(0, i32::MAX), Rect::EMPTY);
        assert_eq!(Rect::new(i32::MIN + 2, -3, 8, 1).translate(-10, 0), Rect::EMPTY);
        assert_eq!(Rect::new(-3, -3, 1, 1).translate(0, i32::MIN), Rect::EMPTY);
        assert_eq!(Rect::new(5, 5, 0, 3).translate(1, 1), Rect::EMPTY);
        // The whole plane moved stays clipped to it.
        let whole = Rect::new(i32::MIN, i32::MIN, u32::MAX, u32::MAX);
        assert_eq!(whole.translate(10, -10), Rect::new(i32::MIN + 10, i32::MIN, u32::MAX - 10, u32::MAX - 10));
    }

    #[test]
    fn contains_points_with_exclusive_far_edges() {
        let r = Rect::new(-2, -2, 4, 3);
        assert!(r.contains(-2, -2));
        assert!(r.contains(1, 0));
        assert!(!r.contains(2, 0));
        assert!(!r.contains(1, 1));
        assert!(!r.contains(-3, -2));
        assert!(!Rect::new(0, 0, 0, 5).contains(0, 0));
    }

    #[test]
    fn contains_rect_cases() {
        let r = Rect::new(-5, -5, 10, 10);
        assert!(r.contains_rect(&r));
        assert!(r.contains_rect(&Rect::new(-5, -5, 1, 1)));
        assert!(r.contains_rect(&Rect::new(4, 4, 1, 1)));
        assert!(!r.contains_rect(&Rect::new(4, 4, 2, 1)));
        assert!(!r.contains_rect(&Rect::new(-6, 0, 1, 1)));
        assert!(r.contains_rect(&Rect::new(100, 100, 0, 0)));
        assert!(Rect::EMPTY.contains_rect(&Rect::EMPTY));
        assert!(!Rect::EMPTY.contains_rect(&Rect::new(0, 0, 1, 1)));
    }

    #[test]
    fn intersect_cases() {
        let a = Rect::new(-4, -4, 8, 8);
        assert_eq!(a.intersect(&Rect::new(0, 0, 10, 10)), Some(Rect::new(0, 0, 4, 4)));
        assert_eq!(a.intersect(&Rect::new(-10, -1, 7, 2)), Some(Rect::new(-4, -1, 1, 2)));
        assert_eq!(a.intersect(&Rect::new(-2, -2, 1, 1)), Some(Rect::new(-2, -2, 1, 1)));
        // Touching edges share no pixel.
        assert_eq!(a.intersect(&Rect::new(4, -4, 3, 8)), None);
        assert_eq!(a.intersect(&Rect::new(-4, -6, 8, 2)), None);
        assert_eq!(a.intersect(&Rect::new(0, 0, 0, 4)), None);
        assert_eq!(Rect::new(0, 0, 0, 4).intersect(&a), None);
    }

    #[test]
    fn union_cases() {
        let a = Rect::new(-4, 2, 2, 2);
        let b = Rect::new(3, -1, 1, 1);
        assert_eq!(a.union(&b), Rect::new(-4, -1, 8, 5));
        assert_eq!(b.union(&a), Rect::new(-4, -1, 8, 5));
        assert_eq!(a.union(&Rect::new(100, 100, 0, 9)), a);
        assert_eq!(Rect::new(100, 100, 9, 0).union(&a), a);
        assert!(Rect::EMPTY.union(&Rect::EMPTY).is_empty());
    }

    #[test]
    fn clip_to_a_surface() {
        assert_eq!(Rect::new(-5, -5, 10, 10).clip(20, 20), Some(Rect::new(0, 0, 5, 5)));
        assert_eq!(Rect::new(15, 18, 10, 10).clip(20, 20), Some(Rect::new(15, 18, 5, 2)));
        assert_eq!(Rect::new(-5, 3, 30, 1).clip(20, 20), Some(Rect::new(0, 3, 20, 1)));
        assert_eq!(Rect::new(20, 0, 5, 5).clip(20, 20), None);
        assert_eq!(Rect::new(-5, 0, 5, 5).clip(20, 20), None);
        assert_eq!(Rect::new(0, 0, 5, 5).clip(0, 20), None);
    }

    #[test]
    fn subtract_shapes() {
        let r = Rect::new(0, 0, 10, 10);
        // A hole in the middle: four pieces.
        let p = r.subtract(&Rect::new(3, 3, 4, 4));
        assert_eq!(
            &*p,
            &[Rect::new(0, 0, 10, 3), Rect::new(0, 7, 10, 3), Rect::new(0, 3, 3, 4), Rect::new(7, 3, 3, 4)]
        );
        // Covered entirely: nothing.
        assert!(r.subtract(&Rect::new(-1, -1, 12, 12)).is_empty());
        assert!(r.subtract(&r).is_empty());
        // Disjoint or empty: itself.
        assert_eq!(&*r.subtract(&Rect::new(10, 0, 5, 5)), &[r]);
        assert_eq!(&*r.subtract(&Rect::EMPTY), &[r]);
        // From an empty rectangle: nothing.
        assert!(Rect::EMPTY.subtract(&r).is_empty());
        // A bite off one edge: one piece.
        assert_eq!(&*r.subtract(&Rect::new(-5, -5, 20, 7)), &[Rect::new(0, 2, 10, 8)]);
        assert_eq!(r.subtract(&Rect::new(5, -5, 20, 20)).iter().copied().collect::<Vec<_>>(), [Rect::new(0, 0, 5, 10)]);
    }

    /// Against a pixel-by-pixel model on random pairs (negative origins
    /// and empty ones included): intersect, union, contains_rect and
    /// subtract say exactly what the pixels say.
    #[test]
    fn randomized_against_pixels() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for _ in 0..3000 {
            let (a, b) = (random_rect(&mut rng), random_rect(&mut rng));
            let i = a.intersect(&b);
            let u = a.union(&b);
            let pieces = a.subtract(&b);
            assert!(pieces.len() <= 4);
            assert!(pieces.iter().all(|p| !p.is_empty() && a.contains_rect(p)));
            let mut a_in_b = true;
            for (x, y) in grid() {
                let (in_a, in_b) = (a.contains(x, y), b.contains(x, y));
                assert_eq!(i.is_some_and(|i| i.contains(x, y)), in_a && in_b, "{a:?} & {b:?} at {x},{y}");
                assert!(!(in_a || in_b) || u.contains(x, y), "{a:?} | {b:?} misses {x},{y}");
                let hits = pieces.iter().filter(|p| p.contains(x, y)).count();
                assert_eq!(hits, usize::from(in_a && !in_b), "{a:?} - {b:?} at {x},{y}: {pieces:?}");
                a_in_b &= !in_a || in_b;
            }
            assert_eq!(b.contains_rect(&a), a_in_b, "{b:?} contains {a:?}");
            // The union is the tightest box: each of its edges touches a or b.
            if !u.is_empty() {
                let non_empty = [a, b].into_iter().filter(|r| !r.is_empty());
                assert_eq!(u.x(), non_empty.clone().map(|r| r.x()).min().unwrap());
                assert_eq!(u.y(), non_empty.clone().map(|r| r.y()).min().unwrap());
                assert_eq!(u.right(), non_empty.clone().map(|r| r.right()).max().unwrap());
                assert_eq!(u.bottom(), non_empty.map(|r| r.bottom()).max().unwrap());
            } else {
                assert!(a.is_empty() && b.is_empty());
            }
        }
    }
}
