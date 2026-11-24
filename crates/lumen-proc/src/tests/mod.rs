//! Host tests for the process-tree rules.

mod exit;
mod model;
mod pid;
mod session;
mod status;
mod wait;

use alloc::vec::Vec;

use crate::{Bug, Errno, Error, ExitAction, ExitStatus, ProcTree, Report, Signal, WaitReady};

pub(super) const INIT: u32 = 1;

/// A tree holding init (pid 1) with room for pids up to 1000.
pub(super) fn tree() -> ProcTree {
    let mut t = ProcTree::new(1000);
    assert_eq!(t.spawn_init(0), Ok(INIT));
    t
}

/// The signal numbered `n`.
pub(super) fn sig(n: u8) -> Signal {
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
    let pid = t.fork(parent, 0).expect("fork");
    t.check_invariants();
    pid
}

/// `pid` exits with `status`, `reaper` adopting its children: the actions.
pub(super) fn exit_with(t: &mut ProcTree, pid: u32, status: ExitStatus, reaper: u32) -> Vec<ExitAction> {
    let out = t.exit(pid, status, Some(reaper)).expect("exit");
    t.check_invariants();
    out.actions().collect()
}

/// `pid` exits with `code`, init adopting its children: the actions.
pub(super) fn exit(t: &mut ProcTree, pid: u32, code: i32) -> Vec<ExitAction> {
    exit_with(t, pid, ExitStatus::exited(code), INIT)
}

/// The groups an exit's actions hang up, in order.
pub(super) fn hangups(actions: &[ExitAction]) -> Vec<u32> {
    actions
        .iter()
        .filter_map(|a| match *a {
            ExitAction::HangupThenContinue { pgid } => Some(pgid),
            _ => None,
        })
        .collect()
}

/// The adopted zombies an exit's actions name, in order (every `Notify`
/// but the last action, which is the exiting process's own).
pub(super) fn adopted(actions: &[ExitAction]) -> Vec<u32> {
    let (_, children) = actions.split_last().expect("an exit has a last action");
    children
        .iter()
        .filter_map(|a| match *a {
            ExitAction::Notify { child, .. } => Some(child),
            _ => None,
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
    Ok(WaitReady::Reported(Report::Reaped { pid, status: ExitStatus::Exited(code), rusage_slot: 0 }))
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
