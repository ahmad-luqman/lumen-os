//! Exit: the zombie, its children moved to the reaper, and what the kernel
//! must then do ([`ExitOutcome`], [`ExitAction`]).

use alloc::vec::Vec;
use core::iter;

use crate::tree::ProcState;
use crate::{Bug, Errno, Error, ExitStatus, ProcTree};

/// Tags an [`ExitOutcome`] entry as a process group (the other entries are
/// adopted zombies). Pids and pgids are at most `i32::MAX`, so the bit is
/// free.
const GROUP: u32 = 1 << 31;

/// One thing the kernel must do after [`ProcTree::exit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitAction {
    /// `child` is a zombie whose parent is `parent`. As Linux's
    /// `do_notify_parent` does, the kernel wakes `parent`'s waits; sends it
    /// `SIGCHLD` unless its disposition is `SIG_IGN`; and, if it is
    /// `SIG_IGN` or `SA_NOCLDWAIT` is set, [`ProcTree::release`]s `child`
    /// (nobody may wait for it). Named for the exiting process, and for
    /// each of its children that was already a zombie when the reaper
    /// adopted it.
    Notify {
        /// The parent to tell (the reaper, for an adopted zombie).
        parent: u32,
        /// The zombie.
        child: u32,
    },
    /// `child`, the exiting process, has no parent to wait for it: the
    /// kernel [`ProcTree::release`]s it now.
    ReleaseNow {
        /// The zombie.
        child: u32,
    },
    /// Send `SIGHUP`, then `SIGCONT`, to every member of process group
    /// `pgid` (Linux's `kill_orphaned_pgrp`).
    HangupThenContinue {
        /// The process group.
        pgid: u32,
    },
}

/// What the kernel must do after [`ProcTree::exit`], as
/// [`ExitOutcome::actions`]. Dropping it frees its list of actions.
#[must_use]
#[derive(Debug)]
pub struct ExitOutcome {
    /// The exiting process.
    pid: u32,
    parent: Option<u32>,
    /// The reaper; `None` only for a childless exit, which has no
    /// children's actions.
    reaper: Option<u32>,
    /// The actions for the exiting process's children, in order: an adopted
    /// zombie's pid, or a pgid tagged with [`GROUP`]. The exiting process's
    /// old children list, reused.
    children: Vec<u32>,
    /// The exiting process's own group, if it is to get `SIGHUP` and
    /// `SIGCONT`.
    own_group: Option<u32>,
}

impl ExitOutcome {
    /// The actions, in Linux's order (`exit_notify`):
    /// 1. for each child of the exiting process in children-list order: a
    ///    [`ExitAction::Notify`] to the reaper if it was a zombie, then a
    ///    [`ExitAction::HangupThenContinue`] if its group fires there;
    /// 2. a [`ExitAction::HangupThenContinue`] for the exiting process's
    ///    own group, if it fires;
    /// 3. the exiting process's [`ExitAction::Notify`] to its parent, or
    ///    [`ExitAction::ReleaseNow`] without one.
    ///
    /// A child's group `g` fires as Linux's `reparent_leader` fires it:
    /// the exiting process is in another group than `g` but in the child's
    /// session, `g` is orphaned once the exiting process is gone and its
    /// children are the reaper's ([`ProcTree::is_orphaned_pgrp`]), and a
    /// member of `g` is stopped (a stop already reported by a wait counts).
    /// Whether `g` was orphaned before doesn't matter. Linux tests each
    /// child right after moving it, while the later children still name the
    /// exiting process as their parent: a later live child in `g` is still a
    /// link. So `g` fires at its last live child among the exiting
    /// process's children, or, when all of those in `g` are zombies, at the
    /// first of them; once at most (Linux's `SIGCONT` ends the stops at
    /// once).
    ///
    /// The own group fires under the same rule with the exiting process's
    /// parent in place of the exiting process, with no exemption for a
    /// parent that is pid 1; never for a process without a parent.
    pub fn actions(&self) -> impl Iterator<Item = ExitAction> + '_ {
        let reaper = self.reaper;
        let children = self.children.iter().map(move |&e| {
            if e & GROUP != 0 {
                ExitAction::HangupThenContinue { pgid: e & !GROUP }
            } else {
                ExitAction::Notify { parent: reaper.expect("invariant: an adopted zombie has a reaper"), child: e }
            }
        });
        let own = self.own_group.map(|pgid| ExitAction::HangupThenContinue { pgid });
        let last = match self.parent {
            Some(parent) => ExitAction::Notify { parent, child: self.pid },
            None => ExitAction::ReleaseNow { child: self.pid },
        };
        children.chain(own).chain(iter::once(last))
    }
}

impl ProcTree {
    /// `pid` ended with `status`: it becomes a zombie, and its children
    /// move to `reaper`, appended to its children in their order. Returns
    /// what the kernel must do ([`ExitOutcome::actions`]).
    ///
    /// The kernel picks the reaper (init, or whatever its design names).
    /// `Some(r)`: `r` must be live and must not be `pid` or a descendant of
    /// it, checked on every exit, children or not ([`Bug::BadReaper`]).
    /// `None`: allowed only when `pid` has no children, live or zombie
    /// ([`Bug::NoReaper`]); so the last live process (init alone, or a lone
    /// root such as a `linux /path` run) can exit. Init exiting while other
    /// processes live (Linux panics instead) gets the same rules as any
    /// process; in Linux its unmoved children's links would be exempt as
    /// init's.
    ///
    /// [`Bug::NotLive`] if `pid` isn't live; `ENOMEM` if room can't be
    /// reserved in the reaper's children list (for the children) or in
    /// `pid`'s old children list (for the actions, which can outnumber the
    /// children: a group fired at a zombie child). Any error leaves the
    /// tree unchanged.
    ///
    /// Allocates only through those two fallible reservations. Finding the
    /// groups to signal scans the children and their groups (quadratic in
    /// the number of children).
    pub fn exit(&mut self, pid: u32, status: ExitStatus, reaper: Option<u32>) -> Result<ExitOutcome, Error> {
        let p = self.live(pid)?;
        let (parent, own_pgid, own_sid, n) = (p.parent, p.pgid, p.sid, p.children.len());
        match reaper {
            Some(r) if self.live(r).is_err() || self.is_self_or_descendant(r, pid) => {
                return Err(Bug::BadReaper { pid, reaper: r }.into());
            }
            None if n > 0 => return Err(Bug::NoReaper { pid }.into()),
            _ => {}
        }
        let actions = match reaper {
            Some(r) => self.move_children(pid, r)?,
            // No children: nothing to move, and the empty list needs no memory.
            None => core::mem::take(&mut self.proc_mut(pid).children),
        };
        self.proc_mut(pid).state = ProcState::Zombie(status);

        let own_group = parent
            .is_some_and(|pp| {
                let q = self.proc(pp);
                q.pgid != own_pgid && q.sid == own_sid
            })
            .then_some(own_pgid)
            .filter(|&g| self.is_orphaned_pgrp(g) && self.has_stopped_member(g));
        Ok(ExitOutcome { pid, parent, reaper, children: actions, own_group })
    }

    /// Moves the exiting `pid`'s children to `reaper` (live, outside
    /// `pid`'s subtree), returning the children's actions in `pid`'s old
    /// children list. Reserves room first: on `ENOMEM` nothing changed.
    fn move_children(&mut self, pid: u32, reaper: u32) -> Result<Vec<u32>, Error> {
        let n = self.proc(pid).children.len();
        let mut needed = 0usize;
        self.child_actions(pid, reaper, &self.proc(pid).children, |_| needed += 1);
        self.proc_mut(reaper).children.try_reserve(n).map_err(|_| Errno::NoMem)?;
        let old = &mut self.proc_mut(pid).children;
        old.try_reserve(needed.saturating_sub(old.len())).map_err(|_| Errno::NoMem)?;

        let mut actions = core::mem::take(&mut self.proc_mut(pid).children);
        for &c in &actions {
            self.proc_mut(c).parent = Some(reaper);
        }
        let start = self.proc(reaper).children.len();
        self.proc_mut(reaper).children.extend_from_slice(&actions);

        actions.clear();
        let moved = &self.proc(reaper).children[start..];
        self.child_actions(pid, reaper, moved, |e| {
            assert!(actions.len() < actions.capacity(), "invariant: exit's actions fit the room reserved");
            actions.push(e);
        });
        assert_eq!(actions.len(), needed, "invariant: exit's actions are the same before and after the move");
        Ok(actions)
    }

    /// Emits the actions for `pid`'s children `kids` (its children list
    /// before the move, or the same pids in the reaper's list after it), in
    /// order: an adopted zombie's pid, a fired group's pgid tagged with
    /// [`GROUP`]. Gives the same answer before and after the move.
    fn child_actions(&self, pid: u32, reaper: u32, kids: &[u32], mut emit: impl FnMut(u32)) {
        let me = self.proc(pid);
        let (my_pgid, my_sid) = (me.pgid, me.sid);
        let live_in = |g: u32, k: &u32| {
            let p = self.proc(*k);
            p.pgid == g && p.state.is_live()
        };
        for (i, &c) in kids.iter().enumerate() {
            let child = self.proc(c);
            let live = child.state.is_live();
            if !live {
                emit(c);
            }
            let g = child.pgid;
            if g == my_pgid || child.sid != my_sid {
                continue;
            }
            let fires_here = if live {
                !kids[i + 1..].iter().any(|k| live_in(g, k))
            } else {
                !kids.iter().any(|k| live_in(g, k)) && !kids[..i].iter().any(|k| self.proc(*k).pgid == g)
            };
            if fires_here && self.orphaned_once_gone(g, Some((pid, reaper))) && self.has_stopped_member(g) {
                emit(g | GROUP);
            }
        }
    }
}
