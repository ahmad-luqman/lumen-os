//! C4 raw futex/sleep ABI checks and musl's process-shared robust mutex.
use crate::common::{self, R, ensure, eq, ok, sys};
use crate::*;
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicI64, AtomicU32, Ordering::SeqCst};

const NS: u64 = 1_000_000_000;
const INTERVAL: u64 = 30_000_000;

#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}
impl Timespec {
    fn of(ns: u64) -> Self {
        Self { sec: (ns / NS) as i64, nsec: (ns % NS) as i64 }
    }
    fn ptr(&self) -> u64 {
        self as *const Self as u64
    }
}
fn now(clock: u64) -> Result<u64, common::Fail> {
    let mut ts = Timespec::of(0);
    eq("clock_gettime", sys(SYS_CLOCK_GETTIME, [clock, &mut ts as *mut _ as u64, 0, 0, 0, 0]), 0)?;
    Ok(ts.sec as u64 * NS + ts.nsec as u64)
}
fn nap() {
    sys(SYS_POLL, [0, 0, 1, 0, 0, 0]);
}
fn fx(addr: u64, op: u64, val: u64, timeout: u64, dest: u64, bits: u64) -> i64 {
    sys(SYS_FUTEX, [addr, op, val, timeout, dest, bits])
}
fn child_exit(result: R) -> ! {
    let code = match result {
        Ok(()) => 0,
        Err(e) => {
            say!("FAIL child: {} (got {})", e.what, e.got);
            99
        }
    };
    sys(SYS_EXIT, [code, 0, 0, 0, 0, 0]);
    panic!("exit returned")
}

struct Mapping {
    addr: u64,
    len: u64,
}
impl Mapping {
    fn new(len: u64, flags: u64, fd: u64) -> Result<Self, common::Fail> {
        Ok(Self { addr: ok("mmap", sys(SYS_MMAP, [0, len, PROT_READ | PROT_WRITE, flags, fd, 0]))? as u64, len })
    }
    fn anon(shared: bool) -> Result<Self, common::Fail> {
        Self::new(4096, MAP_ANONYMOUS | if shared { MAP_SHARED } else { MAP_PRIVATE }, u64::MAX)
    }
    fn word(&self, offset: u64) -> &AtomicU32 {
        assert!(offset.is_multiple_of(4) && offset + 4 <= self.len);
        // SAFETY: the live mmap is aligned and read/write; callers use atomic words.
        unsafe { &*((self.addr + offset) as *const AtomicU32) }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        sys(SYS_MUNMAP, [self.addr, self.len, 0, 0, 0, 0]);
    }
}

struct Child(i64);
impl Child {
    fn join(mut self) -> R {
        let pid = core::mem::replace(&mut self.0, 0);
        let mut status = -1i32;
        eq("wait child", sys(SYS_WAIT4, [pid as u64, &mut status as *mut _ as u64, 0, 0, 0, 0]), pid)?;
        eq("child succeeded", status as i64, 0)
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if self.0 > 0 {
            sys(SYS_KILL, [self.0 as u64, 9, 0, 0, 0, 0]);
            sys(SYS_WAIT4, [self.0 as u64, 0, 0, 0, 0, 0]);
        }
    }
}

struct WaitArgs {
    addr: u64,
    op: u64,
    value: u64,
    bits: u64,
    result: AtomicI64,
}
// SAFETY: musl's clone C ABI, callback and stack retained until wait4 finishes.
unsafe extern "C" {
    fn fork() -> c_int;
    fn clone(f: extern "C" fn(*mut c_void) -> c_int, stack: *mut c_void, flags: c_int, arg: *mut c_void, ...) -> c_int;
    fn pthread_mutexattr_init(attr: *mut u32) -> c_int;
    fn pthread_mutexattr_setpshared(attr: *mut u32, shared: c_int) -> c_int;
    fn pthread_mutexattr_setrobust(attr: *mut u32, robust: c_int) -> c_int;
    fn pthread_mutex_init(mutex: *mut c_void, attr: *const u32) -> c_int;
    fn pthread_mutex_lock(mutex: *mut c_void) -> c_int;
    fn pthread_mutex_consistent(mutex: *mut c_void) -> c_int;
    fn pthread_mutex_unlock(mutex: *mut c_void) -> c_int;
    fn pthread_mutex_destroy(mutex: *mut c_void) -> c_int;
    fn pthread_cond_init(cond: *mut c_void, attr: *const c_void) -> c_int;
    fn pthread_cond_wait(cond: *mut c_void, mutex: *mut c_void) -> c_int;
    fn pthread_cond_signal(cond: *mut c_void) -> c_int;
    fn pthread_cond_broadcast(cond: *mut c_void) -> c_int;
    fn pthread_cond_destroy(cond: *mut c_void) -> c_int;
    fn pthread_create(
        thread: *mut usize,
        attr: *const c_void,
        run: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    fn pthread_join(thread: usize, result: *mut *mut c_void) -> c_int;
}
extern "C" fn waiter(arg: *mut c_void) -> c_int {
    // SAFETY: spawn passes a live WaitArgs retained until this child is reaped.
    let args = unsafe { &*(arg as *const WaitArgs) };
    let result = fx(args.addr, args.op, args.value, 0, 0, args.bits);
    args.result.store(result, SeqCst);
    sys(SYS_EXIT, [u64::from(result != 0), 0, 0, 0, 0, 0]);
    99
}
fn spawn(args: &WaitArgs, stack: &Mapping) -> Result<Child, common::Fail> {
    // SAFETY: disjoint mapped stack and shared arguments live until join;
    // callback uses raw syscalls and atomics, never shared libc TLS state.
    let pid = unsafe {
        clone(
            waiter,
            (stack.addr + stack.len) as *mut c_void,
            (CLONE_VM | SIGCHLD) as c_int,
            args as *const _ as *mut c_void,
        )
    };
    Ok(Child(ok("CLONE_VM waiter", pid as i64)?))
}
fn wake_until(addr: u64, op: u64, bits: u64) -> R {
    loop {
        let count = fx(addr, op, 1, 0, 0, bits);
        if count == 1 {
            return Ok(());
        }
        eq("wake before waiter queued", count, 0)?;
        nap();
    }
}
fn args(addr: u64, op: u64, bits: u64) -> WaitArgs {
    WaitArgs { addr, op, value: 0, bits, result: AtomicI64::new(999) }
}

fn errors_and_demand() -> R {
    let map = Mapping::anon(false)?;
    let relative = Timespec::of(0);
    eq("untouched private WAIT mismatch", fx(map.addr, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 1, 0, 0, 0), -EAGAIN)?;
    let second = Mapping::anon(false)?;
    eq("untouched shared WAIT mismatch", fx(second.addr, FUTEX_WAIT, 1, 0, 0, 0), -EAGAIN)?;
    let third = Mapping::anon(false)?;
    eq("CMP_REQUEUE faults in source", fx(third.addr, FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG, 0, 0, map.addr, 0), 0)?;
    eq("CMP_REQUEUE mismatch", fx(third.addr, FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG, 0, 0, map.addr, 1), -EAGAIN)?;
    eq("plain WAIT realtime", fx(map.addr, FUTEX_WAIT | FUTEX_CLOCK_REALTIME, 0, relative.ptr(), 0, 0), -ENOSYS)?;
    for command in [FUTEX_WAIT, FUTEX_WAIT_BITSET] {
        eq("realtime wait timeout fault", fx(map.addr, command | FUTEX_CLOCK_REALTIME, 0, u64::MAX, 0, 1), -EFAULT)?;
    }
    eq("misaligned", fx(map.addr + 1, FUTEX_WAKE, 1, 0, 0, 0), -EINVAL)?;
    eq("alignment precedes user range", fx(u64::MAX, FUTEX_WAKE, 1, 0, 0, 0), -EINVAL)?;
    eq("outside user", fx(u64::MAX - 3, FUTEX_WAKE, 1, 0, 0, 0), -EFAULT)?;
    eq("unmapped shared", fx(0x4000, FUTEX_WAKE, 1, 0, 0, 0), -EFAULT)?;
    eq("zero wait bitset", fx(map.addr, FUTEX_WAIT_BITSET, 0, 0, 0, 0), -EINVAL)?;
    eq("timeout copy precedes zero bitset", fx(map.addr, FUTEX_WAIT_BITSET, 0, u64::MAX, 0, 0), -EFAULT)?;
    eq("zero wake bitset", fx(map.addr, FUTEX_WAKE_BITSET, 1, 0, 0, 0), -EINVAL)?;
    for bad in [Timespec { sec: -1, nsec: 0 }, Timespec { sec: 0, nsec: -1 }, Timespec { sec: 0, nsec: NS as i64 }] {
        eq("bad futex timeout", fx(map.addr, FUTEX_WAIT, 0, bad.ptr(), 0, 0), -EINVAL)?;
        for command in [FUTEX_WAIT, FUTEX_WAIT_BITSET] {
            eq(
                "realtime wait timeout invalid",
                fx(map.addr, command | FUTEX_CLOCK_REALTIME, 0, bad.ptr(), 0, 1),
                -EINVAL,
            )?;
        }
        eq("bad nanosleep", sys(SYS_NANOSLEEP, [bad.ptr(), 0, 0, 0, 0, 0]), -EINVAL)?;
    }
    for op in [FUTEX_REQUEUE, FUTEX_CMP_REQUEUE] {
        eq("requeue high count bits ignored", fx(0x4000, op, 1, 1 << 32, 0x8000, 0), -EFAULT)?;
        eq("negative wake precedes key fault", fx(0x4000, op, u32::MAX as u64, 1, 0x8000, 0), -EINVAL)?;
        eq("negative requeue precedes key fault", fx(0x4000, op, 1, u32::MAX as u64, 0x8000, 0), -EINVAL)?;
    }
    let inaccessible = Mapping::anon(false)?;
    eq("protect futex page", sys(SYS_MPROTECT, [inaccessible.addr, 4096, 0, 0, 0, 0]), 0)?;
    for addr in [0x4000, inaccessible.addr] {
        eq("private WAIT inaccessible returns EFAULT", fx(addr, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0), -EFAULT)?;
        eq(
            "private CMP_REQUEUE inaccessible returns EFAULT",
            fx(addr, FUTEX_CMP_REQUEUE | FUTEX_PRIVATE_FLAG, 0, 0, map.addr, 0),
            -EFAULT,
        )?;
    }
    for op in [2, 5, 6, 7, 8, 11, 12, 13, 0x4000] {
        eq("unsupported futex op", fx(map.addr, op, 0, 0, 0, 0), -ENOSYS)?;
    }
    eq("expired futex", fx(map.addr, FUTEX_WAIT, 0, relative.ptr(), 0, 0), -ETIMEDOUT)?;
    eq("futex_waitv", sys(SYS_FUTEX_WAITV, [0; 6]), -ENOSYS)
}

fn sleep_floors() -> R {
    let map = Mapping::anon(false)?;
    let req = Timespec::of(INTERVAL);
    let before = now(CLOCK_MONOTONIC)?;
    eq("relative futex timeout", fx(map.addr, FUTEX_WAIT, 0, req.ptr(), 0, 0), -ETIMEDOUT)?;
    ensure("futex timeout floor", now(CLOCK_MONOTONIC)? - before >= INTERVAL, 0)?;
    for clock in [CLOCK_MONOTONIC, CLOCK_REALTIME] {
        let until = now(clock)? + INTERVAL;
        let req = Timespec::of(until);
        eq(
            "absolute futex timeout",
            fx(
                map.addr,
                FUTEX_WAIT_BITSET | if clock == CLOCK_REALTIME { FUTEX_CLOCK_REALTIME } else { 0 },
                0,
                req.ptr(),
                0,
                1,
            ),
            -ETIMEDOUT,
        )?;
        ensure("absolute futex floor", now(clock)? >= until, 0)?;
    }
    let before = now(CLOCK_MONOTONIC)?;
    eq("nanosleep", sys(SYS_NANOSLEEP, [req.ptr(), 0, 0, 0, 0, 0]), 0)?;
    ensure("nanosleep floor", now(CLOCK_MONOTONIC)? - before >= INTERVAL, 0)?;
    for clock in [CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_BOOTTIME] {
        let before = now(clock)?;
        eq("clock_nanosleep relative", sys(SYS_CLOCK_NANOSLEEP, [clock, 0, req.ptr(), 0, 0, 0]), 0)?;
        ensure("relative sleep floor", now(clock)? - before >= INTERVAL, 0)?;
        let until = now(clock)? + INTERVAL;
        let absolute = Timespec::of(until);
        eq(
            "absolute sleep ignores bad rem",
            sys(SYS_CLOCK_NANOSLEEP, [clock, TIMER_ABSTIME, absolute.ptr(), u64::MAX, 0, 0]),
            0,
        )?;
        ensure("absolute sleep floor", now(clock)? >= until, 0)?;
    }
    for clock in [CLOCK_THREAD_CPUTIME_ID, CLOCK_MONOTONIC_RAW, CLOCK_REALTIME_COARSE, CLOCK_MONOTONIC_COARSE] {
        eq("known clock cannot sleep", sys(SYS_CLOCK_NANOSLEEP, [clock, 0, req.ptr(), 0, 0, 0]), -EOPNOTSUPP)?;
    }
    for clock in [CLOCK_PROCESS_CPUTIME_ID, CLOCK_TAI, CLOCK_REALTIME_ALARM, CLOCK_BOOTTIME_ALARM, 99] {
        eq("unsupported or invalid clock", sys(SYS_CLOCK_NANOSLEEP, [clock, 0, req.ptr(), 0, 0, 0]), -EINVAL)?;
    }
    Ok(())
}

fn wakes_requeues_bitsets() -> R {
    let map = Mapping::anon(false)?;
    let stacks = [
        Mapping::new(65536, MAP_PRIVATE | MAP_ANONYMOUS, u64::MAX)?,
        Mapping::new(65536, MAP_PRIVATE | MAP_ANONYMOUS, u64::MAX)?,
    ];
    for private in [0, FUTEX_PRIVATE_FLAG] {
        for command in [FUTEX_REQUEUE, FUTEX_CMP_REQUEUE] {
            let a = args(map.addr, FUTEX_WAIT_BITSET | private, 1);
            let b = args(map.addr, FUTEX_WAIT_BITSET | private, 2);
            let c1 = spawn(&a, &stacks[0])?;
            let c2 = spawn(&b, &stacks[1])?;
            let dest = map.addr + 4;
            map.word(4).store(1, SeqCst); // CMP_REQUEUE must compare the source, not this word.
            let mut moved = 0;
            while moved < 2 {
                moved +=
                    ok("requeue until both queued", fx(map.addr, command | private, 0, 2 - moved, dest, 0))? as u64;
                nap();
            }
            eq("old key empty", fx(map.addr, FUTEX_WAKE | private, 2, 0, 0, 0), 0)?;
            eq("bitset mismatch", fx(dest, FUTEX_WAKE_BITSET | private, 2, 0, 0, 4), 0)?;
            eq("wake first bit", fx(dest, FUTEX_WAKE_BITSET | private, 1, 0, 0, 1), 1)?;
            eq("wake second bit", fx(dest, FUTEX_WAKE_BITSET | private, 1, 0, 0, 2), 1)?;
            c1.join()?;
            c2.join()?;
            eq("first waiter returned zero", a.result.load(SeqCst), 0)?;
            eq("second waiter returned zero", b.result.load(SeqCst), 0)?;
        }
        // A positive control on this exact word proves a wrong-domain wake
        // would have a waiter to find. Probe while requeue confirms it queued.
        let a = args(map.addr, FUTEX_WAIT | private, 0);
        let child = spawn(&a, &stacks[0])?;
        loop {
            if fx(map.addr, FUTEX_REQUEUE | private, 0, 1, map.addr, 0) == 1 {
                break;
            }
            nap();
        }
        eq("private/shared domains differ", fx(map.addr, FUTEX_WAKE | (private ^ FUTEX_PRIVATE_FLAG), 1, 0, 0, 0), 0)?;
        eq("right domain finds waiter even count zero", fx(map.addr, FUTEX_WAKE | private, 0, 0, 0, 0), 1)?;
        child.join()?;
    }
    Ok(())
}

fn requeue_wake_limits() -> R {
    let map = Mapping::anon(false)?;
    let stacks = [
        Mapping::new(65536, MAP_PRIVATE | MAP_ANONYMOUS, u64::MAX)?,
        Mapping::new(65536, MAP_PRIVATE | MAP_ANONYMOUS, u64::MAX)?,
    ];
    for private in [0, FUTEX_PRIVATE_FLAG] {
        for command in [FUTEX_REQUEUE, FUTEX_CMP_REQUEUE] {
            for nr_requeue in [0, 1] {
                let a = args(map.addr, FUTEX_WAIT | private, 0);
                let b = args(map.addr, FUTEX_WAIT | private, 0);
                let c1 = spawn(&a, &stacks[0])?;
                let c2 = spawn(&b, &stacks[1])?;
                // No timing assumption: requeue-to-self confirms both nodes exist.
                loop {
                    let queued = fx(map.addr, FUTEX_REQUEUE | private, 0, 2, map.addr, 0);
                    if queued == 2 {
                        break;
                    }
                    ensure("queue count", (0..=1).contains(&queued), queued)?;
                    nap();
                }
                let dest = map.addr + 4;
                map.word(4).store(1, SeqCst);
                eq(
                    "requeue wake and move counts",
                    fx(map.addr, command | private, 1, nr_requeue, dest, 0),
                    1 + nr_requeue as i64,
                )?;
                let remaining = if nr_requeue == 0 { map.addr } else { dest };
                eq("exactly one waiter remains", fx(remaining, FUTEX_WAKE | private, 2, 0, 0, 0), 1)?;
                eq(
                    "other key has no waiter",
                    fx(if nr_requeue == 0 { dest } else { map.addr }, FUTEX_WAKE | private, 2, 0, 0, 0),
                    0,
                )?;
                c1.join()?;
                c2.join()?;
                eq("first waiter returned zero", a.result.load(SeqCst), 0)?;
                eq("second waiter returned zero", b.result.load(SeqCst), 0)?;
            }
        }
    }
    Ok(())
}

fn file_map(shared: bool) -> Result<Mapping, common::Fail> {
    let fd = ok(
        "open futex file",
        sys(SYS_OPEN, [c"/tmp/c4-futex".as_ptr() as u64, O_RDWR | O_CREAT | O_TRUNC, 0o600, 0, 0, 0]),
    )?;
    let resized = sys(SYS_FTRUNCATE, [fd as u64, 4096, 0, 0, 0, 0]);
    let map = Mapping::new(4096, if shared { MAP_SHARED } else { MAP_PRIVATE }, fd as u64);
    sys(SYS_CLOSE, [fd as u64, 0, 0, 0, 0, 0]);
    sys(SYS_UNLINK, [c"/tmp/c4-futex".as_ptr() as u64, 0, 0, 0, 0, 0]);
    eq("ftruncate", resized, 0)?;
    map
}

fn across_fork() -> R {
    for kind in 0..3 {
        let map = if kind == 0 { file_map(true)? } else { Mapping::anon(kind == 1)? };
        let control = Mapping::anon(true)?;
        let pid = ok("fork waiter", sys(SYS_FORK, [0; 6]))?;
        if pid == 0 {
            let timeout = Timespec::of(INTERVAL);
            let before = now(CLOCK_MONOTONIC).unwrap_or(0);
            let got = fx(map.addr, FUTEX_WAIT, 0, if kind == 2 { timeout.ptr() } else { 0 }, 0, 0);
            control.word(0).store(1, SeqCst);
            let check = eq("cross-fork wait", got, if kind == 2 { -ETIMEDOUT } else { 0 }).and_then(|()| {
                if kind == 2 {
                    ensure("private Anon timeout floor", now(CLOCK_MONOTONIC)? - before >= INTERVAL, 0)
                } else {
                    Ok(())
                }
            });
            child_exit(check);
        }
        let child = Child(pid);
        if kind != 2 {
            wake_until(map.addr, FUTEX_WAKE, 0)?;
        } else {
            let mut probes = 0;
            while control.word(0).load(SeqCst) == 0 {
                eq("private Anon cannot wake across fork", fx(map.addr, FUTEX_WAKE, 1, 0, 0, 0), 0)?;
                probes += 1;
                nap();
            }
            ensure("waker ran during timed wait", probes > 0, probes)?;
        }
        child.join()?;
    }
    Ok(())
}

fn private_file_cow() -> R {
    let map = file_map(false)?;
    let stack = Mapping::new(65536, MAP_PRIVATE | MAP_ANONYMOUS, u64::MAX)?;
    let a = args(map.addr, FUTEX_WAIT, 0);
    let child = spawn(&a, &stack)?;
    // Requeue-to-self confirms WAIT is queued, but can itself fault in the
    // private copy. The kernel CoW test confirms blocking without resolving
    // the key first; here the later store must preserve this shared-op key.
    loop {
        if fx(map.addr, FUTEX_REQUEUE, 0, 1, map.addr, 0) == 1 {
            break;
        }
        nap();
    }
    map.word(0).store(1, SeqCst);
    eq("private file WAIT before store meets WAKE", fx(map.addr, FUTEX_WAKE, 1, 0, 0, 0), 1)?;
    child.join()?;
    let file = file_map(true)?;
    eq("untouched file WAIT demand pages", fx(file.addr, FUTEX_WAIT, 1, 0, 0, 0), -EAGAIN)
}

fn robust_mutex() -> R {
    eq("wrong robust-list length", sys(SYS_SET_ROBUST_LIST, [0, 23, 0, 0, 0, 0]), -EINVAL)?;
    let mut head = 0u64;
    let mut len = 0u64;
    eq(
        "get own robust list",
        sys(SYS_GET_ROBUST_LIST, [0, &mut head as *mut _ as u64, &mut len as *mut _ as u64, 0, 0, 0]),
        0,
    )?;
    eq("robust head size", len as i64, 24)?;
    let original = head;
    eq("set opaque robust head", sys(SYS_SET_ROBUST_LIST, [0x1234, 24, 0, 0, 0, 0]), 0)?;
    len = 0;
    let got = sys(SYS_GET_ROBUST_LIST, [0, &mut head as *mut _ as u64, &mut len as *mut _ as u64, 0, 0, 0]);
    eq("restore robust head", sys(SYS_SET_ROBUST_LIST, [original, 24, 0, 0, 0, 0]), 0)?;
    eq("get registered robust head", got, 0)?;
    eq("robust head round trip", head as i64, 0x1234)?;
    eq("registered robust size", len as i64, 24)?;
    eq(
        "missing robust tid",
        sys(SYS_GET_ROBUST_LIST, [u32::MAX as u64, &mut head as *mut _ as u64, &mut len as *mut _ as u64, 0, 0, 0]),
        -ESRCH,
    )?;
    let map = Mapping::anon(true)?;
    let mutex = map.addr as *mut c_void;
    let mut attr = 0u32;
    // SAFETY: musl x86-64 mutexattr is one unsigned word; mutex storage is a
    // zeroed aligned page, larger than pthread_mutex_t, retained until destroy.
    unsafe {
        eq("mutexattr init", pthread_mutexattr_init(&mut attr) as i64, 0)?;
        eq("mutexattr shared", pthread_mutexattr_setpshared(&mut attr, 1) as i64, 0)?;
        eq("mutexattr robust", pthread_mutexattr_setrobust(&mut attr, 1) as i64, 0)?;
        eq("mutex init", pthread_mutex_init(mutex, &attr) as i64, 0)?;
    }
    // SAFETY: musl fork updates the child's cached tid, which its mutex code
    // uses for ownership. A raw fork would deliberately bypass that bookkeeping.
    let pid = ok("robust owner fork", unsafe { fork() } as i64)?;
    if pid == 0 {
        // SAFETY: the shared mutex is initialized and the mapping is live.
        let locked = unsafe { pthread_mutex_lock(mutex) };
        child_exit(eq("owner lock", locked as i64, 0));
    }
    Child(pid).join()?;
    // SAFETY: child exited while owning this live process-shared mutex;
    // OWNER_DIED transfers it to us, consistent repairs it before unlock.
    unsafe {
        eq("owner died", pthread_mutex_lock(mutex) as i64, EOWNERDEAD)?;
        eq("mark consistent", pthread_mutex_consistent(mutex) as i64, 0)?;
        eq("unlock recovered", pthread_mutex_unlock(mutex) as i64, 0)?;
        eq("lock recovered", pthread_mutex_lock(mutex) as i64, 0)?;
        eq("unlock again", pthread_mutex_unlock(mutex) as i64, 0)?;
        eq("destroy mutex", pthread_mutex_destroy(mutex) as i64, 0)?;
    }
    Ok(())
}

struct ExitArgs {
    page: u64,
    exec: bool,
}
extern "C" fn exiting(arg: *mut c_void) -> c_int {
    // SAFETY: the parent retains this argument and its read/write page until join.
    let args = unsafe { &*(arg as *const ExitArgs) };
    let page = args.page;
    let tid = sys(SYS_SET_TID_ADDRESS, [page, 0, 0, 0, 0, 0]);
    // SAFETY: these aligned atomic words lie in the parent's live mapping.
    let (clear, gate) = unsafe { (&*(page as *const AtomicU32), &*((page + 4) as *const AtomicU32)) };
    clear.store(tid as u32, SeqCst);
    while gate.load(SeqCst) == 0 {
        fx(page + 4, FUTEX_WAIT, 0, 0, 0, 0);
    }
    if args.exec {
        let path = c"/bin/busybox";
        let argv = [path.as_ptr() as u64, c"true".as_ptr() as u64, 0];
        sys(SYS_EXECVE, [path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, 0]);
        sys(SYS_EXIT, [99, 0, 0, 0, 0, 0]);
    } else {
        sys(SYS_EXIT, [0; 6]);
    }
    99
}

fn clear_child_tid() -> R {
    for exec in [false, true] {
        let map = Mapping::anon(false)?;
        let stacks = [
            Mapping::new(65536, MAP_PRIVATE | MAP_ANONYMOUS, u64::MAX)?,
            Mapping::new(65536, MAP_PRIVATE | MAP_ANONYMOUS, u64::MAX)?,
        ];
        let exit_args = ExitArgs { page: map.addr, exec };
        // SAFETY: callback, stack and arguments live until the child is reaped;
        // the callback uses raw syscalls, avoiding shared libc TLS state.
        let pid = unsafe {
            clone(
                exiting,
                (stacks[0].addr + stacks[0].len) as *mut c_void,
                (CLONE_VM | SIGCHLD) as c_int,
                &exit_args as *const _ as *mut c_void,
            )
        };
        let owner = Child(ok("clear tid clone", pid as i64)?);
        while map.word(0).load(SeqCst) == 0 {
            nap();
        }
        let mut a = args(map.addr, FUTEX_WAIT, 0);
        a.value = map.word(0).load(SeqCst) as u64;
        let waiting = spawn(&a, &stacks[1])?;
        loop {
            if fx(map.addr, FUTEX_REQUEUE, 0, 1, map.addr, 0) == 1 {
                break;
            }
            nap();
        }
        map.word(4).store(1, SeqCst);
        ok("release exiting child", fx(map.addr + 4, FUTEX_WAKE, 1, 0, 0, 0))?;
        waiting.join()?;
        owner.join()?;
        eq("clear_child_tid store", map.word(0).load(SeqCst) as i64, 0)?;
        eq("clear_child_tid shared wake", a.result.load(SeqCst), 0)?;
    }
    Ok(())
}

fn robust_exec() -> R {
    let map = Mapping::anon(true)?;
    let pid = ok("robust exec fork", sys(SYS_FORK, [0; 6]))?;
    if pid == 0 {
        let head = map.addr;
        // SAFETY: the page is live, aligned, writable and not concurrently
        // accessed by the parent until wait4 confirms the child has exited.
        unsafe {
            (head as *mut u64).write(head + 32);
            ((head + 8) as *mut u64).write(8);
            ((head + 16) as *mut u64).write(0);
            ((head + 32) as *mut u64).write(head);
        }
        map.word(40).store(sys(SYS_GETTID, [0; 6]) as u32, SeqCst);
        if sys(SYS_SET_ROBUST_LIST, [head, 24, 0, 0, 0, 0]) != 0 {
            child_exit(Err(common::Fail { what: "set robust", got: -1 }));
        }
        let path = c"/bin/busybox";
        let argv = [path.as_ptr() as u64, c"true".as_ptr() as u64, 0];
        let result = sys(SYS_EXECVE, [path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, 0]);
        child_exit(Err(common::Fail { what: "exec returned", got: result }));
    }
    Child(pid).join()?;
    eq("exec released old robust owner", map.word(40).load(SeqCst) as i64, FUTEX_OWNER_DIED)
}

pub fn run() -> c_int {
    common::run_checks(&[
        ("futex-errors-and-demand", &errors_and_demand),
        ("futex-and-sleep-floors", &sleep_floors),
        ("futex-wakes-requeues-bitsets", &wakes_requeues_bitsets),
        ("futex-requeue-wake-limits", &requeue_wake_limits),
        ("futex-keys-across-fork", &across_fork),
        ("futex-private-file-cow", &private_file_cow),
        ("futex-robust-owner-death", &robust_mutex),
        ("futex-clear-child-tid-exit-exec", &clear_child_tid),
        ("futex-robust-exec", &robust_exec),
    ])
}

// ---------------------------------------------------------------------------
// C11: Cross-CPU pthread_mutex and pthread_cond stress
// ---------------------------------------------------------------------------

const SYS_SCHED_YIELD: u64 = 24;
const SYS_SCHED_SETAFFINITY: u64 = 203;
const SYS_SCHED_GETAFFINITY: u64 = 204;
const SYS_GETCPU: u64 = 309;
const MAX_STRESS_CPUS: usize = 8;
const MUTEX_ITERS: u64 = 2000;
const COND_TURNS: u64 = 250;

/// Over-sized, 8-byte aligned backing for musl x86-64 `pthread_mutex_t` (40 B)
/// and `pthread_cond_t` (48 B).
#[repr(C, align(8))]
struct PthreadBuf([u64; 8]);

impl PthreadBuf {
    const fn zeroed() -> Self {
        Self([0; 8])
    }
    fn as_ptr(&self) -> *mut c_void {
        self.0.as_ptr() as *mut c_void
    }
}

fn stress_online_cpus() -> Result<([u32; MAX_STRESS_CPUS], usize), common::Fail> {
    let mut mask = 0u64;
    eq("sched_getaffinity", sys(SYS_SCHED_GETAFFINITY, [0, 8, &mut mask as *mut _ as u64, 0, 0, 0]), 8)?;
    let mut cpus = [0u32; MAX_STRESS_CPUS];
    let mut count = 0usize;
    for cpu in 0..64u32 {
        if (mask & (1u64 << cpu)) != 0 && count < MAX_STRESS_CPUS {
            cpus[count] = cpu;
            count += 1;
        }
    }
    ensure("futex stress requires at least 4 online CPUs", count >= 4, count as i64)?;
    Ok((cpus, count))
}

fn pin_cpu(cpu: u32) -> R {
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

struct MutexStressState {
    mutex: PthreadBuf,
    ready: AtomicU32,
    go: AtomicU32,
    errors: AtomicU32,
    torn: AtomicU32,
    counter: core::cell::UnsafeCell<u64>,
    shadow: core::cell::UnsafeCell<u64>,
}

struct MutexWorkerArg {
    cpu: u32,
    state: *const MutexStressState,
}

extern "C" fn mutex_stress_worker(arg: *mut c_void) -> *mut c_void {
    // SAFETY: parent keeps `MutexWorkerArg` and `MutexStressState` alive until `pthread_join`.
    let w = unsafe { &*(arg as *const MutexWorkerArg) };
    // SAFETY: `w.state` points to the live stack-allocated `MutexStressState`.
    let s = unsafe { &*w.state };
    if pin_cpu(w.cpu).is_err() {
        s.errors.fetch_add(1, SeqCst);
        s.ready.fetch_add(1, SeqCst);
        return core::ptr::null_mut();
    }
    s.ready.fetch_add(1, SeqCst);
    while s.go.load(SeqCst) == 0 {
        sys(SYS_SCHED_YIELD, [0; 6]);
    }
    for _ in 0..MUTEX_ITERS {
        // SAFETY: `s.mutex` is initialized and remains live until all workers join.
        if unsafe { pthread_mutex_lock(s.mutex.as_ptr()) } != 0 {
            s.errors.fetch_add(1, SeqCst);
            return core::ptr::null_mut();
        }
        // SAFETY: `s.mutex` serializes access to `counter` and `shadow`.
        unsafe {
            let a = s.counter.get().read_volatile();
            let b = s.shadow.get().read_volatile();
            if a != b {
                s.torn.fetch_add(1, SeqCst);
            }
            s.counter.get().write_volatile(a + 1);
            for _ in 0..8 {
                core::hint::spin_loop();
            }
            s.shadow.get().write_volatile(b + 1);
        }
        // SAFETY: this thread currently owns `s.mutex`.
        if unsafe { pthread_mutex_unlock(s.mutex.as_ptr()) } != 0 {
            s.errors.fetch_add(1, SeqCst);
            return core::ptr::null_mut();
        }
    }
    core::ptr::null_mut()
}

fn pthread_mutex_stress() -> R {
    let (cpus, count) = stress_online_cpus()?;
    let state = MutexStressState {
        mutex: PthreadBuf::zeroed(),
        ready: AtomicU32::new(0),
        go: AtomicU32::new(0),
        errors: AtomicU32::new(0),
        torn: AtomicU32::new(0),
        counter: core::cell::UnsafeCell::new(0),
        shadow: core::cell::UnsafeCell::new(0),
    };
    // SAFETY: `state.mutex` is aligned, large enough for `pthread_mutex_t`, and destroyed below.
    eq("pthread_mutex_init", i64::from(unsafe { pthread_mutex_init(state.mutex.as_ptr(), core::ptr::null()) }), 0)?;

    let mut args = [const { MutexWorkerArg { cpu: 0, state: core::ptr::null() } }; MAX_STRESS_CPUS];
    let mut tids = [0usize; MAX_STRESS_CPUS];
    for i in 0..count {
        args[i] = MutexWorkerArg { cpu: cpus[i], state: &state };
        // SAFETY: `tids[i]` is writable; `args[i]` and `state` outlive every joined thread.
        let rc = unsafe {
            pthread_create(&mut tids[i], core::ptr::null(), mutex_stress_worker, (&args[i] as *const _) as *mut c_void)
        };
        eq("pthread_create mutex worker", i64::from(rc), 0)?;
    }

    while state.ready.load(SeqCst) < count as u32 {
        sys(SYS_SCHED_YIELD, [0; 6]);
    }
    state.go.store(1, SeqCst);

    for tid in &tids[..count] {
        let mut res = core::ptr::null_mut();
        // SAFETY: `tid` is a live joinable pthread.
        eq("pthread_join mutex worker", i64::from(unsafe { pthread_join(*tid, &mut res) }), 0)?;
    }

    // SAFETY: all threads have joined; reading non-atomic totals and destroying the mutex.
    let (counter, shadow, destroy) =
        unsafe { (state.counter.get().read(), state.shadow.get().read(), pthread_mutex_destroy(state.mutex.as_ptr())) };
    eq("pthread_mutex_destroy", i64::from(destroy), 0)?;
    eq("mutex worker errors", i64::from(state.errors.load(SeqCst)), 0)?;
    eq("mutex torn updates", i64::from(state.torn.load(SeqCst)), 0)?;
    let want = (count as u64 * MUTEX_ITERS) as i64;
    eq("mutex exact counter", counter as i64, want)?;
    eq("mutex exact shadow", shadow as i64, want)
}

struct CondTurnsState {
    mutex: PthreadBuf,
    cond: PthreadBuf,
    n_threads: usize,
    use_broadcast: bool,
    ready: AtomicU32,
    errors: AtomicU32,
    turn: core::cell::UnsafeCell<usize>,
    total: core::cell::UnsafeCell<u64>,
    per_thread: core::cell::UnsafeCell<[u64; MAX_STRESS_CPUS]>,
}

struct CondWorkerArg {
    id: usize,
    cpu: u32,
    state: *const CondTurnsState,
}

extern "C" fn cond_turns_worker(arg: *mut c_void) -> *mut c_void {
    // SAFETY: parent keeps `CondWorkerArg` and `CondTurnsState` alive until `pthread_join`.
    let w = unsafe { &*(arg as *const CondWorkerArg) };
    // SAFETY: `w.state` points to the live stack-allocated `CondTurnsState`.
    let s = unsafe { &*w.state };
    if pin_cpu(w.cpu).is_err() {
        s.errors.fetch_add(1, SeqCst);
        s.ready.fetch_add(1, SeqCst);
        return core::ptr::null_mut();
    }
    s.ready.fetch_add(1, SeqCst);
    while s.ready.load(SeqCst) < s.n_threads as u32 {
        sys(SYS_SCHED_YIELD, [0; 6]);
    }
    for _ in 0..COND_TURNS {
        // SAFETY: `s.mutex` and `s.cond` are initialized and live until join.
        if unsafe { pthread_mutex_lock(s.mutex.as_ptr()) } != 0 {
            s.errors.fetch_add(1, SeqCst);
            return core::ptr::null_mut();
        }
        loop {
            // SAFETY: `s.mutex` is held.
            let cur = unsafe { s.turn.get().read_volatile() };
            if cur == w.id {
                break;
            }
            // SAFETY: `s.mutex` is held and `s.cond` is initialized.
            if unsafe { pthread_cond_wait(s.cond.as_ptr(), s.mutex.as_ptr()) } != 0 {
                s.errors.fetch_add(1, SeqCst);
                // SAFETY: unlock before bailing out.
                unsafe { pthread_mutex_unlock(s.mutex.as_ptr()) };
                return core::ptr::null_mut();
            }
        }
        // SAFETY: `s.mutex` is held; exclusively update turn state.
        unsafe {
            let total = s.total.get().read_volatile();
            s.total.get().write_volatile(total + 1);
            let counts = &mut *s.per_thread.get();
            counts[w.id] += 1;
            s.turn.get().write_volatile((w.id + 1) % s.n_threads);
        }
        // SAFETY: `s.cond` is initialized and live.
        let wake_rc = unsafe {
            if s.use_broadcast { pthread_cond_broadcast(s.cond.as_ptr()) } else { pthread_cond_signal(s.cond.as_ptr()) }
        };
        if wake_rc != 0 {
            s.errors.fetch_add(1, SeqCst);
        }
        // SAFETY: this thread currently holds `s.mutex`.
        if unsafe { pthread_mutex_unlock(s.mutex.as_ptr()) } != 0 {
            s.errors.fetch_add(1, SeqCst);
            return core::ptr::null_mut();
        }
    }
    core::ptr::null_mut()
}

fn run_cond_turns(cpus: &[u32], use_broadcast: bool) -> R {
    let n = cpus.len();
    let state = CondTurnsState {
        mutex: PthreadBuf::zeroed(),
        cond: PthreadBuf::zeroed(),
        n_threads: n,
        use_broadcast,
        ready: AtomicU32::new(0),
        errors: AtomicU32::new(0),
        turn: core::cell::UnsafeCell::new(0),
        total: core::cell::UnsafeCell::new(0),
        per_thread: core::cell::UnsafeCell::new([0; MAX_STRESS_CPUS]),
    };
    // SAFETY: aligned zeroed buffers sized for musl x86-64 pthread_mutex_t / pthread_cond_t.
    unsafe {
        eq("cond mutex init", i64::from(pthread_mutex_init(state.mutex.as_ptr(), core::ptr::null())), 0)?;
        eq("cond init", i64::from(pthread_cond_init(state.cond.as_ptr(), core::ptr::null())), 0)?;
    }

    let mut args = [const { CondWorkerArg { id: 0, cpu: 0, state: core::ptr::null() } }; MAX_STRESS_CPUS];
    let mut tids = [0usize; MAX_STRESS_CPUS];
    for i in 0..n {
        args[i] = CondWorkerArg { id: i, cpu: cpus[i], state: &state };
        // SAFETY: `args[i]` and `state` stay alive until all threads are joined below.
        let rc = unsafe {
            pthread_create(&mut tids[i], core::ptr::null(), cond_turns_worker, (&args[i] as *const _) as *mut c_void)
        };
        eq("pthread_create cond worker", i64::from(rc), 0)?;
    }
    for tid in &tids[..n] {
        let mut res = core::ptr::null_mut();
        // SAFETY: `tid` is a live joinable pthread.
        eq("pthread_join cond worker", i64::from(unsafe { pthread_join(*tid, &mut res) }), 0)?;
    }

    // SAFETY: all threads have joined; inspect state and destroy synchronization primitives.
    let (turn, total, per_thread, c_rc, m_rc) = unsafe {
        (
            state.turn.get().read(),
            state.total.get().read(),
            state.per_thread.get().read(),
            pthread_cond_destroy(state.cond.as_ptr()),
            pthread_mutex_destroy(state.mutex.as_ptr()),
        )
    };
    eq("pthread_cond_destroy", i64::from(c_rc), 0)?;
    eq("pthread_mutex_destroy", i64::from(m_rc), 0)?;
    eq("cond worker errors", i64::from(state.errors.load(SeqCst)), 0)?;
    eq("cond final turn", turn as i64, 0)?;
    eq("cond total handoffs", total as i64, (n as u64 * COND_TURNS) as i64)?;
    for &c in &per_thread[..n] {
        eq("cond per-thread turns", c as i64, COND_TURNS as i64)?;
    }
    Ok(())
}

fn pthread_cond_turns() -> R {
    let (cpus, count) = stress_online_cpus()?;
    // 4-CPU round-robin handoffs with pthread_cond_broadcast (FUTEX_REQUEUE).
    run_cond_turns(&cpus[..count], true)?;
    // 2-CPU ping-pong handoffs across CPUs 0 and 1 with pthread_cond_signal.
    run_cond_turns(&cpus[..2], false)
}

pub fn stress() -> c_int {
    common::run_checks(&[("pthread-mutex-stress", &pthread_mutex_stress), ("pthread-cond-turns", &pthread_cond_turns)])
}
