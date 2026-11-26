//! Pid allocation: the range, wrapping, busy numbers, exhaustion.

use alloc::vec::Vec;

use super::{INIT, exit, fork, reported_pid, slot, user, wait4};
use crate::pid::PidAllocator;
use crate::{Errno, FIRST_PID, INIT_PID, ProcTree};

#[test]
fn pids_count_up_from_two_to_the_maximum_then_wrap() {
    let mut a = PidAllocator::new(5);
    assert_eq!(a.max_pid(), 5);
    let got: Vec<_> = (0..6).map(|_| a.alloc(|_| false).unwrap()).collect();
    assert_eq!(got, [2, 3, 4, 5, 2, 3]);
    assert_eq!((INIT_PID, FIRST_PID), (1, 2));
}

#[test]
fn busy_numbers_are_skipped_and_asked_about_once() {
    let mut a = PidAllocator::new(10);
    let mut asked = Vec::new();
    let busy = [2, 3, 5];
    let pid = a
        .alloc(|n| {
            asked.push(n);
            busy.contains(&n)
        })
        .unwrap();
    assert_eq!(pid, 4);
    assert_eq!(asked, [2, 3, 4]);
    asked.clear();
    let pid = a
        .alloc(|n| {
            asked.push(n);
            busy.contains(&n)
        })
        .unwrap();
    assert_eq!(pid, 6);
    assert_eq!(asked, [5, 6]);
}

#[test]
fn exhaustion_is_eagain_and_leaves_the_cursor() {
    let mut a = PidAllocator::new(6);
    assert_eq!(a.alloc(|_| false), Ok(2));
    let mut asked = Vec::new();
    assert_eq!(
        a.alloc(|n| {
            asked.push(n);
            true
        }),
        Err(Errno::Again)
    );
    // Each of 2..=6 asked once, starting after the last pid.
    assert_eq!(asked, [3, 4, 5, 6, 2]);
    assert_eq!(a.alloc(|_| false), Ok(3));
}

#[test]
fn a_maximum_below_two_hands_out_nothing() {
    for max in [0, 1] {
        let mut a = PidAllocator::new(max);
        assert_eq!(a.alloc(|_| false), Err(Errno::Again));
    }
    let mut a = PidAllocator::new(2);
    assert_eq!(a.alloc(|_| false), Ok(2));
    assert_eq!(a.alloc(|_| false), Ok(2));
    assert_eq!(a.alloc(|n| n == 2), Err(Errno::Again));
}

#[test]
#[should_panic(expected = "positive pid_t")]
fn a_maximum_past_i32_max_is_refused() {
    let _ = PidAllocator::new(i32::MAX as u32 + 1);
}

#[test]
fn the_largest_maximum_is_accepted() {
    let mut a = PidAllocator::new(i32::MAX as u32);
    assert_eq!(a.alloc(|_| false), Ok(2));
}

#[test]
fn pid_wrap_around_skips_live_pids_pgids_and_sids() {
    // Pids 2..=9.
    let mut t = ProcTree::new(9);
    t.spawn_init(slot(0)).unwrap();
    let a = fork(&mut t, INIT); // 2: stays live
    let b = fork(&mut t, INIT); // 3: leads group 3, then reaped; 4 stays in it
    t.setpgid(b, 0, 0).unwrap();
    let c = fork(&mut t, b); // 4
    let d = fork(&mut t, INIT); // 5: leads session 5, then reaped; 6 stays in it
    t.setsid(d).unwrap();
    let e = fork(&mut t, d); // 6
    t.setpgid(e, 0, 0).unwrap();
    let z = fork(&mut t, INIT); // 7: a zombie
    let f = fork(&mut t, INIT); // 8: reaped, free again
    assert_eq!((a, b, c, d, e, z, f), (2, 3, 4, 5, 6, 7, 8));
    exit(&mut t, b, 0);
    assert_eq!(reported_pid(&wait4(&mut t, INIT, b as i32, 0)), Some(b));
    exit(&mut t, d, 0);
    assert_eq!(reported_pid(&wait4(&mut t, INIT, d as i32, 0)), Some(d));
    exit(&mut t, z, 0);
    exit(&mut t, f, 0);
    assert_eq!(reported_pid(&wait4(&mut t, INIT, f as i32, 0)), Some(f));
    assert!(!t.contains(b) && t.pgrp_exists(b), "3 is busy only as a pgid");
    assert!(!t.contains(d) && !t.pgrp_exists(d) && t.is_busy(d), "5 is busy only as a sid");
    assert!(t.is_busy(z), "7 is a zombie's pid");
    assert!(!t.is_busy(f));

    // The cursor is at 8: 9, then wrapping to 2, past 2..=7, to 8.
    assert_eq!(fork(&mut t, INIT), 9);
    assert_eq!(fork(&mut t, INIT), 8);
    // Now 2..=9 are all busy.
    assert_eq!(t.fork(INIT, slot(0)), user(Errno::Again));
    assert_eq!(t.spawn_root(slot(0)), Err(crate::NoFreePid));
    t.check_invariants();
    // Reaping the zombie frees 7; moving 4 out of group 3 frees 3.
    assert_eq!(reported_pid(&wait4(&mut t, INIT, z as i32, 0)), Some(z));
    t.setsid(c).unwrap();
    assert!(!t.is_busy(b));
    assert_eq!(fork(&mut t, INIT), 3);
    assert_eq!(fork(&mut t, INIT), 7);
    assert_eq!(t.fork(INIT, slot(0)), user(Errno::Again));
}

#[test]
fn a_failed_fork_leaves_the_tree_unchanged() {
    let mut t = ProcTree::new(3);
    t.spawn_init(slot(0)).unwrap();
    fork(&mut t, INIT);
    fork(&mut t, INIT);
    let before = alloc::format!("{t:?}");
    assert_eq!(t.fork(INIT, slot(0)), user(Errno::Again));
    assert_eq!(alloc::format!("{t:?}"), before);
    assert_eq!(t.children(INIT).len(), 2);
}

#[test]
fn terminal_reference_keeps_a_reaped_foreground_group_from_pid_reuse() {
    let mut t = ProcTree::new(4);
    t.spawn_init(slot(0)).unwrap();
    let group = fork(&mut t, INIT);
    t.setpgid(group, 0, 0).unwrap();
    let foreground = t.retain_pid(group).unwrap();
    let other_terminal = t.retain_pid(group).unwrap();
    let in_flight_signal = foreground.clone();
    assert_eq!(t.pgrp_members(foreground.number()).collect::<Vec<_>>(), [group]);
    exit(&mut t, group, 0);
    assert_eq!(reported_pid(&wait4(&mut t, INIT, group as i32, 0)), Some(group));
    assert!(!t.contains(group) && !t.pgrp_exists(group));
    // Wrap the allocator while a terminal still names the reaped group.
    let next = fork(&mut t, INIT);
    t.setpgid(next, 0, 0).unwrap();
    assert_eq!(next, 3);
    assert_eq!(t.add_thread(next), Ok(4));
    assert_eq!(t.fork(INIT, slot(0)), user(Errno::Again));
    assert_eq!(t.add_thread(next), user(Errno::Again));
    assert!(t.pgrp_members(foreground.number()).next().is_none(), "terminal signal has no recipients");
    assert_eq!(t.pgrp_members(next).collect::<Vec<_>>(), [next]);
    drop(foreground);
    drop(other_terminal);
    assert!(t.is_busy(group), "an in-flight terminal signal still reserves the number");
    drop(in_flight_signal);
    assert!(!t.is_busy(group));
    assert!(t.retain_pid(group).is_none(), "a free number cannot be retained");
    assert_eq!(fork(&mut t, INIT), group, "reuse is safe after every terminal reference is gone");
    t.check_invariants();
}

#[test]
fn pid_allocation_prunes_expired_terminal_reservations() {
    let mut t = ProcTree::new(128);
    t.spawn_init(slot(0)).unwrap();
    let live = t.retain_pid(INIT).unwrap();
    for _ in 0..16 {
        let pid = fork(&mut t, INIT);
        // Inspect before retain_pid could hide a missing allocator prune.
        assert_eq!(t.retained_pid_entries(), 1, "PID allocation must prune expired reservations");
        let reference = t.retain_pid(pid).unwrap();
        assert_eq!(t.retained_pid_entries(), 2);
        exit(&mut t, pid, 0);
        assert_eq!(reported_pid(&wait4(&mut t, INIT, pid as i32, 0)), Some(pid));
        drop(reference);
    }
    fork(&mut t, INIT);
    assert_eq!(t.retained_pid_entries(), 1, "PID allocation must prune expired reservations");
    drop(live);
}

#[test]
fn retaining_a_pid_prunes_expired_terminal_reservations() {
    let mut t = ProcTree::new(128);
    t.spawn_init(slot(0)).unwrap();
    // Allocate distinct identities up front so no allocator prune can mask
    // a missing prune when a new terminal reservation is created.
    let pids: Vec<_> = (0..16).map(|_| fork(&mut t, INIT)).collect();
    let live = t.retain_pid(INIT).unwrap();
    for pid in pids {
        let reference = t.retain_pid(pid).unwrap();
        assert_eq!(t.retained_pid_entries(), 2, "retaining a PID must prune expired reservations");
        exit(&mut t, pid, 0);
        assert_eq!(reported_pid(&wait4(&mut t, INIT, pid as i32, 0)), Some(pid));
        drop(reference);
    }
    fork(&mut t, INIT);
    assert_eq!(t.retained_pid_entries(), 1);
    drop(live);
}
