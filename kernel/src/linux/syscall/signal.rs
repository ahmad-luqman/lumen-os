//! Linux signal dispositions, masks, senders, frames and alternate stacks.
//! User copies happen outside the signal lock, with Linux's install-before-
//! copy-out ordering. Interruptible waits retain masks until signal delivery.

use super::at::UserPtr;
use super::{copy_pod_from_user, copy_pod_to_user, neg_errno};
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

/// `rt_sigpending(set, sigsetsize)` (127): blocked private | shared pending
/// signals, accepting short output sets as Linux's do_sigpending does.
pub(super) fn sys_rt_sigpending(set: UserPtr, sigsetsize: u64) -> u64 {
    if sigsetsize > 8 {
        return neg_errno(errno::EINVAL);
    }
    let pending = crate::linux::process::current_pending().0.to_le_bytes();
    match copy_to_user(set.addr(), &pending[..sigsetsize as usize]) {
        Ok(()) => 0,
        Err(e) => neg_errno(e.to_errno()),
    }
}

/// Consume a wanted pending signal, or wait interruptibly until its deadline.
/// The signal is consumed even when copying its siginfo fails.
pub(super) fn sys_rt_sigtimedwait(set: UserPtr, info: UserPtr, timeout: UserPtr, sigsetsize: u64) -> u64 {
    super::finish((|| {
        let wanted = read_mask(set, sigsetsize)?;
        let deadline = if timeout.is_null() {
            None
        } else {
            let ns = super::time::read_timespec(timeout.addr())?;
            Some(crate::time::now_ns().saturating_add(ns))
        };
        let got = current_thread().wait_signal(wanted, deadline)?;
        if !info.is_null() {
            copy_to_user(info.addr(), got.bytes()).map_err(|e| e.to_errno())?;
        }
        Ok(u64::from(got.signal().number()))
    })())
}

fn read_mask(ptr: UserPtr, size: u64) -> Result<SigSet, u32> {
    if size != 8 {
        return Err(errno::EINVAL);
    }
    let mut bytes = [0; 8];
    copy_from_user(&mut bytes, ptr.addr()).map_err(|e| e.to_errno())?;
    Ok(SigSet(u64::from_ne_bytes(bytes)).unblockable_stripped())
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

/// Wait with a temporary mask; the delivered frame restores the original mask.
pub(super) fn sys_rt_sigsuspend(mask: UserPtr, sigsetsize: u64) -> u64 {
    let mask = match read_mask(mask, sigsetsize) {
        Ok(mask) => mask,
        Err(error) => return neg_errno(error),
    };
    current_thread().install_temporary_mask(lumen_signal::Blocked::new(mask));
    sys_pause()
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

/// Wait for pending work; only delivery of a handler makes this return EINTR.
pub(super) fn sys_pause() -> u64 {
    let _ = crate::sched::WaitQueue::new().wait_interruptible(None, || false);
    neg_errno(lumen_signal::restart::RestartCode::NoHand as u32)
}

/// `restart_syscall()` (219, B2): resumes the thread's restart block
/// (`linux::signal::restart_syscall`), `-EINTR` with none. The syscall-exit
/// hook makes a call interrupted with `-ERESTART_RESTARTBLOCK` this one.
pub(super) fn sys_restart_syscall(frame: &mut SyscallFrame) -> SyscallReturn {
    frame.rax = crate::linux::signal::restart_syscall();
    SyscallReturn::Sysret
}

crate::ktest!(
    fn signal_waits_validate_and_consume_pending() {
        use super::at::FromReg;
        use crate::linux::process::{self, KTEST_USER_VA as A};
        use crate::{kassert, kassert_eq};
        use lumen_signal::Blocked;
        process::in_test_process(|pid| {
            let ptr = UserPtr::from_reg;
            let mask = SigSet::of(&[Signal::USR1, Signal::USR2]);
            set_current_blocked(Blocked::new(mask));
            copy_to_user(A, &mask.0.to_ne_bytes()).map_err(|_| "mask")?;
            copy_to_user(A + 16, &[0; 16]).map_err(|_| "timeout")?;
            let send = |sig| send_process_checked(pid as i32, Ok(Some(SigInfo::new(sig, 0))), SenderClass::SiUser);
            send(Signal::USR1).map_err(|_| "send")?;
            process::send_thread(None, pid, Some(SigInfo::new(Signal::USR2, 0)), SenderClass::SiUser)
                .map_err(|_| "private send")?;
            kassert_eq!(sys_rt_sigpending(ptr(A + 64), 9), neg_errno(errno::EINVAL));
            kassert_eq!(sys_rt_sigpending(ptr(A + 64), 8), 0);
            let mut bits = [0; 8];
            copy_from_user(&mut bits, A + 64).map_err(|_| "pending")?;
            kassert_eq!(u64::from_ne_bytes(bits), mask.0);
            kassert_eq!(sys_rt_sigpending(ptr(0), 0), 0);
            kassert_eq!(sys_rt_sigpending(ptr(0), 8), neg_errno(errno::EFAULT));
            kassert_eq!(sys_rt_sigtimedwait(ptr(A), ptr(0), ptr(A + 16), 4), neg_errno(errno::EINVAL));
            kassert_eq!(sys_rt_sigtimedwait(ptr(A), ptr(A + 128), ptr(A + 16), 8), u64::from(Signal::USR2.number()));
            let info = read_info(ptr(A + 128)).map_err(|_| "siginfo")?;
            kassert_eq!(info[0], Signal::USR2.number() as u8);
            // Copyout failure still consumes the shared signal.
            kassert_eq!(sys_rt_sigtimedwait(ptr(A), ptr(1), ptr(A + 16), 8), neg_errno(errno::EFAULT));
            kassert_eq!(sys_rt_sigtimedwait(ptr(A), ptr(0), ptr(A + 16), 8), neg_errno(errno::EAGAIN));
            kassert_eq!(current_blocked().set(), mask);
            kassert_eq!(crate::linux::process::current_pending(), SigSet::EMPTY);
            // A positive timeout must wait for its deadline, not just return
            // EAGAIN. Only a floor is asserted: a stalled host may add time.
            const INTERVAL: u64 = 30_000_000;
            super::time::write_timespec(A + 16, INTERVAL).map_err(|_| "positive timeout")?;
            let start = crate::time::now_ns();
            kassert_eq!(sys_rt_sigtimedwait(ptr(A), ptr(0), ptr(A + 16), 8), neg_errno(errno::EAGAIN));
            kassert!(crate::time::now_ns() - start >= INTERVAL);
            // Block until a default-fatal signal is sent. real_blocked must
            // prevent complete_signal from turning this into a group exit.
            for sig in [Signal::USR1, Signal::CHLD] {
                set_current_blocked(Blocked::new(SigSet::of(&[Signal::USR1, Signal::USR2, Signal::CHLD])));
                let worker = process::spawn_test_thread(move |tid| {
                    let me = current_thread();
                    me.set_blocked(Blocked::new(SigSet::of(&[sig])));
                    let got = me.wait_signal(SigSet::of(&[sig]), None);
                    (tid, got, crate::sched::current_sigpending(), me.blocked())
                })?;
                let blocked = worker.settle();
                let sent = send(sig);
                let (_, got, pending, restored) = worker.join()?;
                kassert!(blocked);
                kassert_eq!(sent, Ok(()));
                kassert_eq!(got.map(|i| i.signal()), Ok(sig));
                kassert!(!pending);
                kassert_eq!(restored, Blocked::new(SigSet::of(&[sig])));
            }
            Ok(())
        })?
    }
);

crate::ktest!(
    fn temporary_masks_survive_until_handler_and_sigreturn() {
        use crate::linux::process::{self, KTEST_USER_LEN, KTEST_USER_VA as A};
        use crate::linux::signal;
        use crate::{kassert, kassert_eq};
        use lumen_signal::{Blocked, restart::SyscallExit};
        process::in_test_process(|pid| {
            for nr in [general::__NR_ppoll, general::__NR_pselect6, general::__NR_rt_sigsuspend, general::__NR_pause] {
                let mask = Blocked::new(SigSet::of(&[Signal::USR1]));
                set_current_blocked(if nr == general::__NR_pause { Blocked::EMPTY } else { mask });
                set_current_action(
                    Signal::USR1,
                    KernelSigaction {
                        handler: 0x400100,
                        restorer: 0x400200,
                        flags: u64::from(general::SA_RESTORER | general::SA_RESTART),
                        mask: 0,
                    },
                );
                send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::USR1, 0))), SenderClass::SiUser)
                    .map_err(|_| "send")?;
                copy_to_user(A, &0u64.to_ne_bytes()).map_err(|_| "temporary mask")?;
                copy_to_user(A + 16, &A.to_ne_bytes()).map_err(|_| "mask pointer")?;
                copy_to_user(A + 24, &8u64.to_ne_bytes()).map_err(|_| "mask length")?;
                let mut frame = SyscallFrame {
                    rax: u64::from(nr),
                    rip: 0x400002,
                    rsp: A + KTEST_USER_LEN,
                    cs: 0x23,
                    ss: 0x1b,
                    rflags: 0x202,
                    ..Default::default()
                };
                match nr {
                    general::__NR_ppoll => {
                        frame.r10 = A;
                        frame.r8 = 8;
                    }
                    general::__NR_pselect6 => {
                        frame.r9 = A + 16;
                    }
                    general::__NR_rt_sigsuspend => {
                        frame.rdi = A;
                        frame.rsi = 8;
                    }
                    _ => {}
                }
                super::dispatch(&mut frame);
                kassert_eq!(frame.rax, neg_errno(lumen_signal::restart::RestartCode::NoHand as u32));
                kassert_eq!(current_blocked(), Blocked::EMPTY);
                kassert!(signal::exit_to_user(&mut frame, SyscallExit::new(u64::from(nr))));
                kassert_eq!(frame.rip, 0x400100);
                frame.rsp += 8;
                frame.r11 = 0x202;
                signal::frame::restore(&mut frame).map_err(|_| "sigreturn")?;
                kassert_eq!(frame.rax, neg_errno(errno::EINTR));
                kassert_eq!(frame.rip, 0x400002);
                kassert_eq!(current_blocked(), if nr == general::__NR_pause { Blocked::EMPTY } else { mask });
                kassert!(!signal::exit_to_user(&mut frame, None));
                kassert_eq!(frame.rip, 0x400002);
            }
            // Conversely, a temporary mask that blocks an already-notified
            // signal must allow the wait to time out, not spuriously interrupt.
            set_current_blocked(Blocked::EMPTY);
            send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::USR1, 0))), SenderClass::SiUser)
                .map_err(|_| "send before masking")?;
            copy_to_user(A, &Signal::USR1.bit().to_ne_bytes()).map_err(|_| "blocking mask")?;
            copy_to_user(A + 32, &[0; 16]).map_err(|_| "timeout seconds")?;
            copy_to_user(A + 40, &1_000_000u64.to_ne_bytes()).map_err(|_| "timeout nanos")?;
            let mut frame = SyscallFrame {
                rax: u64::from(general::__NR_ppoll),
                rdx: A + 32,
                r10: A,
                r8: 8,
                rip: 0x400002,
                rsp: A + KTEST_USER_LEN,
                cs: 0x23,
                ss: 0x1b,
                rflags: 0x202,
                ..Default::default()
            };
            super::dispatch(&mut frame);
            kassert_eq!(frame.rax, 0);
            kassert_eq!(current_blocked(), Blocked::EMPTY);
            kassert!(signal::exit_to_user(&mut frame, SyscallExit::new(u64::from(general::__NR_ppoll))));
            frame.rsp += 8;
            frame.r11 = 0x202;
            signal::frame::restore(&mut frame).map_err(|_| "sigreturn after timeout")?;
            kassert_eq!(frame.rax, 0);
            Ok(())
        })?
    }
);

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
