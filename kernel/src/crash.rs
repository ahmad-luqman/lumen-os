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
use core::sync::atomic::{AtomicBool, Ordering};

pub const BEGIN_MARKER: &str = "==== LUMEN CRASH REPORT ====";
pub const END_MARKER: &str = "==== END CRASH REPORT ====";
/// Printed once the report is fully drawn on the framebuffer.
pub const SCREEN_MARKER: &str = "(crash report shown on screen)";

const MAX_FRAMES: usize = 32;
/// How many trace events a crash report shows.
const RECENT_TRACE_EVENTS: u64 = 16;

static CRASHING: AtomicBool = AtomicBool::new(false);
/// Set while the serial-only tail (page walk, heap state) is being written: a
/// fault there still ends the report properly.
static IN_TAIL: AtomicBool = AtomicBool::new(false);

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
    enter(frame.vector, frame.rip);
    let cr2 = arch::read_cr2();
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
    enter(u64::MAX, 0);
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
    if CRASHING.swap(true, Ordering::SeqCst) {
        let mut s = SerialWriter;
        if IN_TAIL.load(Ordering::SeqCst) {
            // Everything but the tail is out: say what happened and close the
            // report so it still parses.
            let _ = writeln!(
                s,
                "\n!!! fault while writing the page walk or heap state (vector {vector:#x}, rip {rip:#018x})"
            );
            let _ = writeln!(s, "{END_MARKER}");
            finish();
        }
        let _ = writeln!(s, "\n!!! nested crash while reporting (vector {vector:#x}, rip {rip:#018x}); halting");
        finish();
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
    // SAFETY: the crash path: one CPU, interrupts off (until #7), CRASHING
    // keeps a second report out, and we halt next, so whatever was using the
    // heap never resumes.
    let _ = unsafe { crate::mm::heap::write_crash_stats(&mut s) };
    IN_TAIL.store(false, Ordering::SeqCst);
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
    // An overflow faults in the guard page; the #PF can't be delivered on
    // the full stack, so it usually arrives as a #DF (on its IST stack) with
    // CR2 still in the guard page.
    if (f.vector == 14 || f.vector == 8)
        && let Some(r) = arch::stacks::guard_hit(cr2)
    {
        let _ = writeln!(
            w,
            "kernel stack overflow (guard page hit): cr2 {cr2:#018x} is in the guard page below the {} stack \
             [{:#x}, {:#x})",
            r.kind.name(),
            r.range.bottom,
            r.range.top
        );
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
    while n < MAX_FRAMES {
        if rbp == 0 || !rbp.is_multiple_of(8) || !arch::stacks::contains(rbp, 16) {
            break;
        }
        // SAFETY: [rbp, rbp+16) lies inside a known, mapped kernel stack.
        let (next, ret) = unsafe { ((rbp as *const u64).read(), (rbp as *const u64).add(1).read()) };
        if !is_kernel_text(ret) {
            break;
        }
        // A return address points just past the call; look up `ret - 1` so a
        // call at the very end of a function still names that function.
        writeln!(w, "  #{n:<2} {ret:#018x} <{}>", Symbolized(ret - 1))?;
        n += 1;
        // Frames move toward the top of their stack; the one link that may
        // go elsewhere is into another known stack (the main stack's first
        // frame links back into `kmain` on the boot stack). MAX_FRAMES
        // bounds the walk either way.
        if next <= rbp && arch::stacks::bounds_of(next) == arch::stacks::bounds_of(rbp) {
            break;
        }
        rbp = next;
    }
    if n == 0 {
        writeln!(w, "  (no frames)")?;
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
