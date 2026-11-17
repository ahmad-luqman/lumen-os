//! Damage: which rectangles of a surface changed since it was last
//! presented.
//!
//! A [`Damage`] holds at most `N` rectangles in a fixed array, so adding
//! one never allocates (the compositor adds damage under its lock, where
//! nothing may allocate). Adding a rectangle:
//!
//! 1. ignores it if it's empty;
//! 2. merges it with each rectangle in the list whose bounding box with
//!    it costs no more pixels than the two cost drawn apart
//!    (`area(a ∪ b) <= area(a) + area(b)`), repeating while the grown
//!    rectangle finds more to merge with. That holds when one contains
//!    the other, when they share a whole edge, or when they overlap by at
//!    least as many pixels as the box adds; two rectangles with a gap
//!    between them never qualify;
//! 3. appends what is left, or, when the list is full, replaces the whole
//!    list with one bounding box of everything in it plus the new one.
//!
//! None of these steps drops a pixel: every pixel of every rectangle added
//! since the last [`Damage::clear`] stays inside some rectangle that
//! [`Damage::rects`] reports, and nothing reported reaches outside the
//! bounding box of what was added.

use crate::rect::Rect;

/// At most `N` (at least 1) rectangles covering every pixel added.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Damage<const N: usize> {
    rects: [Rect; N],
    len: usize,
}

impl<const N: usize> Default for Damage<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// `a` and `b` as one rectangle, if that box covers no more pixels than
/// the two drawn apart.
fn merged(a: &Rect, b: &Rect) -> Option<Rect> {
    let u = a.union(b);
    (u.area() as u128 <= a.area() as u128 + b.area() as u128).then_some(u)
}

impl<const N: usize> Damage<N> {
    /// No damage.
    pub const fn new() -> Self {
        const { assert!(N >= 1, "a damage list holds at least one rectangle") };
        Damage { rects: [Rect::EMPTY; N], len: 0 }
    }

    /// Note that the pixels of `r` changed.
    pub fn add(&mut self, r: Rect) {
        if r.is_empty() {
            return;
        }
        let mut r = r;
        loop {
            let before = self.len;
            let mut i = 0;
            while i < self.len {
                if let Some(m) = merged(&self.rects[i], &r) {
                    r = m;
                    self.len -= 1;
                    self.rects[i] = self.rects[self.len];
                } else {
                    i += 1;
                }
            }
            // A merge grew `r`: it may now merge with one already passed.
            if self.len == before {
                break;
            }
        }
        if self.len < N {
            self.rects[self.len] = r;
            self.len += 1;
        } else {
            let bounds = self.rects.iter().fold(r, |acc, x| acc.union(x));
            self.rects[0] = bounds;
            self.len = 1;
        }
    }

    /// The rectangles to redraw: disjoint or not, none empty, at most `N`.
    pub fn rects(&self) -> &[Rect] {
        &self.rects[..self.len]
    }

    /// Whether nothing changed.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bounding box of all the damage, or `None` if there is none.
    pub fn bounds(&self) -> Option<Rect> {
        let mut it = self.rects().iter();
        let first = *it.next()?;
        Some(it.fold(first, |acc, x| acc.union(x)))
    }

    /// Forget the damage (after presenting it).
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// The damage so far, leaving none behind.
    pub fn take(&mut self) -> Self {
        core::mem::take(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// xorshift64: deterministic randomness without a dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn range(&mut self, lo: i32, hi: i32) -> i32 {
            lo + (self.next() % (hi - lo) as u64) as i32
        }
    }

    #[test]
    fn starts_empty_and_ignores_empty_rects() {
        let mut d = Damage::<4>::new();
        assert!(d.is_empty());
        assert_eq!(d.bounds(), None);
        d.add(Rect::new(5, 5, 0, 9));
        d.add(Rect::EMPTY);
        assert!(d.is_empty());
        assert_eq!(d, Damage::default());
    }

    #[test]
    fn contained_and_containing_rects_merge() {
        let mut d = Damage::<4>::new();
        d.add(Rect::new(0, 0, 10, 10));
        d.add(Rect::new(2, 2, 3, 3));
        assert_eq!(d.rects(), &[Rect::new(0, 0, 10, 10)]);
        d.add(Rect::new(-5, -5, 30, 30));
        assert_eq!(d.rects(), &[Rect::new(-5, -5, 30, 30)]);
    }

    #[test]
    fn rects_sharing_an_edge_merge() {
        let mut d = Damage::<4>::new();
        d.add(Rect::new(0, 0, 10, 5));
        d.add(Rect::new(0, 5, 10, 5));
        assert_eq!(d.rects(), &[Rect::new(0, 0, 10, 10)]);
        d.add(Rect::new(-4, 0, 4, 10));
        assert_eq!(d.rects(), &[Rect::new(-4, 0, 14, 10)]);
    }

    #[test]
    fn overlap_merges_only_when_the_box_costs_nothing_extra() {
        let mut d = Damage::<4>::new();
        // Overlapping and aligned: the box is exactly their pixels.
        d.add(Rect::new(0, 0, 4, 4));
        d.add(Rect::new(2, 0, 4, 4));
        assert_eq!(d.rects(), &[Rect::new(0, 0, 6, 4)]);
        // Overlapping diagonally: a 25-pixel box for 23 pixels drawn, less
        // than the 32 drawn apart: merged.
        let mut d = Damage::<4>::new();
        d.add(Rect::new(0, 0, 4, 4));
        d.add(Rect::new(1, 1, 4, 4));
        assert_eq!(d.rects(), &[Rect::new(0, 0, 5, 5)]);
        // Overlapping by 1 pixel, the box adds 30: kept apart.
        let mut d = Damage::<4>::new();
        d.add(Rect::new(0, 0, 4, 4));
        d.add(Rect::new(3, 3, 4, 4));
        assert_eq!(d.rects().len(), 2);
        // An L: the box would add 9 empty pixels for 1 shared: kept apart.
        let mut d = Damage::<4>::new();
        d.add(Rect::new(0, 0, 4, 1));
        d.add(Rect::new(0, 0, 1, 4));
        assert_eq!(d.rects(), &[Rect::new(0, 0, 4, 1), Rect::new(0, 0, 1, 4)]);
        // A gap between them, or a corner touch: kept apart.
        let mut d = Damage::<4>::new();
        d.add(Rect::new(0, 0, 2, 2));
        d.add(Rect::new(3, 0, 2, 2));
        d.add(Rect::new(2, 2, 1, 1));
        assert_eq!(d.rects().len(), 3);
    }

    #[test]
    fn a_grown_rect_merges_with_ones_already_passed() {
        let mut d = Damage::<4>::new();
        d.add(Rect::new(0, 0, 2, 2));
        d.add(Rect::new(4, 0, 2, 2));
        assert_eq!(d.rects().len(), 2);
        // Bridges the gap: merges with the second, then the first.
        d.add(Rect::new(2, 0, 2, 2));
        assert_eq!(d.rects(), &[Rect::new(0, 0, 6, 2)]);
    }

    #[test]
    fn past_n_everything_collapses_to_the_bounding_box() {
        let mut d = Damage::<2>::new();
        d.add(Rect::new(0, 0, 1, 1));
        d.add(Rect::new(10, 0, 1, 1));
        assert_eq!(d.rects().len(), 2);
        d.add(Rect::new(-3, 7, 1, 1));
        assert_eq!(d.rects(), &[Rect::new(-3, 0, 14, 8)]);
        // One slot: every add after the first collapses into it.
        let mut d = Damage::<1>::new();
        d.add(Rect::new(0, 0, 1, 1));
        d.add(Rect::new(5, 5, 1, 1));
        assert_eq!(d.rects(), &[Rect::new(0, 0, 6, 6)]);
    }

    #[test]
    fn clear_take_and_bounds() {
        let mut d = Damage::<4>::new();
        d.add(Rect::new(0, 0, 1, 1));
        d.add(Rect::new(-5, 8, 2, 2));
        assert_eq!(d.bounds(), Some(Rect::new(-5, 0, 6, 10)));
        let taken = d.take();
        assert!(d.is_empty());
        assert_eq!(taken.rects().len(), 2);
        let mut t = taken;
        t.clear();
        assert!(t.is_empty());
    }

    /// Random rectangles (negative origins, empty ones, a range of list
    /// sizes): after every add, each pixel added so far is covered, nothing
    /// reported is empty or outside the added pixels' bounding box, and
    /// the list stays within `N`.
    fn coverage_is_never_lost<const N: usize>(seed: u64) {
        const LO: i32 = -20;
        const HI: i32 = 40;
        let w = (HI - LO + 1) as usize;
        let mut rng = Rng(seed);
        for _round in 0..60 {
            let mut d = Damage::<N>::new();
            let mut added = vec![false; w * w];
            let mut bounds = Rect::EMPTY;
            for _ in 0..rng.range(1, 30) {
                let r = Rect::new(
                    rng.range(LO, HI - 12),
                    rng.range(LO, HI - 12),
                    rng.range(0, 12) as u32,
                    rng.range(0, 12) as u32,
                );
                d.add(r);
                bounds = bounds.union(&r);
                for y in r.y()..r.bottom() {
                    for x in r.x()..r.right() {
                        added[(y - LO) as usize * w + (x - LO) as usize] = true;
                    }
                }
                assert!(d.rects().len() <= N);
                for rep in d.rects() {
                    assert!(!rep.is_empty(), "{rep:?} in {:?}", d.rects());
                    assert!(bounds.contains_rect(rep), "{rep:?} outside {bounds:?}");
                }
                for y in LO..HI {
                    for x in LO..HI {
                        if added[(y - LO) as usize * w + (x - LO) as usize] {
                            assert!(
                                d.rects().iter().any(|rep| rep.contains(x, y)),
                                "seed {seed}: {x},{y} lost after adding {r:?}: {:?}",
                                d.rects()
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn randomized_coverage_is_never_lost() {
        for seed in [1, 0xdead_beef, 0x1234_5678_9abc_def1] {
            coverage_is_never_lost::<1>(seed);
            coverage_is_never_lost::<2>(seed);
            coverage_is_never_lost::<4>(seed);
            coverage_is_never_lost::<8>(seed);
            coverage_is_never_lost::<32>(seed);
        }
    }
}
