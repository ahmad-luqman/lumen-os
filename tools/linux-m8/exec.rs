//! C2 exec and copy-on-write checks through the real syscall ABI.
use crate::common::{self, R, ensure, eq, ok, sys};
use crate::*;
use core::ffi::{CStr, c_int};
use core::sync::atomic::Ordering::SeqCst;

// SAFETY: musl publishes environ and these functions with their C ABI.
unsafe extern "C" {
    static environ: *const *const u8;
    fn signal(sig: c_int, handler: extern "C" fn(c_int)) -> usize;
    fn getauxval(kind: usize) -> usize;
}

const SELF: &CStr = c"/usr/local/bin/m8_proc";
const SCRIPT: &CStr = c"/tmp/c2-script";
const NESTED: &CStr = c"/tmp/c2-nested";

fn create_fd(path: &CStr) -> Result<u64, common::Fail> {
    ok("create exec fixture", sys(SYS_OPEN, [path.as_ptr() as u64, O_WRONLY | O_CREAT | O_TRUNC, 0o755, 0, 0, 0]))
        .map(|fd| fd as u64)
}

fn create(path: &CStr, bytes: &[u8]) -> R {
    let fd = create_fd(path)?;
    let written = sys(SYS_WRITE, [fd, bytes.as_ptr() as u64, bytes.len() as u64, 0, 0, 0]);
    sys(SYS_CLOSE, [fd, 0, 0, 0, 0, 0]);
    eq("write exec fixture", written, bytes.len() as i64)
}

fn exec(path: &CStr, args: &[u64]) -> i64 {
    sys(SYS_EXECVE, [path.as_ptr() as u64, args.as_ptr() as u64, 0, 0, 0, 0])
}

fn cow() -> R {
    let shared = Shared::new()?;
    let heap = ok("brk query", sys(SYS_BRK, [0; 6]))? as u64;
    eq("grow heap", sys(SYS_BRK, [heap + 8192, 0, 0, 0, 0, 0]), (heap + 8192) as i64)?;
    create(c"/tmp/c2-private", b"original")?;
    let fd = ok("open private file", sys(SYS_OPEN, [c"/tmp/c2-private".as_ptr() as u64, O_RDONLY, 0, 0, 0, 0]))? as u64;
    let mapped = ok("private file map", sys(SYS_MMAP, [0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0]))? as u64;
    let mut stack = 11u64;
    // SAFETY: brk just extended this process's heap; mmap returned a writable page.
    unsafe {
        (heap as *mut u64).write_volatile(11);
        (mapped as *mut u64).write_volatile(11);
    }
    let pid = fork()?;
    if pid == 0 {
        // SAFETY: all three locations are live private memory inherited by fork.
        unsafe {
            core::ptr::write_volatile(&mut stack, 22);
            (heap as *mut u64).write_volatile(22);
            (mapped as *mut u64).write_volatile(22);
        }
        shared.word(0).store(22, SeqCst);
        shared.until(1);
        // SAFETY: mappings and stack are still live in this child.
        let isolated = unsafe {
            read_word(&stack) == 22
                && (heap as *const u64).read_volatile() == 22
                && (mapped as *const u64).read_volatile() == 22
        };
        exit(if isolated && shared.word(1).load(SeqCst) == 33 { 0 } else { 90 });
    }
    shared.until(0);
    // SAFETY: this parent's private mappings and stack are still live.
    let before = unsafe {
        read_word(&stack) == 11
            && (heap as *const u64).read_volatile() == 11
            && (mapped as *const u64).read_volatile() == 11
    };
    // SAFETY: writes target the parent's writable mappings and local stack.
    unsafe {
        core::ptr::write_volatile(&mut stack, 33);
        (heap as *mut u64).write_volatile(33);
        (mapped as *mut u64).write_volatile(33);
    }
    shared.word(1).store(33, SeqCst);
    wait(pid, 0)?;
    let mut original = [0; 8];
    eq("private store did not reach file", sys(SYS_READ, [fd, original.as_mut_ptr() as u64, 8, 0, 0, 0]), 8)?;
    ensure("file unchanged", &original == b"original", 0)?;
    // SAFETY: the parent still owns these locations after child teardown.
    let after = unsafe {
        read_word(&stack) == 33
            && (heap as *const u64).read_volatile() == 33
            && (mapped as *const u64).read_volatile() == 33
    };
    sys(SYS_MUNMAP, [mapped, 4096, 0, 0, 0, 0]);
    sys(SYS_CLOSE, [fd, 0, 0, 0, 0, 0]);
    sys(SYS_UNLINK, [c"/tmp/c2-private".as_ptr() as u64, 0, 0, 0, 0, 0]);
    eq("shrink heap", sys(SYS_BRK, [heap, 0, 0, 0, 0, 0]), heap as i64)?;
    ensure("heap, stack and private file isolated both ways", before && after, 0)
}

fn read_word(p: &u64) -> u64 {
    // SAFETY: the caller supplies a live stack word; volatile defeats constant folding.
    unsafe { core::ptr::read_volatile(p) }
}

fn setup_state() -> R {
    let fd = ok("open", sys(SYS_OPEN, [c"/dev/null".as_ptr() as u64, O_RDONLY, 0, 0, 0, 0]))? as u64;
    eq("cloexec 99", sys(SYS_FCNTL, [fd, F_DUPFD_CLOEXEC, 99, 0, 0, 0]), 99)?;
    eq("inherited 100", sys(SYS_FCNTL, [fd, F_DUPFD, 100, 0, 0, 0]), 100)?;
    sys(SYS_CLOSE, [fd, 0, 0, 0, 0, 0]);
    for (sig, handler) in [(SIGUSR1, 1), (SIGUSR2, 0x12345678)] {
        let action = [handler, 0, 0, 0];
        eq("set exec signal", sys(SYS_RT_SIGACTION, [sig, action.as_ptr() as u64, 0, 8, 0, 0]), 0)?;
    }
    let mask = 1u64 << (SIGUSR2 - 1);
    eq("block USR2", sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, &mask as *const _ as u64, 0, 8, 0, 0]), 0)?;
    eq("queue blocked signal", sys(SYS_KILL, [sys(SYS_GETPID, [0; 6]) as u64, SIGUSR2, 0, 0, 0, 0]), 0)?;
    signals::altstack()?;
    let fcw = 0x0f7fu16;
    let mxcsr = 0x7f80u32;
    // SAFETY: these are valid x87/SSE control words in live readable locals;
    // only this child's floating-point state changes before exec replaces it.
    unsafe {
        core::arch::asm!("fldcw [{cw}]", "ldmxcsr [{mx}]", cw = in(reg) &fcw,
            mx = in(reg) &mxcsr, options(nostack, readonly));
    }
    Ok(())
}

pub fn child_check(args: &[&[u8]]) -> c_int {
    let result = (|| -> R {
        let mut fcw = 0u16;
        let mut mxcsr = 0u32;
        // SAFETY: both outputs are live writable locals of the instruction's width.
        unsafe {
            core::arch::asm!("fnstcw [{cw}]", "stmxcsr [{mx}]", cw = in(reg) &mut fcw,
                mx = in(reg) &mut mxcsr, options(nostack));
        }
        eq("exec resets x87", i64::from(fcw), 0x037f)?;
        eq("exec resets SSE", i64::from(mxcsr), 0x1f80)?;
        ensure(
            "shebang argv",
            matches!(
                args,
                [b"/usr/local/bin/m8_proc", b"exec-check", b"/tmp/c2-script", b"tail-abs"]
                    | [b"/usr/local/bin/m8_proc", b"exec-check", b"./c2-script", b"tail-rel"]
                    | [
                        b"/usr/local/bin/m8_proc",
                        b"exec-check",
                        b"/tmp/c2-script",
                        b"nested arg",
                        b"/tmp/c2-nested",
                        b"tail-nested"
                    ]
            ),
            args.len() as i64,
        )?;
        // SAFETY: the kernel's terminated envp is retained by musl for this process.
        unsafe {
            ensure(
                "envp[0] from initial stack",
                CStr::from_ptr((*environ).cast()).to_bytes() == b"PATH=/bin:/usr/bin",
                0,
            )?;
            ensure("envp[1] order", CStr::from_ptr((*environ.add(1)).cast()).to_bytes() == b"C2=second", 0)?;
            ensure("envp ends after two", (*environ.add(2)).is_null(), 0)?;
        }
        let mut path = [0; 128];
        let n = ok(
            "readlink exe",
            sys(
                SYS_READLINK,
                [c"/proc/self/exe".as_ptr() as u64, path.as_mut_ptr() as u64, path.len() as u64, 0, 0, 0],
            ),
        )? as usize;
        ensure("exe is interpreter", &path[..n] == SELF.to_bytes(), n as i64)?;
        eq("closed cloexec", sys(SYS_FCNTL, [99, F_GETFD, 0, 0, 0, 0]), -EBADF)?;
        eq("kept normal fd", sys(SYS_FCNTL, [100, F_GETFD, 0, 0, 0, 0]), 0)?;
        for (sig, handler) in [(SIGUSR1, 1), (SIGUSR2, 0)] {
            let mut action = [0u64; 4];
            eq("read exec action", sys(SYS_RT_SIGACTION, [sig, 0, action.as_mut_ptr() as u64, 8, 0, 0]), 0)?;
            eq("exec disposition", action[0] as i64, handler)?;
        }
        let mut mask = 0u64;
        eq("get mask", sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, 0, &mut mask as *mut _ as u64, 8, 0, 0]), 0)?;
        eq("mask survives", mask as i64, 1 << (SIGUSR2 - 1))?;
        let mut stack = [0u64; 3];
        eq("get altstack", sys(SYS_SIGALTSTACK, [0, stack.as_mut_ptr() as u64, 0, 0, 0, 0]), 0)?;
        eq("altstack disabled", stack[1] as i64, SS_DISABLE as i64)?;
        Ok(())
    })();
    if let Err(error) = result {
        say!("FAIL exec-child: {} (got {})", error.what, error.got);
        91
    } else {
        0
    }
}

fn scripts() -> R {
    create(SCRIPT, b"#!/usr/local/bin/m8_proc exec-check\n")?;
    create(NESTED, b"#!/tmp/c2-script nested arg\n")?;
    for (path, tail) in [(SCRIPT, c"tail-abs"), (NESTED, c"tail-nested"), (c"./c2-script", c"tail-rel")] {
        let pid = fork()?;
        if pid == 0 {
            if setup_state().is_err() {
                exit(92);
            }
            if path.to_bytes().starts_with(b".") && sys(SYS_CHDIR, [c"/tmp".as_ptr() as u64, 0, 0, 0, 0, 0]) != 0 {
                exit(98);
            }
            // SAFETY: musl's environ points to the original live initial-stack strings.
            let first = unsafe { *environ };
            let env = [first as u64, c"C2=second".as_ptr() as u64, 0];
            let args = [c"discarded".as_ptr() as u64, tail.as_ptr() as u64, 0];
            let ret = sys(SYS_EXECVE, [path.as_ptr() as u64, args.as_ptr() as u64, env.as_ptr() as u64, 0, 0, 0]);
            say!("FAIL exec returned {ret}");
            exit(93);
        }
        wait(pid, 0)?;
    }
    sys(SYS_UNLINK, [SCRIPT.as_ptr() as u64, 0, 0, 0, 0, 0]);
    sys(SYS_UNLINK, [NESTED.as_ptr() as u64, 0, 0, 0, 0, 0]);
    Ok(())
}

fn refusals() -> R {
    create(SCRIPT, b"#!/tmp/c2-script\n")?;
    eq("recursive shebang", exec(SCRIPT, &[SELF.as_ptr() as u64, 0]), -ELOOP)?;
    create(SCRIPT, b"#!\0")?;
    eq("empty interpreter", exec(SCRIPT, &[SELF.as_ptr() as u64, 0]), -EACCES)?;
    create(SCRIPT, b"garbage")?;
    eq("bad ELF", exec(SCRIPT, &[SELF.as_ptr() as u64, 0]), -ENOEXEC)?;
    eq("bad path pointer", sys(SYS_EXECVE, [1, 0, 0, 0, 0, 0]), -EFAULT)?;
    eq("bad argv pointer", sys(SYS_EXECVE, [SELF.as_ptr() as u64, 1, 0, 0, 0, 0]), -EFAULT)?;
    eq("bad string pointer", exec(SELF, &[1, 0]), -EFAULT)?;
    let mut long = [b'x'; 131073];
    long[131072] = 0;
    eq("long argument", exec(SELF, &[long.as_ptr() as u64, 0]), -E2BIG)?;
    long[131071] = 0;
    let pid = fork()?;
    if pid == 0 {
        exec(SELF, &[SELF.as_ptr() as u64, c"exec-exit".as_ptr() as u64, long.as_ptr() as u64, 0]);
        exit(99);
    }
    wait(pid, 42)?;
    long[65535] = 0;
    let mut many = [long.as_ptr() as u64; 34];
    many[33] = 0;
    eq("total argument budget", exec(SELF, &many), -E2BIG)?;
    sys(SYS_UNLINK, [SCRIPT.as_ptr() as u64, 0, 0, 0, 0, 0]);
    Ok(())
}

static EXEC_ACK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
extern "C" fn exec_ack(_: c_int) {
    EXEC_ACK.store(true, SeqCst);
}

fn vfork_exec() -> R {
    // SAFETY: musl installs this C ABI handler with its signal restorer.
    let previous = unsafe { signal(SIGUSR1 as c_int, exec_ack) };
    ensure("install exec acknowledgment", previous != usize::MAX, previous as i64)?;
    let mask = 1u64 << (SIGUSR1 - 1);
    let mut old_mask = 0u64;
    let fd = ok("open parent fd", sys(SYS_OPEN, [c"/dev/null".as_ptr() as u64, O_RDONLY, 0, 0, 0, 0]))? as u64;
    eq("cloexec parent fd", sys(SYS_FCNTL, [fd, F_DUPFD_CLOEXEC, 99, 0, 0, 0]), 99)?;
    let args = [SELF.as_ptr() as u64, c"exec-hold".as_ptr() as u64, 0];
    for (nr, flags) in [(SYS_VFORK, 0u64), (SYS_CLONE, SPAWN_FLAGS | CLONE_FILES)] {
        EXEC_ACK.store(false, SeqCst);
        eq(
            "block exec acknowledgment",
            sys(SYS_RT_SIGPROCMASK, [SIG_BLOCK, &mask as *const _ as u64, &mut old_mask as *mut _ as u64, 8, 0, 0]),
            0,
        )?;
        let pid: i64;
        // SAFETY: the shared-stack child uses registers only before exec, then
        // enters its new stack or exits; both path and argv remain live in the parent.
        unsafe {
            core::arch::asm!(
            "syscall", "test rax, rax", "jnz 2f",
            "mov rdi, r8", "mov rsi, r9", "xor edx, edx", "mov eax, {exec}", "syscall",
            "mov edi, 94", "mov eax, {exit}", "syscall", "ud2", "2:",
            exec = const SYS_EXECVE, exit = const SYS_EXIT,
            inlateout("rax") nr => pid, in("rdi") flags, in("rsi") 0u64,
            in("rdx") 0u64, in("r10") 0u64, in("r8") SELF.as_ptr(), in("r9") args.as_ptr(),
            lateout("rcx") _, lateout("r11") _, options(nostack));
        }
        let pid = ok("vfork exec", pid)?;
        eq(
            "unblock exec acknowledgment",
            sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, &old_mask as *const _ as u64, 0, 8, 0, 0]),
            0,
        )?;
        while !EXEC_ACK.load(SeqCst) {
            core::hint::spin_loop();
        }
        eq("kill exec-held child", sys(SYS_KILL, [pid as u64, SIGKILL, 0, 0, 0, 0]), 0)?;
        let mut status = 0i32;
        eq("reap exec-held child", sys(SYS_WAIT4, [pid as u64, &mut status as *mut _ as u64, 0, 0, 0, 0]), pid)?;
        eq("exec-held child died by SIGKILL", i64::from(status), SIGKILL as i64)?;
        eq("parent keeps cloexec", sys(SYS_FCNTL, [99, F_GETFD, 0, 0, 0, 0]), 1)?;
    }
    let action = [previous as u64, 0, 0, 0];
    eq("restore acknowledgment action", sys(SYS_RT_SIGACTION, [SIGUSR1, action.as_ptr() as u64, 0, 8, 0, 0]), 0)?;
    sys(SYS_CLOSE, [99, 0, 0, 0, 0, 0]);
    sys(SYS_CLOSE, [fd, 0, 0, 0, 0, 0]);
    Ok(())
}

pub fn child_hold() -> c_int {
    if child_exit() != 42 {
        return 95;
    }
    if sys(SYS_KILL, [sys(SYS_GETPPID, [0; 6]) as u64, SIGUSR1, 0, 0, 0, 0]) != 0 {
        return 95;
    }
    loop {
        sys(SYS_PAUSE, [0; 6]);
    }
}

pub fn child_relative() -> c_int {
    let mut path = [0; 128];
    let n = sys(SYS_READLINK, [c"/proc/self/exe".as_ptr() as u64, path.as_mut_ptr() as u64, 128, 0, 0, 0]);
    if n == 9 && &path[..9] == b"/tmp/prog" { 42 } else { 97 }
}

pub fn child_reexec() -> c_int {
    let result = (|| -> R {
        let mut path = [0; 128];
        let n = ok(
            "readlink reexec",
            sys(
                SYS_READLINK,
                [c"/proc/self/exe".as_ptr() as u64, path.as_mut_ptr() as u64, path.len() as u64, 0, 0, 0],
            ),
        )? as usize;
        // Try the lookup even for a bad identity: the independent procfs bound
        // must return ELOOP instead of overflowing the kernel stack in the mutant.
        let opened = sys(SYS_OPEN, [c"/proc/self/exe".as_ptr() as u64, O_RDONLY, 0, 0, 0, 0]);
        if opened >= 0 {
            sys(SYS_CLOSE, [opened as u64, 0, 0, 0, 0, 0]);
        }
        ensure("reexec identity is the real binary", &path[..n] == SELF.to_bytes(), n as i64)?;
        ok("open reexec executable", opened)?;
        Ok(())
    })();
    if let Err(error) = result {
        say!("FAIL reexec-child: {} (got {})", error.what, error.got);
        97
    } else {
        42
    }
}

fn self_reexec() -> R {
    // The second fd's description itself names a magic link. Both exec
    // identities must reach the file, rather than retaining either alias.
    let fd = ok("open executable", sys(SYS_OPEN, [SELF.as_ptr() as u64, O_RDONLY, 0, 0, 0, 0]))? as u64;
    eq("executable fd 90", sys(SYS_FCNTL, [fd, F_DUPFD, 90, 0, 0, 0]), 90)?;
    let alias =
        ok("open executable alias", sys(SYS_OPEN, [c"/proc/self/fd/90".as_ptr() as u64, O_RDONLY, 0, 0, 0, 0]))? as u64;
    eq("alias fd 91", sys(SYS_FCNTL, [alias, F_DUPFD, 91, 0, 0, 0]), 91)?;
    for path in [c"/proc/self/exe", c"/proc/self/fd/90", c"/proc/self/fd/91"] {
        let pid = fork()?;
        if pid == 0 {
            exec(path, &[path.as_ptr() as u64, c"exec-reexec".as_ptr() as u64, 0]);
            exit(99);
        }
        wait(pid, 42)?;
    }
    for fd in [fd, alias, 90, 91] {
        eq("close reexec fd", sys(SYS_CLOSE, [fd, 0, 0, 0, 0, 0]), 0)?;
    }
    Ok(())
}

fn relative_and_bytes() -> R {
    copy_self(c"/tmp/prog")?;
    let pid = fork()?;
    if pid == 0 {
        if sys(SYS_CHDIR, [c"/tmp".as_ptr() as u64, 0, 0, 0, 0, 0]) != 0 {
            exit(98);
        }
        exec(c"./prog", &[c"./prog".as_ptr() as u64, c"exec-relative".as_ptr() as u64, 0]);
        exit(99);
    }
    wait(pid, 42)?;
    copy_self(c"/tmp/c2-\xffprog")?;
    create(c"/tmp/c2-\xffscript", b"#!/tmp/c2-\xffprog exec-byte\n")?;
    for path in [c"/tmp/c2-\xffprog", c"/tmp/c2-\xffscript"] {
        let pid = fork()?;
        if pid == 0 {
            exec(path, &[path.as_ptr() as u64, c"exec-byte".as_ptr() as u64, 0]);
            exit(99);
        }
        wait(pid, 42)?;
    }
    for path in [c"/tmp/c2-\xffprog", c"/tmp/c2-\xffscript"] {
        eq("unlink byte exec", sys(SYS_UNLINK, [path.as_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;
    }
    eq("unlink relative exec", sys(SYS_UNLINK, [c"/tmp/prog".as_ptr() as u64, 0, 0, 0, 0, 0]), 0)
}

fn nesting() -> R {
    let paths = [c"/tmp/c2-n0", c"/tmp/c2-n1", c"/tmp/c2-n2", c"/tmp/c2-n3", c"/tmp/c2-n4", c"/tmp/c2-n5"];
    let bodies: [&[u8]; 6] = [
        b"#!/tmp/c2-n1\n",
        b"#!/tmp/c2-n2\n",
        b"#!/tmp/c2-n3\n",
        b"#!/tmp/c2-n4\n",
        b"#!/tmp/c2-n5\n",
        b"#!/usr/local/bin/m8_proc exec-exit\n",
    ];
    for (path, body) in paths.iter().zip(bodies) {
        create(path, body)?;
    }
    let pid = fork()?;
    if pid == 0 {
        exec(paths[1], &[paths[1].as_ptr() as u64, 0]);
        exit(99);
    }
    wait(pid, 42)?;
    eq("six scripts refused", exec(paths[0], &[paths[0].as_ptr() as u64, 0]), -ELOOP)?;
    for path in paths {
        eq("unlink nesting", sys(SYS_UNLINK, [path.as_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;
    }
    Ok(())
}

pub fn child_exit() -> c_int {
    if sys(SYS_FCNTL, [99, F_GETFD, 0, 0, 0, 0]) == -EBADF { 42 } else { 95 }
}

pub(super) fn copy_self(path: &CStr) -> R {
    let src = ok("open self", sys(SYS_OPEN, [SELF.as_ptr() as u64, O_RDONLY, 0, 0, 0, 0]))? as u64;
    let dst = create_fd(path)?;
    let mut buf = [0; 4096];
    loop {
        let n = ok("read image", sys(SYS_READ, [src, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0]))?;
        if n == 0 {
            break;
        }
        eq("copy image", sys(SYS_WRITE, [dst, buf.as_ptr() as u64, n as u64, 0, 0, 0]), n)?;
    }
    sys(SYS_CLOSE, [src, 0, 0, 0, 0, 0]);
    sys(SYS_CLOSE, [dst, 0, 0, 0, 0, 0]);
    Ok(())
}

fn large() -> R {
    let path = c"/tmp/c2-large";
    copy_self(path)?;
    let dst = ok("open padded image", sys(SYS_OPEN, [path.as_ptr() as u64, O_WRONLY, 0, 0, 0, 0]))? as u64;
    eq("pad beyond cap", sys(SYS_FTRUNCATE, [dst, 17 << 20, 0, 0, 0, 0]), 0)?;
    sys(SYS_CLOSE, [dst, 0, 0, 0, 0, 0]);
    let pid = fork()?;
    if pid == 0 {
        exec(path, &[path.as_ptr() as u64, c"exec-exit".as_ptr() as u64, 0]);
        exit(96);
    }
    wait(pid, 42)?;
    eq("remove padded binary", sys(SYS_UNLINK, [path.as_ptr() as u64, 0, 0, 0, 0, 0]), 0)
}

pub fn run() -> c_int {
    common::run_checks(&[
        ("fork-cow", &cow),
        ("exec-self-identity", &self_reexec),
        ("exec-renamed-fd", &renamed_fd_exec),
        ("exec-random-fresh", &random_fresh),
        ("exec-shebang-state", &scripts),
        ("exec-refusals", &refusals),
        ("exec-relative-and-bytes", &relative_and_bytes),
        ("exec-nesting-limit", &nesting),
        ("vfork-exec-files", &vfork_exec),
        ("exec-over-16m", &large),
    ])
}

pub fn child_byte() -> c_int {
    let mut path = [0; 128];
    let n = sys(SYS_READLINK, [c"/proc/self/exe".as_ptr() as u64, path.as_mut_ptr() as u64, 128, 0, 0, 0]);
    let expected = b"/tmp/c2-\xffprog";
    if n != expected.len() as i64 || &path[..expected.len()] != expected {
        say!("FAIL raw executable identity");
        return 97;
    }
    let fd = sys(SYS_OPEN, [c"/proc/self/exe".as_ptr() as u64, O_RDONLY, 0, 0, 0, 0]);
    if fd < 0 {
        return 97;
    }
    sys(SYS_CLOSE, [fd as u64, 0, 0, 0, 0, 0]);
    42
}

pub fn child_unnamed() -> c_int {
    let mut path = [0; 128];
    let link = sys(SYS_READLINK, [c"/proc/self/exe".as_ptr() as u64, path.as_mut_ptr() as u64, 128, 0, 0, 0]);
    let fd = sys(SYS_OPEN, [c"/proc/self/exe".as_ptr() as u64, O_RDONLY, 0, 0, 0, 0]);
    if link == -ENOENT && fd == -ENOENT { 42 } else { 97 }
}

fn renamed_fd_exec() -> R {
    copy_self(c"/tmp/c2-before")?;
    let fd = ok("open before rename", sys(SYS_OPEN, [c"/tmp/c2-before".as_ptr() as u64, O_RDONLY, 0, 0, 0, 0]))? as u64;
    eq("renamed executable fd", sys(SYS_FCNTL, [fd, F_DUPFD, 90, 0, 0, 0]), 90)?;
    eq(
        "rename executable",
        sys(SYS_RENAME, [c"/tmp/c2-before".as_ptr() as u64, c"/tmp/c2-after".as_ptr() as u64, 0, 0, 0, 0]),
        0,
    )?;
    let pid = fork()?;
    if pid == 0 {
        exec(c"/proc/self/fd/90", &[SELF.as_ptr() as u64, c"exec-unnamed".as_ptr() as u64, 0]);
        exit(99);
    }
    wait(pid, 42)?;
    for fd in [fd, 90] {
        eq("close renamed fd", sys(SYS_CLOSE, [fd, 0, 0, 0, 0, 0]), 0)?;
    }
    eq("unlink renamed executable", sys(SYS_UNLINK, [c"/tmp/c2-after".as_ptr() as u64, 0, 0, 0, 0, 0]), 0)
}

pub fn child_random() -> c_int {
    // SAFETY: musl reads the kernel's auxiliary vector; AT_RANDOM names 16 live bytes on this process's stack.
    let random = unsafe { getauxval(AT_RANDOM) };
    if random != 0 && sys(SYS_WRITE, [98, random as u64, 16, 0, 0, 0]) == 16 { 42 } else { 97 }
}

fn random_fresh() -> R {
    let path = c"/tmp/c2-random";
    let fd =
        ok("create random samples", sys(SYS_OPEN, [path.as_ptr() as u64, O_RDWR | O_CREAT | O_TRUNC, 0o600, 0, 0, 0]))?
            as u64;
    eq("random sample fd", sys(SYS_FCNTL, [fd, F_DUPFD, 98, 0, 0, 0]), 98)?;
    for _ in 0..2 {
        let pid = fork()?;
        if pid == 0 {
            exec(SELF, &[SELF.as_ptr() as u64, c"exec-random".as_ptr() as u64, 0]);
            exit(99);
        }
        wait(pid, 42)?;
    }
    eq("rewind samples", sys(SYS_LSEEK, [fd, 0, 0, 0, 0, 0]), 0)?;
    let mut samples = [0; 32];
    eq("read random samples", sys(SYS_READ, [fd, samples.as_mut_ptr() as u64, 32, 0, 0, 0]), 32)?;
    ensure("AT_RANDOM differs between exec children", samples[..16] != samples[16..], 0)?;
    for fd in [fd, 98] {
        eq("close samples", sys(SYS_CLOSE, [fd, 0, 0, 0, 0, 0]), 0)?;
    }
    eq("unlink samples", sys(SYS_UNLINK, [path.as_ptr() as u64, 0, 0, 0, 0, 0]), 0)
}
