//! Exit: zombies, reparenting, adopted zombies, orphaned process groups,
//! release.

use alloc::vec::Vec;

use super::{INIT, adopted, bug, exit, exit_with, fork, hangups, reaped, reported_pid, tree, user, wait4};
use crate::{Bug, Errno, ExitAction, ExitStatus, ProcState, ProcTree, Signal, WUNTRACED};

use ExitAction::{HangupThenContinue as Hup, Notify, ReleaseNow};

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
    assert_eq!(exit_with(&mut t, a, ExitStatus::exited(0), r), [Notify { parent: INIT, child: a }]);
    assert_eq!(t.children(r), &[x]);
    assert_eq!(t.parent(x), Some(Some(r)));
    exit(&mut t, x, 9);
    assert_eq!(wait4(&mut t, r, -1, 0), reaped(x, 9));
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
    t.spawn_init(0).unwrap();
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

#[test]
fn a_bad_reaper_is_a_bug_and_changes_nothing() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let x = fork(&mut t, a);
    let gx = fork(&mut t, x);
    let dead = fork(&mut t, INIT);
    exit(&mut t, dead, 0);
    for reaper in [a, x, gx, dead, 999] {
        let before = alloc::format!("{t:?}");
        assert_eq!(t.exit(a, ExitStatus::exited(0), Some(reaper)).map(drop), bug(Bug::BadReaper { pid: a, reaper }));
        assert_eq!(alloc::format!("{t:?}"), before, "reaper {reaper}");
    }
    // Checked on every exit: a childless process too (itself, gone, dead).
    for reaper in [gx, 999, dead] {
        let before = alloc::format!("{t:?}");
        assert_eq!(t.exit(gx, ExitStatus::exited(0), Some(reaper)).map(drop), bug(Bug::BadReaper { pid: gx, reaper }));
        assert_eq!(alloc::format!("{t:?}"), before, "reaper {reaper}");
    }
    assert_eq!(t.state(gx), Some(ProcState::Running));
    assert!(t.exit(gx, ExitStatus::exited(0), Some(INIT)).is_ok());
    t.check_invariants();
}

#[test]
fn a_lone_root_exits_without_a_reaper() {
    let mut t = ProcTree::new(10);
    let r = t.spawn_root(7).unwrap();
    let actions: Vec<ExitAction> = t.exit(r, ExitStatus::exited(3), None).unwrap().actions().collect();
    assert_eq!(actions, [ReleaseNow { child: r }]);
    assert_eq!(t.release(r, 7), Ok(ExitStatus::Exited(3)));
    t.check_invariants();
}

#[test]
fn init_alone_exits_without_a_reaper() {
    let mut t = ProcTree::new(10);
    t.spawn_init(0).unwrap();
    let actions: Vec<ExitAction> = t.exit(INIT, ExitStatus::exited(0), None).unwrap().actions().collect();
    assert_eq!(actions, [ReleaseNow { child: INIT }]);
    assert_eq!(t.state(INIT), Some(ProcState::Zombie(ExitStatus::Exited(0))));
    t.check_invariants();
}

#[test]
fn a_childless_process_with_a_parent_exits_without_a_reaper() {
    let mut t = tree();
    let c = fork(&mut t, INIT);
    let actions: Vec<ExitAction> = t.exit(c, ExitStatus::exited(1), None).unwrap().actions().collect();
    assert_eq!(actions, [Notify { parent: INIT, child: c }]);
    t.check_invariants();
}

#[test]
fn no_reaper_for_a_process_with_children_is_a_bug_and_changes_nothing() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let x = fork(&mut t, a);
    // A live child.
    let before = alloc::format!("{t:?}");
    assert_eq!(t.exit(a, ExitStatus::exited(0), None).map(drop), bug(Bug::NoReaper { pid: a }));
    assert_eq!(alloc::format!("{t:?}"), before);
    // Only a zombie child.
    exit(&mut t, x, 0);
    let before = alloc::format!("{t:?}");
    assert_eq!(t.exit(a, ExitStatus::exited(0), None).map(drop), bug(Bug::NoReaper { pid: a }));
    assert_eq!(alloc::format!("{t:?}"), before);
    assert_eq!(t.state(a), Some(ProcState::Running));
}

#[test]
fn only_a_live_process_can_exit() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    exit(&mut t, a, 0);
    assert_eq!(t.exit(a, ExitStatus::exited(1), Some(INIT)).map(drop), bug(Bug::NotLive { pid: a }));
    assert_eq!(t.exit(555, ExitStatus::exited(1), Some(INIT)).map(drop), bug(Bug::NotLive { pid: 555 }));
    assert_eq!(t.state(a), Some(ProcState::Zombie(ExitStatus::Exited(0))));
}

#[test]
fn transitions_need_a_live_process() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    exit(&mut t, a, 0);
    let not_live = Bug::NotLive { pid: a };
    assert_eq!(t.fork(a, 0), bug(not_live));
    assert_eq!(t.fork(555, 0), bug(Bug::NotLive { pid: 555 }));
    assert_eq!(t.exec(a), bug(not_live));
    // A signal reaching a zombie: ESRCH, for the kernel to map.
    assert_eq!(t.stop(a, Signal::SIGSTOP), Err(Errno::Srch));
    assert_eq!(t.cont(a), Err(Errno::Srch));
    assert_eq!(t.stop(555, Signal::SIGSTOP), Err(Errno::Srch));
    assert_eq!(t.setsid(a), bug(not_live));
    assert_eq!(t.setpgid(a, 0, 0), bug(not_live));
    assert_eq!(t.getpgid(a, 0), bug(not_live));
    assert_eq!(t.getsid(555, INIT as i32), bug(Bug::NotLive { pid: 555 }));
    assert_eq!(t.wait4_request(a, -1, 0), bug(not_live));
    assert_eq!(t.waitid_request(555, crate::P_ALL, 0, crate::WEXITED), bug(Bug::NotLive { pid: 555 }));
    t.check_invariants();
}

#[test]
fn stop_and_cont_name_the_parent_to_tell() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let r = t.spawn_root(0).unwrap();
    let changed = Some(crate::StateChanged { parent: Some(INIT) });
    assert_eq!(t.stop(a, Signal::SIGSTOP), Ok(changed));
    assert_eq!(t.stop(a, Signal::SIGTSTP), Ok(None));
    assert_eq!(t.cont(a), Ok(changed));
    assert_eq!(t.cont(a), Ok(None));
    assert_eq!(t.stop(r, Signal::SIGSTOP), Ok(Some(crate::StateChanged { parent: None })));
}

#[test]
fn a_root_has_no_parent_and_is_released_not_waited_for() {
    let mut t = tree();
    let r = t.spawn_root(5).unwrap();
    assert_eq!(t.parent(r), Some(None));
    assert_eq!((t.pgid(r), t.sid(r)), (Some(r), Some(r)));
    let c = fork(&mut t, r);
    assert_eq!(t.release(r, 5), bug(Bug::NotAZombie { pid: r }), "a live process isn't released");
    assert_eq!(exit_with(&mut t, r, ExitStatus::exited(2), INIT), [ReleaseNow { child: r }]);
    assert_eq!(t.parent(c), Some(Some(INIT)));
    assert_eq!(t.rusage_slot(r), Some(5));
    assert_eq!(t.release(r, 5), Ok(ExitStatus::Exited(2)));
    assert!(!t.contains(r));
    assert_eq!(t.release(r, 5), bug(Bug::NotAZombie { pid: r }));
    t.check_invariants();
}

#[test]
fn release_takes_a_zombie_out_of_its_parents_children() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let b = fork(&mut t, INIT);
    exit(&mut t, a, 0);
    assert_eq!(t.release(a, 0), Ok(ExitStatus::Exited(0)));
    assert_eq!(t.children(INIT), &[b]);
    t.check_invariants();
}

/// An adopted zombie whose reaper ignores `SIGCHLD`: the kernel releases
/// it, reading its slot under the same lock hold, through the public API.
#[test]
fn a_reaper_ignoring_sigchld_releases_the_adopted_zombies() {
    let mut t = tree();
    let a = t.fork(INIT, 10).unwrap();
    let x = t.fork(a, 11).unwrap();
    let y = t.fork(a, 12).unwrap();
    t.exit(x, ExitStatus::exited(1), Some(INIT)).unwrap().actions().for_each(drop);
    t.exit(y, ExitStatus::exited(2), Some(INIT)).unwrap().actions().for_each(drop);
    let out = t.exit(a, ExitStatus::exited(3), Some(INIT)).unwrap();
    let mut released = Vec::new();
    for action in out.actions() {
        // Init ignores SIGCHLD in this kernel.
        if let Notify { parent: INIT, child } = action {
            let slot = t.rusage_slot(child).unwrap();
            released.push((child, t.release(child, slot)));
        }
    }
    drop(out);
    assert_eq!(
        released,
        [(x, Ok(ExitStatus::Exited(1))), (y, Ok(ExitStatus::Exited(2))), (a, Ok(ExitStatus::Exited(3)))]
    );
    assert_eq!(t.children(INIT), &[] as &[u32]);
    t.check_invariants();
}

#[test]
fn release_after_the_pid_was_reused_is_refused() {
    // Pids 2..=3.
    let mut t = ProcTree::new(3);
    t.spawn_init(0).unwrap();
    let a = t.fork(INIT, 10).unwrap();
    let b = t.fork(INIT, 11).unwrap();
    assert_eq!((a, b), (2, 3));
    t.exit(a, ExitStatus::exited(0), Some(INIT)).unwrap().actions().for_each(drop);
    // The kernel meant to release a (slot 10), but a wait reaped it first.
    assert_eq!(wait4(&mut t, INIT, a as i32, 0).map(drop), Ok(()));
    let reused = t.fork(INIT, 20).unwrap();
    assert_eq!(reused, a);
    assert_eq!(t.release(a, 10), bug(Bug::NotAZombie { pid: a }), "live again");
    t.exit(reused, ExitStatus::exited(7), Some(INIT)).unwrap().actions().for_each(drop);
    let before = alloc::format!("{t:?}");
    assert_eq!(t.release(a, 10), bug(Bug::WrongSlot { pid: a, expected: 10, found: 20 }));
    assert_eq!(alloc::format!("{t:?}"), before);
    assert_eq!(t.release(a, 20), Ok(ExitStatus::Exited(7)));
}

#[test]
fn init_is_pid_one_once() {
    let mut t = ProcTree::new(10);
    assert_eq!(t.spawn_init(0), Ok(1));
    assert_eq!(t.spawn_init(0), bug(Bug::InitExists));
    assert_eq!(t.parent(1), Some(None));
    assert_eq!((t.pgid(1), t.sid(1)), (Some(1), Some(1)));
    // A zombie init still exists.
    let r = t.spawn_root(0).unwrap();
    let a = fork(&mut t, INIT);
    exit_with(&mut t, INIT, ExitStatus::exited(0), r);
    assert_eq!(t.spawn_init(0), bug(Bug::InitExists));
    // Released, but a (adopted by r) still uses 1 as its pgid and sid.
    assert_eq!(t.release(INIT, 0), Ok(ExitStatus::Exited(0)));
    assert_eq!((t.pgid(a), t.sid(a)), (Some(1), Some(1)));
    assert_eq!(t.spawn_init(0), user(Errno::Again));
    // spawn_init succeeds once 1 is free.
    t.setsid(a).unwrap();
    assert_eq!(t.spawn_init(0), Ok(1));
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
    t.stop(k, Signal::SIGTSTP).unwrap();
    assert_eq!(exit(&mut t, s, 0), [Hup { pgid: j }, Notify { parent: INIT, child: s }]);
    assert!(t.is_orphaned_pgrp(j));
}

#[test]
fn a_reported_stop_still_counts_as_a_stopped_member() {
    let (mut t, s, j, k) = session_with_job();
    t.stop(k, Signal::SIGTSTP).unwrap();
    assert_eq!(reported_pid(&wait4(&mut t, j, k as i32, WUNTRACED)), Some(k));
    assert_eq!(t.state(k), Some(ProcState::Stopped { signal: Signal::SIGTSTP, reported: true }));
    assert_eq!(hangups(&exit(&mut t, s, 0)), [j]);
}

#[test]
fn a_member_with_an_outside_parent_orphans_its_own_group_by_exiting() {
    let (mut t, s, j, k) = session_with_job();
    let k2 = fork(&mut t, j);
    t.stop(k2, Signal::SIGSTOP).unwrap();
    // j is the group's link (its parent s is in group s); k and k2 go to init.
    assert_eq!(exit(&mut t, j, 0), [Hup { pgid: j }, Notify { parent: s, child: j }]);
    assert!(t.is_orphaned_pgrp(j));
    assert_eq!(t.parent(k), Some(Some(INIT)));
}

/// Case A: Linux's link test has no init exemption for the exiting
/// process's own parent (nor does POSIX's definition).
#[test]
fn a_job_under_a_shell_whose_parent_is_init_is_hung_up() {
    let mut t = tree();
    let sh = fork(&mut t, INIT);
    t.setpgid(sh, 0, 0).unwrap();
    let job = fork(&mut t, sh);
    t.stop(job, Signal::SIGTSTP).unwrap();
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
    t.stop(gc, Signal::SIGSTOP).unwrap();
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
    t.stop(m, Signal::SIGSTOP).unwrap();
    t.stop(k, Signal::SIGSTOP).unwrap();
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
    t.stop(k, Signal::SIGSTOP).unwrap();
    let mut with_init = t.clone();
    // j moves to r, in group s of j's session: still a link.
    assert_eq!(exit_with(&mut t, p, ExitStatus::exited(0), r), [Notify { parent: s, child: p }]);
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
    t.stop(k, Signal::SIGSTOP).unwrap();
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
    t.stop(e, Signal::SIGSTOP).unwrap();
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
    t.stop(k, Signal::SIGSTOP).unwrap();
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
    t.stop(c, Signal::SIGSTOP).unwrap();
    t.stop(y, Signal::SIGSTOP).unwrap();
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
        t.stop(g, Signal::SIGSTOP).unwrap();
        want.extend([Notify { parent: INIT, child: c }, Hup { pgid: c }]);
    }
    want.push(Notify { parent: s, child: p });
    assert_eq!(exit(&mut t, p, 0), want);
}

/// p's children c1 and c3 share a group, c2 leads its own; p's own group is
/// orphaned too. Each child group fires at its last live child.
fn three_groups(c3_leads: bool) -> (Vec<ExitAction>, [u32; 5]) {
    let mut t = tree();
    let s = fork(&mut t, INIT);
    t.setsid(s).unwrap();
    // p: in its own group, its parent s (group s) its link.
    let p = fork(&mut t, s);
    t.setpgid(p, 0, 0).unwrap();
    let p2 = fork(&mut t, p);
    t.stop(p2, Signal::SIGSTOP).unwrap();
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
    t.stop(c3, Signal::SIGSTOP).unwrap();
    t.stop(c2, Signal::SIGSTOP).unwrap();
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
