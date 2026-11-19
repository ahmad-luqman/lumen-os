//! `trace runq` and `trace xray`: X-ray's rings (#198) from the shell.
//!
//! - `trace xray <cpu> [since]`: CPU `cpu`'s X-ray events from sequence
//!   number `since` on (the newest 16 without it), read the way the
//!   `Trace` capability reads them (`trace::read_xray`): each number shown
//!   or counted missed, and where the next read would start.
//! - `trace runq`: each CPU's run-queue length as its X-ray ring's newest
//!   `runq` event says it and as the scheduler does, both read under that
//!   queue's lock (`sched::with_run_queue`).
//! - `trace runq park <cpu> <n>`: the same for one CPU while it holds still:
//!   a thread there turns its interrupts off (no tick, so nothing there
//!   runs or takes a thread off its queue), then `n` threads are spawned
//!   there, which wait on its queue; then the queue as both see it, the
//!   threads `threads` lists as ready there, and the queue's changes traced
//!   since the hold began; then the hold ends and the threads run and
//!   finish. A count of what is queued that doesn't move under it, for
//!   `test_xray`.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

use lumen_trace::Event;

use super::Ctx;
use crate::trace::{self, Kind};
use crate::{arch, sched, time};

/// The most threads `trace runq park` queues.
const MAX_PARKED: usize = 16;
/// How long the hold lasts at most, and how long the shell waits for it to
/// begin: a safety net (a step that never comes), never reached in a run
/// that works however slow the host.
const HOLD_LIMIT_NS: u64 = 10_000_000_000;
/// Events copied from a ring at a time.
const BATCH: usize = 256;

/// `trace runq [park <cpu> <n>]`.
pub fn runq(ctx: &mut Ctx<'_>, args: &[String]) {
    const USAGE: &str = "usage: trace runq [park <cpu> <n>]";
    match args {
        [] => {
            let mut t = String::from("runq: each run queue's length, by its X-ray ring and by the scheduler\n");
            for cpu in (0..arch::MAX_CPUS).filter(|&c| sched::running_on(c)) {
                queue_line(&mut t, "", cpu);
            }
            let _ = ctx.out.write_str(&t);
        }
        [park, cpu, n] if park == "park" => match (cpu.parse(), n.parse()) {
            (Ok(cpu), Ok(n)) if (1..=MAX_PARKED).contains(&n) => match self::park(cpu, n) {
                Ok(t) => {
                    let _ = ctx.out.write_str(&t);
                }
                Err(why) => ctx.fail(format_args!("runq park: {why}")),
            },
            _ => ctx.fail(format_args!("{USAGE} (1 to {MAX_PARKED} threads)")),
        },
        _ => ctx.fail(USAGE),
    }
}

/// `  <label>cpu C: trace T, scheduler S (event #N)`: CPU `cpu`'s queue as
/// its X-ray ring's newest `runq` event says (`-` if it holds none) and as
/// the scheduler does, read together under the queue's lock.
fn queue_line(t: &mut String, label: &str, cpu: usize) {
    let Some((len, traced)) = sched::with_run_queue(cpu, |len| (len, trace::xray_run_queue_len(cpu))) else {
        return;
    };
    match traced {
        Some((traced, seq)) => {
            let _ = writeln!(t, "  {label}cpu {cpu}: trace {traced}, scheduler {len} (event #{seq})");
        }
        None => {
            let _ = writeln!(t, "  {label}cpu {cpu}: trace -, scheduler {len} (no runq event in its ring)");
        }
    }
}

/// What `trace runq park` holds and signals.
#[derive(Default)]
struct Hold {
    /// The holder has turned its CPU's interrupts off.
    held: AtomicBool,
    /// The shell is done: let go.
    release: AtomicBool,
    /// The holder let go at [`HOLD_LIMIT_NS`] without being told to.
    timed_out: AtomicBool,
}

/// `trace runq park <cpu> <n>`'s report, or why it couldn't hold `cpu`.
fn park(cpu: usize, n: usize) -> Result<String, String> {
    if cpu == arch::cpu_index() {
        return Err(alloc::format!("cpu {cpu} runs this shell; hold another"));
    }
    if !sched::running_on(cpu) {
        return Err(alloc::format!("no scheduler runs on cpu {cpu}"));
    }
    let hold = Arc::new(Hold::default());
    let h = hold.clone();
    let holder = sched::spawn_on(cpu, "runq-hold", move || {
        // Interrupts off, outside every critical section: no tick, so this
        // CPU neither switches nor takes anything off its queue until the
        // shell lets go (or the limit passes).
        let were_on = arch::irq::disable();
        h.held.store(true, Ordering::Release);
        let limit = time::now_ns().saturating_add(HOLD_LIMIT_NS);
        while !h.release.load(Ordering::Acquire) {
            if time::now_ns() >= limit {
                h.timed_out.store(true, Ordering::Release);
                break;
            }
            core::hint::spin_loop();
        }
        arch::irq::restore(were_on);
    })
    .map_err(|e| alloc::format!("no holder for cpu {cpu}: {e}"))?;
    let wait_until = time::now_ns().saturating_add(HOLD_LIMIT_NS);
    while !hold.held.load(Ordering::Acquire) && time::now_ns() < wait_until {
        time::sleep(1_000_000);
    }
    if !hold.held.load(Ordering::Acquire) {
        hold.release.store(true, Ordering::Release);
        holder.join();
        return Err(alloc::format!("cpu {cpu} never ran its holder"));
    }
    let ring = trace::xray_ring(cpu).ok_or("no X-ray ring for that cpu")?;
    let since = ring.written();
    let mut before = String::new();
    queue_line(&mut before, "before: ", cpu);
    let mut parked = Vec::with_capacity(n);
    let mut refused = None;
    for _ in 0..n {
        match sched::spawn_on(cpu, "runq-park", || {}) {
            Ok(h) => parked.push(h),
            Err(e) => {
                refused = Some(e);
                break;
            }
        }
    }
    let ids: Vec<u64> = parked.iter().map(|h| h.thread().id()).collect();
    let mut t = String::new();
    let _ = write!(t, "runq park: cpu {cpu} held, {} threads parked:", ids.len());
    for id in &ids {
        let _ = write!(t, " {id}");
    }
    t.push('\n');
    t.push_str(&before);
    queue_line(&mut t, "after: ", cpu);
    let mut ready: Vec<u64> = sched::threads()
        .into_iter()
        .filter(|th| th.cpu == cpu && th.state == sched::State::Ready && th.name != "idle")
        .map(|th| th.id)
        .collect();
    ready.sort_unstable();
    write_ids(&mut t, &alloc::format!("  ready on cpu {cpu} (threads):"), &ready);
    let (enqueued, dequeued, missed) = changes_since(cpu, since);
    write_ids(&mut t, "  traced since the hold: enqueued", &enqueued);
    write_ids(&mut t, "  traced since the hold: dequeued", &dequeued);
    let _ = writeln!(t, "  traced since the hold: missed {missed}");
    let timed_out = hold.timed_out.load(Ordering::Acquire);
    hold.release.store(true, Ordering::Release);
    for h in parked {
        h.join();
    }
    holder.join();
    if let Some(e) = refused {
        let _ = writeln!(t, "runq park: spawning stopped early: {e}");
    }
    let how = if timed_out { "the hold ran out before the queue was read" } else { "held throughout" };
    let _ = writeln!(t, "runq park: released ({how})");
    Ok(t)
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

/// The threads CPU `cpu`'s X-ray ring has queued and dequeued on its run
/// queue from sequence number `since` on, and the numbers it missed.
fn changes_since(cpu: usize, since: u64) -> (Vec<u64>, Vec<u64>, u64) {
    let (mut enqueued, mut dequeued, mut missed) = (Vec::new(), Vec::new(), 0);
    let mut out = vec![Event::default(); BATCH];
    let mut next = since;
    while let Some(r) = trace::read_xray(cpu, next, &mut out) {
        missed += r.missed;
        for e in out[..r.copied].iter().filter(|e| e.kind == Kind::RunQueue as u16 && e.args[0] == cpu as u64) {
            match e.args[3] {
                trace::RUNQ_ENQUEUE => enqueued.push(e.args[2]),
                trace::RUNQ_DEQUEUE => dequeued.push(e.args[2]),
                _ => {}
            }
        }
        if r.next == next {
            break; // caught up (or at an event still being written)
        }
        next = r.next;
    }
    (enqueued, dequeued, missed)
}

/// `trace xray <cpu> [since]`.
pub fn events(ctx: &mut Ctx<'_>, args: &[String]) {
    const USAGE: &str = "usage: trace xray <cpu> [since]";
    let (cpu, since) = match args {
        [cpu] => (cpu.parse::<usize>().ok(), None),
        [cpu, since] => match since.parse::<u64>() {
            Ok(s) => (cpu.parse::<usize>().ok(), Some(s)),
            Err(_) => (None, None),
        },
        _ => (None, None),
    };
    let Some((cpu, ring)) = cpu.and_then(|c| Some((c, trace::xray_ring(c)?))) else {
        return ctx.fail(USAGE);
    };
    let first = since.unwrap_or_else(|| ring.written().saturating_sub(16));
    let mut t = String::new();
    let mut out = vec![Event::default(); BATCH];
    let (mut next, mut copied, mut missed) = (first, 0u64, 0u64);
    // At most a ring's worth: events recorded while this reads don't
    // keep it going.
    while copied + missed < trace::XRAY_EVENTS as u64 {
        let Some(r) = trace::read_xray(cpu, next, &mut out) else { break };
        for e in &out[..r.copied] {
            let _ = writeln!(t, "{}", trace::Describe(e));
        }
        copied += r.copied as u64;
        missed += r.missed;
        if r.next == next {
            break;
        }
        next = r.next;
    }
    let _ = writeln!(
        t,
        "xray: cpu {cpu}, {copied} copied, {missed} missed, next {next} (claimed {}, dropped {})",
        ring.written(),
        ring.dropped()
    );
    let _ = ctx.out.write_str(&t);
}
