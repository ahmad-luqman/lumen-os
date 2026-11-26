//! `m8_io`: M8's ring-3 checks of pipes, FIFOs, `poll`/`select` and PTYs
//! (`tests/scenarios/test_linux_io.py`).
//!
//! A `#![no_std]` dynamically linked musl x86-64 executable, built as
//! `tools/linux-m7/`'s are (the Makefile) into `build/linux/`, which both
//! images stage in `/usr/local/bin`. Its checks and their conventions:
//! `common.rs` and ARCHITECTURE section 15, "M8's tests".

#![no_std]
#![no_main]

#[macro_use]
mod common;

use core::arch::global_asm;
use core::ffi::{c_char, c_int, c_void};
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering::SeqCst};

use common::*;

const USAGE: &str = "usage: m8_io <console-poll|pipes>";

// SAFETY: musl C ABI symbols resolved at runtime by `/lib/ld-musl-x86_64.so.1`.
unsafe extern "C" {
    fn posix_spawn(
        pid: *mut c_int,
        path: *const c_char,
        file_actions: *const c_void,
        attrp: *const c_void,
        argv: *const *const c_char,
        envp: *const *const c_char,
    ) -> c_int;
}

const CLOCK_MONOTONIC: u64 = 1;
const AT_FDCWD: u64 = (-100i64) as u64;
const O_RDONLY: u64 = 0;
const O_WRONLY: u64 = 1;
const O_RDWR: u64 = 2;
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;
const O_NONBLOCK: u64 = 0o4000;
const O_DIRECT: u64 = 0o40000;
const O_CLOEXEC: u64 = 0o2000000;
const O_PATH: u64 = 0o10000000;
const SIG_SETMASK: u64 = 2;

const F_GETFD: u64 = 1;
const F_GETFL: u64 = 3;
const F_SETPIPE_SZ: u64 = 1031;
const F_GETPIPE_SZ: u64 = 1032;
const FD_CLOEXEC: u64 = 1;
const FIONREAD: u64 = 0x541b;
const LOCK_EX: u64 = 2;
const LOCK_NB: u64 = 4;

const SIGPIPE: u64 = 13;
const SIG_IGN: u64 = 1;
const SA_RESTORER: u64 = 0x0400_0000;

const S_IFMT: u32 = 0o170_000;
const S_IFIFO: u32 = 0o010_000;
const S_IFCHR: u32 = 0o020_000;
const S_IFDIR: u32 = 0o040_000;
const S_IFBLK: u32 = 0o060_000;
const S_IFREG: u32 = 0o100_000;
const S_IFSOCK: u32 = 0o140_000;

const POLLIN: i16 = 0x0001;
const POLLOUT: i16 = 0x0004;
const POLLERR: i16 = 0x0008;
const POLLHUP: i16 = 0x0010;
const POLLNVAL: i16 = 0x0020;
const POLLRDNORM: i16 = 0x0040;
const POLLWRNORM: i16 = 0x0100;

const NS_PER_US: i64 = 1_000;
const NS_PER_MS: i64 = 1_000_000;
const NS_PER_SEC: i64 = 1_000_000_000;
const US_PER_SEC: i64 = 1_000_000;
const ASK_SEC: i64 = 5;

#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

impl Timespec {
    fn ns(self) -> i64 {
        self.tv_sec.saturating_mul(NS_PER_SEC).saturating_add(self.tv_nsec)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i64,
}

impl Timeval {
    fn us(self) -> i64 {
        self.tv_sec.saturating_mul(US_PER_SEC).saturating_add(self.tv_usec)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PselectSig {
    ss: u64,
    ss_len: u64,
}

fn now_ns() -> Result<i64, common::Fail> {
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    eq("clock_gettime", sys(SYS_CLOCK_GETTIME, [CLOCK_MONOTONIC, &mut ts as *mut Timespec as u64, 0, 0, 0, 0]), 0)?;
    Ok(ts.ns())
}

fn read_one_byte(want: u8) -> R {
    let mut b = [0u8; 1];
    eq("read(0, 1)", sys(SYS_READ, [0, b.as_mut_ptr() as u64, 1, 0, 0, 0]), 1)?;
    eq("read byte", i64::from(b[0]), i64::from(want))
}

fn check_rem(what: &'static str, rem: i64, asked: i64, elapsed: i64) -> R {
    ensure(what, (0..asked).contains(&rem), rem)?;
    ensure(what, rem.saturating_add(elapsed) >= asked, rem)
}

fn check_poll_nval() -> R {
    let mut fds = [
        PollFd { fd: 0, events: POLLIN, revents: 0x5a5a },
        PollFd { fd: 1, events: POLLOUT, revents: 0x5a5a },
        PollFd { fd: 99, events: POLLIN, revents: 0x5a5a },
        PollFd { fd: -1, events: POLLIN, revents: 0x5a5a },
    ];
    eq("poll([0,1,99,-1], 0)", sys(SYS_POLL, [fds.as_mut_ptr() as u64, 4, 0, 0, 0, 0]), 2)?;
    eq("fd 0 revents", i64::from(fds[0].revents), 0)?;
    eq("fd 1 revents", i64::from(fds[1].revents), i64::from(POLLOUT))?;
    eq("fd 99 revents", i64::from(fds[2].revents), i64::from(POLLNVAL))?;
    eq("fd -1 revents", i64::from(fds[3].revents), 0)?;

    // An `O_PATH` descriptor reports `POLLNVAL` in `poll`/`ppoll`, and in
    // `select`/`pselect6` (where `POLLNVAL` is in `POLLIN_SET`, `POLLOUT_SET`
    // and `POLLEX_SET`) reports ready in every requested set rather than EBADF.
    let path = b"/tmp\0";
    let pfd = ok("openat(/tmp, O_PATH)", sys(SYS_OPENAT, [AT_FDCWD, path.as_ptr() as u64, O_PATH, 0, 0, 0]))? as i32;
    let mut one = [PollFd { fd: pfd, events: POLLIN, revents: 0 }];
    let zero_ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    let r = sys(SYS_PPOLL, [one.as_mut_ptr() as u64, 1, &zero_ts as *const Timespec as u64, 0, 8, 0]);
    let mut rset = [0u64; 16];
    let mut wset = [0u64; 16];
    let mut xset = [0u64; 16];
    let idx = (pfd as usize) / 64;
    let bit = 1u64 << ((pfd as usize) % 64);
    rset[idx] |= bit;
    wset[idx] |= bit;
    xset[idx] |= bit;
    let mut zero_tv = Timeval { tv_sec: 0, tv_usec: 0 };
    let sr = sys(
        SYS_SELECT,
        [
            (pfd + 1) as u64,
            rset.as_mut_ptr() as u64,
            wset.as_mut_ptr() as u64,
            xset.as_mut_ptr() as u64,
            &mut zero_tv as *mut Timeval as u64,
            0,
        ],
    );
    let _ = sys(SYS_CLOSE, [pfd as u64, 0, 0, 0, 0, 0]);

    eq("ppoll(O_PATH)", r, 1)?;
    eq("ppoll(O_PATH) revents", i64::from(one[0].revents), i64::from(POLLNVAL))?;
    eq("select(O_PATH)", sr, 3)?;
    ensure("select(O_PATH) rset", (rset[idx] & bit) != 0, rset[idx] as i64)?;
    ensure("select(O_PATH) wset", (wset[idx] & bit) != 0, wset[idx] as i64)?;
    ensure("select(O_PATH) xset", (xset[idx] & bit) != 0, xset[idx] as i64)?;

    // Closed fd in select -> -EBADF.
    let mut bad_set = [0u64; 16];
    bad_set[99 / 64] |= 1u64 << (99 % 64);
    eq(
        "select(closed 99)",
        sys(SYS_SELECT, [100, bad_set.as_mut_ptr() as u64, 0, 0, &mut zero_tv as *mut Timeval as u64, 0]),
        EBADF,
    )
}

fn check_poll_timeout_no_input() -> R {
    let mut fds = [PollFd { fd: 0, events: POLLIN, revents: 0x5a5a }];
    let t0 = now_ns()?;
    eq("poll(0, 50ms)", sys(SYS_POLL, [fds.as_mut_ptr() as u64, 1, 50, 0, 0, 0]), 0)?;
    let elapsed = now_ns()? - t0;
    eq("revents on timeout", i64::from(fds[0].revents), 0)?;
    ensure("poll elapsed >= 50ms", elapsed >= 50 * NS_PER_MS, elapsed)
}

fn check_ppoll_timeout_no_input() -> R {
    let mut fds = [PollFd { fd: 0, events: POLLIN, revents: 0x5a5a }];
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 50 * NS_PER_MS };
    let t0 = now_ns()?;
    eq("ppoll(0, 50ms)", sys(SYS_PPOLL, [fds.as_mut_ptr() as u64, 1, &mut ts as *mut Timespec as u64, 0, 8, 0]), 0)?;
    let elapsed = now_ns()? - t0;
    eq("ppoll revents on timeout", i64::from(fds[0].revents), 0)?;
    ensure("ppoll elapsed >= 50ms", elapsed >= 50 * NS_PER_MS, elapsed)?;
    eq("ppoll rem == 0 on timeout", ts.ns(), 0)
}

fn check_select_timeout_no_input() -> R {
    let mut rset = [1u64; 1];
    let mut tv = Timeval { tv_sec: 0, tv_usec: 50_000 };
    let t0 = now_ns()?;
    eq("select(0, 50ms)", sys(SYS_SELECT, [1, rset.as_mut_ptr() as u64, 0, 0, &mut tv as *mut Timeval as u64, 0]), 0)?;
    let elapsed = now_ns()? - t0;
    eq("select clears rset", rset[0] as i64, 0)?;
    ensure("select elapsed >= 50ms", elapsed >= 50 * NS_PER_MS, elapsed)?;
    eq("select rem == 0 on timeout", tv.us(), 0)?;

    // Zero timeout returns 0 immediately and clears the empty bitset.
    rset[0] = 1;
    let mut zero_tv = Timeval { tv_sec: 0, tv_usec: 0 };
    eq(
        "select(0, 0)",
        sys(SYS_SELECT, [1, rset.as_mut_ptr() as u64, 0, 0, &mut zero_tv as *mut Timeval as u64, 0]),
        0,
    )?;
    eq("select(0) clears rset", rset[0] as i64, 0)
}

fn check_pselect6_timeout_no_input() -> R {
    // Set an initial signal mask, pass a different mask to pselect6, and
    // verify the initial mask is restored on return.
    let orig_mask: u64 = 1u64 << 9; // SIGUSR1
    let mut old: u64 = 0;
    eq(
        "sigprocmask set orig",
        sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, &orig_mask as *const u64 as u64, &mut old as *mut u64 as u64, 8, 0, 0]),
        0,
    )?;

    let temp_mask: u64 = 1u64 << 11; // SIGUSR2
    let sig = PselectSig { ss: &temp_mask as *const u64 as u64, ss_len: 8 };
    let mut rset = [1u64; 1];
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 50 * NS_PER_MS };
    let t0 = now_ns()?;
    eq(
        "pselect6(0, 50ms)",
        sys(
            SYS_PSELECT6,
            [1, rset.as_mut_ptr() as u64, 0, 0, &mut ts as *mut Timespec as u64, &sig as *const PselectSig as u64],
        ),
        0,
    )?;
    let elapsed = now_ns()? - t0;
    eq("pselect6 clears rset", rset[0] as i64, 0)?;
    ensure("pselect6 elapsed >= 50ms", elapsed >= 50 * NS_PER_MS, elapsed)?;
    eq("pselect6 rem == 0 on timeout", ts.ns(), 0)?;

    let mut after: u64 = 0;
    eq("sigprocmask read after", sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, 0, &mut after as *mut u64 as u64, 8, 0, 0]), 0)?;
    // Restore the previous mask saved before this check.
    let _ = sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, &old as *const u64 as u64, 0, 8, 0, 0]);
    eq("pselect6 restored mask", after as i64, orig_mask as i64)
}

fn check_poll_with_input() -> R {
    let mut fds = [PollFd { fd: 0, events: POLLIN, revents: 0 }];
    say!("ready poll");
    eq("poll(0, 5000ms) with input", sys(SYS_POLL, [fds.as_mut_ptr() as u64, 1, (ASK_SEC * 1000) as u64, 0, 0, 0]), 1)?;
    ensure("poll revents has POLLIN", (fds[0].revents & POLLIN) != 0, i64::from(fds[0].revents))?;
    read_one_byte(b'a')?;

    // The scenario sends "ab" together: the second byte is already queued so a
    // zero-timeout poll must see it immediately.
    fds[0].revents = 0;
    eq("poll(0, 0) queued byte", sys(SYS_POLL, [fds.as_mut_ptr() as u64, 1, 0, 0, 0, 0]), 1)?;
    ensure("poll queued revents has POLLIN", (fds[0].revents & POLLIN) != 0, i64::from(fds[0].revents))?;
    read_one_byte(b'b')
}

fn check_ppoll_with_input() -> R {
    let mut fds = [PollFd { fd: 0, events: POLLIN, revents: 0 }];
    let mut ts = Timespec { tv_sec: ASK_SEC, tv_nsec: 0 };
    let t0 = now_ns()?;
    say!("ready ppoll");
    eq(
        "ppoll(0, 5s) with input",
        sys(SYS_PPOLL, [fds.as_mut_ptr() as u64, 1, &mut ts as *mut Timespec as u64, 0, 8, 0]),
        1,
    )?;
    let t1 = now_ns()?;
    ensure("ppoll revents has POLLIN", (fds[0].revents & POLLIN) != 0, i64::from(fds[0].revents))?;
    check_rem("ppoll rem", ts.ns(), ASK_SEC * NS_PER_SEC, t1 - t0)?;
    read_one_byte(b'c')
}

fn check_select_with_input() -> R {
    let mut rset = [1u64; 1];
    let mut tv = Timeval { tv_sec: ASK_SEC, tv_usec: 0 };
    let t0 = now_ns()?;
    say!("ready select");
    eq(
        "select(0, 5s) with input",
        sys(SYS_SELECT, [1, rset.as_mut_ptr() as u64, 0, 0, &mut tv as *mut Timeval as u64, 0]),
        1,
    )?;
    let t1 = now_ns()?;
    eq("select rset bit 0", rset[0] as i64, 1)?;
    let elapsed_us_ceil = (t1 - t0).saturating_add(NS_PER_US - 1) / NS_PER_US;
    check_rem("select rem_us", tv.us(), ASK_SEC * US_PER_SEC, elapsed_us_ceil)?;
    read_one_byte(b'd')
}

fn check_pselect6_with_input() -> R {
    let mut rset = [1u64; 1];
    let mut ts = Timespec { tv_sec: ASK_SEC, tv_nsec: 0 };
    let sig = PselectSig { ss: 0, ss_len: 0 };
    let t0 = now_ns()?;
    say!("ready pselect6");
    eq(
        "pselect6(0, 5s) with input",
        sys(
            SYS_PSELECT6,
            [1, rset.as_mut_ptr() as u64, 0, 0, &mut ts as *mut Timespec as u64, &sig as *const PselectSig as u64],
        ),
        1,
    )?;
    let t1 = now_ns()?;
    eq("pselect6 rset bit 0", rset[0] as i64, 1)?;
    check_rem("pselect6 rem", ts.ns(), ASK_SEC * NS_PER_SEC, t1 - t0)?;
    read_one_byte(b'e')
}

global_asm!(".global m8_io_sigreturn", "m8_io_sigreturn:", "mov eax, 15", "syscall",);

unsafe extern "C" {
    fn m8_io_sigreturn();
}

static SIGPIPE_SEEN: AtomicU64 = AtomicU64::new(0);
static SIGPIPE_INFO_OK: AtomicU64 = AtomicU64::new(0);

extern "C" fn sigpipe_handler(sig: c_int, info: *const u8, _context: *const c_void) {
    SIGPIPE_SEEN.store(sig as u64, SeqCst);
    // SAFETY: SA_SIGINFO passes a readable 128-byte siginfo to this handler.
    let bytes = unsafe { core::slice::from_raw_parts(info, 128) };
    let code = i32::from_le_bytes(bytes[8..12].try_into().unwrap());
    let pid = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    let uid = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
    SIGPIPE_INFO_OK.store(u64::from(code == 0 && i64::from(pid) == sys(SYS_GETPID, [0; 6]) && uid == 0), SeqCst);
}

fn exit_child(code: u64) -> ! {
    sys(SYS_EXIT, [code, 0, 0, 0, 0, 0]);
    loop {}
}

fn wait_status(pid: i64) -> Result<i32, common::Fail> {
    let mut status: i32 = 0;
    eq("wait4", sys(SYS_WAIT4, [pid as u64, &mut status as *mut i32 as u64, 0, 0, 0, 0]), pid)?;
    Ok(status)
}

fn read_exact_all(fd: i32, dst: &mut [u8]) -> Result<usize, common::Fail> {
    let mut off = 0;
    while off < dst.len() {
        let n = sys(SYS_READ, [fd as u64, dst[off..].as_mut_ptr() as u64, (dst.len() - off) as u64, 0, 0, 0]);
        if n == 0 {
            break;
        }
        if n < 0 {
            return Err(common::Fail { what: "read_exact_all", got: n });
        }
        off += n as usize;
    }
    Ok(off)
}

fn write_exact_all(fd: i32, src: &[u8]) -> R {
    let mut off = 0;
    while off < src.len() {
        let n = sys(SYS_WRITE, [fd as u64, src[off..].as_ptr() as u64, (src.len() - off) as u64, 0, 0, 0]);
        if n <= 0 {
            return Err(common::Fail { what: "write_exact_all", got: n });
        }
        off += n as usize;
    }
    Ok(())
}

fn check_busybox_pipeline() -> R {
    let mut p1 = [-1i32; 2];
    let mut p2 = [-1i32; 2];
    eq("pipe p1", sys(SYS_PIPE, [p1.as_mut_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;
    eq("pipe p2", sys(SYS_PIPE, [p2.as_mut_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;

    let c1 = ok("fork printf", sys(SYS_FORK, [0; 6]))?;
    if c1 == 0 {
        let _ = sys(SYS_DUP2, [p1[1] as u64, 1, 0, 0, 0, 0]);
        for fd in [p1[0], p1[1], p2[0], p2[1]] {
            let _ = sys(SYS_CLOSE, [fd as u64, 0, 0, 0, 0, 0]);
        }
        let argv =
            [c"/bin/busybox".as_ptr() as u64, c"printf".as_ptr() as u64, c"cherry\napple\nbanana\n".as_ptr() as u64, 0];
        let envp = [0u64];
        let _ = sys(SYS_EXECVE, [c"/bin/busybox".as_ptr() as u64, argv.as_ptr() as u64, envp.as_ptr() as u64, 0, 0, 0]);
        exit_child(91);
    }

    let c2 = ok("fork sort", sys(SYS_FORK, [0; 6]))?;
    if c2 == 0 {
        let _ = sys(SYS_DUP2, [p1[0] as u64, 0, 0, 0, 0, 0]);
        let _ = sys(SYS_DUP2, [p2[1] as u64, 1, 0, 0, 0, 0]);
        for fd in [p1[0], p1[1], p2[0], p2[1]] {
            let _ = sys(SYS_CLOSE, [fd as u64, 0, 0, 0, 0, 0]);
        }
        let argv = [c"/bin/busybox".as_ptr() as u64, c"sort".as_ptr() as u64, 0];
        let envp = [0u64];
        let _ = sys(SYS_EXECVE, [c"/bin/busybox".as_ptr() as u64, argv.as_ptr() as u64, envp.as_ptr() as u64, 0, 0, 0]);
        exit_child(92);
    }

    let _ = sys(SYS_CLOSE, [p1[0] as u64, 0, 0, 0, 0, 0]);
    let _ = sys(SYS_CLOSE, [p1[1] as u64, 0, 0, 0, 0, 0]);
    let _ = sys(SYS_CLOSE, [p2[1] as u64, 0, 0, 0, 0, 0]);

    let mut out = [0u8; 64];
    let n = read_exact_all(p2[0], &mut out)?;
    let _ = sys(SYS_CLOSE, [p2[0] as u64, 0, 0, 0, 0, 0]);

    eq("c1 status", i64::from(wait_status(c1)?), 0)?;
    eq("c2 status", i64::from(wait_status(c2)?), 0)?;
    const WANT: &[u8] = b"apple\nbanana\ncherry\n";
    eq("pipeline output len", n as i64, WANT.len() as i64)?;
    ensure("pipeline sorted output", &out[..n] == WANT, n as i64)
}

fn check_pipe_buf_atomicity() -> R {
    const CHUNK: usize = 4000;
    const ROUNDS: usize = 24;
    let mut p = [-1i32; 2];
    eq("pipe", sys(SYS_PIPE, [p.as_mut_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;
    // 4000-byte records do not divide 8192: a torn write would cross records.
    eq("F_SETPIPE_SZ 8192", sys(SYS_FCNTL, [p[1] as u64, F_SETPIPE_SZ, 8192, 0, 0, 0]), 8192)?;

    let spawn_writer = |fill: u8| -> Result<i64, common::Fail> {
        let pid = ok("fork writer", sys(SYS_FORK, [0; 6]))?;
        if pid == 0 {
            let _ = sys(SYS_CLOSE, [p[0] as u64, 0, 0, 0, 0, 0]);
            let buf = [fill; CHUNK];
            for _ in 0..ROUNDS {
                let n = sys(SYS_WRITE, [p[1] as u64, buf.as_ptr() as u64, CHUNK as u64, 0, 0, 0]);
                if n != CHUNK as i64 {
                    exit_child(1);
                }
            }
            let _ = sys(SYS_CLOSE, [p[1] as u64, 0, 0, 0, 0, 0]);
            exit_child(0);
        }
        Ok(pid)
    };

    let w1 = spawn_writer(b'A')?;
    let w2 = spawn_writer(b'B')?;
    let _ = sys(SYS_CLOSE, [p[1] as u64, 0, 0, 0, 0, 0]);

    let mut count_a = 0usize;
    let mut count_b = 0usize;
    let mut buf = [0u8; CHUNK];
    for _ in 0..(2 * ROUNDS) {
        let n = read_exact_all(p[0], &mut buf)?;
        eq("atomic chunk size", n as i64, CHUNK as i64)?;
        let first = buf[0];
        ensure("chunk byte is A or B", first == b'A' || first == b'B', i64::from(first))?;
        for &b in &buf {
            if b != first {
                return Err(common::Fail { what: "PIPE_BUF write interleaved", got: i64::from(b) });
            }
        }
        if first == b'A' {
            count_a += 1;
        } else {
            count_b += 1;
        }
    }
    let mut extra = [0u8; 1];
    eq("eof after writers", sys(SYS_READ, [p[0] as u64, extra.as_mut_ptr() as u64, 1, 0, 0, 0]), 0)?;
    let _ = sys(SYS_CLOSE, [p[0] as u64, 0, 0, 0, 0, 0]);
    eq("writer A exit", i64::from(wait_status(w1)?), 0)?;
    eq("writer B exit", i64::from(wait_status(w2)?), 0)?;
    eq("count_a", count_a as i64, ROUNDS as i64)?;
    eq("count_b", count_b as i64, ROUNDS as i64)
}

fn check_eof_fionread_and_sigpipe() -> R {
    let mut p = [-1i32; 2];
    eq("pipe", sys(SYS_PIPE, [p.as_mut_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;

    // fstat reports S_IFIFO and /proc/self/fd/N starts with "pipe:[".
    let st_mode = fstat_mode(p[0])?;
    eq("fstat S_IFIFO", i64::from(st_mode & S_IFMT), i64::from(S_IFIFO))?;

    let mut proc_path = [0u8; 32];
    let prefix = b"/proc/self/fd/";
    proc_path[..prefix.len()].copy_from_slice(prefix);
    let mut fd_num = p[0] as u32;
    let mut digits = [0u8; 10];
    let mut dlen = 0;
    loop {
        digits[dlen] = b'0' + (fd_num % 10) as u8;
        dlen += 1;
        fd_num /= 10;
        if fd_num == 0 {
            break;
        }
    }
    for i in 0..dlen {
        proc_path[prefix.len() + i] = digits[dlen - 1 - i];
    }
    let mut target = [0u8; 64];
    let tlen = ok(
        "readlink /proc/self/fd",
        sys(SYS_READLINK, [proc_path.as_ptr() as u64, target.as_mut_ptr() as u64, target.len() as u64, 0, 0, 0]),
    )? as usize;
    ensure("pipe:[ino] link", tlen > 7 && &target[..6] == b"pipe:[", tlen as i64)?;

    // lseek, pread64, pwrite64 -> -ESPIPE; fsync -> -EINVAL.
    eq("lseek pipe", sys(SYS_LSEEK, [p[0] as u64, 0, 0, 0, 0, 0]), ESPIPE)?;
    let mut tmp = [0u8; 8];
    eq("pread64 pipe", sys(SYS_PREAD64, [p[0] as u64, tmp.as_mut_ptr() as u64, 4, 0, 0, 0]), ESPIPE)?;
    eq("pwrite64 pipe", sys(SYS_PWRITE64, [p[1] as u64, tmp.as_ptr() as u64, 4, 0, 0, 0]), ESPIPE)?;
    eq("fsync pipe", sys(SYS_FSYNC, [p[0] as u64, 0, 0, 0, 0, 0]), EINVAL)?;

    // FIONREAD tracks queued bytes, then EOF on writer close.
    write_exact_all(p[1], b"hello world")?;
    let mut avail: i32 = -1;
    eq("ioctl FIONREAD", sys(SYS_IOCTL, [p[0] as u64, FIONREAD, &mut avail as *mut i32 as u64, 0, 0, 0]), 0)?;
    eq("FIONREAD 11", i64::from(avail), 11)?;
    eq("read 5", sys(SYS_READ, [p[0] as u64, tmp.as_mut_ptr() as u64, 5, 0, 0, 0]), 5)?;
    eq("ioctl FIONREAD 6", sys(SYS_IOCTL, [p[0] as u64, FIONREAD, &mut avail as *mut i32 as u64, 0, 0, 0]), 0)?;
    eq("FIONREAD 6", i64::from(avail), 6)?;
    eq("read 6", sys(SYS_READ, [p[0] as u64, tmp.as_mut_ptr() as u64, 6, 0, 0, 0]), 6)?;
    eq("close w", sys(SYS_CLOSE, [p[1] as u64, 0, 0, 0, 0, 0]), 0)?;
    eq("read EOF", sys(SYS_READ, [p[0] as u64, tmp.as_mut_ptr() as u64, 4, 0, 0, 0]), 0)?;
    eq("close r", sys(SYS_CLOSE, [p[0] as u64, 0, 0, 0, 0, 0]), 0)?;

    // SIGPIPE default terminates child with signal 13.
    let c_dfl = ok("fork sigpipe dfl", sys(SYS_FORK, [0; 6]))?;
    if c_dfl == 0 {
        let mut q = [-1i32; 2];
        if sys(SYS_PIPE, [q.as_mut_ptr() as u64, 0, 0, 0, 0, 0]) != 0 {
            exit_child(10);
        }
        let _ = sys(SYS_CLOSE, [q[0] as u64, 0, 0, 0, 0, 0]);
        let _ = sys(SYS_WRITE, [q[1] as u64, b"x".as_ptr() as u64, 1, 0, 0, 0]);
        exit_child(11);
    }
    let st_dfl = wait_status(c_dfl)?;
    eq("SIGPIPE kills with 13", i64::from(st_dfl & 0x7f), SIGPIPE as i64)?;

    for flags in [0, O_NONBLOCK] {
        // SIGPIPE ignored -> write returns -EPIPE (and bad ptr still -EPIPE first).
        let c_ign = ok("fork sigpipe ign", sys(SYS_FORK, [0; 6]))?;
        if c_ign == 0 {
            let act = [SIG_IGN, 0, 0, 0];
            if sys(SYS_RT_SIGACTION, [SIGPIPE, act.as_ptr() as u64, 0, 8, 0, 0]) != 0 {
                exit_child(20);
            }
            let mut q = [-1i32; 2];
            if sys(SYS_PIPE2, [q.as_mut_ptr() as u64, flags, 0, 0, 0, 0]) != 0 {
                exit_child(21);
            }
            let _ = sys(SYS_CLOSE, [q[0] as u64, 0, 0, 0, 0, 0]);
            if sys(SYS_WRITE, [q[1] as u64, b"x".as_ptr() as u64, 1, 0, 0, 0]) != EPIPE {
                exit_child(22);
            }
            // Bad pointer with no readers must still return -EPIPE before -EFAULT.
            if sys(SYS_WRITE, [q[1] as u64, 0, 4, 0, 0, 0]) != EPIPE {
                exit_child(23);
            }
            exit_child(0);
        }
        eq("SIGPIPE ignored exit", i64::from(wait_status(c_ign)?), 0)?;
    }

    // SIGPIPE caught by handler -> handler runs and write returns -EPIPE.
    let c_catch = ok("fork sigpipe catch", sys(SYS_FORK, [0; 6]))?;
    if c_catch == 0 {
        SIGPIPE_SEEN.store(0, SeqCst);
        let act = [sigpipe_handler as *const () as u64, SA_RESTORER | 4, m8_io_sigreturn as *const () as u64, 0];
        if sys(SYS_RT_SIGACTION, [SIGPIPE, act.as_ptr() as u64, 0, 8, 0, 0]) != 0 {
            exit_child(30);
        }
        let mut q = [-1i32; 2];
        if sys(SYS_PIPE, [q.as_mut_ptr() as u64, 0, 0, 0, 0, 0]) != 0 {
            exit_child(31);
        }
        let _ = sys(SYS_CLOSE, [q[0] as u64, 0, 0, 0, 0, 0]);
        if sys(SYS_WRITE, [q[1] as u64, b"x".as_ptr() as u64, 1, 0, 0, 0]) != EPIPE {
            exit_child(32);
        }
        if SIGPIPE_SEEN.load(SeqCst) != SIGPIPE || SIGPIPE_INFO_OK.load(SeqCst) != 1 {
            exit_child(33);
        }
        exit_child(0);
    }
    eq("SIGPIPE caught exit", i64::from(wait_status(c_catch)?), 0)
}

fn check_nonblock_and_setpipe_sz() -> R {
    let mut p = [-1i32; 2];
    eq("pipe2 O_DIRECT EINVAL", sys(SYS_PIPE2, [p.as_mut_ptr() as u64, O_DIRECT, 0, 0, 0, 0]), EINVAL)?;
    eq("pipe2 bad ptr EFAULT", sys(SYS_PIPE2, [0, 0, 0, 0, 0, 0]), EFAULT)?;
    eq("pipe2 O_NONBLOCK|O_CLOEXEC", sys(SYS_PIPE2, [p.as_mut_ptr() as u64, O_NONBLOCK | O_CLOEXEC, 0, 0, 0, 0]), 0)?;

    eq("r FD_CLOEXEC", sys(SYS_FCNTL, [p[0] as u64, F_GETFD, 0, 0, 0, 0]), FD_CLOEXEC as i64)?;
    eq("w FD_CLOEXEC", sys(SYS_FCNTL, [p[1] as u64, F_GETFD, 0, 0, 0, 0]), FD_CLOEXEC as i64)?;
    let rfl = ok("r F_GETFL", sys(SYS_FCNTL, [p[0] as u64, F_GETFL, 0, 0, 0, 0]))? as u64;
    ensure("r O_NONBLOCK set", (rfl & O_NONBLOCK) != 0, rfl as i64)?;

    let mut b = [0u8; 16];
    eq("nonblock read empty", sys(SYS_READ, [p[0] as u64, b.as_mut_ptr() as u64, 16, 0, 0, 0]), EAGAIN)?;

    // F_GETPIPE_SZ / F_SETPIPE_SZ rules.
    eq("default pipe size", sys(SYS_FCNTL, [p[0] as u64, F_GETPIPE_SZ, 0, 0, 0, 0]), 65536)?;
    eq("shrink to 4096", sys(SYS_FCNTL, [p[1] as u64, F_SETPIPE_SZ, 4096, 0, 0, 0]), 4096)?;
    eq("get 4096", sys(SYS_FCNTL, [p[0] as u64, F_GETPIPE_SZ, 0, 0, 0, 0]), 4096)?;
    eq("over max EPERM", sys(SYS_FCNTL, [p[1] as u64, F_SETPIPE_SZ, 2 * 1024 * 1024, 0, 0, 0]), EPERM)?;

    let large = [0x5a; 4097];
    eq("seed 100", sys(SYS_WRITE, [p[1] as u64, large.as_ptr() as u64, 100, 0, 0, 0]), 100)?;
    eq("atomic 4096 EAGAIN", sys(SYS_WRITE, [p[1] as u64, large.as_ptr() as u64, 4096, 0, 0, 0]), EAGAIN)?;
    let mut queued = -1i32;
    eq(
        "FIONREAD after atomic refusal",
        sys(SYS_IOCTL, [p[0] as u64, FIONREAD, &mut queued as *mut i32 as u64, 0, 0, 0]),
        0,
    )?;
    eq("atomic refusal kept 100", i64::from(queued), 100)?;
    eq("4097 partial 3996", sys(SYS_WRITE, [p[1] as u64, large.as_ptr() as u64, 4097, 0, 0, 0]), 3996)?;
    let mut drain = [0; 4096];
    eq("drain deterministic row", sys(SYS_READ, [p[0] as u64, drain.as_mut_ptr() as u64, 4096, 0, 0, 0]), 4096)?;
    let page = [0x5Au8; 4096];
    eq("fill 4096", sys(SYS_WRITE, [p[1] as u64, page.as_ptr() as u64, 4096, 0, 0, 0]), 4096)?;
    eq("nonblock write full", sys(SYS_WRITE, [p[1] as u64, page.as_ptr() as u64, 1, 0, 0, 0]), EAGAIN)?;
    eq("grow to 8192", sys(SYS_FCNTL, [p[1] as u64, F_SETPIPE_SZ, 8192, 0, 0, 0]), 8192)?;
    eq("write second 4096", sys(SYS_WRITE, [p[1] as u64, page.as_ptr() as u64, 4096, 0, 0, 0]), 4096)?;
    eq("shrink below queued EBUSY", sys(SYS_FCNTL, [p[1] as u64, F_SETPIPE_SZ, 4096, 0, 0, 0]), EBUSY)?;

    // Non-pipe fd -> -EBADF.
    let null_fd =
        ok("open /dev/null", sys(SYS_OPENAT, [AT_FDCWD, c"/dev/null".as_ptr() as u64, O_RDWR, 0, 0, 0]))? as i32;
    eq("F_GETPIPE_SZ on /dev/null", sys(SYS_FCNTL, [null_fd as u64, F_GETPIPE_SZ, 0, 0, 0, 0]), EBADF)?;
    eq("F_SETPIPE_SZ on /dev/null", sys(SYS_FCNTL, [null_fd as u64, F_SETPIPE_SZ, 4096, 0, 0, 0]), EBADF)?;
    let _ = sys(SYS_CLOSE, [null_fd as u64, 0, 0, 0, 0, 0]);
    let _ = sys(SYS_CLOSE, [p[0] as u64, 0, 0, 0, 0, 0]);
    let _ = sys(SYS_CLOSE, [p[1] as u64, 0, 0, 0, 0, 0]);
    Ok(())
}

#[repr(C)]
struct IoVec {
    base: u64,
    len: u64,
}

fn check_pipe_writev_and_faults() -> R {
    let p = pipe(O_NONBLOCK)?;
    eq("writev capacity", sys(SYS_FCNTL, [p[1] as u64, F_SETPIPE_SZ, 4096, 0, 0, 0]), 4096)?;
    let payload = [0x42; 4097];
    let mut out = [0u8; 4096];
    write_exact_all(p[1], &payload[..100])?;
    let spans = [IoVec { base: payload.as_ptr() as u64, len: 4096 }, IoVec { base: payload.as_ptr() as u64, len: 1 }];
    eq("writev total 4097 partial 3996", sys(SYS_WRITEV, [p[1] as u64, spans.as_ptr() as u64, 2, 0, 0, 0]), 3996)?;
    eq("writev drain", sys(SYS_READ, [p[0] as u64, out.as_mut_ptr() as u64, 4096, 0, 0, 0]), 4096)?;
    ensure("writev bytes", out.iter().all(|&b| b == 0x42), 0)?;
    let split =
        [IoVec { base: payload.as_ptr() as u64, len: 2000 }, IoVec { base: payload.as_ptr() as u64, len: 2097 }];
    write_exact_all(p[1], &payload[..100])?;
    eq("writev partial crosses spans", sys(SYS_WRITEV, [p[1] as u64, split.as_ptr() as u64, 2, 0, 0, 0]), 3996)?;
    eq("writev second drain", sys(SYS_READ, [p[0] as u64, out.as_mut_ptr() as u64, 4096, 0, 0, 0]), 4096)?;
    close(p[0])?;
    close(p[1])?;
    for flags in [0, O_NONBLOCK] {
        let p = pipe(flags)?;
        let faulty = [IoVec { base: payload.as_ptr() as u64, len: 100 }, IoVec { base: 0, len: 100 }];
        eq("atomic writev EFAULT", sys(SYS_WRITEV, [p[1] as u64, faulty.as_ptr() as u64, 2, 0, 0, 0]), EFAULT)?;
        let mut queued = -1i32;
        eq(
            "faulted writev FIONREAD",
            sys(SYS_IOCTL, [p[0] as u64, FIONREAD, &mut queued as *mut i32 as u64, 0, 0, 0]),
            0,
        )?;
        eq("faulted atomic writev queued nothing", i64::from(queued), 0)?;
        let good =
            [IoVec { base: payload.as_ptr() as u64, len: 100 }, IoVec { base: payload.as_ptr() as u64, len: 200 }];
        eq("atomic writev success", sys(SYS_WRITEV, [p[1] as u64, good.as_ptr() as u64, 2, 0, 0, 0]), 300)?;
        let dst = [
            IoVec { base: out.as_mut_ptr() as u64, len: 120 },
            IoVec { base: out.as_mut_ptr() as u64 + 120, len: 180 },
        ];
        eq("readv pipe", sys(SYS_READV, [p[0] as u64, dst.as_ptr() as u64, 2, 0, 0, 0]), 300)?;
        ensure("readv bytes", out[..300].iter().all(|&b| b == 0x42), 0)?;
        close(p[0])?;
        close(p[1])?;
    }
    Ok(())
}

fn check_blocked_sigpipe_and_writev() -> R {
    // Each child proves EPIPE, pending SIGPIPE and survival while blocked,
    // then default delivery on unblocking. Includes the nonblocking write
    // site: merely checking EPIPE with SIG_IGN cannot detect a missing send.
    for (flags, vectored, len) in [(O_NONBLOCK, false, 1), (0, true, 1), (O_NONBLOCK, true, 1), (0, true, 4097)] {
        let child = ok("fork blocked SIGPIPE", sys(SYS_FORK, [0; 6]))?;
        if child == 0 {
            let result = (|| -> R {
                let mask = 1u64 << (SIGPIPE - 1);
                eq("block SIGPIPE", sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, &mask as *const u64 as u64, 0, 8, 0, 0]), 0)?;
                let p = pipe(flags)?;
                close(p[0])?;
                let data = [0x23; 4097];
                let spans = [IoVec { base: data.as_ptr() as u64, len }];
                let ret = if vectored {
                    sys(SYS_WRITEV, [p[1] as u64, spans.as_ptr() as u64, 1, 0, 0, 0])
                } else {
                    sys(SYS_WRITE, [p[1] as u64, data.as_ptr() as u64, len, 0, 0, 0])
                };
                eq("blocked SIGPIPE write EPIPE", ret, EPIPE)?;
                let mut pending = 0u64;
                eq("sigpending oversized set", sys(SYS_RT_SIGPENDING, [0, 9, 0, 0, 0, 0]), EINVAL)?;
                eq("sigpending bad pointer", sys(SYS_RT_SIGPENDING, [0, 8, 0, 0, 0, 0]), EFAULT)?;
                eq("sigpending zero size", sys(SYS_RT_SIGPENDING, [0, 0, 0, 0, 0, 0]), 0)?;
                eq("rt_sigpending", sys(SYS_RT_SIGPENDING, [&mut pending as *mut u64 as u64, 8, 0, 0, 0, 0]), 0)?;
                eq("SIGPIPE pending bit 13", (pending & mask) as i64, mask as i64)?;
                say!("ok SIGPIPE pending while blocked");
                let empty = 0u64;
                sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, &empty as *const u64 as u64, 0, 8, 0, 0]);
                Err(common::Fail { what: "survived SIGPIPE unblock", got: 0 })
            })();
            if let Err(e) = result {
                say!("FAIL blocked SIGPIPE: {} (got {})", e.what, e.got);
            }
            exit_child(90);
        }
        eq("SIGPIPE unblocked terminates child", i64::from(wait_status(child)? & 0x7f), SIGPIPE as i64)?;
    }
    Ok(())
}

fn check_proc_pipe_reopen_and_large_write() -> R {
    // Keep each orphaned end reachable at a known /proc magic link.
    for (keep, reopen) in [(0usize, O_RDONLY), (1, O_WRONLY | O_NONBLOCK), (1, O_WRONLY)] {
        let p = pipe(0)?;
        eq("dup pipe for proc reopen", sys(SYS_DUP2, [p[keep] as u64, 20, 0, 0, 0, 0]), 20)?;
        close(p[0])?;
        close(p[1])?;
        let fd = ok(
            "proc pipe reopen without partner",
            sys(SYS_OPENAT, [AT_FDCWD, c"/proc/self/fd/20".as_ptr() as u64, reopen, 0, 0, 0]),
        )? as i32;
        close(fd)?;
        close(20)?;
    }
    const LEN: usize = 1024 * 1024 + 4096;
    let ptr = ok("large write mmap", sys(SYS_MMAP, [0, LEN as u64, 3, 0x22, u64::MAX, 0]))? as u64;
    let p = pipe(0)?;
    let child = ok("fork large writer", sys(SYS_FORK, [0; 6]))?;
    if child == 0 {
        let _ = close(p[0]);
        let n = sys(SYS_WRITE, [p[1] as u64, ptr, LEN as u64, 0, 0, 0]);
        exit_child(if n == LEN as i64 { 0 } else { 91 });
    }
    close(p[1])?;
    let mut buf = [0; 4096];
    let mut total = 0;
    loop {
        let n =
            ok("large write drain", sys(SYS_READ, [p[0] as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0]))?;
        if n == 0 {
            break;
        }
        total += n;
    }
    eq("large blocking write count", total, LEN as i64)?;
    eq("large writer exit", i64::from(wait_status(child)?), 0)?;
    close(p[0])?;
    eq("large write munmap", sys(SYS_MUNMAP, [ptr, LEN as u64, 0, 0, 0, 0]), 0)
}

fn check_dup_redirections() -> R {
    let mut p = [-1i32; 2];
    eq("pipe2 O_CLOEXEC", sys(SYS_PIPE2, [p.as_mut_ptr() as u64, O_CLOEXEC, 0, 0, 0, 0]), 0)?;

    // dup clears FD_CLOEXEC even when oldfd has it set.
    let d1 = ok("dup", sys(SYS_DUP, [p[1] as u64, 0, 0, 0, 0, 0]))? as i32;
    eq("dup clears cloexec", sys(SYS_FCNTL, [d1 as u64, F_GETFD, 0, 0, 0, 0]), 0)?;

    // dup2(fd, fd) is a no-op and preserves FD_CLOEXEC; bad fds -> EBADF.
    eq("dup2 same preserves cloexec", sys(SYS_DUP2, [p[1] as u64, p[1] as u64, 0, 0, 0, 0]), i64::from(p[1]))?;
    eq("cloexec preserved", sys(SYS_FCNTL, [p[1] as u64, F_GETFD, 0, 0, 0, 0]), FD_CLOEXEC as i64)?;
    eq("dup2 closed old", sys(SYS_DUP2, [999, 10, 0, 0, 0, 0]), EBADF)?;
    eq("dup2 out of range new", sys(SYS_DUP2, [p[1] as u64, 1024, 0, 0, 0, 0]), EBADF)?;

    // dup3 validation and O_CLOEXEC flag.
    eq("dup3 same EINVAL", sys(SYS_DUP3, [p[1] as u64, p[1] as u64, 0, 0, 0, 0]), EINVAL)?;
    eq("dup3 bad flag EINVAL", sys(SYS_DUP3, [p[1] as u64, 20, O_NONBLOCK, 0, 0, 0]), EINVAL)?;
    eq("dup3 failed target uninstalled", sys(SYS_FCNTL, [20, F_GETFD, 0, 0, 0, 0]), EBADF)?;
    eq("dup3 flags before EBADF", sys(SYS_DUP3, [999, 21, O_NONBLOCK, 0, 0, 0]), EINVAL)?;
    eq("dup3 same before EBADF", sys(SYS_DUP3, [999, 999, 0, 0, 0, 0]), EINVAL)?;
    eq("dup3 O_CLOEXEC", sys(SYS_DUP3, [p[1] as u64, 20, O_CLOEXEC, 0, 0, 0]), 20)?;
    eq("dup3 cloexec set", sys(SYS_FCNTL, [20, F_GETFD, 0, 0, 0, 0]), FD_CLOEXEC as i64)?;

    // Displacing the last write end via dup2 closes it and causes EOF on the reader.
    let _ = sys(SYS_CLOSE, [p[1] as u64, 0, 0, 0, 0, 0]);
    let _ = sys(SYS_CLOSE, [d1 as u64, 0, 0, 0, 0, 0]);
    // fd 20 is now the sole remaining write end; displace it with p[0].
    eq("dup2 displace write end", sys(SYS_DUP2, [p[0] as u64, 20, 0, 0, 0, 0]), 20)?;
    let mut b = [0u8; 4];
    eq("EOF after displacement", sys(SYS_READ, [p[0] as u64, b.as_mut_ptr() as u64, 4, 0, 0, 0]), 0)?;
    let _ = sys(SYS_CLOSE, [20, 0, 0, 0, 0, 0]);
    let _ = sys(SYS_CLOSE, [p[0] as u64, 0, 0, 0, 0, 0]);
    Ok(())
}

fn check_fifo_open_rules_and_poll() -> R {
    let path = c"/tmp/m8-io-fifo";
    let _ = sys(SYS_UNLINK, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
    eq("mknodat FIFO", sys(SYS_MKNODAT, [AT_FDCWD, path.as_ptr() as u64, u64::from(S_IFIFO | 0o666), 0, 0, 0]), 0)?;

    // O_WRONLY | O_NONBLOCK with no reader -> -ENXIO.
    eq(
        "fifo wronly nonblock no reader",
        sys(SYS_OPENAT, [AT_FDCWD, path.as_ptr() as u64, O_WRONLY | O_NONBLOCK, 0, 0, 0]),
        ENXIO,
    )?;

    // O_RDONLY | O_NONBLOCK returns at once.
    let r =
        ok("fifo rdonly nonblock", sys(SYS_OPENAT, [AT_FDCWD, path.as_ptr() as u64, O_RDONLY | O_NONBLOCK, 0, 0, 0]))?
            as i32;
    let mut pf = [PollFd { fd: r, events: POLLIN, revents: 0 }];
    eq("poll fifo no writer", sys(SYS_POLL, [pf.as_mut_ptr() as u64, 1, 0, 0, 0, 0]), 0)?;
    eq("poll fifo no initial HUP", i64::from(pf[0].revents), 0)?;

    // O_WRONLY | O_NONBLOCK now succeeds because reader is open.
    let w =
        ok("fifo wronly nonblock", sys(SYS_OPENAT, [AT_FDCWD, path.as_ptr() as u64, O_WRONLY | O_NONBLOCK, 0, 0, 0]))?
            as i32;
    let mut both = [
        PollFd { fd: r, events: POLLIN | POLLRDNORM, revents: 0 },
        PollFd { fd: w, events: POLLOUT | POLLWRNORM, revents: 0 },
    ];
    eq("poll both empty", sys(SYS_POLL, [both.as_mut_ptr() as u64, 2, 0, 0, 0, 0]), 1)?;
    eq("poll r empty", i64::from(both[0].revents), 0)?;
    eq("poll w ready", i64::from(both[1].revents), i64::from(POLLOUT | POLLWRNORM))?;

    write_exact_all(w, b"fifo-ok")?;
    eq("poll both with data", sys(SYS_POLL, [both.as_mut_ptr() as u64, 2, 0, 0, 0, 0]), 2)?;
    eq("poll r data", i64::from(both[0].revents), i64::from(POLLIN | POLLRDNORM))?;
    let mut buf = [0u8; 16];
    eq("read fifo", sys(SYS_READ, [r as u64, buf.as_mut_ptr() as u64, 16, 0, 0, 0]), 7)?;
    ensure("fifo bytes", &buf[..7] == b"fifo-ok", 7)?;
    // A reader opened while a writer exists must also see its departure.
    let r2 = ok("fifo second reader", sys(SYS_OPENAT, [AT_FDCWD, path.as_ptr() as u64, O_NONBLOCK, 0, 0, 0]))? as i32;
    close(w)?;
    let mut readers = [PollFd { fd: r, events: POLLIN, revents: 0 }, PollFd { fd: r2, events: POLLIN, revents: 0 }];
    eq("fifo writer departed", sys(SYS_POLL, [readers.as_mut_ptr() as u64, 2, 0, 0, 0, 0]), 2)?;
    eq("first FIFO reader HUP", i64::from(readers[0].revents), i64::from(POLLHUP))?;
    eq("second FIFO reader HUP", i64::from(readers[1].revents), i64::from(POLLHUP))?;
    close(r2)?;
    // A new nonblocking description starts a new observation generation,
    // even though an older reader keeps the same pipe object attached.
    let r3 = ok("fifo third reader", sys(SYS_OPENAT, [AT_FDCWD, path.as_ptr() as u64, O_NONBLOCK, 0, 0, 0]))? as i32;
    let mut fresh = [PollFd { fd: r3, events: POLLIN, revents: 0 }];
    eq("fresh FIFO reader no stale HUP", sys(SYS_POLL, [fresh.as_mut_ptr() as u64, 1, 0, 0, 0, 0]), 0)?;
    close(r3)?;
    let w = ok("fifo reopen writer", sys(SYS_OPENAT, [AT_FDCWD, path.as_ptr() as u64, O_WRONLY | O_NONBLOCK, 0, 0, 0]))?
        as i32;

    let _ = sys(SYS_CLOSE, [r as u64, 0, 0, 0, 0, 0]);
    let mut pw = [PollFd { fd: w, events: POLLOUT, revents: 0 }];
    eq("poll w closed reader", sys(SYS_POLL, [pw.as_mut_ptr() as u64, 1, 0, 0, 0, 0]), 1)?;
    eq("poll w ERR", i64::from(pw[0].revents), i64::from(POLLOUT | POLLERR))?;
    let _ = sys(SYS_CLOSE, [w as u64, 0, 0, 0, 0, 0]);

    // Blocking O_RDONLY / O_WRONLY handshake across fork.
    let child = ok("fork fifo writer", sys(SYS_FORK, [0; 6]))?;
    if child == 0 {
        let fd = sys(SYS_OPENAT, [AT_FDCWD, path.as_ptr() as u64, O_WRONLY, 0, 0, 0]);
        if fd < 0 {
            exit_child(1);
        }
        let n = sys(SYS_WRITE, [fd as u64, b"sync".as_ptr() as u64, 4, 0, 0, 0]);
        let _ = sys(SYS_CLOSE, [fd as u64, 0, 0, 0, 0, 0]);
        exit_child(if n == 4 { 0 } else { 2 });
    }
    let br = ok("blocking fifo rdonly", sys(SYS_OPENAT, [AT_FDCWD, path.as_ptr() as u64, O_RDONLY, 0, 0, 0]))? as i32;
    let n = read_exact_all(br, &mut buf[..4])?;
    let _ = sys(SYS_CLOSE, [br as u64, 0, 0, 0, 0, 0]);
    eq("fifo child exit", i64::from(wait_status(child)?), 0)?;
    eq("blocking fifo len", n as i64, 4)?;
    ensure("blocking fifo data", &buf[..4] == b"sync", 4)?;

    // O_RDWR opens both ends without blocking.
    let rw = ok("fifo rdwr", sys(SYS_OPENAT, [AT_FDCWD, path.as_ptr() as u64, O_RDWR, 0, 0, 0]))? as i32;
    write_exact_all(rw, b"rw")?;
    eq("fifo rdwr read", sys(SYS_READ, [rw as u64, buf.as_mut_ptr() as u64, 2, 0, 0, 0]), 2)?;
    ensure("fifo rdwr bytes", &buf[..2] == b"rw", 2)?;
    let _ = sys(SYS_CLOSE, [rw as u64, 0, 0, 0, 0, 0]);
    let _ = sys(SYS_UNLINK, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
    Ok(())
}

fn check_posix_spawn_cloexec() -> R {
    let mut pid: c_int = -1;
    let ok_path = c"/bin/true";
    let ok_argv = [ok_path.as_ptr(), ptr::null()];
    let envp = [ptr::null()];
    // SAFETY: NUL-terminated path, argv, and envp arrays live for the call.
    let rc =
        unsafe { posix_spawn(&mut pid, ok_path.as_ptr(), ptr::null(), ptr::null(), ok_argv.as_ptr(), envp.as_ptr()) };
    eq("posix_spawn /bin/true", i64::from(rc), 0)?;
    ensure("posix_spawn pid > 0", pid > 0, i64::from(pid))?;
    eq("posix_spawn wait", i64::from(wait_status(i64::from(pid))?), 0)?;

    // Missing program: musl's posix_spawn reports ENOENT through its O_CLOEXEC error pipe.
    let bad_path = c"/no/such/program";
    let bad_argv = [bad_path.as_ptr(), ptr::null()];
    let mut bad_pid: c_int = -1;
    // SAFETY: NUL-terminated path, argv, and envp arrays live for the call.
    let rc_bad = unsafe {
        posix_spawn(&mut bad_pid, bad_path.as_ptr(), ptr::null(), ptr::null(), bad_argv.as_ptr(), envp.as_ptr())
    };
    eq("posix_spawn ENOENT", i64::from(rc_bad), i64::from(ENOENT))
}

fn check_fork_inc_fd_ref() -> R {
    // 1. Child closing inherited write end leaves reader without EOF.
    let mut p = [-1i32; 2];
    eq("pipe2 nonblock", sys(SYS_PIPE2, [p.as_mut_ptr() as u64, O_NONBLOCK, 0, 0, 0, 0]), 0)?;
    let c1 = ok("fork pipe ref", sys(SYS_FORK, [0; 6]))?;
    if c1 == 0 {
        let _ = sys(SYS_CLOSE, [p[1] as u64, 0, 0, 0, 0, 0]);
        exit_child(0);
    }
    eq("c1 exit", i64::from(wait_status(c1)?), 0)?;
    let mut b = [0u8; 16];
    eq("reader still EAGAIN not EOF", sys(SYS_READ, [p[0] as u64, b.as_mut_ptr() as u64, 16, 0, 0, 0]), EAGAIN)?;
    write_exact_all(p[1], b"alive")?;
    eq("read after child close", sys(SYS_READ, [p[0] as u64, b.as_mut_ptr() as u64, 16, 0, 0, 0]), 5)?;
    ensure("alive bytes", &b[..5] == b"alive", 5)?;
    let _ = sys(SYS_CLOSE, [p[1] as u64, 0, 0, 0, 0, 0]);
    eq("EOF after parent close", sys(SYS_READ, [p[0] as u64, b.as_mut_ptr() as u64, 16, 0, 0, 0]), 0)?;
    let _ = sys(SYS_CLOSE, [p[0] as u64, 0, 0, 0, 0, 0]);

    // 2. Child closing inherited flock descriptor leaves parent's lock intact.
    let lpath = c"/tmp/m8-io-flock";
    let fd1 = ok(
        "open flock file",
        sys(SYS_OPENAT, [AT_FDCWD, lpath.as_ptr() as u64, O_RDWR | O_CREAT | O_TRUNC, 0o644, 0, 0]),
    )? as i32;
    eq("flock EX", sys(SYS_FLOCK, [fd1 as u64, LOCK_EX, 0, 0, 0, 0]), 0)?;

    let c2 = ok("fork flock ref", sys(SYS_FORK, [0; 6]))?;
    if c2 == 0 {
        let _ = sys(SYS_CLOSE, [fd1 as u64, 0, 0, 0, 0, 0]);
        exit_child(0);
    }
    eq("c2 exit", i64::from(wait_status(c2)?), 0)?;

    let fd2 = ok("open second flock desc", sys(SYS_OPENAT, [AT_FDCWD, lpath.as_ptr() as u64, O_RDWR, 0, 0, 0]))? as i32;
    eq("second desc LOCK_NB EWOULDBLOCK", sys(SYS_FLOCK, [fd2 as u64, LOCK_EX | LOCK_NB, 0, 0, 0, 0]), EAGAIN)?;
    let _ = sys(SYS_CLOSE, [fd1 as u64, 0, 0, 0, 0, 0]);
    eq("second desc succeeds after parent close", sys(SYS_FLOCK, [fd2 as u64, LOCK_EX | LOCK_NB, 0, 0, 0, 0]), 0)?;
    let _ = sys(SYS_CLOSE, [fd2 as u64, 0, 0, 0, 0, 0]);
    let _ = sys(SYS_UNLINK, [lpath.as_ptr() as u64, 0, 0, 0, 0, 0]);
    Ok(())
}

fn check_mknodat_and_ext4_fifo() -> R {
    // Errno rows: S_IFDIR -> EPERM, bad type -> EINVAL before lookup, S_IFCHR/BLK/SOCK -> EPERM,
    // EEXIST, devfs -> EROFS.
    eq(
        "mknodat S_IFDIR EPERM",
        sys(SYS_MKNODAT, [AT_FDCWD, c"/tmp/mknod-dir".as_ptr() as u64, u64::from(S_IFDIR | 0o755), 0, 0, 0]),
        EPERM,
    )?;
    eq(
        "mknodat bad type EINVAL before lookup",
        sys(
            SYS_MKNODAT,
            [AT_FDCWD, c"/tmp/no-such-parent/x".as_ptr() as u64, u64::from(0o170_000u32 | 0o666), 0, 0, 0],
        ),
        EINVAL,
    )?;
    eq(
        "mknodat S_IFCHR EPERM",
        sys(SYS_MKNODAT, [AT_FDCWD, c"/tmp/mknod-chr".as_ptr() as u64, u64::from(S_IFCHR | 0o666), 0, 0, 0]),
        EPERM,
    )?;
    eq(
        "mknodat S_IFBLK EPERM",
        sys(SYS_MKNODAT, [AT_FDCWD, c"/tmp/mknod-blk".as_ptr() as u64, u64::from(S_IFBLK | 0o666), 0, 0, 0]),
        EPERM,
    )?;
    eq(
        "mknodat S_IFSOCK EPERM",
        sys(SYS_MKNODAT, [AT_FDCWD, c"/tmp/mknod-sock".as_ptr() as u64, u64::from(S_IFSOCK | 0o666), 0, 0, 0]),
        EPERM,
    )?;
    eq(
        "mknodat devfs EROFS",
        sys(SYS_MKNODAT, [AT_FDCWD, c"/dev/mknod-fifo".as_ptr() as u64, u64::from(S_IFIFO | 0o666), 0, 0, 0]),
        EROFS,
    )?;

    let reg = c"/tmp/mknod-reg";
    let _ = sys(SYS_UNLINK, [reg.as_ptr() as u64, 0, 0, 0, 0, 0]);
    eq("mknodat S_IFREG", sys(SYS_MKNODAT, [AT_FDCWD, reg.as_ptr() as u64, u64::from(S_IFREG | 0o644), 0, 0, 0]), 0)?;
    eq(
        "mknodat EEXIST",
        sys(SYS_MKNODAT, [AT_FDCWD, reg.as_ptr() as u64, u64::from(S_IFREG | 0o644), 0, 0, 0]),
        EEXIST,
    )?;
    let _ = sys(SYS_UNLINK, [reg.as_ptr() as u64, 0, 0, 0, 0, 0]);

    // Real ext4 FIFO: create, exercise O_RDWR I/O, and leave one live on disk
    // (plus unlink a second one) so host `e2fsck -fn` checks both paths.
    let ext4_keep = c"/ext4-fifo-keep";
    let ext4_del = c"/ext4-fifo-del";
    let _ = sys(SYS_UNLINK, [ext4_keep.as_ptr() as u64, 0, 0, 0, 0, 0]);
    let _ = sys(SYS_UNLINK, [ext4_del.as_ptr() as u64, 0, 0, 0, 0, 0]);
    eq(
        "ext4 mknod keep",
        sys(SYS_MKNODAT, [AT_FDCWD, ext4_keep.as_ptr() as u64, u64::from(S_IFIFO | 0o644), 0, 0, 0]),
        0,
    )?;
    eq(
        "ext4 mknod EEXIST",
        sys(SYS_MKNODAT, [AT_FDCWD, ext4_keep.as_ptr() as u64, u64::from(S_IFIFO | 0o644), 0, 0, 0]),
        EEXIST,
    )?;
    eq(
        "ext4 mknod del",
        sys(SYS_MKNODAT, [AT_FDCWD, ext4_del.as_ptr() as u64, u64::from(S_IFIFO | 0o644), 0, 0, 0]),
        0,
    )?;

    let fd = ok("open ext4 fifo", sys(SYS_OPENAT, [AT_FDCWD, ext4_keep.as_ptr() as u64, O_RDWR, 0, 0, 0]))? as i32;
    let st_mode = fstat_mode(fd)?;
    eq("ext4 fifo S_IFIFO", i64::from(st_mode & S_IFMT), i64::from(S_IFIFO))?;
    write_exact_all(fd, b"ext4-fifo")?;
    let mut got = [0u8; 16];
    eq("read ext4 fifo", sys(SYS_READ, [fd as u64, got.as_mut_ptr() as u64, 16, 0, 0, 0]), 9)?;
    ensure("ext4 fifo data", &got[..9] == b"ext4-fifo", 9)?;
    let _ = sys(SYS_CLOSE, [fd as u64, 0, 0, 0, 0, 0]);
    eq("unlink ext4 fifo", sys(SYS_UNLINK, [ext4_del.as_ptr() as u64, 0, 0, 0, 0, 0]), 0)
}

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const u8) -> c_int {
    // SAFETY: musl's `__libc_start_main` passes the kernel's `argc` and
    // `argv`, kept for the program's life.
    let (args, n) = unsafe { common::args(argc, argv) };
    match &args[..n] {
        [_, b"console-poll"] => run_checks(&[
            ("poll_nval", &check_poll_nval),
            ("poll_timeout_no_input", &check_poll_timeout_no_input),
            ("ppoll_timeout_no_input", &check_ppoll_timeout_no_input),
            ("select_timeout_no_input", &check_select_timeout_no_input),
            ("pselect6_timeout_no_input", &check_pselect6_timeout_no_input),
            ("poll_with_input", &check_poll_with_input),
            ("ppoll_with_input", &check_ppoll_with_input),
            ("select_with_input", &check_select_with_input),
            ("pselect6_with_input", &check_pselect6_with_input),
        ]),
        [_, b"pipes"] => run_checks(&[
            ("busybox_pipeline", &check_busybox_pipeline),
            ("pipe_buf_atomicity", &check_pipe_buf_atomicity),
            ("eof_fionread_and_sigpipe", &check_eof_fionread_and_sigpipe),
            ("nonblock_and_setpipe_sz", &check_nonblock_and_setpipe_sz),
            ("pipe_writev_and_faults", &check_pipe_writev_and_faults),
            ("blocked_sigpipe_and_writev", &check_blocked_sigpipe_and_writev),
            ("proc_pipe_reopen_and_large_write", &check_proc_pipe_reopen_and_large_write),
            ("dup_redirections", &check_dup_redirections),
            ("fifo_open_rules_and_poll", &check_fifo_open_rules_and_poll),
            ("posix_spawn_cloexec", &check_posix_spawn_cloexec),
            ("fork_inc_fd_ref", &check_fork_inc_fd_ref),
            ("mknodat_and_ext4_fifo", &check_mknodat_and_ext4_fifo),
        ]),
        _ => {
            say!("{USAGE}");
            common::USAGE_EXIT
        }
    }
}
