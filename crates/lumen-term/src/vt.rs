//! Virtual terminals' keys: one keyboard, `N` terminals, each with its own
//! reader. [`Router`] keeps each terminal's keys in a ring of its own
//! (`CAP` keys), in the order they were routed: the kernel routes each key
//! to the terminal shown when it routes it, so keys routed after a switch
//! go to the terminal switched to, while those routed before stay with the
//! one left (whose reader may not have run yet).
//!
//! Fixed-size, without allocation (`const fn new`): the kernel keeps one in
//! a static, filled under a lock.

/// A ring of up to `CAP` keys.
struct Ring<K: Copy, const CAP: usize> {
    keys: [Option<K>; CAP],
    head: usize,
    len: usize,
}

impl<K: Copy, const CAP: usize> Ring<K, CAP> {
    const fn new() -> Self {
        Ring { keys: [None; CAP], head: 0, len: 0 }
    }

    /// Append `key`; false (and nothing kept) when full.
    fn push(&mut self, key: K) -> bool {
        if self.len == CAP {
            return false;
        }
        self.keys[(self.head + self.len) % CAP] = Some(key);
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<K> {
        if self.len == 0 {
            return None;
        }
        let key = self.keys[self.head].take();
        self.head = (self.head + 1) % CAP;
        self.len -= 1;
        key
    }
}

/// The keys routed to each of `N` terminals, waiting for its reader (`CAP`
/// each at most), and how many each dropped.
pub struct Router<K: Copy, const N: usize, const CAP: usize> {
    rings: [Ring<K, CAP>; N],
    dropped: [u64; N],
}

impl<K: Copy, const N: usize, const CAP: usize> Default for Router<K, N, CAP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Copy, const N: usize, const CAP: usize> Router<K, N, CAP> {
    /// No keys waiting.
    ///
    /// # Panics
    /// (At compile time when used in a `const`/`static`.) If `N` or `CAP` is
    /// 0, or `N` is more than a `usize` has bits (see [`Self::mask`]).
    pub const fn new() -> Self {
        assert!(N > 0 && CAP > 0, "a router needs a terminal and room for a key");
        assert!(N <= usize::BITS as usize, "a terminal per bit of the mask");
        Router { rings: [const { Ring::new() }; N], dropped: [0; N] }
    }

    /// Route `key` to terminal `vt`'s ring. False if it didn't fit (the
    /// ring was full: the key is dropped and counted) or there is no
    /// terminal `vt` (not counted).
    pub fn route(&mut self, vt: usize, key: K) -> bool {
        let Some(ring) = self.rings.get_mut(vt) else { return false };
        let kept = ring.push(key);
        if !kept {
            self.dropped[vt] += 1;
        }
        kept
    }

    /// The oldest key routed to terminal `vt`, if any (`None` too for a
    /// terminal that doesn't exist).
    pub fn take(&mut self, vt: usize) -> Option<K> {
        self.rings.get_mut(vt)?.pop()
    }

    /// Whether keys wait for terminal `vt`'s reader.
    pub fn has_keys(&self, vt: usize) -> bool {
        self.rings.get(vt).is_some_and(|r| r.len > 0)
    }

    /// Bit `vt` set for each terminal with keys waiting.
    pub fn mask(&self) -> usize {
        (0..N).filter(|&vt| self.has_keys(vt)).fold(0, |m, vt| m | 1 << vt)
    }

    /// Keys terminal `vt` dropped because its ring was full (0 for one that
    /// doesn't exist).
    pub fn dropped(&self, vt: usize) -> u64 {
        self.dropped.get(vt).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::Router;
    use std::vec::Vec;

    fn drain<const N: usize, const CAP: usize>(r: &mut Router<char, N, CAP>, vt: usize) -> Vec<char> {
        core::iter::from_fn(|| r.take(vt)).collect()
    }

    #[test]
    fn each_terminal_gets_its_own_keys_in_order() {
        let mut r: Router<char, 4, 8> = Router::new();
        assert_eq!(r.mask(), 0);
        for c in "ab".chars() {
            assert!(r.route(0, c));
        }
        // Routed elsewhere before terminal 0's reader ran: its keys stay.
        for c in "xyz".chars() {
            assert!(r.route(1, c));
        }
        assert!(r.has_keys(0) && r.has_keys(1) && !r.has_keys(2));
        assert_eq!(r.mask(), 0b11);
        assert_eq!(drain(&mut r, 1), ['x', 'y', 'z']);
        assert!(r.route(0, 'c'));
        assert_eq!(drain(&mut r, 0), ['a', 'b', 'c']);
        assert_eq!((r.take(2), r.mask()), (None, 0));
    }

    #[test]
    fn a_terminal_that_does_not_exist_takes_nothing() {
        let mut r: Router<char, 4, 8> = Router::new();
        assert!(!r.route(4, 'a'));
        assert!(!r.route(usize::MAX, 'a'));
        assert_eq!((r.take(4), r.has_keys(4), r.dropped(4), r.mask()), (None, false, 0, 0));
    }

    #[test]
    fn a_full_ring_drops_and_counts_the_new_key() {
        let mut r: Router<char, 2, 3> = Router::new();
        for c in "abc".chars() {
            assert!(r.route(0, c));
        }
        assert!(!r.route(0, 'd'));
        assert_eq!((r.dropped(0), r.dropped(1)), (1, 0));
        // The other terminal's ring is its own.
        assert!(r.route(1, 'e'));
        assert_eq!(drain(&mut r, 0), ['a', 'b', 'c']);
        // Room again, and the ring wraps around in order.
        for c in "fgh".chars() {
            assert!(r.route(0, c));
        }
        assert_eq!(r.take(0), Some('f'));
        assert!(r.route(0, 'i'));
        assert_eq!(drain(&mut r, 0), ['g', 'h', 'i']);
        assert_eq!(drain(&mut r, 1), ['e']);
        assert_eq!(r.dropped(0), 1);
    }

    #[test]
    fn interleaved_routing_and_taking_matches_a_model() {
        // A model: one queue per terminal, unbounded; the router agrees
        // while no ring overflows (`route` asserts none did with this seed).
        let mut r: Router<u32, 4, 64> = Router::new();
        let mut model: [Vec<u32>; 4] = Default::default();
        let mut seed = 0x2545_f491_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for key in 0..400 {
            let vt = (next() % 4) as usize;
            if next() % 3 == 0 {
                let want = (!model[vt].is_empty()).then(|| model[vt].remove(0));
                assert_eq!(r.take(vt), want);
            } else {
                assert!(r.route(vt, key));
                model[vt].push(key);
            }
            let mask = (0..4).filter(|&v| !model[v].is_empty()).fold(0, |m, v| m | 1 << v);
            assert_eq!(r.mask(), mask);
        }
    }
}
