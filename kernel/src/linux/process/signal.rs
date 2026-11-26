//! Signal disposition and thread-mask ownership; delivery is C5.

use super::*;

/// A process's signal dispositions and its threads' masks. M8's delivery
/// (pending signals, stops, the alternate stack) adds to it.
pub struct SignalState {
    /// By signal number - 1.
    actions: [KernelSigaction; 64],
    /// By tid.
    threads: BTreeMap<u32, ThreadSignals>,
}

impl SignalState {
    pub(super) fn new() -> Self {
        Self { actions: [KernelSigaction::default(); 64], threads: BTreeMap::new() }
    }

    pub(super) fn fork_copy(&self) -> Self {
        Self { actions: self.actions, threads: BTreeMap::new() }
    }

    /// `signal`'s disposition.
    pub fn action(&self, signal: Signal) -> KernelSigaction {
        self.actions[signal.index()]
    }

    /// Install `action` for `signal`, its mask stripped of `SIGKILL` and
    /// `SIGSTOP`: the action it replaces, or `None` (nothing changed) for
    /// `SIGKILL` or `SIGSTOP`, whose dispositions never change.
    pub fn set_action(&mut self, signal: Signal, mut action: KernelSigaction) -> Option<KernelSigaction> {
        if signal.is_unblockable() {
            return None;
        }
        action.mask = SigSet(action.mask).unblockable_stripped().0;
        Some(core::mem::replace(&mut self.actions[signal.index()], action))
    }

    /// Add thread `tid`'s signal state: panics if it already has one.
    pub(super) fn add_thread(&mut self, tid: u32, signals: ThreadSignals) {
        let displaced = self.threads.insert(tid, signals);
        assert!(displaced.is_none(), "tid {tid} already had signal state");
    }

    /// Remove thread `tid`'s signal state: panics if it has none.
    pub(super) fn remove_thread(&mut self, tid: u32) {
        let removed = self.threads.remove(&tid);
        assert!(removed.is_some(), "thread {tid} had no signal state");
    }

    /// Whether thread `tid` has signal state here.
    #[cfg(feature = "ktest")]
    pub(super) fn has_thread(&self, tid: u32) -> bool {
        self.threads.contains_key(&tid)
    }

    /// The tids with signal state here, ascending.
    #[cfg(feature = "ktest")]
    pub(super) fn tids(&self) -> impl Iterator<Item = u32> + '_ {
        self.threads.keys().copied()
    }

    /// The signals thread `tid` blocks: panics if it isn't one of this
    /// process's live threads.
    pub(super) fn blocked(&self, tid: u32) -> Blocked {
        self.threads.get(&tid).unwrap_or_else(|| panic!("thread {tid} has no signal state")).blocked
    }

    /// Make `blocked` the signals thread `tid` blocks: panics if it isn't
    /// one of this process's live threads.
    pub(super) fn set_blocked(&mut self, tid: u32, blocked: Blocked) {
        self.threads.get_mut(&tid).unwrap_or_else(|| panic!("thread {tid} has no signal state")).blocked = blocked;
    }
}

/// One thread's signal state.
pub struct ThreadSignals {
    /// The signals it blocks (`rt_sigprocmask`): never `SIGKILL` or
    /// `SIGSTOP`.
    blocked: Blocked,
}

impl ThreadSignals {
    pub(super) fn new(blocked: Blocked) -> Self {
        Self { blocked }
    }
}
