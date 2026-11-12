//! Who may map pages where: [`Window`] tokens for the kernel's heap and stack
//! windows.
//!
//! A `Window` is proof that a page-aligned virtual range belongs to its
//! holder alone. It is move-only and has no public constructor: windows come
//! from a [`Claims`] table, which hands each of its ranges out once ever
//! (building a table is `unsafe`: it vouches that nothing else covers those
//! ranges), and a holder can split off disjoint sub-windows
//! ([`Window::split_off`]). So two live windows never overlap, and the
//! kernel's `paging::map` can be safe for whoever holds the window for a
//! page.
//!
//! Claims are never released: a dropped window's range is gone for good,
//! because sub-windows split off it may still be in use.

use core::ops::Range;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::FRAME_SIZE;

/// A page-aligned virtual range that belongs to its holder alone.
#[derive(Debug, PartialEq, Eq)]
pub struct Window {
    start: u64,
    end: u64,
}

impl Window {
    /// The window's addresses.
    pub fn range(&self) -> Range<u64> {
        self.start..self.end
    }

    pub fn len(&self) -> u64 {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// Are `pages` (any range, not necessarily aligned) inside this window?
    pub fn contains(&self, pages: &Range<u64>) -> bool {
        self.start <= pages.start && pages.start <= pages.end && pages.end <= self.end
    }

    /// Does this window own the page at `virt`?
    pub fn owns_page(&self, virt: u64) -> bool {
        virt.checked_add(FRAME_SIZE).is_some_and(|end| self.contains(&(virt..end)))
    }

    /// Give the lowest `len` bytes (whole pages) away as a window of their
    /// own; this window keeps the rest. `None` if it is smaller than `len`.
    pub fn split_off(&mut self, len: u64) -> Option<Window> {
        assert!(len.is_multiple_of(FRAME_SIZE), "window split {len:#x} is not whole pages");
        if len > self.len() {
            return None;
        }
        let low = Window { start: self.start, end: self.start + len };
        self.start += len;
        Some(low)
    }
}

/// A fixed set of ranges, each handed out once ever as a [`Window`].
#[derive(Debug)]
pub struct Claims<const N: usize> {
    ranges: [(u64, u64); N],
    taken: [AtomicBool; N],
}

impl<const N: usize> Claims<N> {
    /// A table of `ranges` (`(start, end)`, page-aligned, disjoint).
    ///
    /// # Safety
    /// Nothing else maps or hands out these ranges: no other table covers
    /// them, and they lie inside the address-space windows meant for
    /// claimants (for the kernel, the heap and stack windows). Every
    /// window's soundness rests on this.
    ///
    /// # Panics
    /// If a range is not page-aligned and non-empty, or two overlap.
    pub const unsafe fn new(ranges: [(u64, u64); N]) -> Claims<N> {
        let mut i = 0;
        while i < N {
            let (start, end) = ranges[i];
            assert!(start.is_multiple_of(FRAME_SIZE) && end.is_multiple_of(FRAME_SIZE) && start < end);
            let mut j = 0;
            while j < i {
                let (s, e) = ranges[j];
                assert!(end <= s || e <= start, "claim ranges overlap");
                j += 1;
            }
            i += 1;
        }
        Claims { ranges, taken: [const { AtomicBool::new(false) }; N] }
    }

    /// Range `i` as a window, the first time it is asked for; `None` ever
    /// after.
    pub fn claim(&self, i: usize) -> Option<Window> {
        let (start, end) = self.ranges[i];
        (!self.taken[i].swap(true, Ordering::AcqRel)).then_some(Window { start, end })
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    const BASE: u64 = 0xffff_c000_0000_0000;
    const P: u64 = FRAME_SIZE;

    fn table() -> Claims<2> {
        // SAFETY: host tests never map anything.
        unsafe { Claims::new([(BASE, BASE + 16 * P), (BASE + 16 * P, BASE + 20 * P)]) }
    }

    #[test]
    fn each_range_is_claimed_once_ever() {
        let claims = table();
        let w = claims.claim(0).unwrap();
        assert_eq!(w.range(), BASE..BASE + 16 * P);
        assert_eq!(claims.claim(0), None);
        drop(w);
        assert_eq!(claims.claim(0), None, "dropping a window doesn't release its claim");
        assert_eq!(claims.claim(1).map(|w| w.range()), Some(BASE + 16 * P..BASE + 20 * P));
    }

    #[test]
    fn split_off_gives_disjoint_windows() {
        let mut w = table().claim(0).unwrap();
        let a = w.split_off(2 * P).unwrap();
        let b = w.split_off(3 * P).unwrap();
        assert_eq!(
            (a.range(), b.range(), w.range()),
            (BASE..BASE + 2 * P, BASE + 2 * P..BASE + 5 * P, BASE + 5 * P..BASE + 16 * P)
        );
        assert_eq!(w.split_off(12 * P), None, "more than is left");
        assert_eq!(w.len(), 11 * P, "a refused split changes nothing");
        let rest = w.split_off(11 * P).unwrap();
        assert!(w.is_empty() && rest.len() == 11 * P);
        assert_eq!(w.split_off(0).map(|z| z.is_empty()), Some(true));
    }

    #[test]
    #[should_panic(expected = "not whole pages")]
    fn split_off_refuses_partial_pages() {
        table().claim(0).unwrap().split_off(100);
    }

    #[test]
    fn contains_and_owns_page_check_both_ends() {
        let w = table().claim(1).unwrap();
        let (lo, hi) = (BASE + 16 * P, BASE + 20 * P);
        assert!(w.owns_page(lo) && w.owns_page(hi - P));
        assert!(!w.owns_page(lo - P) && !w.owns_page(hi));
        assert!(!w.owns_page(u64::MAX - 8), "no overflow at the top");
        assert!(w.contains(&(lo..hi)) && w.contains(&(lo + 8..lo + 16)));
        assert!(!w.contains(&(lo - 8..lo + 8)) && !w.contains(&(hi - 8..hi + 8)));
        assert!(!w.contains(&(lo + 2 * P..lo + P)), "backwards");
    }

    #[test]
    fn overlapping_or_misaligned_tables_are_refused() {
        let overlap = std::panic::catch_unwind(|| {
            // SAFETY: host tests never map anything.
            unsafe { Claims::new([(BASE, BASE + 2 * P), (BASE + P, BASE + 3 * P)]) }
        });
        let misaligned = std::panic::catch_unwind(|| {
            // SAFETY: as above.
            unsafe { Claims::new([(BASE + 8, BASE + P)]) }
        });
        let empty = std::panic::catch_unwind(|| {
            // SAFETY: as above.
            unsafe { Claims::new([(BASE, BASE)]) }
        });
        assert!(overlap.is_err() && misaligned.is_err() && empty.is_err());
    }
}
