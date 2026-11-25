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

use core::ffi::c_int;

use common::{R, ensure, eq, ok, run_checks, sys};

const USAGE: &str = "usage: m8_io <console-poll>";

// Linux x86-64 syscall numbers.
const SYS_READ: u64 = 0;
const SYS_CLOSE: u64 = 3;
const SYS_POLL: u64 = 7;
const SYS_RT_SIGPROCMASK: u64 = 14;
const SYS_SELECT: u64 = 23;
const SYS_CLOCK_GETTIME: u64 = 228;
const SYS_OPENAT: u64 = 257;
const SYS_PSELECT6: u64 = 270;
const SYS_PPOLL: u64 = 271;

const CLOCK_MONOTONIC: u64 = 1;
const AT_FDCWD: u64 = (-100i64) as u64;
const O_PATH: u64 = 0o10000000;
const SIG_SETMASK: u64 = 2;

const EBADF: i64 = -9;

const POLLIN: i16 = 0x0001;
const POLLOUT: i16 = 0x0004;
const POLLNVAL: i16 = 0x0020;

const NS_PER_MS: i64 = 1_000_000;
const NS_PER_SEC: i64 = 1_000_000_000;
const US_PER_SEC: i64 = 1_000_000;

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

#[repr(C)]
#[derive(Clone, Copy)]
struct Timeval {
    tv_sec: i64,
    tv_usec: i64,
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
    Ok(ts.tv_sec.saturating_mul(NS_PER_SEC).saturating_add(ts.tv_nsec))
}

fn read_one_byte(want: u8) -> R {
    let mut b = [0u8; 1];
    eq("read(0, 1)", sys(SYS_READ, [0, b.as_mut_ptr() as u64, 1, 0, 0, 0]), 1)?;
    eq("read byte", i64::from(b[0]), i64::from(want))
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

    // O_PATH descriptor answers POLLNVAL in poll/ppoll and -EBADF in select/pselect6.
    let path = b"/tmp\0";
    let pfd = ok("openat(/tmp, O_PATH)", sys(SYS_OPENAT, [AT_FDCWD, path.as_ptr() as u64, O_PATH, 0, 0, 0]))? as i32;
    let mut one = [PollFd { fd: pfd, events: POLLIN, revents: 0 }];
    let zero_ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    let r = sys(SYS_PPOLL, [one.as_mut_ptr() as u64, 1, &zero_ts as *const Timespec as u64, 0, 8, 0]);
    let mut rset = [0u64; 16];
    rset[(pfd as usize) / 64] |= 1u64 << ((pfd as usize) % 64);
    let mut zero_tv = Timeval { tv_sec: 0, tv_usec: 0 };
    let sr =
        sys(SYS_SELECT, [(pfd + 1) as u64, rset.as_mut_ptr() as u64, 0, 0, &mut zero_tv as *mut Timeval as u64, 0]);
    let _ = sys(SYS_CLOSE, [pfd as u64, 0, 0, 0, 0, 0]);

    eq("ppoll(O_PATH)", r, 1)?;
    eq("ppoll(O_PATH) revents", i64::from(one[0].revents), i64::from(POLLNVAL))?;
    eq("select(O_PATH)", sr, EBADF)?;

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
    let rem = ts.tv_sec.saturating_mul(NS_PER_SEC).saturating_add(ts.tv_nsec);
    ensure("ppoll rem in [0, 50ms]", (0..=50 * NS_PER_MS).contains(&rem), rem)
}

fn check_select_timeout_no_input() -> R {
    let mut rset = [1u64; 1];
    let mut tv = Timeval { tv_sec: 0, tv_usec: 50_000 };
    let t0 = now_ns()?;
    eq("select(0, 50ms)", sys(SYS_SELECT, [1, rset.as_mut_ptr() as u64, 0, 0, &mut tv as *mut Timeval as u64, 0]), 0)?;
    let elapsed = now_ns()? - t0;
    eq("select clears rset", rset[0] as i64, 0)?;
    ensure("select elapsed >= 50ms", elapsed >= 50 * NS_PER_MS, elapsed)?;
    let rem_us = tv.tv_sec.saturating_mul(US_PER_SEC).saturating_add(tv.tv_usec);
    ensure("select rem in [0, 50ms]", (0..=50_000).contains(&rem_us), rem_us)?;

    // Zero timeout returns 0 immediately and leaves tv at {0, 0}.
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
    // verify the original mask is restored on return.
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
    let rem = ts.tv_sec.saturating_mul(NS_PER_SEC).saturating_add(ts.tv_nsec);
    ensure("pselect6 rem in [0, 50ms]", (0..=50 * NS_PER_MS).contains(&rem), rem)?;

    let mut after: u64 = 0;
    eq("sigprocmask read after", sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, 0, &mut after as *mut u64 as u64, 8, 0, 0]), 0)?;
    // Restore original empty mask before leaving.
    let _ = sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, &old as *const u64 as u64, 0, 8, 0, 0]);
    eq("pselect6 restored mask", after as i64, orig_mask as i64)
}

fn check_poll_with_input() -> R {
    say!("ready poll");
    let mut fds = [PollFd { fd: 0, events: POLLIN, revents: 0 }];
    eq("poll(0, 5000ms) with input", sys(SYS_POLL, [fds.as_mut_ptr() as u64, 1, 5_000, 0, 0, 0]), 1)?;
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
    say!("ready ppoll");
    let mut fds = [PollFd { fd: 0, events: POLLIN, revents: 0 }];
    let mut ts = Timespec { tv_sec: 5, tv_nsec: 0 };
    eq(
        "ppoll(0, 5s) with input",
        sys(SYS_PPOLL, [fds.as_mut_ptr() as u64, 1, &mut ts as *mut Timespec as u64, 0, 8, 0]),
        1,
    )?;
    ensure("ppoll revents has POLLIN", (fds[0].revents & POLLIN) != 0, i64::from(fds[0].revents))?;
    let rem = ts.tv_sec.saturating_mul(NS_PER_SEC).saturating_add(ts.tv_nsec);
    ensure("ppoll rem in [0, 5s]", (0..=5 * NS_PER_SEC).contains(&rem), rem)?;
    read_one_byte(b'c')
}

fn check_select_with_input() -> R {
    say!("ready select");
    let mut rset = [1u64; 1];
    let mut tv = Timeval { tv_sec: 5, tv_usec: 0 };
    eq(
        "select(0, 5s) with input",
        sys(SYS_SELECT, [1, rset.as_mut_ptr() as u64, 0, 0, &mut tv as *mut Timeval as u64, 0]),
        1,
    )?;
    eq("select rset bit 0", rset[0] as i64, 1)?;
    let rem_us = tv.tv_sec.saturating_mul(US_PER_SEC).saturating_add(tv.tv_usec);
    ensure("select rem in [0, 5s]", (0..=5 * US_PER_SEC).contains(&rem_us), rem_us)?;
    read_one_byte(b'd')
}

fn check_pselect6_with_input() -> R {
    say!("ready pselect6");
    let mut rset = [1u64; 1];
    let mut ts = Timespec { tv_sec: 5, tv_nsec: 0 };
    let sig = PselectSig { ss: 0, ss_len: 0 };
    eq(
        "pselect6(0, 5s) with input",
        sys(
            SYS_PSELECT6,
            [1, rset.as_mut_ptr() as u64, 0, 0, &mut ts as *mut Timespec as u64, &sig as *const PselectSig as u64],
        ),
        1,
    )?;
    eq("pselect6 rset bit 0", rset[0] as i64, 1)?;
    let rem = ts.tv_sec.saturating_mul(NS_PER_SEC).saturating_add(ts.tv_nsec);
    ensure("pselect6 rem in [0, 5s]", (0..=5 * NS_PER_SEC).contains(&rem), rem)?;
    read_one_byte(b'e')
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
        _ => {
            say!("{USAGE}");
            common::USAGE_EXIT
        }
    }
}
