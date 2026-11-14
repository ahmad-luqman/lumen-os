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
//! ([`yield_now`], [`sleep`], [`WaitQueue::wait_until`]) turns interrupts
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
//! (kmon, or the ktests). A CPU's scheduler runs ([`running_on`]) from the
//! first tick after it is armed (its current and idle threads set and its
//! timer started): until then nothing could wake a sleeper or preempt there,
//! so threads can't be spawned there and `time::sleep` busy-waits.
//!
//! A thread that returns from its entry point (or calls [`exit`]) is dead;
//! the next thread to run on its CPU frees its stack, and only then does
//! [`JoinHandle::join`] return. The unmap's TLB flush is local: the home CPU
//! ran on the stack; a spawner on another CPU wrote its first frame and may
//! keep a stale entry for that page, which nothing uses again (stack
//! addresses are never reused).
//!
//! Lock order: a wait queue's lock, then a CPU's run queue (only
//! [`WaitQueue::wait_until`] nests them), then a thread's `stack` lock (only
//! [`exit`] nests it); then the heap, page tables and frame allocator. Every
//! thread homed on a CPU has room reserved in its queues ([`Cpu::homed`],
//! asserted on every push), so the timer interrupt never allocates. It can
//! free, in [`finish_switch`]: a dead thread's stack (through the page
//! tables and frame allocator), and on the heap a detached thread's record
//! and its joiners' list.
//!
//! Every switch records a `switch` trace event (previous thread's id, next
//! thread's id, the previous thread's state), and the crash report names the
//! thread that was running (`thread: <name> (id N)`, read lock-free).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
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
use crate::trace::Kind;
use crate::{smp, time};

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
}

impl Thread {
    /// A ready thread on CPU `cpu` with a fresh stack, that starts by
    /// running `entry`.
    fn spawned(name: &'static str, cpu: usize, entry: Entry) -> Result<Arc<Thread>, SpawnError> {
        let stack = KernelStack::new(STACK_SIZE, StackKind::Thread).map_err(SpawnError::Stack)?;
        // SAFETY: a fresh, mapped stack (page-aligned top) nothing runs on;
        // the thread owns it, and it is freed only after the thread has died
        // and its CPU has switched away from it.
        let rsp = unsafe { arch::context::prepare(stack.range().top, thread_entry) };
        Ok(Arc::new(Thread::new(name, cpu, State::Ready, rsp, Some(stack), Some(entry))))
    }

    /// A thread for the code running now on this CPU, on a stack it doesn't
    /// own.
    fn adopted(name: &'static str) -> Arc<Thread> {
        Arc::new(Thread::new(name, arch::cpu_index(), State::Running, 0, None, None))
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
        }
    }

    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn state(&self) -> State {
        State::from_raw(self.state.load(Ordering::Acquire)).expect("only a State is stored")
    }

    /// Its stack, unless it has none (see `stack`).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
    pub fn stack(&self) -> Option<StackRange> {
        self.stack.lock().as_ref().map(KernelStack::range)
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
    /// Sleeping threads and the `now_ns` each wakes at.
    sleepers: Vec<(u64, Arc<Thread>)>,
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

    /// Queue a ready thread, in the room reserved for it.
    fn push_ready(&mut self, cpu: usize, t: Arc<Thread>) {
        assert!(self.ready.len() < self.ready.capacity(), "queueing thread {t:?}, which isn't homed on cpu {cpu}");
        self.ready.push_back(t);
        READY[cpu].store(self.ready.len(), Ordering::Release);
    }

    fn pop_ready(&mut self, cpu: usize) -> Option<Arc<Thread>> {
        let t = self.ready.pop_front();
        READY[cpu].store(self.ready.len(), Ordering::Release);
        t
    }

    /// Make every sleeper whose deadline is `now` or earlier ready.
    fn wake_sleepers(&mut self, cpu: usize, now: u64) {
        let mut i = 0;
        while i < self.sleepers.len() {
            if self.sleepers[i].0 <= now {
                let (_, t) = self.sleepers.swap_remove(i);
                t.change(State::Sleeping, State::Ready);
                self.push_ready(cpu, t);
            } else {
                i += 1;
            }
        }
    }
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
/// Set once [`init`] has stopped waiting for the schedulers to start: one
/// that starts after that says so.
static START_WAIT_OVER: AtomicBool = AtomicBool::new(false);

/// Whether CPU `cpu`'s scheduler runs (threads can be spawned there).
pub fn running_on(cpu: usize) -> bool {
    RUNNING.get(cpu).is_some_and(|r| r.load(Ordering::Acquire))
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

/// Can the running thread block here? Only once this CPU's scheduler runs,
/// with interrupts on and no critical section open (so not in an interrupt
/// handler or with a lock held), and not on its idle thread (which must
/// always be ready to run). In that order: the run queue's lock is taken
/// last, only outside every critical section.
fn can_block(cpu: usize) -> bool {
    running_on(cpu) && arch::irq::enabled() && arch::irq::depth() == 0 && !CPUS[cpu].lock().on_idle()
}

/// Why [`spawn_on`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// That CPU's scheduler doesn't run (no such CPU, not online, before
    /// [`init`], or its timer doesn't tick).
    NotRunning,
    /// No stack for it.
    Stack(StackError),
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpawnError::NotRunning => f.write_str("that CPU runs no scheduler"),
            SpawnError::Stack(e) => write!(f, "no stack: {e}"),
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
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
    pub fn thread(&self) -> &Thread {
        &self.thread
    }

    /// Wait until the thread has finished and its stack is freed. Blocks the
    /// calling thread (see [`WaitQueue::wait_until`] for where it can).
    pub fn join(self) {
        let t = &self.thread;
        assert!(!current().is_some_and(|c| Arc::ptr_eq(&c, t)), "thread {t:?} joining itself");
        t.exited.wait_until(|| t.reaped.load(Ordering::Acquire));
    }
}

/// Start a thread running `f` on this CPU.
pub fn spawn(name: &'static str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, SpawnError> {
    spawn_on(arch::cpu_index(), name, f)
}

/// Start a thread running `f` on CPU `cpu`, where it will always run. It is
/// queued behind the threads already ready there.
pub fn spawn_on(cpu: usize, name: &'static str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, SpawnError> {
    if !running_on(cpu) {
        return Err(SpawnError::NotRunning);
    }
    let thread = Thread::spawned(name, cpu, Box::new(f))?;
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
    // Not `irq::enable`: if another CPU is reporting a crash, interrupts
    // stay off (and its stop NMI halts this CPU).
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
        q.sleepers.push((deadline, me));
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
        if q.is_idle_thread(&prev) {
            prev.change(State::Running, State::Ready);
        } else if prev.state() == State::Running {
            // Only this CPU moves its running thread out of Running.
            prev.change(State::Running, State::Ready);
            q.push_ready(cpu, prev.clone());
        }
        let next = match q.pop_ready(cpu) {
            Some(t) => t,
            None => q.idle.clone().expect("a running scheduler has an idle thread"),
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
/// thread switched away from, and if it is dead, free its stack (on its own
/// CPU, which ran on it: see the module docs on the TLB) and wake its
/// joiners. Interrupts off.
fn finish_switch(cpu: usize) {
    let prev = CPUS[cpu].lock().take_switched_from();
    let Some(prev) = prev.filter(|p| p.state() == State::Dead) else { return };
    let stack = prev.stack.lock().take();
    drop(stack);
    prev.reaped.store(true, Ordering::Release);
    prev.exited.wake_all();
}

/// The timer interrupt on this CPU (interrupts off, EOI sent): the first
/// tick of an armed CPU starts its scheduler; after that, wake the sleepers
/// whose deadline has passed, and switch threads if another is ready and
/// the running one's slice is used up (or the idle thread runs).
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
    let switch = {
        let mut q = CPUS[cpu].lock();
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
    /// where the thread can't block (see [`can_block`]).
    pub fn wait_until(&self, mut ready: impl FnMut() -> bool) {
        let cpu = arch::cpu_index();
        assert!(
            can_block(cpu),
            "blocking where a thread can't (no scheduler, interrupts off, a lock held, or the idle thread)"
        );
        loop {
            let were_on = arch::irq::disable();
            {
                let mut waiters = self.waiters.lock();
                if ready() {
                    drop(waiters);
                    arch::irq::restore(were_on);
                    return;
                }
                let me = CPUS[cpu].lock().running().clone();
                // A waker on another CPU may make it ready (and queue it)
                // before this CPU switches away: `schedule` then neither
                // queues it again nor loses it.
                me.change(State::Running, State::Blocked);
                waiters.push_back(me);
            }
            schedule(cpu);
            arch::irq::restore(were_on);
        }
    }

    /// Wake the longest-waiting thread; whether there was one.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests; the shell's blocking read (#62)
    pub fn wake_one(&self) -> bool {
        let first = self.waiters.lock().pop_front();
        first.map(wake).is_some()
    }

    /// Wake every waiting thread; how many there were.
    pub fn wake_all(&self) -> usize {
        let all = core::mem::take(&mut *self.waiters.lock());
        let n = all.len();
        all.into_iter().for_each(wake);
        n
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

/// Make a thread taken off a wait queue ready (only blocked threads are on
/// one, and each is taken off before it is woken).
fn wake(thread: Arc<Thread>) {
    thread.change(State::Blocked, State::Ready);
    enqueue(thread);
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
    let idle = Thread::spawned("idle", 0, Box::new(|| idle_loop(0, || false, || {})))
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
fn yield_until(done: impl Fn() -> bool) -> bool {
    let deadline = time::now_ns() + TEST_DEADLINE_NS;
    while !done() {
        if time::now_ns() > deadline {
            return false;
        }
        yield_now();
    }
    true
}

/// `join`, failing instead of hanging if the thread isn't reaped by the test
/// deadline.
#[cfg(feature = "ktest")]
fn join_bounded(h: JoinHandle) -> crate::ktest::TestResult {
    let t = h.thread.clone();
    if !yield_until(|| t.reaped.load(Ordering::Acquire)) {
        crate::ktest::diag(format_args!("{t:?} wasn't reaped"));
        return Err("a thread didn't finish");
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
            smp::run_on(cpu, || {
                let (lvt, _, initial) = arch::apic::timer_registers();
                LVT.store(u64::from(lvt), Ordering::Relaxed);
                INITIAL.store(u64::from(initial), Ordering::Relaxed);
                READ.store(true, Ordering::Release);
            })
            .map_err(|e| {
                crate::ktest::diag(format_args!("run_on cpu {cpu}: {e}"));
                "run_on failed"
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
        smp::run_on(cpu, || {
            let t = time::now_ns();
            time::sleep(NS);
            // + 1: 0 means "not yet", so `> NS` below means it took `>= NS`.
            ELAPSED.store(time::now_ns() - t + 1, Ordering::Release);
        })
        .map_err(|e| {
            crate::ktest::diag(format_args!("run_on cpu {cpu}: {e}"));
            "run_on failed"
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
    fn a_remote_wake_does_not_wait_for_a_tick() {
        // Waking a thread on an idle CPU sends it the WAKEUP IPI: the thread
        // runs before that CPU's next tick in at least 5 of 20 tries
        // (without the IPI, never: only the tick would find it; 12 to 20 of
        // 20 seen under heavy emulated load).
        use crate::kassert;
        let cpu = last_cpu()?;
        let mut before_tick = 0;
        for _ in 0..20 {
            let s = Arc::new((WaitQueue::new(), AtomicBool::new(false), AtomicU64::new(u64::MAX)));
            let t = s.clone();
            let h = spawn_test(cpu, "woken", move || {
                t.0.wait_until(|| t.1.load(Ordering::Acquire));
                t.2.store(arch::idt::ticks_on(cpu), Ordering::Release);
            })?;
            kassert!(yield_until(|| s.0.waiters.lock().len() == 1));
            s.1.store(true, Ordering::Release);
            let t0 = arch::idt::ticks_on(cpu);
            s.0.wake_all();
            join_bounded(h)?;
            if s.2.load(Ordering::Acquire) == t0 {
                before_tick += 1;
            }
        }
        crate::ktest::diag(format_args!("woken before the next tick {before_tick} of 20 times"));
        kassert!(before_tick >= 5);
        Ok(())
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
