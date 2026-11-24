//! The process tree: parents and children, groups and sessions, wait states,
//! fork, exit with reparenting, and wait.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use crate::wait::{WCONTINUED, WEXITED, WNOWAIT, WSTOPPED};
use crate::{Errno, ExitStatus, INIT_PID, PidAllocator, Signal, WaitOptions, WaitResult, WaitTarget};

/// A process's state as `wait` sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcState {
    /// Running (or sleeping): nothing to report.
    Running,
    /// Stopped by `signal`; `reported` once a wait has reported the stop.
    Stopped {
        /// The signal that stopped it.
        signal: Signal,
        /// Whether a wait has reported this stop.
        reported: bool,
    },
    /// Running again after `SIGCONT` ended a stop; `reported` once a wait
    /// with `WCONTINUED` has reported it.
    Continued {
        /// Whether a wait has reported the continue.
        reported: bool,
    },
    /// Ended, not yet reaped.
    Zombie(ExitStatus),
}

impl ProcState {
    const fn is_live(self) -> bool {
        !matches!(self, Self::Zombie(_))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Proc {
    /// `None` for a process the kernel started itself (init and other roots).
    pub(crate) parent: Option<u32>,
    /// In Linux's children-list order: forked children appended, a dead
    /// parent's children appended in their order.
    pub(crate) children: Vec<u32>,
    pub(crate) pgid: u32,
    pub(crate) sid: u32,
    pub(crate) state: ProcState,
    pub(crate) execed: bool,
    pub(crate) rusage_slot: u64,
}

/// What the kernel must do after [`ProcTree::exit`].
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ExitOutcome {
    /// The parent to send `SIGCHLD`, or `None` for a process without one
    /// (nobody can wait for it: the kernel [`ProcTree::release`]s it).
    pub parent: Option<u32>,
    /// The exiting process's children that were already zombies, now the
    /// reaper's, in children-list order: the reaper must be told of each
    /// (`SIGCHLD`), as it would have been had it been their parent then.
    pub adopted_zombies: Vec<u32>,
    /// Process groups this exit orphaned that have a stopped member: each
    /// gets `SIGHUP` then `SIGCONT` (Linux's `kill_orphaned_pgrp`). Each
    /// group is listed once: the groups of the reparented children (in
    /// children-list order) first, then the exiting process's own.
    pub orphaned_pgrps: Vec<u32>,
}

/// Every process the kernel knows, by pid, with the POSIX process-tree rules.
///
/// Its invariants, which every operation keeps:
/// - each process's parent, if it has one, is a live process that lists it
///   among its children exactly once, and nothing else does (no cycles);
/// - a zombie has no children: they went to the reaper when it exited;
/// - all members of a process group (live and zombie) are in one session;
/// - no pid is handed out while a process uses the number as its pid, pgid
///   or sid.
#[derive(Clone, Debug)]
pub struct ProcTree {
    procs: BTreeMap<u32, Proc>,
    /// `(pgid, pid)` for every process, live and zombie.
    groups: BTreeSet<(u32, u32)>,
    /// `(sid, pid)` for every process, live and zombie.
    sessions: BTreeSet<(u32, u32)>,
    pids: PidAllocator,
}

impl ProcTree {
    /// An empty tree whose pids run from [`crate::FIRST_PID`] to `max_pid`.
    ///
    /// # Panics
    ///
    /// If `max_pid` exceeds `i32::MAX`.
    pub const fn new(max_pid: u32) -> Self {
        Self {
            procs: BTreeMap::new(),
            groups: BTreeSet::new(),
            sessions: BTreeSet::new(),
            pids: PidAllocator::new(max_pid),
        }
    }

    // ---- creation and transitions ----

    /// Adds init, pid 1, with no parent, as the leader of its own group and
    /// session. `EAGAIN` if 1 is already in use. Allocates.
    pub fn spawn_init(&mut self, rusage_slot: u64) -> Result<u32, Errno> {
        if self.is_busy(INIT_PID) {
            return Err(Errno::Again);
        }
        self.insert(INIT_PID, None, INIT_PID, INIT_PID, rusage_slot);
        Ok(INIT_PID)
    }

    /// Adds a process the kernel starts itself, with a new pid, no parent
    /// (`getppid` 0), as the leader of its own group and session. `EAGAIN`
    /// when no pid is free. Allocates.
    pub fn spawn_root(&mut self, rusage_slot: u64) -> Result<u32, Errno> {
        let pid = self.alloc_pid()?;
        self.insert(pid, None, pid, pid, rusage_slot);
        Ok(pid)
    }

    /// `fork`: a new child of `parent`, last in its children list, in its
    /// group and session, running, not exec'd. `rusage_slot` is an opaque
    /// token handed back when the child is reaped. `ESRCH` if `parent` isn't
    /// live, `EAGAIN` when no pid is free; either leaves the tree unchanged.
    /// Allocates.
    pub fn fork(&mut self, parent: u32, rusage_slot: u64) -> Result<u32, Errno> {
        let (pgid, sid) = {
            let p = self.live(parent)?;
            (p.pgid, p.sid)
        };
        let pid = self.alloc_pid()?;
        self.insert(pid, Some(parent), pgid, sid, rusage_slot);
        if let Some(p) = self.procs.get_mut(&parent) {
            p.children.push(pid);
        }
        Ok(pid)
    }

    /// `execve` succeeded: from now on its parent's `setpgid` on it is
    /// `EACCES`. `ESRCH` if `pid` isn't live.
    pub fn exec(&mut self, pid: u32) -> Result<(), Errno> {
        self.live_mut(pid)?.execed = true;
        Ok(())
    }

    /// A stop signal stopped `pid`: its stop is unreported until a wait with
    /// `WUNTRACED`/`WSTOPPED` reports it, and a continue not yet reported is
    /// forgotten. Returns whether it changed state (`false` if it was
    /// already stopped). `ESRCH` if `pid` isn't live.
    pub fn stop(&mut self, pid: u32, signal: Signal) -> Result<bool, Errno> {
        let p = self.live_mut(pid)?;
        if matches!(p.state, ProcState::Stopped { .. }) {
            return Ok(false);
        }
        p.state = ProcState::Stopped { signal, reported: false };
        Ok(true)
    }

    /// `SIGCONT` reached `pid`: a stopped process runs again, its continue
    /// unreported until a wait with `WCONTINUED` reports it. Returns whether
    /// it was stopped (`false`: no change). `ESRCH` if `pid` isn't live.
    pub fn cont(&mut self, pid: u32) -> Result<bool, Errno> {
        let p = self.live_mut(pid)?;
        if !matches!(p.state, ProcState::Stopped { .. }) {
            return Ok(false);
        }
        p.state = ProcState::Continued { reported: false };
        Ok(true)
    }

    /// `pid` ended with `status`: it becomes a zombie and its children move
    /// to `reaper`, appended to its children in their order. Returns what
    /// the kernel must signal ([`ExitOutcome`]).
    ///
    /// The kernel picks the reaper (init, or whatever its design names). It
    /// is used only if `pid` has children, and must then be live and must
    /// not be `pid` or a descendant of it (`EINVAL` otherwise). `ESRCH` if
    /// `pid` isn't live. On an error the tree is unchanged. Allocates.
    pub fn exit(&mut self, pid: u32, status: ExitStatus, reaper: u32) -> Result<ExitOutcome, Errno> {
        let (parent, own_pgid, has_children) = {
            let p = self.live(pid)?;
            (p.parent, p.pgid, !p.children.is_empty())
        };
        if has_children && (self.live(reaper).is_err() || self.is_self_or_descendant(reaper, pid)) {
            return Err(Errno::Inval);
        }
        // The children's own `parent` links still name `pid` until they move,
        // so the "before" orphan checks below see the old tree.
        let children = self.procs.get_mut(&pid).map(|p| core::mem::take(&mut p.children)).unwrap_or_default();

        // The groups whose orphaned state this exit can change: each child's
        // (its parent link changes) and the exiting process's own (it stops
        // counting as a member). Orphanhood is judged before and after.
        let mut candidates: Vec<(u32, bool)> = Vec::new();
        for &c in &children {
            let g = self.procs[&c].pgid;
            if g != own_pgid && !candidates.iter().any(|&(x, _)| x == g) {
                candidates.push((g, self.is_orphaned_pgrp(g)));
            }
        }
        candidates.push((own_pgid, self.is_orphaned_pgrp(own_pgid)));

        if let Some(p) = self.procs.get_mut(&pid) {
            p.state = ProcState::Zombie(status);
        }
        let mut adopted_zombies = Vec::new();
        for &c in &children {
            if let Some(child) = self.procs.get_mut(&c) {
                child.parent = Some(reaper);
                if !child.state.is_live() {
                    adopted_zombies.push(c);
                }
            }
        }
        if !children.is_empty()
            && let Some(r) = self.procs.get_mut(&reaper)
        {
            r.children.extend_from_slice(&children);
        }

        let orphaned_pgrps = candidates
            .into_iter()
            .filter(|&(g, before)| !before && self.is_orphaned_pgrp(g) && self.has_stopped_member(g))
            .map(|(g, _)| g)
            .collect();
        Ok(ExitOutcome { parent, adopted_zombies, orphaned_pgrps })
    }

    /// Removes the zombie `pid` without a wait (its parent ignores
    /// `SIGCHLD`, or it has no parent), returning how it ended and its
    /// `rusage_slot`. `ESRCH` if `pid` isn't a zombie. Frees; never allocates.
    pub fn release(&mut self, pid: u32) -> Result<(ExitStatus, u64), Errno> {
        match self.procs.get(&pid) {
            Some(Proc { state: ProcState::Zombie(status), rusage_slot, .. }) => {
                let out = (*status, *rusage_slot);
                self.remove(pid);
                Ok(out)
            }
            _ => Err(Errno::Srch),
        }
    }

    // ---- wait ----

    /// `wait4`/`waitid` by `caller` (a caller that isn't live has no
    /// children: [`WaitResult::NoChildren`]).
    ///
    /// Invalid options give [`WaitResult::Invalid`] before anything else.
    /// The caller's children matching `target` are then considered in
    /// children-list order (oldest fork first, adopted children after, in
    /// their order), as Linux's `do_wait` walks them; the first with
    /// something to report wins, checked per child as Linux's
    /// `wait_consider_task` does:
    /// 1. a zombie is reported if exits are asked for (`waitid`'s
    ///    `WEXITED`; always for `wait4`): reaped, or left in place under
    ///    `WNOWAIT` ([`WaitResult::Zombie`]);
    /// 2. an unreported stop, with `WUNTRACED`/`WSTOPPED`;
    /// 3. an unreported continue, with `WCONTINUED`.
    ///
    /// Reporting a stop or continue marks it reported, except under
    /// `WNOWAIT`. If nothing is ready: [`WaitResult::NoneReady`] when a
    /// matching child is live, else [`WaitResult::NoChildren`] (so a
    /// `waitid` without `WEXITED` whose only matches are zombies gets
    /// `ECHILD`, as in Linux). With `__WCLONE` and without `__WALL` no child
    /// matches: every child here exits with `SIGCHLD`.
    ///
    /// Frees when it reaps; never allocates.
    pub fn wait(&mut self, caller: u32, target: WaitTarget, options: WaitOptions) -> WaitResult {
        if options.check().is_err() {
            return WaitResult::Invalid;
        }
        let flags = options.effective();
        let consume = flags & WNOWAIT == 0;
        let Some(p) = self.procs.get(&caller).filter(|p| p.state.is_live()) else {
            return WaitResult::NoChildren;
        };
        if !options.takes_sigchld_children() {
            return WaitResult::NoChildren;
        }

        let mut live_match = false;
        let mut found = None;
        for &c in &p.children {
            let child = &self.procs[&c];
            if !target.matches(c, child.pgid) {
                continue;
            }
            match child.state {
                ProcState::Zombie(status) => {
                    if flags & WEXITED != 0 {
                        found = Some((c, Report::Exit(status, child.rusage_slot)));
                        break;
                    }
                }
                ProcState::Stopped { signal, reported: false } if flags & WSTOPPED != 0 => {
                    found = Some((c, Report::Stop(signal)));
                    break;
                }
                ProcState::Continued { reported: false } if flags & WCONTINUED != 0 => {
                    found = Some((c, Report::Cont));
                    break;
                }
                _ => live_match = true,
            }
        }

        let Some((pid, report)) = found else {
            return if live_match { WaitResult::NoneReady } else { WaitResult::NoChildren };
        };
        match report {
            Report::Exit(status, rusage_slot) if consume => {
                self.remove(pid);
                WaitResult::Reaped { pid, status, rusage_slot }
            }
            Report::Exit(status, rusage_slot) => WaitResult::Zombie { pid, status, rusage_slot },
            Report::Stop(signal) => {
                if consume && let Some(child) = self.procs.get_mut(&pid) {
                    child.state = ProcState::Stopped { signal, reported: true };
                }
                WaitResult::Stopped { pid, signal }
            }
            Report::Cont => {
                if consume && let Some(child) = self.procs.get_mut(&pid) {
                    child.state = ProcState::Continued { reported: true };
                }
                WaitResult::Continued { pid }
            }
        }
    }

    // ---- lookups ----

    /// Whether `pid` is in the tree (live or zombie).
    pub fn contains(&self, pid: u32) -> bool {
        self.procs.contains_key(&pid)
    }

    /// How many processes the tree holds, live and zombie.
    pub fn len(&self) -> usize {
        self.procs.len()
    }

    /// Whether the tree holds no process.
    pub fn is_empty(&self) -> bool {
        self.procs.is_empty()
    }

    /// The pids in the tree, ascending.
    pub fn pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.procs.keys().copied()
    }

    /// `pid`'s state, if it is in the tree.
    pub fn state(&self, pid: u32) -> Option<ProcState> {
        self.procs.get(&pid).map(|p| p.state)
    }

    /// `pid`'s parent: `Some(None)` for a process without one, `None` if
    /// `pid` isn't in the tree. `getppid` is the parent's pid, or 0.
    pub fn parent(&self, pid: u32) -> Option<Option<u32>> {
        self.procs.get(&pid).map(|p| p.parent)
    }

    /// `pid`'s children in children-list order (empty if it isn't in the tree).
    pub fn children(&self, pid: u32) -> &[u32] {
        self.procs.get(&pid).map_or(&[], |p| &p.children)
    }

    /// `pid`'s process group, if it is in the tree.
    pub fn pgid(&self, pid: u32) -> Option<u32> {
        self.procs.get(&pid).map(|p| p.pgid)
    }

    /// `pid`'s session, if it is in the tree.
    pub fn sid(&self, pid: u32) -> Option<u32> {
        self.procs.get(&pid).map(|p| p.sid)
    }

    /// Whether `pid` has exec'd, if it is in the tree.
    pub fn has_execed(&self, pid: u32) -> Option<bool> {
        self.procs.get(&pid).map(|p| p.execed)
    }

    /// The pids of process group `pgid`'s members, live and zombie, ascending.
    pub fn pgrp_members(&self, pgid: u32) -> impl Iterator<Item = u32> + '_ {
        self.groups.range((pgid, 0)..=(pgid, u32::MAX)).map(|&(_, pid)| pid)
    }

    /// Whether some process, live or zombie, has `pgid` as its group.
    pub fn pgrp_exists(&self, pgid: u32) -> bool {
        self.pgrp_members(pgid).next().is_some()
    }

    /// Whether process group `pgid` is orphaned, as POSIX defines it and
    /// Linux's `will_become_orphaned_pgrp` decides: no live member has a
    /// parent in another group of the same session. Zombies, and members
    /// whose parent is init (pid 1) or who have none, don't count. A group
    /// with no live member is orphaned.
    pub fn is_orphaned_pgrp(&self, pgid: u32) -> bool {
        !self.pgrp_members(pgid).any(|m| {
            let p = &self.procs[&m];
            if !p.state.is_live() {
                return false;
            }
            match p.parent {
                Some(pp) if pp != INIT_PID => {
                    let parent = &self.procs[&pp];
                    parent.pgid != pgid && parent.sid == p.sid
                }
                _ => false,
            }
        })
    }

    /// Whether number `n` is in use as a pid, a pgid or a sid (by a live or
    /// zombie process): [`PidAllocator::alloc`] skips it.
    pub fn is_busy(&self, n: u32) -> bool {
        self.procs.contains_key(&n)
            || self.pgrp_exists(n)
            || self.sessions.range((n, 0)..=(n, u32::MAX)).next().is_some()
    }

    // ---- internals ----

    pub(crate) fn live(&self, pid: u32) -> Result<&Proc, Errno> {
        self.procs.get(&pid).filter(|p| p.state.is_live()).ok_or(Errno::Srch)
    }

    fn live_mut(&mut self, pid: u32) -> Result<&mut Proc, Errno> {
        self.procs.get_mut(&pid).filter(|p| p.state.is_live()).ok_or(Errno::Srch)
    }

    pub(crate) fn proc(&self, pid: u32) -> Option<&Proc> {
        self.procs.get(&pid)
    }

    fn alloc_pid(&mut self) -> Result<u32, Errno> {
        let mut pids = self.pids;
        let pid = pids.alloc(|n| self.is_busy(n))?;
        self.pids = pids;
        Ok(pid)
    }

    fn insert(&mut self, pid: u32, parent: Option<u32>, pgid: u32, sid: u32, rusage_slot: u64) {
        self.procs.insert(
            pid,
            Proc { parent, children: Vec::new(), pgid, sid, state: ProcState::Running, execed: false, rusage_slot },
        );
        self.groups.insert((pgid, pid));
        self.sessions.insert((sid, pid));
    }

    /// Takes `pid` (a zombie, so childless) out of the tree and its parent's
    /// children list.
    fn remove(&mut self, pid: u32) {
        let Some(p) = self.procs.remove(&pid) else {
            return;
        };
        debug_assert!(p.children.is_empty(), "removed process {pid} still has children");
        self.groups.remove(&(p.pgid, pid));
        self.sessions.remove(&(p.sid, pid));
        if let Some(pp) = p.parent
            && let Some(parent) = self.procs.get_mut(&pp)
        {
            parent.children.retain(|&c| c != pid);
        }
    }

    /// Whether `pid` is `ancestor` or below it.
    fn is_self_or_descendant(&self, mut pid: u32, ancestor: u32) -> bool {
        loop {
            if pid == ancestor {
                return true;
            }
            match self.procs.get(&pid).and_then(|p| p.parent) {
                Some(pp) => pid = pp,
                None => return false,
            }
        }
    }

    fn has_stopped_member(&self, pgid: u32) -> bool {
        self.pgrp_members(pgid).any(|m| matches!(self.procs[&m].state, ProcState::Stopped { .. }))
    }

    /// Moves `pid` to group `pgid` (and session `sid`), keeping the indexes.
    pub(crate) fn set_ids(&mut self, pid: u32, pgid: u32, sid: u32) {
        let Some(p) = self.procs.get_mut(&pid) else {
            return;
        };
        let (old_pgid, old_sid) = (p.pgid, p.sid);
        p.pgid = pgid;
        p.sid = sid;
        if old_pgid != pgid {
            self.groups.remove(&(old_pgid, pid));
            self.groups.insert((pgid, pid));
        }
        if old_sid != sid {
            self.sessions.remove(&(old_sid, pid));
            self.sessions.insert((sid, pid));
        }
    }

    /// Checks every structural invariant, panicking on the first broken one.
    #[cfg(test)]
    pub(crate) fn check_invariants(&self) {
        let mut groups = BTreeSet::new();
        let mut sessions = BTreeSet::new();
        let mut group_sid: BTreeMap<u32, u32> = BTreeMap::new();
        for (&pid, p) in &self.procs {
            groups.insert((p.pgid, pid));
            sessions.insert((p.sid, pid));
            let sid = *group_sid.entry(p.pgid).or_insert(p.sid);
            assert_eq!(sid, p.sid, "group {} spans sessions {sid} and {}", p.pgid, p.sid);
            if let Some(pp) = p.parent {
                let parent = self.procs.get(&pp).expect("parent missing");
                assert!(parent.state.is_live(), "{pid}'s parent {pp} is a zombie");
                let n = parent.children.iter().filter(|&&c| c == pid).count();
                assert_eq!(n, 1, "{pid} listed {n} times by its parent {pp}");
            }
            if !p.state.is_live() {
                assert!(p.children.is_empty(), "zombie {pid} has children");
            }
            for &c in &p.children {
                let child = self.procs.get(&c).expect("child missing");
                assert_eq!(child.parent, Some(pid), "{c} is listed by {pid}, not its parent");
            }
            let mut steps = 0;
            let mut up = p.parent;
            while let Some(a) = up {
                steps += 1;
                assert!(steps <= self.procs.len(), "cycle above {pid}");
                up = self.procs[&a].parent;
            }
        }
        assert_eq!(groups, self.groups, "group index out of date");
        assert_eq!(sessions, self.sessions, "session index out of date");
    }
}

enum Report {
    Exit(ExitStatus, u64),
    Stop(Signal),
    Cont,
}
