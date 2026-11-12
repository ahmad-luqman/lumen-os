//! Kernel monitor: a tiny line-based console on serial.
//!
//! This is a debugging and testing aid, not the shell. It exists so the test
//! harness can script the kernel over serial (and trigger deliberate faults)
//! before there are processes. It is removed when the user-mode shell lands
//! (M2) and the scenario tests move to the shell.

use crate::arch::{self, serial};
use core::arch::asm;
use core::fmt::Write;

const PROMPT: &str = "kmon> ";
/// What `crash <kind>` accepts.
const CRASH_KINDS: &str = "pf ud de gp df wtext nx panic oom dfree heapcorrupt stack bigframe guard";
const MAX_LINE: usize = 128;

pub fn run() -> ! {
    log::info!(target: "kmon", "kernel monitor ready; type 'help'");
    let mut buf = [0u8; MAX_LINE];
    loop {
        serial::write_bytes(PROMPT.as_bytes());
        let len = read_line(&mut buf);
        let line = core::str::from_utf8(&buf[..len]).unwrap_or("").trim();
        dispatch(line);
    }
}

/// Read one line with echo and backspace handling.
fn read_line(buf: &mut [u8]) -> usize {
    let mut len = 0;
    loop {
        let Some(b) = serial::try_read_byte() else {
            core::hint::spin_loop();
            continue;
        };
        match b {
            b'\r' | b'\n' => {
                serial::write_bytes(b"\n");
                return len;
            }
            0x08 | 0x7f if len > 0 => {
                len -= 1;
                serial::write_bytes(b"\x08 \x08");
            }
            0x20..=0x7e if len < buf.len() => {
                buf[len] = b;
                len += 1;
                serial::write_byte(b);
            }
            _ => {}
        }
    }
}

fn dispatch(line: &str) {
    let mut out = serial::SerialWriter;
    let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
    match cmd {
        "" => {}
        "help" => {
            let _ = writeln!(
                out,
                "commands:\n  help              this list\n  echo <text>       print text\n  \
                 int3              take a breakpoint and continue\n  \
                 crash <kind>      deliberate fault: {CRASH_KINDS}\n  \
                 trace [n]         the last n trace events (default 16)\n  \
                 heap              heap usage and free-list summary\n  \
                 ticks             timer interrupts taken (and stray ones)\n  \
                 sleep <ms>        wait that long (guest time)\n  \
                 uptime            nanoseconds since the TSC started\n  \
                 clocks            the TSC's frequency, measured again against each clock\n  \
                 exit              end QEMU (only with lumen.qemu_exit)"
            );
        }
        "echo" => {
            let _ = writeln!(out, "{rest}");
        }
        "int3" => {
            // SAFETY: #BP is handled and returns (see idt::lumen_exception_dispatch).
            unsafe { asm!("int3") };
            let n = arch::idt::BREAKPOINTS.load(core::sync::atomic::Ordering::Relaxed);
            let _ = writeln!(out, "breakpoints taken: {n}");
        }
        "crash" => crash(rest.trim()),
        "trace" => match rest.trim() {
            "" => trace(16),
            arg => match arg.parse() {
                Ok(n) => trace(n),
                Err(_) => {
                    let _ = writeln!(out, "usage: trace [n]");
                }
            },
        },
        "heap" => {
            let _ = crate::mm::heap::write_stats(&mut out);
        }
        "ticks" => {
            let s = arch::idt::strays();
            let _ = writeln!(
                out,
                "ticks: {} (spurious {}, unexpected {}, local APIC errors {})",
                crate::time::ticks(),
                s.spurious,
                s.unexpected,
                s.lapic_errors
            );
        }
        "sleep" => match rest.trim().parse::<u64>() {
            Ok(ms) if ms <= 60_000 => {
                crate::time::sleep(ms * 1_000_000);
                let _ = writeln!(out, "slept {ms} ms");
            }
            _ => {
                let _ = writeln!(out, "usage: sleep <ms> (up to 60000)");
            }
        },
        "uptime" => {
            let _ = writeln!(out, "uptime: {} ns", crate::time::now_ns());
        }
        "clocks" => clocks(),
        "exit" => {
            if crate::cmdline::qemu_exit_enabled() {
                arch::qemu::exit(arch::qemu::ExitCode::Success);
            }
            let _ = writeln!(out, "exit: lumen.qemu_exit not set");
        }
        _ => {
            let _ = writeln!(out, "unknown command: {cmd} (try 'help')");
        }
    }
}

/// The TSC's boot calibration, and its frequency measured again now against
/// the PIT and the HPET (the tests compare them).
fn clocks() {
    use crate::time::{self, Reference};
    let mut out = serial::SerialWriter;
    let (Some(tsc), Some(reference)) = (time::tsc(), time::reference()) else {
        let _ = writeln!(out, "clocks: the TSC isn't calibrated");
        return;
    };
    let _ = write!(out, "clocks: tsc {} Hz (against the {reference} at boot)", tsc.hz());
    for r in [Reference::Pit, Reference::Hpet] {
        let _ = match time::measure(r) {
            Some(Ok(hz)) => write!(out, "; {r} {hz} Hz"),
            Some(Err(e)) => write!(out, "; {r} failed ({e})"),
            None => write!(out, "; {r} none"),
        };
    }
    let _ = writeln!(out);
}

fn trace(n: u64) {
    let mut out = serial::SerialWriter;
    let ring = crate::trace::ring();
    let cap = ring.capacity() as u64;
    if n > cap {
        let _ = writeln!(out, "(the ring holds {cap} events; showing those)");
    }
    let _ = crate::trace::write_recent(&mut out, n, "");
    let _ = writeln!(out, "trace: {} events claimed, {} dropped", ring.written(), ring.dropped());
}

fn crash(kind: &str) {
    match kind {
        "pf" => crash_page_fault(),
        "ud" => crash_invalid_opcode(),
        "de" => crash_divide_error(),
        "gp" => crash_general_protection(),
        "df" => crash_double_fault(),
        "wtext" => crash_write_text(),
        "nx" => crash_execute_data(),
        "panic" => crash_panic(),
        "oom" => crash_out_of_memory(),
        "dfree" => crash_double_free(),
        "heapcorrupt" => crash_corrupt_heap(),
        "guard" => crash_touch_guard(),
        "stack" => crash_stack_overflow(),
        "bigframe" => crash_big_frame_overflow(),
        _ => {
            let _ = writeln!(serial::SerialWriter, "crash: unknown kind '{kind}' ({CRASH_KINDS})");
        }
    }
}

/// Address that is never mapped: the kernel's page tables map nothing in the
/// lower half.
pub const UNMAPPED_ADDR: u64 = 0xdead_b000;

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
    let _ = writeln!(serial::SerialWriter, "crash: write to .text succeeded (W^X broken)");
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
    let _ = writeln!(serial::SerialWriter, "crash: executing .data succeeded (NX broken)");
}

#[inline(never)]
fn crash_panic() {
    panic!("deliberate panic from kmon");
}

/// Ask the heap for more than its window holds (2 TiB) through an ordinary
/// infallible allocation: the null ends in `handle_alloc_error`'s panic.
/// (`black_box` keeps LLVM from deleting an allocation nobody looks at, and
/// with it the failure: it may assume such allocations succeed.)
#[inline(never)]
fn crash_out_of_memory() {
    drop(core::hint::black_box(alloc::vec::Vec::<u8>::with_capacity(1 << 41)));
    let _ = writeln!(serial::SerialWriter, "crash: a 2 TiB allocation succeeded (heap limit broken)");
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

/// Free a block twice after its lower neighbour was freed (so it merged into
/// the neighbour's free block): the heap must refuse the second free (a
/// panic), not corrupt its free list. A 128-byte block stays live, so the
/// report shows one more block in use than at the prompt
/// (`test_double_free_is_refused` checks it).
#[inline(never)]
fn crash_double_free() {
    use core::hint::black_box;
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
    let _ = writeln!(serial::SerialWriter, "crash: a double free went unnoticed");
}

/// Write into a freed block (a use after free) so its free-list link points
/// back at itself, then allocate something too big for it (a walk without
/// the link check would follow the self-link forever): the walk must panic,
/// and the report must still show the heap (its lock is held).
#[inline(never)]
fn crash_corrupt_heap() {
    use core::hint::black_box;
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
    let _ = writeln!(serial::SerialWriter, "crash: a corrupt free list went unnoticed");
}

/// Write into the guard page below the main stack from a healthy stack: a
/// plain #PF (nothing overflowed), which the report names as a stray guard
/// page hit, not an overflow.
#[inline(never)]
fn crash_touch_guard() {
    use lumen_mm::stacks::StackKind;
    let Some(main) = arch::stacks::REGISTRY.iter().find(|r| r.kind == StackKind::Main) else {
        let _ = writeln!(serial::SerialWriter, "crash: no main stack registered");
        return;
    };
    // SAFETY: deliberately faulting: the guard page is never mapped.
    unsafe { core::ptr::write_volatile((main.range.guard() + 8) as *mut u64, 1) };
    let _ = writeln!(serial::SerialWriter, "crash: writing to a guard page succeeded (guard page mapped?)");
}

/// Overflow the main stack with small frames.
#[inline(never)]
fn crash_stack_overflow() {
    let depth = recurse(0);
    let _ = writeln!(serial::SerialWriter, "crash: recursion returned at depth {depth} (no guard page?)");
}

/// Overflow the main stack with frames bigger than a page: stack probes must
/// make it fault in the guard page instead of jumping over it into the
/// mapped stack below.
#[inline(never)]
fn crash_big_frame_overflow() {
    let depth = recurse_big(0);
    let _ = writeln!(serial::SerialWriter, "crash: big-frame recursion returned at depth {depth} (no guard page?)");
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
/// loop), so the main stack runs into its guard page and the crash reporter
/// names the overflow. The `u64::MAX` test never succeeds in practice.
#[inline(never)]
fn recurse(depth: u64) -> u64 {
    let frame = core::hint::black_box([depth; 32]);
    if core::hint::black_box(depth) == u64::MAX {
        return depth;
    }
    recurse(depth + 1) + frame[7]
}
