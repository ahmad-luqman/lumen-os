//! Process-owned signal queues and delivery transitions. All queue operations
//! run under the process's signal lock; sends by tid also hold PROCS so an
//! unlink or tid reuse cannot race lookup and enqueue.

use super::*;
use crate::linux::errno;
use crate::user::UserFault;
use alloc::sync::Weak;
use lumen_signal::altstack::{AltStack, StackError};
use lumen_signal::pending::{
    Disposition, PendingSignals, SenderClass, SigInfo, Target, ThreadCandidate, ThreadSlot, ThreadState, select_target,
};
use lumen_signal::{DefaultAction, SigSet};

/// The first reason the group started ending, serialized by the signal lock.
#[derive(Clone, Copy)]
enum GroupEnd {
    Exit,
    Signal(Signal),
}

/// One stop generation; a thread contributes to remaining at most once.
#[derive(Clone, Copy)]
pub(super) struct GroupStop {
    generation: u64,
    signal: Signal,
    remaining: usize,
}

/// Initial per-thread state, copied under the parent's signal lock.
#[derive(Clone, Copy)]
pub(super) struct ThreadInit {
    pub blocked: Blocked,
    pub alt_stack: AltStack,
}
impl ThreadInit {
    pub const ROOT: Self = Self { blocked: Blocked::EMPTY, alt_stack: AltStack::DISABLED };
}

/// Dispositions, queues and the first group-end decision, serialized by Process::signal.
pub struct SignalState {
    /// Per-signal dispositions, shared by every thread.
    actions: [KernelSigaction; 64],
    /// Shared and private queues, owning each linked ThreadSlot.
    pub(super) pending: PendingSignals,
    pub(super) timers: super::timer::Timers,
    /// Live signal state in ascending tid order.
    threads: BTreeMap<u32, ThreadSignals>,
    /// Cursor for process-directed target selection.
    preferred: Option<ThreadSlot>,
    /// First-wins end decision; Process::exit records its diagnostic.
    group_exit: Option<GroupEnd>,
    pub(super) exec_waiter: Option<u32>,
    pub(super) rlimit_stack: lumen_vm::StackLimit,
    pub(super) ctty: Option<Arc<crate::tty::Tty>>,
    group_stop: Option<GroupStop>,
    stop_generation: u64,
}

/// One linked thread's queue slot, notification handle, mask and alternate stack.
pub struct ThreadSignals {
    /// Private queue identity, valid until remove_thread.
    slot: ThreadSlot,
    /// Shared with UserThread so binding updates the notification target.
    thread: Arc<SpinLock<Weak<sched::Thread>>>,
    /// Explicit ktest stand-in with no runnable scheduler thread.
    synthetic: bool,
    /// Sanitized mask, never containing SIGKILL or SIGSTOP.
    blocked: Blocked,
    real_blocked: Option<Blocked>,
    saved_mask: Option<Blocked>,
    stop_pending: bool,
    stop_dequeued: bool,
    /// Validated alternate-stack configuration.
    alt_stack: AltStack,
    /// Last synchronous diagnostic; only its matching signal consumes it.
    fault: Option<UserFault>,
    /// Failed frame origin, discarded when SEGV reaches a handler.
    frame_failure: Option<crate::linux::signal::frame::FrameFailure>,
}

impl ThreadSignals {
    fn candidate(&self, stopped: bool) -> ThreadCandidate {
        let thread = self.thread.lock().upgrade();
        ThreadCandidate {
            slot: self.slot,
            mask: self.blocked,
            state: if stopped {
                ThreadState::Stopped
            } else {
                match thread.as_ref().map(|t| t.state()) {
                    Some(sched::State::Running) => ThreadState::Running,
                    Some(sched::State::Dead) | None => ThreadState::Exiting,
                    _ => ThreadState::Sleeping,
                }
            },
            has_pending: thread.is_some_and(|t| t.sigpending()),
        }
    }
    fn take_fault(&mut self, signal: Signal) -> Option<UserFault> {
        if self.fault.is_some_and(|fault| fault.signal() == signal) { self.fault.take() } else { None }
    }
    fn notify(&self, fatal: bool) {
        let thread = self.thread.lock().upgrade();
        debug_assert!(thread.is_some() || self.synthetic, "linked thread has no notification target");
        if let Some(thread) = thread {
            thread.set_sigpending();
            if fatal {
                thread.set_fatal_pending();
            }
            sched::kick(&thread);
        }
    }
}

/// A dequeue result; a fatal diagnostic belongs only to the winning signal.
pub enum Delivery {
    None,
    Fatal(Signal, Option<UserFault>),
    Handler(HandlerDelivery),
    Stop(Signal),
    Park,
}

/// A handler selected under siglock, consumed after releasing the lock.
pub struct HandlerDelivery {
    pub info: SigInfo,
    pub action: KernelSigaction,
    pub blocked: Blocked,
    pub saved_mask: Blocked,
    pub stack: AltStack,
    pub fault: Option<UserFault>,
}

fn disposition(a: KernelSigaction) -> Disposition {
    match a.handler {
        0 => Disposition::Default,
        1 => Disposition::Ignore,
        _ => Disposition::Handler,
    }
}
fn kills_by_default(a: KernelSigaction, sig: Signal) -> bool {
    a.handler == 0 && matches!(sig.default_action(), DefaultAction::Term | DefaultAction::Core)
}
fn ignored(a: KernelSigaction, sig: Signal) -> bool {
    a.handler == 1 || (a.handler == 0 && matches!(sig.default_action(), DefaultAction::Ign | DefaultAction::Cont))
}

impl SignalState {
    pub(super) fn new() -> Self {
        Self {
            actions: [KernelSigaction::default(); 64],
            pending: PendingSignals::new(0, 1024).expect("empty pending queues").0,
            timers: super::timer::Timers::default(),
            threads: BTreeMap::new(),
            preferred: None,
            group_exit: None,
            exec_waiter: None,
            rlimit_stack: lumen_vm::StackLimit::DEFAULT,
            ctty: None,
            group_stop: None,
            stop_generation: 0,
        }
    }
    pub fn action(&self, signal: Signal) -> KernelSigaction {
        self.actions[signal.index()]
    }
    /// Replace an action, returning its predecessor; KILL/STOP return None.
    /// Strip unknown flags and unblockable mask bits, and discard ignored pending entries.
    pub fn set_action(&mut self, signal: Signal, mut action: KernelSigaction) -> Option<KernelSigaction> {
        if signal.is_unblockable() {
            return None;
        }
        action.mask = SigSet(action.mask).unblockable_stripped().0;
        action.flags &= u64::from(
            general::SA_NOCLDSTOP
                | general::SA_NOCLDWAIT
                | general::SA_SIGINFO
                | general::SA_RESTORER
                | general::SA_ONSTACK
                | general::SA_RESTART
                | general::SA_NODEFER
                | general::SA_RESETHAND,
        );
        self.pending.disposition_changed(signal, disposition(action));
        Some(core::mem::replace(&mut self.actions[signal.index()], action))
    }
    pub(super) fn fork_copy(&self) -> Self {
        Self { actions: self.actions, rlimit_stack: self.rlimit_stack, ctty: self.ctty.clone(), ..Self::new() }
    }
    pub(super) fn fatal_signal(&self) -> Option<Signal> {
        match self.group_exit {
            Some(GroupEnd::Signal(sig)) => Some(sig),
            _ => None,
        }
    }
    pub(super) fn ending(&self) -> bool {
        self.group_exit.is_some()
    }
    pub(super) fn record_exit(&mut self) {
        self.group_exit.get_or_insert(GroupEnd::Exit);
    }
    pub(super) fn record_signal(&mut self, signal: Signal) {
        self.group_exit.get_or_insert(GroupEnd::Signal(signal));
    }

    /// The link and its refusal checks share the caller's signal lock.
    pub(super) fn check_clone(&self) -> Result<(), u32> {
        if self.ending() {
            return Err(errno::EINTR);
        }
        if self.exec_waiter.is_some() || self.group_stop.is_some() || sched::current_sigpending() {
            return Err(lumen_signal::restart::RestartCode::NoIntr as u32);
        }
        if sched::with_current(|t| t.fatal_pending()).unwrap_or(false) {
            return Err(errno::EINTR);
        }
        Ok(())
    }

    pub(super) fn notify_group_exit(&self, caller: u32) {
        for (&tid, t) in &self.threads {
            if tid != caller {
                t.notify(true);
            }
        }
    }

    /// Kill siblings privately; the exec caller is exempt from these kills.
    pub(super) fn begin_exec(&mut self, process: &Process, caller: u32) -> Result<(), u32> {
        if self.ending() || self.exec_waiter.is_some() {
            return Err(errno::EAGAIN);
        }
        self.exec_waiter = Some(caller);
        self.cancel_stop(process);
        for (&tid, t) in &self.threads {
            if tid != caller {
                self.pending
                    .enqueue(Target::Thread(t.slot), SigInfo::new(Signal::KILL, 0), SenderClass::SiUser)
                    .expect("standard private SIGKILL");
                t.notify(true);
            }
        }
        Ok(())
    }

    pub(super) fn exchange_tid(&mut self, old: u32, pid: u32) {
        let signals = self.threads.remove(&old).expect("exec caller's signals");
        assert!(self.threads.insert(pid, signals).is_none(), "exec leader still linked");
    }

    #[cfg(feature = "ktest")]
    pub(super) fn prefer_thread(&mut self, tid: u32) {
        self.preferred = Some(self.thread(tid).expect("test target").slot);
    }

    #[cfg(feature = "ktest")]
    pub(super) fn stop_remaining(&self) -> Option<usize> {
        self.group_stop.map(|s| s.remaining)
    }

    #[cfg(feature = "ktest")]
    pub(super) fn ktest_dequeue_stop(&mut self, tid: u32, process: &Process) {
        assert!(matches!(self.next(tid), Delivery::Stop(Signal::STOP)));
        self.start_stop(tid, Signal::STOP, process);
    }
    pub(super) fn thread_init(&self, tid: u32, request: ProcessClone) -> ThreadInit {
        let t = self.thread(tid).expect("live parent signal tid");
        ThreadInit {
            blocked: t.blocked,
            alt_stack: if request.keeps_alt_stack() { t.alt_stack } else { AltStack::DISABLED },
        }
    }
    fn start_group_exit(&mut self, sig: Signal) {
        self.group_exit.get_or_insert(GroupEnd::Signal(sig));
        for t in self.threads.values() {
            t.notify(true);
        }
    }
    /// Reserve a private queue before publishing any tid.
    pub(super) fn prepare_thread(&mut self, owner: &UserThread, init: ThreadInit) -> Result<ThreadSignals, u32> {
        let slot = self.pending.add_thread().map_err(|_| errno::ENOMEM)?;
        Ok(ThreadSignals {
            slot,
            thread: Arc::clone(&owner.kernel_handle),
            synthetic: owner.synthetic,
            blocked: init.blocked,
            real_blocked: None,
            saved_mask: None,
            stop_pending: false,
            stop_dequeued: false,
            alt_stack: init.alt_stack,
            fault: None,
            frame_failure: None,
        })
    }
    pub(super) fn cancel_thread(&mut self, signals: ThreadSignals) {
        self.pending.remove_thread(signals.slot).expect("reserved signal slot");
    }
    #[cfg(feature = "ktest")]
    pub(super) fn slot_count(&self) -> usize {
        self.pending.slot_count()
    }
    pub(super) fn insert_thread(&mut self, tid: u32, signals: ThreadSignals) {
        let slot = signals.slot;
        assert!(self.threads.insert(tid, signals).is_none(), "duplicate signal tid");
        self.preferred.get_or_insert(slot);
    }
    #[cfg(feature = "ktest")]
    pub(super) fn add_thread(&mut self, tid: u32, owner: &UserThread, init: ThreadInit) {
        let signals = self.prepare_thread(owner, init).expect("test signal queue");
        self.insert_thread(tid, signals);
    }
    /// Remove a live tid and private queue; panics if it was not linked.
    pub(super) fn remove_thread(&mut self, tid: u32) {
        let removed = self.threads.remove(&tid).expect("live signal tid");
        self.pending.remove_thread(removed.slot).expect("live pending slot");
        if self.preferred == Some(removed.slot) {
            self.preferred = self.threads.values().next().map(|t| t.slot);
        }
        // Shared pending signals survive the departing private queue.
        for t in self.threads.values() {
            if !self.ending() && self.pending.has_deliverable(t.slot, t.blocked).expect("live slot") {
                t.notify(false);
            }
        }
    }
    pub fn thread(&self, tid: u32) -> Option<&ThreadSignals> {
        self.threads.get(&tid)
    }
    #[cfg(feature = "ktest")]
    pub(super) fn has_thread(&self, tid: u32) -> bool {
        self.thread(tid).is_some()
    }
    #[cfg(feature = "ktest")]
    /// Linked tids in ascending order.
    pub(super) fn tids(&self) -> impl Iterator<Item = u32> + '_ {
        self.threads.keys().copied()
    }
    /// Its sanitized mask; panics if tid is not linked.
    pub(super) fn blocked(&self, tid: u32) -> Blocked {
        self.thread(tid).expect("live signal tid").blocked
    }
    /// Replace the mask and notify if pending work is now deliverable; panics for an unlinked tid.
    pub(super) fn set_blocked(&mut self, tid: u32, blocked: Blocked) {
        let t = self.threads.get_mut(&tid).expect("live signal tid");
        t.blocked = blocked;
        if self.pending.has_deliverable(t.slot, blocked).expect("live slot") {
            t.notify(false);
        }
    }
    /// Recompute the caller's notification after consuming or masking work.
    fn refresh_current(&self, tid: u32) {
        let t = self.thread(tid).expect("current signal tid");
        if !self.ending()
            && self.group_stop.is_none()
            && !sched::with_current(|t| t.fatal_pending()).unwrap_or(false)
            && !self.pending.has_deliverable(t.slot, t.blocked).expect("live slot")
        {
            sched::clear_current_sigpending();
        }
    }
    fn configure_stack(&mut self, tid: u32, rsp: u64, new: Option<(u64, u64, u32)>) -> Result<AltStack, StackError> {
        let t = self.threads.get_mut(&tid).expect("current signal tid");
        let old = t.alt_stack;
        if let Some((sp, size, flags)) = new {
            t.alt_stack = old.configure(rsp, sp, size, flags)?;
        }
        Ok(old)
    }
    fn install_context(&mut self, tid: u32, mask: Blocked, stack: AltStack) {
        let t = self.threads.get_mut(&tid).expect("current signal tid");
        t.alt_stack = stack;
        t.saved_mask = None;
        self.set_blocked(tid, mask);
    }
    pub(super) fn send(&mut self, tid: Option<u32>, info: SigInfo, sender: SenderClass) -> Result<(), u32> {
        self.send_result(tid, info, sender, None).map(|_| ())
    }
    pub(super) fn send_result(
        &mut self,
        tid: Option<u32>,
        info: SigInfo,
        sender: SenderClass,
        timer_instance: Option<u64>,
    ) -> Result<Option<lumen_signal::pending::Enqueued>, u32> {
        let sig = info.signal();
        let thread = tid.map(|tid| self.threads.get(&tid).ok_or(errno::ESRCH)).transpose()?;
        let directed = thread.map(|t| (t, t.blocked.contains(sig)));
        let target = thread.map_or(Target::Process, |t| Target::Thread(t.slot));
        let real_blocks = |t: &ThreadSignals| t.real_blocked.is_some_and(|b| b.contains(sig));
        let keep_ignored = thread.map_or_else(
            || self.threads.values().any(real_blocks) || self.threads.values().all(|t| t.blocked.contains(sig)),
            |t| t.blocked.contains(sig) || real_blocks(t),
        );
        if self.ending() {
            return Ok(None);
        }
        self.pending.prepare_signal(sig);
        if ignored(self.action(sig), sig) && !keep_ignored {
            return Ok(None);
        }
        let enqueued = match timer_instance {
            Some(instance) => self.pending.enqueue_timer(target, info, instance),
            None => self.pending.enqueue(target, info, sender),
        }
        .map_err(|_| errno::EAGAIN)?;
        let chosen = match target {
            // A stopped target retains even an unblocked default-fatal
            // signal until continue; only SIGKILL may end the stop.
            Target::Thread(_) => directed.filter(|_| sig == Signal::KILL || self.group_stop.is_none()),
            Target::Process => {
                let candidates: Vec<_> =
                    self.threads.values().map(|t| t.candidate(self.group_stop.is_some())).collect();
                self.preferred.and_then(|p| select_target(sig, &candidates, p).expect("live cursor")).map(|slot| {
                    let t = self.threads.values().find(|t| t.slot == slot).expect("chosen slot");
                    (t, t.blocked.contains(sig))
                })
            }
        };
        if let Some((t, blocked)) = chosen {
            self.preferred = Some(t.slot);
            if !blocked {
                if kills_by_default(self.action(sig), sig)
                    && !self.threads.values().any(|t| t.real_blocked.is_some_and(|b| b.contains(sig)))
                {
                    self.start_group_exit(sig);
                } else {
                    t.notify(false);
                }
            }
        }
        Ok(Some(enqueued))
    }
    /// Force on the current linked tid. Reset the disposition when requested or
    /// blocked/ignored; once ending, drop the request. None preserves the fault slot.
    fn force(&mut self, tid: u32, info: SigInfo, reset: bool, fault: Option<UserFault>) {
        if self.ending() {
            return;
        }
        let sig = info.signal();
        let t = self.thread(tid).expect("current signal tid");
        if reset || t.blocked.contains(sig) || self.action(sig).handler == 1 {
            self.set_action(sig, KernelSigaction::default());
            let mut mask = self.blocked(tid).set();
            mask.remove(sig);
            self.set_blocked(tid, Blocked::new(mask));
        }
        if fault.is_some() {
            self.threads.get_mut(&tid).expect("current signal tid").fault = fault;
        }
        debug_assert!(!sig.is_realtime());
        self.send(Some(tid), info, SenderClass::SiUser).expect("standard forced signal");
    }
    fn force_frame_failure(
        &mut self,
        tid: u32,
        failed: Option<Signal>,
        reason: crate::linux::signal::frame::FailureReason,
    ) {
        if self.ending() {
            return;
        }
        self.threads.get_mut(&tid).expect("current signal tid").frame_failure =
            Some(crate::linux::signal::frame::FrameFailure { signal: failed.unwrap_or(Signal::SEGV), reason });
        self.force(tid, SigInfo::new(Signal::SEGV, general::SI_KERNEL as i32), failed == Some(Signal::SEGV), None);
    }
    fn take_frame_failure(&mut self, tid: u32) -> Option<crate::linux::signal::frame::FrameFailure> {
        self.threads.get_mut(&tid).expect("current signal tid").frame_failure.take()
    }
    fn next(&mut self, tid: u32) -> Delivery {
        loop {
            let t = self.threads.get_mut(&tid).expect("current signal tid");
            match self.group_exit {
                Some(GroupEnd::Exit) => return Delivery::Fatal(Signal::KILL, None),
                Some(GroupEnd::Signal(signal)) => return Delivery::Fatal(signal, t.take_fault(signal)),
                None => {}
            }
            if self.exec_waiter.is_some_and(|caller| caller != tid) {
                return Delivery::Fatal(Signal::KILL, None);
            }
            if self.group_stop.is_some() {
                return Delivery::Park;
            }
            let Some(info) = self.pending.dequeue_entry(t.slot, t.blocked).expect("current slot") else {
                sched::clear_current_sigpending();
                return Delivery::None;
            };
            let info = self.timers.delivered(info);
            let sig = info.signal();
            let action = self.actions[sig.index()];
            if action.handler == 0 && sig.default_action() == DefaultAction::Stop {
                t.stop_dequeued = true;
                return Delivery::Stop(sig);
            }
            if ignored(action, sig) || action.handler == 0 {
                if kills_by_default(action, sig) {
                    // The fatal dequeue is not the group decision. Like
                    // get_signal -> do_group_exit, the later locked recheck
                    // must allow a concurrent de_thread to win this gap.
                    return Delivery::Fatal(sig, t.take_fault(sig));
                }
                continue;
            }
            if action.flags & u64::from(general::SA_RESETHAND) != 0 {
                self.actions[sig.index()].handler = 0;
            }
            if sig == Signal::SEGV {
                t.frame_failure = None;
            }
            return Delivery::Handler(HandlerDelivery {
                info,
                action,
                blocked: t.blocked,
                saved_mask: t.saved_mask.unwrap_or(t.blocked),
                stack: t.alt_stack,
                fault: t.take_fault(sig),
            });
        }
    }

    /// Return the cancelled stop's signal and SIGCONT notification reason.
    /// Completion is authoritative even before its process-tree publication.
    pub(super) fn cancel_stop(&mut self, process: &Process) -> Option<(Signal, lumen_proc::WaitStatus)> {
        let cancelled = self.group_stop.take().map(|stop| {
            let status = if stop.remaining == 0 {
                lumen_proc::WaitStatus::Continued
            } else {
                lumen_proc::WaitStatus::Stopped(stop.signal)
            };
            (stop.signal, status)
        });
        for t in self.threads.values_mut() {
            t.stop_pending = false;
            t.stop_dequeued = false;
        }
        process.stopped.store(false, Ordering::SeqCst);
        process.stop_seq.fetch_add(1, Ordering::SeqCst);
        process.stop_wait.wake_all();
        cancelled
    }

    pub(super) fn start_stop(&mut self, tid: u32, sig: Signal, process: &Process) {
        if self.ending() || self.exec_waiter.is_some() || !self.thread(tid).is_some_and(|t| t.stop_dequeued) {
            return;
        }
        if self.group_stop.is_none() {
            self.stop_generation = self.stop_generation.checked_add(1).expect("stop generation overflow");
            self.group_stop =
                Some(GroupStop { generation: self.stop_generation, signal: sig, remaining: self.threads.len() });
            process.stopped.store(true, Ordering::SeqCst);
            for t in self.threads.values_mut() {
                t.stop_pending = true;
                t.notify(false);
            }
        }
    }

    pub(super) fn participate_stop(&mut self, tid: u32) -> Option<GroupStop> {
        let t = self.threads.get_mut(&tid).expect("live stop participant");
        if !core::mem::take(&mut t.stop_pending) {
            return None;
        }
        let stop = self.group_stop.as_mut().expect("pending stop generation");
        stop.remaining -= 1;
        (stop.remaining == 0).then_some(*stop)
    }

    /// A completion can publish only the signal of its still-live generation.
    pub(super) fn completed_stop_signal(&self, stop: GroupStop) -> Option<Signal> {
        self.group_stop.filter(|s| s.generation == stop.generation && s.remaining == 0).map(|s| s.signal)
    }
}

impl Process {
    /// Refuse exec when another group action has already won.
    pub fn check_exec(&self) -> Result<(), u32> {
        let state = self.signal.lock();
        if state.ending() || state.exec_waiter.is_some() {
            return Err(errno::EAGAIN);
        }
        Ok(())
    }

    /// Reset handled dispositions, the alternate stack, fault and frame-failure
    /// diagnostics, preserving non-timer pending signals and the blocked mask.
    /// POSIX timers and their queued notifications are retired; REAL survives.
    pub fn exec_signals(&self, tid: u32) {
        let mut state = self.signal.lock();
        state.timers.clear(self.pid(), false);
        state.pending.discard_timers();
        for action in &mut state.actions {
            let handler = if action.handler == SIG_IGN { SIG_IGN } else { 0 };
            *action = KernelSigaction { handler, ..KernelSigaction::default() };
        }
        let t = state.threads.get_mut(&tid).expect("exec's live tid");
        t.alt_stack = AltStack::DISABLED;
        t.fault = None;
        t.frame_failure = None;
        t.saved_mask = None;
        t.real_blocked = None;
    }
    /// Dequeue and choose an action in one signal hold, fatal state first.
    pub fn next_signal(&self, tid: u32) -> Delivery {
        debug_assert_eq!(tid, current_thread().tid());
        let mut state = self.signal.lock();
        let next = state.next(tid);
        if matches!(next, Delivery::Stop(Signal::STOP)) {
            state.start_stop(tid, Signal::STOP, self);
        }
        if state.ending() {
            let _ = state.cancel_stop(self);
        }
        next
    }

    /// Job-control stops recheck the dequeue mark after the orphan check;
    /// a racing continue clears it, preventing a late stop of the group.
    pub fn start_job_stop(&self, tid: u32, sig: Signal) {
        if sig != Signal::STOP && !PROCS.lock().orphaned(self) {
            #[cfg(feature = "ktest")]
            delay_window(&STOP_GAP_DELAY);
            self.signal.lock().start_stop(tid, sig, self);
        }
    }

    /// Participate once, publish a completed stop, then sleep killably.
    pub fn park_stopped(&self, tid: u32) {
        let (completed, seen) = {
            let mut state = self.signal.lock();
            (state.participate_stop(tid), self.stop_seq.load(Ordering::SeqCst))
        };
        #[cfg(feature = "ktest")]
        stop_publication_window(self.pid());
        if let Some(stop) = completed {
            PROCS.lock().complete_stop(self, stop);
        }
        let _ = self.stop_wait.wait_killable(None, || {
            !self.stopped.load(Ordering::SeqCst) || self.stop_seq.load(Ordering::SeqCst) != seen
        });
    }
    /// Force a synchronous signal on the current thread and activate its return hook.
    pub fn force_signal(&self, tid: u32, info: SigInfo, reset: bool, fault: Option<UserFault>) {
        debug_assert_eq!(tid, current_thread().tid());
        // sigpending gates the interrupt return hook; synchronous faults set it directly.
        sched::with_current(|t| t.set_sigpending());
        self.signal.lock().force(tid, info, reset, fault);
    }
}

#[cfg(feature = "ktest")]
pub(super) static STOP_GAP_DELAY: SpinLock<Option<Arc<TransitionDelay>>> = SpinLock::new(None);

#[cfg(feature = "ktest")]
struct StopPublicationPause {
    pid: u32,
    reached: VforkDone,
    release: VforkDone,
}

#[cfg(feature = "ktest")]
static STOP_PUBLICATION_PAUSE: SpinLock<Option<Arc<StopPublicationPause>>> = SpinLock::new(None);

#[cfg(feature = "ktest")]
fn stop_publication_window(pid: u32) {
    let pause = STOP_PUBLICATION_PAUSE.lock().as_ref().filter(|p| p.pid == pid).cloned();
    if let Some(pause) = pause {
        pause.reached.complete();
        pause.release.wait();
    }
}

impl UserThread {
    /// Save and replace the mask atomically for a signal-aware wait.
    pub fn install_temporary_mask(&self, mask: Blocked) {
        let mut state = self.process.signal.lock();
        let t = state.threads.get_mut(&self.tid()).expect("current signal tid");
        assert!(t.saved_mask.is_none(), "nested temporary signal mask");
        t.saved_mask = Some(t.blocked);
        state.set_blocked(self.tid(), mask);
        state.refresh_current(self.tid());
    }

    /// Keep an interrupted wait's mask until delivery; otherwise restore it.
    pub fn restore_saved_sigmask_unless(&self, interrupted: bool) {
        if interrupted {
            return;
        }
        let mut state = self.process.signal.lock();
        if let Some(mask) = state.threads.get_mut(&self.tid()).expect("current signal tid").saved_mask.take() {
            state.set_blocked(self.tid(), mask);
        }
    }

    /// Consume a wanted signal without delivering a handler, preserving the
    /// original mask and default-fatal dispositions throughout the wait.
    pub fn wait_signal(&self, wanted: SigSet, deadline: Option<u64>) -> Result<SigInfo, u32> {
        let tid = self.tid();
        {
            let mut state = self.process.signal.lock();
            let t = state.thread(tid).expect("current signal tid");
            let slot = t.slot;
            if let Some(info) = state.pending.dequeue_wanted_entry(slot, wanted).expect("live slot") {
                state.refresh_current(tid);
                return Ok(state.timers.delivered(info));
            }
            if deadline.is_some_and(|dl| crate::time::now_ns() >= dl) {
                return Err(errno::EAGAIN);
            }
            let t = state.threads.get_mut(&tid).expect("current signal tid");
            t.real_blocked = Some(t.blocked);
            let mask = Blocked::new(SigSet(t.blocked.set().0 & !wanted.0));
            state.set_blocked(tid, mask);
            state.refresh_current(tid);
        }
        let wake = sched::WaitQueue::new().wait_interruptible(deadline, || false);
        let mut state = self.process.signal.lock();
        let t = state.threads.get_mut(&tid).expect("current signal tid");
        let slot = t.slot;
        let mask = t.real_blocked.take().expect("signal wait mask");
        let info = state.pending.dequeue_wanted_entry(slot, wanted).expect("live slot");
        let info = info.map(|info| state.timers.delivered(info));
        state.set_blocked(tid, mask);
        // A consumed wanted signal must not interrupt the next blocking call.
        state.refresh_current(tid);
        info.ok_or(if wake == sched::Wake::TimedOut { errno::EAGAIN } else { errno::EINTR })
    }

    /// Queue SIGSEGV with the failed frame's diagnostic.
    pub fn force_frame_failure(&self, failed: Option<Signal>, reason: crate::linux::signal::frame::FailureReason) {
        sched::with_current(|t| t.set_sigpending());
        self.process.signal.lock().force_frame_failure(self.tid(), failed, reason);
    }
    pub fn take_frame_failure(&self) -> Option<crate::linux::signal::frame::FrameFailure> {
        self.process.signal.lock().take_frame_failure(self.tid())
    }
    pub fn alt_stack(&self) -> AltStack {
        self.process.signal.lock().thread(self.tid()).expect("current signal tid").alt_stack
    }
    pub fn configure_alt_stack(&self, rsp: u64, new: Option<(u64, u64, u32)>) -> Result<AltStack, StackError> {
        self.process.signal.lock().configure_stack(self.tid(), rsp, new)
    }
    pub fn install_signal_context(&self, mask: Blocked, stack: AltStack) {
        self.process.signal.lock().install_context(self.tid(), mask, stack);
    }
}

/// Lookup and generation are serialized against unlink and tid reuse.
#[cfg(feature = "ktest")]
pub fn send_thread(tgid: Option<u32>, tid: u32, info: Option<SigInfo>, sender: SenderClass) -> Result<(), u32> {
    send_thread_checked(tgid, tid, Ok(info), sender)
}
/// Defer signal validation errors until the target has been found.
pub fn send_thread_checked(
    tgid: Option<u32>,
    tid: u32,
    info: Result<Option<SigInfo>, u32>,
    sender: SenderClass,
) -> Result<(), u32> {
    PROCS.lock().send_thread(tgid, tid, info, sender)
}
pub fn send_process_checked(pid: i32, info: Result<Option<SigInfo>, u32>, sender: SenderClass) -> Result<(), u32> {
    let caller = current_process().pid();
    PROCS.lock().send_process(caller, pid, info, sender)
}

/// Blocked signals pending in the calling thread's private or shared queue.
/// Snapshot both queues and the mask under one signal-lock hold.
pub fn current_pending() -> SigSet {
    let me = current_thread();
    let state = me.process().signal.lock();
    let thread = state.threads.get(&me.tid()).expect("current signal tid");
    let pending = state.pending.pending(thread.slot).expect("current signal slot");
    SigSet(pending.0 & thread.blocked.set().0)
}

/// Queue a standard signal on the calling thread (`SIGPIPE` from `pipe_write`),
/// honouring `SIG_IGN` and the thread's blocked mask (Linux `send_sig`).
pub fn send_current_signal(sig: Signal) {
    let me = current_thread();
    debug_assert!(!sig.is_realtime());
    let mut bytes = [0; 128];
    bytes[8..12].copy_from_slice(&(general::SI_USER as i32).to_le_bytes());
    bytes[16..20].copy_from_slice(&me.process().pid().to_le_bytes());
    // Lumen currently has only uid 0, as sys_kill's sender_info does.
    let info = SigInfo::from_user(sig, bytes);
    me.process()
        .signal
        .lock()
        .send(Some(me.tid()), info, SenderClass::SiUser)
        .expect("current thread is linked and standard signals cannot exhaust the queue");
}

crate::ktest!(
    fn continue_preserves_unpublished_stop_notifications() {
        use crate::{kassert, kassert_eq};
        in_test_process(|_| {
            set_current_blocked(Blocked::new(SigSet::of(&[Signal::CHLD])));
            for completed in [true, false] {
                for suppress in [false, true] {
                    set_current_action(
                        Signal::CHLD,
                        KernelSigaction {
                            flags: if suppress { u64::from(general::SA_NOCLDSTOP) } else { 0 },
                            ..Default::default()
                        },
                    );
                    let after = Arc::new(VforkDone::new());
                    let finish = Arc::new(VforkDone::new());
                    let (resumed, end) = (Arc::clone(&after), Arc::clone(&finish));
                    let (pid, parked) = ktest_fork_child(move || {
                        let me = current_thread();
                        let _ = me.process().next_signal(me.tid());
                        me.process().park_stopped(me.tid());
                        resumed.complete();
                        end.wait();
                        record_exit(UserExit::Exited(0));
                        let _ = take_current();
                    })?;
                    let child = lookup(pid).ok_or("child")?;
                    let extra = if completed { None } else { Some(add_ktest_thread(&child)?) };
                    let pause =
                        Arc::new(StopPublicationPause { pid, reached: VforkDone::new(), release: VforkDone::new() });
                    *STOP_PUBLICATION_PAUSE.lock() = Some(Arc::clone(&pause));
                    send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::STOP, 0))), SenderClass::SiUser)
                        .map_err(|_| "stop child")?;
                    let handle = parked.start();
                    pause.reached.wait();
                    let parent = current_process();
                    let before = parent.child_seq.load(Ordering::SeqCst);
                    let remaining = child.signal.lock().group_stop.map(|s| s.remaining);
                    let sent =
                        send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::CONT, 0))), SenderClass::SiUser);
                    let seq = parent.child_seq.load(Ordering::SeqCst);
                    let request = wait4_request(pid as i32, general::WCONTINUED | general::WNOHANG).unwrap();
                    let report = wait_child(request);
                    let notification =
                        current_thread().wait_signal(SigSet::of(&[Signal::CHLD]), Some(crate::time::now_ns()));
                    // Let the delayed completion run, then repeat CONT. Neither
                    // may publish another event or change the consumed status.
                    pause.release.complete();
                    after.wait();
                    let repeated =
                        send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::CONT, 0))), SenderClass::SiUser);
                    let again = wait_child(request);
                    let seq_after = parent.child_seq.load(Ordering::SeqCst);
                    let duplicate =
                        current_thread().wait_signal(SigSet::of(&[Signal::CHLD]), Some(crate::time::now_ns()));
                    *STOP_PUBLICATION_PAUSE.lock() = None;
                    if let Some(extra) = extra {
                        remove_ktest_thread(&extra);
                    }
                    finish.complete();
                    let exit = ktest_reap(pid, handle)?;
                    // Consume exit's separate SIGCHLD before the next fixture.
                    let _ = current_thread().wait_signal(SigSet::of(&[Signal::CHLD]), Some(crate::time::now_ns()));
                    kassert_eq!(exit, 0);
                    kassert_eq!(remaining, Some(usize::from(!completed)));
                    kassert_eq!(sent, Ok(()));
                    kassert_eq!(seq, before + 1);
                    let report = report.map_err(|_| "continued wait")?;
                    kassert!(report.is_some());
                    kassert_eq!(report.unwrap().status().word(), 0xffff);
                    if suppress {
                        kassert!(matches!(notification, Err(errno::EAGAIN)));
                    } else {
                        let info = notification.map_err(|_| "SIGCHLD missing")?;
                        kassert_eq!(
                            i32::from_ne_bytes(info.bytes()[8..12].try_into().unwrap()),
                            if completed { 6 } else { 5 }
                        );
                        kassert_eq!(u32::from_ne_bytes(info.bytes()[16..20].try_into().unwrap()), pid);
                        kassert_eq!(
                            u32::from_ne_bytes(info.bytes()[24..28].try_into().unwrap()),
                            if completed { Signal::CONT.number() } else { Signal::STOP.number() }
                        );
                    }
                    kassert_eq!(repeated, Ok(()));
                    kassert!(matches!(again, Ok(None)));
                    kassert_eq!(seq_after, seq);
                    kassert!(matches!(duplicate, Err(errno::EAGAIN)));
                }
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn continue_then_stop_releases_the_previous_park() {
        use crate::{kassert, kassert_eq};
        in_test_process(|pid| {
            let process = current_process();
            let phase = Arc::new(AtomicU32::new(0));
            let child_phase = Arc::clone(&phase);
            let worker = spawn_test_thread(move |tid| {
                let me = current_process();
                let _ = sched::WaitQueue::new().wait_interruptible(None, || false);
                child_phase.store(1, Ordering::SeqCst);
                me.park_stopped(tid);
                child_phase.store(2, Ordering::SeqCst);
                if matches!(me.next_signal(tid), Delivery::Park) {
                    me.park_stopped(tid);
                }
            })?;
            worker.settle();
            let send = |sig| send_process_checked(pid as i32, Ok(Some(SigInfo::new(sig, 0))), SenderClass::SiUser);
            send(Signal::STOP).map_err(|_| "stop")?;
            let first = process.next_signal(pid);
            let _ = process.signal.lock().participate_stop(pid);
            while phase.load(Ordering::SeqCst) == 0 || !worker.blocked() {
                sched::yield_now();
            }
            // Same-CPU worker cannot observe the intermediate unstopped state.
            let on = crate::arch::irq::disable();
            let continued = send(Signal::CONT);
            let stopped = send(Signal::STOP);
            let second = process.next_signal(pid);
            let _ = process.signal.lock().participate_stop(pid);
            crate::arch::irq::restore(on);
            let blocked = worker.settle();
            let observed = phase.load(Ordering::SeqCst);
            let remaining = process.signal.lock().group_stop.map(|s| s.remaining);
            // Rescue either implementation before asserting so no task leaks.
            send(Signal::CONT).map_err(|_| "final continue")?;
            worker.join()?;
            kassert_eq!(continued, Ok(()));
            kassert_eq!(stopped, Ok(()));
            kassert!(matches!(first, Delivery::Stop(Signal::STOP)));
            kassert!(matches!(second, Delivery::Stop(Signal::STOP)));
            kassert!(blocked);
            kassert_eq!(observed, 2);
            kassert_eq!(remaining, Some(0));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn stop_generations_count_once_and_continue_cancels_dequeue() {
        use crate::{kassert, kassert_eq};
        in_test_process(|pid| {
            let me = current_thread();
            let process = me.process();
            let extra = add_ktest_thread(process)?;
            let sent = send_thread(None, me.tid(), Some(SigInfo::new(Signal::STOP, 0)), SenderClass::SiUser);
            let first = process.next_signal(me.tid());
            let counted = process.signal.lock().participate_stop(me.tid());
            let twice = process.signal.lock().participate_stop(me.tid());
            // A departing participant completes the stop through the real
            // unlink path; no thread may be counted twice.
            remove_ktest_thread(&extra);
            let stopped = PROCS.lock().ps_rows().into_iter().find(|p| p.pid == pid).unwrap().state;
            send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::CONT, 0))), SenderClass::SiUser)
                .map_err(|_| "continue")?;
            kassert_eq!(sent, Ok(()));
            kassert!(matches!(first, Delivery::Stop(Signal::STOP)));
            kassert!(counted.is_none() && twice.is_none());
            kassert!(matches!(stopped, ProcState::Stopped { signal: Signal::STOP, .. }));
            kassert!(!process.stopped.load(Ordering::SeqCst));
            kassert!(process.signal.lock().group_stop.is_none());
            // Force the gap between a job-stop dequeue and its orphan check.
            send_thread(None, me.tid(), Some(SigInfo::new(Signal::TSTP, 0)), SenderClass::SiUser)
                .map_err(|_| "job stop")?;
            kassert!(matches!(process.next_signal(me.tid()), Delivery::Stop(Signal::TSTP)));
            send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::CONT, 0))), SenderClass::SiUser)
                .map_err(|_| "racing continue")?;
            process.signal.lock().start_stop(me.tid(), Signal::TSTP, process);
            kassert!(!process.stopped.load(Ordering::SeqCst));
            kassert!(matches!(process.next_signal(me.tid()), Delivery::None));
            // A completion from an older generation cannot publish after cont.
            let stale = GroupStop { generation: 1, signal: Signal::STOP, remaining: 0 };
            PROCS.lock().complete_stop(process, stale);
            let now = PROCS.lock().ps_rows().into_iter().find(|p| p.pid == pid).unwrap().state;
            kassert!(matches!(now, ProcState::Continued { .. }));
            // Delay a real G1 completion across CONT and a completed but
            // unpublished G2. Merely testing with no live stop misses this race.
            send_thread(None, pid, Some(SigInfo::new(Signal::STOP, 0)), SenderClass::SiUser).map_err(|_| "G1 send")?;
            kassert!(matches!(process.next_signal(pid), Delivery::Stop(Signal::STOP)));
            let g1 = process.signal.lock().participate_stop(pid).ok_or("G1 completion")?;
            send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::CONT, 0))), SenderClass::SiUser)
                .map_err(|_| "G1 continue")?;
            send_thread(None, pid, Some(SigInfo::new(Signal::TSTP, 0)), SenderClass::SiUser).map_err(|_| "G2 send")?;
            kassert!(matches!(process.next_signal(pid), Delivery::Stop(Signal::TSTP)));
            // The fixture is orphaned; start under siglock to isolate the
            // completion interleaving from the separate orphan-group policy.
            let g2 = {
                let mut state = process.signal.lock();
                state.start_stop(pid, Signal::TSTP, process);
                state.participate_stop(pid).ok_or("G2 completion")?
            };
            let (after_stale, after_live) = {
                let mut table = PROCS.lock();
                table.complete_stop(process, g1);
                let after_stale = table.ps_rows().into_iter().find(|p| p.pid == pid).unwrap().state;
                table.complete_stop(process, g2);
                let after_live = table.ps_rows().into_iter().find(|p| p.pid == pid).unwrap().state;
                (after_stale, after_live)
            };
            send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::CONT, 0))), SenderClass::SiUser)
                .map_err(|_| "G2 cleanup")?;
            kassert!(matches!(after_stale, ProcState::Continued { .. }));
            kassert!(matches!(after_live, ProcState::Stopped { signal: Signal::TSTP, .. }));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn directed_fatal_signal_stays_pending_while_stopped() {
        use crate::{kassert, kassert_eq};
        let mut state = one_thread_state();
        state.group_stop = Some(GroupStop { generation: 1, signal: Signal::STOP, remaining: 0 });
        state.send(Some(9), SigInfo::new(Signal::TERM, 0), SenderClass::Other).map_err(|_| "send TERM")?;
        kassert!(!state.ending());
        let slot = state.thread(9).unwrap().slot;
        kassert!(state.pending.pending(slot).unwrap().contains(Signal::TERM));
        kassert!(matches!(state.next(9), Delivery::Park));
        state.group_stop = None;
        kassert!(matches!(state.next(9), Delivery::Fatal(Signal::TERM, None)));
        kassert_eq!(state.fatal_signal(), None); // exit step 1 records the decision
        let mut state = one_thread_state();
        state.group_stop = Some(GroupStop { generation: 1, signal: Signal::STOP, remaining: 0 });
        state.send(Some(9), SigInfo::new(Signal::KILL, 0), SenderClass::Other).map_err(|_| "send KILL")?;
        kassert!(matches!(state.next(9), Delivery::Fatal(Signal::KILL, None)));
        Ok(())
    }
);

crate::ktest!(
    fn signal_send_refuses_unlinked_tid_and_discards_ignored_pending() {
        use crate::{kassert, kassert_eq};
        in_test_process(|_| {
            let me = current_thread();
            let extra = add_ktest_thread(me.process())?;
            let tid = extra.tid();
            remove_ktest_thread(&extra);
            kassert_eq!(send_thread(None, tid, None, SenderClass::Other), Err(errno::ESRCH));
            let info = SigInfo::new(Signal::USR1, 0);
            kassert_eq!(
                send_thread(Some(me.process().pid() + 1), me.tid(), Some(info), SenderClass::Other),
                Err(errno::ESRCH)
            );
            me.set_blocked(Blocked::new(SigSet::of(&[Signal::USR1])));
            kassert_eq!(send_thread(None, me.tid(), Some(info), SenderClass::Other), Ok(()));
            set_current_action(Signal::USR1, KernelSigaction { handler: 1, flags: u64::MAX, ..Default::default() });
            kassert_eq!(current_action(Signal::USR1).flags & 0x100, 0);
            me.set_blocked(Blocked::EMPTY);
            kassert!(matches!(me.process().next_signal(me.tid()), Delivery::None));
            // An unblocked ignored send is discarded at generation, not
            // merely skipped later by dequeue under the old disposition.
            send_thread(None, me.tid(), Some(info), SenderClass::Other).map_err(|_| "send ignored")?;
            set_current_action(Signal::USR1, KernelSigaction { handler: 0x400000, ..Default::default() });
            kassert!(matches!(me.process().next_signal(me.tid()), Delivery::None));
            set_current_action(Signal::USR1, KernelSigaction { handler: 1, ..Default::default() });
            // Ignored, blocked signals stay pending until the disposition is
            // changed; once a handler is installed they become deliverable.
            me.set_blocked(Blocked::new(SigSet::of(&[Signal::USR1])));
            send_thread(None, me.tid(), Some(info), SenderClass::Other).map_err(|_| "send blocked ignored")?;
            set_current_action(Signal::USR1, KernelSigaction { handler: 0x400000, ..Default::default() });
            me.set_blocked(Blocked::EMPTY);
            kassert!(matches!(me.process().next_signal(me.tid()), Delivery::Handler(_)));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn fatal_signal_precedes_handlers_and_keeps_first_status() {
        use crate::{kassert, kassert_eq};
        let fault = UserFault::Trap { vector: 14, error_code: 4, rip: 0x400000, cr2: 0x12345000 };
        let mut state = one_thread_state();
        state.set_action(Signal::USR1, KernelSigaction { handler: 0x400000, ..Default::default() });
        state.send(Some(9), SigInfo::new(Signal::USR1, 0), SenderClass::Other).map_err(|_| "send handler")?;
        state.force(9, SigInfo::fault(Signal::SEGV, 1, 0x12345000), false, Some(fault));
        state.force(9, SigInfo::fault(Signal::ILL, 2, 0x400000), false, None);
        let Delivery::Fatal(sig, diagnostic) = state.next(9) else { return Err("handler preceded fatal signal") };
        kassert_eq!(sig, Signal::SEGV);
        kassert!(matches!(diagnostic, Some(UserFault::Trap { vector: 14, .. })));

        let mut state = one_thread_state();
        state.set_action(Signal::SEGV, KernelSigaction { handler: 0x400000, ..Default::default() });
        state.force(9, SigInfo::fault(Signal::SEGV, 1, 0x12345000), false, Some(fault));
        state.send(Some(9), SigInfo::new(Signal::KILL, 0), SenderClass::Other).map_err(|_| "send kill")?;
        let Delivery::Fatal(sig, diagnostic) = state.next(9) else { return Err("kill did not win") };
        kassert_eq!(sig, Signal::KILL);
        // A superseded fault must not replace the kill's exit status.
        kassert!(diagnostic.is_none());
        let mut state = one_thread_state();
        for sig in [Signal::TRAP, Signal::SEGV] {
            state.set_action(sig, KernelSigaction { handler: 0x400000, ..Default::default() });
        }
        state.force(9, SigInfo::fault(Signal::TRAP, 2, 0x400100), false, None);
        state.force(9, SigInfo::fault(Signal::SEGV, 1, 0x12345000), false, Some(fault));
        let Delivery::Handler(handler) = state.next(9) else { return Err("missing trap handler") };
        kassert_eq!(handler.info.signal(), Signal::TRAP);
        kassert!(handler.fault.is_none());
        let Delivery::Handler(handler) = state.next(9) else { return Err("missing segv handler") };
        kassert_eq!(handler.fault, Some(fault));
        Ok(())
    }
);

crate::ktest!(
    fn forcing_without_a_fault_preserves_another_signal_diagnostic() {
        use crate::{kassert, kassert_eq};
        let mut state = one_thread_state();
        for sig in [Signal::TRAP, Signal::SEGV] {
            state.set_action(sig, KernelSigaction { handler: 0x400000, ..Default::default() });
        }
        let fault = UserFault::Trap { vector: 1, error_code: 0, rip: 0x400100, cr2: 0 };
        state.force(9, SigInfo::fault(Signal::TRAP, 2, 0x400100), false, Some(fault));
        state.force(9, SigInfo::new(Signal::SEGV, general::SI_KERNEL as i32), false, None);
        let Delivery::Handler(handler) = state.next(9) else { return Err("trap handler missing") };
        kassert_eq!(handler.fault, Some(fault));
        kassert!(matches!(state.next(9), Delivery::Handler(_)));
        Ok(())
    }
);

#[cfg(feature = "ktest")]
fn one_thread_state() -> SignalState {
    let mut state = SignalState::new();
    let owner = UserThread::unbound_for_ktest(Arc::clone(&lookup(INIT_PID).expect("init")));
    state.add_thread(9, &owner, ThreadInit::ROOT);
    state
}

crate::ktest!(
    fn exec_signal_reset_preserves_pending_and_refuses_threads() {
        use crate::{kassert, kassert_eq};
        in_test_process(|_| {
            let me = current_thread();
            let extra = add_ktest_thread(me.process())?;
            let refused = me.process().check_exec();
            remove_ktest_thread(&extra);
            kassert_eq!(refused, Ok(()));
            kassert_eq!(me.process().check_exec(), Ok(()));
            let mask = Blocked::new(SigSet::of(&[Signal::USR1]));
            me.set_blocked(mask);
            set_current_action(Signal::USR1, KernelSigaction { handler: 0x400000, ..Default::default() });
            set_current_action(
                Signal::USR2,
                KernelSigaction {
                    handler: SIG_IGN,
                    flags: u64::from(general::SA_RESTART),
                    mask: 1234,
                    restorer: 0x401000,
                },
            );
            send_thread(None, me.tid(), Some(SigInfo::new(Signal::USR1, 0)), SenderClass::Other).map_err(|_| "send")?;
            send_process_checked(
                me.process().pid() as i32,
                Ok(Some(SigInfo::new(Signal::USR1, 0))),
                SenderClass::Other,
            )
            .map_err(|_| "send shared")?;
            me.process().exec_signals(me.tid());
            kassert_eq!(me.blocked(), mask);
            kassert_eq!(current_action(Signal::USR1), KernelSigaction::default());
            kassert_eq!(current_action(Signal::USR2), KernelSigaction { handler: SIG_IGN, ..Default::default() });
            // Install a handler again before unblocking, so delivery proves the
            // reset preserved the queued signal without terminating this test.
            set_current_action(Signal::USR1, KernelSigaction { handler: 0x500000, ..Default::default() });
            me.set_blocked(Blocked::EMPTY);
            // Both the private and shared pending entries survive.
            kassert!(matches!(me.process().next_signal(me.tid()), Delivery::Handler(_)));
            kassert!(matches!(me.process().next_signal(me.tid()), Delivery::Handler(_)));
            me.process().record_exit(UserExit::Exited(0));
            kassert_eq!(me.process().check_exec(), Err(errno::EAGAIN));
            Ok(())
        })?
    }
);
