//! Crash reporter: every CPU exception and every panic ends here.
//!
//! Signature feature 5: no silent triple faults. The report goes to serial
//! (for the test harness and `make run` logs) and to the screen, and contains
//! the exception, decoded error code, faulting address, registers, and a
//! symbolized frame-pointer backtrace.
//!
//! The reporter must not make things worse: it takes no locks it could
//! deadlock on, follows a frame pointer only if it points into a known stack
//! ([`crate::arch::stacks::contains`]), and detects a crash while reporting a
//! crash (it then prints a one-line notice and stops).
//!
//! The output format is parsed by `tests/scenarios/test_crash.py`; change
//! both together.

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

static CRASHING: AtomicBool = AtomicBool::new(false);

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
    emit(&|w: &mut dyn Write| write_exception(w, frame, cr2));
    finish()
}

/// Report a Rust panic and stop.
pub fn panic(info: &PanicInfo<'_>) -> ! {
    enter(u64::MAX, 0);
    let rbp = arch::read_rbp();
    emit(&|w: &mut dyn Write| {
        let _ = writeln!(w, "panic: {}", info.message());
        if let Some(loc) = info.location() {
            let _ = writeln!(w, "  at {}:{}:{}", loc.file(), loc.line(), loc.column());
        }
        let _ = write_backtrace(w, None, rbp);
    });
    finish()
}

/// Guard against a crash inside the crash reporter.
fn enter(vector: u64, rip: u64) {
    if CRASHING.swap(true, Ordering::SeqCst) {
        let mut s = SerialWriter;
        let _ = writeln!(s, "\n!!! nested crash while reporting (vector {vector:#x}, rip {rip:#018x}); halting");
        finish();
    }
}

fn emit(body: &dyn Fn(&mut dyn Write)) {
    let mut s = SerialWriter;
    let _ = writeln!(s, "\n{BEGIN_MARKER}");
    body(&mut s);
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
        if next <= rbp {
            break; // frames must move toward the stack top
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
        struct Buf {
            data: [u8; 96],
            len: usize,
        }
        impl core::fmt::Write for Buf {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let end = self.len + s.len();
                self.data.get_mut(self.len..end).ok_or(core::fmt::Error)?.copy_from_slice(s.as_bytes());
                self.len = end;
                Ok(())
            }
        }
        let mut b = Buf { data: [0; 96], len: 0 };
        let _ = write!(b, "{}", PageFaultCause(0b00010));
        crate::kassert_eq!(
            core::str::from_utf8(&b.data[..b.len]).unwrap_or(""),
            "page fault: write of non-present page (kernel mode)"
        );
        Ok(())
    }
);
