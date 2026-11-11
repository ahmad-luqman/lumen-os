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
                 crash <kind>      deliberate fault: pf ud de gp df panic\n  \
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

fn crash(kind: &str) {
    match kind {
        "pf" => crash_page_fault(),
        "ud" => crash_invalid_opcode(),
        "de" => crash_divide_error(),
        "gp" => crash_general_protection(),
        "df" => crash_double_fault(),
        "panic" => crash_panic(),
        _ => {
            let _ = writeln!(serial::SerialWriter, "crash: unknown kind '{kind}' (pf ud de gp df panic)");
        }
    }
}

/// Address that is never mapped: Limine base revision >= 3 maps nothing in
/// the lower half except what we ask for.
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

#[inline(never)]
fn crash_panic() {
    panic!("deliberate panic from kmon");
}
