//! Process-tree ownership and atomic identity/lifetime transitions.

use super::signal::ThreadInit;
use super::*;
use lumen_signal::pending::{SenderClass, SigInfo};

/// Every process by pid and every user thread by tid, with the process
/// tree (`PROCS`). Its methods are the only writers of its fields.
pub(super) struct ProcTable {
    tree: ProcTree,
    /// Live processes and zombies, by pid.
    procs: BTreeMap<u32, Arc<Process>>,
    /// Live user threads, by tid.
    threads: BTreeMap<u32, Arc<UserThread>>,
}

pub(super) struct WaitResult {
    pub ready: lumen_proc::WaitReady,
    pub gone: Option<Arc<Process>>,
    pub ticks: [u64; 2],
}

impl ProcTable {
    pub(super) const fn new(max_pid: u32) -> Self {
        Self { tree: ProcTree::new(max_pid), procs: BTreeMap::new(), threads: BTreeMap::new() }
    }
    pub(super) fn live_process_count(&self) -> usize {
        self.procs.keys().filter(|&&pid| !matches!(self.tree.state(pid), Some(ProcState::Zombie(_)))).count()
    }

    pub(super) fn process_time(&self, pid: u32) -> [u64; 2] {
        let Some(process) = self.procs.get(&pid) else { return [0; 2] };
        let mut ticks = process.dead_ticks.each_ref().map(|t| t.load(Ordering::Relaxed));
        for thread in self.threads.values().filter(|t| t.process().pid() == pid) {
            if let Some(kernel) = thread.scheduler_thread() {
                let live = kernel.cpu_time();
                for i in 0..2 {
                    ticks[i] = ticks[i].saturating_add(live[i]);
                }
            }
        }
        ticks
    }

    pub(super) fn priority(&self, caller: &UserThread, which: i32, who: i32, nice: Option<i32>) -> Result<u64, u32> {
        if !(0..=2).contains(&which) {
            return Err(errno::EINVAL);
        }
        let group = if which == 1 && who == 0 {
            self.tree.getpgid(caller.process().pid(), 0).map_err(tree_errno)? as i32
        } else {
            who
        };
        let matches = |tid: u32, pid: u32| match which {
            0 => tid == if who == 0 { caller.tid() } else { who as u32 },
            1 => self.tree.getpgid(caller.process().pid(), pid as i32).ok() == Some(group as u32),
            _ => who == 0,
        };
        let mut best = None;
        for (&tid, thread) in &self.threads {
            if matches(tid, thread.process().pid())
                && let Some(kernel) = thread.scheduler_thread()
            {
                if let Some(nice) = nice {
                    kernel.set_nice(nice);
                }
                let value = (20 - i32::from(kernel.nice())) as u64;
                best = Some(best.unwrap_or(0).max(value));
            }
        }
        // Linux retains the leader task until reap, even after its last
        // runnable thread and scheduler state have gone away.
        for (&pid, process) in &self.procs {
            if !matches(pid, pid) {
                continue;
            }
            let mut retained = process.exited_leader_nice.lock();
            if let Some(value) = retained.as_mut() {
                if let Some(nice) = nice {
                    *value = nice.clamp(-20, 19) as i8;
                }
                best = Some(best.unwrap_or(0).max((20 - i32::from(*value)) as u64));
            }
        }
        best.ok_or(errno::ESRCH)
    }

    /// Record a successful single-threaded exec in the process tree.
    pub(super) fn exec(&mut self, pid: u32) {
        self.tree.exec(pid).expect("exec of a live process");
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
    pub(super) fn wait(&mut self, parent: &Process, request: lumen_proc::WaitRequest) -> Result<WaitResult, u32> {
        let ready = self.tree.wait(request).map_err(tree_errno)?;
        let ticks = match &ready {
            lumen_proc::WaitReady::Reported(report) => {
                let own = self.process_time(report.pid());
                let children = self.procs.get(&report.pid()).expect("reported process").children_time();
                core::array::from_fn(|i| own[i].saturating_add(children[i]))
            }
            _ => [0; 2],
        };
        let gone = match &ready {
            lumen_proc::WaitReady::Reported(lumen_proc::Report::Reaped { pid, .. }) => {
                let gone = self.procs.remove(pid).expect("reaped process in PROCS");
                for (counter, ticks) in parent.children_ticks.iter().zip(ticks) {
                    counter.fetch_add(ticks, Ordering::Relaxed);
                }
                parent.child_changed();
                Some(gone)
            }
            _ => None,
        };
        Ok(WaitResult { ready, gone, ticks })
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
        self.insert_thread(pid, thread, ThreadInit::ROOT);
        self.insert_process(pid, process);
        Some(pid)
    }

    /// Return the child's pid, holding the parent's signal throughout; refuse
    /// with EINTR (exiting/fatal), ERESTARTNOINTR (pending signal), or the
    /// tree's errno (EAGAIN when no pid is free).
    pub(super) fn link_child(
        &mut self,
        parent: &UserThread,
        thread: &Arc<UserThread>,
        request: ProcessClone,
    ) -> Result<u32, u32> {
        let signal = parent.process().signal.lock();
        if signal.ending() || sched::with_current(|t| t.fatal_pending()).unwrap_or(false) {
            return Err(errno::EINTR);
        }
        if sched::current_sigpending() {
            return Err(lumen_signal::restart::RestartCode::NoIntr as u32);
        }
        let child = thread.process();
        let pid = self.tree.fork(parent.process().pid(), RusageSlot::new(child.usage_slot)).map_err(tree_errno)?;
        child.set_pid(pid);
        // PROCS is held and the child is unreachable until insert_process:
        // no other thread can take its signal lock while we hold the parent's.
        child.inherit_signals(&signal);
        let init = signal.thread_init(parent.tid(), request);
        self.insert_thread(pid, thread, init);
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
        self.insert_thread(tid, thread, ThreadInit::ROOT);
        Ok(tid)
    }

    /// Put `process` in `procs` as `pid`: panics if the pid is taken.
    pub(super) fn insert_process(&mut self, pid: u32, process: &Arc<Process>) {
        let displaced = self.procs.insert(pid, Arc::clone(process));
        assert!(displaced.is_none(), "pid {pid} handed out while in PROCS");
    }

    /// Give `thread` tid `tid` and put it in `threads` and in its process's
    /// signal state (`Process::signal` under `PROCS`, section 9's order).
    pub(super) fn insert_thread(&mut self, tid: u32, thread: &Arc<UserThread>, init: ThreadInit) {
        thread.set_tid(tid);
        thread.process().signal.lock().add_thread(tid, thread, init);
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
        if let Some(kernel) = thread.scheduler_thread() {
            if tid == pid {
                *thread.process().exited_leader_nice.lock() = Some(kernel.nice());
            }
            for (counter, ticks) in thread.process().dead_ticks.iter().zip(kernel.cpu_time()) {
                counter.fetch_add(ticks, Ordering::Relaxed);
            }
        }
        let completed = {
            let mut state = thread.process().signal.lock();
            let completed = state.participate_stop(tid);
            state.remove_thread(tid);
            completed
        };
        if let Some(stop) = completed {
            self.complete_stop(thread.process(), stop);
        }
        let gone = self.threads.remove(&tid).unwrap_or_else(|| panic!("thread {tid} wasn't in PROCS"));
        assert!(Arc::ptr_eq(&gone, thread), "tid {tid} in PROCS is another thread");
        gone
    }

    /// Whether `pid` has no thread left in `threads` or the tree.
    pub(super) fn no_thread_left(&self, pid: u32) -> bool {
        self.tree.threads(pid).is_empty() && !self.threads.contains_key(&pid)
    }

    /// `pid`, in `procs` with no thread left (asserted), ends with `status`
    /// in the tree, its children going to pid 1. Notify publishes child_changed;
    /// the return value holds the processes that left `PROCS` (a root, a child
    /// of an autoreaping parent, and their released zombies), for the
    /// caller to drop with `PROCS` released. A process whose zombie waits
    /// for its parent's wait stays. `NoMem` (nothing changed) if the
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
                ExitAction::Notify { parent, child, status, released } => {
                    Self::notify_child(procs.get(&parent).expect("live parent"), child, status.into());
                    if released.is_some() {
                        gone.push(procs.remove(&child).expect("released zombie"));
                    }
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
    pub(super) fn send_thread(
        &mut self,
        tgid: Option<u32>,
        tid: u32,
        info: Result<Option<SigInfo>, u32>,
        sender: SenderClass,
    ) -> Result<(), u32> {
        let t = self.threads.get(&tid).cloned().ok_or(errno::ESRCH)?;
        if tgid.is_some_and(|pid| pid != t.process().pid()) {
            return Err(errno::ESRCH);
        }
        if let Some(info) = info? {
            self.send_to(t.process(), Some(tid), info, sender)?;
        }
        Ok(())
    }
    pub(super) fn send_process(
        &mut self,
        caller: u32,
        pid: i32,
        info: Result<Option<SigInfo>, u32>,
        sender: SenderClass,
    ) -> Result<(), u32> {
        if pid > 0 {
            let process = self.procs.get(&(pid as u32)).cloned().ok_or(errno::ESRCH)?;
            if let Some(info) = info? {
                self.send_to(&process, None, info, sender)?;
            }
            return Ok(());
        }
        let group = if pid == 0 {
            self.tree.getpgid(caller, 0).ok()
        } else if pid < -1 {
            Some(pid.unsigned_abs())
        } else {
            None
        };
        let mut targets = Vec::new();
        for (&id, process) in &self.procs {
            let matches = if pid == -1 {
                id != INIT_PID && id != caller
            } else {
                self.tree.getpgid(caller, id as i32).ok() == group
            };
            if !matches {
                continue;
            }
            targets.push(Arc::clone(process));
        }
        if targets.is_empty() {
            return Err(errno::ESRCH);
        }
        if let Some(info) = info? {
            for process in targets {
                self.send_to(&process, None, info, sender)?;
            }
        }
        Ok(())
    }

    /// The table hold pins identities through continue and parent notification.
    fn send_to(&mut self, process: &Process, tid: Option<u32>, info: SigInfo, sender: SenderClass) -> Result<(), u32> {
        // The kernel's reaper has no user thread and cannot be killed or stopped.
        if process.pid() == INIT_PID {
            return Ok(());
        }
        let mut state = process.signal.lock();
        let change = if info.signal() == Signal::CONT && !state.ending() {
            let cancelled = state.cancel_stop(process);
            if let Some((signal, _)) = cancelled {
                // The signal state may have completed (or begun) the stop
                // before its tree publication. Materialize it only inside
                // this PROCS hold, then replace it with the continued status.
                // Waiters cannot see that intermediate tree state; only the
                // CONT notification selected below is sent.
                let _ = self.tree.stop(process.pid(), signal).expect("live continuing process");
            }
            self.tree
                .cont(process.pid())
                .ok()
                .map(|change| (change, cancelled.map_or(lumen_proc::WaitStatus::Continued, |(_, status)| status)))
        } else {
            None
        };
        let result = state.send(tid, info, sender);
        if state.ending() {
            let _ = state.cancel_stop(process);
        }
        drop(state);
        if let Some((change, status)) = change {
            self.notify_change(process.pid(), change, status);
        }
        result
    }

    pub(super) fn start_job_stop(&self, process: &Process, tid: u32, sig: Signal) {
        let group = self.tree.pgid(process.pid()).expect("live stop group");
        if !self.tree.is_orphaned_pgrp(group) {
            process.signal.lock().start_stop(tid, sig, process);
        }
    }

    pub(super) fn complete_stop(&mut self, process: &Process, stop: super::signal::GroupStop) {
        let state = process.signal.lock();
        if state.ending() {
            return;
        }
        let Some(signal) = state.completed_stop_signal(stop) else { return };
        let change = self.tree.stop(process.pid(), signal).expect("live stopped process");
        drop(state);
        self.notify_change(process.pid(), change, lumen_proc::WaitStatus::Stopped(signal));
    }

    fn notify_change(&self, child: u32, change: lumen_proc::StateChange, status: lumen_proc::WaitStatus) {
        if let lumen_proc::StateChange::Changed { parent: Some(parent) } = change {
            Self::notify_child(self.procs.get(&parent).expect("live parent"), child, status);
        }
    }

    fn notify_child(parent: &Process, child: u32, status: lumen_proc::WaitStatus) {
        parent.child_changed();
        let mut state = parent.signal.lock();
        let action = state.action(Signal::CHLD);
        if action.handler == SIG_IGN
            || (matches!(status, lumen_proc::WaitStatus::Stopped(_) | lumen_proc::WaitStatus::Continued)
                && action.flags & u64::from(general::SA_NOCLDSTOP) != 0)
        {
            return;
        }
        let status = status.siginfo();
        let mut bytes = [0; 128];
        bytes[8..12].copy_from_slice(&status.code.to_ne_bytes());
        bytes[16..20].copy_from_slice(&child.to_ne_bytes());
        bytes[24..28].copy_from_slice(&status.status.to_ne_bytes());
        state.send(None, SigInfo::from_user(Signal::CHLD, bytes), SenderClass::SiUser).expect("standard SIGCHLD");
    }
}

crate::ktest!(
    fn reaped_usage_includes_descendants_once() {
        use crate::{kassert, kassert_eq};
        fn process() -> Arc<Process> {
            Arc::new(Process::new(
                None,
                None,
                None,
                SignalState::new(),
                ConsoleState::new(ConsoleIo::Capture, Vec::new()),
                false,
                None,
            ))
        }
        let mut table = ProcTable::new(PID_MAX);
        let init = process();
        table.link_init(&init).map_err(|_| "init")?;
        let parent = Arc::new(UserThread::unbound_for_ktest(process()));
        table.link_root(&parent).ok_or("parent")?;
        let child = Arc::new(UserThread::unbound_for_ktest(process()));
        let pid = table.link_child(&parent, &child, ProcessClone::FORK).map_err(|_| "child")?;
        kassert_eq!(table.live_process_count(), 3);
        for (counter, value) in child.process().dead_ticks.iter().zip([7, 11]) {
            counter.store(value, Ordering::Relaxed);
        }
        for (counter, value) in child.process().children_ticks.iter().zip([13, 17]) {
            counter.store(value, Ordering::Relaxed);
        }
        kassert_eq!(table.process_time(pid), [7, 11]);
        drop(table.unlink_thread(&child));
        drop(table.exit_in_tree(pid, ExitStatus::Exited(0)).map_err(|_| "exit")?);
        kassert_eq!(table.live_process_count(), 2);
        let peek = table
            .waitid_request(parent.process().pid(), 1, pid as i32, general::WEXITED | general::WNOWAIT)
            .map_err(|_| "peek request")?;
        for _ in 0..2 {
            let result = table.wait(parent.process(), peek).map_err(|_| "peek")?;
            kassert_eq!(result.ticks, [20, 28]);
            kassert!(result.gone.is_none());
            kassert_eq!(parent.process().children_time(), [0, 0]);
        }
        let reap = table.wait4_request(parent.process().pid(), pid as i32, 0).map_err(|_| "wait4 request")?;
        let result = table.wait(parent.process(), reap).map_err(|_| "reap")?;
        kassert_eq!(result.ticks, [20, 28]);
        kassert!(result.gone.is_some());
        kassert_eq!(parent.process().children_time(), [20, 28]);
        kassert_eq!(table.process_time(parent.process().pid()), [0, 0]);
        kassert!(matches!(table.wait(parent.process(), reap), Err(errno::ECHILD)));
        kassert_eq!(parent.process().children_time(), [20, 28]);
        Ok(())
    }
);
