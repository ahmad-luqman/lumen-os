//! Exit: zombies, reparenting, adopted zombies, orphaned process groups.

use alloc::vec;
use alloc::vec::Vec;

use super::{INIT, exit, fork, sig, tree, wait4};
use crate::{Errno, ExitOutcome, ExitStatus, ProcState, ProcTree, WaitResult};

#[test]
fn exit_makes_a_zombie_and_names_the_parent_to_signal() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let b = fork(&mut t, a);
    let out = exit(&mut t, b, 3);
    assert_eq!(out, ExitOutcome { parent: Some(a), adopted_zombies: vec![], orphaned_pgrps: vec![] });
    assert_eq!(t.state(b), Some(ProcState::Zombie(ExitStatus::Exited(3))));
    assert_eq!(t.parent(b), Some(Some(a)));
    assert_eq!(t.children(a), &[b]);
}

#[test]
fn children_move_to_the_reaper_in_their_order() {
    let mut t = tree();
    let older = fork(&mut t, INIT);
    let a = fork(&mut t, INIT);
    let x = fork(&mut t, a);
    let y = fork(&mut t, a);
    let z = fork(&mut t, a);
    let out = exit(&mut t, a, 0);
    assert_eq!(out.parent, Some(INIT));
    assert_eq!(t.children(INIT), &[older, a, x, y, z]);
    assert_eq!(t.children(a), &[] as &[u32]);
    for c in [x, y, z] {
        assert_eq!(t.parent(c), Some(Some(INIT)));
    }
}

#[test]
fn the_reaper_need_not_be_init() {
    let mut t = tree();
    let r = fork(&mut t, INIT);
    let a = fork(&mut t, INIT);
    let x = fork(&mut t, a);
    let out = t.exit(a, ExitStatus::exited(0), r).unwrap();
    t.check_invariants();
    assert_eq!(out.parent, Some(INIT));
    assert_eq!(t.children(r), &[x]);
    assert_eq!(t.parent(x), Some(Some(r)));
    exit(&mut t, x, 9);
    assert_eq!(wait4(&mut t, r, -1, 0).pid(), Some(x));
}

#[test]
fn an_orphan_that_was_already_a_zombie_is_reported_to_the_reaper() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let x = fork(&mut t, a);
    let y = fork(&mut t, a);
    let z = fork(&mut t, a);
    exit(&mut t, z, 3);
    exit(&mut t, x, 1);
    let out = exit(&mut t, a, 0);
    assert_eq!(out.adopted_zombies, vec![x, z]);
    assert_eq!(t.parent(x), Some(Some(INIT)));
    // The reaper reaps them: the dead parent first (older), then x, then z.
    assert_eq!(wait4(&mut t, INIT, -1, 0).pid(), Some(a));
    assert_eq!(
        wait4(&mut t, INIT, -1, 0),
        WaitResult::Reaped { pid: x, status: ExitStatus::Exited(1), rusage_slot: 0 }
    );
    assert_eq!(wait4(&mut t, INIT, -1, 0).pid(), Some(z));
    assert_eq!(wait4(&mut t, INIT, -1, crate::WNOHANG), WaitResult::NoneReady);
    assert_eq!(t.parent(y), Some(Some(INIT)));
}

#[test]
fn a_bad_reaper_is_einval_and_changes_nothing() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let x = fork(&mut t, a);
    let gx = fork(&mut t, x);
    let dead = fork(&mut t, INIT);
    exit(&mut t, dead, 0);
    for reaper in [a, x, gx, dead, 999] {
        let before = t.clone();
        assert_eq!(t.exit(a, ExitStatus::exited(0), reaper), Err(Errno::Inval), "reaper {reaper}");
        assert_eq!(alloc::format!("{t:?}"), alloc::format!("{before:?}"));
    }
    // A childless process needs no reaper.
    assert!(t.exit(gx, ExitStatus::exited(0), 999).is_ok());
    t.check_invariants();
}

#[test]
fn only_a_live_process_can_exit() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    exit(&mut t, a, 0);
    assert_eq!(t.exit(a, ExitStatus::exited(1), INIT), Err(Errno::Srch));
    assert_eq!(t.exit(555, ExitStatus::exited(1), INIT), Err(Errno::Srch));
    assert_eq!(t.state(a), Some(ProcState::Zombie(ExitStatus::Exited(0))));
}

#[test]
fn transitions_need_a_live_process() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    exit(&mut t, a, 0);
    assert_eq!(t.fork(a, 0), Err(Errno::Srch));
    assert_eq!(t.fork(555, 0), Err(Errno::Srch));
    assert_eq!(t.exec(a), Err(Errno::Srch));
    assert_eq!(t.stop(a, sig(19)), Err(Errno::Srch));
    assert_eq!(t.cont(a), Err(Errno::Srch));
    assert_eq!(t.setsid(a), Err(Errno::Srch));
    assert_eq!(t.setpgid(a, 0, 0), Err(Errno::Srch));
    t.check_invariants();
}

#[test]
fn a_root_has_no_parent_and_is_released_not_waited_for() {
    let mut t = tree();
    let r = t.spawn_root(5).unwrap();
    assert_eq!(t.parent(r), Some(None));
    assert_eq!((t.pgid(r), t.sid(r)), (Some(r), Some(r)));
    let c = fork(&mut t, r);
    assert_eq!(t.release(r), Err(Errno::Srch), "a live process isn't released");
    let out = t.exit(r, ExitStatus::exited(2), INIT).unwrap();
    assert_eq!(out.parent, None);
    assert_eq!(t.parent(c), Some(Some(INIT)));
    assert_eq!(t.release(r), Ok((ExitStatus::Exited(2), 5)));
    assert!(!t.contains(r));
    assert_eq!(t.release(r), Err(Errno::Srch));
    t.check_invariants();
}

#[test]
fn release_takes_a_zombie_out_of_its_parents_children() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let b = fork(&mut t, INIT);
    exit(&mut t, a, 0);
    assert_eq!(t.release(a), Ok((ExitStatus::Exited(0), 0)));
    assert_eq!(t.children(INIT), &[b]);
    t.check_invariants();
}

#[test]
fn init_is_pid_one_once() {
    let mut t = ProcTree::new(10);
    assert_eq!(t.spawn_init(0), Ok(1));
    assert_eq!(t.spawn_init(0), Err(Errno::Again));
    assert_eq!(t.parent(1), Some(None));
    assert_eq!((t.pgid(1), t.sid(1)), (Some(1), Some(1)));
}

/// A session led by `s` (a child of init), a job group `j` (a child of `s`
/// leading its own group) with a member `k` (j's child).
fn session_with_job() -> (ProcTree, u32, u32, u32) {
    let mut t = tree();
    let s = fork(&mut t, INIT);
    t.setsid(s).unwrap();
    let j = fork(&mut t, s);
    t.setpgid(s, j as i32, 0).unwrap();
    let k = fork(&mut t, j);
    t.check_invariants();
    (t, s, j, k)
}

#[test]
fn a_group_with_a_stopped_member_is_orphaned_by_its_last_outside_parents_exit() {
    let (mut t, s, j, k) = session_with_job();
    assert!(!t.is_orphaned_pgrp(j));
    t.stop(k, sig(20)).unwrap();
    let out = exit(&mut t, s, 0);
    assert!(t.is_orphaned_pgrp(j));
    assert_eq!(out.orphaned_pgrps, vec![j]);
}

#[test]
fn a_member_with_an_outside_parent_orphans_its_own_group_by_exiting() {
    let (mut t, _s, j, k) = session_with_job();
    let k2 = fork(&mut t, j);
    t.stop(k2, sig(19)).unwrap();
    // j is the group's link (its parent s is in group s); k and k2 go to init.
    let out = exit(&mut t, j, 0);
    assert!(t.is_orphaned_pgrp(j));
    assert_eq!(out.orphaned_pgrps, vec![j]);
    assert_eq!(t.parent(k), Some(Some(INIT)));
}

#[test]
fn a_group_is_not_orphaned_while_a_members_parent_is_in_another_group_of_the_session() {
    let (mut t, s, j, k) = session_with_job();
    // m: a child of t2 (group s), moved into group j.
    let t2 = fork(&mut t, s);
    let m = fork(&mut t, t2);
    t.setpgid(t2, m as i32, j as i32).unwrap();
    t.stop(m, sig(19)).unwrap();
    t.stop(k, sig(19)).unwrap();
    let out = exit(&mut t, s, 0);
    assert!(!t.is_orphaned_pgrp(j), "m's parent t2 is in group s of the same session");
    assert_eq!(out.orphaned_pgrps, Vec::<u32>::new());
    // t2's exit removes that last link: a child's group, orphaned now.
    let out = exit(&mut t, t2, 0);
    assert!(t.is_orphaned_pgrp(j));
    assert_eq!(out.orphaned_pgrps, vec![j]);
}

#[test]
fn an_orphaned_group_without_a_stopped_member_is_not_signalled() {
    let (mut t, s, j, _k) = session_with_job();
    let out = exit(&mut t, s, 0);
    assert!(t.is_orphaned_pgrp(j));
    assert_eq!(out.orphaned_pgrps, Vec::<u32>::new());
}

#[test]
fn a_group_already_orphaned_is_not_signalled_again() {
    let (mut t, s, j, k) = session_with_job();
    let k2 = fork(&mut t, j);
    exit(&mut t, s, 0);
    t.stop(k, sig(19)).unwrap();
    assert!(t.is_orphaned_pgrp(j));
    let out = exit(&mut t, k2, 0);
    assert_eq!(out.orphaned_pgrps, Vec::<u32>::new());
}

#[test]
fn a_parent_in_another_session_is_no_link() {
    let mut t = tree();
    let s = fork(&mut t, INIT);
    t.setsid(s).unwrap();
    let d = fork(&mut t, s);
    t.setsid(d).unwrap();
    let e = fork(&mut t, d);
    t.stop(e, sig(19)).unwrap();
    // Group d's only outside parent (s) is in another session.
    assert!(t.is_orphaned_pgrp(d));
    let out = exit(&mut t, s, 0);
    assert_eq!(out.orphaned_pgrps, Vec::<u32>::new());
}

#[test]
fn a_parent_that_is_init_is_no_link_but_another_parent_is() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    t.setpgid(a, 0, 0).unwrap();
    // a's parent is init, in group 1 of the same session: no link.
    assert!(t.is_orphaned_pgrp(a));
    let r = t.spawn_root(0).unwrap();
    let b = fork(&mut t, r);
    t.setpgid(b, 0, 0).unwrap();
    // b's parent r (group r, same session r) is a link.
    assert!(!t.is_orphaned_pgrp(b));
    // A root has no parent: its own group has no link.
    assert!(t.is_orphaned_pgrp(r));
}

#[test]
fn zombies_do_not_keep_a_group_from_being_orphaned() {
    let (mut t, s, j, k) = session_with_job();
    // z: another of s's children, in group j, then a zombie.
    let z = fork(&mut t, s);
    t.setpgid(s, z as i32, j as i32).unwrap();
    exit(&mut t, z, 0);
    t.stop(k, sig(19)).unwrap();
    let out = exit(&mut t, s, 0);
    assert_eq!(out.orphaned_pgrps, vec![j]);
    assert_eq!(out.adopted_zombies, vec![z]);
}

#[test]
fn both_paths_report_each_group_once_childrens_first() {
    let mut t = tree();
    let s = fork(&mut t, INIT);
    t.setsid(s).unwrap();
    // p: in its own group, its parent s (group s) its link.
    let p = fork(&mut t, s);
    t.setpgid(p, 0, 0).unwrap();
    let p2 = fork(&mut t, p);
    t.stop(p2, sig(19)).unwrap();
    // p's children c1, c3 in group g; c2 in group h; both linked only by p.
    let c1 = fork(&mut t, p);
    let c2 = fork(&mut t, p);
    let c3 = fork(&mut t, p);
    t.setpgid(p, c2 as i32, 0).unwrap();
    t.setpgid(p, c1 as i32, 0).unwrap();
    t.setpgid(p, c3 as i32, c1 as i32).unwrap();
    t.stop(c3, sig(19)).unwrap();
    t.stop(c2, sig(19)).unwrap();
    let out = exit(&mut t, p, 0);
    assert_eq!(out.orphaned_pgrps, vec![c1, c2, p]);
}
