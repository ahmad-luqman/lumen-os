//! Exit: zombies, reparenting, adopted zombies, parentless exits,
//! autoreaping, orphaned process groups.

use alloc::vec::Vec;

use lumen_signal::Signal;

use super::{
    Act, INIT, adopted, bug, exit, exit_full, exit_with, fork, hangups, reaped, reported_pid, slot, stop, tree, wait4,
};
use crate::{
    Bug, Errno, ExitAction, ExitError, ExitStatus, NoFreePid, ProcState, ProcTree, Role, SpawnInitError, StateChange,
    WUNTRACED,
};

use super::Act::{Hup, Notify, NotifyReleased, Released};

/// `t.exit(..)`'s error, the tree left as it was.
fn exit_err(t: &mut ProcTree, pid: u32, reaper: Option<u32>) -> ExitError {
    let before = alloc::format!("{t:?}");
    let err = t.exit(pid, ExitStatus::exited(0), reaper, |_| false).map(drop).expect_err("exit failed");
    assert_eq!(alloc::format!("{t:?}"), before, "an exit error changed the tree");
    err
}

#[test]
fn exit_makes_a_zombie_and_names_the_parent_to_signal() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let b = fork(&mut t, a);
    assert_eq!(exit(&mut t, b, 3), [Notify { parent: a, child: b }]);
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
    assert_eq!(exit(&mut t, a, 0), [Notify { parent: INIT, child: a }]);
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
    assert_eq!(exit_with(&mut t, a, ExitStatus::exited(0), Some(r)), [Notify { parent: INIT, child: a }]);
    assert_eq!(t.children(r), &[x]);
    assert_eq!(t.parent(x), Some(Some(r)));
    exit(&mut t, x, 9);
    assert_eq!(wait4(&mut t, r, -1, 0), reaped(x, 9));
}

#[test]
fn an_adopted_zombie_is_reported_to_a_non_init_reaper() {
    let mut t = tree();
    let r = fork(&mut t, INIT);
    let a = fork(&mut t, INIT);
    let x = fork(&mut t, a);
    exit(&mut t, x, 4);
    assert_eq!(
        exit_with(&mut t, a, ExitStatus::exited(0), Some(r)),
        [Notify { parent: r, child: x }, Notify { parent: INIT, child: a }]
    );
    assert_eq!(wait4(&mut t, r, -1, 0), reaped(x, 4));
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
    let actions = exit(&mut t, a, 0);
    assert_eq!(
        actions,
        [Notify { parent: INIT, child: x }, Notify { parent: INIT, child: z }, Notify { parent: INIT, child: a }]
    );
    assert_eq!(t.parent(x), Some(Some(INIT)));
    // The reaper reaps them: the dead parent first (older), then x, then z.
    assert_eq!(reported_pid(&wait4(&mut t, INIT, -1, 0)), Some(a));
    assert_eq!(wait4(&mut t, INIT, -1, 0), reaped(x, 1));
    assert_eq!(reported_pid(&wait4(&mut t, INIT, -1, 0)), Some(z));
    assert_eq!(wait4(&mut t, INIT, -1, crate::WNOHANG), Ok(crate::WaitReady::NoneReady));
    assert_eq!(t.parent(y), Some(Some(INIT)));
}

#[test]
fn adopted_zombies_come_in_fork_order_not_pid_order() {
    // Pids 2..=5: after a wrap the younger child has the lower pid.
    let mut t = ProcTree::new(5);
    t.spawn_init(slot(0)).unwrap();
    let a = fork(&mut t, INIT); // 2
    let x = fork(&mut t, a); // 3
    let y = fork(&mut t, a); // 4
    exit(&mut t, x, 0);
    assert_eq!(wait4(&mut t, a, x as i32, 0), reaped(x, 0));
    let z = fork(&mut t, a); // 5
    let w = fork(&mut t, a); // wraps to 3
    assert_eq!((y, z, w), (4, 5, 3));
    exit(&mut t, w, 0);
    exit(&mut t, y, 0);
    assert_eq!(adopted(&exit(&mut t, a, 0)), [y, w]);
}

/// The actions carry each zombie's status, and the slots of the zombies
/// that left the tree, whole (above 32 bits).
#[test]
fn actions_carry_statuses_and_released_slots() {
    let big = u64::from(u32::MAX) + 5;
    let mut t = tree();
    let r = fork(&mut t, INIT);
    let a = t.fork(INIT, slot(big)).unwrap();
    let x = t.fork(a, slot(big + 1)).unwrap();
    let killed = ExitStatus::Killed { signal: Signal::SEGV, core: true };
    exit_full(&mut t, x, killed, Some(INIT), &[]);
    // r and init both autoreap: x and a leave the tree in exit.
    let actions = exit_full(&mut t, a, ExitStatus::exited(-1), Some(r), &[r, INIT]);
    assert_eq!(
        actions,
        [
            ExitAction::Notify { parent: r, child: x, status: killed, released: Some(slot(big + 1)) },
            ExitAction::Notify { parent: INIT, child: a, status: ExitStatus::Exited(255), released: Some(slot(big)) },
        ]
    );
    assert!(!t.contains(x) && !t.contains(a));
    assert_eq!(t.children(r), &[] as &[u32]);
}

#[test]
fn an_autoreaping_parent_gets_notified_and_its_zombie_leaves() {
    let mut t = tree();
    let p = fork(&mut t, INIT);
    let c = fork(&mut t, p);
    // SIG_IGN or SA_NOCLDWAIT: the crate can't tell them apart; the kernel
    // sends SIGCHLD to a handler, so the action is a notification either way.
    assert_eq!(
        exit_full(&mut t, c, ExitStatus::exited(2), Some(INIT), &[p]).iter().map(Act::from).collect::<Vec<_>>(),
        [NotifyReleased { parent: p, child: c }]
    );
    assert!(!t.contains(c));
    assert_eq!(t.children(p), &[] as &[u32]);
    assert_eq!(wait4(&mut t, p, -1, crate::WNOHANG), super::user(Errno::Child));
}

#[test]
fn an_autoreaping_reaper_takes_live_children_but_not_zombies() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let x = fork(&mut t, a);
    let y = fork(&mut t, a);
    exit(&mut t, x, 1);
    let actions: Vec<Act> =
        exit_full(&mut t, a, ExitStatus::exited(0), Some(INIT), &[INIT]).iter().map(Act::from).collect();
    assert_eq!(actions, [NotifyReleased { parent: INIT, child: x }, NotifyReleased { parent: INIT, child: a }]);
    assert_eq!(t.children(INIT), &[y]);
    assert!(!t.contains(x) && !t.contains(a));
}

#[test]
fn a_bad_reaper_is_a_bug_and_changes_nothing() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let x = fork(&mut t, a);
    let gx = fork(&mut t, x);
    let dead = fork(&mut t, INIT);
    exit(&mut t, dead, 0);
    for reaper in [a, x, gx, dead, 999] {
        assert_eq!(exit_err(&mut t, a, Some(reaper)), ExitError::Bug(Bug::BadReaper { pid: a, reaper }));
    }
    // Checked on every exit: a childless process too (itself, gone, dead).
    for reaper in [gx, 999, dead] {
        assert_eq!(exit_err(&mut t, gx, Some(reaper)), ExitError::Bug(Bug::BadReaper { pid: gx, reaper }));
    }
    assert_eq!(t.state(gx), Some(ProcState::Running));
    exit(&mut t, gx, 0);
}

#[test]
fn a_lone_root_exits_without_a_reaper() {
    let mut t = ProcTree::new(10);
    let r = t.spawn_root(slot(7)).unwrap();
    let actions = exit_full(&mut t, r, ExitStatus::exited(3), None, &[]);
    assert_eq!(actions, [ExitAction::Released { child: r, status: ExitStatus::Exited(3), slot: slot(7) }]);
    assert!(!t.contains(r));
}

#[test]
fn init_alone_exits_without_a_reaper() {
    let mut t = ProcTree::new(10);
    t.spawn_init(slot(0)).unwrap();
    assert_eq!(exit_with(&mut t, INIT, ExitStatus::exited(0), None), [Released { child: INIT }]);
    assert!(!t.contains(INIT));
}

#[test]
fn a_childless_process_with_a_parent_exits_without_a_reaper() {
    let mut t = tree();
    let c = fork(&mut t, INIT);
    assert_eq!(exit_with(&mut t, c, ExitStatus::exited(1), None), [Notify { parent: INIT, child: c }]);
}

/// A `linux /path` run that forks, its child exits, and it exits without a
/// wait: the zombie leaves with it.
#[test]
fn a_root_exiting_without_a_reaper_releases_its_zombie_children() {
    let mut t = ProcTree::new(10);
    let r = t.spawn_root(slot(1)).unwrap();
    let c = t.fork(r, slot(2)).unwrap();
    assert_eq!(exit_with(&mut t, c, ExitStatus::exited(5), Some(r)), [Notify { parent: r, child: c }]);
    let actions = exit_full(&mut t, r, ExitStatus::exited(0), None, &[]);
    assert_eq!(
        actions,
        [
            ExitAction::Released { child: c, status: ExitStatus::Exited(5), slot: slot(2) },
            ExitAction::Released { child: r, status: ExitStatus::Exited(0), slot: slot(1) },
        ]
    );
    assert!(!t.contains(c) && !t.contains(r));
}

#[test]
fn a_root_exiting_without_a_reaper_leaves_its_live_children_roots() {
    let mut t = ProcTree::new(10);
    let r = t.spawn_root(slot(1)).unwrap();
    let c = t.fork(r, slot(2)).unwrap();
    assert_eq!(exit_with(&mut t, r, ExitStatus::exited(0), None), [Released { child: r }]);
    assert_eq!(t.parent(c), Some(None));
    assert_eq!(t.state(c), Some(ProcState::Running));
    let actions = exit_full(&mut t, c, ExitStatus::exited(6), None, &[]);
    assert_eq!(actions, [ExitAction::Released { child: c, status: ExitStatus::Exited(6), slot: slot(2) }]);
    assert!(!t.contains(c));
}

/// A parentless exit makes the children roots: no link for their groups.
#[test]
fn a_parentless_exit_orphans_its_childrens_groups() {
    let mut t = ProcTree::new(20);
    let r = t.spawn_root(slot(0)).unwrap();
    let j = fork(&mut t, r);
    t.setpgid(r, j as i32, 0).unwrap();
    let k = fork(&mut t, j);
    stop(&mut t, k, Signal::TSTP);
    assert!(!t.is_orphaned_pgrp(j), "r, in group r of session r, is j's link");
    assert_eq!(exit_with(&mut t, r, ExitStatus::exited(0), None), [Hup { pgid: j }, Released { child: r }]);
    assert_eq!(t.parent(j), Some(None));
    assert!(t.is_orphaned_pgrp(j));
}

#[test]
fn only_a_live_process_can_exit() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    exit(&mut t, a, 0);
    let not_live = |pid| ExitError::Bug(Bug::NotLive { pid, role: Role::Exiting });
    assert_eq!(exit_err(&mut t, a, Some(INIT)), not_live(a));
    assert_eq!(exit_err(&mut t, 555, Some(INIT)), not_live(555));
    assert_eq!(t.state(a), Some(ProcState::Zombie(ExitStatus::Exited(0))));
}

#[test]
fn transitions_need_a_live_process() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    exit(&mut t, a, 0);
    let caller = |pid| Bug::NotLive { pid, role: Role::Caller };
    assert_eq!(t.fork(a, slot(0)), bug(Bug::NotLive { pid: a, role: Role::Parent }));
    assert_eq!(t.fork(555, slot(0)), bug(Bug::NotLive { pid: 555, role: Role::Parent }));
    assert_eq!(t.exec(a), Err(caller(a)));
    // A signal reaching a zombie, or a stale pid: ESRCH, for the kernel to map.
    assert_eq!(t.stop(a, Signal::STOP), Err(Errno::Srch));
    assert_eq!(t.cont(a), Err(Errno::Srch));
    assert_eq!(t.stop(555, Signal::STOP), Err(Errno::Srch));
    assert_eq!(t.setsid(a), bug(caller(a)));
    assert_eq!(t.setpgid(a, 0, 0), bug(caller(a)));
    assert_eq!(t.getpgid(a, 0), bug(caller(a)));
    assert_eq!(t.getsid(555, INIT as i32), bug(caller(555)));
    assert_eq!(t.wait4_request(a, -1, 0), bug(caller(a)));
    assert_eq!(t.waitid_request(555, crate::P_ALL, 0, crate::WEXITED), bug(caller(555)));
    t.check_invariants();
}

#[test]
fn stop_and_cont_name_the_parent_to_tell() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let r = t.spawn_root(slot(0)).unwrap();
    let changed = StateChange::Changed { parent: Some(INIT) };
    assert_eq!(t.stop(a, Signal::STOP), Ok(changed));
    assert_eq!(t.stop(a, Signal::TSTP), Ok(StateChange::Unchanged));
    assert_eq!(t.cont(a), Ok(changed));
    assert_eq!(t.cont(a), Ok(StateChange::Unchanged));
    assert_eq!(t.stop(r, Signal::STOP), Ok(StateChange::Changed { parent: None }));
}

#[test]
fn a_root_has_no_parent_and_is_released_not_waited_for() {
    let mut t = tree();
    let r = t.spawn_root(slot(5)).unwrap();
    assert_eq!(t.parent(r), Some(None));
    assert_eq!((t.pgid(r), t.sid(r)), (Some(r), Some(r)));
    let c = fork(&mut t, r);
    let actions = exit_full(&mut t, r, ExitStatus::exited(2), Some(INIT), &[]);
    assert_eq!(actions, [ExitAction::Released { child: r, status: ExitStatus::Exited(2), slot: slot(5) }]);
    assert_eq!(t.parent(c), Some(Some(INIT)));
    assert!(!t.contains(r));
}

#[test]
fn spawn_root_fails_without_a_free_pid() {
    let mut t = ProcTree::new(2);
    assert_eq!(t.spawn_root(slot(0)), Ok(2));
    assert_eq!(t.spawn_root(slot(0)), Err(NoFreePid));
}

#[test]
fn the_actions_must_all_be_carried_out() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let mut actions = t.exit(a, ExitStatus::exited(0), Some(INIT), |_| false).unwrap().into_iter();
    assert_eq!(actions.next().as_ref().map(Act::from), Some(Notify { parent: INIT, child: a }));
    assert_eq!(actions.next(), None);
    assert_eq!(actions.next(), None, "each action is handed out once");
}

#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "every exit action is carried out"))]
fn dropping_exit_actions_unread_is_a_bug() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let outcome = t.exit(a, ExitStatus::exited(0), Some(INIT), |_| false).unwrap();
    drop(outcome);
}

#[test]
fn init_is_pid_one_once() {
    let mut t = ProcTree::new(10);
    assert_eq!(t.spawn_init(slot(0)), Ok(()));
    assert_eq!(t.spawn_init(slot(0)), Err(SpawnInitError::Exists));
    assert_eq!(t.parent(1), Some(None));
    assert_eq!((t.pgid(1), t.sid(1)), (Some(1), Some(1)));
    // Init exiting to another reaper: its children are the root's; init,
    // without a parent, leaves.
    let r = t.spawn_root(slot(0)).unwrap();
    let a = fork(&mut t, INIT);
    let b = fork(&mut t, INIT);
    exit(&mut t, b, 4);
    assert_eq!(
        exit_with(&mut t, INIT, ExitStatus::exited(0), Some(r)),
        [Notify { parent: r, child: b }, Released { child: INIT }]
    );
    assert_eq!(t.children(r), &[a, b]);
    // Gone, but a still uses 1 as its pgid and sid.
    assert_eq!((t.pgid(a), t.sid(a)), (Some(1), Some(1)));
    assert_eq!(t.spawn_init(slot(0)), Err(SpawnInitError::NumberBusy));
    // spawn_init succeeds once 1 is free.
    assert_eq!(wait4(&mut t, r, b as i32, 0), reaped(b, 4));
    t.setsid(a).unwrap();
    assert_eq!(t.spawn_init(slot(0)), Ok(()));
    t.check_invariants();
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
    stop(&mut t, k, Signal::TSTP);
    assert_eq!(exit(&mut t, s, 0), [Hup { pgid: j }, Notify { parent: INIT, child: s }]);
    assert!(t.is_orphaned_pgrp(j));
}

#[test]
fn a_reported_stop_still_counts_as_a_stopped_member() {
    let (mut t, s, j, k) = session_with_job();
    stop(&mut t, k, Signal::TSTP);
    assert_eq!(reported_pid(&wait4(&mut t, j, k as i32, WUNTRACED)), Some(k));
    assert_eq!(t.state(k), Some(ProcState::Stopped { signal: Signal::TSTP, reported: true }));
    assert_eq!(hangups(&exit(&mut t, s, 0)), [j]);
}

#[test]
fn a_member_with_an_outside_parent_orphans_its_own_group_by_exiting() {
    let (mut t, s, j, k) = session_with_job();
    let k2 = fork(&mut t, j);
    stop(&mut t, k2, Signal::STOP);
    // j is the group's link (its parent s is in group s); k and k2 go to init.
    assert_eq!(exit(&mut t, j, 0), [Hup { pgid: j }, Notify { parent: s, child: j }]);
    assert!(t.is_orphaned_pgrp(j));
    assert_eq!(t.parent(k), Some(Some(INIT)));
}

/// Case A: Linux fires because `kill_orphaned_pgrp`'s outer test (sh's
/// parent, init, is in another group of sh's session) has no init
/// exemption, while `job`, adopted by init, is no link under
/// `will_become_orphaned_pgrp`'s `is_global_init` exemption.
#[test]
fn a_job_under_a_shell_whose_parent_is_init_is_hung_up() {
    let mut t = tree();
    let sh = fork(&mut t, INIT);
    t.setpgid(sh, 0, 0).unwrap();
    let job = fork(&mut t, sh);
    stop(&mut t, job, Signal::TSTP);
    assert_eq!(exit(&mut t, sh, 0), [Hup { pgid: sh }, Notify { parent: INIT, child: sh }]);
}

/// Case B: a zombie child leading a group with a stopped member: the group
/// was orphaned already, and is hung up anyway, as Linux's
/// `reparent_leader` runs for zombies too.
#[test]
fn a_zombie_childs_group_with_a_stopped_member_is_hung_up() {
    let mut t = tree();
    let s = fork(&mut t, INIT);
    t.setsid(s).unwrap();
    let p = fork(&mut t, s);
    let c = fork(&mut t, p);
    t.setpgid(p, c as i32, 0).unwrap();
    let gc = fork(&mut t, c);
    assert_eq!(exit(&mut t, c, 0), [Notify { parent: p, child: c }]);
    stop(&mut t, gc, Signal::STOP);
    assert!(t.is_orphaned_pgrp(c), "orphaned before p's exit");
    assert_eq!(
        exit(&mut t, p, 0),
        [Notify { parent: INIT, child: c }, Hup { pgid: c }, Notify { parent: s, child: p }]
    );
}

#[test]
fn a_group_is_not_orphaned_while_a_members_parent_is_in_another_group_of_the_session() {
    let (mut t, s, j, k) = session_with_job();
    // m: a child of t2 (group s), moved into group j.
    let t2 = fork(&mut t, s);
    let m = fork(&mut t, t2);
    t.setpgid(t2, m as i32, j as i32).unwrap();
    stop(&mut t, m, Signal::STOP);
    stop(&mut t, k, Signal::STOP);
    assert_eq!(hangups(&exit(&mut t, s, 0)), Vec::<u32>::new());
    assert!(!t.is_orphaned_pgrp(j), "m's parent t2 is in group s of the same session");
    // t2's exit removes that last link: a child's group, orphaned now.
    assert_eq!(exit(&mut t, t2, 0), [Hup { pgid: j }, Notify { parent: INIT, child: t2 }]);
    assert!(t.is_orphaned_pgrp(j));
}

#[test]
fn a_non_init_reaper_in_another_group_of_the_session_is_a_new_link() {
    // Session s: r and p in group s; p's child j leads group j, with k.
    let mut t = tree();
    let s = fork(&mut t, INIT);
    t.setsid(s).unwrap();
    let r = fork(&mut t, s);
    let p = fork(&mut t, s);
    let j = fork(&mut t, p);
    t.setpgid(p, j as i32, 0).unwrap();
    let k = fork(&mut t, j);
    stop(&mut t, k, Signal::STOP);
    let mut with_init = t.clone();
    // j moves to r, in group s of j's session: still a link.
    assert_eq!(exit_with(&mut t, p, ExitStatus::exited(0), Some(r)), [Notify { parent: s, child: p }]);
    assert_eq!(t.parent(j), Some(Some(r)));
    assert!(!t.is_orphaned_pgrp(j));
    // Init as the reaper is no link.
    assert_eq!(exit(&mut with_init, p, 0), [Hup { pgid: j }, Notify { parent: s, child: p }]);
}

#[test]
fn an_orphaned_group_without_a_stopped_member_is_not_signalled() {
    let (mut t, s, j, _k) = session_with_job();
    assert_eq!(hangups(&exit(&mut t, s, 0)), Vec::<u32>::new());
    assert!(t.is_orphaned_pgrp(j));
}

#[test]
fn an_exit_inside_an_orphaned_group_is_no_link_and_hangs_up_nothing() {
    let (mut t, s, j, k) = session_with_job();
    let k2 = fork(&mut t, j);
    exit(&mut t, s, 0);
    stop(&mut t, k, Signal::STOP);
    assert!(t.is_orphaned_pgrp(j));
    // k2's parent j is in k2's own group: no link was lost.
    assert_eq!(exit(&mut t, k2, 0), [Notify { parent: j, child: k2 }]);
}

#[test]
fn a_parent_in_another_session_is_no_link() {
    let mut t = tree();
    let s = fork(&mut t, INIT);
    t.setsid(s).unwrap();
    let d = fork(&mut t, s);
    t.setsid(d).unwrap();
    let e = fork(&mut t, d);
    stop(&mut t, e, Signal::STOP);
    // Group d's only outside parent (s) is in another session.
    assert!(t.is_orphaned_pgrp(d));
    assert_eq!(hangups(&exit(&mut t, s, 0)), Vec::<u32>::new());
}

#[test]
fn a_parent_that_is_init_is_no_link_but_another_parent_is() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    t.setpgid(a, 0, 0).unwrap();
    // a's parent is init, in group 1 of the same session: no link.
    assert!(t.is_orphaned_pgrp(a));
    let r = t.spawn_root(slot(0)).unwrap();
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
    stop(&mut t, k, Signal::STOP);
    // j, s's last live child in group j, fires it, before z is told of.
    assert_eq!(
        exit(&mut t, s, 0),
        [Hup { pgid: j }, Notify { parent: INIT, child: z }, Notify { parent: INIT, child: s }]
    );
}

/// Where a group fires among the exiting process's children: at its last
/// live child in the group, or at its first when all of them are zombies;
/// once.
#[test]
fn a_group_fires_at_its_last_live_child_or_its_first_zombie() {
    let mut t = tree();
    let s = fork(&mut t, INIT);
    t.setsid(s).unwrap();
    let p = fork(&mut t, s);
    // Group b: b (live), a (zombie), c (live, stopped).
    let b = fork(&mut t, p);
    t.setpgid(p, b as i32, 0).unwrap();
    let a = fork(&mut t, p);
    t.setpgid(p, a as i32, b as i32).unwrap();
    let c = fork(&mut t, p);
    t.setpgid(p, c as i32, b as i32).unwrap();
    // Group z1: z1 and z2, zombies; its stopped member y, z1's child.
    let z1 = fork(&mut t, p);
    t.setpgid(p, z1 as i32, 0).unwrap();
    let y = fork(&mut t, z1);
    let z2 = fork(&mut t, p);
    t.setpgid(p, z2 as i32, z1 as i32).unwrap();
    exit(&mut t, a, 0);
    exit(&mut t, z1, 0);
    exit(&mut t, z2, 0);
    stop(&mut t, c, Signal::STOP);
    stop(&mut t, y, Signal::STOP);
    assert_eq!(t.children(p), &[b, a, c, z1, z2]);
    assert_eq!(
        exit(&mut t, p, 0),
        [
            Notify { parent: INIT, child: a },
            Hup { pgid: b },
            Notify { parent: INIT, child: z1 },
            Hup { pgid: z1 },
            Notify { parent: INIT, child: z2 },
            Notify { parent: s, child: p },
        ]
    );
}

/// The actions can outnumber the children: each child here is a zombie
/// leading a group with a stopped member, two actions each.
#[test]
fn every_child_a_zombie_whose_group_fires() {
    let mut t = tree();
    let s = fork(&mut t, INIT);
    t.setsid(s).unwrap();
    let p = fork(&mut t, s);
    let mut want = Vec::new();
    for _ in 0..5 {
        let c = fork(&mut t, p);
        t.setpgid(p, c as i32, 0).unwrap();
        let g = fork(&mut t, c);
        exit(&mut t, c, 0);
        stop(&mut t, g, Signal::STOP);
        want.extend([Notify { parent: INIT, child: c }, Hup { pgid: c }]);
    }
    want.push(Notify { parent: s, child: p });
    assert_eq!(exit(&mut t, p, 0), want);
}

/// p's children c1 and c3 share a group, c2 leads its own; p's own group is
/// orphaned too. Each child group fires at its last live child.
fn three_groups(c3_leads: bool) -> (Vec<Act>, [u32; 5]) {
    let mut t = tree();
    let s = fork(&mut t, INIT);
    t.setsid(s).unwrap();
    // p: in its own group, its parent s (group s) its link.
    let p = fork(&mut t, s);
    t.setpgid(p, 0, 0).unwrap();
    let p2 = fork(&mut t, p);
    stop(&mut t, p2, Signal::STOP);
    let c1 = fork(&mut t, p);
    let c2 = fork(&mut t, p);
    let c3 = fork(&mut t, p);
    t.setpgid(p, c2 as i32, 0).unwrap();
    if c3_leads {
        t.setpgid(p, c3 as i32, 0).unwrap();
        t.setpgid(p, c1 as i32, c3 as i32).unwrap();
    } else {
        t.setpgid(p, c1 as i32, 0).unwrap();
        t.setpgid(p, c3 as i32, c1 as i32).unwrap();
    }
    stop(&mut t, c3, Signal::STOP);
    stop(&mut t, c2, Signal::STOP);
    (exit(&mut t, p, 0), [s, p, c1, c2, c3])
}

#[test]
fn groups_fire_in_the_order_of_their_last_live_child_then_the_own() {
    // c3 joins c1's group: [c2, c1, p], not sorted ([c1, c2, p]) nor in
    // order of first appearance.
    let (actions, [s, p, c1, c2, _c3]) = three_groups(false);
    assert_eq!(actions, [Hup { pgid: c2 }, Hup { pgid: c1 }, Hup { pgid: p }, Notify { parent: s, child: p }]);
    // c1 joins c3's group: [c2, c3, p], not in order of first appearance
    // ([c3, c2, p]).
    let (actions, [s, p, _c1, c2, c3]) = three_groups(true);
    assert_eq!(actions, [Hup { pgid: c2 }, Hup { pgid: c3 }, Hup { pgid: p }, Notify { parent: s, child: p }]);
}

/// The own group's parent must be in the same session: a session leader
/// whose parent is init (session 1) is no link for its own group.
#[test]
fn a_session_leaders_own_group_has_no_link_in_another_session() {
    let mut t = tree();
    let s = fork(&mut t, INIT);
    t.setsid(s).unwrap();
    let c = fork(&mut t, s);
    stop(&mut t, c, Signal::TSTP);
    assert_eq!(exit(&mut t, s, 0), [Notify { parent: INIT, child: s }]);
}

/// A stopped process alone in its group, killed: its own stop doesn't count
/// (its group exit ended it), so nothing is hung up.
#[test]
fn a_stopped_process_alone_in_its_group_exits_without_a_hangup() {
    let mut t = tree();
    let s = fork(&mut t, INIT);
    t.setsid(s).unwrap();
    let a = fork(&mut t, s);
    t.setpgid(a, 0, 0).unwrap();
    stop(&mut t, a, Signal::STOP);
    let killed = ExitStatus::Killed { signal: Signal::KILL, core: false };
    assert_eq!(exit_with(&mut t, a, killed, Some(INIT)), [Notify { parent: s, child: a }]);
}

#[test]
fn pgrp_members_lists_a_group_live_and_zombie() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    t.setpgid(a, 0, 0).unwrap();
    let b = fork(&mut t, a);
    let c = fork(&mut t, a);
    exit(&mut t, c, 0);
    assert_eq!(t.pgrp_members(a).collect::<Vec<_>>(), [a, b, c]);
    assert_eq!(t.pgrp_members(4242).count(), 0);
}

#[test]
fn bugs_and_errors_display() {
    let bug = Bug::NotLive { pid: 7, role: Role::Exiting };
    assert_eq!(alloc::format!("{bug}"), "Exiting 7 isn't live");
    assert_eq!(alloc::format!("{}", Bug::BadReaper { pid: 3, reaper: 4 }), "4 can't reap the children of 3");
    assert_eq!(alloc::format!("{}", crate::Error::User(Errno::Child)), "ECHILD");
    assert_eq!(alloc::format!("{}", crate::Error::Bug(bug)), "kernel bug: Exiting 7 isn't live");
}

#[test]
fn autoreap_is_asked_once_for_the_reaper_and_once_for_the_parent() {
    let mut t = tree();
    let r = fork(&mut t, INIT);
    let p = fork(&mut t, INIT);
    let a = fork(&mut t, p);
    let x = fork(&mut t, a);
    let y = fork(&mut t, a);
    exit(&mut t, x, 0);
    exit(&mut t, y, 0);
    let asked = core::cell::RefCell::new(Vec::new());
    let out = t
        .exit(a, ExitStatus::exited(0), Some(r), |q| {
            asked.borrow_mut().push(q);
            false
        })
        .unwrap();
    out.into_iter().for_each(drop);
    let mut asked = asked.into_inner();
    asked.sort_unstable();
    let mut want = alloc::vec![r, p];
    want.sort_unstable();
    assert_eq!(asked, want, "two zombie children, yet the reaper asked once");
}
