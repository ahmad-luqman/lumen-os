//! Signals (M8's cluster, ARCHITECTURE section 8, "B0's stubs"): M6's
//! `rt_sigaction` and `rt_sigprocmask`, moved here from `proc` (B1 keeps
//! their behaviour, over the process's `SignalState` and each thread's
//! `Blocked` mask);
//! and M8's `rt_sigreturn`, `rt_sigpending`, `rt_sigtimedwait`,
//! `rt_sigqueueinfo`, `rt_tgsigqueueinfo`, `rt_sigsuspend`, `sigaltstack`,
//! `kill`, `tkill`, `tgkill`, `pause`, `restart_syscall`.

use super::at::UserPtr;
use super::{copy_pod_from_user, copy_pod_to_user, neg_errno, not_implemented};
use crate::arch::syscall::{SyscallFrame, SyscallReturn};
use crate::linux::process::{
    KernelSigaction, current_action, current_blocked, set_current_action, set_current_blocked,
};
use crate::linux::{errno, general};
use crate::user::{copy_from_user, copy_to_user};
use lumen_signal::{SigSet, Signal};

/// `rt_sigaction(sig, act, oldact, sigsetsize)` (13), M6's order: the
/// arguments checked, `act` copied in, `oldact` written before the action
/// changes (so a faulting `oldact` changes nothing), two holds of the
/// process's signal state (C5 moves it to Linux's order, one hold).
pub(super) fn sys_rt_sigaction(sig: u64, act: u64, oldact: u64, sigsetsize: u64) -> u64 {
    let signal = u32::try_from(sig).ok().and_then(Signal::new);
    let Some(signal) = signal.filter(|_| sigsetsize == 8) else {
        return neg_errno(errno::EINVAL);
    };
    if act != 0 && signal.is_unblockable() {
        return neg_errno(errno::EINVAL);
    }
    let new_action = if act != 0 {
        // SAFETY: `KernelSigaction` is 4 `u64`s (32 bytes); all bit patterns are valid.
        match unsafe { copy_pod_from_user::<KernelSigaction>(act) } {
            Ok(a) => Some(a),
            Err(e) => return neg_errno(e.to_errno()),
        }
    } else {
        None
    };

    let prev = current_action(signal);
    if oldact != 0 {
        // Write `oldact` BEFORE mutating the table so a faulting `oldact` leaves state intact.
        // SAFETY: `KernelSigaction` has zero padding bytes.
        if let Err(e) = unsafe { copy_pod_to_user(oldact, &prev) } {
            return neg_errno(e.to_errno());
        }
    }
    // `set_action` strips `SIGKILL` and `SIGSTOP` from the mask; their own
    // dispositions were refused above, so it never refuses here.
    if let Some(a) = new_action {
        let replaced = set_current_action(signal, a);
        debug_assert!(replaced.is_some(), "{signal:?}'s disposition refused after the check");
    }
    0
}

pub(super) fn sys_rt_sigprocmask(how: u64, set: u64, oldset: u64, sigsetsize: u64) -> u64 {
    if sigsetsize != 8 {
        return neg_errno(errno::EINVAL);
    }
    let new_mask = if set != 0 {
        if how != u64::from(general::SIG_BLOCK)
            && how != u64::from(general::SIG_UNBLOCK)
            && how != u64::from(general::SIG_SETMASK)
        {
            return neg_errno(errno::EINVAL);
        }
        let mut raw = [0u8; 8];
        if let Err(e) = copy_from_user(&mut raw, set) {
            return neg_errno(e.to_errno());
        }
        Some(u64::from_le_bytes(raw))
    } else {
        None
    };

    let prev = current_blocked();
    if oldset != 0
        && let Err(e) = copy_to_user(oldset, &prev.set().0.to_le_bytes())
    {
        return neg_errno(e.to_errno());
    }
    if let Some(mask) = new_mask {
        // `how` was checked above; `Blocked` never blocks KILL or STOP.
        match prev.apply(how as u32, SigSet(mask)) {
            Ok(updated) => set_current_blocked(updated),
            Err(_) => return neg_errno(errno::EINVAL),
        }
    }
    0
}

/// `rt_sigreturn()` (15): M8 stub, replaced by C5 (#384). It returns
/// through the frame, which C5 restores from the signal frame on the user
/// stack (an `iretq`, `SyscallReturn::Iret`).
pub(super) fn sys_rt_sigreturn(frame: &mut SyscallFrame) -> SyscallReturn {
    frame.rax = not_implemented(general::__NR_rt_sigreturn);
    SyscallReturn::Sysret
}

/// `rt_sigpending(set, sigsetsize)` (127): M8 stub, replaced by C6 (#385).
pub(super) fn sys_rt_sigpending(_set: UserPtr, _sigsetsize: u64) -> u64 {
    not_implemented(general::__NR_rt_sigpending)
}

/// `rt_sigtimedwait(set, info, timeout, sigsetsize)` (128): M8 stub, replaced by C6 (#385).
pub(super) fn sys_rt_sigtimedwait(_set: UserPtr, _info: UserPtr, _timeout: UserPtr, _sigsetsize: u64) -> u64 {
    not_implemented(general::__NR_rt_sigtimedwait)
}

/// `rt_sigqueueinfo(tgid, sig, info)` (129): M8 stub, replaced by C5 (#384).
pub(super) fn sys_rt_sigqueueinfo(_tgid: i32, _sig: i32, _info: UserPtr) -> u64 {
    not_implemented(general::__NR_rt_sigqueueinfo)
}

/// `rt_tgsigqueueinfo(tgid, tid, sig, info)` (297): M8 stub, replaced by C5 (#384).
pub(super) fn sys_rt_tgsigqueueinfo(_tgid: i32, _tid: i32, _sig: i32, _info: UserPtr) -> u64 {
    not_implemented(general::__NR_rt_tgsigqueueinfo)
}

/// `rt_sigsuspend(mask, sigsetsize)` (130): M8 stub, replaced by C6 (#385).
pub(super) fn sys_rt_sigsuspend(_mask: UserPtr, _sigsetsize: u64) -> u64 {
    not_implemented(general::__NR_rt_sigsuspend)
}

/// `sigaltstack(ss, old_ss)` (131): M8 stub, replaced by C5 (#384). The
/// frame holds the caller's `rsp`, which says whether it runs on the
/// alternate stack (`SS_ONSTACK`, `-EPERM` for a change then).
pub(super) fn sys_sigaltstack(_ss: UserPtr, _old_ss: UserPtr, _frame: &SyscallFrame) -> u64 {
    not_implemented(general::__NR_sigaltstack)
}

/// `kill(pid, sig)` (62): M8 stub, replaced by C5 (#384) and C8 (#387).
pub(super) fn sys_kill(_pid: i32, _sig: i32) -> u64 {
    not_implemented(general::__NR_kill)
}

/// `tkill(tid, sig)` (200): M8 stub, replaced by C5 (#384).
pub(super) fn sys_tkill(_tid: i32, _sig: i32) -> u64 {
    not_implemented(general::__NR_tkill)
}

/// `tgkill(tgid, tid, sig)` (234): M8 stub, replaced by C5 (#384).
pub(super) fn sys_tgkill(_tgid: i32, _tid: i32, _sig: i32) -> u64 {
    not_implemented(general::__NR_tgkill)
}

/// `pause()` (34): M8 stub, replaced by C6 (#385).
pub(super) fn sys_pause() -> u64 {
    not_implemented(general::__NR_pause)
}

/// `restart_syscall()` (219, B2): resumes the thread's restart block
/// (`linux::signal::restart_syscall`), `-EINTR` with none. The syscall-exit
/// hook makes a call interrupted with `-ERESTART_RESTARTBLOCK` this one.
pub(super) fn sys_restart_syscall(frame: &mut SyscallFrame) -> SyscallReturn {
    frame.rax = crate::linux::signal::restart_syscall();
    SyscallReturn::Sysret
}

crate::ktest!(
    fn sigaction_and_sigprocmask_round_trip() {
        // Through `dispatch`: an action set and read back (its mask without
        // SIGKILL), a faulting `oldact` leaving it unchanged, the three
        // `how`s on the calling thread's mask (never KILL or STOP); a mask
        // is the thread's (keyed by tid), an action the process's.
        use crate::linux::process::{self, KTEST_USER_LEN, KTEST_USER_VA};
        use crate::{kassert, kassert_eq};
        use alloc::sync::Arc;
        use lumen_signal::Blocked;
        process::in_test_process(|_| -> Result<(), &'static str> {
            let call = |nr: u32, rdi: u64, rsi: u64, rdx: u64, r10: u64| {
                let mut frame = SyscallFrame { rax: u64::from(nr), rdi, rsi, rdx, r10, ..SyscallFrame::default() };
                let _ = super::dispatch(&mut frame);
                frame.rax
            };
            let (act_at, old_at, set_at, oldset_at) =
                (KTEST_USER_VA, KTEST_USER_VA + 64, KTEST_USER_VA + 128, KTEST_USER_VA + 136);
            let unmapped = KTEST_USER_VA + KTEST_USER_LEN;
            let put_act = |a: &KernelSigaction| {
                // SAFETY: `KernelSigaction` has zero padding bytes.
                unsafe { copy_pod_to_user(act_at, a) }.map_err(|_| "copy the action in")
            };
            let put_set = |s: SigSet| copy_to_user(set_at, &s.0.to_le_bytes()).map_err(|_| "copy the set in");
            let oldset = || {
                let mut raw = [0u8; 8];
                copy_from_user(&mut raw, oldset_at).map(|()| u64::from_le_bytes(raw)).map_err(|_| "read oldset")
            };
            let usr1 = u64::from(general::SIGUSR1);
            let (sigaction, sigprocmask) = (general::__NR_rt_sigaction, general::__NR_rt_sigprocmask);

            let act = KernelSigaction {
                handler: 0x1000,
                flags: u64::from(general::SA_RESTORER),
                restorer: 0x2000,
                mask: SigSet::of(&[Signal::USR2, Signal::KILL]).0,
            };
            put_act(&act)?;
            kassert_eq!(call(sigaction, usr1, act_at, 0, 8), 0);
            kassert_eq!(call(sigaction, usr1, 0, old_at, 8), 0);
            // SAFETY: `KernelSigaction` is 4 `u64`s; all bit patterns are valid.
            let got = unsafe { copy_pod_from_user::<KernelSigaction>(old_at) }.map_err(|_| "read oldact")?;
            kassert_eq!(got, KernelSigaction { mask: Signal::USR2.bit(), ..act });
            // A faulting `oldact` with a new action: EFAULT, nothing changed.
            put_act(&KernelSigaction { handler: 0x3000, ..act })?;
            kassert_eq!(call(sigaction, usr1, act_at, unmapped, 8), neg_errno(errno::EFAULT));
            kassert_eq!(current_action(Signal::USR1).handler, 0x1000);
            // SIGKILL's disposition never changes: EINVAL, an `oldact`
            // holding a sentinel untouched.
            let sentinel = KernelSigaction { handler: 0x5e17, flags: 1, restorer: 2, mask: 3 };
            // SAFETY: `KernelSigaction` has zero padding bytes.
            unsafe { copy_pod_to_user(old_at, &sentinel) }.map_err(|_| "copy the sentinel in")?;
            kassert_eq!(call(sigaction, u64::from(general::SIGKILL), act_at, old_at, 8), neg_errno(errno::EINVAL));
            // SAFETY: as above.
            let kept = unsafe { copy_pod_from_user::<KernelSigaction>(old_at) }.map_err(|_| "read the sentinel")?;
            kassert_eq!(kept, sentinel);
            kassert_eq!(current_action(Signal::KILL), KernelSigaction::default());

            let block = u64::from(general::SIG_BLOCK);
            put_set(SigSet::of(&[Signal::USR1, Signal::KILL, Signal::STOP]))?;
            kassert_eq!(call(sigprocmask, block, set_at, oldset_at, 8), 0);
            kassert_eq!(oldset()?, 0);
            kassert_eq!(current_blocked().set(), SigSet::of(&[Signal::USR1]));
            put_set(SigSet::of(&[Signal::USR2]))?;
            kassert_eq!(call(sigprocmask, block, set_at, oldset_at, 8), 0);
            kassert_eq!(oldset()?, Signal::USR1.bit());
            put_set(SigSet::of(&[Signal::USR1]))?;
            kassert_eq!(call(sigprocmask, u64::from(general::SIG_UNBLOCK), set_at, 0, 8), 0);
            kassert_eq!(current_blocked().set(), SigSet::of(&[Signal::USR2]));
            put_set(SigSet::of(&[Signal::TERM, Signal::STOP]))?;
            kassert_eq!(call(sigprocmask, u64::from(general::SIG_SETMASK), set_at, 0, 8), 0);
            kassert_eq!(current_blocked().set(), SigSet::of(&[Signal::TERM]));
            kassert_eq!(call(sigprocmask, 7, set_at, 0, 8), neg_errno(errno::EINVAL));
            kassert_eq!(call(sigprocmask, 0, 0, oldset_at, 8), 0);
            kassert_eq!(oldset()?, Signal::TERM.bit());

            // Per thread, per process: a second thread of this process keeps
            // its own (empty) mask, unchanged by the leader's call, and sees
            // the leader's action; then, the running thread made that second
            // thread for a while, its syscalls name it (`gettid`) and its
            // process (`getpid`), and its `SIG_BLOCK` changes its own mask,
            // not the leader's.
            let me = process::current_process();
            let leader = process::current_thread();
            let pid = u64::from(me.pid());
            let extra = process::add_ktest_thread(&me)?;
            let checked = (|| {
                kassert_eq!(extra.blocked(), Blocked::EMPTY);
                put_set(SigSet::of(&[Signal::HUP]))?;
                kassert_eq!(call(sigprocmask, block, set_at, 0, 8), 0);
                kassert_eq!(current_blocked().set(), SigSet::of(&[Signal::TERM, Signal::HUP]));
                kassert_eq!(extra.blocked(), Blocked::EMPTY);
                kassert!(Arc::ptr_eq(extra.process(), &me));
                kassert_eq!(extra.process().action(Signal::USR1).handler, 0x1000);

                let swapped = crate::sched::set_current_user_thread(Some(Arc::clone(&extra)));
                let as_extra = (|| {
                    kassert_eq!(call(general::__NR_gettid, 0, 0, 0, 0), u64::from(extra.tid()));
                    kassert_eq!(call(general::__NR_getpid, 0, 0, 0, 0), pid);
                    put_set(SigSet::of(&[Signal::USR2]))?;
                    kassert_eq!(call(sigprocmask, block, set_at, 0, 8), 0);
                    Ok(())
                })();
                let back = crate::sched::set_current_user_thread(swapped);
                kassert!(back.is_some_and(|b| Arc::ptr_eq(&b, &extra)));
                as_extra?;
                kassert_eq!(extra.blocked().set(), SigSet::of(&[Signal::USR2]));
                kassert_eq!(leader.blocked().set(), SigSet::of(&[Signal::TERM, Signal::HUP]));
                kassert_eq!(call(general::__NR_gettid, 0, 0, 0, 0), pid);
                Ok(())
            })();
            process::remove_ktest_thread(&extra);
            checked
        })?
    }
);
