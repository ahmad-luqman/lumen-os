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
    pending: PendingSignals,
    /// Live signal state in ascending tid order.
    threads: BTreeMap<u32, ThreadSignals>,
    /// Cursor for process-directed target selection.
    preferred: Option<ThreadSlot>,
    /// First-wins end decision; Process::exit records its diagnostic.
    group_exit: Option<GroupEnd>,
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
    /// Validated alternate-stack configuration.
    alt_stack: AltStack,
    /// Last synchronous diagnostic; only its matching signal consumes it.
    fault: Option<UserFault>,
    /// Failed frame origin, discarded when SEGV reaches a handler.
    frame_failure: Option<crate::linux::signal::frame::FrameFailure>,
}

impl ThreadSignals {
    fn candidate(&self) -> ThreadCandidate {
        let thread = self.thread.lock().upgrade();
        ThreadCandidate {
            slot: self.slot,
            mask: self.blocked,
            state: match thread.as_ref().map(|t| t.state()) {
                Some(sched::State::Running) => ThreadState::Running,
                Some(sched::State::Dead) | None => ThreadState::Exiting,
                _ => ThreadState::Sleeping,
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
}

/// A handler selected under siglock, consumed after releasing the lock.
pub struct HandlerDelivery {
    pub info: SigInfo,
    pub action: KernelSigaction,
    pub blocked: Blocked,
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
            threads: BTreeMap::new(),
            preferred: None,
            group_exit: None,
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
        Self { actions: self.actions, ..Self::new() }
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
    /// Insert a linked tid; panics if the tid already exists or its queue cannot be allocated.
    pub(super) fn add_thread(&mut self, tid: u32, owner: &UserThread, init: ThreadInit) {
        let slot = self.pending.add_thread().expect("allocate thread signal queue");
        let signals = ThreadSignals {
            slot,
            thread: Arc::clone(&owner.kernel_handle),
            synthetic: owner.synthetic,
            blocked: init.blocked,
            alt_stack: init.alt_stack,
            fault: None,
            frame_failure: None,
        };
        assert!(self.threads.insert(tid, signals).is_none(), "duplicate signal tid");
        self.preferred.get_or_insert(slot);
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
            if self.pending.has_deliverable(t.slot, t.blocked).expect("live slot") {
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
    fn configure_stack(&mut self, tid: u32, rsp: u64, new: Option<(u64, u64, u32)>) -> Result<AltStack, StackError> {
        let t = self.threads.get_mut(&tid).expect("current signal tid");
        let old = t.alt_stack;
        if let Some((sp, size, flags)) = new {
            t.alt_stack = old.configure(rsp, sp, size, flags)?;
        }
        Ok(old)
    }
    fn install_context(&mut self, tid: u32, mask: Blocked, stack: AltStack) {
        self.threads.get_mut(&tid).expect("current signal tid").alt_stack = stack;
        self.set_blocked(tid, mask);
    }
    pub(super) fn send(&mut self, tid: Option<u32>, info: SigInfo, sender: SenderClass) -> Result<(), u32> {
        let sig = info.signal();
        let thread = tid.map(|tid| self.threads.get(&tid).ok_or(errno::ESRCH)).transpose()?;
        let directed = thread.map(|t| (t, t.blocked.contains(sig)));
        let target = thread.map_or(Target::Process, |t| Target::Thread(t.slot));
        let keep_ignored =
            directed.map_or_else(|| self.threads.values().all(|t| t.blocked.contains(sig)), |(_, blocked)| blocked);
        if self.ending() {
            return Ok(());
        }
        self.pending.prepare_signal(sig);
        if ignored(self.action(sig), sig) && !keep_ignored {
            return Ok(());
        }
        self.pending.enqueue(target, info, sender).map_err(|_| errno::EAGAIN)?;
        let chosen = match target {
            Target::Thread(_) => directed,
            Target::Process => {
                let candidates: Vec<_> = self.threads.values().map(ThreadSignals::candidate).collect();
                self.preferred.and_then(|p| select_target(sig, &candidates, p).expect("live cursor")).map(|slot| {
                    let t = self.threads.values().find(|t| t.slot == slot).expect("chosen slot");
                    (t, t.blocked.contains(sig))
                })
            }
        };
        if let Some((t, blocked)) = chosen {
            self.preferred = Some(t.slot);
            if !blocked {
                if kills_by_default(self.action(sig), sig) {
                    self.start_group_exit(sig);
                } else {
                    t.notify(false);
                }
            }
        }
        Ok(())
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
            let Some(info) = self.pending.dequeue(t.slot, t.blocked).expect("current slot") else {
                sched::clear_current_sigpending();
                return Delivery::None;
            };
            let sig = info.signal();
            let action = self.actions[sig.index()];
            if ignored(action, sig) || action.handler == 0 {
                if kills_by_default(action, sig) {
                    self.start_group_exit(sig);
                }
                // Group stop/continue delivery is supplied by C6.
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
                stack: t.alt_stack,
                fault: t.take_fault(sig),
            });
        }
    }
}

impl Process {
    /// Refuse multi-threaded exec until de_thread is available, and refuse group exit.
    pub fn check_exec(&self) -> Result<(), u32> {
        if PROCS.lock().has_extra_threads(self.pid()) || self.signal.lock().ending() {
            return Err(errno::EAGAIN);
        }
        Ok(())
    }

    /// Reset handled dispositions, the alternate stack, fault and frame-failure
    /// diagnostics, preserving pending queues and the blocked mask.
    pub fn exec_signals(&self, tid: u32) {
        let mut state = self.signal.lock();
        for action in &mut state.actions {
            let handler = if action.handler == SIG_IGN { SIG_IGN } else { 0 };
            *action = KernelSigaction { handler, ..KernelSigaction::default() };
        }
        let t = state.threads.get_mut(&tid).expect("exec's live tid");
        t.alt_stack = AltStack::DISABLED;
        t.fault = None;
        t.frame_failure = None;
    }
    /// Dequeue and choose an action in one signal hold, fatal state first.
    pub fn next_signal(&self, tid: u32) -> Delivery {
        debug_assert_eq!(tid, current_thread().tid());
        self.signal.lock().next(tid)
    }
    /// Force a synchronous signal on the current thread and activate its return hook.
    pub fn force_signal(&self, tid: u32, info: SigInfo, reset: bool, fault: Option<UserFault>) {
        debug_assert_eq!(tid, current_thread().tid());
        // sigpending gates the interrupt return hook; synchronous faults set it directly.
        sched::with_current(|t| t.set_sigpending());
        self.signal.lock().force(tid, info, reset, fault);
    }
}
impl UserThread {
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
            kassert_eq!(refused, Err(errno::EAGAIN));
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
