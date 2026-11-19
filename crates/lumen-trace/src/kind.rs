//! Event kinds, numbered in one place: the kernel's `trace::Kind` takes its
//! values from here, and apps (X-ray, through `lumen-app-api`) decode the
//! events they read with the same numbers and layouts, so the two can't
//! disagree.
//!
//! The X-ray kinds have typed layouts ([`RunQueue`], [`Irq`], [`LockWait`]):
//! `args()` is what the kernel records, `decode` reads an [`Event`] back
//! (`None` for another kind, or args no recorder writes). Their layouts are
//! pinned by the tests below.

use crate::Event;

/// A boot phase ended (main ring). `args[0]`: the phase's number.
pub const BOOT_PHASE: u16 = 1;
/// A CPU exception, or a device interrupt nothing handles (main ring).
/// `args`: vector, rip, error code.
pub const EXCEPTION: u16 = 2;
/// A log line (main ring). `args`: level (1 error .. 5 trace), the tag's
/// first 8 bytes little-endian.
pub const LOG: u16 = 3;
/// A panic is being reported (main ring). `args[0]`: the source line.
pub const PANIC: u16 = 4;
/// The scheduler switched threads (main ring). `args`: previous thread,
/// next thread, the previous thread's state.
pub const SWITCH: u16 = 5;
/// A run queue changed (X-ray ring): [`RunQueue`].
pub const RUN_QUEUE: u16 = 6;
/// A device interrupt a handler took (X-ray ring): [`Irq`].
pub const IRQ: u16 = 7;
/// A lock had to wait (X-ray ring): [`LockWait`].
pub const LOCK_WAIT: u16 = 8;
/// Recorded by in-kernel tests.
pub const TEST: u16 = 0xff;

/// What happened to a run queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum RunQueueOp {
    /// A thread was queued.
    Enqueue = 1,
    /// A thread was taken off to run.
    Dequeue = 2,
}

impl TryFrom<u64> for RunQueueOp {
    type Error = u64;

    fn try_from(raw: u64) -> Result<Self, u64> {
        match raw {
            1 => Ok(RunQueueOp::Enqueue),
            2 => Ok(RunQueueOp::Dequeue),
            other => Err(other),
        }
    }
}

/// [`RUN_QUEUE`]: CPU `cpu`'s run queue changed. Recorded into that CPU's
/// X-ray ring (by whichever CPU changed it), so `cpu` is also the ring's.
/// `args`: `[cpu, len, thread, op]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunQueue {
    /// The queue's (and the ring's) CPU.
    pub cpu: u16,
    /// Its length after the change.
    pub len: u64,
    /// The thread queued or taken off.
    pub thread: u64,
    pub op: RunQueueOp,
}

impl RunQueue {
    pub const KIND: u16 = RUN_QUEUE;

    pub fn args(&self) -> [u64; 4] {
        [u64::from(self.cpu), self.len, self.thread, self.op as u64]
    }

    pub fn decode(e: &Event) -> Option<RunQueue> {
        if e.kind != Self::KIND {
            return None;
        }
        let [cpu, len, thread, op] = e.args;
        Some(RunQueue { cpu: u16::try_from(cpu).ok()?, len, thread, op: RunQueueOp::try_from(op).ok()? })
    }
}

/// [`IRQ`]: a device interrupt a handler took, on the CPU whose X-ray ring
/// it is in. `args`: `[vector, rip, 0, 0]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Irq {
    pub vector: u8,
    /// Where the CPU was when it came.
    pub rip: u64,
}

impl Irq {
    pub const KIND: u16 = IRQ;

    pub fn args(&self) -> [u64; 4] {
        [u64::from(self.vector), self.rip, 0, 0]
    }

    pub fn decode(e: &Event) -> Option<Irq> {
        if e.kind != Self::KIND {
            return None;
        }
        Some(Irq { vector: u8::try_from(e.args[0]).ok()?, rip: e.args[1] })
    }
}

/// [`LOCK_WAIT`]: a lock had to wait, on the CPU whose X-ray ring it is in.
/// `args`: `[lock, spins, waited, line]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockWait {
    /// The lock's address.
    pub lock: u64,
    /// How many times it found the lock held before taking it (at least 1).
    pub spins: u64,
    /// TSC ticks from first finding it held to taking it.
    pub waited: u64,
    /// The line of the `lock` call (its file isn't recorded).
    pub line: u32,
}

impl LockWait {
    pub const KIND: u16 = LOCK_WAIT;

    pub fn args(&self) -> [u64; 4] {
        [self.lock, self.spins, self.waited, u64::from(self.line)]
    }

    pub fn decode(e: &Event) -> Option<LockWait> {
        if e.kind != Self::KIND {
            return None;
        }
        let [lock, spins, waited, line] = e.args;
        Some(LockWait { lock, spins, waited, line: u32::try_from(line).ok()? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: u16, args: [u64; 4]) -> Event {
        Event { kind, args, ..Event::default() }
    }

    #[test]
    fn kinds_have_their_numbers() {
        // The numbers are an interface (apps decode them): pinned.
        assert_eq!(
            [BOOT_PHASE, EXCEPTION, LOG, PANIC, SWITCH, RUN_QUEUE, IRQ, LOCK_WAIT, TEST],
            [1, 2, 3, 4, 5, 6, 7, 8, 0xff]
        );
    }

    #[test]
    fn run_queue_layout_round_trips() {
        let rq = RunQueue { cpu: 3, len: 2, thread: 41, op: RunQueueOp::Dequeue };
        assert_eq!(rq.args(), [3, 2, 41, 2]);
        assert_eq!(RunQueue::decode(&event(RUN_QUEUE, rq.args())), Some(rq));
        let rq = RunQueue { op: RunQueueOp::Enqueue, ..rq };
        assert_eq!(rq.args()[3], 1);
        assert_eq!(RunQueue::decode(&event(RUN_QUEUE, rq.args())), Some(rq));
        // Another kind, an unknown op, a CPU past u16: not a RunQueue.
        assert_eq!(RunQueue::decode(&event(IRQ, rq.args())), None);
        assert_eq!(RunQueue::decode(&event(RUN_QUEUE, [3, 2, 41, 3])), None);
        assert_eq!(RunQueue::decode(&event(RUN_QUEUE, [1 << 16, 2, 41, 1])), None);
        assert_eq!(RunQueueOp::try_from(0), Err(0));
    }

    #[test]
    fn irq_layout_round_trips() {
        let irq = Irq { vector: 0x54, rip: 0xffff_ffff_8000_1234 };
        assert_eq!(irq.args(), [0x54, 0xffff_ffff_8000_1234, 0, 0]);
        assert_eq!(Irq::decode(&event(IRQ, irq.args())), Some(irq));
        assert_eq!(Irq::decode(&event(IRQ, [0x100, 0, 0, 0])), None);
        assert_eq!(Irq::decode(&event(LOCK_WAIT, irq.args())), None);
    }

    #[test]
    fn lock_wait_layout_round_trips() {
        let w = LockWait { lock: 0xffff_8000_0010_0000, spins: 7, waited: 1234, line: 321 };
        assert_eq!(w.args(), [0xffff_8000_0010_0000, 7, 1234, 321]);
        assert_eq!(LockWait::decode(&event(LOCK_WAIT, w.args())), Some(w));
        assert_eq!(LockWait::decode(&event(LOCK_WAIT, [0, 1, 1, 1 << 32])), None);
        assert_eq!(LockWait::decode(&event(RUN_QUEUE, w.args())), None);
    }
}
