//! X-ray's logic (#208; `apps/xray` is its window): the trace events an app
//! reads from each CPU's two rings (`trace_read`, ARCHITECTURE section 8,
//! "The `Trace` capability") turned into what X-ray shows. Host-tested,
//! `no_std` with `alloc`.
//!
//! - **Per CPU** ([`Cpu`]): the run queue's length as its newest `runq`
//!   event says it, the thread running as its newest `switch` says, and
//!   counts of switches, handled device interrupts and locks that waited,
//!   plus the sequence numbers the kernel counted missed and the events of
//!   X-ray's kinds that didn't decode.
//! - **Which ring holds what** (section 14): a CPU's main ring
//!   ([`Stream::Main`]) holds its `switch` events among others; its X-ray
//!   ring ([`Stream::Xray`]) its `runq` (the queue's changes, in the ring of
//!   the queue's CPU), `irq` and `lock` events. An event of another kind is
//!   left alone.
//! - **Reading on** ([`View::read`]): each read's `next` is where the next
//!   one starts ([`View::since`]); its `missed` is added up.
//! - **CPUs** ([`Presence`]): each starts unread (nothing known: every
//!   value `None`). `trace_cpus` is one past the highest online CPU, and an
//!   index below it may be offline (`trace_read` refuses it):
//!   [`View::offline`] marks one so, forgetting its run queue and thread
//!   (its counts stay); a later read marks it online again.
//! - **Columns** ([`Column`], [`View::end_column`]): what each CPU did
//!   between two ends, kept for the last [`COLUMNS`] ends (the lanes),
//!   the oldest overwritten.
//! - **The frozen view** ([`replay`]): a fixed trace of three CPUs (the
//!   middle one offline) read the same way, for `run xray --replay`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod replay;

use alloc::vec::Vec;
use lumen_trace::Event;
use lumen_trace::kind::{self, Irq, LockWait, RunQueue, Stream, Switch};

/// The columns kept for each CPU: the lanes' width.
pub const COLUMNS: usize = 80;

/// Whether a CPU was there for a column.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Span {
    /// Not written yet, or the CPU not read yet: nothing to show.
    #[default]
    Empty,
    /// Its events counted.
    Counted,
    /// The CPU offline: nothing to show.
    Offline,
}

/// Whether a CPU's rings have been read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    /// No read taken yet: nothing known.
    Unread,
    /// Its last read taken.
    Online,
    /// Its last read refused: not online.
    Offline,
}

/// What a CPU did in one column: between two [`View::end_column`]s.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Column {
    /// Whether it was there to count.
    pub span: Span,
    /// The run queue's longest length in it: its length when the column
    /// began (if known) and each `runq` event's in it.
    pub runq: u64,
    /// `switch` events.
    pub switches: u32,
    /// `irq` events: handled device interrupts.
    pub irqs: u32,
    /// `lock` events: locks that waited.
    pub locks: u32,
}

/// One CPU as its rings say it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cpu {
    /// Unread, online or offline.
    pub presence: Presence,
    /// The run queue's length, from its newest `runq` event (none read
    /// yet, or offline: `None`).
    pub runq: Option<u64>,
    /// The thread running, from its newest `switch` event (none read yet,
    /// or offline: `None`).
    pub thread: Option<u64>,
    /// `switch` events read (`None` until its main ring's first read).
    pub switches: Option<u64>,
    /// `irq` events read (`None` until its X-ray ring's first read).
    pub irqs: Option<u64>,
    /// `lock` events read (`None` until its X-ray ring's first read).
    pub locks: Option<u64>,
    /// Sequence numbers its reads counted missed (gone from a ring before
    /// they were read), both rings together (`None` until a first read).
    pub missed: Option<u64>,
    /// Events of X-ray's kinds that didn't decode (a field out of range),
    /// or a `runq` event for another CPU's queue: counted, not shown.
    pub odd: u64,
    /// Where each ring's next read starts (by [`Stream`]'s number).
    since: [u64; 2],
    /// The column being gathered.
    column: Column,
    /// The last [`COLUMNS`] columns, by their place in the lanes.
    history: [Column; COLUMNS],
}

impl Cpu {
    fn new() -> Cpu {
        Cpu {
            presence: Presence::Unread,
            runq: None,
            thread: None,
            switches: None,
            irqs: None,
            locks: None,
            missed: None,
            odd: 0,
            since: [0; 2],
            column: Column::default(),
            history: [Column::default(); COLUMNS],
        }
    }

    /// One event of ring `stream`, read from CPU `cpu`'s.
    fn apply(&mut self, cpu: usize, stream: Stream, e: &Event) {
        match (stream, e.kind) {
            (Stream::Main, kind::SWITCH) => match Switch::decode(e) {
                Some(s) => {
                    self.thread = Some(s.to);
                    bump(&mut self.switches);
                    self.column.switches = self.column.switches.saturating_add(1);
                }
                None => self.odd += 1,
            },
            (Stream::Xray, kind::RUN_QUEUE) => match RunQueue::decode(e) {
                Some(q) if usize::from(q.cpu) == cpu => {
                    self.runq = Some(q.len);
                    self.column.runq = self.column.runq.max(q.len);
                }
                _ => self.odd += 1,
            },
            (Stream::Xray, kind::IRQ) => match Irq::decode(e) {
                Some(_) => {
                    bump(&mut self.irqs);
                    self.column.irqs = self.column.irqs.saturating_add(1);
                }
                None => self.odd += 1,
            },
            (Stream::Xray, kind::LOCK_WAIT) => match LockWait::decode(e) {
                Some(_) => {
                    bump(&mut self.locks);
                    self.column.locks = self.column.locks.saturating_add(1);
                }
                None => self.odd += 1,
            },
            _ => {}
        }
    }

    /// Column `i` of its lane (`i` below [`COLUMNS`]).
    pub fn column(&self, i: usize) -> Column {
        self.history[i]
    }
}

/// One more of a count known by now.
fn bump(count: &mut Option<u64>) {
    *count = Some(count.unwrap_or(0).saturating_add(1));
}

/// Every CPU X-ray reads, and where the lanes' next column goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    cpus: Vec<Cpu>,
    /// The column [`View::end_column`] writes next.
    cursor: usize,
}

impl View {
    /// `cpus` CPUs (`trace_cpus`), none read yet.
    pub fn new(cpus: usize) -> View {
        View { cpus: (0..cpus).map(|_| Cpu::new()).collect(), cursor: 0 }
    }

    pub fn cpus(&self) -> &[Cpu] {
        &self.cpus
    }

    /// Where the next read of CPU `cpu`'s ring `stream` starts (0 for one
    /// never read: the oldest event the ring holds).
    pub fn since(&self, cpu: usize, stream: Stream) -> u64 {
        self.cpus[cpu].since[stream as usize]
    }

    /// A read of CPU `cpu`'s ring `stream` taken: its header's `next` and
    /// `missed`, and the events it copied, oldest first.
    pub fn read(&mut self, cpu: usize, stream: Stream, next: u64, missed: u64, events: &[Event]) {
        let c = &mut self.cpus[cpu];
        c.presence = Presence::Online;
        c.since[stream as usize] = next;
        c.missed = Some(c.missed.unwrap_or(0).saturating_add(missed));
        // Known from its ring's first read on, at 0 if nothing came.
        match stream {
            Stream::Main => c.switches = Some(c.switches.unwrap_or(0)),
            Stream::Xray => {
                c.irqs = Some(c.irqs.unwrap_or(0));
                c.locks = Some(c.locks.unwrap_or(0));
            }
        }
        for e in events {
            c.apply(cpu, stream, e);
        }
    }

    /// CPU `cpu`'s read refused: it isn't online. Its run queue and
    /// thread are forgotten (no longer true), and so is the column it was
    /// gathering (written as offline, its length not carried); its counts
    /// stay.
    pub fn offline(&mut self, cpu: usize) {
        let c = &mut self.cpus[cpu];
        c.presence = Presence::Offline;
        c.runq = None;
        c.thread = None;
        c.column = Column::default();
    }

    /// End the column: each CPU's goes into its lane at the cursor, and the
    /// next one begins with the run queue's length as it is. The column
    /// written.
    pub fn end_column(&mut self) -> usize {
        let at = self.cursor;
        for c in &mut self.cpus {
            let span = match c.presence {
                Presence::Unread => Span::Empty,
                Presence::Online => Span::Counted,
                Presence::Offline => Span::Offline,
            };
            c.history[at] = Column { span, ..c.column };
            c.column = Column { runq: c.runq.unwrap_or(0), ..Column::default() };
        }
        self.cursor = (at + 1) % COLUMNS;
        at
    }

    /// The column the next [`View::end_column`] writes.
    pub fn cursor(&self) -> usize {
        self.cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_trace::kind::{RunQueueOp, ThreadState};

    fn ev(seq: u64, kind: u16, args: [u64; 4]) -> Event {
        Event { seq, kind, args, ..Event::default() }
    }

    fn runq(seq: u64, cpu: u16, len: u64) -> Event {
        let q = RunQueue { cpu, len, thread: 9, op: RunQueueOp::Enqueue };
        ev(seq, kind::RUN_QUEUE, q.args())
    }

    fn switch(seq: u64, to: u64) -> Event {
        ev(seq, kind::SWITCH, Switch { from: 1, to, state: ThreadState::Ready }.args())
    }

    fn irq(seq: u64) -> Event {
        ev(seq, kind::IRQ, Irq { vector: 0x54, rip: 0 }.args())
    }

    fn lock(seq: u64) -> Event {
        ev(seq, kind::LOCK_WAIT, LockWait { lock: 0x1000, spins: 3, waited: 40, line: 12 }.args())
    }

    /// The counts as one tuple: switches, irqs, locks, missed.
    fn counts(c: &Cpu) -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
        (c.switches, c.irqs, c.locks, c.missed)
    }

    #[test]
    fn a_new_view_knows_nothing_and_reads_from_the_oldest() {
        let v = View::new(2);
        assert_eq!(v.cpus().len(), 2);
        for (i, c) in v.cpus().iter().enumerate() {
            assert_eq!(c.presence, Presence::Unread);
            assert_eq!((c.runq, c.thread, c.odd), (None, None, 0));
            assert_eq!(counts(c), (None, None, None, None));
            assert_eq!(v.since(i, Stream::Main), 0);
            assert_eq!(v.since(i, Stream::Xray), 0);
        }
        assert_eq!(v.cursor(), 0);
    }

    #[test]
    fn each_ring_gives_its_own_kinds() {
        let mut v = View::new(1);
        v.read(0, Stream::Xray, 5, 0, &[runq(1, 0, 2), irq(2), lock(3), runq(4, 0, 3)]);
        v.read(0, Stream::Main, 3, 0, &[switch(0, 7), switch(1, 11), ev(2, kind::LOG, [0; 4])]);
        let c = &v.cpus()[0];
        assert_eq!(c.presence, Presence::Online);
        assert_eq!(c.runq, Some(3), "the newest runq event's length");
        assert_eq!(c.thread, Some(11), "the newest switch's thread");
        assert_eq!(counts(c), (Some(2), Some(1), Some(1), Some(0)));
        assert_eq!(c.odd, 0);
        assert_eq!((v.since(0, Stream::Xray), v.since(0, Stream::Main)), (5, 3));
    }

    #[test]
    fn counts_are_known_from_their_ring_s_first_read() {
        // An empty read of the X-ray ring makes its counts 0; the switches,
        // from the main ring, stay unknown until that ring is read.
        let mut v = View::new(1);
        v.read(0, Stream::Xray, 0, 0, &[]);
        assert_eq!(counts(&v.cpus()[0]), (None, Some(0), Some(0), Some(0)));
        v.read(0, Stream::Main, 0, 0, &[]);
        assert_eq!(counts(&v.cpus()[0]), (Some(0), Some(0), Some(0), Some(0)));
        assert_eq!((v.cpus()[0].runq, v.cpus()[0].thread), (None, None), "no event: still unknown");
    }

    #[test]
    fn a_kind_in_the_other_ring_is_left_alone() {
        // A switch in the X-ray ring, or a runq, irq or lock event in the
        // main one, isn't where section 14 puts it: not counted.
        let mut v = View::new(1);
        v.read(0, Stream::Xray, 1, 0, &[switch(0, 7)]);
        v.read(0, Stream::Main, 3, 0, &[runq(0, 0, 4), irq(1), lock(2)]);
        let c = &v.cpus()[0];
        assert_eq!((c.runq, c.thread), (None, None));
        assert_eq!(counts(c), (Some(0), Some(0), Some(0), Some(0)));
    }

    #[test]
    fn events_that_dont_decode_are_counted_odd() {
        let mut v = View::new(2);
        v.read(
            0,
            Stream::Xray,
            4,
            0,
            &[
                ev(0, kind::RUN_QUEUE, [0, 1, 9, 7]),       // no such op
                runq(1, 1, 5),                              // another CPU's queue
                ev(2, kind::IRQ, [0x1_00, 0, 0, 0]),        // a vector past a byte
                ev(3, kind::LOCK_WAIT, [0, 0, 0, 1 << 33]), // a line past u32
            ],
        );
        v.read(0, Stream::Main, 1, 0, &[ev(0, kind::SWITCH, [1, 2, 9, 0])]); // no such state
        let c = &v.cpus()[0];
        assert_eq!(c.odd, 5);
        assert_eq!((c.runq, c.thread), (None, None));
        assert_eq!(counts(c), (Some(0), Some(0), Some(0), Some(0)));
    }

    #[test]
    fn missed_numbers_add_up_and_reads_go_on_from_next() {
        let mut v = View::new(1);
        v.read(0, Stream::Xray, 10, 4, &[runq(4, 0, 1)]);
        v.read(0, Stream::Xray, 300, 250, &[]);
        v.read(0, Stream::Main, 7, 3, &[]);
        assert_eq!(v.cpus()[0].missed, Some(257));
        assert_eq!(v.since(0, Stream::Xray), 300);
        assert_eq!(v.since(0, Stream::Main), 7);
        v.read(0, Stream::Main, 8, u64::MAX, &[]);
        assert_eq!(v.cpus()[0].missed, Some(u64::MAX), "saturating");
    }

    #[test]
    fn offline_forgets_the_queue_and_thread_and_a_read_brings_it_back() {
        let mut v = View::new(3);
        v.read(0, Stream::Xray, 2, 0, &[runq(0, 0, 2), irq(1)]);
        v.read(0, Stream::Main, 1, 0, &[switch(0, 5)]);
        v.offline(1);
        v.read(2, Stream::Main, 1, 0, &[]);
        let presence: Vec<Presence> = v.cpus().iter().map(|c| c.presence).collect();
        assert_eq!(presence, [Presence::Online, Presence::Offline, Presence::Online]);
        assert_eq!(counts(&v.cpus()[1]), (None, None, None, None), "never read");
        v.offline(0);
        let c = &v.cpus()[0];
        assert_eq!(c.presence, Presence::Offline);
        assert_eq!((c.runq, c.thread), (None, None), "no longer true");
        assert_eq!(counts(c), (Some(1), Some(1), Some(0), Some(0)), "counts stay");
        v.read(0, Stream::Main, 2, 0, &[]);
        assert_eq!(v.cpus()[0].presence, Presence::Online);
    }

    #[test]
    fn a_column_is_written_as_the_cpu_was() {
        let mut v = View::new(3);
        v.read(0, Stream::Xray, 1, 0, &[runq(0, 0, 3)]);
        v.read(1, Stream::Xray, 1, 0, &[runq(0, 1, 2)]);
        v.offline(1);
        v.end_column();
        let spans: Vec<Span> = v.cpus().iter().map(|c| c.column(0).span).collect();
        assert_eq!(spans, [Span::Counted, Span::Offline, Span::Empty]);
        // Nor does the column it went offline in.
        assert_eq!(v.cpus()[1].column(0), Column { span: Span::Offline, ..Column::default() });
        // An offline CPU's next column doesn't carry its old length.
        v.end_column();
        assert_eq!(v.cpus()[1].column(1), Column { span: Span::Offline, ..Column::default() });
        assert_eq!(v.cpus()[0].column(1).runq, 3);
    }

    #[test]
    fn a_column_keeps_the_longest_queue_and_its_counts() {
        let mut v = View::new(1);
        v.read(0, Stream::Xray, 6, 0, &[runq(0, 0, 1), runq(1, 0, 4), runq(2, 0, 2), irq(3), irq(4), lock(5)]);
        v.read(0, Stream::Main, 1, 0, &[switch(0, 3)]);
        assert_eq!(v.end_column(), 0);
        assert_eq!(v.cpus()[0].column(0), Column { span: Span::Counted, runq: 4, switches: 1, irqs: 2, locks: 1 });
        // No events: the next column carries the length as it was (2).
        assert_eq!(v.end_column(), 1);
        assert_eq!(v.cpus()[0].column(1), Column { span: Span::Counted, runq: 2, ..Column::default() });
        // A length that drops to 0 in a column still shows what it began with.
        v.read(0, Stream::Xray, 7, 0, &[runq(6, 0, 0)]);
        v.end_column();
        assert_eq!(v.cpus()[0].column(2).runq, 2);
        v.end_column();
        assert_eq!(v.cpus()[0].column(3).runq, 0);
    }

    #[test]
    fn columns_wrap_round_the_lane() {
        let mut v = View::new(1);
        for i in 0..COLUMNS {
            v.read(0, Stream::Xray, i as u64 + 1, 0, &[runq(i as u64, 0, i as u64)]);
            assert_eq!(v.end_column(), i);
        }
        assert_eq!(v.cursor(), 0);
        v.read(0, Stream::Xray, 1000, 0, &[runq(999, 0, 999)]);
        assert_eq!(v.end_column(), 0, "the oldest column overwritten");
        assert_eq!(v.cpus()[0].column(0).runq, 999);
        assert_eq!(v.cpus()[0].column(1).runq, 1);
        assert_eq!(v.cursor(), 1);
    }

    #[test]
    fn counts_in_a_column_saturate() {
        let mut v = View::new(1);
        v.cpus[0].column.irqs = u32::MAX;
        v.cpus[0].column.locks = u32::MAX;
        v.cpus[0].column.switches = u32::MAX;
        v.read(0, Stream::Xray, 2, 0, &[irq(0), lock(1)]);
        v.read(0, Stream::Main, 1, 0, &[switch(0, 4)]);
        v.end_column();
        let col = v.cpus()[0].column(0);
        assert_eq!((col.irqs, col.locks, col.switches), (u32::MAX, u32::MAX, u32::MAX));
        assert_eq!(counts(&v.cpus()[0]), (Some(1), Some(1), Some(1), Some(0)));
    }
}
