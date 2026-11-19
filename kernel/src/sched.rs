//! Kernel threads: preemptive round robin, one run queue per CPU.
//!
//! A [`Thread`] has its own kernel stack from `mm::stack` (with a guard page,
//! hard rule 4), its saved `rsp` while it isn't running, and a [`State`].
//! [`spawn`] starts one on this CPU, [`spawn_on`] on another; **a thread
//! never moves**: it runs only on the CPU it was spawned on (its home), so
//! only that CPU ever takes threads off its queue, and once the spawner has
//! laid out its first frame (published by the run queue's lock), a thread's
//! saved registers are touched only by its home CPU, with interrupts off.
//! Waking a thread from another CPU puts it on its home's queue and sends
//! that CPU the `WAKEUP` IPI (in case it is halted in its idle loop).
//!
//! **Switching** ([`schedule`]) happens only with interrupts off and no
//! critical section open (`arch::irq::depth() == 0`, asserted): a held
//! `SpinLock`'s section belongs to the CPU, so one open across a switch
//! would be closed by another thread. A thread that gives up the CPU
//! ([`yield_now`], [`sleep`], [`WaitQueue::wait_until`],
//! [`WaitQueue::wait_until_or_deadline`]) turns interrupts
//! off with `arch::irq::disable` and keeps whether they were on in a local
//! on its own stack, restoring it when it runs again ([`exit`] never does). The timer
//! interrupt ([`tick`], 1 kHz on every CPU) preempts: it wakes sleepers
//! whose deadline has passed and switches when another thread is ready and
//! the running thread's slice ([`SLICE_TICKS`]) is used up, or the idle
//! thread runs; the preempted thread resumes inside the interrupt handler
//! and `iretq`s back.
//!
//! Every CPU has an idle thread, which runs only when nothing else is ready
//! and is never queued: on an AP it is the AP's `ap_main` context (on its
//! main stack), looping in [`idle_loop`] (`smp::run_on` jobs run there), on the boot CPU a
//! thread of its own. `kmain`'s context becomes the boot CPU's `main` thread
//! (the serial shell, or the ktests). A CPU's scheduler runs ([`running_on`]) from the
//! first tick after it is armed (its current and idle threads set and its
//! timer started): until then nothing could wake a sleeper or preempt there,
//! so threads can't be spawned there and `time::sleep` busy-waits.
//!
//! A thread that returns from its entry point (or calls [`exit`]) is dead;
//! the next thread to run on its CPU frees its stack and is done with it
//! ([`FINISHING`]), and only then does [`JoinHandle::join`] return. The unmap's TLB flush is local: the home CPU
//! ran on the stack; a spawner on another CPU wrote its first frame and may
//! keep a stale entry for that page, which nothing uses again (stack
//! addresses are never reused).
//!
//! Lock order: a wait queue's lock, then a CPU's run queue (only
//! [`WaitQueue::wait_until_or_deadline`] nests them, to put the thread on
//! its CPU's sleepers too), then a thread's `stack` lock (only
//! [`exit`] nests it); then the heap, page tables and frame allocator. Every
//! thread homed on a CPU has room reserved in its queues ([`Cpu::homed`],
//! asserted on every push), so the timer interrupt never allocates. It can
//! free, in [`finish_switch`]: a dead thread's stack (through the page
//! tables and frame allocator), and on the heap its joiners' list (a
//! thread's record is freed when [`register`] prunes its `Weak`, after the
//! last reference is gone). For a joined thread the stack and the list are
//! freed before [`JoinHandle::join`] returns ([`FINISHING`]).
//!
//! For the shell's `cpus` and `top`, each tick counts against the running
//! thread and, on the idle thread, as an idle tick of its CPU (sampling at
//! 1 kHz: [`cpu_ticks`], [`threads`]); every thread is listed in a registry
//! of weak references, pruned of the dropped ones as threads are created.
//!
//! Every switch records a `switch` trace event (previous thread's id, next
//! thread's id, the previous thread's state), and the crash report names the
//! thread that was running (`thread: <name> (id N)`, read lock-free). Every
//! change to a run queue records a `runq` event in that CPU's X-ray ring
//! (`trace::record_run_queue`), under the queue's lock ([`run_queue`]
//! reads the queue's length and the ring's newest such event under it).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::fmt;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use log::{info, warn};
use lumen_mm::stacks::{StackKind, StackRange};

use crate::arch::{self, MAX_CPUS};
use crate::mm::stack::{KernelStack, STACK_SIZE, StackError};
use crate::sync::SpinLock;
use crate::trace::{self, Kind};
use crate::{smp, time};
use lumen_trace::kind::{RunQueue, RunQueueOp};

/// Timer ticks (1 ms each) a thread runs before the next ready thread on its
/// CPU gets a turn.
pub const SLICE_TICKS: u32 = 10;

/// How long [`init`] waits, in all, for every CPU's scheduler to start.
const START_TIMEOUT_NS: u64 = 1_000_000_000;

/// Where a thread is in its life.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Waiting for a turn: on its CPU's run queue (or about to be put there),
    /// or an idle thread that isn't running (never queued).
    Ready = 0,
    /// Its CPU's current thread.
    Running = 1,
    /// Waiting for a deadline ([`sleep`]).
    Sleeping = 2,
    /// Waiting on a [`WaitQueue`].
    Blocked = 3,
    /// Finished; its stack is freed once its CPU has switched away.
    Dead = 4,
}

impl State {
    const ALL: [State; 5] = [State::Ready, State::Running, State::Sleeping, State::Blocked, State::Dead];

    fn from_raw(raw: u8) -> Option<State> {
        Self::ALL.into_iter().find(|&s| s as u8 == raw)
    }

    /// The name of the state whose number is `raw` (a trace event's), if any.
    pub fn name_of(raw: u64) -> Option<&'static str> {
        u8::try_from(raw).ok().and_then(State::from_raw).map(State::name)
    }

    pub fn name(self) -> &'static str {
        match self {
            State::Ready => "ready",
            State::Running => "running",
            State::Sleeping => "sleeping",
            State::Blocked => "blocked",
            State::Dead => "dead",
        }
    }
}

/// A thread's entry point.
type Entry = Box<dyn FnOnce() + Send>;

/// The `rsp` [`arch::context::switch`] saved while a thread isn't running
/// (0 for a thread adopted from running code until it first switches away:
/// never loaded, `schedule` asserts).
struct SavedRsp(UnsafeCell<u64>);

// SAFETY: only the thread's home CPU touches the value after the thread is
// shared, inside `schedule` with interrupts off: it saves into it while
// switching away from the thread and reads it while switching to it, and a
// thread is never on two CPUs (it never moves; `schedule` asserts both
// threads are its CPU's). The value the spawner stores before the thread is
// shared is published by the run queue's lock.
unsafe impl Sync for SavedRsp {}

impl SavedRsp {
    /// Where `switch` saves it.
    ///
    /// # Safety
    /// On the thread's home CPU, with interrupts off.
    unsafe fn slot(&self) -> *mut u64 {
        self.0.get()
    }

    /// What `switch` loads.
    ///
    /// # Safety
    /// On the thread's home CPU, with interrupts off, the thread not running.
    unsafe fn get(&self) -> u64 {
        // SAFETY: upheld by the caller: nothing writes it meanwhile.
        unsafe { *self.0.get() }
    }
}

/// A kernel thread. Threads never move between CPUs: if that ever changes,
/// `SavedRsp`'s `Sync` argument, a woken thread sitting on its queue while
/// it still runs (see [`WaitQueue::wait_until`]), the callers that keep
/// `cpu_index()` across a switch, and freeing stacks with a local TLB flush
/// all have to be revisited.
pub struct Thread {
    id: u64,
    name: &'static str,
    /// Its home CPU, the only one it runs on.
    cpu: usize,
    state: AtomicU8,
    rsp: SavedRsp,
    /// Its stack: `None` for a thread adopted from code that was already
    /// running on a stack it doesn't own (`main`, an AP's idle thread), and
    /// once freed after the thread died.
    stack: SpinLock<Option<KernelStack>>,
    /// Its entry point, until it starts.
    entry: SpinLock<Option<Entry>>,
    /// Set once it is dead and its stack freed.
    reaped: AtomicBool,
    /// Joiners waiting for `reaped`.
    exited: WaitQueue,
    /// Timer ticks that found it running.
    ticks: AtomicU64,
    /// Its [`WaitQueue`] waits that the timer ended at their deadline, not
    /// a waker (for the ktests: [`deadline_wakes_in`]).
    #[cfg(feature = "ktest")]
    deadline_wakes: AtomicU64,
}

impl Thread {
    /// A ready thread on CPU `cpu` with a fresh stack of `stack_size` bytes,
    /// that starts by running `entry`.
    fn spawned(name: &'static str, cpu: usize, stack_size: u64, entry: Entry) -> Result<Arc<Thread>, SpawnError> {
        let stack = KernelStack::new(stack_size, StackKind::Thread).map_err(SpawnError::Stack)?;
        // SAFETY: a fresh, mapped stack (page-aligned top) nothing runs on;
        // the thread owns it, and it is freed only after the thread has died
        // and its CPU has switched away from it.
        let rsp = unsafe { arch::context::prepare(stack.range().top, thread_entry) };
        Ok(register(Arc::new(Thread::new(name, cpu, State::Ready, rsp, Some(stack), Some(entry)))))
    }

    /// A thread for the code running now on this CPU, on a stack it doesn't
    /// own.
    fn adopted(name: &'static str) -> Arc<Thread> {
        register(Arc::new(Thread::new(name, arch::cpu_index(), State::Running, 0, None, None)))
    }

    fn new(
        name: &'static str,
        cpu: usize,
        state: State,
        rsp: u64,
        stack: Option<KernelStack>,
        entry: Option<Entry>,
    ) -> Thread {
        Thread {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            name,
            cpu,
            state: AtomicU8::new(state as u8),
            rsp: SavedRsp(UnsafeCell::new(rsp)),
            stack: SpinLock::new(stack),
            entry: SpinLock::new(entry),
            reaped: AtomicBool::new(false),
            exited: WaitQueue::new(),
            ticks: AtomicU64::new(0),
            #[cfg(feature = "ktest")]
            deadline_wakes: AtomicU64::new(0),
        }
    }

    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The name it was spawned with.
    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn state(&self) -> State {
        State::from_raw(self.state.load(Ordering::Acquire)).expect("only a State is stored")
    }

    /// Its stack, unless it has none (see `stack`).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
    pub fn stack(&self) -> Option<StackRange> {
        self.stack.lock().as_ref().map(KernelStack::range)
    }

    /// Move it from `from` to `to` if it is in `from`; whether it was. For a
    /// thread two wakers race for (a queue's waker and its deadline): only
    /// the one that moves it queues it.
    fn try_change(&self, from: State, to: State) -> bool {
        self.state.compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire).is_ok()
    }

    /// Move it from `from` to `to`, panicking if it wasn't in `from` (a
    /// broken scheduler invariant: such a thread would be lost, or queued
    /// twice).
    fn change(&self, from: State, to: State) {
        let moved = self.state.compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire);
        if let Err(was) = moved {
            let was = State::from_raw(was).map_or("?", State::name);
            panic!(
                "thread {} (id {}, cpu {}) was {was}, not {}, on its way to {}",
                self.name,
                self.id,
                self.cpu,
                from.name(),
                to.name()
            );
        }
    }
}

impl fmt::Debug for Thread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (id {}, cpu {}, {})", self.name, self.id, self.cpu, self.state().name())
    }
}

/// One CPU's scheduler state.
struct Cpu {
    /// The thread it runs.
    current: Option<Arc<Thread>>,
    /// What it runs when nothing is ready (never queued).
    idle: Option<Arc<Thread>>,
    /// Threads waiting for a turn, in order.
    ready: VecDeque<Arc<Thread>>,
    /// Sleeping threads, and threads waiting on a queue with a deadline
    /// ([`WaitQueue::wait_until_or_deadline`]), with the `now_ns` each
    /// wakes at.
    sleepers: Vec<Sleeper>,
    /// The thread it last switched away from, kept alive until the switch
    /// is finished ([`finish_switch`]: a dead one's stack is freed there).
    switched_from: Option<Arc<Thread>>,
    /// Threads homed here, not yet reaped (the idle thread aside): `ready`
    /// and `sleepers` always have room for this many, so pushing onto them
    /// never allocates (the timer interrupt pushes onto them).
    homed: usize,
    /// Ticks left in the current thread's slice.
    slice_left: u32,
}

impl Cpu {
    const fn new() -> Cpu {
        Cpu {
            current: None,
            idle: None,
            ready: VecDeque::new(),
            sleepers: Vec::new(),
            switched_from: None,
            homed: 0,
            slice_left: SLICE_TICKS,
        }
    }

    /// Count one more thread homed here, and make room for it.
    fn home(&mut self) {
        self.homed += 1;
        self.ready.reserve(self.homed.saturating_sub(self.ready.len()));
        self.sleepers.reserve(self.homed.saturating_sub(self.sleepers.len()));
    }

    /// Take the thread this CPU switched away from; a dead one is no longer
    /// homed here.
    fn take_switched_from(&mut self) -> Option<Arc<Thread>> {
        let prev = self.switched_from.take();
        if prev.as_ref().is_some_and(|p| p.state() == State::Dead) {
            self.homed -= 1;
        }
        prev
    }

    fn running(&self) -> &Arc<Thread> {
        self.current.as_ref().expect("a running scheduler has a current thread")
    }

    fn is_idle_thread(&self, t: &Arc<Thread>) -> bool {
        self.idle.as_ref().is_some_and(|i| Arc::ptr_eq(i, t))
    }

    fn on_idle(&self) -> bool {
        self.current.as_ref().is_some_and(|c| self.is_idle_thread(c))
    }

    /// Queue a ready thread, in the room reserved for it. Every change to
    /// the queue goes through here or [`pop_ready`](Self::pop_ready), each
    /// recording a `runq` event in the CPU's X-ray ring under its lock.
    fn push_ready(&mut self, cpu: usize, t: Arc<Thread>) {
        assert!(self.ready.len() < self.ready.capacity(), "queueing thread {t:?}, which isn't homed on cpu {cpu}");
        let id = t.id;
        self.ready.push_back(t);
        self.queue_changed(cpu, id, RunQueueOp::Enqueue);
    }

    fn pop_ready(&mut self, cpu: usize) -> Option<Arc<Thread>> {
        let t = self.ready.pop_front()?;
        self.queue_changed(cpu, t.id, RunQueueOp::Dequeue);
        Some(t)
    }

    /// Publish the queue's new length ([`READY`]) and trace the change.
    fn queue_changed(&self, cpu: usize, thread: u64, op: RunQueueOp) {
        let len = self.ready.len();
        READY[cpu].store(len, Ordering::Release);
        const _: () = assert!(MAX_CPUS <= u16::MAX as usize);
        trace::record_run_queue(RunQueue { cpu: cpu as u16, len: len as u64, thread, op });
    }

    /// Make every sleeper whose deadline is `now` or earlier ready. A
    /// thread waiting on a queue with that deadline is made ready only if
    /// it is still blocked (its queue's waker may have woken it first: then
    /// the entry is just dropped).
    fn wake_sleepers(&mut self, cpu: usize, now: u64) {
        let mut i = 0;
        while i < self.sleepers.len() {
            if self.sleepers[i].at <= now {
                #[cfg(feature = "ktest")]
                count_queue_deadline(cpu, &self.sleepers[i], now);
                let Sleeper { thread, on_queue, .. } = self.sleepers.swap_remove(i);
                if !on_queue {
                    thread.change(State::Sleeping, State::Ready);
                    self.push_ready(cpu, thread);
                } else if thread.try_change(State::Blocked, State::Ready) {
                    #[cfg(feature = "ktest")]
                    thread.deadline_wakes.fetch_add(1, Ordering::Relaxed);
                    self.push_ready(cpu, thread);
                }
            } else {
                i += 1;
            }
        }
    }

    /// Drop `me`'s deadline entry, if it is still there (it woke some other
    /// way).
    fn forget_deadline(&mut self, me: &Arc<Thread>) {
        self.sleepers.retain(|s| !(s.on_queue && Arc::ptr_eq(&s.thread, me)));
    }
}

/// A thread waiting for a deadline: asleep ([`sleep`]), or blocked on a
/// [`WaitQueue`] until the deadline at the latest (`on_queue`).
struct Sleeper {
    at: u64,
    thread: Arc<Thread>,
    on_queue: bool,
}

static CPUS: [SpinLock<Cpu>; MAX_CPUS] = [const { SpinLock::new(Cpu::new()) }; MAX_CPUS];
/// Each CPU whose current and idle threads are set and whose timer is
/// started: its next tick sets `RUNNING`.
static ARMED: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// Whether each CPU's scheduler runs: armed, and its timer has ticked. Only
/// moves to true.
static RUNNING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// How many threads are on each CPU's run queue, so its idle loop can check
/// for work without the lock.
static READY: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
/// Each CPU's current thread, for the crash reporter (lock-free); only that
/// CPU stores it. It points into an `Arc` that CPU's scheduler holds:
/// `current`, or, during a switch, `schedule`'s `prev` local (until it is
/// set to the next thread, just before the switch itself) and then its
/// `next` local (until that moves into `current`); a crash never unwinds
/// them.
static CURRENT: [AtomicPtr<Thread>; MAX_CPUS] = [const { AtomicPtr::new(ptr::null_mut()) }; MAX_CPUS];
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The id the next thread made will get: every thread made from now on has
/// at least this id (ids only grow).
pub fn next_thread_id() -> u64 {
    NEXT_ID.load(Ordering::Relaxed)
}
/// Every thread not yet dropped (weakly: the registry keeps none alive).
/// Pruned of the dropped ones whenever a thread is created. Nothing else is
/// locked while it is held but the heap (which it allocates from, and which
/// takes the page tables' and the frame allocator's locks to grow), so it
/// must never be taken under the heap's, the page tables' or the frame
/// allocator's locks.
static THREADS: SpinLock<Vec<Weak<Thread>>> = SpinLock::new(Vec::new());
/// The id of the dead thread each CPU's [`finish_switch`] is still working
/// on (0: none; ids start at 1), cleared once it is done with it: its stack
/// freed, its joiners woken, their list freed, its reference dropped.
/// [`JoinHandle::join`] waits for this to clear, so nothing `finish_switch`
/// frees for a joined thread lands after `join` returns (#183: the joiners'
/// list, one 32-byte block freed after the wake, landed inside the next
/// ktest's heap accounting on loaded KVM), and the thread is no longer
/// listed by [`threads`] once `join` returns.
static FINISHING: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Ktests: the dead thread (0: none) whose [`finish_switch`], as it releases
/// it ([`Released`]), waits for its joiner to be past `join`'s wait for
/// `reaped` ([`PAST_REAPED`]) and then holds 20 ms, before its joiners' list
/// is freed and its reference dropped: #183's window, widened while the
/// joiner is surely running.
#[cfg(feature = "ktest")]
static HOLD_FINISHING: AtomicU64 = AtomicU64::new(0);
/// Ktests: the last thread whose joiner got past `join`'s wait for `reaped`.
#[cfg(feature = "ktest")]
static PAST_REAPED: AtomicU64 = AtomicU64::new(0);
/// Ktests: the last hold's joiners, how many (`u64::MAX`: no hold).
#[cfg(feature = "ktest")]
static HELD_JOINERS: AtomicU64 = AtomicU64::new(u64::MAX);
/// Ktests: whether the last hold saw its joiner past its wait (else it gave
/// up after 1 s).
#[cfg(feature = "ktest")]
static HELD_SAW_JOINER: AtomicBool = AtomicBool::new(false);
/// Ktests: how long the last hold lasted, past seeing its joiner (ns, by
/// `now_ns`).
#[cfg(feature = "ktest")]
static HELD_NS: AtomicU64 = AtomicU64::new(0);
/// Ticks each CPU's scheduler has taken (from its start), and how many of
/// them found the idle thread running. A tick is counted in `TICKED` before
/// `IDLE_TICKS` (whose increment releases it), and [`cpu_ticks`] reads them
/// the other way round (acquiring), so a reading never has more idle ticks
/// than ticks.
static TICKED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static IDLE_TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Ktests: the `now_ns` each CPU's last tick read, and the tick before
/// that. [`tick`] stores them once that CPU's scheduler runs, so
/// `LAST_TICK_NS` is 0 until the first tick that finds it running, and
/// `PREV_TICK_NS` until the one after. Each is written only by its own
/// CPU's tick and read on that CPU: `LAST_TICK_NS` by threads pinned there
/// (the deadline race's waiter) and by the tick itself (to shift it into
/// `PREV_TICK_NS`), `PREV_TICK_NS` by that CPU's tick (in
/// [`count_queue_deadline`]) and by the race's waiter, so `Relaxed` is
/// enough.
#[cfg(feature = "ktest")]
static LAST_TICK_NS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
#[cfg(feature = "ktest")]
static PREV_TICK_NS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Ktests: per CPU, the queue-wait deadline entries ([`Sleeper`]s
/// `on_queue`) its ticks fired; of those, the ones the tick before had
/// already read a time at or past (fired a tick late), and the ones fired
/// before their deadline (early). A queue deadline fires at the first tick
/// after it, so both are always 0:
/// - late: such an entry is pushed in `WaitQueue::block` with interrupts
///   off from before its check that the deadline hadn't passed, so every
///   tick on this CPU before the push read a time before the deadline, and
///   every later tick before the one that fires it saw the entry and didn't
///   fire it, so read a time before the deadline too;
/// - early: [`Cpu::wake_sleepers`] fires an entry only at `at <= now`, so
///   this guards that condition.
///
/// This assumes threads are pinned (the check, the push and the ticks run
/// on one CPU) and that `now_ns` is monotonic on a CPU (one TSC frequency,
/// set before any scheduler runs). Same writer and readers as
/// [`PREV_TICK_NS`] (the counts are read by threads pinned to that CPU).
#[cfg(feature = "ktest")]
static QUEUE_DEADLINES_FIRED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
#[cfg(feature = "ktest")]
static QUEUE_DEADLINES_LATE: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
#[cfg(feature = "ktest")]
static QUEUE_DEADLINES_EARLY: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

/// Ktests: count `s`, which this CPU's tick (having read `now`) is firing,
/// in the `QUEUE_DEADLINES_*` counts if it is a queue wait's.
#[cfg(feature = "ktest")]
fn count_queue_deadline(cpu: usize, s: &Sleeper, now: u64) {
    if s.on_queue {
        QUEUE_DEADLINES_FIRED[cpu].fetch_add(1, Ordering::Relaxed);
        if PREV_TICK_NS[cpu].load(Ordering::Relaxed) >= s.at {
            QUEUE_DEADLINES_LATE[cpu].fetch_add(1, Ordering::Relaxed);
        }
        if s.at > now {
            QUEUE_DEADLINES_EARLY[cpu].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Ktests: CPU `cpu`'s queue deadlines fired, late and early so far (read
/// on that CPU).
#[cfg(feature = "ktest")]
fn queue_deadlines(cpu: usize) -> [u64; 3] {
    [&QUEUE_DEADLINES_FIRED, &QUEUE_DEADLINES_LATE, &QUEUE_DEADLINES_EARLY].map(|c| c[cpu].load(Ordering::Relaxed))
}

/// Add `t` to [`THREADS`], dropping the entries of threads gone since.
/// Not under any other lock (it allocates).
fn register(t: Arc<Thread>) -> Arc<Thread> {
    let mut all = THREADS.lock();
    all.retain(|w| w.strong_count() > 0);
    all.push(Arc::downgrade(&t));
    t
}

/// A thread as [`threads`] saw it.
#[derive(Clone, Copy, Debug)]
pub struct ThreadInfo {
    pub id: u64,
    pub name: &'static str,
    pub cpu: usize,
    pub state: State,
    /// Timer ticks (1 ms each) that found it running.
    pub ticks: u64,
}

/// Every thread still referenced (by a join handle, the scheduler, a wait
/// queue), in the order they were created; that includes dead threads still
/// referenced.
pub fn threads() -> Vec<ThreadInfo> {
    // Collected first so the `Arc`s drop after `THREADS` is released (the
    // last one dropped drops a thread's fields; its record goes when
    // `register` prunes the `Weak`).
    let live: Vec<Arc<Thread>> = THREADS.lock().iter().filter_map(Weak::upgrade).collect();
    live.iter()
        .map(|t| ThreadInfo {
            id: t.id,
            name: t.name,
            cpu: t.cpu,
            state: t.state(),
            ticks: t.ticks.load(Ordering::Relaxed),
        })
        .collect()
}

/// A CPU's scheduler ticks, and those that found it idle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTicks {
    pub ticks: u64,
    pub idle: u64,
}

impl CpuTicks {
    /// The ticks taken since `earlier` (a reading of the same CPU). Each
    /// reading has idle <= ticks, but two readings' differences need not
    /// (an earlier reading's ticks may include idle ticks it didn't see):
    /// the idle ticks are clamped to the ticks.
    pub fn since(self, earlier: CpuTicks) -> CpuTicks {
        let ticks = self.ticks.saturating_sub(earlier.ticks);
        CpuTicks { ticks, idle: self.idle.saturating_sub(earlier.idle).min(ticks) }
    }

    /// How busy the CPU was over these ticks, in whole percent.
    pub fn busy_percent(self) -> u64 {
        lumen_time::busy_percent(self.ticks, self.idle)
    }
}

/// CPU `cpu`'s [`CpuTicks`] (zeros for a CPU index that doesn't exist).
pub fn cpu_ticks(cpu: usize) -> CpuTicks {
    match (TICKED.get(cpu), IDLE_TICKS.get(cpu)) {
        (Some(t), Some(i)) => {
            // Idle first (acquiring what `tick` released with it), then the
            // ticks, which are then at least as many as the idle ticks seen.
            let idle = i.load(Ordering::Acquire);
            CpuTicks { ticks: t.load(Ordering::Relaxed), idle }
        }
        _ => CpuTicks::default(),
    }
}
/// Set once [`init`] has stopped waiting for the schedulers to start: one
/// that starts after that says so.
static START_WAIT_OVER: AtomicBool = AtomicBool::new(false);

/// A CPU's run queue as the scheduler and its X-ray ring see it, read
/// together under the queue's lock ([`run_queue`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueView {
    /// Threads on the queue.
    pub len: usize,
    /// The length the ring's newest `runq` event for this queue says, and
    /// that event's sequence number; `None` if the ring holds none.
    pub traced: Option<(u64, u64)>,
}

/// CPU `cpu`'s [`QueueView`], read under its queue's lock, so nothing
/// changes the queue between the two readings (a change records its event
/// under the same lock). `None` past the CPUs Lumen supports. The ring's
/// scan takes no lock and allocates nothing.
pub fn run_queue(cpu: usize) -> Option<QueueView> {
    let q = CPUS.get(cpu)?.lock();
    let traced = trace::newest_run_queue(cpu).map(|(rq, seq)| (rq.len, seq));
    Some(QueueView { len: q.ready.len(), traced })
}

/// Whether CPU `cpu`'s scheduler runs (threads can be spawned there).
pub fn running_on(cpu: usize) -> bool {
    RUNNING.get(cpu).is_some_and(|r| r.load(Ordering::Acquire))
}

/// Ktests: `f`'s result, and how many of the running thread's queue waits
/// the timer ended at their deadline while it ran. 0 for a wait that
/// returned ready proves a waker ended it (or it never blocked), unless the
/// wake came after the deadline: its result can't, since it looks once more
/// at its deadline, so a lost wake still returns ready. Sound only if `f`
/// makes no other wait with a deadline on this thread.
#[cfg(feature = "ktest")]
pub(crate) fn deadline_wakes_in<R>(f: impl FnOnce() -> R) -> (R, u64) {
    let me = current().expect("ktests run on a thread");
    let count = || me.deadline_wakes.load(Ordering::Relaxed);
    let before = count();
    let r = f();
    (r, count() - before)
}

/// The thread running on this CPU, once its threads are set up.
pub fn current() -> Option<Arc<Thread>> {
    CPUS[arch::cpu_index()].lock().current.clone()
}

/// The running thread's id and name, for the crash report: lock-free, on
/// the CPU that crashed (whose current thread can't change any more). Set
/// from when this CPU's threads are set up (`sched::init` on the boot CPU,
/// coming online on an AP).
pub fn running_here() -> Option<(u64, &'static str)> {
    let t = CURRENT[arch::cpu_index()].load(Ordering::Acquire);
    // SAFETY: a non-null `CURRENT` points into an `Arc<Thread>` that stays
    // alive while it is stored there (see `CURRENT`).
    (!t.is_null()).then(|| unsafe { ((*t).id, (*t).name) })
}

/// Can the running thread block here? Only with interrupts on and
/// everything [`blockable_but_for_interrupts`] asks.
fn can_block(cpu: usize) -> bool {
    arch::irq::enabled() && blockable_but_for_interrupts(cpu)
}

/// Could the running thread block here if interrupts were on? For the crash
/// path (`crash::halt_if_stopping_elsewhere`), with interrupts off.
pub(crate) fn can_block_with_interrupts_on() -> bool {
    blockable_but_for_interrupts(arch::cpu_index())
}

/// Everything [`can_block`] asks but interrupts on: this CPU's scheduler
/// runs, no critical section is open (a lock held; it doesn't tell an
/// interrupt handler, which opens none, from a thread), and the running
/// thread isn't the idle thread (which must always be ready to run). In
/// that order: the run queue's lock is taken last, only outside every
/// critical section.
fn blockable_but_for_interrupts(cpu: usize) -> bool {
    running_on(cpu) && arch::irq::depth() == 0 && !CPUS[cpu].lock().on_idle()
}

/// Whether the running thread can block here (see [`can_block`]).
pub fn can_block_now() -> bool {
    can_block(arch::cpu_index())
}

/// Why [`spawn_on`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// That CPU's scheduler doesn't run (no such CPU, not online, before
    /// [`init`], or its timer doesn't tick).
    NotRunning,
    /// No stack for it.
    Stack(StackError),
    /// [`run_sized`] or [`scope`] on a thread that can't wait for another
    /// (an interrupt handler, a lock held, interrupts off, the idle thread).
    CantWait,
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpawnError::NotRunning => f.write_str("that CPU runs no scheduler"),
            SpawnError::Stack(e) => write!(f, "no stack: {e}"),
            SpawnError::CantWait => f.write_str("this thread can't wait for it"),
        }
    }
}

/// A spawned thread: wait for it to finish with [`join`](Self::join), or
/// drop it to let it run on its own.
#[must_use = "dropping the handle detaches the thread"]
pub struct JoinHandle {
    thread: Arc<Thread>,
}

impl JoinHandle {
    pub fn thread(&self) -> &Thread {
        &self.thread
    }

    /// Wait until the thread has finished and its CPU's [`finish_switch`]
    /// is done with it ([`FINISHING`]): its stack and its joiners' list
    /// freed, its CPU's reference dropped. Nothing that CPU frees for it
    /// lands after this returns (its record goes when a later spawn's
    /// [`register`] prunes the `Weak`). Blocks the calling thread (see
    /// [`WaitQueue::wait_until`] for where it can).
    pub fn join(self) {
        let t = &self.thread;
        assert!(!current().is_some_and(|c| Arc::ptr_eq(&c, t)), "thread {t:?} joining itself");
        t.exited.wait_until(|| t.reaped.load(Ordering::Acquire));
        #[cfg(feature = "ktest")]
        PAST_REAPED.store(t.id, Ordering::Release);
        // Normally done already; longer if that vCPU is paused, or held by a
        // ktest (`HOLD_FINISHING`).
        while FINISHING[t.cpu].load(Ordering::Acquire) == t.id {
            yield_now();
        }
    }
}

/// Start a thread running `f` on this CPU.
pub fn spawn(name: &'static str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, SpawnError> {
    spawn_on(arch::cpu_index(), name, f)
}

/// Run `f` in a new thread on this CPU with a stack of `stack_size` bytes
/// (a nonzero multiple of 4 KiB; anything else panics) instead of
/// [`STACK_SIZE`], and wait for it to finish: for deeply recursive code (the
/// script interpreter). As it is joined before this returns, `f` may borrow
/// from the caller. If no thread can be made, or the calling thread can't
/// block (see [`JoinHandle::join`]), `f` is dropped unrun.
pub fn run_sized<'a>(name: &'static str, stack_size: u64, f: impl FnOnce() + Send + 'a) -> Result<(), SpawnError> {
    // The SAFETY argument below needs a panic to end the kernel, not unwind
    // past the join.
    #[cfg(not(panic = "abort"))]
    compile_error!("sched::run_sized relies on panic = \"abort\"");
    let cpu = arch::cpu_index();
    if !can_block(cpu) {
        return Err(SpawnError::CantWait);
    }
    let f: Box<dyn FnOnce() + Send + 'a> = Box::new(f);
    // SAFETY: only the closure's lifetime bound changes (same layout). The
    // thread is joined below before this function returns (a join has no
    // timeout, and nothing kills a thread), so `f` (and what it borrows) is
    // used only while those borrows are alive; a panic aborts the kernel,
    // so nothing unwinds past the join. If spawning fails, `spawn_with`
    // drops `f` before returning.
    let f: Entry = unsafe { core::mem::transmute::<Box<dyn FnOnce() + Send + 'a>, Entry>(f) };
    spawn_with(cpu, name, stack_size, f)?.join();
    Ok(())
}

/// Run `f` with a [`Scope`] whose threads may borrow from the caller
/// (`'env`), and join every thread it started before returning
/// ([`run_sized`]'s argument applied to several threads): #157's Mandelbrot
/// window lends each CPU a band of its surface. `Err(CantWait)` (and `f`
/// unrun) if the calling thread can't block to join them. `f` must return
/// where the caller can still block (the joins follow it): with a lock
/// guard returned or left held, the join would panic. That is liveness, not
/// a safety obligation (the joins are never skipped).
pub fn scope<'env, R>(f: impl FnOnce(&Scope<'env>) -> R) -> Result<R, SpawnError> {
    // The SAFETY argument in `Scope::spawn_on` needs a panic to end the
    // kernel, not unwind past the joins.
    #[cfg(not(panic = "abort"))]
    compile_error!("sched::scope relies on panic = \"abort\"");
    if !can_block(arch::cpu_index()) {
        return Err(SpawnError::CantWait);
    }
    let scope = Scope { threads: core::cell::RefCell::new(Vec::new()), _env: core::marker::PhantomData };
    let out = f(&scope);
    for t in scope.threads.into_inner() {
        t.join();
    }
    Ok(out)
}

/// The threads of a [`scope`]: each joined before `scope` returns, so
/// they may borrow what outlives the call (`'env`). Not `Sync`: its
/// threads can't reach it (only the caller spawns).
pub struct Scope<'env> {
    threads: core::cell::RefCell<Vec<JoinHandle>>,
    /// Invariant in `'env`, as `std::thread::Scope` is. This is what makes
    /// `spawn_on` sound: if `Scope<'env>` were covariant (a
    /// `PhantomData<&'env ()>`), a `&Scope<'long>` would coerce to a
    /// `&Scope<'short>` inside `f`, and a thread could borrow a local of
    /// `f` that dies before the joins (a use after free; it compiles, and
    /// no test in the kernel would catch it). Invariance pins `'env` to
    /// the lifetime chosen at the `scope` call, which outlives the joins.
    _env: core::marker::PhantomData<&'env mut &'env ()>,
}

impl<'env> Scope<'env> {
    /// Start a thread running `f` on CPU `cpu` (see [`spawn_on`]), joined
    /// when the scope ends. If it can't be started, `f` is dropped unrun
    /// and the threads already started still run (and are joined).
    pub fn spawn_on(&self, cpu: usize, name: &'static str, f: impl FnOnce() + Send + 'env) -> Result<(), SpawnError> {
        let f: Box<dyn FnOnce() + Send + 'env> = Box::new(f);
        // SAFETY: only the closure's lifetime bound changes (same layout).
        // `scope` joins every thread pushed below before it returns (a join
        // has no timeout, and nothing kills a thread), and `'env` outlives
        // that call: it is a parameter of `scope`, `Scope` is invariant in
        // it (see `_env`: covariance would let `'env` shrink to a local of
        // `scope`'s closure), and `&Scope` can't leave that closure (its
        // lifetime is higher-ranked). So `f` and what it borrows are used
        // only while those borrows are alive; a panic aborts the kernel, so
        // nothing unwinds past the joins. If spawning fails, `spawn_with`
        // drops `f` before returning.
        let f: Entry = unsafe { core::mem::transmute::<Box<dyn FnOnce() + Send + 'env>, Entry>(f) };
        let thread = spawn_with(cpu, name, STACK_SIZE, f)?;
        self.threads.borrow_mut().push(thread);
        Ok(())
    }
}

/// Start a thread running `f` on CPU `cpu`, where it will always run. It is
/// queued behind the threads already ready there.
pub fn spawn_on(cpu: usize, name: &'static str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, SpawnError> {
    spawn_with(cpu, name, STACK_SIZE, Box::new(f))
}

fn spawn_with(cpu: usize, name: &'static str, stack_size: u64, f: Entry) -> Result<JoinHandle, SpawnError> {
    if !running_on(cpu) {
        return Err(SpawnError::NotRunning);
    }
    let thread = Thread::spawned(name, cpu, stack_size, f)?;
    {
        let mut q = CPUS[cpu].lock();
        q.home();
        q.push_ready(cpu, thread.clone());
    }
    let _ = smp::wake(cpu); // as in `enqueue`
    Ok(JoinHandle { thread })
}

/// Put a ready thread on its CPU's queue, and wake that CPU if it isn't this
/// one (it may be halted in its idle loop). If the wakeup IPI can't be sent,
/// the CPU's next tick finds the thread (its scheduler runs, so it ticks).
fn enqueue(thread: Arc<Thread>) {
    let cpu = thread.cpu;
    CPUS[cpu].lock().push_ready(cpu, thread);
    let _ = smp::wake(cpu);
}

/// Where every spawned thread starts (through `arch::context`), on its own
/// stack, switched to by [`schedule`] with interrupts off and no critical
/// section open.
extern "C" fn thread_entry() -> ! {
    finish_switch(arch::cpu_index());
    // Not `irq::enable`: once a crash report or a reboot has begun on
    // another CPU (interrupts off for good), they stay off here, and a
    // wait then halts this CPU (`crash::halt_if_stopping_elsewhere`) if
    // the report's stop NMI (or, for a reboot, the reset) hasn't already.
    arch::irq::restore(true);
    // No reference to itself is kept across the entry (`exit` never
    // returns, so it would never be dropped).
    let entry = current().expect("a thread runs on a CPU with a scheduler").entry.lock().take();
    entry.expect("a thread starts once")();
    exit()
}

/// Give the CPU to the next ready thread on it, if any; the running thread
/// stays ready and gets its turn again. Returns at once before the
/// scheduler runs on this CPU. Outside every critical section.
pub fn yield_now() {
    let cpu = arch::cpu_index();
    if !running_on(cpu) {
        return;
    }
    let were_on = arch::irq::disable();
    schedule(cpu);
    arch::irq::restore(were_on);
}

/// Yield this CPU until every thread in `threads` is blocked in a wait, or
/// until `deadline` (`time::now_ns`) has passed, if there is one: whether
/// they all were. Each check is one pass over them with interrupts off, so
/// nothing on this CPU (a timer tick, IRQ 1 waking the keyboard's readers)
/// changes a state between two reads: for threads on this CPU it is a
/// snapshot. A set that is all blocked returns true even past the deadline
/// (the check comes first). A `Dead` thread never counts as blocked, nor
/// does a `Sleeping` one. Needs a running scheduler on this CPU (yielding
/// is what lets threads here run), and the calling thread must not be in
/// `threads` (it is running, never blocked, so the wait wouldn't end).
///
/// `Blocked` is set only in [`WaitQueue`]'s wait, under the queue's lock,
/// right after the thread is queued: its last allocation for the wait is
/// done, and it allocates nothing more until woken. For threads on this
/// CPU, each one read as blocked has also switched away. The serial shell
/// waits this way for the screen shells, whose first wait is for a key,
/// before its first prompt (#119).
pub fn yield_until_blocked<'a>(threads: impl Iterator<Item = &'a JoinHandle> + Clone, deadline: Option<u64>) -> bool {
    loop {
        let were_on = arch::irq::disable();
        let all = threads.clone().all(|h| h.thread.state() == State::Blocked);
        arch::irq::restore(were_on);
        if all {
            return true;
        }
        if deadline.is_some_and(|d| time::now_ns() >= d) {
            return false;
        }
        yield_now();
    }
}

/// Block the running thread for at least `ns` nanoseconds (by `now_ns`): the
/// first tick on its CPU after the deadline makes it ready, and it runs when
/// its turn comes (at once if the CPU is idle, else after the running
/// thread's slice and the threads ahead of it; later where an emulator
/// delivers ticks late). Returns false, doing nothing, where it can't block
/// (see [`can_block`]): `time::sleep` busy-waits then.
#[must_use = "false means it didn't sleep"]
pub fn sleep(ns: u64) -> bool {
    let cpu = arch::cpu_index();
    if !can_block(cpu) {
        return false;
    }
    let deadline = time::now_ns().saturating_add(ns);
    let were_on = arch::irq::disable();
    {
        let mut q = CPUS[cpu].lock();
        let me = q.running().clone();
        me.change(State::Running, State::Sleeping);
        assert!(q.sleepers.len() < q.sleepers.capacity(), "thread {me:?} sleeping without room reserved");
        q.sleepers.push(Sleeper { at: deadline, thread: me, on_queue: false });
    }
    schedule(cpu);
    arch::irq::restore(were_on);
    true
}

/// End the running thread (a thread's entry returning does this). Its stack
/// is freed by the next thread on its CPU, and then its joiners wake. Only a
/// spawned thread can exit (not `main` or an idle thread; not tested).
pub fn exit() -> ! {
    let cpu = arch::cpu_index();
    assert!(running_on(cpu), "exit where no scheduler runs");
    let _ = arch::irq::disable();
    {
        let q = CPUS[cpu].lock();
        let me = q.running();
        assert!(me.stack.lock().is_some() && !q.on_idle(), "only a spawned thread can exit, not {me:?}");
        me.change(State::Running, State::Dead);
    }
    schedule(cpu);
    unreachable!("a dead thread was switched back to")
}

/// Switch from the running thread to the next ready one on CPU `cpu` (this
/// one), or its idle thread, or keep running it. The running thread goes
/// back on the queue only if it is still running and isn't the idle thread
/// (not if it is sleeping, blocked, dead, or already woken and queued again
/// by a waker). With
/// interrupts off and no critical section open; returns when the running
/// thread is switched back to.
fn schedule(cpu: usize) {
    assert!(!arch::irq::enabled() && arch::irq::depth() == 0, "switching threads with interrupts on or a lock held");
    let (save, load) = {
        let mut q = CPUS[cpu].lock();
        assert!(q.switched_from.is_none(), "the last switch on cpu {cpu} wasn't finished");
        let prev = q.current.take().expect("a running scheduler has a current thread");
        assert_eq!(prev.cpu, cpu, "thread {prev:?} isn't cpu {cpu}'s");
        let idle = q.is_idle_thread(&prev);
        // Only this CPU moves its running thread out of Running; one still
        // running (not the idle thread, never queued) goes back on the queue.
        let requeue = !idle && prev.state() == State::Running;
        if idle || requeue {
            prev.change(State::Running, State::Ready);
        }
        let next = if requeue && q.ready.is_empty() {
            // It would be queued and taken straight off again: it just
            // keeps running, recording no run-queue change (a thread
            // polling by yielding would fill the X-ray ring otherwise).
            prev.clone()
        } else {
            if requeue {
                q.push_ready(cpu, prev.clone());
            }
            match q.pop_ready(cpu) {
                Some(t) => t,
                None => q.idle.clone().expect("a running scheduler has an idle thread"),
            }
        };
        q.slice_left = SLICE_TICKS;
        assert_eq!(next.cpu, cpu, "thread {next:?} isn't cpu {cpu}'s");
        next.change(State::Ready, State::Running);
        if Arc::ptr_eq(&next, &prev) {
            q.current = Some(next);
            return;
        }
        // SAFETY: this CPU is both threads' home (asserted), interrupts are
        // off, and `next` isn't running.
        let (save, load) = unsafe { (prev.rsp.slot(), next.rsp.get()) };
        // Checked before `CURRENT` names `next`, so a crash here names `prev`,
        // whose stack it runs on.
        assert!(load != 0, "switching to thread {next:?}, which has no saved context");
        crate::ktrace!(Kind::Switch, prev.id, next.id, prev.state());
        CURRENT[cpu].store(Arc::as_ptr(&next).cast_mut(), Ordering::Release);
        q.current = Some(next);
        q.switched_from = Some(prev);
        (save, load)
    };
    // SAFETY: interrupts are off (asserted above); `save` is the running
    // thread's `rsp` slot, which `switched_from` keeps alive until the next
    // thread has finished the switch; `load` was saved by `switch` (or made
    // by `context::prepare`) on `next`'s stack, which stays mapped until it
    // dies and is switched away from, and which no CPU runs on (`next` is
    // this CPU's, and wasn't running).
    unsafe { arch::context::switch(save, load) };
    finish_switch(cpu);
}

/// Finish a switch on CPU `cpu`, on the thread switched to: let go of the
/// thread switched away from, and if it is dead, publish it in
/// [`FINISHING`], free its stack (on its own CPU, which ran on it: see the
/// module docs on the TLB), mark it reaped, wake its joiners, then free
/// their list and drop the reference ([`Released`]), and clear `FINISHING`
/// last. Interrupts off.
fn finish_switch(cpu: usize) {
    let prev = CPUS[cpu].lock().take_switched_from();
    let Some(prev) = prev.filter(|p| p.state() == State::Dead) else { return };
    // Published before `reaped`, so a joiner that sees `reaped` then sees
    // this id or a later store: the clearing below, or a later dead
    // thread's id (never its own). Release, so that a later id carries the
    // clearing's ordering too.
    FINISHING[cpu].store(prev.id, Ordering::Release);
    let stack = prev.stack.lock().take();
    drop(stack);
    prev.reaped.store(true, Ordering::Release);
    // The joiners, taken out and woken; their list (what they pushed
    // themselves onto) is freed, and the reference dropped, only after the
    // wakes, as `released` drops.
    let released = Released { joiners: prev.exited.take_waiters(), thread: prev };
    for j in &released.joiners {
        wake(j.clone());
    }
    drop(released);
    // Release: everything above happens before a joiner sees it cleared.
    // Must stay after `drop(released)`: a drop guard for this clear would run
    // before `released` drops (locals drop in reverse order), reopening #183.
    FINISHING[cpu].store(0, Ordering::Release);
}

/// What [`finish_switch`] releases for a dead thread, as this drops: its
/// joiners' list (#183's 32-byte block, for one joiner) and its reference.
/// In ktests, [`HOLD_FINISHING`]'s thread holds first ([`hold_if_asked`]), so
/// anything in `finish_switch` before `drop(released)` comes before the hold.
struct Released {
    joiners: VecDeque<Arc<Thread>>,
    // Held to be dropped here, after the joiners' list (read by ktests).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    thread: Arc<Thread>,
}

impl Drop for Released {
    fn drop(&mut self) {
        // The fields are dropped after this returns, so `FINISHING` must not
        // be cleared here: that would come before the list's free and the
        // reference drop, reopening #183, and no hold could show it (nothing
        // runs between the end of this and the free).
        #[cfg(feature = "ktest")]
        hold_if_asked(self);
    }
}

/// Ktests: if [`HOLD_FINISHING`] names `r`'s thread, wait (up to 1 s) for its
/// joiner to be past `join`'s wait for `reaped`, then hold 20 ms, noting how
/// many joiners there were and whether it saw that one.
#[cfg(feature = "ktest")]
fn hold_if_asked(r: &Released) {
    let id = r.thread.id;
    if HOLD_FINISHING.load(Ordering::Relaxed) != id {
        return;
    }
    let deadline = time::now_ns() + 1_000_000_000;
    while PAST_REAPED.load(Ordering::Acquire) != id && time::now_ns() < deadline {
        core::hint::spin_loop();
    }
    HELD_SAW_JOINER.store(PAST_REAPED.load(Ordering::Acquire) == id, Ordering::Relaxed);
    HELD_JOINERS.store(r.joiners.len() as u64, Ordering::Relaxed);
    let start = time::now_ns();
    time::spin(20_000_000);
    HELD_NS.store(time::now_ns() - start, Ordering::Relaxed);
}

/// The timer interrupt on this CPU (interrupts off, EOI sent): the first
/// tick of an armed CPU starts its scheduler; after that, count the tick
/// (against the running thread, and as idle if that is the idle thread),
/// wake the sleepers whose deadline has passed, and switch threads if
/// another is ready and the running one's slice is used up (or the idle
/// thread runs).
pub fn tick() {
    let cpu = arch::cpu_index();
    if !running_on(cpu) {
        if ARMED[cpu].load(Ordering::Acquire) {
            RUNNING[cpu].store(true, Ordering::Release);
            if START_WAIT_OVER.load(Ordering::Acquire) {
                info!(target: "sched", "cpu {cpu}'s scheduler started late (after the boot CPU stopped waiting)");
            }
        }
        return;
    }
    // Interrupts arrive only outside every critical section (they are off
    // inside).
    assert_eq!(arch::irq::depth(), 0, "a timer interrupt inside a critical section");
    let now = time::now_ns();
    #[cfg(feature = "ktest")]
    {
        PREV_TICK_NS[cpu].store(LAST_TICK_NS[cpu].load(Ordering::Relaxed), Ordering::Relaxed);
        LAST_TICK_NS[cpu].store(now, Ordering::Relaxed);
    }
    let switch = {
        let mut q = CPUS[cpu].lock();
        // Counted before waking anyone: the thread this tick interrupted.
        TICKED[cpu].fetch_add(1, Ordering::Relaxed);
        q.running().ticks.fetch_add(1, Ordering::Relaxed);
        if q.on_idle() {
            IDLE_TICKS[cpu].fetch_add(1, Ordering::Release);
        }
        q.wake_sleepers(cpu, now);
        q.slice_left = q.slice_left.saturating_sub(1);
        !q.ready.is_empty() && (q.slice_left == 0 || q.on_idle())
    };
    if switch {
        schedule(cpu);
    }
}

/// Threads waiting for something; whoever makes it happen wakes them.
/// Dropping one with threads waiting panics (they would never wake).
pub struct WaitQueue {
    waiters: SpinLock<VecDeque<Arc<Thread>>>,
}

impl WaitQueue {
    pub const fn new() -> WaitQueue {
        WaitQueue { waiters: SpinLock::new(VecDeque::new()) }
    }

    /// Block the running thread until `ready()`. `ready` is checked with the
    /// queue's lock held, so a waker that makes it true and then calls
    /// [`wake_one`](Self::wake_one) or [`wake_all`](Self::wake_all) can't be
    /// missed; it must not block or take a lock held around a wake. A thread
    /// woken while `ready()` is still false waits again, at the back. Panics
    /// where the thread can't block (see [`can_block`]), except where it
    /// can't because a crash report or a reboot on another CPU turned
    /// interrupts off for good: then it halts this CPU for it (see
    /// `crash::halt_if_stopping_elsewhere` for which cases it tells
    /// apart).
    pub fn wait_until(&self, ready: impl FnMut() -> bool) {
        let woke = self.block(None, ready);
        debug_assert!(woke, "a wait without a deadline returned unready");
    }

    /// [`wait_until`](Self::wait_until), but at most until `deadline`
    /// (`time::now_ns`): whether `ready()` held. A waker and the deadline
    /// may race to wake the thread: whichever moves it out of `Blocked`
    /// first queues it, and the other's entry is dropped (by that waker, by
    /// the timer, or by the thread itself once it runs). `ready()` is
    /// checked once more at the deadline. Like a sleep, the deadline is
    /// noticed at the first tick after it. Panics where the thread can't
    /// block, as `wait_until` does.
    pub fn wait_until_or_deadline(&self, deadline: u64, ready: impl FnMut() -> bool) -> bool {
        self.block(Some(deadline), ready)
    }

    /// Both waits: block until `ready()` (true) or `deadline` passes
    /// (false).
    fn block(&self, deadline: Option<u64>, mut ready: impl FnMut() -> bool) -> bool {
        let cpu = arch::cpu_index();
        if !can_block(cpu) {
            // Or halt for another CPU's crash report (#106).
            crate::crash::halt_if_stopping_elsewhere();
            panic!("blocking where a thread can't (no scheduler, interrupts off, a lock held, or the idle thread)");
        }
        let me = CPUS[cpu].lock().running().clone();
        loop {
            let were_on = arch::irq::disable();
            {
                let mut waiters = self.waiters.lock();
                let is_ready = ready();
                if is_ready || deadline.is_some_and(|d| time::now_ns() >= d) {
                    drop(waiters);
                    arch::irq::restore(were_on);
                    return is_ready;
                }
                // On the queue first, before the run queue's lock: pushing
                // may allocate, and that lock is kept short (the timer
                // takes it on every tick). No waker can take the entry
                // before the thread is blocked: the queue's lock is held
                // until then.
                waiters.push_back(me.clone());
                // From this push until the thread is woken nothing may
                // allocate for it: the serial prompt's readiness (#119,
                // `yield_until_blocked`) relies on a blocked thread having
                // made its last allocation for the wait. `Blocked` is set
                // only here, and the deadline's `sleepers.push` below uses
                // room reserved beforehand (asserted).

                // A waker on another CPU may make it ready (and queue it)
                // before this CPU switches away: `schedule` then neither
                // queues it again nor loses it. The timer can't (interrupts
                // are off here until the switch).
                me.change(State::Running, State::Blocked);
                if let Some(at) = deadline {
                    let mut q = CPUS[cpu].lock();
                    assert!(q.sleepers.len() < q.sleepers.capacity(), "thread {me:?} waiting without room reserved");
                    q.sleepers.push(Sleeper { at, thread: me.clone(), on_queue: true });
                }
            }
            schedule(cpu);
            // Normally its waker took it off. But it may have been woken
            // some other way (its deadline, or a waker holding an entry from
            // an earlier wait that a deadline ended): then its entries must
            // come off, so no waker or tick finds it later, blocked on
            // something else. (The timer can't run on this CPU before the
            // deadline entry is gone: interrupts are still off.)
            self.forget(&me);
            if deadline.is_some() {
                CPUS[cpu].lock().forget_deadline(&me);
            }
            arch::irq::restore(were_on);
        }
    }

    /// Take `me` off the queue, if it is still on.
    fn forget(&self, me: &Arc<Thread>) {
        self.waiters.lock().retain(|w| !Arc::ptr_eq(w, me));
    }

    /// Wake the longest-waiting thread; whether there was one (a thread
    /// already woken some other way, e.g. by its deadline, doesn't count,
    /// and the next is tried). A thread whose deadline won the race may
    /// still be on the queue, blocked again on the same queue by then: the
    /// wake then goes to it, ahead of threads that queued after it, which
    /// is a wake like any other (it rechecks its condition).
    pub fn wake_one(&self) -> bool {
        loop {
            let first = self.waiters.lock().pop_front();
            match first {
                None => return false,
                Some(t) => {
                    if wake(t) {
                        return true;
                    }
                }
            }
        }
    }

    /// The waiting threads, taken off the queue (not woken). For a caller
    /// that must free their list at a point of its own ([`finish_switch`]):
    /// [`WaitQueue::wake_all`] frees it after its wakes.
    fn take_waiters(&self) -> VecDeque<Arc<Thread>> {
        core::mem::take(&mut *self.waiters.lock())
    }

    /// Wake every waiting thread; how many it woke. The queue's list is
    /// freed after the wakes, here.
    pub fn wake_all(&self) -> usize {
        let all = core::mem::take(&mut *self.waiters.lock());
        all.into_iter().filter(|t| wake(t.clone())).count()
    }
}

impl Default for WaitQueue {
    fn default() -> WaitQueue {
        WaitQueue::new()
    }
}

impl Drop for WaitQueue {
    fn drop(&mut self) {
        assert!(self.waiters.lock().is_empty(), "a wait queue dropped with threads waiting on it");
    }
}

/// Make a thread taken off a wait queue ready: whether it did. It is
/// blocked unless something else woke it first (its deadline, or a waker
/// holding an entry from an earlier wait); then it isn't queued again, and
/// it takes its own entries off once it runs.
fn wake(thread: Arc<Thread>) -> bool {
    if !thread.try_change(State::Blocked, State::Ready) {
        return false;
    }
    enqueue(thread);
    true
}

/// Make `current` CPU `cpu`'s running thread and `idle` its idle thread.
fn install(cpu: usize, current: Arc<Thread>, idle: Arc<Thread>) {
    let mut q = CPUS[cpu].lock();
    CURRENT[cpu].store(Arc::as_ptr(&current).cast_mut(), Ordering::Release);
    q.current = Some(current);
    q.idle = Some(idle);
}

/// Set up the boot CPU's threads: `kmain`'s context becomes the `main`
/// thread, and the boot CPU gets an idle thread. Then wake the other CPUs
/// to start their timers (each AP's idle loop does once the timer count is
/// published) and wait, bounded, until every CPU's scheduler runs (from its
/// timer's first tick). After `time::init` and `smp::wait_online`, with
/// interrupts on.
pub fn init() {
    assert_eq!(arch::cpu_index(), 0, "sched::init is for the boot CPU");
    assert!(time::timer_published(), "sched::init before time::init");
    assert!(arch::irq::enabled(), "sched::init with interrupts off: no tick could start a scheduler");
    let idle = Thread::spawned("idle", 0, STACK_SIZE, Box::new(|| idle_loop(0, || false, || {})))
        .unwrap_or_else(|e| panic!("the boot CPU's idle thread: {e}"));
    install(0, Thread::adopted("main"), idle);
    CPUS[0].lock().home(); // `main`: it can sleep and block too
    // The boot CPU's timer already runs (`time::init`).
    ARMED[0].store(true, Ordering::Release);
    let aps: Vec<usize> = (1..MAX_CPUS).filter(|&c| smp::online_lapic_id(c).is_some()).collect();
    for &cpu in &aps {
        if !smp::wake(cpu) {
            warn!(target: "sched", "cpu {cpu}: the wakeup IPI wasn't sent; its scheduler starts only once it wakes");
        }
    }
    let cpus: Vec<usize> = core::iter::once(0).chain(aps).collect();
    let deadline = time::now_ns() + START_TIMEOUT_NS;
    while !cpus.iter().all(|&c| running_on(c)) && time::now_ns() < deadline {
        core::hint::spin_loop();
    }
    START_WAIT_OVER.store(true, Ordering::Release);
    for &cpu in cpus.iter().filter(|&&c| !running_on(c)) {
        let why =
            if ARMED[cpu].load(Ordering::Acquire) { "its timer doesn't tick" } else { "it didn't start its timer" };
        warn!(target: "sched", "cpu {cpu}'s scheduler didn't start in 1 s ({why}): no threads run there");
    }
    let n = cpus.iter().filter(|&&c| running_on(c)).count();
    info!(target: "sched", "threads on {n} CPU{}, {SLICE_TICKS} ms slices", if n == 1 { "" } else { "s" });
}

/// An AP's life from `smp::ap_main` on: its running context becomes its idle
/// thread, which starts its timer (and with the first tick its scheduler)
/// once the boot CPU publishes the timer's count, yields to ready threads, and
/// runs the job `run_job` takes whenever `has_job()` (`smp::run_on`'s
/// mailbox, from these frames, so a crash in a job backtraces through
/// `ap_main`).
pub fn run_ap_idle(has_job: impl Fn() -> bool, run_job: impl Fn()) -> ! {
    let cpu = arch::cpu_index();
    let me = Thread::adopted("idle");
    install(cpu, me.clone(), me);
    idle_loop(cpu, has_job, run_job);
    unreachable!("an idle loop returned")
}

/// A CPU's idle thread: halt until there is work (a ready thread, a job, or
/// for an AP whose timer isn't started yet, the timer's count), then do it.
/// It never blocks: a job that sleeps busy-waits, and one that waits on a
/// queue panics.
fn idle_loop(cpu: usize, has_job: impl Fn() -> bool, run_job: impl Fn()) {
    loop {
        if !ARMED[cpu].load(Ordering::Acquire) {
            let started = {
                let _off = arch::irq::push_off();
                time::start_timer_here()
            };
            if started {
                ARMED[cpu].store(true, Ordering::Release);
            }
        }
        // Checked with interrupts off: a wakeup can't slip in before the halt.
        arch::irq::wait_unless(|| {
            READY[cpu].load(Ordering::Acquire) > 0
                || has_job()
                || (!ARMED[cpu].load(Ordering::Acquire) && time::timer_published())
        });
        run_job();
        yield_now();
    }
}

// ---------------------------------------------------------------------------
// Tests

/// The CPUs whose scheduler runs.
#[cfg(feature = "ktest")]
fn running_cpus() -> Vec<usize> {
    (0..MAX_CPUS).filter(|&c| running_on(c)).collect()
}

/// How long a test waits for threads to make progress before failing (no
/// test waits this long when it passes). Waits on the threads' progress are
/// bounded by it, so a thread that never runs fails the test instead of
/// hanging it.
#[cfg(feature = "ktest")]
const TEST_DEADLINE_NS: u64 = 10_000_000_000;

/// `spawn_on` for tests: the error as a diag line and a test failure.
#[cfg(feature = "ktest")]
fn spawn_test(cpu: usize, name: &'static str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, &'static str> {
    spawn_on(cpu, name, f).map_err(|e| {
        crate::ktest::diag(format_args!("spawning {name} on cpu {cpu}: {e}"));
        "spawn failed"
    })
}

/// Yield until `done()` or the test deadline; whether `done()`.
#[cfg(feature = "ktest")]
pub(crate) fn yield_until(done: impl Fn() -> bool) -> bool {
    let deadline = time::now_ns() + TEST_DEADLINE_NS;
    while !done() {
        if time::now_ns() > deadline {
            return false;
        }
        yield_now();
    }
    true
}

/// `join`, failing instead of hanging if the thread isn't reaped, or its
/// CPU done with it ([`FINISHING`]), by the test deadline.
#[cfg(feature = "ktest")]
pub(crate) fn join_bounded(h: JoinHandle) -> crate::ktest::TestResult {
    let t = h.thread.clone();
    if !yield_until(|| t.reaped.load(Ordering::Acquire)) {
        crate::ktest::diag(format_args!("{t:?} wasn't reaped"));
        return Err("a thread didn't finish");
    }
    if !yield_until(|| FINISHING[t.cpu].load(Ordering::Acquire) != t.id) {
        crate::ktest::diag(format_args!("cpu {} is still finishing {t:?}", t.cpu));
        return Err("a thread's CPU didn't let go of it");
    }
    h.join();
    Ok(())
}

/// The `args` (previous id, next id, previous state) of each switch this
/// CPU's ring holds from `start` on.
#[cfg(feature = "ktest")]
fn switches_since(start: u64) -> impl Iterator<Item = [u64; 4]> {
    let ring = crate::trace::ring();
    (start..ring.written()).filter_map(|seq| ring.read(seq)).filter(|e| e.kind == Kind::Switch as u16).map(|e| e.args)
}

/// Whether the current CPU's ring has, from `start` on, a switch away from
/// thread `id` in state `state`.
#[cfg(feature = "ktest")]
fn switched_away(start: u64, id: u64, state: State) -> bool {
    switches_since(start).any(|a| a[0] == id && a[2] == state as u64)
}

/// The last CPU whose scheduler runs (an AP, in the ktests' 4-CPU boot).
#[cfg(feature = "ktest")]
fn last_cpu() -> Result<usize, &'static str> {
    running_cpus().last().copied().ok_or("no CPU runs a scheduler")
}

crate::ktest!(
    fn busy_and_idle_ticks_are_counted() {
        // On the last CPU: a thread spinning there until the CPU has taken
        // TICKS ticks reads them busy (it reads the CPU's ticks itself at its
        // start and end, so the span is only its spinning); then, with
        // nothing to run there, the CPU's next TICKS ticks read idle; and
        // each span's ticks, and its idle ticks, agree with its timer
        // interrupts (to within one at either end). Counted in ticks, not
        // over a fixed time: a loaded host delivers a vCPU it stalls ticks
        // at less than 1 kHz (#134: 18 in a 100 ms spin, and 12 in a 100 ms
        // sleep), which says nothing about how they're counted. TICKS is
        // what 100 ms gives at the full rate, so the shares keep the
        // granularity they had then. DEADLINE_NS only keeps a CPU whose
        // ticks stopped, or slowed below TICKS in DEADLINE_NS (20 a second),
        // from hanging the run: a hang guard, not a rate check (an AP's timer
        // is checked for the boot CPU's count by
        // `ap_timers_are_programmed_for_1_khz`, and that count's rate against
        // the HPET by `time::the_timer_is_programmed_for_1_khz`); half the
        // test deadline, so the spinner is done before `join_bounded` gives
        // up on it.
        use crate::kassert;
        use core::sync::atomic::{Ordering::SeqCst, compiler_fence};
        const TICKS: u64 = time::TICK_HZ / 10;
        const DEADLINE_NS: u64 = TEST_DEADLINE_NS / 2;
        /// A span of the CPU's ticks, from `take_ticks`.
        #[derive(Clone, Copy)]
        struct Span {
            counted: CpuTicks,
            /// Idle ticks counted, unclamped (`CpuTicks::since` caps them at
            /// the ticks).
            idle: u64,
            /// How long it took (reported only: the host decides it).
            took_ns: u64,
            /// The fewest and most ticks its timer interrupts allow.
            low: u64,
            high: u64,
        }
        impl Span {
            /// Whether it took its ticks, as many as its interrupts allow,
            /// and at most one more idle tick than ticks.
            fn holds(&self) -> bool {
                let ticks = self.counted.ticks;
                ticks >= TICKS && (self.low..=self.high).contains(&ticks) && self.idle <= ticks + 1
            }
        }
        let cpu = last_cpu()?;
        // On the measured CPU the test thread's spinning would make the idle
        // span busy; elsewhere, it spins through the idle span too, so a
        // count read from the wrong CPU, its own, reads busy and fails.
        if cpu == arch::cpu_index() {
            crate::ktest::diag(format_args!("the test runs on cpu {cpu}, the last one"));
            return Err("busy_and_idle needs a CPU other than the test's");
        }
        // The CPU's ticks from now until it has taken TICKS, or DEADLINE_NS
        // has passed, spinning between readings. Each end's reading sits
        // between two reads of the CPU's timer interrupts (`idt::ticks_on`:
        // the handler counts each before `sched::tick` counts it, interrupts
        // off between, so at most one is ever between the two counts; the
        // fences keep the reads in that order). Every interrupt between the
        // inner reads is a counted tick, but for one maybe still in its
        // handler at the end; every counted tick an interrupt between the
        // outer ones, but for one taken just before the first and counted
        // after the start reading. So the ticks counted are within those,
        // however long the host stalls anything in between. (The ±1 only
        // matters for a tick mid-handler at a read, which this can't make
        // happen on purpose.)
        let take_ticks = move || {
            let bracketed = || {
                let before = arch::idt::ticks_on(cpu);
                compiler_fence(SeqCst);
                let ticks = cpu_ticks(cpu);
                compiler_fence(SeqCst);
                (before, ticks, arch::idt::ticks_on(cpu))
            };
            let (i0, start, i1) = bracketed();
            let started_ns = time::now_ns();
            let deadline = started_ns + DEADLINE_NS;
            while cpu_ticks(cpu).since(start).ticks < TICKS && time::now_ns() < deadline {
                core::hint::spin_loop();
            }
            let (i2, end, i3) = bracketed();
            Span {
                counted: end.since(start),
                idle: end.idle.saturating_sub(start.idle),
                took_ns: time::now_ns() - started_ns,
                low: i2.saturating_sub(i1).saturating_sub(1),
                high: i3.saturating_sub(i0) + 1,
            }
        };
        let check = |what: &str, s: Span| {
            crate::ktest::diag(format_args!(
                "cpu {cpu}: {what} {}% busy of {} ticks ({} idle), in {} ms (until {TICKS} ticks, or {} ms); \
                 timer interrupts allow {} to {}",
                s.counted.busy_percent(),
                s.counted.ticks,
                s.idle,
                s.took_ns / 1_000_000,
                DEADLINE_NS / 1_000_000,
                s.low,
                s.high,
            ));
            s.holds()
        };
        let span = Arc::new(SpinLock::new(None));
        let result = span.clone();
        let spinner = spawn_test(cpu, "busy-test", move || *result.lock() = Some(take_ticks()))?;
        join_bounded(spinner)?;
        let busy: Span = span.lock().ok_or("the spinner didn't report")?;
        // The spinner is always ready there, so no tick of its span finds the
        // CPU idle: none counted idle (a floor would let a few through).
        kassert!(check("spinning", busy) && busy.idle == 0);
        let idle = take_ticks();
        kassert!(check("idle", idle) && idle.counted.busy_percent() <= 20);
        Ok(())
    }
);

/// Spawn two threads on `cpu` that never yield, and check that they
/// interleave: each counts its own loops, and runs until both have seen the
/// other's count move on [`ROUNDS`] separate occasions (so each was switched
/// away from and back to that often, by preemption alone). Each also checks
/// it always runs on `cpu`.
#[cfg(feature = "ktest")]
fn interleave_on(cpu: usize) -> crate::ktest::TestResult {
    use crate::kassert;
    const ROUNDS: u64 = 3;
    #[derive(Default)]
    struct Shared {
        counts: [AtomicU64; 2],
        seen: [AtomicU64; 2],
        wrong_cpu: AtomicBool,
    }
    let shared = Arc::new(Shared::default());
    let deadline = time::now_ns() + TEST_DEADLINE_NS;
    let spin = |me: usize, s: Arc<Shared>| {
        move || {
            let other = 1 - me;
            let mut last = s.counts[other].load(Ordering::Relaxed);
            while s.seen.iter().any(|n| n.load(Ordering::Relaxed) < ROUNDS) && time::now_ns() <= deadline {
                if arch::cpu_index() != cpu {
                    s.wrong_cpu.store(true, Ordering::Relaxed);
                }
                s.counts[me].fetch_add(1, Ordering::Relaxed);
                let now = s.counts[other].load(Ordering::Relaxed);
                if now != last {
                    last = now;
                    s.seen[me].fetch_add(1, Ordering::Relaxed);
                }
                core::hint::spin_loop();
            }
        }
    };
    let a = spawn_test(cpu, "spin-a", spin(0, shared.clone()))?;
    let b = spawn_test(cpu, "spin-b", spin(1, shared.clone()))?;
    join_bounded(a)?;
    join_bounded(b)?;
    let seen = [shared.seen[0].load(Ordering::Relaxed), shared.seen[1].load(Ordering::Relaxed)];
    crate::ktest::diag(format_args!("cpu {cpu}: each saw the other run {seen:?} times"));
    kassert!(seen.iter().all(|&n| n >= ROUNDS));
    kassert!(!shared.wrong_cpu.load(Ordering::Relaxed));
    Ok(())
}

crate::ktest!(
    fn two_threads_interleave() {
        // On this CPU, alongside `main`: preemption by the timer alone
        // switches between two threads that never yield.
        interleave_on(arch::cpu_index())
    }
);

crate::ktest!(
    fn threads_run_on_every_cpu() {
        // Every online CPU runs a scheduler; two non-yielding threads on
        // each interleave there (so every CPU's timer preempts), and never
        // run anywhere else.
        use crate::{kassert, kassert_eq};
        let cpus = running_cpus();
        let online: Vec<usize> = (0..MAX_CPUS).filter(|&c| smp::online_lapic_id(c).is_some()).collect();
        crate::ktest::diag(format_args!("schedulers on cpus {cpus:?}"));
        kassert_eq!(cpus, online);
        kassert!(cpus.len() > 1);
        for cpu in cpus {
            interleave_on(cpu)?;
        }
        Ok(())
    }
);

crate::ktest!(
    fn slices_are_10_ticks() {
        // On an AP (nothing else ready there), two non-yielding threads each
        // note the CPU's tick count whenever they see the other has run, so
        // one thread's turns are 2 slices (20 ticks) apart. Over 10 of them
        // (from its second turn: the first may follow a partial slice) they
        // span 200 ticks, give or take 6: a mark can be read a tick or so
        // late when a tick is pending as the thread resumes, never early.
        // Counted in ticks, not time, so late emulated ticks don't matter.
        use crate::{kassert, kassert_eq};
        const MARKS: usize = 12;
        #[derive(Default)]
        struct Shared {
            counts: [AtomicU64; 2],
            marks: [[AtomicU64; MARKS]; 2],
            taken: [AtomicUsize; 2],
        }
        kassert_eq!(SLICE_TICKS, 10);
        let cpu = last_cpu()?;
        kassert!(cpu != arch::cpu_index());
        let s = Arc::new(Shared::default());
        let deadline = time::now_ns() + TEST_DEADLINE_NS;
        let mark = |me: usize, s: Arc<Shared>| {
            move || {
                let other = 1 - me;
                let mut last = s.counts[other].load(Ordering::Relaxed);
                while s.taken.iter().any(|n| n.load(Ordering::Relaxed) < MARKS) && time::now_ns() <= deadline {
                    s.counts[me].fetch_add(1, Ordering::Relaxed);
                    let now = s.counts[other].load(Ordering::Relaxed);
                    if now != last {
                        last = now;
                        let i = s.taken[me].load(Ordering::Relaxed);
                        if i < MARKS {
                            s.marks[me][i].store(arch::idt::ticks_on(cpu), Ordering::Relaxed);
                            s.taken[me].store(i + 1, Ordering::Relaxed);
                        }
                    }
                    core::hint::spin_loop();
                }
            }
        };
        let a = spawn_test(cpu, "slice-a", mark(0, s.clone()))?;
        let b = spawn_test(cpu, "slice-b", mark(1, s.clone()))?;
        join_bounded(a)?;
        join_bounded(b)?;
        kassert_eq!(s.taken[0].load(Ordering::Relaxed), MARKS);
        let marks: Vec<u64> = s.marks[0].iter().map(|m| m.load(Ordering::Relaxed)).collect();
        let gaps: Vec<u64> = marks.windows(2).map(|w| w[1] - w[0]).collect();
        let span = marks[MARKS - 1] - marks[1];
        crate::ktest::diag(format_args!("ticks between slice-a's turns: {gaps:?} (10 turns: {span})"));
        kassert!(span.abs_diff(20 * u64::from(SLICE_TICKS)) <= 6);
        Ok(())
    }
);

crate::ktest!(
    fn ap_timers_are_programmed_for_1_khz() {
        // Every AP's local APIC timer runs periodic on the timer vector with
        // the boot CPU's calibrated 1 kHz count (read on each AP, from a
        // `run_on` job). The boot CPU's is checked against the HPET by
        // `time::the_timer_is_programmed_for_1_khz`.
        use crate::{kassert, kassert_eq};
        static LVT: AtomicU64 = AtomicU64::new(0);
        static INITIAL: AtomicU64 = AtomicU64::new(0);
        static READ: AtomicBool = AtomicBool::new(false);
        let timer = time::lapic_timer().ok_or("timer not calibrated")?;
        let count = lumen_time::periodic_count(timer.hz(), time::TICK_HZ).ok_or("no 1 kHz count")?;
        let aps: Vec<usize> = running_cpus().into_iter().filter(|&c| c != 0).collect();
        kassert!(!aps.is_empty());
        for cpu in aps {
            READ.store(false, Ordering::Release);
            smp::run_on_test(cpu, || {
                let (lvt, _, initial) = arch::apic::timer_registers();
                LVT.store(u64::from(lvt), Ordering::Relaxed);
                INITIAL.store(u64::from(initial), Ordering::Relaxed);
                READ.store(true, Ordering::Release);
            })?;
            kassert!(yield_until(|| READ.load(Ordering::Acquire)));
            let lvt = LVT.load(Ordering::Relaxed);
            kassert_eq!((lvt & 0xff, lvt & (1 << 16), lvt & (1 << 17)), (u64::from(arch::idt::TIMER), 0, 1 << 17));
            kassert_eq!(INITIAL.load(Ordering::Relaxed), u64::from(count));
        }
        Ok(())
    }
);

crate::ktest!(
    fn sleeping_thread_wakes() {
        // A thread sleeping 20 ms blocks (its CPU switches away from it,
        // sleeping) and wakes no earlier than its deadline.
        use crate::kassert;
        let start = crate::trace::ring().written();
        let elapsed = Arc::new(AtomicU64::new(0));
        let out = elapsed.clone();
        let h = spawn_test(arch::cpu_index(), "sleeper", move || {
            let t0 = time::now_ns();
            time::sleep(20_000_000);
            out.store(time::now_ns() - t0, Ordering::Relaxed);
        })?;
        let id = h.thread().id();
        join_bounded(h)?;
        let ns = elapsed.load(Ordering::Relaxed);
        crate::ktest::diag(format_args!("slept {ns} ns"));
        kassert!(switched_away(start, id, State::Sleeping));
        kassert!(ns >= 20_000_000);
        Ok(())
    }
);

crate::ktest!(
    fn time_sleep_spins_where_a_thread_cannot_block() {
        // Where a thread can't block, `time::sleep` busy-waits: the whole
        // time passes and the CPU switches no thread (with a lock held, and
        // with interrupts off), and on an AP's idle thread (a `run_on` job)
        // the whole time passes (the idle thread blocking would trip
        // `schedule`'s checks; its ring is another CPU's).
        use crate::kassert;
        static LOCK: SpinLock<()> = SpinLock::new(());
        static ELAPSED: AtomicU64 = AtomicU64::new(0);
        const NS: u64 = 2_000_000;
        let ring = crate::trace::ring();
        let switched = |from: u64| switches_since(from).next().is_some();
        let (w0, t0) = (ring.written(), time::now_ns());
        {
            let _held = LOCK.lock();
            time::sleep(NS);
        }
        kassert!(time::now_ns() - t0 >= NS && !switched(w0));
        let were_on = arch::irq::disable();
        let (w1, t1) = (ring.written(), time::now_ns());
        time::sleep(NS);
        let spun = time::now_ns() - t1 >= NS && !switched(w1);
        arch::irq::restore(were_on);
        kassert!(spun);
        let cpu = last_cpu()?;
        ELAPSED.store(0, Ordering::Relaxed);
        smp::run_on_test(cpu, || {
            let t = time::now_ns();
            time::sleep(NS);
            // + 1: 0 means "not yet", so `> NS` below means it took `>= NS`.
            ELAPSED.store(time::now_ns() - t + 1, Ordering::Release);
        })?;
        kassert!(yield_until(|| ELAPSED.load(Ordering::Acquire) != 0));
        kassert!(ELAPSED.load(Ordering::Acquire) > NS);
        Ok(())
    }
);

crate::ktest!(
    fn yielding_with_interrupts_off_keeps_them_off() {
        // A switch away and back restores this thread's own interrupt state,
        // not the other thread's: `other` (interrupts on) runs during the
        // yield, which is made only once it is waiting for its turn.
        use crate::kassert;
        let (armed, ran) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (a, r) = (armed.clone(), ran.clone());
        let other = spawn_test(arch::cpu_index(), "other", move || {
            while !a.load(Ordering::Acquire) {
                yield_now();
            }
            r.store(true, Ordering::Release);
        })?;
        let were_on = arch::irq::disable();
        armed.store(true, Ordering::Release);
        yield_now();
        let still_off = !arch::irq::enabled();
        let switched = ran.load(Ordering::Acquire);
        arch::irq::restore(were_on);
        join_bounded(other)?;
        kassert!(were_on && still_off && switched);
        Ok(())
    }
);

crate::ktest!(
    fn wait_queue_wakes_a_blocked_thread() {
        // A thread waiting on a queue blocks until its condition holds and
        // it is woken: on this CPU, and from another (the remote wake path:
        // its home's queue and a wakeup IPI). Woken without its condition,
        // it checks again (counted) and blocks again.
        use crate::{kassert, kassert_eq};
        #[derive(Default)]
        struct Shared {
            queue: WaitQueue,
            go: AtomicBool,
            checks: AtomicU64,
            passed: AtomicBool,
        }
        let mut cpus = running_cpus();
        cpus.truncate(2);
        for cpu in cpus {
            let s = Arc::new(Shared::default());
            let t = s.clone();
            let h = spawn_test(cpu, "waiter", move || {
                t.queue.wait_until(|| {
                    t.checks.fetch_add(1, Ordering::AcqRel);
                    t.go.load(Ordering::Acquire)
                });
                t.passed.store(true, Ordering::Release);
            })?;
            let blocked = || h.thread().state() == State::Blocked && !s.queue.waiters.lock().is_empty();
            kassert!(yield_until(blocked));
            kassert_eq!(s.checks.load(Ordering::Acquire), 1);
            kassert!(s.queue.wake_one());
            kassert!(yield_until(|| blocked() && s.checks.load(Ordering::Acquire) == 2));
            kassert!(!s.passed.load(Ordering::Acquire));
            s.go.store(true, Ordering::Release);
            kassert_eq!(s.queue.wake_all(), 1);
            join_bounded(h)?;
            kassert!(s.passed.load(Ordering::Acquire));
            kassert!(!s.queue.wake_one());
        }
        Ok(())
    }
);

crate::ktest!(
    fn wait_queue_ping_pong_across_cpus_loses_no_wakeup() {
        // Two threads on two other CPUs hand a turn back and forth through
        // wait queues, 3000 times each: wakes often land while the woken
        // thread is still switching out. A lost wakeup stalls them (caught by
        // the deadline); a thread queued twice trips `schedule`'s checks. The
        // condition is slow on purpose: checked under the queue's lock that
        // costs nothing, but a check outside it would leave a wide window.
        use crate::kassert;
        const ROUNDS: u64 = 3000;
        #[derive(Default)]
        struct Shared {
            queues: [WaitQueue; 2],
            turn: AtomicU64,
            rounds: AtomicU64,
        }
        let cpus = running_cpus();
        kassert!(cpus.len() >= 3);
        let s = Arc::new(Shared::default());
        let player = |me: u64, s: Arc<Shared>| {
            move || {
                let (mine, theirs) = (me as usize, 1 - me as usize);
                for _ in 0..ROUNDS {
                    s.queues[mine].wait_until(|| {
                        let mine = s.turn.load(Ordering::Acquire) == me;
                        (0..100).for_each(|_| core::hint::spin_loop());
                        mine
                    });
                    s.rounds.fetch_add(1, Ordering::Relaxed);
                    s.turn.store(1 - me, Ordering::Release);
                    s.queues[theirs].wake_all();
                }
            }
        };
        let a = spawn_test(cpus[1], "ping", player(0, s.clone()))?;
        let b = spawn_test(cpus[2], "pong", player(1, s.clone()))?;
        let (ra, rb) = (join_bounded(a), join_bounded(b));
        crate::ktest::diag(format_args!("{} of {} rounds", s.rounds.load(Ordering::Relaxed), 2 * ROUNDS));
        ra?;
        rb?;
        Ok(())
    }
);

crate::ktest!(
    fn switches_are_traced() {
        // Switching to a thread and from it (dead) are both in the ring.
        use crate::kassert;
        let start = crate::trace::ring().written();
        let h = spawn_test(arch::cpu_index(), "traced", || {})?;
        let id = h.thread().id();
        join_bounded(h)?;
        let me = current().ok_or("no current thread")?.id();
        kassert!(switches_since(start).any(|a| a[0] == me && a[1] == id));
        kassert!(switched_away(start, id, State::Dead));
        Ok(())
    }
);

crate::ktest!(
    fn a_dead_threads_stack_is_freed() {
        // Once `join` returns, the thread's stack is unregistered and its
        // pages unmapped (its bottom page checked): for a thread on this CPU
        // and one on another (joined from here), 130 times over (more than
        // the registry holds, so a stack that isn't freed fails the spawns).
        use crate::kassert;
        /// Lets the thread finish when dropped, even if a check fails first
        /// (else it would yield forever, starving its CPU's idle thread).
        struct Go(Arc<AtomicBool>);
        impl Drop for Go {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let cpus = [arch::cpu_index(), last_cpu()?];
        for round in 0..130 {
            let cpu = cpus[round % 2];
            let go = Go(Arc::new(AtomicBool::new(false)));
            let g = go.0.clone();
            // It waits until its stack has been looked at.
            let h = spawn_test(cpu, "brief", move || {
                while !g.load(Ordering::Acquire) {
                    yield_now();
                }
            })?;
            let range = h.thread().stack().ok_or("a spawned thread has a stack")?;
            kassert!(arch::stacks::stack_of(range.bottom).is_some());
            let thread = h.thread.clone();
            drop(go);
            join_bounded(h)?;
            kassert!(thread.state() == State::Dead && thread.stack().is_none());
            kassert!(arch::stacks::stack_of(range.bottom).is_none());
            let bottom = lumen_mm::VirtAddr::new(range.bottom).ok_or("a stack address is canonical")?;
            kassert!(crate::mm::paging::translate(bottom).is_none());
        }
        Ok(())
    }
);

crate::ktest!(
    fn a_thread_dying_into_a_fresh_one_is_reaped() {
        // `a` dies and `b`, never run yet, runs next: `b`'s start finishes
        // the switch and frees `a`. (A tick between the spawns can make `a`
        // die into `main` instead; it is tried until the trace shows the
        // switch from `a`, dead, to `b`, at most 20 times.)
        use crate::kassert;
        let mut seen = false;
        for _ in 0..20 {
            let start = crate::trace::ring().written();
            let a = spawn_test(arch::cpu_index(), "brief-a", || {})?;
            let b = spawn_test(arch::cpu_index(), "brief-b", || {})?;
            let (ia, ib) = (a.thread().id(), b.thread().id());
            join_bounded(a)?;
            join_bounded(b)?;
            if switches_since(start).any(|s| s == [ia, ib, State::Dead as u64, 0]) {
                seen = true;
                break;
            }
        }
        kassert!(seen);
        Ok(())
    }
);

crate::ktest!(
    fn join_blocks_until_the_thread_is_reaped() {
        // `join` itself (not `join_bounded`, which polls first) blocks the
        // caller until the reaping wakes it: for a thread on this CPU and one
        // on another. A watchdog on a third CPU wakes the joiners after 2 s,
        // so a missing wake fails instead of hanging.
        use crate::kassert;
        let cpus = running_cpus();
        kassert!(cpus.len() >= 3);
        let start = crate::trace::ring().written();
        let me = current().ok_or("no current thread")?.id();
        for cpu in [arch::cpu_index(), cpus[1]] {
            let h = spawn_test(cpu, "slow", || time::sleep(5_000_000))?;
            let t = h.thread.clone();
            let (done, fired) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
            let (d, f, tw) = (done.clone(), fired.clone(), t.clone());
            let deadline = time::now_ns() + 2_000_000_000;
            let dog = spawn_test(cpus[2], "watchdog", move || {
                while !d.load(Ordering::Acquire) {
                    if time::now_ns() > deadline {
                        f.store(true, Ordering::Release);
                        tw.exited.wake_all();
                        return;
                    }
                    time::sleep(1_000_000);
                }
            })?;
            h.join();
            done.store(true, Ordering::Release);
            join_bounded(dog)?;
            kassert!(!fired.load(Ordering::Acquire));
            kassert!(t.reaped.load(Ordering::Acquire) && t.stack().is_none());
        }
        kassert!(switched_away(start, me, State::Blocked));
        Ok(())
    }
);

crate::ktest!(
    fn wake_one_is_fifo_and_wake_all_wakes_every_waiter() {
        // Three threads block in a known order: `wake_one` releases them in
        // that order; in a second round `wake_all` releases all three.
        use crate::{kassert, kassert_eq};
        const N: usize = 3;
        struct Shared {
            queue: WaitQueue,
            go: AtomicBool,
            seq: AtomicUsize,
            order: [AtomicUsize; N],
        }
        let cpu = last_cpu()?;
        for round in 0..2 {
            let s = Arc::new(Shared {
                queue: WaitQueue::new(),
                go: AtomicBool::new(false),
                seq: AtomicUsize::new(0),
                order: [const { AtomicUsize::new(usize::MAX) }; N],
            });
            let mut handles = Vec::new();
            for i in 0..N {
                let t = s.clone();
                handles.push(spawn_test(cpu, "waiter", move || {
                    t.queue.wait_until(|| t.go.load(Ordering::Acquire));
                    t.order[i].store(t.seq.fetch_add(1, Ordering::AcqRel), Ordering::Release);
                })?);
                kassert!(yield_until(|| s.queue.waiters.lock().len() == i + 1));
            }
            s.go.store(true, Ordering::Release);
            if round == 0 {
                for i in 0..N {
                    kassert!(s.queue.wake_one());
                    kassert!(yield_until(|| s.order[i].load(Ordering::Acquire) != usize::MAX));
                }
                let order: Vec<usize> = s.order.iter().map(|o| o.load(Ordering::Acquire)).collect();
                kassert_eq!(order, (0..N).collect::<Vec<_>>());
            } else {
                kassert_eq!(s.queue.wake_all(), N);
                kassert!(yield_until(|| s.seq.load(Ordering::Acquire) == N));
            }
            for h in handles {
                join_bounded(h)?;
            }
        }
        Ok(())
    }
);

crate::ktest!(
    fn every_homed_thread_has_room_in_its_queues() {
        // The timer never allocates: with 40 threads homed on a CPU (blocked,
        // so on neither queue) its ready and sleeper queues already have room
        // for all of them (pushing asserts it too); once they are reaped the
        // count is back.
        use crate::{kassert, kassert_eq};
        const N: usize = 40;
        let cpu = last_cpu()?;
        let room = || {
            let q = CPUS[cpu].lock();
            (q.homed, q.ready.capacity() >= q.homed && q.sleepers.capacity() >= q.homed)
        };
        let base = room().0;
        let s = Arc::new((WaitQueue::new(), AtomicBool::new(false)));
        let mut handles = Vec::new();
        for i in 0..N {
            let t = s.clone();
            handles.push(spawn_test(cpu, "parked", move || t.0.wait_until(|| t.1.load(Ordering::Acquire)))?);
            let (homed, ok) = room();
            kassert_eq!(homed, base + i + 1);
            kassert!(ok);
        }
        kassert!(yield_until(|| s.0.waiters.lock().len() == N));
        s.1.store(true, Ordering::Release);
        s.0.wake_all();
        for h in handles {
            join_bounded(h)?;
        }
        kassert_eq!(room().0, base);
        Ok(())
    }
);

crate::ktest!(
    fn yield_until_blocked_returns_once_every_thread_is_blocked() {
        // The serial prompt's wait for the screen shells (#119), on threads
        // on this CPU that block at different times: after 2 yields, only
        // after a 50 ms sleep (`Sleeping`, not `Blocked`, meanwhile), after
        // 8 yields, at once. The plan is run once per rotation, so each
        // thread is the sleeper, the last to block, once. Right after the
        // wait returns, every one is blocked and on the queue. A wait that
        // returned early (none, or one yield), or that checked only some of
        // them (any subset leaves out the sleeper in some rotation) or took
        // a sleeper for blocked, would find the sleeper not yet blocked:
        // when the wait starts, few if any have run yet. If they all
        // blocked before the wait even began (this thread preempted while
        // spawning them), the setup is redone, so a wait that does nothing
        // is very unlikely to pass. Once they are woken (none blocked
        // again), a wait whose deadline has passed returns at once, saying
        // they aren't all blocked. Then they are joined.
        use crate::{kassert, kassert_eq};
        const NAMES: [&str; 4] = ["parking-0", "parking-1", "parking-2", "parking-3"];
        // Yields before blocking; `None`: sleep 50 ms instead.
        const PLAN: [Option<usize>; 4] = [Some(2), None, Some(8), Some(0)];
        const SLEEP_NS: u64 = 50_000_000;
        let cpu = arch::cpu_index();
        let all_blocked = |hs: &[JoinHandle]| hs.iter().all(|h| h.thread().state() == State::Blocked);
        let states = |hs: &[JoinHandle]| {
            for h in hs {
                crate::ktest::diag(format_args!("{}: {}", h.thread().name(), h.thread().state().name()));
            }
        };
        let release = |s: &Arc<(WaitQueue, AtomicBool)>, hs: Vec<JoinHandle>| -> crate::ktest::TestResult {
            s.1.store(true, Ordering::Release);
            s.0.wake_all();
            for h in hs {
                join_bounded(h)?;
            }
            Ok(())
        };
        for rotation in 0..PLAN.len() {
            let mut plan = PLAN;
            plan.rotate_right(rotation);
            let mut done = false;
            for attempt in 1..=3 {
                let s = Arc::new((WaitQueue::new(), AtomicBool::new(false)));
                let mut handles = Vec::new();
                for (name, step) in NAMES.into_iter().zip(plan) {
                    let t = s.clone();
                    handles.push(spawn_test(cpu, name, move || {
                        match step {
                            Some(yields) => (0..yields).for_each(|_| yield_now()),
                            None => {
                                assert!(sleep(SLEEP_NS), "a test thread couldn't sleep");
                            }
                        }
                        t.0.wait_until(|| t.1.load(Ordering::Acquire));
                    })?);
                }
                if all_blocked(&handles) {
                    // Preempted while spawning, long enough for all of them.
                    crate::ktest::diag(format_args!(
                        "rotation {rotation}, attempt {attempt}: all blocked before the wait; again"
                    ));
                    release(&s, handles)?;
                    continue;
                }
                kassert!(yield_until_blocked(handles.iter(), Some(time::now_ns() + TEST_DEADLINE_NS)));
                if !all_blocked(&handles) {
                    crate::ktest::diag(format_args!("rotation {rotation}:"));
                    states(&handles);
                }
                kassert!(all_blocked(&handles));
                kassert_eq!(s.0.waiters.lock().len(), NAMES.len());
                s.1.store(true, Ordering::Release);
                kassert_eq!(s.0.wake_all(), NAMES.len());
                kassert!(!yield_until_blocked(handles.iter(), Some(0)));
                for h in handles {
                    join_bounded(h)?;
                }
                done = true;
                break;
            }
            if !done {
                return Err("the threads blocked before the wait in every attempt");
            }
        }
        Ok(())
    }
);

crate::ktest!(
    fn a_remote_wake_sends_its_cpu_the_wakeup_ipi() {
        // Waking a thread on another CPU sends that CPU the WAKEUP IPI, both
        // ways (this CPU to the last, and the last to this one): in each of
        // 20 tries that CPU's own WAKEUP count rises after the wake (waited
        // for: the IPI can land after the thread ran and was joined; another
        // wake to that CPU could stand in, never hide one). How often the
        // thread ran before its CPU's next tick is reported, not judged: a
        // host stalling the vCPU decides it, and it often does without the
        // IPI.
        use crate::kassert;
        let cpu = last_cpu()?;
        let here = arch::cpu_index();
        kassert!(cpu != here);
        let mut before_tick = 0;
        for tried in 1..=20 {
            let s = Arc::new((WaitQueue::new(), AtomicBool::new(false), AtomicU64::new(u64::MAX)));
            let t = s.clone();
            let h = spawn_test(cpu, "woken", move || {
                t.0.wait_until(|| t.1.load(Ordering::Acquire));
                t.2.store(arch::idt::ticks_on(cpu), Ordering::Release);
            })?;
            kassert!(yield_until(|| s.0.waiters.lock().len() == 1));
            s.1.store(true, Ordering::Release);
            let (t0, ipis) = (arch::idt::ticks_on(cpu), arch::idt::wakeups_on(cpu));
            s.0.wake_all();
            join_bounded(h)?;
            if s.2.load(Ordering::Acquire) == t0 {
                before_tick += 1;
            }
            if !yield_until(|| arch::idt::wakeups_on(cpu) != ipis) {
                crate::ktest::diag(format_args!("try {tried}: cpu {cpu} took no WAKEUP IPI after the wake"));
                return Err("a remote wake sent its CPU no WAKEUP IPI");
            }
            // The other way: a thread on the last CPU wakes one on this CPU
            // (here, only yielding meanwhile).
            let s = Arc::new((WaitQueue::new(), AtomicBool::new(false)));
            let (t, w) = (s.clone(), s.clone());
            let woken = spawn_test(here, "woken-here", move || t.0.wait_until(|| t.1.load(Ordering::Acquire)))?;
            kassert!(yield_until(|| s.0.waiters.lock().len() == 1));
            let ipis = arch::idt::wakeups_on(here);
            join_bounded(spawn_test(cpu, "waker", move || {
                w.1.store(true, Ordering::Release);
                w.0.wake_all();
            })?)?;
            join_bounded(woken)?;
            if !yield_until(|| arch::idt::wakeups_on(here) != ipis) {
                crate::ktest::diag(format_args!("try {tried}: cpu {here} took no WAKEUP IPI after cpu {cpu}'s wake"));
                return Err("a remote wake sent its CPU no WAKEUP IPI");
            }
        }
        crate::ktest::diag(format_args!(
            "woken before the next tick {before_tick} of 20 times, each with a WAKEUP IPI"
        ));
        Ok(())
    }
);

crate::ktest!(
    fn a_lost_wake_is_ended_by_the_timer_and_counted() {
        // `deadline_wakes_in`, the other ktests' proof that a wake ended a
        // wait: a wait whose condition is made true without waking it (a
        // lost wake, by a thread on this CPU once the wait is on its queue)
        // returns ready at its deadline, and counts one deadline wake; a
        // sleep between counts none. A try where the wait didn't block
        // before its 20 ms deadline (a host stall) proves nothing: taken
        // again, up to 5 times.
        use crate::{kassert, kassert_eq};
        kassert_eq!(deadline_wakes_in(|| sleep(2_000_000)).1, 0);
        for tried in 1..=5 {
            let s = Arc::new((WaitQueue::new(), AtomicBool::new(false)));
            let t = s.clone();
            let loser = spawn("lost-wake", move || {
                // Once the wait is on the queue (or over): true, and no wake.
                let _ = yield_until(|| t.0.waiters.lock().len() == 1 || t.1.load(Ordering::Acquire));
                t.1.store(true, Ordering::Release);
            })
            .map_err(|_| "spawn failed")?;
            let deadline = time::now_ns() + 20_000_000;
            let (woke, ended) =
                deadline_wakes_in(|| s.0.wait_until_or_deadline(deadline, || s.1.load(Ordering::Acquire)));
            s.1.store(true, Ordering::Release);
            join_bounded(loser)?;
            crate::ktest::diag(format_args!("try {tried}: returned {woke}, ended by the timer {ended} times"));
            kassert!(ended <= 1);
            if woke {
                // Only the timer could end it: the condition came with no
                // wake.
                return if ended == 1 { Ok(()) } else { Err("the timer ended a wait, uncounted") };
            }
        }
        Err("no try's wait blocked before its deadline")
    }
);

crate::ktest!(
    fn every_state_round_trips() {
        // Adding a state must not compile until this match (and so, by the
        // comment, `ALL`) is updated: a state missing from `ALL` would make
        // `Thread::state` panic.
        use crate::kassert_eq;
        for s in State::ALL {
            match s {
                State::Ready | State::Running | State::Sleeping | State::Blocked | State::Dead => {} // keep in step with `ALL`
            }
            kassert_eq!(State::from_raw(s as u8), Some(s));
            kassert_eq!(State::name_of(s as u64), Some(s.name()));
        }
        kassert_eq!(State::name_of(State::ALL.len() as u64), None);
        Ok(())
    }
);

crate::ktest!(
    fn spawning_where_no_scheduler_runs_is_refused() {
        // A CPU index past the limit, and every index no online CPU has.
        use crate::kassert;
        kassert!(matches!(spawn_on(MAX_CPUS, "nowhere", || {}), Err(SpawnError::NotRunning)));
        for cpu in (0..MAX_CPUS).filter(|&c| smp::online_lapic_id(c).is_none()) {
            kassert!(matches!(spawn_on(cpu, "offline", || {}), Err(SpawnError::NotRunning)));
        }
        Ok(())
    }
);

/// Ktests: whether `t` has a deadline entry on its CPU, or is on `queue`.
#[cfg(feature = "ktest")]
fn entries_of(t: &Arc<Thread>, queue: &WaitQueue) -> (bool, bool) {
    let deadline = CPUS[t.cpu].lock().sleepers.iter().any(|s| s.on_queue && Arc::ptr_eq(&s.thread, t));
    let queued = queue.waiters.lock().iter().any(|w| Arc::ptr_eq(w, t));
    (deadline, queued)
}

crate::ktest!(
    fn a_wait_with_a_deadline_ends_at_it_or_when_woken() {
        // Nobody wakes it: false, no earlier than the deadline, and neither
        // entry is left behind. A waker on another CPU wakes it long before
        // its deadline: true. Then 200 waits a waker races with the
        // deadline, waking it before and after the tick that notices the
        // deadline: never a panic, a lost thread, an early return or an
        // entry left behind, each side winning sometimes (the racer's mark
        // sometimes before the deadline), and every queue deadline on the
        // waiter's CPU noticed at the first tick after it, never earlier or
        // later.
        use crate::{kassert, kassert_eq};
        let me = current().ok_or("the ktests run on a thread")?;
        let q = WaitQueue::new();
        let start = time::now_ns();
        kassert!(!q.wait_until_or_deadline(start + 5_000_000, || false));
        kassert!(time::now_ns() >= start + 5_000_000);
        kassert_eq!(entries_of(&me, &q), (false, false));
        // Already ready, or the deadline already passed: no wait.
        kassert!(q.wait_until_or_deadline(0, || true));
        kassert!(!q.wait_until_or_deadline(0, || false));

        let s = Arc::new((WaitQueue::new(), AtomicBool::new(false)));
        let w = s.clone();
        let waker = spawn_test(last_cpu()?, "waker", move || {
            let _ = sleep(2_000_000);
            w.1.store(true, Ordering::Release);
            w.0.wake_all();
        })?;
        let start = time::now_ns();
        kassert!(s.0.wait_until_or_deadline(start + 10_000_000_000, || s.1.load(Ordering::Acquire)));
        kassert!(time::now_ns() - start < 5_000_000_000);
        join_bounded(waker)?;
        kassert_eq!(entries_of(&me, &s.0), (false, false));

        // The race, on two other CPUs (so the switches don't fill this
        // CPU's trace ring, which holds the boot's events): each round the
        // racer marks the round and wakes the queue 0, 0.4, 0.8, 1.2 or 1.6
        // ms after it starts, and the waiter's deadline is 1 ms after the
        // start (the wait returns true if the racer's mark came first).
        // Then the waiter blocks again at once on another queue: a stale
        // wake (the racer may still hold an entry from the wait before) may
        // wake it, but it waits on until its deadline.
        //
        // A deadline is noticed at the first tick after it, so where between
        // two ticks a round starts decides when its deadline fires, and so
        // which racer delays it beats. That second wait ends at a tick, so
        // rounds started right after it would all put their deadline at one
        // point, a few µs past the next tick: noticed then if that tick runs
        // a few µs late, else a whole tick later, after every racer delay
        // (#122). So the rounds start 0, 0.2, 0.4, 0.6 or 0.8 ms after the
        // waiter's last tick, each such phase paired with each racer delay
        // equally often: whether ticks come on time (KVM) or up to about a
        // tick late (loaded TCG), some pairs put the tick that notices the
        // deadline well before the racer's wake, and others well after it.
        const ROUNDS: u64 = 200;
        const DEADLINE_NS: u64 = 1_000_000;
        const STEPS: u64 = 5;
        const PHASE_STEP_NS: u64 = 200_000;
        const DELAY_STEP_NS: u64 = 400_000;
        // Every phase falls inside one tick period, and the delays and the
        // deadline are set against a 1 ms tick.
        const _: () = assert!((STEPS - 1) * PHASE_STEP_NS < 1_000_000_000 / time::TICK_HZ);
        const _: () = assert!(time::TICK_HZ == 1000);
        // Every (phase, delay) pair the same number of times.
        const _: () = assert!(ROUNDS.is_multiple_of(STEPS * STEPS));
        const PER_PAIR: u64 = ROUNDS / (STEPS * STEPS);
        /// Round `n`'s phase and racer delay, as indices below `STEPS` (the
        /// grid's row and column): each pair once in every `STEPS * STEPS`
        /// rounds.
        fn pair(n: u64) -> (usize, usize) {
            ((n / STEPS % STEPS) as usize, (n % STEPS) as usize)
        }
        let phase = |n: u64| pair(n).0 as u64 * PHASE_STEP_NS;
        let delay = |n: u64| pair(n).1 as u64 * DELAY_STEP_NS;
        let cpus = running_cpus();
        let (waiter_cpu, racer_cpu) = match cpus.as_slice() {
            [.., a, b] => (*b, *a),
            _ => return Err("the race needs two CPUs"),
        };
        /// What the waiter is doing, for a hang's diag (`Race::stage`).
        const STAGES: [&str; 5] =
            ["starting", "spinning to a phase", "waiting on the queue", "waiting on the other queue", "done"];
        /// What the racer did in one round: its `now_ns` when it saw the
        /// round and when it marked it (0: never), and how many threads its
        /// `wake_all` moved out of `Blocked`.
        #[derive(Default)]
        struct Mark {
            seen_ns: AtomicU64,
            marked_ns: AtomicU64,
            woken: AtomicU64,
        }
        impl Mark {
            /// When the racer saw and marked the round, if it did.
            fn read(&self) -> Option<(u64, u64)> {
                match (self.seen_ns.load(Ordering::Relaxed), self.marked_ns.load(Ordering::Relaxed)) {
                    (0, _) | (_, 0) => None,
                    seen_marked => Some(seen_marked),
                }
            }
        }
        struct Race {
            queue: WaitQueue,
            other: WaitQueue,
            round: AtomicU64,
            marked: AtomicU64,
            done: AtomicBool,
            /// An index into `STAGES`.
            stage: AtomicU8,
            /// The first round that broke the race, and how (0: before the
            /// rounds).
            broken: SpinLock<Option<(u64, &'static str)>>,
            /// The waiter CPU's last tick as the waiter began (0: that CPU's
            /// scheduler never ticked, so every phase would be 0).
            first_tick_ns: AtomicU64,
            /// The waiter CPU's queue deadlines fired, late and early during
            /// the race ([`queue_deadlines`]), its tick before last and last
            /// as the waiter ended, and whether one tick later the tick
            /// before last was the last one read before it, all as the
            /// waiter read them there.
            deadlines: SpinLock<[u64; 3]>,
            ticks_at_end: SpinLock<(u64, u64)>,
            prev_follows_last: AtomicBool,
            /// For the diags: what the racer did each round (index = round;
            /// 0 unused), and each round as the waiter saw it, recorded as
            /// it ends (in room reserved up front: no allocation mid-race).
            marks: Vec<Mark>,
            rounds: SpinLock<Vec<Round>>,
        }
        impl Race {
            fn breaks(&self, n: u64, how: &'static str) {
                self.broken.lock().get_or_insert((n, how));
            }
        }
        /// One round as the waiter saw it.
        #[derive(Clone, Copy)]
        struct Round {
            n: u64,
            /// The waiter CPU's last tick when the spin to the round's phase
            /// last checked it, and when the waiter read it again just
            /// before `start_ns`: a tick came between the two if they differ
            /// (the round lost its phase).
            phase_tick_ns: u64,
            tick_before_ns: u64,
            start_ns: u64,
            /// When the wait returned, and the waiter CPU's last tick then,
            /// `ticks` ticks after `tick_before_ns`.
            returned_ns: u64,
            tick_after_ns: u64,
            ticks: u64,
            woke: bool,
        }
        impl Round {
            fn lost(&self) -> bool {
                self.tick_before_ns != self.phase_tick_ns
            }
            /// Ready before its deadline: the racer marked the round first
            /// (a deadline fires only once it has passed).
            fn ready_early(&self) -> bool {
                self.woke && self.returned_ns < self.start_ns + DEADLINE_NS
            }
        }
        let race = Arc::new(Race {
            queue: WaitQueue::new(),
            other: WaitQueue::new(),
            round: AtomicU64::new(0),
            marked: AtomicU64::new(0),
            done: AtomicBool::new(false),
            stage: AtomicU8::new(0),
            broken: SpinLock::new(None),
            first_tick_ns: AtomicU64::new(0),
            deadlines: SpinLock::new([0; 3]),
            ticks_at_end: SpinLock::new((0, 0)),
            prev_follows_last: AtomicBool::new(false),
            marks: (0..=ROUNDS).map(|_| Mark::default()).collect(),
            rounds: SpinLock::new(Vec::with_capacity(ROUNDS as usize)),
        });
        let r = race.clone();
        let racer = spawn_test(racer_cpu, "racer", move || {
            let mut seen = 0;
            while !r.done.load(Ordering::Acquire) {
                let n = r.round.load(Ordering::Acquire);
                if n == seen {
                    core::hint::spin_loop();
                    continue;
                }
                seen = n;
                let seen_ns = time::now_ns();
                let mut now = seen_ns;
                while now < seen_ns + delay(n) {
                    core::hint::spin_loop();
                    now = time::now_ns();
                }
                r.marked.store(n, Ordering::Release);
                let woken = r.queue.wake_all();
                if let Some(m) = r.marks.get(n as usize) {
                    m.seen_ns.store(seen_ns, Ordering::Relaxed);
                    m.marked_ns.store(now, Ordering::Relaxed);
                    m.woken.store(woken as u64, Ordering::Relaxed);
                }
            }
        })?;
        let r = race.clone();
        let spawned = spawn_test(waiter_cpu, "waiter", move || {
            let stage = |s: u8| r.stage.store(s, Ordering::Relaxed);
            let me = match current() {
                Some(me) if arch::cpu_index() == waiter_cpu => me,
                _ => {
                    r.breaks(0, "the waiter isn't a thread on its cpu");
                    stage(4);
                    return;
                }
            };
            let last_tick = || LAST_TICK_NS[waiter_cpu].load(Ordering::Relaxed);
            let ticks = || TICKED[waiter_cpu].load(Ordering::Relaxed);
            r.first_tick_ns.store(last_tick(), Ordering::Relaxed);
            let before = queue_deadlines(waiter_cpu);
            for n in 1..=ROUNDS {
                if r.done.load(Ordering::Acquire) {
                    break;
                }
                stage(1);
                // Spin until `phase(n)` past the last tick. The tick is read
                // on both sides of `now_ns`, and both are read again if a
                // tick came between.
                let phase_tick_ns = loop {
                    if r.done.load(Ordering::Acquire) {
                        break None;
                    }
                    let (before, now, after) = (last_tick(), time::now_ns(), last_tick());
                    if before == after && now >= after + phase(n) {
                        break Some(after);
                    }
                    core::hint::spin_loop();
                };
                let Some(phase_tick_ns) = phase_tick_ns else { break };
                r.round.store(n, Ordering::Release);
                let (tick_before_ns, ticks_before) = (last_tick(), ticks());
                let start_ns = time::now_ns();
                stage(2);
                let woke =
                    r.queue.wait_until_or_deadline(start_ns + DEADLINE_NS, || r.marked.load(Ordering::Acquire) == n);
                let returned_ns = time::now_ns();
                r.rounds.lock().push(Round {
                    n,
                    phase_tick_ns,
                    tick_before_ns,
                    start_ns,
                    returned_ns,
                    tick_after_ns: last_tick(),
                    ticks: ticks() - ticks_before,
                    woke,
                });
                // A timed-out wait returns at its deadline at the earliest.
                if !woke && returned_ns < start_ns + DEADLINE_NS {
                    r.breaks(n, "a timed-out wait returned before its deadline");
                }
                stage(3);
                let d2 = time::now_ns() + 100_000;
                if r.other.wait_until_or_deadline(d2, || false) || time::now_ns() < d2 {
                    r.breaks(n, "the wait on the other queue returned before its deadline");
                }
                if entries_of(&me, &r.queue) != (false, false) || entries_of(&me, &r.other) != (false, false) {
                    r.breaks(n, "a wait left an entry behind");
                }
            }
            let after = queue_deadlines(waiter_cpu);
            *r.deadlines.lock() = core::array::from_fn(|i| after[i] - before[i]);
            // The detector's ticks are live: its CPU's last two ticks, read
            // with no tick between. A tick is an interrupt on this CPU, so
            // this thread sees all of a tick's stores or none of them (it
            // stores `PREV_TICK_NS` and `LAST_TICK_NS`, then counts itself in
            // `TICKED`): two equal readings of `TICKED` around other reads
            // mean no tick ran between them. The compiler fences keep the
            // compiler from moving those `Relaxed` loads across one another
            // (x86 doesn't reorder loads with loads, and takes interrupts
            // between instructions).
            let fence = || core::sync::atomic::compiler_fence(Ordering::SeqCst);
            *r.ticks_at_end.lock() = loop {
                let t = ticks();
                fence();
                let ends = (PREV_TICK_NS[waiter_cpu].load(Ordering::Relaxed), last_tick());
                fence();
                if ticks() == t {
                    break ends;
                }
            };
            // And not stale: across exactly one tick, `PREV_TICK_NS` takes
            // the `LAST_TICK_NS` before it. `last` is tick `t`'s (`TICKED`
            // read as `t` on both sides of it), and `prev` is read while
            // `TICKED` is `t + 1` (on both sides again: the spin's last
            // reading and the check after), so exactly tick `t + 1` ran
            // between the two: it moved tick `t`'s value into `prev`. Any
            // other interleaving is read again.
            let follows = loop {
                let t = ticks();
                fence();
                let last = last_tick();
                fence();
                if ticks() != t || last == 0 {
                    continue;
                }
                while ticks() == t {
                    core::hint::spin_loop();
                }
                fence();
                let prev = PREV_TICK_NS[waiter_cpu].load(Ordering::Relaxed);
                fence();
                if ticks() == t + 1 {
                    break prev == last;
                }
            };
            r.prev_follows_last.store(follows, Ordering::Relaxed);
            stage(4);
        });
        let waiter = match spawned {
            Ok(h) => h,
            Err(e) => {
                race.done.store(true, Ordering::Release);
                drop(racer); // it stops on its own now
                return Err(e);
            }
        };
        let waiter_thread = waiter.thread.clone();
        if let Err(e) = join_bounded(waiter) {
            race.done.store(true, Ordering::Release);
            drop(racer); // it stops on its own now
            let (count, woke) = {
                let rounds = race.rounds.lock();
                (rounds.len(), rounds.iter().filter(|x| x.woke).count())
            };
            let stage = STAGES.get(usize::from(race.stage.load(Ordering::Relaxed))).unwrap_or(&"?");
            crate::ktest::diag(format_args!(
                "the waiter didn't finish: {stage}, in round {} after {count} rounds ({woke} ready first, {} timed \
                 out); the racer's last mark round {}; cpu {waiter_cpu} at {} ticks; the waiter's (deadline entry, \
                 queued) on the queue {:?}, on the other queue {:?}",
                race.round.load(Ordering::Acquire),
                count - woke,
                race.marked.load(Ordering::Acquire),
                TICKED[waiter_cpu].load(Ordering::Relaxed),
                entries_of(&waiter_thread, &race.queue),
                entries_of(&waiter_thread, &race.other),
            ));
            return Err(e);
        }
        race.done.store(true, Ordering::Release);
        // Checked after the diags, which a racer that didn't stop shouldn't
        // keep from printing.
        let racer_joined = join_bounded(racer);
        let rounds = core::mem::take(&mut *race.rounds.lock());
        let broken = *race.broken.lock();
        let first_tick_ns = race.first_tick_ns.load(Ordering::Relaxed);
        let [fired, late_deadlines, early_deadlines] = *race.deadlines.lock();
        let (prev_tick_ns, last_tick_ns) = *race.ticks_at_end.lock();
        let prev_follows_last = race.prev_follows_last.load(Ordering::Relaxed);
        let mark = |x: &Round| race.marks.get(x.n as usize).and_then(Mark::read);
        let woke = rounds.iter().filter(|x| x.woke).count();
        let timed_out = rounds.len() - woke;
        let ready_early = rounds.iter().filter(|x| x.ready_early()).count();
        // The grid leaves out rounds that didn't race as paired: the racer
        // never saw them (it skips a round the waiter is done with before
        // it looks), or they lost their phase.
        let mut grid = [[0u64; STEPS as usize]; STEPS as usize];
        let mut left_out = 0;
        for x in &rounds {
            if mark(x).is_none() || x.lost() {
                left_out += 1;
            } else if !x.woke {
                let (p, d) = pair(x.n);
                grid[p][d] += 1;
            }
        }
        let steps_us = |step: u64| {
            let v: Vec<_> = (0..STEPS).map(|i| alloc::format!("{}", i * step / 1000)).collect();
            alloc::format!("{} µs", v.join(", "))
        };
        crate::ktest::diag(format_args!(
            "of {ROUNDS} racing waits, {woke} were ready first ({ready_early} before their deadline), {timed_out} \
             timed out; timed out, of {PER_PAIR} less any left out, by phase {} (rows) and racer delay {} \
             (columns): {grid:?}, {left_out} rounds left out (the racer never saw them or they lost their phase)",
            steps_us(PHASE_STEP_NS),
            steps_us(DELAY_STEP_NS)
        ));
        let since = |t: u64, from: u64| t as i64 - from as i64;
        // Signed ns as µs, one decimal.
        let us = |ns: i64| {
            let a = ns.unsigned_abs();
            alloc::format!("{}{}.{}", if ns < 0 { "-" } else { "" }, a / 1000, a % 1000 / 100)
        };
        let spread = |mut v: Vec<i64>| {
            v.sort_unstable();
            match v.len() {
                0 => alloc::string::String::from("none"),
                len => {
                    let mid = if len % 2 == 1 { v[len / 2] } else { (v[len / 2 - 1] + v[len / 2]) / 2 };
                    alloc::format!("{} / {} / {} µs", us(v[0]), us(mid), us(v[len - 1]))
                }
            }
        };
        let lost = rounds.iter().filter(|x| x.lost()).count();
        let past_phase = rounds.iter().map(|x| since(x.start_ns, x.phase_tick_ns + phase(x.n))).collect();
        let late = rounds.iter().filter(|x| !x.woke).map(|x| since(x.returned_ns, x.start_ns + DEADLINE_NS)).collect();
        let seen: Vec<_> = rounds
            .iter()
            .filter_map(|x| {
                let (s, k) = mark(x)?;
                Some((since(s, x.start_ns), since(k, s + delay(x.n))))
            })
            .collect();
        let missed = rounds.len() - seen.len();
        let waker_won = rounds
            .iter()
            .filter(|x| x.woke && race.marks.get(x.n as usize).is_some_and(|m| m.woken.load(Ordering::Relaxed) > 0))
            .count();
        crate::ktest::diag(format_args!(
            "min / median / max: rounds started {} past their phase ({lost} lost it: a tick came between the spin's \
             check and the tick read just before the start); timed-out waits returned {} after their deadline; the \
             racer saw rounds {} after they started and marked them {} after its delay ({missed} rounds it never \
             saw); its wake_all moved a thread out of Blocked in {waker_won} of the {woke} rounds ready first",
            spread(past_phase),
            spread(late),
            spread(seen.iter().map(|s| s.0).collect()),
            spread(seen.iter().map(|s| s.1).collect()),
        ));
        crate::ktest::diag(format_args!(
            "queue deadlines on the waiter's cpu {waiter_cpu} during the race: {fired} fired, {late_deadlines} a \
             tick late, {early_deadlines} early; its ticks before last and last at {prev_tick_ns} and \
             {last_tick_ns} ns as the waiter ended (a tick later, the tick before last was that last one: \
             {prev_follows_last}); its last tick as the waiter began at {first_tick_ns} ns"
        ));
        let checks = || -> crate::ktest::TestResult {
            if let Some((n, how)) = broken {
                crate::ktest::diag(format_args!("the race broke first in round {n}: {how}"));
            }
            kassert!(broken.is_none());
            kassert_eq!(rounds.len(), ROUNDS as usize);
            // The sweep ran: the waiter's CPU had ticked, so the phases
            // were real.
            kassert!(first_tick_ns != 0);
            // The exact checks first, so a deterministic failure names
            // itself. Every queue deadline on the waiter's CPU fired at the
            // first tick after it. `fired > 0` checks the counting is wired
            // up: the waits on the other queue alone fire up to 200.
            kassert!(fired > 0 && late_deadlines == 0 && early_deadlines == 0);
            // The ticks the detector compares were being recorded, and
            // `PREV_TICK_NS` was the tick before the last, not stale.
            kassert!(0 < prev_tick_ns && prev_tick_ns < last_tick_ns);
            kassert!(prev_follows_last);
            // Then the ones that depend on timing, with margins.
            kassert!(woke > 0 && timed_out > 0);
            // Some rounds were won by the racer's mark before the deadline.
            kassert!(ready_early > 0);
            Ok(())
        };
        let result = checks();
        // Every round, if a check failed: which side was off (times from
        // the round's start, its deadline at +DEADLINE_NS).
        if result.is_err() {
            let at = |t: u64, x: &Round| match t {
                0 => alloc::string::String::from("never"),
                t => alloc::format!("{} µs", us(since(t, x.start_ns))),
            };
            for x in &rounds {
                let (s, k) = mark(x).unwrap_or((0, 0));
                crate::ktest::diag(format_args!(
                    "round {}: phase {} µs{}, delay {} µs, {}; ticks at {} and {} ({} ticks apart); returned at {}; \
                     the racer saw it at {}, marked it at {}",
                    x.n,
                    phase(x.n) / 1000,
                    if x.lost() { " (lost)" } else { "" },
                    delay(x.n) / 1000,
                    if x.woke { "ready first" } else { "timed out" },
                    at(x.tick_before_ns, x),
                    at(x.tick_after_ns, x),
                    x.ticks,
                    at(x.returned_ns, x),
                    at(s, x),
                    at(k, x),
                ));
            }
        }
        result?;
        racer_joined
    }
);

crate::ktest!(
    fn queue_deadlines_due_on_one_tick_all_fire_on_it() {
        // Three threads on one CPU block on queues until one shared
        // deadline: the tick that first reads a time past it fires all three
        // entries, none a tick late and none early, and each wait returns
        // false no earlier than it. (A `wake_sleepers` that stepped past the
        // entry swapped into a fired one's place would fire it a tick late.)
        use crate::{kassert, kassert_eq};
        const WAITERS: usize = 3;
        let cpu = last_cpu()?;
        // CPU `cpu`'s queue deadline counts, read there.
        let counts = || -> Result<[u64; 3], &'static str> {
            let got = Arc::new(SpinLock::new([0u64; 3]));
            let g = got.clone();
            join_bounded(spawn_test(cpu, "counts", move || *g.lock() = queue_deadlines(cpu))?)?;
            Ok(*got.lock())
        };
        // "All on one tick" is judged only on a try whose three all blocked
        // before the deadline (their entries counted, then a time read
        // before it); a host stalling the spawning past it means another
        // try with a fresh deadline, up to TRIES. Every try checks what
        // holds however late they blocked: none fired early or a tick late,
        // and each wait returned false no earlier than its deadline.
        const TRIES: usize = 5;
        for tried in 1..=TRIES {
            let before = counts()?;
            let deadline = time::now_ns() + 100_000_000;
            let results = Arc::new(SpinLock::new(Vec::with_capacity(WAITERS)));
            let mut handles = Vec::new();
            for _ in 0..WAITERS {
                let res = results.clone();
                handles.push(spawn_test(cpu, "due", move || {
                    let q = WaitQueue::new();
                    let (woke, ended) = deadline_wakes_in(|| q.wait_until_or_deadline(deadline, || false));
                    res.lock().push((woke, time::now_ns() >= deadline, ended));
                })?);
            }
            let threads: Vec<_> = handles.iter().map(|h| h.thread.clone()).collect();
            let deadline_entries = || {
                let q = CPUS[cpu].lock();
                threads.iter().filter(|t| q.sleepers.iter().any(|s| s.on_queue && Arc::ptr_eq(&s.thread, t))).count()
            };
            let (entries, in_time) = loop {
                let entries = deadline_entries();
                let before = time::now_ns() < deadline;
                if entries == WAITERS || !before {
                    break (entries, entries == WAITERS && before);
                }
                yield_now();
            };
            for h in handles {
                join_bounded(h)?;
            }
            let after = counts()?;
            let [fired, late, early]: [u64; 3] = core::array::from_fn(|i| after[i] - before[i]);
            crate::ktest::diag(format_args!(
                "cpu {cpu}: {fired} queue deadlines fired, {late} a tick late, {early} early (try {tried}; {entries} of \
                 {WAITERS} seen blocked before the deadline; each waiter's ended by the timer: {:?})",
                results.lock().iter().map(|r| r.2).collect::<Vec<_>>()
            ));
            kassert_eq!(results.lock().len(), WAITERS);
            // A deadline ends a wait once at most: a deadline firing early
            // wakes a waiter that blocks again, to be fired again.
            kassert!(results.lock().iter().all(|&(woke, after_deadline, ended)| !woke && after_deadline && ended <= 1));
            kassert!(late == 0 && early == 0);
            if in_time {
                kassert!(fired >= WAITERS as u64 && results.lock().iter().all(|r| r.2 == 1));
                return Ok(());
            }
        }
        Err("the waiters never all blocked before their deadline")
    }
);

/// Ktests: how many times `t` is on `queue`.
#[cfg(feature = "ktest")]
fn times_on(t: &Arc<Thread>, queue: &WaitQueue) -> usize {
    queue.waiters.lock().iter().filter(|w| Arc::ptr_eq(w, t)).count()
}

crate::ktest!(
    fn a_stale_wake_leaves_a_plain_wait_on_its_queue_once() {
        // A thread blocked in `wait_until` and woken by something other than
        // its queue's waker (a waker holding its entry from an earlier wait
        // that a deadline ended; here `wake` itself) finds `ready()` false
        // and blocks again: on its queue once, having taken its old entry
        // off, not twice.
        use core::sync::atomic::AtomicU64;

        use crate::kassert;
        let s = Arc::new((WaitQueue::new(), AtomicBool::new(false), AtomicU64::new(0)));
        let t = s.clone();
        let h = spawn_test(last_cpu()?, "stale", move || {
            t.0.wait_until(|| {
                t.2.fetch_add(1, Ordering::AcqRel);
                t.1.load(Ordering::Acquire)
            })
        })?;
        let th = h.thread.clone();
        let blocked_after = |checks: u64| {
            yield_until(|| {
                th.state() == State::Blocked && s.2.load(Ordering::Acquire) == checks && times_on(&th, &s.0) > 0
            })
        };
        kassert!(blocked_after(1));
        kassert!(wake(th.clone()));
        kassert!(blocked_after(2));
        let once = times_on(&th, &s.0);
        s.1.store(true, Ordering::Release);
        s.0.wake_all();
        join_bounded(h)?;
        kassert!(once == 1);
        kassert!(times_on(&th, &s.0) == 0);
        Ok(())
    }
);

crate::ktest!(
    fn wake_one_skips_a_thread_that_isnt_blocked() {
        // A leftover entry for a thread that isn't blocked (here this one,
        // running) at the front of the queue: `wake_one` passes over it and
        // wakes the blocked thread behind it, and says it woke one.
        use crate::kassert;
        let me = current().ok_or("the ktests run on a thread")?;
        let s = Arc::new((WaitQueue::new(), AtomicBool::new(false)));
        let t = s.clone();
        let h = spawn_test(last_cpu()?, "behind", move || t.0.wait_until(|| t.1.load(Ordering::Acquire)))?;
        let th = h.thread.clone();
        kassert!(yield_until(|| th.state() == State::Blocked && times_on(&th, &s.0) == 1));
        s.0.waiters.lock().push_front(me.clone());
        s.1.store(true, Ordering::Release);
        kassert!(s.0.wake_one());
        join_bounded(h)?;
        kassert!(times_on(&me, &s.0) == 0 && times_on(&th, &s.0) == 0);
        Ok(())
    }
);

crate::ktest!(
    fn a_joined_thread_is_released_before_join_returns() {
        // #183: a dying thread's CPU wakes its joiners, then frees their list
        // (the 32-byte block the heap stress saw freed late) and drops its
        // reference to the thread. With that CPU held 20 ms once the joiner
        // is past its wait for `reaped` (`HOLD_FINISHING`), `join` still
        // returns only once the CPU is done: the thread is gone from
        // `threads()` once `join` returns, and nothing that CPU frees lands
        // after it, not even 50 ms later. (A `FINISHING` cleared anywhere
        // before the release lets the free land in that window, while the
        // joiner surely runs.) The thread exits only once its joiner is on
        // its queue, and `HOLD_FINISHING` is set before `join` puts it there,
        // so the hold applies with that joiner.
        use crate::{kassert, kassert_eq};
        let me = arch::cpu_index();
        let cpu = (0..MAX_CPUS).find(|&c| c != me && running_on(c)).ok_or("needs a second CPU")?;
        let h = spawn_test(cpu, "released", || {
            let me = current().expect("a thread");
            let deadline = time::now_ns() + TEST_DEADLINE_NS;
            while me.exited.waiters.lock().is_empty() && time::now_ns() < deadline {
                let _ = sleep(1_000_000);
            }
        })?;
        let id = h.thread.id;
        HELD_JOINERS.store(u64::MAX, Ordering::Relaxed);
        HELD_SAW_JOINER.store(false, Ordering::Relaxed);
        HELD_NS.store(0, Ordering::Relaxed);
        HOLD_FINISHING.store(id, Ordering::Relaxed);
        // A plain `join`, which blocks on the thread's queue (polling, as
        // `join_bounded` does, never would). Bounded all the same: the thread
        // exits by its deadline, and the hold by 1 s and 20 ms.
        h.join();
        let heap = crate::mm::heap::stats();
        HOLD_FINISHING.store(0, Ordering::Relaxed);
        kassert!(threads().iter().all(|t| t.id != id));
        // The hold applied, with its one joiner past its wait, all 20 ms.
        kassert_eq!(HELD_JOINERS.load(Ordering::Relaxed), 1);
        kassert!(HELD_SAW_JOINER.load(Ordering::Relaxed));
        kassert!(HELD_NS.load(Ordering::Relaxed) >= 20_000_000);
        // `join` read the right CPU's slot.
        kassert!(FINISHING[cpu].load(Ordering::Acquire) != id);
        let _ = sleep(50_000_000);
        let after = crate::mm::heap::stats();
        kassert_eq!((after.used, after.live), (heap.used, heap.live));
        Ok(())
    }
);

crate::ktest!(
    fn a_scope_joins_every_thread_before_it_returns() {
        // Threads on every CPU whose scheduler runs borrow the caller's
        // array (no `Arc`, no `'static`), each sleeping first so a missing
        // join shows: when `scope` returns every slot is written and every
        // thread has exited. A spawn on a CPU without a scheduler fails
        // (its closure dropped unrun) and the others still run.
        use crate::{kassert, kassert_eq};
        let cpus: Vec<usize> = (0..MAX_CPUS).filter(|&c| running_on(c)).collect();
        kassert!(cpus.len() > 1);
        let slots = [const { AtomicUsize::new(0) }; MAX_CPUS];
        let ran_unstarted = AtomicBool::new(false);
        let missing = (0..MAX_CPUS).find(|&c| !running_on(c));
        let (started, refused) = scope(|s| {
            let mut started = 0;
            for &cpu in &cpus {
                let slot = &slots[cpu];
                if s.spawn_on(cpu, "scoped", move || {
                    let _ = sleep(20_000_000);
                    slot.store(arch::cpu_index() + 1, Ordering::Release);
                })
                .is_ok()
                {
                    started += 1;
                }
            }
            let refused =
                missing.map(|cpu| s.spawn_on(cpu, "nowhere", || ran_unstarted.store(true, Ordering::Release)));
            (started, refused)
        })
        .map_err(|_| "this thread can block")?;
        kassert_eq!(started, cpus.len());
        for &cpu in &cpus {
            kassert_eq!(slots[cpu].load(Ordering::Acquire), cpu + 1);
        }
        if let Some(r) = refused {
            kassert!(r == Err(SpawnError::NotRunning));
        }
        kassert!(!ran_unstarted.load(Ordering::Acquire));
        // Each thread in turn the only slow one: a join skipped for any of
        // them (not only the first) shows in its round, its flag unset.
        for slow in 0..cpus.len() {
            let done = [const { AtomicBool::new(false) }; MAX_CPUS];
            scope(|s| {
                for (k, &cpu) in cpus.iter().enumerate() {
                    let flag = &done[k];
                    s.spawn_on(cpu, "scoped", move || {
                        if k == slow {
                            let _ = sleep(30_000_000);
                        }
                        flag.store(true, Ordering::Release);
                    })
                    .map_err(|_| "spawn failed")?;
                }
                Ok::<(), &'static str>(())
            })
            .map_err(|_| "this thread can block")??;
            for (k, flag) in done.iter().enumerate().take(cpus.len()) {
                let joined = flag.load(Ordering::Acquire);
                if !joined {
                    crate::ktest::diag(format_args!("round {slow}: thread {k} not joined"));
                }
                kassert!(joined);
            }
        }
        // Every scoped thread has exited and been released: `join` waits
        // for its CPU to let go of it, and `scope` drops the handles.
        kassert!(threads().iter().all(|t| t.name != "scoped"));
        Ok(())
    }
);
