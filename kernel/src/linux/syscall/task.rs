//! Tasks (M8's cluster, ARCHITECTURE section 8, "B0's stubs"): M6's `exit`
//! and `exit_group`, `getpid`, `gettid`, `getppid` and `set_tid_address`,
//! moved here from `dispatch`'s arms (B1 keeps their behaviour, reading the
//! running thread's `UserThread` and its `Process`); and
//! M8's `clone`, `fork`, `vfork`, `execve`, `wait4`, `waitid`, `setpgid`,
//! `setsid`, `getpgid`, `getsid`.

use super::at::UserPtr;
use super::not_implemented;
use crate::arch;
use crate::arch::syscall::{SyscallFrame, SyscallReturn};
use crate::linux::process;
use crate::linux::process::{current_process, current_thread, record_exit};
use crate::linux::{errno, general};
use crate::user::UserExit;

/// `exit` and `exit_group` (one arm in `dispatch`).
pub(super) fn sys_exit(frame: &mut SyscallFrame) -> SyscallReturn {
    let status = (frame.rdi & 0xff) as i32;
    let (_, is_test) = record_exit(UserExit::Exited(status));
    if is_test {
        crate::user::note_exit_status(status);
    }
    // SAFETY: unwinds cleanly back to `enter_ring3`; no lock held and
    // no live Rust `Drop` values on this stack frame.
    unsafe { arch::syscall::unwind_to_kernel() }
}

pub(super) fn sys_getpid(frame: &mut SyscallFrame) -> SyscallReturn {
    frame.rax = u64::from(current_process().pid());
    SyscallReturn::Sysret
}

pub(super) fn sys_getppid(frame: &mut SyscallFrame) -> SyscallReturn {
    frame.rax = u64::from(current_process().ppid());
    SyscallReturn::Sysret
}

pub(super) fn sys_gettid(frame: &mut SyscallFrame) -> SyscallReturn {
    frame.rax = u64::from(current_thread().tid());
    SyscallReturn::Sysret
}

pub(super) fn sys_set_tid_address(frame: &mut SyscallFrame) -> SyscallReturn {
    let tidptr = frame.rdi;
    let thread = current_thread();
    thread.set_clear_child_tid(tidptr);
    frame.rax = u64::from(thread.tid());
    SyscallReturn::Sysret
}

/// Validate M8's process-clone flags before allocating anything.
fn clone_flags(flags: u32, tls: u64) -> Result<(), u32> {
    let allowed = general::CSIGNAL
        | general::CLONE_VM
        | general::CLONE_FS
        | general::CLONE_FILES
        | general::CLONE_SIGHAND
        | general::CLONE_THREAD
        | general::CLONE_VFORK
        | general::CLONE_SETTLS
        | general::CLONE_PARENT_SETTID
        | general::CLONE_CHILD_SETTID
        | general::CLONE_CHILD_CLEARTID
        | general::CLONE_SYSVSEM
        | general::CLONE_DETACHED
        | general::CLONE_IO
        | general::CLONE_PTRACE
        | general::CLONE_UNTRACED;
    if flags & !allowed != 0
        || (flags & general::CLONE_THREAD != 0 && flags & general::CLONE_SIGHAND == 0)
        || (flags & general::CLONE_SIGHAND != 0 && flags & general::CLONE_VM == 0)
        || (flags & general::CLONE_SIGHAND != 0 && flags & general::CLONE_THREAD == 0)
        || (flags & general::CLONE_THREAD == 0 && flags & general::CSIGNAL != general::SIGCHLD)
        || (flags & general::CLONE_THREAD != 0
            && flags & (general::CLONE_FILES | general::CLONE_FS) != (general::CLONE_FILES | general::CLONE_FS))
    {
        return Err(errno::EINVAL);
    }
    if flags & general::CLONE_SETTLS != 0 && tls >= crate::user::USER_MAX {
        return Err(errno::EPERM);
    }
    // Thread creation and group exit are C3; never silently create a process
    // for a request to create a thread.
    if flags & general::CLONE_THREAD != 0 {
        return Err(errno::ENOSYS);
    }
    Ok(())
}

/// A process clone returns its pid to the parent and zero to the child.
pub(super) fn sys_clone(
    flags: u64,
    newsp: UserPtr,
    parent_tid: UserPtr,
    child_tid: UserPtr,
    tls: u64,
    frame: &SyscallFrame,
) -> u64 {
    super::finish((|| {
        let flags = u32::try_from(flags).map_err(|_| errno::EINVAL)?;
        clone_flags(flags, tls)?;
        process::fork_current(flags, newsp.addr(), parent_tid.addr(), child_tid.addr(), tls, frame)
    })())
}

pub(super) fn sys_fork(frame: &SyscallFrame) -> u64 {
    super::finish(process::fork_current(general::SIGCHLD, 0, 0, 0, 0, frame))
}

pub(super) fn sys_vfork(frame: &SyscallFrame) -> u64 {
    super::finish(process::fork_current(general::CLONE_VM | general::CLONE_VFORK | general::SIGCHLD, 0, 0, 0, 0, frame))
}

/// `execve(path, argv, envp)` (59): M8 stub, replaced by C2 (#381). It
/// returns through the frame, which a successful `execve` rewrites to the
/// new image's entry.
pub(super) fn sys_execve(_path: UserPtr, _argv: UserPtr, _envp: UserPtr, frame: &mut SyscallFrame) -> SyscallReturn {
    frame.rax = not_implemented(general::__NR_execve);
    SyscallReturn::Sysret
}

/// Reap before copyout, as Linux does even when a status pointer faults.
pub(super) fn sys_wait4(pid: i32, wstatus: UserPtr, options: u32, rusage: UserPtr) -> u64 {
    super::finish((|| {
        let request = process::wait4_request(pid, options)?;
        let Some(report) = process::wait_child(request)? else {
            return Ok(0);
        };
        if !wstatus.is_null() {
            crate::user::copy_to_user(wstatus.addr(), &report.status().word().to_ne_bytes())
                .map_err(|_| errno::EFAULT)?;
        }
        write_rusage(rusage)?;
        Ok(u64::from(report.pid()))
    })())
}

/// A nonblocking wait with no event clears the siginfo fields. WNOWAIT
/// reports a zombie without releasing its pid or process-table entry.
pub(super) fn sys_waitid(idtype: i32, id: i32, infop: UserPtr, options: u32, rusage: UserPtr) -> u64 {
    super::finish((|| {
        let request = process::waitid_request(idtype, id, options)?;
        let report = process::wait_child(request)?;
        if report.is_some() {
            write_rusage(rusage)?;
        }
        if !infop.is_null() {
            // siginfo_t's child fields: three ints, padding, pid, uid,
            // status, padding, utime and stime. Zeroed bytes avoid exposing
            // kernel padding; C12 supplies tick accounting.
            let mut info = [0u8; core::mem::size_of::<general::siginfo_t>()];
            if let Some(report) = report {
                let status = report.status().siginfo();
                info[0..4].copy_from_slice(&general::SIGCHLD.to_ne_bytes());
                info[8..12].copy_from_slice(&status.code.to_ne_bytes());
                info[16..20].copy_from_slice(&report.pid().to_ne_bytes());
                info[24..28].copy_from_slice(&status.status.to_ne_bytes());
            }
            crate::user::copy_to_user(infop.addr(), &info).map_err(|_| errno::EFAULT)?;
        }
        Ok(0)
    })())
}

fn write_rusage(ptr: UserPtr) -> Result<(), u32> {
    if !ptr.is_null() {
        // Tick accounting is C12. No other resource counters are maintained.
        crate::user::copy_to_user(ptr.addr(), &[0; core::mem::size_of::<general::rusage>()])
            .map_err(|_| errno::EFAULT)?;
    }
    Ok(())
}

/// `setpgid(pid, pgid)` (109): M8 stub, replaced by C8 (#387).
pub(super) fn sys_setpgid(_pid: i32, _pgid: i32) -> u64 {
    not_implemented(general::__NR_setpgid)
}

/// `setsid()` (112): M8 stub, replaced by C8 (#387).
pub(super) fn sys_setsid() -> u64 {
    not_implemented(general::__NR_setsid)
}

/// `getpgid(pid)` (121): M8 stub, replaced by C8 (#387).
pub(super) fn sys_getpgid(_pid: i32) -> u64 {
    not_implemented(general::__NR_getpgid)
}

/// `getsid(pid)` (124): M8 stub, replaced by C8 (#387).
pub(super) fn sys_getsid(_pid: i32) -> u64 {
    not_implemented(general::__NR_getsid)
}

crate::ktest!(
    fn identity_syscalls_name_the_process_and_thread() {
        // Through `dispatch`: `set_tid_address` stores its pointer and
        // returns the tid; the leader's tid and the process's pid and group
        // are the pid; a root's parent is 0.
        use crate::kassert_eq;
        use crate::linux::process::in_test_process;
        in_test_process(|pid| {
            let call = |nr: u32, rdi: u64| {
                let mut frame = SyscallFrame { rax: u64::from(nr), rdi, ..SyscallFrame::default() };
                let _ = super::dispatch(&mut frame);
                frame.rax
            };
            kassert_eq!(call(general::__NR_set_tid_address, 0x5000), u64::from(pid));
            kassert_eq!(current_thread().clear_child_tid(), 0x5000);
            kassert_eq!(call(general::__NR_gettid, 0), u64::from(pid));
            kassert_eq!(call(general::__NR_getpid, 0), u64::from(pid));
            kassert_eq!(call(general::__NR_getppid, 0), 0);
            kassert_eq!(call(general::__NR_getpgrp, 0), u64::from(pid));
            Ok(())
        })?
    }
);
