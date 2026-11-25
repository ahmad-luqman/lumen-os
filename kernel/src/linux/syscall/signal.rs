//! Signals (M8's cluster, ARCHITECTURE section 8, "B0's stubs"): M6's
//! `rt_sigaction` and `rt_sigprocmask`, moved here from `proc` unchanged;
//! and M8's `rt_sigreturn`, `rt_sigpending`, `rt_sigtimedwait`,
//! `rt_sigqueueinfo`, `rt_tgsigqueueinfo`, `rt_sigsuspend`, `sigaltstack`,
//! `kill`, `tkill`, `tgkill`, `pause`, `restart_syscall`.

use super::at::UserPtr;
use super::{copy_pod_from_user, copy_pod_to_user, neg_errno, not_implemented};
use crate::arch::syscall::{SyscallFrame, SyscallReturn};
use crate::linux::process::{KernelSigaction, with_current_signals};
use crate::linux::{errno, general};
use crate::user::{copy_from_user, copy_to_user};

/// Mask of unblockable signals (`SIGKILL` = 9, `SIGSTOP` = 19, 1-indexed).
const UNBLOCKABLE_SIGS: u64 = (1u64 << (general::SIGKILL - 1)) | (1u64 << (general::SIGSTOP - 1));

pub(super) fn sys_rt_sigaction(sig: u64, act: u64, oldact: u64, sigsetsize: u64) -> u64 {
    if sigsetsize != 8 || !(1..=64).contains(&sig) {
        return neg_errno(errno::EINVAL);
    }
    if act != 0 && (sig == u64::from(general::SIGKILL) || sig == u64::from(general::SIGSTOP)) {
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

    let idx = (sig - 1) as usize;
    let prev = with_current_signals(|actions, _| actions[idx]);
    if oldact != 0 {
        // Write `oldact` BEFORE mutating the table so a faulting `oldact` leaves state intact.
        // SAFETY: `KernelSigaction` has zero padding bytes.
        if let Err(e) = unsafe { copy_pod_to_user(oldact, &prev) } {
            return neg_errno(e.to_errno());
        }
    }
    if let Some(mut a) = new_action {
        a.mask &= !UNBLOCKABLE_SIGS;
        with_current_signals(|actions, _| actions[idx] = a);
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

    let prev = with_current_signals(|_, me| me.blocked);
    if oldset != 0
        && let Err(e) = copy_to_user(oldset, &prev.to_le_bytes())
    {
        return neg_errno(e.to_errno());
    }
    if let Some(mask) = new_mask {
        let updated = if how == u64::from(general::SIG_BLOCK) {
            prev | mask
        } else if how == u64::from(general::SIG_UNBLOCK) {
            prev & !mask
        } else {
            mask
        };
        with_current_signals(|_, me| me.blocked = updated & !UNBLOCKABLE_SIGS);
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

/// `restart_syscall()` (219): M8 stub, replaced by B2 (#373). It is passed
/// the frame, which B2's restart may rewrite (section 6).
pub(super) fn sys_restart_syscall(frame: &mut SyscallFrame) -> SyscallReturn {
    frame.rax = not_implemented(general::__NR_restart_syscall);
    SyscallReturn::Sysret
}
