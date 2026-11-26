//! Linux futex operations, robust-list registration and interruptible sleeps.

use core::num::NonZeroU32;
#[cfg(feature = "ktest")]
use lumen_signal::restart::RestartCode;

use super::{at::UserPtr, finish};
use crate::linux::{errno, futex, general, process, signal};
use crate::{sched, time, user};

use super::time::{ERESTARTNOHAND, NS_PER_SEC as NS, read_timespec as timespec, write_timespec};

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
            // args: user word, private flag (0/1), expected value, nonzero bitset.
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

/// `futex(uaddr, op, val, timeout_or_val2, uaddr2, val3)` (202): six non-PI
/// operations; all other operations return ENOSYS.
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
            let bits = if command == general::FUTEX_WAIT { lumen_futex::FUTEX_BITSET_MATCH_ANY.get() } else { val3 };
            let until = if timeout == 0 {
                None
            } else {
                Some(deadline(timespec(timeout)?, command == general::FUTEX_WAIT_BITSET, realtime))
            };
            wait(addr, private, val, NonZeroU32::new(bits).ok_or(errno::EINVAL)?, until)
        }
        general::FUTEX_WAKE | general::FUTEX_WAKE_BITSET => {
            let bits = if command == general::FUTEX_WAKE { lumen_futex::FUTEX_BITSET_MATCH_ANY.get() } else { val3 };
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

/// `set_robust_list(head, len)` (273): store an opaque head pointer,
/// dereferenced only during cleanup.
pub(super) fn sys_set_robust_list(head: UserPtr, len: u64) -> u64 {
    if len != size_of::<general::robust_list_head>() as u64 {
        return finish(Err(errno::EINVAL));
    }
    process::current_thread().set_robust_list(head.addr());
    0
}

/// `get_robust_list(pid, head_ptr, len_ptr)` (274): pid zero selects the caller.
/// Every Lumen process is root (Linux: `ptrace_may_access`), so any live tid is allowed.
pub(super) fn sys_get_robust_list(pid: i32, head_ptr: UserPtr, len_ptr: UserPtr) -> u64 {
    finish((|| {
        let thread = if pid == 0 {
            process::current_thread()
        } else {
            process::lookup_thread(pid as u32).ok_or(errno::ESRCH)?
        };
        let head = thread.robust_list();
        user::copy_to_user(len_ptr.addr(), &(size_of::<general::robust_list_head>() as u64).to_ne_bytes())
            .map_err(|_| errno::EFAULT)?;
        user::copy_to_user(head_ptr.addr(), &head.to_ne_bytes()).map_err(|_| errno::EFAULT)?;
        Ok(0)
    })())
}

fn sleep(until: u64, rem: u64, absolute: bool) -> Result<u64, u32> {
    let queue = sched::WaitQueue::new();
    if queue.wait_interruptible(Some(until), || false) == sched::Wake::TimedOut {
        return Ok(0);
    }
    interrupted_sleep(until, rem, absolute)
}

fn interrupted_sleep(until: u64, rem: u64, absolute: bool) -> Result<u64, u32> {
    if absolute {
        return Err(ERESTARTNOHAND);
    }
    let left = until.saturating_sub(time::now_ns());
    if left == 0 {
        return Ok(0);
    }
    if rem != 0 {
        write_timespec(rem, left)?;
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

/// `nanosleep(req, rem)` (35): sleep for at least req on the monotonic clock.
/// Preserve its deadline across restart_syscall and report remaining time on interruption.
pub(super) fn sys_nanosleep(req: UserPtr, rem: UserPtr) -> u64 {
    finish(timespec(req.addr()).and_then(|ns| sleep(deadline(ns, false, false), rem.addr(), false)))
}

/// `clock_nanosleep(clockid, flags, req, rem)` (230): relative sleeps restart
/// with a saved deadline. Absolute sleeps leave rem untouched and return
/// ERESTARTNOHAND when interrupted.
pub(super) fn sys_clock_nanosleep(clockid: i32, flags: u32, req: UserPtr, rem: UserPtr) -> u64 {
    finish((|| {
        match clockid as u32 {
            general::CLOCK_REALTIME | general::CLOCK_MONOTONIC | general::CLOCK_BOOTTIME => {}
            general::CLOCK_MONOTONIC_RAW | general::CLOCK_REALTIME_COARSE | general::CLOCK_MONOTONIC_COARSE => {
                return Err(errno::EOPNOTSUPP);
            }
            // CLOCK_TAI and CPU-time sleeps are unsupported deviations.
            _ => return Err(errno::EINVAL),
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
            user::copy_to_user(A, &[0xff; 16]).map_err(|_| "prefill rem")?;
            kassert_eq!(sleep(until, A, false), Err(RestartCode::RestartBlock as u32));
            let after = time::now_ns();
            let block = sched::take_restart_block().ok_or("sleep restart block")?;
            kassert_eq!(block.deadline, Some(until));
            kassert_eq!(block.args[0], A);
            let remaining = timespec(A).map_err(|_| "remaining time")?;
            kassert!(remaining <= 60 * NS);
            kassert!(remaining >= until.saturating_sub(after));
            kassert_eq!(interrupted_sleep(0, u64::MAX, false), Ok(0));
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
