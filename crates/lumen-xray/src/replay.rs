//! The fixed trace X-ray's frozen view shows (`run xray --replay`, its
//! golden): three CPUs, the middle one going offline (an index hole below
//! `trace_cpus`, as on a machine whose CPUs aren't numbered densely), read
//! column by column through [`View::read`] exactly as live reads are, so
//! the view it gives is what the same events read live would give.
//!
//! - **CPU 0**: a run queue rising and falling (0 to 4 and back, every 16
//!   columns), two switches a column among four threads, a few interrupts
//!   and a lock that waited every 13 columns.
//! - **CPU 1**: one thread queued and two switching for the first
//!   [`CPU1_COLUMNS`] columns, an interrupt each; then offline (each read
//!   refused).
//! - **CPU 2**: three threads taking turns (each turn an enqueue to 3 and a
//!   dequeue to 2), interrupts every 7 columns, two locks that waited in
//!   column 40; 12 numbers of its X-ray ring gone before its first read (a
//!   backlog's, not counted) and 5 more in column 30 (counted missed).
//!
//! Column 0 is every CPU's backlog (left out of the counts: a gap), as a
//! live view's first column is.

use alloc::vec::Vec;
use lumen_trace::Event;
use lumen_trace::kind::{self, Irq, LockWait, RunQueue, RunQueueOp, Stream, Switch, ThreadState};

use crate::{COLUMNS, View};

/// The CPUs the trace has (`trace_cpus` for it).
pub const CPUS: usize = 3;

/// The threads CPU 0 switches among.
const CPU0_THREADS: [u64; 4] = [3, 7, 11, 15];
/// The threads switching on CPU 1 while it is there.
const CPU1_THREADS: [u64; 2] = [31, 32];
/// The columns CPU 1 is read for before its reads are refused.
pub const CPU1_COLUMNS: usize = 10;
/// The threads taking turns on CPU 2.
const CPU2_THREADS: [u64; 3] = [21, 22, 23];

/// One ring of one CPU: its events since the last read, numbered on from
/// its last.
struct Ring {
    cpu: u16,
    stream: Stream,
    next: u64,
    /// What its next read counts missed.
    missed: u64,
    events: Vec<Event>,
}

impl Ring {
    /// CPU `cpu`'s ring `stream`, its first `gone` numbers already gone
    /// (counted missed by its first read).
    fn new(cpu: u16, stream: Stream, gone: u64) -> Ring {
        Ring { cpu, stream, next: gone, missed: gone, events: Vec::new() }
    }

    fn push(&mut self, kind: u16, args: [u64; 4]) {
        self.events.push(Event { seq: self.next, tsc: self.next * 1000, cpu: self.cpu, kind, pid: 0, args });
        self.next += 1;
    }

    fn runq(&mut self, len: u64, thread: u64, op: RunQueueOp) {
        self.push(kind::RUN_QUEUE, RunQueue { cpu: self.cpu, len, thread, op }.args());
    }

    fn switch(&mut self, from: u64, to: u64) {
        self.push(kind::SWITCH, Switch { from, to, state: ThreadState::Ready }.args());
    }

    fn irqs(&mut self, n: u32) {
        for _ in 0..n {
            self.push(kind::IRQ, Irq { vector: 0x54, rip: 0xffff_ffff_8000_1000 }.args());
        }
    }

    fn locks(&mut self, n: u32) {
        for _ in 0..n {
            self.push(kind::LOCK_WAIT, LockWait { lock: 0x2000, spins: 12, waited: 900, line: 77 }.args());
        }
    }

    /// `n` numbers gone before they were read: the next read counts them
    /// missed.
    fn skip(&mut self, n: u64) {
        self.next += n;
        self.missed += n;
    }

    /// The events since the last read, read into `v` as `trace_read`
    /// would hand them over.
    fn read_into(&mut self, v: &mut View) {
        let events = core::mem::take(&mut self.events);
        v.read(usize::from(self.cpu), self.stream, self.next, core::mem::take(&mut self.missed), &events);
    }
}

/// Switch `i` among `threads` taking turns: from the one running to the next.
fn turn(threads: &[u64], i: usize) -> (u64, u64) {
    (threads[i % threads.len()], threads[(i + 1) % threads.len()])
}

/// CPU 0's run-queue length in column `k`: 0 to 4 and back, every 16.
fn cpu0_len(k: usize) -> u64 {
    let p = (k % 16) as u64;
    if p < 8 { p / 2 } else { (16 - p) / 2 }
}

/// CPU 0's interrupts in column `k`: 3 every 5 columns, else 1 every 3.
fn cpu0_irqs(k: usize) -> u32 {
    if k.is_multiple_of(5) { 3 } else { u32::from(k.is_multiple_of(3)) }
}

/// The view the fixed trace gives once every column is read.
pub fn view() -> View {
    let mut v = View::new(CPUS);
    let (mut main0, mut xray0) = (Ring::new(0, Stream::Main, 0), Ring::new(0, Stream::Xray, 0));
    let (mut main1, mut xray1) = (Ring::new(1, Stream::Main, 0), Ring::new(1, Stream::Xray, 0));
    let (mut main2, mut xray2) = (Ring::new(2, Stream::Main, 0), Ring::new(2, Stream::Xray, 12));
    for k in 0..COLUMNS {
        // CPU 0.
        let len = cpu0_len(k);
        let before = k.checked_sub(1).map(cpu0_len);
        if before != Some(len) {
            let op = if before.is_some_and(|b| len < b) { RunQueueOp::Dequeue } else { RunQueueOp::Enqueue };
            xray0.runq(len, 40 + k as u64, op);
        }
        for i in 2 * k..2 * k + 2 {
            let (from, to) = turn(&CPU0_THREADS, i);
            main0.switch(from, to);
        }
        xray0.irqs(cpu0_irqs(k));
        xray0.locks(u32::from(k % 13 == 7));
        // CPU 2: each turn, the thread running goes back on the queue and
        // the next comes off it.
        if k == 30 {
            xray2.skip(5);
        }
        for i in 2 * k..2 * k + 2 {
            let (from, to) = turn(&CPU2_THREADS, i);
            xray2.runq(3, from, RunQueueOp::Enqueue);
            xray2.runq(2, to, RunQueueOp::Dequeue);
            main2.switch(from, to);
        }
        xray2.irqs(if k % 7 == 0 { 2 } else { 0 });
        xray2.locks(if k == 40 { 2 } else { 0 });

        for ring in [&mut main0, &mut xray0, &mut main2, &mut xray2] {
            ring.read_into(&mut v);
        }
        // CPU 1: there, then gone.
        if k < CPU1_COLUMNS {
            if k == 0 {
                xray1.runq(1, 33, RunQueueOp::Enqueue);
            }
            let (from, to) = turn(&CPU1_THREADS, k);
            main1.switch(from, to);
            xray1.irqs(1);
            main1.read_into(&mut v);
            xray1.read_into(&mut v);
        } else {
            v.offline(1);
        }
        v.end_column();
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Column, Presence, Span};

    #[test]
    fn the_frozen_view_is_pinned() {
        let v = view();
        assert_eq!(v.cpus().len(), CPUS);
        assert_eq!(v.cursor(), 0, "every column written once");
        let [c0, c1, c2] = [&v.cpus()[0], &v.cpus()[1], &v.cpus()[2]];
        // CPU 0: column 79's length is 79 % 16 = 15: (16 - 15) / 2 = 0.
        assert_eq!(c0.presence, Presence::Online);
        assert_eq!(c0.runq, Some(0));
        assert_eq!(c0.thread, Some(CPU0_THREADS[(2 * 79 + 2) % 4]));
        // Every count leaves column 0 out (the backlog).
        assert_eq!(c0.switches, Some(158));
        // irqs: 16 columns k % 5 == 0 (3 each), and those k % 3 == 0 but
        // not % 5 (1 each): 27 multiples of 3 below 80, 6 of 15; less
        // column 0's 3.
        assert_eq!(c0.irqs, Some(16 * 3 + (27 - 6) - 3));
        assert_eq!(c0.locks, Some(6), "columns 7, 20, 33, 46, 59, 72");
        assert_eq!((c0.missed, c0.odd), (Some(0), 0));
        assert_eq!(v.since(0, Stream::Main), 160);
        // CPU 1: read for its first columns, then offline: its queue and
        // thread forgotten, its counts kept.
        assert_eq!(c1.presence, Presence::Offline);
        assert_eq!((c1.runq, c1.thread), (None, None));
        assert_eq!((c1.switches, c1.irqs, c1.locks, c1.missed), (Some(9), Some(9), Some(0), Some(0)));
        // CPU 2.
        assert_eq!(c2.presence, Presence::Online);
        assert_eq!(c2.runq, Some(2));
        assert_eq!(c2.switches, Some(158));
        assert_eq!(c2.thread, Some(CPU2_THREADS[160 % 3]));
        assert_eq!(c2.irqs, Some(11 * 2), "columns 7, 14, ..., 77");
        assert_eq!(c2.locks, Some(2));
        assert_eq!(c2.missed, Some(5), "the backlog's 12 left out");
        assert_eq!(v.since(2, Stream::Xray), 12 + 4 * COLUMNS as u64 + 24 + 2 + 5);
    }

    #[test]
    fn its_columns_show_the_queue_rising_and_falling() {
        let v = view();
        let lens: Vec<u64> = (0..16).map(|k| v.cpus()[0].column(k).runq).collect();
        // Each column's longest: the length it began with, or a longer one
        // reached in it.
        assert_eq!(lens, [0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 2, 2, 1, 1]);
        let counted = Span::Counted;
        assert_eq!(v.cpus()[0].column(7), Column { span: counted, runq: 3, switches: 2, irqs: 0, locks: 1 });
        assert_eq!(v.cpus()[0].column(15), Column { span: counted, runq: 1, switches: 2, irqs: 3, locks: 0 });
        assert_eq!(v.cpus()[2].column(40), Column { span: counted, runq: 3, switches: 2, irqs: 0, locks: 2 });
        // Column 0: every CPU's backlog, a gap.
        assert!(v.cpus().iter().all(|c| c.column(0) == Column { span: Span::CatchingUp, ..Column::default() }));
        // CPU 1: counted while there, offline after, its length not carried.
        let c1 = &v.cpus()[1];
        assert!(
            (1..CPU1_COLUMNS)
                .all(|k| c1.column(k) == Column { span: counted, runq: 1, switches: 1, irqs: 1, locks: 0 })
        );
        assert!((CPU1_COLUMNS..COLUMNS).all(|k| c1.column(k) == Column { span: Span::Offline, ..Column::default() }));
    }
}
