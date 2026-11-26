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
use crate::linux::process::{self, ProcessClone, clone_flags};
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

/// `clone` (56): the low 32 flag bits select a process clone. C3 adds threads.
pub(super) fn sys_clone(
    flags: u32,
    newsp: UserPtr,
    parent_tid: UserPtr,
    child_tid: UserPtr,
    tls: u64,
    frame: &SyscallFrame,
) -> u64 {
    super::finish(
        clone_flags(flags, tls).and_then(|flags| {
            process::fork_current(flags, newsp.addr(), parent_tid.addr(), child_tid.addr(), tls, frame)
        }),
    )
}

/// `fork()` (57): return the child's pid to its parent, zero to the child.
pub(super) fn sys_fork(frame: &SyscallFrame) -> u64 {
    super::finish(process::fork_current(ProcessClone::FORK, 0, 0, 0, 0, frame))
}

/// `vfork()` (58): share the mm and wait killably for child exit (C2: exec).
pub(super) fn sys_vfork(frame: &SyscallFrame) -> u64 {
    super::finish(process::fork_current(ProcessClone::VFORK, 0, 0, 0, 0, frame))
}

/// `execve(path, argv, envp)` (59): M8 stub, replaced by C2 (#381). It
/// returns through the frame, which a successful `execve` rewrites to the
/// new image's entry.
pub(super) fn sys_execve(_path: UserPtr, _argv: UserPtr, _envp: UserPtr, frame: &mut SyscallFrame) -> SyscallReturn {
    frame.rax = not_implemented(general::__NR_execve);
    SyscallReturn::Sysret
}

/// `wait4` (61): reap before copyout, including on EFAULT. C6 adds stop/continue events.
pub(super) fn sys_wait4(pid: i32, wstatus: UserPtr, options: u32, rusage: UserPtr) -> u64 {
    super::finish(wait4(pid, wstatus, options, rusage))
}

fn wait4(pid: i32, wstatus: UserPtr, options: u32, rusage: UserPtr) -> Result<u64, u32> {
    let request = process::wait4_request(pid, options)?;
    let Some(report) = process::wait_child(request)? else {
        return Ok(0);
    };
    if !wstatus.is_null() {
        crate::user::copy_to_user(wstatus.addr(), &report.status().word().to_ne_bytes()).map_err(|_| errno::EFAULT)?;
    }
    write_rusage(rusage)?;
    Ok(u64::from(report.pid()))
}

const SI_SIGNO: u64 = 0;
const SI_ERRNO: u64 = 4;
const SI_CODE: u64 = 8;
const SI_PID: u64 = 16;
const SI_UID: u64 = 20;
const SI_STATUS: u64 = 24;

/// `waitid` (247): WNOWAIT retains the zombie; no event (including errors)
/// clears only the six child siginfo fields. C6 adds stop/continue events.
pub(super) fn sys_waitid(idtype: i32, id: i32, infop: UserPtr, options: u32, rusage: UserPtr) -> u64 {
    super::finish(waitid(idtype, id, infop, options, rusage))
}

fn waitid(idtype: i32, id: i32, infop: UserPtr, options: u32, rusage: UserPtr) -> Result<u64, u32> {
    // Linux's SYSCALL_DEFINE5(waitid) writes these fields even when
    // kernel_waitid returns an error. A bad copyout supersedes that error.
    let result = process::waitid_request(idtype, id, options).and_then(process::wait_child);
    let (signo, code, pid, status) = if let Ok(Some(report)) = &result {
        write_rusage(rusage)?;
        let status = report.status().siginfo();
        (general::SIGCHLD, status.code as u32, report.pid(), status.status as u32)
    } else {
        (0, 0, 0, 0)
    };
    let fields = [(SI_SIGNO, signo), (SI_ERRNO, 0), (SI_CODE, code), (SI_PID, pid), (SI_UID, 0), (SI_STATUS, status)];
    if !infop.is_null() {
        // Linux checks the full siginfo address range but touches only these
        // fields: inaccessible padding must not make an otherwise valid write fail.
        let end = infop.addr().checked_add(core::mem::size_of::<general::siginfo_t>() as u64).ok_or(errno::EFAULT)?;
        if end > crate::user::USER_MAX {
            return Err(errno::EFAULT);
        }
        for (offset, value) in fields {
            crate::user::copy_to_user(infop.addr() + offset, &value.to_ne_bytes()).map_err(|_| errno::EFAULT)?;
        }
    }
    result.map(|_| 0)
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
