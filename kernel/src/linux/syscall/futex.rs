//! Linux futex operations, robust-list registration and interruptible sleeps.

use core::num::NonZeroU32;
use core::sync::atomic::Ordering;
use lumen_signal::restart::RestartCode;

use super::{at::UserPtr, finish};
use crate::linux::{errno, futex, general, process, signal};
use crate::{sched, time, user};

const NS: u64 = 1_000_000_000;

fn timespec(addr: u64) -> Result<u64, u32> {
    let mut bytes = [0; 16];
    user::copy_from_user(&mut bytes, addr).map_err(|_| errno::EFAULT)?;
    let sec = i64::from_ne_bytes(bytes[..8].try_into().unwrap());
    let nsec = i64::from_ne_bytes(bytes[8..].try_into().unwrap());
    if sec < 0 || !(0..NS as i64).contains(&nsec) {
        return Err(errno::EINVAL);
    }
    Ok((sec as u64).saturating_mul(NS).saturating_add(nsec as u64))
}

fn deadline(duration: u64, absolute: bool, realtime: bool) -> u64 {
    if !absolute {
        return time::now_ns().saturating_add(duration);
    }
    if !realtime {
        return duration;
    }
    // Read monotonic after realtime so conversion cannot shorten a sleep.
    let (sec, nsec) = time::realtime_sec_nsec();
    let now = sec.saturating_mul(NS).saturating_add(nsec);
    time::now_ns().saturating_add(duration.saturating_sub(now))
}

fn wait(addr: u64, private: bool, value: u32, bits: NonZeroU32, deadline: Option<u64>) -> Result<u64, u32> {
    match futex::wait(addr, private, value, bits, deadline) {
        Err(signal::ERESTARTSYS) if deadline.is_some() => Err(signal::interrupted_with(signal::RestartBlock {
            resume: resume_futex,
            deadline,
            args: [addr, u64::from(private), u64::from(value), u64::from(bits.get())],
        })),
        result => result,
    }
}

fn resume_futex(block: &signal::RestartBlock) -> u64 {
    finish(wait(
        block.args[0],
        block.args[1] != 0,
        block.args[2] as u32,
        NonZeroU32::new(block.args[3] as u32).expect("saved nonzero bitset"),
        block.deadline,
    ))
}

/// Dispatch the six supported non-PI operations; all others return ENOSYS.
pub(super) fn sys_futex(uaddr: UserPtr, op: u32, val: u32, timeout_or_val2: u64, uaddr2: UserPtr, val3: u32) -> u64 {
    finish(do_futex(uaddr.addr(), op, val, timeout_or_val2, uaddr2.addr(), val3))
}

fn do_futex(addr: u64, op: u32, val: u32, timeout: u64, dest: u64, val3: u32) -> Result<u64, u32> {
    let command = op & !(general::FUTEX_PRIVATE_FLAG | general::FUTEX_CLOCK_REALTIME);
    let private = op & general::FUTEX_PRIVATE_FLAG != 0;
    let realtime = op & general::FUTEX_CLOCK_REALTIME != 0;
    if realtime && command != general::FUTEX_WAIT_BITSET {
        return Err(errno::ENOSYS);
    }
    match command {
        general::FUTEX_WAIT | general::FUTEX_WAIT_BITSET => {
            let bits = if command == general::FUTEX_WAIT { u32::MAX } else { val3 };
            let bits = NonZeroU32::new(bits).ok_or(errno::EINVAL)?;
            let until = if timeout == 0 {
                None
            } else {
                Some(deadline(timespec(timeout)?, command == general::FUTEX_WAIT_BITSET, realtime))
            };
            wait(addr, private, val, bits, until)
        }
        general::FUTEX_WAKE | general::FUTEX_WAKE_BITSET => {
            let bits = if command == general::FUTEX_WAKE { u32::MAX } else { val3 };
            futex::wake(addr, private, val as i32, NonZeroU32::new(bits).ok_or(errno::EINVAL)?)
        }
        general::FUTEX_REQUEUE | general::FUTEX_CMP_REQUEUE => futex::requeue(
            addr,
            private,
            val as i32,
            dest,
            timeout as u32 as i32,
            (command == general::FUTEX_CMP_REQUEUE).then_some(val3),
        ),
        _ => Err(errno::ENOSYS),
    }
}

/// Store an opaque head pointer; it is accessed only during robust cleanup.
pub(super) fn sys_set_robust_list(head: UserPtr, len: u64) -> u64 {
    if len != 24 {
        return finish(Err(errno::EINVAL));
    }
    process::current_thread().robust_list.store(head.addr(), Ordering::Relaxed);
    0
}

/// Root may inspect any live tid; pid zero selects the calling thread.
pub(super) fn sys_get_robust_list(pid: i32, head_ptr: UserPtr, len_ptr: UserPtr) -> u64 {
    finish((|| {
        let thread = if pid == 0 {
            process::current_thread()
        } else {
            process::lookup_thread(pid as u32).ok_or(errno::ESRCH)?
        };
        let head = thread.robust_list.load(Ordering::Relaxed);
        user::copy_to_user(len_ptr.addr(), &24u64.to_ne_bytes()).map_err(|_| errno::EFAULT)?;
        user::copy_to_user(head_ptr.addr(), &head.to_ne_bytes()).map_err(|_| errno::EFAULT)?;
        Ok(0)
    })())
}

fn sleep(until: u64, rem: u64, absolute: bool) -> Result<u64, u32> {
    let queue = sched::WaitQueue::new();
    if queue.wait_interruptible(Some(until), || false) == sched::Wake::TimedOut {
        return Ok(0);
    }
    if absolute {
        return Err(RestartCode::NoHand as u32);
    }
    if rem != 0 {
        let left = until.saturating_sub(time::now_ns());
        let mut bytes = [0; 16];
        bytes[..8].copy_from_slice(&(left / NS).to_ne_bytes());
        bytes[8..].copy_from_slice(&(left % NS).to_ne_bytes());
        user::copy_to_user(rem, &bytes).map_err(|_| errno::EFAULT)?;
    }
    Err(signal::interrupted_with(signal::RestartBlock {
        resume: resume_sleep,
        deadline: Some(until),
        args: [rem, 0, 0, 0],
    }))
}

fn resume_sleep(block: &signal::RestartBlock) -> u64 {
    finish(sleep(block.deadline.expect("sleep deadline"), block.args[0], false))
}

/// Sleep for at least req on the monotonic clock, preserving its deadline
/// across restart_syscall and reporting remaining time on interruption.
pub(super) fn sys_nanosleep(req: UserPtr, rem: UserPtr) -> u64 {
    finish(timespec(req.addr()).and_then(|ns| sleep(deadline(ns, false, false), rem.addr(), false)))
}

/// Relative sleeps restart with a saved deadline. Absolute sleeps leave rem
/// untouched and return ERESTARTNOHAND when interrupted.
pub(super) fn sys_clock_nanosleep(clockid: i32, flags: u32, req: UserPtr, rem: UserPtr) -> u64 {
    finish((|| {
        if ![general::CLOCK_REALTIME, general::CLOCK_MONOTONIC, general::CLOCK_BOOTTIME].contains(&(clockid as u32)) {
            return Err(errno::EINVAL);
        }
        let ns = timespec(req.addr())?;
        let absolute = flags & general::TIMER_ABSTIME != 0;
        sleep(deadline(ns, absolute, clockid as u32 == general::CLOCK_REALTIME), rem.addr(), absolute)
    })())
}

crate::ktest!(
    fn sleep_and_futex_restart_preserve_deadlines() {
        use crate::linux::process::{KTEST_USER_VA as A, in_test_process};
        use crate::{kassert, kassert_eq};
        in_test_process(|_| {
            let me = sched::current().ok_or("scheduler thread")?;
            let until = time::now_ns().saturating_add(60 * NS);
            signal::raise_for_test(&me, lumen_signal::Signal::USR1);
            kassert_eq!(sleep(until, A, false), Err(RestartCode::RestartBlock as u32));
            let block = sched::take_restart_block().ok_or("sleep restart block")?;
            kassert_eq!(block.deadline, Some(until));
            kassert_eq!(block.args[0], A);
            let remaining = timespec(A).map_err(|_| "remaining time")?;
            kassert!(remaining <= 60 * NS);
            // A saved expired deadline must win over a pending signal, without
            // re-reading the request or installing another restart block.
            let expired = signal::RestartBlock { deadline: Some(0), ..block };
            kassert_eq!((expired.resume)(&expired), 0);
            kassert!(sched::restart_block().is_none());
            kassert_eq!(sleep(until, u64::MAX, true), Err(RestartCode::NoHand as u32));
            kassert!(sched::restart_block().is_none());
            kassert_eq!(sleep(until, u64::MAX, false), Err(errno::EFAULT));
            kassert!(sched::restart_block().is_none());
            user::copy_to_user(A, &0u32.to_ne_bytes()).map_err(|_| "word")?;
            let bits = NonZeroU32::new(7).unwrap();
            kassert_eq!(wait(A, true, 0, bits, Some(until)), Err(RestartCode::RestartBlock as u32));
            let block = sched::take_restart_block().ok_or("futex restart block")?;
            kassert_eq!(block.deadline, Some(until));
            kassert_eq!(block.args, [A, 1, 0, 7]);
            let expired = signal::RestartBlock { deadline: Some(0), ..block };
            kassert_eq!((expired.resume)(&expired), finish(Err(errno::ETIMEDOUT)));
            kassert!(sched::restart_block().is_none());
            kassert_eq!(wait(A, true, 0, bits, None), Err(signal::ERESTARTSYS));
            kassert!(sched::restart_block().is_none());
            sched::clear_current_sigpending();
            Ok(())
        })?
    }
);
