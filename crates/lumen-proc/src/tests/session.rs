//! `setsid`, `setpgid`, `getpgid`, `getsid` and `tcsetpgrp`.

use super::{INIT, exit, fork, sig, tree, wait4};
use crate::{Errno, ProcTree, Sigttou, Tcsetpgrp, Terminal};

/// A session leader `s` (a child of init) and its child `c`, in group `s`.
fn session() -> (ProcTree, u32, u32) {
    let mut t = tree();
    let s = fork(&mut t, INIT);
    assert_eq!(t.setsid(s), Ok(s));
    let c = fork(&mut t, s);
    (t, s, c)
}

// ---- setsid ----

#[test]
fn setsid_makes_a_new_session_and_group() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    assert_eq!((t.pgid(a), t.sid(a)), (Some(INIT), Some(INIT)));
    assert_eq!(t.setsid(a), Ok(a));
    t.check_invariants();
    assert_eq!((t.pgid(a), t.sid(a)), (Some(a), Some(a)));
    let b = fork(&mut t, a);
    assert_eq!((t.pgid(b), t.sid(b)), (Some(a), Some(a)));
}

#[test]
fn setsid_refuses_a_group_leader() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    t.setpgid(a, 0, 0).unwrap();
    assert_eq!(t.setsid(a), Err(Errno::Perm));
    assert_eq!(t.sid(a), Some(INIT));
    // init leads group 1 (and session 1).
    assert_eq!(t.setsid(INIT), Err(Errno::Perm));
}

#[test]
fn setsid_refuses_a_session_leader() {
    let (mut t, s, _c) = session();
    assert_eq!(t.setsid(s), Err(Errno::Perm));
}

#[test]
fn setsid_refuses_while_a_group_still_has_its_pid() {
    let (mut t, s, _c) = session();
    // a leads group a, b joins it, a moves to group s: group a lives on in b.
    let a = fork(&mut t, s);
    t.setpgid(a, 0, 0).unwrap();
    let b = fork(&mut t, a);
    t.setpgid(a, 0, s as i32).unwrap();
    assert_eq!(t.pgid(a), Some(s));
    assert_eq!(t.pgid(b), Some(a));
    assert_eq!(t.setsid(a), Err(Errno::Perm));
    // A zombie still holds the group.
    exit(&mut t, b, 0);
    assert_eq!(t.setsid(a), Err(Errno::Perm));
    assert_eq!(wait4(&mut t, a, -1, 0).pid(), Some(b));
    assert_eq!(t.setsid(a), Ok(a));
    t.check_invariants();
}

// ---- setpgid ----

#[test]
fn setpgid_zero_means_self_and_own_pid() {
    let (mut t, s, c) = session();
    assert_eq!(t.setpgid(c, 0, 0), Ok(()));
    assert_eq!(t.pgid(c), Some(c));
    let d = fork(&mut t, s);
    assert_eq!(t.setpgid(d, d as i32, 0), Ok(()));
    assert_eq!(t.pgid(d), Some(d));
    let e = fork(&mut t, s);
    assert_eq!(t.setpgid(e, 0, e as i32), Ok(()));
    assert_eq!(t.pgid(e), Some(e));
    // Back into an existing group of the session, and a no-op.
    assert_eq!(t.setpgid(e, 0, s as i32), Ok(()));
    assert_eq!(t.pgid(e), Some(s));
    assert_eq!(t.setpgid(e, 0, s as i32), Ok(()));
    t.check_invariants();
}

#[test]
fn setpgid_moves_a_child_that_has_not_execd() {
    let (mut t, s, c) = session();
    assert_eq!(t.setpgid(s, c as i32, 0), Ok(()));
    assert_eq!(t.pgid(c), Some(c));
    let d = fork(&mut t, s);
    assert_eq!(t.setpgid(s, d as i32, c as i32), Ok(()));
    assert_eq!(t.pgid(d), Some(c));
    // A zombie child that never exec'd, as in Linux.
    exit(&mut t, d, 0);
    assert_eq!(t.setpgid(s, d as i32, s as i32), Ok(()));
    assert_eq!(t.pgid(d), Some(s));
    t.check_invariants();
}

#[test]
fn setpgid_refuses_a_negative_pgid_first() {
    let (mut t, s, c) = session();
    assert_eq!(t.setpgid(c, 0, -1), Err(Errno::Inval));
    assert_eq!(t.setpgid(c, 9999, -1), Err(Errno::Inval));
    assert_eq!(t.setpgid(c, INIT as i32, i32::MIN), Err(Errno::Inval));
    // A session leader too: EINVAL comes before EPERM.
    assert_eq!(t.setpgid(s, 0, -5), Err(Errno::Inval));
    // pgid 0 takes a negative pid's value.
    assert_eq!(t.setpgid(c, -5, 0), Err(Errno::Inval));
}

#[test]
fn setpgid_is_esrch_for_neither_self_nor_child() {
    let (mut t, s, c) = session();
    let g = fork(&mut t, c);
    let sib = fork(&mut t, s);
    assert_eq!(t.setpgid(c, 9999, 0), Err(Errno::Srch), "no such process");
    assert_eq!(t.setpgid(c, -5, 3), Err(Errno::Srch), "negative pid");
    assert_eq!(t.setpgid(s, g as i32, 0), Err(Errno::Srch), "grandchild");
    assert_eq!(t.setpgid(c, sib as i32, 0), Err(Errno::Srch), "sibling");
    assert_eq!(t.setpgid(c, s as i32, 0), Err(Errno::Srch), "parent");
    assert_eq!(t.setpgid(c, INIT as i32, 0), Err(Errno::Srch), "init");
    // A zombie that isn't the caller's child.
    exit(&mut t, g, 0);
    assert_eq!(t.setpgid(s, g as i32, 0), Err(Errno::Srch));
}

#[test]
fn setpgid_is_eacces_for_a_child_that_execd() {
    let (mut t, s, c) = session();
    t.exec(c).unwrap();
    assert_eq!(t.has_execed(c), Some(true));
    assert_eq!(t.setpgid(s, c as i32, 0), Err(Errno::Acces));
    // EACCES before the target group's EPERM.
    assert_eq!(t.setpgid(s, c as i32, 4242), Err(Errno::Acces));
    // Its own setpgid still works.
    assert_eq!(t.setpgid(c, 0, 0), Ok(()));
}

#[test]
fn setpgid_is_eperm_for_a_child_in_another_session() {
    let (mut t, s, c) = session();
    t.setsid(c).unwrap();
    assert_eq!(t.setpgid(s, c as i32, s as i32), Err(Errno::Perm));
    // Another session comes before exec'd.
    t.exec(c).unwrap();
    assert_eq!(t.setpgid(s, c as i32, s as i32), Err(Errno::Perm));
}

#[test]
fn setpgid_is_eperm_for_a_session_leader() {
    let (mut t, s, _c) = session();
    assert_eq!(t.setpgid(s, 0, 0), Err(Errno::Perm));
    assert_eq!(t.setpgid(s, s as i32, s as i32), Err(Errno::Perm));
    assert_eq!(t.setpgid(INIT, 0, 0), Err(Errno::Perm));
}

#[test]
fn setpgid_is_eperm_for_a_group_elsewhere_or_nowhere() {
    let (mut t, s, c) = session();
    let other = fork(&mut t, INIT);
    t.setsid(other).unwrap();
    // A group in another session.
    assert_eq!(t.setpgid(c, 0, other as i32), Err(Errno::Perm));
    assert_eq!(t.setpgid(c, 0, INIT as i32), Err(Errno::Perm));
    // A number no group has: unused, or a process that leads no group.
    assert_eq!(t.setpgid(c, 0, 4242), Err(Errno::Perm));
    let d = fork(&mut t, s);
    assert_eq!(t.setpgid(c, 0, d as i32), Err(Errno::Perm));
    assert_eq!(t.pgid(c), Some(s));
    // A group whose leader is gone still counts.
    let l = fork(&mut t, s);
    t.setpgid(l, 0, 0).unwrap();
    let m = fork(&mut t, l);
    exit(&mut t, l, 0);
    assert_eq!(wait4(&mut t, s, l as i32, 0).pid(), Some(l));
    assert_eq!(t.pgid(m), Some(l));
    assert_eq!(t.setpgid(c, 0, l as i32), Ok(()));
    t.check_invariants();
}

// ---- getpgid / getsid ----

#[test]
fn getpgid_and_getsid_look_up_any_process() {
    let (mut t, s, c) = session();
    t.setpgid(c, 0, 0).unwrap();
    assert_eq!(t.getpgid(c, 0), Ok(c));
    assert_eq!(t.getsid(c, 0), Ok(s));
    assert_eq!(t.getpgid(c, s as i32), Ok(s));
    assert_eq!(t.getsid(c, INIT as i32), Ok(INIT));
    assert_eq!(t.getpgid(INIT, c as i32), Ok(c));
    exit(&mut t, c, 0);
    assert_eq!(t.getpgid(INIT, c as i32), Ok(c), "a zombie");
    assert_eq!(t.getsid(INIT, c as i32), Ok(s), "a zombie");
    for pid in [9999, -1, i32::MIN] {
        assert_eq!(t.getpgid(INIT, pid), Err(Errno::Srch));
        assert_eq!(t.getsid(INIT, pid), Err(Errno::Srch));
    }
}

// ---- tcsetpgrp ----

/// `s`'s session with a background job `j` (its own group) and `c` in group
/// `s`; the terminal is the session's, `s`'s group in the foreground.
fn job_control() -> (ProcTree, u32, u32, u32, Terminal) {
    let (mut t, s, c) = session();
    let j = fork(&mut t, s);
    t.setpgid(s, j as i32, 0).unwrap();
    let tty = Terminal { is_controlling: true, session: Some(s), foreground: Some(s) };
    (t, s, c, j, tty)
}

#[test]
fn tcsetpgrp_sets_a_group_of_the_session() {
    let (t, s, c, j, tty) = job_control();
    assert_eq!(t.tcsetpgrp(s, tty, j as i32, Sigttou::Delivered), Ok(Tcsetpgrp::Set(j)));
    assert_eq!(t.tcsetpgrp(c, tty, s as i32, Sigttou::Delivered), Ok(Tcsetpgrp::Set(s)));
    // No foreground group: nobody is in the background.
    let none = Terminal { foreground: None, ..tty };
    assert_eq!(t.tcsetpgrp(j, none, j as i32, Sigttou::Delivered), Ok(Tcsetpgrp::Set(j)));
}

#[test]
fn tcsetpgrp_refuses_a_negative_pgrp() {
    let (t, s, _c, _j, tty) = job_control();
    assert_eq!(t.tcsetpgrp(s, tty, -1, Sigttou::Delivered), Err(Errno::Inval));
    let other = Terminal { is_controlling: false, ..tty };
    assert_eq!(t.tcsetpgrp(s, other, -1, Sigttou::Delivered), Err(Errno::Inval), "EINVAL before ENOTTY");
}

#[test]
fn tcsetpgrp_is_enotty_off_the_callers_controlling_terminal() {
    let (t, s, _c, j, tty) = job_control();
    let not_ctty = Terminal { is_controlling: false, ..tty };
    assert_eq!(t.tcsetpgrp(s, not_ctty, j as i32, Sigttou::Delivered), Err(Errno::NoTty));
    let other_session = Terminal { session: Some(INIT), ..tty };
    assert_eq!(t.tcsetpgrp(s, other_session, j as i32, Sigttou::Delivered), Err(Errno::NoTty));
    let no_session = Terminal { session: None, ..tty };
    assert_eq!(t.tcsetpgrp(s, no_session, j as i32, Sigttou::Delivered), Err(Errno::NoTty));
    // Off its controlling terminal, a background caller isn't stopped.
    assert_eq!(t.tcsetpgrp(j, not_ctty, j as i32, Sigttou::Delivered), Err(Errno::NoTty));
}

#[test]
fn tcsetpgrp_is_esrch_for_a_number_nobody_uses() {
    let (t, s, _c, _j, tty) = job_control();
    assert_eq!(t.tcsetpgrp(s, tty, 4242, Sigttou::Delivered), Err(Errno::Srch));
    assert_eq!(t.tcsetpgrp(s, tty, 0, Sigttou::Delivered), Err(Errno::Srch));
}

#[test]
fn tcsetpgrp_is_eperm_for_a_group_in_another_session() {
    let (mut t, s, c, _j, tty) = job_control();
    assert_eq!(t.tcsetpgrp(s, tty, INIT as i32, Sigttou::Delivered), Err(Errno::Perm));
    let other = fork(&mut t, INIT);
    t.setsid(other).unwrap();
    let o2 = fork(&mut t, other);
    assert_eq!(t.tcsetpgrp(s, tty, other as i32, Sigttou::Delivered), Err(Errno::Perm));
    // A process that leads no group: judged by its own session, as Linux does.
    assert_eq!(t.tcsetpgrp(s, tty, o2 as i32, Sigttou::Delivered), Err(Errno::Perm));
    assert_eq!(t.tcsetpgrp(s, tty, c as i32, Sigttou::Delivered), Ok(Tcsetpgrp::Set(c)));
}

#[test]
fn tcsetpgrp_is_eperm_for_a_number_held_only_as_a_session() {
    let mut t = tree();
    let d = fork(&mut t, INIT);
    t.setsid(d).unwrap();
    let e = fork(&mut t, d);
    t.setpgid(e, 0, 0).unwrap();
    exit(&mut t, d, 0);
    assert_eq!(wait4(&mut t, INIT, d as i32, 0).pid(), Some(d));
    // d is gone and leads no group, but e's session still holds the number.
    assert!(t.is_busy(d));
    let tty = Terminal { is_controlling: true, session: Some(d), foreground: Some(e) };
    assert_eq!(t.tcsetpgrp(e, tty, d as i32, Sigttou::Delivered), Err(Errno::Perm));
    assert_eq!(t.tcsetpgrp(e, tty, e as i32, Sigttou::Delivered), Ok(Tcsetpgrp::Set(e)));
}

#[test]
fn a_background_caller_gets_sigttou_unless_it_blocks_or_ignores_it() {
    let (t, _s, _c, j, tty) = job_control();
    assert_eq!(t.tcsetpgrp(j, tty, j as i32, Sigttou::Delivered), Ok(Tcsetpgrp::Sigttou { pgrp: j }));
    // The background check comes first: even before EINVAL and ESRCH.
    assert_eq!(t.tcsetpgrp(j, tty, -1, Sigttou::Delivered), Ok(Tcsetpgrp::Sigttou { pgrp: j }));
    assert_eq!(t.tcsetpgrp(j, tty, 4242, Sigttou::Delivered), Ok(Tcsetpgrp::Sigttou { pgrp: j }));
    // Blocked or ignored: it goes on to the other checks.
    assert_eq!(t.tcsetpgrp(j, tty, j as i32, Sigttou::BlockedOrIgnored), Ok(Tcsetpgrp::Set(j)));
    assert_eq!(t.tcsetpgrp(j, tty, -1, Sigttou::BlockedOrIgnored), Err(Errno::Inval));
}

#[test]
fn an_orphaned_background_caller_gets_enotty() {
    let (mut t, s, _c, j, tty) = job_control();
    let k = fork(&mut t, j);
    t.stop(k, sig(19)).unwrap();
    exit(&mut t, s, 0);
    assert!(t.is_orphaned_pgrp(j));
    assert_eq!(t.tcsetpgrp(j, tty, j as i32, Sigttou::Delivered), Err(Errno::NoTty));
    assert_eq!(t.tcsetpgrp(j, tty, j as i32, Sigttou::BlockedOrIgnored), Ok(Tcsetpgrp::Set(j)));
}

#[test]
fn a_dead_caller_is_esrch() {
    let (mut t, _s, c, _j, tty) = job_control();
    exit(&mut t, c, 0);
    assert_eq!(t.tcsetpgrp(c, tty, c as i32, Sigttou::Delivered), Err(Errno::Srch));
}
