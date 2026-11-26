//! Kernel threads: preemptive round robin, one run queue per CPU.
//!
//! A [`Thread`] has its own kernel stack from `mm::stack` (with a guard page,
//! hard rule 4), its saved `rsp` while it isn't running, and a [`State`].
//! [`spawn`] and [`spawn_on`] pin kernel threads. Unpinned user threads
//! (roots, and fork children within the inherited mask) and
//! [`spawn_migratable`] threads may move between allowed CPUs while Ready,
//! after `finish_switch` releases their `on_cpu` ownership. Run queue locks
//! serialize placement; the outgoing CPU saves all state before releasing
//! ownership, and the incoming CPU acquires it before restoring state.
//!
//! **Switching** ([`schedule`]) happens only with interrupts off and no
//! critical section open (`arch::irq::depth() == 0`, asserted): a held
//! `SpinLock`'s section belongs to the CPU, so one open across a switch
//! would be closed by another thread. A thread that gives up the CPU
//! ([`yield_now`], [`sleep`], [`WaitQueue::wait_until`],
//! [`WaitQueue::wait_until_or_deadline`], and M8's
//! [`WaitQueue::wait_interruptible`] and [`WaitQueue::wait_killable`], which
//! a pending signal ([`Thread::sigpending`], [`Thread::fatal_pending`]) and
//! a sender's [`kick`] also end: Linux's `TASK_INTERRUPTIBLE` and
//! `TASK_KILLABLE`) turns interrupts
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
//! ([`FINISHING`]), and only then does [`JoinHandle::join`] return. The
//! unmap's TLB flush is local; CPUs that ran the thread earlier, its
//! spawner and a scoped thread's caller may keep stale entries for its pages. Nothing uses them until the stack's addresses are
//! reused, and they are reused only once every CPU has flushed them
//! (`mm::tlb`, #300).
//!
//! Lock order: a wait queue's lock, then run queues in ascending CPU order
//! (steals, affinity relocation after a wake/kick/tick, and finish_switch
//! may hold two; a signal sender may hold its signal lock around [`kick`]),
//! then a thread's `stack` lock (only
//! [`exit`] nests it); then the heap, page tables and frame allocator. Every
//! live thread has room reserved in every CPU's queues ([`Cpu::reserved`],
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
//! thread that was running (`thread: <name> (id N)`, with a nonblocking
//! leaf-lock read and `?` if busy). Its `ThreadName` is 16 bytes inline,
//! set at spawn and changed only by the owner on exec. Reading it allocates
//! nothing; the spawn functions take anything `Into<ThreadName>`,
//! a `&str` cut to 15 bytes). Every
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
use core::sync::atomic::{AtomicBool, AtomicI8, AtomicPtr, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use log::{info, warn};
use lumen_mm::stacks::{StackKind, StackRange};

use crate::arch::{self, MAX_CPUS};
use crate::linux::process::UserThread;
use crate::linux::signal::RestartBlock;
use crate::mm::stack::{KernelStack, STACK_SIZE, StackError};
use crate::mm::user_vm::AddressSpace;
use crate::sync::{SpinLock, SpinLockGuard};
use crate::trace;
use crate::{smp, time};
use lumen_trace::ThreadName;
use lumen_trace::kind::{RunQueue, RunQueueOp, Switch, ThreadState};

/// Timer ticks (1 ms each) a thread runs before the next ready thread on its
/// CPU gets a turn, at nice 0.
pub const SLICE_TICKS: u32 = 10;
const _: () = assert!(MAX_CPUS <= 64);
const ALL_CPUS: u64 = u64::MAX >> (64 - MAX_CPUS);

/// How long [`init`] waits, in all, for every CPU's scheduler to start.
const START_TIMEOUT_NS: u64 = 1_000_000_000;

/// Where a thread is in its life. The numbers are `ThreadState`'s, which a
/// `switch` trace event records (pinned in `lumen_trace::kind`).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Waiting for a turn: on its CPU's run queue (or about to be put there),
    /// or an idle thread that isn't running (never queued).
    Ready = ThreadState::Ready as u8,
    /// Its CPU's current thread.
    Running = ThreadState::Running as u8,
    /// Waiting for a deadline ([`sleep`]).
    Sleeping = ThreadState::Sleeping as u8,
    /// Waiting on a [`WaitQueue`].
    Blocked = ThreadState::Blocked as u8,
    /// Finished; its stack is freed once its CPU has switched away.
    Dead = ThreadState::Dead as u8,
}

impl From<State> for ThreadState {
    fn from(s: State) -> ThreadState {
        match s {
            State::Ready => ThreadState::Ready,
            State::Running => ThreadState::Running,
            State::Sleeping => ThreadState::Sleeping,
            State::Blocked => ThreadState::Blocked,
            State::Dead => ThreadState::Dead,
        }
    }
}

impl From<ThreadState> for State {
    fn from(s: ThreadState) -> State {
        match s {
            ThreadState::Ready => State::Ready,
            ThreadState::Running => State::Running,
            ThreadState::Sleeping => State::Sleeping,
            ThreadState::Blocked => State::Blocked,
            ThreadState::Dead => State::Dead,
        }
    }
}

impl State {
    const ALL: [State; 5] = [State::Ready, State::Running, State::Sleeping, State::Blocked, State::Dead];

    fn from_raw(raw: u8) -> Option<State> {
        Self::ALL.into_iter().find(|&s| s as u8 == raw)
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

// SAFETY: the CPU owning `on_cpu` alone accesses the saved context, with
// interrupts off. A Release in finish_switch publishes the completed save;
// queue locks and an Acquire of on_cpu precede the next CPU's claim. Initial
// context is published by the first enqueue's lock.
unsafe impl Sync for SavedRsp {}

impl SavedRsp {
    /// Where `switch` saves it.
    ///
    /// # Safety
    /// On the thread's owning CPU, with interrupts off.
    unsafe fn slot(&self) -> *mut u64 {
        self.0.get()
    }

    /// What `switch` loads.
    ///
    /// # Safety
    /// On the thread's owning CPU, with interrupts off, the thread not running.
    unsafe fn get(&self) -> u64 {
        // SAFETY: upheld by the caller: nothing writes it meanwhile.
        unsafe { *self.0.get() }
    }
}

/// Per-user-thread hardware and VM state carried across context switches,
/// and the thread's Linux identity and restart block.
///
/// Accessed only on its thread's owning CPU with interrupts off: by
/// `schedule` switching to and from the thread, and by the thread itself
/// (`detach_user_space`, a `UserSpaceGuard`, `arch_prctl`'s
/// `set_current_fs_base`, the `#PF` path's `current_address_space`, the
/// restart-block accessors (`set_restart_block`, `take_restart_block`,
/// `restart_block`, including the syscall-exit hook's `resolve_restart`
/// reset), and `current_user_thread`/`set_current_user_thread`, which a Linux
/// process's registration, syscalls and takedown call); or exclusively in
/// `Drop`.
struct UserThreadState {
    address_space: Arc<AddressSpace>,
    fpu: UnsafeCell<arch::fpu::UserFpuState>,
    fs_base: AtomicU64,
    kernel_rsp: AtomicU64,
    user_exit_rsp: AtomicU64,
    pending_debug_trap: arch::percpu::AtomicDebugTrap,
    /// The Linux thread it runs ([`set_current_user_thread`]): `None` until
    /// a root registers; fork children get it at spawn. None again after
    /// takedown, and for a ktest's
    /// [`UserSpaceGuard`]. Read lock-free by its own thread
    /// ([`current_user_thread`]).
    linux: Option<Arc<UserThread>>,
    /// What `restart_syscall` resumes (B2): set by a handler that returns
    /// `-ERESTART_RESTARTBLOCK`, taken by `restart_syscall`, and reset by
    /// the syscall-exit hook's `resolve_restart`. Read and written only by
    /// its own thread with interrupts off
    /// ([`set_restart_block`], [`take_restart_block`]).
    restart: UnsafeCell<Option<RestartBlock>>,
}

impl UserThreadState {
    fn new(address_space: Arc<AddressSpace>) -> Self {
        Self {
            address_space,
            fpu: UnsafeCell::new(arch::fpu::UserFpuState::new()),
            fs_base: AtomicU64::new(0),
            kernel_rsp: AtomicU64::new(0),
            user_exit_rsp: AtomicU64::new(0),
            pending_debug_trap: arch::percpu::AtomicDebugTrap::new(),
            linux: None,
            restart: UnsafeCell::new(None),
        }
    }

    /// Save the live CPU FPU/SSE/AVX registers into this thread's buffer.
    ///
    /// # Safety
    /// Called on this thread's owning CPU with interrupts disabled.
    unsafe fn save_fpu(&self) {
        // SAFETY: exclusive access on the owning CPU with interrupts off.
        unsafe { (*self.fpu.get()).save() };
    }

    /// Restore the CPU's FPU/SSE/AVX registers from this thread's buffer.
    ///
    /// # Safety
    /// Called on this thread's owning CPU with interrupts disabled.
    unsafe fn restore_fpu(&self) {
        // SAFETY: exclusive access on the owning CPU with interrupts off.
        unsafe { (*self.fpu.get()).restore() };
    }
}

/// Thread-local slot for optional [`UserThreadState`].
struct UserSlot(UnsafeCell<Option<UserThreadState>>);

// SAFETY: only the owning thread with interrupts off, or the CPU claiming
// it through on_cpu, accesses the slot. Release after the full switch and
// Acquire before the next claim serialize migration. Drop is exclusive.
unsafe impl Sync for UserSlot {}

impl UserSlot {
    fn new(state: Option<UserThreadState>) -> Self {
        Self(UnsafeCell::new(state))
    }

    /// Borrow the inner `Option<UserThreadState>`.
    ///
    /// # Safety
    /// Called on the thread's owning CPU with interrupts disabled.
    unsafe fn as_ref(&self) -> Option<&UserThreadState> {
        // SAFETY: upheld by the caller.
        unsafe { (*self.0.get()).as_ref() }
    }

    /// Raw mutable pointer to the inner `Option<UserThreadState>`.
    ///
    /// # Safety
    /// Called on the thread's owning CPU with interrupts disabled, with no
    /// overlapping reference into this slot.
    unsafe fn slot(&self) -> *mut Option<UserThreadState> {
        self.0.get()
    }
}

#[derive(Clone, Copy)]
pub(crate) struct TaskName {
    display: ThreadName,
    bytes: [u8; 16],
}
impl TaskName {
    /// At most fifteen raw comm bytes, NUL-terminated and zero-padded,
    /// with a separately sanitized display name, without allocating.
    /// Each invalid UTF-8 sequence displays as `?`.
    pub(crate) fn from_comm(comm: &[u8]) -> Self {
        let end = comm.iter().take(15).position(|&b| b == 0).unwrap_or(comm.len().min(15));
        let mut bytes = [0; 16];
        bytes[..end].copy_from_slice(&comm[..end]);
        let mut display_bytes = [0; 15];
        let mut len = 0;
        // Valid chunks plus one ASCII byte per invalid sequence cannot
        // exceed the raw comm's length. Controls use ThreadName's rule.
        for chunk in bytes[..end].utf8_chunks() {
            let valid = chunk.valid().as_bytes();
            display_bytes[len..len + valid.len()].copy_from_slice(valid);
            len += valid.len();
            if !chunk.invalid().is_empty() {
                display_bytes[len] = b'?';
                len += 1;
            }
        }
        let display =
            ThreadName::new(core::str::from_utf8(&display_bytes[..len]).expect("valid UTF-8 chunks and ASCII"));
        Self { display, bytes }
    }

    fn from_display(display: ThreadName) -> Self {
        let mut bytes = [0; 16];
        bytes[..display.as_str().len()].copy_from_slice(display.as_str().as_bytes());
        Self { display, bytes }
    }
}

impl From<&str> for TaskName {
    fn from(name: &str) -> Self {
        Self::from_display(ThreadName::from(name))
    }
}

/// A schedulable thread whose saved state belongs exclusively to its
/// `on_cpu` owner, or is available for a new owner under its run queue lock.
pub struct Thread {
    id: u64,
    /// Leaf lock; only this thread changes its name after construction.
    name: SpinLock<TaskName>,
    /// Its queue's CPU, or the CPU it last ran on; changed under both queues.
    cpu: AtomicUsize,
    /// Held through the complete outgoing context save, including the stack.
    on_cpu: AtomicBool,
    affinity: AtomicU64,
    /// Immutable: kernel pins and CPU-local user probes cannot be widened.
    pinned: bool,
    nice: AtomicI8,
    state: AtomicU8,
    rsp: SavedRsp,
    /// Optional per-process user VM, FPU, TLS, ring-3 stack and restart-block state.
    user: UserSlot,
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
    cpu_time: [AtomicU64; 2],
    /// Its [`WaitQueue`] waits that the timer ended at their deadline, not
    /// a waker (for the ktests: [`deadline_wakes_in`]).
    #[cfg(feature = "ktest")]
    deadline_wakes: AtomicU64,
    /// Itself, for [`kick`] (which is handed a `&Thread` and queues an
    /// `Arc`): set when it is made, and never upgraded once it is dropped.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // `kick`'s: the ktests; C5's senders next
    me: Weak<Thread>,
    /// Linux's `TIF_SIGPENDING`, "look at the pending signals": set by a
    /// sender before its [`kick`]. The ktests aside, only the thread itself
    /// clears it, in the syscall-exit hook when nothing deliverable is left
    /// (`linux::signal::settle_pending`); production callers hold the signal
    /// lock when the thread has a process. An interruptible wait reads it.
    sigpending: AtomicBool,
    /// Linux's `__fatal_signal_pending`: set before the [`kick`] by whatever
    /// ends the thread (a group kill, `de_thread`), never cleared. A
    /// killable wait reads it, and so does [`kick`] (whether a killable
    /// wait ends).
    fatal_pending: AtomicBool,
    /// The [`WaitKind`] of its latest [`WaitQueue`] wait, stored before it
    /// is set `Blocked` (so a [`kick`] that sees it blocked sees this
    /// wait's kind, or a later one's).
    wait_kind: AtomicU8,
}

/// Placement and priority installed before a thread is published.
#[derive(Clone, Copy)]
struct Placement {
    affinity: u64,
    pinned: bool,
    nice: i8,
}

impl Placement {
    const fn pinned(cpu: usize) -> Self {
        // Invalid spawn targets are rejected before constructing the thread.
        Self { affinity: if cpu < MAX_CPUS { 1 << cpu } else { 0 }, pinned: true, nice: 0 }
    }

    const fn migratable() -> Self {
        Self { affinity: ALL_CPUS, pinned: false, nice: 0 }
    }
}

/// Why an affinity change is refused (both map to EINVAL for C12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AffinityError {
    /// The requested mask contains no online CPU.
    NoOnlineCpu,
    /// The thread has an immutable pin (including idle threads).
    Pinned,
}

impl Thread {
    /// A ready thread on CPU `cpu` with a fresh stack of `stack_size` bytes,
    /// that starts by running `entry`.
    fn spawned(
        name: TaskName,
        cpu: usize,
        stack_size: u64,
        entry: Entry,
        user: Option<UserThreadState>,
        placement: Placement,
    ) -> Result<Arc<Thread>, SpawnError> {
        let stack = KernelStack::new(stack_size, StackKind::Thread).map_err(SpawnError::Stack)?;
        // SAFETY: a fresh, mapped stack (page-aligned top) nothing runs on;
        // the thread owns it, and it is freed only after the thread has died
        // and its CPU has switched away from it.
        let rsp = unsafe { arch::context::prepare(stack.range().top, thread_entry) };
        Ok(register(Arc::new_cyclic(|me| {
            Thread::new(name, cpu, State::Ready, rsp, Some(stack), Some(entry), user, me.clone(), placement)
        })))
    }

    /// A thread for the code running now on this CPU, on a stack it doesn't
    /// own.
    fn adopted(name: ThreadName) -> Arc<Thread> {
        register(Arc::new_cyclic(|me| {
            Thread::new(
                TaskName::from_display(name),
                arch::cpu_index(),
                State::Running,
                0,
                None,
                None,
                None,
                me.clone(),
                Placement::pinned(arch::cpu_index()),
            )
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        name: TaskName,
        cpu: usize,
        state: State,
        rsp: u64,
        stack: Option<KernelStack>,
        entry: Option<Entry>,
        user: Option<UserThreadState>,
        me: Weak<Thread>,
        placement: Placement,
    ) -> Thread {
        Thread {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            name: SpinLock::new(name),
            cpu: AtomicUsize::new(cpu),
            on_cpu: AtomicBool::new(state == State::Running),
            affinity: AtomicU64::new(placement.affinity),
            pinned: placement.pinned,
            nice: AtomicI8::new(placement.nice),
            state: AtomicU8::new(state as u8),
            rsp: SavedRsp(UnsafeCell::new(rsp)),
            user: UserSlot::new(user),
            stack: SpinLock::new(stack),
            entry: SpinLock::new(entry),
            reaped: AtomicBool::new(false),
            exited: WaitQueue::new(),
            ticks: AtomicU64::new(0),
            cpu_time: [const { AtomicU64::new(0) }; 2],
            #[cfg(feature = "ktest")]
            deadline_wakes: AtomicU64::new(0),
            me,
            sigpending: AtomicBool::new(false),
            fatal_pending: AtomicBool::new(false),
            wait_kind: AtomicU8::new(WaitKind::Uninterruptible as u8),
        }
    }

    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Both name representations, copied together for fork.
    pub(crate) fn task_name(&self) -> TaskName {
        *self.name.lock()
    }

    /// Its current displayed name, updated by exec from the path as given.
    pub fn name(&self) -> ThreadName {
        self.name.lock().display
    }

    fn charge_tick(&self, from_user: bool) {
        self.ticks.fetch_add(1, Ordering::Relaxed);
        self.cpu_time[usize::from(!from_user)].fetch_add(1, Ordering::Relaxed);
    }

    /// User and system CPU time in timer ticks (one millisecond each).
    pub fn cpu_time(&self) -> [u64; 2] {
        self.cpu_time.each_ref().map(|t| t.load(Ordering::Relaxed))
    }

    /// Linux comm bytes, NUL-terminated and zero-filled to sixteen bytes.
    pub fn comm(&self) -> [u8; 16] {
        self.name.lock().bytes
    }

    /// The queue CPU, or the CPU this thread last ran on.
    pub fn cpu(&self) -> usize {
        self.cpu.load(Ordering::Acquire)
    }

    /// The scheduling priority, clamped to Linux's nice range.
    pub fn nice(&self) -> i8 {
        self.nice.load(Ordering::Relaxed)
    }

    /// Set the priority used when the next slice starts.
    pub fn set_nice(&self, nice: i32) {
        self.nice.store(nice.clamp(-20, 19) as i8, Ordering::Relaxed);
    }

    /// CPUs on which this thread may next run.
    pub fn affinity(&self) -> u64 {
        self.affinity.load(Ordering::Acquire)
    }

    fn allows(&self, cpu: usize) -> bool {
        self.affinity() & (1 << cpu) != 0
    }

    /// Set a CPU mask with at least one online CPU. Immutable pins refuse
    /// every change. A running thread leaves an excluded CPU at its next
    /// switch; sleepers and blocked threads move only after waking.
    /// Callers must not hold a run queue lock.
    pub fn set_affinity(&self, mask: u64) -> Result<(), AffinityError> {
        if self.pinned {
            return Err(AffinityError::Pinned);
        }
        let mask = mask & ALL_CPUS;
        if mask & online_mask() == 0 {
            return Err(AffinityError::NoOnlineCpu);
        }
        let (cpu, q) = lock_queue_of(self);
        self.affinity.store(mask, Ordering::Release);
        drop(q);
        relocate_disallowed(self);
        let _ = smp::wake(cpu);
        Ok(())
    }

    /// Claim the saved context under its queue lock before restoring it.
    fn claim(&self) {
        assert!(!self.on_cpu.swap(true, Ordering::Acquire), "next thread still on a CPU");
    }

    /// Publish a complete context save; the caller holds the queue lock or
    /// the thread is dead, so no new owner races the remaining cleanup.
    fn release(&self) {
        self.on_cpu.store(false, Ordering::Release);
    }

    fn may_move_to(&self, cpu: usize) -> bool {
        !self.pinned && self.state() == State::Ready && self.allows(cpu) && !self.on_cpu.load(Ordering::Acquire)
    }

    pub fn state(&self) -> State {
        State::from_raw(self.state.load(Ordering::Acquire)).expect("only a State is stored")
    }

    /// Its stack, unless it has none (see `stack`).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
    pub fn stack(&self) -> Option<StackRange> {
        self.stack.lock().as_ref().map(KernelStack::range)
    }

    /// Whether it has been told to look at its pending signals
    /// (`sigpending`, Linux's `TIF_SIGPENDING`).
    pub fn sigpending(&self) -> bool {
        self.sigpending.load(Ordering::SeqCst)
    }

    /// Whether something ends it (`fatal_pending`, Linux's
    /// `__fatal_signal_pending`): once set, never cleared.
    pub fn fatal_pending(&self) -> bool {
        self.fatal_pending.load(Ordering::SeqCst)
    }

    /// Tell it to look at its pending signals: a sender's first step,
    /// before its [`kick`].
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // the ktests; C5's senders next
    pub fn set_sigpending(&self) {
        self.sigpending.store(true, Ordering::SeqCst);
    }

    /// Mark it as ending (a group kill, `de_thread`), also setting
    /// `sigpending`, before the [`kick`] that
    /// ends a killable wait. Never undone.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // the ktests; C3's group kills next
    pub fn set_fatal_pending(&self) {
        self.set_sigpending();
        self.fatal_pending.store(true, Ordering::SeqCst);
    }

    /// Ktests only: release a broken syscall-exit loop so its test can
    /// join the user thread and report the failed return assertion.
    #[cfg(feature = "ktest")]
    pub(crate) fn clear_sigpending_for_test(&self) {
        self.sigpending.store(false, Ordering::SeqCst);
    }

    /// Whether a wait of `kind` must end for what is pending: never an
    /// uninterruptible one, an interruptible one on `sigpending`, a killable
    /// one on `fatal_pending`.
    fn pending_for(&self, kind: WaitKind) -> bool {
        match kind {
            WaitKind::Uninterruptible => false,
            WaitKind::Interruptible => self.sigpending(),
            WaitKind::Killable => self.fatal_pending(),
        }
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
                self.name.try_lock().map(|n| n.display).unwrap_or(ThreadName::new("?")),
                self.id,
                self.cpu(),
                from.name(),
                to.name()
            );
        }
    }
}

impl fmt::Debug for Thread {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (id {}, cpu {}, {})",
            self.name.try_lock().map(|n| n.display).unwrap_or(ThreadName::new("?")),
            self.id,
            self.cpu(),
            self.state().name()
        )
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
    /// Published non-idle threads plus unpublished reservations counted
    /// here. Both ready and sleeper capacity cover this count, independent
    /// of placement; a thread is published only after reserving every CPU.
    reserved: usize,
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
            reserved: 0,
            slice_left: SLICE_TICKS,
        }
    }

    /// Reserve for one more live thread. Failure leaves the count unchanged;
    /// a successful earlier buffer growth may remain available for reuse.
    fn try_home(&mut self) -> Result<(), SpawnError> {
        let reserved = self.reserved.checked_add(1).ok_or(SpawnError::NoMemory)?;
        self.ready.try_reserve(reserved.saturating_sub(self.ready.len())).map_err(|_| SpawnError::NoMemory)?;
        self.sleepers.try_reserve(reserved.saturating_sub(self.sleepers.len())).map_err(|_| SpawnError::NoMemory)?;
        self.reserved = reserved;
        Ok(())
    }

    /// Runnable work includes the current non-idle thread only while it is
    /// Running; an outgoing Ready thread is already counted in the queue.
    fn runnable(&self) -> usize {
        self.ready.len()
            + usize::from(self.current.as_ref().is_some_and(|t| !self.is_idle_thread(t) && t.state() == State::Running))
    }

    /// Take the thread this CPU has finished switching away from.
    fn take_switched_from(&mut self) -> Option<Arc<Thread>> {
        self.switched_from.take()
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
        assert!(self.ready.len() < self.ready.capacity(), "queueing thread {t:?}, which isn't reserved on cpu {cpu}");
        let id = t.id;
        self.ready.push_back(t);
        self.queue_changed(cpu, id, RunQueueOp::Enqueue);
    }

    /// Take the oldest eligible thread with a complete saved context, or
    /// this CPU's current thread woken before it switched away.
    fn pop_ready(&mut self, cpu: usize) -> Option<Arc<Thread>> {
        let at = self.ready.iter().position(|t| {
            t.allows(cpu)
                && (!t.on_cpu.load(Ordering::Acquire)
                    || core::ptr::eq(Arc::as_ptr(t), CURRENT[cpu].load(Ordering::Acquire)))
        })?;
        let t = self.ready.remove(at).expect("position exists");
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
/// for work without the lock and balance_pass can select donors.
static READY: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
/// Each CPU's current thread, for the crash reporter (lock-free); only that
/// CPU stores it. It points into an `Arc` that CPU's scheduler holds:
/// `current`, or, during a switch, `schedule`'s `prev` local (until it is
/// set to the next thread, just before the switch itself) and then its
/// `next` local (until that moves into `current`); a crash never unwinds
/// them.
static CURRENT: [AtomicPtr<Thread>; MAX_CPUS] = [const { AtomicPtr::new(ptr::null_mut()) }; MAX_CPUS];
/// Each CPU's idle thread's id (0 until its threads are set up), for
/// [`running_id_here`]; stored once, by [`install`].
static IDLE_ID: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
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
/// Ktests: the knob that stalls one thread's next [`WaitQueue`] wait at a
/// [`StallAt`] point, holding only the queue's lock (with interrupts off),
/// while another CPU kicks it: before `Blocked`, before the pending read,
/// or after that read was false (`kick_racing_a_block_is_never_lost`).
#[cfg(feature = "ktest")]
pub(crate) static BLOCK_DELAY: BlockDelay = BlockDelay::new();

/// Where [`BLOCK_DELAY`] stalls a wait.
#[cfg(feature = "ktest")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StallAt {
    /// Queued, its wait's kind stored, not yet `Blocked`: a kick then finds
    /// it running and does nothing, so only its own read of what is pending,
    /// after `Blocked`, can end the wait.
    BeforeBlocked = 1,
    /// `Blocked`, before the pending read: either the kick or that read
    /// can end the wait. A separate state assertion checks the kick.
    AfterBlocked = 2,
    /// `Blocked`, after the pending read was false, before `schedule`:
    /// only the kick can queue it and end the wait.
    AfterRead = 3,
    /// User state saved, before assembly saves the outgoing kernel stack.
    MidSwitch = 4,
}

/// [`BLOCK_DELAY`]'s state: the thread to stall (0: none) and where,
/// whether it is stalled now, and the kicker's go-ahead that ends the stall
/// (or, failing it, a bound of 1 s, noted in `timed_out`).
#[cfg(feature = "ktest")]
pub(crate) struct BlockDelay {
    thread: AtomicU64,
    at: AtomicU8,
    stalled: AtomicBool,
    go: AtomicBool,
    timed_out: AtomicBool,
}

#[cfg(feature = "ktest")]
impl BlockDelay {
    const fn new() -> Self {
        Self {
            thread: AtomicU64::new(0),
            at: AtomicU8::new(0),
            stalled: AtomicBool::new(false),
            go: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
        }
    }

    /// Stall thread `id`'s next queue wait once, `at` that point.
    pub(crate) fn arm(&self, id: u64, at: StallAt) {
        self.stalled.store(false, Ordering::SeqCst);
        self.go.store(false, Ordering::SeqCst);
        self.timed_out.store(false, Ordering::SeqCst);
        self.at.store(at as u8, Ordering::SeqCst);
        self.thread.store(id, Ordering::SeqCst);
    }

    /// Whether the armed thread is at its configured `StallAt` point now;
    /// at `BeforeBlocked` it is still running.
    pub(crate) fn stalled(&self) -> bool {
        self.stalled.load(Ordering::SeqCst)
    }

    /// End the stall (the kicker has kicked), or, if the thread never got
    /// to it, stall nothing more.
    pub(crate) fn release(&self) {
        self.thread.store(0, Ordering::SeqCst);
        self.go.store(true, Ordering::SeqCst);
    }

    /// Whether the last stall ended at its bound, not by [`release`](Self::release).
    pub(crate) fn timed_out(&self) -> bool {
        self.timed_out.load(Ordering::SeqCst)
    }

    /// The wait's side, at point `at`.
    fn stall(&self, me: &Thread, at: StallAt) {
        if self.at.load(Ordering::SeqCst) != at as u8
            || self.thread.compare_exchange(me.id, 0, Ordering::SeqCst, Ordering::SeqCst).is_err()
        {
            return;
        }
        self.stalled.store(true, Ordering::SeqCst);
        let end = time::now_ns() + 1_000_000_000;
        let timed_out = loop {
            if self.go.load(Ordering::SeqCst) {
                break false;
            }
            if time::now_ns() >= end {
                break true;
            }
            core::hint::spin_loop();
        };
        // Preserve why the loop ended: a late release must not turn an
        // expired stall into an apparently clean round.
        self.timed_out.store(timed_out, Ordering::SeqCst);
        self.stalled.store(false, Ordering::SeqCst);
    }
}

/// Ticks each CPU's scheduler has taken (from its start), and how many of
/// them found the idle thread running. A tick is counted in `TICKED` before
/// `IDLE_TICKS` (whose increment releases it), and [`cpu_ticks`] reads them
/// the other way round (acquiring), so a reading never has more idle ticks
/// than ticks.
static TICKED: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
static IDLE_TICKS: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];
/// Change only the calling thread's name after exec.
pub(crate) fn set_current_name(name: TaskName) {
    with_current(|t| *t.name.lock() = name).expect("exec on a running thread");
}

/// Total context switches performed on each CPU (monotonic).
static SWITCHES: [AtomicU64; MAX_CPUS] = [const { AtomicU64::new(0) }; MAX_CPUS];

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
    pub name: ThreadName,
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
            name: t.name(),
            cpu: t.cpu(),
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
    /// The ring's newest `runq` event; `None` if the ring holds none.
    pub traced: Option<Traced>,
    /// The ring's next sequence number then: every change to the queue
    /// before this reading has a smaller one, every change after it a
    /// larger one (both record under the lock).
    pub head: u64,
}

/// The newest `runq` event in a CPU's X-ray ring ([`QueueView`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Traced {
    /// The queue's length it says.
    pub len: u64,
    /// Its sequence number.
    pub seq: u64,
}

/// CPU `cpu`'s [`QueueView`], read under its queue's lock, so nothing
/// changes the queue between the two readings (a change records its event
/// under the same lock). `None` past the CPUs Lumen supports. The ring's
/// scan takes no lock and allocates nothing.
pub fn run_queue(cpu: usize) -> Option<QueueView> {
    let q = CPUS.get(cpu)?.lock();
    let head = trace::xray_ring(cpu)?.written();
    let traced = trace::newest_run_queue(cpu).map(|(rq, seq)| Traced { len: rq.len, seq });
    Some(QueueView { len: q.ready.len(), traced, head })
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
    let _irq = arch::irq::push_off();
    CPUS[arch::cpu_index()].lock().current.clone()
}

/// The running thread's id and name, with a nonblocking leaf-lock read
/// and `?` if busy. Preemption is excluded while selecting its CPU and
/// reading the current slot. Set
/// from when this CPU's threads are set up (`sched::init` on the boot CPU,
/// coming online on an AP).
pub fn running_here() -> Option<(u64, ThreadName)> {
    let _irq = arch::irq::push_off();
    let t = CURRENT[arch::cpu_index()].load(Ordering::Acquire);
    // SAFETY: a non-null `CURRENT` points into an `Arc<Thread>` that stays
    // alive while it is stored there (see `CURRENT`).
    (!t.is_null())
        .then(|| unsafe { ((*t).id, (*t).name.try_lock().map(|n| n.display).unwrap_or(ThreadName::new("?"))) })
}

/// Total context switches performed on CPU `cpu` so far (0 for out-of-range CPU).
pub fn switch_count(cpu: usize) -> u64 {
    SWITCHES.get(cpu).map_or(0, |c| c.load(Ordering::Acquire))
}

/// The current thread's [`AddressSpace`], if it is a user thread (lock-free,
/// safe from `#PF` and syscall handlers).
pub fn current_address_space() -> Option<Arc<AddressSpace>> {
    let _irq = arch::irq::push_off();
    let t = CURRENT[arch::cpu_index()].load(Ordering::Acquire);
    if t.is_null() {
        return None;
    }
    // SAFETY: `t` is the running thread on this CPU and interrupts are off so
    // `schedule()` cannot run or mutate `user`.
    unsafe { (*t).user.as_ref().map(|u| u.address_space.clone()) }
}

/// The current thread's Linux thread (a clone), if it is a user thread whose
/// process is registered (lock-free, safe from syscall and fault handlers).
pub fn current_user_thread() -> Option<Arc<UserThread>> {
    let _irq = arch::irq::push_off();
    let t = CURRENT[arch::cpu_index()].load(Ordering::Acquire);
    if t.is_null() {
        return None;
    }
    // SAFETY: `t` is the running thread on this CPU and interrupts are off, so
    // `schedule()` cannot run or change `user`; only this thread writes
    // `linux` (`set_current_user_thread`) or takes the state
    // (`detach_user_space`), and it is here.
    unsafe { (*t).user.as_ref().and_then(|u| u.linux.clone()) }
}

/// Make `linux` the current user thread's Linux thread (`None` to detach
/// it), returning the one it replaces, which the caller drops with
/// interrupts on. Only the thread itself sets it, with interrupts off; only
/// `linux::process`'s registration and takedown call it.
///
/// Panics if the current thread has no user state (a kernel thread).
pub(crate) fn set_current_user_thread(linux: Option<Arc<UserThread>>) -> Option<Arc<UserThread>> {
    let _irq = arch::irq::push_off();
    let t = CURRENT[arch::cpu_index()].load(Ordering::Acquire);
    assert!(!t.is_null(), "set_current_user_thread before sched::init");
    // SAFETY: `t` is the running thread on this CPU with interrupts off, and
    // no other reference into its slot is live: none is held across a call
    // into this module, and nothing else on this CPU runs meanwhile.
    let slot = unsafe { &mut *(*t).user.slot() };
    let user = slot.as_mut().expect("set_current_user_thread on a kernel thread");
    core::mem::replace(&mut user.linux, linux)
}

/// Snapshot the running user's live FP state with context switches excluded.
pub fn snapshot_user_fpu() -> Option<arch::fpu::UserFpuState> {
    with_user_fpu(|fp| {
        fp.save();
        fp.clone()
    })
}

/// Restore only an image validated by lumen-signal, or the initial state.
pub fn restore_signal_fpu(fp: Option<lumen_signal::frame::ValidatedFp<'_>>) {
    with_user_fpu(|state| state.install_signal(fp)).expect("signal on a user thread");
}

fn with_user_fpu<R>(f: impl FnOnce(&mut arch::fpu::UserFpuState) -> R) -> Option<R> {
    let _irq = arch::irq::push_off();
    with_current(|t| {
        // SAFETY: this is the owning thread with interrupts off. The callback
        // cannot retain the borrow, block, or access this buffer recursively.
        unsafe { t.user.as_ref().map(|u| f(&mut *u.fpu.get())) }
    })
    .flatten()
}

/// Make `block` the current user thread's restart block (`None` to clear
/// it): what `restart_syscall` resumes after the syscall-exit hook turns
/// `-ERESTART_RESTARTBLOCK` into a restart. Handlers go through
/// `linux::signal::interrupted_with`. Only the thread itself calls it,
/// with interrupts off here.
///
/// Panics if the current thread has no user state (a kernel thread).
pub(crate) fn set_restart_block(block: Option<RestartBlock>) {
    with_restart_block(|slot| *slot = block).expect("set_restart_block on a kernel thread");
}

/// Access the current user thread's restart block with interrupts off.
fn with_restart_block<R>(f: impl FnOnce(&mut Option<RestartBlock>) -> R) -> Option<R> {
    let _irq = arch::irq::push_off();
    with_current(|t| {
        // SAFETY: only the running thread touches its user state and restart
        // block, interrupts are off, and no reference escapes this callback.
        unsafe { t.user.as_ref().map(|u| f(&mut *u.restart.get())) }
    })
    .flatten()
}

/// Take the current user thread's restart block, leaving none
/// (`restart_syscall`): `None` if it has none, or is a kernel thread.
pub fn take_restart_block() -> Option<RestartBlock> {
    with_restart_block(Option::take).flatten()
}

/// The current user thread's restart block, left in place: the handler's
/// no-overwrite assertion and the ktests.
pub(crate) fn restart_block() -> Option<RestartBlock> {
    with_restart_block(|slot| *slot).flatten()
}

/// Clear the running thread's flag when nothing deliverable remains (the
/// ktests aside). Production callers hold the signal lock when the thread
/// has a process.
pub(crate) fn clear_current_sigpending() {
    let _ = with_current(|t| t.sigpending.store(false, Ordering::SeqCst));
}

/// Whether the running thread has a signal to look at (`sigpending`),
/// lock-free: the syscall-exit path's checks, with interrupts off. `false`
/// before this CPU's threads are set up.
pub fn current_sigpending() -> bool {
    let _irq = arch::irq::push_off();
    let t = CURRENT[arch::cpu_index()].load(Ordering::Acquire);
    // SAFETY: a non-null `CURRENT` is the running thread, alive while it
    // runs; interrupts are off, so it can't change under this read.
    !t.is_null() && unsafe { (*t).sigpending() }
}

/// `f` on the running thread, lock-free (`None` before this CPU's threads
/// are set up): for the syscall-exit hook, which must not take the run
/// queue's lock on every return to ring 3.
pub fn with_current<R>(f: impl FnOnce(&Thread) -> R) -> Option<R> {
    let t = {
        let _irq = arch::irq::push_off();
        CURRENT[arch::cpu_index()].load(Ordering::Acquire)
    };
    // SAFETY: this points to the calling thread, whose execution keeps it
    // alive. A switch transfers its owning Arc through the run queue and
    // current slots, even across CPUs; on resume it is the same thread.
    // No borrowed UserSlot state may escape an IRQ-off caller's section.
    (!t.is_null()).then(|| f(unsafe { &*t }))
}

/// The current user thread's `IA32_FS_BASE` (`0` for non-user threads).
pub fn current_fs_base() -> u64 {
    let _irq = arch::irq::push_off();
    let t = CURRENT[arch::cpu_index()].load(Ordering::Acquire);
    if t.is_null() {
        return 0;
    }
    // SAFETY: `t` is the running thread on this CPU with interrupts off.
    unsafe { (*t).user.as_ref().map_or(0, |u| u.fs_base.load(Ordering::Relaxed)) }
}

/// Update the current user thread's `fs_base` and load it into `IA32_FS_BASE`
/// atomically (with interrupts off so a context switch cannot interleave).
///
/// Panics if `addr >= USER_MAX` or if the current thread has no `UserThreadState`.
pub fn set_current_fs_base(addr: u64) {
    let _irq = arch::irq::push_off();
    let t = CURRENT[arch::cpu_index()].load(Ordering::Acquire);
    assert!(!t.is_null(), "set_current_fs_base before sched::init");
    // SAFETY: `t` is the running thread on this CPU with interrupts off.
    let user = unsafe { (*t).user.as_ref() }.expect("set_current_fs_base on a kernel thread");
    // SAFETY: interrupts are off on this thread's owning CPU and `write_fs_base`
    // asserts `addr < USER_MAX`.
    unsafe { arch::fpu::write_fs_base(addr) };
    user.fs_base.store(addr, Ordering::Relaxed);
}

/// Replace the current thread's address space and load its root atomically.
/// process::replace_current_mm is the only Linux caller: it updates the process's
/// matching reference before re-enabling interrupts, then retires the old mm.
pub(crate) fn replace_user_space(space: Arc<AddressSpace>) -> Arc<AddressSpace> {
    let _irq = arch::irq::push_off();
    let t = CURRENT[arch::cpu_index()].load(Ordering::Acquire);
    assert!(!t.is_null());
    // SAFETY: only the current thread accesses its slot, with interrupts off.
    let user = unsafe { &mut *(*t).user.slot() }.as_mut().expect("user thread");
    let old = core::mem::replace(&mut user.address_space, space);
    // SAFETY: interrupts are off; user keeps the new space alive until its
    // next switch/detach, and old keeps the previously loaded space alive here.
    unsafe { AddressSpace::switch_loaded(Some(&user.address_space)) };
    old
}

/// Detach the current thread's [`UserThreadState`], switching `CR3` back to
/// `kernel_root()` and clearing `TSS.rsp0`, `PerCpu`, and `IA32_FS_BASE`.
/// A Linux process's thread is detached by its takedown
/// (`linux::process::take_current`), which first clears `linux`: a debug
/// assertion checks it is gone, so no Linux identity is dropped here
/// unnoticed.
///
/// Returns the detached [`AddressSpace`] (if any) with interrupts restored,
/// so the caller can tear it down (or give up its process user) with
/// interrupts on.
pub fn detach_user_space() -> Option<Arc<AddressSpace>> {
    let detached = {
        let _irq = arch::irq::push_off();
        let cpu = arch::cpu_index();
        let t = CURRENT[cpu].load(Ordering::Acquire);
        if t.is_null() {
            return None;
        }
        // SAFETY: `t` is the running thread on `cpu` with interrupts off.
        let prev = unsafe { (*(*t).user.slot()).take() };
        debug_assert!(
            prev.as_ref().is_none_or(|u| u.linux.is_none()),
            "detach_user_space on a Linux thread its process's takedown hasn't detached"
        );
        if prev.is_some() {
            // SAFETY: interrupts are off on `cpu`; `prev` keeps the previously
            // loaded `AddressSpace` alive across the `CR3` switch.
            unsafe { AddressSpace::switch_loaded(None) };
            // SAFETY: interrupts are off on `cpu` and no user context is active.
            unsafe {
                arch::gdt::set_rsp0(cpu, 0);
                arch::fpu::write_fs_base(0);
            }
            arch::percpu::set_kernel_rsp(0);
            arch::percpu::set_user_exit_rsp(0);
            arch::percpu::set_pending_debug_trap(None);
        }
        prev
    };
    // `UserFpuState` drops here with interrupts restored (heap dealloc safe).
    detached.map(|u| u.address_space)
}

/// RAII guard attaching an [`AddressSpace`] to the current kernel thread (e.g.
/// for in-kernel demand-paging and `.lumen_fixup` ktests) with full preemption
/// safety. Detaches and restores `kernel_root()` on drop.
#[cfg(feature = "ktest")]
pub struct UserSpaceGuard {
    _not_send: core::marker::PhantomData<*const ()>,
}

#[cfg(feature = "ktest")]
impl UserSpaceGuard {
    /// Attach `space` to the current thread and activate its `CR3`.
    pub fn enter(space: Arc<AddressSpace>) -> Self {
        // Allocate `UserFpuState` before disabling interrupts.
        let state = UserThreadState::new(space);
        {
            let _irq = arch::irq::push_off();
            let cpu = arch::cpu_index();
            let t = CURRENT[cpu].load(Ordering::Acquire);
            assert!(!t.is_null(), "UserSpaceGuard::enter before sched::init");
            // SAFETY: `t` is the running thread on `cpu` with interrupts off.
            let slot = unsafe { &mut *(*t).user.slot() };
            assert!(slot.is_none(), "nested UserSpaceGuard on cpu {cpu}");
            *slot = Some(state);
            let installed = slot.as_ref().expect("just stored");
            // SAFETY: interrupts are off on `cpu`; `installed.address_space` is
            // alive in `(*t).user` and no prior user space was loaded.
            unsafe {
                AddressSpace::switch_loaded(Some(&installed.address_space));
                arch::gdt::set_rsp0(cpu, 0);
                arch::fpu::write_fs_base(0);
                installed.restore_fpu();
            }
            arch::percpu::set_kernel_rsp(0);
            arch::percpu::set_user_exit_rsp(0);
            arch::percpu::set_pending_debug_trap(None);
        }
        Self { _not_send: core::marker::PhantomData }
    }
}

#[cfg(feature = "ktest")]
impl Drop for UserSpaceGuard {
    fn drop(&mut self) {
        let _ = detach_user_space();
    }
}

/// The running thread's id on this CPU and whether it is the CPU's idle
/// thread, lock-free (for the profiler's timer-tick samples, `perf`): `None`
/// before this CPU's threads are set up. Called with interrupts off, so the
/// running thread can't change under it.
pub fn running_id_here() -> Option<(u64, bool)> {
    let cpu = arch::cpu_index();
    running_here().map(|(id, _)| (id, id == IDLE_ID[cpu].load(Ordering::Relaxed)))
}

/// Can the running thread block here? Only with interrupts on and
/// everything [`blockable_but_for_interrupts`] asks.
fn can_block() -> bool {
    arch::irq::enabled() && blockable_but_for_interrupts()
}

/// Could the running thread block here if interrupts were on? For the crash
/// path (`crash::halt_if_stopping_elsewhere`), with interrupts off.
pub(crate) fn can_block_with_interrupts_on() -> bool {
    blockable_but_for_interrupts()
}

/// Everything [`can_block`] asks but interrupts on: first reject an open
/// critical section, then disable interrupts to select the current CPU,
/// check its scheduler and finally its run queue for a non-idle thread.
/// An interrupt handler that opens no section is not distinguished from
/// a thread here; can_block also requires interrupts enabled.
fn blockable_but_for_interrupts() -> bool {
    if arch::irq::depth() != 0 {
        return false;
    }
    let _irq = arch::irq::push_off();
    let cpu = arch::cpu_index();
    running_on(cpu) && !CPUS[cpu].lock().on_idle()
}

/// Whether the running thread can block here (see [`can_block`]).
pub fn can_block_now() -> bool {
    can_block()
}

/// Why [`spawn_on`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpawnError {
    /// That CPU's scheduler doesn't run (no such CPU, not online, before
    /// [`init`], or its timer doesn't tick).
    NotRunning,
    /// No room to reserve the per-CPU queues.
    NoMemory,
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
            SpawnError::NoMemory => f.write_str("no memory for scheduler queues"),
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

    /// Whether the thread has ended and its CPU is done with it: whether
    /// [`join`](Self::join) would return at once. For a caller that must
    /// not block for good on a thread whose CPU may never run it (one
    /// stuck with interrupts off): poll this, then join.
    pub fn is_finished(&self) -> bool {
        let t = &self.thread;
        t.reaped.load(Ordering::Acquire) && FINISHING[t.cpu()].load(Ordering::Acquire) != t.id
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
        while FINISHING[t.cpu()].load(Ordering::Acquire) == t.id {
            yield_now();
        }
    }
}

/// Start a thread running `f` on this CPU.
pub fn spawn(name: impl Into<ThreadName>, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, SpawnError> {
    spawn_on(arch::cpu_index(), name, f)
}

/// Run `f` in a new thread on this CPU with a stack of `stack_size` bytes
/// (a nonzero multiple of 4 KiB; anything else panics) instead of
/// [`STACK_SIZE`], and wait for it to finish: for deeply recursive code (the
/// script interpreter). As it is joined before this returns, `f` may borrow
/// from the caller. If no thread can be made, or the calling thread can't
/// block (see [`JoinHandle::join`]), `f` is dropped unrun.
pub fn run_sized<'a>(
    name: impl Into<ThreadName>,
    stack_size: u64,
    f: impl FnOnce() + Send + 'a,
) -> Result<(), SpawnError> {
    // The SAFETY argument below needs a panic to end the kernel, not unwind
    // past the join.
    #[cfg(not(panic = "abort"))]
    compile_error!("sched::run_sized relies on panic = \"abort\"");
    let cpu = arch::cpu_index();
    if !can_block() {
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
    spawn_with(cpu, name.into(), stack_size, f)?.join();
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
    if !can_block() {
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
    pub fn spawn_on(
        &self,
        cpu: usize,
        name: impl Into<ThreadName>,
        f: impl FnOnce() + Send + 'env,
    ) -> Result<(), SpawnError> {
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
        let thread = spawn_with(cpu, name.into(), STACK_SIZE, f)?;
        self.threads.borrow_mut().push(thread);
        Ok(())
    }
}

/// Start a thread running `f` on CPU `cpu`, where it will always run. It is
/// queued behind the threads already ready there.
pub fn spawn_on(
    cpu: usize,
    name: impl Into<ThreadName>,
    f: impl FnOnce() + Send + 'static,
) -> Result<JoinHandle, SpawnError> {
    spawn_with(cpu, name.into(), STACK_SIZE, Box::new(f))
}

/// Start a migratable user thread on CPU `cpu`, with clean FPU state and zero TLS.
/// Roots install their Linux identity at registration. Registered threads
/// must call take_current before returning; an unregistered thread's state
/// is detached by thread_entry. Fork children use spawn_fork_child.
pub fn spawn_user_on(
    cpu: usize,
    name: impl Into<ThreadName>,
    space: Arc<AddressSpace>,
    f: impl FnOnce() + Send + 'static,
) -> Result<JoinHandle, SpawnError> {
    let name = TaskName::from_display(name.into());
    spawn_configured(cpu, name, STACK_SIZE, Box::new(f), Some(UserThreadState::new(space)), Placement::migratable())
}

/// Start a user probe permanently pinned to `cpu`, with clean FPU and TLS.
/// The registration and cleanup contract is the same as [`spawn_user_on`].
pub fn spawn_user_pinned_on(
    cpu: usize,
    name: impl Into<ThreadName>,
    space: Arc<AddressSpace>,
    f: impl FnOnce() + Send + 'static,
) -> Result<JoinHandle, SpawnError> {
    let name = TaskName::from_display(name.into());
    spawn_configured(cpu, name, STACK_SIZE, Box::new(f), Some(UserThreadState::new(space)), Placement::pinned(cpu))
}

/// Spawn a fork child with inherited affinity, nice and FPU state, on the
/// caller's CPU if allowed, otherwise its lowest allowed online CPU. It has
/// its Linux identity and TLS installed before enqueue. The closure f must
/// hold itself at a start gate until its identity is linked. Before returning,
/// f must either call take_current, or clear linux and detach the space on
/// cancellation without releasing the creator's process-user count. This
/// function supplies neither the gate nor that cleanup.
pub(crate) fn spawn_fork_child(
    name: impl Into<TaskName>,
    space: Arc<AddressSpace>,
    linux: Arc<UserThread>,
    fs_base: u64,
    f: impl FnOnce() + Send + 'static,
) -> Result<JoinHandle, SpawnError> {
    assert!(fs_base < arch::fpu::USER_MAX);
    let mut state = UserThreadState::new(space);
    let (cpu, placement) = {
        // Snapshot placement with the caller's FPU state: migration cannot
        // change the current CPU between selecting its mask and its queue.
        let _irq = arch::irq::push_off();
        state.fpu.get_mut().save();
        let me = current().expect("fork on a running thread");
        let affinity = me.affinity();
        let here = arch::cpu_index();
        let cpu = if affinity & (1 << here) != 0 { here } else { lowest_online(affinity) };
        (cpu, Placement { affinity, pinned: false, nice: me.nice() })
    };
    state.fs_base.store(fs_base, Ordering::Relaxed);
    state.linux = Some(linux);
    spawn_configured(cpu, name.into(), STACK_SIZE, Box::new(f), Some(state), placement)
}

/// CPUs whose schedulers can accept threads. CPU hot-unplug is unsupported.
pub fn online_mask() -> u64 {
    (0..MAX_CPUS).filter(|&cpu| running_on(cpu)).fold(0, |mask, cpu| mask | (1 << cpu))
}

/// The lowest online CPU allowed by a nonempty effective affinity mask.
fn lowest_online(mask: u64) -> usize {
    let online = mask & online_mask();
    debug_assert!(online != 0);
    online.trailing_zeros() as usize
}

/// Reserve before publishing a new thread. Every CPU can receive it later,
/// including as a sleeper; neither migration nor a timer tick may allocate.
fn reserve_thread() -> Result<Reservation<'static>, SpawnError> {
    reserve_queues(&CPUS)
}

fn reserve_queues(queues: &[SpinLock<Cpu>]) -> Result<Reservation<'_>, SpawnError> {
    let mut reservation = Reservation { queues, cpus: 0 };
    for q in queues {
        q.lock().try_home()?;
        reservation.cpus += 1;
    }
    Ok(reservation)
}

/// Counts owned by an unpublished thread. A failed reservation or stack
/// creation rolls them back; enqueue transfers them to finish_switch.
struct Reservation<'a> {
    queues: &'a [SpinLock<Cpu>],
    cpus: usize,
}

impl Reservation<'_> {
    fn commit(self) {
        core::mem::forget(self);
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        for q in &self.queues[..self.cpus] {
            q.lock().reserved -= 1;
        }
    }
}

/// Lock the queue owning a thread, retrying a concurrent relocation.
fn lock_queue_of(thread: &Thread) -> (usize, SpinLockGuard<'static, Cpu>) {
    loop {
        let cpu = thread.cpu();
        let queue = CPUS[cpu].lock();
        if thread.cpu() == cpu {
            return (cpu, queue);
        }
    }
}

const BALANCE_TICKS: u64 = 100;

/// Hold two queues in CPU-index order. No caller holds a queue already.
fn with_queues<R>(a: usize, b: usize, f: impl FnOnce(&mut Cpu, &mut Cpu) -> R) -> R {
    assert_ne!(a, b);
    let (low, high) = (a.min(b), a.max(b));
    let mut first = CPUS[low].lock();
    let mut second = CPUS[high].lock();
    if a == low { f(&mut first, &mut second) } else { f(&mut second, &mut first) }
}

/// Move a saved Ready thread while both queue locks are held.
fn move_ready(from: usize, source: &mut Cpu, to: usize, dest: &mut Cpu, at: usize) {
    let thread = source.ready.remove(at).expect("queued thread");
    assert_eq!(thread.state(), State::Ready);
    assert!(!thread.on_cpu.load(Ordering::Acquire));
    source.forget_deadline(&thread);
    source.queue_changed(from, thread.id, RunQueueOp::Dequeue);
    thread.cpu.store(to, Ordering::Release);
    dest.push_ready(to, thread);
}

/// One balancing pass: the busiest online queue donates its last eligible
/// saved thread, only when it has at least two more runnable threads.
fn balance(cpu: usize) -> bool {
    #[cfg(feature = "ktest")]
    if !AUTO_BALANCE.load(Ordering::Relaxed) {
        return false;
    }
    balance_pass(cpu)
}

fn balance_pass(cpu: usize) -> bool {
    let mut tried = 1u64 << cpu;
    loop {
        let Some(from) = (0..MAX_CPUS)
            .filter(|&other| tried & (1 << other) == 0 && running_on(other))
            .max_by_key(|&other| READY[other].load(Ordering::Acquire))
        else {
            return false;
        };
        tried |= 1 << from;
        // No queued donor, or even counting its running thread cannot meet
        // the threshold. Recheck exact runnable counts under both locks.
        let ready = READY[from].load(Ordering::Acquire);
        if ready == 0 || ready + 1 < READY[cpu].load(Ordering::Acquire) + 2 {
            continue;
        }
        let moved = with_queues(from, cpu, |source, dest| {
            if source.runnable() < dest.runnable() + 2 {
                return false;
            }
            let Some(at) = source.ready.iter().rposition(|t| t.may_move_to(cpu)) else {
                return false;
            };
            move_ready(from, source, cpu, dest, at);
            true
        });
        if moved {
            let _ = smp::wake(cpu);
            return true;
        }
        // Pinned or still-running queued threads must not hide another donor.
    }
}

/// Affinity redirects only a fully saved Ready thread. Wakes racing the
/// outgoing switch stay on its old queue until finish_switch releases it.
fn relocate_disallowed(thread: &Thread) {
    loop {
        let from = thread.cpu();
        let mask = thread.affinity();
        if mask & (1 << from) != 0 {
            return;
        }
        let to = lowest_online(mask);
        let retry = with_queues(from, to, |source, dest| {
            if thread.cpu() != from || thread.affinity() != mask {
                return true;
            }
            if thread.state() == State::Ready
                && !thread.on_cpu.load(Ordering::Acquire)
                && let Some(at) = source.ready.iter().position(|t| t.id == thread.id)
            {
                move_ready(from, source, to, dest, at);
            }
            false
        });
        if !retry {
            let _ = smp::wake(to);
            return;
        }
    }
}

/// Route sleepers just woken by a tick after releasing their queue lock.
fn route_ready(cpu: usize) {
    loop {
        let thread =
            CPUS[cpu].lock().ready.iter().find(|t| !t.allows(cpu) && !t.on_cpu.load(Ordering::Acquire)).cloned();
        let Some(thread) = thread else { return };
        relocate_disallowed(&thread);
    }
}

/// Start a kernel thread on this CPU, eligible to run on every online CPU.
#[allow(dead_code)] // public scheduler API; exercised by the ktests
pub fn spawn_migratable(
    name: impl Into<ThreadName>,
    f: impl FnOnce() + Send + 'static,
) -> Result<JoinHandle, SpawnError> {
    let name = TaskName::from_display(name.into());
    spawn_configured(arch::cpu_index(), name, STACK_SIZE, Box::new(f), None, Placement::migratable())
}

fn spawn_with(cpu: usize, name: ThreadName, stack_size: u64, f: Entry) -> Result<JoinHandle, SpawnError> {
    let name = TaskName::from_display(name);
    spawn_configured(cpu, name, stack_size, f, None, Placement::pinned(cpu))
}

fn spawn_configured(
    cpu: usize,
    name: TaskName,
    stack_size: u64,
    f: Entry,
    user: Option<UserThreadState>,
    placement: Placement,
) -> Result<JoinHandle, SpawnError> {
    if !running_on(cpu) {
        return Err(SpawnError::NotRunning);
    }
    let reservation = reserve_thread()?;
    let linux = user.as_ref().and_then(|u| u.linux.clone());
    let thread = Thread::spawned(name, cpu, stack_size, f, user, placement)?;
    if let Some(linux) = linux {
        linux.bind_kernel_thread(&thread);
    }
    reservation.commit();
    CPUS[cpu].lock().push_ready(cpu, thread.clone());
    let _ = smp::wake(cpu); // the next timer tick also finds it if the IPI fails
    Ok(JoinHandle { thread })
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
    // A user thread whose entry didn't already detach (one that registered
    // no Linux process: a process's takedown detaches): switch CR3 back to
    // `kernel_root()` and tear down user mappings with interrupts ON (so
    // `AddressSpace::drop` in `finish_switch` only frees the bare L4).
    if let Some(space) = detach_user_space()
        && Arc::strong_count(&space) == 1
    {
        space.teardown();
    }
    exit()
}

/// Give the CPU to the next ready thread on it, if any; the running thread
/// stays ready and gets its turn again. Returns at once before the
/// scheduler runs on this CPU. Outside every critical section.
pub fn yield_now() {
    let were_on = arch::irq::disable();
    let cpu = arch::cpu_index();
    if !running_on(cpu) {
        arch::irq::restore(were_on);
        return;
    }
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
    if !can_block() {
        return false;
    }
    let deadline = time::now_ns().saturating_add(ns);
    let were_on = arch::irq::disable();
    let cpu = arch::cpu_index();
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
    let _ = arch::irq::disable();
    let cpu = arch::cpu_index();
    assert!(running_on(cpu), "exit where no scheduler runs");
    {
        let q = CPUS[cpu].lock();
        let me = q.running();
        assert!(me.stack.lock().is_some() && !q.on_idle(), "only a spawned thread can exit, not {me:?}");
        me.change(State::Running, State::Dead);
    }
    schedule(cpu);
    unreachable!("a dead thread was switched back to")
}

/// Save `prev`'s user CPU state, switch `CR3` (`next`'s `AddressSpace` or
/// `kernel_root()`), and restore `next`'s user CPU state (or clear it for a
/// kernel/idle thread). Runs **before** `arch::context::switch` with
/// interrupts off so freshly spawned user threads entering `thread_entry`
/// already have their `CR3`, FPU, TLS, and `TSS.rsp0`/`PerCpu` installed.
///
/// # Safety
/// Must be called on `cpu` with interrupts disabled, no locks held, both
/// contexts owned by `cpu` through on_cpu, `prev != next`, and both pointers
/// alive until `finish_switch` runs.
unsafe fn switch_user_context(cpu: usize, prev: *const Thread, next: *const Thread) {
    // SAFETY: upheld by caller; interrupts are off on `cpu` and both threads
    // are kept alive by `CPUS[cpu].switched_from` and `CPUS[cpu].current`.
    let prev_user = unsafe { (*prev).user.as_ref() };
    // SAFETY: same as above.
    let next_user = unsafe { (*next).user.as_ref() };

    // 1. Save outgoing user thread's FPU and PerCpu ring-3 fields.
    if let Some(pu) = prev_user {
        // SAFETY: interrupts are off on `prev`'s owning CPU.
        unsafe { pu.save_fpu() };
        pu.kernel_rsp.store(arch::percpu::kernel_rsp(), Ordering::Relaxed);
        pu.user_exit_rsp.store(arch::percpu::user_exit_rsp(), Ordering::Relaxed);
        pu.pending_debug_trap.store(arch::percpu::take_pending_debug_trap());
    }

    // 2. Switch CR3 to `next`'s AddressSpace root or back to `kernel_root()`,
    //    updating `AddressSpace::active` and `PerCpu::loaded_space`.
    // SAFETY: interrupts are off on `cpu`; both `prev` (whose space
    // `PerCpu::loaded_space` records) and `next` remain alive in
    // `CPUS[cpu].switched_from` and `CPUS[cpu].current` until `finish_switch`.
    unsafe { AddressSpace::switch_loaded(next_user.map(|nu| &*nu.address_space)) };

    // 3. Restore incoming user thread's TSS.rsp0, PerCpu, IA32_FS_BASE, and FPU,
    //    or clear all user CPU state for a kernel/idle thread.
    if let Some(nu) = next_user {
        let krsp = nu.kernel_rsp.load(Ordering::Relaxed);
        // SAFETY: interrupts are off on `cpu`; `krsp` is `next`'s live ring-3
        // kernel stack top (or 0 before `enter_ring3`), `fs_base < USER_MAX`.
        unsafe {
            arch::gdt::set_rsp0(cpu, krsp);
            arch::fpu::write_fs_base(nu.fs_base.load(Ordering::Relaxed));
            nu.restore_fpu();
        }
        arch::percpu::set_kernel_rsp(krsp);
        arch::percpu::set_user_exit_rsp(nu.user_exit_rsp.load(Ordering::Relaxed));
        arch::percpu::set_pending_debug_trap(nu.pending_debug_trap.take());
    } else {
        // SAFETY: interrupts are off on `cpu`; clearing user state for kernel/idle.
        unsafe {
            arch::gdt::set_rsp0(cpu, 0);
            arch::fpu::write_fs_base(0);
        }
        arch::percpu::set_kernel_rsp(0);
        arch::percpu::set_user_exit_rsp(0);
        arch::percpu::set_pending_debug_trap(None);
    }
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
    let (save, load, prev_ptr, next_ptr) = {
        let mut q = CPUS[cpu].lock();
        assert!(q.switched_from.is_none(), "the last switch on cpu {cpu} wasn't finished");
        let prev = q.current.take().expect("a running scheduler has a current thread");
        assert_eq!(prev.cpu(), cpu, "thread {prev:?} isn't cpu {cpu}'s");
        let idle = q.is_idle_thread(&prev);
        // Only this CPU moves its running thread out of Running; one still
        // running (not the idle thread, never queued) goes back on the queue.
        let requeue = !idle && prev.state() == State::Running;
        if idle || requeue {
            prev.change(State::Running, State::Ready);
        }
        // Queued and taken straight off again, it just keeps running,
        // recording no run-queue change (a thread polling by yielding would
        // fill the X-ray ring otherwise).
        let keep = requeue && prev.allows(cpu) && q.ready.is_empty();
        if requeue && !keep {
            q.push_ready(cpu, prev.clone());
        }
        let next = if keep {
            prev.clone()
        } else {
            q.pop_ready(cpu).unwrap_or_else(|| q.idle.clone().expect("a running scheduler has an idle thread"))
        };
        q.slice_left = lumen_time::slice_ticks(next.nice());
        assert_eq!(next.cpu(), cpu, "thread {next:?} isn't cpu {cpu}'s");
        next.change(State::Ready, State::Running);
        if Arc::ptr_eq(&next, &prev) {
            q.current = Some(next);
            return;
        }
        next.claim();
        // SAFETY: this CPU owns both threads through on_cpu; interrupts are
        // off, and `next` isn't running.
        let (save, load) = unsafe { (prev.rsp.slot(), next.rsp.get()) };
        // Checked before `CURRENT` names `next`, so a crash here names `prev`,
        // whose stack it runs on.
        assert!(load != 0, "switching to thread {next:?}, which has no saved context");
        trace::record_switch(Switch { from: prev.id, to: next.id, state: prev.state().into() });
        SWITCHES[cpu].fetch_add(1, Ordering::Release);
        let prev_ptr = Arc::as_ptr(&prev);
        let next_ptr = Arc::as_ptr(&next);
        CURRENT[cpu].store(next_ptr.cast_mut(), Ordering::Release);
        q.current = Some(next);
        q.switched_from = Some(prev);
        (save, load, prev_ptr, next_ptr)
    };
    // Switch user FPU, CR3, TSS.rsp0, PerCpu, and IA32_FS_BASE before switching
    // kernel stacks (with interrupts off, no SpinLock held).
    // SAFETY: interrupts are off, `prev_ptr` and `next_ptr` stay alive in
    // `CPUS[cpu].switched_from` and `CPUS[cpu].current` until `finish_switch`.
    unsafe { switch_user_context(cpu, prev_ptr, next_ptr) };
    #[cfg(feature = "ktest")]
    // SAFETY: switched_from keeps prev_ptr alive until the actual switch;
    // its on_cpu ownership still excludes every other CPU's execution.
    SWITCH_DELAY.stall(unsafe { &*prev_ptr }, StallAt::MidSwitch);
    // SAFETY: interrupts are off (asserted above); `save` is the running
    // thread's `rsp` slot, which `switched_from` keeps alive until the next
    // thread has finished the switch; `load` was saved by `switch` (or made
    // by `context::prepare`) on `next`'s stack, which stays mapped until it
    // dies and is switched away from, and which no CPU runs on (`next` is
    // this CPU's, and wasn't running).
    unsafe { arch::context::switch(save, load) };
    finish_switch(arch::cpu_index());
}

/// Finish the complete outgoing save on the CPU now running this stack.
/// A live thread releases on_cpu under its queue locks and relocates if
/// Ready and disallowed here. A dead thread is published in
/// [`FINISHING`]: free its stack (on the CPU it died on: see the
/// module docs on the TLB), mark it reaped, wake its joiners, then free
/// their list and drop the reference ([`Released`]), and clear `FINISHING`
/// last. Interrupts off.
fn finish_switch(cpu: usize) {
    let prev = CPUS[cpu].lock().take_switched_from();
    let Some(prev) = prev else { return };
    // Decide while ownership still excludes execution elsewhere: after
    // release a live thread might run and die before this CPU reads it.
    if prev.state() != State::Dead {
        release_previous(prev);
        return;
    }
    prev.release();
    for q in &CPUS {
        q.lock().reserved -= 1;
    }
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

/// Publish the completed save and release this CPU's reference before a
/// new CPU can run the thread. Otherwise it could die and be joined while
/// this CPU still holds a reference whose later drop frees its metadata.
fn release_previous(mut prev: Arc<Thread>) {
    loop {
        let from = prev.cpu(); // stable while this CPU still owns on_cpu
        let mask = prev.affinity();
        if mask & (1 << from) != 0 {
            let q = CPUS[from].lock();
            if !prev.allows(from) {
                continue;
            }
            prev.release();
            drop(prev);
            drop(q);
            return;
        }
        let to = lowest_online(mask);
        match with_queues(from, to, |source, dest| {
            if prev.affinity() != mask {
                return Err(prev);
            }
            prev.release();
            if prev.state() == State::Ready {
                let at = source.ready.iter().position(|t| t.id == prev.id).expect("Ready outgoing thread queued");
                move_ready(from, source, to, dest, at);
            }
            // Still under the queues: nobody can claim the released thread
            // until this CPU has let go of its last reference to it.
            drop(prev);
            Ok(())
        }) {
            Ok(()) => {
                let _ = smp::wake(to);
                return;
            }
            Err(thread) => prev = thread,
        }
    }
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
/// periodically balance runnable work, wake expired sleepers and route_ready
/// to honor their affinity. Switch if another thread is ready and the slice
/// expired (or idle runs), or force a switch if the running thread's mask
/// excludes this CPU.
pub fn tick(from_user: bool) {
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
    if TICKED[cpu].load(Ordering::Relaxed).wrapping_add(1).is_multiple_of(BALANCE_TICKS) {
        balance(cpu);
    }
    {
        let mut q = CPUS[cpu].lock();
        // Counted before waking anyone: the thread this tick interrupted.
        TICKED[cpu].fetch_add(1, Ordering::Relaxed);
        q.running().charge_tick(from_user);
        if q.on_idle() {
            IDLE_TICKS[cpu].fetch_add(1, Ordering::Release);
        }
        q.wake_sleepers(cpu, now);
        q.slice_left = q.slice_left.saturating_sub(1);
    }
    route_ready(cpu);
    let switch = {
        let q = CPUS[cpu].lock();
        (!q.ready.is_empty() && (q.slice_left == 0 || q.on_idle())) || !q.running().allows(cpu)
    };
    if switch {
        schedule(cpu);
    }
}

/// How a [`WaitQueue`] wait may end before its condition holds or its
/// deadline passes (stored in `Thread::wait_kind` before the thread is set
/// `Blocked`, for [`kick`]).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaitKind {
    /// Only a wake or the deadline ends it (Linux's `TASK_UNINTERRUPTIBLE`):
    /// [`WaitQueue::wait_until`], [`WaitQueue::wait_until_or_deadline`].
    Uninterruptible = 0,
    /// A pending signal to look at (`sigpending`) ends it too
    /// ([`WaitQueue::wait_interruptible`]).
    Interruptible = 1,
    /// Only something that ends the thread (`fatal_pending`) ends it too
    /// ([`WaitQueue::wait_killable`], Linux's `TASK_KILLABLE`).
    Killable = 2,
}

impl WaitKind {
    fn from_u8(value: u8) -> Self {
        match value {
            0 => Self::Uninterruptible,
            1 => Self::Interruptible,
            2 => Self::Killable,
            _ => unreachable!("only a WaitKind is stored"),
        }
    }
}

/// How a [`WaitQueue::wait_interruptible`] or [`WaitQueue::wait_killable`]
/// ended.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wake {
    /// Its condition held (checked first: a wake that won the race with a
    /// signal is never lost).
    Woken,
    /// Its deadline passed (never before it), the condition still false.
    TimedOut,
    /// A signal to look at (interruptible) or the thread's end (killable)
    /// reached it, the condition still false and the deadline not passed.
    Interrupted,
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
    /// woken while `ready()` is still false waits again, at the back. No
    /// signal ends it (Linux's `TASK_UNINTERRUPTIBLE`: kernel-internal waits
    /// that must finish). Panics
    /// where the thread can't block (see [`can_block`]), except where it
    /// can't because a crash report or a reboot on another CPU turned
    /// interrupts off for good: then it halts this CPU for it (see
    /// `crash::halt_if_stopping_elsewhere` for which cases it tells
    /// apart).
    pub fn wait_until(&self, ready: impl FnMut() -> bool) {
        let woke = self.block(WaitKind::Uninterruptible, None, ready);
        debug_assert!(woke == Wake::Woken, "a wait without a deadline returned unready");
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
        self.block(WaitKind::Uninterruptible, Some(deadline), ready) == Wake::Woken
    }

    /// Until `ready()` holds ([`Wake::Woken`]), `deadline` (`time::now_ns`)
    /// passes ([`Wake::TimedOut`]), or the calling thread has a signal to
    /// look at (`sigpending`: a signal to deliver, the group's exit or
    /// stop) ([`Wake::Interrupted`]), checked in that order, each time,
    /// with the queue's lock held. `ready` reads atomics only (a condition
    /// kept under a `SleepMutex` is read through a sequence number its
    /// changers bump), and must not block or take a lock held around a
    /// wake. A sender sets `sigpending` and then calls [`kick`]: the thread
    /// reads it again after it is queued and `Blocked`, so of the sender's
    /// store and the thread's block one sees the other (no signal is lost).
    /// Every way out takes the thread's entries off the queue and its CPU's
    /// sleepers. Panics where the thread can't block, as `wait_until` does.
    pub fn wait_interruptible(&self, deadline: Option<u64>, ready: impl FnMut() -> bool) -> Wake {
        self.block(WaitKind::Interruptible, deadline, ready)
    }

    /// [`wait_interruptible`](Self::wait_interruptible), but only what ends
    /// the thread (`fatal_pending`: `SIGKILL`, the group's exit,
    /// `de_thread`) interrupts it, never a signal it would handle or ignore
    /// (Linux's `TASK_KILLABLE`).
    pub fn wait_killable(&self, deadline: Option<u64>, ready: impl FnMut() -> bool) -> Wake {
        self.block(WaitKind::Killable, deadline, ready)
    }

    /// Every wait: block, as `kind` says, until `ready()`, `deadline` or
    /// what interrupts a wait of that kind.
    fn block(&self, kind: WaitKind, deadline: Option<u64>, mut ready: impl FnMut() -> bool) -> Wake {
        if !can_block() {
            // Or halt for another CPU's crash report (#106).
            crate::crash::halt_if_stopping_elsewhere();
            panic!("blocking where a thread can't (no scheduler, interrupts off, a lock held, or the idle thread)");
        }
        let me = current().expect("a blocking thread");
        loop {
            let were_on = arch::irq::disable();
            let cpu = arch::cpu_index();
            let interrupted = {
                let mut waiters = self.waiters.lock();
                let answer = if ready() {
                    Some(Wake::Woken)
                } else if deadline.is_some_and(|d| time::now_ns() >= d) {
                    Some(Wake::TimedOut)
                } else if me.pending_for(kind) {
                    Some(Wake::Interrupted)
                } else {
                    None
                };
                if let Some(answer) = answer {
                    drop(waiters);
                    arch::irq::restore(were_on);
                    return answer;
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
                // only here, and the deadline's `sleepers.push` uses
                // room reserved beforehand (asserted).

                // The deadline's entry before `Blocked`: only this CPU's
                // tick reads it, and that can't run before the switch.
                if let Some(at) = deadline {
                    let mut q = CPUS[cpu].lock();
                    assert!(q.sleepers.len() < q.sleepers.capacity(), "thread {me:?} waiting without room reserved");
                    q.sleepers.push(Sleeper { at, thread: me.clone(), on_queue: true });
                }
                // A waker (or a kick) on another CPU may make it ready (and
                // queue it) before this CPU switches away: `schedule` then
                // neither queues it again nor loses it. The timer can't
                // (interrupts are off here until the switch).
                me.wait_kind.store(kind as u8, Ordering::SeqCst);
                #[cfg(feature = "ktest")]
                BLOCK_DELAY.stall(&me, StallAt::BeforeBlocked);
                me.change(State::Running, State::Blocked);
                #[cfg(feature = "ktest")]
                BLOCK_DELAY.stall(&me, StallAt::AfterBlocked);
                // Only now, `Blocked`, does it look at what is pending
                // (Linux's `set_current_state` before `signal_pending`): a
                // sender stores the flag, fences, and then `kick` reads the
                // state. With both fences one sees the other's store: the
                // sender a `Blocked` thread to queue, or this the flag.
                // Still `Blocked`, it moves itself back to running and
                // keeps the CPU; queued by a kick (or a wake) already, it
                // switches through `schedule`, which takes it off the run
                // queue again.
                core::sync::atomic::fence(Ordering::SeqCst);
                let pending = me.pending_for(kind);
                #[cfg(feature = "ktest")]
                if !pending {
                    BLOCK_DELAY.stall(&me, StallAt::AfterRead);
                }
                pending && me.try_change(State::Blocked, State::Running)
            };
            if !interrupted {
                schedule(cpu);
            }
            // Normally its waker took it off. But it may have been woken
            // some other way (its deadline, a kick, or a waker holding an
            // entry from an earlier wait that a deadline ended), or not
            // switched away at all (interrupted before it did): then its
            // entries must come off, so no waker or tick finds it later,
            // blocked on something else. `cpu` deliberately names the old
            // queue: migration already removed its deadline before moving
            // us. Without migration its timer still cannot run (IRQs off).
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
/// holding an entry from an earlier wait); then it is not queued again.
/// Removes its old deadline before enqueueing on its stable queue CPU,
/// then relocates if affinity excludes that CPU and the context is saved.
/// An on_cpu thread stays put until finish_switch completes the save.
fn wake(thread: Arc<Thread>) -> bool {
    let (cpu, mut q) = lock_queue_of(&thread);
    if !thread.try_change(State::Blocked, State::Ready) {
        return false;
    }
    q.forget_deadline(&thread);
    q.push_ready(cpu, thread.clone());
    drop(q);
    relocate_disallowed(&thread);
    let _ = smp::wake(thread.cpu());
    true
}

/// Move `thread` out of a wait a signal ends, as a wake does: an
/// interruptible one, or a killable one if its `fatal_pending` is set. A
/// sender calls it after its stores of `sigpending` (and, for what ends the
/// thread, `fatal_pending`): see [`WaitQueue::wait_interruptible`] for why
/// none is lost. A thread in any other wait (or running) is left alone: it
/// looks at `sigpending` at its next return to ring 3 or interruptible
/// wait. A stale kind may spuriously wake a later wait, which harmlessly
/// rechecks its condition and pending flags. Never blocks; callable from
/// any CPU while holding a signal or wait-queue lock, but not a run-queue
/// lock or a lock nested inside it (thread stacks, heap, page tables and
/// frame allocator). It locks the target's queue and may then take both
/// source and destination queues in ascending CPU order to honor affinity.
/// It never takes a wait-queue lock; `smp::wake` takes no lock.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // the ktests; C5's senders next
pub fn kick(thread: &Thread) {
    // Pairs with the fence in `WaitQueue::block` between `Blocked` and its
    // read of the flags.
    core::sync::atomic::fence(Ordering::SeqCst);
    if thread.state.load(Ordering::SeqCst) != State::Blocked as u8 {
        return;
    }
    let kind = WaitKind::from_u8(thread.wait_kind.load(Ordering::SeqCst));
    if !thread.pending_for(kind) {
        return;
    }
    // Upgraded only to queue it: a thread being dropped isn't blocked.
    if let Some(me) = thread.me.upgrade() {
        let _ = wake(me);
    }
}

/// Make `current` CPU `cpu`'s running thread and `idle` its idle thread.
fn install(cpu: usize, current: Arc<Thread>, idle: Arc<Thread>) {
    let mut q = CPUS[cpu].lock();
    CURRENT[cpu].store(Arc::as_ptr(&current).cast_mut(), Ordering::Release);
    IDLE_ID[cpu].store(idle.id, Ordering::Relaxed);
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
    let idle = Thread::spawned(
        "idle".into(),
        0,
        STACK_SIZE,
        Box::new(|| idle_loop(0, || false, || {})),
        None,
        Placement::pinned(0),
    )
    .unwrap_or_else(|e| panic!("the boot CPU's idle thread: {e}"));
    install(0, Thread::adopted("main".into()), idle);
    reserve_thread().expect("reserve main thread queues").commit(); // main can sleep and block too
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
    let me = Thread::adopted("idle".into());
    install(cpu, me.clone(), me);
    idle_loop(cpu, has_job, run_job);
    unreachable!("an idle loop returned")
}

/// A CPU's idle thread: halt until there is work (a ready thread, a job, or
/// for an AP whose timer isn't started yet, the timer's count), then do it.
/// It attempts to steal runnable work before halting. It never blocks:
/// a job that sleeps busy-waits, and one that waits on a
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
        if running_on(cpu) {
            balance(cpu);
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
pub(crate) fn running_cpus() -> Vec<usize> {
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

/// `spawn_migratable` for tests, retaining the spawn failure's diagnosis.
#[cfg(feature = "ktest")]
pub(crate) fn spawn_test_migratable(
    name: &'static str,
    f: impl FnOnce() + Send + 'static,
) -> Result<JoinHandle, &'static str> {
    spawn_migratable(name, f).map_err(|e| {
        crate::ktest::diag(format_args!("spawning migratable {name}: {e}"));
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
    if !yield_until(|| FINISHING[t.cpu()].load(Ordering::Acquire) != t.id) {
        crate::ktest::diag(format_args!("cpu {} is still finishing {t:?}", t.cpu()));
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
    (start..ring.written())
        .filter_map(|seq| ring.read(seq))
        .filter(|e| e.kind == trace::Kind::Switch as u16)
        .map(|e| e.args)
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
        // its current queue and a wakeup IPI). Woken without its condition,
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
        // The timer never allocates: with 40 threads reserved on every CPU (blocked,
        // so on neither queue) its ready and sleeper queues already have room
        // for all of them (pushing asserts it too); once they are reaped the
        // count is back.
        use crate::{kassert, kassert_eq};
        const N: usize = 40;
        let cpu = last_cpu()?;
        let room = || {
            let q = CPUS[cpu].lock();
            (q.reserved, q.ready.capacity() >= q.reserved && q.sleepers.capacity() >= q.reserved)
        };
        let base = room().0;
        let s = Arc::new((WaitQueue::new(), AtomicBool::new(false)));
        let mut handles = Vec::new();
        for i in 0..N {
            let t = s.clone();
            handles.push(spawn_test(cpu, "parked", move || t.0.wait_until(|| t.1.load(Ordering::Acquire)))?);
            let (reserved, ok) = room();
            kassert_eq!(reserved, base + i + 1);
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
            kassert_eq!(State::from(ThreadState::from(s)), s);
            kassert_eq!(ThreadState::from(s) as u8, s as u8);
        }
        kassert_eq!(State::from_raw(State::ALL.len() as u8), None);
        Ok(())
    }
);

crate::ktest!(
    fn spawning_where_no_scheduler_runs_is_refused() {
        // A CPU index past the limit, and every index no online CPU has.
        use crate::kassert;
        kassert!(matches!(spawn_on(MAX_CPUS, "nowhere", || {}), Err(SpawnError::NotRunning)));
        kassert!(matches!(spawn_on(usize::MAX, "nowhere", || {}), Err(SpawnError::NotRunning)));
        for cpu in (0..MAX_CPUS).filter(|&c| smp::online_lapic_id(c).is_none()) {
            kassert!(matches!(spawn_on(cpu, "offline", || {}), Err(SpawnError::NotRunning)));
        }
        Ok(())
    }
);

/// Ktests: whether `t` has a deadline entry on its CPU, or is on `queue`.
#[cfg(feature = "ktest")]
fn entries_of(t: &Arc<Thread>, queue: &WaitQueue) -> (bool, bool) {
    let deadline = CPUS[t.cpu()].lock().sleepers.iter().any(|s| s.on_queue && Arc::ptr_eq(&s.thread, t));
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

/// A [`Wake`] as a number a test thread hands back (0: no answer yet).
#[cfg(feature = "ktest")]
fn wake_code(w: Wake) -> u8 {
    match w {
        Wake::Woken => 1,
        Wake::TimedOut => 2,
        Wake::Interrupted => 3,
    }
}

/// A waiter thread's side of the blocking-wait ktests: a queue, a condition
/// its waits end on (`ready`), the go-ahead for its next wait (`go`, a
/// round number), and each wait's answer ([`wake_code`]) and round.
#[cfg(feature = "ktest")]
#[derive(Default)]
struct Waiting {
    queue: WaitQueue,
    ready: AtomicBool,
    go: AtomicU64,
    answer: AtomicU8,
    answered: AtomicU64,
}

#[cfg(feature = "ktest")]
impl Waiting {
    /// The waiter's wait for round `round`'s go-ahead (yielding), or false
    /// at the test deadline.
    fn wait_go(&self, round: u64) -> bool {
        yield_until(|| self.go.load(Ordering::SeqCst) >= round)
    }

    /// Record round `round`'s answer.
    fn answer(&self, round: u64, w: Wake) {
        self.answer.store(wake_code(w), Ordering::SeqCst);
        self.answered.store(round, Ordering::SeqCst);
    }

    /// The tester's wait for round `round`'s answer: it, or `None` at the
    /// test deadline (then the waiter is let go, its condition made true
    /// and its queue woken, so the thread ends and can be joined).
    fn wait_answer(&self, round: u64) -> Option<u8> {
        if yield_until(|| self.answered.load(Ordering::SeqCst) >= round) {
            return Some(self.answer.load(Ordering::SeqCst));
        }
        self.ready.store(true, Ordering::SeqCst);
        self.queue.wake_all();
        None
    }
}

crate::ktest!(
    fn wait_interruptible_answers_woken_timed_out_and_interrupted() {
        // Each answer, in its order (ready, then the deadline, then what is
        // pending), on this thread: already ready (with a signal pending
        // too) is `Woken`; a signal pending and not ready is `Interrupted`
        // without blocking; a passed deadline wins over it; a killable or
        // plain wait ignores it. Then blocked: the deadline (`TimedOut`, no
        // earlier: a floor), a waker on another CPU (`Woken`) and a kick
        // from another CPU (`Interrupted`, well before its deadline), each
        // leaving no entry behind; and a kick to a plain wait leaves it
        // blocked until its waker comes.
        use crate::{kassert, kassert_eq};
        let me = current().ok_or("the ktests run on a thread")?;
        let q = WaitQueue::new();
        let far = |now: u64| Some(now + TEST_DEADLINE_NS);
        kassert_eq!(q.wait_interruptible(None, || true), Wake::Woken);
        me.set_sigpending();
        let unblocked = (|| {
            kassert_eq!(q.wait_interruptible(None, || true), Wake::Woken);
            kassert_eq!(q.wait_interruptible(far(time::now_ns()), || false), Wake::Interrupted);
            kassert_eq!(q.wait_interruptible(Some(0), || false), Wake::TimedOut);
            let start = time::now_ns();
            kassert_eq!(q.wait_killable(Some(start + 2_000_000), || false), Wake::TimedOut);
            kassert!(time::now_ns() >= start + 2_000_000);
            let start = time::now_ns();
            kassert!(!q.wait_until_or_deadline(start + 2_000_000, || false));
            kassert!(time::now_ns() >= start + 2_000_000);
            kassert_eq!(entries_of(&me, &q), (false, false));
            Ok(())
        })();
        clear_current_sigpending();
        unblocked?;

        let start = time::now_ns();
        kassert_eq!(q.wait_interruptible(Some(start + 5_000_000), || false), Wake::TimedOut);
        kassert!(time::now_ns() >= start + 5_000_000);
        kassert_eq!(entries_of(&me, &q), (false, false));

        // A signal observed after Blocked but before scheduling must
        // remove both its queue entry and its far-future deadline entry.
        kassert_eq!(
            kick_into_a_stalled_block(false, StallAt::BeforeBlocked, true)?,
            Some(wake_code(Wake::Interrupted))
        );

        // Woken by a waker on another CPU.
        let s = Arc::new(Waiting::default());
        let (w, target) = (s.clone(), me.clone());
        let waker = spawn_test(last_cpu()?, "waker", move || {
            if yield_until(|| target.state() == State::Blocked && entries_of(&target, &w.queue).1) {
                w.ready.store(true, Ordering::SeqCst);
                w.queue.wake_all();
            }
        })?;
        let woke = s.queue.wait_interruptible(far(time::now_ns()), || s.ready.load(Ordering::SeqCst));
        join_bounded(waker)?;
        kassert_eq!(woke, Wake::Woken);
        kassert_eq!(entries_of(&me, &s.queue), (false, false));

        // Interrupted by a kick from another CPU, once it is blocked.
        let s = Arc::new(Waiting::default());
        let (k, target) = (s.clone(), me.clone());
        let kicker = spawn_test(last_cpu()?, "kicker", move || {
            if yield_until(|| target.state() == State::Blocked && entries_of(&target, &k.queue).1) {
                target.set_sigpending();
                kick(&target);
            }
        })?;
        let kicked = s.queue.wait_interruptible(far(time::now_ns()), || false);
        clear_current_sigpending();
        join_bounded(kicker)?;
        kassert_eq!(kicked, Wake::Interrupted);
        kassert_eq!(entries_of(&me, &s.queue), (false, false));

        // A kick to a plain wait: still blocked a while after it, until its
        // waker comes.
        let s = Arc::new(Waiting::default());
        let (k, target) = (s.clone(), me.clone());
        let still_blocked = AtomicBool::new(false);
        let cpu = last_cpu()?;
        scope(|scope| -> Result<(), &'static str> {
            scope
                .spawn_on(cpu, "kicker", || {
                    if yield_until(|| target.state() == State::Blocked && entries_of(&target, &k.queue).1) {
                        target.set_sigpending();
                        kick(&target);
                        let _ = sleep(5_000_000);
                        still_blocked.store(target.state() == State::Blocked, Ordering::SeqCst);
                    }
                    k.ready.store(true, Ordering::SeqCst);
                    k.queue.wake_all();
                })
                .map_err(|_| "spawn failed")?;
            s.queue.wait_until(|| s.ready.load(Ordering::SeqCst));
            clear_current_sigpending();
            Ok(())
        })
        .map_err(|_| "scope failed")??;
        kassert!(still_blocked.load(Ordering::SeqCst));
        Ok(())
    }
);

/// Exercise a configured stall, retrying only rounds whose 1 s stall
/// bound expired on a busy host. The far-deadline round covers cleanup
/// when the waiter observes the signal itself, without scheduling.
#[cfg(feature = "ktest")]
fn kick_into_a_stalled_block(killable: bool, at: StallAt, deadline: bool) -> Result<Option<u8>, &'static str> {
    for _ in 0..5 {
        let s = Arc::new(Waiting::default());
        let w = s.clone();
        let clean_entries = Arc::new(AtomicBool::new(false));
        let clean = clean_entries.clone();
        let waiter = spawn_test(last_cpu()?, "stalled", move || {
            if w.wait_go(1) {
                let ready = || w.ready.load(Ordering::SeqCst);
                let until = deadline.then(|| time::now_ns() + TEST_DEADLINE_NS);
                let woke = if killable {
                    w.queue.wait_killable(until, ready)
                } else {
                    w.queue.wait_interruptible(until, ready)
                };
                clean.store(entries_of(&current().expect("a thread"), &w.queue) == (false, false), Ordering::SeqCst);
                w.answer(1, woke);
            }
        })?;
        let t = waiter.thread.clone();
        BLOCK_DELAY.arm(t.id, at);
        s.go.store(1, Ordering::SeqCst);
        let stalled = yield_until(|| BLOCK_DELAY.stalled());
        // No pending flag means no wake, even for an interruptible kind.
        kick(&t);
        let before_signal = t.state();
        if killable {
            t.set_fatal_pending();
        } else {
            t.set_sigpending();
        }
        kick(&t);
        // Capture before release: after the false read only kick can
        // change Blocked to Ready. BeforeBlocked deliberately stays Running.
        let after_kick = t.state();
        BLOCK_DELAY.release();
        let answer = s.wait_answer(1);
        join_bounded(waiter)?;
        if BLOCK_DELAY.timed_out() {
            crate::ktest::diag(format_args!(
                "expired stall: at={at:?}, answer={answer:?}, before_signal={before_signal:?}, after_kick={after_kick:?}"
            ));
            continue;
        }
        crate::kassert!(stalled);
        crate::kassert_eq!(before_signal, if at == StallAt::BeforeBlocked { State::Running } else { State::Blocked });
        crate::kassert!(t.sigpending()); // a fatal flag implies pending too
        crate::kassert_eq!(after_kick, if at == StallAt::BeforeBlocked { State::Running } else { State::Ready });
        crate::kassert!(clean_entries.load(Ordering::SeqCst));
        return Ok(answer);
    }
    Err("no clean stalled round in five attempts")
}

/// The same stalled rounds for interruptible and killable waits.
#[cfg(feature = "ktest")]
fn stalled_rounds(killable: bool) -> crate::ktest::TestResult {
    for at in [StallAt::BeforeBlocked, StallAt::AfterBlocked, StallAt::AfterRead] {
        for _ in 0..3 {
            let answer = kick_into_a_stalled_block(killable, at, false)?;
            if answer != Some(wake_code(Wake::Interrupted)) {
                crate::ktest::diag(format_args!("stalled {at:?}"));
            }
            crate::kassert_eq!(answer, Some(wake_code(Wake::Interrupted)));
        }
    }
    Ok(())
}

crate::ktest!(
    fn kick_racing_a_block_is_never_lost() {
        // BeforeBlocked proves the waiter's pending read; AfterBlocked
        // permits either side to end the wait but checks the kick's state
        // change; AfterRead requires kick to queue the waiter. Then 300
        // unstalled rounds vary when the kick lands around the block.
        use crate::kassert;
        const ROUNDS: u64 = 300;
        stalled_rounds(false)?;
        let s = Arc::new(Waiting::default());
        let w = s.clone();
        let waiter = spawn_test(last_cpu()?, "kicked", move || {
            for round in 1..=ROUNDS {
                if !w.wait_go(round) {
                    return;
                }
                let woke = w.queue.wait_interruptible(None, || w.ready.load(Ordering::SeqCst));
                clear_current_sigpending();
                w.answer(round, woke);
            }
        })?;
        let t = waiter.thread.clone();
        let mut lost = None;
        for round in 1..=ROUNDS {
            s.go.store(round, Ordering::SeqCst);
            for _ in 0..(round % 20) * 500 {
                core::hint::spin_loop();
            }
            t.set_sigpending();
            kick(&t);
            match s.wait_answer(round) {
                Some(a) if a == wake_code(Wake::Interrupted) => {}
                other => {
                    lost = Some((round, other));
                    // Let a waiter still in its loop go.
                    s.go.store(ROUNDS, Ordering::SeqCst);
                    s.ready.store(true, Ordering::SeqCst);
                    s.queue.wake_all();
                    break;
                }
            }
        }
        join_bounded(waiter)?;
        if let Some((round, answer)) = lost {
            crate::ktest::diag(format_args!("round {round}: answer {answer:?}"));
        }
        kassert!(lost.is_none());
        Ok(())
    }
);

crate::ktest!(
    fn fatal_kick_racing_a_killable_block_is_never_lost() {
        // The same three stalls with fatal_pending alone (its setter also
        // sets sigpending), as de_thread does without group exit. AfterRead
        // requires kick's killable leg; each round uses a fresh thread
        // because fatal_pending is never cleared.
        stalled_rounds(true)?;
        Ok(())
    }
);

crate::ktest!(
    fn wait_killable_ignores_a_non_fatal_kick() {
        // A killable wait on another CPU, kicked with `sigpending` alone:
        // still blocked a while after (a floor), and woken by its waker
        // (`Woken`). Waiting again with `sigpending` still set, it blocks
        // (confirmed), and only a kick after `fatal_pending` ends it:
        // `Interrupted`.
        use crate::{kassert, kassert_eq};
        let s = Arc::new(Waiting::default());
        let w = s.clone();
        let waiter = spawn_test(last_cpu()?, "killable", move || {
            for round in 1..=2 {
                if !w.wait_go(round) {
                    return;
                }
                let woke = w.queue.wait_killable(None, || w.ready.load(Ordering::SeqCst));
                w.ready.store(false, Ordering::SeqCst);
                w.answer(round, woke);
            }
        })?;
        let t = waiter.thread.clone();
        let blocked = || t.state() == State::Blocked && entries_of(&t, &s.queue).1;
        s.go.store(1, Ordering::SeqCst);
        let first = (|| {
            kassert!(yield_until(blocked));
            t.set_sigpending();
            kick(&t);
            let _ = sleep(5_000_000);
            kassert!(blocked());
            kassert_eq!(s.answered.load(Ordering::SeqCst), 0);
            Ok(())
        })();
        s.ready.store(true, Ordering::SeqCst);
        s.queue.wake_all();
        let woke = s.wait_answer(1);
        s.go.store(2, Ordering::SeqCst);
        let second_blocked = yield_until(blocked);
        t.set_fatal_pending();
        kick(&t);
        let killed = s.wait_answer(2);
        join_bounded(waiter)?;
        first?;
        kassert_eq!(woke, Some(wake_code(Wake::Woken)));
        kassert!(second_blocked);
        kassert_eq!(killed, Some(wake_code(Wake::Interrupted)));
        Ok(())
    }
);

crate::ktest!(
    fn interrupt_return_to_kernel_keeps_sigpending() {
        let me = current().ok_or("a thread")?;
        crate::kassert!(arch::irq::enabled());
        me.set_sigpending();
        let start = time::ticks();
        while time::ticks().wrapping_sub(start) < 3 {
            core::hint::spin_loop();
        }
        let pending = me.sigpending();
        clear_current_sigpending();
        crate::kassert!(pending);
        Ok(())
    }
);

crate::ktest!(
    fn exec_name_reaches_crash_snapshot_without_blocking() {
        use crate::kassert_eq;
        let old = with_current(|t| t.task_name()).ok_or("thread")?;
        set_current_name(TaskName::from_comm(b"exec-name"));
        let name = running_here().ok_or("snapshot")?.1;
        let busy = with_current(|t| {
            let _held = t.name.lock();
            running_here().map(|(_, name)| name)
        })
        .flatten();
        set_current_name(old);
        kassert_eq!(name, ThreadName::new("exec-name"));
        kassert_eq!(busy, Some(ThreadName::new("?")));
        Ok(())
    }
);

/// Tests drive balance passes explicitly; ordinary timer/idle balancing is
/// paused only for the duration of these sequential ktests.
#[cfg(feature = "ktest")]
static AUTO_BALANCE: AtomicBool = AtomicBool::new(true);

#[cfg(feature = "ktest")]
struct ManualBalance;

#[cfg(feature = "ktest")]
impl ManualBalance {
    fn new() -> Self {
        assert!(AUTO_BALANCE.swap(false, Ordering::SeqCst));
        Self
    }
}

#[cfg(feature = "ktest")]
impl Drop for ManualBalance {
    fn drop(&mut self) {
        AUTO_BALANCE.store(true, Ordering::SeqCst);
    }
}

crate::ktest!(
    fn nice_scales_slices() {
        use crate::kassert_eq;
        let _manual = ManualBalance::new();
        let observed = Arc::new(SpinLock::new(Vec::with_capacity(40)));
        let out = observed.clone();
        let cpu = last_cpu()?;
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let peer = spawn_test(cpu, "nice-peer", move || {
            let end = time::now_ns() + TEST_DEADLINE_NS;
            while !done.load(Ordering::Acquire) && time::now_ns() < end {
                yield_now();
            }
        })?;
        let h = spawn_test(cpu, "nice-slices", move || {
            let me = current().expect("thread");
            for nice in -20..=19 {
                me.set_nice(nice);
                let were_on = arch::irq::disable();
                // The peer has nice 0: returning must refill with this
                // incoming thread's priority, not the outgoing peer's.
                yield_now();
                let cpu = arch::cpu_index();
                let start = CPUS[cpu].lock().slice_left;
                let after = if start > 1 {
                    tick(false);
                    Some(CPUS[cpu].lock().slice_left)
                } else {
                    None // a final tick switches away and refills on return
                };
                out.lock().push((nice, start, after));
                arch::irq::restore(were_on);
            }
        })?;
        let joined = join_bounded(h);
        stop.store(true, Ordering::Release);
        join_bounded(peer)?;
        joined?;
        let observed = observed.lock();
        kassert_eq!(observed.len(), 40);
        for &(nice, start, after) in observed.iter() {
            kassert_eq!(start, lumen_time::slice_ticks(nice as i8));
            if let Some(after) = after {
                kassert_eq!(after, start - 1);
            }
        }
        Ok(())
    }
);

crate::ktest!(
    fn threads_migrate_to_idle_cpus() {
        use crate::{kassert, kassert_eq};
        let _manual = ManualBalance::new();
        let here = arch::cpu_index();
        let cpus = running_cpus();
        kassert!(cpus.len() > 1);
        let results = Arc::new([const { AtomicUsize::new(usize::MAX) }; MAX_CPUS]);
        let mut handles = Vec::with_capacity(cpus.len());
        let mut passes = 0;
        let stop = Arc::new(AtomicBool::new(false));
        let mut balanced = true;
        let refused;
        {
            // Keep every new thread queued on this CPU until the explicit
            // passes. A remote CPU may execute a stolen thread immediately.
            let _irq = arch::irq::push_off();
            for i in 0..cpus.len() {
                let out = results.clone();
                let stop = stop.clone();
                handles.push(spawn_test_migratable("migrate", move || {
                    out[i].store(arch::cpu_index(), Ordering::Release);
                    let end = time::now_ns() + TEST_DEADLINE_NS;
                    while !stop.load(Ordering::Acquire) && time::now_ns() < end {
                        core::hint::spin_loop();
                    }
                })?);
            }
            for &cpu in cpus.iter().filter(|&&cpu| cpu != here) {
                if balance_pass(cpu) {
                    passes += 1;
                } else {
                    balanced = false;
                }
            }
            let target = *cpus.iter().find(|&&cpu| cpu != here).unwrap();
            let end = time::now_ns() + TEST_DEADLINE_NS;
            while !results.iter().any(|v| v.load(Ordering::Acquire) == target) && time::now_ns() < end {
                core::hint::spin_loop();
            }
            // Source: this runner plus one ready thread. Destination: one
            // running worker. A runnable difference of one must not steal.
            refused = !balance_pass(target);
        }
        stop.store(true, Ordering::Release);
        for h in handles {
            join_bounded(h)?;
        }
        kassert!(balanced && refused);
        kassert_eq!(passes, cpus.len() - 1);
        let mut seen: Vec<_> = results[..cpus.len()].iter().map(|v| v.load(Ordering::Acquire)).collect();
        seen.sort_unstable();
        kassert_eq!(seen, cpus);
        Ok(())
    }
);

crate::ktest!(
    fn balancing_skips_pinned_donor_and_moves_user_roots() {
        use crate::{kassert, kassert_eq};
        let _manual = ManualBalance::new();
        let here = arch::cpu_index();
        let cpus = running_cpus();
        let mut others = cpus.iter().copied().filter(|&cpu| cpu != here);
        let donor = others.next().ok_or("needs three CPUs")?;
        let target = others.next().ok_or("needs three CPUs")?;
        let stop = Arc::new(AtomicBool::new(false));
        let held = Arc::new(AtomicBool::new(false));
        let (done, ready) = (stop.clone(), held.clone());
        let holder = spawn_test(donor, "donor-holder", move || {
            // Keep both roots queued until the explicit balance pass.
            let _irq = arch::irq::push_off();
            ready.store(true, Ordering::Release);
            let end = time::now_ns() + TEST_DEADLINE_NS;
            while !done.load(Ordering::Acquire) && time::now_ns() < end {
                core::hint::spin_loop();
            }
        })?;
        kassert!(yield_until(|| held.load(Ordering::Acquire)));
        let ran = Arc::new([const { AtomicUsize::new(usize::MAX) }; 2]);
        let mut handles = Vec::with_capacity(5);
        let moved;
        {
            let _irq = arch::irq::push_off();
            // The busiest queue has only immutable pins. The next donor
            // has two ordinary user roots; neither has changed its affinity.
            for _ in 0..3 {
                handles.push(spawn_test(here, "pinned-donor", || {})?);
            }
            for i in 0..2 {
                let out = ran.clone();
                let space = Arc::new(AddressSpace::new().map_err(|_| "root space")?);
                handles.push(
                    spawn_user_on(donor, "user-root", space, move || {
                        out[i].store(arch::cpu_index(), Ordering::Release);
                    })
                    .map_err(|e| {
                        crate::ktest::diag(format_args!("spawn user root: {e}"));
                        "spawn user root"
                    })?,
                );
            }
            moved = balance_pass(target);
        }
        stop.store(true, Ordering::Release);
        join_bounded(holder)?;
        for handle in handles {
            join_bounded(handle)?;
        }
        kassert!(moved);
        let mut seen = ran.each_ref().map(|v| v.load(Ordering::Acquire));
        seen.sort_unstable();
        let mut expected = [donor, target];
        expected.sort_unstable();
        kassert_eq!(seen, expected);
        Ok(())
    }
);

crate::ktest!(
    fn pinned_threads_never_migrate() {
        use crate::{kassert, kassert_eq};
        let _manual = ManualBalance::new();
        let here = arch::cpu_index();
        let there = last_cpu()?;
        kassert!(here != there);
        let ran = Arc::new([const { AtomicUsize::new(usize::MAX) }; 3]);
        let mut hs = Vec::with_capacity(3);
        {
            let _irq = arch::irq::push_off();
            let r = ran.clone();
            hs.push(spawn_test(here, "pinned", move || r[0].store(arch::cpu_index(), Ordering::Release))?);
            let r = ran.clone();
            hs.push(spawn_test_migratable("movable", move || r[1].store(arch::cpu_index(), Ordering::Release))?);
            let r = ran.clone();
            hs.push(spawn_test(here, "pinned-last", move || r[2].store(arch::cpu_index(), Ordering::Release))?);
            // The last queued thread is pinned: the pass must search back
            // and move the unpinned control in this very same pass.
            kassert!(balance_pass(there));
            kassert_eq!(hs[0].thread.cpu(), here);
            kassert_eq!(hs[1].thread.cpu(), there);
            kassert_eq!(hs[2].thread.cpu(), here);
        }
        for h in hs {
            join_bounded(h)?;
        }
        kassert_eq!(ran.each_ref().map(|v| v.load(Ordering::Acquire)), [here, there, here]);
        // With the helpers gone, only this runner is runnable. These
        // trailing passes are refused by the runnable-count threshold.
        for _ in 0..8 {
            kassert!(!balance_pass(there));
        }
        Ok(())
    }
);

crate::ktest!(
    fn affinity_wakes_and_kicks_follow_migration() {
        use crate::{kassert, kassert_eq};
        let _manual = ManualBalance::new();
        let here = arch::cpu_index();
        let there = last_cpu()?;
        let waiting = Arc::new(Waiting::default());
        let w = waiting.clone();
        let seen = Arc::new([const { AtomicUsize::new(usize::MAX) }; 2]);
        let out = seen.clone();
        let h = spawn_test_migratable("affinity-wake", move || {
            for round in 1..=2 {
                let deadline = Some(time::now_ns() + TEST_DEADLINE_NS);
                let answer = if round == 1 {
                    w.queue.wait_interruptible(deadline, || w.ready.load(Ordering::SeqCst))
                } else {
                    w.queue.wait_killable(deadline, || w.ready.load(Ordering::SeqCst))
                };
                out[round as usize - 1].store(arch::cpu_index(), Ordering::Release);
                w.answer(round, answer);
            }
        })?;
        let t = h.thread.clone();
        kassert_eq!(t.set_affinity(0), Err(AffinityError::NoOnlineCpu));
        kassert!(yield_until(|| t.state() == State::Blocked && !t.on_cpu.load(Ordering::Acquire)));
        kassert_eq!(t.set_affinity(1 << there), Ok(()));
        kassert_eq!(t.cpu(), here); // blocked threads do not move
        t.set_sigpending();
        {
            // The old CPU cannot run route_ready before this assertion:
            // a fully saved wait must be relocated by wake itself.
            let _irq = arch::irq::push_off();
            kick(&t);
            kassert_eq!(t.cpu(), there);
        }
        kassert_eq!(waiting.wait_answer(1), Some(wake_code(Wake::Interrupted)));
        kassert!(yield_until(|| t.state() == State::Blocked && !t.on_cpu.load(Ordering::Acquire)));
        kassert_eq!(t.set_affinity(1 << here), Ok(()));
        t.set_fatal_pending();
        kick(&t);
        kassert_eq!(waiting.wait_answer(2), Some(wake_code(Wake::Interrupted)));
        join_bounded(h)?;
        kassert_eq!(seen.each_ref().map(|v| v.load(Ordering::Acquire)), [there, here]);
        kassert!(t.sigpending() && t.fatal_pending());
        for q in &CPUS {
            kassert!(!q.lock().sleepers.iter().any(|s| s.thread.id == t.id));
        }
        Ok(())
    }
);

/// A ring-3 program with live register sentinels across nine syscall rounds.
/// Each round publishes snapshots and waits for the kernel test's go-ahead.
#[cfg(feature = "ktest")]
fn migration_program() -> (Vec<u8>, Vec<u8>, Vec<u64>) {
    const DATA: u64 = 0x60_0000;
    let mut code = Vec::new();
    let mut data = alloc::vec![0u8; 4096];
    // TLS at +0x300, XMM input at +0x100, snapshots at +0x400.
    for (i, b) in data[0x100..0x200].iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(37).wrapping_add(11);
    }
    data[0x300..0x308].copy_from_slice(&0x1234_5678_9abc_def0u64.to_le_bytes());
    fn mov_imm(code: &mut Vec<u8>, reg: u8, value: u64) {
        code.extend_from_slice(&[0x48 | (reg >> 3), 0xb8 | (reg & 7)]);
        code.extend_from_slice(&value.to_le_bytes());
    }
    fn mem(code: &mut Vec<u8>, prefix: &[u8], reg: u8, offset: u32) {
        code.extend_from_slice(prefix);
        code.push(0x83 | ((reg & 7) << 3)); // [rbx + disp32]
        code.extend_from_slice(&offset.to_le_bytes());
    }
    // arch_prctl(ARCH_SET_FS, DATA + 0x300)
    mov_imm(&mut code, 0, 158);
    mov_imm(&mut code, 7, 0x1002);
    mov_imm(&mut code, 6, DATA + 0x300);
    code.extend_from_slice(&[0x0f, 0x05]);
    mov_imm(&mut code, 3, DATA);
    for reg in 0..16 {
        let prefix = if reg < 8 { &[0xf3, 0x0f, 0x6f][..] } else { &[0xf3, 0x44, 0x0f, 0x6f][..] };
        mem(&mut code, prefix, reg, 0x100 + u32::from(reg) * 16);
    }
    // All syscall-preserved GPRs, including caller-saved r8/r9/r10.
    const REGS: [u8; 12] = [3, 5, 7, 6, 2, 8, 9, 10, 12, 13, 14, 15];
    let mut expected = Vec::new();
    for reg in REGS {
        let value = if reg == 3 { DATA } else { 0x1020_3040_5060_7000 + u64::from(reg) };
        mov_imm(&mut code, reg, value);
        expected.push(value);
    }
    for round in 1..=9u32 {
        code.extend_from_slice(&[0xb8, 39, 0, 0, 0, 0x0f, 0x05]); // getpid; syscall
        for reg in 0..16 {
            let prefix = if reg < 8 { &[0xf3, 0x0f, 0x7f][..] } else { &[0xf3, 0x44, 0x0f, 0x7f][..] };
            mem(&mut code, prefix, reg, 0x400 + u32::from(reg) * 16);
        }
        for (i, reg) in REGS.into_iter().enumerate() {
            mem(&mut code, &[0x48 | ((reg >> 3) << 2), 0x89], reg, 0x600 + i as u32 * 8);
        }
        // mov rax, fs:[0]; mov [rbx+0x700], rax; snapshot rsp too.
        code.extend_from_slice(&[0x64, 0x48, 0x8b, 0x04, 0x25, 0, 0, 0, 0]);
        mem(&mut code, &[0x48, 0x89], 0, 0x700);
        mem(&mut code, &[0x48, 0x89], 4, 0x708);
        code.extend_from_slice(&[0xc7, 0x03]); // mov dword [rbx], round
        code.extend_from_slice(&round.to_le_bytes());
        code.extend_from_slice(&[0x83, 0x7b, 4, round as u8, 0x72, 0xfa]); // wait [rbx+4] >= round
    }
    code.extend_from_slice(&[0xb8, 231, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05, 0x0f, 0x0b]);
    (code, data, expected)
}

crate::ktest!(
    fn migration_keeps_user_state() {
        use crate::linux::process::{self, ConsoleIo, Stdio};
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        use lumen_vm::Prot;
        const CODE: u64 = 0x40_0000;
        const STACK: u64 = 0x50_0000;
        const DATA: u64 = 0x60_0000;
        let _manual = ManualBalance::new();
        let cpus = running_cpus();
        kassert!(cpus.len() > 1);
        let (code, data, gprs) = migration_program();
        let space = Arc::new(AddressSpace::new().map_err(|_| "space")?);
        space.map_eager_with_bytes(CODE, 4096, Prot::READ | Prot::EXEC, &code).map_err(|_| "code")?;
        space.map_eager_with_bytes(STACK, 4096, Prot::READ | Prot::WRITE, &[]).map_err(|_| "stack")?;
        space.map_eager_with_bytes(DATA, 4096, Prot::READ | Prot::WRITE, &data).map_err(|_| "data")?;
        let stdio = Stdio::open().map_err(|_| "stdio")?;
        let exit = Arc::new(SpinLock::new(None));
        let out = exit.clone();
        let h = spawn_user_on(cpus[0], "migrate-user", space.clone(), move || {
            process::register_current(ConsoleIo::Capture, false, Vec::new(), None, stdio);
            // SAFETY: spawn_user_on installed the address space; code and
            // stack are mapped user pages and the entry runs with IRQs on.
            unsafe { arch::syscall::enter_ring3(CODE, STACK + 4096, 0x202) };
            *out.lock() = process::take_current().exit;
        })
        .map_err(|_| "spawn user")?;
        let t = h.thread.clone();
        // Keep mappings alive if the user exits as soon as the last
        // checkpoint is released, before this CPU detaches its guard.
        kassert!(space.add_user());
        let guard = UserSpaceGuard::enter(space.clone());
        let mut moves = 0;
        let result = (|| {
            for round in 1..=9u32 {
                kassert!(yield_until(|| {
                    let mut mark = [0u8; 4];
                    copy_from_user(&mut mark, DATA).is_ok() && u32::from_le_bytes(mark) == round
                }));
                let mut xmm = [0u8; 256];
                copy_from_user(&mut xmm, DATA + 0x400).map_err(|_| "read XMM")?;
                kassert_eq!(xmm.as_slice(), &data[0x100..0x200]);
                let mut regs = [0u8; 96];
                copy_from_user(&mut regs, DATA + 0x600).map_err(|_| "read GPRs")?;
                for (bytes, &expected) in regs.as_chunks::<8>().0.iter().zip(&gprs) {
                    kassert_eq!(u64::from_le_bytes(*bytes), expected);
                }
                let mut tls_rsp = [0u8; 16];
                copy_from_user(&mut tls_rsp, DATA + 0x700).map_err(|_| "read TLS/RSP")?;
                kassert_eq!(&tls_rsp[..8], &data[0x300..0x308]);
                kassert_eq!(u64::from_le_bytes(tls_rsp[8..].try_into().unwrap()), STACK + 4096);
                if round < 9 {
                    let target = cpus[round as usize % cpus.len()];
                    kassert_eq!(t.set_affinity(1 << target), Ok(()));
                    // First confirm placement, then require a tick charged
                    // there: a queued thread has not necessarily run yet.
                    kassert!(yield_until(|| t.cpu() == target));
                    let before = t.ticks.load(Ordering::Acquire);
                    kassert!(yield_until(|| t.ticks.load(Ordering::Acquire) > before));
                    kassert!(space.active_mask() & (1 << target) != 0);
                    moves += 1;
                }
                copy_to_user(DATA + 4, &round.to_le_bytes()).map_err(|_| "release round")?;
            }
            Ok(())
        })();
        if result.is_err() {
            // Release every checkpoint so a failed assertion still joins.
            let _ = copy_to_user(DATA + 4, &9u32.to_le_bytes());
        }
        drop(guard);
        space.drop_user();
        join_bounded(h)?;
        result?;
        kassert_eq!(moves, 8);
        kassert!(matches!(*exit.lock(), Some(crate::user::UserExit::Exited(0))));
        Ok(())
    }
);

crate::ktest!(
    fn migration_keeps_deferred_state() {
        use crate::{kassert, kassert_eq};
        use arch::percpu::{DebugCode, DebugOrigin, DebugTrap};
        let _manual = ManualBalance::new();
        let cpus = running_cpus();
        let space = Arc::new(AddressSpace::new().map_err(|_| "space")?);
        let mm = space.clone();
        let result = Arc::new(SpinLock::new(None));
        let out = result.clone();
        let h = spawn_test_migratable("migrate-debug", move || {
            let _space = UserSpaceGuard::enter(mm.clone());
            let me = current().expect("thread");
            let run = || -> crate::ktest::TestResult {
                me.set_sigpending();
                me.set_fatal_pending();
                for cpu in cpus.into_iter().cycle().take(8) {
                    let were_on = arch::irq::disable();
                    arch::percpu::set_kernel_rsp(0x1234_5000);
                    arch::percpu::set_user_exit_rsp(0x2345_6000);
                    arch::percpu::set_pending_debug_trap(Some(DebugTrap {
                        code: DebugCode::Trace,
                        rip: 0x4567_8000,
                        origin: DebugOrigin::Lstar,
                    }));
                    me.set_affinity(1 << cpu).expect("migratable thread");
                    yield_now();
                    let landed = arch::cpu_index();
                    let krsp = arch::percpu::kernel_rsp();
                    let exit_rsp = arch::percpu::user_exit_rsp();
                    let debug = arch::percpu::take_pending_debug_trap();
                    let loaded = arch::percpu::loaded_space();
                    // Clear the synthetic state before restoring IRQs.
                    arch::percpu::set_kernel_rsp(0);
                    arch::percpu::set_user_exit_rsp(0);
                    arch::irq::restore(were_on);
                    kassert_eq!(landed, cpu);
                    kassert_eq!(krsp, 0x1234_5000);
                    kassert_eq!(exit_rsp, 0x2345_6000);
                    let debug = debug.ok_or("lost deferred trap")?;
                    kassert!(matches!(debug.code, DebugCode::Trace));
                    kassert_eq!(debug.origin, DebugOrigin::Lstar);
                    kassert_eq!(debug.rip, 0x4567_8000);
                    kassert_eq!(loaded, Arc::as_ptr(&mm));
                    kassert_eq!(mm.active_mask(), 1 << cpu);
                    kassert!(me.sigpending() && me.fatal_pending());
                }
                Ok(())
            };
            *out.lock() = Some(run());
        })?;
        join_bounded(h)?;
        result.lock().take().ok_or("missing result")??;
        kassert_eq!(space.active_mask(), 0);
        Ok(())
    }
);

#[cfg(feature = "ktest")]
static SWITCH_DELAY: BlockDelay = BlockDelay::new();

crate::ktest!(
    fn automatic_balancer_moves_runnable_threads() {
        use crate::{kassert, kassert_eq};
        let here = arch::cpu_index();
        kassert!(running_cpus().len() >= 4);
        let seen = Arc::new([const { AtomicUsize::new(usize::MAX) }; 3]);
        let stop = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::with_capacity(3);
        {
            let _irq = arch::irq::push_off();
            for i in 0..3 {
                let seen = seen.clone();
                let stop = stop.clone();
                handles.push(spawn_test_migratable("auto-migrate", move || {
                    let end = time::now_ns() + TEST_DEADLINE_NS;
                    while !stop.load(Ordering::Acquire) && time::now_ns() < end {
                        let cpu = arch::cpu_index();
                        if cpu != here {
                            seen[i].store(cpu, Ordering::Release);
                        }
                        core::hint::spin_loop();
                    }
                })?);
            }
        }
        // Stay runnable too: four runnable threads on four CPUs must spread
        // all three workers away. No explicit balance call or timed rate.
        let end = time::now_ns() + TEST_DEADLINE_NS;
        while seen.iter().any(|v| v.load(Ordering::Acquire) == usize::MAX) && time::now_ns() < end {
            core::hint::spin_loop();
        }
        let cpus = seen.each_ref().map(|v| v.load(Ordering::Acquire));
        stop.store(true, Ordering::Release);
        for h in handles {
            join_bounded(h)?;
        }
        kassert_eq!(cpus.iter().filter(|&&cpu| cpu != usize::MAX && cpu != here).count(), 3);
        Ok(())
    }
);

#[cfg(feature = "ktest")]
fn deadline_follows_affinity(on_queue: bool) -> crate::ktest::TestResult {
    use crate::{kassert, kassert_eq};
    let _manual = ManualBalance::new();
    let here = arch::cpu_index();
    let there = last_cpu()?;
    for _ in 0..5 {
        let seen = Arc::new(AtomicUsize::new(usize::MAX));
        let answer = Arc::new(SpinLock::new(None));
        let (out, result) = (seen.clone(), answer.clone());
        let h = spawn_test_migratable("deadline-move", move || {
            let wake = if on_queue {
                WaitQueue::new().wait_interruptible(Some(time::now_ns() + 100_000_000), || false)
            } else {
                assert!(sleep(100_000_000));
                Wake::TimedOut
            };
            out.store(arch::cpu_index(), Ordering::Release);
            *result.lock() = Some(wake);
        })?;
        let t = h.thread.clone();
        let state = if on_queue { State::Blocked } else { State::Sleeping };
        let waiting = yield_until(|| t.state() == state && !t.on_cpu.load(Ordering::Acquire) || h.is_finished());
        let changed = {
            // The timer on the original CPU cannot expire this deadline
            // between checking the state and setting its affinity.
            let _irq = arch::irq::push_off();
            if waiting && t.state() == state {
                kassert_eq!(t.set_affinity(1 << there), Ok(()));
                kassert_eq!(t.state(), state);
                kassert_eq!(t.cpu(), here);
                true
            } else {
                false
            }
        };
        let finished = yield_until(|| h.is_finished());
        // Rescue a broken route_ready before asserting: the disallowed
        // Ready thread otherwise remains stranded on its old queue.
        if !finished {
            t.set_affinity(1 << here).expect("migratable helper");
        }
        join_bounded(h)?;
        if !changed {
            continue; // the deadline passed before we changed the mask
        }
        kassert!(finished);
        kassert_eq!(seen.load(Ordering::Acquire), there);
        kassert_eq!(*answer.lock(), Some(Wake::TimedOut));
        kassert!(!CPUS[here].lock().sleepers.iter().any(|s| s.thread.id == t.id));
        return Ok(());
    }
    Err("no sleeping affinity change in five attempts")
}

crate::ktest!(
    fn sleeping_deadline_follows_affinity() {
        deadline_follows_affinity(false)
    }
);

crate::ktest!(
    fn wait_deadline_follows_affinity() {
        deadline_follows_affinity(true)
    }
);

crate::ktest!(
    fn affinity_refuses_immutable_pins() {
        use crate::{kassert, kassert_eq};
        let here = arch::cpu_index();
        let me = current().expect("runner");
        let original = me.affinity();
        kassert_eq!(me.set_affinity(ALL_CPUS), Err(AffinityError::Pinned));
        kassert_eq!(me.affinity(), original);
        let idle = CPUS[here].lock().idle.clone().expect("idle");
        kassert_eq!(idle.set_affinity(ALL_CPUS), Err(AffinityError::Pinned));
        let space = Arc::new(AddressSpace::new().map_err(|_| "space")?);
        let result = Arc::new(SpinLock::new(None));
        let out = result.clone();
        let h = spawn_user_pinned_on(here, "pinned-user", space, move || {
            let me = current().expect("user probe");
            *out.lock() = Some((me.cpu(), me.affinity(), me.set_affinity(ALL_CPUS)));
        })
        .map_err(|e| {
            crate::ktest::diag(format_args!("pinned user: {e}"));
            "spawn pinned user"
        })?;
        kassert!(h.thread.pinned);
        kassert_eq!(h.thread.affinity(), 1 << here);
        join_bounded(h)?;
        kassert_eq!(*result.lock(), Some((here, 1 << here, Err(AffinityError::Pinned))));
        Ok(())
    }
);

crate::ktest!(
    fn queue_reservations_fail_and_roll_back() {
        use crate::{kassert, kassert_eq};
        let queues = [SpinLock::new(Cpu::new()), SpinLock::new(Cpu::new())];
        // The first queue can reserve without allocating; the second has
        // no buffers. Its real try_reserve failure must roll back the first.
        queues[0].lock().try_home().map_err(|_| "prepare queue")?;
        queues[0].lock().reserved -= 1;
        queues[1].lock().reserved = 4095;
        let failed = {
            let _irq = arch::irq::push_off();
            // 4096 Arc slots require at least 32 KiB; small unrelated
            // allocations remain available while this reservation fails.
            crate::mm::heap::refuse_from(16 * 1024);
            let failed = reserve_queues(&queues);
            crate::mm::heap::refuse_from(usize::MAX);
            failed
        };
        kassert!(matches!(failed, Err(SpawnError::NoMemory)));
        kassert_eq!(queues[0].lock().reserved, 0);
        kassert_eq!(queues[1].lock().reserved, 4095);
        queues[1].lock().reserved = 0;
        let reserved = reserve_queues(&queues).map_err(|_| "reserve queues")?;
        for q in &queues {
            kassert_eq!(q.lock().reserved, 1);
        }
        drop(reserved); // a later Thread::spawned failure uses this same rollback
        for q in &queues {
            kassert_eq!(q.lock().reserved, 0);
        }
        Ok(())
    }
);

crate::ktest!(
    fn migration_waits_for_complete_switch() {
        use crate::{kassert, kassert_eq};
        let _manual = ManualBalance::new();
        let source = last_cpu()?;
        let target = running_cpus()
            .into_iter()
            .find(|&cpu| cpu != source && cpu != arch::cpu_index())
            .ok_or("needs three CPUs")?;
        for _ in 0..5 {
            let gate = Arc::new(AtomicBool::new(false));
            let started = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let ran = Arc::new(AtomicUsize::new(usize::MAX));
            let (g, s, r) = (gate.clone(), started.clone(), ran.clone());
            let owner = spawn_test_migratable("switch-owner", move || {
                current().unwrap().set_affinity(1 << source).expect("migratable owner");
                yield_now();
                s.store(true, Ordering::SeqCst);
                let end = time::now_ns() + TEST_DEADLINE_NS;
                while !g.load(Ordering::SeqCst) && time::now_ns() < end {
                    core::hint::spin_loop();
                }
                yield_now();
                r.store(arch::cpu_index(), Ordering::SeqCst);
            })?;
            kassert!(yield_until(|| started.load(Ordering::SeqCst)));
            let mut peers = Vec::new();
            for _ in 0..2 {
                let done = stop.clone();
                peers.push(spawn_test(source, "switch-peer", move || {
                    let end = time::now_ns() + TEST_DEADLINE_NS;
                    while !done.load(Ordering::SeqCst) && time::now_ns() < end {
                        core::hint::spin_loop();
                    }
                })?);
            }
            let t = owner.thread.clone();
            SWITCH_DELAY.arm(t.id, StallAt::MidSwitch);
            t.set_affinity((1 << source) | (1 << target)).expect("migratable thread");
            gate.store(true, Ordering::SeqCst);
            let stalled = yield_until(|| SWITCH_DELAY.stalled());
            // The owner is queued Ready but still runs on its unsaved stack.
            let state = t.state();
            let on_cpu = t.on_cpu.load(Ordering::Acquire);
            let stolen = balance_pass(target);
            t.set_affinity(1 << target).expect("migratable thread");
            let before_release = t.cpu();
            SWITCH_DELAY.release();
            stop.store(true, Ordering::SeqCst);
            join_bounded(owner)?;
            for h in peers {
                join_bounded(h)?;
            }
            if SWITCH_DELAY.timed_out() {
                continue;
            }
            kassert!(stalled && on_cpu && !stolen);
            kassert_eq!(state, State::Ready);
            kassert_eq!(before_release, source);
            kassert_eq!(ran.load(Ordering::SeqCst), target);
            return Ok(());
        }
        Err("no complete switch stall in five attempts")
    }
);

/// Set the caller's Linux comm and its sanitized display snapshot under one lock.
pub fn set_current_comm(bytes: [u8; 16]) {
    let name = TaskName::from_comm(&bytes);
    with_current(|t| *t.name.lock() = name).expect("running thread");
}

crate::ktest!(
    fn task_name_normalizes_raw_comm() {
        use crate::kassert_eq;
        for (input, expected, display) in [
            (b"".as_slice(), [0; 16], ""),
            (b"a\0trailing", *b"a\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0", "a"),
            (b"abcdefghijklmnopq", *b"abcdefghijklmno\0", "abcdefghijklmno"),
            (b"a\nb", *b"a\nb\0\0\0\0\0\0\0\0\0\0\0\0\0", "a?b"),
            (b"a\xffb", *b"a\xffb\0\0\0\0\0\0\0\0\0\0\0\0\0", "a?b"),
        ] {
            let name = TaskName::from_comm(input);
            kassert_eq!(name.bytes, expected);
            kassert_eq!(name.display.as_str(), display);
        }
        Ok(())
    }
);

crate::ktest!(
    fn task_name_display_needs_no_heap() {
        use crate::kassert_eq;
        let inputs: [&[u8]; 6] = [
            b"a\xffb",
            b"\xf0\x9f",
            b"\xc0\xaf",
            b"a\xe2\x82b",
            b"\xff\xc2\x85\xe2\x80\xa8\xc3\xa9",
            b"abcdefghijklmn\xc3\xa9",
        ];
        let (names, displayed, converted) = {
            let _irq = arch::irq::push_off();
            crate::mm::heap::refuse_from(0);
            let names = inputs.map(TaskName::from_comm);
            let displayed = TaskName::from_display(ThreadName::new("display"));
            let converted = TaskName::from("converted");
            crate::mm::heap::refuse_from(usize::MAX);
            (names, displayed, converted)
        };
        for ((name, input), expected) in
            names.iter().zip(inputs).zip(["a?b", "?", "??", "a?b", "???é", "abcdefghijklmn?"])
        {
            let len = input.len().min(15);
            kassert_eq!(&name.bytes[..len], &input[..len]);
            kassert_eq!(name.bytes[len], 0);
            kassert_eq!(name.display.as_str(), expected);
        }
        kassert_eq!(displayed.display.as_str(), "display");
        kassert_eq!(converted.display.as_str(), "converted");
        Ok(())
    }
);

crate::ktest!(
    fn cpu_ticks_distinguish_user_and_system() {
        use crate::kassert_eq;
        // An unregistered thread: synthetic charges cannot perturb live CPU totals.
        let t = Thread::new(
            TaskName::from_display(ThreadName::new("tick-test")),
            0,
            State::Blocked,
            0,
            None,
            None,
            None,
            Weak::new(),
            Placement::pinned(0),
        );
        kassert_eq!(t.cpu_time(), [0, 0]);
        t.charge_tick(true);
        t.charge_tick(false);
        t.charge_tick(false);
        kassert_eq!(t.cpu_time(), [1, 2]);
        kassert_eq!(t.ticks.load(Ordering::Relaxed), 3);
        Ok(())
    }
);
