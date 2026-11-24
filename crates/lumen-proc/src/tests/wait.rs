//! `wait4`/`waitid`: targets, options, which child wins, what is reported.

use super::{INIT, exit, fork, sig, tree, wait4, waitid};
use crate::wait::{
    __WALL, __WCLONE, __WNOTHREAD, P_ALL, P_PGID, P_PID, P_PIDFD, WCONTINUED, WEXITED, WNOHANG, WNOWAIT, WSTOPPED,
    WUNTRACED,
};
use crate::{Errno, ExitStatus, ProcState, ProcTree, WaitOptions, WaitResult, WaitStatus, WaitTarget};

fn reaped(pid: u32, code: u8) -> WaitResult {
    WaitResult::Reaped { pid, status: ExitStatus::Exited(code), rusage_slot: 0 }
}

#[test]
fn constants_are_linuxs() {
    assert_eq!(
        [WNOHANG, WUNTRACED, WSTOPPED, WEXITED, WCONTINUED, WNOWAIT, __WNOTHREAD, __WALL, __WCLONE],
        [1, 2, 2, 4, 8, 0x0100_0000, 0x2000_0000, 0x4000_0000, 0x8000_0000]
    );
    assert_eq!([P_ALL, P_PID, P_PGID, P_PIDFD], [0, 1, 2, 3]);
    let errnos = [
        (Errno::Perm, 1, "EPERM"),
        (Errno::Srch, 3, "ESRCH"),
        (Errno::Child, 10, "ECHILD"),
        (Errno::Again, 11, "EAGAIN"),
        (Errno::Acces, 13, "EACCES"),
        (Errno::Inval, 22, "EINVAL"),
        (Errno::NoTty, 25, "ENOTTY"),
    ];
    for (e, n, name) in errnos {
        assert_eq!(e.to_errno(), n);
        assert_eq!(alloc::format!("{e}"), name);
    }
}

#[test]
fn wait4_pid_selects_its_target() {
    let t = |pid| WaitTarget::from_wait4_pid(pid, 42);
    assert_eq!(t(5), Ok(WaitTarget::Pid(5)));
    assert_eq!(t(i32::MAX), Ok(WaitTarget::Pid(i32::MAX as u32)));
    assert_eq!(t(0), Ok(WaitTarget::Pgid(42)));
    assert_eq!(t(-1), Ok(WaitTarget::Any));
    assert_eq!(t(-2), Ok(WaitTarget::Pgid(2)));
    assert_eq!(t(-7), Ok(WaitTarget::Pgid(7)));
    assert_eq!(t(-i32::MAX), Ok(WaitTarget::Pgid(i32::MAX as u32)));
    assert_eq!(t(i32::MIN), Err(Errno::Srch));
}

#[test]
fn waitid_idtype_selects_its_target() {
    let t = |idtype, id| WaitTarget::from_waitid(idtype, id, 42);
    assert_eq!(t(P_ALL, 0), Ok(WaitTarget::Any));
    assert_eq!(t(P_ALL, -5), Ok(WaitTarget::Any));
    assert_eq!(t(P_ALL, 9), Ok(WaitTarget::Any));
    assert_eq!(t(P_PID, 9), Ok(WaitTarget::Pid(9)));
    assert_eq!(t(P_PID, 0), Err(Errno::Inval));
    assert_eq!(t(P_PID, -1), Err(Errno::Inval));
    assert_eq!(t(P_PGID, 0), Ok(WaitTarget::Pgid(42)));
    assert_eq!(t(P_PGID, 7), Ok(WaitTarget::Pgid(7)));
    assert_eq!(t(P_PGID, -1), Err(Errno::Inval));
    assert_eq!(t(P_PIDFD, 3), Err(Errno::Inval));
    assert_eq!(t(4, 1), Err(Errno::Inval));
    assert_eq!(t(-1, 1), Err(Errno::Inval));
}

#[test]
fn wait4_accepts_its_options_and_refuses_the_rest() {
    let ok = [0, WNOHANG, WUNTRACED, WCONTINUED, __WNOTHREAD, __WCLONE, __WALL];
    for &a in &ok {
        for &b in &ok {
            assert_eq!(WaitOptions::wait4(a | b).check(), Ok(()), "{:#x}", a | b);
        }
    }
    for bad in [WEXITED, WNOWAIT, 0x10, 0x20, 0x8000, WNOHANG | WNOWAIT, u32::MAX] {
        assert_eq!(WaitOptions::wait4(bad).check(), Err(Errno::Inval), "{bad:#x}");
    }
    assert!(WaitOptions::wait4(WNOHANG).nohang());
    assert!(!WaitOptions::wait4(WUNTRACED).nohang());
}

#[test]
fn waitid_needs_one_of_exited_stopped_continued() {
    for need in [WEXITED, WSTOPPED, WCONTINUED] {
        for extra in [0, WNOHANG, WNOWAIT, __WNOTHREAD, __WCLONE, __WALL, WEXITED | WSTOPPED | WCONTINUED] {
            assert_eq!(WaitOptions::waitid(need | extra).check(), Ok(()), "{:#x}", need | extra);
        }
    }
    for bad in [0, WNOHANG, WNOWAIT, __WALL, WNOHANG | WNOWAIT | __WALL | __WCLONE | __WNOTHREAD] {
        assert_eq!(WaitOptions::waitid(bad).check(), Err(Errno::Inval), "{bad:#x}");
    }
    for bad in [WEXITED | 0x10, WEXITED | 0x0200_0000] {
        assert_eq!(WaitOptions::waitid(bad).check(), Err(Errno::Inval), "{bad:#x}");
    }
    assert!(WaitOptions::waitid(WEXITED | WNOHANG).nohang());
}

#[test]
fn invalid_options_are_einval_and_reap_nothing() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    exit(&mut t, a, 3);
    assert_eq!(wait4(&mut t, INIT, -1, WNOWAIT), WaitResult::Invalid);
    assert_eq!(WaitResult::Invalid.errno(), Some(Errno::Inval));
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WNOHANG), WaitResult::Invalid);
    assert!(t.contains(a));
    assert_eq!(wait4(&mut t, INIT, -1, 0), reaped(a, 3));
}

#[test]
fn no_children_is_echild() {
    let mut t = tree();
    assert_eq!(wait4(&mut t, INIT, -1, 0), WaitResult::NoChildren);
    assert_eq!(wait4(&mut t, INIT, -1, WNOHANG), WaitResult::NoChildren);
    assert_eq!(WaitResult::NoChildren.errno(), Some(Errno::Child));
    let a = fork(&mut t, INIT);
    let g = fork(&mut t, a);
    // A grandchild, self, a nonexistent pid, the parent: not children.
    assert_eq!(wait4(&mut t, INIT, g as i32, WNOHANG), WaitResult::NoChildren);
    assert_eq!(wait4(&mut t, INIT, INIT as i32, WNOHANG), WaitResult::NoChildren);
    assert_eq!(wait4(&mut t, INIT, 999, WNOHANG), WaitResult::NoChildren);
    assert_eq!(wait4(&mut t, g, INIT as i32, WNOHANG), WaitResult::NoChildren);
    assert_eq!(wait4(&mut t, g, -1, WNOHANG), WaitResult::NoChildren);
    // A group no child is in.
    assert_eq!(wait4(&mut t, INIT, -(g as i32), WNOHANG), WaitResult::NoChildren);
    // A caller that isn't live, or isn't there.
    let z = fork(&mut t, INIT);
    let zc = fork(&mut t, z);
    exit(&mut t, zc, 0);
    exit(&mut t, z, 0);
    assert_eq!(t.wait(z, WaitTarget::Any, WaitOptions::wait4(0)), WaitResult::NoChildren);
    assert_eq!(t.wait(777, WaitTarget::Any, WaitOptions::wait4(0)), WaitResult::NoChildren);
}

#[test]
fn a_live_child_with_nothing_to_report_is_none_ready() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    for opts in [0, WNOHANG, WUNTRACED, WCONTINUED, WNOHANG | WUNTRACED | WCONTINUED] {
        assert_eq!(wait4(&mut t, INIT, -1, opts), WaitResult::NoneReady, "{opts:#x}");
        assert_eq!(wait4(&mut t, INIT, a as i32, opts), WaitResult::NoneReady, "{opts:#x}");
        assert_eq!(wait4(&mut t, INIT, 0, opts), WaitResult::NoneReady, "{opts:#x}");
    }
    assert_eq!(WaitResult::NoneReady.errno(), None);
    assert_eq!(WaitResult::NoneReady.pid(), None);
    assert_eq!(WaitResult::NoneReady.status(), None);
}

#[test]
fn reaping_removes_the_zombie_and_reports_its_status() {
    let mut t = tree();
    let a = t.fork(INIT, 0xabc).unwrap();
    let b = t.fork(INIT, 0xdef).unwrap();
    t.exit(a, ExitStatus::exited(7), INIT).unwrap();
    t.exit(b, ExitStatus::Killed { signal: sig(9), core: true }, INIT).unwrap();
    let r = wait4(&mut t, INIT, -1, 0);
    assert_eq!(r, WaitResult::Reaped { pid: a, status: ExitStatus::Exited(7), rusage_slot: 0xabc });
    assert_eq!(r.pid(), Some(a));
    assert_eq!(r.status().map(WaitStatus::word), Some(7 << 8));
    assert!(!t.contains(a));
    assert_eq!(t.children(INIT), &[b]);
    let r = wait4(&mut t, INIT, -1, 0);
    assert_eq!(
        r,
        WaitResult::Reaped { pid: b, status: ExitStatus::Killed { signal: sig(9), core: true }, rusage_slot: 0xdef }
    );
    assert_eq!(r.status().map(WaitStatus::word), Some(0x89));
    assert_eq!(wait4(&mut t, INIT, -1, 0), WaitResult::NoChildren);
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
    assert_eq!(wait4(&mut t, INIT, -1, WNOHANG), WaitResult::NoneReady);
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
    assert_eq!(wait4(&mut t, INIT, b as i32, WNOHANG), WaitResult::NoneReady);
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
    assert_eq!(waitid(&mut t, INIT, P_PGID, b as i32, WEXITED | WNOHANG), WaitResult::NoneReady);
    // Group 0: the caller's own (1): a.
    assert_eq!(wait4(&mut t, INIT, 0, 0), reaped(a, 1));
    exit(&mut t, b, 2);
    assert_eq!(waitid(&mut t, INIT, P_PGID, 0, WEXITED | WNOHANG), WaitResult::NoChildren);
    assert_eq!(waitid(&mut t, INIT, P_PGID, b as i32, WEXITED), reaped(b, 2));
}

#[test]
fn stops_are_reported_once_and_only_when_asked() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    assert_eq!(t.stop(a, sig(19)), Ok(true));
    assert_eq!(t.stop(a, sig(20)), Ok(false));
    assert_eq!(wait4(&mut t, INIT, -1, WNOHANG), WaitResult::NoneReady);
    assert_eq!(wait4(&mut t, INIT, -1, WCONTINUED | WNOHANG), WaitResult::NoneReady);
    let r = wait4(&mut t, INIT, -1, WUNTRACED);
    assert_eq!(r, WaitResult::Stopped { pid: a, signal: sig(19) });
    assert_eq!(r.status().map(WaitStatus::word), Some((19 << 8) | 0x7f));
    assert_eq!(t.state(a), Some(ProcState::Stopped { signal: sig(19), reported: true }));
    assert_eq!(wait4(&mut t, INIT, -1, WUNTRACED | WNOHANG), WaitResult::NoneReady);
    // waitid's name for it.
    assert_eq!(t.cont(a), Ok(true));
    t.stop(a, sig(21)).unwrap();
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WSTOPPED), WaitResult::Stopped { pid: a, signal: sig(21) });
}

#[test]
fn continues_are_reported_once_and_only_when_asked() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    assert_eq!(t.cont(a), Ok(false));
    assert_eq!(wait4(&mut t, INIT, -1, WCONTINUED | WNOHANG), WaitResult::NoneReady);
    t.stop(a, sig(19)).unwrap();
    assert_eq!(t.cont(a), Ok(true));
    assert_eq!(t.cont(a), Ok(false));
    assert_eq!(wait4(&mut t, INIT, -1, WUNTRACED | WNOHANG), WaitResult::NoneReady);
    let r = wait4(&mut t, INIT, -1, WCONTINUED);
    assert_eq!(r, WaitResult::Continued { pid: a });
    assert_eq!(r.status().map(WaitStatus::word), Some(0xffff));
    assert_eq!(wait4(&mut t, INIT, -1, WCONTINUED | WNOHANG), WaitResult::NoneReady);
    // A new stop forgets an unreported continue.
    t.stop(a, sig(19)).unwrap();
    t.cont(a).unwrap();
    t.stop(a, sig(20)).unwrap();
    assert_eq!(wait4(&mut t, INIT, -1, WCONTINUED | WUNTRACED), WaitResult::Stopped { pid: a, signal: sig(20) });
    assert_eq!(wait4(&mut t, INIT, -1, WCONTINUED | WUNTRACED | WNOHANG), WaitResult::NoneReady);
}

#[test]
fn wnowait_leaves_the_zombie_and_the_reports() {
    let mut t = tree();
    let a = t.fork(INIT, 77).unwrap();
    t.exit(a, ExitStatus::exited(4), INIT).unwrap();
    let peek = WaitResult::Zombie { pid: a, status: ExitStatus::Exited(4), rusage_slot: 77 };
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WEXITED | WNOWAIT), peek);
    assert_eq!(waitid(&mut t, INIT, P_PID, a as i32, WEXITED | WNOWAIT), peek);
    assert_eq!(peek.status().map(|s| s.siginfo().status), Some(4));
    assert_eq!(t.state(a), Some(ProcState::Zombie(ExitStatus::Exited(4))));
    assert_eq!(
        waitid(&mut t, INIT, P_ALL, 0, WEXITED),
        WaitResult::Reaped { pid: a, status: ExitStatus::Exited(4), rusage_slot: 77 }
    );
    assert!(!t.contains(a));

    let b = fork(&mut t, INIT);
    t.stop(b, sig(19)).unwrap();
    for _ in 0..2 {
        assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WSTOPPED | WNOWAIT), WaitResult::Stopped { pid: b, signal: sig(19) });
    }
    assert_eq!(t.state(b), Some(ProcState::Stopped { signal: sig(19), reported: false }));
    t.cont(b).unwrap();
    for _ in 0..2 {
        assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WCONTINUED | WNOWAIT), WaitResult::Continued { pid: b });
    }
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WCONTINUED), WaitResult::Continued { pid: b });
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WCONTINUED | WNOHANG), WaitResult::NoneReady);
}

#[test]
fn waitid_without_wexited_skips_zombies() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    exit(&mut t, a, 0);
    // Only a zombie matches: ECHILD, as in Linux.
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WSTOPPED | WCONTINUED | WNOHANG), WaitResult::NoChildren);
    let b = fork(&mut t, INIT);
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WSTOPPED | WNOHANG), WaitResult::NoneReady);
    t.stop(b, sig(19)).unwrap();
    // The zombie, first in order, is passed over for b's stop.
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WSTOPPED), WaitResult::Stopped { pid: b, signal: sig(19) });
    assert!(t.contains(a));
}

#[test]
fn a_stop_ahead_of_a_zombie_wins_with_wuntraced() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    let b = fork(&mut t, INIT);
    t.stop(a, sig(19)).unwrap();
    exit(&mut t, b, 1);
    assert_eq!(wait4(&mut t, INIT, -1, WUNTRACED), WaitResult::Stopped { pid: a, signal: sig(19) });
    assert_eq!(wait4(&mut t, INIT, -1, WUNTRACED), reaped(b, 1));
}

#[test]
fn a_stopped_child_that_exits_is_reaped() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    t.stop(a, sig(19)).unwrap();
    t.exit(a, ExitStatus::Killed { signal: sig(9), core: false }, INIT).unwrap();
    assert_eq!(
        wait4(&mut t, INIT, -1, WUNTRACED),
        WaitResult::Reaped { pid: a, status: ExitStatus::Killed { signal: sig(9), core: false }, rusage_slot: 0 }
    );
}

#[test]
fn wclone_alone_matches_no_child_and_wall_matches_all() {
    let mut t = tree();
    let a = fork(&mut t, INIT);
    exit(&mut t, a, 0);
    assert_eq!(wait4(&mut t, INIT, -1, __WCLONE), WaitResult::NoChildren);
    assert_eq!(waitid(&mut t, INIT, P_ALL, 0, WEXITED | __WCLONE), WaitResult::NoChildren);
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
            let run = fork(&mut t, INIT);
            let cont = fork(&mut t, INIT);
            let stop = fork(&mut t, INIT);
            let zombie = fork(&mut t, INIT);
            t.stop(cont, sig(19)).unwrap();
            t.cont(cont).unwrap();
            t.stop(stop, sig(20)).unwrap();
            exit(&mut t, zombie, 6);
            let _ = run;

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
            let options = if waitid_call { WaitOptions::waitid(bits) } else { WaitOptions::wait4(bits) };
            let got = t.wait(INIT, WaitTarget::Any, options);
            t.check_invariants();
            let keep = bits & WNOWAIT != 0;
            let ok = match (want, got) {
                (Want::Invalid, WaitResult::Invalid)
                | (Want::NoChildren, WaitResult::NoChildren)
                | (Want::NoneReady, WaitResult::NoneReady) => true,
                (Want::Cont, WaitResult::Continued { pid }) => {
                    pid == cont && t.state(cont) == Some(crate::ProcState::Continued { reported: !keep })
                }
                (Want::Stop, WaitResult::Stopped { pid, signal }) => {
                    pid == stop
                        && signal == sig(20)
                        && t.state(stop) == Some(crate::ProcState::Stopped { signal: sig(20), reported: !keep })
                }
                (Want::Zombie, WaitResult::Zombie { pid, .. }) => keep && pid == zombie && t.contains(zombie),
                (Want::Zombie, WaitResult::Reaped { pid, .. }) => !keep && pid == zombie && !t.contains(zombie),
                _ => false,
            };
            assert!(ok, "waitid {waitid_call} options {bits:#x}: wanted {want:?}, got {got:?}");
        }
    }
}
