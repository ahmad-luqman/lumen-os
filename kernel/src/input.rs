//! The desktop's input rings (#151; ARCHITECTURE section 7,
//! `InputDevice`): every tablet frame ([`POINTER`]) and every key event
//! ([`KEYS`]), each numbered in order, for readers that need them all (M4's
//! compositor, #153; the shell's `mouse events` and `kbd events`).
//!
//! Each ring is a `lumen_input::Ring` behind one `SpinLock`, a leaf:
//! nothing is taken under it and nothing allocates, so it may be pushed
//! under any other lock (the keyboard's decoding pass pushes under
//! `crate::vt`'s router lock and the keyboard's own). Reading takes
//! nothing out, so several readers can read one ring; a full ring
//! overwrites its oldest event. The `mouse` thread pushes each frame
//! before it publishes the newest state (`crate::mouse`), and the
//! keyboard's decoding pass each key event it decodes
//! (`crate::keyboard::read_key`). In text mode nothing reads them: they
//! fill and wrap.

use alloc::vec::Vec;

use lumen_input::{InputDevice, KeyEvent, PointerEvent, Ring, Seq};

use crate::sync::SpinLock;

/// A device's ring behind its lock: an `InputDevice` readers can share.
pub struct LockedRing<E: Copy, const N: usize>(SpinLock<Ring<E, N>>);

impl<E: Copy, const N: usize> LockedRing<E, N> {
    pub const fn new() -> Self {
        Self(SpinLock::new(Ring::new()))
    }

    /// Add `event` as the newest; the number it got.
    pub fn push(&self, event: E) -> Seq {
        self.0.lock().push(event)
    }

    /// The newest `n` events (fewer if the ring holds fewer), oldest
    /// first, with their numbers.
    pub fn recent(&self, n: usize) -> Vec<(Seq, E)> {
        // Copied out under the lock, the list allocated before taking it
        // (nothing allocates under a leaf).
        let mut out = Vec::with_capacity(n.min(N));
        let ring = self.0.lock();
        out.extend(ring.recent(n));
        out
    }
}

impl<E: Copy + Send, const N: usize> InputDevice for LockedRing<E, N> {
    type Event = E;

    fn next(&self, after: Seq) -> Option<(Seq, E)> {
        self.0.lock().next(after)
    }

    fn newest(&self) -> Seq {
        self.0.lock().newest()
    }
}

/// The tablet's frames: 256 (QMP sends a frame every 0.7 to 3 ms, so a
/// burst fills it in under a second).
pub static POINTER: LockedRing<PointerEvent, 256> = LockedRing::new();

/// The keyboard's key events: 128, the same size as a terminal's ring.
pub static KEYS: LockedRing<KeyEvent, 128> = LockedRing::new();

crate::ktest!(
    fn a_reader_reads_a_ring_in_order_and_counts_what_it_missed() {
        // A ring of the kernel's own kind (a static, so a `Reader` can hold
        // it), pushed and read on one CPU.
        use crate::kassert_eq;
        use lumen_input::Reader;
        static RING: LockedRing<u32, 4> = LockedRing::new();
        let mut reader = Reader::from_now(&RING);
        let first = RING.push(1);
        kassert_eq!(reader.next(), Some(1));
        kassert_eq!(reader.after(), first);
        for e in 2..=7 {
            RING.push(e);
        }
        // 2 and 3 were overwritten before the reader looked.
        kassert_eq!(reader.next(), Some(4));
        kassert_eq!(reader.lost(), 2);
        let recent: Vec<u32> = RING.recent(2).into_iter().map(|(_, e)| e).collect();
        kassert_eq!(recent, alloc::vec![6, 7]);
        Ok(())
    }
);
