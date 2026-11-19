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
//! While it waits, [`Silence`] notices a CPU whose timer stopped ticking
//! while the others' went on (judged by progress, not time), and
//! [`header`] says how many samples each CPU took. [`FnIndex`] finds the
//! function an address is in by binary search, for symbolizing.

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
/// waits. The state is one word: an arming generation (the top
/// [`GEN_BITS`] bits), `want` (samples it is armed for) and `len` (taken so
/// far), [`LEN_BITS`] bits each. The writer stores a sample's words into
/// slot `len`, then moves the state from `len` to `len + 1` by
/// compare-and-swap (Release), so a reader that sees `len` (Acquire) sees
/// every slot below it. [`arm`](Self::arm) and [`disarm`](Self::disarm)
/// store a new state outright, with the next generation; a writer that
/// raced them fails its compare-and-swap, and the slot it wrote is at or
/// past the new `len`, so no reader takes it before the writer writes it
/// again (the writer's records are serialized: one interrupt at a time on
/// one CPU, so that rewrite comes before the slot is published). The
/// generation is what makes the swap fail: without it, a disarm and a
/// re-arm for the same count between a writer's load and its swap would
/// bring back the exact state it loaded. A raced swap could still succeed
/// only if a multiple of 2^[`GEN_BITS`] armings came between the writer's
/// load and its swap, a few instructions with interrupts off.
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
    /// Room for `N` must fit the state's `want` and `len` fields.
    const FITS: () = assert!(N > 0 && N <= LEN_MASK as usize);

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
        self.rearm(want);
        want
    }

    /// Stop taking samples (what it held is dropped too).
    pub fn disarm(&self) {
        self.rearm(0);
    }

    /// Store a fresh state for `want` samples, in the next generation. For
    /// the one arming reader (a load then a store: two readers at once
    /// could reuse a generation).
    fn rearm(&self, want: usize) {
        let generation = (self.state.load(Ordering::Relaxed) >> GEN_SHIFT).wrapping_add(1);
        self.state.store(generation << GEN_SHIFT | (want as u64) << LEN_BITS, Ordering::Release);
    }

    /// Record `sample` if armed and not yet full: whether it was taken. For
    /// the buffer's one writer only.
    pub fn record(&self, sample: Sample) -> bool {
        self.claim().is_some_and(|state| self.publish(state, sample))
    }

    /// The writer's first step: the state it will write under, if armed and
    /// not yet full. Acquire, pairing with the arming's Release store: the
    /// reader's copy of the previous arming, made before it re-armed,
    /// happens before this arming's slot writes, so it never reads one of
    /// them.
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

/// The bits of a state word's `want` and of its `len`.
pub const LEN_BITS: u32 = 20;
/// The bits of a state word's arming generation (the rest).
pub const GEN_BITS: u32 = 64 - 2 * LEN_BITS;
const GEN_SHIFT: u32 = 2 * LEN_BITS;
const LEN_MASK: u64 = (1 << LEN_BITS) - 1;

/// A state word's `(want, len)`.
fn split(state: u64) -> (usize, usize) {
    (((state >> LEN_BITS) & LEN_MASK) as usize, (state & LEN_MASK) as usize)
}

/// One row of a ranking: a key (a function, a thread) and its samples.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row<K> {
    pub key: K,
    pub samples: u64,
}

/// Samples ranked by some key, idle ones counted apart. Made only by
/// [`by_function`] and [`by_thread`], so its idle samples never outnumber
/// its samples.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ranking<K> {
    samples: u64,
    idle: u64,
    rows: Vec<Row<K>>,
}

impl<K> Ranking<K> {
    /// Every sample, idle or not.
    pub fn samples(&self) -> u64 {
        self.samples
    }

    /// The samples that found a CPU's idle thread running (in no row).
    pub fn idle(&self) -> u64 {
        self.idle
    }

    /// The busy samples by key, most first; ties in key order.
    pub fn rows(&self) -> &[Row<K>] {
        &self.rows
    }

    /// The samples that weren't idle: what the rows add up to.
    pub fn busy(&self) -> u64 {
        self.samples - self.idle
    }

    /// A row's share of the busy samples, in whole percent (rounded; 0 when
    /// there are none).
    pub fn share(&self, row: &Row<K>) -> u64 {
        lumen_time::percent(row.samples, self.busy())
    }
}

/// Rank the busy samples by the key of their instruction pointer, which
/// `key_of` gives (a function, say). `key_of` is called once per distinct
/// address, not per sample.
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

/// Whether every CPU has taken its `want` samples (true for no CPUs): the
/// end of a profile that wasn't stopped.
pub fn all_taken(counts: impl IntoIterator<Item = usize>, want: usize) -> bool {
    counts.into_iter().all(|n| n >= want)
}

/// How many polls in a row a CPU may take no tick while another CPU ticks
/// before [`Silence`] calls it silent (at `perf top`'s 10 ms polls, about
/// a second of the other CPUs' polls).
pub const SILENT_POLLS: u32 = 100;

/// Notices CPUs that have stopped taking timer interrupts while a profile
/// waits for their samples (one stuck with interrupts off, say): a CPU is
/// silent once it has taken no tick across [`SILENT_POLLS`] polls in a row
/// in each of which some other CPU took one, and it doesn't have its
/// samples yet. A poll where no CPU ticked (the whole guest stalled) counts
/// for nothing. It is a hint for a person, not a verdict: a host that stalls
/// one vCPU for that long (about a second of the others' polls) makes a
/// healthy CPU look silent.
#[derive(Clone, Debug)]
pub struct Silence {
    last: Vec<u64>,
    still: Vec<u32>,
    said: Vec<bool>,
}

impl Silence {
    /// From each CPU's timer interrupts so far (the baseline; the CPUs are
    /// the indices into the slices [`observe`](Self::observe) takes).
    pub fn new(baseline: &[u64]) -> Silence {
        let n = baseline.len();
        Silence { last: baseline.to_vec(), still: alloc::vec![0; n], said: alloc::vec![false; n] }
    }

    /// One poll: each CPU's timer interrupts so far and whether it has all
    /// its samples. The CPUs found silent at this poll (each said once).
    pub fn observe(&mut self, ticks: &[u64], done: &[bool]) -> Vec<usize> {
        assert!(ticks.len() == self.last.len() && done.len() == self.last.len());
        let moved: Vec<bool> = ticks.iter().zip(&self.last).map(|(t, l)| t != l).collect();
        let movers = moved.iter().filter(|&&m| m).count();
        self.last.copy_from_slice(ticks);
        let mut found = Vec::new();
        for (i, &m) in moved.iter().enumerate() {
            if m || done[i] {
                self.still[i] = 0;
            } else if movers > 0 {
                // Some other CPU ticked (this one didn't).
                self.still[i] += 1;
                if self.still[i] >= SILENT_POLLS && !self.said[i] {
                    self.said[i] = true;
                    found.push(i);
                }
            }
        }
        found
    }

    /// Every CPU found silent so far.
    pub fn silent(&self) -> Vec<usize> {
        self.said.iter().enumerate().filter(|&(_, &s)| s).map(|(i, _)| i).collect()
    }
}

/// What one CPU took in a profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Taken {
    pub cpu: usize,
    pub samples: usize,
}

/// `perf top`'s header line, from what each CPU took, the samples asked
/// for per CPU, the CPUs found silent and the idle samples among them all.
/// A profile where every CPU took `want` says `<want> samples per CPU`; any
/// other says what each took, marking the silent ones.
pub fn header(want: usize, taken: &[Taken], silent: &[usize], idle: u64) -> String {
    let total: u64 = taken.iter().map(|t| t.samples as u64).sum();
    let per_cpu = if all_taken(taken.iter().map(|t| t.samples), want) {
        alloc::format!("{want} samples per CPU")
    } else {
        let counts: Vec<String> = taken
            .iter()
            .map(|t| {
                let mark = if silent.contains(&t.cpu) { " (silent)" } else { "" };
                alloc::format!("cpu {}: {}{mark}", t.cpu, t.samples)
            })
            .collect();
        alloc::format!("stopped at {} of {want} samples per CPU", counts.join(", "))
    };
    let n = taken.len();
    let plural = if n == 1 { "" } else { "s" };
    let busy = total.saturating_sub(idle);
    alloc::format!("perf top: {per_cpu} on {n} CPU{plural}: {total} samples, {idle} idle, {busy} busy")
}

/// The functions of a symbol table, sorted by address, for finding the
/// one an address is in by binary search (a symbol table is a list to
/// scan: several thousand functions for the kernel).
///
/// Same answers as `lumen_elf::SymbolTable::lookup` where sized functions
/// don't overlap: the sized function containing the address, else the
/// zero-sized one at or below it nearest to it, the first in table order
/// among equal addresses. Where sized functions overlap, an address in
/// several of them resolves to the one starting nearest below it (a scan
/// takes the first in table order), and an address past an inner
/// function's end but inside an outer one resolves to the outer one, as a
/// scan does.
#[derive(Clone, Debug, Default)]
pub struct FnIndex<'a> {
    /// (start, size, name), size > 0, by start; equal starts in table order.
    sized: Vec<(u64, u64, &'a str)>,
    /// For each entry of `sized`, the index of the entry at or before it
    /// that ends furthest (the earliest such), for addresses past an inner
    /// function's end.
    furthest: Vec<usize>,
    /// (start, name) of the zero-sized ones, by start, one per address (the
    /// first in table order).
    zero_sized: Vec<(u64, &'a str)>,
}

impl<'a> FnIndex<'a> {
    /// From the functions' (start, size, name), in table order.
    pub fn new(functions: impl IntoIterator<Item = (u64, u64, &'a str)>) -> FnIndex<'a> {
        let mut index = FnIndex::default();
        for (start, size, name) in functions {
            if size == 0 {
                index.zero_sized.push((start, name));
            } else {
                index.sized.push((start, size, name));
            }
        }
        // Stable: equal starts keep table order (and the dedup keeps the
        // first of them).
        index.sized.sort_by_key(|f| f.0);
        index.zero_sized.sort_by_key(|f| f.0);
        index.zero_sized.dedup_by_key(|f| f.0);
        let end = |f: &(u64, u64, &str)| f.0.saturating_add(f.1);
        let mut best = 0;
        index.furthest = (0..index.sized.len())
            .map(|i| {
                if end(&index.sized[i]) > end(&index.sized[best]) {
                    best = i;
                }
                best
            })
            .collect();
        index
    }

    /// How many functions it holds (zero-sized ones at one address once).
    pub fn len(&self) -> usize {
        self.sized.len() + self.zero_sized.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The function `addr` is in and the offset into it.
    pub fn lookup(&self, addr: u64) -> Option<(&'a str, u64)> {
        let contains = |f: &(u64, u64, &str)| addr - f.0 < f.1;
        let end = self.sized.partition_point(|f| f.0 <= addr);
        if let Some(last) = end.checked_sub(1) {
            let start = self.sized[last].0;
            let first = self.sized.partition_point(|f| f.0 < start);
            if let Some(&(_, _, name)) = self.sized[first..end].iter().find(|f| contains(f)) {
                return Some((name, addr - start));
            }
            let outer = self.sized[self.furthest[last]];
            if contains(&outer) {
                return Some((outer.2, addr - outer.0));
            }
        }
        let end = self.zero_sized.partition_point(|f| f.0 <= addr);
        let &(start, name) = self.zero_sized.get(end.checked_sub(1)?)?;
        Some((name, addr - start))
    }
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
    fn a_disarm_and_rearm_for_the_same_count_still_drops_a_raced_record() {
        // arm(8), claim slot 0, disarm, arm(8) again: the
        // state's want and len are back where the claim saw them, and only
        // the generation tells the armings apart.
        let b: Buffer<8> = Buffer::new();
        b.arm(8);
        let claimed = b.claim().unwrap();
        b.disarm();
        b.arm(8);
        assert!(!b.publish(claimed, s(99, 99)), "the old arming's sample must not publish");
        assert_eq!(b.progress(), (0, 8));
        assert!(b.record(s(1, 1)));
        let mut out = Vec::new();
        b.copy_into(&mut out);
        assert_eq!(out, [s(1, 1)]);
    }

    #[test]
    fn generations_wrap_without_touching_want_or_len() {
        let b: Buffer<4> = Buffer::new();
        b.state.store(u64::MAX << GEN_SHIFT, Ordering::Relaxed);
        assert_eq!(b.arm(3), 3);
        assert_eq!(b.state.load(Ordering::Relaxed) >> GEN_SHIFT, 0, "wrapped");
        assert!(b.record(s(1, 1)));
        assert_eq!(b.progress(), (1, 3));
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
        let samples =
            [s(0x110, 5), s(0x120, 5), s(0x210, 6), idle(0x110), s(0x900, 7), s(0x150, 6), idle(0x5), s(0x110, 6)];
        let mut calls = 0;
        let r = by_function(&samples, |rip| {
            calls += 1;
            resolve(rip)
        });
        assert_eq!(calls, 5, "once per distinct busy address (repeats and idle ones skipped)");
        assert_eq!((r.samples, r.idle, r.busy()), (8, 2, 6));
        assert_eq!(
            r.rows,
            [Row { key: Some("a"), samples: 4 }, Row { key: None, samples: 1 }, Row { key: Some("b"), samples: 1 }]
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
    fn shares_dont_overflow_on_huge_counts() {
        let r = Ranking { samples: u64::MAX, idle: 0, rows: alloc::vec![Row { key: 0u8, samples: u64::MAX / 2 }] };
        assert_eq!(r.share(&r.rows[0]), 50);
    }

    #[test]
    fn all_taken_needs_every_cpu_at_its_count() {
        assert!(all_taken([3, 3], 3));
        assert!(!all_taken([3, 2], 3), "one sample short is not done");
        assert!(!all_taken([0], 1));
        assert!(all_taken([], 5));
    }

    #[test]
    fn a_cpu_is_silent_after_enough_polls_where_only_others_tick() {
        let (mut t0, t1) = (0u64, 7u64);
        let mut q = Silence::new(&[t0, t1]);
        for poll in 1..SILENT_POLLS {
            t0 += 1;
            assert!(q.observe(&[t0, t1], &[false, false]).is_empty(), "poll {poll}");
        }
        t0 += 1;
        assert_eq!(q.observe(&[t0, t1], &[false, false]), [1]);
        t0 += 1;
        assert!(q.observe(&[t0, t1], &[false, false]).is_empty(), "said once");
        assert_eq!(q.silent(), [1]);
    }

    #[test]
    fn a_stalled_guest_a_finished_cpu_or_a_tick_resets_silence() {
        let mut q = Silence::new(&[0, 0]);
        // Nobody ticks: the whole guest stalled, nothing counts.
        for _ in 0..2 * SILENT_POLLS {
            assert!(q.observe(&[0, 0], &[false, false]).is_empty());
        }
        // CPU 1 has all its samples: not silent, however long.
        let mut t0 = 0;
        for _ in 0..2 * SILENT_POLLS {
            t0 += 1;
            assert!(q.observe(&[t0, 0], &[false, true]).is_empty());
        }
        // One tick of CPU 1 just short of the limit starts it over.
        for _ in 0..SILENT_POLLS - 1 {
            t0 += 1;
            q.observe(&[t0, 0], &[false, false]);
        }
        t0 += 1;
        assert!(q.observe(&[t0, 1], &[false, false]).is_empty());
        for _ in 0..SILENT_POLLS - 1 {
            t0 += 1;
            assert!(q.observe(&[t0, 1], &[false, false]).is_empty());
        }
        assert!(q.silent().is_empty());
    }

    #[test]
    fn header_says_per_cpu_counts_unless_every_cpu_took_its_samples() {
        let t = |cpu, samples| Taken { cpu, samples };
        assert_eq!(
            header(500, &[t(0, 500), t(1, 500)], &[], 600),
            "perf top: 500 samples per CPU on 2 CPUs: 1000 samples, 600 idle, 400 busy"
        );
        assert_eq!(
            header(10, &[t(0, 10)], &[], 10),
            "perf top: 10 samples per CPU on 1 CPU: 10 samples, 10 idle, 0 busy"
        );
        assert_eq!(
            header(500, &[t(0, 500), t(1, 499)], &[], 0),
            "perf top: stopped at cpu 0: 500, cpu 1: 499 of 500 samples per CPU on 2 CPUs: 999 samples, 0 idle, 999 busy"
        );
        assert_eq!(
            header(500, &[t(0, 212), t(3, 0)], &[3], 200),
            "perf top: stopped at cpu 0: 212, cpu 3: 0 (silent) of 500 samples per CPU on 2 CPUs: 212 samples, 200 idle, 12 busy"
        );
    }

    /// `lumen_elf::SymbolTable::lookup`'s rule, as a scan.
    fn scan<'a>(fns: &[(u64, u64, &'a str)], addr: u64) -> Option<(&'a str, u64)> {
        let mut fallback: Option<(u64, &str)> = None;
        for &(start, size, name) in fns {
            if start > addr {
                continue;
            }
            if size != 0 {
                if addr - start < size {
                    return Some((name, addr - start));
                }
            } else if fallback.is_none_or(|f| start > f.0) {
                fallback = Some((start, name));
            }
        }
        fallback.map(|(start, name)| (name, addr - start))
    }

    #[test]
    fn fn_index_agrees_with_a_scan_where_functions_dont_overlap() {
        // Made-up functions in shuffled table order: sized ones with gaps,
        // zero-sized ones among them (two at one address), one sized and
        // one zero-sized at the same start.
        let fns = [
            (0x300, 0x40, "c"),
            (0x100, 0x80, "a"),
            (0x1f0, 0, "z1"),
            (0x200, 0x10, "b"),
            (0x1f0, 0, "z2"),
            (0x400, 0, "z3"),
            (0x300, 0, "z4"),
            (0x500, 0x1, "d"),
            // Two sized ones at one start: the first in table order that
            // contains the address.
            (0x600, 0x10, "s"),
            (0x600, 0x40, "t"),
        ];
        let index = FnIndex::new(fns);
        assert_eq!(index.len(), fns.len() - 1, "the two zero-sized ones at 0x1f0 kept once");
        for addr in 0..0x700 {
            assert_eq!(index.lookup(addr), scan(&fns, addr), "addr {addr:#x}");
        }
        assert_eq!(index.lookup(0x608), Some(("s", 8)));
        assert_eq!(index.lookup(0x618), Some(("t", 0x18)));
        assert_eq!(index.lookup(0x1f8), Some(("z1", 8)), "equal addresses: the first in table order");
        assert!(FnIndex::new([]).lookup(5).is_none());
    }

    #[test]
    fn fn_index_resolves_nested_functions() {
        // An inner function inside an outer one, listed inner first: inside
        // the inner one both find it; past its end, both find the outer.
        let fns = [(0x120, 0x10, "inner"), (0x100, 0x100, "outer"), (0x400, 0, "z")];
        let index = FnIndex::new(fns);
        for addr in 0x100..0x500 {
            assert_eq!(index.lookup(addr), scan(&fns, addr), "addr {addr:#x}");
        }
        assert_eq!(index.lookup(0x125), Some(("inner", 5)));
        assert_eq!(index.lookup(0x140), Some(("outer", 0x40)));
        // Outer listed first: inside the inner one the index takes the
        // nearest start (the scan, the first listed): the documented case.
        let fns = [(0x100, 0x100, "outer"), (0x120, 0x10, "inner")];
        assert_eq!(FnIndex::new(fns).lookup(0x125), Some(("inner", 5)));
        assert_eq!(scan(&fns, 0x125), Some(("outer", 0x25)));
    }

    #[test]
    fn fn_index_keeps_table_order_among_many_aliases() {
        // 40 sized aliases at each of two starts (sizes growing, so the
        // first listed that contains an address is the smallest that does)
        // and 40 zero-sized at each of two more, the starts interleaved in
        // table order: enough entries, out of order, that an unstable sort
        // would reorder equal starts.
        const NAMES: [&str; 40] = [
            "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9", "b0", "b1", "b2", "b3", "b4", "b5", "b6", "b7",
            "b8", "b9", "c0", "c1", "c2", "c3", "c4", "c5", "c6", "c7", "c8", "c9", "d0", "d1", "d2", "d3", "d4", "d5",
            "d6", "d7", "d8", "d9",
        ];
        let mut fns = Vec::new();
        for (i, name) in NAMES.iter().enumerate() {
            fns.push((0x3000, i as u64 + 1, *name));
            fns.push((0x1000, i as u64 + 1, *name));
            fns.push((0x4000, 0, *name));
            fns.push((0x2000, 0, *name));
        }
        let index = FnIndex::new(fns.iter().copied());
        for addr in 0x1000..0x4010 {
            assert_eq!(index.lookup(addr), scan(&fns, addr), "addr {addr:#x}");
        }
        assert_eq!(index.lookup(0x1005), Some(("a5", 5)));
        assert_eq!(index.lookup(0x3005), Some(("a5", 5)));
        assert_eq!(index.lookup(0x2005), Some(("a0", 5)));
    }

    #[test]
    fn fn_index_agrees_with_a_scan_on_random_tables() {
        // A small LCG: deterministic, no dependency.
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: u64| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (x >> 33) % n
        };
        let names = ["f0", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9"];
        for _ in 0..200 {
            // Non-overlapping sized functions laid out left to right, and
            // zero-sized ones anywhere; then shuffled.
            let mut fns = Vec::new();
            let mut at = next(16);
            for name in names {
                if next(3) == 0 {
                    fns.push((next(0x200), 0, name));
                } else {
                    let size = 1 + next(16);
                    fns.push((at, size, name));
                    at += size + next(8);
                }
            }
            for i in (1..fns.len()).rev() {
                fns.swap(i, next(i as u64 + 1) as usize);
            }
            let index = FnIndex::new(fns.iter().copied());
            for addr in 0..0x220 {
                assert_eq!(index.lookup(addr), scan(&fns, addr), "addr {addr:#x} in {fns:?}");
            }
        }
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
