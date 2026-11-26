//! C4 raw futex/sleep ABI checks and musl's process-shared robust mutex.
use crate::common::{self, R, ensure, eq, ok, sys};
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicI64, AtomicU32, Ordering::SeqCst};

const FUTEX: u64 = 202;
const PRIVATE: u64 = 128;
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
    eq("clock_gettime", sys(228, [clock, &mut ts as *mut _ as u64, 0, 0, 0, 0]), 0)?;
    Ok(ts.sec as u64 * NS + ts.nsec as u64)
}
fn pause() {
    sys(7, [0, 0, 1, 0, 0, 0]);
}
fn fx(addr: u64, op: u64, val: u64, timeout: u64, dest: u64, bits: u64) -> i64 {
    sys(FUTEX, [addr, op, val, timeout, dest, bits])
}
fn child_exit(result: R) -> ! {
    let code = match result {
        Ok(()) => 0,
        Err(e) => {
            say!("FAIL child: {} (got {})", e.what, e.got);
            99
        }
    };
    sys(60, [code, 0, 0, 0, 0, 0]);
    panic!("exit returned")
}

struct Mapping {
    addr: u64,
    len: u64,
}
impl Mapping {
    fn new(len: u64, flags: u64, fd: u64) -> Result<Self, common::Fail> {
        Ok(Self { addr: ok("mmap", sys(9, [0, len, 3, flags, fd, 0]))? as u64, len })
    }
    fn anon(shared: bool) -> Result<Self, common::Fail> {
        Self::new(4096, if shared { 0x21 } else { 0x22 }, u64::MAX)
    }
    fn word(&self, offset: u64) -> &AtomicU32 {
        assert!(offset.is_multiple_of(4) && offset + 4 <= self.len);
        // SAFETY: the live mmap is aligned and read/write; callers use atomic words.
        unsafe { &*((self.addr + offset) as *const AtomicU32) }
    }
}
impl Drop for Mapping {
    fn drop(&mut self) {
        sys(11, [self.addr, self.len, 0, 0, 0, 0]);
    }
}

struct Child(i64);
impl Child {
    fn join(mut self) -> R {
        let pid = core::mem::replace(&mut self.0, 0);
        let mut status = -1i32;
        eq("wait child", sys(61, [pid as u64, &mut status as *mut _ as u64, 0, 0, 0, 0]), pid)?;
        eq("child succeeded", status as i64, 0)
    }
}
impl Drop for Child {
    fn drop(&mut self) {
        if self.0 > 0 {
            sys(62, [self.0 as u64, 9, 0, 0, 0, 0]);
            sys(61, [self.0 as u64, 0, 0, 0, 0, 0]);
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
}
extern "C" fn waiter(arg: *mut c_void) -> c_int {
    // SAFETY: spawn passes a live WaitArgs retained until this child is reaped.
    let args = unsafe { &*(arg as *const WaitArgs) };
    let result = fx(args.addr, args.op, args.value, 0, 0, args.bits);
    args.result.store(result, SeqCst);
    sys(60, [u64::from(result != 0), 0, 0, 0, 0, 0]);
    99
}
fn spawn(args: &WaitArgs, stack: &Mapping) -> Result<Child, common::Fail> {
    // SAFETY: disjoint mapped stack and shared arguments live until join;
    // callback uses raw syscalls and atomics, never shared libc TLS state.
    let pid =
        unsafe { clone(waiter, (stack.addr + stack.len) as *mut c_void, 0x100 | 17, args as *const _ as *mut c_void) };
    Ok(Child(ok("CLONE_VM waiter", pid as i64)?))
}
fn wake_until(addr: u64, op: u64, bits: u64) -> R {
    loop {
        let count = fx(addr, op, 1, 0, 0, bits);
        if count == 1 {
            return Ok(());
        }
        eq("wake before waiter queued", count, 0)?;
        pause();
    }
}
fn args(addr: u64, op: u64, bits: u64) -> WaitArgs {
    WaitArgs { addr, op, value: 0, bits, result: AtomicI64::new(999) }
}

fn errors_and_demand() -> R {
    let map = Mapping::anon(false)?;
    let relative = Timespec::of(0);
    eq("untouched private WAIT mismatch", fx(map.addr, PRIVATE, 1, 0, 0, 0), -11)?;
    let second = Mapping::anon(false)?;
    eq("untouched shared WAIT mismatch", fx(second.addr, 0, 1, 0, 0, 0), -11)?;
    let third = Mapping::anon(false)?;
    eq("CMP_REQUEUE faults in source", fx(third.addr, 4 | PRIVATE, 0, 0, map.addr, 0), 0)?;
    eq("CMP_REQUEUE mismatch", fx(third.addr, 4 | PRIVATE, 0, 0, map.addr, 1), -11)?;
    eq("plain WAIT realtime", fx(map.addr, 256, 0, relative.ptr(), 0, 0), -38)?;
    eq("misaligned", fx(map.addr + 1, 1, 1, 0, 0, 0), -22)?;
    eq("outside user", fx(u64::MAX - 3, 1, 1, 0, 0, 0), -14)?;
    eq("unmapped shared", fx(0x4000, 1, 1, 0, 0, 0), -14)?;
    eq("zero wait bitset", fx(map.addr, 9, 0, 0, 0, 0), -22)?;
    eq("zero wake bitset", fx(map.addr, 10, 1, 0, 0, 0), -22)?;
    for bad in [Timespec { sec: -1, nsec: 0 }, Timespec { sec: 0, nsec: -1 }, Timespec { sec: 0, nsec: NS as i64 }] {
        eq("bad futex timeout", fx(map.addr, 0, 0, bad.ptr(), 0, 0), -22)?;
        eq("bad nanosleep", sys(35, [bad.ptr(), 0, 0, 0, 0, 0]), -22)?;
    }
    for op in [3, 4] {
        eq("negative wake precedes key fault", fx(0x4000, op, u32::MAX as u64, 1, 0x8000, 0), -22)?;
        eq("negative requeue precedes key fault", fx(0x4000, op, 1, u32::MAX as u64, 0x8000, 0), -22)?;
    }
    for op in [2, 5, 6, 7, 8, 11, 12, 13, 0x4000] {
        eq("unsupported futex op", fx(map.addr, op, 0, 0, 0, 0), -38)?;
    }
    eq("expired futex", fx(map.addr, 0, 0, relative.ptr(), 0, 0), -110)?;
    eq("futex_waitv", sys(449, [0; 6]), -38)
}

fn sleep_floors() -> R {
    let map = Mapping::anon(false)?;
    let req = Timespec::of(INTERVAL);
    let before = now(1)?;
    eq("relative futex timeout", fx(map.addr, 0, 0, req.ptr(), 0, 0), -110)?;
    ensure("futex timeout floor", now(1)? - before >= INTERVAL, 0)?;
    for clock in [1, 0] {
        let until = now(clock)? + INTERVAL;
        let req = Timespec::of(until);
        eq("absolute futex timeout", fx(map.addr, 9 | if clock == 0 { 256 } else { 0 }, 0, req.ptr(), 0, 1), -110)?;
        ensure("absolute futex floor", now(clock)? >= until, 0)?;
    }
    let before = now(1)?;
    eq("nanosleep", sys(35, [req.ptr(), 0, 0, 0, 0, 0]), 0)?;
    ensure("nanosleep floor", now(1)? - before >= INTERVAL, 0)?;
    for clock in [0, 1, 7] {
        let before = now(clock)?;
        eq("clock_nanosleep relative", sys(230, [clock, 0, req.ptr(), 0, 0, 0]), 0)?;
        ensure("relative sleep floor", now(clock)? - before >= INTERVAL, 0)?;
        let until = now(clock)? + INTERVAL;
        let absolute = Timespec::of(until);
        eq("absolute sleep ignores bad rem", sys(230, [clock, 1, absolute.ptr(), u64::MAX, 0, 0]), 0)?;
        ensure("absolute sleep floor", now(clock)? >= until, 0)?;
    }
    for clock in [2, 3, 99] {
        eq("CPU or invalid clock", sys(230, [clock, 0, req.ptr(), 0, 0, 0]), -22)?;
    }
    Ok(())
}

fn wakes_requeues_bitsets() -> R {
    let map = Mapping::anon(false)?;
    let stacks = [Mapping::new(65536, 0x22, u64::MAX)?, Mapping::new(65536, 0x22, u64::MAX)?];
    for private in [0, PRIVATE] {
        for command in [3, 4] {
            let a = args(map.addr, 9 | private, 1);
            let b = args(map.addr, 9 | private, 2);
            let c1 = spawn(&a, &stacks[0])?;
            let c2 = spawn(&b, &stacks[1])?;
            let dest = map.addr + 4;
            let mut moved = 0;
            while moved < 2 {
                moved +=
                    ok("requeue until both queued", fx(map.addr, command | private, 0, 2 - moved, dest, 0))? as u64;
                pause();
            }
            eq("old key empty", fx(map.addr, 1 | private, 2, 0, 0, 0), 0)?;
            eq("bitset mismatch", fx(dest, 10 | private, 2, 0, 0, 4), 0)?;
            eq("wake first bit", fx(dest, 10 | private, 1, 0, 0, 1), 1)?;
            eq("wake second bit", fx(dest, 10 | private, 1, 0, 0, 2), 1)?;
            c1.join()?;
            c2.join()?;
            eq("first waiter returned zero", a.result.load(SeqCst), 0)?;
            eq("second waiter returned zero", b.result.load(SeqCst), 0)?;
        }
        // A positive control on this exact word proves a wrong-domain wake
        // would have a waiter to find. Probe while requeue confirms it queued.
        let a = args(map.addr, private, 0);
        let child = spawn(&a, &stacks[0])?;
        loop {
            if fx(map.addr, 3 | private, 0, 1, map.addr, 0) == 1 {
                break;
            }
            pause();
        }
        eq("private/shared domains differ", fx(map.addr, 1 | (private ^ PRIVATE), 1, 0, 0, 0), 0)?;
        eq("right domain finds waiter even count zero", fx(map.addr, 1 | private, 0, 0, 0, 0), 1)?;
        child.join()?;
    }
    Ok(())
}

fn file_map(shared: bool) -> Result<Mapping, common::Fail> {
    let fd = ok("open futex file", sys(2, [c"/tmp/c4-futex".as_ptr() as u64, 0x242, 0o600, 0, 0, 0]))?;
    let resized = sys(77, [fd as u64, 4096, 0, 0, 0, 0]);
    let map = Mapping::new(4096, if shared { 1 } else { 2 }, fd as u64);
    sys(3, [fd as u64, 0, 0, 0, 0, 0]);
    sys(87, [c"/tmp/c4-futex".as_ptr() as u64, 0, 0, 0, 0, 0]);
    eq("ftruncate", resized, 0)?;
    map
}

fn across_fork() -> R {
    for kind in 0..3 {
        let map = if kind == 0 { file_map(true)? } else { Mapping::anon(kind == 1)? };
        let control = Mapping::anon(true)?;
        let pid = ok("fork waiter", sys(57, [0; 6]))?;
        if pid == 0 {
            let timeout = Timespec::of(INTERVAL);
            let before = now(1).unwrap_or(0);
            let got = fx(map.addr, 0, 0, if kind == 2 { timeout.ptr() } else { 0 }, 0, 0);
            control.word(0).store(1, SeqCst);
            let check = eq("cross-fork wait", got, if kind == 2 { -110 } else { 0 }).and_then(|()| {
                if kind == 2 { ensure("private Anon timeout floor", now(1)? - before >= INTERVAL, 0) } else { Ok(()) }
            });
            child_exit(check);
        }
        let child = Child(pid);
        if kind != 2 {
            wake_until(map.addr, 1, 0)?;
        } else {
            let mut probes = 0;
            while control.word(0).load(SeqCst) == 0 {
                eq("private Anon cannot wake across fork", fx(map.addr, 1, 1, 0, 0, 0), 0)?;
                probes += 1;
                pause();
            }
            ensure("waker ran during timed wait", probes > 0, probes)?;
        }
        child.join()?;
    }
    Ok(())
}

fn private_file_cow() -> R {
    let map = file_map(false)?;
    let stack = Mapping::new(65536, 0x22, u64::MAX)?;
    let a = args(map.addr, 0, 0);
    let child = spawn(&a, &stack)?;
    // Requeue to itself proves WAIT on the untouched writable-private file
    // has queued. The later store must not change the shared-op key.
    loop {
        if fx(map.addr, 3, 0, 1, map.addr, 0) == 1 {
            break;
        }
        pause();
    }
    map.word(0).store(1, SeqCst);
    eq("private file WAIT before store meets WAKE", fx(map.addr, 1, 1, 0, 0, 0), 1)?;
    child.join()?;
    let file = file_map(true)?;
    eq("untouched file WAIT demand pages", fx(file.addr, 0, 1, 0, 0, 0), -11)
}

fn robust_mutex() -> R {
    eq("wrong robust-list length", sys(273, [0, 23, 0, 0, 0, 0]), -22)?;
    let mut head = 0u64;
    let mut len = 0u64;
    eq("get own robust list", sys(274, [0, &mut head as *mut _ as u64, &mut len as *mut _ as u64, 0, 0, 0]), 0)?;
    eq("robust head size", len as i64, 24)?;
    eq(
        "missing robust tid",
        sys(274, [u32::MAX as u64, &mut head as *mut _ as u64, &mut len as *mut _ as u64, 0, 0, 0]),
        -3,
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
        eq("owner died", pthread_mutex_lock(mutex) as i64, 130)?;
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
    let tid = sys(218, [page, 0, 0, 0, 0, 0]);
    // SAFETY: these aligned atomic words lie in the parent's live mapping.
    let (clear, gate) = unsafe { (&*(page as *const AtomicU32), &*((page + 4) as *const AtomicU32)) };
    clear.store(tid as u32, SeqCst);
    while gate.load(SeqCst) == 0 {
        fx(page + 4, 0, 0, 0, 0, 0);
    }
    if args.exec {
        let path = c"/bin/busybox";
        let argv = [path.as_ptr() as u64, c"true".as_ptr() as u64, 0];
        sys(59, [path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, 0]);
        sys(60, [99, 0, 0, 0, 0, 0]);
    } else {
        sys(60, [0; 6]);
    }
    99
}

fn clear_child_tid() -> R {
    for exec in [false, true] {
        let map = Mapping::anon(false)?;
        let stacks = [Mapping::new(65536, 0x22, u64::MAX)?, Mapping::new(65536, 0x22, u64::MAX)?];
        let exit_args = ExitArgs { page: map.addr, exec };
        // SAFETY: callback, stack and arguments live until the child is reaped;
        // the callback uses raw syscalls, avoiding shared libc TLS state.
        let pid = unsafe {
            clone(
                exiting,
                (stacks[0].addr + stacks[0].len) as *mut c_void,
                0x100 | 17,
                &exit_args as *const _ as *mut c_void,
            )
        };
        let owner = Child(ok("clear tid clone", pid as i64)?);
        while map.word(0).load(SeqCst) == 0 {
            pause();
        }
        let mut a = args(map.addr, 0, 0);
        a.value = map.word(0).load(SeqCst) as u64;
        let waiting = spawn(&a, &stacks[1])?;
        loop {
            if fx(map.addr, 3, 0, 1, map.addr, 0) == 1 {
                break;
            }
            pause();
        }
        map.word(4).store(1, SeqCst);
        ok("release exiting child", fx(map.addr + 4, 1, 1, 0, 0, 0))?;
        waiting.join()?;
        owner.join()?;
        eq("clear_child_tid store", map.word(0).load(SeqCst) as i64, 0)?;
        eq("clear_child_tid shared wake", a.result.load(SeqCst), 0)?;
    }
    Ok(())
}

fn robust_exec() -> R {
    let map = Mapping::anon(true)?;
    let pid = ok("robust exec fork", sys(57, [0; 6]))?;
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
        map.word(40).store(sys(186, [0; 6]) as u32, SeqCst);
        if sys(273, [head, 24, 0, 0, 0, 0]) != 0 {
            child_exit(Err(common::Fail { what: "set robust", got: -1 }));
        }
        let path = c"/bin/busybox";
        let argv = [path.as_ptr() as u64, c"true".as_ptr() as u64, 0];
        let result = sys(59, [path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, 0]);
        child_exit(Err(common::Fail { what: "exec returned", got: result }));
    }
    Child(pid).join()?;
    eq("exec released old robust owner", map.word(40).load(SeqCst) as i64, 0x4000_0000)
}

pub fn run() -> c_int {
    common::run_checks(&[
        ("futex-errors-and-demand", &errors_and_demand),
        ("futex-and-sleep-floors", &sleep_floors),
        ("futex-wakes-requeues-bitsets", &wakes_requeues_bitsets),
        ("futex-keys-across-fork", &across_fork),
        ("futex-private-file-cow", &private_file_cow),
        ("futex-robust-owner-death", &robust_mutex),
        ("futex-clear-child-tid-exit-exec", &clear_child_tid),
        ("futex-robust-exec", &robust_exec),
    ])
}
