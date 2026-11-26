//! The process tree: parents and children, groups and sessions, wait states,
//! fork and wait. Exit is in `exit.rs`, sessions in `session.rs`.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use lumen_signal::Signal;

use crate::pid::PidAllocator;
use crate::wait::{Report, WaitOptions, WaitReady, WaitRequest, WaitTarget};
use crate::{Bug, Errno, Error, ExitStatus, INIT_PID, NoFreePid, Role, RusageSlot, SlotRef, SpawnInitError};

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
    /// `None` for a root: a process the kernel started itself (init and
    /// other roots), or one whose parent exited without a reaper.
    pub(crate) parent: Option<u32>,
    /// In Linux's children-list order: each child appended when forked or
    /// adopted (a dead parent's children appended in their order).
    pub(crate) children: Vec<u32>,
    pub(crate) pgid: u32,
    pub(crate) sid: u32,
    pub(crate) state: ProcState,
    pub(crate) execed: bool,
    /// The [`RusageSlot`]'s value: the tree owns the token until the
    /// process leaves it.
    pub(crate) slot: u64,
    /// Its threads' tids but the leader's (whose tid is the pid), in the
    /// order [`ProcTree::add_thread`] handed them out. Empty once it is a
    /// zombie.
    pub(crate) tids: Vec<u32>,
}

/// What [`ProcTree::stop`] or [`ProcTree::cont`] did.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateChange {
    /// The wait state changed. As Linux's `do_notify_parent_cldstop` does,
    /// the kernel wakes `parent`'s waits, and sends it `SIGCHLD` unless its
    /// disposition is `SIG_IGN` or it set `SA_NOCLDSTOP`.
    Changed {
        /// The parent to tell, or `None` for a root.
        parent: Option<u32>,
    },
    /// Nothing changed (already stopped, or not stopped): nobody to tell.
    Unchanged,
}

/// A PID number reserved against reuse until the last clone is dropped.
///
/// It retains identity, not a process or group membership: a terminal signal
/// to a reaped group finds no recipients, never a new group with that number.
#[derive(Clone, Debug)]
pub struct PidRef(Arc<u32>);

impl PidRef {
    /// The reserved PID, process-group or session number.
    pub fn number(&self) -> u32 {
        *self.0
    }
}

/// Every process the kernel knows, by pid, with the POSIX process-tree rules.
///
/// Its invariants, which every operation keeps:
/// - each process's parent, if it has one, is a live process that lists it
///   among its children exactly once, and nothing else does (no cycles);
/// - a zombie has a parent and no children: its children went to the reaper
///   (or became roots) when it exited, and a zombie without a parent (or
///   whose parent autoreaps) leaves the tree inside [`ProcTree::exit`];
/// - all members of a process group (live and zombie) are in one session;
/// - no pid is handed out while a process uses the number as its pid, pgid
///   or sid, a live thread uses it as its tid, or a [`PidRef`] retains it;
/// - each tid ([`ProcTree::add_thread`]) belongs to exactly one live
///   process, and is never a pid, pgid or sid.
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
    /// Every non-leader tid, with the pid of the process it belongs to.
    tids: BTreeMap<u32, u32>,
    /// Weak entries never keep a reservation alive; creation/allocation prunes them.
    references: BTreeMap<u32, Weak<u32>>,
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
            tids: BTreeMap::new(),
            references: BTreeMap::new(),
            pids: PidAllocator::new(max_pid),
        }
    }

    // ---- creation and transitions ----

    /// Adds init, pid 1 ([`INIT_PID`]), with no parent, as the leader of its
    /// own group and session, taking its `rusage_slot` until it leaves the
    /// tree.
    ///
    /// [`SpawnInitError::Exists`] while a process 1 is in the tree;
    /// [`SpawnInitError::NumberBusy`] while the number is still some
    /// process's pgid or sid. Allocates infallibly (see the crate's
    /// Allocation section).
    pub fn spawn_init(&mut self, rusage_slot: RusageSlot) -> Result<(), SpawnInitError> {
        if self.procs.contains_key(&INIT_PID) {
            return Err(SpawnInitError::Exists);
        }
        if self.is_busy(INIT_PID) {
            return Err(SpawnInitError::NumberBusy);
        }
        self.insert(INIT_PID, None, INIT_PID, INIT_PID, rusage_slot);
        Ok(())
    }

    /// Adds a process the kernel starts itself, with a new pid, which it
    /// returns, no parent (`getppid` 0), as the leader of its own group and
    /// session, taking its `rusage_slot` until it leaves the tree.
    /// [`NoFreePid`] when no pid is free. Allocates infallibly.
    pub fn spawn_root(&mut self, rusage_slot: RusageSlot) -> Result<u32, NoFreePid> {
        let pid = self.alloc_pid().map_err(|_| NoFreePid)?;
        self.insert(pid, None, pid, pid, rusage_slot);
        Ok(pid)
    }

    /// `fork`: a new child of `parent`, last in its children list, in its
    /// group and session, running, not exec'd, taking its `rusage_slot`
    /// until it leaves the tree.
    ///
    /// [`Bug::NotLive`] if `parent` isn't live; `ENOMEM` if room for the
    /// child in the parent's children list can't be reserved; `EAGAIN` when
    /// no pid is free. Any error leaves the tree unchanged. Besides that
    /// fallible reservation, allocates infallibly (the process's node and
    /// its two index entries).
    pub fn fork(&mut self, parent: u32, rusage_slot: RusageSlot) -> Result<u32, Error> {
        let (pgid, sid) = {
            let p = self.live(parent, Role::Parent)?;
            (p.pgid, p.sid)
        };
        self.proc_mut(parent).children.try_reserve(1).map_err(|_| Errno::NoMem)?;
        let pid = self.alloc_pid()?;
        self.insert(pid, Some(parent), pgid, sid, rusage_slot);
        // No allocation: the room was reserved above.
        self.proc_mut(parent).children.push(pid);
        Ok(pid)
    }

    /// `execve` by `pid` succeeded: from now on its parent's `setpgid` on it
    /// is `EACCES`. [`Bug::NotLive`] if `pid` isn't live.
    ///
    /// An `execve` runs with one thread left (`de_thread`): the kernel
    /// removes the others ([`ProcTree::remove_thread`]) first, and a debug
    /// assertion checks none is registered.
    pub fn exec(&mut self, pid: u32) -> Result<(), Bug> {
        let p = self.live_entry_mut(pid).ok_or(Bug::NotLive { pid, role: Role::Caller })?;
        debug_assert!(p.tids.is_empty(), "exec by {pid} with threads {:?} still registered", p.tids);
        p.execed = true;
        Ok(())
    }

    /// A new thread of `pid` (`clone` with `CLONE_THREAD`): its tid, a number
    /// handed out as [`ProcTree::fork`] hands out a pid (after the last one,
    /// wrapping, never a number in use), counted by [`ProcTree::is_busy`]
    /// until [`ProcTree::remove_thread`] or `pid`'s exit frees it. The
    /// leader's tid is the pid itself and is never added.
    ///
    /// [`Bug::NotLive`] (as [`Role::Caller`]) if `pid` isn't live; `ENOMEM`
    /// if room for the tid in its thread list can't be reserved; `EAGAIN`
    /// when no number is free. Any error leaves the tree unchanged. Besides
    /// that fallible reservation, allocates infallibly (one index entry).
    pub fn add_thread(&mut self, pid: u32) -> Result<u32, Error> {
        self.live(pid, Role::Caller)?;
        self.proc_mut(pid).tids.try_reserve(1).map_err(|_| Errno::NoMem)?;
        let tid = self.alloc_pid()?;
        self.tids.insert(tid, pid);
        // No allocation: the room was reserved above.
        self.proc_mut(pid).tids.push(tid);
        Ok(tid)
    }

    /// Thread `tid` of `pid` ended: its number is free again.
    ///
    /// [`Bug::NotThread`] if `tid` isn't one of `pid`'s added threads: the
    /// leader's tid (the pid, which the process keeps until it is reaped),
    /// another process's tid, a number no thread has, or any tid of a
    /// zombie (its exit freed them). The tree is then unchanged. Frees;
    /// never allocates.
    pub fn remove_thread(&mut self, pid: u32, tid: u32) -> Result<(), Error> {
        if self.tids.get(&tid) != Some(&pid) {
            return Err(Bug::NotThread { pid, tid }.into());
        }
        self.tids.remove(&tid);
        self.proc_mut(pid).tids.retain(|&t| t != tid);
        Ok(())
    }

    /// A stop signal stopped `pid`: its stop is unreported until a wait with
    /// `WUNTRACED`/`WSTOPPED` reports it, and a continue not yet reported is
    /// forgotten. Whether it changed state ([`StateChange`]).
    ///
    /// The kernel decides whether a stop happens: as Linux's `get_signal`
    /// does, a job-control stop (`SIGTSTP`, `SIGTTIN`, `SIGTTOU`) does
    /// nothing in an orphaned group ([`ProcTree::is_orphaned_pgrp`]), while
    /// `SIGSTOP` always stops.
    ///
    /// `ESRCH` if `pid` isn't live: a zombie (a signal reaching it stops
    /// nothing), or not in the tree (a stale pid in the kernel); the kernel
    /// decides what the sender sees.
    pub fn stop(&mut self, pid: u32, signal: Signal) -> Result<StateChange, Errno> {
        let p = self.live_entry_mut(pid).ok_or(Errno::Srch)?;
        if matches!(p.state, ProcState::Stopped { .. }) {
            return Ok(StateChange::Unchanged);
        }
        p.state = ProcState::Stopped { signal, reported: false };
        Ok(StateChange::Changed { parent: p.parent })
    }

    /// `SIGCONT` reached `pid`: a stopped process runs again, its continue
    /// unreported until a wait with `WCONTINUED` reports it; a stop not yet
    /// reported is forgotten. Whether it was stopped ([`StateChange`]).
    ///
    /// `ESRCH` if `pid` isn't live: a zombie (expected when the kernel
    /// carries out a [`crate::ExitAction::HangupThenContinue`] on a group
    /// with zombie members), or not in the tree (a stale pid).
    pub fn cont(&mut self, pid: u32) -> Result<StateChange, Errno> {
        let p = self.live_entry_mut(pid).ok_or(Errno::Srch)?;
        if !matches!(p.state, ProcState::Stopped { .. }) {
            return Ok(StateChange::Unchanged);
        }
        p.state = ProcState::Continued { reported: false };
        Ok(StateChange::Changed { parent: p.parent })
    }

    // ---- wait ----

    /// Checks a `wait4(pid, options)` by `caller` in Linux's order: `EINVAL`
    /// for an option `wait4` doesn't take, then `ESRCH` for `pid ==
    /// i32::MIN`. `pid` is `n > 0` the child `n`, `0` any child in the
    /// caller's group (resolved here, once), `-1` any child, `n < -1` any
    /// child in group `-n`. [`Bug::NotLive`] if `caller` isn't live.
    pub fn wait4_request(&self, caller: u32, pid: i32, options: u32) -> Result<WaitRequest, Error> {
        let pgid = self.live(caller, Role::Caller)?.pgid;
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
        let pgid = self.live(caller, Role::Caller)?.pgid;
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
        let p = self.live(caller, Role::Caller)?;
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
            ProcState::Zombie(status) if consume => {
                let rusage_slot = self.remove(pid);
                Report::Reaped { pid, status, rusage_slot }
            }
            ProcState::Zombie(status) => Report::Zombie { pid, status, lent_slot: SlotRef(self.proc(pid).slot) },
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

    /// How many processes are in the tree, live and zombie.
    pub fn len(&self) -> usize {
        self.procs.len()
    }

    /// Whether the tree holds no process.
    pub fn is_empty(&self) -> bool {
        self.procs.is_empty()
    }

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

    /// `pid`'s threads' tids but the leader's, in the order they were added
    /// (empty if it isn't in the tree, or is a zombie).
    pub fn threads(&self, pid: u32) -> &[u32] {
        self.procs.get(&pid).map_or(&[], |p| &p.tids)
    }

    /// `pid`'s process group, if it is in the tree.
    pub fn pgid(&self, pid: u32) -> Option<u32> {
        self.procs.get(&pid).map(|p| p.pgid)
    }

    /// `pid`'s session, if it is in the tree.
    pub fn sid(&self, pid: u32) -> Option<u32> {
        self.procs.get(&pid).map(|p| p.sid)
    }

    /// The pids of process group `pgid`'s members, live and zombie,
    /// ascending: what a [`crate::ExitAction::HangupThenContinue`] or a
    /// `kill(-pgrp)` reaches. Allocates nothing. Read under the tree's
    /// lock: after it is dropped the group may have changed.
    pub fn pgrp_members(&self, pgid: u32) -> impl Iterator<Item = u32> + '_ {
        self.groups.range((pgid, 0)..=(pgid, u32::MAX)).map(|&(_, pid)| pid)
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
    /// zombie process), as a live thread's tid, or by a [`PidRef`]: no new
    /// process or thread gets it.
    pub fn is_busy(&self, n: u32) -> bool {
        self.procs.contains_key(&n)
            || self.tids.contains_key(&n)
            || self.pgrp_exists(n)
            || self.sessions.range((n, 0)..=(n, u32::MAX)).next().is_some()
            || self.references.get(&n).is_some_and(|reference| reference.strong_count() != 0)
    }

    /// Reserve an already-used number until the last returned reference drops.
    /// Returns `None` for a free number. Allocates infallibly for a new
    /// reservation; repeated references share it. Expired weak entries are
    /// reclaimed here and before PID allocation. Dropping a reference needs
    /// no process-tree lock and never allocates.
    pub fn retain_pid(&mut self, number: u32) -> Option<PidRef> {
        if !self.is_busy(number) {
            return None;
        }
        if let Some(reference) = self.references.get(&number).and_then(Weak::upgrade) {
            return Some(PidRef(reference));
        }
        self.references.retain(|_, reference| reference.strong_count() != 0);
        let reference = Arc::new(number);
        self.references.insert(number, Arc::downgrade(&reference));
        Some(PidRef(reference))
    }

    #[cfg(test)]
    pub(crate) fn retained_pid_entries(&self) -> usize {
        self.references.len()
    }

    // ---- internals ----

    /// Whether some process, live or zombie, has `pgid` as its group.
    pub(crate) fn pgrp_exists(&self, pgid: u32) -> bool {
        self.pgrp_members(pgid).next().is_some()
    }

    /// The session of process group `pgid`, if some process is in it.
    pub(crate) fn pgrp_sid(&self, pgid: u32) -> Option<u32> {
        self.pgrp_members(pgid).next().map(|m| self.proc(m).sid)
    }

    /// [`ProcTree::is_orphaned_pgrp`], or, with `gone` = `(pid, reaper)`,
    /// what it will be once `pid` has exited: its children judged with
    /// `reaper` as their parent (no parent for `None`: no link). Only asked
    /// about groups `pid` isn't in, so `pid` itself needs no skipping.
    /// Gives the same answer before and after [`ProcTree::exit`] moves the
    /// children.
    pub(crate) fn orphaned_once_gone(&self, pgid: u32, gone: Option<(u32, Option<u32>)>) -> bool {
        !self.pgrp_members(pgid).any(|m| {
            let p = self.proc(m);
            debug_assert!(gone.is_none_or(|(pid, _)| pid != m), "invariant: the exiting process isn't in the group");
            if !p.state.is_live() {
                return false;
            }
            let parent = match (p.parent, gone) {
                (Some(pp), Some((pid, reaper))) if pp == pid => reaper,
                (parent, _) => parent,
            };
            parent.is_some_and(|pp| pp != INIT_PID && self.links(pp, pgid, p.sid))
        })
    }

    /// Whether `parent` is a link for a member of group `pgid` in session
    /// `sid`: in another group of the same session.
    pub(crate) fn links(&self, parent: u32, pgid: u32, sid: u32) -> bool {
        let q = self.proc(parent);
        q.pgid != pgid && q.sid == sid
    }

    /// Whether a member of `pgid` is stopped, its stop reported by a wait
    /// or not, as Linux's `has_stopped_jobs` counts it.
    pub(crate) fn has_stopped_member(&self, pgid: u32) -> bool {
        self.pgrp_members(pgid).any(|m| matches!(self.proc(m).state, ProcState::Stopped { .. }))
    }

    pub(crate) fn live(&self, pid: u32, role: Role) -> Result<&Proc, Bug> {
        self.procs.get(&pid).filter(|p| p.state.is_live()).ok_or(Bug::NotLive { pid, role })
    }

    fn live_entry_mut(&mut self, pid: u32) -> Option<&mut Proc> {
        self.procs.get_mut(&pid).filter(|p| p.state.is_live())
    }

    pub(crate) fn proc(&self, pid: u32) -> &Proc {
        self.procs.get(&pid).expect("invariant: the process is in the tree")
    }

    pub(crate) fn proc_mut(&mut self, pid: u32) -> &mut Proc {
        self.procs.get_mut(&pid).expect("invariant: the process is in the tree")
    }

    fn alloc_pid(&mut self) -> Result<u32, Errno> {
        self.references.retain(|_, reference| reference.strong_count() != 0);
        let mut pids = self.pids;
        let pid = pids.alloc(|n| self.is_busy(n))?;
        self.pids = pids;
        Ok(pid)
    }

    fn insert(&mut self, pid: u32, parent: Option<u32>, pgid: u32, sid: u32, rusage_slot: RusageSlot) {
        let slot = rusage_slot.into_raw();
        self.procs.insert(
            pid,
            Proc {
                parent,
                children: Vec::new(),
                pgid,
                sid,
                state: ProcState::Running,
                execed: false,
                slot,
                tids: Vec::new(),
            },
        );
        self.groups.insert((pgid, pid));
        self.sessions.insert((sid, pid));
    }

    /// Takes `pid` (a zombie, so childless) out of the tree and its parent's
    /// children list, handing back its token.
    pub(crate) fn remove(&mut self, pid: u32) -> RusageSlot {
        let p = self.procs.remove(&pid).expect("invariant: a removed process is in the tree");
        assert!(p.children.is_empty(), "invariant: removed process {pid} still has children");
        assert!(p.tids.is_empty(), "invariant: removed process {pid} still has threads");
        self.groups.remove(&(p.pgid, pid));
        self.sessions.remove(&(p.sid, pid));
        if let Some(pp) = p.parent {
            self.proc_mut(pp).children.retain(|&c| c != pid);
        }
        RusageSlot::new(p.slot)
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

    /// Frees every tid of `pid` (its exit). Frees; never allocates.
    pub(crate) fn free_threads(&mut self, pid: u32) {
        for tid in core::mem::take(&mut self.proc_mut(pid).tids) {
            self.tids.remove(&tid);
        }
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

    /// The value of `pid`'s token, if it is in the tree.
    #[cfg(test)]
    pub(crate) fn slot_of(&self, pid: u32) -> Option<u64> {
        self.procs.get(&pid).map(|p| p.slot)
    }

    /// Whether `pid` has exec'd, if it is in the tree.
    #[cfg(test)]
    pub(crate) fn has_execed(&self, pid: u32) -> Option<bool> {
        self.procs.get(&pid).map(|p| p.execed)
    }

    /// Checks every structural invariant, panicking on the first broken one
    /// (host tests, and the kernel's ktests through the `test-util`
    /// feature).
    #[cfg(any(test, feature = "test-util"))]
    pub fn check_invariants(&self) {
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
                assert!(p.parent.is_some(), "zombie {pid} has no parent");
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
        let mut tids = BTreeMap::new();
        for (&pid, p) in &self.procs {
            assert!(p.state.is_live() || p.tids.is_empty(), "zombie {pid} has threads {:?}", p.tids);
            for &tid in &p.tids {
                assert!(tids.insert(tid, pid).is_none(), "tid {tid} listed twice");
                assert!(!self.procs.contains_key(&tid), "tid {tid} is a pid");
                assert!(groups.range((tid, 0)..=(tid, u32::MAX)).next().is_none(), "tid {tid} is a pgid");
                assert!(sessions.range((tid, 0)..=(tid, u32::MAX)).next().is_none(), "tid {tid} is a sid");
            }
        }
        assert_eq!(tids, self.tids, "tid index out of date");
    }
}
