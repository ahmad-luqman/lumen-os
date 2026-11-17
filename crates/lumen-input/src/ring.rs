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
//!   counted in [`Reader::lost`]. That is the only loss a gap means:
//!   frames QEMU dropped at the device never reach the ring
//!   ([`AfterOverrun`] marks the frames after them instead).

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

    /// Skip to after `seq`, never backwards (the desktop's cut, section
    /// 11). Events skipped this way aren't counted as lost.
    pub fn skip_to(&mut self, seq: Seq) {
        self.after = self.after.max(seq);
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

/// A reader is an iterator over its device's events: `next` gives the next
/// event, counting a gap's events in [`Reader::lost`], and `None` when it
/// has caught up (a later event makes it give more).
impl<D: InputDevice + 'static> Iterator for Reader<D> {
    type Item = D::Event;

    fn next(&mut self) -> Option<D::Event> {
        let (seq, event) = self.device.next(self.after)?;
        self.lost += seq.0 - self.after.0 - 1;
        self.after = seq;
        Some(event)
    }
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

/// Marks the tablet's frames that may follow frames lost at the device
/// (`PointerEvent::after_overrun`): every frame of a batch that counted as
/// an overrun (`may_have_lost_frames`) except its first, and the first
/// frame of the next batch that brings one (virtio re-posts each buffer as
/// it is popped, so a frame can be dropped while the batch is still being
/// read, i.e. anywhere after its first frame, or just after its last).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AfterOverrun {
    /// The last batch with an overrun brought no frame after its loss yet.
    carry: bool,
}

impl AfterOverrun {
    pub const fn new() -> Self {
        Self { carry: false }
    }

    /// Whether each of a batch's `frames` follows a possible loss, in
    /// order, given whether the batch counted as an `overrun`; then
    /// remembers the batch for the next one.
    pub fn batch(&mut self, frames: usize, overrun: bool) -> impl Iterator<Item = bool> + use<> {
        let carry = self.carry;
        // A batch with no frame passes a loss on to the next frame.
        self.carry = overrun || (carry && frames == 0);
        (0..frames).map(move |i| if i == 0 { carry } else { overrun })
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
        assert_eq!(r.next(), None);
        for e in 2..=6 {
            push(d, e);
        }
        // 2 and 3 were overwritten (the ring holds 4, 5, 6).
        assert_eq!(r.next(), Some(4));
        assert_eq!(r.lost(), 2);
        assert_eq!(r.next(), Some(5));
        assert_eq!(r.next(), Some(6));
        assert_eq!(r.next(), None);
        assert_eq!((r.after(), r.lost()), (Seq(6), 2));
    }

    #[test]
    fn two_readers_read_one_ring() {
        let d = device::<8>();
        let (mut a, mut b) = (Reader::from_now(d), Reader::from_now(d));
        push(d, 7);
        assert_eq!(a.next(), Some(7));
        assert_eq!(b.next(), Some(7));
        assert_eq!((a.lost(), b.lost()), (0, 0));
    }

    #[test]
    fn skip_to_never_goes_backwards_and_isnt_a_loss() {
        let d = device::<8>();
        let mut r = Reader::from_now(d);
        let seqs: Vec<Seq> = (0..5).map(|e| push(d, e)).collect();
        r.skip_to(seqs[2]);
        assert_eq!(r.next(), Some(3));
        r.skip_to(seqs[0]);
        assert_eq!(r.after(), seqs[3]);
        assert_eq!(r.next(), Some(4));
        assert_eq!(r.lost(), 0);
    }

    #[test]
    fn after_overrun_marks_an_overrun_batch_after_its_first_and_the_next_batch_s_first() {
        let mut marks = AfterOverrun::new();
        let batch = |m: &mut AfterOverrun, frames, overrun| m.batch(frames, overrun).collect::<Vec<_>>();
        assert_eq!(batch(&mut marks, 2, false), [false, false]);
        assert_eq!(batch(&mut marks, 3, true), [false, true, true]);
        assert_eq!(batch(&mut marks, 2, false), [true, false]);
        assert_eq!(batch(&mut marks, 1, false), [false]);
        // An overrun batch with no frame passes the mark on, across empty
        // batches, to the next frame.
        assert_eq!(batch(&mut marks, 0, true), Vec::<bool>::new());
        assert_eq!(batch(&mut marks, 0, false), Vec::<bool>::new());
        assert_eq!(batch(&mut marks, 2, false), [true, false]);
        // Back-to-back overruns: the second's first frame follows the
        // first's loss.
        assert_eq!(batch(&mut marks, 2, true), [false, true]);
        assert_eq!(batch(&mut marks, 2, true), [true, true]);
        assert_eq!(batch(&mut marks, 1, false), [true]);
        assert_eq!(batch(&mut marks, 1, false), [false]);
    }
}
