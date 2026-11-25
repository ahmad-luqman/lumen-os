//! What M8's test programs share (`mod common;` in each; ARCHITECTURE
//! section 15, "M8's tests"): the raw `syscall`, a line written in one
//! `write`, the panic handler, argument bytes, and the checks' runner.
//!
//! A mode runs its checks through [`run_checks`]: one `ok <check>` line per
//! check passed; the first failed check prints `FAIL <check>: <what> (got
//! <value>)` and the program exits with the check's number (from 1), as
//! `m7_vfs` does. A mode has fewer than 64 checks, so that number is never
//! [`USAGE_EXIT`] (or 127, a panic's). A check about a syscall calls it by
//! number ([`sys`], the raw `-errno`); a check about libc's use of the
//! kernel calls musl.

#![allow(dead_code)]

use core::arch::asm;
use core::ffi::c_int;
use core::fmt;

// SAFETY: musl C ABI symbols resolved at runtime by `/lib/ld-musl-x86_64.so.1`
// via `libc.musl-x86_64.so.1`.
unsafe extern "C" {
    pub fn _exit(status: c_int) -> !;
}

/// libcore is built to unwind, so its objects name this symbol; with
/// `panic=abort` nothing ever calls it.
#[unsafe(no_mangle)]
extern "C" fn rust_eh_personality() {}

pub const WRITE: u64 = 1;

/// The exit code for arguments no mode takes (`EX_USAGE`).
pub const USAGE_EXIT: c_int = 64;

/// A raw `syscall` with up to six arguments: the kernel's return value
/// (`-errno` on failure).
pub fn sys(n: u64, a: [u64; 6]) -> i64 {
    let ret: i64;
    // SAFETY: the Linux x86-64 syscall convention: the number in rax, the
    // arguments in rdi, rsi, rdx, r10, r8, r9; rcx and r11 clobbered. The
    // callers' duty: every pointer passed points at memory this program owns
    // for the call, and every count fits the buffer behind its pointer.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") n as i64 => ret,
            in("rdi") a[0],
            in("rsi") a[1],
            in("rdx") a[2],
            in("r10") a[3],
            in("r8") a[4],
            in("r9") a[5],
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// A line built in a fixed buffer, then written to stdout in one `write`.
/// One longer than the buffer is cut, and still ends in its newline.
pub struct Line {
    buf: [u8; 512],
    len: usize,
}

impl Line {
    pub fn new() -> Self {
        Self { buf: [0; 512], len: 0 }
    }

    pub fn send(&mut self) {
        if self.len == self.buf.len() {
            self.buf[self.len - 1] = b'\n';
        }
        let mut off = 0;
        while off < self.len {
            let rest = &self.buf[off..self.len];
            let n = sys(WRITE, [1, rest.as_ptr() as u64, rest.len() as u64, 0, 0, 0]);
            if n <= 0 {
                break;
            }
            off += n as usize;
        }
        self.len = 0;
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.buf.len() - self.len;
        let n = s.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// `println!` for these programs: one line, one `write`.
macro_rules! say {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let mut line = $crate::common::Line::new();
        let _ = writeln!(line, $($arg)*);
        line.send();
    }};
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    say!("PANIC {}", info);
    // SAFETY: `_exit` terminates the process immediately without unwinding.
    unsafe { _exit(127) }
}

/// A failed expectation: what was expected, and what came instead.
pub struct Fail {
    pub what: &'static str,
    pub got: i64,
}

pub type R = Result<(), Fail>;

/// `got == want`, or a [`Fail`] saying `what`.
pub fn eq(what: &'static str, got: i64, want: i64) -> R {
    if got == want { Ok(()) } else { Err(Fail { what, got }) }
}

/// `cond`, or a [`Fail`] saying `what` with `got`.
pub fn ensure(what: &'static str, cond: bool, got: i64) -> R {
    if cond { Ok(()) } else { Err(Fail { what, got }) }
}

/// A syscall that returns a descriptor or a count: its value, or a [`Fail`].
pub fn ok(what: &'static str, ret: i64) -> Result<i64, Fail> {
    if ret >= 0 { Ok(ret) } else { Err(Fail { what, got: ret }) }
}

/// Run a mode's checks in order, printing `ok <name>` for each that passes;
/// the first that fails prints why and is the program's exit code, its
/// number. 0 when all pass.
pub fn run_checks(checks: &[(&str, &dyn Fn() -> R)]) -> c_int {
    assert!(checks.len() < USAGE_EXIT as usize, "a mode has fewer than 64 checks");
    for (i, (name, check)) in checks.iter().enumerate() {
        match check() {
            Ok(()) => say!("ok {name}"),
            Err(Fail { what, got }) => {
                say!("FAIL {name}: {what} (got {got})");
                return (i + 1) as c_int;
            }
        }
    }
    0
}

/// The NUL-terminated C string at `p` as bytes.
///
/// # Safety
/// `p` points at a NUL-terminated string that outlives the program.
pub unsafe fn arg(p: *const u8) -> &'static [u8] {
    let mut n = 0;
    // SAFETY: the caller's contract: readable up to and including its NUL.
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: the `n` bytes before the NUL are readable for the program's life.
    unsafe { core::slice::from_raw_parts(p, n) }
}

/// `main`'s arguments as bytes, at most 16 of them: more is a usage error,
/// printed, and the program exits [`USAGE_EXIT`].
///
/// # Safety
/// `argv` holds `argc` pointers to NUL-terminated strings that outlive the
/// program (the kernel's initial stack).
pub unsafe fn args(argc: c_int, argv: *const *const u8) -> ([&'static [u8]; 16], usize) {
    let mut out: [&[u8]; 16] = [b""; 16];
    let n = argc.max(0) as usize;
    if n > out.len() {
        say!("too many arguments ({n}, at most {})", out.len());
        // SAFETY: `_exit` terminates the process immediately without unwinding.
        unsafe { _exit(USAGE_EXIT) }
    }
    for (i, slot) in out[..n].iter_mut().enumerate() {
        // SAFETY: the caller's contract: `argv[i]` for `i < argc` is valid.
        *slot = unsafe { arg(*argv.add(i)) };
    }
    (out, n)
}
