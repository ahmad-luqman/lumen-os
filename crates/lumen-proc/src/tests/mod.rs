//! Host tests for the process-tree rules.

mod exit;
mod model;
mod pid;
mod session;
mod status;
mod wait;

use crate::{ExitOutcome, ExitStatus, ProcTree, Signal, WaitOptions, WaitResult, WaitTarget};

pub(super) const INIT: u32 = 1;

/// A tree holding init (pid 1) with room for pids up to 1000.
pub(super) fn tree() -> ProcTree {
    let mut t = ProcTree::new(1000);
    assert_eq!(t.spawn_init(0), Ok(INIT));
    t
}

pub(super) fn sig(n: u8) -> Signal {
    Signal::new(n).expect("a valid signal")
}

/// Forks a child of `parent` (rusage slot 0).
pub(super) fn fork(t: &mut ProcTree, parent: u32) -> u32 {
    let pid = t.fork(parent, 0).expect("fork");
    t.check_invariants();
    pid
}

/// `pid` exits with `code`, init reaping its children.
pub(super) fn exit(t: &mut ProcTree, pid: u32, code: i32) -> ExitOutcome {
    let out = t.exit(pid, ExitStatus::exited(code), INIT).expect("exit");
    t.check_invariants();
    out
}

/// `wait4(pid, options)` by `caller`.
pub(super) fn wait4(t: &mut ProcTree, caller: u32, pid: i32, options: u32) -> WaitResult {
    let target = WaitTarget::from_wait4_pid(pid, t.pgid(caller).unwrap_or(0)).expect("target");
    let r = t.wait(caller, target, WaitOptions::wait4(options));
    t.check_invariants();
    r
}

/// `waitid(idtype, id, options)` by `caller`.
pub(super) fn waitid(t: &mut ProcTree, caller: u32, idtype: i32, id: i32, options: u32) -> WaitResult {
    let target = WaitTarget::from_waitid(idtype, id, t.pgid(caller).unwrap_or(0)).expect("target");
    let r = t.wait(caller, target, WaitOptions::waitid(options));
    t.check_invariants();
    r
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
