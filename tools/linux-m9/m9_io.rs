//! `m9_io`: M9's ring-3 checks of `epoll`, `eventfd` and `timerfd`
//! (`tests/scenarios/test_linux_io.py`; ARCHITECTURE section 15, "M9's tests").
//!
//! B5 (#476) supplies `epoll`, `epoll-pwait-blocked` and `send-sig`;
//! B6 (#477) supplies `evfd`, including signal interruption and POLLOUT wakes.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

#[macro_use]
mod common;

use core::arch::global_asm;
use core::ffi::c_int;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use common::*;

const USAGE: &str = "usage: m9_io <evfd|epoll|epoll-pwait-blocked|send-sig>";

const SYS_RT_SIGACTION: u64 = 13;
const SYS_RT_SIGPROCMASK: u64 = 14;
const SYS_READV: u64 = 19;
const SYS_WRITEV: u64 = 20;
const SYS_DUP: u64 = 32;
const SYS_NANOSLEEP: u64 = 35;
const SYS_GETPID: u64 = 39;
const SYS_FORK: u64 = 57;
const SYS_WAIT4: u64 = 61;
const SYS_KILL: u64 = 62;
const SYS_FCNTL: u64 = 72;
const SYS_RT_SIGPENDING: u64 = 127;
const SYS_OPENAT: u64 = 257;
const SYS_UNLINKAT: u64 = 263;
const SYS_READLINKAT: u64 = 267;

const AT_FDCWD: u64 = (-100i64) as u64;
const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_BOOTTIME: u64 = 7;
const CLOCK_REALTIME_ALARM: u64 = 8;
const CLOCK_BOOTTIME_ALARM: u64 = 9;
const EFD_SEMAPHORE: u64 = 0x1;
const EFD_NONBLOCK: u64 = 0x800;
const EFD_CLOEXEC: u64 = 0x8_0000;
const MAX_COUNTER: u64 = u64::MAX - 1;
const TFD_NONBLOCK: u64 = 0x800;
const TFD_CLOEXEC: u64 = 0x8_0000;
const TFD_TIMER_ABSTIME: u64 = 0x1;
const TFD_TIMER_CANCEL_ON_SET: u64 = 0x2;
const F_GETFD: u64 = 1;
const F_SETFL: u64 = 4;
const FD_CLOEXEC: i64 = 1;
const O_NONBLOCK: u64 = 0o4000;
const O_CLOEXEC: u64 = 0o2000000;
const POLLIN: i16 = 0x0001;
const POLLOUT: i16 = 0x0004;
const POLL_INF: u64 = (-1i64) as u64;
const SIGUSR1: u64 = 10;
const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;
const SA_RESTORER: u64 = 0x0400_0000;
const SA_RESTART: u64 = 0x1000_0000;
const S_IFMT: u32 = 0o170_000;
const NS_PER_MS: u64 = 1_000_000;
const NS_PER_SEC: u64 = 1_000_000_000;
const ENOENT: i64 = -2;
const EEXIST: i64 = -17;
const O_RDWR: u64 = 2;
const O_CREAT: u64 = 0o100;
const O_TRUNC: u64 = 0o1000;
const O_PATH: u64 = 0o10000000;
const EPOLLIN: u32 = 0x0001;
const EPOLLWAKEUP: u32 = 1 << 29;
const EPOLLONESHOT: u32 = 1 << 30;
const EPOLLET: u32 = 1 << 31;
const EPOLLEXCLUSIVE: u32 = 1 << 28;
const EPOLL_CLOEXEC: u64 = 0o2_000_000;
const EPOLL_CTL_ADD: u64 = 1;
const EPOLL_CTL_DEL: u64 = 2;
const EPOLL_CTL_MOD: u64 = 3;
const SIG_SETMASK: u64 = 2;
const TEMP_MASK: u64 = 1 << (12 - 1); // SIGUSR2, distinct from the caller's mask.

#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
struct EpollEvent {
    events: u32,
    data: u64,
}

// `SA_RESTORER` trampoline invoking `rt_sigreturn` (nr 15) on x86-64.
global_asm!(".global m9_io_sigreturn", "m9_io_sigreturn:", "mov eax, 15", "syscall", "ud2");

// SAFETY: `m9_io_sigreturn` is defined in the `global_asm!` block above and
// implements the x86-64 Linux `rt_sigreturn` trampoline ABI.
unsafe extern "C" {
    fn m9_io_sigreturn();
}

static USR1_MASK: AtomicU64 = AtomicU64::new(0);

static SIG_HITS: AtomicU64 = AtomicU64::new(0);
static SIG_ACK_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn usr1_handler(_sig: c_int) {
    USR1_MASK.store(get_sigmask().unwrap_or(u64::MAX), Ordering::SeqCst);
    SIG_HITS.fetch_add(1, Ordering::SeqCst);
    let fd = SIG_ACK_FD.load(Ordering::Acquire);
    if fd >= 0 {
        let one = 1u64;
        let _ = sys(SYS_WRITE, [fd as u64, &one as *const u64 as u64, 8, 0, 0, 0]);
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

impl Timespec {
    const fn from_ns(ns: u64) -> Self {
        Self { tv_sec: (ns / NS_PER_SEC) as i64, tv_nsec: (ns % NS_PER_SEC) as i64 }
    }

    fn ns(self) -> u64 {
        (self.tv_sec as u64).saturating_mul(NS_PER_SEC).saturating_add(self.tv_nsec as u64)
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Itimerspec {
    it_interval: Timespec,
    it_value: Timespec,
}

fn clock_ns(clock: u64) -> Result<u64, Fail> {
    let mut ts = Timespec::default();
    eq("clock_gettime", sys(SYS_CLOCK_GETTIME, [clock, &mut ts as *mut Timespec as u64, 0, 0, 0, 0]), 0)?;
    Ok(ts.ns())
}

fn sleep_ms(ms: u64) -> R {
    let req = Timespec::from_ns(ms * NS_PER_MS);
    eq("nanosleep", sys(SYS_NANOSLEEP, [&req as *const Timespec as u64, 0, 0, 0, 0, 0]), 0)
}

fn read_u64(what: &'static str, fd: i32) -> Result<u64, Fail> {
    let mut val = 0u64;
    eq(what, sys(SYS_READ, [fd as u64, &mut val as *mut u64 as u64, 8, 0, 0, 0]), 8)?;
    Ok(val)
}

fn write_u64(what: &'static str, fd: i32, val: u64) -> R {
    eq(what, sys(SYS_WRITE, [fd as u64, &val as *const u64 as u64, 8, 0, 0, 0]), 8)
}

fn check_proc_fd_link(fd: i32, want: &[u8]) -> R {
    struct PathBuf {
        buf: [u8; 64],
        len: usize,
    }
    impl core::fmt::Write for PathBuf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let n = s.len().min(self.buf.len() - self.len);
            self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
            self.len += n;
            Ok(())
        }
    }
    let mut p = PathBuf { buf: [0; 64], len: 0 };
    let _ = write!(p, "/proc/self/fd/{fd}\0");
    let mut target = [0u8; 64];
    let n = ok(
        "readlinkat(/proc/self/fd)",
        sys(SYS_READLINKAT, [AT_FDCWD, p.buf.as_ptr() as u64, target.as_mut_ptr() as u64, target.len() as u64, 0, 0]),
    )? as usize;
    ensure("proc fd target", &target[..n] == want, n as i64)
}

fn wait_child(pid: i64) -> R {
    let mut status: i32 = -1;
    eq("wait4", sys(SYS_WAIT4, [pid as u64, &mut status as *mut i32 as u64, 0, 0, 0, 0]), pid)?;
    eq("child exit status", i64::from(status), 0)
}

fn set_usr1_action(handler: u64, flags: u64) -> R {
    let act = [handler, flags, m9_io_sigreturn as *const () as u64, 0];
    eq("rt_sigaction(SIGUSR1)", sys(SYS_RT_SIGACTION, [SIGUSR1, act.as_ptr() as u64, 0, 8, 0, 0]), 0)
}

fn check_eventfd_counter_and_semaphore() -> R {
    // Legacy `eventfd(initval)` (284) -> `eventfd2(initval, 0)`.
    let efd = ok("eventfd(3)", sys(SYS_EVENTFD, [3, 0, 0, 0, 0, 0]))? as i32;
    check_proc_fd_link(efd, b"anon_inode:[eventfd]")?;

    // `fstat`: st_mode == 0o600 with no S_IFMT bits (offset 24 on x86-64).
    let mut statbuf = [0u8; 144];
    eq("fstat(efd)", sys(SYS_FSTAT, [efd as u64, statbuf.as_mut_ptr() as u64, 0, 0, 0, 0]), 0)?;
    let st_mode = u32::from_le_bytes(statbuf[24..28].try_into().unwrap());
    eq("efd st_mode & S_IFMT", i64::from(st_mode & S_IFMT), 0)?;
    eq("efd st_mode", i64::from(st_mode), 0o600)?;

    // Transfers under 8 bytes return -EINVAL (Linux v6.12 fs/eventfd.c).
    let mut small = [0u8; 8];
    for len in [0u64, 4, 7] {
        eq("short read(efd)", sys(SYS_READ, [efd as u64, small.as_mut_ptr() as u64, len, 0, 0, 0]), EINVAL)?;
        eq("short write(efd)", sys(SYS_WRITE, [efd as u64, small.as_ptr() as u64, len, 0, 0, 0]), EINVAL)?;
    }
    // Writing u64::MAX returns -EINVAL.
    let max_val = u64::MAX;
    eq("write(efd, u64::MAX)", sys(SYS_WRITE, [efd as u64, &max_val as *const u64 as u64, 8, 0, 0, 0]), EINVAL)?;

    // Counter mode accumulates and drains to 0 on read.
    write_u64("write(efd, 5)", efd, 5)?;
    let v = read_u64("read(efd)", efd)?;
    eq("eventfd sum", v as i64, 8)?;

    // Switch to O_NONBLOCK via fcntl(F_SETFL): empty read returns -EAGAIN.
    eq("fcntl(F_SETFL, O_NONBLOCK)", sys(SYS_FCNTL, [efd as u64, F_SETFL, O_NONBLOCK, 0, 0, 0]), 0)?;
    let mut tmp = 0u64;
    eq("nonblock empty read", sys(SYS_READ, [efd as u64, &mut tmp as *mut u64 as u64, 8, 0, 0, 0]), EAGAIN)?;
    close(efd)?;

    // Invalid flags rejected with -EINVAL.
    eq("eventfd2 bad flags", sys(SYS_EVENTFD2, [0, 0x100, 0, 0, 0, 0]), EINVAL)?;

    // Semaphore mode decrements by 1 per read.
    let sfd = ok(
        "eventfd2(2, SEM|NB|CLOEXEC)",
        sys(SYS_EVENTFD2, [2, EFD_SEMAPHORE | EFD_NONBLOCK | EFD_CLOEXEC, 0, 0, 0, 0]),
    )? as i32;
    eq("sfd FD_CLOEXEC", sys(SYS_FCNTL, [sfd as u64, F_GETFD, 0, 0, 0, 0]), FD_CLOEXEC)?;
    eq("sem read 1", read_u64("sem read 1", sfd)? as i64, 1)?;
    eq("sem read 2", read_u64("sem read 2", sfd)? as i64, 1)?;
    eq("sem empty", sys(SYS_READ, [sfd as u64, &mut tmp as *mut u64 as u64, 8, 0, 0, 0]), EAGAIN)?;
    write_u64("sem write 3", sfd, 3)?;
    for _ in 0..3 {
        eq("sem read after write", read_u64("sem read", sfd)? as i64, 1)?;
    }
    eq("sem empty again", sys(SYS_READ, [sfd as u64, &mut tmp as *mut u64 as u64, 8, 0, 0, 0]), EAGAIN)?;
    close(sfd)
}

fn check_eventfd_blocking_and_overflow_wait() -> R {
    // Non-blocking overflow returns -EAGAIN; writing 0 at MAX_COUNTER succeeds.
    let nb = ok("eventfd2(0, NB)", sys(SYS_EVENTFD2, [0, EFD_NONBLOCK | EFD_CLOEXEC, 0, 0, 0, 0]))? as i32;
    write_u64("write MAX_COUNTER", nb, MAX_COUNTER)?;
    let one = 1u64;
    eq("nonblock overflow", sys(SYS_WRITE, [nb as u64, &one as *const u64 as u64, 8, 0, 0, 0]), EAGAIN)?;
    write_u64("write 0 at MAX_COUNTER", nb, 0)?;
    let drained = read_u64("drain MAX_COUNTER", nb)?;
    ensure("drained == MAX_COUNTER", drained == MAX_COUNTER, 0)?;
    close(nb)?;

    // Blocking read wakes when another process writes.
    let efd = ok("eventfd2(0, CLOEXEC)", sys(SYS_EVENTFD2, [0, EFD_CLOEXEC, 0, 0, 0, 0]))? as i32;
    let pid = ok("fork reader test", sys(SYS_FORK, [0; 6]))?;
    if pid == 0 {
        let _ = sleep_ms(30);
        if write_u64("child write", efd, 7).is_err() {
            exit_group(1);
        }
        exit_group(0);
    }
    let got = read_u64("blocking read", efd)?;
    eq("blocking read value", got as i64, 7)?;
    wait_child(pid)?;

    // Blocking overflow wait: fill to MAX_COUNTER, child sleeps then drains,
    // parent's blocking write(5) unblocks and leaves 5 in the counter.
    write_u64("fill MAX_COUNTER", efd, MAX_COUNTER)?;
    let pid2 = ok("fork overflow test", sys(SYS_FORK, [0; 6]))?;
    if pid2 == 0 {
        let _ = sleep_ms(30);
        match read_u64("child drain", efd) {
            Ok(v) if v == MAX_COUNTER => exit_group(0),
            _ => exit_group(2),
        }
    }
    write_u64("blocking write after drain", efd, 5)?;
    wait_child(pid2)?;
    let after = read_u64("read after overflow wait", efd)?;
    eq("value after overflow wait", after as i64, 5)?;
    close(efd)
}

fn check_eventfd_poll_masks() -> R {
    let efd = ok("eventfd2(0, CLOEXEC)", sys(SYS_EVENTFD2, [0, EFD_CLOEXEC, 0, 0, 0, 0]))? as i32;
    let mut pfd = PollFd { fd: efd, events: POLLIN | POLLOUT, revents: 0 };

    // Empty counter: POLLOUT only.
    eq("poll empty", sys(SYS_POLL, [&mut pfd as *mut PollFd as u64, 1, 0, 0, 0, 0]), 1)?;
    eq("poll empty revents", i64::from(pfd.revents), i64::from(POLLOUT))?;

    // Non-zero below MAX_COUNTER: POLLIN | POLLOUT.
    write_u64("write 1", efd, 1)?;
    pfd.revents = 0;
    eq("poll non-zero", sys(SYS_POLL, [&mut pfd as *mut PollFd as u64, 1, 0, 0, 0, 0]), 1)?;
    eq("poll non-zero revents", i64::from(pfd.revents), i64::from(POLLIN | POLLOUT))?;

    // Full at MAX_COUNTER: POLLIN only (no POLLOUT).
    write_u64("fill to max", efd, MAX_COUNTER - 1)?;
    pfd.revents = 0;
    eq("poll full", sys(SYS_POLL, [&mut pfd as *mut PollFd as u64, 1, 0, 0, 0, 0]), 1)?;
    eq("poll full revents", i64::from(pfd.revents), i64::from(POLLIN))?;

    // Blocking poll(POLLOUT, -1) wakes when another process drains MAX_COUNTER.
    let pid_out = ok("fork poll(POLLOUT) wake", sys(SYS_FORK, [0; 6]))?;
    if pid_out == 0 {
        let _ = sleep_ms(30);
        match read_u64("child drain for POLLOUT", efd) {
            Ok(v) if v == MAX_COUNTER => exit_group(0),
            _ => exit_group(1),
        }
    }
    let mut out_fd = PollFd { fd: efd, events: POLLOUT, revents: 0 };
    eq("blocking poll(efd, POLLOUT)", sys(SYS_POLL, [&mut out_fd as *mut PollFd as u64, 1, POLL_INF, 0, 0, 0]), 1)?;
    eq("blocking poll(POLLOUT) revents", i64::from(out_fd.revents), i64::from(POLLOUT))?;
    wait_child(pid_out)?;

    // Drained back to 0: POLLOUT only.
    pfd.revents = 0;
    eq("poll drained", sys(SYS_POLL, [&mut pfd as *mut PollFd as u64, 1, 0, 0, 0, 0]), 1)?;
    eq("poll drained revents", i64::from(pfd.revents), i64::from(POLLOUT))?;

    // Blocking poll(POLLIN, -1) wakes on write from another process.
    let pid = ok("fork poll(POLLIN) wake", sys(SYS_FORK, [0; 6]))?;
    if pid == 0 {
        let _ = sleep_ms(30);
        if write_u64("child poll wake", efd, 1).is_err() {
            exit_group(1);
        }
        exit_group(0);
    }
    let mut wake_fd = PollFd { fd: efd, events: POLLIN, revents: 0 };
    eq("blocking poll(efd)", sys(SYS_POLL, [&mut wake_fd as *mut PollFd as u64, 1, POLL_INF, 0, 0, 0]), 1)?;
    eq("blocking poll revents", i64::from(wake_fd.revents), i64::from(POLLIN))?;
    eq("read after poll wake", read_u64("read wake", efd)? as i64, 1)?;
    wait_child(pid)?;
    close(efd)
}

fn check_timerfd_monotonic_oneshot_and_interval() -> R {
    // Error validation matches Linux v6.12 fs/timerfd.c (alarm clocks always
    // -EPERM because Lumen models no CAP_WAKE_ALARM).
    eq("timerfd_create bad flags", sys(SYS_TIMERFD_CREATE, [CLOCK_MONOTONIC, 0x100, 0, 0, 0, 0]), EINVAL)?;
    eq("timerfd_create bad clock", sys(SYS_TIMERFD_CREATE, [2, 0, 0, 0, 0, 0]), EINVAL)?;
    eq("timerfd_create REALTIME_ALARM", sys(SYS_TIMERFD_CREATE, [CLOCK_REALTIME_ALARM, 0, 0, 0, 0, 0]), EPERM)?;
    eq("timerfd_create BOOTTIME_ALARM", sys(SYS_TIMERFD_CREATE, [CLOCK_BOOTTIME_ALARM, 0, 0, 0, 0, 0]), EPERM)?;

    let tfd = ok("timerfd_create(MONO)", sys(SYS_TIMERFD_CREATE, [CLOCK_MONOTONIC, TFD_CLOEXEC, 0, 0, 0, 0]))? as i32;
    check_proc_fd_link(tfd, b"anon_inode:[timerfd]")?;
    eq("tfd FD_CLOEXEC", sys(SYS_FCNTL, [tfd as u64, F_GETFD, 0, 0, 0, 0]), FD_CLOEXEC)?;

    // write on timerfd -> -EINVAL; short read -> -EINVAL.
    let mut buf = [0u8; 8];
    eq("write(tfd)", sys(SYS_WRITE, [tfd as u64, buf.as_ptr() as u64, 8, 0, 0, 0]), EINVAL)?;
    eq("short read(tfd)", sys(SYS_READ, [tfd as u64, buf.as_mut_ptr() as u64, 4, 0, 0, 0]), EINVAL)?;

    // Disarmed gettime is zero.
    let mut cur = Itimerspec::default();
    eq("gettime disarmed", sys(SYS_TIMERFD_GETTIME, [tfd as u64, &mut cur as *mut Itimerspec as u64, 0, 0, 0, 0]), 0)?;
    eq("disarmed value", cur.it_value.ns() as i64, 0)?;
    eq("disarmed interval", cur.it_interval.ns() as i64, 0)?;

    // Bad settime flags / timespec -> -EINVAL; TFD_TIMER_CANCEL_ON_SET accepted.
    let zero = Itimerspec::default();
    eq(
        "settime bad flags",
        sys(SYS_TIMERFD_SETTIME, [tfd as u64, 0x10, &zero as *const Itimerspec as u64, 0, 0, 0]),
        EINVAL,
    )?;
    let bad_ts =
        Itimerspec { it_interval: Timespec::default(), it_value: Timespec { tv_sec: 0, tv_nsec: 1_000_000_000 } };
    eq(
        "settime bad nsec",
        sys(SYS_TIMERFD_SETTIME, [tfd as u64, 0, &bad_ts as *const Itimerspec as u64, 0, 0, 0]),
        EINVAL,
    )?;
    eq(
        "settime CANCEL_ON_SET",
        sys(SYS_TIMERFD_SETTIME, [tfd as u64, TFD_TIMER_CANCEL_ON_SET, &zero as *const Itimerspec as u64, 0, 0, 0]),
        0,
    )?;

    // One-shot timer (30 ms) on CLOCK_MONOTONIC: blocking read returns 1 after >= 30 ms.
    let t0 = clock_ns(CLOCK_MONOTONIC)?;
    let oneshot = Itimerspec { it_interval: Timespec::default(), it_value: Timespec::from_ns(30 * NS_PER_MS) };
    eq("settime oneshot", sys(SYS_TIMERFD_SETTIME, [tfd as u64, 0, &oneshot as *const Itimerspec as u64, 0, 0, 0]), 0)?;
    let n = read_u64("read oneshot", tfd)?;
    let elapsed = clock_ns(CLOCK_MONOTONIC)?.saturating_sub(t0);
    eq("oneshot count", n as i64, 1)?;
    ensure("oneshot elapsed >= 30ms", elapsed >= 30 * NS_PER_MS, elapsed as i64)?;
    eq(
        "gettime after oneshot",
        sys(SYS_TIMERFD_GETTIME, [tfd as u64, &mut cur as *mut Itimerspec as u64, 0, 0, 0, 0]),
        0,
    )?;
    eq("oneshot remaining 0", cur.it_value.ns() as i64, 0)?;

    // Interval timer (15 ms) on CLOCK_MONOTONIC: sleep >= 4 intervals, count >= 3 (floor).
    let periodic =
        Itimerspec { it_interval: Timespec::from_ns(15 * NS_PER_MS), it_value: Timespec::from_ns(15 * NS_PER_MS) };
    eq(
        "settime periodic",
        sys(SYS_TIMERFD_SETTIME, [tfd as u64, 0, &periodic as *const Itimerspec as u64, 0, 0, 0]),
        0,
    )?;
    sleep_ms(60)?;
    let ticks = read_u64("read periodic", tfd)?;
    ensure("periodic count >= 3", ticks >= 3, ticks as i64)?;

    // Disarm via settime(0) returns previous interval, and gettime reads zero.
    let mut old = Itimerspec::default();
    eq(
        "settime disarm",
        sys(
            SYS_TIMERFD_SETTIME,
            [tfd as u64, 0, &zero as *const Itimerspec as u64, &mut old as *mut Itimerspec as u64, 0, 0],
        ),
        0,
    )?;
    eq("old interval == 15ms", old.it_interval.ns() as i64, (15 * NS_PER_MS) as i64)?;
    eq(
        "gettime after disarm",
        sys(SYS_TIMERFD_GETTIME, [tfd as u64, &mut cur as *mut Itimerspec as u64, 0, 0, 0, 0]),
        0,
    )?;
    eq("disarm value == 0", cur.it_value.ns() as i64, 0)?;
    eq("disarm interval == 0", cur.it_interval.ns() as i64, 0)?;

    eq("tfd set O_NONBLOCK", sys(SYS_FCNTL, [tfd as u64, F_SETFL, O_NONBLOCK, 0, 0, 0]), 0)?;
    let mut tmp = 0u64;
    eq("disarmed read EAGAIN", sys(SYS_READ, [tfd as u64, &mut tmp as *mut u64 as u64, 8, 0, 0, 0]), EAGAIN)?;
    close(tfd)?;

    // CLOCK_BOOTTIME behaves as monotonic.
    let bfd =
        ok("timerfd_create(BOOTTIME)", sys(SYS_TIMERFD_CREATE, [CLOCK_BOOTTIME, TFD_CLOEXEC, 0, 0, 0, 0]))? as i32;
    let boot_shot = Itimerspec { it_interval: Timespec::default(), it_value: Timespec::from_ns(20 * NS_PER_MS) };
    eq(
        "settime boottime",
        sys(SYS_TIMERFD_SETTIME, [bfd as u64, 0, &boot_shot as *const Itimerspec as u64, 0, 0, 0]),
        0,
    )?;
    eq("boottime read == 1", read_u64("read boottime", bfd)? as i64, 1)?;
    close(bfd)
}

fn check_timerfd_realtime_abstime_and_poll() -> R {
    let tfd =
        ok("timerfd_create(REALTIME)", sys(SYS_TIMERFD_CREATE, [CLOCK_REALTIME, TFD_CLOEXEC, 0, 0, 0, 0]))? as i32;

    // TFD_TIMER_ABSTIME on CLOCK_REALTIME set 200 ms ahead: blocking read
    // returns 1 no earlier than that realtime (floor).
    let rt0 = clock_ns(CLOCK_REALTIME)?;
    let target = rt0.saturating_add(200 * NS_PER_MS);
    let abs_future = Itimerspec { it_interval: Timespec::default(), it_value: Timespec::from_ns(target) };
    eq(
        "settime realtime +200ms",
        sys(SYS_TIMERFD_SETTIME, [tfd as u64, TFD_TIMER_ABSTIME, &abs_future as *const Itimerspec as u64, 0, 0, 0]),
        0,
    )?;
    let n = read_u64("read realtime +200ms", tfd)?;
    let rt1 = clock_ns(CLOCK_REALTIME)?;
    eq("realtime +200ms count", n as i64, 1)?;
    ensure("realtime reached target", rt1 >= target, (rt1.saturating_sub(rt0)) as i64)?;

    // Absolute time already past: blocking read returns 1 (no time asserted).
    let past = Itimerspec { it_interval: Timespec::default(), it_value: Timespec { tv_sec: 1, tv_nsec: 0 } };
    eq(
        "settime realtime past",
        sys(SYS_TIMERFD_SETTIME, [tfd as u64, TFD_TIMER_ABSTIME, &past as *const Itimerspec as u64, 0, 0, 0]),
        0,
    )?;
    eq("realtime past count", read_u64("read realtime past", tfd)? as i64, 1)?;
    close(tfd)?;

    // timerfd under poll: disarmed is neither POLLIN nor POLLOUT; armed wakes with POLLIN (timeout -1).
    let pfd_t = ok(
        "timerfd_create(MONO poll)",
        sys(SYS_TIMERFD_CREATE, [CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC, 0, 0, 0, 0]),
    )? as i32;
    let mut pfd = PollFd { fd: pfd_t, events: POLLIN | POLLOUT, revents: 0 };
    eq("poll disarmed tfd", sys(SYS_POLL, [&mut pfd as *mut PollFd as u64, 1, 0, 0, 0, 0]), 0)?;
    eq("poll disarmed revents", i64::from(pfd.revents), 0)?;

    let shot = Itimerspec { it_interval: Timespec::default(), it_value: Timespec::from_ns(25 * NS_PER_MS) };
    eq(
        "settime poll shot",
        sys(SYS_TIMERFD_SETTIME, [pfd_t as u64, 0, &shot as *const Itimerspec as u64, 0, 0, 0]),
        0,
    )?;
    eq("blocking poll(tfd)", sys(SYS_POLL, [&mut pfd as *mut PollFd as u64, 1, POLL_INF, 0, 0, 0]), 1)?;
    eq("poll(tfd) revents", i64::from(pfd.revents), i64::from(POLLIN))?;
    eq("read after poll(tfd)", read_u64("read poll tfd", pfd_t)? as i64, 1)?;
    pfd.revents = 0;
    eq("poll(tfd) empty after read", sys(SYS_POLL, [&mut pfd as *mut PollFd as u64, 1, 0, 0, 0, 0]), 0)?;
    eq("poll(tfd) empty revents", i64::from(pfd.revents), 0)?;
    close(pfd_t)
}

/// Signal interruption of blocking `read` on an empty `eventfd` and a disarmed
/// `timerfd`: without `SA_RESTART` both return `-EINTR`; with `SA_RESTART`
/// both restart after the handler runs and complete once a value arrives.
fn check_signal_interruption() -> R {
    let ppid = ok("getpid", sys(SYS_GETPID, [0; 6]))? as u64;

    // 1. Without SA_RESTART: child pulses SIGUSR1 until stop pipe closes so a
    //    busy host can never miss the blocking window. Ignore SIGUSR1 before
    //    `wait4` so a final in-flight signal cannot interrupt `wait4` with
    //    `-EINTR`.
    SIG_ACK_FD.store(-1, Ordering::Release);

    for is_timer in [false, true] {
        set_usr1_action(usr1_handler as *const () as u64, SA_RESTORER)?;
        let fd = if is_timer {
            ok("timerfd_create(EINTR)", sys(SYS_TIMERFD_CREATE, [CLOCK_MONOTONIC, TFD_CLOEXEC, 0, 0, 0, 0]))? as i32
        } else {
            ok("eventfd2(EINTR)", sys(SYS_EVENTFD2, [0, EFD_CLOEXEC, 0, 0, 0, 0]))? as i32
        };
        let mut stop = [0i32; 2];
        eq("pipe2(stop)", sys(SYS_PIPE2, [stop.as_mut_ptr() as u64, O_CLOEXEC, 0, 0, 0, 0]), 0)?;
        SIG_HITS.store(0, Ordering::SeqCst);

        let pid = ok("fork EINTR signaller", sys(SYS_FORK, [0; 6]))?;
        if pid == 0 {
            let _ = sys(SYS_CLOSE, [stop[1] as u64, 0, 0, 0, 0, 0]);
            let mut sp = PollFd { fd: stop[0], events: POLLIN, revents: 0 };
            loop {
                let _ = sys(SYS_KILL, [ppid, SIGUSR1, 0, 0, 0, 0]);
                sp.revents = 0;
                if sys(SYS_POLL, [&mut sp as *mut PollFd as u64, 1, 10, 0, 0, 0]) != 0 {
                    break;
                }
            }
            exit_group(0);
        }
        close(stop[0])?;
        let mut val = 0u64;
        let ret = sys(SYS_READ, [fd as u64, &mut val as *mut u64 as u64, 8, 0, 0, 0]);
        set_usr1_action(SIG_IGN, 0)?;
        close(stop[1])?;
        wait_child(pid)?;
        close(fd)?;
        eq(if is_timer { "timerfd read EINTR" } else { "eventfd read EINTR" }, ret, EINTR)?;
        ensure("EINTR handler ran", SIG_HITS.load(Ordering::SeqCst) >= 1, SIG_HITS.load(Ordering::SeqCst) as i64)?;
    }

    // 2. With SA_RESTART: child pulses SIGUSR1 until the handler has
    //    acknowledged at least 3 deliveries (so the blocking read is
    //    interrupted and restarted), then delivers the eventfd write / timerfd
    //    arm.
    set_usr1_action(usr1_handler as *const () as u64, SA_RESTORER | SA_RESTART)?;

    for is_timer in [false, true] {
        let fd = if is_timer {
            ok("timerfd_create(SA_RESTART)", sys(SYS_TIMERFD_CREATE, [CLOCK_MONOTONIC, TFD_CLOEXEC, 0, 0, 0, 0]))?
                as i32
        } else {
            ok("eventfd2(SA_RESTART)", sys(SYS_EVENTFD2, [0, EFD_CLOEXEC, 0, 0, 0, 0]))? as i32
        };
        let ready = ok("eventfd2(ready)", sys(SYS_EVENTFD2, [0, EFD_CLOEXEC, 0, 0, 0, 0]))? as i32;
        let ack = ok("eventfd2(ack)", sys(SYS_EVENTFD2, [0, EFD_CLOEXEC, 0, 0, 0, 0]))? as i32;
        SIG_HITS.store(0, Ordering::SeqCst);
        SIG_ACK_FD.store(ack, Ordering::Release);

        let pid = ok("fork SA_RESTART child", sys(SYS_FORK, [0; 6]))?;
        if pid == 0 {
            if read_u64("child wait ready", ready).is_err() {
                exit_group(1);
            }
            let mut acks = 0u64;
            while acks < 3 {
                let _ = sleep_ms(10);
                if sys(SYS_KILL, [ppid, SIGUSR1, 0, 0, 0, 0]) != 0 {
                    exit_group(2);
                }
                match read_u64("child wait ack", ack) {
                    Ok(n) => acks = acks.saturating_add(n),
                    Err(_) => exit_group(3),
                }
            }
            if is_timer {
                let shot = Itimerspec { it_interval: Timespec::default(), it_value: Timespec::from_ns(10 * NS_PER_MS) };
                if sys(SYS_TIMERFD_SETTIME, [fd as u64, 0, &shot as *const Itimerspec as u64, 0, 0, 0]) != 0 {
                    exit_group(4);
                }
            } else if write_u64("child write after SA_RESTART", fd, 42).is_err() {
                exit_group(4);
            }
            exit_group(0);
        }

        write_u64("signal child ready", ready, 1)?;
        let got = read_u64(if is_timer { "timerfd read SA_RESTART" } else { "eventfd read SA_RESTART" }, fd)?;
        SIG_ACK_FD.store(-1, Ordering::Release);
        wait_child(pid)?;
        close(fd)?;
        close(ready)?;
        close(ack)?;
        eq(
            if is_timer { "timerfd SA_RESTART value" } else { "eventfd SA_RESTART value" },
            got as i64,
            if is_timer { 1 } else { 42 },
        )?;
        ensure(
            "SA_RESTART handler ran >= 3 times",
            SIG_HITS.load(Ordering::SeqCst) >= 3,
            SIG_HITS.load(Ordering::SeqCst) as i64,
        )?;
    }

    // Restore SIGUSR1 to SIG_DFL.
    set_usr1_action(SIG_DFL, 0)
}

fn make_pipe() -> Result<(i32, i32), Fail> {
    let mut fds = [0i32; 2];
    eq("pipe2", sys(SYS_PIPE2, [fds.as_mut_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;
    Ok((fds[0], fds[1]))
}

fn ep_create1(flags: u64) -> Result<i32, Fail> {
    Ok(ok("epoll_create1", sys(SYS_EPOLL_CREATE1, [flags, 0, 0, 0, 0, 0]))? as i32)
}

fn ep_ctl(epfd: i32, op: u64, fd: i32, ev: Option<&EpollEvent>) -> i64 {
    let ptr = ev.map_or(0, |e| e as *const EpollEvent as u64);
    sys(SYS_EPOLL_CTL, [epfd as u64, op, fd as u64, ptr, 0, 0])
}

fn ep_wait(epfd: i32, out: &mut [EpollEvent], timeout_ms: i32) -> i64 {
    sys(SYS_EPOLL_WAIT, [epfd as u64, out.as_mut_ptr() as u64, out.len() as u64, timeout_ms as u32 as u64, 0, 0])
}

fn write_all(fd: i32, data: &[u8]) -> R {
    eq("write", sys(SYS_WRITE, [fd as u64, data.as_ptr() as u64, data.len() as u64, 0, 0, 0]), data.len() as i64)
}

fn read_n(fd: i32, buf: &mut [u8]) -> R {
    eq("read", sys(SYS_READ, [fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0]), buf.len() as i64)
}

fn set_sigmask(mask: u64) -> R {
    eq("rt_sigprocmask set", sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, &mask as *const _ as u64, 0, 8, 0, 0]), 0)
}

fn get_sigmask() -> Result<u64, Fail> {
    let mut got = 0u64;
    eq("rt_sigprocmask get", sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, 0, &mut got as *mut _ as u64, 8, 0, 0]), 0)?;
    Ok(got)
}

fn check_lt_et_and_oneshot() -> R {
    let ep = ep_create1(0)?;
    let (r, w) = make_pipe()?;
    let mut evs = [EpollEvent::default(); 4];
    let mut buf = [0u8; 4];

    // Level-triggered: reports repeatedly while unread data remains, stops once drained.
    let lt = EpollEvent { events: EPOLLIN | EPOLLWAKEUP, data: 0x101 };
    eq("add LT", ep_ctl(ep, EPOLL_CTL_ADD, r, Some(&lt)), 0)?;
    eq("LT empty", ep_wait(ep, &mut evs, 0), 0)?;

    write_all(w, b"ab")?;
    eq("LT ready 1", ep_wait(ep, &mut evs, 0), 1)?;
    eq("LT events 1", { evs[0].events } as i64, EPOLLIN as i64)?;
    eq("LT data 1", { evs[0].data } as i64, 0x101)?;

    // Partial read leaves 1 byte: LT must keep reporting.
    read_n(r, &mut buf[..1])?;
    eq("LT ready 2 after partial read", ep_wait(ep, &mut evs, 0), 1)?;
    eq("LT events 2", { evs[0].events } as i64, EPOLLIN as i64)?;

    // Drain remaining byte: LT stops reporting.
    read_n(r, &mut buf[..1])?;
    eq("LT drained", ep_wait(ep, &mut evs, 0), 0)?;

    // Edge-triggered: reports once per new write even while unread bytes remain.
    let et = EpollEvent { events: EPOLLIN | EPOLLET, data: 0x202 };
    eq("mod ET", ep_ctl(ep, EPOLL_CTL_MOD, r, Some(&et)), 0)?;
    eq("ET empty", ep_wait(ep, &mut evs, 0), 0)?;

    write_all(w, b"xy")?;
    eq("ET first report", ep_wait(ep, &mut evs, 0), 1)?;
    eq("ET data", { evs[0].data } as i64, 0x202)?;
    eq("ET quiet without new write", ep_wait(ep, &mut evs, 0), 0)?;

    // A new write produces a fresh edge even though "xy" was not drained.
    write_all(w, b"z")?;
    eq("ET second edge", ep_wait(ep, &mut evs, 0), 1)?;
    eq("ET quiet again", ep_wait(ep, &mut evs, 0), 0)?;
    read_n(r, &mut buf[..3])?;

    // One-shot: reports once, stays disarmed across new writes until MOD.
    let os = EpollEvent { events: EPOLLIN | EPOLLONESHOT, data: 0x303 };
    eq("mod ONESHOT", ep_ctl(ep, EPOLL_CTL_MOD, r, Some(&os)), 0)?;
    write_all(w, b"1")?;
    eq("ONESHOT first report", ep_wait(ep, &mut evs, 0), 1)?;
    eq("ONESHOT data", { evs[0].data } as i64, 0x303)?;
    eq("ONESHOT disarmed", ep_wait(ep, &mut evs, 0), 0)?;

    // Even a fresh write does NOT re-arm a disarmed ONESHOT interest.
    write_all(w, b"2")?;
    eq("ONESHOT stays disarmed after write", ep_wait(ep, &mut evs, 0), 0)?;

    // Re-arming with MOD immediately surfaces the pending readable pipe.
    let os2 = EpollEvent { events: EPOLLIN | EPOLLONESHOT, data: 0x304 };
    eq("rearm ONESHOT", ep_ctl(ep, EPOLL_CTL_MOD, r, Some(&os2)), 0)?;
    eq("ONESHOT rearmed report", ep_wait(ep, &mut evs, 0), 1)?;
    eq("ONESHOT rearmed data", { evs[0].data } as i64, 0x304)?;
    read_n(r, &mut buf[..2])?;

    close(r)?;
    close(w)?;
    close(ep)
}

fn check_epoll_ctl_errors_and_exclusive() -> R {
    eq("epoll_create(0) EINVAL", sys(SYS_EPOLL_CREATE, [0; 6]), EINVAL)?;
    eq("epoll_create(-1) EINVAL", sys(SYS_EPOLL_CREATE, [(-1i32) as u32 as u64, 0, 0, 0, 0, 0]), EINVAL)?;
    let ep_old = ok("epoll_create(1)", sys(SYS_EPOLL_CREATE, [1, 0, 0, 0, 0, 0]))? as i32;
    close(ep_old)?;

    eq("epoll_create1 bad flags", sys(SYS_EPOLL_CREATE1, [1, 0, 0, 0, 0, 0]), EINVAL)?;
    let ep = ep_create1(EPOLL_CLOEXEC)?;
    // Linux v6.12 fs/eventpoll.c:1067-1076: no read or write handler.
    // The merged anonymous-object I/O paths must reject epoll, including
    // empty transfers and faulting iovec pointers (before importing them).
    let mut io_buf = [0u8; 8];
    for syscall in [SYS_READ, SYS_WRITE] {
        for len in [0, 8] {
            eq("epoll scalar I/O EINVAL", sys(syscall, [ep as u64, io_buf.as_mut_ptr() as u64, len, 0, 0, 0]), EINVAL)?;
        }
    }
    for syscall in [SYS_READV, SYS_WRITEV] {
        let iov = [io_buf.as_mut_ptr() as u64, io_buf.len() as u64];
        eq("epoll vector I/O EINVAL", sys(syscall, [ep as u64, iov.as_ptr() as u64, 1, 0, 0, 0]), EINVAL)?;
        eq("epoll empty vector EINVAL", sys(syscall, [ep as u64, 0, 0, 0, 0, 0]), EINVAL)?;
        eq("epoll bad vector EINVAL", sys(syscall, [ep as u64, 1, 1, 0, 0, 0]), EINVAL)?;
    }
    eq("EPOLL_CLOEXEC set", sys(SYS_FCNTL, [ep as u64, F_GETFD, 0, 0, 0, 0]), FD_CLOEXEC)?;
    let ep_dup = ok("dup(ep)", sys(SYS_DUP, [ep as u64, 0, 0, 0, 0, 0]))? as i32;

    let (r, w) = make_pipe()?;
    let reg_path = b"/tmp/m9-epoll-reg\0";
    let reg = ok(
        "open regular file",
        sys(SYS_OPENAT, [AT_FDCWD, reg_path.as_ptr() as u64, O_RDWR | O_CREAT | O_TRUNC, 0o644, 0, 0]),
    )? as i32;
    let null_fd =
        ok("open /dev/null", sys(SYS_OPENAT, [AT_FDCWD, b"/dev/null\0".as_ptr() as u64, O_RDWR, 0, 0, 0]))? as i32;
    let opath = ok("open O_PATH", sys(SYS_OPENAT, [AT_FDCWD, b"/tmp\0".as_ptr() as u64, O_PATH, 0, 0, 0]))? as i32;

    let ev = EpollEvent { events: EPOLLIN, data: 1 };

    // Linux error order: -EPERM for non-pollable target even when epfd is not an epoll.
    eq("reg on reg EPERM", ep_ctl(reg, EPOLL_CTL_ADD, reg, Some(&ev)), EPERM)?;
    eq("reg on ep EPERM", ep_ctl(ep, EPOLL_CTL_ADD, reg, Some(&ev)), EPERM)?;
    eq("/dev/null EPERM", ep_ctl(ep, EPOLL_CTL_ADD, null_fd, Some(&ev)), EPERM)?;
    eq("closed fd EBADF", ep_ctl(ep, EPOLL_CTL_ADD, 999, Some(&ev)), EBADF)?;
    eq("O_PATH fd EBADF", ep_ctl(ep, EPOLL_CTL_ADD, opath, Some(&ev)), EBADF)?;
    eq("O_PATH epfd EBADF", ep_ctl(opath, EPOLL_CTL_ADD, r, Some(&ev)), EBADF)?;
    eq("self add EINVAL", ep_ctl(ep, EPOLL_CTL_ADD, ep, Some(&ev)), EINVAL)?;
    eq("dup(epfd) add EINVAL", ep_ctl(ep, EPOLL_CTL_ADD, ep_dup, Some(&ev)), EINVAL)?;
    eq("non-epoll epfd EINVAL", ep_ctl(r, EPOLL_CTL_ADD, w, Some(&ev)), EINVAL)?;

    // EPOLLEXCLUSIVE rules.
    let ep2 = ep_create1(0)?;
    let excl = EpollEvent { events: EPOLLIN | EPOLLEXCLUSIVE, data: 7 };
    eq("EPOLLEXCLUSIVE on epoll target EINVAL", ep_ctl(ep, EPOLL_CTL_ADD, ep2, Some(&excl)), EINVAL)?;
    let excl_os = EpollEvent { events: EPOLLIN | EPOLLEXCLUSIVE | EPOLLONESHOT, data: 7 };
    eq("EPOLLEXCLUSIVE | EPOLLONESHOT EINVAL", ep_ctl(ep, EPOLL_CTL_ADD, r, Some(&excl_os)), EINVAL)?;
    eq("EPOLLEXCLUSIVE ADD accepted", ep_ctl(ep, EPOLL_CTL_ADD, r, Some(&excl)), 0)?;
    eq("EPOLLEXCLUSIVE dup ADD EEXIST", ep_ctl(ep, EPOLL_CTL_ADD, r, Some(&excl)), EEXIST)?;
    eq("EPOLLEXCLUSIVE MOD with flag EINVAL", ep_ctl(ep, EPOLL_CTL_MOD, r, Some(&excl)), EINVAL)?;
    eq("EPOLLEXCLUSIVE MOD existing EINVAL", ep_ctl(ep, EPOLL_CTL_MOD, r, Some(&ev)), EINVAL)?;
    eq("EPOLLEXCLUSIVE DEL", ep_ctl(ep, EPOLL_CTL_DEL, r, None), 0)?;
    eq("DEL absent ENOENT", ep_ctl(ep, EPOLL_CTL_DEL, r, None), ENOENT)?;
    eq("MOD absent ENOENT", ep_ctl(ep, EPOLL_CTL_MOD, r, Some(&ev)), ENOENT)?;

    for fd in [ep2, ep_dup, ep, r, w, reg, null_fd, opath] {
        close(fd)?;
    }
    eq("unlink reg", sys(SYS_UNLINKAT, [AT_FDCWD, reg_path.as_ptr() as u64, 0, 0, 0, 0]), 0)
}

fn check_dup_fork_and_proc_fd() -> R {
    let ep = ep_create1(0)?;

    check_proc_fd_link(ep, b"anon_inode:[eventpoll]")?;

    // Closing one dup keeps the interest; closing the last removes it.
    let (r1, w) = make_pipe()?;
    let r2 = ok("dup(r1)", sys(SYS_DUP, [r1 as u64, 0, 0, 0, 0, 0]))? as i32;
    let ev = EpollEvent { events: EPOLLIN, data: 0xDEAD_BEEF };
    eq("add r1", ep_ctl(ep, EPOLL_CTL_ADD, r1, Some(&ev)), 0)?;
    close(r1)?;
    eq("DEL closed r1 EBADF", ep_ctl(ep, EPOLL_CTL_DEL, r1, None), EBADF)?;

    write_all(w, b"d")?;
    let mut out = [EpollEvent::default(); 2];
    eq("dup survives close(r1)", ep_wait(ep, &mut out, 0), 1)?;
    eq("dup reports old data", { out[0].data } as i64, 0xDEAD_BEEF)?;
    close(r2)?;
    eq("last close removes interest", ep_wait(ep, &mut out, 0), 0)?;
    close(w)?;

    // `fork` shares the epoll description: child sees event on shared epoll
    // and its DEL is visible to the parent.
    let (fr, fw) = make_pipe()?;
    let fev = EpollEvent { events: EPOLLIN, data: 0xF00D };
    eq("add fork pipe", ep_ctl(ep, EPOLL_CTL_ADD, fr, Some(&fev)), 0)?;

    let child = ok("fork", sys(SYS_FORK, [0; 6]))?;
    if child == 0 {
        let mut cev = [EpollEvent::default(); 2];
        let n = ep_wait(ep, &mut cev, -1);
        if n != 1 || { cev[0].data } != 0xF00D {
            exit_group(11);
        }
        let mut b = [0u8; 1];
        if sys(SYS_READ, [fr as u64, b.as_mut_ptr() as u64, 1, 0, 0, 0]) != 1 || b[0] != b'k' {
            exit_group(12);
        }
        if ep_ctl(ep, EPOLL_CTL_DEL, fr, None) != 0 {
            exit_group(13);
        }
        exit_group(0);
    }

    // The scenario observes this child blocked in ps before releasing the
    // parent with a line of input. No timeout can hide a lost notification.
    say!("epoll-wake: pid={child} fork-ready");
    read_n(0, &mut [0u8; 2])?;
    write_all(fw, b"k")?;
    wait_child(child)?;
    // Child deleted `fr` from the shared epoll set, so another write must not appear.
    write_all(fw, b"m")?;
    eq("shared epoll reflects child DEL", ep_wait(ep, &mut out, 0), 0)?;

    close(fr)?;
    close(fw)?;
    close(ep)
}

fn check_nested_epoll_and_poll() -> R {
    // 5-instance chain: c[0] -> c[1] -> c[2] -> c[3] -> c[4] (4 nesting edges) accepted.
    let mut c = [0i32; 7];
    for slot in &mut c {
        *slot = ep_create1(0)?;
    }
    let ev = EpollEvent { events: EPOLLIN, data: 0x55 };
    for i in 0..4 {
        eq("nest chain edge", ep_ctl(c[i], EPOLL_CTL_ADD, c[i + 1], Some(&ev)), 0)?;
    }
    // 6th instance downward (`c[4] -> c[5]`) and upward (`c[6] -> c[0]`) -> -ELOOP.
    eq("6th nest downward ELOOP", ep_ctl(c[4], EPOLL_CTL_ADD, c[5], Some(&ev)), ELOOP)?;
    eq("6th nest upward ELOOP", ep_ctl(c[6], EPOLL_CTL_ADD, c[0], Some(&ev)), ELOOP)?;
    // Direct and transitive cycles -> -ELOOP.
    eq("direct cycle ELOOP", ep_ctl(c[1], EPOLL_CTL_ADD, c[0], Some(&ev)), ELOOP)?;
    eq("deep cycle ELOOP", ep_ctl(c[4], EPOLL_CTL_ADD, c[0], Some(&ev)), ELOOP)?;

    // Readiness propagates through the entire 5-instance chain and wakes `poll(epfd)`.
    let (r, w) = make_pipe()?;
    let pev = EpollEvent { events: EPOLLIN, data: 0x99 };
    eq("add pipe to leaf epoll", ep_ctl(c[4], EPOLL_CTL_ADD, r, Some(&pev)), 0)?;
    let mut out = [EpollEvent::default(); 2];
    eq("root epoll empty", ep_wait(c[0], &mut out, 0), 0)?;

    let mut pfd = [PollFd { fd: c[0], events: POLLIN, revents: 0 }];
    eq("poll empty epfd", sys(SYS_POLL, [pfd.as_mut_ptr() as u64, 1, 0, 0, 0, 0]), 0)?;

    // The child waits for the scenario to confirm the parent blocked in
    // poll(c[0], -1), then writes once. No timeout supplies a final recheck.
    let me = ok("getpid", sys(SYS_GETPID, [0; 6]))?;
    let child = ok("fork nested writer", sys(SYS_FORK, [0; 6]))?;
    if child == 0 {
        if read_n(0, &mut [0u8; 2]).is_err() || write_all(w, b"n").is_err() {
            exit_group(14);
        }
        exit_group(0);
    }
    say!("epoll-wake: pid={me} nested-ready");
    eq("poll wakes on nested epfd", sys(SYS_POLL, [pfd.as_mut_ptr() as u64, 1, u64::MAX, 0, 0, 0]), 1)?;
    ensure("poll revents POLLIN", (pfd[0].revents & POLLIN) != 0, i64::from(pfd[0].revents))?;
    eq("root epoll reports inner", ep_wait(c[0], &mut out, 0), 1)?;
    eq("root epoll data", { out[0].data } as i64, 0x55)?;
    eq("leaf epoll reports pipe", ep_wait(c[4], &mut out, 0), 1)?;
    eq("leaf epoll data", { out[0].data } as i64, 0x99)?;

    wait_child(child)?;

    let mut b = [0u8; 1];
    read_n(r, &mut b)?;
    eq("root epoll drained", ep_wait(c[0], &mut out, 0), 0)?;

    close(r)?;
    close(w)?;
    for fd in c {
        close(fd)?;
    }
    Ok(())
}

fn check_timeouts_and_pwait_pending() -> R {
    let ep = ep_create1(0)?;
    let (r, w) = make_pipe()?;
    let ev = EpollEvent { events: EPOLLIN, data: 1 };
    eq("add timeout pipe", ep_ctl(ep, EPOLL_CTL_ADD, r, Some(&ev)), 0)?;
    let mut out = [EpollEvent::default(); 2];

    // Busy-host rule: floors only, never ceilings.
    let t0 = clock_ns(CLOCK_MONOTONIC)?;
    eq("epoll_wait 50ms timeout", ep_wait(ep, &mut out, 50), 0)?;
    let elapsed = clock_ns(CLOCK_MONOTONIC)? - t0;
    ensure("epoll_wait elapsed >= 50ms", elapsed >= 50 * NS_PER_MS, elapsed as i64)?;

    let ts = Timespec { tv_sec: 0, tv_nsec: (50 * NS_PER_MS) as i64 };
    let t0 = clock_ns(CLOCK_MONOTONIC)?;
    eq(
        "epoll_pwait2 50ms timeout",
        sys(SYS_EPOLL_PWAIT2, [ep as u64, out.as_mut_ptr() as u64, out.len() as u64, &ts as *const _ as u64, 0, 8]),
        0,
    )?;
    let elapsed2 = clock_ns(CLOCK_MONOTONIC)? - t0;
    ensure("epoll_pwait2 elapsed >= 50ms", elapsed2 >= 50 * NS_PER_MS, elapsed2 as i64)?;

    // Invalid `epoll_pwait2` timespec and `epoll_pwait` sigsetsize.
    let bad_ts = Timespec { tv_sec: 0, tv_nsec: NS_PER_SEC as i64 };
    eq(
        "epoll_pwait2 bad tv_nsec EINVAL",
        sys(SYS_EPOLL_PWAIT2, [ep as u64, out.as_mut_ptr() as u64, 1, &bad_ts as *const _ as u64, 0, 8]),
        EINVAL,
    )?;
    let neg_ts = Timespec { tv_sec: -1, tv_nsec: 0 };
    eq(
        "epoll_pwait2 neg tv_sec EINVAL",
        sys(SYS_EPOLL_PWAIT2, [ep as u64, out.as_mut_ptr() as u64, 1, &neg_ts as *const _ as u64, 0, 8]),
        EINVAL,
    )?;
    let empty_mask = 0u64;
    eq(
        "epoll_pwait bad sigsetsize EINVAL",
        sys(SYS_EPOLL_PWAIT, [ep as u64, out.as_mut_ptr() as u64, 1, 0, &empty_mask as *const _ as u64, 4]),
        EINVAL,
    )?;

    // `epoll_pwait` mask rule (pending before call): block SIGUSR1, raise it
    // so it is pending while blocked, confirm handler did not run, then call
    // `epoll_pwait` with a mask that unblocks SIGUSR1. Even with SA_RESTART,
    // `epoll_pwait` returns -EINTR immediately, delivers the handler, and
    // restores the original blocked mask.
    SIG_HITS.store(0, Ordering::SeqCst);
    set_usr1_action(usr1_handler as *const () as u64, SA_RESTORER | SA_RESTART)?;
    let usr1_bit = 1u64 << (SIGUSR1 - 1);
    set_sigmask(usr1_bit)?;
    let me = ok("getpid", sys(SYS_GETPID, [0; 6]))?;
    eq("kill(self, SIGUSR1)", sys(SYS_KILL, [me as u64, SIGUSR1, 0, 0, 0, 0]), 0)?;
    let mut pend = 0u64;
    eq("rt_sigpending", sys(SYS_RT_SIGPENDING, [&mut pend as *mut _ as u64, 8, 0, 0, 0, 0]), 0)?;
    ensure("SIGUSR1 pending while blocked", (pend & usr1_bit) != 0, pend as i64)?;
    eq("handler not yet run", SIG_HITS.load(Ordering::SeqCst) as i64, 0)?;

    eq(
        "epoll_pwait pending signal EINTR",
        sys(SYS_EPOLL_PWAIT, [ep as u64, out.as_mut_ptr() as u64, 1, u64::MAX, &TEMP_MASK as *const _ as u64, 8]),
        EINTR,
    )?;
    eq("handler ran once", SIG_HITS.load(Ordering::SeqCst) as i64, 1)?;
    eq("sigmask restored", get_sigmask()? as i64, usr1_bit as i64)?;

    set_sigmask(0)?;
    close(r)?;
    close(w)?;
    close(ep)
}

fn check_pwait_restores_mask_on_success() -> R {
    let ep = ep_create1(0)?;
    let (r, w) = make_pipe()?;
    eq("add mask pipe", ep_ctl(ep, EPOLL_CTL_ADD, r, Some(&EpollEvent { events: EPOLLIN, data: 7 })), 0)?;
    let original = 1 << (SIGUSR1 - 1);
    set_sigmask(original)?;
    let mut out = [EpollEvent::default(); 1];
    let zero = Timespec::default();
    for ready in [false, true] {
        if ready {
            write_all(w, b"r")?;
        }
        for syscall in [SYS_EPOLL_PWAIT, SYS_EPOLL_PWAIT2] {
            let timeout = if syscall == SYS_EPOLL_PWAIT { 0 } else { &zero as *const _ as u64 };
            // The second masked call also detects a leaked saved-mask slot.
            for _ in 0..2 {
                eq(
                    "masked normal return",
                    sys(syscall, [ep as u64, out.as_mut_ptr() as u64, 1, timeout, &TEMP_MASK as *const _ as u64, 8]),
                    i64::from(ready),
                )?;
                eq("normal return restored original mask", get_sigmask()? as i64, original as i64)?;
            }
        }
    }
    set_sigmask(0)?;
    close(r)?;
    close(w)?;
    close(ep)
}

fn run_pwait_blocked() -> R {
    let ep = ep_create1(0)?;
    let (r, w) = make_pipe()?;
    let ev = EpollEvent { events: EPOLLIN, data: 1 };
    eq("add blocked pipe", ep_ctl(ep, EPOLL_CTL_ADD, r, Some(&ev)), 0)?;

    SIG_HITS.store(0, Ordering::SeqCst);
    set_usr1_action(usr1_handler as *const () as u64, SA_RESTORER | SA_RESTART)?;
    let usr1_bit = 1u64 << (SIGUSR1 - 1);
    set_sigmask(usr1_bit)?;

    let pid = ok("getpid", sys(SYS_GETPID, [0; 6]))?;
    say!("epoll-pwait: pid={pid} ready");

    let mut out = [EpollEvent::default(); 2];
    let ret = sys(
        SYS_EPOLL_PWAIT,
        [ep as u64, out.as_mut_ptr() as u64, out.len() as u64, (-1i32) as u32 as u64, &TEMP_MASK as *const _ as u64, 8],
    );
    eq("epoll_pwait blocked EINTR", ret, EINTR)?;
    ensure("handler ran", SIG_HITS.load(Ordering::SeqCst) >= 1, SIG_HITS.load(Ordering::SeqCst) as i64)?;
    eq("handler ran under temporary mask", USR1_MASK.load(Ordering::SeqCst) as i64, (TEMP_MASK | usr1_bit) as i64)?;
    eq("sigmask restored after blocked EINTR", get_sigmask()? as i64, usr1_bit as i64)?;

    set_sigmask(0)?;
    close(r)?;
    close(w)?;
    close(ep)
}

fn parse_u64(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() {
        return None;
    }
    let mut val = 0u64;
    for &b in bytes {
        if !b.is_ascii_digit() {
            return None;
        }
        val = val.checked_mul(10)?.checked_add(u64::from(b - b'0'))?;
    }
    Some(val)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn main(argc: c_int, argv: *const *const u8) -> c_int {
    // SAFETY: the C runtime passes `argc` NUL-terminated strings in `argv`.
    let (args, n) = unsafe { common::args(argc, argv) };
    if n < 2 {
        say!("{USAGE}");
        return common::USAGE_EXIT;
    }
    match args[1] {
        b"evfd" => run_checks(&[
            ("eventfd_counter_and_semaphore", &check_eventfd_counter_and_semaphore),
            ("eventfd_blocking_and_overflow_wait", &check_eventfd_blocking_and_overflow_wait),
            ("eventfd_poll_masks", &check_eventfd_poll_masks),
            ("timerfd_monotonic_oneshot_and_interval", &check_timerfd_monotonic_oneshot_and_interval),
            ("timerfd_realtime_abstime_and_poll", &check_timerfd_realtime_abstime_and_poll),
            ("signal_interruption", &check_signal_interruption),
        ]),
        b"epoll" => run_checks(&[
            ("lt_et_and_oneshot", &check_lt_et_and_oneshot),
            ("epoll_ctl_errors_and_exclusive", &check_epoll_ctl_errors_and_exclusive),
            ("dup_fork_and_proc_fd", &check_dup_fork_and_proc_fd),
            ("nested_epoll_and_poll", &check_nested_epoll_and_poll),
            ("timeouts_and_pwait_pending", &check_timeouts_and_pwait_pending),
            ("pwait_restores_mask_on_success", &check_pwait_restores_mask_on_success),
        ]),
        b"epoll-pwait-blocked" => run_checks(&[("epoll_pwait_blocked_signal", &run_pwait_blocked)]),
        b"send-sig" if n == 4 => {
            let (Some(pid), Some(sig)) = (parse_u64(args[2]), parse_u64(args[3])) else {
                say!("usage: m9_io send-sig <pid> <sig>");
                return common::USAGE_EXIT;
            };
            run_checks(&[("send_sig", &|| eq("kill", sys(SYS_KILL, [pid, sig, 0, 0, 0, 0]), 0))])
        }
        _ => {
            say!("{USAGE}");
            common::USAGE_EXIT
        }
    }
}
