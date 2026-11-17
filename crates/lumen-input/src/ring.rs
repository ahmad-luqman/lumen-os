//! Input devices as rings of numbered events (#151; ARCHITECTURE section
//! 7, `InputDevice`): each device keeps its newest events, numbered in
//! order from 1, and a reader asks for the first event after the last one
//! it saw.
//!
//! - [`Ring`]: a fixed array of the newest `N` events and the next number;
//!   [`Ring::push`] overwrites the oldest and returns the [`Seq`] it gave.
//!   Reading takes nothing out, so several readers can read one ring.
//! - [`InputDevice`]: what a reader needs of a device (the kernel's are
//!   rings behind a lock).
//! - [`Reader`]: one reader's place in one device's ring; a gap in the
//!   numbers (the ring overwrote events before this reader got to them) is
//!   counted in [`Reader::lost`] and given with the event after it
//!   ([`Read::lost_before`]). That is the only loss a gap means: frames
//!   QEMU dropped at the device never reach the ring
//!   ([`crate::AfterOverrun`] marks the frames after them instead).

/// An event's number on its device: 1 for the first, never reused in a
/// boot. [`Seq::ZERO`] is "before the first event".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(u64);

impl Seq {
    pub const ZERO: Seq = Seq(0);

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A stream of numbered events: the tablet's frames or the keyboard's key
/// events.
pub trait InputDevice: Sync {
    /// What this device's ring holds.
    type Event: Copy;
    /// The first event still in the ring numbered after `after`, with its
    /// number; `None` if there is none yet (also for an `after` past
    /// [`InputDevice::newest`]). A number past `after + 1` means the events
    /// in between were overwritten before this reader got to them.
    fn next(&self, after: Seq) -> Option<(Seq, Self::Event)>;
    /// The newest event's number ([`Seq::ZERO`] before the first).
    fn newest(&self) -> Seq;
}

/// One reader's place in one device's ring (the kernel's devices are
/// statics, hence `'static`; a reader can't be handed another device's
/// events).
pub struct Reader<D: InputDevice + 'static> {
    device: &'static D,
    after: Seq,
    lost: u64,
}

impl<D: InputDevice + 'static> Reader<D> {
    /// Starting after the device's newest event.
    pub fn from_now(device: &'static D) -> Reader<D> {
        Reader { device, after: device.newest(), lost: 0 }
    }

    /// The next event, with its number and the events the ring
    /// overwrote between it and the last one this reader got (also counted
    /// in [`Reader::lost`]); `None` once it has caught up (a later event
    /// makes it give more).
    pub fn read(&mut self) -> Option<Read<D::Event>> {
        let (seq, event) = self.device.next(self.after)?;
        let lost_before = seq.0 - self.after.0 - 1;
        self.lost += lost_before;
        self.after = seq;
        Some(Read { seq, lost_before, event })
    }

    /// Skip to after `seq`, never backwards and never past the device's
    /// newest event (the desktop's cut, section 11; a number from another
    /// device's ring, past this one's newest, is a bug: it stops at the
    /// newest rather than make the reader wait for the ring to catch up).
    /// Events skipped this way aren't counted as lost.
    pub fn skip_to(&mut self, seq: Seq) {
        let newest = self.device.newest();
        // (Not in this crate's host tests, which check the clamp itself.)
        #[cfg(not(test))]
        debug_assert!(seq <= newest, "skip_to {seq:?} past the device's newest {newest:?}");
        self.after = self.after.max(seq.min(newest));
    }

    /// The number of the last event this reader got (or skipped to).
    pub fn after(&self) -> Seq {
        self.after
    }

    /// Events the ring overwrote before this reader got to them.
    pub fn lost(&self) -> u64 {
        self.lost
    }
}

/// One event a [`Reader`] got: its number, the events the ring overwrote
/// just before it (a gap this reader fell behind into), and the event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Read<E> {
    pub seq: Seq,
    pub lost_before: u64,
    pub event: E,
}

/// The newest `N` events of one device, and the number the next one gets.
#[derive(Clone, Debug)]
pub struct Ring<E: Copy, const N: usize> {
    /// Event `n` (from 1) is at `(n - 1) % N` while it is one of the newest
    /// `N`.
    events: [Option<E>; N],
    newest: u64,
}

impl<E: Copy, const N: usize> Default for Ring<E, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: Copy, const N: usize> Ring<E, N> {
    const NOT_EMPTY: () = assert!(N > 0, "a ring holds at least one event");

    pub const fn new() -> Self {
        let () = Self::NOT_EMPTY;
        Ring { events: [const { None }; N], newest: 0 }
    }

    /// Add `event` as the newest, overwriting the oldest once the ring is
    /// full; the number it got.
    pub fn push(&mut self, event: E) -> Seq {
        self.newest += 1;
        self.events[self.slot(self.newest)] = Some(event);
        Seq(self.newest)
    }

    /// The first event still here numbered after `after`, with its number.
    pub fn next(&self, after: Seq) -> Option<(Seq, E)> {
        if after.0 >= self.newest {
            return None;
        }
        let n = (after.0 + 1).max(self.oldest());
        Some((Seq(n), self.get(n)))
    }

    /// The newest event's number ([`Seq::ZERO`] before the first).
    pub fn newest(&self) -> Seq {
        Seq(self.newest)
    }

    /// The newest `n` events still here (fewer if the ring has fewer),
    /// oldest first, with their numbers.
    pub fn recent(&self, n: usize) -> impl Iterator<Item = (Seq, E)> + '_ {
        let held = self.newest - (self.oldest() - 1);
        let first = self.newest - held.min(n as u64) + 1;
        (first..=self.newest).map(|k| (Seq(k), self.get(k)))
    }

    /// The oldest number still here (1 past `newest` when empty).
    fn oldest(&self) -> u64 {
        self.newest.saturating_sub(N as u64 - 1).max(1)
    }

    fn slot(&self, n: u64) -> usize {
        ((n - 1) % N as u64) as usize
    }

    fn get(&self, n: u64) -> E {
        self.events[self.slot(n)].expect("an event numbered within the ring is there")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::vec::Vec;

    use super::*;

    /// A device for the tests: a ring behind a lock, leaked to `'static`.
    struct Device<const N: usize>(Mutex<Ring<u32, N>>);

    impl<const N: usize> InputDevice for Device<N> {
        type Event = u32;
        fn next(&self, after: Seq) -> Option<(Seq, u32)> {
            self.0.lock().unwrap().next(after)
        }
        fn newest(&self) -> Seq {
            self.0.lock().unwrap().newest()
        }
    }

    fn device<const N: usize>() -> &'static Device<N> {
        Box::leak(Box::new(Device(Mutex::new(Ring::new()))))
    }

    fn push<const N: usize>(d: &Device<N>, e: u32) -> Seq {
        d.0.lock().unwrap().push(e)
    }

    fn ev<const N: usize>(r: &mut Reader<Device<N>>) -> Option<u32> {
        r.read().map(|read| read.event)
    }

    #[test]
    fn push_returns_the_seq_it_gave() {
        let mut ring = Ring::<u32, 2>::new();
        assert_eq!(ring.newest(), Seq::ZERO);
        assert_eq!(ring.push(10).get(), 1);
        assert_eq!(ring.push(11).get(), 2);
        assert_eq!(ring.push(12).get(), 3);
        assert_eq!(ring.newest().get(), 3);
    }

    #[test]
    fn events_come_in_order_and_none_after_the_newest() {
        let mut ring = Ring::<u32, 4>::new();
        assert_eq!(ring.next(Seq::ZERO), None);
        for e in 10..13 {
            ring.push(e);
        }
        assert_eq!(ring.next(Seq::ZERO), Some((Seq(1), 10)));
        assert_eq!(ring.next(Seq(1)), Some((Seq(2), 11)));
        assert_eq!(ring.next(Seq(2)), Some((Seq(3), 12)));
        assert_eq!(ring.next(Seq(3)), None);
        // An `after` past the newest finds nothing too.
        assert_eq!(ring.next(Seq(9)), None);
    }

    #[test]
    fn a_full_ring_overwrites_its_oldest() {
        let mut ring = Ring::<u32, 3>::new();
        for e in 1..=5 {
            ring.push(e * 10);
        }
        // Events 1 and 2 were overwritten: asking after 0 gives event 3.
        assert_eq!(ring.next(Seq::ZERO), Some((Seq(3), 30)));
        assert_eq!(ring.next(Seq(3)), Some((Seq(4), 40)));
        let recent: Vec<_> = ring.recent(2).collect();
        assert_eq!(recent, [(Seq(4), 40), (Seq(5), 50)]);
        let all: Vec<_> = ring.recent(99).collect();
        assert_eq!(all, [(Seq(3), 30), (Seq(4), 40), (Seq(5), 50)]);
        assert_eq!(Ring::<u32, 3>::new().recent(5).count(), 0);
    }

    #[test]
    fn a_reader_that_falls_behind_counts_the_gap() {
        let d = device::<3>();
        push(d, 1);
        let mut r = Reader::from_now(d);
        assert_eq!(r.after(), Seq(1));
        assert_eq!(ev(&mut r), None);
        for e in 2..=6 {
            push(d, e);
        }
        // 2 and 3 were overwritten (the ring holds 4, 5, 6): the read says
        // so with the event it comes before.
        assert_eq!(r.read(), Some(Read { seq: Seq(4), lost_before: 2, event: 4 }));
        assert_eq!(r.lost(), 2);
        assert_eq!(ev(&mut r), Some(5));
        assert_eq!(ev(&mut r), Some(6));
        assert_eq!(ev(&mut r), None);
        assert_eq!((r.after(), r.lost()), (Seq(6), 2));
    }

    #[test]
    fn two_readers_read_one_ring() {
        let d = device::<8>();
        let (mut a, mut b) = (Reader::from_now(d), Reader::from_now(d));
        push(d, 7);
        assert_eq!(ev(&mut a), Some(7));
        assert_eq!(ev(&mut b), Some(7));
        assert_eq!((a.lost(), b.lost()), (0, 0));
    }

    #[test]
    fn skip_to_never_goes_backwards_and_isnt_a_loss() {
        let d = device::<8>();
        let mut r = Reader::from_now(d);
        let seqs: Vec<Seq> = (0..5).map(|e| push(d, e)).collect();
        r.skip_to(seqs[2]);
        assert_eq!(ev(&mut r), Some(3));
        r.skip_to(seqs[0]);
        assert_eq!(r.after(), seqs[3]);
        assert_eq!(r.read(), Some(Read { seq: seqs[4], lost_before: 0, event: 4 }));
        assert_eq!(r.lost(), 0);
    }

    #[test]
    fn skip_to_stops_at_the_newest() {
        // A number from another ring, past this one's newest (a bug, which
        // a `debug_assert` catches outside these tests): the reader stops at
        // the newest and still gets the next event pushed.
        let other = device::<8>();
        let far = (0..5).map(|e| push(other, e)).last().unwrap();
        let d = device::<8>();
        push(d, 10);
        let mut r = Reader::from_now(d);
        r.skip_to(far);
        assert_eq!(r.after(), Seq(1));
        push(d, 11);
        assert_eq!(ev(&mut r), Some(11));
    }
}
