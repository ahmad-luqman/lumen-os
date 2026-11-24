//! Host tests for the process-tree rules.

mod exit;
mod model;
mod pid;
mod session;
mod status;
mod wait;

use alloc::vec::Vec;

use lumen_signal::Signal;

use crate::{Bug, Errno, Error, ExitAction, ExitStatus, ProcTree, Report, RusageSlot, StateChange, WaitReady};

pub(super) const INIT: u32 = 1;

/// A tree holding init (pid 1) with room for pids up to 1000.
pub(super) fn tree() -> ProcTree {
    let mut t = ProcTree::new(1000);
    assert_eq!(t.spawn_init(slot(0)), Ok(()));
    t
}

/// The token `n`.
pub(super) fn slot(n: u64) -> RusageSlot {
    RusageSlot::new(n)
}

/// The signal numbered `n`.
pub(super) fn sig(n: u32) -> Signal {
    Signal::new(n).expect("a valid signal")
}

/// `Err(Error::User(e))`.
pub(super) fn user<T>(e: Errno) -> Result<T, Error> {
    Err(Error::User(e))
}

/// `Err(Error::Bug(b))`.
pub(super) fn bug<T>(b: Bug) -> Result<T, Error> {
    Err(Error::Bug(b))
}

/// Forks a child of `parent` (rusage slot 0).
pub(super) fn fork(t: &mut ProcTree, parent: u32) -> u32 {
    let pid = t.fork(parent, slot(0)).expect("fork");
    t.check_invariants();
    pid
}

/// `pid` is stopped by `signal` (whether it changed state is ignored).
pub(super) fn stop(t: &mut ProcTree, pid: u32, signal: Signal) {
    let _ = t.stop(pid, signal).expect("stop");
}

/// `pid` is continued (whether it changed state is ignored).
pub(super) fn cont(t: &mut ProcTree, pid: u32) {
    let _: StateChange = t.cont(pid).expect("cont");
}

/// An exit action without its status and slot (the tests that pin those
/// use [`ExitAction`] itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Act {
    /// `Notify`, the zombie left for a wait.
    Notify { parent: u32, child: u32 },
    /// `Notify`, the zombie already removed (`released`).
    NotifyReleased { parent: u32, child: u32 },
    /// `Released`.
    Released { child: u32 },
    /// `HangupThenContinue`.
    Hup { pgid: u32 },
}

impl From<&ExitAction> for Act {
    fn from(a: &ExitAction) -> Self {
        match *a {
            ExitAction::Notify { parent, child, released: None, .. } => Self::Notify { parent, child },
            ExitAction::Notify { parent, child, released: Some(_), .. } => Self::NotifyReleased { parent, child },
            ExitAction::Released { child, .. } => Self::Released { child },
            ExitAction::HangupThenContinue { pgid } => Self::Hup { pgid },
        }
    }
}

/// `pid` exits with `status`, `reaper` adopting its children and the
/// parents in `reaping` autoreaping: the actions, all carried out.
pub(super) fn exit_full(
    t: &mut ProcTree,
    pid: u32,
    status: ExitStatus,
    reaper: Option<u32>,
    reaping: &[u32],
) -> Vec<ExitAction> {
    let out = t.exit(pid, status, reaper, |p| reaping.contains(&p)).expect("exit");
    t.check_invariants();
    out.into_iter().collect()
}

/// `pid` exits with `status`, `reaper` adopting its children, nobody
/// autoreaping: the actions, without status and slot.
pub(super) fn exit_with(t: &mut ProcTree, pid: u32, status: ExitStatus, reaper: Option<u32>) -> Vec<Act> {
    exit_full(t, pid, status, reaper, &[]).iter().map(Act::from).collect()
}

/// `pid` exits with `code`, init adopting its children: the actions.
pub(super) fn exit(t: &mut ProcTree, pid: u32, code: i32) -> Vec<Act> {
    exit_with(t, pid, ExitStatus::exited(code), Some(INIT))
}

/// The groups an exit's actions hang up, in order.
pub(super) fn hangups(actions: &[Act]) -> Vec<u32> {
    actions
        .iter()
        .filter_map(|a| match *a {
            Act::Hup { pgid } => Some(pgid),
            _ => None,
        })
        .collect()
}

/// The adopted zombies an exit's actions name, in order (every
/// notification but the last action, which is the exiting process's own).
pub(super) fn adopted(actions: &[Act]) -> Vec<u32> {
    let (_, children) = actions.split_last().expect("an exit has a last action");
    children
        .iter()
        .filter_map(|a| match *a {
            Act::Notify { child, .. } | Act::NotifyReleased { child, .. } | Act::Released { child } => Some(child),
            Act::Hup { .. } => None,
        })
        .collect()
}

/// `wait4(pid, options)` by `caller`, through its request.
pub(super) fn wait4(t: &mut ProcTree, caller: u32, pid: i32, options: u32) -> Result<WaitReady, Error> {
    let request = t.wait4_request(caller, pid, options)?;
    let r = t.wait(request);
    t.check_invariants();
    r
}

/// `waitid(idtype, id, options)` by `caller`, through its request.
pub(super) fn waitid(t: &mut ProcTree, caller: u32, idtype: i32, id: i32, options: u32) -> Result<WaitReady, Error> {
    let request = t.waitid_request(caller, idtype, id, options)?;
    let r = t.wait(request);
    t.check_invariants();
    r
}

/// A reaped child that exited with `code` (slot 0).
pub(super) fn reaped(pid: u32, code: u8) -> Result<WaitReady, Error> {
    Ok(WaitReady::Reported(Report::Reaped { pid, status: ExitStatus::Exited(code), rusage_slot: slot(0) }))
}

/// A report.
pub(super) fn reported(report: Report) -> Result<WaitReady, Error> {
    Ok(WaitReady::Reported(report))
}

/// The pid a wait reported, if it reported one.
pub(super) fn reported_pid(r: &Result<WaitReady, Error>) -> Option<u32> {
    match r {
        Ok(WaitReady::Reported(report)) => Some(report.pid()),
        _ => None,
    }
}

/// SplitMix64: a small seeded generator, one seed one sequence.
pub(super) struct Rng(u64);

impl Rng {
    pub(super) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(super) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number in `0..n` (`n > 0`).
    pub(super) fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    pub(super) fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub(super) fn pick<T: Copy>(&mut self, items: &[T]) -> Option<T> {
        if items.is_empty() { None } else { Some(items[self.below(items.len() as u64) as usize]) }
    }
}
