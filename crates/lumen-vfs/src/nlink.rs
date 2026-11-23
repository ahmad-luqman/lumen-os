//! In-memory corrections to on-disk link counts, for the kernel's ext4
//! adapter.
//!
//! A namespace operation that fails part-way rolls the link counts it already
//! changed back; if a rollback write fails too, the disk keeps a count that
//! disagrees with the entries that really exist. [`NlinkBounds`] records what
//! each such count must be (at least a floor, at most a ceiling, or exactly
//! both) until a write of the right count lands. Reading the disk and logging
//! stay with the caller: each method that sets a bound is handed the count the
//! caller read (`None` if it couldn't) and says whether the kept bound changed,
//! which is when the caller logs it.

use alloc::collections::BTreeMap;

/// One inode's bound: at least `floor`, at most `ceiling`. Never trivial
/// (`floor > 0` or a ceiling), and `floor <= ceiling` whenever there is a
/// ceiling: [`NlinkBounds`]'s setters keep both, so a stored bound always
/// admits a count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Bound {
    floor: u32,
    ceiling: Option<u32>,
}

impl Bound {
    /// The bound pinning the count to `exact`.
    const fn exact(exact: u32) -> Self {
        Self { floor: exact, ceiling: Some(exact) }
    }

    fn is_trivial(self) -> bool {
        self.floor == 0 && self.ceiling.is_none()
    }
}

/// Per-inode link-count bounds, keyed by on-disk inode number: one map, so a
/// floor and a ceiling can't disagree.
#[derive(Debug, Default)]
pub struct NlinkBounds {
    bounds: BTreeMap<u32, Bound>,
}

impl NlinkBounds {
    /// No bounds.
    pub const fn new() -> Self {
        Self { bounds: BTreeMap::new() }
    }

    /// `ino`'s floor, 0 without one.
    pub fn floor(&self, ino: u32) -> u32 {
        self.bounds.get(&ino).map_or(0, |b| b.floor)
    }

    /// `ino`'s ceiling, if it has one.
    pub fn ceiling(&self, ino: u32) -> Option<u32> {
        self.bounds.get(&ino).and_then(|b| b.ceiling)
    }

    /// A count read from the disk (`raw`) brought within `ino`'s bound.
    pub fn apply(&self, ino: u32, raw: u32) -> u32 {
        let floor = self.floor(ino);
        let with_floor = raw.max(floor);
        match self.ceiling(ino) {
            Some(ceiling) => with_floor.min(ceiling),
            None => with_floor,
        }
    }

    /// The count to assume for `ino` when its disk count can't be read: its
    /// floor if it has one, else its ceiling, else nothing known.
    pub fn assumed(&self, ino: u32) -> Option<u32> {
        match self.floor(ino) {
            0 => self.ceiling(ino),
            floor => Some(floor),
        }
    }

    /// Require at least `min` links for `ino`, a count a failed write may have
    /// left too low on disk; `disk` is what the caller read there. Nothing is
    /// kept when `min` is 0 or the disk already holds at least `min`. A
    /// ceiling below the new floor goes. Whether the floor changed.
    pub fn raise_floor(&mut self, ino: u32, disk: Option<u32>, min: u32) -> bool {
        if min == 0 || disk.is_some_and(|n| n >= min) {
            return false;
        }
        let old = self.bounds.get(&ino).copied().unwrap_or(Bound { floor: 0, ceiling: None });
        let floor = old.floor.max(min);
        let ceiling = old.ceiling.filter(|&c| c >= floor);
        self.bounds.insert(ino, Bound { floor, ceiling });
        floor != old.floor
    }

    /// `ino` should have `expected` links and the disk holds `disk` (`None`:
    /// unreadable): nothing is kept when they agree, else the count is pinned
    /// to `expected`. Whether a pin was set that wasn't already there.
    pub fn reconcile(&mut self, ino: u32, disk: Option<u32>, expected: u32) -> bool {
        if disk == Some(expected) {
            self.clear(ino);
            return false;
        }
        let pin = Bound::exact(expected);
        self.bounds.insert(ino, pin) != Some(pin)
    }

    /// Drop `ino`'s floor, keeping a ceiling: even one below the floor
    /// that just landed on disk, which [`Self::apply`] then still caps to
    /// (unlike [`Self::raise_floor`], which drops such a ceiling). As the
    /// two maps this replaced did. The kernel's floors come from counts
    /// with the bounds applied, so none is known to land above a ceiling.
    pub fn clear_floor(&mut self, ino: u32) {
        if let Some(bound) = self.bounds.get_mut(&ino) {
            bound.floor = 0;
            if bound.is_trivial() {
                self.bounds.remove(&ino);
            }
        }
    }

    /// Drop `ino`'s bound: a write of its right count landed.
    pub fn clear(&mut self, ino: u32) {
        self.bounds.remove(&ino);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_raises_to_the_floor_and_lowers_to_the_ceiling() {
        let mut b = NlinkBounds::new();
        assert_eq!(b.apply(5, 3), 3);
        assert!(b.raise_floor(5, Some(1), 2));
        assert_eq!(b.apply(5, 1), 2);
        assert_eq!(b.apply(5, 4), 4);
        assert!(b.reconcile(5, Some(4), 3));
        assert_eq!((b.apply(5, 1), b.apply(5, 9)), (3, 3));
        assert_eq!(b.apply(6, 9), 9); // another inode is untouched
    }

    #[test]
    fn raise_floor_keeps_nothing_the_disk_already_holds() {
        let mut b = NlinkBounds::new();
        assert!(!b.raise_floor(1, Some(3), 3));
        assert!(!b.raise_floor(1, Some(4), 3));
        assert!(!b.raise_floor(1, None, 0));
        assert_eq!((b.floor(1), b.ceiling(1)), (0, None));
        assert!(b.bounds.is_empty()); // not even a trivial bound for an unreadable disk
        assert!(b.raise_floor(1, None, 3));
        assert_eq!(b.floor(1), 3);
        assert!(!b.raise_floor(1, None, 2)); // lower: unchanged
        assert_eq!(b.floor(1), 3);
    }

    #[test]
    fn a_floor_raised_past_a_ceiling_drops_it() {
        let mut b = NlinkBounds::new();
        assert!(b.reconcile(1, Some(5), 2));
        assert!(b.raise_floor(1, Some(1), 3));
        assert_eq!((b.floor(1), b.ceiling(1)), (3, None));
        // A ceiling at the new floor stays.
        assert!(b.reconcile(2, None, 3));
        assert!(!b.raise_floor(2, Some(1), 3));
        assert_eq!((b.floor(2), b.ceiling(2)), (3, Some(3)));
    }

    #[test]
    fn reconcile_pins_and_clears() {
        let mut b = NlinkBounds::new();
        assert!(!b.reconcile(1, Some(2), 2));
        assert_eq!((b.floor(1), b.ceiling(1)), (0, None));
        assert!(b.reconcile(1, None, 2));
        assert!(!b.reconcile(1, Some(7), 2)); // the same pin again: not logged twice
        assert_eq!((b.floor(1), b.ceiling(1)), (2, Some(2)));
        assert!(!b.reconcile(1, Some(2), 2)); // the disk caught up
        assert_eq!((b.floor(1), b.ceiling(1)), (0, None));
        // Pinned to 0: a ceiling alone.
        assert!(b.reconcile(1, Some(1), 0));
        assert_eq!((b.floor(1), b.ceiling(1), b.apply(1, 4)), (0, Some(0), 0));
    }

    #[test]
    fn clear_floor_keeps_a_ceiling_and_clear_drops_both() {
        let mut b = NlinkBounds::new();
        assert!(b.reconcile(1, None, 2));
        b.clear_floor(1);
        assert_eq!((b.floor(1), b.ceiling(1)), (0, Some(2)));
        assert_eq!(b.assumed(1), Some(2));
        b.clear(1);
        assert_eq!((b.floor(1), b.ceiling(1), b.assumed(1)), (0, None, None));
        assert!(b.raise_floor(1, None, 4));
        b.clear_floor(1);
        assert!(b.bounds.is_empty()); // nothing trivial is kept
    }

    #[test]
    fn assumed_prefers_the_floor() {
        let mut b = NlinkBounds::new();
        assert_eq!(b.assumed(1), None);
        b.raise_floor(1, None, 3);
        assert_eq!(b.assumed(1), Some(3));
        b.reconcile(1, None, 4);
        assert_eq!(b.assumed(1), Some(4));
        // A floor below a ceiling: the floor, which `apply` still raises to
        // and the ceiling still caps.
        b.reconcile(2, None, 5);
        b.clear_floor(2);
        b.raise_floor(2, Some(1), 3);
        assert_eq!((b.floor(2), b.ceiling(2)), (3, Some(5)));
        assert_eq!(b.assumed(2), Some(3));
        assert_eq!((b.apply(2, 1), b.apply(2, 9)), (3, 5));
    }

    /// The kernel's two maps before `NlinkBounds` (`Ext4FsInner`'s
    /// `nlink_floors`/`nlink_ceilings` and their methods, as of #307), with
    /// the disk count handed in instead of read.
    #[derive(Default)]
    struct TwoMaps {
        floors: BTreeMap<u32, u32>,
        ceilings: BTreeMap<u32, u32>,
    }

    impl TwoMaps {
        fn nlink_floor(&self, ino: u32) -> u32 {
            self.floors.get(&ino).copied().unwrap_or(0)
        }

        fn nlink_ceiling(&self, ino: u32) -> Option<u32> {
            self.ceilings.get(&ino).copied()
        }

        fn apply_nlink_bounds(&self, ino: u32, raw: u32) -> u32 {
            let floor = self.nlink_floor(ino);
            let with_floor = raw.max(floor);
            match self.nlink_ceiling(ino) {
                Some(ceil) => with_floor.min(ceil.max(floor)),
                None => with_floor,
            }
        }

        /// The kernel's `clamp_to_bounds` (step 2's `clamp_bounds` then) without
        /// a disk count.
        fn assumed(&self, ino: u32) -> Option<u32> {
            match (self.nlink_floor(ino), self.nlink_ceiling(ino)) {
                (0, None) => None,
                (0, Some(c)) => Some(c),
                (f, Some(c)) => Some(f.min(c.max(f))),
                (f, None) => Some(f),
            }
        }

        fn record_nlink_floor(&mut self, ino: u32, disk: Option<u32>, min: u32) -> bool {
            if min == 0 {
                return false;
            }
            if disk.is_some_and(|n| n >= min) {
                return false;
            }
            let old_floor = self.nlink_floor(ino);
            let new_floor = *self.floors.entry(ino).and_modify(|cur| *cur = (*cur).max(min)).or_insert(min);
            if self.ceilings.get(&ino).is_some_and(|&c| c < new_floor) {
                self.ceilings.remove(&ino);
            }
            new_floor != old_floor
        }

        fn set_nlink_floor(&mut self, ino: u32, floor: u32) {
            if floor == 0 {
                self.floors.remove(&ino);
            } else {
                self.floors.insert(ino, floor);
                if self.ceilings.get(&ino).is_some_and(|&c| c < floor) {
                    self.ceilings.remove(&ino);
                }
            }
        }

        fn set_nlink_ceiling(&mut self, ino: u32, ceiling: u32) {
            self.ceilings.insert(ino, ceiling);
            if self.floors.get(&ino).is_some_and(|&f| f > ceiling) {
                self.floors.remove(&ino);
            }
        }

        fn set_nlink_exact(&mut self, ino: u32, disk: Option<u32>, exact: u32) -> bool {
            if disk == Some(exact) {
                self.clear_nlink_bounds(ino);
                return false;
            }
            let unchanged = self.nlink_floor(ino) == exact && self.nlink_ceiling(ino) == Some(exact);
            self.set_nlink_floor(ino, exact);
            self.set_nlink_ceiling(ino, exact);
            !unchanged
        }

        fn clear_nlink_floor(&mut self, ino: u32) {
            self.floors.remove(&ino);
        }

        fn clear_nlink_bounds(&mut self, ino: u32) {
            self.floors.remove(&ino);
            self.ceilings.remove(&ino);
        }
    }

    /// SplitMix64: a fixed seed, the same run every time.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u32 {
            (self.next() % n) as u32
        }

        /// A count from 0 to 5, or (rarely) the disk unreadable.
        fn disk(&mut self) -> Option<u32> {
            match self.below(8) {
                7 => None,
                n => Some(n.min(5)),
            }
        }
    }

    #[test]
    fn agrees_with_the_two_maps_it_replaced() {
        const INOS: u64 = 4;
        let mut rng = Rng(0x6e6c_696e_6b73);
        for _run in 0..200 {
            let mut model = TwoMaps::default();
            let mut bounds = NlinkBounds::new();
            for step in 0..200 {
                let ino = rng.below(INOS);
                match rng.below(5) {
                    0 => {
                        let (disk, min) = (rng.disk(), rng.below(6));
                        let logged = model.record_nlink_floor(ino, disk, min);
                        assert_eq!(bounds.raise_floor(ino, disk, min), logged, "step {step}: raise_floor");
                    }
                    1 => {
                        let (disk, exact) = (rng.disk(), rng.below(6));
                        let logged = model.set_nlink_exact(ino, disk, exact);
                        assert_eq!(bounds.reconcile(ino, disk, exact), logged, "step {step}: reconcile");
                    }
                    2 => {
                        model.clear_nlink_floor(ino);
                        bounds.clear_floor(ino);
                    }
                    3 => {
                        model.clear_nlink_bounds(ino);
                        bounds.clear(ino);
                    }
                    _ => {}
                }
                for ino in 0..INOS as u32 {
                    let what = (model.nlink_floor(ino), model.nlink_ceiling(ino));
                    assert_eq!((bounds.floor(ino), bounds.ceiling(ino)), what, "step {step}: inode {ino}");
                    assert_eq!(bounds.assumed(ino), model.assumed(ino), "step {step}: inode {ino}");
                    for raw in 0..7 {
                        assert_eq!(bounds.apply(ino, raw), model.apply_nlink_bounds(ino, raw), "step {step}");
                    }
                }
                for (ino, b) in &bounds.bounds {
                    assert!(!b.is_trivial(), "step {step}: inode {ino} kept a trivial bound");
                    assert!(b.ceiling.is_none_or(|c| b.floor <= c), "step {step}: inode {ino}: {b:?}");
                }
            }
        }
    }
}
