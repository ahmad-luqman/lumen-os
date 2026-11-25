//! Exit: the zombie, its children moved to the reaper (or made roots), the
//! zombies that leave the tree at once, and what the kernel must then do
//! ([`ExitOutcome`], [`ExitAction`]).

use alloc::vec::Vec;

use lumen_signal::Signal;

use crate::tree::ProcState;
use crate::{Bug, ExitError, ExitStatus, ProcTree, Role, RusageSlot};

/// One thing the kernel must do after [`ProcTree::exit`]. Each carries only
/// signals to send and slots to free: no further tree change is needed, and
/// the slots may be freed at any time. The recipients are named by number,
/// though (a parent, a group), and a number can be reaped and reused once
/// the lock is dropped: the kernel resolves them (the parent's process, the
/// group's members) in the same lock hold as the exit, as Linux sends these
/// signals under `tasklist_lock`.
#[derive(Debug, PartialEq, Eq)]
pub enum ExitAction {
    /// `child` became a zombie whose parent is `parent`. As Linux's
    /// `do_notify_parent` does, the kernel wakes `parent`'s waits and sends
    /// it `SIGCHLD` unless its disposition is `SIG_IGN` (a handler with
    /// `SA_NOCLDWAIT` still gets it). Named for the exiting process, and for
    /// each of its children that was already a zombie when the reaper
    /// adopted it.
    Notify {
        /// The parent to tell (the reaper, for an adopted zombie).
        parent: u32,
        /// The zombie.
        child: u32,
        /// How it ended (for `SIGCHLD`'s `siginfo`).
        status: ExitStatus,
        /// The zombie's token if [`ProcTree::exit`] already removed it
        /// (`autoreap(parent)` held: `SIG_IGN` or `SA_NOCLDWAIT`), for the
        /// kernel to free; `None` while it waits for a wait.
        released: Option<RusageSlot>,
    },
    /// `child` ended with no parent (a root, or a zombie child of a process
    /// that exited without a reaper) and left the tree: the kernel frees
    /// its token.
    Released {
        /// The process.
        child: u32,
        /// How it ended.
        status: ExitStatus,
        /// Its token, for the kernel to free.
        slot: RusageSlot,
    },
    /// Send `SIGHUP`, then `SIGCONT`, to every member of process group
    /// `pgid` (Linux's `kill_orphaned_pgrp`; [`ProcTree::pgrp_members`]).
    HangupThenContinue {
        /// The process group.
        pgid: u32,
    },
}

/// What the kernel must do after [`ProcTree::exit`]: iterate it once
/// ([`ExitActions`]; see [`ExitAction`] for what may wait for the unlock),
/// in Linux's order (`exit_notify`):
/// 1. for each child of the exiting process in children-list order: a
///    [`ExitAction::Notify`] to the reaper (or [`ExitAction::Released`],
///    without one) if it was a zombie, then a
///    [`ExitAction::HangupThenContinue`] if its group fires there;
/// 2. a [`ExitAction::HangupThenContinue`] for the exiting process's own
///    group, if it fires;
/// 3. the exiting process's [`ExitAction::Notify`] to its parent, or
///    [`ExitAction::Released`] without one.
///
/// A child's group `g` fires as Linux's `reparent_leader` fires it (when
/// the exiting process isn't pid 1; see [`ProcTree::exit`]): the exiting
/// process is in another group than `g` but in the child's session, `g` is
/// orphaned once the exiting process is gone and its children are the
/// reaper's, or roots ([`ProcTree::is_orphaned_pgrp`]), and a member of `g`
/// is stopped (a stop already reported by a wait counts). Whether `g` was
/// orphaned before doesn't matter. Linux tests each child right after
/// moving it, while the later children still name the exiting process as
/// their parent: a later live child in `g` is still a link. So `g` fires at
/// its last live child among the exiting process's children, or, when all
/// of those in `g` are zombies, at the first of them; once at most (Linux's
/// `SIGCONT` ends the stops at once).
///
/// The own group fires under the same rule with the exiting process's
/// parent in place of the exiting process, with no exemption for a parent
/// that is pid 1; never for a process without a parent.
///
/// Dropping it, or its [`ExitActions`] before the last action, is a bug
/// (a debug assertion).
#[must_use]
#[derive(Debug)]
pub struct ExitOutcome(ExitActions);

impl IntoIterator for ExitOutcome {
    type Item = ExitAction;
    type IntoIter = ExitActions;

    fn into_iter(self) -> ExitActions {
        self.0
    }
}

/// An [`ExitOutcome`]'s actions, each handed out once. It owns the list
/// (the exiting process's old children list, reused) and frees it when
/// dropped.
#[must_use]
#[derive(Debug)]
pub struct ExitActions {
    reaper: Option<u32>,
    /// The children's actions, [`ChildAction`]s encoded.
    children: Vec<u32>,
    /// Where the next child action starts in `children`.
    next: usize,
    own_group: Option<u32>,
    /// The exiting process's own action, `None` once handed out.
    last: Option<ExitAction>,
}

impl Iterator for ExitActions {
    type Item = ExitAction;

    fn next(&mut self) -> Option<ExitAction> {
        if self.next < self.children.len() {
            let (action, words) = ChildAction::decode(&self.children[self.next..]);
            self.next += words;
            return Some(action.into_exit_action(self.reaper));
        }
        if let Some(pgid) = self.own_group.take() {
            return Some(ExitAction::HangupThenContinue { pgid });
        }
        self.last.take()
    }
}

impl Drop for ExitActions {
    fn drop(&mut self) {
        // A failing test's unwinding drops it half-read: don't abort.
        #[cfg(test)]
        if std::thread::panicking() {
            return;
        }
        debug_assert!(self.last.is_none(), "invariant: every exit action is carried out");
    }
}

/// Tags an encoded [`ChildAction::Hangup`]: its one word is the pgid with
/// this bit set. Pids and pgids are at most `i32::MAX`, so the bit is free.
const GROUP: u32 = 1 << 31;
/// In a zombie's status word: killed by the signal in the low byte...
const KILLED: u32 = 1 << 8;
/// ...dumping core.
const CORE: u32 = 1 << 9;
/// In a zombie's status word: it was released, its slot in the next two
/// words (low, high).
const RELEASED: u32 = 1 << 10;

/// One of a child's actions, as [`ProcTree::exit`] stores it in the
/// exiting process's old children list: a hangup in one word (`pgid |
/// GROUP`), a zombie in two (pid, status) or four (pid, status, slot).
#[derive(Debug, PartialEq, Eq)]
enum ChildAction {
    Zombie { child: u32, status: ExitStatus, released: Option<u64> },
    Hangup { pgid: u32 },
}

impl ChildAction {
    /// How many words it takes.
    fn words(&self) -> usize {
        match self {
            Self::Hangup { .. } => 1,
            Self::Zombie { released: None, .. } => 2,
            Self::Zombie { released: Some(_), .. } => 4,
        }
    }

    /// Appends its words to `out`, which has room for them.
    fn encode(&self, out: &mut Vec<u32>) {
        assert!(out.capacity() - out.len() >= self.words(), "invariant: exit's actions fit the room reserved");
        match *self {
            Self::Hangup { pgid } => out.push(pgid | GROUP),
            Self::Zombie { child, status, released } => {
                let status_word = match status {
                    ExitStatus::Exited(code) => u32::from(code),
                    ExitStatus::Killed { signal, core } => KILLED | signal.number() | if core { CORE } else { 0 },
                };
                out.push(child);
                match released {
                    None => out.push(status_word),
                    Some(slot) => out.extend([status_word | RELEASED, slot as u32, (slot >> 32) as u32]),
                }
            }
        }
    }

    /// The action at the start of `words`, and how many words it took.
    fn decode(words: &[u32]) -> (Self, usize) {
        let first = words[0];
        if first & GROUP != 0 {
            return (Self::Hangup { pgid: first & !GROUP }, 1);
        }
        let s = words[1];
        let status = if s & KILLED != 0 {
            let signal = Signal::new(s & 0xff).expect("invariant: an encoded signal is valid");
            ExitStatus::Killed { signal, core: s & CORE != 0 }
        } else {
            ExitStatus::Exited(s as u8)
        };
        if s & RELEASED != 0 {
            let slot = u64::from(words[2]) | (u64::from(words[3]) << 32);
            (Self::Zombie { child: first, status, released: Some(slot) }, 4)
        } else {
            (Self::Zombie { child: first, status, released: None }, 2)
        }
    }

    fn into_exit_action(self, reaper: Option<u32>) -> ExitAction {
        match (self, reaper) {
            (Self::Hangup { pgid }, _) => ExitAction::HangupThenContinue { pgid },
            (Self::Zombie { child, status, released }, Some(parent)) => {
                ExitAction::Notify { parent, child, status, released: released.map(RusageSlot::new) }
            }
            (Self::Zombie { child, status, released }, None) => ExitAction::Released {
                child,
                status,
                slot: RusageSlot::new(released.expect("invariant: a parentless exit releases its zombie children")),
            },
        }
    }
}

impl ProcTree {
    /// `pid` ended with `status`: it becomes a zombie, its children move to
    /// the reaper, and every tree change is made here, under the kernel's
    /// lock, as Linux's `exit_notify` makes them under `tasklist_lock`.
    /// Returns what the kernel must then do ([`ExitOutcome`]).
    ///
    /// - `reaper` `Some(r)`: the children are appended to `r`'s children in
    ///   their order. `r` must be live and must not be `pid` or a descendant
    ///   of it, checked on every exit, children or not.
    /// - `reaper` `None`: Lumen's rule for a run with no init: the children
    ///   become roots (no parent; no link for the orphaned-group tests), and
    ///   those already zombies leave the tree ([`ExitAction::Released`]).
    ///   Linux always has a reaper: a kernel with an init process passes
    ///   `Some`.
    /// - `autoreap(parent)` says whether `parent` has `SIGCHLD` at `SIG_IGN`
    ///   or a handler with `SA_NOCLDWAIT`. It is asked once for the reaper
    ///   and once for `pid`'s parent. The zombies such a parent would get
    ///   leave the tree here (`pid` itself, or the adopted zombies), their
    ///   tokens in [`ExitAction::Notify`]'s `released`. A process without a
    ///   parent leaves the tree too ([`ExitAction::Released`]).
    ///
    /// Init exiting with other processes live (Linux panics instead) gets
    /// the same rules as any process; in Linux its unmoved children's links
    /// would be exempt as init's, so the groups fired may differ.
    ///
    /// [`Bug::NotLive`] if `pid` isn't live; [`Bug::BadReaper`] for a wrong
    /// reaper; [`ExitError::NoMem`] if room can't be reserved in the
    /// reaper's children list (for the children) or in `pid`'s old children
    /// list (for the children's actions: two words for a zombie, four for
    /// one released, one for a hangup; after the children themselves when
    /// there is no reaper). Any error leaves the tree unchanged.
    ///
    /// `pid`'s added threads ([`ProcTree::add_thread`]) still registered
    /// are freed with it: their numbers are free again.
    ///
    /// Allocates only through those two fallible reservations; frees the
    /// nodes of the zombies that leave and the freed tids' index entries. Finding the groups to fire takes
    /// O(children² + children × group size), done twice (once to size the
    /// reservation).
    pub fn exit(
        &mut self,
        pid: u32,
        status: ExitStatus,
        reaper: Option<u32>,
        autoreap: impl Fn(u32) -> bool,
    ) -> Result<ExitOutcome, ExitError> {
        let p = self.live(pid, Role::Exiting)?;
        let (parent, own_pgid, own_sid, n) = (p.parent, p.pgid, p.sid, p.children.len());
        if let Some(r) = reaper
            && (self.live(r, Role::Exiting).is_err() || self.is_self_or_descendant(r, pid))
        {
            return Err(Bug::BadReaper { pid, reaper: r }.into());
        }
        // Each parent's disposition, asked once. Without a reaper the zombie
        // children leave at once.
        let reaper_reaps = reaper.is_none_or(&autoreap);
        let parent_reaps = parent.is_some_and(&autoreap);

        // Room first: nothing changes until both reservations hold.
        let mut words = 0;
        for i in 0..n {
            let (zombie, hangup) = self.child_actions(pid, reaper, reaper_reaps, &self.proc(pid).children, i);
            words += zombie.map_or(0, |a| a.words()) + hangup.map_or(0, |a| a.words());
        }
        if let Some(r) = reaper {
            self.proc_mut(r).children.try_reserve(n).map_err(|_| ExitError::NoMem)?;
        }
        // With a reaper the children are read back from its list, so the
        // old list needs room for the actions only; without one they stay at
        // its front while their actions go after them.
        let need = if reaper.is_some() { words } else { n + words };
        let old = &mut self.proc_mut(pid).children;
        old.try_reserve(need.saturating_sub(old.len())).map_err(|_| ExitError::NoMem)?;

        let mut list = core::mem::take(&mut self.proc_mut(pid).children);
        match reaper {
            Some(r) => {
                for &c in &list {
                    self.proc_mut(c).parent = Some(r);
                }
                let start = self.proc(r).children.len();
                // No allocation: the room was reserved above.
                self.proc_mut(r).children.extend_from_slice(&list);
                list.clear();
                for i in 0..n {
                    let moved = &self.proc(r).children[start..start + n];
                    let (zombie, hangup) = self.child_actions(pid, reaper, reaper_reaps, moved, i);
                    for action in [zombie, hangup].into_iter().flatten() {
                        action.encode(&mut list);
                    }
                }
            }
            None => {
                for i in 0..n {
                    let (zombie, hangup) = self.child_actions(pid, None, reaper_reaps, &list[..n], i);
                    for action in [zombie, hangup].into_iter().flatten() {
                        action.encode(&mut list);
                    }
                }
                for &c in &list[..n] {
                    self.proc_mut(c).parent = None;
                }
                list.drain(..n);
            }
        }
        assert_eq!(list.len(), words, "invariant: exit's two passes agree");
        self.proc_mut(pid).state = ProcState::Zombie(status);
        // Any thread still registered goes with it (the kernel removes each
        // as its thread exits, so only a bookkeeping bug leaves one).
        self.free_threads(pid);

        let own_group = parent.is_some_and(|pp| self.links(pp, own_pgid, own_sid))
            && self.is_orphaned_pgrp(own_pgid)
            && self.has_stopped_member(own_pgid);

        // The zombies that leave now: the released children, then `pid`.
        let mut at = 0;
        while at < list.len() {
            let (action, used) = ChildAction::decode(&list[at..]);
            if let ChildAction::Zombie { child, released: Some(_), .. } = action {
                let _token = self.remove(child);
            }
            at += used;
        }
        let last = match parent {
            Some(parent) => {
                let released = parent_reaps.then(|| self.remove(pid));
                ExitAction::Notify { parent, child: pid, status, released }
            }
            None => ExitAction::Released { child: pid, status, slot: self.remove(pid) },
        };
        Ok(ExitOutcome(ExitActions {
            reaper,
            children: list,
            next: 0,
            own_group: own_group.then_some(own_pgid),
            last: Some(last),
        }))
    }

    /// The actions for child `kids[i]` of the exiting `pid` (`kids` its
    /// children, in order: its list before the move, or the same pids in the
    /// reaper's list after it; the answer is the same): its zombie's, if it
    /// is one, and its group's hangup, if the group fires here.
    fn child_actions(
        &self,
        pid: u32,
        reaper: Option<u32>,
        reaper_reaps: bool,
        kids: &[u32],
        i: usize,
    ) -> (Option<ChildAction>, Option<ChildAction>) {
        let c = kids[i];
        let child = self.proc(c);
        let zombie = match child.state {
            ProcState::Zombie(status) => {
                Some(ChildAction::Zombie { child: c, status, released: reaper_reaps.then_some(child.slot) })
            }
            _ => None,
        };
        let g = child.pgid;
        let fires_here = || {
            let live_in_g = |k: &u32| {
                let p = self.proc(*k);
                p.pgid == g && p.state.is_live()
            };
            let at = kids.iter().rposition(live_in_g).or_else(|| kids.iter().position(|k| self.proc(*k).pgid == g));
            at == Some(i)
        };
        let fires = self.links(pid, g, child.sid)
            && fires_here()
            && self.orphaned_once_gone(g, Some((pid, reaper)))
            && self.has_stopped_member(g);
        (zombie, fires.then_some(ChildAction::Hangup { pgid: g }))
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use lumen_signal::Signal;

    use super::ChildAction;
    use crate::ExitStatus;

    /// Every status, slots across the 32-bit boundary, and hangups at the
    /// largest pgid, through the words and back.
    #[test]
    fn child_actions_round_trip_through_their_words() {
        let mut statuses = Vec::new();
        statuses.extend((0..=255).map(ExitStatus::Exited));
        for n in 1..=64 {
            for core in [false, true] {
                statuses.push(ExitStatus::Killed { signal: Signal::new(n).unwrap(), core });
            }
        }
        let slots = [None, Some(0), Some(7), Some(u64::from(u32::MAX)), Some(u64::from(u32::MAX) + 1), Some(u64::MAX)];
        let mut actions = Vec::new();
        for (i, status) in statuses.into_iter().enumerate() {
            let released = slots[i % slots.len()];
            actions.push(ChildAction::Zombie { child: i as u32 + 2, status, released });
            actions.push(ChildAction::Hangup { pgid: i32::MAX as u32 - i as u32 });
        }
        let words: usize = actions.iter().map(ChildAction::words).sum();
        let mut list = Vec::with_capacity(words);
        for a in &actions {
            a.encode(&mut list);
        }
        assert_eq!(list.len(), words);
        let mut at = 0;
        for want in &actions {
            let (got, used) = ChildAction::decode(&list[at..]);
            assert_eq!(&got, want);
            assert_eq!(used, want.words());
            at += used;
        }
        assert_eq!(at, list.len());
    }
}
