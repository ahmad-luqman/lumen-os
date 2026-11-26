//! C3: musl pthreads and exact clone/exec/exit ABI checks.
use crate::common::{R, ensure};
use crate::*;
use core::ffi::{c_int, c_void};
use core::sync::atomic::Ordering::SeqCst;

// SAFETY: these declarations match musl's x86-64 C ABI.
unsafe extern "C" {
    fn pthread_create(
        thread: *mut usize,
        attr: *const c_void,
        run: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    fn pthread_join(thread: usize, result: *mut *mut c_void) -> c_int;
    fn __errno_location() -> *mut c_int;
}
const THREAD: u64 = CLONE_VM | CLONE_FS | CLONE_FILES | CLONE_SIGHAND | CLONE_THREAD;
static RELEASE: AtomicU32 = AtomicU32::new(0);
static READY: AtomicU32 = AtomicU32::new(0);
static TIDS: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];

fn create(run: extern "C" fn(*mut c_void) -> *mut c_void, arg: usize) -> Result<usize, common::Fail> {
    let mut thread = 0;
    // SAFETY: output is writable; callbacks interpret arg as an integer or a
    // live shared fixture, retained until join or process exit.
    let result = unsafe { pthread_create(&mut thread, core::ptr::null(), run, arg as *mut c_void) };
    eq("pthread_create", result as i64, 0)?;
    Ok(thread)
}
fn join(thread: usize, expected: usize) -> R {
    let mut result = core::ptr::null_mut();
    // SAFETY: thread is a successfully created, joinable, unjoined pthread.
    eq("pthread_join", unsafe { pthread_join(thread, &mut result) } as i64, 0)?;
    eq("pthread result", result as usize as i64, expected as i64)
}
fn nap() {
    sys(SYS_POLL, [0, 0, 1, 0, 0, 0]);
}
fn await_release() {
    while RELEASE.load(SeqCst) == 0 {
        sys(SYS_FUTEX, [&RELEASE as *const _ as u64, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0]);
    }
}
fn release() {
    RELEASE.store(1, SeqCst);
    sys(SYS_FUTEX, [&RELEASE as *const _ as u64, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, i32::MAX as u64, 0, 0, 0]);
}

extern "C" fn pthread_probe(arg: *mut c_void) -> *mut c_void {
    let mut mask = 0u64;
    let ret = sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, 0, &mut mask as *mut _ as u64, 8, 0, 0]);
    let tid = sys(SYS_GETTID, [0; 6]);
    let pid = sys(SYS_GETPID, [0; 6]);
    // SAFETY: musl returns this thread's live TLS errno word.
    unsafe {
        *__errno_location() = 99;
    }
    if ret != 0 || mask & (1 << (SIGUSR1 - 1)) == 0 || tid == pid {
        return 99usize as *mut c_void;
    }
    arg
}
fn pthreads() -> R {
    let mask = 1u64 << (SIGUSR1 - 1);
    let mut old = 0u64;
    eq(
        "block before clone",
        sys(SYS_RT_SIGPROCMASK, [SIG_BLOCK, &mask as *const _ as u64, &mut old as *mut _ as u64, 8, 0, 0]),
        0,
    )?;
    // SAFETY: musl returns this thread's live TLS errno word.
    unsafe {
        *__errno_location() = 77;
    }
    let thread = create(pthread_probe, 0x1234)?;
    join(thread, 0x1234)?;
    // SAFETY: parent TLS is still live after joining the child.
    eq("separate TLS", unsafe { *__errno_location() } as i64, 77)?;
    eq("restore mask", sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, &old as *const _ as u64, 0, 8, 0, 0]), 0)
}

struct Raw {
    tid: AtomicU32,
    observed: AtomicU32,
    ready: AtomicU32,
    release: AtomicU32,
}

/// The raw syscall ABI: musl's public clone wrapper rejects CLONE_THREAD
/// and CHILD_CLEARTID before entering the kernel.
///
/// # Safety
/// `stack` ends a live, disjoint writable stack, and `arg` and `tid` remain
/// valid until the child's clear_child_tid wake. The callback never returns.
#[unsafe(naked)]
unsafe extern "C" fn clone_thread(stack: u64, flags: u64, arg: *const Raw, tid: *mut u32) -> i64 {
    core::arch::naked_asm!(
        "and rdi, -16",
        "sub rdi, 16",
        "mov [rdi], rdx",
        "mov r10, rcx",
        "mov rdx, rcx",
        "xchg rdi, rsi",
        "xor r8d, r8d",
        "mov eax, 56",
        "syscall",
        "test rax, rax",
        "jnz 2f",
        "mov rdi, [rsp]",
        "call {run}",
        "ud2",
        "2: ret",
        run = sym raw_probe,
    );
}

extern "C" fn raw_probe(arg: *mut c_void) -> c_int {
    // SAFETY: raw_clone keeps this shared fixture alive through clear_child_tid.
    let state = unsafe { &*(arg as *const Raw) };
    // The child's first user action must see PARENT_SETTID, before any syscall.
    let first = state.tid.load(SeqCst);
    let tid = sys(SYS_GETTID, [0; 6]) as u32;
    state.observed.store(if first == tid { tid } else { u32::MAX }, SeqCst);
    state.ready.store(1, SeqCst);
    while state.release.load(SeqCst) == 0 {
        nap();
    }
    exit(0)
}
fn raw_clone() -> R {
    for exit_signal in [0, 9, 255] {
        let stack = ok("thread stack", sys(SYS_MMAP, [0, 65536, 3, 0x22, u64::MAX, 0]))? as u64;
        let state = Raw {
            tid: AtomicU32::new(0),
            observed: AtomicU32::new(0),
            ready: AtomicU32::new(0),
            release: AtomicU32::new(0),
        };
        let word = &state.tid as *const _ as *mut u32;
        // SAFETY: disjoint writable stack and atomic fixture live through the
        // child's final clear/wake. Its callback uses no shared libc TLS.
        let tid = unsafe {
            clone_thread(stack + 65536, THREAD | CLONE_PARENT_SETTID | CLONE_CHILD_CLEARTID | exit_signal, &state, word)
        };
        ok("thread clone ignores exit signal", tid)?;
        while state.ready.load(SeqCst) == 0 {
            nap();
        }
        state.release.store(1, SeqCst);
        while state.tid.load(SeqCst) != 0 {
            let r = sys(SYS_FUTEX, [word as u64, FUTEX_WAIT, tid as u64, 0, 0, 0]);
            ensure("clear_child_tid wake", r == 0 || r == -EAGAIN, r)?;
        }
        eq("parent tid before child entry", state.observed.load(SeqCst) as i64, tid)?;
        eq("thread is not a wait child", sys(SYS_WAIT4, [tid as u64, 0, WNOHANG, 0, 0, 0]), -ECHILD)?;
        while sys(SYS_TKILL, [tid as u64, 0, 0, 0, 0, 0]) == 0 {
            nap();
        }
        eq("free stack", sys(SYS_MUNMAP, [stack, 65536, 0, 0, 0, 0]), 0)?;
    }
    Ok(())
}
fn flags() -> R {
    for invalid in [
        THREAD & !CLONE_FS,
        THREAD & !CLONE_FILES,
        THREAD & !CLONE_VM,
        THREAD & !CLONE_SIGHAND,
        THREAD | CLONE_PARENT,
        CLONE_VM | CLONE_SIGHAND | SIGCHLD,
        0,
        9,
    ] {
        eq("clone EINVAL before TLS", sys(SYS_CLONE, [invalid | CLONE_SETTLS, 0, 0, 0, 1 << 47, 0]), -EINVAL)?;
    }
    for tls in [1 << 47, u64::MAX] {
        eq("thread TLS range", sys(SYS_CLONE, [CLONE_PTHREAD, 0, 0, 0, tls, 0]), -EPERM)?;
    }
    clone_shared_resources()
}

fn decimal(mut n: u32) -> [u8; 12] {
    let mut out = [0; 12];
    let mut reverse = [0; 10];
    let mut count = 0;
    loop {
        reverse[count] = b'0' + (n % 10) as u8;
        count += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    for i in 0..count {
        out[i] = reverse[count - i - 1];
    }
    out
}
extern "C" fn executor(arg: *mut c_void) -> *mut c_void {
    let i = arg as usize;
    TIDS[i].store(sys(SYS_GETTID, [0; 6]) as u32, SeqCst);
    READY.fetch_add(1, SeqCst);
    await_release();
    let a = decimal(TIDS[0].load(SeqCst));
    let b = decimal(TIDS[1].load(SeqCst));
    let args = [c"m8_proc".as_ptr() as u64, c"thread-image".as_ptr() as u64, a.as_ptr() as u64, b.as_ptr() as u64, 0];
    let result = sys(SYS_EXECVE, [c"/usr/local/bin/m8_proc".as_ptr() as u64, args.as_ptr() as u64, 0, 0, 0, 0]);
    // The losing exec may be refused before the winner zaps it. It must
    // remain available for that kill instead of imposing a new group exit.
    if result == -EAGAIN {
        loop {
            sys(SYS_PAUSE, [0; 6]);
        }
    }
    exit(98)
}
fn exec_threads() -> R {
    for count in [1, 2] {
        let pid = fork()?;
        if pid == 0 {
            RELEASE.store(0, SeqCst);
            READY.store(0, SeqCst);
            for t in &TIDS {
                t.store(0, SeqCst);
            }
            for i in 0..count {
                if create(executor, i).is_err() {
                    exit(97);
                }
            }
            while READY.load(SeqCst) != count as u32 {
                nap();
            }
            release();
            loop {
                sys(SYS_PAUSE, [0; 6]);
            }
        }
        wait(pid, 42)?;
    }
    Ok(())
}
pub fn image(args: &[&[u8]]) -> c_int {
    let pid = sys(SYS_GETPID, [0; 6]);
    if sys(SYS_GETTID, [0; 6]) != pid {
        return 97;
    }
    for arg in args {
        let tid = arg.iter().fold(0u64, |v, b| v * 10 + u64::from(b - b'0'));
        if tid != 0 && sys(SYS_TGKILL, [pid as u64, tid, 0, 0, 0, 0]) != -ESRCH {
            return 98;
        }
    }
    say!("ok thread-exec-image");
    42
}
extern "C" fn exiter(arg: *mut c_void) -> *mut c_void {
    READY.store(1, SeqCst);
    await_release();
    if arg as usize == 1 {
        sys(SYS_EXIT_GROUP, [37, 0, 0, 0, 0, 0]);
    } else {
        let pid = sys(SYS_GETPID, [0; 6]);
        while sys(SYS_TKILL, [pid as u64, 0, 0, 0, 0, 0]) == 0 {
            nap();
        }
        if arg as usize == 2 {
            say!("ok root-leader-exited");
        }
        exit(89);
    }
    core::ptr::null_mut()
}
fn exits() -> R {
    for group in [false, true] {
        let pid = fork()?;
        if pid == 0 {
            RELEASE.store(0, SeqCst);
            READY.store(0, SeqCst);
            if create(exiter, usize::from(group)).is_err() {
                exit(97);
            }
            while READY.load(SeqCst) == 0 {
                nap();
            }
            release();
            if !group {
                exit(23);
            }
            loop {
                sys(SYS_PAUSE, [0; 6]);
            }
        }
        wait(pid, if group { 37 } else { 23 })?;
    }
    Ok(())
}

pub fn root(exec: bool) -> c_int {
    RELEASE.store(0, SeqCst);
    READY.store(0, SeqCst);
    let thread = if exec { create(executor, 0) } else { create(exiter, 2) };
    if thread.is_err() {
        return 97;
    }
    while READY.load(SeqCst) != 1 {
        nap();
    }
    release();
    if !exec {
        exit(23);
    }
    loop {
        sys(SYS_PAUSE, [0; 6]);
    }
}
extern "C" fn holder(_: *mut c_void) -> *mut c_void {
    READY.fetch_add(1, SeqCst);
    await_release();
    core::ptr::null_mut()
}
pub fn exhaust() -> c_int {
    common::run_checks(&[("thread-stack-exhaustion", &|| {
        RELEASE.store(0, SeqCst);
        READY.store(0, SeqCst);
        let mut threads = [0usize; 512];
        let mut count = 0;
        let mut error = 0;
        while count < threads.len() {
            // SAFETY: writable output and a static callback with no argument.
            error = unsafe { pthread_create(&mut threads[count], core::ptr::null(), holder, core::ptr::null_mut()) };
            if error != 0 {
                break;
            }
            count += 1;
        }
        while READY.load(SeqCst) != count as u32 {
            nap();
        }
        say!("threads: exhausted count={count} errno={error} ready");
        // An external shell runs a kernel worker before releasing us by byte.
        let mut byte = [0];
        let read = sys(SYS_READ, [0, byte.as_mut_ptr() as u64, 1, 0, 0, 0]);
        release();
        for thread in &threads[..count] {
            join(*thread, 0)?;
        }
        eq("exhaustion control byte", read, 1)?;
        eq("stack cap errno", error as i64, EAGAIN)?;
        ensure("512-slot registry exercised", count > 128 && count < 480, count as i64)
    })])
}
pub fn exec_checks() -> c_int {
    common::run_checks(&[("thread-exec", &exec_threads)])
}

pub fn run() -> c_int {
    common::run_checks(&[
        ("thread-clone-flags", &flags),
        ("thread-parent-tid-and-clear", &raw_clone),
        ("pthread-create-join-mask-tls", &pthreads),
        ("thread-exit-status", &exits),
    ])
}

// ---------------------------------------------------------------------------
// C11: Multithreaded workload pinned across every online CPU
// ---------------------------------------------------------------------------

const SYS_SCHED_YIELD: u64 = 24;
const SYS_SCHED_SETAFFINITY: u64 = 203;
const SYS_SCHED_GETAFFINITY: u64 = 204;
const SYS_GETCPU: u64 = 309;
const MAX_SMP_CPUS: usize = 8;
const HALF_WORK: u32 = 20_000;

struct SmpShared {
    n_cpus: u32,
    ready: AtomicU32,
    midpoint: AtomicU32,
    errors: AtomicU32,
    migrated: AtomicU32,
    progress: [AtomicU32; MAX_SMP_CPUS],
    checksum: [core::sync::atomic::AtomicU64; MAX_SMP_CPUS],
}

struct SmpWorkerArg {
    idx: usize,
    cpu: u32,
    shared: *const SmpShared,
}

fn pin_and_confirm(cpu: u32) -> R {
    let mask = 1u64 << cpu;
    eq("sched_setaffinity", sys(SYS_SCHED_SETAFFINITY, [0, 8, &mask as *const _ as u64, 0, 0, 0]), 0)?;
    loop {
        eq("sched_yield", sys(SYS_SCHED_YIELD, [0; 6]), 0)?;
        let mut here = u32::MAX;
        let mut node = u32::MAX;
        eq("getcpu", sys(SYS_GETCPU, [&mut here as *mut _ as u64, &mut node as *mut _ as u64, 0, 0, 0, 0]), 0)?;
        eq("getcpu node zero", i64::from(node), 0)?;
        if here == cpu {
            return Ok(());
        }
    }
}

fn expected_checksum(cpu: u32) -> u64 {
    let mut acc = 0xcbf2_9ce4_8422_2325u64 ^ u64::from(cpu);
    for i in 0..(2 * HALF_WORK) {
        acc = acc.wrapping_mul(0x0000_0100_0000_01b3) ^ u64::from(i);
    }
    acc
}

extern "C" fn smp_worker(arg: *mut c_void) -> *mut c_void {
    // SAFETY: parent retains `SmpWorkerArg` and `SmpShared` until `pthread_join`.
    let w = unsafe { &*(arg as *const SmpWorkerArg) };
    // SAFETY: `w.shared` points to the live stack-allocated `SmpShared`.
    let s = unsafe { &*w.shared };
    if pin_and_confirm(w.cpu).is_err() {
        s.errors.fetch_add(1, SeqCst);
        s.ready.fetch_add(1, SeqCst);
        s.midpoint.fetch_add(1, SeqCst);
        return core::ptr::null_mut();
    }
    s.ready.fetch_add(1, SeqCst);
    while s.ready.load(SeqCst) < s.n_cpus {
        sys(SYS_SCHED_YIELD, [0; 6]);
    }

    let mut acc = 0xcbf2_9ce4_8422_2325u64 ^ u64::from(w.cpu);
    for phase in 0..2u32 {
        let base = phase * HALF_WORK;
        for step in 0..HALF_WORK {
            acc = acc.wrapping_mul(0x0000_0100_0000_01b3) ^ u64::from(base + step);
            if step % 2000 == 0 {
                let mut here = u32::MAX;
                let mut node = u32::MAX;
                let rc = sys(SYS_GETCPU, [&mut here as *mut _ as u64, &mut node as *mut _ as u64, 0, 0, 0, 0]);
                if rc != 0 || node != 0 || here != w.cpu {
                    s.migrated.fetch_add(1, SeqCst);
                }
            }
        }
        s.progress[w.idx].fetch_add(HALF_WORK, SeqCst);
        if phase == 0 {
            s.midpoint.fetch_add(1, SeqCst);
            while s.midpoint.load(SeqCst) < s.n_cpus {
                sys(SYS_SCHED_YIELD, [0; 6]);
            }
        }
    }
    s.checksum[w.idx].store(acc, SeqCst);
    core::ptr::null_mut()
}

fn smp_all_cpus() -> R {
    let mut mask = 0u64;
    eq("sched_getaffinity", sys(SYS_SCHED_GETAFFINITY, [0, 8, &mut mask as *mut _ as u64, 0, 0, 0]), 8)?;
    let mut cpus = [0u32; MAX_SMP_CPUS];
    let mut count = 0usize;
    for cpu in 0..64u32 {
        if (mask & (1u64 << cpu)) != 0 && count < MAX_SMP_CPUS {
            cpus[count] = cpu;
            count += 1;
        }
    }
    ensure("smp workload requires at least 4 online CPUs", count >= 4, count as i64)?;

    let shared = SmpShared {
        n_cpus: count as u32,
        ready: AtomicU32::new(0),
        midpoint: AtomicU32::new(0),
        errors: AtomicU32::new(0),
        migrated: AtomicU32::new(0),
        progress: [const { AtomicU32::new(0) }; MAX_SMP_CPUS],
        checksum: [const { core::sync::atomic::AtomicU64::new(0) }; MAX_SMP_CPUS],
    };
    let mut args = [const { SmpWorkerArg { idx: 0, cpu: 0, shared: core::ptr::null() } }; MAX_SMP_CPUS];
    let mut tids = [0usize; MAX_SMP_CPUS];
    for i in 0..count {
        args[i] = SmpWorkerArg { idx: i, cpu: cpus[i], shared: &shared };
        tids[i] = create(smp_worker, &args[i] as *const SmpWorkerArg as usize)?;
    }
    for tid in &tids[..count] {
        join(*tid, 0)?;
    }
    eq("smp worker pin errors", i64::from(shared.errors.load(SeqCst)), 0)?;
    eq("smp pinned thread migrations", i64::from(shared.migrated.load(SeqCst)), 0)?;
    for i in 0..count {
        eq("smp cpu progress", i64::from(shared.progress[i].load(SeqCst)), i64::from(2 * HALF_WORK))?;
        eq("smp cpu checksum", shared.checksum[i].load(SeqCst) as i64, expected_checksum(cpus[i]) as i64)?;
    }
    say!("smp: cpus={count}");
    Ok(())
}

pub fn smp_workload() -> c_int {
    common::run_checks(&[("smp-all-cpus", &smp_all_cpus)])
}
