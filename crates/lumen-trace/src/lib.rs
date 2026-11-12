//! Lock-free trace ring buffer for the Lumen kernel.
//!
//! The kernel records fixed-size binary [`Event`]s (timestamp, CPU, kind,
//! pid, four arguments) into a [`TraceRing`] that always overwrites the
//! oldest entries. Recording must be cheap and must never block: no locks,
//! no allocation, and safe to call from an exception handler that
//! interrupted another `record` on the same CPU.
//!
//! # How it works: a seqlock per slot
//!
//! Every event gets a sequence number `n` from one atomic counter
//! (`fetch_add`) and goes into slot `n % N`. Each slot has a stamp:
//!
//! - `0`: never written;
//! - `2n + 1`: event `n` is being written;
//! - `2n + 2`: event `n` is complete.
//!
//! A writer claims its slot by compare-and-swap from an even stamp older than
//! its own to `2n + 1`, stores the event's words, then publishes `2n + 2`. If
//! the slot is being written (odd stamp) or already holds a newer event, the
//! writer drops its event and counts it in [`TraceRing::dropped`] rather than
//! waiting: the slot's writer may be the very code this writer interrupted,
//! so waiting could deadlock.
//!
//! A reader of event `n` reads the stamp, copies the words, and reads the
//! stamp again. Unless both reads say `2n + 2`, the event was never written,
//! was dropped, was overwritten, or is still being written, and the reader
//! skips it. So a reader never returns a torn event, and never waits for a
//! writer either.
//!
//! Events are ordered by `seq`. Their timestamps usually are too, but not
//! always: if an interrupt (or another CPU's writer) records an event between
//! another writer reading its timestamp and claiming its `seq`, the two come
//! out in opposite orders. Consumers that need time order sort by `tsc`.
//!
//! The event words are atomics (relaxed loads and stores), which is what
//! makes the concurrent reads well-defined in Rust's memory model; on x86-64
//! they compile to plain moves.
//!
//! "Single producer per CPU" is the intended use (one ring per CPU), but
//! nothing here depends on it: concurrent writers on different CPUs are also
//! safe, which is how the host tests stress it.
//!
//! The protocol logic is tested on any host; the memory orderings are only
//! exercised on weakly ordered hardware (arm64). On x86-64 (TSO) the fences
//! cost nothing and a missing one is unlikely to show up in tests.
//!
//! This crate is `no_std` and has host-side tests (`cargo test -p lumen-trace`).

#![no_std]

use core::sync::atomic::{AtomicU64, Ordering, fence};

/// One trace event as recorded.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Event {
    /// 0-based sequence number among all events ever claimed (dropped events
    /// use numbers too, so recorded events can have gaps).
    pub seq: u64,
    /// Timestamp (raw TSC ticks in the kernel until the TSC is calibrated).
    pub tsc: u64,
    /// The CPU that recorded it.
    pub cpu: u16,
    /// What happened; the kernel defines the values.
    pub kind: u16,
    /// The process it happened in (0: the kernel).
    pub pid: u32,
    /// Meaning depends on `kind`.
    pub args: [u64; 4],
}

/// Why an event can't be read ([`TraceRing::try_read`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    /// No event with this sequence number has been claimed yet.
    NotClaimed,
    /// Its writer dropped it (the slot was busy), or has claimed the
    /// sequence number but not yet the slot.
    NotRecorded,
    /// Its writer is storing it right now (or was interrupted doing so).
    BeingWritten,
    /// A newer event has taken its slot.
    Overwritten,
}

const WORDS: usize = 6;

/// One slot: a stamp and the event packed into words.
struct Slot {
    stamp: AtomicU64,
    words: [AtomicU64; WORDS],
}

impl Slot {
    const fn new() -> Self {
        Self { stamp: AtomicU64::new(0), words: [const { AtomicU64::new(0) }; WORDS] }
    }
}

const fn writing(seq: u64) -> u64 {
    2 * seq + 1
}

const fn complete(seq: u64) -> u64 {
    2 * seq + 2
}

/// A ring of `N` event slots. Newer events overwrite the oldest. `N` must be
/// a power of two (so `seq % N` is a mask) and non-zero. Sequence numbers
/// are assumed never to reach 2^63 (the stamps are `2 * seq + 2`): centuries
/// at any realistic event rate.
pub struct TraceRing<const N: usize> {
    /// Sequence number of the next event (= events ever claimed).
    head: AtomicU64,
    dropped: AtomicU64,
    slots: [Slot; N],
}

impl<const N: usize> Default for TraceRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> TraceRing<N> {
    /// An empty ring; `const`, so it can be a `static`.
    pub const fn new() -> Self {
        const { assert!(N.is_power_of_two(), "TraceRing size must be a power of two") };
        Self { head: AtomicU64::new(0), dropped: AtomicU64::new(0), slots: [const { Slot::new() }; N] }
    }

    /// How many events the ring holds.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Record an event; returns its sequence number, or `None` if it was
    /// dropped because its slot was being written or already held a newer
    /// event (see the crate docs). Drops are counted in [`dropped`](Self::dropped).
    #[must_use = "None means the event was dropped (it is counted in dropped())"]
    pub fn record(&self, tsc: u64, cpu: u16, kind: u16, pid: u32, args: [u64; 4]) -> Option<u64> {
        let seq = self.head.fetch_add(1, Ordering::Relaxed);
        let slot = &self.slots[seq as usize % N];
        let mut stamp = slot.stamp.load(Ordering::Relaxed);
        loop {
            // Odd: someone (perhaps the code we interrupted) is writing it.
            // At or past our own stamp: a newer event already took it.
            if stamp & 1 == 1 || stamp >= writing(seq) {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            // Acquire: synchronises with the previous occupant's Release
            // publish, so our word stores are ordered after its stores
            // (otherwise a reader could see a mix of the two events).
            match slot.stamp.compare_exchange_weak(stamp, writing(seq), Ordering::Acquire, Ordering::Relaxed) {
                Ok(_) => break,
                Err(now) => stamp = now,
            }
        }
        // Keep the word stores after the odd stamp (seqlock writer).
        fence(Ordering::Release);
        let packed = cpu as u64 | (kind as u64) << 16 | (pid as u64) << 32;
        let words = [tsc, packed, args[0], args[1], args[2], args[3]];
        for (w, v) in slot.words.iter().zip(words) {
            w.store(v, Ordering::Relaxed);
        }
        slot.stamp.store(complete(seq), Ordering::Release);
        Some(seq)
    }

    /// Events ever claimed (recorded or dropped). The newest claimed sequence
    /// number is `written() - 1`; that event may still be in flight, or dropped.
    pub fn written(&self) -> u64 {
        self.head.load(Ordering::Acquire)
    }

    /// Events dropped because their slot was being written or already held a
    /// newer event.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Event `seq`, if it is still in the ring and was completely written;
    /// [`try_read`](Self::try_read) says why not.
    pub fn read(&self, seq: u64) -> Option<Event> {
        self.try_read(seq).ok()
    }

    /// Event `seq`, or why it can't be read. The reason comes from the
    /// slot's stamp, so it reflects the slot at the moment of reading. (A
    /// writer that dropped its event because the slot already held a newer
    /// one shows as `Overwritten`.)
    pub fn try_read(&self, seq: u64) -> Result<Event, Missing> {
        if seq >= self.written() {
            return Err(Missing::NotClaimed);
        }
        let slot = &self.slots[seq as usize % N];
        let stamp = slot.stamp.load(Ordering::Acquire);
        if stamp != complete(seq) {
            return Err(Self::why_missing(seq, stamp));
        }
        let mut w = [0; WORDS];
        for (dst, src) in w.iter_mut().zip(&slot.words) {
            *dst = src.load(Ordering::Relaxed);
        }
        // Keep the word loads before the second stamp read (seqlock reader).
        fence(Ordering::Acquire);
        let again = slot.stamp.load(Ordering::Relaxed);
        if again != complete(seq) {
            return Err(Self::why_missing(seq, again)); // overwritten while we copied
        }
        Ok(Event {
            seq,
            tsc: w[0],
            cpu: w[1] as u16,
            kind: (w[1] >> 16) as u16,
            pid: (w[1] >> 32) as u32,
            args: [w[2], w[3], w[4], w[5]],
        })
    }

    /// Why claimed event `seq` isn't readable, given its slot's stamp.
    fn why_missing(seq: u64, stamp: u64) -> Missing {
        if stamp == writing(seq) {
            Missing::BeingWritten
        } else if stamp > complete(seq) {
            Missing::Overwritten
        } else {
            // The slot still holds (or is writing) an older event: this
            // event's writer dropped it, or hasn't claimed the slot yet.
            Missing::NotRecorded
        }
    }

    /// Copy the newest events, oldest first, into `out`; returns how many
    /// were copied. Of the newest `min(out.len(), N)` sequence numbers, those
    /// dropped, overwritten or still being written are skipped (not replaced
    /// by older events), so the result may have gaps in `seq` but is always
    /// in order and never torn.
    pub fn snapshot(&self, out: &mut [Event]) -> usize {
        let head = self.written();
        let want = out.len().min(N) as u64;
        let mut copied = 0;
        for seq in head.saturating_sub(want)..head {
            if let Some(e) = self.read(seq) {
                out[copied] = e;
                copied += 1;
            }
        }
        copied
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::sync::Barrier;
    use std::sync::atomic::AtomicBool;
    use std::{thread, vec, vec::Vec};

    /// Arguments derived from one value, so a torn event is detectable.
    fn args_for(x: u64) -> [u64; 4] {
        [x, !x, x.wrapping_mul(0x9e37_79b9_7f4a_7c15), x ^ 0xdead_beef]
    }

    /// Sets the flag when dropped, so reader threads stop even if the writer
    /// panics on a failed assert (otherwise `thread::scope` waits forever
    /// and the test hangs instead of failing).
    struct StopOnDrop<'a>(&'a AtomicBool);

    impl Drop for StopOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    /// Record an event whose every field derives from `x` (see `consistent`).
    fn record_derived<const N: usize>(ring: &TraceRing<N>, x: u64, cpu: u16) -> Option<u64> {
        ring.record(x, cpu, (x >> 32) as u16, x as u32, args_for(x))
    }

    fn consistent(e: &Event) -> bool {
        e.args == args_for(e.tsc) && e.pid == e.tsc as u32 && e.kind == (e.tsc >> 32) as u16
    }

    #[test]
    fn records_and_reads_back_every_field() {
        let ring = TraceRing::<8>::new();
        assert_eq!(ring.capacity(), 8);
        assert_eq!(ring.read(0), None, "never written");
        assert_eq!(ring.record(123, 7, 0xbeef, 0xdead_cafe, [1, 2, 3, u64::MAX]), Some(0));
        let e = ring.read(0).unwrap();
        assert_eq!(e, Event { seq: 0, tsc: 123, cpu: 7, kind: 0xbeef, pid: 0xdead_cafe, args: [1, 2, 3, u64::MAX] });
        assert_eq!((ring.written(), ring.dropped()), (1, 0));
    }

    #[test]
    fn try_read_says_why_an_event_is_missing() {
        let ring = TraceRing::<4>::new();
        assert_eq!(ring.try_read(0), Err(Missing::NotClaimed));
        assert_eq!(ring.try_read(u64::MAX), Err(Missing::NotClaimed), "no overflow at the top");
        for i in 0..6u64 {
            let _ = ring.record(i, 0, 1, 0, [0; 4]);
        }
        assert_eq!(ring.try_read(1), Err(Missing::Overwritten), "event 5 took slot 1");
        assert_eq!(ring.try_read(5).map(|e| e.tsc), Ok(5));
        // Event 6 is claimed, but its writer is mid-write.
        ring.head.store(7, Ordering::Relaxed);
        ring.slots[2].stamp.store(writing(6), Ordering::Relaxed);
        assert_eq!(ring.try_read(6), Err(Missing::BeingWritten));
        // Event 7 was dropped: slot 3 still holds event 3.
        ring.head.store(8, Ordering::Relaxed);
        assert_eq!(ring.try_read(7), Err(Missing::NotRecorded));
        assert_eq!(ring.try_read(3), Ok(ring.read(3).unwrap()));
    }

    #[test]
    fn wraparound_keeps_the_newest_events_in_order() {
        let ring = TraceRing::<8>::new();
        for i in 0..20u64 {
            assert_eq!(ring.record(i, 0, 1, 0, args_for(i)), Some(i));
        }
        assert_eq!(ring.written(), 20);
        // 0..12 were overwritten; reading them gives nothing, not stale data.
        assert!((0..12).all(|s| ring.read(s).is_none()));
        assert_eq!(ring.read(20), None, "not written yet");
        let mut out = [Event::default(); 16];
        let n = ring.snapshot(&mut out);
        assert_eq!(n, 8, "at most the capacity");
        let seqs: Vec<u64> = out[..n].iter().map(|e| e.seq).collect();
        assert_eq!(seqs, (12..20).collect::<Vec<_>>());
        assert!(out[..n].iter().all(|e| e.tsc == e.seq && e.args == args_for(e.seq)));
    }

    #[test]
    fn snapshot_into_a_small_buffer_takes_the_newest() {
        let ring = TraceRing::<16>::new();
        for i in 0..10u64 {
            let _ = ring.record(i, 0, 0, 0, [0; 4]);
        }
        let mut out = [Event::default(); 3];
        assert_eq!(ring.snapshot(&mut out), 3);
        assert_eq!(out.map(|e| e.seq), [7, 8, 9]);
        let mut none: [Event; 0] = [];
        assert_eq!(ring.snapshot(&mut none), 0);
        assert_eq!(TraceRing::<4>::new().snapshot(&mut out), 0, "empty ring");
    }

    #[test]
    fn a_busy_slot_drops_the_new_event_instead_of_waiting() {
        // Simulate an interrupted writer: slot 1 is stuck mid-write.
        let ring = TraceRing::<4>::new();
        let _ = ring.record(0, 0, 0, 0, [0; 4]);
        ring.slots[1].stamp.store(writing(1), Ordering::Relaxed);
        ring.head.store(2, Ordering::Relaxed);
        // Events 2 and 3 land in empty slots, event 4 overwrites event 0 in
        // slot 0, and event 5 maps to the busy slot 1.
        assert_eq!(ring.record(2, 0, 0, 0, [0; 4]), Some(2));
        assert_eq!(ring.record(3, 0, 0, 0, [0; 4]), Some(3));
        assert_eq!(ring.record(4, 0, 0, 0, [0; 4]), Some(4));
        assert_eq!(ring.record(5, 0, 0, 0, [0; 4]), None);
        assert_eq!(ring.dropped(), 1);
        assert_eq!(ring.read(1), None, "an event being written is never read");
        assert_eq!(ring.read(5), None);
    }

    #[test]
    fn a_stale_writer_never_overwrites_a_newer_event() {
        // A writer that claimed seq 1 but was delayed past seq 5 (same slot
        // in a ring of 4) must drop its event, not clobber event 5.
        let ring = TraceRing::<4>::new();
        ring.head.store(5, Ordering::Relaxed);
        assert_eq!(ring.record(55, 0, 0, 0, [5; 4]), Some(5));
        ring.head.store(1, Ordering::Relaxed);
        assert_eq!(ring.record(11, 0, 0, 0, [1; 4]), None);
        ring.head.store(6, Ordering::Relaxed); // undo the test's rewind
        assert_eq!(ring.read(5).map(|e| e.tsc), Some(55));
    }

    /// One writer, several readers snapshotting concurrently: every event a
    /// reader sees is internally consistent (not torn), in strictly
    /// increasing `seq` order, and (one writer) in increasing `tsc` order.
    #[test]
    fn concurrent_readers_never_see_torn_events() {
        const EVENTS: u64 = 300_000;
        let ring = TraceRing::<64>::new();
        let done = AtomicBool::new(false);
        // Everyone starts together, or an optimized writer can finish before
        // the readers are scheduled (it did on CI) and nothing is tested.
        let start = Barrier::new(4);
        let seen = thread::scope(|s| {
            let readers: Vec<_> = (0..3)
                .map(|_| {
                    s.spawn(|| {
                        let mut out = vec![Event::default(); 64];
                        let mut seen = 0usize;
                        start.wait();
                        while !done.load(Ordering::Acquire) || seen == 0 {
                            let n = ring.snapshot(&mut out);
                            for pair in out[..n].windows(2) {
                                assert!(pair[0].seq < pair[1].seq, "out of order: {pair:?}");
                                assert!(pair[0].tsc < pair[1].tsc, "timestamps out of order: {pair:?}");
                            }
                            for e in &out[..n] {
                                assert!(consistent(e), "torn event: {e:?}");
                                assert_eq!(e.tsc, e.seq, "event filed under the wrong seq: {e:?}");
                            }
                            seen += n;
                        }
                        seen
                    })
                })
                .collect();
            start.wait();
            let stop = StopOnDrop(&done);
            for i in 0..EVENTS {
                let r = record_derived(&ring, i, 1);
                assert_eq!(r, Some(i), "a single writer never drops");
            }
            drop(stop);
            readers.into_iter().map(|r| r.join().unwrap()).sum::<usize>()
        });
        assert!(seen > 0, "readers saw events");
        assert_eq!(ring.written(), EVENTS);
    }

    /// Several writers at once (as on different CPUs sharing a ring; this
    /// approximates, but does not model, an interrupt nesting inside a
    /// record) plus a reader: the reader never sees a torn event. Once the
    /// writers stop, the newest events missing are at most the drop count (a
    /// coarse check; exact accounting is in
    /// `contended_slot_stamp_never_goes_backwards`).
    #[test]
    fn concurrent_writers_never_tear_events() {
        const PER_WRITER: u64 = 100_000;
        let ring = TraceRing::<32>::new();
        let done = AtomicBool::new(false);
        let start = Barrier::new(5);
        thread::scope(|s| {
            let reader = s.spawn(|| {
                let mut out = [Event::default(); 32];
                let mut seen = 0;
                start.wait();
                while !done.load(Ordering::Acquire) || seen == 0 {
                    let n = ring.snapshot(&mut out);
                    for e in &out[..n] {
                        assert!(consistent(e), "torn event: {e:?}");
                    }
                    assert!(out[..n].windows(2).all(|p| p[0].seq < p[1].seq));
                    seen += n;
                }
            });
            let writers: Vec<_> = (0..4u64)
                .map(|w| {
                    let (ring, start) = (&ring, &start);
                    s.spawn(move || {
                        start.wait();
                        for i in 0..PER_WRITER {
                            let x = w << 40 | i;
                            let _ = record_derived(ring, x, w as u16);
                        }
                    })
                })
                .collect();
            let stop = StopOnDrop(&done);
            for w in writers {
                w.join().unwrap();
            }
            drop(stop);
            reader.join().unwrap();
        });
        assert_eq!(ring.written(), 4 * PER_WRITER);
        // Once writers stop, each of the newest events is readable and
        // consistent, or was dropped (an older writer still held its slot).
        let head = ring.written();
        let newest: Vec<Event> = (head - 32..head).filter_map(|s| ring.read(s)).collect();
        assert!(newest.iter().all(consistent));
        assert!(32 - newest.len() as u64 <= ring.dropped(), "events missing without being counted");
    }

    /// Readers hammer the two hottest slots of a tiny ring while one writer
    /// overwrites them, so reads land inside writes far more often than with
    /// a big ring: still never torn, never filed under the wrong `seq`.
    #[test]
    fn hot_slot_readers_never_see_torn_or_misfiled_events() {
        const EVENTS: u64 = 1_000_000;
        let ring = TraceRing::<2>::new();
        let done = AtomicBool::new(false);
        let start = Barrier::new(4);
        thread::scope(|s| {
            for _ in 0..3 {
                s.spawn(|| {
                    start.wait();
                    while !done.load(Ordering::Relaxed) {
                        let head = ring.written();
                        for seq in head.saturating_sub(3)..=head {
                            if let Some(e) = ring.read(seq) {
                                assert!(consistent(&e), "torn event: {e:?}");
                                assert_eq!(e.tsc, e.seq, "event filed under the wrong seq: {e:?}");
                            }
                        }
                    }
                });
            }
            start.wait();
            let _stop = StopOnDrop(&done);
            for i in 0..EVENTS {
                assert_eq!(record_derived(&ring, i, 1), Some(i));
            }
        });
    }

    /// Writers fight over a single slot, so compare-and-swap failures (the
    /// stamp changed between the load and the claim) are frequent (the test
    /// asserts only that writers collided: `dropped() > 0`). The
    /// stamp must never go backwards (a stale writer overwriting a newer
    /// event), and every claimed event is accounted for: recorded or dropped.
    #[test]
    fn contended_slot_stamp_never_goes_backwards() {
        // Keep writing until the writers have collided enough, whatever the
        // scheduler does (on a small CI runner an optimized writer can finish
        // a fixed batch within one time slice and never overlap the others).
        const COLLISIONS: u64 = 100;
        const MAX_PER_WRITER: u64 = 20_000_000;
        let ring = TraceRing::<1>::new();
        let done = AtomicBool::new(false);
        let start = Barrier::new(5);
        let recorded = thread::scope(|s| {
            s.spawn(|| {
                let mut last = 0;
                start.wait();
                while !done.load(Ordering::Relaxed) {
                    let now = ring.slots[0].stamp.load(Ordering::Relaxed);
                    assert!(now >= last, "stamp went backwards: {last} -> {now}");
                    last = now;
                }
            });
            let writers: Vec<_> = (0..4u64)
                .map(|w| {
                    let (ring, start) = (&ring, &start);
                    s.spawn(move || {
                        start.wait();
                        let mut recorded = 0;
                        for i in 0..MAX_PER_WRITER {
                            if ring.dropped() >= COLLISIONS {
                                break;
                            }
                            recorded += record_derived(ring, w << 40 | i, w as u16).is_some() as u64;
                        }
                        recorded
                    })
                })
                .collect();
            let _stop = StopOnDrop(&done);
            writers.into_iter().map(|w| w.join().unwrap()).sum::<u64>()
        });
        assert_eq!(recorded + ring.dropped(), ring.written(), "every claimed event is recorded or dropped");
        assert!(ring.dropped() >= COLLISIONS, "the writers collided");
        if let Some(e) = ring.read(ring.written() - 1) {
            assert!(consistent(&e));
        }
    }
}
