//! The crate's Allocation section, checked with an allocator that counts
//! or refuses this thread's calls (the test harness's other threads
//! allocate when they like): `exit` and `fork` turn a refused reservation
//! into `ENOMEM` with the tree unchanged; `exit` allocates nothing else;
//! `wait`, `release` and the calls that neither allocate nor free never
//! call the allocator; dropping an `ExitOutcome` frees.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ptr;

use lumen_proc::{
    Controlling, Errno, Error, ExitStatus, ProcTree, Report, Signal, Sigttou, WNOHANG, WUNTRACED, WaitReady,
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Calls pass through, uncounted.
    Off,
    /// Calls pass through, counted.
    Count,
    /// Allocations fail (null); frees pass through.
    Refuse,
}

thread_local! {
    // Const-initialized `Cell`s: reading them never allocates.
    static MODE: Cell<Mode> = const { Cell::new(Mode::Off) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    static FREES: Cell<usize> = const { Cell::new(0) };
}

struct Checking;

impl Checking {
    /// Whether this allocation goes ahead (counting it if counted).
    fn allow() -> bool {
        match MODE.with(Cell::get) {
            Mode::Off => true,
            Mode::Count => {
                ALLOCS.with(|n| n.set(n.get() + 1));
                true
            }
            Mode::Refuse => false,
        }
    }
}

// SAFETY: forwards to the system allocator, only counting calls or
// returning null (an allowed allocation failure) in place of allocating.
unsafe impl GlobalAlloc for Checking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !Self::allow() {
            return ptr::null_mut();
        }
        // SAFETY: same contract as ours.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if MODE.with(Cell::get) == Mode::Count {
            FREES.with(|n| n.set(n.get() + 1));
        }
        // SAFETY: same contract as ours.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if !Self::allow() {
            return ptr::null_mut();
        }
        // SAFETY: same contract as ours.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Checking = Checking;

/// Runs `f` with this thread's allocations counted: its result, the
/// allocations and the frees.
fn counted<T>(f: impl FnOnce() -> T) -> (T, usize, usize) {
    ALLOCS.with(|n| n.set(0));
    FREES.with(|n| n.set(0));
    MODE.with(|m| m.set(Mode::Count));
    let out = f();
    MODE.with(|m| m.set(Mode::Off));
    (out, ALLOCS.with(Cell::get), FREES.with(Cell::get))
}

/// Runs `f` with this thread's allocations refused.
fn refused<T>(f: impl FnOnce() -> T) -> T {
    MODE.with(|m| m.set(Mode::Refuse));
    let out = f();
    MODE.with(|m| m.set(Mode::Off));
    out
}

const INIT: u32 = 1;

fn tree() -> ProcTree {
    let mut t = ProcTree::new(1000);
    t.spawn_init(0).unwrap();
    t
}

fn exit(t: &mut ProcTree, pid: u32, reaper: u32) {
    t.exit(pid, ExitStatus::exited(0), Some(reaper)).unwrap().actions().for_each(drop);
}

/// A reaper `r` (init's child) whose children list has room for 16 more.
fn roomy_reaper(t: &mut ProcTree) -> u32 {
    let r = t.fork(INIT, 0).unwrap();
    for _ in 0..16 {
        let c = t.fork(r, 0).unwrap();
        exit(t, c, INIT);
        let request = t.wait4_request(r, c as i32, 0).unwrap();
        assert!(matches!(t.wait(request), Ok(WaitReady::Reported(Report::Reaped { .. }))));
    }
    assert!(t.children(r).is_empty());
    r
}

/// A session led by `s`, its child `p` with `n` children, each a zombie
/// leading a group with a stopped member: `p`'s exit has `2n + 1` actions.
fn zombie_groups(t: &mut ProcTree, n: usize) -> u32 {
    let s = t.fork(INIT, 0).unwrap();
    t.setsid(s).unwrap();
    let p = t.fork(s, 0).unwrap();
    for _ in 0..n {
        let c = t.fork(p, 0).unwrap();
        t.setpgid(p, c as i32, 0).unwrap();
        let g = t.fork(c, 0).unwrap();
        exit(t, c, INIT);
        let _ = t.stop(g, Signal::SIGSTOP).unwrap();
    }
    p
}

#[test]
fn exit_is_enomem_and_changes_nothing_when_the_reapers_list_cant_grow() {
    let mut t = tree();
    // Init's list full at its capacity (4), a with two children.
    let a = t.fork(INIT, 0).unwrap();
    for _ in 0..3 {
        t.fork(INIT, 0).unwrap();
    }
    t.fork(a, 0).unwrap();
    t.fork(a, 0).unwrap();
    let before = format!("{t:?}");
    let got = refused(|| t.exit(a, ExitStatus::exited(0), Some(INIT)).map(drop));
    assert_eq!(got, Err(Error::User(Errno::NoMem)));
    assert_eq!(format!("{t:?}"), before);
    exit(&mut t, a, INIT);
}

#[test]
fn exit_is_enomem_and_changes_nothing_when_the_actions_cant_fit() {
    let mut t = tree();
    let r = roomy_reaper(&mut t);
    // Three children (a list of capacity 4), six actions for them.
    let p = zombie_groups(&mut t, 3);
    let before = format!("{t:?}");
    let got = refused(|| t.exit(p, ExitStatus::exited(0), Some(r)).map(drop));
    assert_eq!(got, Err(Error::User(Errno::NoMem)));
    assert_eq!(format!("{t:?}"), before);
    let out = t.exit(p, ExitStatus::exited(0), Some(r)).unwrap();
    assert_eq!(out.actions().count(), 7);
}

#[test]
fn exit_allocates_nothing_when_its_lists_have_room_and_its_outcome_frees() {
    let mut t = tree();
    let r = roomy_reaper(&mut t);
    // Two zombie children leading groups (two actions each) and two live
    // ones in p's group (none): four actions fit p's list of four.
    let p = zombie_groups(&mut t, 2);
    t.fork(p, 0).unwrap();
    t.fork(p, 0).unwrap();
    assert_eq!(t.children(p).len(), 4);
    let (out, allocs, _) = counted(|| t.exit(p, ExitStatus::exited(0), Some(r)).unwrap());
    assert_eq!(allocs, 0, "exit allocated");
    let (n, allocs, frees) = counted(|| out.actions().count());
    assert_eq!((n, allocs, frees), (5, 0, 0));
    let ((), allocs, frees) = counted(|| drop(out));
    assert_eq!((allocs, frees), (0, 1), "dropping the outcome frees its list");
}

#[test]
fn fork_is_enomem_and_changes_nothing_when_the_parents_list_cant_grow() {
    let mut t = tree();
    let a = t.fork(INIT, 0).unwrap();
    let before = format!("{t:?}");
    assert_eq!(refused(|| t.fork(a, 0)), Err(Error::User(Errno::NoMem)));
    assert_eq!(format!("{t:?}"), before);
    assert!(t.fork(a, 0).is_ok());
}

#[test]
fn wait_release_and_the_lookups_never_allocate() {
    let mut t = tree();
    let a = t.fork(INIT, 0).unwrap();
    let b = t.fork(INIT, 0).unwrap();
    let c = t.fork(INIT, 0).unwrap();
    let r = t.spawn_root(9).unwrap();
    exit(&mut t, a, INIT);
    exit(&mut t, r, INIT);
    let (_, allocs, _) = counted(|| {
        let tty = Some(Controlling { session: Some(INIT), foreground: Some(INIT) });
        t.exec(c).unwrap();
        let _ = t.stop(b, Signal::SIGSTOP).unwrap();
        let reap = t.wait4_request(INIT, a as i32, 0).unwrap();
        assert!(matches!(t.wait(reap), Ok(WaitReady::Reported(Report::Reaped { .. }))));
        let stopped = t.wait4_request(INIT, -1, WUNTRACED).unwrap();
        assert!(matches!(t.wait(stopped), Ok(WaitReady::Reported(Report::Stopped { .. }))));
        let _ = t.cont(b).unwrap();
        let none = t.wait4_request(INIT, -1, WNOHANG).unwrap();
        assert_eq!(t.wait(none), Ok(WaitReady::NoneReady));
        let echild = t.waitid_request(INIT, lumen_proc::P_PID, 999, lumen_proc::WEXITED).unwrap();
        assert_eq!(t.wait(echild), Err(Error::User(Errno::Child)));
        assert_eq!(t.release(r, 9), Ok(ExitStatus::Exited(0)));
        assert_eq!(t.getpgid(c, 0), Ok(INIT));
        assert_eq!(t.getsid(c, b as i32), Ok(INIT));
        assert!(t.tcsetpgrp(c, tty, INIT as i32, Sigttou::Delivered).is_ok());
        let _ = (t.is_orphaned_pgrp(INIT), t.is_busy(INIT), t.rusage_slot(c));
    });
    assert_eq!(allocs, 0, "allocated");
}
