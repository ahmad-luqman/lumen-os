//! What M8's test programs share (`mod common;` in each; ARCHITECTURE
//! section 15, "M8's tests"): the raw `syscall`, a line written in one
//! `write`, the panic handler, argument bytes, the checks' runner, syscall
//! numbers and errno constants, and close/pipe/fstat_mode helpers.
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

// Linux x86-64 syscall numbers.
pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_CLOSE: u64 = 3;
pub const SYS_FSTAT: u64 = 5;
pub const SYS_POLL: u64 = 7;
pub const SYS_LSEEK: u64 = 8;
pub const SYS_RT_SIGACTION: u64 = 13;
pub const SYS_RT_SIGPROCMASK: u64 = 14;
pub const SYS_IOCTL: u64 = 16;
pub const SYS_PREAD64: u64 = 17;
pub const SYS_PWRITE64: u64 = 18;
pub const SYS_PIPE: u64 = 22;
pub const SYS_READV: u64 = 19;
pub const SYS_WRITEV: u64 = 20;
pub const SYS_GETPID: u64 = 39;
pub const SYS_RT_SIGPENDING: u64 = 127;
pub const SYS_MMAP: u64 = 9;
pub const SYS_MUNMAP: u64 = 11;
pub const SYS_SELECT: u64 = 23;
pub const SYS_DUP: u64 = 32;
pub const SYS_DUP2: u64 = 33;
pub const SYS_FORK: u64 = 57;
pub const SYS_EXECVE: u64 = 59;
pub const SYS_EXIT: u64 = 60;
pub const SYS_EXIT_GROUP: u64 = 231;
pub const SYS_WAIT4: u64 = 61;
pub const SYS_FCNTL: u64 = 72;
pub const SYS_FLOCK: u64 = 73;
pub const SYS_FSYNC: u64 = 74;
pub const SYS_UNLINK: u64 = 87;
pub const SYS_READLINK: u64 = 89;
pub const SYS_CLOCK_GETTIME: u64 = 228;
pub const SYS_OPENAT: u64 = 257;
pub const SYS_MKNODAT: u64 = 259;
pub const SYS_PSELECT6: u64 = 270;
pub const SYS_PPOLL: u64 = 271;
pub const SYS_DUP3: u64 = 292;
pub const SYS_PIPE2: u64 = 293;

pub const EPERM: i64 = -1;
/// Positive errno returned by libc posix_spawn, unlike raw syscall errors.
pub const ENOENT_POSITIVE: i32 = 2;
pub const ENXIO: i64 = -6;
pub const EBADF: i64 = -9;
pub const EAGAIN: i64 = -11;
pub const EFAULT: i64 = -14;
pub const EBUSY: i64 = -16;
pub const EEXIST: i64 = -17;
pub const EINVAL: i64 = -22;
pub const ESPIPE: i64 = -29;
pub const EROFS: i64 = -30;
pub const EPIPE: i64 = -32;

/// Close a descriptor, checking the syscall result.
pub fn close(fd: i32) -> R {
    eq("close", sys(SYS_CLOSE, [fd as u64, 0, 0, 0, 0, 0]), 0)
}

/// A fresh pipe with the requested pipe2 flags.
pub fn pipe(flags: u64) -> Result<[i32; 2], Fail> {
    let mut pair = [-1; 2];
    eq("pipe2", sys(SYS_PIPE2, [pair.as_mut_ptr() as u64, flags, 0, 0, 0, 0]), 0)?;
    Ok(pair)
}

/// Terminate every thread of the process immediately without returning through
/// borrowed stack frames.
pub fn exit_group(code: u64) -> ! {
    sys(SYS_EXIT_GROUP, [code, 0, 0, 0, 0, 0]);
    // SAFETY: fallback termination if exit_group ever returned.
    unsafe { _exit(code as c_int) }
}

/// Lock-free first-error slot shared across worker threads so a worker failure
/// preserves the operation name and errno without hanging or allocating.
pub struct WorkerError {
    state: core::sync::atomic::AtomicU32, // 0 = ok, 1 = writing, 2 = published
    what_ptr: core::sync::atomic::AtomicU64,
    what_len: core::sync::atomic::AtomicU64,
    got: core::sync::atomic::AtomicI64,
}

impl WorkerError {
    pub const fn new() -> Self {
        use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64};
        Self {
            state: AtomicU32::new(0),
            what_ptr: AtomicU64::new(0),
            what_len: AtomicU64::new(0),
            got: AtomicI64::new(0),
        }
    }

    pub fn record(&self, fail: Fail) {
        use core::sync::atomic::Ordering::{AcqRel, Acquire, Relaxed, Release};
        if self.state.compare_exchange(0, 1, AcqRel, Acquire).is_ok() {
            self.what_ptr.store(fail.what.as_ptr() as u64, Relaxed);
            self.what_len.store(fail.what.len() as u64, Relaxed);
            self.got.store(fail.got, Relaxed);
            self.state.store(2, Release);
        }
    }

    pub fn has_error(&self) -> bool {
        self.state.load(core::sync::atomic::Ordering::Acquire) != 0
    }

    pub fn check(&self) -> R {
        use core::sync::atomic::Ordering::{Acquire, Relaxed};
        if self.state.load(Acquire) == 0 {
            return Ok(());
        }
        while self.state.load(Acquire) != 2 {
            core::hint::spin_loop();
        }
        // SAFETY: `what_ptr` and `what_len` came from a `&'static str` published
        // before state transitioned to 2.
        let bytes = unsafe {
            core::slice::from_raw_parts(self.what_ptr.load(Relaxed) as *const u8, self.what_len.load(Relaxed) as usize)
        };
        let what = core::str::from_utf8(bytes).unwrap_or("worker error");
        Err(Fail { what, got: self.got.load(Relaxed) })
    }
}

/// Linux x86-64 stat's st_mode (offset 24).
pub fn fstat_mode(fd: i32) -> Result<u32, Fail> {
    let mut stat = [0u8; 144];
    eq("fstat", sys(SYS_FSTAT, [fd as u64, stat.as_mut_ptr() as u64, 0, 0, 0, 0]), 0)?;
    Ok(u32::from_le_bytes(stat[24..28].try_into().unwrap()))
}
