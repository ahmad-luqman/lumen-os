//! `m8_proc`: M8's ring-3 checks of processes, threads, signals, faults,
//! `futex`, timers, sessions and the small calls (`tests/scenarios/test_linux_proc.py`).
//!
//! A `#![no_std]` dynamically linked musl x86-64 executable, built as
//! `tools/linux-m7/`'s are (the Makefile) into `build/linux/`, which both
//! images stage in `/usr/local/bin`. Its checks and their conventions:
//! `common.rs` and ARCHITECTURE section 15, "M8's tests". Unknown modes
//! print usage and exit 64.

#![no_std]
#![no_main]

#[macro_use]
mod common;
mod exec;
mod futex;
mod sessions;
mod signal_waits;
mod signals;
mod small;
mod threads;
mod timers;

use core::ffi::c_int;

const USAGE: &str = "usage: m8_proc fork-wait|signal-spin|signals|faults|signal-bad <kind>|signal-fatal blocked|ignored|signal-hold|signal-probe <pid>|signal-wait restart|eintr|signal-kill <pid>";

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const u8) -> c_int {
    // SAFETY: musl's `__libc_start_main` passes the kernel's `argc` and
    // `argv`, kept for the program's life.
    let (args, n) = unsafe { common::args(argc, argv) };
    match &args[..n] {
        [_, b"fork-wait"] => fork_wait(),
        [_, b"posix-shm"] => posix_shm::run(b"none"),
        [_, b"posix-shm-fail", stage] => posix_shm::run(stage),
        [_, b"sessions"] => sessions::run(),
        [_, b"sessions-exec-hold"] => sessions::exec_hold(),
        [_, b"threads"] => threads::run(),
        [_, b"timers"] => timers::run(),
        [_, b"timer-image"] => timers::image(),
        [_, b"thread-exhaust"] => threads::exhaust(),
        [_, b"thread-root-exec"] => threads::root(true),
        [_, b"thread-root-exit"] => threads::root(false),
        [_, b"thread-image", tids @ ..] => threads::image(tids),
        [_, b"signal-waits"] => signal_waits::run(),
        [_, b"c6-kill", pid, sig] => signal_waits::send(pid, sig),
        [_, b"c6-wait", kind, mode] => signal_waits::blocking(kind, *mode == b"restart"),
        [_, b"futex"] => futex::run(),
        [_, b"futex-stress"] => futex::stress(),
        [_, b"futex-block"] => futex::block_witness(),
        [_, b"smp-workload"] => threads::smp_workload(),
        [_, b"small-syscalls"] => small::run(),
        [_, b"small-comm"] => small::comm_child(),
        [_, b"small-stack-reduced"] => small::stack_child(512 * 1024),
        [_, b"small-budget-must-fail"] => {
            say!("FAIL reduced argument budget: exec succeeded");
            1
        }
        [_, b"small-stack"] => small::stack_child(64 * 1024 * 1024),
        [_, b"fork-exec"] => {
            let result = exec::run();
            if result == 0 { threads::exec_checks() } else { result }
        }
        [_, b"exec-exit", ..] => exec::child_exit(),
        [_, b"exec-hold"] => exec::child_hold(),
        [_, b"exec-relative"] => exec::child_relative(),
        [_, b"exec-reexec"] => exec::child_reexec(),
        [_, b"exec-byte", ..] => exec::child_byte(),
        [_, b"exec-unnamed"] => exec::child_unnamed(),
        [_, b"exec-random"] => exec::child_random(),
        [_, b"exec-check", ..] => exec::child_check(&args[..n]),
        [_, b"signal-spin"] => signals::spin(),
        [_, b"signals"] => signals::signals(),
        [_, b"faults"] => signals::faults(),
        [_, b"signal-bad", kind] => signals::bad(kind),
        [_, b"signal-fatal", kind] => signals::fatal(kind),
        [_, b"signal-hold"] => signals::hold(),
        [_, b"signal-probe", pid] => signals::probe(pid),
        [_, b"signal-wait", b"restart"] => signals::wait(true),
        [_, b"signal-wait", b"eintr"] => signals::wait(false),
        [_, b"signal-kill", pid] => signals::kill(pid),
        _ => {
            say!("{USAGE}");
            common::USAGE_EXIT
        }
    }
}

// C1 (#380): use raw syscalls for ABI checks, including child exit. Each
// child either exits immediately or waits on an explicitly shared word.
use common::{R, eq, ok, sys};
use core::sync::atomic::{AtomicU32, Ordering::SeqCst};

// C4 futex and sleep ABI constants shared by the ring-3 checks.
const SYS_FUTEX: u64 = 202;
const SYS_NANOSLEEP: u64 = 35;
const SYS_CLOCK_NANOSLEEP: u64 = 230;
const SYS_CLOCK_GETTIME: u64 = 228;
const SYS_SET_TID_ADDRESS: u64 = 218;
const SYS_SET_ROBUST_LIST: u64 = 273;
const SYS_GET_ROBUST_LIST: u64 = 274;
const SYS_FUTEX_WAITV: u64 = 449;
const SYS_MPROTECT: u64 = 10;
const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_REQUEUE: u64 = 3;
const FUTEX_CMP_REQUEUE: u64 = 4;
const FUTEX_WAIT_BITSET: u64 = 9;
const FUTEX_WAKE_BITSET: u64 = 10;
const FUTEX_PRIVATE_FLAG: u64 = 128;
const FUTEX_CLOCK_REALTIME: u64 = 256;
const FUTEX_OWNER_DIED: i64 = 0x4000_0000;
const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_PROCESS_CPUTIME_ID: u64 = 2;
const CLOCK_THREAD_CPUTIME_ID: u64 = 3;
const CLOCK_MONOTONIC_RAW: u64 = 4;
const CLOCK_REALTIME_COARSE: u64 = 5;
const CLOCK_MONOTONIC_COARSE: u64 = 6;
const CLOCK_BOOTTIME: u64 = 7;
const CLOCK_REALTIME_ALARM: u64 = 8;
const CLOCK_BOOTTIME_ALARM: u64 = 9;
const CLOCK_TAI: u64 = 11;
const TIMER_ABSTIME: u64 = 1;
const EOPNOTSUPP: i64 = 95;
const EOWNERDEAD: i64 = 130;
const MAP_SHARED: u64 = 1;
const MAP_ANONYMOUS: u64 = 0x20;
const EAGAIN: i64 = 11;
const ETIMEDOUT: i64 = 110;
const ESRCH: i64 = 3;

const SYS_READ: u64 = 0;
const SYS_KILL: u64 = 62;
const SYS_FTRUNCATE: u64 = 77;
const SYS_RT_SIGQUEUEINFO: u64 = 129;
const SYS_SIGALTSTACK: u64 = 131;
const SYS_TKILL: u64 = 200;
const SYS_TGKILL: u64 = 234;
const SYS_OPENAT: u64 = 257;
const SYS_RT_TGSIGQUEUEINFO: u64 = 297;
const SYS_OPEN: u64 = 2;
const SYS_CLOSE: u64 = 3;
const SYS_POLL: u64 = 7;
const SYS_MMAP: u64 = 9;
const SYS_MUNMAP: u64 = 11;
const SYS_RT_SIGACTION: u64 = 13;
const SYS_RT_SIGPROCMASK: u64 = 14;
const SYS_GETPID: u64 = 39;
const SYS_CLONE: u64 = 56;
const SYS_FORK: u64 = 57;
const SYS_VFORK: u64 = 58;
const SYS_EXIT: u64 = 60;
const SYS_WAIT4: u64 = 61;
const SYS_FCNTL: u64 = 72;
const SYS_GETCWD: u64 = 79;
const SYS_CHDIR: u64 = 80;
const SYS_UMASK: u64 = 95;
const SYS_GETPPID: u64 = 110;
const SYS_ARCH_PRCTL: u64 = 158;
const SYS_GETTID: u64 = 186;
const SYS_EXIT_GROUP: u64 = 231;
const SYS_WAITID: u64 = 247;
const SIGCHLD: u64 = 17;
const SIGUSR1: u64 = 10;
const SIGUSR2: u64 = 12;
const SIG_BLOCK: u64 = 0;
const SS_DISABLE: u64 = 2;
const AT_RANDOM: usize = 25;
const SYS_RENAME: u64 = 82;
const SYS_LSEEK: u64 = 8;
const SIG_SETMASK: u64 = 2;
const CLD_EXITED: i64 = 1;
const SI_SIGNO: usize = 0;
const SI_ERRNO: usize = 1;
const SI_CODE: usize = 2;
const SI_PID: usize = 4;
const SI_UID: usize = 5;
const SI_STATUS: usize = 6;
const CLONE_VM: u64 = 0x100;
const CLONE_FS: u64 = 0x200;
const CLONE_FILES: u64 = 0x400;
const CLONE_SIGHAND: u64 = 0x800;
const CLONE_PIDFD: u64 = 0x1000;
const CLONE_VFORK: u64 = 0x4000;
const CLONE_PARENT: u64 = 0x8000;
const CLONE_THREAD: u64 = 0x10000;
const CLONE_SETTLS: u64 = 0x80000;
const CLONE_PARENT_SETTID: u64 = 0x100000;
const CLONE_CHILD_CLEARTID: u64 = 0x200000;
const CLONE_CHILD_SETTID: u64 = 0x1000000;
const CLONE_PTHREAD: u64 = 0x7d0f00;
const SPAWN_FLAGS: u64 = CLONE_VM | CLONE_VFORK | SIGCHLD;
const WNOHANG: u64 = 1;
const WEXITED: u64 = 4;
const WNOWAIT: u64 = 0x01000000;
const P_ALL: u64 = 0;
const P_PID: u64 = 1;
const EPERM: i64 = 1;
const ENOENT: i64 = 2;
const EBADF: i64 = 9;
const ECHILD: i64 = 10;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const ENOSYS: i64 = 38;
const F_GETFD: u64 = 1;
const ARCH_GET_FS: u64 = 0x1003;

fn exit(code: u64) -> ! {
    sys(SYS_EXIT, [code, 0, 0, 0, 0, 0]);
    panic!("exit returned")
}

fn fork() -> Result<i64, common::Fail> {
    ok("fork", sys(SYS_FORK, [0; 6]))
}

fn wait(pid: i64, code: i64) -> R {
    let mut status = -1i32;
    eq("wait pid", sys(SYS_WAIT4, [pid as u64, &mut status as *mut _ as u64, 0, 0, 0, 0]), pid)?;
    eq("exit status word", i64::from(status), (code & 255) << 8)
}

/// Fork a child that exits with code, leaving its pid for the caller to wait.
fn child_exits(code: u64) -> Result<i64, common::Fail> {
    let pid = fork()?;
    if pid == 0 {
        exit(code);
    }
    Ok(pid)
}

fn gone(pid: i64) -> R {
    eq("child is reaped", sys(SYS_WAIT4, [pid as u64, 0, WNOHANG, 0, 0, 0]), -ECHILD)
}

fn statuses() -> R {
    eq("no children", sys(SYS_WAIT4, [u64::MAX, 0, 0, 0, 0, 0]), -ECHILD)?;
    for code in [0, 1, 42, 127, 255, 256, 511] {
        let pid = child_exits(code as u64)?;
        wait(pid, code)?;
        gone(pid)?;
    }
    let pid = fork()?;
    if pid == 0 {
        sys(SYS_EXIT_GROUP, [29, 0, 0, 0, 0, 0]);
        exit(99);
    }
    wait(pid, 29)
}

struct Shared(u64);
impl Shared {
    fn new() -> Result<Self, common::Fail> {
        let ptr = ok("shared mmap", sys(SYS_MMAP, [0, 4096, 3, 0x21, u64::MAX, 0]))? as u64;
        let shared = Self(ptr);
        for i in 0..8 {
            shared.word(i).store(0, SeqCst);
        }
        Ok(shared)
    }
    fn word(&self, i: usize) -> &AtomicU32 {
        assert!(i < 8);
        // SAFETY: mmap returned an aligned, writable page; each atomic is
        // in its initialized first 32 bytes, and the mapping lives with self.
        unsafe { &*((self.0 as *const AtomicU32).add(i)) }
    }
    fn until(&self, i: usize) -> u32 {
        loop {
            let value = self.word(i).load(SeqCst);
            if value != 0 {
                return value;
            }
            sys(SYS_POLL, [0, 0, 1, 0, 0, 0]);
        }
    }
}
impl Drop for Shared {
    fn drop(&mut self) {
        sys(SYS_MUNMAP, [self.0, 4096, 0, 0, 0, 0]);
    }
}

fn nohang() -> R {
    let shared = Shared::new()?;
    let pid = fork()?;
    if pid == 0 {
        shared.until(0);
        exit(73);
    }
    let mut status = 0x1234i32;
    let ret = sys(SYS_WAIT4, [pid as u64, &mut status as *mut _ as u64, WNOHANG, 0, 0, 0]);
    let mut info = [0x55i32; 32];
    let idret = sys(SYS_WAITID, [P_PID, pid as u64, info.as_mut_ptr() as u64, WNOHANG | WEXITED, 0, 0]);
    shared.word(0).store(1, SeqCst);
    wait(pid, 73)?;
    eq("WNOHANG live child", ret, 0)?;
    eq("WNOHANG leaves status", i64::from(status), 0x1234)?;
    eq("waitid WNOHANG", idret, 0)?;
    check_empty_info(&info)?;
    info.fill(0x55);
    eq("waitid ECHILD", sys(SYS_WAITID, [P_PID, pid as u64, info.as_mut_ptr() as u64, WEXITED, 0, 0]), -ECHILD)?;
    check_empty_info(&info)
}

fn check_empty_info(info: &[i32; 32]) -> R {
    for (i, word) in info.iter().enumerate() {
        eq(
            "waitid fields and untouched padding",
            i64::from(*word),
            if [SI_SIGNO, SI_ERRNO, SI_CODE, SI_PID, SI_UID, SI_STATUS].contains(&i) { 0 } else { 0x55 },
        )?;
    }
    Ok(())
}

fn zombie() -> R {
    let pid = child_exits(87)?;
    for _ in 0..2 {
        let mut info = [0i32; 32];
        eq(
            "waitid WNOWAIT",
            sys(SYS_WAITID, [P_PID, pid as u64, info.as_mut_ptr() as u64, WNOWAIT | WEXITED, 0, 0]),
            0,
        )?;
        eq("si_signo", i64::from(info[SI_SIGNO]), SIGCHLD as i64)?;
        eq("si_code", i64::from(info[SI_CODE]), CLD_EXITED)?;
        eq("si_pid", i64::from(info[SI_PID]), pid)?;
        eq("si_uid", i64::from(info[SI_UID]), 0)?;
        eq("si_status", i64::from(info[SI_STATUS]), 87)?;
    }
    wait(pid, 87)?;
    let pid = child_exits(44)?;
    let mut info = [0i32; 32];
    eq("waitid reap", sys(SYS_WAITID, [P_PID, pid as u64, info.as_mut_ptr() as u64, WEXITED, 0, 0]), 0)?;
    eq("waitid reaped status", i64::from(info[SI_STATUS]), 44)?;
    gone(pid)
}

fn copyout_fault() -> R {
    let pid = child_exits(12)?;
    eq("wait4 bad pointer", sys(SYS_WAIT4, [pid as u64, 1, 0, 0, 0, 0]), -EFAULT)?;
    gone(pid)?;
    let pid = child_exits(13)?;
    eq("waitid bad pointer", sys(SYS_WAITID, [P_PID, pid as u64, 1, WEXITED, 0, 0]), -EFAULT)?;
    gone(pid)
}

fn autoreap() -> R {
    for (handler, flags) in [(1u64, 0u64), (0, 2)] {
        let action = [handler, flags, 0, 0];
        let mut old = [0u64; 4];
        eq(
            "SIGCHLD disposition",
            sys(SYS_RT_SIGACTION, [SIGCHLD, action.as_ptr() as u64, old.as_mut_ptr() as u64, 8, 0, 0]),
            0,
        )?;
        let pid = child_exits(33)?;
        let ret = sys(SYS_WAIT4, [pid as u64, 0, 0, 0, 0, 0]);
        eq("restore SIGCHLD", sys(SYS_RT_SIGACTION, [SIGCHLD, old.as_ptr() as u64, 0, 8, 0, 0]), 0)?;
        eq("automatic reap", ret, -ECHILD)?;
    }
    Ok(())
}

fn proc_path(pid: u32) -> [u8; 32] {
    let mut path = [0u8; 32];
    path[..6].copy_from_slice(b"/proc/");
    let mut digits = [0u8; 10];
    let (mut n, mut count) = (pid, 0);
    loop {
        digits[count] = b'0' + (n % 10) as u8;
        count += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for i in 0..count {
        path[6 + i] = digits[count - 1 - i];
    }
    path
}

fn orphan() -> R {
    let shared = Shared::new()?;
    let pid = fork()?;
    if pid == 0 {
        let grandchild = match fork() {
            Ok(p) => p,
            Err(_) => exit(98),
        };
        if grandchild == 0 {
            // Force a positive control before allowing our parent to exit.
            let parent = sys(SYS_GETPPID, [0; 6]);
            shared.word(0).store(parent as u32, SeqCst);
            shared.until(2);
            shared.word(1).store((sys(SYS_GETPPID, [0; 6]) + 1) as u32, SeqCst);
            exit(0);
        }
        shared.word(3).store(grandchild as u32, SeqCst);
        shared.until(0);
        exit(21);
    }
    wait(pid, 21)?;
    shared.word(2).store(1, SeqCst);
    let parent = shared.until(1);
    eq("orphan parent plus one is init plus one", i64::from(parent), 2)?;
    let grandchild = shared.word(3).load(SeqCst);
    // Wait for init to remove the orphan before the root's exact frame count.
    // Only the scenario's outer hang guard applies; this isn't a time bound.
    let path = proc_path(grandchild);
    loop {
        let fd = sys(SYS_OPEN, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
        if fd == -ENOENT {
            break;
        }
        let fd = ok("orphan proc lookup", fd)?;
        sys(SYS_CLOSE, [fd as u64, 0, 0, 0, 0, 0]);
        sys(SYS_POLL, [0, 0, 1, 0, 0, 0]);
    }
    eq("original parent", i64::from(shared.word(0).load(SeqCst)), pid)?;
    Ok(())
}

/// The shared-stack child never enters Rust or returns into the parent's
/// stack frames. It writes its pid, then exits entirely in assembly.
fn shared_clone(nr: u64, flags: u64, stack: u64, word: &AtomicU32) -> i64 {
    let ret: i64;
    // SAFETY: all pointers name live writable mappings. The child uses no
    // stack at all and exits before the parent resumes this Rust frame.
    unsafe {
        core::arch::asm!(
            "syscall", "test rax, rax", "jnz 2f",
            // Block once before publishing completion, so an incorrectly
            // unblocked parent observes the missing child result.
            "mov eax, {sys_poll}", "xor edi, edi", "xor esi, esi", "mov edx, 1", "syscall",
            "mov eax, {sys_getpid}", "syscall", "mov [r9], eax",
            "mov edi, 41", "mov eax, {sys_exit}", "syscall", "ud2", "2:",
            sys_poll = const SYS_POLL,
            sys_getpid = const SYS_GETPID,
            sys_exit = const SYS_EXIT,
            inlateout("rax") nr => ret, in("rdi") flags,
            in("rsi") stack, in("rdx") 0u64, in("r10") 0u64,
            in("r8") 0u64, in("r9") word as *const _ as u64,
            lateout("rcx") _, lateout("r11") _, options(nostack),
        );
    }
    ret
}

fn vforks() -> R {
    let shared = Shared::new()?;
    for nr in [SYS_CLONE, SYS_VFORK] {
        shared.word(0).store(0, SeqCst);
        let pid = ok("vfork/clone", shared_clone(nr, SPAWN_FLAGS, 0, shared.word(0)))?;
        let seen = shared.word(0).load(SeqCst);
        wait(pid, 41)?;
        eq("child completed before parent returned", i64::from(seen), pid)?;
    }
    Ok(())
}

fn clone_tids() -> R {
    let shared = Shared::new()?;
    // Separate mm: child SETTID is visible only to the child; parent SETTID
    // is visible to the parent before the syscall returns.
    let mut parent_tid = 0u32;
    let mut child_tid = 0u32;
    let flags = SIGCHLD | CLONE_PARENT_SETTID | CLONE_CHILD_SETTID;
    let pid = ok(
        "clone tid flags",
        sys(SYS_CLONE, [flags, 0, &mut parent_tid as *mut _ as u64, &mut child_tid as *mut _ as u64, 0, 0]),
    )?;
    if pid == 0 {
        let tid = sys(SYS_GETTID, [0; 6]);
        shared.word(0).store(child_tid, SeqCst);
        exit(if child_tid as i64 == tid { 0 } else { 99 });
    }
    wait(pid, 0)?;
    eq("parent tid", i64::from(parent_tid), pid)?;
    eq("child tid", i64::from(shared.word(0).load(SeqCst)), pid)?;
    eq("private child tid store", i64::from(child_tid), 0)?;
    // The same word with a shared mm is set in the child and cleared at
    // exit. The assembly child never touches the parent's stack.
    let flags = SPAWN_FLAGS | CLONE_CHILD_SETTID | CLONE_CHILD_CLEARTID;
    let ret: i64;
    shared.word(1).store(123, SeqCst);
    // SAFETY: child_tid is mapped and aligned; the child immediately exits
    // in assembly, without returning into this shared Rust stack.
    unsafe {
        core::arch::asm!("syscall", "test rax, rax", "jnz 2f",
            "mov eax, [r10]", "mov [r9], eax", "mov eax, {sys_exit}", "xor edi, edi", "syscall", "ud2", "2:",
            sys_exit = const SYS_EXIT,
            inlateout("rax") SYS_CLONE => ret, in("rdi") flags, in("rsi") 0u64,
            in("rdx") 0u64, in("r10") shared.word(1) as *const _ as u64,
            in("r8") 0u64, in("r9") shared.word(2) as *const _ as u64,
            lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    let pid = ok("clone clear tid", ret)?;
    let cleared = shared.word(1).load(SeqCst);
    wait(pid, 0)?;
    eq("shared set tid before entry", i64::from(shared.word(2).load(SeqCst)), pid)?;
    eq("shared clear tid before vfork release", i64::from(cleared), 0)
}

fn rejected_flags() -> R {
    for flags in [
        0,
        9,
        SIGCHLD | CLONE_PIDFD,
        SIGCHLD | CLONE_PARENT,
        SIGCHLD | CLONE_THREAD,
        SIGCHLD | CLONE_SIGHAND,
        SIGCHLD | (CLONE_VM | CLONE_SIGHAND),
    ] {
        eq("invalid clone flags", sys(SYS_CLONE, [flags, 0, 0, 0, 0, 0]), -EINVAL)?;
    }
    for high in [1 << 32, 1 << 33] {
        let pid = ok("legacy clone ignores high flag bits", sys(SYS_CLONE, [SIGCHLD | high, 0, 0, 0, 0, 0]))?;
        if pid == 0 {
            exit(0);
        }
        wait(pid, 0)?;
        gone(pid)?;
    }
    eq("invalid TLS", sys(SYS_CLONE, [SIGCHLD | CLONE_SETTLS, 0, 0, 0, 1 << 63, 0]), -EPERM)?;
    eq("wait4 invalid options", sys(SYS_WAIT4, [u64::MAX, 0, WEXITED, 0, 0, 0]), -EINVAL)?;
    eq("waitid no event options", sys(SYS_WAITID, [P_ALL, 0, 0, WNOHANG, 0, 0]), -EINVAL)
}

fn fork_wait() -> c_int {
    common::run_checks(&[
        ("exit-status", &statuses),
        ("wait-nohang", &nohang),
        ("waitid-zombie", &zombie),
        ("wait-copyout-fault", &copyout_fault),
        ("sigchld-autoreap", &autoreap),
        ("orphan-init", &orphan),
        ("vfork-clone", &vforks),
        ("clone-tids", &clone_tids),
        ("clone-wait-errors", &rejected_flags),
        ("fork-context", &fork_context),
        ("clone-stack-tls", &clone_stack_tls),
        ("clone-shared-resources", &clone_shared_resources),
        ("fork-inherited-state", &fork_inherited_state),
        ("rusage", &small::usage),
        ("wait-pgrp", &sessions::wait_pgrp),
    ])
}

fn clone_shared_resources() -> R {
    for share in [false, true] {
        let old_mask = sys(SYS_UMASK, [0o027, 0, 0, 0, 0, 0]);
        let fd = ok("open clone descriptor", sys(SYS_OPEN, [c"/dev/null".as_ptr() as u64, 0, 0, 0, 0, 0]))?;
        let flags = SPAWN_FLAGS | if share { CLONE_FS | CLONE_FILES } else { 0 };
        let pid: i64;
        // SAFETY: the vfork child touches no Rust stack, closes its fd and
        // changes its umask through raw syscalls, then exits in assembly.
        unsafe {
            core::arch::asm!("syscall", "test rax, rax", "jnz 2f",
                "mov eax, {sys_close}", "mov rdi, r9", "syscall",
                "mov eax, {sys_umask}", "mov edi, 0x3f", "syscall",
                "mov eax, {sys_exit}", "xor edi, edi", "syscall", "ud2", "2:",
                sys_close = const SYS_CLOSE,
                sys_exit = const SYS_EXIT,
                sys_umask = const SYS_UMASK,
                inlateout("rax") SYS_CLONE => pid, in("rdi") flags,
                in("rsi") 0u64, in("rdx") 0u64, in("r10") 0u64,
                in("r8") 0u64, in("r9") fd,
                lateout("rcx") _, lateout("r11") _, options(nostack));
        }
        let pid = ok("clone resources", pid)?;
        wait(pid, 0)?;
        let descriptor = sys(SYS_FCNTL, [fd as u64, F_GETFD, 0, 0, 0, 0]);
        let mask = sys(SYS_UMASK, [old_mask as u64, 0, 0, 0, 0, 0]);
        if descriptor >= 0 {
            sys(SYS_CLOSE, [fd as u64, 0, 0, 0, 0, 0]);
        }
        eq("CLONE_FILES shares closes", descriptor, if share { -EBADF } else { 0 })?;
        eq("CLONE_FS shares umask", mask, if share { 0o077 } else { 0o027 })?;
    }
    // Tid copy faults do not turn a successful clone into EFAULT.
    let pid = ok(
        "clone faulting tids",
        sys(SYS_CLONE, [SIGCHLD | (CLONE_PARENT_SETTID | CLONE_CHILD_SETTID), 0, 1, 1, 0, 0]),
    )?;
    if pid == 0 {
        exit(55);
    }
    wait(pid, 55)
}

fn fork_inherited_state() -> R {
    let action = [1u64, 0, 0, 0];
    let mut old_action = [0u64; 4];
    let mask = 1u64 << (SIGUSR1 - 1);
    let mut old_mask = 0u64;
    eq(
        "set inherited action",
        sys(SYS_RT_SIGACTION, [SIGUSR1, action.as_ptr() as u64, old_action.as_mut_ptr() as u64, 8, 0, 0]),
        0,
    )?;
    eq(
        "set inherited mask",
        sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, &mask as *const _ as u64, &mut old_mask as *mut _ as u64, 8, 0, 0]),
        0,
    )?;
    eq("set inherited cwd", sys(SYS_CHDIR, [c"/tmp".as_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;
    let pid = fork()?;
    if pid == 0 {
        let mut seen_action = [0u64; 4];
        let mut seen_mask = 0u64;
        let mut cwd = [0u8; 16];
        let a = sys(SYS_RT_SIGACTION, [SIGUSR1, 0, seen_action.as_mut_ptr() as u64, 8, 0, 0]);
        let m = sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, 0, &mut seen_mask as *mut _ as u64, 8, 0, 0]);
        let c = sys(SYS_GETCWD, [cwd.as_mut_ptr() as u64, cwd.len() as u64, 0, 0, 0, 0]);
        exit(if a == 0 && m == 0 && seen_action == action && seen_mask == mask && c == 5 && &cwd[..5] == b"/tmp\0" {
            0
        } else {
            99
        });
    }
    eq("restore cwd", sys(SYS_CHDIR, [c"/".as_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;
    eq("restore action", sys(SYS_RT_SIGACTION, [SIGUSR1, old_action.as_ptr() as u64, 0, 8, 0, 0]), 0)?;
    eq("restore mask", sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, &old_mask as *const _ as u64, 0, 8, 0, 0]), 0)?;
    wait(pid, 0)
}

fn fork_context() -> R {
    let mut fs_before = 0u64;
    eq("read parent TLS", sys(SYS_ARCH_PRCTL, [ARCH_GET_FS, &mut fs_before as *mut _ as u64, 0, 0, 0, 0]), 0)?;
    let mut private = 17u64;
    let pid: i64;
    let simd: u64;
    let preserved: u64;
    const MARKER: u64 = 0x123456789abcdef;
    // SAFETY: fork has no pointer arguments. Both continuations have their
    // own copied stack; all modified registers are declared to the compiler.
    unsafe {
        core::arch::asm!("movq xmm15, r9", "syscall", "movq r8, xmm15",
            inlateout("rax") SYS_FORK => pid, in("r9") MARKER,
            inlateout("r15") MARKER => preserved, lateout("r8") simd,
            lateout("xmm15") _, lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    let pid = ok("context fork", pid)?;
    let mut fs_after = 0u64;
    let got_fs = sys(SYS_ARCH_PRCTL, [ARCH_GET_FS, &mut fs_after as *mut _ as u64, 0, 0, 0, 0]);
    if pid == 0 {
        // SAFETY: private is this child's live stack variable; volatile
        // prevents the compiler from replacing the memory check with a constant.
        unsafe {
            core::ptr::write_volatile(&mut private, 99);
        }
        exit(if simd == MARKER && preserved == MARKER && got_fs == 0 && fs_after == fs_before { 0 } else { 98 });
    }
    wait(pid, 0)?;
    eq("parent FPU", simd as i64, MARKER as i64)?;
    eq("parent GPR", preserved as i64, MARKER as i64)?;
    // SAFETY: private is a live stack variable and is still mapped.
    eq("fork stack is private", unsafe { core::ptr::read_volatile(&private) } as i64, 17)
}

fn clone_stack_tls() -> R {
    let page = Shared::new()?;
    let stack = page.0 + 4096;
    let tls = page.0 + 128;
    let pid: i64;
    // SAFETY: the child stack and output pointers lie in our writable page;
    // the child uses only assembly and raw syscalls with its temporary TLS.
    unsafe {
        core::arch::asm!("syscall", "test rax, rax", "jnz 2f",
            "mov [r9], rsp", "mov eax, {sys_arch_prctl}", "mov edi, {arch_get_fs}", "lea rsi, [r9+8]", "syscall",
            "mov eax, {sys_exit}", "xor edi, edi", "syscall", "ud2", "2:",
            sys_exit = const SYS_EXIT,
            sys_arch_prctl = const SYS_ARCH_PRCTL,
            arch_get_fs = const ARCH_GET_FS,
            inlateout("rax") SYS_CLONE => pid, in("rdi") (SPAWN_FLAGS | CLONE_SETTLS),
            in("rsi") stack, in("rdx") 0u64, in("r10") 0u64,
            in("r8") tls, in("r9") page.0,
            lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    let pid = ok("clone stack TLS", pid)?;
    wait(pid, 0)?;
    // SAFETY: the child finished its stores before the vfork completion;
    // the two aligned u64s are inside the still-live shared mapping.
    let (seen_stack, seen_tls) = unsafe { ((page.0 as *const u64).read(), ((page.0 + 8) as *const u64).read()) };
    eq("child stack", seen_stack as i64, stack as i64)?;
    eq("child TLS", seen_tls as i64, tls as i64)?;
    let pid: i64;
    // SAFETY: the child touches only the mapped marker, then exits without
    // any stack reference. It never returns into this parent's Rust frame.
    unsafe {
        core::arch::asm!("syscall", "test rax, rax", "jnz 2f",
            "mov dword ptr [r9], 1", "mov eax, {sys_exit}", "xor edi, edi", "syscall", "ud2", "2:",
            sys_exit = const SYS_EXIT,
            inlateout("rax") SYS_CLONE => pid, in("rdi") SPAWN_FLAGS,
            in("rsi") 1u64 << 63, in("rdx") 0u64, in("r10") 0u64,
            in("r8") 0u64, in("r9") page.word(4) as *const _ as u64,
            lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    let pid = ok("clone noncanonical stack", pid)?;
    wait(pid, 0)?;
    eq("noncanonical RSP reached ring 3", i64::from(page.word(4).load(SeqCst)), 1)
}

const SYS_WRITE: u64 = 1;
const SYS_BRK: u64 = 12;
const SYS_EXECVE: u64 = 59;
const SYS_UNLINK: u64 = 87;
const SYS_READLINK: u64 = 89;
const SYS_PAUSE: u64 = 34;
const SIGKILL: u64 = 9;
const F_DUPFD: u64 = 0;
const F_DUPFD_CLOEXEC: u64 = 1030;
const O_WRONLY: u64 = 1;
const O_RDWR: u64 = 2;
const O_CREAT: u64 = 0x40;
const O_TRUNC: u64 = 0x200;
const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const MAP_PRIVATE: u64 = 2;
const EACCES: i64 = 13;
const ENOEXEC: i64 = 8;
const E2BIG: i64 = 7;
const ELOOP: i64 = 40;

const O_RDONLY: u64 = 0;

/// POSIX shared memory and named semaphores through musl, including its
/// temporary-file/link publication of a newly created semaphore.
mod posix_shm {
    use super::{
        EAGAIN, EBADF, ELOOP, ENOENT, F_GETFD, MAP_SHARED, O_CREAT, O_RDWR, PROT_READ, PROT_WRITE, SIGKILL, SYS_FCNTL,
        SYS_KILL, SYS_OPEN, wait,
    };
    use crate::common::{self, R, ensure, eq, ok, sys};
    use core::ffi::{c_char, c_int, c_void};
    use core::sync::atomic::{AtomicU32, Ordering::SeqCst};

    // SAFETY: declarations match musl's x86-64 C ABI; sem_t is opaque here.
    unsafe extern "C" {
        fn shm_open(name: *const c_char, flags: c_int, mode: u32) -> c_int;
        fn shm_unlink(name: *const c_char) -> c_int;
        fn sem_open(name: *const c_char, flags: c_int, ...) -> *mut c_void;
        fn sem_close(sem: *mut c_void) -> c_int;
        fn sem_unlink(name: *const c_char) -> c_int;
        fn sem_post(sem: *mut c_void) -> c_int;
        fn sem_wait(sem: *mut c_void) -> c_int;
        fn sem_trywait(sem: *mut c_void) -> c_int;
        fn fork() -> c_int;
        fn ftruncate(fd: c_int, size: i64) -> c_int;
        fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, off: i64) -> *mut c_void;
        fn munmap(addr: *mut c_void, len: usize) -> c_int;
        fn __errno_location() -> *mut c_int;
    }

    const EINTR: i64 = 4;
    const EEXIST: i64 = 17;
    const O_DIRECTORY: u64 = 0x10000;
    const O_EXCL: c_int = 0x80;
    const SYS_SYMLINK: u64 = 88;
    const EXCLUSIVE: c_int = O_CREAT as c_int | O_EXCL;
    const SIZE: usize = 8192;
    const NAME: &core::ffi::CStr = c"/lumen-433-shm";
    const READY: &core::ffi::CStr = c"/lumen-433-ready";
    const DONE: &core::ffi::CStr = c"/lumen-433-done";

    fn errno() -> i64 {
        // SAFETY: musl supplies this thread's live errno slot.
        unsafe { i64::from(*__errno_location()) }
    }

    fn libc_ok(what: &'static str, ret: c_int) -> Result<c_int, common::Fail> {
        if ret == -1 { Err(common::Fail { what, got: -errno() }) } else { Ok(ret) }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Failure {
        None,
        ChildEarly,
        ChildLate,
        Parent,
    }

    fn metadata() -> R {
        let fd = ok("open /dev/shm", sys(SYS_OPEN, [c"/dev/shm".as_ptr() as u64, O_DIRECTORY, 0, 0, 0, 0]))? as i32;
        let mode = common::fstat_mode(fd)?;
        common::close(fd)?;
        eq("/dev/shm directory and sticky mode", i64::from(mode), 0o041777)?;
        // SAFETY: names are NUL-terminated and retained for every call.
        unsafe {
            let fd = shm_open(NAME.as_ptr(), EXCLUSIVE | O_RDWR as c_int, 0o600);
            ensure("shm_open create", fd >= 0, errno())?;
            eq("shm descriptor is CLOEXEC", sys(SYS_FCNTL, [fd as u64, F_GETFD, 0, 0, 0, 0]), 1)?;
            eq("shm exclusive collision", shm_open(NAME.as_ptr(), EXCLUSIVE | O_RDWR as c_int, 0o600) as i64, -1)?;
            eq("shm EEXIST", errno(), EEXIST)?;
            common::close(fd)?;
            libc_ok("shm_unlink", shm_unlink(NAME.as_ptr()))?;
            // A final symlink must not be followed by shm_open.
            eq(
                "symlink",
                sys(SYS_SYMLINK, [c"/dev/zero".as_ptr() as u64, c"/dev/shm/lumen-433-shm".as_ptr() as u64, 0, 0, 0, 0]),
                0,
            )?;
            eq("shm nofollow", shm_open(NAME.as_ptr(), O_RDWR as c_int, 0) as i64, -1)?;
            eq("shm ELOOP", errno(), ELOOP)?;
            libc_ok("unlink symlink", shm_unlink(NAME.as_ptr()))?;
        }
        Ok(())
    }

    fn shared_memory_and_semaphores(failure: Failure) -> R {
        // SAFETY: all names are static C strings. Successful mmap results
        // cover SIZE writable bytes, aligned for AtomicU32; accesses stay
        // in those mappings until munmap. Semaphore pointers are checked
        // against musl's SEM_FAILED (NULL) before use; each successful open
        // owns a reference. The single-threaded fork inherits no held libc locks.
        unsafe {
            let fd = shm_open(NAME.as_ptr(), EXCLUSIVE | O_RDWR as c_int, 0o600);
            ensure("shm_open", fd >= 0, errno())?;
            libc_ok("ftruncate", ftruncate(fd, SIZE as i64))?;
            let mapping =
                mmap(core::ptr::null_mut(), SIZE, (PROT_READ | PROT_WRITE) as c_int, MAP_SHARED as c_int, fd, 0);
            ensure("MAP_SHARED", mapping as usize != usize::MAX, errno())?;
            common::close(fd)?;
            let words = mapping.cast::<AtomicU32>();
            eq("shm starts zero", (*words).load(SeqCst) as i64, 0)?;
            (*words).store(17, SeqCst);
            // Leave the second page unfaulted until after fork.
            let ready = sem_open(READY.as_ptr(), EXCLUSIVE, 0o600u32, 0u32);
            let done = sem_open(DONE.as_ptr(), EXCLUSIVE, 0o600u32, 0u32);
            ensure("sem_open create", !ready.is_null() && !done.is_null(), errno())?;
            ensure("sem exclusive collision", sem_open(READY.as_ptr(), EXCLUSIVE, 0o600u32, 0u32).is_null(), errno())?;
            eq("sem EEXIST", errno(), EEXIST)?;
            eq("zero semaphore", sem_trywait(done) as i64, -1)?;
            eq("sem EAGAIN", errno(), EAGAIN)?;
            let pid = i64::from(libc_ok("musl fork", fork())?);
            if pid == 0 {
                let result = (|| -> R {
                    if failure == Failure::ChildEarly {
                        libc_ok("injected child before post", ftruncate(-1, 0))?;
                    }
                    eq("child inherits shared data", (*words).load(SeqCst) as i64, 17)?;
                    // Reopening ready creates a fresh mapping; musl's table
                    // returns the inherited done mapping. The shm reopen is
                    // asserted at a second VA. This tests the published link;
                    // the file-backed futex wait path depends on scheduling.
                    libc_ok("child close ready", sem_close(ready))?;
                    // Keep the inherited done handle alive for failure
                    // notification even if reopening or later checks fail.
                    let ready = sem_open(READY.as_ptr(), 0);
                    let done = sem_open(DONE.as_ptr(), 0);
                    ensure("child sem_open existing", !ready.is_null() && !done.is_null(), errno())?;
                    let fd = shm_open(NAME.as_ptr(), O_RDWR as c_int, 0);
                    ensure("child shm_open existing", fd >= 0, errno())?;
                    let second = mmap(
                        core::ptr::null_mut(),
                        SIZE,
                        (PROT_READ | PROT_WRITE) as c_int,
                        MAP_SHARED as c_int,
                        fd,
                        0,
                    );
                    ensure("child second mapping", second as usize != usize::MAX && second != mapping, errno())?;
                    common::close(fd)?;
                    let other = second.cast::<AtomicU32>();
                    eq("reopened data", (*other).load(SeqCst) as i64, 17)?;
                    (*words.add(1)).store(29, SeqCst);
                    libc_ok("child sem_post", sem_post(done))?;
                    libc_ok("child sem_wait", sem_wait(ready))?;
                    if failure == Failure::ChildLate {
                        libc_ok("injected child after wait", ftruncate(-1, 0))?;
                    }
                    eq("inherited mapping sees parent", (*words).load(SeqCst) as i64, 41)?;
                    eq("reopened mapping sees parent", (*other).load(SeqCst) as i64, 41)?;
                    eq("late fault sees parent", (*other.add(1024)).load(SeqCst) as i64, 47)?;
                    eq("inherited late fault sees parent", (*words.add(1024)).load(SeqCst) as i64, 47)?;
                    (*other).store(43, SeqCst);
                    libc_ok("child unmap reopened", munmap(second, SIZE))?;
                    libc_ok("child unmap inherited", munmap(mapping, SIZE))?;
                    libc_ok("child post after unlink", sem_post(done))?;
                    libc_ok("child close ready", sem_close(ready))?;
                    libc_ok("child close done", sem_close(done))?;
                    Ok(())
                })();
                if let Err(e) = result {
                    say!("FAIL shm child: {} (got {})", e.what, e.got);
                    // The parent may be in either wait. The inherited
                    // handle remains valid regardless of the failing step.
                    // The second post covers a failure at the first
                    // sem_post(done), which no failure stage injects.
                    for _ in 0..2 {
                        if let Err(e) = libc_ok("child failure sem_post", sem_post(done)) {
                            say!("FAIL shm child release: {} (got {})", e.what, e.got);
                        }
                    }
                    common::exit_group(1);
                }
                if let Err(e) = libc_ok("child close inherited done", sem_close(done)) {
                    say!("FAIL shm child: {} (got {})", e.what, e.got);
                    common::exit_group(1);
                }
                common::exit_group(0);
            }
            let result = (|| -> R {
                libc_ok("parent sem_wait", sem_wait(done))?;
                eq("parent sees child store", (*words.add(1)).load(SeqCst) as i64, 29)?;
                if failure == Failure::Parent {
                    libc_ok("injected parent before release", ftruncate(-1, 0))?;
                }
                libc_ok("shm_unlink while mapped", shm_unlink(NAME.as_ptr()))?;
                eq("shm name gone", shm_open(NAME.as_ptr(), O_RDWR as c_int, 0) as i64, -1)?;
                eq("shm ENOENT", errno(), ENOENT)?;
                for name in [READY, DONE] {
                    libc_ok("sem_unlink while open", sem_unlink(name.as_ptr()))?;
                    ensure("sem name gone", sem_open(name.as_ptr(), 0).is_null(), errno())?;
                    eq("sem ENOENT", errno(), ENOENT)?;
                }
                (*words).store(41, SeqCst);
                (*words.add(1024)).store(47, SeqCst);
                libc_ok("parent post after unlink", sem_post(ready))?;
                libc_ok("parent wait after unlink", sem_wait(done))?;
                eq("parent sees reopened child store", (*words).load(SeqCst) as i64, 43)?;
                Ok(())
            })();
            if result.is_err() {
                // Release a child still waiting for ready before reaping.
                // If the semaphore itself fails, kill is the fallback.
                if let Err(e) = libc_ok("parent failure sem_post", sem_post(ready)) {
                    say!("FAIL shm parent release: {} (got {})", e.what, e.got);
                    let killed = sys(SYS_KILL, [pid as u64, SIGKILL, 0, 0, 0, 0]);
                    if killed < 0 {
                        say!("FAIL shm parent kill (got {killed})");
                    }
                }
            }
            let reaped = loop {
                match wait(pid, 0) {
                    Err(e) if e.got == -EINTR => continue,
                    result => break result,
                }
            };
            if let Err(ref e) = reaped {
                say!("FAIL shm child wait: {} (got {})", e.what, e.got);
            }
            result?;
            reaped?;
            eq("token consumed", sem_trywait(done) as i64, -1)?;
            eq("consumed EAGAIN", errno(), EAGAIN)?;
            libc_ok("parent sem_close ready", sem_close(ready))?;
            libc_ok("parent sem_close done", sem_close(done))?;
            libc_ok("parent munmap", munmap(mapping, SIZE))?;
        }
        Ok(())
    }

    pub fn run(stage: &[u8]) -> c_int {
        let failure = match stage {
            b"none" => Failure::None,
            b"child-early" => Failure::ChildEarly,
            b"child-late" => Failure::ChildLate,
            b"parent" => Failure::Parent,
            _ => return common::USAGE_EXIT,
        };
        // Check that libc_ok turns a real libc failure into -EBADF before
        // running the shm checks. The injected failures make their own calls.
        // SAFETY: ftruncate has no pointer arguments; -1 is deliberately invalid.
        let bad_fd = libc_ok("ftruncate errno", unsafe { ftruncate(-1, 0) });
        match bad_fd {
            Err(e) if e.got == -EBADF => {}
            Err(e) => {
                say!("FAIL libc_ok expected -EBADF, got Err({}: {})", e.what, e.got);
                return 1;
            }
            Ok(ret) => {
                say!("FAIL libc_ok expected -EBADF, got Ok({ret})");
                return 1;
            }
        }
        let mut result = common::run_checks(&[
            ("shm-metadata-and-open-flags", &metadata),
            ("shm-and-semaphores-across-fork", &|| shared_memory_and_semaphores(failure)),
        ]);
        // libc owns the namespace spelling. ENOENT is expected after the
        // normal path already unlinked; any other cleanup error is reported.
        // SAFETY: names are static C strings, valid for each libc call.
        unsafe {
            for cleanup in [
                libc_ok("cleanup shm_unlink", shm_unlink(NAME.as_ptr())),
                libc_ok("cleanup sem_unlink ready", sem_unlink(READY.as_ptr())),
                libc_ok("cleanup sem_unlink done", sem_unlink(DONE.as_ptr())),
            ] {
                if let Err(e) = cleanup
                    && e.got != -ENOENT
                {
                    say!("FAIL shm cleanup: {} (got {})", e.what, e.got);
                    if result == 0 {
                        result = 3;
                    }
                }
            }
        }
        result
    }
}
