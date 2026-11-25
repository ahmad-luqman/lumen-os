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
use crate::linux::general;
use crate::linux::process::{current_process, current_thread, record_exit};
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

/// `clone(flags, newsp, parent_tid, child_tid, tls)` (56): M8 stub, replaced by C1 (#380) and C3 (#382).
/// Linux reads only `flags`' low 32 bits (`lower_32_bits(clone_flags)`).
/// The frame is the parent's registers at the `syscall`, from which the
/// child's starting ones are made.
pub(super) fn sys_clone(
    _flags: u32,
    _newsp: UserPtr,
    _parent_tid: UserPtr,
    _child_tid: UserPtr,
    _tls: u64,
    _frame: &SyscallFrame,
) -> u64 {
    not_implemented(general::__NR_clone)
}

/// `fork()` (57): M8 stub, replaced by C1 (#380). The frame is the
/// parent's registers at the `syscall`, from which the child's starting
/// ones are made.
pub(super) fn sys_fork(_frame: &SyscallFrame) -> u64 {
    not_implemented(general::__NR_fork)
}

/// `vfork()` (58): M8 stub, replaced by C1 (#380). The frame as `fork`'s.
pub(super) fn sys_vfork(_frame: &SyscallFrame) -> u64 {
    not_implemented(general::__NR_vfork)
}

/// `execve(path, argv, envp)` (59): M8 stub, replaced by C2 (#381). It
/// returns through the frame, which a successful `execve` rewrites to the
/// new image's entry.
pub(super) fn sys_execve(_path: UserPtr, _argv: UserPtr, _envp: UserPtr, frame: &mut SyscallFrame) -> SyscallReturn {
    frame.rax = not_implemented(general::__NR_execve);
    SyscallReturn::Sysret
}

/// `wait4(pid, wstatus, options, rusage)` (61): M8 stub, replaced by C1 (#380) and C6 (#385).
pub(super) fn sys_wait4(_pid: i32, _wstatus: UserPtr, _options: u32, _rusage: UserPtr) -> u64 {
    not_implemented(general::__NR_wait4)
}

/// `waitid(idtype, id, infop, options, rusage)` (247): M8 stub, replaced by C1 (#380) and C6 (#385).
pub(super) fn sys_waitid(_idtype: i32, _id: i32, _infop: UserPtr, _options: u32, _rusage: UserPtr) -> u64 {
    not_implemented(general::__NR_waitid)
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
