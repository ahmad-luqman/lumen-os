//! The process tree: parents and children, groups and sessions, wait states,
//! fork, release and wait. Exit is in `exit.rs`, sessions in `session.rs`.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use crate::pid::PidAllocator;
use crate::wait::{Report, WaitOptions, WaitReady, WaitRequest, WaitTarget};
use crate::{Bug, Errno, Error, ExitStatus, INIT_PID, Signal};

/// A process's state as `wait` sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcState {
    /// Running (or sleeping): nothing to report.
    Running,
    /// Stopped by `signal`.
    Stopped {
        /// The signal that stopped it.
        signal: Signal,
        /// Whether a wait without `WNOWAIT` has reported this stop.
        reported: bool,
    },
    /// Running again after `SIGCONT` ended a stop.
    Continued {
        /// Whether a wait without `WNOWAIT` has reported the continue.
        reported: bool,
    },
    /// Ended, not yet reaped.
    Zombie(ExitStatus),
}

impl ProcState {
    pub(crate) const fn is_live(self) -> bool {
        !matches!(self, Self::Zombie(_))
    }
}

#[cfg_attr(test, derive(Clone))]
#[derive(Debug)]
pub(crate) struct Proc {
    /// `None` for a process the kernel started itself (init and other roots).
    pub(crate) parent: Option<u32>,
    /// In Linux's children-list order: each child appended when forked or
    /// adopted (a dead parent's children appended in their order).
    pub(crate) children: Vec<u32>,
    pub(crate) pgid: u32,
    pub(crate) sid: u32,
    pub(crate) state: ProcState,
    pub(crate) execed: bool,
    pub(crate) rusage_slot: u64,
}

/// A process's wait state changed ([`ProcTree::stop`], [`ProcTree::cont`]):
/// the kernel sends `SIGCHLD` to `parent` and wakes its waits.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StateChanged {
    /// The parent to notify, or `None` for a process without one.
    pub parent: Option<u32>,
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
///
/// A breach of them is a bug in this crate: it panics with a message
/// starting "invariant:".
#[cfg_attr(test, derive(Clone))]
#[derive(Debug)]
pub struct ProcTree {
    pub(crate) procs: BTreeMap<u32, Proc>,
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
    /// session. `rusage_slot` is an opaque token the kernel gives the
    /// process, handed back when it is reaped or released.
    ///
    /// [`Bug::InitExists`] while a process 1 is in the tree; `EAGAIN` while
    /// the number is still some process's pgid or sid (it succeeds once 1
    /// is free). Allocates infallibly (see the crate's Allocation section).
    pub fn spawn_init(&mut self, rusage_slot: u64) -> Result<u32, Error> {
        if self.procs.contains_key(&INIT_PID) {
            return Err(Bug::InitExists.into());
        }
        if self.is_busy(INIT_PID) {
            return Err(Errno::Again.into());
        }
        self.insert(INIT_PID, None, INIT_PID, INIT_PID, rusage_slot);
        Ok(INIT_PID)
    }

    /// Adds a process the kernel starts itself, with a new pid, no parent
    /// (`getppid` 0), as the leader of its own group and session.
    /// `rusage_slot` is an opaque token the kernel gives the process,
    /// handed back when it is reaped or released. `EAGAIN` when no pid is
    /// free. Allocates infallibly.
    pub fn spawn_root(&mut self, rusage_slot: u64) -> Result<u32, Error> {
        let pid = self.alloc_pid()?;
        self.insert(pid, None, pid, pid, rusage_slot);
        Ok(pid)
    }

    /// `fork`: a new child of `parent`, last in its children list, in its
    /// group and session, running, not exec'd. `rusage_slot` is an opaque
    /// token the kernel gives the child, handed back when it is reaped or
    /// released.
    ///
    /// [`Bug::NotLive`] if `parent` isn't live; `ENOMEM` if room for the
    /// child in the parent's children list can't be reserved; `EAGAIN` when
    /// no pid is free. Any error leaves the tree unchanged. Besides that
    /// fallible reservation, allocates infallibly (the tree node).
    pub fn fork(&mut self, parent: u32, rusage_slot: u64) -> Result<u32, Error> {
        let (pgid, sid) = {
            let p = self.live(parent)?;
            (p.pgid, p.sid)
        };
        self.proc_mut(parent).children.try_reserve(1).map_err(|_| Errno::NoMem)?;
        let pid = self.alloc_pid()?;
        self.insert(pid, Some(parent), pgid, sid, rusage_slot);
        // No allocation: the room was reserved above.
        self.proc_mut(parent).children.push(pid);
        Ok(pid)
    }

    /// `execve` succeeded: from now on its parent's `setpgid` on it is
    /// `EACCES`. [`Bug::NotLive`] if `pid` isn't live.
    pub fn exec(&mut self, pid: u32) -> Result<(), Error> {
        self.live_mut(pid)?.execed = true;
        Ok(())
    }

    /// A stop signal stopped `pid`: its stop is unreported until a wait with
    /// `WUNTRACED`/`WSTOPPED` reports it, and a continue not yet reported is
    /// forgotten. The parent to notify if it changed state; `None` if it
    /// was already stopped. `ESRCH` if `pid` isn't live: a signal reaching
    /// a zombie (a `kill`, or a [`crate::ExitAction::HangupThenContinue`]
    /// to a group with zombie members) stops nothing; the kernel decides
    /// what its sender sees.
    pub fn stop(&mut self, pid: u32, signal: Signal) -> Result<Option<StateChanged>, Errno> {
        let p = self.live_for_signal(pid)?;
        if matches!(p.state, ProcState::Stopped { .. }) {
            return Ok(None);
        }
        p.state = ProcState::Stopped { signal, reported: false };
        Ok(Some(StateChanged { parent: p.parent }))
    }

    /// `SIGCONT` reached `pid`: a stopped process runs again, its continue
    /// unreported until a wait with `WCONTINUED` reports it; a stop not yet
    /// reported is forgotten. The parent to notify if it was stopped;
    /// `None` (no change) otherwise. `ESRCH` if `pid` isn't live, as for
    /// [`ProcTree::stop`].
    pub fn cont(&mut self, pid: u32) -> Result<Option<StateChanged>, Errno> {
        let p = self.live_for_signal(pid)?;
        if !matches!(p.state, ProcState::Stopped { .. }) {
            return Ok(None);
        }
        p.state = ProcState::Continued { reported: false };
        Ok(Some(StateChanged { parent: p.parent }))
    }

    /// Removes the zombie `pid` without a wait (its parent ignores
    /// `SIGCHLD` or has `SA_NOCLDWAIT`, or it has none: see
    /// [`crate::ExitAction`]), returning how it ended. `rusage_slot` must be
    /// the token the kernel gave it; on success that token is handed back
    /// for the last time.
    ///
    /// [`Bug::NotAZombie`] if `pid` isn't a zombie; [`Bug::WrongSlot`] if
    /// its slot isn't `rusage_slot` (the pid was reused since): either
    /// leaves the tree unchanged. The check is only as strong as the
    /// tokens are unique: a slot freed at a reap and handed out again to a
    /// process that got the same pid would pass. Frees; never allocates.
    pub fn release(&mut self, pid: u32, rusage_slot: u64) -> Result<ExitStatus, Error> {
        let Some(&Proc { state: ProcState::Zombie(status), rusage_slot: found, .. }) = self.procs.get(&pid) else {
            return Err(Bug::NotAZombie { pid }.into());
        };
        if found != rusage_slot {
            return Err(Bug::WrongSlot { pid, expected: rusage_slot, found }.into());
        }
        self.remove(pid);
        Ok(status)
    }

    // ---- wait ----

    /// Checks a `wait4(pid, options)` by `caller` in Linux's order: `EINVAL`
    /// for an option `wait4` doesn't take, then `ESRCH` for `pid ==
    /// i32::MIN`. `pid` is `n > 0` the child `n`, `0` any child in the
    /// caller's group (resolved here, once), `-1` any child, `n < -1` any
    /// child in group `-n`. [`Bug::NotLive`] if `caller` isn't live.
    pub fn wait4_request(&self, caller: u32, pid: i32, options: u32) -> Result<WaitRequest, Error> {
        let pgid = self.live(caller)?.pgid;
        let options = WaitOptions::wait4(options)?;
        let target = WaitTarget::from_wait4_pid(pid, pgid)?;
        Ok(WaitRequest { caller, target, options })
    }

    /// Checks a `waitid(idtype, id, options)` by `caller` in Linux's order:
    /// `EINVAL` for an option `waitid` doesn't take or for none of
    /// `WEXITED`, `WSTOPPED`, `WCONTINUED`, then `EINVAL` for a bad `idtype`
    /// or `id`: `P_ALL` any child; `P_PID` the child `id > 0`; `P_PGID`
    /// group `id >= 0`, `0` the caller's (resolved here, once); `P_PIDFD`
    /// refused until pidfds exist. [`Bug::NotLive`] if `caller` isn't live.
    pub fn waitid_request(&self, caller: u32, idtype: i32, id: i32, options: u32) -> Result<WaitRequest, Error> {
        let pgid = self.live(caller)?.pgid;
        let options = WaitOptions::waitid(options)?;
        let target = WaitTarget::from_waitid(idtype, id, pgid)?;
        Ok(WaitRequest { caller, target, options })
    }

    /// One pass of a `wait4`/`waitid` ([`ProcTree::wait4_request`],
    /// [`ProcTree::waitid_request`]); a blocking wait repeats it with the
    /// same request each time the caller is woken.
    ///
    /// The caller's children matching the target are considered in
    /// children-list order (each child appended when forked or adopted), as
    /// Linux's `do_wait` walks them; the first with something to report
    /// wins, checked per child as Linux's `wait_consider_task` does:
    /// 1. a zombie is reported if exits are asked for (`waitid`'s
    ///    `WEXITED`; always for `wait4`): reaped, or left in place under
    ///    `WNOWAIT` ([`Report::Zombie`]);
    /// 2. an unreported stop, with `WUNTRACED`/`WSTOPPED`;
    /// 3. an unreported continue, with `WCONTINUED`.
    ///
    /// Reporting a stop or continue marks it reported, except under
    /// `WNOWAIT`. If nothing is ready: [`WaitReady::NoneReady`] when a
    /// matching child is live. `ECHILD` when no child matches, when only
    /// zombies match a wait without `WEXITED` (as in Linux), and with
    /// `__WCLONE` without `__WALL` (every child here exits with `SIGCHLD`).
    /// [`Bug::NotLive`] if the caller isn't live.
    ///
    /// Frees when it reaps; never allocates.
    pub fn wait(&mut self, request: WaitRequest) -> Result<WaitReady, Error> {
        let WaitRequest { caller, target, options } = request;
        let p = self.live(caller)?;
        if !options.takes_sigchld_children() {
            return Err(Errno::Child.into());
        }
        let mut live_match = false;
        let found = p.children.iter().find_map(|&c| {
            let child = self.proc(c);
            if !target.matches(c, child.pgid) {
                return None;
            }
            match child.state {
                ProcState::Zombie(_) if options.exited() => Some((c, child.state)),
                ProcState::Zombie(_) => None,
                ProcState::Stopped { reported: false, .. } if options.stopped() => Some((c, child.state)),
                ProcState::Continued { reported: false } if options.continued() => Some((c, child.state)),
                _ => {
                    live_match = true;
                    None
                }
            }
        });
        let Some((pid, state)) = found else {
            return if live_match { Ok(WaitReady::NoneReady) } else { Err(Errno::Child.into()) };
        };
        let consume = options.consumes();
        let report = match state {
            ProcState::Zombie(status) => {
                let rusage_slot = self.proc(pid).rusage_slot;
                if consume {
                    self.remove(pid);
                    Report::Reaped { pid, status, rusage_slot }
                } else {
                    Report::Zombie { pid, status, rusage_slot }
                }
            }
            ProcState::Stopped { signal, .. } => {
                if consume {
                    self.proc_mut(pid).state = ProcState::Stopped { signal, reported: true };
                }
                Report::Stopped { pid, signal }
            }
            ProcState::Continued { .. } => {
                if consume {
                    self.proc_mut(pid).state = ProcState::Continued { reported: true };
                }
                Report::Continued { pid }
            }
            ProcState::Running => unreachable!("invariant: a running child has nothing to report"),
        };
        Ok(WaitReady::Reported(report))
    }

    // ---- lookups ----

    /// Whether `pid` is in the tree (live or zombie).
    pub fn contains(&self, pid: u32) -> bool {
        self.procs.contains_key(&pid)
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

    /// The `rusage_slot` the kernel gave `pid`, if it is in the tree. Valid
    /// only while the tree stays locked: once the lock is dropped the pid
    /// may be reaped and reused. For [`ProcTree::release`] of a zombie an
    /// [`crate::ExitAction`] names, read it under the same lock hold as the
    /// exit.
    pub fn rusage_slot(&self, pid: u32) -> Option<u64> {
        self.procs.get(&pid).map(|p| p.rusage_slot)
    }

    /// Whether process group `pgid` is orphaned, as POSIX defines it and
    /// Linux's `will_become_orphaned_pgrp` decides: no live member has a
    /// parent in another group of the same session. Zombies don't count,
    /// nor do members without a parent. Nor do members whose parent is pid
    /// 1: that exemption is Linux's (`is_global_init`), not POSIX's, and it
    /// holds whichever reaper the kernel names (a member adopted by another
    /// reaper is judged by that reaper's group and session). A group with no
    /// live member is orphaned.
    pub fn is_orphaned_pgrp(&self, pgid: u32) -> bool {
        self.orphaned_once_gone(pgid, None)
    }

    /// Whether number `n` is in use as a pid, a pgid or a sid (by a live or
    /// zombie process): no new process gets it as its pid.
    pub fn is_busy(&self, n: u32) -> bool {
        self.procs.contains_key(&n)
            || self.pgrp_exists(n)
            || self.sessions.range((n, 0)..=(n, u32::MAX)).next().is_some()
    }

    // ---- internals ----

    /// The pids of process group `pgid`'s members, live and zombie, ascending.
    pub(crate) fn pgrp_members(&self, pgid: u32) -> impl Iterator<Item = u32> + '_ {
        self.groups.range((pgid, 0)..=(pgid, u32::MAX)).map(|&(_, pid)| pid)
    }

    /// Whether some process, live or zombie, has `pgid` as its group.
    pub(crate) fn pgrp_exists(&self, pgid: u32) -> bool {
        self.pgrp_members(pgid).next().is_some()
    }

    /// The session of process group `pgid`, if some process is in it.
    pub(crate) fn pgrp_sid(&self, pgid: u32) -> Option<u32> {
        self.pgrp_members(pgid).next().map(|m| self.proc(m).sid)
    }

    /// [`ProcTree::is_orphaned_pgrp`], or, with `gone` = `(pid, reaper)`,
    /// what it will be once `pid` has exited: `pid` skipped as a member,
    /// and its children judged with `reaper` as their parent. Both give the
    /// same answer before and after [`ProcTree::exit`] moves the children.
    pub(crate) fn orphaned_once_gone(&self, pgid: u32, gone: Option<(u32, u32)>) -> bool {
        !self.pgrp_members(pgid).any(|m| {
            let p = self.proc(m);
            if !p.state.is_live() || gone.is_some_and(|(pid, _)| pid == m) {
                return false;
            }
            let parent = match (p.parent, gone) {
                (Some(pp), Some((pid, reaper))) if pp == pid => Some(reaper),
                (parent, _) => parent,
            };
            match parent {
                Some(pp) if pp != INIT_PID => {
                    let parent = self.proc(pp);
                    parent.pgid != pgid && parent.sid == p.sid
                }
                _ => false,
            }
        })
    }

    /// Whether a member of `pgid` is stopped, its stop reported by a wait
    /// or not, as Linux's `has_stopped_jobs` counts it.
    pub(crate) fn has_stopped_member(&self, pgid: u32) -> bool {
        self.pgrp_members(pgid).any(|m| matches!(self.proc(m).state, ProcState::Stopped { .. }))
    }

    pub(crate) fn live(&self, pid: u32) -> Result<&Proc, Error> {
        self.procs.get(&pid).filter(|p| p.state.is_live()).ok_or(Error::Bug(Bug::NotLive { pid }))
    }

    fn live_mut(&mut self, pid: u32) -> Result<&mut Proc, Error> {
        self.procs.get_mut(&pid).filter(|p| p.state.is_live()).ok_or(Error::Bug(Bug::NotLive { pid }))
    }

    fn live_for_signal(&mut self, pid: u32) -> Result<&mut Proc, Errno> {
        self.procs.get_mut(&pid).filter(|p| p.state.is_live()).ok_or(Errno::Srch)
    }

    pub(crate) fn proc(&self, pid: u32) -> &Proc {
        self.procs.get(&pid).expect("invariant: the process is in the tree")
    }

    pub(crate) fn proc_mut(&mut self, pid: u32) -> &mut Proc {
        self.procs.get_mut(&pid).expect("invariant: the process is in the tree")
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
        let p = self.procs.remove(&pid).expect("invariant: a removed process is in the tree");
        assert!(p.children.is_empty(), "invariant: removed process {pid} still has children");
        self.groups.remove(&(p.pgid, pid));
        self.sessions.remove(&(p.sid, pid));
        if let Some(pp) = p.parent {
            self.proc_mut(pp).children.retain(|&c| c != pid);
        }
    }

    /// Whether `pid` (in the tree) is `ancestor` or below it.
    pub(crate) fn is_self_or_descendant(&self, mut pid: u32, ancestor: u32) -> bool {
        // A walk up passes each process at most once (no cycles).
        for _ in 0..=self.procs.len() {
            if pid == ancestor {
                return true;
            }
            match self.proc(pid).parent {
                Some(pp) => pid = pp,
                None => return false,
            }
        }
        panic!("invariant: no cycle above {pid}");
    }

    /// Moves `pid` to group `pgid`, keeping the index.
    pub(crate) fn set_pgid(&mut self, pid: u32, pgid: u32) {
        let old = core::mem::replace(&mut self.proc_mut(pid).pgid, pgid);
        if old != pgid {
            self.groups.remove(&(old, pid));
            self.groups.insert((pgid, pid));
        }
    }

    /// Moves `pid` to session `sid`, keeping the index.
    pub(crate) fn set_sid(&mut self, pid: u32, sid: u32) {
        let old = core::mem::replace(&mut self.proc_mut(pid).sid, sid);
        if old != sid {
            self.sessions.remove(&(old, pid));
            self.sessions.insert((sid, pid));
        }
    }

    /// The pids in the tree, ascending.
    #[cfg(test)]
    pub(crate) fn pids(&self) -> impl Iterator<Item = u32> + '_ {
        self.procs.keys().copied()
    }

    /// Whether `pid` has exec'd, if it is in the tree.
    #[cfg(test)]
    pub(crate) fn has_execed(&self, pid: u32) -> Option<bool> {
        self.procs.get(&pid).map(|p| p.execed)
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
