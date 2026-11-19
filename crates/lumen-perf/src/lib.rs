//! The logic behind `perf top` (#205), the kernel's sampling profiler.
//!
//! The kernel's local APIC timer interrupt, on each CPU, records what it
//! interrupted (a [`Sample`]: the instruction pointer, the running thread,
//! whether that was the CPU's idle thread) into that CPU's [`Buffer`] while
//! a profile runs. `perf top` then ranks the samples by function
//! ([`by_function`]) or by thread ([`by_thread`]), idle samples counted
//! apart.
//!
//! A profile is a number of samples per CPU, never a duration: the buffer
//! takes exactly as many as it was armed for and then refuses the rest, so
//! a loaded host that delays ticks makes a profile slower, never smaller.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

/// What one timer interrupt found running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// The interrupted instruction pointer.
    pub rip: u64,
    /// The running thread's id (0: none yet, before the CPU's threads are
    /// set up).
    pub thread: u64,
    /// Whether the running thread was the CPU's idle thread.
    pub idle: bool,
}

/// The idle flag's bit in a stored thread word (thread ids never get near
/// it: they count up from 1).
const IDLE_BIT: u64 = 1 << 63;

/// A buffer of up to `N` samples, filled by one writer (a CPU's timer
/// interrupt) while it is armed, and read by a thread.
///
/// Lock-free and wait-free for the writer: [`record`](Self::record) never
/// waits. The state is one word, `want << 32 | len`: armed for `want`
/// samples, `len` taken so far. The writer stores a sample's words into
/// slot `len`, then moves the state from `(want, len)` to `(want, len + 1)`
/// by compare-and-swap (Release), so a reader that sees `len` (Acquire)
/// sees every slot below it. [`arm`](Self::arm) and [`disarm`](Self::disarm)
/// store a new state outright; a writer that raced them fails its
/// compare-and-swap, and the slot it wrote is at or past the new `len`,
/// so no reader takes it before the writer writes it again (the writer's
/// records are serialized: one interrupt at a time on one CPU, so that
/// rewrite comes before the slot is published). The state can't come back
/// to the value the writer read before its swap: a new arming starts at
/// `len` 0, and only this writer moves `len` up.
///
/// Callers keep to one writer at a time per buffer and one arming reader at
/// a time; misuse can mix samples up but is never undefined behaviour (all
/// of it is atomics).
pub struct Buffer<const N: usize> {
    state: AtomicU64,
    rips: [AtomicU64; N],
    threads: [AtomicU64; N],
}

impl<const N: usize> Default for Buffer<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Buffer<N> {
    /// Room for `N` must fit the state's 32-bit halves.
    const FITS: () = assert!(N > 0 && N <= u32::MAX as usize);

    pub const fn new() -> Self {
        let () = Self::FITS;
        Buffer {
            state: AtomicU64::new(0),
            rips: [const { AtomicU64::new(0) }; N],
            threads: [const { AtomicU64::new(0) }; N],
        }
    }

    /// The most samples it can be armed for.
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Start taking up to `want` samples (at most `N`), dropping whatever it
    /// held: how many it will take.
    pub fn arm(&self, want: usize) -> usize {
        let want = want.min(N);
        self.state.store((want as u64) << 32, Ordering::Release);
        want
    }

    /// Stop taking samples (what it held is dropped too).
    pub fn disarm(&self) {
        self.state.store(0, Ordering::Release);
    }

    /// Record `sample` if armed and not yet full: whether it was taken. For
    /// the buffer's one writer only.
    pub fn record(&self, sample: Sample) -> bool {
        self.claim().is_some_and(|state| self.publish(state, sample))
    }

    /// The writer's first step: the state it will write under, if armed and
    /// not yet full.
    fn claim(&self) -> Option<u64> {
        let state = self.state.load(Ordering::Acquire);
        let (want, len) = split(state);
        (len < want).then_some(state)
    }

    /// The writer's second step: store `sample` in the slot `state` names,
    /// then publish it, unless the state changed since (re-armed or
    /// disarmed): whether it was published.
    fn publish(&self, state: u64, sample: Sample) -> bool {
        let (_, len) = split(state);
        let thread = sample.thread & !IDLE_BIT | if sample.idle { IDLE_BIT } else { 0 };
        self.rips[len].store(sample.rip, Ordering::Relaxed);
        self.threads[len].store(thread, Ordering::Relaxed);
        self.state.compare_exchange(state, state + 1, Ordering::Release, Ordering::Relaxed).is_ok()
    }

    /// `(taken, wanted)`: the samples taken so far and how many it was armed
    /// for (`(0, 0)` disarmed).
    pub fn progress(&self) -> (usize, usize) {
        let (want, len) = split(self.state.load(Ordering::Acquire));
        (len, want)
    }

    /// Append every sample taken so far to `out`. Safe while the writer
    /// still records: only published slots are read.
    pub fn copy_into(&self, out: &mut Vec<Sample>) {
        let (len, _) = self.progress();
        out.extend((0..len).map(|i| {
            let thread = self.threads[i].load(Ordering::Relaxed);
            Sample {
                rip: self.rips[i].load(Ordering::Relaxed),
                thread: thread & !IDLE_BIT,
                idle: thread & IDLE_BIT != 0,
            }
        }));
    }
}

/// A state word's `(want, len)`.
fn split(state: u64) -> (usize, usize) {
    ((state >> 32) as usize, (state & u64::from(u32::MAX)) as usize)
}

/// One row of a ranking: a key (a function, a thread) and its samples.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row<K> {
    pub key: K,
    pub samples: u64,
}

/// Samples ranked by some key, idle ones counted apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ranking<K> {
    /// Every sample, idle or not.
    pub samples: u64,
    /// The samples that found a CPU's idle thread running (in no row).
    pub idle: u64,
    /// The other samples by key, most first; ties in key order.
    pub rows: Vec<Row<K>>,
}

impl<K> Ranking<K> {
    /// The samples that weren't idle: what the rows add up to.
    pub fn busy(&self) -> u64 {
        self.samples - self.idle
    }

    /// A row's share of the busy samples, in whole percent (rounded; 0 when
    /// there are none).
    pub fn share(&self, row: &Row<K>) -> u64 {
        let busy = self.busy();
        if busy == 0 {
            return 0;
        }
        (u128::from(row.samples.min(busy)) * 100 + u128::from(busy) / 2) as u64 / busy
    }
}

/// Rank the busy samples by the key of their instruction pointer, which
/// `key_of` gives (a function, say: its symbol's name, or `None` where
/// nothing resolves). `key_of` is called once per distinct address, not per
/// sample (symbol lookup is a scan).
pub fn by_function<K: Ord>(samples: &[Sample], mut key_of: impl FnMut(u64) -> K) -> Ranking<K> {
    let mut by_rip: BTreeMap<u64, u64> = BTreeMap::new();
    for s in samples.iter().filter(|s| !s.idle) {
        *by_rip.entry(s.rip).or_default() += 1;
    }
    let mut by_key: BTreeMap<K, u64> = BTreeMap::new();
    for (rip, n) in by_rip {
        *by_key.entry(key_of(rip)).or_default() += n;
    }
    ranked(samples, by_key)
}

/// Rank the busy samples by thread id.
pub fn by_thread(samples: &[Sample]) -> Ranking<u64> {
    let mut by_key: BTreeMap<u64, u64> = BTreeMap::new();
    for s in samples.iter().filter(|s| !s.idle) {
        *by_key.entry(s.thread).or_default() += 1;
    }
    ranked(samples, by_key)
}

fn ranked<K: Ord>(samples: &[Sample], counts: BTreeMap<K, u64>) -> Ranking<K> {
    let idle = samples.iter().filter(|s| s.idle).count() as u64;
    let mut rows: Vec<Row<K>> = counts.into_iter().map(|(key, samples)| Row { key, samples }).collect();
    // Stable: equal counts keep the map's key order.
    rows.sort_by_key(|r| core::cmp::Reverse(r.samples));
    Ranking { samples: samples.len() as u64, idle, rows }
}

/// `perf top`'s default rows shown.
pub const DEFAULT_ROWS: usize = 10;
/// `perf top`'s default samples per CPU.
pub const DEFAULT_SAMPLES: usize = 1000;

/// `perf top [n] [--samples N] [--threads]`, parsed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TopArgs {
    /// Rows to show.
    pub rows: usize,
    /// Samples to take per CPU.
    pub samples: usize,
    /// Rank by thread instead of by function.
    pub threads: bool,
}

/// Parse `perf top`'s arguments (after `top`). `max_samples` is the
/// buffer's capacity: more is refused, not clamped, so the count asked for
/// is the count taken.
pub fn parse_top<S: AsRef<str>>(args: &[S], max_samples: usize) -> Result<TopArgs, String> {
    let mut top = TopArgs { rows: DEFAULT_ROWS, samples: DEFAULT_SAMPLES.min(max_samples), threads: false };
    let mut rows_seen = false;
    let mut it = args.iter().map(AsRef::as_ref);
    while let Some(arg) = it.next() {
        match arg {
            "--threads" => top.threads = true,
            "--samples" => {
                let n = it.next().ok_or_else(|| "--samples needs a count".to_string())?;
                top.samples = match n.parse::<usize>() {
                    Ok(n @ 1..) if n <= max_samples => n,
                    _ => return Err(alloc::format!("--samples takes 1 to {max_samples}, not {n:?}")),
                };
            }
            n if !rows_seen && !n.starts_with('-') => {
                top.rows = match n.parse::<usize>() {
                    Ok(n @ 1..) => n,
                    _ => return Err(alloc::format!("not a row count: {n:?}")),
                };
                rows_seen = true;
            }
            other => return Err(alloc::format!("unexpected argument {other:?}")),
        }
    }
    Ok(top)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    fn s(rip: u64, thread: u64) -> Sample {
        Sample { rip, thread, idle: false }
    }

    fn idle(rip: u64) -> Sample {
        Sample { rip, thread: 1, idle: true }
    }

    #[test]
    fn buffer_takes_exactly_what_it_was_armed_for() {
        let b: Buffer<8> = Buffer::new();
        assert!(!b.record(s(1, 1)), "disarmed: refused");
        assert_eq!(b.arm(3), 3);
        assert!(b.record(s(10, 2)) && b.record(idle(11)) && b.record(s(12, 3)));
        assert!(!b.record(s(13, 4)), "full: refused");
        assert_eq!(b.progress(), (3, 3));
        let mut out = Vec::new();
        b.copy_into(&mut out);
        assert_eq!(out, [s(10, 2), idle(11), s(12, 3)]);
    }

    #[test]
    fn arming_clamps_to_capacity_and_starts_over() {
        let b: Buffer<4> = Buffer::new();
        assert_eq!(b.capacity(), 4);
        assert_eq!(b.arm(100), 4);
        assert!(b.record(s(1, 1)));
        assert_eq!(b.arm(2), 2);
        assert_eq!(b.progress(), (0, 2), "a new arming drops what was held");
        b.disarm();
        assert_eq!(b.progress(), (0, 0));
        assert!(!b.record(s(1, 1)));
    }

    #[test]
    fn a_huge_thread_id_doesnt_read_back_as_idle() {
        let b: Buffer<2> = Buffer::new();
        b.arm(2);
        assert!(b.record(Sample { rip: 1, thread: u64::MAX, idle: false }));
        let mut out = Vec::new();
        b.copy_into(&mut out);
        assert!(!out[0].idle && out[0].thread == !IDLE_BIT);
    }

    #[test]
    fn copy_while_recording_sees_only_published_samples() {
        // A writer thread records samples whose two words agree (rip ==
        // thread); a reader copies concurrently: every sample it sees must
        // be whole, and the counts must only grow up to the arming.
        const N: usize = 4096;
        let b: Arc<Buffer<N>> = Arc::new(Buffer::new());
        b.arm(N);
        let done = Arc::new(AtomicBool::new(false));
        let writer = {
            let (b, done) = (b.clone(), done.clone());
            std::thread::spawn(move || {
                for i in 1..=N as u64 {
                    assert!(b.record(s(i, i)));
                }
                done.store(true, Ordering::Release);
            })
        };
        let mut last = 0;
        loop {
            let finished = done.load(Ordering::Acquire);
            let mut out = Vec::new();
            b.copy_into(&mut out);
            assert!(out.len() >= last);
            last = out.len();
            for (i, x) in out.iter().enumerate() {
                assert_eq!((x.rip, x.thread), (i as u64 + 1, i as u64 + 1));
            }
            if finished {
                break;
            }
        }
        writer.join().unwrap();
        assert_eq!(b.progress(), (N, N));
    }

    #[test]
    fn a_record_raced_by_rearming_is_dropped_and_its_slot_rewritten() {
        // The writer claims slot 2 of an arming, the reader re-arms (and
        // then disarms and re-arms) before the writer publishes: the
        // publish fails, the new arming holds nothing, and its slot 2 is
        // published only after the writer writes it again.
        let b: Buffer<8> = Buffer::new();
        b.arm(8);
        assert!(b.record(s(1, 1)) && b.record(s(2, 2)));
        let claimed = b.claim().expect("armed, not full");
        b.arm(8);
        assert!(!b.publish(claimed, s(99, 99)), "a stale claim must not publish");
        assert_eq!(b.progress(), (0, 8));
        let claimed = b.claim().unwrap();
        b.disarm();
        assert!(!b.publish(claimed, s(98, 98)));
        assert_eq!(b.progress(), (0, 0));
        b.arm(8);
        for i in 10..13 {
            assert!(b.record(s(i, i)));
        }
        let mut out = Vec::new();
        b.copy_into(&mut out);
        assert_eq!(out, [s(10, 10), s(11, 11), s(12, 12)], "slot 2 rewritten before it was published");
    }

    #[test]
    fn rearming_under_a_running_writer_keeps_counts_in_bounds() {
        // A writer records as fast as it can while the reader re-arms with
        // different sizes: whatever the interleaving, a reading never has
        // more samples than its arming, and every sample is whole.
        let b: Arc<Buffer<64>> = Arc::new(Buffer::new());
        let stop = Arc::new(AtomicBool::new(false));
        let writer = {
            let (b, stop) = (b.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    i += 1;
                    b.record(s(i, i));
                }
            })
        };
        for round in 0..5000usize {
            let want = 1 + round % 64;
            b.arm(want);
            let mut out = Vec::new();
            b.copy_into(&mut out);
            let (len, armed) = b.progress();
            assert!(armed == want && len <= want && out.len() <= want);
            assert!(out.iter().all(|x| x.rip == x.thread && !x.idle));
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
    }

    #[test]
    fn by_function_groups_addresses_and_counts_idle_apart() {
        // Functions: 0x100..0x200 is "a", 0x200..0x300 is "b"; 0x900 resolves
        // to nothing.
        let resolve = |rip: u64| match rip {
            0x100..0x200 => Some("a"),
            0x200..0x300 => Some("b"),
            _ => None,
        };
        let samples = [s(0x110, 5), s(0x120, 5), s(0x210, 6), idle(0x110), s(0x900, 7), s(0x150, 6), idle(0x5)];
        let mut calls = 0;
        let r = by_function(&samples, |rip| {
            calls += 1;
            resolve(rip)
        });
        assert_eq!(calls, 5, "once per distinct busy address (idle ones skipped)");
        assert_eq!((r.samples, r.idle, r.busy()), (7, 2, 5));
        assert_eq!(
            r.rows,
            [Row { key: Some("a"), samples: 3 }, Row { key: None, samples: 1 }, Row { key: Some("b"), samples: 1 }]
        );
        assert_eq!(r.rows.iter().map(|r| r.samples).sum::<u64>(), r.busy());
    }

    #[test]
    fn ties_rank_in_key_order() {
        let samples = [s(3, 30), s(1, 10), s(2, 20), s(1, 10), s(3, 30)];
        let r = by_thread(&samples);
        let order: Vec<(u64, u64)> = r.rows.iter().map(|r| (r.key, r.samples)).collect();
        assert_eq!(order, [(10, 2), (30, 2), (20, 1)]);
    }

    #[test]
    fn by_thread_counts_threads_and_leaves_idle_out() {
        let samples = [s(1, 4), s(2, 4), idle(3), s(4, 9)];
        let r = by_thread(&samples);
        assert_eq!(r.rows, [Row { key: 4, samples: 2 }, Row { key: 9, samples: 1 }]);
        assert_eq!((r.samples, r.idle), (4, 1));
    }

    #[test]
    fn all_idle_or_none_gives_no_rows() {
        let r = by_thread(&[idle(1), idle(2)]);
        assert!(r.rows.is_empty());
        assert_eq!((r.samples, r.idle, r.busy()), (2, 2, 0));
        let r = by_function(&[], |_| 0u8);
        assert_eq!((r.samples, r.idle, r.rows.len()), (0, 0, 0));
    }

    #[test]
    fn shares_round_to_whole_percent_of_busy_samples() {
        let samples = [s(1, 1), s(1, 1), s(2, 2), idle(0), idle(0)];
        let r = by_thread(&samples);
        assert_eq!(r.share(&r.rows[0]), 67);
        assert_eq!(r.share(&r.rows[1]), 33);
        let none = by_thread(&[idle(0)]);
        assert_eq!(none.share(&Row { key: 1, samples: 0 }), 0);
    }

    #[test]
    fn top_arguments() {
        let p = |a: &[&str]| parse_top(a, 2048);
        assert_eq!(p(&[]), Ok(TopArgs { rows: DEFAULT_ROWS, samples: DEFAULT_SAMPLES, threads: false }));
        assert_eq!(p(&["5"]), Ok(TopArgs { rows: 5, samples: DEFAULT_SAMPLES, threads: false }));
        assert_eq!(p(&["--samples", "200", "3", "--threads"]), Ok(TopArgs { rows: 3, samples: 200, threads: true }));
        assert_eq!(p(&["--samples", "2048"]).map(|t| t.samples), Ok(2048));
        assert!(p(&["--samples", "2049"]).is_err(), "past capacity: refused, not clamped");
        assert!(p(&["--samples", "0"]).is_err());
        assert!(p(&["--samples"]).is_err());
        assert!(p(&["0"]).is_err());
        assert!(p(&["3", "4"]).is_err());
        assert!(p(&["--bogus"]).is_err());
        assert_eq!(parse_top::<&str>(&[], 100).map(|t| t.samples), Ok(100), "the default fits the capacity");
    }
}
