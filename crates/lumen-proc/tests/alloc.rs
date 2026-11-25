//! The crate's Allocation section, checked with an allocator that counts
//! or refuses this thread's calls (the test harness's other threads
//! allocate when they like): `exit` and `fork` turn a refused reservation
//! into `ENOMEM` with the tree unchanged, and so does `add_thread`; `exit`
//! allocates nothing else, its process's threads still registered
//! included; a reaping `wait` and `remove_thread` never allocate; the calls documented as neither
//! allocating nor freeing call the allocator not at all; dropping an
//! `ExitOutcome`'s actions frees their list.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ptr;

use lumen_proc::{
    Controlling, Errno, Error, ExitError, ExitStatus, ProcTree, Report, RusageSlot, Sigttou, WCONTINUED, WEXITED,
    WNOHANG, WNOWAIT, WUNTRACED, WaitReady,
};
use lumen_signal::Signal;

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

fn slot() -> RusageSlot {
    RusageSlot::new(0)
}

fn tree() -> ProcTree {
    let mut t = ProcTree::new(1000);
    t.spawn_init(slot()).unwrap();
    t
}

/// `pid` exits to `reaper`, nobody autoreaping, its actions carried out.
fn exit(t: &mut ProcTree, pid: u32, reaper: u32) {
    t.exit(pid, ExitStatus::exited(0), Some(reaper), |_| false).unwrap().into_iter().for_each(drop);
}

/// `parent` gets a children list with room for 16: it forks 16 children,
/// then each exits and is reaped.
fn give_room(t: &mut ProcTree, parent: u32) {
    let kids: Vec<u32> = (0..16).map(|_| t.fork(parent, slot()).unwrap()).collect();
    for c in kids {
        exit(t, c, INIT);
        let request = t.wait4_request(parent, c as i32, 0).unwrap();
        assert!(matches!(t.wait(request), Ok(WaitReady::Reported(Report::Reaped { .. }))));
    }
    assert!(t.children(parent).is_empty());
}

/// A session led by `s`, its child `p` with `n` children, each a zombie
/// leading a group with a stopped member: `p`'s exit has three words per
/// child (a zombie's two and a hangup's one), and `2n + 1` actions.
fn zombie_groups(t: &mut ProcTree, n: usize) -> u32 {
    let s = t.fork(INIT, slot()).unwrap();
    t.setsid(s).unwrap();
    let p = t.fork(s, slot()).unwrap();
    for _ in 0..n {
        let c = t.fork(p, slot()).unwrap();
        t.setpgid(p, c as i32, 0).unwrap();
        let g = t.fork(c, slot()).unwrap();
        exit(t, c, INIT);
        let _ = t.stop(g, Signal::STOP).unwrap();
    }
    p
}

#[test]
fn exit_is_enomem_and_changes_nothing_when_the_reapers_list_cant_grow() {
    let mut t = tree();
    // Init's list full at its capacity (4), a with two children.
    let a = t.fork(INIT, slot()).unwrap();
    for _ in 0..3 {
        t.fork(INIT, slot()).unwrap();
    }
    t.fork(a, slot()).unwrap();
    t.fork(a, slot()).unwrap();
    let before = format!("{t:?}");
    let got = refused(|| t.exit(a, ExitStatus::exited(0), Some(INIT), |_| false).map(drop));
    assert_eq!(got, Err(ExitError::NoMem));
    assert_eq!(format!("{t:?}"), before);
    exit(&mut t, a, INIT);
}

#[test]
fn exit_is_enomem_and_changes_nothing_when_the_actions_cant_fit() {
    let mut t = tree();
    let r = t.fork(INIT, slot()).unwrap();
    give_room(&mut t, r);
    // Three children (a list of capacity 4) and nine words for them.
    let p = zombie_groups(&mut t, 3);
    let before = format!("{t:?}");
    let got = refused(|| t.exit(p, ExitStatus::exited(0), Some(r), |_| false).map(drop));
    assert_eq!(got, Err(ExitError::NoMem));
    assert_eq!(format!("{t:?}"), before);
    let actions = t.exit(p, ExitStatus::exited(0), Some(r), |_| false).unwrap();
    assert_eq!(actions.into_iter().count(), 7);
}

#[test]
fn exit_allocates_nothing_when_its_lists_have_room_and_its_actions_free() {
    let mut t = tree();
    let r = t.fork(INIT, slot()).unwrap();
    give_room(&mut t, r);
    // p's list of 4 (its capacity) has room for its actions' 3 words: a
    // zombie child's two and its group's hangup (the live ones, in p's
    // group, have none).
    let p = zombie_groups(&mut t, 1);
    for _ in 0..3 {
        t.fork(p, slot()).unwrap();
    }
    assert_eq!(t.children(p).len(), 4);
    let (out, allocs, _) = counted(|| t.exit(p, ExitStatus::exited(0), Some(r), |_| false).unwrap());
    assert_eq!(allocs, 0, "exit allocated");
    let (n, allocs, frees) = counted(|| out.into_iter().count());
    assert_eq!((n, allocs, frees), (3, 0, 1), "the actions' list is freed when they are done");
}

#[test]
fn an_autoreaping_exit_allocates_nothing_when_its_lists_have_room() {
    let mut t = tree();
    let p = t.fork(INIT, slot()).unwrap();
    let c = t.fork(p, slot()).unwrap();
    let z = t.fork(p, slot()).unwrap();
    exit(&mut t, z, INIT);
    // Init and p both autoreap: z and p leave inside exit. p's list of 4
    // holds z's four words (a released zombie's), init's has room for c.
    let (out, allocs, _) = counted(|| t.exit(p, ExitStatus::exited(0), Some(INIT), |_| true).unwrap());
    assert_eq!(allocs, 0, "exit allocated");
    assert_eq!(out.into_iter().count(), 2);
    assert!(!t.contains(p) && !t.contains(z) && t.contains(c));
}

#[test]
fn fork_is_enomem_and_changes_nothing_when_the_parents_list_cant_grow() {
    let mut t = tree();
    let a = t.fork(INIT, slot()).unwrap();
    let before = format!("{t:?}");
    assert_eq!(refused(|| t.fork(a, slot())), Err(Error::User(Errno::NoMem)));
    assert_eq!(format!("{t:?}"), before);
    assert!(t.fork(a, slot()).is_ok());
}

#[test]
fn a_reaping_wait_never_allocates() {
    let mut t = tree();
    let a = t.fork(INIT, slot()).unwrap();
    exit(&mut t, a, INIT);
    let ((), allocs, _) = counted(|| {
        let reap = t.wait4_request(INIT, a as i32, 0).unwrap();
        assert!(matches!(t.wait(reap), Ok(WaitReady::Reported(Report::Reaped { .. }))));
    });
    assert_eq!(allocs, 0, "allocated");
}

#[test]
fn the_calls_that_neither_allocate_nor_free_dont() {
    let mut t = tree();
    let b = t.fork(INIT, slot()).unwrap();
    let c = t.fork(INIT, slot()).unwrap();
    let z = t.fork(INIT, slot()).unwrap();
    exit(&mut t, z, INIT);
    let ((), allocs, frees) = counted(|| {
        let tty = Some(Controlling { session: Some(INIT), foreground: Some(INIT) });
        t.exec(c).unwrap();
        let _ = t.stop(b, Signal::STOP).unwrap();
        let stopped = t.wait4_request(INIT, -1, WUNTRACED).unwrap();
        assert!(matches!(t.wait(stopped), Ok(WaitReady::Reported(Report::Stopped { .. }))));
        let _ = t.cont(b).unwrap();
        let continued = t.wait4_request(INIT, b as i32, WCONTINUED).unwrap();
        assert!(matches!(t.wait(continued), Ok(WaitReady::Reported(Report::Continued { .. }))));
        let peek = t.waitid_request(INIT, lumen_proc::P_PID, z as i32, WEXITED | WNOWAIT).unwrap();
        assert!(matches!(t.wait(peek), Ok(WaitReady::Reported(Report::Zombie { .. }))));
        let none = t.wait4_request(INIT, b as i32, WNOHANG).unwrap();
        assert_eq!(t.wait(none), Ok(WaitReady::NoneReady));
        let echild = t.waitid_request(INIT, lumen_proc::P_PID, 999, WEXITED).unwrap();
        assert_eq!(t.wait(echild), Err(Error::User(Errno::Child)));
        assert_eq!(t.getpgid(c, 0), Ok(INIT));
        assert_eq!(t.getsid(c, b as i32), Ok(INIT));
        assert!(t.tcsetpgrp(c, tty, INIT as i32, Sigttou::Delivered).is_ok());
        let _ = (t.is_orphaned_pgrp(INIT), t.is_busy(INIT), t.pgrp_members(INIT).count(), t.children(INIT).len());
        let _ = t.threads(b).len();
        // A refused removal: the leader's tid.
        assert!(t.remove_thread(b, b).is_err());
    });
    assert_eq!((allocs, frees), (0, 0), "allocated or freed");
}

#[test]
fn add_thread_is_enomem_and_changes_nothing_when_its_list_cant_grow() {
    let mut t = tree();
    let a = t.fork(INIT, slot()).unwrap();
    let before = format!("{t:?}");
    assert_eq!(refused(|| t.add_thread(a)), Err(Error::User(Errno::NoMem)));
    assert_eq!(format!("{t:?}"), before);
    assert!(t.add_thread(a).is_ok());
}

#[test]
fn remove_thread_never_allocates() {
    let mut t = tree();
    let a = t.fork(INIT, slot()).unwrap();
    let tids: Vec<u32> = (0..8).map(|_| t.add_thread(a).unwrap()).collect();
    let ((), allocs, _) = counted(|| {
        for &tid in &tids {
            t.remove_thread(a, tid).unwrap();
        }
    });
    assert_eq!(allocs, 0, "allocated");
    assert!(t.threads(a).is_empty());
}

#[test]
fn exit_with_threads_still_registered_allocates_nothing() {
    let mut t = tree();
    let a = t.fork(INIT, slot()).unwrap();
    for _ in 0..8 {
        t.add_thread(a).unwrap();
    }
    let (out, allocs, frees) = counted(|| t.exit(a, ExitStatus::exited(0), Some(INIT), |_| false).unwrap());
    assert_eq!(allocs, 0, "exit allocated");
    assert!(frees > 0, "the threads' index entries and list weren't freed");
    out.into_iter().for_each(drop);
    assert!(t.threads(a).is_empty());
}
