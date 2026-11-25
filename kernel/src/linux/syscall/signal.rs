//! Signals (M8's cluster, ARCHITECTURE section 8, "B0's stubs"): M6's
//! `rt_sigaction` and `rt_sigprocmask`, moved here from `proc` unchanged.

use super::{copy_pod_from_user, copy_pod_to_user, neg_errno};
use crate::linux::process::{KernelSigaction, with_current, with_current_mut};
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
    let prev = with_current(|p| p.sigactions[idx]);
    if oldact != 0 {
        // Write `oldact` BEFORE mutating the table so a faulting `oldact` leaves state intact.
        // SAFETY: `KernelSigaction` has zero padding bytes.
        if let Err(e) = unsafe { copy_pod_to_user(oldact, &prev) } {
            return neg_errno(e.to_errno());
        }
    }
    if let Some(mut a) = new_action {
        a.mask &= !UNBLOCKABLE_SIGS;
        with_current_mut(|p| p.sigactions[idx] = a);
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

    let prev = with_current(|p| p.sigmask);
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
        with_current_mut(|p| p.sigmask = updated & !UNBLOCKABLE_SIGS);
    }
    0
}
