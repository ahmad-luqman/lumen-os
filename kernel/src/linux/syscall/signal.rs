//! Linux signal dispositions, masks, senders, frames and alternate stacks.
//! User copies happen outside the signal lock, with Linux's install-before-
//! copy-out ordering. Signal wait and stop/continue syscalls belong to C6.

use super::at::UserPtr;
use super::{copy_pod_from_user, copy_pod_to_user, neg_errno, not_implemented};
use crate::arch::syscall::{SyscallFrame, SyscallReturn};
use crate::linux::process::{
    KernelSigaction, current_action, current_blocked, current_process, current_thread, send_process_checked,
    send_thread_checked, set_current_action, set_current_blocked,
};
use crate::linux::{errno, general};
use crate::user::{copy_from_user, copy_to_user};
use core::mem::{offset_of, size_of};
use lumen_signal::abi;
use lumen_signal::{
    SigSet, Signal,
    pending::{SenderClass, SigInfo},
};

/// Install the new disposition in one hold, then copy the replaced action
/// out. An output fault does not roll back the installation (Linux order).
/// rt_sigaction(sig, act, oldact, sigsetsize) (13): swap under siglock before copying oldact.
pub(super) fn sys_rt_sigaction(sig: u64, act: u64, oldact: u64, sigsetsize: u64) -> u64 {
    if sigsetsize != 8 {
        return neg_errno(errno::EINVAL);
    }
    let new = if act == 0 {
        None
    } else {
        // SAFETY: KernelSigaction is four u64s, with no padding or invalid bits.
        match unsafe { copy_pod_from_user::<KernelSigaction>(act) } {
            Ok(a) => Some(a),
            Err(e) => return neg_errno(e.to_errno()),
        }
    };
    let Some(sig) = Signal::new(sig as u32) else { return neg_errno(errno::EINVAL) };
    let prev = match new {
        Some(a) => match set_current_action(sig, a) {
            Some(old) => old,
            None => return neg_errno(errno::EINVAL),
        },
        None => current_action(sig),
    };
    if oldact != 0 {
        // SAFETY: KernelSigaction has no padding.
        if let Err(e) = unsafe { copy_pod_to_user(oldact, &prev) } {
            return neg_errno(e.to_errno());
        }
    }
    0
}

/// rt_sigprocmask(how, set, oldset, sigsetsize) (14): apply before copying oldset.
pub(super) fn sys_rt_sigprocmask(how: u64, set: u64, oldset: u64, sigsetsize: u64) -> u64 {
    if sigsetsize != 8 {
        return neg_errno(errno::EINVAL);
    }
    let new = if set == 0 {
        None
    } else {
        let mut bytes = [0; 8];
        if let Err(e) = copy_from_user(&mut bytes, set) {
            return neg_errno(e.to_errno());
        }
        Some(SigSet(u64::from_le_bytes(bytes)))
    };
    let prev = current_blocked();
    if let Some(mask) = new {
        match prev.apply(how as u32, mask) {
            Ok(mask) => set_current_blocked(mask),
            Err(_) => return neg_errno(errno::EINVAL),
        }
    }
    if oldset != 0
        && let Err(e) = copy_to_user(oldset, &prev.set().0.to_le_bytes())
    {
        return neg_errno(e.to_errno());
    }
    0
}

/// Restore a validated frame and return every GPR through iretq.
/// rt_sigreturn() (15): validate and restore the user frame or force SIGSEGV.
pub(super) fn sys_rt_sigreturn(frame: &mut SyscallFrame) -> SyscallReturn {
    if let Err(error) = crate::linux::signal::frame::restore(frame) {
        crate::linux::signal::force_sigsegv(None, error);
    }
    SyscallReturn::Iret
}

/// `rt_sigpending(set, sigsetsize)` (127): M8 stub, replaced by C6 (#385).
pub(super) fn sys_rt_sigpending(_set: UserPtr, _sigsetsize: u64) -> u64 {
    not_implemented(general::__NR_rt_sigpending)
}

/// `rt_sigtimedwait(set, info, timeout, sigsetsize)` (128): M8 stub, replaced by C6 (#385).
pub(super) fn sys_rt_sigtimedwait(_set: UserPtr, _info: UserPtr, _timeout: UserPtr, _sigsetsize: u64) -> u64 {
    not_implemented(general::__NR_rt_sigtimedwait)
}

fn read_info(info: UserPtr) -> Result<[u8; 128], u32> {
    let mut bytes = [0; 128];
    copy_from_user(&mut bytes, info.addr()).map_err(|e| e.to_errno())?;
    Ok(bytes)
}
fn queued_info(target: i32, sig: i32, bytes: [u8; 128]) -> Result<Result<Option<SigInfo>, u32>, u32> {
    let code = i32::from_le_bytes(bytes[8..12].try_into().unwrap());
    if (code >= 0 || code == general::SI_TKILL) && target as u32 != current_thread().tid() {
        return Err(errno::EPERM);
    }
    Ok(signal_info(sig, bytes))
}
fn signal_info(sig: i32, bytes: [u8; 128]) -> Result<Option<SigInfo>, u32> {
    if sig == 0 {
        return Ok(None);
    }
    Ok(Some(SigInfo::from_user(Signal::new(sig as u32).ok_or(errno::EINVAL)?, bytes)))
}
fn sent(result: Result<(), u32>) -> u64 {
    super::finish(result.map(|()| 0))
}
/// rt_sigqueueinfo(tgid, sig, info) (129): copy, check sender code, then find and queue.
pub(super) fn sys_rt_sigqueueinfo(tgid: i32, sig: i32, info: UserPtr) -> u64 {
    sent(read_info(info).and_then(|bytes| {
        let info = queued_info(tgid, sig, bytes)?;
        if tgid <= 0 {
            return Err(errno::ESRCH);
        }
        send_process_checked(tgid, info, SenderClass::Other)
    }))
}
/// rt_tgsigqueueinfo(tgid, tid, sig, info) (297): copy before ID checks and send to one tid.
pub(super) fn sys_rt_tgsigqueueinfo(tgid: i32, tid: i32, sig: i32, info: UserPtr) -> u64 {
    sent(read_info(info).and_then(|bytes| {
        if tgid <= 0 || tid <= 0 {
            return Err(errno::EINVAL);
        }
        let info = queued_info(tid, sig, bytes)?;
        send_thread_checked(Some(tgid as u32), tid as u32, info, SenderClass::Other)
    }))
}

/// `rt_sigsuspend(mask, sigsetsize)` (130): M8 stub, replaced by C6 (#385).
pub(super) fn sys_rt_sigsuspend(_mask: UserPtr, _sigsetsize: u64) -> u64 {
    not_implemented(general::__NR_rt_sigsuspend)
}

/// sigaltstack(ss, old_ss) (131): configure using the interrupted rsp; an on-stack change is EPERM.
pub(super) fn sys_sigaltstack(ss: UserPtr, old_ss: UserPtr, frame: &SyscallFrame) -> u64 {
    let new = if ss.addr() == 0 {
        None
    } else {
        let mut bytes = [0; size_of::<abi::Stack>()];
        if let Err(e) = copy_from_user(&mut bytes, ss.addr()) {
            return neg_errno(e.to_errno());
        }
        Some((
            u64::from_le_bytes(bytes[offset_of!(abi::Stack, sp)..offset_of!(abi::Stack, sp) + 8].try_into().unwrap()),
            u64::from_le_bytes(bytes[offset_of!(abi::Stack, size)..].try_into().unwrap()),
            u32::from_le_bytes(
                bytes[offset_of!(abi::Stack, flags)..offset_of!(abi::Stack, flags) + 4].try_into().unwrap(),
            ),
        ))
    };
    let old = match current_thread().configure_alt_stack(frame.rsp, new) {
        Ok(old) => old,
        Err(e) => return e.errno() as i64 as u64,
    };
    if old_ss.addr() != 0 {
        let mut bytes = [0; size_of::<abi::Stack>()];
        bytes[offset_of!(abi::Stack, sp)..offset_of!(abi::Stack, sp) + 8].copy_from_slice(&old.sp().to_le_bytes());
        bytes[offset_of!(abi::Stack, flags)..offset_of!(abi::Stack, flags) + 4]
            .copy_from_slice(&old.status(frame.rsp).to_le_bytes());
        bytes[offset_of!(abi::Stack, size)..].copy_from_slice(&old.size().to_le_bytes());
        if let Err(e) = copy_to_user(old_ss.addr(), &bytes) {
            return neg_errno(e.to_errno());
        }
    }
    0
}
fn sender_info(sig: i32, code: i32) -> Result<Option<SigInfo>, u32> {
    let mut bytes = [0; 128];
    bytes[8..12].copy_from_slice(&code.to_le_bytes());
    bytes[16..20].copy_from_slice(&current_process().pid().to_le_bytes());
    signal_info(sig, bytes)
}
/// kill(pid, sig) (62): find process/group targets before checking the signal number.
pub(super) fn sys_kill(pid: i32, sig: i32) -> u64 {
    sent(send_process_checked(pid, sender_info(sig, general::SI_USER as i32), SenderClass::SiUser))
}
/// tkill(tid, sig) (200): validate a positive tid and send SI_TKILL.
pub(super) fn sys_tkill(tid: i32, sig: i32) -> u64 {
    thread_kill(None, tid, sig)
}
/// tgkill(tgid, tid, sig) (234): send only when the tid belongs to the requested group.
pub(super) fn sys_tgkill(tgid: i32, tid: i32, sig: i32) -> u64 {
    if tgid <= 0 {
        return neg_errno(errno::EINVAL);
    }
    thread_kill(Some(tgid as u32), tid, sig)
}
fn thread_kill(tgid: Option<u32>, tid: i32, sig: i32) -> u64 {
    if tid <= 0 {
        return neg_errno(errno::EINVAL);
    }
    sent(send_thread_checked(tgid, tid as u32, sender_info(sig, general::SI_TKILL), SenderClass::Other))
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
        // SIGKILL), a faulting `oldact` retaining the new action, the three
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
            // A faulting oldact: EFAULT after installing the new action.
            put_act(&KernelSigaction { handler: 0x3000, ..act })?;
            kassert_eq!(call(sigaction, usr1, act_at, unmapped, 8), neg_errno(errno::EFAULT));
            kassert_eq!(current_action(Signal::USR1).handler, 0x3000);
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

            // Input faults precede semantic checks; output faults follow install.
            kassert_eq!(call(sigaction, 0, unmapped, 0, 8), neg_errno(errno::EFAULT));
            kassert_eq!(call(sigprocmask, 7, unmapped, 0, 8), neg_errno(errno::EFAULT));
            put_set(SigSet::of(&[Signal::ALRM]))?;
            kassert_eq!(
                call(sigprocmask, u64::from(general::SIG_SETMASK), set_at, unmapped, 8),
                neg_errno(errno::EFAULT)
            );
            kassert_eq!(current_blocked().set(), SigSet::of(&[Signal::ALRM]));
            set_current_blocked(Blocked::new(SigSet::of(&[Signal::TERM])));

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
                kassert_eq!(extra.process().action(Signal::USR1).handler, 0x3000);

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
