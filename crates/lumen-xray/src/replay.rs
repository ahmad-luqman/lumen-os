//! The fixed trace X-ray's frozen view shows (`run xray --replay`, its
//! golden): three CPUs, the middle one offline (an index hole below
//! `trace_cpus`, as on a machine whose CPUs aren't numbered densely), read
//! column by column through [`View::read`] exactly as live reads are, so
//! the view it gives is what the same events read live would give.
//!
//! - **CPU 0**: a run queue rising and falling (0 to 4 and back, every 16
//!   columns), two switches a column among four threads, a few interrupts
//!   and a lock that waited every 13 columns.
//! - **CPU 1**: offline (each read refused).
//! - **CPU 2**: three threads taking turns (each turn an enqueue to 3 and a
//!   dequeue to 2), interrupts every 7 columns, two locks that waited in
//!   column 40; its first X-ray read counted 12 numbers missed.

use alloc::vec::Vec;
use lumen_trace::Event;
use lumen_trace::kind::{self, Irq, LockWait, RunQueue, RunQueueOp, Stream, Switch, ThreadState};

use crate::{COLUMNS, View};

/// The CPUs the trace has (`trace_cpus` for it).
pub const CPUS: usize = 3;

/// The threads CPU 0 switches among.
const CPU0_THREADS: [u64; 4] = [3, 7, 11, 15];
/// The threads taking turns on CPU 2.
const CPU2_THREADS: [u64; 3] = [21, 22, 23];

/// One ring's events, numbered on from its last.
struct Ring {
    next: u64,
    events: Vec<Event>,
}

impl Ring {
    fn new(first: u64) -> Ring {
        Ring { next: first, events: Vec::new() }
    }

    fn push(&mut self, cpu: u16, kind: u16, args: [u64; 4]) {
        self.events.push(Event { seq: self.next, tsc: self.next * 1000, cpu, kind, pid: 0, args });
        self.next += 1;
    }

    fn runq(&mut self, cpu: u16, len: u64, thread: u64, op: RunQueueOp) {
        self.push(cpu, kind::RUN_QUEUE, RunQueue { cpu, len, thread, op }.args());
    }

    fn switch(&mut self, cpu: u16, from: u64, to: u64) {
        self.push(cpu, kind::SWITCH, Switch { from, to, state: ThreadState::Ready }.args());
    }

    fn irqs(&mut self, cpu: u16, n: u32) {
        for _ in 0..n {
            self.push(cpu, kind::IRQ, Irq { vector: 0x54, rip: 0xffff_ffff_8000_1000 }.args());
        }
    }

    fn locks(&mut self, cpu: u16, n: u32) {
        for _ in 0..n {
            self.push(cpu, kind::LOCK_WAIT, LockWait { lock: 0x2000, spins: 12, waited: 900, line: 77 }.args());
        }
    }

    /// The events since the last take, and the `next` a read of them says.
    fn take(&mut self) -> (u64, Vec<Event>) {
        (self.next, core::mem::take(&mut self.events))
    }
}

/// CPU 0's run-queue length in column `k`: 0 to 4 and back, every 16.
fn cpu0_len(k: usize) -> u64 {
    let p = (k % 16) as u64;
    if p < 8 { p / 2 } else { (16 - p) / 2 }
}

/// The view the fixed trace gives once every column is read.
pub fn view() -> View {
    let mut v = View::new(CPUS);
    let (mut main0, mut xray0) = (Ring::new(0), Ring::new(0));
    // CPU 2's X-ray ring starts past 12 numbers already gone.
    let (mut main2, mut xray2) = (Ring::new(0), Ring::new(12));
    let mut missed2 = 12;
    let mut thread0 = CPU0_THREADS[0];
    let mut thread2 = CPU2_THREADS[0];
    for k in 0..COLUMNS {
        // CPU 0.
        let len = cpu0_len(k);
        if k == 0 || len != cpu0_len(k - 1) {
            let op = if k > 0 && len < cpu0_len(k - 1) { RunQueueOp::Dequeue } else { RunQueueOp::Enqueue };
            xray0.runq(0, len, 40 + k as u64, op);
        }
        for s in 0..2 {
            let to = CPU0_THREADS[(2 * k + s + 1) % CPU0_THREADS.len()];
            main0.switch(0, thread0, to);
            thread0 = to;
        }
        xray0.irqs(
            0,
            if k % 5 == 0 {
                3
            } else if k % 3 == 0 {
                1
            } else {
                0
            },
        );
        xray0.locks(0, u32::from(k % 13 == 7));
        // CPU 2: each turn, the thread running goes back on the queue and
        // the next comes off it.
        for _ in 0..2 {
            let to = CPU2_THREADS[(CPU2_THREADS.iter().position(|&t| t == thread2).unwrap_or(0) + 1) % 3];
            xray2.runq(2, 3, thread2, RunQueueOp::Enqueue);
            xray2.runq(2, 2, to, RunQueueOp::Dequeue);
            main2.switch(2, thread2, to);
            thread2 = to;
        }
        xray2.irqs(2, if k % 7 == 0 { 2 } else { 0 });
        xray2.locks(2, if k == 40 { 2 } else { 0 });

        for (cpu, stream, ring, missed) in [
            (0, Stream::Main, &mut main0, 0),
            (0, Stream::Xray, &mut xray0, 0),
            (2, Stream::Main, &mut main2, 0),
            (2, Stream::Xray, &mut xray2, core::mem::take(&mut missed2)),
        ] {
            let (next, events) = ring.take();
            v.read(cpu, stream, next, missed, &events);
        }
        v.offline(1);
        v.end_column();
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Column;

    #[test]
    fn the_frozen_view_is_pinned() {
        let v = view();
        assert_eq!(v.cpus().len(), CPUS);
        assert_eq!(v.cursor(), 0, "every column written once");
        let [c0, c1, c2] = [&v.cpus()[0], &v.cpus()[1], &v.cpus()[2]];
        // CPU 0: column 79's length is 79 % 16 = 15: (16 - 15) / 2 = 0.
        assert!(c0.online);
        assert_eq!(c0.runq, Some(0));
        assert_eq!(c0.thread, Some(CPU0_THREADS[(2 * 79 + 2) % 4]));
        assert_eq!(c0.switches, 160);
        // irqs: 16 columns k % 5 == 0 (3 each), and those k % 3 == 0 but
        // not % 5 (1 each): 27 multiples of 3 below 80, 6 of 15.
        assert_eq!(c0.irqs, 16 * 3 + (27 - 6));
        assert_eq!(c0.locks, 6, "columns 7, 20, 33, 46, 59, 72");
        assert_eq!((c0.missed, c0.odd), (0, 0));
        assert_eq!(v.since(0, Stream::Main), 160);
        // CPU 1: never read.
        assert!(!c1.online);
        assert_eq!((c1.runq, c1.thread, c1.switches), (None, None, 0));
        // CPU 2.
        assert!(c2.online);
        assert_eq!(c2.runq, Some(2));
        assert_eq!(c2.switches, 160);
        assert_eq!(c2.thread, Some(CPU2_THREADS[160 % 3]));
        assert_eq!(c2.irqs, 12 * 2, "columns 0, 7, ..., 77");
        assert_eq!(c2.locks, 2);
        assert_eq!(c2.missed, 12);
        assert_eq!(v.since(2, Stream::Xray), 12 + 4 * COLUMNS as u64 + 24 + 2);
    }

    #[test]
    fn its_columns_show_the_queue_rising_and_falling() {
        let v = view();
        let lens: Vec<u64> = (0..16).map(|k| v.cpus()[0].column(k).runq).collect();
        // Each column's longest: the length it began with, or a longer one
        // reached in it.
        assert_eq!(lens, [0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 3, 3, 2, 2, 1, 1]);
        assert_eq!(v.cpus()[0].column(7), Column { runq: 3, switches: 2, irqs: 0, locks: 1 });
        assert_eq!(v.cpus()[0].column(15), Column { runq: 1, switches: 2, irqs: 3, locks: 0 });
        assert_eq!(v.cpus()[2].column(40), Column { runq: 3, switches: 2, irqs: 0, locks: 2 });
        assert!(v.cpus()[1].history.iter().all(|c| *c == Column::default()));
    }
}
