//! `trace runq` and `trace xray`: X-ray's rings (#198) from the shell.
//!
//! - `trace xray <cpu> [since]`: CPU `cpu`'s X-ray events from sequence
//!   number `since` on (the newest 16 without it), read as the `Trace`
//!   capability reads them (`trace::read`): each number shown or counted
//!   missed, and where the next read would start.
//! - `trace runq`: each CPU's run-queue length as its X-ray ring's newest
//!   `runq` event says it and as the scheduler does, read together under
//!   that queue's lock (`sched::run_queue`).
//! - `trace runq park <n>`: the same for this CPU while it holds still:
//!   with its interrupts off (no tick, so nothing here switches or takes a
//!   thread off its queue), `n` threads are spawned here and wait on its
//!   queue; the queue before and after, the threads `threads` lists as
//!   ready here, and the queue's changes traced meanwhile are read, then
//!   interrupts come back on and the threads run and finish. A count of
//!   what is queued that nothing moves under it, for `test_xray`.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;

use lumen_trace::Event;
use lumen_trace::kind::{RunQueue, RunQueueOp};

use super::Ctx;
use crate::trace::{self, Stream};
use crate::{arch, sched};

/// The most threads `trace runq park` queues.
const MAX_PARKED: usize = 16;

/// `trace runq [park <n>]`.
pub fn runq(ctx: &mut Ctx<'_>, args: &[String]) {
    const USAGE: &str = "usage: trace runq [park <n>]";
    match args {
        [] => {
            let mut t = String::from("runq: each run queue's length, by its X-ray ring and by the scheduler\n");
            for cpu in (0..arch::MAX_CPUS).filter(|&c| sched::running_on(c)) {
                queue_line(&mut t, "", cpu);
            }
            let _ = ctx.out.write_str(&t);
        }
        [park, n] if park == "park" => match n.parse() {
            Ok(n) if (1..=MAX_PARKED).contains(&n) => match self::park(n) {
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
    let Some(view) = sched::run_queue(cpu) else { return };
    let len = view.len;
    match view.traced {
        Some((traced, seq)) => {
            let _ = writeln!(t, "  {label}cpu {cpu}: trace {traced}, scheduler {len} (event #{seq})");
        }
        None => {
            let _ = writeln!(t, "  {label}cpu {cpu}: trace -, scheduler {len} (no runq event in its ring)");
        }
    }
}

/// `trace runq park <n>`'s report, or why it couldn't park.
fn park(n: usize) -> Result<String, String> {
    let cpu = arch::cpu_index();
    let ring = trace::xray_ring(cpu).ok_or("no X-ray ring for this cpu")?;
    let mut t = String::new();
    let mut parked = Vec::with_capacity(n);
    let mut refused = None;
    {
        // Interrupts off, outside every critical section (a shell command
        // runs in none): no tick, so this CPU neither switches nor takes
        // anything off its queue until they are back on. Spawning (a stack,
        // the registry) and `threads` take locks and allocate, which works
        // with interrupts off; nothing here waits for an interrupt.
        let were_on = arch::irq::disable();
        let since = ring.written();
        queue_line(&mut t, "before: ", cpu);
        for _ in 0..n {
            match sched::spawn_on(cpu, "runq-park", || {}) {
                Ok(h) => parked.push(h),
                Err(e) => {
                    refused = Some(e);
                    break;
                }
            }
        }
        queue_line(&mut t, "after: ", cpu);
        let mut ready: Vec<u64> = sched::threads()
            .into_iter()
            .filter(|th| th.cpu == cpu && th.state == sched::State::Ready && th.name != "idle")
            .map(|th| th.id)
            .collect();
        ready.sort_unstable();
        write_ids(&mut t, &alloc::format!("  ready on cpu {cpu} (threads):"), &ready);
        let (enqueued, dequeued, missed) = changes_since(cpu, since);
        write_ids(&mut t, "  traced meanwhile: enqueued", &enqueued);
        write_ids(&mut t, "  traced meanwhile: dequeued", &dequeued);
        let _ = writeln!(t, "  traced meanwhile: missed {missed}");
        arch::irq::restore(were_on);
    }
    let mut head = alloc::format!("runq park: cpu {cpu} held, {} threads parked:", parked.len());
    for h in &parked {
        let _ = write!(head, " {}", h.thread().id());
    }
    head.push('\n');
    for h in parked {
        h.join();
    }
    if let Some(e) = refused {
        let _ = writeln!(t, "runq park: spawning stopped early: {e}");
    }
    let _ = writeln!(t, "runq park: released");
    head.push_str(&t);
    Ok(head)
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
/// queue from sequence number `since` on, and the numbers it missed: one
/// read of the whole ring.
fn changes_since(cpu: usize, since: u64) -> (Vec<u64>, Vec<u64>, u64) {
    let mut out = vec![Event::default(); trace::XRAY_EVENTS];
    let Ok(r) = trace::read(Stream::Xray, cpu, since, &mut out) else {
        return (Vec::new(), Vec::new(), 0);
    };
    let (mut enqueued, mut dequeued) = (Vec::new(), Vec::new());
    for rq in out[..r.copied].iter().filter_map(RunQueue::decode).filter(|rq| usize::from(rq.cpu) == cpu) {
        match rq.op {
            RunQueueOp::Enqueue => enqueued.push(rq.thread),
            RunQueueOp::Dequeue => dequeued.push(rq.thread),
        }
    }
    (enqueued, dequeued, r.missed)
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
        return ctx.fail(format_args!("trace xray: cpu {cpu}: {}", trace::ReadError::NoSuchCpu));
    };
    let first = since.unwrap_or_else(|| ring.written().saturating_sub(16));
    let mut out = vec![Event::default(); trace::XRAY_EVENTS];
    let r = match trace::read(Stream::Xray, cpu, first, &mut out) {
        Ok(r) => r,
        Err(e) => return ctx.fail(format_args!("trace xray: cpu {cpu}: {e}")),
    };
    let mut t = String::new();
    for e in &out[..r.copied] {
        let _ = writeln!(t, "{}", trace::Describe(e));
    }
    let claimed = ring.written();
    let _ = write!(
        t,
        "xray: cpu {cpu}, {} copied, {} missed, next {} (claimed {claimed}, dropped {})",
        r.copied,
        r.missed,
        r.next,
        ring.dropped()
    );
    if r.next < claimed {
        let _ = write!(t, "; more from #{} on", r.next);
    }
    t.push('\n');
    let _ = ctx.out.write_str(&t);
}
