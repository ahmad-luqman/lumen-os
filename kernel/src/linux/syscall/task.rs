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

/// `vfork()` (58): share the mm and wait killably for child exit or exec.
pub(super) fn sys_vfork(frame: &SyscallFrame) -> u64 {
    super::finish(process::fork_current(ProcessClone::VFORK, 0, 0, 0, 0, frame))
}

/// Replace the single-threaded process image, returning through a fresh iret frame.
pub(super) fn sys_execve(path: UserPtr, argv: UserPtr, envp: UserPtr, frame: &mut SyscallFrame) -> SyscallReturn {
    match execve(path, argv, envp) {
        Ok(program) => {
            *frame = SyscallFrame {
                rip: program.entry,
                rsp: program.rsp,
                cs: u64::from(arch::gdt::USER_CODE),
                ss: u64::from(arch::gdt::USER_DATA),
                rflags: arch::syscall::RFLAGS_USER_FIXED,
                ..SyscallFrame::default()
            };
            SyscallReturn::Iret
        }
        Err(error) => {
            frame.rax = super::finish(Err(error));
            SyscallReturn::Sysret
        }
    }
}

fn execve(path: UserPtr, argv: UserPtr, envp: UserPtr) -> Result<crate::mm::user_vm::LoadedProgram, u32> {
    let path = super::at::UserPath::copy_in(path, false)?;
    let file = open_exec(path.as_bytes()).map_err(|e| e.to_errno())?;
    current_process().check_exec()?;
    let stack = lumen_vm::StackSize::from_rlimit(current_process().stack_limit(None).soft());
    let (argv, envp, mut budget) = copy_exec_args(path.as_bytes(), argv.addr(), envp.addr(), stack.arg_limit())?;
    let (file, argv) = follow_scripts(file, path.as_bytes(), argv, &mut budget)?;
    let (prepared, program) = prepare_exec(file, argv, envp, path.as_bytes(), stack)?;
    // C3 adds de_thread here; until then only one live thread may commit.
    current_process().check_exec()?;
    commit_exec(prepared);
    Ok(program)
}

fn open_exec(path: &[u8]) -> Result<crate::linux::exec::ExecFile, lumen_vfs::VfsError> {
    let cwd = process::current_cwd();
    crate::linux::exec::open_path(Some(&cwd.0), path)
}

fn copy_exec_args(
    execfn: &[u8],
    argv: u64,
    envp: u64,
    limit: usize,
) -> Result<(ExecStrings, ExecStrings, lumen_elf::args::ArgBudget), u32> {
    let argc = count_user_ptrs(argv)?;
    let envc = count_user_ptrs(envp)?;
    let mut budget = lumen_elf::args::ArgBudget::new(argc, envc, limit).map_err(|_| errno::E2BIG)?;
    charge_exec(&mut budget, execfn.len() + 1)?;
    let envp = exec_strings(envp, envc, &mut budget)?;
    let mut argv = exec_strings(argv, argc, &mut budget)?;
    if argc == 0 {
        charge_exec(&mut budget, 1)?;
        argv = ExecStrings::from_slices(&[b""])?;
    }
    Ok((argv, envp, budget))
}

fn follow_scripts(
    mut file: crate::linux::exec::ExecFile,
    spelling: &[u8],
    mut argv: ExecStrings,
    budget: &mut lumen_elf::args::ArgBudget,
) -> Result<(crate::linux::exec::ExecFile, ExecStrings), u32> {
    use lumen_elf::shebang;
    let mut spelling = exec_copy(spelling)?;
    let mut nesting = shebang::Nesting::new();
    loop {
        exec_interrupted()?;
        let mut head = [0; shebang::BINPRM_BUF_SIZE];
        let len = file.source.len().map_err(|e| e.to_errno())?.min(head.len() as u64) as usize;
        file.source.read_exact(0, &mut head[..len]).map_err(|e| e.to_errno())?;
        let Some(script) = shebang::parse(&head[..len]).map_err(|e| exec_errno(e.errno()))? else { break };
        let original = argv.refs()?;
        let rewritten = budget.rewrite(&script, &spelling, &original).map_err(|e| exec_errno(e.errno()))?;
        let copied = ExecStrings::from_slices(&rewritten)?;
        if let Some(error) = script.open_error() {
            return Err(exec_errno(error.errno()));
        }
        let next = open_exec(script.interpreter()).map_err(|e| e.to_errno())?;
        nesting.follow_interpreter().map_err(|e| exec_errno(e.errno()))?;
        spelling = exec_copy(script.interpreter())?;
        file = next;
        argv = copied;
    }
    Ok((file, argv))
}

struct PreparedExec {
    space: alloc::sync::Arc<crate::mm::user_vm::AddressSpace>,
    files: process::PreparedFiles,
    exe: Option<alloc::sync::Arc<[u8]>>,
    name: crate::sched::TaskName,
}

fn prepare_exec(
    file: crate::linux::exec::ExecFile,
    argv: ExecStrings,
    envp: ExecStrings,
    execfn: &[u8],
    stack: lumen_vm::StackSize,
) -> Result<(PreparedExec, crate::mm::user_vm::LoadedProgram), u32> {
    let exe = file.image.as_ref().map(|image| alloc::sync::Arc::clone(&image.path));
    let exec = crate::linux::exec::Exec::from_source(file.source, file.image, open_exec).map_err(|e| e.to_errno())?;
    let (space, program) =
        exec.load_with_stack(&argv.refs()?, &envp.refs()?, execfn, stack).map_err(|e| e.to_errno())?;
    let files = process::prepare_exec_files()?;
    let name = crate::sched::TaskName::from_comm(execfn.rsplit(|&b| b == b'/').next().unwrap_or_default());
    Ok((PreparedExec { space: alloc::sync::Arc::new(space), files, exe, name }, program))
}

/// Commit a fully prepared replacement without a recoverable error.
fn commit_exec(prepared: PreparedExec) {
    // Point of no return: release the old image's userspace synchronization.
    let thread = current_thread();
    let old = crate::sched::current_address_space().expect("exec in user space");
    crate::linux::futex::release_user_sync(&thread, old.users() > 1);
    process::commit_unshare(prepared.files);
    process::finish_exec();
    current_process().complete_vfork();
    process::replace_current_mm(prepared.space);
    process::mark_exec();
    process::set_exe_path(prepared.exe);
    crate::sched::restore_signal_fpu(None);
    crate::sched::set_current_fs_base(0);
    crate::sched::set_restart_block(None);
    crate::sched::set_current_name(prepared.name);
}

fn exec_errno(error: lumen_elf::Errno) -> u32 {
    match error {
        lumen_elf::Errno::NoExec => errno::ENOEXEC,
        lumen_elf::Errno::Access => errno::EACCES,
        lumen_elf::Errno::Loop => errno::ELOOP,
        lumen_elf::Errno::TooBig => errno::E2BIG,
        lumen_elf::Errno::NoMem => errno::ENOMEM,
    }
}

/// NUL-terminated strings in reverse argument order, with cumulative ends.
/// Storage is bounded by the charged bytes plus four bytes per argument.
struct ExecStrings {
    bytes: alloc::vec::Vec<u8>,
    ends: alloc::vec::Vec<u32>,
}
impl ExecStrings {
    fn from_slices(strings: &[&[u8]]) -> Result<Self, u32> {
        let size = strings.iter().try_fold(0usize, |n, s| n.checked_add(s.len() + 1)).ok_or(errno::E2BIG)?;
        let mut result = Self { bytes: alloc::vec::Vec::new(), ends: alloc::vec::Vec::new() };
        result.bytes.try_reserve_exact(size).map_err(|_| errno::ENOMEM)?;
        result.ends.try_reserve_exact(strings.len()).map_err(|_| errno::ENOMEM)?;
        for s in strings.iter().rev() {
            result.bytes.extend_from_slice(s);
            result.bytes.push(0);
            result.ends.push(result.bytes.len() as u32);
        }
        Ok(result)
    }
    fn refs(&self) -> Result<alloc::vec::Vec<&[u8]>, u32> {
        let mut refs = alloc::vec::Vec::new();
        refs.try_reserve_exact(self.ends.len()).map_err(|_| errno::ENOMEM)?;
        for i in (0..self.ends.len()).rev() {
            let start = if i == 0 { 0 } else { self.ends[i - 1] as usize };
            refs.push(&self.bytes[start..self.ends[i] as usize - 1]);
        }
        Ok(refs)
    }
}

fn exec_interrupted() -> Result<(), u32> {
    if crate::sched::with_current(|t| t.fatal_pending()).unwrap_or(false) { Err(errno::EINTR) } else { Ok(()) }
}

fn user_arg(base: u64, index: usize) -> Result<u64, u32> {
    let address = base.checked_add((index as u64).checked_mul(8).ok_or(errno::EFAULT)?).ok_or(errno::EFAULT)?;
    let mut word = [0; 8];
    crate::user::copy_from_user(&mut word, address).map_err(|_| errno::EFAULT)?;
    Ok(u64::from_ne_bytes(word))
}

fn exec_copy(bytes: &[u8]) -> Result<alloc::vec::Vec<u8>, u32> {
    let mut copy = alloc::vec::Vec::new();
    copy.try_reserve_exact(bytes.len()).map_err(|_| errno::ENOMEM)?;
    copy.extend_from_slice(bytes);
    Ok(copy)
}

fn charge_exec(budget: &mut lumen_elf::args::ArgBudget, len: usize) -> Result<(), u32> {
    budget.charge(core::num::NonZeroUsize::new(len).expect("C string includes NUL")).map_err(|_| errno::E2BIG)
}

fn count_user_ptrs(base: u64) -> Result<usize, u32> {
    if base == 0 {
        return Ok(0);
    }
    let mut n = 0;
    loop {
        let pointer = user_arg(base, n)?;
        if pointer == 0 {
            return Ok(n);
        }
        if n == lumen_elf::args::MAX_ARG_STRINGS {
            return Err(errno::E2BIG);
        }
        exec_interrupted()?;
        n += 1;
    }
}

/// Length including NUL, without reading beyond the terminating user page.
fn exec_strlen(mut address: u64) -> Result<usize, u32> {
    let mut len = 0;
    let mut buf = [0; 256];
    while len < lumen_elf::args::MAX_ARG_STRLEN {
        let count = buf.len().min(4096 - (address as usize & 4095)).min(lumen_elf::args::MAX_ARG_STRLEN - len);
        crate::user::copy_from_user(&mut buf[..count], address).map_err(|_| errno::EFAULT)?;
        if let Some(end) = buf[..count].iter().position(|&b| b == 0) {
            return Ok(len + end + 1);
        }
        len += count;
        address = address.checked_add(count as u64).ok_or(errno::EFAULT)?;
    }
    Err(errno::E2BIG)
}

fn exec_strings(base: u64, n: usize, budget: &mut lumen_elf::args::ArgBudget) -> Result<ExecStrings, u32> {
    let mut strings = ExecStrings { bytes: alloc::vec::Vec::new(), ends: alloc::vec::Vec::new() };
    strings.ends.try_reserve_exact(n).map_err(|_| errno::ENOMEM)?;
    let mut total = 0usize;
    for i in (0..n).rev() {
        exec_interrupted()?;
        let pointer = user_arg(base, i)?;
        if pointer == 0 {
            return Err(errno::EFAULT);
        }
        let len = exec_strlen(pointer)?;
        charge_exec(budget, len)?;
        total += len;
        strings.ends.push(total as u32);
    }
    strings.bytes.try_reserve_exact(total).map_err(|_| errno::ENOMEM)?;
    strings.bytes.resize(total, 0);
    let mut start = 0;
    for (i, &end) in (0..n).rev().zip(&strings.ends) {
        exec_interrupted()?;
        let pointer = user_arg(base, i)?;
        if pointer == 0 {
            return Err(errno::EFAULT);
        }
        let out = &mut strings.bytes[start..end as usize];
        crate::user::copy_from_user(out, pointer).map_err(|_| errno::EFAULT)?;
        if out.last() != Some(&0) {
            return Err(errno::EFAULT);
        }
        start = end as usize;
    }
    Ok(strings)
}

/// `wait4` (61): reap before copyout, including on EFAULT. C6 adds stop/continue events.
pub(super) fn sys_wait4(pid: i32, wstatus: UserPtr, options: u32, rusage: UserPtr) -> u64 {
    super::finish(wait4(pid, wstatus, options, rusage))
}

fn wait4(pid: i32, wstatus: UserPtr, options: u32, rusage: UserPtr) -> Result<u64, u32> {
    let request = process::wait4_request(pid, options)?;
    let Some((report, ticks)) = process::wait_child_with_usage(request)? else {
        return Ok(0);
    };
    if !wstatus.is_null() {
        crate::user::copy_to_user(wstatus.addr(), &report.status().word().to_ne_bytes()).map_err(|_| errno::EFAULT)?;
    }
    write_rusage(rusage, ticks)?;
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
    let result = process::waitid_request(idtype, id, options).and_then(process::wait_child_with_usage);
    let (signo, code, pid, status) = if let Ok(Some((report, ticks))) = &result {
        write_rusage(rusage, *ticks)?;
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

fn write_rusage(ptr: UserPtr, ticks: [u64; 2]) -> Result<(), u32> {
    if !ptr.is_null() {
        super::sys::write_usage(ptr, ticks)?;
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

crate::ktest!(
    fn execve_replaces_frame_and_preserves_failed_call() {
        use crate::linux::process::{KTEST_USER_VA as A, in_test_process};
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        let _image =
            crate::linux::exec::ExecFixture::new("/tmp/c2-frame", &crate::mm::user_vm::build_relro_test_elf())?;
        let _script = crate::linux::exec::ExecFixture::new("/tmp/c2-script-frame", b"#!/tmp/c2-frame\n")?;
        for scripted in [false, true] {
            in_test_process(move |pid| {
                let path = if scripted { "/tmp/c2-script-frame" } else { "/tmp/c2-frame" };
                let mut cpath = alloc::string::String::from(path);
                cpath.push('\0');
                copy_to_user(A, cpath.as_bytes()).map_err(|_| "path")?;
                let old = crate::sched::current_address_space().ok_or("old mm")?;
                crate::sched::set_current_fs_base(A);
                let call = |argv: u64| {
                    let mut frame = SyscallFrame {
                        rax: u64::from(general::__NR_execve),
                        rdi: A,
                        rsi: argv,
                        rip: 0x123456,
                        rsp: A + 4096,
                        rdx: 0,
                        r15: 123,
                        ..SyscallFrame::default()
                    };
                    let mode = super::dispatch(&mut frame);
                    (frame, mode)
                };
                let (bad, mode) = call(1);
                kassert_eq!(mode, SyscallReturn::Sysret);
                kassert_eq!(bad.rax as i64, -i64::from(errno::EFAULT));
                kassert_eq!((bad.rip, bad.rsp, bad.r15), (0x123456, A + 4096, 123));
                kassert_eq!(crate::sched::current_fs_base(), A);
                kassert_eq!(old.users(), 1);
                kassert_eq!(process::process_exe_path(pid), None);
                // A private Files table is the first allocation at this size.
                // Warm and successfully load under the same refusal threshold first.
                let exec = crate::linux::exec::Exec::from_path("/tmp/c2-frame").map_err(|_| "plan")?;
                crate::mm::heap::refuse_from(16 * 1024);
                let warm = exec.load(&[b""], &[], path.as_bytes());
                crate::mm::heap::refuse_from(usize::MAX);
                warm.map_err(|_| "load below Files threshold")?.0.teardown();
                let free = crate::mm::frame::free_frames();
                let (failed, mode) = process::with_test_shared_files(|| {
                    crate::mm::heap::refuse_from(16 * 1024);
                    let result = call(0);
                    crate::mm::heap::refuse_from(usize::MAX);
                    result
                });
                kassert_eq!(failed.rax as i64, -i64::from(errno::ENOMEM));
                kassert_eq!(mode, SyscallReturn::Sysret);
                kassert_eq!((failed.rip, failed.rsp, failed.r15), (0x123456, A + 4096, 123));
                kassert_eq!(old.users(), 1);
                kassert!(alloc::sync::Arc::ptr_eq(&old, &crate::sched::current_address_space().unwrap()));
                kassert_eq!(crate::sched::current_fs_base(), A);
                kassert_eq!(process::process_exe_path(pid), None);
                kassert_eq!(crate::mm::frame::free_frames(), free);
                let (frame, mode) = call(0);
                kassert_eq!((scripted, frame.rax as i64), (scripted, 0));
                kassert_eq!(mode, SyscallReturn::Iret);
                kassert_eq!(frame.rip, 0x401000);
                kassert_eq!(frame.rsp % 16, 0);
                kassert_eq!((frame.cs, frame.ss, frame.rflags), (0x23, 0x1b, 0x202));
                kassert_eq!(
                    [
                        frame.rax, frame.rbx, frame.rcx, frame.rdx, frame.rdi, frame.rsi, frame.rbp, frame.r8,
                        frame.r9, frame.r10, frame.r11, frame.r12, frame.r13, frame.r14, frame.r15
                    ],
                    [0; 15]
                );
                kassert_eq!(crate::sched::current_fs_base(), 0);
                kassert_eq!(old.users(), 0);
                kassert_eq!(process::process_exe_path(pid).as_deref(), Some(b"/tmp/c2-frame".as_slice()));
                kassert_eq!(crate::sched::with_current(|t| t.name()), Some(lumen_trace::ThreadName::from_path(path)));
                let mut word = [0; 8];
                copy_from_user(&mut word, frame.rsp).map_err(|_| "argc")?;
                kassert_eq!(u64::from_ne_bytes(word), if scripted { 2 } else { 1 });
                copy_from_user(&mut word, frame.rsp + 8).map_err(|_| "argv[0]")?;
                if scripted {
                    let mut interpreter = [0; 14];
                    copy_from_user(&mut interpreter, u64::from_ne_bytes(word)).map_err(|_| "interpreter argument")?;
                    kassert_eq!(&interpreter, b"/tmp/c2-frame\0");
                } else {
                    let mut empty = [1];
                    copy_from_user(&mut empty, u64::from_ne_bytes(word)).map_err(|_| "empty argument")?;
                    kassert_eq!(empty, [0]); // synthetic argv[0]
                }
                kassert!(!crate::sched::current_address_space().unwrap().contains_vma(A));
                Ok(())
            })??;
        }
        Ok(())
    }
);

crate::ktest!(
    fn exec_pointer_count_is_bounded_before_allocation() {
        use crate::mm::heap;
        use crate::{kassert_eq, linux::process::in_test_process};
        in_test_process(|_| {
            const BASE: u64 = 0x2000_0000;
            const LIMIT: usize = 2 * 1024 * 1024;
            const N: usize = LIMIT / 8 + 1;
            const LEN: u64 = LIMIT as u64 + 4096;
            let mm = crate::sched::current_address_space().ok_or("mm")?;
            mm.map_eager_with_bytes(BASE, LEN, lumen_vm::Prot::READ | lumen_vm::Prot::WRITE, &[])
                .map_err(|_| "map pointers")?;
            let mut page = [0u8; 4096];
            for word in page.as_chunks_mut::<8>().0 {
                word.copy_from_slice(&BASE.to_ne_bytes());
            }
            for off in (0..LEN).step_by(4096) {
                crate::user::copy_to_user(BASE + off, &page).map_err(|_| "pointers")?;
            }
            crate::user::copy_to_user(BASE + N as u64 * 8, &0u64.to_ne_bytes()).map_err(|_| "terminator")?;
            // Make any pointer-vector allocation grow the heap, regardless of earlier tests.
            let held = heap::fill()?;
            let before = heap::stats().size;
            let error = copy_exec_args(b"/x", BASE, 0, LIMIT).err();
            let after = heap::stats().size;
            heap::unfill(held);
            kassert_eq!(error, Some(errno::E2BIG));
            kassert_eq!(after, before);
            crate::user::copy_to_user(BASE + N as u64 * 8, &BASE.to_ne_bytes()).map_err(|_| "remove terminator")?;
            kassert_eq!(copy_exec_args(b"/x", BASE, 0, LIMIT).err(), Some(errno::EFAULT));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn exec_argument_walk_observes_fatal_signal() {
        use crate::{kassert_eq, linux::process::in_test_process};
        in_test_process(|_| {
            let base = process::KTEST_USER_VA;
            crate::user::copy_to_user(base, &base.to_ne_bytes()).map_err(|_| "argument pointer")?;
            crate::sched::with_current(|thread| thread.set_fatal_pending()).ok_or("current thread")?;
            kassert_eq!(copy_exec_args(b"/x", base, 0, 131072).err(), Some(errno::EINTR));
            Ok(())
        })?
    }
);
