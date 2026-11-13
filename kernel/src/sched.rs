//! Kernel threads: preemptive round robin, one run queue per CPU.
//!
//! A [`Thread`] has its own kernel stack from `mm::stack` (with a guard page,
//! hard rule 4), its saved `rsp` while it isn't running, and a [`State`].
//! [`spawn`] starts one on this CPU, [`spawn_on`] on another; **a thread
//! never moves**: it runs only on the CPU it was spawned on (its home), so
//! only that CPU ever takes threads off its queue, and a thread's saved
//! registers are touched only by that CPU, with interrupts off. Waking a
//! thread from another CPU puts it on its home's queue and sends that CPU
//! the `WAKEUP` IPI (in case it is halted in its idle loop).
//!
//! **Switching** ([`schedule`]) happens only with interrupts off and no
//! critical section open (`arch::irq::depth() == 0`, asserted): a held
//! `SpinLock`'s section belongs to the CPU, so one open across a switch
//! would be closed by another thread. A thread that gives up the CPU
//! ([`yield_now`], [`sleep`], [`WaitQueue::wait_until`], [`exit`]) turns
//! interrupts off with `arch::irq::disable` and keeps whether they were on
//! in a local on its own stack, restoring it when it runs again. The timer
//! interrupt ([`tick`], 1 kHz on every CPU) preempts: it wakes sleepers
//! whose deadline has passed and, when the running thread's slice
//! ([`SLICE_TICKS`]) is used up and another thread is ready, switches; the
//! preempted thread resumes inside the interrupt handler and `iretq`s back.
//!
//! Every CPU has an idle thread, which runs only when nothing else is ready
//! and is never queued: on an AP it is the AP's own `smp::ap_main` loop (on
//! its main stack; `smp::run_on` jobs still run there), on the boot CPU a
//! thread of its own. `kmain`'s context becomes the boot CPU's `main` thread
//! (kmon, or the ktests).
//!
//! A thread that returns from its entry point (or calls [`exit`]) is dead;
//! the next thread to run on its CPU frees its stack (the unmap's TLB flush
//! is local, and that CPU is the one that ran on it), and only then does
//! [`JoinHandle::join`] return.
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

/// How long [`init`] waits, in all, for every other CPU's scheduler to start.
const START_TIMEOUT_NS: u64 = 1_000_000_000;

/// Where a thread is in its life.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// On its CPU's run queue (or about to be put there), waiting for a turn.
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
    fn from_raw(raw: u8) -> State {
        match raw {
            0 => State::Ready,
            1 => State::Running,
            2 => State::Sleeping,
            3 => State::Blocked,
            _ => State::Dead,
        }
    }

    /// The name of the state whose number is `raw` (a trace event's), if any.
    pub fn name_of(raw: u64) -> Option<&'static str> {
        u8::try_from(raw).ok().filter(|&r| r <= State::Dead as u8).map(|r| State::from_raw(r).name())
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

/// A kernel thread.
pub struct Thread {
    id: u64,
    name: &'static str,
    /// Its home CPU, the only one it runs on.
    cpu: usize,
    state: AtomicU8,
    /// The `rsp` [`arch::context::switch`] saved while it isn't running.
    rsp: UnsafeCell<u64>,
    /// Its stack, `None` for a thread made from code that was already
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

// SAFETY: `rsp` is the only field without its own synchronization. Only the
// thread's home CPU touches it, inside `schedule` with interrupts off: it
// saves into it while switching away from the thread and reads it while
// switching to it, and a thread is never on two CPUs (it never moves). The
// value `spawn_on` stores before the thread is shared is published by the
// run queue's lock.
unsafe impl Sync for Thread {}

impl Thread {
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn state(&self) -> State {
        State::from_raw(self.state.load(Ordering::Acquire))
    }

    /// Its stack, unless it has none (see `stack`).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
    pub fn stack(&self) -> Option<StackRange> {
        self.stack.lock().as_ref().map(KernelStack::range)
    }

    fn set_state(&self, state: State) {
        self.state.store(state as u8, Ordering::Release);
    }

    fn transition(&self, from: State, to: State) -> bool {
        self.state.compare_exchange(from as u8, to as u8, Ordering::AcqRel, Ordering::Acquire).is_ok()
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
            rsp: UnsafeCell::new(rsp),
            stack: SpinLock::new(stack),
            entry: SpinLock::new(entry),
            reaped: AtomicBool::new(false),
            exited: WaitQueue::new(),
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
            slice_left: SLICE_TICKS,
        }
    }

    fn on_idle(&self) -> bool {
        match (&self.current, &self.idle) {
            (Some(c), Some(i)) => Arc::ptr_eq(c, i),
            _ => false,
        }
    }

    /// Make every sleeper whose deadline is `now` or earlier ready.
    fn wake_sleepers(&mut self, now: u64) {
        let mut i = 0;
        while i < self.sleepers.len() {
            if self.sleepers[i].0 <= now {
                let (_, t) = self.sleepers.swap_remove(i);
                if t.transition(State::Sleeping, State::Ready) {
                    self.ready.push_back(t);
                }
            } else {
                i += 1;
            }
        }
    }
}

static CPUS: [SpinLock<Cpu>; MAX_CPUS] = [const { SpinLock::new(Cpu::new()) }; MAX_CPUS];
/// Whether each CPU's scheduler runs: its current and idle threads are set
/// and its timer ticks. Only moves to true.
static RUNNING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
/// How many threads are on each CPU's run queue, for its idle loop to read
/// without the lock.
static READY: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
/// Each CPU's current thread, for the crash reporter (lock-free). Points into
/// an `Arc` that CPU's `current` (or, mid-switch, `switched_from`) holds.
static CURRENT: [AtomicPtr<Thread>; MAX_CPUS] = [const { AtomicPtr::new(ptr::null_mut()) }; MAX_CPUS];
/// Each CPU's idle thread (null until set), to tell it apart lock-free.
static IDLE: [AtomicPtr<Thread>; MAX_CPUS] = [const { AtomicPtr::new(ptr::null_mut()) }; MAX_CPUS];
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Whether CPU `cpu`'s scheduler runs (threads can be spawned there).
pub fn running_on(cpu: usize) -> bool {
    RUNNING.get(cpu).is_some_and(|r| r.load(Ordering::Acquire))
}

/// The thread running on this CPU, once its scheduler is set up.
pub fn current() -> Option<Arc<Thread>> {
    CPUS[arch::cpu_index()].lock().current.clone()
}

/// The running thread's id and name, for the crash report: lock-free, on
/// the CPU that crashed (whose current thread can't change any more).
pub fn running_here() -> Option<(u64, &'static str)> {
    let t = CURRENT[arch::cpu_index()].load(Ordering::Acquire);
    // SAFETY: a non-null `CURRENT` points into an `Arc<Thread>` this CPU's
    // run queue holds (as `current`, or `switched_from` until the switch is
    // finished) for as long as it is stored here; only this CPU stores it.
    (!t.is_null()).then(|| unsafe { ((*t).id, (*t).name) })
}

fn is_idle(cpu: usize) -> bool {
    let idle = IDLE[cpu].load(Ordering::Relaxed);
    !idle.is_null() && CURRENT[cpu].load(Ordering::Relaxed) == idle
}

/// Can the running thread block here? Only once this CPU's scheduler runs,
/// not on its idle thread (which must always be ready to run), and with
/// interrupts on and no critical section open (so not in an interrupt
/// handler or with a lock held).
fn can_block(cpu: usize) -> bool {
    running_on(cpu) && arch::irq::enabled() && arch::irq::depth() == 0 && !is_idle(cpu)
}

/// Why [`spawn_on`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// That CPU's scheduler doesn't run (no such CPU, not online, or before
    /// [`init`]).
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
        t.exited.wait_until(|| t.reaped.load(Ordering::Acquire));
    }
}

/// Start a thread running `f` on this CPU.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub fn spawn(name: &'static str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, SpawnError> {
    spawn_on(arch::cpu_index(), name, f)
}

/// Start a thread running `f` on CPU `cpu`, where it will always run. It is
/// queued behind the threads already ready there.
pub fn spawn_on(cpu: usize, name: &'static str, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, SpawnError> {
    if !running_on(cpu) {
        return Err(SpawnError::NotRunning);
    }
    let thread = Arc::new(new_thread(name, cpu, Box::new(f))?);
    enqueue(thread.clone());
    Ok(JoinHandle { thread })
}

/// A ready thread with a fresh stack, that starts by running `entry`.
fn new_thread(name: &'static str, cpu: usize, entry: Entry) -> Result<Thread, SpawnError> {
    let stack = KernelStack::new(STACK_SIZE, StackKind::Thread).map_err(SpawnError::Stack)?;
    // SAFETY: a fresh, mapped stack (page-aligned top) nothing runs on; the
    // thread owns it, and it is freed only after the thread has died and its
    // CPU has switched away from it.
    let rsp = unsafe { arch::context::prepare(stack.range().top, thread_entry) };
    Ok(Thread::new(name, cpu, State::Ready, rsp, Some(stack), Some(entry)))
}

/// A thread for the code running now on this CPU, on a stack it doesn't own.
fn adopt(name: &'static str) -> Arc<Thread> {
    Arc::new(Thread::new(name, arch::cpu_index(), State::Running, 0, None, None))
}

/// Put a ready thread on its CPU's queue, and wake that CPU if it isn't this
/// one (it may be halted in its idle loop; if the IPI can't be sent, its next
/// tick picks the thread up).
fn enqueue(thread: Arc<Thread>) {
    let cpu = thread.cpu;
    {
        let mut q = CPUS[cpu].lock();
        q.ready.push_back(thread);
        READY[cpu].store(q.ready.len(), Ordering::Release);
    }
    if cpu != arch::cpu_index() {
        smp::wake(cpu);
    }
}

/// Where every spawned thread starts (through `arch::context`), on its own
/// stack, switched to by [`schedule`] with interrupts off and no critical
/// section open.
extern "C" fn thread_entry() -> ! {
    finish_switch(arch::cpu_index());
    arch::irq::enable();
    let me = current().expect("a thread runs on a CPU with a scheduler");
    let entry = me.entry.lock().take().expect("a thread starts once");
    drop(me);
    entry();
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

/// Block the running thread for at least `ns` nanoseconds (by `now_ns`; the
/// tick on its CPU wakes it, so up to a tick later, or more where an
/// emulator delivers ticks late). Returns false, doing nothing, where it
/// can't block (see [`can_block`]): `time::sleep` busy-waits then.
pub fn sleep(ns: u64) -> bool {
    let cpu = arch::cpu_index();
    if !can_block(cpu) {
        return false;
    }
    let deadline = time::now_ns().saturating_add(ns);
    let were_on = arch::irq::disable();
    {
        let mut q = CPUS[cpu].lock();
        let me = q.current.clone().expect("a running scheduler has a current thread");
        me.set_state(State::Sleeping);
        q.sleepers.push((deadline, me));
    }
    schedule(cpu);
    arch::irq::restore(were_on);
    true
}

/// End the running thread (a thread's entry returning does this). Its stack
/// is freed by the next thread on its CPU, and then its joiners wake.
pub fn exit() -> ! {
    let cpu = arch::cpu_index();
    assert!(running_on(cpu) && !is_idle(cpu), "only a spawned thread can exit");
    let _ = arch::irq::disable();
    CPUS[cpu].lock().current.as_ref().expect("a running scheduler has a current thread").set_state(State::Dead);
    schedule(cpu);
    unreachable!("a dead thread was switched back to")
}

/// Switch from the running thread to the next ready one on CPU `cpu` (this
/// one), or its idle thread, or keep running it. The running thread goes
/// back on the queue only if it is still running (not if it is sleeping,
/// blocked, dead, or already woken and queued again by a waker). With
/// interrupts off and no critical section open; returns when the running
/// thread is switched back to.
fn schedule(cpu: usize) {
    assert!(!arch::irq::enabled() && arch::irq::depth() == 0, "switching threads with interrupts on or a lock held");
    let (save, load) = {
        let mut q = CPUS[cpu].lock();
        let prev = q.current.take().expect("a running scheduler has a current thread");
        let prev_is_idle = q.idle.as_ref().is_some_and(|i| Arc::ptr_eq(i, &prev));
        if prev_is_idle {
            prev.set_state(State::Ready);
        } else if prev.transition(State::Running, State::Ready) {
            q.ready.push_back(prev.clone());
        }
        let next = match q.ready.pop_front() {
            Some(t) => t,
            None => q.idle.clone().expect("a running scheduler has an idle thread"),
        };
        READY[cpu].store(q.ready.len(), Ordering::Release);
        q.slice_left = SLICE_TICKS;
        next.set_state(State::Running);
        if Arc::ptr_eq(&next, &prev) {
            q.current = Some(next);
            return;
        }
        crate::ktrace!(Kind::Switch, prev.id, next.id, prev.state());
        CURRENT[cpu].store(Arc::as_ptr(&next).cast_mut(), Ordering::Release);
        let save = prev.rsp.get();
        // SAFETY: this CPU is `next`'s home, the only one that touches its
        // `rsp`, and it isn't running it (see `Thread`'s `Sync`).
        let load = unsafe { *next.rsp.get() };
        q.current = Some(next);
        q.switched_from = Some(prev);
        (save, load)
    };
    // SAFETY: interrupts are off (asserted above); `save` is the running
    // thread's `rsp` cell, which `switched_from` keeps alive until the next
    // thread has finished the switch; `load` was saved by `switch` (or made
    // by `context::prepare`) on `next`'s stack, which stays mapped until it
    // dies and is switched away from, and which no CPU runs on (`next` is
    // this CPU's, and wasn't running).
    unsafe { arch::context::switch(save, load) };
    finish_switch(cpu);
}

/// Finish a switch on CPU `cpu`, on the thread switched to: let go of the
/// thread switched away from, and if it is dead, free its stack (on its own
/// CPU, which ran on it, so the unmap's local TLB flush is enough) and wake
/// its joiners. Interrupts off.
fn finish_switch(cpu: usize) {
    let Some(prev) = CPUS[cpu].lock().switched_from.take() else { return };
    if prev.state() == State::Dead {
        drop(prev.stack.lock().take());
        prev.reaped.store(true, Ordering::Release);
        prev.exited.wake_all();
    }
}

/// The timer interrupt on this CPU (interrupts off, EOI sent): wake the
/// sleepers whose deadline has passed, and switch threads if the running
/// one's slice is used up (or the idle thread runs) and another is ready.
pub fn tick() {
    let cpu = arch::cpu_index();
    if !running_on(cpu) || arch::irq::depth() != 0 {
        return;
    }
    let now = time::now_ns();
    let switch = {
        let mut q = CPUS[cpu].lock();
        q.wake_sleepers(now);
        READY[cpu].store(q.ready.len(), Ordering::Release);
        q.slice_left = q.slice_left.saturating_sub(1);
        !q.ready.is_empty() && (q.slice_left == 0 || q.on_idle())
    };
    if switch {
        schedule(cpu);
    }
}

/// Threads waiting for something; whoever makes it happen wakes them.
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
    /// missed; it must not block or take a lock held around a wake. Panics
    /// where the thread can't block (see [`can_block`]).
    pub fn wait_until(&self, mut ready: impl FnMut() -> bool) {
        let cpu = arch::cpu_index();
        assert!(can_block(cpu), "blocking where a thread can't (no scheduler, the idle thread, or interrupts off)");
        loop {
            let were_on = arch::irq::disable();
            {
                let mut waiters = self.waiters.lock();
                if ready() {
                    drop(waiters);
                    arch::irq::restore(were_on);
                    return;
                }
                let me = CPUS[cpu].lock().current.clone().expect("a running scheduler has a current thread");
                // A waker on another CPU may make it ready (and queue it)
                // before this CPU switches away: `schedule` then neither
                // queues it again nor loses it.
                me.set_state(State::Blocked);
                waiters.push_back(me);
            }
            schedule(cpu);
            arch::irq::restore(were_on);
        }
    }

    /// Wake the longest-waiting thread; whether there was one.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests; input (#58)
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

fn wake(thread: Arc<Thread>) {
    if thread.transition(State::Blocked, State::Ready) {
        enqueue(thread);
    }
}

/// Run the scheduler on the boot CPU: `kmain`'s context becomes the `main`
/// thread, the boot CPU gets an idle thread, and the other CPUs are woken
/// to start their timers and schedulers (each AP's idle loop does that once
/// the timer count is published); waits for them, bounded. After
/// `time::init` and `smp::wait_online`, with interrupts on.
pub fn init() {
    assert_eq!(arch::cpu_index(), 0, "sched::init is for the boot CPU");
    assert!(time::timer_published(), "sched::init before time::init");
    let idle = Arc::new(
        new_thread("idle", 0, Box::new(|| idle_loop(0, || false, || {})))
            .unwrap_or_else(|e| panic!("the boot CPU's idle thread: {e}")),
    );
    let main = adopt("main");
    {
        let mut q = CPUS[0].lock();
        CURRENT[0].store(Arc::as_ptr(&main).cast_mut(), Ordering::Release);
        IDLE[0].store(Arc::as_ptr(&idle).cast_mut(), Ordering::Release);
        q.current = Some(main);
        q.idle = Some(idle);
    }
    // The boot CPU's timer already ticks (`time::init`).
    RUNNING[0].store(true, Ordering::Release);
    let cpus: Vec<usize> = (1..MAX_CPUS).filter(|&c| smp::online_lapic_id(c).is_some()).collect();
    for &cpu in &cpus {
        smp::wake(cpu);
    }
    let deadline = time::now_ns() + START_TIMEOUT_NS;
    while !cpus.iter().all(|&c| running_on(c)) && time::now_ns() < deadline {
        core::hint::spin_loop();
    }
    for &cpu in cpus.iter().filter(|&&c| !running_on(c)) {
        warn!(target: "sched", "cpu {cpu}'s scheduler didn't start: its threads can't run");
    }
    let n = 1 + cpus.iter().filter(|&&c| running_on(c)).count();
    info!(target: "sched", "threads on {n} CPU{}, {SLICE_TICKS} ms slices", if n == 1 { "" } else { "s" });
}

/// An AP's life from `smp::ap_main` on: its running context becomes its idle
/// thread, which starts its timer and scheduler once the boot CPU publishes
/// the timer's count, runs ready threads, and runs the job `run_job` takes
/// whenever `has_job()` (`smp::run_on`'s mailbox, from these frames, so a
/// crash in a job backtraces through `ap_main`).
pub fn run_ap_idle(has_job: impl Fn() -> bool, run_job: impl Fn()) -> ! {
    let cpu = arch::cpu_index();
    let me = adopt("idle");
    {
        let mut q = CPUS[cpu].lock();
        CURRENT[cpu].store(Arc::as_ptr(&me).cast_mut(), Ordering::Release);
        IDLE[cpu].store(Arc::as_ptr(&me).cast_mut(), Ordering::Release);
        q.current = Some(me.clone());
        q.idle = Some(me);
    }
    idle_loop(cpu, has_job, run_job);
    unreachable!("an idle loop returned")
}

/// A CPU's idle thread: halt until there is work (a ready thread, a job, or
/// for an AP whose timer isn't running yet, the timer's count), then do it.
fn idle_loop(cpu: usize, has_job: impl Fn() -> bool, run_job: impl Fn()) {
    loop {
        if !running_on(cpu) {
            let started = {
                let _off = arch::irq::push_off();
                time::start_timer_here()
            };
            if started {
                RUNNING[cpu].store(true, Ordering::Release);
            }
        }
        // Checked with interrupts off: a wakeup can't slip in before the halt.
        arch::irq::wait_unless(|| {
            READY[cpu].load(Ordering::Acquire) > 0 || has_job() || (!running_on(cpu) && time::timer_published())
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
/// test waits this long when it passes; bounded so a broken scheduler fails
/// instead of hanging).
#[cfg(feature = "ktest")]
const TEST_DEADLINE_NS: u64 = 10_000_000_000;

/// Spawn two threads on `cpu` that never yield, and check that they
/// interleave: each counts its own loops, and runs until both have seen the
/// other's count move on three separate occasions (so each was switched
/// away from and back to at least three times, by preemption alone). Each
/// also checks it always runs on `cpu`.
#[cfg(feature = "ktest")]
fn interleave_on(cpu: usize) -> crate::ktest::TestResult {
    use crate::kassert;
    struct Shared {
        counts: [AtomicU64; 2],
        seen: [AtomicU64; 2],
        wrong_cpu: AtomicBool,
        deadline: u64,
    }
    let shared = Arc::new(Shared {
        counts: [AtomicU64::new(0), AtomicU64::new(0)],
        seen: [AtomicU64::new(0), AtomicU64::new(0)],
        wrong_cpu: AtomicBool::new(false),
        deadline: time::now_ns() + TEST_DEADLINE_NS,
    });
    let spin = |me: usize, s: Arc<Shared>| {
        move || {
            let other = 1 - me;
            let mut last = s.counts[other].load(Ordering::Relaxed);
            loop {
                let both = s.seen[0].load(Ordering::Relaxed) >= 3 && s.seen[1].load(Ordering::Relaxed) >= 3;
                if both || time::now_ns() > s.deadline {
                    break;
                }
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
    let a = spawn_on(cpu, "spin-a", spin(0, shared.clone())).map_err(|_| "spawn failed")?;
    let b = spawn_on(cpu, "spin-b", spin(1, shared.clone())).map_err(|_| "spawn failed")?;
    a.join();
    b.join();
    let seen = [shared.seen[0].load(Ordering::Relaxed), shared.seen[1].load(Ordering::Relaxed)];
    crate::ktest::diag(format_args!("cpu {cpu}: each saw the other run {seen:?} times"));
    kassert!(seen[0] >= 3 && seen[1] >= 3);
    kassert!(!shared.wrong_cpu.load(Ordering::Relaxed));
    Ok(())
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

crate::ktest!(
    fn two_threads_interleave() {
        // On this CPU, while `main` waits for them: preemption by the timer
        // alone switches between two threads that never yield.
        interleave_on(arch::cpu_index())
    }
);

crate::ktest!(
    fn threads_run_on_every_cpu() {
        // Two non-yielding threads on every CPU interleave there (so every
        // CPU's timer preempts), and never run anywhere else.
        use crate::kassert;
        let cpus = running_cpus();
        crate::ktest::diag(format_args!("schedulers on cpus {cpus:?}"));
        kassert!(cpus.len() > 1);
        kassert!(cpus.iter().all(|&c| smp::online_lapic_id(c).is_some()));
        for cpu in cpus {
            interleave_on(cpu)?;
        }
        Ok(())
    }
);

crate::ktest!(
    fn sleeping_thread_wakes() {
        // A thread sleeping 20 ms blocks (it is seen sleeping while `main`
        // runs on the same CPU) and wakes no earlier than its deadline.
        use crate::kassert;
        let elapsed = Arc::new(AtomicU64::new(0));
        let out = elapsed.clone();
        let h = spawn("sleeper", move || {
            let t0 = time::now_ns();
            time::sleep(20_000_000);
            out.store(time::now_ns() - t0, Ordering::Relaxed);
        })
        .map_err(|_| "spawn failed")?;
        let seen_sleeping = yield_until(|| h.thread().state() == State::Sleeping);
        h.join();
        let ns = elapsed.load(Ordering::Relaxed);
        crate::ktest::diag(format_args!("slept {ns} ns"));
        kassert!(seen_sleeping);
        kassert!(ns >= 20_000_000);
        Ok(())
    }
);

crate::ktest!(
    fn wait_queue_wakes_a_blocked_thread() {
        // A thread waiting on a queue blocks until its condition holds and
        // it is woken: on this CPU, and from another (the remote wake path:
        // its home's queue and a wakeup IPI).
        use crate::{kassert, kassert_eq};
        struct Shared {
            queue: WaitQueue,
            go: AtomicBool,
            passed: AtomicBool,
        }
        let mut cpus = running_cpus();
        cpus.truncate(2);
        for cpu in cpus {
            let s = Arc::new(Shared {
                queue: WaitQueue::new(),
                go: AtomicBool::new(false),
                passed: AtomicBool::new(false),
            });
            let t = s.clone();
            let h = spawn_on(cpu, "waiter", move || {
                t.queue.wait_until(|| t.go.load(Ordering::Acquire));
                t.passed.store(true, Ordering::Release);
            })
            .map_err(|_| "spawn failed")?;
            kassert!(yield_until(|| h.thread().state() == State::Blocked));
            kassert!(!s.passed.load(Ordering::Acquire));
            // Woken without its condition: it blocks again.
            kassert!(s.queue.wake_one());
            kassert!(yield_until(|| h.thread().state() == State::Blocked && !s.queue.waiters.lock().is_empty()));
            kassert!(!s.passed.load(Ordering::Acquire));
            s.go.store(true, Ordering::Release);
            kassert_eq!(s.queue.wake_all(), 1);
            h.join();
            kassert!(s.passed.load(Ordering::Acquire));
            kassert!(!s.queue.wake_one());
        }
        Ok(())
    }
);

crate::ktest!(
    fn switches_are_traced() {
        // Switching to a thread and from it (dead) are both in the ring.
        use crate::kassert;
        let ring = crate::trace::ring();
        let start = ring.written();
        let h = spawn("traced", || {}).map_err(|_| "spawn failed")?;
        let id = h.thread().id();
        h.join();
        let me = current().ok_or("no current thread")?.id();
        let switches: Vec<[u64; 4]> = (start..ring.written())
            .filter_map(|seq| ring.read(seq))
            .filter(|e| e.kind == Kind::Switch as u16)
            .map(|e| e.args)
            .collect();
        kassert!(switches.iter().any(|a| a[0] == me && a[1] == id));
        kassert!(switches.iter().any(|a| a[0] == id && a[2] == State::Dead as u64));
        Ok(())
    }
);

crate::ktest!(
    fn a_dead_threads_stack_is_freed() {
        // Once `join` returns, the thread's stack is unregistered and its
        // pages unmapped.
        use crate::kassert;
        let h = spawn("brief", || {}).map_err(|_| "spawn failed")?;
        let range = h.thread().stack().ok_or("a spawned thread has a stack")?;
        kassert!(arch::stacks::stack_of(range.bottom).is_some());
        let thread = h.thread.clone();
        h.join();
        kassert!(thread.state() == State::Dead && thread.stack().is_none());
        kassert!(arch::stacks::stack_of(range.bottom).is_none());
        let bottom = lumen_mm::VirtAddr::new(range.bottom).ok_or("a stack address is canonical")?;
        kassert!(crate::mm::paging::translate(bottom).is_none());
        Ok(())
    }
);

crate::ktest!(
    fn spawning_where_no_scheduler_runs_is_refused() {
        use crate::kassert;
        kassert!(matches!(spawn_on(MAX_CPUS, "nowhere", || {}), Err(SpawnError::NotRunning)));
        Ok(())
    }
);
