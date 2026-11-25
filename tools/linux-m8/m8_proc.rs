//! `m8_proc`: M8's ring-3 checks of processes, threads, signals, faults,
//! `futex`, timers, sessions and the small calls (`tests/scenarios/test_linux_proc.py`).
//!
//! A `#![no_std]` dynamically linked musl x86-64 executable, built as
//! `tools/linux-m7/`'s are (the Makefile) into `build/linux/`, which both
//! images stage in `/usr/local/bin`. Its checks and their conventions:
//! `common.rs` and ARCHITECTURE section 15, "M8's tests". The `fork-wait`
//! mode checks C1's process lifecycle and clone ABI.

#![no_std]
#![no_main]

#[macro_use]
mod common;

use core::ffi::c_int;

const USAGE: &str = "usage: m8_proc <mode> (fork-wait)";

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const u8) -> c_int {
    // SAFETY: musl's `__libc_start_main` passes the kernel's `argc` and
    // `argv`, kept for the program's life.
    let (args, n) = unsafe { common::args(argc, argv) };
    match &args[..n] {
        [_, b"fork-wait"] => fork_wait(),
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

fn exit(code: u64) -> ! {
    sys(60, [code, 0, 0, 0, 0, 0]);
    panic!("exit returned")
}

fn fork() -> Result<i64, common::Fail> {
    ok("fork", sys(57, [0; 6]))
}

fn wait(pid: i64, code: i64) -> R {
    let mut status = -1i32;
    eq("wait pid", sys(61, [pid as u64, &mut status as *mut _ as u64, 0, 0, 0, 0]), pid)?;
    eq("exit status word", i64::from(status), (code & 255) << 8)
}

fn statuses() -> R {
    eq("no children", sys(61, [u64::MAX, 0, 0, 0, 0, 0]), -10)?;
    for code in [0, 1, 42, 127, 255, 256, 511] {
        let pid = fork()?;
        if pid == 0 {
            exit(code as u64);
        }
        wait(pid, code)?;
        eq("already reaped", sys(61, [pid as u64, 0, 1, 0, 0, 0]), -10)?;
    }
    let pid = fork()?;
    if pid == 0 {
        sys(231, [29, 0, 0, 0, 0, 0]);
        exit(99);
    }
    wait(pid, 29)
}

struct Shared(u64);
impl Shared {
    fn new() -> Result<Self, common::Fail> {
        let ptr = ok("shared mmap", sys(9, [0, 4096, 3, 0x21, u64::MAX, 0]))? as u64;
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
            sys(7, [0, 0, 1, 0, 0, 0]);
        }
    }
}
impl Drop for Shared {
    fn drop(&mut self) {
        sys(11, [self.0, 4096, 0, 0, 0, 0]);
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
    let ret = sys(61, [pid as u64, &mut status as *mut _ as u64, 1, 0, 0, 0]);
    let mut info = [0x55i32; 32];
    let idret = sys(247, [1, pid as u64, info.as_mut_ptr() as u64, 5, 0, 0]);
    shared.word(0).store(1, SeqCst);
    wait(pid, 73)?;
    eq("WNOHANG live child", ret, 0)?;
    eq("WNOHANG leaves status", i64::from(status), 0x1234)?;
    eq("waitid WNOHANG", idret, 0)?;
    for i in [0, 1, 2, 4, 5, 6] {
        eq("waitid zero field", i64::from(info[i]), 0)?;
    }
    Ok(())
}

fn zombie() -> R {
    let pid = fork()?;
    if pid == 0 {
        exit(87);
    }
    for _ in 0..2 {
        let mut info = [0i32; 32];
        eq("waitid WNOWAIT", sys(247, [1, pid as u64, info.as_mut_ptr() as u64, 0x01000004, 0, 0]), 0)?;
        eq("si_signo", i64::from(info[0]), 17)?;
        eq("si_code", i64::from(info[2]), 1)?;
        eq("si_pid", i64::from(info[4]), pid)?;
        eq("si_uid", i64::from(info[5]), 0)?;
        eq("si_status", i64::from(info[6]), 87)?;
    }
    wait(pid, 87)?;
    let pid = fork()?;
    if pid == 0 {
        exit(44);
    }
    let mut info = [0i32; 32];
    eq("waitid reap", sys(247, [1, pid as u64, info.as_mut_ptr() as u64, 4, 0, 0]), 0)?;
    eq("waitid reaped status", i64::from(info[6]), 44)?;
    eq("waitid removed zombie", sys(61, [pid as u64, 0, 0, 0, 0, 0]), -10)
}

fn copyout_fault() -> R {
    let pid = fork()?;
    if pid == 0 {
        exit(12);
    }
    eq("wait4 bad pointer", sys(61, [pid as u64, 1, 0, 0, 0, 0]), -14)?;
    eq("EFAULT still reaps", sys(61, [pid as u64, 0, 0, 0, 0, 0]), -10)?;
    let pid = fork()?;
    if pid == 0 {
        exit(13);
    }
    eq("waitid bad pointer", sys(247, [1, pid as u64, 1, 4, 0, 0]), -14)?;
    eq("waitid EFAULT still reaps", sys(61, [pid as u64, 0, 0, 0, 0, 0]), -10)
}

fn autoreap() -> R {
    for (handler, flags) in [(1u64, 0u64), (0, 2)] {
        let action = [handler, flags, 0, 0];
        let mut old = [0u64; 4];
        eq("SIGCHLD disposition", sys(13, [17, action.as_ptr() as u64, old.as_mut_ptr() as u64, 8, 0, 0]), 0)?;
        let pid = fork()?;
        if pid == 0 {
            exit(33);
        }
        let ret = sys(61, [pid as u64, 0, 0, 0, 0, 0]);
        eq("restore SIGCHLD", sys(13, [17, old.as_ptr() as u64, 0, 8, 0, 0]), 0)?;
        eq("automatic reap", ret, -10)?;
    }
    Ok(())
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
            let parent = sys(110, [0; 6]);
            shared.word(0).store(parent as u32, SeqCst);
            while sys(110, [0; 6]) != 1 {
                sys(7, [0, 0, 1, 0, 0, 0]);
            }
            shared.word(1).store(1, SeqCst);
            exit(0);
        }
        shared.until(0);
        exit(21);
    }
    wait(pid, 21)?;
    eq("original parent", i64::from(shared.word(0).load(SeqCst)), pid)?;
    eq("orphan parent is init", i64::from(shared.until(1)), 1)
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
            "mov eax, 7", "xor edi, edi", "xor esi, esi", "mov edx, 1", "syscall",
            "mov eax, 39", "syscall", "mov [r9], eax",
            "mov edi, 41", "mov eax, 60", "syscall", "ud2", "2:",
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
    for nr in [56, 58] {
        shared.word(0).store(0, SeqCst);
        let pid = ok("vfork/clone", shared_clone(nr, 0x4111, 0, shared.word(0)))?;
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
    let flags = 17 | 0x100000 | 0x1000000;
    let pid = ok(
        "clone tid flags",
        sys(56, [flags, 0, &mut parent_tid as *mut _ as u64, &mut child_tid as *mut _ as u64, 0, 0]),
    )?;
    if pid == 0 {
        let tid = sys(186, [0; 6]);
        shared.word(0).store(child_tid, SeqCst);
        exit(if child_tid as i64 == tid { 0 } else { 99 });
    }
    wait(pid, 0)?;
    eq("parent tid", i64::from(parent_tid), pid)?;
    eq("child tid", i64::from(shared.word(0).load(SeqCst)), pid)?;
    eq("private child tid store", i64::from(child_tid), 0)?;
    // The same word with a shared mm is set in the child and cleared at
    // exit. The assembly child never touches the parent's stack.
    let flags = 0x4111 | 0x1000000 | 0x200000;
    let ret: i64;
    shared.word(1).store(123, SeqCst);
    // SAFETY: child_tid is mapped and aligned; the child immediately exits
    // in assembly, without returning into this shared Rust stack.
    unsafe {
        core::arch::asm!("syscall", "test rax, rax", "jnz 2f",
            "mov eax, [r10]", "mov [r9], eax", "mov eax, 60", "xor edi, edi", "syscall", "ud2", "2:",
            inlateout("rax") 56u64 => ret, in("rdi") flags, in("rsi") 0u64,
            in("rdx") 0u64, in("r10") shared.word(1) as *const _ as u64,
            in("r8") 0u64, in("r9") shared.word(2) as *const _ as u64,
            lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    let pid = ok("clone clear tid", ret)?;
    wait(pid, 0)?;
    eq("shared set tid before entry", i64::from(shared.word(2).load(SeqCst)), pid)?;
    eq("shared clear tid at exit", i64::from(shared.word(1).load(SeqCst)), 0)
}

fn rejected_flags() -> R {
    for flags in [0, 9, 17 | 0x1000, 17 | 0x8000, 17 | 0x10000, 17 | 0x800, 17 | 0x900, 17 | (1 << 32), 17 | (1 << 33)]
    {
        eq("invalid clone flags", sys(56, [flags, 0, 0, 0, 0, 0]), -22)?;
    }
    eq("invalid TLS", sys(56, [17 | 0x80000, 0, 0, 0, 1 << 63, 0]), -1)?;
    eq("wait4 invalid options", sys(61, [u64::MAX, 0, 4, 0, 0, 0]), -22)?;
    eq("waitid no event options", sys(247, [0, 0, 0, 1, 0, 0]), -22)
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
    ])
}

fn clone_shared_resources() -> R {
    for share in [false, true] {
        let old_mask = sys(95, [0o027, 0, 0, 0, 0, 0]);
        let fd = ok("open clone descriptor", sys(2, [c"/dev/null".as_ptr() as u64, 0, 0, 0, 0, 0]))?;
        let flags = 0x4111 | if share { 0x600 } else { 0 };
        let pid: i64;
        // SAFETY: the vfork child touches no Rust stack, closes its fd and
        // changes its umask through raw syscalls, then exits in assembly.
        unsafe {
            core::arch::asm!("syscall", "test rax, rax", "jnz 2f",
                "mov eax, 3", "mov rdi, r9", "syscall",
                "mov eax, 95", "mov edi, 0x3f", "syscall",
                "mov eax, 60", "xor edi, edi", "syscall", "ud2", "2:",
                inlateout("rax") 56u64 => pid, in("rdi") flags,
                in("rsi") 0u64, in("rdx") 0u64, in("r10") 0u64,
                in("r8") 0u64, in("r9") fd,
                lateout("rcx") _, lateout("r11") _, options(nostack));
        }
        let pid = ok("clone resources", pid)?;
        wait(pid, 0)?;
        let descriptor = sys(72, [fd as u64, 1, 0, 0, 0, 0]);
        let mask = sys(95, [old_mask as u64, 0, 0, 0, 0, 0]);
        if descriptor >= 0 {
            sys(3, [fd as u64, 0, 0, 0, 0, 0]);
        }
        eq("CLONE_FILES shares closes", descriptor, if share { -9 } else { 0 })?;
        eq("CLONE_FS shares umask", mask, if share { 0o077 } else { 0o027 })?;
    }
    // Tid copy faults do not turn a successful clone into EFAULT.
    let pid = ok("clone faulting tids", sys(56, [17 | 0x1100000, 0, 1, 1, 0, 0]))?;
    if pid == 0 {
        exit(55);
    }
    wait(pid, 55)
}

fn fork_inherited_state() -> R {
    let action = [1u64, 0, 0, 0];
    let mut old_action = [0u64; 4];
    let mask = 1u64 << 9; // SIGUSR1
    let mut old_mask = 0u64;
    eq("set inherited action", sys(13, [10, action.as_ptr() as u64, old_action.as_mut_ptr() as u64, 8, 0, 0]), 0)?;
    eq("set inherited mask", sys(14, [2, &mask as *const _ as u64, &mut old_mask as *mut _ as u64, 8, 0, 0]), 0)?;
    eq("set inherited cwd", sys(80, [c"/tmp".as_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;
    let pid = fork()?;
    if pid == 0 {
        let mut seen_action = [0u64; 4];
        let mut seen_mask = 0u64;
        let mut cwd = [0u8; 16];
        let a = sys(13, [10, 0, seen_action.as_mut_ptr() as u64, 8, 0, 0]);
        let m = sys(14, [2, 0, &mut seen_mask as *mut _ as u64, 8, 0, 0]);
        let c = sys(79, [cwd.as_mut_ptr() as u64, cwd.len() as u64, 0, 0, 0, 0]);
        exit(if a == 0 && m == 0 && seen_action == action && seen_mask == mask && c == 5 && &cwd[..5] == b"/tmp\0" {
            0
        } else {
            99
        });
    }
    eq("restore cwd", sys(80, [c"/".as_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;
    eq("restore action", sys(13, [10, old_action.as_ptr() as u64, 0, 8, 0, 0]), 0)?;
    eq("restore mask", sys(14, [2, &old_mask as *const _ as u64, 0, 8, 0, 0]), 0)?;
    wait(pid, 0)
}

fn fork_context() -> R {
    let mut fs_before = 0u64;
    eq("read parent TLS", sys(158, [0x1003, &mut fs_before as *mut _ as u64, 0, 0, 0, 0]), 0)?;
    let mut private = 17u64;
    let pid: i64;
    let simd: u64;
    let preserved: u64;
    const MARKER: u64 = 0x123456789abcdef;
    // SAFETY: fork has no pointer arguments. Both continuations have their
    // own copied stack; all modified registers are declared to the compiler.
    unsafe {
        core::arch::asm!("movq xmm15, r9", "syscall", "movq r8, xmm15",
            inlateout("rax") 57u64 => pid, in("r9") MARKER,
            inlateout("r15") MARKER => preserved, lateout("r8") simd,
            lateout("xmm15") _, lateout("rcx") _, lateout("r11") _, options(nostack));
    }
    let pid = ok("context fork", pid)?;
    let mut fs_after = 0u64;
    let got_fs = sys(158, [0x1003, &mut fs_after as *mut _ as u64, 0, 0, 0, 0]);
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
            "mov [r9], rsp", "mov eax, 158", "mov edi, 0x1003", "lea rsi, [r9+8]", "syscall",
            "mov eax, 60", "xor edi, edi", "syscall", "ud2", "2:",
            inlateout("rax") 56u64 => pid, in("rdi") 0x84111u64,
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
    let pid = ok("clone noncanonical stack", sys(56, [17, 1 << 63, 0, 0, 0, 0]))?;
    if pid == 0 {
        exit(99);
    }
    let mut status = 0i32;
    eq("wait bad stack child", sys(61, [pid as u64, &mut status as *mut _ as u64, 0, 0, 0, 0]), pid)?;
    eq("bad stack is child SIGBUS without core", i64::from(status), 7)
}
