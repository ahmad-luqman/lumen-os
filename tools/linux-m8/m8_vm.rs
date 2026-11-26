//! `m8_vm`: M8's ring-3 checks of cross-CPU TLB shootdowns
//! (`tests/scenarios/test_linux_vm.py`).
//!
//! A `#![no_std]` dynamically linked musl x86-64 executable, built as
//! `tools/linux-m7/`'s are (the Makefile) into `build/linux/`, which both
//! images stage in `/usr/local/bin`. Its checks and their conventions:
//! `common.rs` and ARCHITECTURE section 15, "M8's tests".

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

#[macro_use]
mod common;

use common::{R, ensure, eq, ok, sys};
use core::arch::global_asm;
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

const USAGE: &str = "usage: m8_vm shootdown";

const SYS_MMAP: u64 = 9;
const SYS_MPROTECT: u64 = 10;
const SYS_MUNMAP: u64 = 11;
const SYS_RT_SIGACTION: u64 = 13;
const SYS_SCHED_YIELD: u64 = 24;
const SYS_FORK: u64 = 57;
const SYS_EXIT: u64 = 60;
const SYS_WAIT4: u64 = 61;
const SYS_SCHED_SETAFFINITY: u64 = 203;
const SYS_SCHED_GETAFFINITY: u64 = 204;
const SYS_GETCPU: u64 = 309;

const PROT_NONE: u64 = 0;
const PROT_READ: u64 = 1;
const PROT_WRITE: u64 = 2;
const MAP_SHARED: u64 = 0x01;
const MAP_PRIVATE: u64 = 0x02;
const MAP_ANONYMOUS: u64 = 0x20;

const PAGE_SIZE: u64 = 4096;
const BULK_PAGES: u64 = 34; // > FULL_FLUSH_PAGES (32)
const MAX_WORKERS: usize = 8;
const POST_PROBES: u64 = 64;

const SIGSEGV: u64 = 11;
const SA_SIGINFO: u64 = 4;
const SA_RESTORER: u64 = 0x0400_0000;
const SEGV_MAPERR: i64 = 1;
const SEGV_ACCERR: i64 = 2;

const SI_CODE_OFF: usize = 8;
const UC_MCONTEXT: usize = 40;
const MC_RAX: usize = 104;
const MC_RIP: usize = 128;

const SENTINEL: u32 = 0x5a5a_c3c3;
const OLD_VAL: u32 = 0x1122_3344;
const NEW_VAL: u32 = 0x5566_7788;
const POST_FORK_TAG: u32 = 0xdead_0000;

// SAFETY: musl x86-64 pthread symbols and local assembly fault-probe stubs.
unsafe extern "C" {
    fn pthread_create(
        thread: *mut usize,
        attr: *const c_void,
        run: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    fn pthread_join(thread: usize, result: *mut *mut c_void) -> c_int;
    fn m8_vm_restorer();
    fn m8_vm_try_load(addr: *const u32, out: *mut u32) -> i64;
    fn m8_vm_try_load_recover();
    fn m8_vm_try_store(addr: *mut u32, val: u32) -> i64;
    fn m8_vm_try_store_recover();
}

// Per-thread lock-free fault-catching load/store stubs. The faulting
// instruction sits directly at the hidden symbol address, and no stack slot
// is touched, so `segv_handler` recovers any thread concurrently by setting
// `mc_rax = si_code` and `mc_rip = *_recover`.
global_asm!(
    r#"
.hidden m8_vm_restorer
.global m8_vm_restorer
m8_vm_restorer:
    mov rax, 15
    syscall
    ud2

.hidden m8_vm_try_load
.global m8_vm_try_load
m8_vm_try_load:
    mov eax, [rdi]
    mov [rsi], eax
    xor eax, eax
    ret
.hidden m8_vm_try_load_recover
.global m8_vm_try_load_recover
m8_vm_try_load_recover:
    ret

.hidden m8_vm_try_store
.global m8_vm_try_store
m8_vm_try_store:
    mov [rdi], esi
    xor eax, eax
    ret
.hidden m8_vm_try_store_recover
.global m8_vm_try_store_recover
m8_vm_try_store_recover:
    ret
"#
);

/// `SIGSEGV` handler that redirects only known probe instructions to their
/// recovery `ret`, returning `si_code` in `rax`. Any unexpected fault exits
/// immediately with status 99 so real bugs are never hidden.
extern "C" fn segv_handler(sig: u64, info: *const u8, uc: *mut u8) {
    if sig != SIGSEGV || info.is_null() || uc.is_null() {
        // SAFETY: immediate termination on invalid signal delivery.
        unsafe { common::_exit(99) };
    }
    // SAFETY: Linux x86-64 SA_SIGINFO passes readable siginfo_t and writable ucontext_t.
    let (si_code, rip) = unsafe {
        let code = core::ptr::read_unaligned(info.add(SI_CODE_OFF).cast::<i32>()) as i64;
        let mc = uc.add(UC_MCONTEXT);
        let rip = core::ptr::read_unaligned(mc.add(MC_RIP).cast::<u64>());
        (if code == 0 { -1 } else { code }, rip)
    };
    let recover = if rip == m8_vm_try_load as *const () as u64 {
        m8_vm_try_load_recover as *const () as u64
    } else if rip == m8_vm_try_store as *const () as u64 {
        m8_vm_try_store_recover as *const () as u64
    } else {
        // SAFETY: unexpected SIGSEGV outside probe instructions; abort loudly.
        unsafe { common::_exit(99) };
    };
    // SAFETY: mc_rax and mc_rip lie in the live ucontext_t on this thread's stack.
    unsafe {
        let mc = uc.add(UC_MCONTEXT);
        core::ptr::write_unaligned(mc.add(MC_RAX).cast::<i64>(), si_code);
        core::ptr::write_unaligned(mc.add(MC_RIP).cast::<u64>(), recover);
    }
}

fn install_segv_handler() -> R {
    let act = [segv_handler as *const () as u64, SA_SIGINFO | SA_RESTORER, m8_vm_restorer as *const () as u64, 0];
    eq("install SIGSEGV handler", sys(SYS_RT_SIGACTION, [SIGSEGV, act.as_ptr() as u64, 0, 8, 0, 0]), 0)
}

/// Probe reading a `u32` at `addr`. Returns `Ok(val)` if readable, or `Err(si_code)` on `SIGSEGV`.
fn try_load_u32(addr: u64) -> Result<u32, i64> {
    let mut val = 0u32;
    // SAFETY: `m8_vm_try_load` catches SIGSEGV via `segv_handler`; `val` is a live stack local.
    let rc = unsafe { m8_vm_try_load(addr as *const u32, &mut val) };
    if rc == 0 { Ok(val) } else { Err(rc) }
}

/// Probe storing `val` to a `u32` at `addr`. Returns `Ok(())` if writable, or `Err(si_code)` on `SIGSEGV`.
fn try_store_u32(addr: u64, val: u32) -> Result<(), i64> {
    // SAFETY: `m8_vm_try_store` catches SIGSEGV via `segv_handler`.
    let rc = unsafe { m8_vm_try_store(addr as *mut u32, val) };
    if rc == 0 { Ok(()) } else { Err(rc) }
}

/// Discover online CPU indices from `sched_getaffinity`. Fails if fewer than 4 CPUs are online.
fn discover_online_cpus() -> Result<(u64, [u32; MAX_WORKERS], usize), common::Fail> {
    let mut mask = 0u64;
    eq("sched_getaffinity", sys(SYS_SCHED_GETAFFINITY, [0, 8, &mut mask as *mut _ as u64, 0, 0, 0]), 8)?;
    let mut cpus = [0u32; MAX_WORKERS];
    let mut count = 0usize;
    for cpu in 0..64u32 {
        if (mask & (1u64 << cpu)) != 0 && count < MAX_WORKERS {
            cpus[count] = cpu;
            count += 1;
        }
    }
    ensure("cross-CPU shootdown requires at least 4 online CPUs", count >= 4, count as i64)?;
    Ok((mask, cpus, count))
}

/// Restore the current thread's affinity mask.
fn set_affinity_mask(mask: u64) -> R {
    eq("restore sched_setaffinity", sys(SYS_SCHED_SETAFFINITY, [0, 8, &mask as *const _ as u64, 0, 0, 0]), 0)
}

/// Pin the current thread to `cpu` and confirm placement with `getcpu`.
fn pin_to_cpu(cpu: u32) -> R {
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

fn spawn_pthread(run: extern "C" fn(*mut c_void) -> *mut c_void, arg: *mut c_void) -> Result<usize, common::Fail> {
    let mut tid = 0usize;
    // SAFETY: `tid` is writable; callers keep `arg` alive until `join_pthread`.
    let rc = unsafe { pthread_create(&mut tid, core::ptr::null(), run, arg) };
    eq("pthread_create", i64::from(rc), 0)?;
    Ok(tid)
}

fn join_pthread_or_abort(tid: usize) {
    let mut res = core::ptr::null_mut();
    // SAFETY: `tid` is a live joinable pthread created by `spawn_pthread`.
    let rc = unsafe { pthread_join(tid, &mut res) };
    if rc != 0 {
        say!("FAIL pthread_join: failed (got {rc})");
        common::exit_group(99);
    }
}

struct Mapping {
    addr: u64,
    len: u64,
}

impl Mapping {
    fn new(len: u64, prot: u64, flags: u64) -> Result<Self, common::Fail> {
        let addr = ok("mmap", sys(SYS_MMAP, [0, len, prot, flags, u64::MAX, 0]))? as u64;
        Ok(Self { addr, len })
    }

    fn unmap(mut self) -> R {
        let addr = self.addr;
        let len = self.len;
        self.len = 0;
        eq("munmap", sys(SYS_MUNMAP, [addr, len, 0, 0, 0, 0]), 0)
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        if self.len != 0 {
            sys(SYS_MUNMAP, [self.addr, self.len, 0, 0, 0, 0]);
        }
    }
}

// ---------------------------------------------------------------------------
// 1 & 2. Cross-CPU `munmap` (single & bulk >32 pages) and `mprotect` shootdown
// ---------------------------------------------------------------------------

struct ProbeShared {
    base: u64,
    pages: u64,
    mode: u32, // 0 = load-fault (munmap/PROT_NONE), 1 = store-fault (PROT_READ)
    expected_code: i64,
    primed: AtomicU32,
    after: AtomicU32,
    abort: AtomicU32,
    err: common::WorkerError,
    bad_before: AtomicU32,
    stale_after: AtomicU64,
    wrong_code: AtomicU64,
    faults_after: AtomicU64,
}

struct ProbeWorker {
    cpu: u32,
    slot: u64,
    shared: *const ProbeShared,
}

extern "C" fn probe_reader(arg: *mut c_void) -> *mut c_void {
    // SAFETY: main thread keeps `ProbeWorker` and `ProbeShared` alive until join.
    let w = unsafe { &*(arg as *const ProbeWorker) };
    // SAFETY: `w.shared` points to the live stack-allocated `ProbeShared`.
    let s = unsafe { &*w.shared };
    if let Err(e) = pin_to_cpu(w.cpu) {
        s.err.record(e);
        s.abort.store(1, Ordering::Release);
        s.primed.fetch_add(1, Ordering::AcqRel);
        return core::ptr::null_mut();
    }

    // Prime this CPU's TLB across every page in the range.
    if s.mode == 0 {
        for p in 0..s.pages {
            let va = s.base + p * PAGE_SIZE;
            let want = SENTINEL ^ (p as u32);
            if try_load_u32(va) != Ok(want) {
                s.bad_before.fetch_add(1, Ordering::AcqRel);
            }
        }
    } else {
        let va = s.base + w.slot;
        if try_store_u32(va, SENTINEL ^ w.cpu).is_err() {
            s.bad_before.fetch_add(1, Ordering::AcqRel);
        }
    }
    s.primed.fetch_add(1, Ordering::AcqRel);

    // Probe continuously until the mutator completes its syscall and sets `after` (or aborts).
    let mut page_idx = 0u64;
    while s.after.load(Ordering::Acquire) == 0 && s.abort.load(Ordering::Acquire) == 0 {
        if s.mode == 0 {
            let va = s.base + page_idx * PAGE_SIZE;
            let want = SENTINEL ^ (page_idx as u32);
            if let Ok(v) = try_load_u32(va)
                && v != want
            {
                s.bad_before.fetch_add(1, Ordering::AcqRel);
            }
            page_idx = (page_idx + 1) % s.pages;
        } else {
            let va = s.base + w.slot;
            let _ = try_store_u32(va, SENTINEL ^ w.cpu);
        }
        core::hint::spin_loop();
    }
    if s.abort.load(Ordering::Acquire) != 0 {
        return core::ptr::null_mut();
    }

    // After shootdown returns: every single probe on every page must fault with `expected_code`.
    let count = POST_PROBES.max(2 * s.pages);
    for i in 0..count {
        let res = if s.mode == 0 {
            let p = i % s.pages;
            try_load_u32(s.base + p * PAGE_SIZE).map(|_| ())
        } else {
            try_store_u32(s.base + w.slot, 0xbad0_0000 | w.cpu)
        };
        match res {
            Ok(()) => {
                s.stale_after.fetch_add(1, Ordering::AcqRel);
            }
            Err(code) => {
                if code != s.expected_code {
                    s.wrong_code.fetch_add(1, Ordering::AcqRel);
                }
                s.faults_after.fetch_add(1, Ordering::AcqRel);
            }
        }
    }
    if s.mode == 1 {
        // Under PROT_READ, reading the slot must still succeed and match the pre-protect value.
        if try_load_u32(s.base + w.slot) != Ok(SENTINEL ^ w.cpu) {
            s.bad_before.fetch_add(1, Ordering::AcqRel);
        }
    }
    core::ptr::null_mut()
}

fn run_probe_round(
    cpus: &[u32],
    base: u64,
    pages: u64,
    mode: u32,
    expected_code: i64,
    mutate: impl FnOnce() -> R,
) -> R {
    pin_to_cpu(cpus[0])?;
    let shared = ProbeShared {
        base,
        pages,
        mode,
        expected_code,
        primed: AtomicU32::new(0),
        after: AtomicU32::new(0),
        abort: AtomicU32::new(0),
        err: common::WorkerError::new(),
        bad_before: AtomicU32::new(0),
        stale_after: AtomicU64::new(0),
        wrong_code: AtomicU64::new(0),
        faults_after: AtomicU64::new(0),
    };
    let remotes = cpus.len() - 1;
    let mut workers = [const { ProbeWorker { cpu: 0, slot: 0, shared: core::ptr::null() } }; MAX_WORKERS];
    let mut tids = [0usize; MAX_WORKERS];
    let mut started = 0usize;
    for i in 0..remotes {
        workers[i] = ProbeWorker { cpu: cpus[i + 1], slot: ((i + 1) as u64) * 64, shared: &shared };
        match spawn_pthread(probe_reader, (&workers[i] as *const ProbeWorker) as *mut c_void) {
            Ok(tid) => {
                tids[i] = tid;
                started += 1;
            }
            Err(e) => {
                shared.abort.store(1, Ordering::Release);
                for tid in &tids[..started] {
                    join_pthread_or_abort(*tid);
                }
                return Err(e);
            }
        }
    }

    // Confirm CPU 0 itself primes its TLB before mutating.
    let prime_res = (|| -> R {
        if mode == 0 {
            for p in 0..pages {
                eq(
                    "cpu0 prime load",
                    i64::from(try_load_u32(base + p * PAGE_SIZE).unwrap_or(0)),
                    i64::from(SENTINEL ^ (p as u32)),
                )?;
            }
        } else {
            ensure("cpu0 prime store", try_store_u32(base, SENTINEL ^ cpus[0]).is_ok(), 0)?;
        }
        Ok(())
    })();
    if let Err(e) = prime_res {
        shared.abort.store(1, Ordering::Release);
        for tid in &tids[..started] {
            join_pthread_or_abort(*tid);
        }
        return Err(e);
    }

    while shared.primed.load(Ordering::Acquire) < started as u32 && !shared.err.has_error() {
        sys(SYS_SCHED_YIELD, [0; 6]);
    }
    if shared.err.has_error() {
        shared.abort.store(1, Ordering::Release);
        for tid in &tids[..started] {
            join_pthread_or_abort(*tid);
        }
        return shared.err.check();
    }

    let mutate_res = mutate();
    shared.after.store(1, Ordering::Release);

    // Check CPU 0 immediately after mutation as well.
    let cpu0_res = if mode == 0 { try_load_u32(base).map(|_| ()) } else { try_store_u32(base, 0xbad0_0000) };

    for tid in &tids[..started] {
        join_pthread_or_abort(*tid);
    }
    shared.err.check()?;
    mutate_res?;
    eq("cpu0 fault code", cpu0_res.err().unwrap_or(0), expected_code)?;
    eq("reader pre-check errors", i64::from(shared.bad_before.load(Ordering::Acquire)), 0)?;
    eq("stale accesses after shootdown", shared.stale_after.load(Ordering::Acquire) as i64, 0)?;
    eq("unexpected si_code after shootdown", shared.wrong_code.load(Ordering::Acquire) as i64, 0)?;
    let min_faults = (remotes as u64) * POST_PROBES.max(2 * pages);
    eq("all post-shootdown probes faulted", shared.faults_after.load(Ordering::Acquire) as i64, min_faults as i64)
}

fn munmap_shootdown() -> R {
    let (orig_mask, cpus, count) = discover_online_cpus()?;
    let cpus = &cpus[..count];
    let res = (|| {
        for pages in [1u64, BULK_PAGES] {
            let len = pages * PAGE_SIZE;
            let map = Mapping::new(len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS)?;
            for p in 0..pages {
                // SAFETY: freshly mapped writable private page, exclusively ours before spawning readers.
                unsafe { ((map.addr + p * PAGE_SIZE) as *mut u32).write_volatile(SENTINEL ^ (p as u32)) };
            }
            let addr = map.addr;
            run_probe_round(cpus, addr, pages, 0, SEGV_MAPERR, move || map.unmap())?;
        }
        Ok(())
    })();
    set_affinity_mask(orig_mask)?;
    res
}

fn mprotect_shootdown() -> R {
    let (orig_mask, cpus, count) = discover_online_cpus()?;
    let cpus = &cpus[..count];
    let res = (|| {
        let map = Mapping::new(PAGE_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS)?;
        // SAFETY: freshly mapped writable private page.
        unsafe { (map.addr as *mut u32).write_volatile(SENTINEL) };

        // 1. RW -> PROT_NONE: every CPU's load must fault with SEGV_ACCERR.
        run_probe_round(cpus, map.addr, 1, 0, SEGV_ACCERR, || {
            eq("mprotect PROT_NONE", sys(SYS_MPROTECT, [map.addr, PAGE_SIZE, PROT_NONE, 0, 0, 0]), 0)
        })?;

        // 2. Restore RW, prime writable TLBs on every CPU, then narrow RW -> PROT_READ:
        // every CPU's store must fault with SEGV_ACCERR while reads stay intact.
        eq("mprotect RW", sys(SYS_MPROTECT, [map.addr, PAGE_SIZE, PROT_READ | PROT_WRITE, 0, 0, 0]), 0)?;
        run_probe_round(cpus, map.addr, 1, 1, SEGV_ACCERR, || {
            eq("mprotect PROT_READ", sys(SYS_MPROTECT, [map.addr, PAGE_SIZE, PROT_READ, 0, 0, 0]), 0)
        })?;
        map.unmap()
    })();
    set_affinity_mask(orig_mask)?;
    res
}

// ---------------------------------------------------------------------------
// 3. Threaded `fork` write-protect full shootdown
// ---------------------------------------------------------------------------

#[repr(C)]
struct ForkShared {
    post_fork: AtomicU32,
    child_check: AtomicU32,
    stop: AtomicU32,
    err: common::WorkerError,
    started: [AtomicU32; MAX_WORKERS],
    pre_writes: [AtomicU32; MAX_WORKERS],
    post_written: [AtomicU32; MAX_WORKERS],
}

struct ForkWriterArg {
    idx: usize,
    cpu: u32,
    priv_active: u64,
    priv_primed: u64,
    shared: *const ForkShared,
}

extern "C" fn fork_writer(arg: *mut c_void) -> *mut c_void {
    // SAFETY: main thread keeps `ForkWriterArg` and mappings alive until join.
    let w = unsafe { &*(arg as *const ForkWriterArg) };
    // SAFETY: `w.shared` lies in the live `MAP_SHARED` control page.
    let s = unsafe { &*w.shared };
    if let Err(e) = pin_to_cpu(w.cpu) {
        s.err.record(e);
        s.stop.store(1, Ordering::Release);
        s.started[w.idx].store(1, Ordering::Release);
        s.post_written[w.idx].store(1, Ordering::Release);
        return core::ptr::null_mut();
    }

    let off = (w.idx as u64) * 64;
    // SAFETY: each sibling writes exclusively to its own 64-byte-spaced AtomicU32 slot.
    let active_slot = unsafe { &*((w.priv_active + off) as *const AtomicU32) };
    // SAFETY: same disjoint slot on the second private page.
    let primed_slot = unsafe { &*((w.priv_primed + off) as *const AtomicU32) };

    // Prime writable TLB entries for both private pages on this CPU before fork.
    active_slot.store(1, Ordering::Release);
    primed_slot.store(1 + w.cpu, Ordering::Release);
    s.started[w.idx].store(1, Ordering::Release);

    let mut seq = 2u32;
    let mut did_post = false;
    while s.stop.load(Ordering::Acquire) == 0 {
        if s.post_fork.load(Ordering::Acquire) == 0 {
            let val = 1 + (seq & 0x0fff_ffff);
            active_slot.store(val, Ordering::Relaxed);
            seq = seq.wrapping_add(1);
            s.pre_writes[w.idx].fetch_add(1, Ordering::Release);
        } else {
            let post_val = POST_FORK_TAG | w.cpu;
            active_slot.store(post_val, Ordering::Release);
            primed_slot.store(post_val, Ordering::Release);
            if !did_post {
                did_post = true;
                s.post_written[w.idx].store(1, Ordering::Release);
            }
        }
        core::hint::spin_loop();
    }
    core::ptr::null_mut()
}

fn fork_write_protect_shootdown() -> R {
    let (orig_mask, cpus, count) = discover_online_cpus()?;
    let cpus = &cpus[..count];
    let remotes = count - 1;
    let res = (|| {
        pin_to_cpu(cpus[0])?;

        for _round in 0..8 {
            let priv_active = Mapping::new(PAGE_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS)?;
            let priv_primed = Mapping::new(PAGE_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS)?;
            let ctrl = Mapping::new(PAGE_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS)?;
            // SAFETY: `ctrl` is a zeroed 4 KiB page larger than `ForkShared` and 4096-aligned.
            let shared = unsafe { &*(ctrl.addr as *const ForkShared) };

            let mut args =
                [const { ForkWriterArg { idx: 0, cpu: 0, priv_active: 0, priv_primed: 0, shared: core::ptr::null() } };
                    MAX_WORKERS];
            let mut tids = [0usize; MAX_WORKERS];
            let mut started = 0usize;
            for i in 0..remotes {
                args[i] = ForkWriterArg {
                    idx: i,
                    cpu: cpus[i + 1],
                    priv_active: priv_active.addr,
                    priv_primed: priv_primed.addr,
                    shared,
                };
                match spawn_pthread(fork_writer, (&args[i] as *const ForkWriterArg) as *mut c_void) {
                    Ok(tid) => {
                        tids[i] = tid;
                        started += 1;
                    }
                    Err(e) => {
                        shared.stop.store(1, Ordering::Release);
                        for tid in &tids[..started] {
                            join_pthread_or_abort(*tid);
                        }
                        return Err(e);
                    }
                }
            }

            // Wait until every sibling is confirmed running on its CPU and actively writing.
            for i in 0..remotes {
                while !shared.err.has_error()
                    && (shared.started[i].load(Ordering::Acquire) == 0
                        || shared.pre_writes[i].load(Ordering::Acquire) < 4)
                {
                    sys(SYS_SCHED_YIELD, [0; 6]);
                }
            }
            if shared.err.has_error() {
                shared.stop.store(1, Ordering::Release);
                for tid in &tids[..started] {
                    join_pthread_or_abort(*tid);
                }
                return shared.err.check();
            }

            // Raw `fork` from this multithreaded parent while siblings are actively writing on other CPUs.
            let pid = sys(SYS_FORK, [0; 6]);
            if pid < 0 {
                shared.stop.store(1, Ordering::Release);
                for tid in &tids[..started] {
                    join_pthread_or_abort(*tid);
                }
                return Err(common::Fail { what: "threaded fork", got: pid });
            }
            if pid == 0 {
                // Child: wait until every sibling has performed its post-fork writes in the parent,
                // then verify none of those post-fork writes reached either private page here.
                while shared.child_check.load(Ordering::Acquire) == 0 {
                    sys(SYS_SCHED_YIELD, [0; 6]);
                }
                for i in 0..remotes {
                    let off = (i as u64) * 64;
                    // SAFETY: mapped private pages inherited from parent.
                    let a = unsafe { ((priv_active.addr + off) as *const u32).read_volatile() };
                    // SAFETY: mapped private pages inherited from parent.
                    let p = unsafe { ((priv_primed.addr + off) as *const u32).read_volatile() };
                    if a == 0 || (a & 0xffff_0000) == POST_FORK_TAG {
                        sys(SYS_EXIT, [10 + i as u64, 0, 0, 0, 0, 0]);
                    }
                    if p != 1 + cpus[i + 1] {
                        sys(SYS_EXIT, [20 + i as u64, 0, 0, 0, 0, 0]);
                    }
                }
                // Confirm child's own CoW write works cleanly.
                // SAFETY: child owns its private mapping.
                unsafe {
                    (priv_primed.addr as *mut u32).write_volatile(0xcafe_babe);
                    if (priv_primed.addr as *const u32).read_volatile() != 0xcafe_babe {
                        sys(SYS_EXIT, [30, 0, 0, 0, 0, 0]);
                    }
                }
                sys(SYS_EXIT, [0; 6]);
                panic!("child exit returned");
            }

            // Parent: let every sibling perform its post-fork writes on its remote CPU.
            shared.post_fork.store(1, Ordering::Release);
            for i in 0..remotes {
                while !shared.err.has_error() && shared.post_written[i].load(Ordering::Acquire) == 0 {
                    sys(SYS_SCHED_YIELD, [0; 6]);
                }
            }
            // Release the child to inspect its private pages AFTER all parent post-fork writes landed.
            shared.child_check.store(1, Ordering::Release);

            let mut status = -1i32;
            let wait_res = sys(SYS_WAIT4, [pid as u64, &mut status as *mut _ as u64, 0, 0, 0, 0]);

            shared.stop.store(1, Ordering::Release);
            for tid in &tids[..started] {
                join_pthread_or_abort(*tid);
            }

            shared.err.check()?;
            eq("wait4 child", wait_res, pid)?;
            eq("child saw no post-fork writes", i64::from(status), 0)?;

            // Parent must see the post-fork values written by its siblings.
            for i in 0..remotes {
                let off = (i as u64) * 64;
                let want = i64::from(POST_FORK_TAG | cpus[i + 1]);
                // SAFETY: siblings have joined; parent owns these private pages.
                let got_a = unsafe { ((priv_active.addr + off) as *const u32).read_volatile() };
                // SAFETY: siblings have joined; parent owns these private pages.
                let got_p = unsafe { ((priv_primed.addr + off) as *const u32).read_volatile() };
                eq("parent active post-fork value", i64::from(got_a), want)?;
                eq("parent primed post-fork value", i64::from(got_p), want)?;
            }
        }
        Ok(())
    })();
    set_affinity_mask(orig_mask)?;
    res
}

// ---------------------------------------------------------------------------
// 4. Copy-on-write split single-page shootdown with siblings reading the page
// ---------------------------------------------------------------------------

struct CowReaderShared {
    page: u64,
    primed: AtomicU32,
    split_done: AtomicU32,
    abort: AtomicU32,
    err: common::WorkerError,
    bad_before: AtomicU32,
    stale_after: AtomicU64,
    bad_value: AtomicU64,
}

struct CowReaderArg {
    cpu: u32,
    shared: *const CowReaderShared,
}

extern "C" fn cow_reader(arg: *mut c_void) -> *mut c_void {
    // SAFETY: main thread keeps `CowReaderArg` and `CowReaderShared` alive until join.
    let w = unsafe { &*(arg as *const CowReaderArg) };
    // SAFETY: `w.shared` points to the live stack-allocated `CowReaderShared`.
    let s = unsafe { &*w.shared };
    if let Err(e) = pin_to_cpu(w.cpu) {
        s.err.record(e);
        s.abort.store(1, Ordering::Release);
        s.primed.fetch_add(1, Ordering::AcqRel);
        return core::ptr::null_mut();
    }
    let ptr = s.page as *const AtomicU32;
    // SAFETY: `s.page` is a live readable private page until all readers join.
    let first = unsafe { (*ptr).load(Ordering::Acquire) };
    if first != OLD_VAL {
        s.bad_before.fetch_add(1, Ordering::AcqRel);
    }
    s.primed.fetch_add(1, Ordering::AcqRel);

    while s.split_done.load(Ordering::Acquire) == 0 && s.abort.load(Ordering::Acquire) == 0 {
        // SAFETY: `s.page` remains readable throughout the CoW split.
        let v = unsafe { (*ptr).load(Ordering::Relaxed) };
        if v != OLD_VAL && v != NEW_VAL {
            s.bad_value.fetch_add(1, Ordering::AcqRel);
        }
        core::hint::spin_loop();
    }
    if s.abort.load(Ordering::Acquire) != 0 {
        return core::ptr::null_mut();
    }

    // After the split and store completed on CPU 0, every remote reader must
    // see NEW_VAL — never OLD_VAL from a stale TLB entry to the old frame.
    for _ in 0..POST_PROBES {
        // SAFETY: `s.page` is mapped and readable.
        let v = unsafe { (*ptr).load(Ordering::Acquire) };
        if v == OLD_VAL {
            s.stale_after.fetch_add(1, Ordering::AcqRel);
        } else if v != NEW_VAL {
            s.bad_value.fetch_add(1, Ordering::AcqRel);
        }
    }
    core::ptr::null_mut()
}

fn cow_split_shootdown() -> R {
    let (orig_mask, cpus, count) = discover_online_cpus()?;
    let cpus = &cpus[..count];
    let remotes = count - 1;
    let res = (|| {
        pin_to_cpu(cpus[0])?;

        for _round in 0..8 {
            let priv_page = Mapping::new(PAGE_SIZE, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS)?;
            let ctrl = Mapping::new(PAGE_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS)?;
            // SAFETY: freshly mapped private page.
            unsafe { (*(priv_page.addr as *const AtomicU32)).store(OLD_VAL, Ordering::Release) };
            // SAFETY: freshly mapped shared page.
            let release_child = unsafe { &*(ctrl.addr as *const AtomicU32) };

            // Fork a child that pins the shared private frame at refcount 2 until released.
            let pid = ok("cow hold fork", sys(SYS_FORK, [0; 6]))?;
            if pid == 0 {
                while release_child.load(Ordering::Acquire) == 0 {
                    sys(SYS_SCHED_YIELD, [0; 6]);
                }
                // SAFETY: private page in child must still hold OLD_VAL after parent split.
                let v = unsafe { (priv_page.addr as *const u32).read_volatile() };
                sys(SYS_EXIT, [if v == OLD_VAL { 0 } else { 44 }, 0, 0, 0, 0, 0]);
                panic!("child exit returned");
            }

            let shared = CowReaderShared {
                page: priv_page.addr,
                primed: AtomicU32::new(0),
                split_done: AtomicU32::new(0),
                abort: AtomicU32::new(0),
                err: common::WorkerError::new(),
                bad_before: AtomicU32::new(0),
                stale_after: AtomicU64::new(0),
                bad_value: AtomicU64::new(0),
            };
            let mut args = [const { CowReaderArg { cpu: 0, shared: core::ptr::null() } }; MAX_WORKERS];
            let mut tids = [0usize; MAX_WORKERS];
            let mut started = 0usize;
            for i in 0..remotes {
                args[i] = CowReaderArg { cpu: cpus[i + 1], shared: &shared };
                match spawn_pthread(cow_reader, (&args[i] as *const CowReaderArg) as *mut c_void) {
                    Ok(tid) => {
                        tids[i] = tid;
                        started += 1;
                    }
                    Err(e) => {
                        shared.abort.store(1, Ordering::Release);
                        for tid in &tids[..started] {
                            join_pthread_or_abort(*tid);
                        }
                        release_child.store(1, Ordering::Release);
                        let _ = sys(SYS_WAIT4, [pid as u64, 0, 0, 0, 0, 0]);
                        return Err(e);
                    }
                }
            }

            while shared.primed.load(Ordering::Acquire) < started as u32 && !shared.err.has_error() {
                sys(SYS_SCHED_YIELD, [0; 6]);
            }
            if shared.err.has_error() {
                shared.abort.store(1, Ordering::Release);
                for tid in &tids[..started] {
                    join_pthread_or_abort(*tid);
                }
                release_child.store(1, Ordering::Release);
                let _ = sys(SYS_WAIT4, [pid as u64, 0, 0, 0, 0, 0]);
                return shared.err.check();
            }

            // Trigger the copy-on-write split on CPU 0 while every sibling CPU holds
            // a cached read-only TLB entry to the old physical frame.
            // SAFETY: `priv_page` is a live writable VMA; this store triggers CoW split.
            unsafe { (*(priv_page.addr as *const AtomicU32)).store(NEW_VAL, Ordering::Release) };
            shared.split_done.store(1, Ordering::Release);

            for tid in &tids[..started] {
                join_pthread_or_abort(*tid);
            }

            release_child.store(1, Ordering::Release);
            let mut status = -1i32;
            eq("wait4 cow child", sys(SYS_WAIT4, [pid as u64, &mut status as *mut _ as u64, 0, 0, 0, 0]), pid)?;
            eq("cow child preserved OLD_VAL", i64::from(status), 0)?;
            shared.err.check()?;
            eq("cow reader pre-check errors", i64::from(shared.bad_before.load(Ordering::Acquire)), 0)?;
            eq("stale OLD_VAL reads after CoW split", shared.stale_after.load(Ordering::Acquire) as i64, 0)?;
            eq("unexpected values during CoW split", shared.bad_value.load(Ordering::Acquire) as i64, 0)?;
        }
        Ok(())
    })();
    set_affinity_mask(orig_mask)?;
    res
}

fn shootdown() -> c_int {
    if let Err(e) = install_segv_handler() {
        say!("FAIL setup: {} (got {})", e.what, e.got);
        return 1;
    }
    common::run_checks(&[
        ("munmap-shootdown", &munmap_shootdown),
        ("mprotect-shootdown", &mprotect_shootdown),
        ("fork-write-protect-shootdown", &fork_write_protect_shootdown),
        ("cow-split-shootdown", &cow_split_shootdown),
    ])
}

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const u8) -> c_int {
    // SAFETY: musl's `__libc_start_main` passes the kernel's `argc` and
    // `argv`, kept for the program's life.
    let (args, n) = unsafe { common::args(argc, argv) };
    match &args[..n] {
        [_, b"shootdown"] => shootdown(),
        _ => {
            say!("{USAGE}");
            common::USAGE_EXIT
        }
    }
}
