//! `wait4`/`waitid`: requests, targets, options, which child wins, what is
//! reported.

use super::{INIT, bug, exit, exit_with, fork, reaped, reported, tree, user, wait4, waitid};
use crate::wait::{
    __WALL, __WCLONE, __WNOTHREAD, P_ALL, P_PGID, P_PID, P_PIDFD, WCONTINUED, WEXITED, WNOHANG, WNOWAIT, WSTOPPED,
    WUNTRACED,
};
use crate::{Errno, Error, ExitStatus, ProcState, ProcTree, Report, Signal, WaitReady, WaitStatus, WaitTarget};

const NONE_READY: Result<WaitReady, Error> = Ok(WaitReady::NoneReady);

fn echild() -> Result<WaitReady, Error> {
    user(Errno::Child)
}

#[test]
fn constants_are_linuxs() {
    use linux_raw_sys::{errno, general};
    let w = [WNOHANG, WUNTRACED, WSTOPPED, WEXITED, WCONTINUED, WNOWAIT, __WNOTHREAD, __WALL, __WCLONE];
    let lw = [
        general::WNOHANG,
        general::WUNTRACED,
        general::WSTOPPED,
        general::WEXITED,
        general::WCONTINUED,
        general::WNOWAIT,
        general::__WNOTHREAD,
        general::__WALL,
        general::__WCLONE,
    ];
    assert_eq!(w, lw);
    let p = [P_ALL, P_PID, P_PGID, P_PIDFD].map(|n| n as u32);
    assert_eq!(p, [general::P_ALL, general::P_PID, general::P_PGID, general::P_PIDFD]);
    let errnos = [
        (Errno::Perm, errno::EPERM, "EPERM"),
        (Errno::Srch, errno::ESRCH, "ESRCH"),
        (Errno::Child, errno::ECHILD, "ECHILD"),
        (Errno::Again, errno::EAGAIN, "EAGAIN"),
        (Errno::NoMem, errno::ENOMEM, "ENOMEM"),
        (Errno::Acces, errno::EACCES, "EACCES"),
        (Errno::Inval, errno::EINVAL, "EINVAL"),
        (Errno::NoTty, errno::ENOTTY, "ENOTTY"),
    ];
    for (e, n, name) in errnos {
        assert_eq!(e.to_errno(), n);
        assert_eq!(alloc::format!("{e}"), name);
    }
}

#[test]
fn wait4_pid_selects_its_target() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    t.setpgid(a, 0, 0).unwrap();
    let target = |pid| t.wait4_request(a, pid, 0).map(|r| r.target());
    assert_eq!(target(5), Ok(WaitTarget::Pid(5)));
    assert_eq!(target(i32::MAX), Ok(WaitTarget::Pid(i32::MAX as u32)));
    assert_eq!(target(0), Ok(WaitTarget::Pgid(a)), "the caller's group");
    assert_eq!(target(-1), Ok(WaitTarget::Any));
    assert_eq!(target(-2), Ok(WaitTarget::Pgid(2)));
    assert_eq!(target(-7), Ok(WaitTarget::Pgid(7)));
    assert_eq!(target(-i32::MAX), Ok(WaitTarget::Pgid(i32::MAX as u32)));
    assert_eq!(target(i32::MIN), user(Errno::Srch));
    // The options are checked first.
    assert_eq!(t.wait4_request(a, i32::MIN, WNOWAIT), user(Errno::Inval));
    let r = t.wait4_request(a, 0, WNOHANG).unwrap();
    assert_eq!(r.caller(), a);
    assert!(r.options().nohang());
}

#[test]
fn waitid_idtype_selects_its_target() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    t.setpgid(a, 0, 0).unwrap();
    let target = |idtype, id| t.waitid_request(a, idtype, id, WEXITED).map(|r| r.target());
    assert_eq!(target(P_ALL, 0), Ok(WaitTarget::Any));
    assert_eq!(target(P_ALL, -5), Ok(WaitTarget::Any));
    assert_eq!(target(P_ALL, 9), Ok(WaitTarget::Any));
    assert_eq!(target(P_PID, 9), Ok(WaitTarget::Pid(9)));
    assert_eq!(target(P_PID, 0), user(Errno::Inval));
    assert_eq!(target(P_PID, -1), user(Errno::Inval));
    assert_eq!(target(P_PGID, 0), Ok(WaitTarget::Pgid(a)), "the caller's group");
    assert_eq!(target(P_PGID, 7), Ok(WaitTarget::Pgid(7)));
    assert_eq!(target(P_PGID, -1), user(Errno::Inval));
    assert_eq!(target(P_PIDFD, 3), user(Errno::Inval));
    assert_eq!(target(4, 1), user(Errno::Inval));
    assert_eq!(target(-1, 1), user(Errno::Inval));
    assert!(t.waitid_request(a, P_ALL, 0, WEXITED | WNOHANG).unwrap().options().nohang());
}

#[test]
fn wait4_accepts_its_options_and_refuses_the_rest() {
    let t = tree();
    let ok = [0, WNOHANG, WUNTRACED, WCONTINUED, __WNOTHREAD, __WCLONE, __WALL];
    for &a in &ok {
        for &b in &ok {
            assert!(t.wait4_request(INIT, -1, a | b).is_ok(), "{:#x}", a | b);
        }
    }
    for bad in [WEXITED, WNOWAIT, 0x10, 0x20, 0x8000, WNOHANG | WNOWAIT, u32::MAX] {
        assert_eq!(t.wait4_request(INIT, -1, bad), user(Errno::Inval), "{bad:#x}");
    }
    assert!(t.wait4_request(INIT, -1, WNOHANG).unwrap().options().nohang());
    assert!(!t.wait4_request(INIT, -1, WUNTRACED).unwrap().options().nohang());
}

#[test]
fn waitid_needs_one_of_exited_stopped_continued() {
    let t = tree();
    for need in [WEXITED, WSTOPPED, WCONTINUED] {
        for extra in [0, WNOHANG, WNOWAIT, __WNOTHREAD, __WCLONE, __WALL, WEXITED | WSTOPPED | WCONTINUED] {
            assert!(t.waitid_request(INIT, P_ALL, 0, need | extra).is_ok(), "{:#x}", need | extra);
        }
    }
    for bad in [0, WNOHANG, WNOWAIT, __WALL, WNOHANG | WNOWAIT | __WALL | __WCLONE | __WNOTHREAD] {
        assert_eq!(t.waitid_request(INIT, P_ALL, 0, bad), user(Errno::Inval), "{bad:#x}");
    }
    for bad in [WEXITED | 0x10, WEXITED | 0x0200_0000] {
        assert_eq!(t.waitid_request(INIT, P_ALL, 0, bad), user(Errno::Inval), "{bad:#x}");
    }
}

#[test]
fn invalid_options_are_einval_and_reap_nothing() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    exit(&mut t, a, 3);
    assert_eq!(wait4(&mut t, INIT, -1, WNOWAIT), user(Errno::Inval));
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WNOHANG), user(Errno::Inval));
    // Options before the id: EINVAL, not the target's error.
    assert_eq!(waitid(&mut t, INIT, P_PID, -1, 0), user(Errno::Inval));
    assert!(t.contains(a));
    assert_eq!(wait4(&mut t, INIT, -1, 0), reaped(a, 3));
}

#[test]
fn no_children_is_echild() {
    let mut t = tree();
    assert_eq!(wait4(&mut t, INIT, -1, 0), echild());
    assert_eq!(wait4(&mut t, INIT, -1, WNOHANG), echild());
    let a = fork(&mut t, INIT);
    let g = fork(&mut t, a);
    // A grandchild, self, a nonexistent pid, the parent: not children.
    assert_eq!(wait4(&mut t, INIT, g as i32, WNOHANG), echild());
    assert_eq!(wait4(&mut t, INIT, INIT as i32, WNOHANG), echild());
    assert_eq!(wait4(&mut t, INIT, 999, WNOHANG), echild());
    assert_eq!(wait4(&mut t, g, INIT as i32, WNOHANG), echild());
    assert_eq!(wait4(&mut t, g, -1, WNOHANG), echild());
    // A group no child is in.
    assert_eq!(wait4(&mut t, INIT, -(g as i32), WNOHANG), echild());
}

#[test]
fn a_caller_that_died_since_its_request_is_a_bug() {
    let mut t = tree();
    let z = fork(&mut t, INIT);
    let request = t.wait4_request(z, -1, 0).unwrap();
    exit(&mut t, z, 0);
    assert_eq!(t.wait(request), bug(crate::Bug::NotLive { pid: z }));
}

#[test]
fn a_live_child_with_nothing_to_report_is_none_ready() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    for opts in [0, WNOHANG, WUNTRACED, WCONTINUED, WNOHANG | WUNTRACED | WCONTINUED] {
        assert_eq!(wait4(&mut t, INIT, -1, opts), NONE_READY, "{opts:#x}");
        assert_eq!(wait4(&mut t, INIT, a as i32, opts), NONE_READY, "{opts:#x}");
        assert_eq!(wait4(&mut t, INIT, 0, opts), NONE_READY, "{opts:#x}");
    }
}

#[test]
fn a_request_is_reused_across_retries() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let request = t.wait4_request(INIT, 0, 0).unwrap();
    assert_eq!(t.wait(request), NONE_READY);
    // The caller's group was resolved at the call: a later move doesn't
    // change what the request waits for.
    let b = fork(&mut t, INIT);
    t.setpgid(b, 0, 0).unwrap();
    exit(&mut t, b, 2);
    assert_eq!(t.wait(request), NONE_READY, "b is in another group");
    exit(&mut t, a, 1);
    assert_eq!(t.wait(request), reaped(a, 1));
}

#[test]
fn reaping_removes_the_zombie_and_reports_its_status() {
    let mut t = tree();
    let a = t.fork(INIT, 0xabc).unwrap();
    let b = t.fork(INIT, 0xdef).unwrap();
    exit_with(&mut t, a, ExitStatus::exited(7), INIT);
    exit_with(&mut t, b, ExitStatus::Killed { signal: Signal::SIGKILL, core: true }, INIT);
    let Ok(WaitReady::Reported(r)) = wait4(&mut t, INIT, -1, 0) else { panic!("nothing reaped") };
    assert_eq!(r.pid(), a);
    assert_eq!(r.status().word(), 7 << 8);
    assert_eq!(r, Report::Reaped { pid: a, status: ExitStatus::Exited(7), rusage_slot: 0xabc });
    assert!(!t.contains(a));
    assert_eq!(t.children(INIT), &[b]);
    let Ok(WaitReady::Reported(r)) = wait4(&mut t, INIT, -1, 0) else { panic!("nothing reaped") };
    assert_eq!(r.status().word(), 0x89);
    assert_eq!(
        r,
        Report::Reaped {
            pid: b,
            status: ExitStatus::Killed { signal: Signal::SIGKILL, core: true },
            rusage_slot: 0xdef
        }
    );
    assert_eq!(wait4(&mut t, INIT, -1, 0), echild());
}

#[test]
fn the_first_ready_child_in_fork_order_wins() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let b = fork(&mut t, INIT);
    let c = fork(&mut t, INIT);
    exit(&mut t, c, 3);
    exit(&mut t, b, 2);
    assert_eq!(wait4(&mut t, INIT, -1, 0), reaped(b, 2));
    assert_eq!(wait4(&mut t, INIT, -1, 0), reaped(c, 3));
    assert_eq!(wait4(&mut t, INIT, -1, WNOHANG), NONE_READY);
    exit(&mut t, a, 1);
    assert_eq!(wait4(&mut t, INIT, -1, 0), reaped(a, 1));
}

#[test]
fn fork_order_not_pid_order_decides_after_a_wrap() {
    // Pids 2..=4: after a wrap the older child has the higher pid.
    let mut t = ProcTree::new(4);
    t.spawn_init(0).unwrap();
    let a = fork(&mut t, INIT); // 2
    let b = fork(&mut t, INIT); // 3
    exit(&mut t, a, 0);
    assert_eq!(wait4(&mut t, INIT, a as i32, 0), reaped(a, 0));
    let c = fork(&mut t, INIT); // 4
    let d = fork(&mut t, INIT); // wraps to 2
    assert_eq!((b, c, d), (3, 4, 2));
    exit(&mut t, d, 9);
    exit(&mut t, c, 8);
    assert_eq!(t.children(INIT), &[b, c, d]);
    assert_eq!(wait4(&mut t, INIT, -1, 0), reaped(c, 8));
    assert_eq!(wait4(&mut t, INIT, -1, 0), reaped(d, 9));
}

#[test]
fn adopted_children_come_after_the_reapers_own() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let x = fork(&mut t, a);
    let b = fork(&mut t, INIT);
    exit(&mut t, x, 5);
    exit(&mut t, a, 4);
    exit(&mut t, b, 6);
    assert_eq!(t.children(INIT), &[a, b, x]);
    assert_eq!(wait4(&mut t, INIT, -1, 0), reaped(a, 4));
    assert_eq!(wait4(&mut t, INIT, -1, 0), reaped(b, 6));
    assert_eq!(wait4(&mut t, INIT, -1, 0), reaped(x, 5));
}

#[test]
fn a_pid_target_takes_only_that_child() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let b = fork(&mut t, INIT);
    exit(&mut t, a, 1);
    assert_eq!(wait4(&mut t, INIT, b as i32, WNOHANG), NONE_READY);
    exit(&mut t, b, 2);
    assert_eq!(wait4(&mut t, INIT, b as i32, 0), reaped(b, 2));
    assert_eq!(waitid(&mut t, INIT, P_PID, a as i32, WEXITED), reaped(a, 1));
}

#[test]
fn a_group_target_takes_only_children_in_that_group() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let b = fork(&mut t, INIT);
    let c = fork(&mut t, INIT);
    t.setpgid(b, 0, 0).unwrap();
    t.setpgid(c, 0, b as i32).unwrap();
    exit(&mut t, a, 1);
    exit(&mut t, c, 3);
    // Group b: only c is ready; a (ready, group 1) is passed over.
    assert_eq!(wait4(&mut t, INIT, -(b as i32), 0), reaped(c, 3));
    assert_eq!(waitid(&mut t, INIT, P_PGID, b as i32, WEXITED | WNOHANG), NONE_READY);
    // Group 0: the caller's own (1): a.
    assert_eq!(wait4(&mut t, INIT, 0, 0), reaped(a, 1));
    exit(&mut t, b, 2);
    assert_eq!(waitid(&mut t, INIT, P_PGID, 0, WEXITED | WNOHANG), echild());
    assert_eq!(waitid(&mut t, INIT, P_PGID, b as i32, WEXITED), reaped(b, 2));
}

#[test]
fn stops_are_reported_once_and_only_when_asked() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    assert!(t.stop(a, Signal::SIGSTOP).unwrap().is_some());
    assert_eq!(t.stop(a, Signal::SIGTSTP), Ok(None));
    assert_eq!(wait4(&mut t, INIT, -1, WNOHANG), NONE_READY);
    assert_eq!(wait4(&mut t, INIT, -1, WCONTINUED | WNOHANG), NONE_READY);
    let r = wait4(&mut t, INIT, -1, WUNTRACED);
    assert_eq!(r, reported(Report::Stopped { pid: a, signal: Signal::SIGSTOP }));
    let Ok(WaitReady::Reported(r)) = r else { unreachable!() };
    assert_eq!(r.status().word(), (19 << 8) | 0x7f);
    assert_eq!(t.state(a), Some(ProcState::Stopped { signal: Signal::SIGSTOP, reported: true }));
    assert_eq!(wait4(&mut t, INIT, -1, WUNTRACED | WNOHANG), NONE_READY);
    // waitid's name for it.
    assert!(t.cont(a).unwrap().is_some());
    t.stop(a, Signal::SIGTTIN).unwrap();
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WSTOPPED), reported(Report::Stopped { pid: a, signal: Signal::SIGTTIN }));
}

#[test]
fn continues_are_reported_once_and_only_when_asked() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    assert_eq!(t.cont(a), Ok(None));
    assert_eq!(wait4(&mut t, INIT, -1, WCONTINUED | WNOHANG), NONE_READY);
    t.stop(a, Signal::SIGSTOP).unwrap();
    assert!(t.cont(a).unwrap().is_some());
    assert_eq!(t.cont(a), Ok(None));
    assert_eq!(wait4(&mut t, INIT, -1, WUNTRACED | WNOHANG), NONE_READY, "cont forgets an unreported stop");
    let r = wait4(&mut t, INIT, -1, WCONTINUED);
    assert_eq!(r, reported(Report::Continued { pid: a }));
    let Ok(WaitReady::Reported(r)) = r else { unreachable!() };
    assert_eq!(r.status().word(), 0xffff);
    assert_eq!(wait4(&mut t, INIT, -1, WCONTINUED | WNOHANG), NONE_READY);
    // A new stop forgets an unreported continue.
    t.stop(a, Signal::SIGSTOP).unwrap();
    t.cont(a).unwrap();
    t.stop(a, Signal::SIGTSTP).unwrap();
    assert_eq!(
        wait4(&mut t, INIT, -1, WCONTINUED | WUNTRACED),
        reported(Report::Stopped { pid: a, signal: Signal::SIGTSTP })
    );
    assert_eq!(wait4(&mut t, INIT, -1, WCONTINUED | WUNTRACED | WNOHANG), NONE_READY);
}

#[test]
fn wnowait_leaves_the_zombie_and_the_reports() {
    let mut t = tree();
    let a = t.fork(INIT, 77).unwrap();
    exit_with(&mut t, a, ExitStatus::exited(4), INIT);
    let peek = || reported(Report::Zombie { pid: a, status: ExitStatus::Exited(4), rusage_slot: 77 });
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WEXITED | WNOWAIT), peek());
    assert_eq!(waitid(&mut t, INIT, P_PID, a as i32, WEXITED | WNOWAIT), peek());
    assert_eq!(t.state(a), Some(ProcState::Zombie(ExitStatus::Exited(4))));
    assert_eq!(WaitStatus::from(ExitStatus::Exited(4)).siginfo().status, 4);
    assert_eq!(
        waitid(&mut t, INIT, P_ALL, 0, WEXITED),
        reported(Report::Reaped { pid: a, status: ExitStatus::Exited(4), rusage_slot: 77 })
    );
    assert!(!t.contains(a));

    let b = fork(&mut t, INIT);
    t.stop(b, Signal::SIGSTOP).unwrap();
    for _ in 0..2 {
        assert_eq!(
            waitid(&mut t, INIT, P_ALL, 0, WSTOPPED | WNOWAIT),
            reported(Report::Stopped { pid: b, signal: Signal::SIGSTOP })
        );
    }
    assert_eq!(t.state(b), Some(ProcState::Stopped { signal: Signal::SIGSTOP, reported: false }));
    t.cont(b).unwrap();
    for _ in 0..2 {
        assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WCONTINUED | WNOWAIT), reported(Report::Continued { pid: b }));
    }
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WCONTINUED), reported(Report::Continued { pid: b }));
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WCONTINUED | WNOHANG), NONE_READY);
}

#[test]
fn waitid_without_wexited_skips_zombies() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    exit(&mut t, a, 0);
    // Only a zombie matches: ECHILD, as in Linux.
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WSTOPPED | WCONTINUED | WNOHANG), echild());
    let b = fork(&mut t, INIT);
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WSTOPPED | WNOHANG), NONE_READY);
    t.stop(b, Signal::SIGSTOP).unwrap();
    // The zombie, first in order, is passed over for b's stop.
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WSTOPPED), reported(Report::Stopped { pid: b, signal: Signal::SIGSTOP }));
    assert!(t.contains(a));
}

#[test]
fn a_stop_ahead_of_a_zombie_wins_with_wuntraced() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let b = fork(&mut t, INIT);
    t.stop(a, Signal::SIGSTOP).unwrap();
    exit(&mut t, b, 1);
    assert_eq!(wait4(&mut t, INIT, -1, WUNTRACED), reported(Report::Stopped { pid: a, signal: Signal::SIGSTOP }));
    assert_eq!(wait4(&mut t, INIT, -1, WUNTRACED), reaped(b, 1));
}

#[test]
fn a_stopped_child_that_exits_is_reaped() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    t.stop(a, Signal::SIGSTOP).unwrap();
    let killed = ExitStatus::Killed { signal: Signal::SIGKILL, core: false };
    exit_with(&mut t, a, killed, INIT);
    assert_eq!(wait4(&mut t, INIT, -1, WUNTRACED), reported(Report::Reaped { pid: a, status: killed, rusage_slot: 0 }));
}

#[test]
fn wclone_alone_matches_no_child_and_wall_matches_all() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    exit(&mut t, a, 0);
    assert_eq!(wait4(&mut t, INIT, -1, __WCLONE), echild());
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WEXITED | __WCLONE), echild());
    assert_eq!(wait4(&mut t, INIT, -1, __WNOTHREAD | WNOHANG), reaped(a, 0));
    let b = fork(&mut t, INIT);
    exit(&mut t, b, 0);
    assert_eq!(wait4(&mut t, INIT, -1, __WALL | __WCLONE), reaped(b, 0));
    let c = fork(&mut t, INIT);
    exit(&mut t, c, 0);
    assert_eq!(wait4(&mut t, INIT, -1, __WALL), reaped(c, 0));
}

/// Every combination of options, `wait4`'s and `waitid`'s, on four children
/// in this order: running, continued (unreported), stopped (unreported),
/// zombie. The expected answer is worked out here from the rules.
#[test]
fn every_option_combination_picks_as_the_rules_say() {
    const FLAGS: [u32; 9] = [WNOHANG, WUNTRACED, WEXITED, WCONTINUED, WNOWAIT, __WNOTHREAD, __WALL, __WCLONE, 0x10];
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Want {
        Invalid,
        NoChildren,
        NoneReady,
        Cont,
        Stop,
        Zombie,
    }
    for waitid_call in [false, true] {
        for mask in 0..(1u32 << FLAGS.len()) {
            let bits = (0..FLAGS.len()).filter(|i| mask & (1 << i) != 0).fold(0, |b, i| b | FLAGS[i]);
            let mut t = tree();
            let _run = fork(&mut t, INIT);
            let cont = fork(&mut t, INIT);
            let stop = fork(&mut t, INIT);
            let zombie = fork(&mut t, INIT);
            t.stop(cont, Signal::SIGSTOP).unwrap();
            t.cont(cont).unwrap();
            t.stop(stop, Signal::SIGTSTP).unwrap();
            exit(&mut t, zombie, 6);

            let allowed = if waitid_call {
                WNOHANG | WNOWAIT | WEXITED | WSTOPPED | WCONTINUED | __WNOTHREAD | __WALL | __WCLONE
            } else {
                WNOHANG | WUNTRACED | WCONTINUED | __WNOTHREAD | __WALL | __WCLONE
            };
            let eff = if waitid_call { bits } else { bits | WEXITED };
            let want = if bits & !allowed != 0 || (waitid_call && bits & (WEXITED | WSTOPPED | WCONTINUED) == 0) {
                Want::Invalid
            } else if bits & __WCLONE != 0 && bits & __WALL == 0 {
                Want::NoChildren
            } else if eff & WCONTINUED != 0 {
                Want::Cont
            } else if eff & WSTOPPED != 0 {
                Want::Stop
            } else if eff & WEXITED != 0 {
                Want::Zombie
            } else {
                Want::NoneReady
            };
            let got = if waitid_call { waitid(&mut t, INIT, P_ALL, 0, bits) } else { wait4(&mut t, INIT, -1, bits) };
            let keep = bits & WNOWAIT != 0;
            let ok = match (want, &got) {
                (Want::Invalid, Err(Error::User(Errno::Inval)))
                | (Want::NoChildren, Err(Error::User(Errno::Child)))
                | (Want::NoneReady, Ok(WaitReady::NoneReady)) => true,
                (Want::Cont, Ok(WaitReady::Reported(Report::Continued { pid }))) => {
                    *pid == cont && t.state(cont) == Some(ProcState::Continued { reported: !keep })
                }
                (Want::Stop, Ok(WaitReady::Reported(Report::Stopped { pid, signal }))) => {
                    *pid == stop
                        && *signal == Signal::SIGTSTP
                        && t.state(stop) == Some(ProcState::Stopped { signal: Signal::SIGTSTP, reported: !keep })
                }
                (Want::Zombie, Ok(WaitReady::Reported(Report::Zombie { pid, .. }))) => {
                    keep && *pid == zombie && t.contains(zombie)
                }
                (Want::Zombie, Ok(WaitReady::Reported(Report::Reaped { pid, .. }))) => {
                    !keep && *pid == zombie && !t.contains(zombie)
                }
                _ => false,
            };
            assert!(ok, "waitid {waitid_call} options {bits:#x}: wanted {want:?}, got {got:?}");
        }
    }
}
