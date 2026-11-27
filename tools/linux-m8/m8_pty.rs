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
use core::fmt;

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
const POLLERR: i16 = 0x0008;
const POLLHUP: i16 = 0x0010;
const WNOHANG: u64 = 1;
const ECHILD: i64 = -10;
const SIGHUP: u64 = 1;
const SIGKILL: u64 = 9;
const SIGUSR1: u64 = 10;
const SIGPIPE: u64 = 13;
const SIGWINCH: u64 = 28;
const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;
// High fixed descriptors that reach the grandchild through ptyrun passing on
// non-CLOEXEC fds (> 2) across fork/execve without colliding with driver fds
// or ptyrun's own master/err/winch descriptors.
const P2C_FD: i32 = 40;
const DIAG_FD: i32 = 41;
const ALIVE_FD: i32 = 42;
const GIVE_UP_NS: u64 = 20_000_000_000;

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
    loop {
        let r = sys(SYS_WAIT4, [pid as u64, &mut status as *mut i32 as u64, options, 0, 0, 0]);
        if r == EINTR {
            continue;
        }
        eq("wait4 pid", r, pid)?;
        return Ok(status);
    }
}
fn try_wait(pid: i64) -> Result<Option<i32>, Fail> {
    let mut status = 0;
    loop {
        let r = sys(SYS_WAIT4, [pid as u64, &mut status as *mut i32 as u64, WNOHANG, 0, 0, 0]);
        if r == EINTR {
            continue;
        }
        if r == 0 {
            return Ok(None);
        }
        eq("wait4 WNOHANG", r, pid)?;
        return Ok(Some(status));
    }
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

// Relay backpressure and partial-write checks rely on Lumen's 4096-byte PTY
// output queue (`tty.rs`'s `BUFFER`, `POLLOUT` while `len < 4096`), 4095-byte
// raw input buffer (`lumen_tty::BUFFER_SIZE - 1`, `POLLOUT` while `ld.accepts`
// a byte), `ptyrun`'s 2048-byte `PendingBuf`, and atomic commit of a 2048-byte
// slave write into an empty output queue under one tty lock hold.

/// Read-only non-blocking FIFO endpoint held by the test driver. Because the
/// driver holds no write end and `ptyrun` never opens the FIFO, the sole writer
/// is `m8_pty ptyrun-child`, so any early child exit surfaces immediately as
/// `POLLHUP`/EOF even when the relayed PTY is stalled.
struct FifoReader {
    path: &'static [u8],
    fd: i32,
}

impl FifoReader {
    fn new(path: &'static [u8]) -> Result<Self, Fail> {
        let _ = sys(SYS_UNLINK, [path.as_ptr() as u64, 0, 0, 0, 0, 0]);
        eq("mknodat fifo", sys(SYS_MKNODAT, [AT_FDCWD, path.as_ptr() as u64, S_IFIFO | 0o600, 0, 0, 0]), 0)?;
        let fd = ok("open fifo ro", open(path, O_RDONLY | NONBLOCK | O_CLOEXEC))? as i32;
        Ok(Self { path, fd })
    }
}

impl Drop for FifoReader {
    fn drop(&mut self) {
        let _ = close(self.fd);
        let _ = sys(SYS_UNLINK, [self.path.as_ptr() as u64, 0, 0, 0, 0, 0]);
    }
}

fn remaining_ms(deadline: u64) -> Result<Option<i32>, Fail> {
    let t = now()?;
    if t >= deadline {
        return Ok(None);
    }
    let ms = (deadline - t).div_ceil(1_000_000).min(i32::MAX as u64) as i32;
    Ok(Some(ms.max(1)))
}

fn poll_wait_until(fd: i32, events: i16, deadline: u64) -> Result<Option<i16>, Fail> {
    let mut p = [0u8; 8];
    p[..4].copy_from_slice(&fd.to_ne_bytes());
    p[4..6].copy_from_slice(&events.to_ne_bytes());
    loop {
        let Some(ms) = remaining_ms(deadline)? else { return Ok(None) };
        p[6..8].fill(0);
        let r = sys(SYS_POLL, [p.as_mut_ptr() as u64, 1, ms as i64 as u64, 0, 0, 0]);
        if r == EINTR {
            continue;
        }
        let ready = ok("poll_wait_until", r)?;
        if ready == 0 {
            continue;
        }
        return Ok(Some(i16::from_ne_bytes(p[6..].try_into().unwrap())));
    }
}

fn poll_two_until(fd0: i32, ev0: i16, fd1: i32, ev1: i16, deadline: u64) -> Result<Option<(i16, i16)>, Fail> {
    let mut p = [0u8; 16];
    p[0..4].copy_from_slice(&fd0.to_ne_bytes());
    p[4..6].copy_from_slice(&ev0.to_ne_bytes());
    p[8..12].copy_from_slice(&fd1.to_ne_bytes());
    p[12..14].copy_from_slice(&ev1.to_ne_bytes());
    loop {
        let Some(ms) = remaining_ms(deadline)? else { return Ok(None) };
        p[6..8].fill(0);
        p[14..16].fill(0);
        let r = sys(SYS_POLL, [p.as_mut_ptr() as u64, 2, ms as i64 as u64, 0, 0, 0]);
        if r == EINTR {
            continue;
        }
        let ready = ok("poll_two_until", r)?;
        if ready == 0 {
            continue;
        }
        return Ok(Some((
            i16::from_ne_bytes(p[6..8].try_into().unwrap()),
            i16::from_ne_bytes(p[14..16].try_into().unwrap()),
        )));
    }
}

fn read_exact_until(step: &'static str, fd: i32, dst: &mut [u8], deadline: u64) -> R {
    let mut off = 0;
    while off < dst.len() {
        if poll_wait_until(fd, POLLIN, deadline)?.is_none() {
            say!("FAIL timeout ({step}): read {off}/{} bytes", dst.len());
            return Err(Fail { what: step, got: off as i64 });
        }
        let n = read(fd, &mut dst[off..]);
        if n > 0 {
            off += n as usize;
        } else if n == EINTR || n == EAGAIN {
            continue;
        } else {
            return Err(Fail { what: step, got: n });
        }
    }
    Ok(())
}

fn write_all_until(step: &'static str, fd: i32, src: &[u8], deadline: u64) -> R {
    let mut off = 0;
    while off < src.len() {
        let n = sys(SYS_WRITE, [fd as u64, src[off..].as_ptr() as u64, (src.len() - off) as u64, 0, 0, 0]);
        if n > 0 {
            off += n as usize;
        } else if n == EINTR {
            continue;
        } else if n == EAGAIN {
            if poll_wait_until(fd, POLLOUT, deadline)?.is_none() {
                say!("FAIL timeout ({step}): wrote {off}/{} bytes", src.len());
                return Err(Fail { what: step, got: off as i64 });
            }
        } else {
            return Err(Fail { what: step, got: n });
        }
    }
    Ok(())
}

fn write_all_fd(fd: i32, src: &[u8]) -> R {
    write_all_until("write_all_fd", fd, src, now()? + GIVE_UP_NS)
}

fn read_all_eof_until(step: &'static str, fd: i32, dst: &mut [u8], deadline: u64) -> Result<usize, Fail> {
    let mut len = 0;
    while len < dst.len() {
        if poll_wait_until(fd, POLLIN, deadline)?.is_none() {
            say!("FAIL timeout ({step}): read {len} bytes before EOF");
            return Err(Fail { what: step, got: len as i64 });
        }
        let n = read(fd, &mut dst[len..]);
        if n > 0 {
            len += n as usize;
        } else if n == 0 {
            break;
        } else if n == EINTR || n == EAGAIN {
            continue;
        } else {
            return Err(Fail { what: step, got: n });
        }
    }
    Ok(len)
}

fn drain_outer_tail(master: i32, tail: &mut [u8; 160], tail_len: &mut usize) {
    let mut buf = [0u8; 256];
    loop {
        let n = read(master, &mut buf);
        if n > 0 {
            let n = n as usize;
            let cap = tail.len();
            if n >= cap {
                tail.copy_from_slice(&buf[n - cap..n]);
                *tail_len = cap;
            } else {
                let drop = (*tail_len + n).saturating_sub(cap);
                if drop > 0 {
                    tail.copy_within(drop..*tail_len, 0);
                    *tail_len -= drop;
                }
                tail[*tail_len..*tail_len + n].copy_from_slice(&buf[..n]);
                *tail_len += n;
            }
        } else if n == EINTR {
            continue;
        } else {
            break;
        }
    }
}

fn format_escaped(src: &[u8], out: &mut [u8; 320]) -> usize {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut at = 0;
    for &b in src {
        if (0x20..=0x7e).contains(&b) {
            if at + 1 > out.len() {
                break;
            }
            out[at] = b;
            at += 1;
        } else {
            if at + 4 > out.len() {
                break;
            }
            out[at] = b'\\';
            out[at + 1] = b'x';
            out[at + 2] = HEX[(b >> 4) as usize];
            out[at + 3] = HEX[(b & 0xf) as usize];
            at += 4;
        }
    }
    at
}

fn kill_and_reap(pid: i64) -> i32 {
    let _ = sys(62, [pid as u64, SIGKILL, 0, 0, 0, 0]);
    let mut status = 0i32;
    loop {
        let r = sys(SYS_WAIT4, [pid as u64, &mut status as *mut i32 as u64, 0, 0, 0, 0]);
        if r == EINTR {
            continue;
        }
        if r == pid {
            return status;
        }
        // ECHILD or other error means already reaped.
        return if r == ECHILD { -1 } else { r as i32 };
    }
}

/// Report an early child or `ptyrun` exit without blocking on a stalled
/// `ptyrun` before printing the diagnosis, and without double-reaping if the
/// caller already collected `reaped`.
fn fail_early_exit(what: &'static str, pid: i64, reaped: Option<i32>, outer_master: Option<i32>) -> R {
    let mut tail = [0u8; 160];
    let mut tail_len = 0usize;
    if let Some(m) = outer_master {
        drain_outer_tail(m, &mut tail, &mut tail_len);
    }
    let mut esc = [0u8; 320];
    let esc_len = format_escaped(&tail[..tail_len], &mut esc);
    let text = core::str::from_utf8(&esc[..esc_len]).unwrap_or("");
    let status = match reaped {
        Some(st) => st,
        None => {
            // Print what we saw before touching ptyrun, then try non-blocking
            // reap; if ptyrun is still alive (e.g. stalled on a full outer PTY
            // after the child died), SIGKILL it so cleanup never hangs.
            say!("FAIL detail ({what}): outer=\"{text}\"");
            match try_wait(pid) {
                Ok(Some(st)) => st,
                Ok(None) => kill_and_reap(pid),
                Err(Fail { got: ECHILD, .. }) => -1,
                Err(_) => kill_and_reap(pid),
            }
        }
    };
    say!("FAIL detail ({what}): ptyrun wait status={status} outer=\"{text}\"");
    Err(Fail { what, got: status as i64 })
}

/// Read exact bytes from `c2p_fd` while watching for early child exit (`c2p_fd`
/// EOF/HUP), early `ptyrun` exit (`alive_fd` HUP; the child closes `ALIVE_FD`
/// immediately so only `ptyrun` holds its write end), or a 20 s give-up deadline.
fn recv_child(
    step: &'static str,
    c2p_fd: i32,
    alive_fd: i32,
    pid: i64,
    outer_master: Option<i32>,
    dst: &mut [u8],
) -> R {
    let deadline = now()? + GIVE_UP_NS;
    let mut off = 0;
    while off < dst.len() {
        let Some((r0, r1)) = poll_two_until(c2p_fd, POLLIN, alive_fd, POLLIN, deadline)? else {
            say!("FAIL timeout ({step}): read {off}/{} bytes on c2p", dst.len());
            return fail_early_exit(step, pid, None, outer_master);
        };
        if (r0 & POLLIN) != 0 {
            let n = read(c2p_fd, &mut dst[off..]);
            if n > 0 {
                off += n as usize;
                continue;
            }
            if n == EINTR || n == EAGAIN {
                continue;
            }
            if n == 0 {
                return fail_early_exit(step, pid, None, outer_master);
            }
            return Err(Fail { what: "read c2p", got: n });
        }
        if (r0 & (POLLHUP | POLLERR)) != 0 || (r1 & (POLLHUP | POLLERR)) != 0 {
            return fail_early_exit(step, pid, None, outer_master);
        }
    }
    Ok(())
}

fn check_below_fixed(fds: &[i32]) -> R {
    for &fd in fds {
        ensure("fd below fixed relay fds", fd < P2C_FD, fd as i64)?;
    }
    Ok(())
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

fn set_sig_handler(sig: u64, handler: u64) -> R {
    let act = [handler, 0, 0, 0];
    eq("rt_sigaction", sys(SYS_RT_SIGACTION, [sig, act.as_ptr() as u64, 0, 8, 0, 0]), 0)
}

fn get_sig_handler(sig: u64) -> Result<u64, Fail> {
    let mut old = [0u64; 4];
    eq("rt_sigaction query", sys(SYS_RT_SIGACTION, [sig, 0, old.as_mut_ptr() as u64, 8, 0, 0]), 0)?;
    Ok(old[0])
}

/// Fixed-buffer writer targeting `DIAG_FD` (or fd 2 if `DIAG_FD` is not open)
/// so child failure diagnostics go directly to serial stderr, never into the
/// relayed PTY stream.
struct DiagLine {
    buf: [u8; 256],
    len: usize,
}

impl DiagLine {
    fn new() -> Self {
        Self { buf: [0; 256], len: 0 }
    }

    fn send(&mut self) {
        if self.len == self.buf.len() {
            self.buf[self.len - 1] = b'\n';
        }
        let target = if sys(SYS_FCNTL, [DIAG_FD as u64, F_GETFL, 0, 0, 0, 0]) >= 0 { DIAG_FD } else { 2 };
        let mut off = 0;
        while off < self.len {
            let n = sys(SYS_WRITE, [target as u64, self.buf[off..].as_ptr() as u64, (self.len - off) as u64, 0, 0, 0]);
            if n > 0 {
                off += n as usize;
            } else if n != EINTR {
                break;
            }
        }
        self.len = 0;
    }
}

impl fmt::Write for DiagLine {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.buf.len() - self.len;
        let n = s.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

fn child_status(res: R, ok_code: c_int) -> c_int {
    match res {
        Ok(()) => ok_code,
        Err(f) => {
            use core::fmt::Write as _;
            let mut line = DiagLine::new();
            let _ = writeln!(line, "FAIL ptyrun-child: {} (got {})", f.what, f.got);
            line.send();
            1
        }
    }
}

/// Replace the current child process with `/usr/local/bin/ptyrun /usr/local/bin/m8_pty ptyrun-child ...`.
fn exec_ptyrun_child(mode: &'static [u8], c2p_path: Option<&'static [u8]>) -> ! {
    let mut argv: [*const u8; 6] = [core::ptr::null(); 6];
    argv[0] = b"/usr/local/bin/ptyrun\0".as_ptr();
    argv[1] = b"/usr/local/bin/m8_pty\0".as_ptr();
    argv[2] = b"ptyrun-child\0".as_ptr();
    argv[3] = mode.as_ptr();
    if let Some(path) = c2p_path {
        argv[4] = path.as_ptr();
    }
    // SAFETY: `environ` is musl's NUL-terminated environment pointer array and
    // every non-null element of `argv` points to a static NUL-terminated string.
    let envp = unsafe { environ };
    let r = sys(SYS_EXECVE, [argv[0] as u64, argv.as_ptr() as u64, envp as u64, 0, 0, 0]);
    let _ = child_status(Err(Fail { what: "exec ptyrun", got: r }), 0);
    // SAFETY: terminates this child immediately without running parent destructors.
    unsafe { _exit(127) }
}

/// Fork and exec `/usr/local/bin/ptyrun` wrapping `m8_pty ptyrun-child <mode>`.
/// Validates that every caller fd is `< P2C_FD`; in the child, saves serial
/// stderr to `DIAG_FD`, resets `SIGPIPE` to `SIG_DFL`, wires `stdin_fd`/`stdout_fd`/
/// optional `stderr_fd`, optional `p2c[0]` on `P2C_FD`, and optional `alive[1]`
/// on `ALIVE_FD`, then closes `close_in_child` plus `p2c[0]`/`p2c[1]` and
/// `alive[0]`/`alive[1]`. In the parent, closes `p2c[0]` and `alive[1]`.
#[allow(clippy::too_many_arguments)]
fn spawn_ptyrun_child(
    stdin_fd: i32,
    stdout_fd: i32,
    stderr_fd: Option<i32>,
    close_in_child: &[i32],
    mode: &'static [u8],
    c2p_path: Option<&'static [u8]>,
    p2c: Option<[i32; 2]>,
    alive: Option<[i32; 2]>,
) -> Result<i64, Fail> {
    check_below_fixed(&[stdin_fd, stdout_fd])?;
    if let Some(fd) = stderr_fd {
        check_below_fixed(&[fd])?;
    }
    check_below_fixed(close_in_child)?;
    if let Some(p) = p2c {
        check_below_fixed(&p)?;
    }
    if let Some(a) = alive {
        check_below_fixed(&a)?;
    }
    let pid = fork()?;
    if pid == 0 {
        if let Err(f) = (|| -> R {
            dup2_fd(2, DIAG_FD)?;
            set_sig_handler(SIGPIPE, SIG_DFL)?;
            if let Some(p) = p2c {
                dup2_fd(p[0], P2C_FD)?;
            }
            if let Some(a) = alive {
                dup2_fd(a[1], ALIVE_FD)?;
            }
            dup2_fd(stdin_fd, 0)?;
            dup2_fd(stdout_fd, 1)?;
            if let Some(fd) = stderr_fd {
                dup2_fd(fd, 2)?;
            }
            for &fd in close_in_child {
                if fd > 2 && fd != DIAG_FD && fd != P2C_FD && fd != ALIVE_FD {
                    let _ = close(fd);
                }
            }
            if let Some(p) = p2c {
                if p[0] != P2C_FD {
                    let _ = close(p[0]);
                }
                let _ = close(p[1]);
            }
            if let Some(a) = alive {
                let _ = close(a[0]);
                if a[1] != ALIVE_FD {
                    let _ = close(a[1]);
                }
            }
            Ok(())
        })() {
            child_end(Err(f));
        }
        exec_ptyrun_child(mode, c2p_path);
    }
    if let Some(p) = p2c {
        let _ = close(p[0]);
    }
    if let Some(a) = alive {
        let _ = close(a[1]);
    }
    Ok(pid)
}

fn ptyrun_relay_two_way_backpressure() -> R {
    let outer = Pair::new(NOCTTY)?;
    let orig_termios = termios(outer.slave)?;
    let c2p = FifoReader::new(b"/tmp/ptyrun-bp-c2p\0")?;
    let p2c = pipe(0)?;
    let alive = pipe(0)?;

    let pid = spawn_ptyrun_child(
        outer.slave,
        outer.slave,
        None,
        &[outer.master, outer.slave],
        b"backpressure\0",
        Some(c2p.path),
        Some(p2c),
        Some(alive),
    )?;

    // Wait for child startup (after ptyrun put outer.slave into raw + O_NONBLOCK).
    let mut tag = [0u8; 1];
    recv_child("bp start", c2p.fd, alive[0], pid, Some(outer.master), &mut tag)?;
    eq("bp start tag", tag[0] as i64, b'S' as i64)?;
    let raw_t = termios(outer.slave)?;
    eq("bp outer raw lflag", u32::from_ne_bytes(raw_t[12..16].try_into().unwrap()) as i64, 0)?;
    ensure("bp outer O_NONBLOCK set", (get_fl(outer.slave)? & NONBLOCK) != 0, 0)?;

    // Fill the outer slave's output queue (TIOCOUTQ == 4096) so stdout is stalled.
    let fill = [b'F'; 4096];
    write_all_fd(outer.slave, &fill)?;
    eq("bp outer TIOCOUTQ full", getint(outer.slave, TIOCOUTQ)? as i64, 4096)?;

    // Release child to write 5120 bytes blocking in 2048-byte chunks (each first
    // chunk commits atomically so ptyrun's one read takes 2048 into PendingBuf)
    // and then non-blocking until EAGAIN, reporting the total accepted bytes.
    write_all_fd(p2c[1], b"G")?;
    let mut report = [0u8; 5];
    recv_child("bp write report", c2p.fd, alive[0], pid, Some(outer.master), &mut report)?;
    eq("bp write done tag", report[0] as i64, b'W' as i64)?;
    let accepted = u32::from_le_bytes([report[1], report[2], report[3], report[4]]) as usize;
    say!("bp accepted={accepted}");
    eq("bp accepted == 4096+2048", accepted as i64, 4096 + 2048)?;
    eq("bp outer still undrained", getint(outer.slave, TIOCOUTQ)? as i64, 4096)?;

    // While stdout is completely undrained and ptyrun's to_stdout is non-empty,
    // send input through stdin and confirm the child receives it via c2p.
    write_all_fd(outer.master, b"backpressure-ok\n")?;
    recv_child("bp input ack", c2p.fd, alive[0], pid, Some(outer.master), &mut tag)?;
    eq("bp input received while stalled", tag[0] as i64, b'I' as i64)?;

    // Now drain outer.master and confirm all 4096 fill bytes + accepted payload bytes arrived intact.
    let drain_dl = now()? + GIVE_UP_NS;
    let mut seen_fill = 0usize;
    let mut buf = [0u8; 512];
    while seen_fill < 4096 {
        let want = (4096 - seen_fill).min(buf.len());
        read_exact_until("bp drain fill", outer.master, &mut buf[..want], drain_dl)?;
        for &b in &buf[..want] {
            eq("bp fill byte", b as i64, b'F' as i64)?;
        }
        seen_fill += want;
    }
    let mut seen_payload = 0usize;
    while seen_payload < accepted {
        let want = (accepted - seen_payload).min(buf.len());
        read_exact_until("bp drain payload", outer.master, &mut buf[..want], drain_dl)?;
        for (j, &b) in buf[..want].iter().enumerate() {
            eq("bp payload byte", b as i64, pattern_byte(seen_payload + j) as i64)?;
        }
        seen_payload += want;
    }

    eq("bp ptyrun exit", wait(pid, 0)? as i64, 0)?;
    close(p2c[1])?;
    close(alive[0])?;
    ensure("bp termios restored", termios(outer.slave)? == orig_termios, 0)?;
    eq("bp O_NONBLOCK cleared", (get_fl(outer.slave)? & NONBLOCK) as i64, 0)
}

fn ptyrun_relay_partial_writes() -> R {
    let outer = Pair::new(NOCTTY)?;
    let orig_termios = termios(outer.slave)?;
    let c2p = FifoReader::new(b"/tmp/ptyrun-pw-c2p\0")?;
    let p2c = pipe(0)?;
    let alive = pipe(0)?;

    let pid = spawn_ptyrun_child(
        outer.slave,
        outer.slave,
        None,
        &[outer.master, outer.slave],
        b"partial-rw\0",
        Some(c2p.path),
        Some(p2c),
        Some(alive),
    )?;

    let mut tag = [0u8; 1];
    recv_child("pw start", c2p.fd, alive[0], pid, Some(outer.master), &mut tag)?;
    eq("pw start tag", tag[0] as i64, b'S' as i64)?;

    // Half A: leave only 137 bytes free in the outer slave's output queue
    // (TIOCOUTQ) so the stdout write that fills the last 137 bytes is partial
    // (137 < 2048). Receiving 'A' after the child's 5120-byte write only
    // confirms ptyrun's to_stdout is non-empty (5120 > 4096 inner queue);
    // spinning until TIOCOUTQ reaches 4096 confirms that partial write landed
    // before draining in 193-byte chunks.
    const PREFILL: usize = 4096 - 137;
    const OUT_LEN: usize = 5120;
    let prefill = [b'P'; PREFILL];
    write_all_fd(outer.slave, &prefill)?;
    eq("pw prefill TIOCOUTQ", getint(outer.slave, TIOCOUTQ)? as i64, PREFILL as i64)?;
    write_all_fd(p2c[1], b"1")?;
    recv_child("pw half-a write", c2p.fd, alive[0], pid, Some(outer.master), &mut tag)?;
    eq("pw half-a write done tag", tag[0] as i64, b'A' as i64)?;
    let spin_dl = now()? + GIVE_UP_NS;
    loop {
        let q = getint(outer.slave, TIOCOUTQ)?;
        if q >= 4096 {
            break;
        }
        if let Some(st) = try_wait(pid)? {
            return fail_early_exit("pw exited before TIOCOUTQ reached 4096", pid, Some(st), Some(outer.master));
        }
        if (poll(c2p.fd, POLLIN)? & (POLLHUP | POLLERR)) != 0 {
            return fail_early_exit("pw child died before TIOCOUTQ reached 4096", pid, None, Some(outer.master));
        }
        if now()? >= spin_dl {
            say!("FAIL timeout (pw TIOCOUTQ spin): TIOCOUTQ={q}");
            return fail_early_exit("pw TIOCOUTQ timeout", pid, None, Some(outer.master));
        }
        let _ = sys(SYS_SCHED_YIELD, [0; 6]);
    }
    eq("pw outer TIOCOUTQ filled by partial write", getint(outer.slave, TIOCOUTQ)? as i64, 4096)?;

    let drain_dl = now()? + GIVE_UP_NS;
    let mut chunk = [0u8; 193];
    let mut got_prefill = 0usize;
    while got_prefill < PREFILL {
        let want = (PREFILL - got_prefill).min(chunk.len());
        read_exact_until("pw drain prefill", outer.master, &mut chunk[..want], drain_dl)?;
        for &b in &chunk[..want] {
            eq("pw prefill byte", b as i64, b'P' as i64)?;
        }
        got_prefill += want;
    }
    let mut got_out = 0usize;
    while got_out < OUT_LEN {
        let want = (OUT_LEN - got_out).min(chunk.len());
        read_exact_until("pw drain output", outer.master, &mut chunk[..want], drain_dl)?;
        for (j, &b) in chunk[..want].iter().enumerate() {
            eq("pw stdout partial byte", b as i64, pattern_byte(got_out + j) as i64)?;
        }
        got_out += want;
    }

    // Half B: push 8191 bytes (4095 inner input buffer + >=1 in ptyrun's
    // to_master + <=4095 outer input buffer) in 2048-byte chunks while the child
    // is paused, then release the child to wait for FIONREAD to reach 4095 and
    // drain all 8191 bytes in 137-byte chunks.
    const IN_LEN: usize = 8191;
    let push_dl = now()? + GIVE_UP_NS;
    let mut send_buf = [0u8; 2048];
    let mut sent = 0usize;
    while sent < IN_LEN {
        let n = (IN_LEN - sent).min(send_buf.len());
        for (j, slot) in send_buf[..n].iter_mut().enumerate() {
            *slot = pattern_byte(sent + j + 1000);
        }
        if let Err(f) = write_all_until("pw 8191 push", outer.master, &send_buf[..n], push_dl) {
            say!("FAIL detail (pw 8191 push): total sent={sent}/{IN_LEN}");
            let _ = kill_and_reap(pid);
            return Err(f);
        }
        sent += n;
    }
    write_all_fd(p2c[1], b"2")?;
    recv_child("pw master drain", c2p.fd, alive[0], pid, Some(outer.master), &mut tag)?;
    eq("pw master partial ok", tag[0] as i64, b'K' as i64)?;

    eq("pw ptyrun exit", wait(pid, 0)? as i64, 0)?;
    close(p2c[1])?;
    close(alive[0])?;
    ensure("pw termios restored", termios(outer.slave)? == orig_termios, 0)?;
    eq("pw O_NONBLOCK cleared", (get_fl(outer.slave)? & NONBLOCK) as i64, 0)
}

fn ptyrun_relay_stdout_write_error_cleanup() -> R {
    let outer = Pair::new(NOCTTY)?;
    let orig_termios = termios(outer.slave)?;
    let ro_fd = ok("open /dev/null rdonly", open(b"/dev/null\0", O_RDONLY))? as i32;
    eq("ro_fd initially blocking", (get_fl(ro_fd)? & NONBLOCK) as i64, 0)?;
    let err_pipe = pipe(0)?;
    let c2p = FifoReader::new(b"/tmp/ptyrun-eb-c2p\0")?;
    let p2c = pipe(0)?;
    let alive = pipe(0)?;

    let pid = spawn_ptyrun_child(
        outer.slave,
        ro_fd,
        Some(err_pipe[1]),
        &[outer.master, outer.slave, ro_fd, err_pipe[0], err_pipe[1]],
        b"write-exit-0\0",
        Some(c2p.path),
        Some(p2c),
        Some(alive),
    )?;
    close(err_pipe[1])?;

    // Confirm ptyrun entered raw mode and set O_NONBLOCK on stdout before the error trigger.
    let mut tag = [0u8; 1];
    recv_child("stdout err ready", c2p.fd, alive[0], pid, None, &mut tag)?;
    eq("stdout err child ready", tag[0] as i64, b'S' as i64)?;
    let raw_t = termios(outer.slave)?;
    eq("stdout err outer raw lflag", u32::from_ne_bytes(raw_t[12..16].try_into().unwrap()) as i64, 0)?;
    ensure("stdout err O_NONBLOCK set", (get_fl(ro_fd)? & NONBLOCK) != 0, 0)?;
    write_all_fd(p2c[1], b"G")?;

    let mut err_buf = [0u8; 512];
    let err_len = read_all_eof_until("stdout err stderr eof", err_pipe[0], &mut err_buf, now()? + GIVE_UP_NS)?;
    close(err_pipe[0])?;

    let status = wait(pid, 0)?;
    close(p2c[1])?;
    close(alive[0])?;
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
    let c2p = FifoReader::new(b"/tmp/ptyrun-sp-c2p\0")?;
    let p2c = pipe(0)?;
    let alive = pipe(0)?;

    let pid = spawn_ptyrun_child(
        outer.slave,
        out_pipe[1],
        Some(err_pipe[1]),
        &[outer.master, outer.slave, out_pipe[0], out_pipe[1], err_pipe[0], err_pipe[1]],
        b"sigpipe-exit-0\0",
        Some(c2p.path),
        Some(p2c),
        Some(alive),
    )?;
    close(err_pipe[1])?;

    // Wait for child to confirm SIGPIPE is SIG_DFL and ptyrun entered raw + O_NONBLOCK.
    let mut tag = [0u8; 1];
    recv_child("sigpipe ready", c2p.fd, alive[0], pid, None, &mut tag)?;
    eq("sigpipe child ready", tag[0] as i64, b'S' as i64)?;
    let raw_t = termios(outer.slave)?;
    eq("sigpipe outer raw lflag", u32::from_ne_bytes(raw_t[12..16].try_into().unwrap()) as i64, 0)?;
    ensure("sigpipe stdout O_NONBLOCK set", (get_fl(out_pipe[1])? & NONBLOCK) != 0, 0)?;

    // Close the sole reader end of stdout's pipe so ptyrun's write triggers SIGPIPE + EPIPE.
    close(out_pipe[0])?;
    write_all_fd(p2c[1], b"G")?;

    let mut err_buf = [0u8; 512];
    let err_len = read_all_eof_until("sigpipe stderr eof", err_pipe[0], &mut err_buf, now()? + GIVE_UP_NS)?;
    close(err_pipe[0])?;

    let status = wait(pid, 0)?;
    close(p2c[1])?;
    close(alive[0])?;
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
    let c2p = FifoReader::new(b"/tmp/ptyrun-ws-c2p\0")?;
    let alive = pipe(0)?;
    check_below_fixed(&[outer.master, outer.slave, alive[0], alive[1]])?;

    let pid = fork()?;
    if pid == 0 {
        if let Err(f) = (|| -> R {
            dup2_fd(2, DIAG_FD)?;
            set_sig_handler(SIGPIPE, SIG_DFL)?;
            dup2_fd(alive[1], ALIVE_FD)?;
            let _ = close(alive[0]);
            if alive[1] != ALIVE_FD {
                let _ = close(alive[1]);
            }
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
        exec_ptyrun_child(b"winsize\0", Some(c2p.path));
    }
    let _ = close(alive[1]);

    let mut tag = [0u8; 1];
    // Child confirmed initial 25x85 size and blocked SIGWINCH.
    recv_child("ws init", c2p.fd, alive[0], pid, Some(outer.master), &mut tag)?;
    eq("ws initial size tag", tag[0] as i64, b'1' as i64)?;

    // First resize: 40x120.
    set_winsize(outer.master, 40, 120)?;
    recv_child("ws first resize", c2p.fd, alive[0], pid, Some(outer.master), &mut tag)?;
    eq("ws first resize tag", tag[0] as i64, b'2' as i64)?;

    // Second resize: 50x132 (confirms handler is not one-shot).
    set_winsize(outer.master, 50, 132)?;
    recv_child("ws second resize", c2p.fd, alive[0], pid, Some(outer.master), &mut tag)?;
    eq("ws second resize tag", tag[0] as i64, b'3' as i64)?;

    eq("ws ptyrun exit", wait(pid, 0)? as i64, 0)?;
    close(alive[0])?;
    ensure("ws termios restored", termios(outer.slave)? == orig_termios, 0)
}

fn ptyrun_relay_exit_status_and_eof() -> R {
    // 1) Child exit status mapped as a shell would (code, or 128+signal).
    for (label, mode, want_code) in [
        ("relay forwards exit 42", b"exit-42\0" as &'static [u8], 42i64),
        ("relay forwards 128+SIGUSR1", b"self-usr1\0", (128 + SIGUSR1) as i64),
    ] {
        let outer = Pair::new(NOCTTY)?;
        let orig_termios = termios(outer.slave)?;
        let pid =
            spawn_ptyrun_child(outer.slave, outer.slave, None, &[outer.master, outer.slave], mode, None, None, None)?;
        eq(label, wait(pid, 0)? as i64, want_code << 8)?;
        ensure("exit-status termios restored", termios(outer.slave)? == orig_termios, 0)?;
    }

    // 2) Piped stdin EOF without trailing newline sends double VEOF so canonical child finishes.
    let in_pipe = pipe(0)?;
    let out_pipe = pipe(0)?;
    let pid = spawn_ptyrun_child(
        in_pipe[0],
        out_pipe[1],
        None,
        &[in_pipe[0], in_pipe[1], out_pipe[0], out_pipe[1]],
        b"read-eof\0",
        None,
        None,
        None,
    )?;
    close(in_pipe[0])?;
    close(out_pipe[1])?;
    write_all_fd(in_pipe[1], b"no-newline")?;
    close(in_pipe[1])?;
    let mut out_buf = [0u8; 256];
    let out_len = match read_all_eof_until("eof stdout", out_pipe[0], &mut out_buf, now()? + GIVE_UP_NS) {
        Ok(n) => n,
        Err(f) => {
            let _ = close(out_pipe[0]);
            let _ = kill_and_reap(pid);
            return Err(f);
        }
    };
    close(out_pipe[0])?;
    eq("eof ptyrun exit", wait(pid, 0)? as i64, 0)?;
    ensure("eof child output", contains_bytes(&out_buf[..out_len], b"EOF:no-newline"), out_len as i64)
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

fn child_backpressure(c2p_path: &[u8]) -> c_int {
    child_status(
        (|| -> R {
            let c2p = open_cstr(c2p_path, O_WRONLY)?;
            raw(0, 1, 0)?;
            write_all_fd(c2p, b"S")?;
            let mut go = [0u8; 1];
            read_exact_until("bp child go", P2C_FD, &mut go, now()? + GIVE_UP_NS)?;
            eq("bp child go", go[0] as i64, b'G' as i64)?;
            // Write 2048-byte chunks so the first write commits atomically into
            // the empty PTY output queue and ptyrun's first read fills PendingBuf.
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
            // Write non-blocking until EAGAIN so the parent can bound total
            // buffering (cap at 16384 so an unbounded-buffer bug still finishes).
            nonblock(1, true)?;
            while written < 16384 {
                let n = (16384 - written).min(out.len());
                for (j, slot) in out[..n].iter_mut().enumerate() {
                    *slot = pattern_byte(written + j);
                }
                let r = sys(SYS_WRITE, [1, out.as_ptr() as u64, n as u64, 0, 0, 0]);
                if r > 0 {
                    written += r as usize;
                } else if r == EINTR {
                    continue;
                } else if r == EAGAIN {
                    break;
                } else {
                    return Err(Fail { what: "bp nonblock write", got: r });
                }
            }
            nonblock(1, false)?;
            let mut report = [0u8; 5];
            report[0] = b'W';
            report[1..5].copy_from_slice(&(written as u32).to_le_bytes());
            write_all_fd(c2p, &report)?;
            let mut got = [0u8; 16];
            read_exact_until("bp child input", 0, &mut got, now()? + GIVE_UP_NS)?;
            ensure("bp child got input", &got == b"backpressure-ok\n", got[0] as i64)?;
            write_all_fd(c2p, b"I")?;
            let _ = close(c2p);
            Ok(())
        })(),
        0,
    )
}

fn child_partial_rw(c2p_path: &[u8]) -> c_int {
    child_status(
        (|| -> R {
            let c2p = open_cstr(c2p_path, O_WRONLY)?;
            raw(0, 1, 0)?;
            write_all_fd(c2p, b"S")?;
            let mut step = [0u8; 1];
            read_exact_until("pw step 1", P2C_FD, &mut step, now()? + GIVE_UP_NS)?;
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
            read_exact_until("pw step 2", P2C_FD, &mut step, now()? + GIVE_UP_NS)?;
            eq("pw step 2", step[0] as i64, b'2' as i64)?;
            // Wait for FIONREAD to reach 4095 (inner raw input buffer full,
            // leaving pending bytes in ptyrun's to_master) before draining.
            let spin_dl = now()? + GIVE_UP_NS;
            loop {
                let q = getint(0, FIONREAD)?;
                if q >= 4095 {
                    break;
                }
                if (poll(0, 0)? & POLLHUP) != 0 {
                    return Err(Fail { what: "pw inner slave hung up before FIONREAD 4095", got: q as i64 });
                }
                if now()? >= spin_dl {
                    return Err(Fail { what: "pw FIONREAD timeout", got: q as i64 });
                }
                let _ = sys(SYS_SCHED_YIELD, [0; 6]);
            }
            let drain_dl = now()? + GIVE_UP_NS;
            let mut chunk = [0u8; 137];
            let mut got = 0usize;
            while got < 8191 {
                let want = (8191 - got).min(chunk.len());
                read_exact_until("pw child drain", 0, &mut chunk[..want], drain_dl)?;
                for (j, &b) in chunk[..want].iter().enumerate() {
                    eq("pw child input byte", b as i64, pattern_byte(got + j + 1000) as i64)?;
                }
                got += want;
            }
            write_all_fd(c2p, b"K")?;
            let _ = close(c2p);
            Ok(())
        })(),
        0,
    )
}

fn child_write_exit_0(c2p_path: &[u8]) -> c_int {
    child_status(
        (|| -> R {
            let c2p = open_cstr(c2p_path, O_WRONLY)?;
            set_sig_handler(SIGHUP, SIG_IGN)?;
            write_all_fd(c2p, b"S")?;
            let mut go = [0u8; 1];
            read_exact_until("ebadf child go", P2C_FD, &mut go, now()? + GIVE_UP_NS)?;
            eq("ebadf child go", go[0] as i64, b'G' as i64)?;
            write_all_fd(1, b"stdout-ebadf-trigger\n")?;
            let _ = close(c2p);
            Ok(())
        })(),
        0,
    )
}

fn child_sigpipe_exit_0(c2p_path: &[u8]) -> c_int {
    child_status(
        (|| -> R {
            let c2p = open_cstr(c2p_path, O_WRONLY)?;
            eq("child SIGPIPE is SIG_DFL", get_sig_handler(SIGPIPE)? as i64, SIG_DFL as i64)?;
            set_sig_handler(SIGHUP, SIG_IGN)?;
            write_all_fd(c2p, b"S")?;
            let mut go = [0u8; 1];
            read_exact_until("sp child go", P2C_FD, &mut go, now()? + GIVE_UP_NS)?;
            eq("sp child go", go[0] as i64, b'G' as i64)?;
            write_all_fd(1, b"stdout-epipe-trigger\n")?;
            let _ = close(c2p);
            Ok(())
        })(),
        0,
    )
}

fn child_winsize(c2p_path: &[u8]) -> c_int {
    let _ = close(P2C_FD);
    child_status(
        (|| -> R {
            let c2p = open_cstr(c2p_path, O_WRONLY)?;
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
        0,
    )
}

fn child_self_usr1() -> c_int {
    let _ = close(P2C_FD);
    let _ = kill(sys(SYS_GETPID, [0; 6]), SIGUSR1);
    99
}

fn child_read_eof() -> c_int {
    let _ = close(P2C_FD);
    child_status(
        (|| -> R {
            let mut buf = [0u8; 64];
            let len = read_all_eof_until("child read-eof", 0, &mut buf, now()? + GIVE_UP_NS)?;
            let text = core::str::from_utf8(&buf[..len]).unwrap_or("");
            say!("EOF:{text}");
            Ok(())
        })(),
        0,
    )
}

fn child_console_echo() -> c_int {
    let _ = close(P2C_FD);
    child_status(
        (|| -> R {
            let con = ok("open /dev/console", open(b"/dev/console\0", O_RDONLY | NOCTTY))? as i32;
            let con_t = termios(con)?;
            let _ = close(con);
            let lflag = u32::from_ne_bytes(con_t[12..16].try_into().unwrap());
            say!("RELAY_CONSOLE_LFLAG:{lflag}");
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
        7,
    )
}

fn dump_termios() -> c_int {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    match termios(0) {
        Ok(t) => {
            let mut out = [0u8; 72];
            for (i, &b) in t.iter().enumerate() {
                out[i * 2] = HEX[(b >> 4) as usize];
                out[i * 2 + 1] = HEX[(b & 0xf) as usize];
            }
            let hex = core::str::from_utf8(&out).unwrap_or("");
            say!("TERMIOS:{hex}");
            0
        }
        Err(f) => {
            say!("FAIL termios: {} (got {})", f.what, f.got);
            1
        }
    }
}

fn run_ptyrun_child(argv: &[&[u8]]) -> c_int {
    // Only ptyrun should hold ALIVE_FD so alive[0] HUP tracks ptyrun's exit.
    let _ = close(ALIVE_FD);
    match argv {
        [_, _, b"backpressure", c2p] => child_backpressure(c2p),
        [_, _, b"partial-rw", c2p] => child_partial_rw(c2p),
        [_, _, b"write-exit-0", c2p] => child_write_exit_0(c2p),
        [_, _, b"sigpipe-exit-0", c2p] => child_sigpipe_exit_0(c2p),
        [_, _, b"winsize", c2p] => child_winsize(c2p),
        [_, _, b"exit-42"] => {
            let _ = close(P2C_FD);
            42
        }
        [_, _, b"self-usr1"] => child_self_usr1(),
        [_, _, b"read-eof"] => child_read_eof(),
        [_, _, b"console-echo"] => child_console_echo(),
        _ => USAGE_EXIT,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const u8) -> c_int {
    // SAFETY: musl passes the kernel's `argc` valid NUL-terminated C strings.
    let (args, n) = unsafe { common::args(argc, argv) };
    match &args[..n] {
        [] | [_] => run_checks(&[
            ("pty_data_and_timers", &|| isolated(data_and_timers)),
            ("pty_job_control_and_hangup", &|| isolated(job_control)),
            ("pty_orphans_and_leader_exit", &|| isolated(orphan_and_leader_exit)),
        ]),
        [_, b"ptyrun-relay"] => run_checks(&[
            ("ptyrun_relay_two_way_backpressure", &|| isolated(ptyrun_relay_two_way_backpressure)),
            ("ptyrun_relay_partial_writes", &|| isolated(ptyrun_relay_partial_writes)),
            ("ptyrun_relay_stdout_write_error_cleanup", &|| isolated(ptyrun_relay_stdout_write_error_cleanup)),
            ("ptyrun_relay_sigpipe_reaches_cleanup", &|| isolated(ptyrun_relay_sigpipe_reaches_cleanup)),
            ("ptyrun_relay_sigwinch_forwards_resize", &|| isolated(ptyrun_relay_sigwinch_forwards_resize)),
            ("ptyrun_relay_exit_status_and_eof", &|| isolated(ptyrun_relay_exit_status_and_eof)),
        ]),
        [_, b"termios"] => dump_termios(),
        [_, b"ptyrun-child", ..] => run_ptyrun_child(&args[..n]),
        _ => {
            say!("usage: m8_pty [ptyrun-relay | termios | ptyrun-child <mode> ...]");
            USAGE_EXIT
        }
    }
}
