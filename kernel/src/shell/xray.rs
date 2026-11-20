//! `trace runq` and `trace xray`: X-ray's rings (#198) from the shell.
//!
//! - `trace xray <cpu> [since]`: CPU `cpu`'s X-ray events from sequence
//!   number `since` on (the newest 16 without it), read as the `Trace`
//!   capability reads them (`trace::read`): each number shown or counted
//!   missed, where the next read would start, and why it stopped short of
//!   the head if it did.
//! - `trace runq`: each CPU's run-queue length as its X-ray ring's newest
//!   `runq` event says it and as the scheduler does, read together under
//!   that queue's lock (`sched::run_queue`); `MISMATCH` (and a failure) if
//!   they differ.
//! - `trace runq park <n>`: the same for this CPU while it holds still:
//!   with its interrupts off (no tick, so nothing here switches or takes a
//!   thread off its queue), `n` threads are spawned here and wait on its
//!   queue; the queue before and after, this CPU's scheduler ticks before
//!   and after (equal: the hold held), the threads `threads` lists as
//!   ready here, and the queue's changes traced between the two readings
//!   are read, then interrupts come back on and the threads run and
//!   finish. A count of what is queued that nothing moves under it, for
//!   `test_xray`.
//! - `trace runq spin <n>` / `trace runq spin stop`: `n` threads spinning
//!   on the last CPU with a scheduler until stopped (interrupts on, so the
//!   timer takes turns among them): a run queue that stays `n - 1` long,
//!   apart from what else wakes there, for as long as a test needs to read
//!   it (`test_xray::test_xray_matches_the_scheduler`). One set at a time.

use alloc::collections::TryReserveError;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use lumen_trace::kind::{RunQueue, RunQueueOp};
use lumen_trace::{Event, Missing, Stream};

use super::Ctx;
use crate::sync::SpinLock;
use crate::trace::{self, ReadError};
use crate::{arch, sched, time};

/// The most threads `trace runq park` queues.
const MAX_PARKED: usize = 16;
/// How long `park` holds its CPU at least, past the spawns: two timer
/// ticks, so a hold that didn't hold (interrupts on after all) would see a
/// tick and say so. A floor on the hold, not a bound on anything.
const HOLD_AT_LEAST_NS: u64 = 2_000_000;

/// `trace runq spin`'s threads while they spin: their CPU, the flag that
/// stops them, and their handles (empty while they are being started).
struct Spinning {
    cpu: usize,
    stop: Arc<AtomicBool>,
    threads: Vec<sched::JoinHandle>,
}

/// The one set of spinning threads, if any.
static SPINNING: SpinLock<Option<Spinning>> = SpinLock::new(None);

/// A buffer for one read of a whole ring, or why there is none (a 224 KiB
/// allocation: refused rather than aborting the kernel).
fn ring_buffer() -> Result<Vec<Event>, TryReserveError> {
    let mut out = Vec::new();
    out.try_reserve_exact(trace::XRAY_EVENTS)?;
    out.resize(trace::XRAY_EVENTS, Event::default());
    Ok(out)
}

/// `trace runq [park <n> | spin <n> | spin stop]`.
pub fn runq(ctx: &mut Ctx<'_>, args: &[String]) {
    const USAGE: &str = "usage: trace runq [park <n> | spin <n> | spin stop]";
    match args {
        [] => {
            let mut t = String::from("runq: each run queue's length, by its X-ray ring and by the scheduler\n");
            let mut agree = true;
            for cpu in (0..trace::cpus()).filter(|&c| sched::running_on(c)) {
                agree &= queue_line(&mut t, "", cpu);
            }
            let _ = ctx.out.write_str(&t);
            if !agree {
                ctx.fail("runq: the trace and the scheduler disagree");
            }
        }
        [park, n] if park == "park" => match n.parse() {
            Ok(n) if (1..=MAX_PARKED).contains(&n) => self::park(ctx, n),
            _ => ctx.fail(format_args!("{USAGE} (1 to {MAX_PARKED} threads)")),
        },
        [spin, stop] if spin == "spin" && stop == "stop" => spin_stop(ctx),
        [spin, n] if spin == "spin" => match n.parse() {
            Ok(n) if (1..=MAX_PARKED).contains(&n) => self::spin(ctx, n),
            _ => ctx.fail(format_args!("{USAGE} (1 to {MAX_PARKED} threads)")),
        },
        _ => ctx.fail(USAGE),
    }
}

/// `  <label>cpu C: trace T, scheduler S (event #N)` for a [`sched::QueueView`]
/// of CPU `cpu`'s queue (`-` if its ring holds no `runq` event); with
/// ` MISMATCH` when the two lengths differ. Whether they agree (no event and
/// an empty queue do).
fn write_view(t: &mut String, label: &str, cpu: usize, view: sched::QueueView) -> bool {
    let len = view.len;
    let agree = match view.traced {
        Some(traced) => {
            let _ = write!(t, "  {label}cpu {cpu}: trace {}, scheduler {len} (event #{})", traced.len, traced.seq);
            traced.len == len as u64
        }
        None => {
            let _ = write!(t, "  {label}cpu {cpu}: trace -, scheduler {len} (no runq event in its ring)");
            len == 0
        }
    };
    t.push_str(if agree { "\n" } else { " MISMATCH\n" });
    agree
}

/// [`write_view`] for CPU `cpu`'s queue as it is now.
fn queue_line(t: &mut String, label: &str, cpu: usize) -> bool {
    sched::run_queue(cpu).is_none_or(|view| write_view(t, label, cpu, view))
}

/// `trace runq park <n>`.
fn park(ctx: &mut Ctx<'_>, n: usize) {
    let cpu = arch::cpu_index();
    // Before the hold: nothing inside it may fail to allocate this.
    let Ok(mut out) = ring_buffer() else {
        return ctx.fail("runq park: no memory for the read buffer");
    };
    let mut t = String::new();
    let mut parked = Vec::with_capacity(n);
    let mut refused = None;
    let mut problems: Vec<String> = Vec::new();
    {
        // Interrupts off, outside every critical section (a shell command
        // runs in none): no tick, so this CPU neither switches nor takes
        // anything off its queue until they are back on. Spawning (a stack,
        // the registry) and `threads` take locks and allocate, which works
        // with interrupts off; nothing here waits for an interrupt.
        let were_on = arch::irq::disable();
        let ticks_before = sched::cpu_ticks(cpu).ticks;
        let before = sched::run_queue(cpu);
        for _ in 0..n {
            match sched::spawn_on(cpu, "runq-park", || {}) {
                Ok(h) => parked.push(h),
                Err(e) => {
                    refused = Some(e);
                    break;
                }
            }
        }
        time::spin(HOLD_AT_LEAST_NS);
        let after = sched::run_queue(cpu);
        let mut ready: Vec<u64> = sched::threads()
            .into_iter()
            .filter(|th| th.cpu == cpu && th.state == sched::State::Ready && th.name != "idle")
            .map(|th| th.id)
            .collect();
        let ticks_after = sched::cpu_ticks(cpu).ticks;
        arch::irq::restore(were_on);

        let (Some(before), Some(after)) = (before, after) else {
            return ctx.fail("runq park: no run queue for this cpu");
        };
        write_view(&mut t, "before: ", cpu, before);
        write_view(&mut t, "after: ", cpu, after);
        let _ = writeln!(t, "  ticks on cpu {cpu}: before {ticks_before}, after {ticks_after}");
        if ticks_before != ticks_after {
            problems.push(String::from("this cpu ticked during the hold"));
        }
        ready.sort_unstable();
        write_ids(&mut t, &alloc::format!("  ready on cpu {cpu} (threads):"), &ready);
        match changes_between(cpu, before.head, after.head, &mut out) {
            Ok(c) => {
                write_ids(&mut t, "  traced meanwhile: enqueued", &c.enqueued);
                write_ids(&mut t, "  traced meanwhile: dequeued", &c.dequeued);
                let _ = writeln!(t, "  traced meanwhile: missed {}", c.missed);
                if c.next < after.head {
                    problems.push(alloc::format!("the read stopped at #{} short of #{}", c.next, after.head));
                }
            }
            Err(e) => problems.push(alloc::format!("the X-ray ring: {e}")),
        }
    }
    let mut head = alloc::format!("runq park: cpu {cpu} held, {} threads parked:", parked.len());
    for h in &parked {
        let _ = write!(head, " {}", h.thread().id());
    }
    head.push('\n');
    for h in parked {
        h.join();
    }
    let _ = writeln!(t, "runq park: released");
    let _ = ctx.out.write_str(&head);
    let _ = ctx.out.write_str(&t);
    if let Some(e) = refused {
        problems.push(alloc::format!("spawning stopped early: {e}"));
    }
    if !problems.is_empty() {
        ctx.fail(format_args!("runq park: {}", problems.join("; ")));
    }
}

/// `trace runq spin <n>`: `n` threads spinning on the last CPU with a
/// scheduler until `trace runq spin stop`.
fn spin(ctx: &mut Ctx<'_>, n: usize) {
    let Some(cpu) = (0..arch::MAX_CPUS).rev().find(|&c| sched::running_on(c)) else {
        return ctx.fail("runq spin: no cpu runs a scheduler");
    };
    let stop = Arc::new(AtomicBool::new(false));
    // Claimed before any thread starts (a second shell's spin is refused
    // meanwhile), filled in once they have; the lock is never held while
    // printing.
    let busy = {
        let mut slot = SPINNING.lock();
        match slot.as_ref() {
            Some(s) => Some(s.cpu),
            None => {
                *slot = Some(Spinning { cpu, stop: stop.clone(), threads: Vec::new() });
                None
            }
        }
    };
    if let Some(on) = busy {
        return ctx.fail(format_args!("runq spin: already spinning on cpu {on}"));
    }
    let mut threads = Vec::with_capacity(n);
    let mut refused = None;
    for _ in 0..n {
        let flag = stop.clone();
        match sched::spawn_on(cpu, "runq-spin", move || {
            while !flag.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }) {
            Ok(h) => threads.push(h),
            Err(e) => {
                refused = Some(e);
                break;
            }
        }
    }
    if let Some(e) = refused {
        // Stop the ones that started, and give the slot back.
        stop.store(true, Ordering::Relaxed);
        for h in threads {
            h.join();
        }
        *SPINNING.lock() = None;
        return ctx.fail(format_args!("runq spin: spawning failed: {e}"));
    }
    let mut t = alloc::format!("runq spin: {n} threads spinning on cpu {cpu}:");
    for h in &threads {
        let _ = write!(t, " {}", h.thread().id());
    }
    t.push('\n');
    if let Some(s) = SPINNING.lock().as_mut() {
        s.threads = threads;
    }
    let _ = ctx.out.write_str(&t);
}

/// `trace runq spin stop`: the spinning threads told to stop, and joined.
fn spin_stop(ctx: &mut Ctx<'_>) {
    let taken = {
        let mut slot = SPINNING.lock();
        match slot.as_ref() {
            None => Err("runq spin: nothing is spinning"),
            // Still being started by another shell: left to it.
            Some(s) if s.threads.is_empty() => Err("runq spin: still starting"),
            Some(_) => slot.take().ok_or("runq spin: nothing is spinning"),
        }
    };
    let s = match taken {
        Ok(s) => s,
        Err(why) => return ctx.fail(why),
    };
    s.stop.store(true, Ordering::Relaxed);
    let n = s.threads.len();
    for h in s.threads {
        h.join();
    }
    let _ = writeln!(ctx.out, "runq spin: {n} threads stopped on cpu {}", s.cpu);
}

/// `<label> 1 2 3`, or `<label> none`, as a line.
fn write_ids(t: &mut String, label: &str, ids: &[u64]) {
    t.push_str(label);
    if ids.is_empty() {
        t.push_str(" none");
    }
    for id in ids {
        let _ = write!(t, " {id}");
    }
    t.push('\n');
}

/// The run-queue changes CPU `cpu`'s X-ray ring holds with sequence
/// numbers in `since..until`, read in one go into `out` (a whole ring).
struct Changes {
    enqueued: Vec<u64>,
    dequeued: Vec<u64>,
    missed: u64,
    /// Where the read stopped: `until` or more unless an event not yet
    /// written held it.
    next: u64,
}

fn changes_between(cpu: usize, since: u64, until: u64, out: &mut [Event]) -> Result<Changes, ReadError> {
    let r = trace::read(Stream::Xray, cpu, since, out)?;
    let mut c = Changes { enqueued: Vec::new(), dequeued: Vec::new(), missed: r.missed, next: r.next };
    // Only this CPU's queue records in its ring.
    for rq in out[..r.copied].iter().filter(|e| e.seq < until).filter_map(RunQueue::decode) {
        match rq.op {
            RunQueueOp::Enqueue => c.enqueued.push(rq.thread),
            RunQueueOp::Dequeue => c.dequeued.push(rq.thread),
        }
    }
    Ok(c)
}

/// `trace xray <cpu> [since]`: one read of the ring from `since` (the
/// newest 16 without it), up to its head or an event not yet written.
pub fn events(ctx: &mut Ctx<'_>, args: &[String]) {
    const USAGE: &str = "usage: trace xray <cpu> [since]";
    let parsed = match args {
        [cpu] => cpu.parse::<usize>().ok().map(|c| (c, None)),
        [cpu, since] => cpu.parse::<usize>().ok().zip(since.parse::<u64>().ok().map(Some)),
        _ => None,
    };
    let Some((cpu, since)) = parsed else {
        return ctx.fail(USAGE);
    };
    let Some(ring) = trace::xray_ring(cpu) else {
        return ctx.fail(format_args!("trace xray: cpu {cpu}: {}", ReadError::NoSuchCpu));
    };
    let Ok(mut out) = ring_buffer() else {
        return ctx.fail("trace xray: no memory for the read buffer");
    };
    // The head before the read: a read that ends short of it stopped at an
    // event not yet written (not at one recorded since).
    let head = ring.written();
    let first = since.unwrap_or_else(|| head.saturating_sub(16));
    let r = match trace::read(Stream::Xray, cpu, first, &mut out) {
        Ok(r) => r,
        Err(e) => return ctx.fail(format_args!("trace xray: cpu {cpu}: {e}")),
    };
    let mut t = String::new();
    for e in &out[..r.copied] {
        let _ = writeln!(t, "{}", trace::Describe(e));
    }
    let _ = write!(
        t,
        "xray: cpu {cpu}, {} copied, {} missed, next {} (claimed {head}, dropped {})",
        r.copied,
        r.missed,
        r.next,
        ring.dropped()
    );
    if r.next < head {
        let why = match ring.try_read(r.next) {
            Err(Missing::BeingWritten) => "being written",
            Err(Missing::NotRecorded) => "claimed, not yet written, or dropped",
            Ok(_) => "written since",
            Err(Missing::Overwritten | Missing::NotClaimed) => "gone since",
        };
        let _ = write!(t, "; stopped at #{} ({why})", r.next);
    }
    t.push('\n');
    let _ = ctx.out.write_str(&t);
}
