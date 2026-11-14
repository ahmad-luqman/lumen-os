//! Virtual terminals' keys: one keyboard, `N` terminals, each with its own
//! reader. [`Router`] sends every key typed to the terminal shown when it
//! was typed, in order, into that terminal's own ring of `CAP` keys, so a
//! key typed after a switch goes to the terminal switched to, never to the
//! one left (whose reader may not have run yet).
//!
//! Fixed-size, without allocation (`const fn new`): the kernel keeps one in
//! a static and fills it with interrupts off.

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

/// Which of `N` terminals is shown, and the keys typed on each waiting for
/// its reader (`CAP` each at most).
pub struct Router<K: Copy, const N: usize, const CAP: usize> {
    shown: usize,
    rings: [Ring<K, CAP>; N],
    dropped: u64,
}

impl<K: Copy, const N: usize, const CAP: usize> Default for Router<K, N, CAP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Copy, const N: usize, const CAP: usize> Router<K, N, CAP> {
    /// Terminal 0 shown, no keys waiting.
    ///
    /// # Panics
    /// (At compile time when used in a `const`/`static`.) If `N` or `CAP` is 0.
    pub const fn new() -> Self {
        assert!(N > 0 && CAP > 0, "a router needs a terminal and room for a key");
        Router { shown: 0, rings: [const { Ring::new() }; N], dropped: 0 }
    }

    /// The terminal shown (where keys go).
    pub fn shown(&self) -> usize {
        self.shown
    }

    /// Show terminal `vt`: keys typed from now on go to it (those already
    /// routed stay with their terminal). False, changing nothing, if there
    /// is no terminal `vt`.
    pub fn show(&mut self, vt: usize) -> bool {
        if vt >= N {
            return false;
        }
        self.shown = vt;
        true
    }

    /// A key typed: to the shown terminal's ring. False if that ring was
    /// full (the key is dropped and counted).
    pub fn route(&mut self, key: K) -> bool {
        let kept = self.rings[self.shown].push(key);
        if !kept {
            self.dropped += 1;
        }
        kept
    }

    /// The oldest key routed to terminal `vt`, if any (`None` too for a
    /// terminal that doesn't exist).
    pub fn take(&mut self, vt: usize) -> Option<K> {
        self.rings.get_mut(vt)?.pop()
    }

    /// Keys waiting for terminal `vt`'s reader.
    pub fn waiting(&self, vt: usize) -> usize {
        self.rings.get(vt).map_or(0, |r| r.len)
    }

    /// Keys dropped because their terminal's ring was full.
    pub fn dropped(&self) -> u64 {
        self.dropped
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
    fn keys_go_to_the_terminal_shown_when_typed() {
        let mut r: Router<char, 4, 8> = Router::new();
        assert_eq!(r.shown(), 0);
        for c in "ab".chars() {
            assert!(r.route(c));
        }
        // Switched before terminal 0's reader ran: its keys stay its own.
        assert!(r.show(1));
        for c in "xyz".chars() {
            assert!(r.route(c));
        }
        assert_eq!((r.waiting(0), r.waiting(1), r.waiting(2)), (2, 3, 0));
        assert_eq!(drain(&mut r, 1), ['x', 'y', 'z']);
        assert!(r.show(0));
        assert!(r.route('c'));
        assert_eq!(drain(&mut r, 0), ['a', 'b', 'c']);
        assert_eq!(r.take(2), None);
    }

    #[test]
    fn showing_a_terminal_that_does_not_exist_changes_nothing() {
        let mut r: Router<char, 4, 8> = Router::new();
        assert!(r.show(3));
        assert!(!r.show(4));
        assert!(!r.show(usize::MAX));
        assert_eq!(r.shown(), 3);
        assert_eq!((r.take(4), r.waiting(4)), (None, 0));
    }

    #[test]
    fn a_full_ring_drops_and_counts_the_new_key() {
        let mut r: Router<char, 2, 3> = Router::new();
        for c in "abc".chars() {
            assert!(r.route(c));
        }
        assert!(!r.route('d'));
        assert_eq!(r.dropped(), 1);
        // The other terminal's ring is its own.
        assert!(r.show(1));
        assert!(r.route('e'));
        assert_eq!(drain(&mut r, 0), ['a', 'b', 'c']);
        // Room again, and the ring wraps around in order.
        assert!(r.show(0));
        for c in "fgh".chars() {
            assert!(r.route(c));
        }
        assert_eq!(r.take(0), Some('f'));
        assert!(r.route('i'));
        assert_eq!(drain(&mut r, 0), ['g', 'h', 'i']);
        assert_eq!(drain(&mut r, 1), ['e']);
        assert_eq!(r.dropped(), 1);
    }

    #[test]
    fn interleaved_typing_and_switching_matches_a_model() {
        // A model: one queue per terminal, unbounded; the router agrees
        // while no ring overflows (`route` asserts none did with this seed).
        let mut r: Router<u32, 4, 64> = Router::new();
        let mut model: [Vec<u32>; 4] = Default::default();
        let mut shown = 0;
        let mut seed = 0x2545_f491_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for key in 0..160 {
            match next() % 5 {
                0 => {
                    shown = (next() % 4) as usize;
                    assert!(r.show(shown));
                }
                1 => {
                    let vt = (next() % 4) as usize;
                    let got = r.take(vt);
                    let want = (!model[vt].is_empty()).then(|| model[vt].remove(0));
                    assert_eq!(got, want);
                }
                _ => {
                    assert!(r.route(key));
                    model[shown].push(key);
                }
            }
        }
        for (vt, keys) in model.iter().enumerate() {
            assert_eq!(r.waiting(vt), keys.len());
        }
    }
}
