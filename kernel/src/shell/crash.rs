//! `crash <kind> [cpu|thread]`: deliberate faults, for the crash reporter's
//! tests (and for seeing a crash report).
//!
//! A fault that doesn't happen (protection broken) is reported on serial and
//! the screen, whichever console asked for it: the tests read serial.

use super::Ctx;
use crate::arch;
use alloc::string::String;
use core::arch::asm;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

/// What `crash <kind>` accepts.
pub const KINDS: &[&str] = &[
    "pf",
    "ud",
    "de",
    "gp",
    "df",
    "wtext",
    "nx",
    "panic",
    "oom",
    "dfree",
    "heapcorrupt",
    "stack",
    "bigframe",
    "guard",
    "relock",
    "relocklog",
    "blockheld",
    "blockirqoff",
    "selfblock",
    "bystander",
    "firstrun",
    "heldelsewhere",
];

/// Writes to serial and to the virtual terminal whose shell ran the
/// command (the first, with the log lines, for serial's): a fault that
/// didn't happen, the heap's line before a heap crash. The tests read
/// serial.
struct Out;

/// The virtual terminal `crash` was run on ([`run`] sets it before the
/// fault, which may run in another thread or on another CPU).
static OUT_VT: AtomicUsize = AtomicUsize::new(0);

impl Write for Out {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        // Under the line lock, like every serial writer but the crash
        // reporter (one lock per piece of a `write!`).
        crate::klog::LockedSerial.write_str(s)?;
        crate::term::write_str_to(OUT_VT.load(Ordering::Relaxed), s);
        Ok(())
    }
}

/// `crash <kind> [cpu|thread]`: run the fault here, on another online CPU
/// (from its idle thread; its crash report then stops this one), or in a new
/// thread on this CPU (named `shell-crash`), which the shell waits for.
pub fn run(ctx: &mut Ctx<'_>, args: &[String]) {
    // Stored only once the fault will run (a remote one's after its job is
    // claimed): an earlier refusal leaves a fault under way where it was.
    let vt = match ctx.out.place() {
        crate::term::Place::Vt(vt) => vt,
        _ => 0,
    };
    let kind = args.first().map_or("", String::as_str);
    let Some(fault) = crash_fn(kind) else {
        return ctx.fail(format_args!("crash: unknown kind '{kind}' ({})", KINDS.join(" ")));
    };
    if args.len() > 2 {
        return ctx.fail("crash: usage: crash <kind> [cpu|thread]");
    }
    let target = args.get(1).map(String::as_str);
    if target == Some("thread") {
        OUT_VT.store(vt, Ordering::Relaxed);
        match crate::sched::spawn("shell-crash", fault) {
            Ok(handle) => handle.join(),
            Err(e) => ctx.fail(format_args!("crash: no thread: {e}")),
        }
        return;
    }
    match target.map(str::parse::<usize>) {
        None | Some(Ok(0)) => {
            OUT_VT.store(vt, Ordering::Relaxed);
            fault();
        }
        Some(Ok(cpu)) => {
            if REMOTE.try_update(Ordering::Acquire, Ordering::Relaxed, |s| (s != HANDED).then_some(HANDED)).is_err() {
                return ctx.fail("crash: a crash sent to another CPU hasn't finished yet");
            }
            REMOTE_KIND.store(KINDS.iter().position(|k| *k == kind).expect("a known kind"), Ordering::Relaxed);
            OUT_VT.store(vt, Ordering::Relaxed);
            match crate::smp::run_on(cpu, run_remote) {
                Ok(()) => wait_remote(ctx, cpu),
                Err(e) => {
                    REMOTE.store(IDLE, Ordering::Release);
                    ctx.fail(format_args!("crash: cpu {cpu} {e}"));
                }
            }
        }
        Some(Err(_)) => ctx.fail("crash: the cpu is a number (0: this one) or 'thread'"),
    }
}

/// Where `crash <kind> <cpu>` is: nothing sent ([`IDLE`]), a fault sent to
/// another CPU that hasn't returned ([`HANDED`]: claimed before the kind is
/// stored, so a second one, from either shell, is refused rather than
/// changing the kind under the first), or one that returned there
/// ([`RETURNED`], protection broken).
static REMOTE: AtomicU8 = AtomicU8::new(IDLE);
const IDLE: u8 = 0;
const HANDED: u8 = 1;
const RETURNED: u8 = 2;
/// The kind sent (an index into [`KINDS`]).
static REMOTE_KIND: AtomicUsize = AtomicUsize::new(0);

/// The job `crash <kind> <cpu>` hands to the other CPU (`run_on` takes a
/// plain `fn()`).
fn run_remote() {
    let kind = KINDS[REMOTE_KIND.load(Ordering::Relaxed)];
    crash_fn(kind).expect("a known kind")();
    REMOTE.store(RETURNED, Ordering::Release);
}

/// Wait until the other CPU's crash report stops this one, or its fault
/// returns (it then says why, a failure: `Ctx::fail`). Asleep, allocating nothing: going on (back
/// to the prompt, freeing the line's words) could leave the heap's lock
/// held when the report stops this CPU, and the report would show the heap
/// as maybe mid-update (a screen shell could still be allocating if
/// someone types there meanwhile; not tested). After 10 s (not tested) the
/// job is taken back if the CPU hasn't taken it; if it has, it is left to
/// finish, and another remote crash is refused until it returns.
fn wait_remote(ctx: &mut Ctx<'_>, cpu: usize) {
    let deadline = crate::time::now_ns() + 10_000_000_000;
    while REMOTE.load(Ordering::Acquire) == HANDED {
        if crate::time::now_ns() > deadline {
            if crate::smp::withdraw(cpu) {
                REMOTE.store(IDLE, Ordering::Release);
                ctx.fail(format_args!("crash: cpu {cpu} didn't take it in 10 s; taken back"));
            } else {
                ctx.fail(format_args!("crash: cpu {cpu} took it but hasn't finished in 10 s"));
            }
            return;
        }
        crate::time::sleep(1_000_000);
    }
    REMOTE.store(IDLE, Ordering::Release);
    ctx.fail(format_args!("crash: cpu {cpu} ran it and went on"));
}

fn crash_fn(kind: &str) -> Option<fn()> {
    Some(match kind {
        "pf" => crash_page_fault,
        "ud" => crash_invalid_opcode,
        "de" => crash_divide_error,
        "gp" => crash_general_protection,
        "df" => crash_double_fault,
        "wtext" => crash_write_text,
        "nx" => crash_execute_data,
        "panic" => crash_panic,
        "oom" => crash_out_of_memory,
        "dfree" => crash_double_free,
        "heapcorrupt" => crash_corrupt_heap,
        "guard" => crash_touch_guard,
        "stack" => crash_stack_overflow,
        "bigframe" => crash_big_frame_overflow,
        "relock" => crash_relock,
        "relocklog" => crash_relock_log,
        "blockheld" => crash_block_with_lock_held,
        "blockirqoff" => crash_block_with_interrupts_off,
        "selfblock" => crash_block_after_disabling_for_good,
        "bystander" => crash_with_a_bystander_blocking,
        "firstrun" => crash_with_a_bystander_starting,
        "heldelsewhere" => crash_with_a_bystander_holding,
        _ => return None,
    })
}

/// Address that is never mapped: the kernel's page tables map nothing in the
/// lower half.
const UNMAPPED_ADDR: u64 = 0xdead_b000;

#[inline(never)]
fn crash_page_fault() {
    // SAFETY: deliberately faulting; the crash reporter takes over.
    unsafe { core::ptr::write_volatile(UNMAPPED_ADDR as *mut u64, 0x1) };
}

#[inline(never)]
fn crash_invalid_opcode() {
    // SAFETY: deliberately faulting.
    unsafe { asm!("ud2") };
}

#[inline(never)]
fn crash_divide_error() {
    // SAFETY: deliberately faulting (divide by zero).
    unsafe { asm!("div {0:e}", in(reg) core::hint::black_box(0u32), inout("eax") 1u32 => _, inout("edx") 0u32 => _) };
}

#[inline(never)]
fn crash_general_protection() {
    // SAFETY: deliberately faulting: a non-canonical address raises #GP.
    unsafe { core::ptr::read_volatile(0x8000_0000_0000_0000u64 as *const u64) };
}

/// Point the stack at unmapped memory and push: the #PF can't push its own
/// frame, so the CPU raises #DF, which runs on its IST stack.
#[inline(never)]
fn crash_double_fault() {
    // SAFETY: deliberately faulting; never returns.
    unsafe { asm!("mov rsp, {0}", "push rax", "ud2", in(reg) UNMAPPED_ADDR + 0x1000, options(noreturn)) };
}

/// Write to the kernel's own code: `.text` is mapped read-only and CR0.WP
/// makes that bind in ring 0, so this must page-fault. If protection is
/// broken it writes back the byte it read (changing nothing) and says so,
/// which fails the test cleanly instead of corrupting the kernel.
#[inline(never)]
fn crash_write_text() {
    let target = crash_write_text as *const () as *mut u8;
    // SAFETY: deliberately faulting; if the write is allowed it stores the
    // byte that was already there.
    unsafe { core::ptr::write_volatile(target, core::ptr::read_volatile(target)) };
    let _ = writeln!(Out, "crash: write to .text succeeded (W^X broken)");
}

/// One `ret` instruction in `.data` (writable, so it must not be executable).
#[unsafe(link_section = ".data.lumen_nx_probe")]
#[used]
static RET_IN_DATA: [u8; 1] = [0xc3];

/// Call into `.data`: NX must make the instruction fetch page-fault. If it
/// doesn't, the `ret` returns and this says so.
#[inline(never)]
fn crash_execute_data() {
    let target = core::ptr::from_ref(&RET_IN_DATA) as u64;
    // SAFETY: deliberately faulting; if NX is broken the callee is a single
    // `ret`, which returns here with every register intact except the ones
    // `clobber_abi` lists.
    unsafe { asm!("call {0}", in(reg) target, clobber_abi("C")) };
    let _ = writeln!(Out, "crash: executing .data succeeded (NX broken)");
}

#[inline(never)]
fn crash_panic() {
    panic!("deliberate panic from the shell");
}

/// Ask the heap for more than its window holds (2 TiB) through an ordinary
/// infallible allocation: the null ends in `handle_alloc_error`'s panic.
/// (`black_box` keeps LLVM from deleting an allocation nobody looks at, and
/// with it the failure: it may assume such allocations succeed.)
#[inline(never)]
fn crash_out_of_memory() {
    drop(core::hint::black_box(alloc::vec::Vec::<u8>::with_capacity(1 << 41)));
    let _ = writeln!(Out, "crash: a 2 TiB allocation succeeded (heap limit broken)");
}

/// Allocate `layout` or end in `handle_alloc_error`, keeping LLVM from
/// seeing through the pointer (see [`crash_out_of_memory`]).
fn alloc_or_die(layout: core::alloc::Layout) -> *mut u8 {
    // SAFETY: every caller's layout has a non-zero size.
    let p = core::hint::black_box(unsafe { alloc::alloc::alloc(layout) });
    if p.is_null() {
        alloc::alloc::handle_alloc_error(layout);
    }
    p
}

/// The heap's summary line, on serial and the screen, before a heap crash
/// does its damage: what the tests compare the report's heap line with
/// (the shell's own allocations for the command included).
fn heap_before() {
    let _ = crate::mm::heap::write_stats(&mut Out);
}

/// Free a block twice after its lower neighbour was freed (so it merged into
/// the neighbour's free block): the heap must refuse the second free (a
/// panic), not corrupt its free list. A 128-byte block stays live, so the
/// report shows one more block in use than the summary printed first
/// (`test_double_free_is_refused` checks it).
#[inline(never)]
fn crash_double_free() {
    use core::hint::black_box;
    heap_before();
    let keep = black_box(alloc::boxed::Box::new([0u8; 128]));
    let layout = core::alloc::Layout::from_size_align(64, 16).expect("valid layout");
    let (a, b) = (alloc_or_die(layout), alloc_or_die(layout));
    // SAFETY: deliberately breaks `dealloc`'s contract (the second free of
    // `b`); `black_box` keeps LLVM from reasoning about it, and the heap
    // refuses that free before changing anything.
    unsafe {
        alloc::alloc::dealloc(black_box(a), layout);
        alloc::alloc::dealloc(black_box(b), layout);
        alloc::alloc::dealloc(black_box(b), layout);
    }
    drop(keep);
    let _ = writeln!(Out, "crash: a double free went unnoticed");
}

/// Write into a freed block (a use after free) so its free-list link points
/// back at itself, then allocate something too big for it (a walk without
/// the link check would follow the self-link forever): the walk must panic,
/// and the report must still show the heap (its lock is held).
#[inline(never)]
fn crash_corrupt_heap() {
    use core::hint::black_box;
    heap_before();
    let layout = core::alloc::Layout::from_size_align(64, 16).expect("valid layout");
    let (a, b) = (alloc_or_die(layout), alloc_or_die(layout));
    let offset = a as usize - crate::mm::paging::HEAP_BASE as usize;
    // SAFETY: deliberately corrupting the heap: `a` is freed, then the
    // second word of its free block (the link to the next free block) is
    // pointed at `a` itself. `b` keeps `a` from merging with the free tail.
    unsafe {
        alloc::alloc::dealloc(black_box(a), layout);
        black_box(a).cast::<usize>().add(1).write_volatile(offset);
    }
    let big = core::alloc::Layout::from_size_align(128, 16).expect("valid layout");
    let p = alloc_or_die(big);
    // SAFETY: only reached if the corruption went unnoticed; both blocks
    // are live allocations with these layouts.
    unsafe {
        alloc::alloc::dealloc(p, big);
        alloc::alloc::dealloc(b, layout);
    }
    let _ = writeln!(Out, "crash: a corrupt free list went unnoticed");
}

/// Write into the guard page below the stack this runs on (see
/// [`crash_stack_overflow`]) from a healthy stack: a
/// plain #PF (nothing overflowed), which the report names as a stray guard
/// page hit, not an overflow.
#[inline(never)]
fn crash_touch_guard() {
    // The stack it runs on.
    let Some(main) = arch::stacks::stack_of(arch::read_rsp()) else {
        let _ = writeln!(Out, "crash: not on a registered stack");
        return;
    };
    // SAFETY: deliberately faulting: the guard page is never mapped.
    unsafe { core::ptr::write_volatile((main.range.guard() + 8) as *mut u64, 1) };
    let _ = writeln!(Out, "crash: writing to a guard page succeeded (guard page mapped?)");
}

/// Overflow the stack this runs on with small frames (the `main` thread's
/// from the serial shell, a screen shell's thread's from there, a new
/// thread's with `thread`, the idle thread's on another CPU).
#[inline(never)]
fn crash_stack_overflow() {
    let depth = recurse(0);
    let _ = writeln!(Out, "crash: recursion returned at depth {depth} (no guard page?)");
}

/// Overflow the stack this runs on (see [`crash_stack_overflow`]) with
/// frames bigger than a page: stack probes must
/// make it fault in the guard page instead of jumping over it into the
/// mapped stack below.
#[inline(never)]
fn crash_big_frame_overflow() {
    let depth = recurse_big(0);
    let _ = writeln!(Out, "crash: big-frame recursion returned at depth {depth} (no guard page?)");
}

/// [`recurse`] with a 16 KiB frame of which only the lowest word is written:
/// without stack probes the frame would jump over the guard page and that
/// write would land in the stack mapped below.
#[inline(never)]
fn recurse_big(depth: u64) -> u64 {
    let mut frame = core::mem::MaybeUninit::<[u64; 2048]>::uninit();
    let lowest = core::hint::black_box(frame.as_mut_ptr().cast::<u64>());
    // SAFETY: `lowest` is the first word of this frame's own buffer.
    unsafe { lowest.write_volatile(depth) };
    if core::hint::black_box(depth) == u64::MAX {
        return depth;
    }
    // SAFETY: written just above; the buffer lives until this returns.
    recurse_big(depth + 1) + unsafe { lowest.read_volatile() }
}

/// Recurse without end: each call keeps a 256-byte array live across the
/// next call (not a tail call; `black_box` stops LLVM folding it into a
/// loop), so the stack runs into its guard page and the crash reporter
/// names the overflow. The `u64::MAX` test never succeeds in practice.
#[inline(never)]
fn recurse(depth: u64) -> u64 {
    let frame = core::hint::black_box([depth; 32]);
    if core::hint::black_box(depth) == u64::MAX {
        return depth;
    }
    recurse(depth + 1) + frame[7]
}

/// Take a lock this CPU already holds: `SpinLock::lock` must panic (naming
/// the lock and this CPU) instead of spinning forever with interrupts off.
/// Says which lock first, so the test can compare the report with it.
#[inline(never)]
fn crash_relock() {
    static RELOCK: crate::sync::SpinLock<()> = crate::sync::SpinLock::new(());
    let _ = writeln!(Out, "crash: locking the SpinLock at {:p} twice on cpu {}", &RELOCK, arch::cpu_index());
    let first = RELOCK.lock();
    let second = RELOCK.lock(); // the re-lock: panics here
    // Only if `lock` handed the same lock out twice. A missed re-lock spins
    // in `lock` instead: on CPU 0 serial goes quiet; from another CPU the
    // shell says it hasn't finished.
    drop((second, first));
    let _ = writeln!(Out, "crash: SpinLock::lock returned while this cpu held it (mutual exclusion broken)");
}

/// Log while holding the log's line lock (`with_line_lock` forbids it):
/// the logger's `lock` must report the re-lock, and the report, which never
/// waits for that lock, must still come out whole.
#[inline(never)]
fn crash_relock_log() {
    crate::klog::with_line_lock(|| log::info!(target: "crash", "logging under the line lock"));
    let _ = writeln!(Out, "crash: logging under the line lock went unnoticed");
}

/// Never woken: the faults below wait on it for good, or rather panic or
/// halt their CPU on the way.
static NEVER: crate::sched::WaitQueue = crate::sched::WaitQueue::new();

/// Block with a lock held (a critical section open), no crash report under
/// way: `WaitQueue`'s wait must panic.
#[inline(never)]
fn crash_block_with_lock_held() {
    static HELD: crate::sync::SpinLock<()> = crate::sync::SpinLock::new(());
    let held = HELD.lock();
    NEVER.wait_until(|| false);
    drop(held);
    let _ = writeln!(Out, "crash: blocking with a lock held went unnoticed");
}

/// Block with interrupts off but no critical section open, no crash report
/// under way, in a wait with a deadline: it must panic too (interrupts off
/// alone don't mean another CPU's report is stopping this one).
#[inline(never)]
fn crash_block_with_interrupts_off() {
    let were_on = arch::irq::disable();
    NEVER.wait_until_or_deadline(u64::MAX, || false);
    arch::irq::restore(were_on);
    let _ = writeln!(Out, "crash: blocking with interrupts off went unnoticed");
}

/// Turn interrupts off for good here, as this CPU's own crash path or a
/// reboot would, then block: the wait must panic (a report then follows,
/// or a crash during this CPU's own report says so), not halt this CPU
/// quietly as it does for another CPU's report. If another CPU runs
/// threads, a thread there turns them off for good too once this CPU has
/// (and spins), before this CPU blocks: as on a report's owner, whose
/// stopped CPUs run `disable_for_good` after it. All of it in a critical
/// section here until the wait, so no other thread runs on this CPU
/// meanwhile.
#[inline(never)]
fn crash_block_after_disabling_for_good() {
    static OFF_THERE: AtomicBool = AtomicBool::new(false);
    let section = arch::irq::push_off();
    let other = other_cpu();
    if let Some(other) = other {
        let follower = crate::sched::spawn_on(other, "for-good", || {
            while !arch::irq::disabled_for_good() {
                core::hint::spin_loop();
            }
            arch::irq::disable_for_good();
            OFF_THERE.store(true, Ordering::Release);
            loop {
                core::hint::spin_loop();
            }
        });
        if let Err(e) = follower {
            drop(section);
            let _ = writeln!(Out, "crash: no thread on cpu {other}: {e}");
            return;
        }
    }
    arch::irq::disable_for_good();
    if let Some(other) = other
        && !crate::smp::spin_until(crate::time::now_ns() + WAIT_NS, || OFF_THERE.load(Ordering::Acquire))
    {
        panic!("cpu {other} didn't turn interrupts off for good within {WAIT_S} s of this one");
    }
    // Interrupts stay off.
    drop(section);
    NEVER.wait_until(|| false);
    let _ = writeln!(Out, "crash: blocking with interrupts off for good went unnoticed");
}

/// How long the faults below wait for another CPU, at most, each time
/// (each fault's waits add up to less than the shell's 10 s).
const WAIT_S: u64 = 4;
const WAIT_NS: u64 = WAIT_S * 1_000_000_000;

/// The highest CPU but this one that runs threads, if any.
fn other_cpu() -> Option<usize> {
    let me = arch::cpu_index();
    (0..arch::MAX_CPUS).rev().find(|&c| c != me && crate::sched::running_on(c))
}

/// What the thread on the other CPU does in [`crash_with_a_bystander`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bystander {
    /// Closes its critical section, then blocks.
    Leaves,
    /// Closes its section and yields to a new thread, which blocks on its
    /// first run.
    FirstRun,
    /// Blocks with its section still open.
    Holds,
}

/// This CPU's crash begins (interrupts off for good) while a thread on
/// another CPU is in a critical section; that thread then leaves it,
/// interrupts staying off, and blocks before the report's stop NMI has
/// reached its CPU: it must halt for this CPU's report, not panic on the
/// can't-block check (a second crash: #106, where a screen shell on the
/// boot CPU did so).
#[inline(never)]
fn crash_with_a_bystander_blocking() {
    crash_with_a_bystander(Bystander::Leaves);
}

/// [`crash_with_a_bystander_blocking`], but the thread that blocks on the
/// other CPU is a new one, queued there before this CPU's crash begins and
/// first run after (the first thread yields to it): its entry leaves
/// interrupts off, as they are off for good.
#[inline(never)]
fn crash_with_a_bystander_starting() {
    crash_with_a_bystander(Bystander::FirstRun);
}

/// [`crash_with_a_bystander_blocking`], but the thread blocks with its
/// critical section still open (a lock held): a bug whatever else is
/// going on, so it must panic and, being first, own the report.
#[inline(never)]
fn crash_with_a_bystander_holding() {
    crash_with_a_bystander(Bystander::Holds);
}

/// The three above, with the race made certain. A thread on the other CPU
/// (see [`other_cpu`]) waits in a critical section, so nothing else runs
/// there, until interrupts are off for good; then it does what `what`
/// says. This CPU turns interrupts off for good, as its crash path does
/// before claiming the report, once that thread is in its section (the
/// other CPU not halted yet, checked), and faults only once the other CPU
/// has halted for the report to come (with `FirstRun`, after checking it
/// was the new thread's wait that halted it). With `Holds` the other CPU
/// must not halt: its panic's report stops this one while it waits. Each
/// wait is [`WAIT_S`] at most, then it panics saying which.
fn crash_with_a_bystander(what: Bystander) {
    static IN_SECTION: AtomicBool = AtomicBool::new(false);
    static FIRST_RAN: AtomicBool = AtomicBool::new(false);
    let me = arch::cpu_index();
    let Some(other) = other_cpu() else {
        let _ = writeln!(Out, "crash: no other cpu runs threads");
        return;
    };
    let bystander = crate::sched::spawn_on(other, "bystander", move || {
        let section = arch::irq::push_off();
        IN_SECTION.store(true, Ordering::Release);
        while !arch::irq::disabled_for_good() {
            core::hint::spin_loop();
        }
        if what == Bystander::Holds {
            NEVER.wait_until(|| false);
        }
        // Interrupts stay off: they were turned off for good meanwhile.
        drop(section);
        if what == Bystander::FirstRun {
            crate::sched::yield_now();
        }
        NEVER.wait_until(|| false);
    });
    if let Err(e) = bystander {
        let _ = writeln!(Out, "crash: no thread on cpu {other}: {e}");
        return;
    }
    if !crate::smp::spin_until(crate::time::now_ns() + WAIT_NS, || IN_SECTION.load(Ordering::Acquire)) {
        panic!("the bystander thread on cpu {other} wasn't in its critical section within {WAIT_S} s");
    }
    if what == Bystander::FirstRun {
        // Queued behind the bystander, which holds the CPU until it yields.
        let second = crate::sched::spawn_on(other, "first-run", || {
            FIRST_RAN.store(true, Ordering::Release);
            NEVER.wait_until(|| false);
        });
        if let Err(e) = second {
            panic!("no second thread on cpu {other}: {e}");
        }
    }
    assert!(!crate::crash::halted(other), "cpu {other} halted before a crash began");
    arch::irq::disable_for_good();
    if !crate::smp::spin_until(crate::time::now_ns() + WAIT_NS, || crate::crash::halted(other)) {
        panic!("cpu {other} didn't halt within {WAIT_S} s after cpu {me} turned interrupts off for good");
    }
    if what == Bystander::Holds {
        panic!("cpu {other} halted quietly, its thread blocking with a lock held");
    }
    if what == Bystander::FirstRun && !FIRST_RAN.load(Ordering::Acquire) {
        panic!("cpu {other} halted before its new thread first ran");
    }
    crash_page_fault();
}

crate::ktest!(
    fn every_crash_kind_has_a_fault() {
        // `KINDS` (help, completion, `crash <kind> <cpu>`) and `crash_fn`
        // list the same kinds.
        use crate::kassert;
        for kind in KINDS {
            if crash_fn(kind).is_none() {
                crate::ktest::diag(format_args!("no fault for {kind}"));
            }
            kassert!(crash_fn(kind).is_some());
        }
        Ok(())
    }
);
