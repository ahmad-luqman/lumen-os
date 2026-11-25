//! Process-tree ownership and atomic identity/lifetime transitions.

use super::*;

/// Every process by pid and every user thread by tid, with the process
/// tree (`PROCS`). Its methods are the only writers of its fields.
pub(super) struct ProcTable {
    tree: ProcTree,
    /// Live processes and zombies, by pid.
    procs: BTreeMap<u32, Arc<Process>>,
    /// Live user threads, by tid.
    threads: BTreeMap<u32, Arc<UserThread>>,
}

impl ProcTable {
    pub(super) const fn new(max_pid: u32) -> Self {
        Self { tree: ProcTree::new(max_pid), procs: BTreeMap::new(), threads: BTreeMap::new() }
    }
    pub(super) fn parent(&self, pid: u32) -> u32 {
        self.tree.parent(pid).expect("live process in tree").unwrap_or(0)
    }
    pub(super) fn lookup(&self, pid: u32) -> Option<Arc<Process>> {
        self.procs.get(&pid).cloned()
    }
    pub(super) fn lookup_thread(&self, tid: u32) -> Option<Arc<UserThread>> {
        self.threads.get(&tid).cloned()
    }
    pub(super) fn active_pids(&self) -> Vec<u32> {
        self.procs.keys().copied().filter(|&pid| pid != INIT_PID).collect()
    }
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    pub(super) fn has_extra_threads(&self, pid: u32) -> bool {
        !self.tree.threads(pid).is_empty()
    }
    #[cfg(feature = "ktest")]
    pub(super) fn is_busy(&self, tid: u32) -> bool {
        self.tree.is_busy(tid)
    }
    pub(super) fn wait4_request(&self, caller: u32, pid: i32, options: u32) -> Result<lumen_proc::WaitRequest, u32> {
        self.tree.wait4_request(caller, pid, options).map_err(tree_errno)
    }
    pub(super) fn waitid_request(
        &self,
        caller: u32,
        idtype: i32,
        id: i32,
        options: u32,
    ) -> Result<lumen_proc::WaitRequest, u32> {
        self.tree.waitid_request(caller, idtype, id, options).map_err(tree_errno)
    }
    pub(super) fn wait(
        &mut self,
        parent: &Process,
        request: lumen_proc::WaitRequest,
    ) -> Result<(lumen_proc::WaitReady, Option<Arc<Process>>), u32> {
        let ready = self.tree.wait(request).map_err(tree_errno)?;
        let gone = match &ready {
            lumen_proc::WaitReady::Reported(lumen_proc::Report::Reaped { pid, .. }) => {
                let gone = self.procs.remove(pid).expect("reaped process in PROCS");
                parent.child_changed();
                Some(gone)
            }
            _ => None,
        };
        Ok((ready, gone))
    }

    /// Link `init` as pid 1 (`ProcTree::spawn_init`), setting its pid, or
    /// the tree's refusal (nothing changed).
    pub(super) fn link_init(&mut self, init: &Arc<Process>) -> Result<(), lumen_proc::SpawnInitError> {
        self.tree.spawn_init(RusageSlot::new(init.usage_slot))?;
        init.set_pid(INIT_PID);
        self.insert_process(INIT_PID, init);
        Ok(())
    }

    /// Link `thread` and its process as a new root (`spawn_root`, no
    /// parent): the pid, which is the leader's tid too, or `None` (nothing
    /// changed) when no pid is free.
    pub(super) fn link_root(&mut self, thread: &Arc<UserThread>) -> Option<u32> {
        let process = thread.process();
        let pid = self.tree.spawn_root(RusageSlot::new(process.usage_slot)).ok()?;
        process.set_pid(pid);
        self.insert_thread(pid, thread);
        self.insert_process(pid, process);
        Some(pid)
    }

    pub(super) fn link_child(&mut self, parent: &UserThread, thread: &Arc<UserThread>) -> Result<u32, u32> {
        let signal = parent.process().signal.lock();
        if parent.process().exit.lock().is_some() || sched::with_current(|t| t.fatal_pending()).unwrap_or(false) {
            return Err(errno::EINTR);
        }
        if sched::current_sigpending() {
            return Err(lumen_signal::restart::RestartCode::NoIntr as u32);
        }
        let child = thread.process();
        let pid = self.tree.fork(parent.process().pid(), RusageSlot::new(child.usage_slot)).map_err(tree_errno)?;
        child.set_pid(pid);
        child.inherit_signals(&signal);
        let blocked = signal.blocked(parent.tid());
        self.insert_thread(pid, thread);
        thread.set_blocked(blocked);
        self.insert_process(pid, child);
        Ok(pid)
    }

    /// Link `thread`, a non-leader thread of its (linked) process, with a
    /// tid from the tree (`ProcTree::add_thread`): the tid, or the tree's
    /// refusal (nothing changed).
    #[cfg(feature = "ktest")] // M8's `clone(CLONE_THREAD)` (C3) next
    pub(super) fn link_thread(&mut self, thread: &Arc<UserThread>) -> Result<u32, lumen_proc::Error> {
        let pid = thread.process().pid();
        let tid = self.tree.add_thread(pid)?;
        self.insert_thread(tid, thread);
        Ok(tid)
    }

    /// Put `process` in `procs` as `pid`: panics if the pid is taken.
    pub(super) fn insert_process(&mut self, pid: u32, process: &Arc<Process>) {
        let displaced = self.procs.insert(pid, Arc::clone(process));
        assert!(displaced.is_none(), "pid {pid} handed out while in PROCS");
    }

    /// Give `thread` tid `tid` and put it in `threads` and in its process's
    /// signal state (`Process::signal` under `PROCS`, section 9's order).
    pub(super) fn insert_thread(&mut self, tid: u32, thread: &Arc<UserThread>) {
        thread.set_tid(tid);
        thread.process().signal.lock().add_thread(tid, ThreadSignals::new(Blocked::EMPTY));
        let displaced = self.threads.insert(tid, Arc::clone(thread));
        assert!(displaced.is_none(), "tid {tid} handed out while in PROCS");
    }

    /// Unlink `thread`: out of `threads`, its process's signal state and,
    /// for a non-leader, the tree. Returns `PROCS`' reference, for the
    /// caller to drop with `PROCS` released. Panics if it isn't linked.
    pub(super) fn unlink_thread(&mut self, thread: &Arc<UserThread>) -> Arc<UserThread> {
        let (pid, tid) = (thread.process().pid(), thread.tid());
        if tid != pid
            && let Err(e) = self.tree.remove_thread(pid, tid)
        {
            panic!("thread {tid} of {pid} unlinked: {e}");
        }
        thread.process().signal.lock().remove_thread(tid);
        let gone = self.threads.remove(&tid).unwrap_or_else(|| panic!("thread {tid} wasn't in PROCS"));
        assert!(Arc::ptr_eq(&gone, thread), "tid {tid} in PROCS is another thread");
        gone
    }

    /// Whether `pid` has no thread left in `threads` or the tree.
    pub(super) fn no_thread_left(&self, pid: u32) -> bool {
        self.tree.threads(pid).is_empty() && !self.threads.contains_key(&pid)
    }

    /// `pid`, in `procs` with no thread left (asserted), ends with `status`
    /// in the tree, its children going to pid 1, and every action B1 can
    /// reach carried out: the processes that left `PROCS` (a root, a child
    /// of an autoreaping parent, and their released zombies), for the
    /// caller to drop with `PROCS` released. A process whose zombie waits
    /// for its parent's wait (C1) stays. `NoMem` (nothing changed) if the
    /// heap refuses the tree's reservations or room for that list.
    pub(super) fn exit_in_tree(&mut self, pid: u32, status: ExitStatus) -> Result<Vec<Arc<Process>>, ExitError> {
        assert!(self.procs.contains_key(&pid), "pid {pid} exits while not in PROCS");
        assert!(self.no_thread_left(pid), "pid {pid} exits with a thread still linked");
        // At most itself and each zombie child leave.
        let mut gone = Vec::new();
        gone.try_reserve(self.tree.children(pid).len() + 1).map_err(|_| ExitError::NoMem)?;
        let Self { tree, procs, .. } = self;
        let autoreap = |parent: u32| procs.get(&parent).is_some_and(|p| p.autoreaps());
        let outcome = tree.exit(pid, status, Some(INIT_PID), autoreap)?;
        for action in outcome {
            match action {
                ExitAction::Released { child, .. } => {
                    gone.push(procs.remove(&child).expect("released process"));
                }
                ExitAction::Notify { parent, child, released, .. } => {
                    procs.get(&parent).expect("live parent").child_changed();
                    if released.is_some() {
                        gone.push(procs.remove(&child).expect("released zombie"));
                    }
                    // SIGCHLD delivery is C6; the child-event wait is live now.
                }
                // Nothing to signal before M8's signal delivery.
                ExitAction::HangupThenContinue { .. } => {}
            }
        }
        Ok(gone)
    }

    /// Check that the table agrees with itself and its tree, both ways
    /// (ktests): each thread's tid is its key and its process is `procs`'
    /// for its pid; each process's pid is its key, the threads mapping to
    /// it are exactly its live leader (but pid 1's: none) and the tree's
    /// threads of it, and its signal state holds exactly those tids;
    /// `procs` holds every process in the tree; and the tree's own
    /// invariants.
    #[cfg(feature = "ktest")]
    pub(super) fn check(&self) -> Result<(), &'static str> {
        for (&tid, thread) in &self.threads {
            if thread.tid() != tid {
                return Err("a thread's tid isn't its key");
            }
            let pid = thread.process().pid();
            if !self.procs.get(&pid).is_some_and(|p| Arc::ptr_eq(p, thread.process())) {
                return Err("a thread's process isn't PROCS' process for its pid");
            }
        }
        for (&pid, process) in &self.procs {
            if process.pid() != pid {
                return Err("a process's pid isn't its key");
            }
            let mut tids: Vec<u32> = self.tree.threads(pid).to_vec();
            if let Some(leader) = self.threads.get(&pid) {
                if !Arc::ptr_eq(leader.process(), process) {
                    return Err("a pid's leader thread is another process's");
                }
                tids.push(pid);
            }
            tids.sort_unstable();
            let mapped: Vec<u32> =
                self.threads.iter().filter(|(_, t)| Arc::ptr_eq(t.process(), process)).map(|(&tid, _)| tid).collect();
            if mapped != tids {
                return Err("a process's threads in PROCS aren't its leader and the tree's");
            }
            let signal = process.signal.lock();
            if !signal.tids().eq(tids.iter().copied()) || !tids.iter().all(|&t| signal.has_thread(t)) {
                return Err("a process's signal state isn't exactly its threads'");
            }
        }
        if self.procs.keys().any(|&pid| !self.tree.contains(pid)) || self.tree.len() != self.procs.len() {
            return Err("PROCS and the tree hold different processes");
        }
        self.tree.check_invariants();
        Ok(())
    }
    pub(super) fn ps_rows(&self) -> Vec<PsRow> {
        let t = self;
        let mut rows: BTreeMap<u32, PsRow> = BTreeMap::new();
        for &pid in t.procs.keys() {
            let ppid = t.tree.parent(pid).expect("a process in PROCS is in the tree").unwrap_or(0);
            let state = t.tree.state(pid).expect("a process in PROCS is in the tree");
            rows.insert(pid, PsRow { pid, ppid, threads: Vec::new(), state });
        }
        for (&tid, thread) in &t.threads {
            let pid = thread.process().pid();
            let row = rows.get_mut(&pid).expect("a thread's process is in PROCS");
            let entry = (tid, thread.kernel_thread.load(Ordering::Acquire));
            if tid == pid { row.threads.insert(0, entry) } else { row.threads.push(entry) }
        }
        rows.into_values().collect()
    }
}
