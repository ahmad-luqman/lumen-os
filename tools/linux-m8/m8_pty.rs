//! C9 PTY integration checks. Pipes and wait4 statuses order every process
//! transition; time checks impose only a requested timeout's lower bound.
#![no_std]
#![no_main]

#[macro_use]
mod common;
use common::*;

const RDWR: u64 = 2;
const NONBLOCK: u64 = 0x800;
const NOCTTY: u64 = 0x100;
const TCGETS: u64 = 0x5401;
const TCSETS: u64 = 0x5402;
const TCSETSW: u64 = 0x5403;
const TCFLSH: u64 = 0x540b;
const TCSETSF: u64 = 0x5404;
const SETCTTY: u64 = 0x540e;
const GETPGRP: u64 = 0x540f;
const SETPGRP: u64 = 0x5410;
const NOTTY: u64 = 0x5422;
const GETSID: u64 = 0x5429;
const PTN: u64 = 0x80045430;
const PTLCK: u64 = 0x40045431;

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

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    run_checks(&[
        ("pty_data_and_timers", &|| isolated(data_and_timers)),
        ("pty_job_control_and_hangup", &|| isolated(job_control)),
        ("pty_orphans_and_leader_exit", &|| isolated(orphan_and_leader_exit)),
    ])
}
