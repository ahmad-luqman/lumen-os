//! Threads as numbers: tids from the pid allocator, counted busy while they
//! live, freed by `remove_thread` or the process's exit.

use alloc::vec::Vec;

use super::{INIT, bug, exit, fork, slot, tree, user};
use crate::{Bug, Controlling, Errno, ProcTree, Role, Sigttou, Tcsetpgrp};

#[test]
fn a_tid_is_handed_out_as_the_next_pid_would_be() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    assert_eq!(a, 2);
    // The next number after the last one handed out, pid or tid alike.
    assert_eq!(t.add_thread(a), Ok(3));
    assert_eq!(t.add_thread(a), Ok(4));
    t.check_invariants();
    assert_eq!(fork(&mut t, INIT), 5);
    assert_eq!(t.add_thread(INIT), Ok(6));
    assert_eq!(t.threads(a), [3, 4]);
    assert_eq!(t.threads(INIT), [6]);
    assert_eq!(t.threads(5), [] as [u32; 0]);
    // Threads are no processes: the tree holds init, a and the fork.
    assert_eq!((t.len(), t.is_empty()), (3, false));
    assert_eq!(t.threads(999), [] as [u32; 0]);
    t.check_invariants();
}

#[test]
fn a_live_tid_is_busy_so_no_pid_or_tid_reuses_it() {
    // Pids 2..=5: a process (2) with threads 3 and 4 leaves one number free.
    let mut t = ProcTree::new(5);
    t.spawn_init(slot(0)).unwrap();
    let a = fork(&mut t, INIT);
    let (t3, t4) = (t.add_thread(a).unwrap(), t.add_thread(a).unwrap());
    assert_eq!((t3, t4), (3, 4));
    assert!(t.is_busy(t3) && t.is_busy(t4));
    assert_eq!(fork(&mut t, INIT), 5);
    // Every number is taken: a pid, two tids and a pid.
    assert_eq!(t.fork(INIT, slot(0)), user(Errno::Again));
    assert_eq!(t.add_thread(a), user(Errno::Again));
    assert_eq!(t.spawn_root(slot(0)), Err(crate::NoFreePid));
    // Removing thread 4 frees exactly 4, for the next fork after the wrap.
    assert_eq!(t.remove_thread(a, t4), Ok(()));
    assert!(!t.is_busy(t4) && t.is_busy(t3));
    assert_eq!(t.threads(a), [t3]);
    t.check_invariants();
    assert_eq!(fork(&mut t, INIT), t4);
    t.check_invariants();
}

#[test]
fn exhaustion_leaves_the_tree_unchanged() {
    let mut t = ProcTree::new(3);
    t.spawn_init(slot(0)).unwrap();
    let a = fork(&mut t, INIT);
    let tid = t.add_thread(a).unwrap();
    assert_eq!(t.add_thread(a), user(Errno::Again));
    assert_eq!(t.threads(a), [tid]);
    t.check_invariants();
}

#[test]
fn only_a_live_process_gets_a_thread() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    assert_eq!(t.add_thread(77), bug(Bug::NotLive { pid: 77, role: Role::Caller }));
    let _ = exit(&mut t, a, 0);
    // A zombie gets none either; nothing was handed out.
    assert_eq!(t.add_thread(a), bug(Bug::NotLive { pid: a, role: Role::Caller }));
    t.check_invariants();
    assert_eq!(fork(&mut t, INIT), a + 1);
}

#[test]
fn remove_thread_refuses_what_isnt_that_process_thread() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let b = fork(&mut t, INIT);
    let ta = t.add_thread(a).unwrap();
    let tb = t.add_thread(b).unwrap();
    // The leader's tid is the pid: the process keeps it until it is reaped.
    assert_eq!(t.remove_thread(a, a), bug(Bug::NotThread { pid: a, tid: a }));
    // Another process's thread, a number no thread has, a process not in
    // the tree.
    assert_eq!(t.remove_thread(a, tb), bug(Bug::NotThread { pid: a, tid: tb }));
    assert_eq!(t.remove_thread(a, 500), bug(Bug::NotThread { pid: a, tid: 500 }));
    assert_eq!(t.remove_thread(77, ta), bug(Bug::NotThread { pid: 77, tid: ta }));
    // Unchanged by the refusals.
    assert_eq!((t.threads(a), t.threads(b)), ([ta].as_slice(), [tb].as_slice()));
    assert!(t.is_busy(ta) && t.is_busy(tb));
    t.check_invariants();
    // Removed once, a second removal is refused.
    assert_eq!(t.remove_thread(a, ta), Ok(()));
    assert_eq!(t.remove_thread(a, ta), bug(Bug::NotThread { pid: a, tid: ta }));
    t.check_invariants();
}

#[test]
fn exit_frees_the_threads_still_registered() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let tids: Vec<u32> = (0..3).map(|_| t.add_thread(a).unwrap()).collect();
    assert_eq!(t.remove_thread(a, tids[1]), Ok(()));
    let _ = exit(&mut t, a, 0);
    t.check_invariants();
    // The zombie keeps its pid, not its threads.
    assert!(t.is_busy(a));
    assert_eq!(t.threads(a), [] as [u32; 0]);
    for &tid in &tids {
        assert!(!t.is_busy(tid), "tid {tid} still busy after its process exited");
        assert_eq!(t.remove_thread(a, tid), bug(Bug::NotThread { pid: a, tid }));
    }
}

#[test]
fn a_tid_names_no_process_for_the_group_and_session_calls() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let tid = t.add_thread(a).unwrap();
    // getpgid, getsid and setpgid look processes up by pid: ESRCH, as for
    // a number nobody uses.
    assert_eq!(t.getpgid(a, tid as i32), user(Errno::Srch));
    assert_eq!(t.getsid(a, tid as i32), user(Errno::Srch));
    assert_eq!(t.setpgid(INIT, tid as i32, 0), user(Errno::Srch));
    // A tid is no group: moving into it is EPERM (a group elsewhere or
    // nowhere).
    assert_eq!(t.setpgid(a, 0, tid as i32), user(Errno::Perm));
    t.check_invariants();
}

#[test]
fn tcsetpgrp_to_a_tid_is_eperm_not_esrch() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let sid = t.setsid(a).unwrap();
    let tty = Some(Controlling { session: Some(sid), foreground: Some(a) });
    let tid = t.add_thread(a).unwrap();
    // The number is in use (a live tid), but not a group of the session.
    assert_eq!(t.tcsetpgrp(a, tty, tid as i32, Sigttou::Delivered), user(Errno::Perm));
    // Positive control: its own group is set; a free number is ESRCH.
    assert_eq!(t.tcsetpgrp(a, tty, a as i32, Sigttou::Delivered), Ok(Tcsetpgrp::Set(a)));
    let free = tid + 1;
    assert!(!t.is_busy(free));
    assert_eq!(t.tcsetpgrp(a, tty, free as i32, Sigttou::Delivered), user(Errno::Srch));
    // Once the thread is gone its number is free: ESRCH again.
    assert_eq!(t.remove_thread(a, tid), Ok(()));
    assert_eq!(t.tcsetpgrp(a, tty, tid as i32, Sigttou::Delivered), user(Errno::Srch));
}

#[test]
fn a_refused_exit_keeps_the_threads_registered() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let child = fork(&mut t, a);
    let tids: Vec<u32> = (0..3).map(|_| t.add_thread(a).unwrap()).collect();
    let before = alloc::format!("{t:?}");
    // Its own child can't reap its children: the exit is refused.
    let got = t.exit(a, crate::ExitStatus::exited(0), Some(child), |_| false).map(drop);
    assert_eq!(got.unwrap_err(), crate::ExitError::Bug(Bug::BadReaper { pid: a, reaper: child }));
    assert_eq!(alloc::format!("{t:?}"), before);
    assert_eq!(t.threads(a), tids.as_slice());
    assert!(tids.iter().all(|&tid| t.is_busy(tid)));
    t.check_invariants();
}

#[test]
#[should_panic(expected = "still registered")]
fn exec_with_threads_registered_is_a_bug() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let _tid = t.add_thread(a).unwrap();
    let _ = t.exec(a);
}

#[test]
fn not_thread_names_both_numbers() {
    let text = alloc::format!("{}", Bug::NotThread { pid: 4, tid: 9 });
    assert_eq!(text, "9 isn't a thread 4 added");
}

#[test]
fn nproc_task_count_tracks_processes_threads_and_zombies_without_init() {
    use super::{exit_full, reaped, wait4, waitid};
    use crate::{ExitStatus, P_PID, Report, WEXITED, WNOWAIT, WaitReady};

    let mut t = ProcTree::new(1000);
    assert_eq!(t.nproc_task_count(), 0);
    assert!(!t.allows_new_task(0));
    assert!(t.allows_new_task(1));

    // Pid 1 (init) is kernel-owned and excluded from NPROC accounting.
    t.spawn_init(slot(0)).unwrap();
    assert_eq!(t.nproc_task_count(), 0);
    assert!(!t.allows_new_task(0));
    assert!(t.allows_new_task(1));

    // Root process counts as 1 task.
    let root = t.spawn_root(slot(0)).unwrap();
    assert_eq!(t.nproc_task_count(), 1);
    assert!(!t.allows_new_task(0));
    assert!(!t.allows_new_task(1));
    assert!(t.allows_new_task(2));
    assert!(t.allows_new_task(u64::MAX));

    // Forked child and non-leader threads each add 1.
    let child = fork(&mut t, root);
    assert_eq!(t.nproc_task_count(), 2);
    assert!(!t.allows_new_task(2));
    assert!(t.allows_new_task(3));

    let tid1 = t.add_thread(root).unwrap();
    let tid2 = t.add_thread(child).unwrap();
    assert_eq!(t.nproc_task_count(), 4);
    assert!(!t.allows_new_task(4));
    assert!(t.allows_new_task(5));

    // Removing a non-leader thread drops the count immediately.
    t.remove_thread(root, tid1).unwrap();
    assert_eq!(t.nproc_task_count(), 3);

    // Exiting child frees its extra thread, but the unreaped zombie still counts as 1.
    let _ = exit(&mut t, child, 0);
    assert_eq!(t.nproc_task_count(), 2);
    assert!(!t.allows_new_task(2));
    assert!(t.allows_new_task(3));
    assert!(!t.is_busy(tid2));

    // WNOWAIT peeks without reaping, so the count stays 2.
    assert!(matches!(
        waitid(&mut t, root, P_PID, child as i32, WEXITED | WNOWAIT),
        Ok(WaitReady::Reported(Report::Zombie { .. }))
    ));
    assert_eq!(t.nproc_task_count(), 2);

    // Reaping removes the zombie and frees its slot.
    assert_eq!(wait4(&mut t, root, child as i32, 0), reaped(child, 0));
    assert_eq!(t.nproc_task_count(), 1);
    assert!(t.allows_new_task(2));

    // An autoreaped child drops immediately on exit.
    let auto = fork(&mut t, root);
    assert_eq!(t.nproc_task_count(), 2);
    let _ = exit_full(&mut t, auto, ExitStatus::exited(0), Some(INIT), &[root]);
    assert_eq!(t.nproc_task_count(), 1);

    // Exiting the root removes it immediately (no parent).
    let _ = exit_full(&mut t, root, ExitStatus::exited(0), Some(INIT), &[]);
    assert_eq!(t.nproc_task_count(), 0);
}
