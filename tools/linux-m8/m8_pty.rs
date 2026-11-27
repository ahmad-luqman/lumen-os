//! C9 PTY integration checks and #437 `ptyrun` relay regression tests. Pipes,
//! FIFOs and wait4 statuses order every process transition; time checks impose
//! only a requested timeout's lower bound.
#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

#[macro_use]
mod common;
use common::*;
use core::ffi::c_int;

// SAFETY: musl exposes `environ` as a NUL-terminated array of C string pointers.
unsafe extern "C" {
    static environ: *const *const u8;
}

const AT_FDCWD: u64 = (-100i64) as u64;
const O_RDONLY: u64 = 0;
const O_WRONLY: u64 = 1;
const RDWR: u64 = 2;
const NONBLOCK: u64 = 0x800;
const NOCTTY: u64 = 0x100;
const O_CLOEXEC: u64 = 0x8_0000;
const S_IFIFO: u64 = 0o010_000;
const F_GETFL: u64 = 3;
const EINTR: i64 = -4;
const TCGETS: u64 = 0x5401;
const TCSETS: u64 = 0x5402;
const TCSETSW: u64 = 0x5403;
const TCFLSH: u64 = 0x540b;
const TCSETSF: u64 = 0x5404;
const SETCTTY: u64 = 0x540e;
const GETPGRP: u64 = 0x540f;
const SETPGRP: u64 = 0x5410;
const TIOCOUTQ: u64 = 0x5411;
const TIOCGWINSZ: u64 = 0x5413;
const TIOCSWINSZ: u64 = 0x5414;
const FIONREAD: u64 = 0x541b;
const SYS_SCHED_YIELD: u64 = 24;
const NOTTY: u64 = 0x5422;
const GETSID: u64 = 0x5429;
const PTN: u64 = 0x80045430;
const PTLCK: u64 = 0x40045431;
const POLLIN: i16 = 0x0001;
const POLLOUT: i16 = 0x0004;
const SIGHUP: u64 = 1;
const SIGUSR1: u64 = 10;
const SIGPIPE: u64 = 13;
const SIGWINCH: u64 = 28;
const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;

fn ioctl(fd: i32, cmd: u64, arg: u64) -> i64 {
    sys(SYS_IOCTL, [fd as u64, cmd, arg, 0, 0, 0])
}
fn open(path: &[u8], flags: u64) -> i64 {
    sys(SYS_OPENAT, [(-100i64) as u64, path.as_ptr() as u64, flags, 0, 0, 0])
}
fn read(fd: i32, buf: &mut [u8]) -> i64 {
    sys(SYS_READ, [fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0])
}
fn write(fd: i32, buf: &[u8]) -> R {
    eq("write", sys(SYS_WRITE, [fd as u64, buf.as_ptr() as u64, buf.len() as u64, 0, 0, 0]), buf.len() as i64)
}
fn receive(fd: i32, want: &[u8]) -> R {
    let mut buf = [0; 64];
    eq("read count", read(fd, &mut buf[..want.len()]), want.len() as i64)?;
    ensure("read bytes", &buf[..want.len()] == want, buf[0] as i64)
}
fn wait(pid: i64, options: u64) -> Result<i32, Fail> {
    let mut status = 0;
    eq("wait4 pid", sys(SYS_WAIT4, [pid as u64, &mut status as *mut i32 as u64, options, 0, 0, 0]), pid)?;
    Ok(status)
}
fn child_end(result: R) -> ! {
    let status = match result {
        Ok(()) => 0,
        Err(f) => {
            say!("FAIL child: {} (got {})", f.what, f.got);
            1
        }
    };
    // SAFETY: terminates this child without running copied parent destructors.
    unsafe { _exit(status) }
}
fn fork() -> Result<i64, Fail> {
    ok("fork", sys(SYS_FORK, [0; 6]))
}
fn isolated(check: fn() -> R) -> R {
    let pid = fork()?;
    if pid == 0 {
        child_end((|| {
            ok("setsid", sys(112, [0; 6]))?;
            check()
        })());
    }
    eq("isolated child exit", wait(pid, 0)? as i64, 0)
}
fn kill(pid: i64, signal: u64) -> R {
    eq("kill", sys(62, [pid as u64, signal, 0, 0, 0, 0]), 0)
}
fn block(signals: &[u32]) -> R {
    let mask = signals.iter().fold(0u64, |mask, sig| mask | 1 << (sig - 1));
    eq("sigprocmask", sys(SYS_RT_SIGPROCMASK, [0, &mask as *const u64 as u64, 0, 8, 0, 0]), 0)
}
fn await_signal(sig: u32) -> R {
    let mask = 1u64 << (sig - 1);
    let mut info = [0u8; 128];
    eq("sigtimedwait", sys(128, [&mask as *const u64 as u64, info.as_mut_ptr() as u64, 0, 8, 0, 0]), sig as i64)
}
fn nonblock(fd: i32, on: bool) -> R {
    let value = i32::from(on);
    eq("FIONBIO", ioctl(fd, 0x5421, &value as *const i32 as u64), 0)
}
fn group(fd: i32, pid: i64) -> R {
    let group = pid as i32;
    eq("TIOCSPGRP", ioctl(fd, SETPGRP, &group as *const i32 as u64), 0)
}
fn getint(fd: i32, cmd: u64) -> Result<i32, Fail> {
    let mut value = -1;
    eq("integer ioctl", ioctl(fd, cmd, &mut value as *mut i32 as u64), 0)?;
    Ok(value)
}
fn termios(fd: i32) -> Result<[u8; 36], Fail> {
    let mut termios = [0; 36];
    eq("TCGETS", ioctl(fd, TCGETS, termios.as_mut_ptr() as u64), 0)?;
    Ok(termios)
}
fn set(fd: i32, termios: &[u8; 36]) -> R {
    eq("TCSETS", ioctl(fd, TCSETS, termios.as_ptr() as u64), 0)
}
fn raw(fd: i32, min: u8, time: u8) -> R {
    let mut t = termios(fd)?;
    t[..8].fill(0); // input/output translations off
    t[12..16].fill(0); // canonical, echo and ISIG off
    t[17 + 6] = min;
    t[17 + 5] = time;
    set(fd, &t)
}
fn poll(fd: i32, events: i16) -> Result<i16, Fail> {
    let mut p = [0u8; 8];
    p[..4].copy_from_slice(&fd.to_ne_bytes());
    p[4..6].copy_from_slice(&events.to_ne_bytes());
    ok("poll", sys(SYS_POLL, [p.as_mut_ptr() as u64, 1, 0, 0, 0, 0]))?;
    Ok(i16::from_ne_bytes(p[6..].try_into().unwrap()))
}
fn now() -> Result<u64, Fail> {
    let mut t = [0u64; 2];
    eq("clock_gettime", sys(SYS_CLOCK_GETTIME, [1, t.as_mut_ptr() as u64, 0, 0, 0, 0]), 0)?;
    Ok(t[0] * 1_000_000_000 + t[1])
}
struct Pair {
    master: i32,
    slave: i32,
    path: [u8; 16],
}
impl Pair {
    fn new(flags: u64) -> Result<Self, Fail> {
        let master = ok("ptmx", open(b"/dev/ptmx\0", RDWR | NONBLOCK))? as i32;
        let n = getint(master, PTN)?;
        ensure("PTY number", (0..256).contains(&n), n as i64)?;
        let mut path = [0; 16];
        path[..9].copy_from_slice(b"/dev/pts/");
        let mut at = 9;
        for place in [100, 10, 1] {
            if n >= place || place == 1 {
                path[at] = b'0' + (n / place % 10) as u8;
                at += 1;
            }
        }
        eq("locked slave", open(&path, RDWR), -5)?;
        eq("initial lock", getint(master, 0x80045439)? as i64, 1)?;
        eq("master before first slave", read(master, &mut [0]), EAGAIN)?;
        let unlock = 0i32;
        eq("unlockpt", ioctl(master, PTLCK, &unlock as *const i32 as u64), 0)?;
        let slave = ok("slave", open(&path, RDWR | flags))? as i32;
        let lock = 1i32;
        eq("slave NULL TIOCSPTLCK ENOTTY", ioctl(slave, PTLCK, 0), -25)?;
        eq("slave valid TIOCSPTLCK ENOTTY", ioctl(slave, PTLCK, &lock as *const i32 as u64), -25)?;
        eq("slave TIOCSPTLCK leaves lock unchanged", getint(master, 0x80045439)? as i64, 0)?;
        eq("slave mode", (fstat_mode(slave)? & 0o177777) as i64, 0o20620)?;
        Ok(Self { master, slave, path })
    }
}
impl Drop for Pair {
    fn drop(&mut self) {
        let _ = close(self.slave);
        let _ = close(self.master);
    }
}
fn data_and_timers() -> R {
    let p = Pair::new(NOCTTY)?;
    eq("no automatic ctty", open(b"/dev/tty\0", RDWR), ENXIO)?;
    eq("slave unowned pgrp", ioctl(p.slave, GETPGRP, &mut 0i32 as *mut i32 as u64), -25)?;
    nonblock(p.slave, true)?;
    write(p.master, b"ab\x7fc\n")?;
    eq("canonical slave POLLIN|POLLRDNORM", poll(p.slave, 0x41)? as i64, 0x41)?;
    eq("echo master POLLIN|POLLRDNORM", poll(p.master, 0x41)? as i64, 0x41)?;
    receive(p.slave, b"ac\n")?;
    receive(p.master, b"ab\x08 \x08c\r\n")?;
    eq("drained canonical", read(p.slave, &mut [0; 8]), EAGAIN)?;
    write(p.slave, b"out\n")?;
    eq("TIOCOUTQ", getint(p.slave, 0x5411)? as i64, 5)?;
    eq("output master POLLIN|POLLRDNORM", poll(p.master, 0x41)? as i64, 0x41)?;
    receive(p.master, b"out\r\n")?;
    let settings = termios(p.slave)?;
    write(p.master, b"keep\n")?;
    set(p.slave, &settings)?;
    receive(p.slave, b"keep\n")?;
    write(p.master, b"flush\n")?;
    eq("TCSETSF", ioctl(p.slave, TCSETSF, settings.as_ptr() as u64), 0)?;
    eq("TCSETSF discards queued input", read(p.slave, &mut [0; 8]), EAGAIN)?;
    eq("TCSETSF clears readiness", poll(p.slave, 0x41)? as i64, 0)?;
    write(p.master, b"partial")?;
    eq("partial canonical not ready", poll(p.slave, 3)? as i64, 0)?;
    eq("TCIFLUSH", ioctl(p.slave, 0x540b, 0), 0)?;
    eq("TCOFLUSH", ioctl(p.slave, 0x540b, 1), 0)?;
    eq("invalid flush", ioctl(p.slave, 0x540b, 3), EINVAL)?;
    eq("packet mode refused", ioctl(p.master, 0x5420, 0), EINVAL)?;
    raw(p.slave, 2, 0)?;
    write(p.master, b"x")?;
    eq("VMIN poll", poll(p.slave, 3)? as i64, 0)?;
    eq("raw queued count", getint(p.slave, 0x541b)? as i64, 1)?;
    receive(p.slave, b"x")?; // O_NONBLOCK bypasses VMIN
    raw(p.slave, 0, 0)?;
    nonblock(p.slave, false)?;
    eq("MIN0 TIME0", read(p.slave, &mut [0; 8]), 0)?;
    raw(p.slave, 0, 1)?;
    let start = now()?;
    eq("VTIME empty timeout", read(p.slave, &mut [0; 8]), 0)?;
    ensure("VTIME floor", now()? - start >= 100_000_000, 0)?;
    raw(p.slave, 2, 1)?;
    write(p.master, b"y")?;
    let start = now()?;
    let mut bytes = [0; 2];
    eq("interbyte count", read(p.slave, &mut bytes), 1)?;
    eq("interbyte byte", bytes[0] as i64, b'y' as i64)?;
    ensure("interbyte floor", now()? - start >= 100_000_000, 0)?;
    write(p.master, b"z")?;
    let mut b = [0; 2];
    eq("interbyte short read", read(p.slave, &mut b), 1)?;
    eq("interbyte data", b[0] as i64, b'z' as i64)?;
    close(p.slave)?;
    // Avoid closing a recycled descriptor in Drop.
    let mut p = p;
    p.slave = -1;
    eq("master last slave EIO", read(p.master, &mut [0]), -5)?;
    ensure("master HUP", poll(p.master, 0)? & 16 != 0, 0)?;
    p.slave = ok("reopen slave", open(&p.path, RDWR | NOCTTY))? as i32;
    eq("reopen clears HUP", (poll(p.master, 0)? & 16) as i64, 0)?;
    eq("reopen master empty", read(p.master, &mut [0]), EAGAIN)
}
fn job_control() -> R {
    let p = Pair::new(0)?; // session leader's readable open acquires it
    let me = sys(SYS_GETPID, [0; 6]);
    eq("master TIOCNOTTY ENOTTY", ioctl(p.master, NOTTY, 0), -25)?;
    eq("master TIOCNOTTY preserves session", getint(p.slave, GETSID)? as i64, me)?;
    eq("master TIOCNOTTY preserves foreground", getint(p.slave, GETPGRP)? as i64, me)?;
    close(ok("dev tty alias", open(b"/dev/tty\0", RDWR))? as i32)?;
    // A fork inherits ctty; setsid clears it, without detaching the parent's.
    let child = fork()?;
    if child == 0 {
        child_end((|| {
            close(ok("inherited ctty", open(b"/dev/tty\0", RDWR))? as i32)?;
            ok("child setsid", sys(112, [0; 6]))?;
            eq("setsid clears ctty", open(b"/dev/tty\0", RDWR), ENXIO)
        })());
    }
    eq("setsid child", wait(child, 0)? as i64, 0)?;
    let stops = [
        "background read SIGTTIN",
        "background TCSETS SIGTTOU",
        "background write SIGTTOU",
        "NULL TIOCSPGRP SIGTTOU before copy",
        "NULL TCSETS SIGTTOU before copy",
        "NULL TCSETSW SIGTTOU before copy",
        "NULL TCSETSF SIGTTOU before copy",
        "TCFLSH(3) SIGTTOU before EINVAL",
    ];
    for (operation, stop) in stops.into_iter().enumerate() {
        let settings = operation == 1;
        if operation == 2 {
            let mut t = termios(p.slave)?;
            let flags = u32::from_ne_bytes(t[12..16].try_into().unwrap()) | 0x100;
            t[12..16].copy_from_slice(&flags.to_ne_bytes());
            set(p.slave, &t)?;
        }
        let child = fork()?;
        if child == 0 {
            child_end((|| {
                eq("new background pgrp", sys(109, [0; 6]), 0)?;
                if settings {
                    set(p.slave, &termios(p.slave)?)?;
                } else if operation == 2 {
                    write(p.slave, b"background write")?;
                } else if operation == 3 {
                    // First attempt must stop before touching NULL. The
                    // foreground retry reaches the copy and returns EFAULT.
                    eq("foreground NULL TIOCSPGRP", ioctl(p.slave, SETPGRP, 0), -14)?;
                } else if (4..=6).contains(&operation) {
                    let cmd = [TCSETS, TCSETSW, TCSETSF][operation - 4];
                    eq("foreground NULL TCSETS* EFAULT", ioctl(p.slave, cmd, 0), -14)?;
                } else if operation == 7 {
                    eq("foreground TCFLSH(3) EINVAL", ioctl(p.slave, TCFLSH, 3), -22)?;
                } else {
                    block(&[21])?;
                    eq("blocked TTIN read EIO", read(p.slave, &mut [0]), -5)?;
                    let mask = 1u64 << 20;
                    eq("unblock TTIN", sys(SYS_RT_SIGPROCMASK, [1, &mask as *const u64 as u64, 0, 8, 0, 0]), 0)?;
                    receive(p.slave, b"go\n")?;
                }
                Ok(())
            })());
        }
        let signal = if operation == 0 { 21 } else { 22 };
        eq(stop, wait(child, 2)? as i64, (signal << 8) | 127)?;
        group(p.slave, child)?;
        if operation == 0 {
            write(p.master, b"go\n")?;
        }
        kill(child, 18)?;
        eq("foreground child resumes", wait(child, 0)? as i64, 0)?;
        block(&[22])?;
        group(p.slave, me)?;
        // Restore default unblocked TTOU for the next child.
        let mask = 1u64 << 21;
        eq("unblock TTOU", sys(SYS_RT_SIGPROCMASK, [1, &mask as *const u64 as u64, 0, 8, 0, 0]), 0)?;
    }
    let ready = pipe(0)?;
    let go = pipe(0)?;
    let child = fork()?;
    if child == 0 {
        child_end((|| {
            eq("SIGTSTP child pgrp", sys(109, [0; 6]), 0)?;
            write(ready[1], b"z")?;
            receive(go[0], b"g")
        })());
    }
    receive(ready[0], b"z")?;
    group(p.slave, child)?;
    write(p.master, b"\x1a")?;
    eq("Ctrl-Z foreground SIGTSTP", wait(child, 2)? as i64, (20 << 8) | 127)?;
    kill(child, 18)?;
    write(go[1], b"g")?;
    eq("Ctrl-Z child resumes", wait(child, 0)? as i64, 0)?;
    block(&[22])?;
    group(p.slave, me)?;
    let mask = 1u64 << 21;
    eq("unblock TTOU after Ctrl-Z", sys(SYS_RT_SIGPROCMASK, [1, &mask as *const u64 as u64, 0, 8, 0, 0]), 0)?;
    close(go[0])?;
    close(go[1])?;
    let child = fork()?;
    if child == 0 {
        child_end((|| {
            eq("SIGINT child pgrp", sys(109, [0; 6]), 0)?;
            block(&[2])?;
            write(ready[1], b"r")?;
            await_signal(2)
        })());
    }
    receive(ready[0], b"r")?;
    group(p.slave, child)?;
    write(p.master, b"\x03")?;
    eq("Ctrl-C foreground SIGINT", wait(child, 0)? as i64, 0)?;
    block(&[22, 28, 1, 18])?;
    group(p.slave, me)?;
    close(ready[0])?;
    close(ready[1])?;
    let size = [41u16, 101, 0, 0];
    eq("TIOCSWINSZ", ioctl(p.master, 0x5414, size.as_ptr() as u64), 0)?;
    await_signal(28)?;
    let mut got = [0u16; 4];
    eq("TIOCGWINSZ", ioctl(p.slave, 0x5413, got.as_mut_ptr() as u64), 0)?;
    ensure("window size", got == size, got[0] as i64)?;
    eq("TIOCNOTTY", ioctl(p.slave, NOTTY, 0), 0)?;
    await_signal(1)?;
    await_signal(18)?;
    eq("detached alias", open(b"/dev/tty\0", RDWR), ENXIO)?;
    eq("explicit TIOCSCTTY", ioctl(p.slave, SETCTTY, 0), 0)?;
    eq("idempotent TIOCSCTTY", ioctl(p.slave, SETCTTY, 0), 0)?;
    eq("explicit pgrp", getint(p.slave, GETPGRP)? as i64, me)?;
    let duplicate = ok("dup master", sys(SYS_DUP, [p.master as u64, 0, 0, 0, 0, 0]))? as i32;
    close(duplicate)?;
    eq("dup close preserves tty", getint(p.slave, GETSID)? as i64, me)?;
    let mut after_hangup = termios(p.slave)?;
    close(p.master)?;
    let mut p = p;
    p.master = -1;
    await_signal(1)?;
    await_signal(18)?;
    let before_ioctl = after_hangup;
    eq("hung-up TCGETS", ioctl(p.slave, TCGETS, after_hangup.as_mut_ptr() as u64), -5)?;
    ensure("hung-up TCGETS leaves buffer", after_hangup == before_ioctl, 0)?;
    eq("hung-up TCSETS", ioctl(p.slave, TCSETS, after_hangup.as_ptr() as u64), -5)?;
    eq("hung-up TCSETSF", ioctl(p.slave, TCSETSF, after_hangup.as_ptr() as u64), -5)?;
    eq("hung-up NULL TCGETS", ioctl(p.slave, TCGETS, 0), -5)?;
    eq("hung-up NULL TCSETS", ioctl(p.slave, TCSETS, 0), -5)?;
    eq("hung-up TIOCSPGRP", ioctl(p.slave, SETPGRP, &me as *const i64 as u64), -25)?;
    eq("hung-up NULL TIOCSPGRP", ioctl(p.slave, SETPGRP, 0), -25)?;
    eq("hung-up read", read(p.slave, &mut [0]), 0)?;
    eq("hung-up write", sys(SYS_WRITE, [p.slave as u64, b"x".as_ptr() as u64, 1, 0, 0, 0]), -5)?;
    ensure("slave HUP", poll(p.slave, 0)? & 16 != 0, 0)?;
    eq("slave node removed", open(&p.path, RDWR), -2)?;
    eq("hangup clears ctty", open(b"/dev/tty\0", RDWR), ENXIO)
}
fn orphan_stop_exit() -> R {
    let report = pipe(0)?;
    let parent = fork()?;
    if parent == 0 {
        child_end((|| {
            let child = fork()?;
            if child == 0 {
                child_end((|| {
                    eq("stopped orphan pgrp", sys(109, [0; 6]), 0)?;
                    block(&[1, 18])?;
                    kill(sys(SYS_GETPID, [0; 6]), 19)?;
                    await_signal(1)?;
                    await_signal(18)?;
                    write(report[1], b"resumed")
                })());
            }
            eq("orphan stop observed", wait(child, 2)? as i64, (19 << 8) | 127)?;
            Ok(()) // exit orphans the stopped child's group
        })());
    }
    eq("orphaning parent exit", wait(parent, 0)? as i64, 0)?;
    receive(report[0], b"resumed")?;
    close(report[0])?;
    close(report[1])
}

fn orphan_and_leader_exit() -> R {
    block(&[1, 18])?; // Closing our own master during cleanup sends HUP/CONT.
    orphan_stop_exit()?;
    let p = Pair::new(0)?;
    // Grandchild's parent leaves our session; the group is orphaned while
    // its controlling terminal still belongs to this live session leader.
    let ready = pipe(0)?;
    let go = pipe(0)?;
    let report = pipe(0)?;
    let parent = fork()?;
    if parent == 0 {
        child_end((|| {
            let child = fork()?;
            if child == 0 {
                child_end((|| {
                    eq("orphan child pgrp", sys(109, [0; 6]), 0)?;
                    write(ready[1], b"r")?;
                    receive(go[0], b"g")?;
                    eq("orphan background read EIO", read(p.slave, &mut [0]), -5)?;
                    write(report[1], b"o")
                })());
            }
            receive(ready[0], b"r")?;
            ok("parent leaves session", sys(112, [0; 6]))?;
            write(go[1], b"g")?;
            eq("orphan child exit", wait(child, 0)? as i64, 0)
        })());
    }
    receive(report[0], b"o")?;
    eq("orphan parent exit", wait(parent, 0)? as i64, 0)?;
    for fd in [ready[0], ready[1], go[0], go[1], report[0], report[1]] {
        close(fd)?;
    }
    // A separate leader takes a second pair; its foreground child survives
    // the leader and confirms both SIGHUP and session-wide disassociation.
    let q = Pair::new(NOCTTY)?;
    let setup = pipe(0)?;
    let done = pipe(0)?;
    let leader = fork()?;
    if leader == 0 {
        child_end((|| {
            ok("second setsid", sys(112, [0; 6]))?;
            eq("second acquire", ioctl(q.slave, SETCTTY, 0), 0)?;
            let child = fork()?;
            if child == 0 {
                child_end((|| {
                    eq("foreground observer group", sys(109, [0; 6]), 0)?;
                    block(&[1])?;
                    write(setup[1], b"s")?;
                    await_signal(1)?;
                    eq("leader exit clears ctty", open(b"/dev/tty\0", RDWR), ENXIO)?;
                    write(q.slave, b"alive")?;
                    write(done[1], b"d")
                })());
            }
            receive(setup[0], b"s")?;
            group(q.slave, child)?;
            Ok(()) // exit sends HUP to the foreground child
        })());
    }
    eq("leader exit", wait(leader, 0)? as i64, 0)?;
    receive(done[0], b"d")?;
    receive(q.master, b"alive")?;
    for fd in [setup[0], setup[1], done[0], done[1]] {
        close(fd)?;
    }
    Ok(())
}

struct Fifo {
    path: &'static [u8],
    fd: i32,
}

impl Fifo {
    fn new(path: &'static [u8]) -> Result<Self, Fail> {
        let _ = sys(SYS_UNLINK, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
        eq("mknodat fifo", sys(SYS_MKNODAT, [AT_FDCWD, path.as_ptr() as u64, S_IFIFO | 0o600, 0, 0, 0]), 0)?;
        let fd = ok("open fifo rw", open(path, RDWR | O_CLOEXEC))? as i32;
        Ok(Self { path, fd })
    }
}

impl Drop for Fifo {
    fn drop(&mut self) {
        let _ = close(self.fd);
        let _ = sys(SYS_UNLINK, [self.path.as_ptr() as u64, 0, 0, 0, 0, 0]);
    }
}

fn poll_wait(fd: i32, events: i16) -> Result<i16, Fail> {
    let mut p = [0u8; 8];
    p[..4].copy_from_slice(&fd.to_ne_bytes());
    p[4..6].copy_from_slice(&events.to_ne_bytes());
    loop {
        let r = sys(SYS_POLL, [p.as_mut_ptr() as u64, 1, (-1i64) as u64, 0, 0, 0]);
        if r == EINTR {
            continue;
        }
        ok("poll_wait", r)?;
        return Ok(i16::from_ne_bytes(p[6..].try_into().unwrap()));
    }
}

fn wait_retry(pid: i64) -> Result<i32, Fail> {
    let mut status = 0i32;
    loop {
        let r = sys(SYS_WAIT4, [pid as u64, &mut status as *mut i32 as u64, 0, 0, 0, 0]);
        if r == EINTR {
            continue;
        }
        eq("wait4 pid", r, pid)?;
        return Ok(status);
    }
}

fn read_exact_fd(fd: i32, dst: &mut [u8]) -> R {
    let mut off = 0;
    while off < dst.len() {
        let n = read(fd, &mut dst[off..]);
        if n > 0 {
            off += n as usize;
        } else if n == EINTR {
            continue;
        } else if n == EAGAIN {
            let _ = poll_wait(fd, POLLIN)?;
        } else {
            return Err(Fail { what: "read_exact_fd", got: n });
        }
    }
    Ok(())
}

fn write_all_fd(fd: i32, src: &[u8]) -> R {
    let mut off = 0;
    while off < src.len() {
        let n = sys(SYS_WRITE, [fd as u64, src[off..].as_ptr() as u64, (src.len() - off) as u64, 0, 0, 0]);
        if n > 0 {
            off += n as usize;
        } else if n == EINTR {
            continue;
        } else if n == EAGAIN {
            let _ = poll_wait(fd, POLLOUT)?;
        } else {
            return Err(Fail { what: "write_all_fd", got: n });
        }
    }
    Ok(())
}

fn read_all_eof(fd: i32, dst: &mut [u8]) -> Result<usize, Fail> {
    let mut len = 0;
    while len < dst.len() {
        let n = read(fd, &mut dst[len..]);
        if n > 0 {
            len += n as usize;
        } else if n == 0 {
            break;
        } else if n == EINTR {
            continue;
        } else if n == EAGAIN {
            let _ = poll_wait(fd, POLLIN)?;
        } else {
            return Err(Fail { what: "read_all_eof", got: n });
        }
    }
    Ok(len)
}

fn get_fl(fd: i32) -> Result<u64, Fail> {
    Ok(ok("fcntl F_GETFL", sys(SYS_FCNTL, [fd as u64, F_GETFL, 0, 0, 0, 0]))? as u64)
}

fn get_winsize(fd: i32) -> Result<(u16, u16), Fail> {
    let mut ws = [0u16; 4];
    eq("TIOCGWINSZ", ioctl(fd, TIOCGWINSZ, ws.as_mut_ptr() as u64), 0)?;
    Ok((ws[0], ws[1]))
}

fn set_winsize(fd: i32, rows: u16, cols: u16) -> R {
    let ws = [rows, cols, 0u16, 0u16];
    eq("TIOCSWINSZ", ioctl(fd, TIOCSWINSZ, ws.as_ptr() as u64), 0)
}

fn pattern_byte(i: usize) -> u8 {
    ((i.wrapping_mul(73).wrapping_add(19)) % 94 + 33) as u8
}

fn contains_bytes(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

fn dup2_fd(from: i32, to: i32) -> R {
    if from != to {
        eq("dup2", sys(SYS_DUP2, [from as u64, to as u64, 0, 0, 0, 0]), to as i64)?;
    }
    Ok(())
}

fn ignore_signal(sig: u64) -> R {
    let act = [SIG_IGN, 0, 0, 0];
    eq("rt_sigaction SIG_IGN", sys(SYS_RT_SIGACTION, [sig, act.as_ptr() as u64, 0, 8, 0, 0]), 0)
}

fn get_sig_handler(sig: u64) -> Result<u64, Fail> {
    let mut old = [0u64; 4];
    eq("rt_sigaction query", sys(SYS_RT_SIGACTION, [sig, 0, old.as_mut_ptr() as u64, 8, 0, 0]), 0)?;
    Ok(old[0])
}

/// Replace the current child process with `/usr/local/bin/ptyrun /usr/local/bin/m8_pty ptyrun-child ...`.
fn exec_ptyrun_child(mode: &'static [u8], arg1: Option<&'static [u8]>, arg2: Option<&'static [u8]>) -> ! {
    let mut argv: [*const u8; 7] = [core::ptr::null(); 7];
    argv[0] = b"/usr/local/bin/ptyrun\0".as_ptr();
    argv[1] = b"/usr/local/bin/m8_pty\0".as_ptr();
    argv[2] = b"ptyrun-child\0".as_ptr();
    argv[3] = mode.as_ptr();
    let mut idx = 4;
    if let Some(a) = arg1 {
        argv[idx] = a.as_ptr();
        idx += 1;
    }
    if let Some(a) = arg2 {
        argv[idx] = a.as_ptr();
        idx += 1;
    }
    argv[idx] = core::ptr::null();
    // SAFETY: `environ` is musl's NUL-terminated environment pointer array and
    // every non-null element of `argv` points to a static NUL-terminated string.
    let envp = unsafe { environ };
    let r = sys(SYS_EXECVE, [argv[0] as u64, argv.as_ptr() as u64, envp as u64, 0, 0, 0]);
    say!("FAIL exec ptyrun: got {r}");
    // SAFETY: terminates this child immediately without running parent destructors.
    unsafe { _exit(127) }
}

fn ptyrun_relay_two_way_backpressure() -> R {
    let outer = Pair::new(NOCTTY)?;
    let orig_termios = termios(outer.slave)?;
    let c2p = Fifo::new(b"/tmp/ptyrun-bp-c2p\0")?;
    let p2c = Fifo::new(b"/tmp/ptyrun-bp-p2c\0")?;

    let pid = fork()?;
    if pid == 0 {
        if let Err(f) = (|| -> R {
            dup2_fd(outer.slave, 0)?;
            dup2_fd(outer.slave, 1)?;
            let _ = close(outer.master);
            if outer.slave > 2 {
                let _ = close(outer.slave);
            }
            Ok(())
        })() {
            child_end(Err(f));
        }
        exec_ptyrun_child(b"backpressure\0", Some(c2p.path), Some(p2c.path));
    }

    // Wait for child startup (after ptyrun put outer.slave into raw + O_NONBLOCK).
    let mut tag = [0u8; 1];
    read_exact_fd(c2p.fd, &mut tag)?;
    eq("bp start tag", tag[0] as i64, b'S' as i64)?;
    let raw_t = termios(outer.slave)?;
    eq("bp outer raw lflag", u32::from_ne_bytes(raw_t[12..16].try_into().unwrap()) as i64, 0)?;
    ensure("bp outer O_NONBLOCK set", (get_fl(outer.slave)? & NONBLOCK) != 0, 0)?;

    // Fill outer's to_master ring completely (4096 bytes) so stdout is stalled.
    let fill = [b'F'; 4096];
    write_all_fd(outer.slave, &fill)?;
    eq("bp outer TIOCOUTQ full", getint(outer.slave, TIOCOUTQ)? as i64, 4096)?;

    // Release child to write 5120 bytes (4096 inner to_master + >=1024 in ptyrun to_stdout).
    write_all_fd(p2c.fd, b"G")?;
    read_exact_fd(c2p.fd, &mut tag)?;
    eq("bp write done tag", tag[0] as i64, b'W' as i64)?;
    eq("bp outer still undrained", getint(outer.slave, TIOCOUTQ)? as i64, 4096)?;

    // While stdout is completely undrained and ptyrun's to_stdout is non-empty,
    // send input through stdin and confirm the child receives it via side-channel.
    write_all_fd(outer.master, b"backpressure-ok\n")?;
    read_exact_fd(c2p.fd, &mut tag)?;
    eq("bp input received while stalled", tag[0] as i64, b'I' as i64)?;

    // Now drain outer.master and confirm all 4096 fill bytes + 5120 payload bytes arrived intact.
    let mut seen_fill = 0usize;
    let mut buf = [0u8; 512];
    while seen_fill < 4096 {
        let want = (4096 - seen_fill).min(buf.len());
        read_exact_fd(outer.master, &mut buf[..want])?;
        for &b in &buf[..want] {
            eq("bp fill byte", b as i64, b'F' as i64)?;
        }
        seen_fill += want;
    }
    let mut seen_payload = 0usize;
    while seen_payload < 5120 {
        let want = (5120 - seen_payload).min(buf.len());
        read_exact_fd(outer.master, &mut buf[..want])?;
        for (j, &b) in buf[..want].iter().enumerate() {
            eq("bp payload byte", b as i64, pattern_byte(seen_payload + j) as i64)?;
        }
        seen_payload += want;
    }

    eq("bp ptyrun exit", wait_retry(pid)? as i64, 0)?;
    ensure("bp termios restored", termios(outer.slave)? == orig_termios, 0)?;
    eq("bp O_NONBLOCK cleared", (get_fl(outer.slave)? & NONBLOCK) as i64, 0)
}

fn ptyrun_relay_partial_writes() -> R {
    let outer = Pair::new(NOCTTY)?;
    let orig_termios = termios(outer.slave)?;
    let c2p = Fifo::new(b"/tmp/ptyrun-pw-c2p\0")?;
    let p2c = Fifo::new(b"/tmp/ptyrun-pw-p2c\0")?;

    let pid = fork()?;
    if pid == 0 {
        if let Err(f) = (|| -> R {
            dup2_fd(outer.slave, 0)?;
            dup2_fd(outer.slave, 1)?;
            let _ = close(outer.master);
            if outer.slave > 2 {
                let _ = close(outer.slave);
            }
            Ok(())
        })() {
            child_end(Err(f));
        }
        exec_ptyrun_child(b"partial-rw\0", Some(c2p.path), Some(p2c.path));
    }

    let mut tag = [0u8; 1];
    read_exact_fd(c2p.fd, &mut tag)?;
    eq("pw start tag", tag[0] as i64, b'S' as i64)?;

    // Half A: leave only 137 bytes free in outer's to_master so ptyrun's first
    // write to stdout is forced to be a partial write (137 < 2048). Child writes
    // 5120 bytes (4096 inner to_master + >=1024 in ptyrun to_stdout), so its write
    // cannot complete until ptyrun has read 2048 bytes and partial-written 137
    // bytes into outer.slave (filling TIOCOUTQ to 4096). Only after confirming
    // TIOCOUTQ == 4096 does the parent drain in 193-byte odd chunks.
    const PREFILL: usize = 4096 - 137;
    const OUT_LEN: usize = 5120;
    let prefill = [b'P'; PREFILL];
    write_all_fd(outer.slave, &prefill)?;
    eq("pw prefill TIOCOUTQ", getint(outer.slave, TIOCOUTQ)? as i64, PREFILL as i64)?;
    write_all_fd(p2c.fd, b"1")?;
    read_exact_fd(c2p.fd, &mut tag)?;
    eq("pw half-a write done tag", tag[0] as i64, b'A' as i64)?;
    while getint(outer.slave, TIOCOUTQ)? < 4096 {
        let _ = sys(SYS_SCHED_YIELD, [0; 6]);
    }
    eq("pw outer TIOCOUTQ filled by partial write", getint(outer.slave, TIOCOUTQ)? as i64, 4096)?;

    let mut chunk = [0u8; 193];
    let mut got_prefill = 0usize;
    while got_prefill < PREFILL {
        let want = (PREFILL - got_prefill).min(chunk.len());
        read_exact_fd(outer.master, &mut chunk[..want])?;
        for &b in &chunk[..want] {
            eq("pw prefill byte", b as i64, b'P' as i64)?;
        }
        got_prefill += want;
    }
    let mut got_out = 0usize;
    while got_out < OUT_LEN {
        let want = (OUT_LEN - got_out).min(chunk.len());
        read_exact_fd(outer.master, &mut chunk[..want])?;
        for (j, &b) in chunk[..want].iter().enumerate() {
            eq("pw stdout partial byte", b as i64, pattern_byte(got_out + j) as i64)?;
        }
        got_out += want;
    }

    // Half B: push 8191 bytes (4095 inner + >=1 to_master + <=4095 outer) in
    // 2048-byte chunks before the child starts reading, forcing partial writes
    // into inner.master's 4095-byte raw input buffer, then release child to
    // confirm FIONREAD == 4095 and read in 137-byte chunks.
    const IN_LEN: usize = 8191;
    let mut send_buf = [0u8; 2048];
    let mut sent = 0usize;
    while sent < IN_LEN {
        let n = (IN_LEN - sent).min(send_buf.len());
        for (j, slot) in send_buf[..n].iter_mut().enumerate() {
            *slot = pattern_byte(sent + j + 1000);
        }
        write_all_fd(outer.master, &send_buf[..n])?;
        sent += n;
    }
    write_all_fd(p2c.fd, b"2")?;
    read_exact_fd(c2p.fd, &mut tag)?;
    eq("pw master partial ok", tag[0] as i64, b'K' as i64)?;

    eq("pw ptyrun exit", wait_retry(pid)? as i64, 0)?;
    ensure("pw termios restored", termios(outer.slave)? == orig_termios, 0)?;
    eq("pw O_NONBLOCK cleared", (get_fl(outer.slave)? & NONBLOCK) as i64, 0)
}

fn ptyrun_relay_stdout_write_error_cleanup() -> R {
    let outer = Pair::new(NOCTTY)?;
    let orig_termios = termios(outer.slave)?;
    let ro_fd = ok("open /dev/null rdonly", open(b"/dev/null\0", O_RDONLY))? as i32;
    eq("ro_fd initially blocking", (get_fl(ro_fd)? & NONBLOCK) as i64, 0)?;
    let err_pipe = pipe(0)?;

    let pid = fork()?;
    if pid == 0 {
        if let Err(f) = (|| -> R {
            dup2_fd(outer.slave, 0)?;
            dup2_fd(ro_fd, 1)?;
            dup2_fd(err_pipe[1], 2)?;
            let _ = close(outer.master);
            if outer.slave > 2 {
                let _ = close(outer.slave);
            }
            if ro_fd > 2 {
                let _ = close(ro_fd);
            }
            let _ = close(err_pipe[0]);
            if err_pipe[1] > 2 {
                let _ = close(err_pipe[1]);
            }
            Ok(())
        })() {
            child_end(Err(f));
        }
        exec_ptyrun_child(b"write-exit-0\0", None, None);
    }

    close(err_pipe[1])?;
    let mut err_buf = [0u8; 512];
    let err_len = read_all_eof(err_pipe[0], &mut err_buf)?;
    close(err_pipe[0])?;

    let status = wait_retry(pid)?;
    // Must exit non-zero (1) reporting the stdout write error, not child's 0.
    eq("stdout err exit status", status as i64, 1 << 8)?;
    ensure("stdout err stderr message", contains_bytes(&err_buf[..err_len], b"relay write stdout (got -9)"), 0)?;
    ensure("stdout err termios restored", termios(outer.slave)? == orig_termios, 0)?;
    eq("stdout err O_NONBLOCK cleared", (get_fl(ro_fd)? & NONBLOCK) as i64, 0)?;
    close(ro_fd)
}

fn ptyrun_relay_sigpipe_reaches_cleanup() -> R {
    let outer = Pair::new(NOCTTY)?;
    let orig_termios = termios(outer.slave)?;
    let out_pipe = pipe(0)?;
    let err_pipe = pipe(0)?;
    let c2p = Fifo::new(b"/tmp/ptyrun-sp-c2p\0")?;
    let p2c = Fifo::new(b"/tmp/ptyrun-sp-p2c\0")?;

    let pid = fork()?;
    if pid == 0 {
        if let Err(f) = (|| -> R {
            dup2_fd(outer.slave, 0)?;
            dup2_fd(out_pipe[1], 1)?;
            dup2_fd(err_pipe[1], 2)?;
            let _ = close(outer.master);
            if outer.slave > 2 {
                let _ = close(outer.slave);
            }
            let _ = close(out_pipe[0]);
            if out_pipe[1] > 2 {
                let _ = close(out_pipe[1]);
            }
            let _ = close(err_pipe[0]);
            if err_pipe[1] > 2 {
                let _ = close(err_pipe[1]);
            }
            Ok(())
        })() {
            child_end(Err(f));
        }
        exec_ptyrun_child(b"sigpipe-exit-0\0", Some(c2p.path), Some(p2c.path));
    }

    close(err_pipe[1])?;

    // Wait for child to confirm SIGPIPE/SIGWINCH are SIG_DFL and ptyrun entered raw + O_NONBLOCK.
    let mut tag = [0u8; 1];
    read_exact_fd(c2p.fd, &mut tag)?;
    eq("sigpipe child ready", tag[0] as i64, b'S' as i64)?;
    let raw_t = termios(outer.slave)?;
    eq("sigpipe outer raw lflag", u32::from_ne_bytes(raw_t[12..16].try_into().unwrap()) as i64, 0)?;
    ensure("sigpipe stdout O_NONBLOCK set", (get_fl(out_pipe[1])? & NONBLOCK) != 0, 0)?;

    // Close the sole reader end of stdout's pipe so ptyrun's write triggers SIGPIPE + EPIPE.
    close(out_pipe[0])?;
    write_all_fd(p2c.fd, b"G")?;

    let mut err_buf = [0u8; 512];
    let err_len = read_all_eof(err_pipe[0], &mut err_buf)?;
    close(err_pipe[0])?;

    let status = wait_retry(pid)?;
    // Must reach cleanup and exit 1, never killed by SIGPIPE (status 13) or returning child's 0.
    eq("sigpipe exit status", status as i64, 1 << 8)?;
    ensure("sigpipe stderr message", contains_bytes(&err_buf[..err_len], b"relay write stdout (got -32)"), 0)?;
    ensure("sigpipe termios restored", termios(outer.slave)? == orig_termios, 0)?;
    eq("sigpipe O_NONBLOCK cleared", (get_fl(out_pipe[1])? & NONBLOCK) as i64, 0)?;
    close(out_pipe[1])
}

fn ptyrun_relay_sigwinch_forwards_resize() -> R {
    let outer = Pair::new(NOCTTY)?;
    set_winsize(outer.master, 25, 85)?;
    let orig_termios = termios(outer.slave)?;
    let c2p = Fifo::new(b"/tmp/ptyrun-ws-c2p\0")?;

    let pid = fork()?;
    if pid == 0 {
        if let Err(f) = (|| -> R {
            ok("ws setsid", sys(112, [0; 6]))?;
            let _ = close(outer.master);
            let _ = close(outer.slave);
            let s = ok("ws open outer slave", open(&outer.path, RDWR))? as i32;
            eq("ws TIOCSCTTY", ioctl(s, SETCTTY, 0), 0)?;
            dup2_fd(s, 0)?;
            dup2_fd(s, 1)?;
            if s > 2 {
                let _ = close(s);
            }
            Ok(())
        })() {
            child_end(Err(f));
        }
        exec_ptyrun_child(b"winsize\0", Some(c2p.path), None);
    }

    let mut tag = [0u8; 1];
    // Child confirmed initial 25x85 size and blocked SIGWINCH.
    read_exact_fd(c2p.fd, &mut tag)?;
    eq("ws initial size tag", tag[0] as i64, b'1' as i64)?;

    // First resize: 40x120.
    set_winsize(outer.master, 40, 120)?;
    read_exact_fd(c2p.fd, &mut tag)?;
    eq("ws first resize tag", tag[0] as i64, b'2' as i64)?;

    // Second resize: 50x132 (confirms handler is not one-shot and pipe drained).
    set_winsize(outer.master, 50, 132)?;
    read_exact_fd(c2p.fd, &mut tag)?;
    eq("ws second resize tag", tag[0] as i64, b'3' as i64)?;

    eq("ws ptyrun exit", wait_retry(pid)? as i64, 0)?;
    ensure("ws termios restored", termios(outer.slave)? == orig_termios, 0)
}

fn ptyrun_relay_exit_status_and_eof() -> R {
    // 1) Non-zero child exit code forwarded verbatim.
    {
        let outer = Pair::new(NOCTTY)?;
        let orig_termios = termios(outer.slave)?;
        let pid = fork()?;
        if pid == 0 {
            if let Err(f) = (|| -> R {
                dup2_fd(outer.slave, 0)?;
                dup2_fd(outer.slave, 1)?;
                let _ = close(outer.master);
                if outer.slave > 2 {
                    let _ = close(outer.slave);
                }
                Ok(())
            })() {
                child_end(Err(f));
            }
            exec_ptyrun_child(b"exit-42\0", None, None);
        }
        eq("relay forwards exit 42", wait_retry(pid)? as i64, 42 << 8)?;
        ensure("exit-42 termios restored", termios(outer.slave)? == orig_termios, 0)?;
    }

    // 2) Signal-terminated child reported as 128 + sig.
    {
        let outer = Pair::new(NOCTTY)?;
        let orig_termios = termios(outer.slave)?;
        let pid = fork()?;
        if pid == 0 {
            if let Err(f) = (|| -> R {
                dup2_fd(outer.slave, 0)?;
                dup2_fd(outer.slave, 1)?;
                let _ = close(outer.master);
                if outer.slave > 2 {
                    let _ = close(outer.slave);
                }
                Ok(())
            })() {
                child_end(Err(f));
            }
            exec_ptyrun_child(b"self-usr1\0", None, None);
        }
        eq("relay forwards 128+SIGUSR1", wait_retry(pid)? as i64, ((128 + SIGUSR1) as i64) << 8)?;
        ensure("self-usr1 termios restored", termios(outer.slave)? == orig_termios, 0)?;
    }

    // 3) Piped stdin EOF without trailing newline sends double VEOF so canonical child finishes.
    {
        let in_pipe = pipe(0)?;
        let out_pipe = pipe(0)?;
        let pid = fork()?;
        if pid == 0 {
            if let Err(f) = (|| -> R {
                dup2_fd(in_pipe[0], 0)?;
                dup2_fd(out_pipe[1], 1)?;
                let _ = close(in_pipe[1]);
                if in_pipe[0] > 2 {
                    let _ = close(in_pipe[0]);
                }
                let _ = close(out_pipe[0]);
                if out_pipe[1] > 2 {
                    let _ = close(out_pipe[1]);
                }
                Ok(())
            })() {
                child_end(Err(f));
            }
            exec_ptyrun_child(b"read-eof\0", None, None);
        }
        close(in_pipe[0])?;
        close(out_pipe[1])?;
        write_all_fd(in_pipe[1], b"no-newline")?;
        close(in_pipe[1])?;
        let mut out_buf = [0u8; 256];
        let out_len = read_all_eof(out_pipe[0], &mut out_buf)?;
        close(out_pipe[0])?;
        eq("eof ptyrun exit", wait_retry(pid)? as i64, 0)?;
        ensure("eof child output", contains_bytes(&out_buf[..out_len], b"EOF:no-newline"), out_len as i64)?;
    }

    Ok(())
}

fn open_cstr(arg: &[u8], flags: u64) -> Result<i32, Fail> {
    let mut path = [0u8; 128];
    if arg.len() >= path.len() {
        return Err(Fail { what: "fifo path too long", got: arg.len() as i64 });
    }
    path[..arg.len()].copy_from_slice(arg);
    path[arg.len()] = 0;
    Ok(ok("open fifo path", open(&path, flags))? as i32)
}

fn run_ptyrun_child(argv: &[&[u8]]) -> c_int {
    let mode = argv[2];
    let res: R = match mode {
        b"backpressure" => (|| -> R {
            let c2p = open_cstr(argv[3], O_WRONLY)?;
            let p2c = open_cstr(argv[4], O_RDONLY)?;
            raw(0, 1, 0)?;
            write_all_fd(c2p, b"S")?;
            let mut go = [0u8; 1];
            read_exact_fd(p2c, &mut go)?;
            eq("bp child go", go[0] as i64, b'G' as i64)?;
            let mut out = [0u8; 256];
            let mut written = 0usize;
            while written < 5120 {
                let n = (5120 - written).min(out.len());
                for (j, slot) in out[..n].iter_mut().enumerate() {
                    *slot = pattern_byte(written + j);
                }
                write_all_fd(1, &out[..n])?;
                written += n;
            }
            write_all_fd(c2p, b"W")?;
            let mut got = [0u8; 16];
            read_exact_fd(0, &mut got)?;
            ensure("bp child got input", &got == b"backpressure-ok\n", got[0] as i64)?;
            write_all_fd(c2p, b"I")?;
            let _ = close(c2p);
            let _ = close(p2c);
            Ok(())
        })(),
        b"partial-rw" => (|| -> R {
            let c2p = open_cstr(argv[3], O_WRONLY)?;
            let p2c = open_cstr(argv[4], O_RDONLY)?;
            raw(0, 1, 0)?;
            write_all_fd(c2p, b"S")?;
            let mut step = [0u8; 1];
            read_exact_fd(p2c, &mut step)?;
            eq("pw step 1", step[0] as i64, b'1' as i64)?;
            let mut out = [0u8; 2048];
            let mut written = 0usize;
            while written < 5120 {
                let n = (5120 - written).min(out.len());
                for (j, slot) in out[..n].iter_mut().enumerate() {
                    *slot = pattern_byte(written + j);
                }
                write_all_fd(1, &out[..n])?;
                written += n;
            }
            write_all_fd(c2p, b"A")?;
            read_exact_fd(p2c, &mut step)?;
            eq("pw step 2", step[0] as i64, b'2' as i64)?;
            while getint(0, FIONREAD)? < 4095 {
                let _ = sys(SYS_SCHED_YIELD, [0; 6]);
            }
            let mut chunk = [0u8; 137];
            let mut got = 0usize;
            while got < 8191 {
                let want = (8191 - got).min(chunk.len());
                read_exact_fd(0, &mut chunk[..want])?;
                for (j, &b) in chunk[..want].iter().enumerate() {
                    eq("pw child input byte", b as i64, pattern_byte(got + j + 1000) as i64)?;
                }
                got += want;
            }
            write_all_fd(c2p, b"K")?;
            let _ = close(c2p);
            let _ = close(p2c);
            Ok(())
        })(),
        b"write-exit-0" => (|| -> R {
            ignore_signal(SIGHUP)?;
            write_all_fd(1, b"stdout-ebadf-trigger\n")
        })(),
        b"sigpipe-exit-0" => (|| -> R {
            let c2p = open_cstr(argv[3], O_WRONLY)?;
            let p2c = open_cstr(argv[4], O_RDONLY)?;
            eq("child SIGPIPE is SIG_DFL", get_sig_handler(SIGPIPE)? as i64, SIG_DFL as i64)?;
            eq("child SIGWINCH is SIG_DFL", get_sig_handler(SIGWINCH)? as i64, SIG_DFL as i64)?;
            ignore_signal(SIGHUP)?;
            write_all_fd(c2p, b"S")?;
            let mut go = [0u8; 1];
            read_exact_fd(p2c, &mut go)?;
            eq("sp child go", go[0] as i64, b'G' as i64)?;
            write_all_fd(1, b"stdout-epipe-trigger\n")?;
            let _ = close(c2p);
            let _ = close(p2c);
            Ok(())
        })(),
        b"winsize" => (|| -> R {
            let c2p = open_cstr(argv[3], O_WRONLY)?;
            block(&[SIGWINCH as u32])?;
            let (r, c) = get_winsize(0)?;
            eq("ws init rows", r as i64, 25)?;
            eq("ws init cols", c as i64, 85)?;
            write_all_fd(c2p, b"1")?;
            await_signal(SIGWINCH as u32)?;
            let (r, c) = get_winsize(0)?;
            eq("ws first rows", r as i64, 40)?;
            eq("ws first cols", c as i64, 120)?;
            write_all_fd(c2p, b"2")?;
            await_signal(SIGWINCH as u32)?;
            let (r, c) = get_winsize(0)?;
            eq("ws second rows", r as i64, 50)?;
            eq("ws second cols", c as i64, 132)?;
            write_all_fd(c2p, b"3")?;
            let _ = close(c2p);
            Ok(())
        })(),
        b"exit-42" => return 42,
        b"self-usr1" => {
            let _ = kill(sys(SYS_GETPID, [0; 6]), SIGUSR1);
            return 99;
        }
        b"read-eof" => (|| -> R {
            let mut buf = [0u8; 64];
            let len = read_all_eof(0, &mut buf)?;
            let text = core::str::from_utf8(&buf[..len]).unwrap_or("");
            say!("EOF:{text}");
            Ok(())
        })(),
        b"console-echo" => (|| -> R {
            say!("RELAY_CONSOLE_READY");
            let mut buf = [0u8; 64];
            let mut len = 0usize;
            while len < buf.len() {
                let n = read(0, &mut buf[len..len + 1]);
                if n == EINTR {
                    continue;
                }
                if n <= 0 {
                    break;
                }
                if buf[len] == b'\n' || buf[len] == b'\r' {
                    break;
                }
                len += 1;
            }
            let text = core::str::from_utf8(&buf[..len]).unwrap_or("");
            say!("RELAY_GOT:{text}");
            Ok(())
        })(),
        _ => return USAGE_EXIT,
    };
    match res {
        Ok(()) => {
            if mode == b"console-echo" {
                7
            } else {
                0
            }
        }
        Err(f) => {
            say!("FAIL ptyrun-child: {} (got {})", f.what, f.got);
            1
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const u8) -> c_int {
    // SAFETY: musl passes the kernel's `argc` valid NUL-terminated C strings.
    let (args, n) = unsafe { common::args(argc, argv) };
    if n <= 1 {
        return run_checks(&[
            ("pty_data_and_timers", &|| isolated(data_and_timers)),
            ("pty_job_control_and_hangup", &|| isolated(job_control)),
            ("pty_orphans_and_leader_exit", &|| isolated(orphan_and_leader_exit)),
        ]);
    }
    match args[1] {
        b"ptyrun-relay" => run_checks(&[
            ("ptyrun_relay_two_way_backpressure", &|| isolated(ptyrun_relay_two_way_backpressure)),
            ("ptyrun_relay_partial_writes", &|| isolated(ptyrun_relay_partial_writes)),
            ("ptyrun_relay_stdout_write_error_cleanup", &|| isolated(ptyrun_relay_stdout_write_error_cleanup)),
            ("ptyrun_relay_sigpipe_reaches_cleanup", &|| isolated(ptyrun_relay_sigpipe_reaches_cleanup)),
            ("ptyrun_relay_sigwinch_forwards_resize", &|| isolated(ptyrun_relay_sigwinch_forwards_resize)),
            ("ptyrun_relay_exit_status_and_eof", &|| isolated(ptyrun_relay_exit_status_and_eof)),
        ]),
        b"ptyrun-child" if n >= 3 => run_ptyrun_child(&args[..n]),
        _ => {
            say!("usage: m8_pty [ptyrun-relay | ptyrun-child <mode> ...]");
            USAGE_EXIT
        }
    }
}
