//! Crash reporter: every CPU exception and every panic ends here.
//!
//! Signature feature 5: no silent triple faults. The report goes to serial
//! (for the test harness and `make run` logs) and to the screen, and contains
//! the exception, decoded error code, faulting address, registers and a
//! symbolized frame-pointer backtrace; on serial only, it then gives the last
//! 16 trace events (what led up to it), for a page fault the page-table walk
//! of the faulting address, and the heap's state.
//!
//! The reporter must not make things worse: it takes no locks it could
//! deadlock on, follows a frame pointer only if it points into a known stack
//! ([`crate::arch::stacks::contains`]), and detects a crash while reporting a
//! crash (it then prints a one-line notice and stops).
//!
//! The output format is parsed by `tools/harness/lumen_harness/crash.py`
//! (markers, backtrace, page walk), `tests/scenarios/test_crash.py` and, for
//! the heap line (`Snapshot::write` in `mm::heap`), `tests/scenarios/test_mm.py`;
//! change them together.

use crate::arch::{self, TrapFrame, serial::SerialWriter};
use crate::symbols::Symbolized;
use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use lumen_mm::stacks::{Hit, Registered};

pub const BEGIN_MARKER: &str = "==== LUMEN CRASH REPORT ====";
pub const END_MARKER: &str = "==== END CRASH REPORT ====";
/// Printed once the report is fully drawn on the framebuffer.
pub const SCREEN_MARKER: &str = "(crash report shown on screen)";

const MAX_FRAMES: usize = 32;
/// How many trace events a crash report shows.
const RECENT_TRACE_EVENTS: u64 = 16;

static CRASHING: AtomicBool = AtomicBool::new(false);
/// CR2 when the first report began: a nested crash only blames a guard page
/// if CR2 has changed since (only a #PF sets it).
static FIRST_CR2: AtomicU64 = AtomicU64::new(0);
/// Set while the serial-only tail (page walk, heap state) is being written: a
/// fault there still ends the report properly.
static IN_TAIL: AtomicBool = AtomicBool::new(false);
/// Whether interrupts were on when the report began (after
/// `disable_for_good`: they must not be).
static ON_AT_ENTRY: AtomicBool = AtomicBool::new(false);

unsafe extern "C" {
    static __text_start: u8;
    static __text_end: u8;
}

fn is_kernel_text(addr: u64) -> bool {
    let start = &raw const __text_start as u64;
    let end = &raw const __text_end as u64;
    (start..end).contains(&addr)
}

/// Report a fatal CPU exception and stop.
pub fn exception(frame: &TrapFrame) -> ! {
    // Interrupts are already off (an interrupt gate); keep them off.
    arch::irq::disable_for_good();
    enter(frame.vector, frame.rip);
    let cr2 = arch::read_cr2();
    FIRST_CR2.store(cr2, Ordering::SeqCst);
    let walk = |w: &mut dyn Write| {
        if frame.vector == 14 {
            // Lock-free: the fault may have happened with the page-table
            // lock held. A table entry pointing outside RAM ends the walk
            // instead of faulting.
            let _ = crate::mm::paging::write_walk(w, cr2);
        }
    };
    emit(&|w: &mut dyn Write| write_exception(w, frame, cr2), &walk);
    finish()
}

/// Report a Rust panic and stop.
pub fn panic(info: &PanicInfo<'_>) -> ! {
    // Before anything else: a panic can come with interrupts on, and a
    // timer interrupt must not run (or a lock guard turn them back on)
    // while the report is written.
    arch::irq::disable_for_good();
    enter(u64::MAX, 0);
    FIRST_CR2.store(arch::read_cr2(), Ordering::SeqCst);
    crate::ktrace!(crate::trace::Kind::Panic, info.location().map_or(0, |l| l.line()));
    let rbp = arch::read_rbp();
    emit(
        &|w: &mut dyn Write| {
            let _ = writeln!(w, "panic: {}", info.message());
            if let Some(loc) = info.location() {
                let _ = writeln!(w, "  at {}:{}:{}", loc.file(), loc.line(), loc.column());
            }
            let _ = write_backtrace(w, None, rbp);
        },
        &|_| {},
    );
    finish()
}

/// Guard against a crash inside the crash reporter.
fn enter(vector: u64, rip: u64) {
    ON_AT_ENTRY.store(arch::irq::enabled(), Ordering::SeqCst);
    if CRASHING.swap(true, Ordering::SeqCst) {
        let mut s = SerialWriter;
        if IN_TAIL.load(Ordering::SeqCst) {
            // Everything but the tail is out: say what happened and close the
            // report so it still parses.
            let _ = writeln!(
                s,
                "\n!!! fault while writing the page walk or heap state (vector {vector:#x}, rip {rip:#018x})"
            );
            write_nested_guard_hit(&mut s, vector);
            let _ = writeln!(s, "{END_MARKER}");
            finish();
        }
        let _ = writeln!(s, "\n!!! nested crash while reporting (vector {vector:#x}, rip {rip:#018x}); halting");
        write_nested_guard_hit(&mut s, vector);
        finish();
    }
}

/// A nested #PF or #DF whose CR2 changed since the report began and is in a
/// guard page most likely means the reporter overflowed the stack it runs
/// on: say so (lock-free, bounded). Other nested crashes don't set CR2.
fn write_nested_guard_hit(w: &mut dyn Write, vector: u64) {
    write_nested_note(w, vector, arch::read_cr2(), FIRST_CR2.load(Ordering::SeqCst));
}

/// [`write_nested_guard_hit`] with the registers passed in (testable).
fn write_nested_note(w: &mut dyn Write, vector: u64, cr2: u64, first_cr2: u64) {
    if !matches!(vector, 8 | 14) || cr2 == first_cr2 {
        return;
    }
    if let Some(r) = arch::stacks::guard_hit(cr2) {
        let _ = writeln!(
            w,
            "!!! cr2 {cr2:#018x} is in the guard page below the {} stack: the crash reporter may have overflowed it",
            r.kind.name()
        );
    }
}

/// Write the report: `body` to serial and the screen; the trace events, then
/// `tail` and the heap's state (the riskiest parts, last) to serial only.
fn emit(body: &dyn Fn(&mut dyn Write), tail: &dyn Fn(&mut dyn Write)) {
    let mut s = SerialWriter;
    let _ = writeln!(s, "\n{BEGIN_MARKER}");
    body(&mut s);
    // What led up to it (serial only: the screen has no room). The ring is
    // lock-free, so reading it here can't deadlock.
    // (SerialWriter never fails, so nothing is cut short silently.)
    let ring = crate::trace::ring();
    let _ = writeln!(s, "recent trace events (oldest first):");
    let _ = crate::trace::write_recent(&mut s, RECENT_TRACE_EVENTS, "  ");
    let _ = writeln!(s, "trace: {} events claimed, {} dropped", ring.written(), ring.dropped());
    // If the tail faults, `enter` closes the report (the screen copy is then
    // lost; serial has everything else).
    IN_TAIL.store(true, Ordering::SeqCst);
    tail(&mut s);
    // Last: it trusts the heap's own bounds (base, length); if those are
    // corrupt the walk can fault, and `IN_TAIL` then closes the report.
    // Doesn't block (reads the heap anyway if its lock is held); the
    // free-list walk is bounded and stays inside those bounds.
    // SAFETY: the crash path: one CPU, interrupts off for good, CRASHING
    // keeps a second report out, and we halt next, so whatever was using the
    // heap never resumes.
    let _ = unsafe { crate::mm::heap::write_crash_stats(&mut s) };
    IN_TAIL.store(false, Ordering::SeqCst);
    // Interrupts must have been off from the start (`disable_for_good`) and
    // still be, now that every lock taken while reporting is released.
    let state = match (ON_AT_ENTRY.load(Ordering::SeqCst), arch::irq::enabled()) {
        (false, false) => "off",
        (true, _) => "on when the report began (the crash path didn't turn them off)",
        (false, true) => "on (the crash path turned them back on)",
    };
    let _ = writeln!(s, "interrupts: {state}");
    let _ = writeln!(s, "{END_MARKER}");
    // Serial first: if drawing the screen faults, the serial report survives.
    // SAFETY: we are on the crash path; CRASHING guarantees we run alone.
    if unsafe { crate::fb::with_crash_screen(|w| body(w)) } {
        let _ = writeln!(s, "{SCREEN_MARKER}");
    }
}

fn finish() -> ! {
    if crate::cmdline::qemu_exit_enabled() {
        arch::qemu::exit(arch::qemu::ExitCode::Crash);
    }
    arch::halt_forever()
}

fn write_exception(w: &mut dyn Write, f: &TrapFrame, cr2: u64) {
    let _ = writeln!(
        w,
        "exception: {} (vector {}, error code {:#x})",
        arch::idt::exception_name(f.vector),
        f.vector,
        f.error_code
    );
    if f.vector == 14 {
        let _ = writeln!(w, "  {}", PageFaultCause(f.error_code));
        let _ = writeln!(w, "  faulting address (cr2): {cr2:#018x}");
    }
    if matches!(f.vector, 8 | 14)
        && let Some(r) = arch::stacks::guard_hit(cr2)
    {
        write_guard_hit(w, f, cr2, r);
    }
    let _ = writeln!(w, "rip: {:#018x} <{}>", f.rip, Symbolized(f.rip));
    let _ = writeln!(w, "rsp: {:#018x}  rflags: {:#010x}  cs: {:#06x}  ss: {:#06x}", f.rsp, f.rflags, f.cs, f.ss);
    let regs = [
        ("rax", f.rax),
        ("rbx", f.rbx),
        ("rcx", f.rcx),
        ("rdx", f.rdx),
        ("rsi", f.rsi),
        ("rdi", f.rdi),
        ("rbp", f.rbp),
        ("r8 ", f.r8),
        ("r9 ", f.r9),
        ("r10", f.r10),
        ("r11", f.r11),
        ("r12", f.r12),
        ("r13", f.r13),
        ("r14", f.r14),
        ("r15", f.r15),
    ];
    for row in regs.chunks(4) {
        for (name, v) in row {
            let _ = write!(w, "{name}: {v:#018x}  ");
        }
        let _ = writeln!(w);
    }
    let _ = writeln!(
        w,
        "cr0: {:#018x}  cr3: {:#018x}  cr4: {:#018x}",
        arch::read_cr0(),
        arch::read_cr3(),
        arch::read_cr4()
    );
    let _ = write_backtrace(w, Some(f.rip), f.rbp);
}

/// A fault whose CR2 is in a stack's guard page. It is that stack
/// overflowing only on a #DF with the interrupted `rsp` in the guard page or
/// the stack's bottom page: an overflow's #PF can't be delivered on the full
/// stack, so it arrives as a #DF on its IST stack, CR2 still in the guard
/// page. A #PF that was delivered had room on its stack, so it is a stray
/// access, or the stack below running off its top (`rsp` in or at the top of
/// that stack). On a #DF the verdict is hedged: nothing sets CR2 on one, and
/// the state it saves is not architecturally defined.
fn write_guard_hit(w: &mut dyn Write, f: &TrapFrame, cr2: u64, r: Registered) {
    let (name, bottom, top) = (r.kind.name(), r.range.bottom, r.range.top);
    let double_fault = f.vector == 8;
    let overflow = double_fault && (r.range.guard()..bottom + lumen_mm::FRAME_SIZE).contains(&f.rsp);
    let headline = if overflow { "kernel stack overflow (guard page hit)" } else { "guard page hit" };
    let _ =
        write!(w, "{headline}: cr2 {cr2:#018x} is in the guard page below the {name} stack [{bottom:#x}, {top:#x})");
    if overflow {
        let _ = writeln!(w);
        return;
    }
    // The stack whose top is this guard page, if any.
    let below = r.range.guard().checked_sub(1).and_then(|a| match arch::stacks::REGISTRY.find(a) {
        Some(Hit::Stack(b)) if b.range.top == r.range.guard() => Some(b),
        _ => None,
    });
    if let Some(b) = below {
        let _ = write!(w, ", above the {} stack", b.kind.name());
    }
    let rsp = f.rsp;
    match below {
        Some(b) if (b.range.bottom..=b.range.top).contains(&rsp) => {
            let _ = write!(w, "; rsp {rsp:#018x} is in the {} stack: it may have run off its top", b.kind.name());
        }
        _ if double_fault => {
            let _ = write!(
                w,
                "; rsp {rsp:#018x} is not near it: probably a stray access (on a #DF, cr2 may be stale and the \
                 saved state is not architecturally defined)"
            );
        }
        _ => {
            let _ = write!(w, "; rsp {rsp:#018x} is not near it: a stray access, not an overflow");
        }
    }
    let _ = writeln!(w);
}

/// Why a backtrace stopped before reaching the end of the chain (a null
/// frame pointer, set by `_start`).
enum Stop {
    Limit,
    UnknownFrame(u64),
    NotKernelCode(u64),
    NotUpward(u64),
    SecondSwitch(u64),
}

/// Decoded #PF error code.
struct PageFaultCause(u64);

impl fmt::Display for PageFaultCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let e = self.0;
        let access = if e & (1 << 4) != 0 {
            "instruction fetch"
        } else if e & (1 << 1) != 0 {
            "write"
        } else {
            "read"
        };
        let cause = if e & 1 != 0 { "protection violation" } else { "non-present page" };
        let mode = if e & (1 << 2) != 0 { "user" } else { "kernel" };
        write!(f, "page fault: {access} of {cause} ({mode} mode)")?;
        if e & (1 << 3) != 0 {
            f.write_str(", reserved bit set in page table")?;
        }
        Ok(())
    }
}

/// Walk the frame-pointer chain starting at `rbp`.
///
/// With `-C force-frame-pointers=yes` every function begins with
/// `push rbp; mov rbp, rsp`, so `[rbp]` holds the caller's rbp and `[rbp+8]`
/// the return address. We only dereference a frame that lies entirely inside a
/// known stack and stop at the first frame that looks wrong.
fn write_backtrace(w: &mut dyn Write, rip: Option<u64>, mut rbp: u64) -> fmt::Result {
    writeln!(w, "backtrace:")?;
    let mut n = 0;
    if let Some(rip) = rip {
        writeln!(w, "  #{n:<2} {rip:#018x} <{}>", Symbolized(rip))?;
        n += 1;
    }
    // Frames move toward the top of their stack. The chain may change stacks
    // once: the main stack's first frame links back into `kmain` on the boot
    // stack (`arch::switch_stack`).
    let mut switched = false;
    let stop = loop {
        if rbp == 0 {
            break None;
        }
        if n == MAX_FRAMES {
            break Some(Stop::Limit);
        }
        if !rbp.is_multiple_of(8) || !arch::stacks::contains(rbp, 16) {
            break Some(Stop::UnknownFrame(rbp));
        }
        // SAFETY: [rbp, rbp+16) lies inside a known, mapped kernel stack
        // (one CPU: a registered stack is mapped until it is retracted, and
        // the boot stack is never unmapped).
        let (next, ret) = unsafe { ((rbp as *const u64).read(), (rbp as *const u64).add(1).read()) };
        if !is_kernel_text(ret) {
            break Some(Stop::NotKernelCode(ret));
        }
        // A return address points just past the call; look up `ret - 1` so a
        // call at the very end of a function still names that function.
        writeln!(w, "  #{n:<2} {ret:#018x} <{}>", Symbolized(ret - 1))?;
        n += 1;
        let (here, there) = (arch::stacks::bounds_of(rbp), arch::stacks::bounds_of(next));
        if next != 0 && there.is_some() && there != here {
            if switched {
                break Some(Stop::SecondSwitch(next));
            }
            switched = true;
        } else if next != 0 && there == here && next <= rbp {
            break Some(Stop::NotUpward(next));
        }
        rbp = next;
    };
    if n == 0 {
        writeln!(w, "  (no frames)")?;
    }
    match stop {
        None => {}
        Some(Stop::Limit) => writeln!(w, "  (stopped after {MAX_FRAMES} frames)")?,
        Some(Stop::UnknownFrame(p)) => writeln!(w, "  (stopped: frame pointer {p:#x} is not in a known stack)")?,
        Some(Stop::NotKernelCode(r)) => writeln!(w, "  (stopped: return address {r:#x} is not kernel code)")?,
        Some(Stop::NotUpward(p)) => writeln!(w, "  (stopped: frame pointer {p:#x} does not move up its stack)")?,
        Some(Stop::SecondSwitch(p)) => {
            writeln!(w, "  (stopped: frame pointer {p:#x} would switch stacks a second time)")?
        }
    }
    Ok(())
}

crate::ktest!(
    fn page_fault_cause_is_decoded() {
        use core::fmt::Write as _;
        let mut b = crate::ktest::Buf::new();
        let _ = write!(b, "{}", PageFaultCause(0b00010));
        crate::kassert_eq!(b.as_str(), "page fault: write of non-present page (kernel mode)");
        Ok(())
    }
);

/// A trap frame with only `vector` and `rsp` set (for tests of the report).
#[cfg(feature = "ktest")]
fn trap_frame(vector: u64, rsp: u64) -> TrapFrame {
    TrapFrame {
        r15: 0,
        r14: 0,
        r13: 0,
        r12: 0,
        r11: 0,
        r10: 0,
        r9: 0,
        r8: 0,
        rbp: 0,
        rdi: 0,
        rsi: 0,
        rdx: 0,
        rcx: 0,
        rbx: 0,
        rax: 0,
        vector,
        error_code: 0,
        rip: 0,
        cs: 0,
        rflags: 0,
        rsp,
        ss: 0,
    }
}

crate::ktest!(
    fn guard_hit_is_classified() {
        // A hit in the main stack's guard page (the #MC IST stack is right
        // below it): an overflow only on a #DF with rsp in the guard page or
        // the bottom page; a #PF never is one.
        use crate::kassert;
        use lumen_mm::FRAME_SIZE;
        use lumen_mm::stacks::StackKind;
        let find = |kind| arch::stacks::REGISTRY.iter().find(|r| r.kind == kind).ok_or("stack not registered");
        let (main, lowest) = (find(StackKind::Main)?, find(StackKind::DoubleFault)?);
        let line = |r: Registered, vector, rsp| {
            let mut buf = crate::ktest::Buf::new();
            write_guard_hit(&mut buf, &trap_frame(vector, rsp), r.range.guard() + 8, r);
            buf
        };
        let (guard, bottom) = (main.range.guard(), main.range.bottom);
        for rsp in [guard, guard + 8, bottom + FRAME_SIZE - 8] {
            kassert!(line(main, 8, rsp).as_str().starts_with("kernel stack overflow (guard page hit): "));
        }
        let far_df = line(main, 8, bottom + FRAME_SIZE);
        let far_df = far_df.as_str();
        kassert!(far_df.starts_with("guard page hit: ") && far_df.contains("above the #MC IST stack"));
        kassert!(far_df.contains("probably a stray access") && far_df.contains("cr2 may be stale"));
        let pf_at_guard = line(main, 14, guard);
        kassert!(pf_at_guard.as_str().starts_with("guard page hit: "));
        kassert!(pf_at_guard.as_str().contains("is in the #MC IST stack: it may have run off its top"));
        let far_pf = line(main, 14, bottom + 4 * FRAME_SIZE);
        kassert!(far_pf.as_str().ends_with("a stray access, not an overflow\n"));
        kassert!(!far_pf.as_str().contains("may be stale"));
        // The lowest stack in the window has nothing below it.
        let no_neighbour = line(lowest, 14, bottom);
        kassert!(no_neighbour.as_str().starts_with("guard page hit: ") && !no_neighbour.as_str().contains("above the"));
        Ok(())
    }
);

crate::ktest!(
    fn nested_crash_names_a_fresh_guard_hit_only() {
        // Only a nested #PF or #DF whose CR2 changed since the report began
        // blames a guard page: other faults (and panics) don't set CR2.
        use crate::kassert;
        let main = arch::stacks::REGISTRY
            .iter()
            .find(|r| r.kind == lumen_mm::stacks::StackKind::Main)
            .ok_or("no main stack")?;
        let cr2 = main.range.guard() + 8;
        let note = |vector, first| {
            let mut buf = crate::ktest::Buf::new();
            write_nested_note(&mut buf, vector, cr2, first);
            buf
        };
        kassert!(note(14, 0).as_str().contains("the crash reporter may have overflowed it"));
        kassert!(note(8, 0).as_str().contains("below the kernel main stack"));
        kassert!(note(14, cr2).as_str().is_empty()); // CR2 left over from the first fault
        kassert!(note(13, 0).as_str().is_empty());
        kassert!(note(u64::MAX, 0).as_str().is_empty()); // a panic
        Ok(())
    }
);

crate::ktest!(
    fn backtrace_allows_one_stack_switch() {
        // Planted frame records: one switch between known stacks is followed
        // (as from the main stack into the boot stack), a second is not, and
        // a link down inside one stack ends the walk; each stop says why.
        use crate::{kassert, kassert_eq};
        use lumen_mm::stacks::StackKind;
        let other = crate::mm::stack::KernelStack::new(4 * lumen_mm::FRAME_SIZE, StackKind::Test)
            .map_err(|_| "allocating a stack")?;
        let a = other.range().bottom; // above the main stack (allocated later)
        let ret = write_backtrace as *const () as u64 + 1; // inside kernel text
        let mut local = core::hint::black_box([0u64; 2]); // a record on the main stack
        let on_main = local.as_mut_ptr();
        let frames = |buf: &crate::ktest::Buf| buf.as_str().lines().filter(|l| l.starts_with("  #")).count();
        let walk = |records: &[(u64, u64, u64)], start| {
            for &(at, next, ret) in records {
                // SAFETY: each record is in the stack this test owns (nothing
                // runs on it) or in `local`; both stay valid while walked.
                unsafe {
                    (at as *mut u64).write_volatile(next);
                    (at as *mut u64).add(1).write_volatile(ret);
                }
            }
            let mut buf = crate::ktest::Buf::new();
            let _ = write_backtrace(&mut buf, None, start);
            buf
        };
        let (r1, r2, r3) = (a + 64, on_main as u64, a + 128);
        let twice = walk(&[(r1, r2, ret), (r2, r3, ret), (r3, 0, ret)], r1);
        kassert_eq!(frames(&twice), 2);
        kassert!(twice.as_str().contains("would switch stacks a second time"));
        let down = walk(&[(a + 128, a + 64, ret)], a + 128);
        kassert_eq!(frames(&down), 1);
        kassert!(down.as_str().contains("does not move up its stack"));
        core::hint::black_box(&mut local);
        drop(other);
        Ok(())
    }
);
