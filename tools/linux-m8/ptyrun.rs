//! `ptyrun`: D3's (#395) PTY runner and console-to-PTY relay.
//!
//! Two modes:
//! - Scripted PTY-master mode (does not depend on `/dev/console` being a full
//!   `Tty` yet): `ptyrun --script <path> -- <program> [args...]`. Opens
//!   `/dev/ptmx`, starts `<program>` on `/dev/pts/N` as a session leader with
//!   it as its controlling terminal, sets a 24x80 window size, and runs the
//!   script's `expect` / `send` / `winsize` / `wait_exit` steps directly over
//!   the PTY master, printing one `ok <step>` line per completed step.
//! - Interactive relay mode: `ptyrun <program> [args...]`. Opens `/dev/ptmx`,
//!   copies stdin's window size onto the PTY (default 24x80), forwards
//!   `SIGWINCH` resizes from stdin (`TIOCGWINSZ`) to the PTY master
//!   (`TIOCSWINSZ`), puts stdin in raw mode without echo if it is a tty, sets
//!   `O_NONBLOCK` on stdout, ignores `SIGPIPE` in the parent (resetting it to
//!   `SIG_DFL` in the child), forks the program on the slave as a session
//!   leader with it as its controlling terminal, and relays bytes between
//!   stdin/stdout and the PTY master. Restores stdout's flags, stdin's termios,
//!   `SIGWINCH`, and `SIGPIPE` on exit.
//!
//! A `#![no_std]` dynamically linked musl x86-64 executable, built into
//! `build/linux/ptyrun` and staged in `/usr/local/bin/ptyrun`.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]

#[macro_use]
mod common;

use common::{
    _exit, EAGAIN, Fail, R, SYS_DUP2, SYS_EXECVE, SYS_FCNTL, SYS_FORK, SYS_IOCTL, SYS_OPENAT, SYS_POLL, SYS_READ,
    SYS_RT_SIGACTION, SYS_WAIT4, SYS_WRITE, USAGE_EXIT, close, eq, ok, pipe, sys,
};
use core::arch::global_asm;
use core::ffi::c_int;
use core::fmt;
use core::sync::atomic::{AtomicI32, Ordering};

// SA_RESTORER stub invoking rt_sigreturn (nr 15) on x86-64.
global_asm!(".global ptyrun_sigreturn", "ptyrun_sigreturn:", "mov eax, 15", "syscall", "ud2");

// SAFETY: musl exposes `environ` as a NUL-terminated array of C string pointers,
// and `ptyrun_sigreturn` is defined in the `global_asm!` block above.
unsafe extern "C" {
    static environ: *const *const u8;
    fn ptyrun_sigreturn();
}

const AT_FDCWD: u64 = (-100i64) as u64;
const SYS_SETSID: u64 = 112;

const O_RDONLY: u64 = 0;
const O_RDWR: u64 = 2;
const O_NOCTTY: u64 = 0x100;
const O_NONBLOCK: u64 = 0x800;
const O_CLOEXEC: u64 = 0x8_0000;

const F_GETFL: u64 = 3;
const F_SETFL: u64 = 4;

const SIGPIPE: u64 = 13;
const SIGWINCH: u64 = 28;
const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;
const SA_RESTORER: u64 = 0x0400_0000;

const ICANON: u32 = 0x2;
const VEOF: usize = 4;

const EINTR: i64 = -4;
const EIO: i64 = -5;

const TCGETS: u64 = 0x5401;
const TCSETS: u64 = 0x5402;
const TIOCSCTTY: u64 = 0x540e;
const TIOCGWINSZ: u64 = 0x5413;
const TIOCSWINSZ: u64 = 0x5414;
const TIOCGPTN: u64 = 0x8004_5430;
const TIOCSPTLCK: u64 = 0x4004_5431;

const POLLIN: i16 = 0x0001;
const POLLOUT: i16 = 0x0004;
const POLLERR: i16 = 0x0008;
const POLLHUP: i16 = 0x0010;
const POLLNVAL: i16 = 0x0020;

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;

const USAGE: &str = "usage: ptyrun [--script <file> --] <program> [args...]";

/// Fixed-buffer line writer targeting stderr (fd 2) so failure diagnostics do
/// not depend on stdout being writable.
struct ErrLine {
    buf: [u8; 512],
    len: usize,
}

impl ErrLine {
    fn new() -> Self {
        Self { buf: [0; 512], len: 0 }
    }

    fn send(&mut self) {
        if self.len == self.buf.len() {
            self.buf[self.len - 1] = b'\n';
        }
        let mut off = 0;
        while off < self.len {
            let n = write_raw(2, &self.buf[off..self.len]);
            if n > 0 {
                off += n as usize;
            } else if n != EINTR {
                break;
            }
        }
        self.len = 0;
    }
}

impl fmt::Write for ErrLine {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let room = self.buf.len() - self.len;
        let n = s.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

macro_rules! say_err {
    ($($arg:tt)*) => {{
        use core::fmt::Write as _;
        let mut line = ErrLine::new();
        let _ = writeln!(line, $($arg)*);
        line.send();
    }};
}

static WINCH_PIPE_W: AtomicI32 = AtomicI32::new(-1);

/// Async-signal-safe `SIGWINCH` handler: performs one atomic load and one raw
/// `write` syscall to the non-blocking self-pipe (`write_raw` is a direct
/// syscall and never touches libc `errno`). `EAGAIN` on a full pipe is
/// harmless because the read end is already readable and will coalesce resizes.
extern "C" fn sigwinch_handler(_sig: c_int) {
    let fd = WINCH_PIPE_W.load(Ordering::Acquire);
    if fd >= 0 {
        let byte = [b'w'];
        let _ = write_raw(fd, &byte);
    }
}

fn set_sigaction(sig: u64, action: &[u64; 4], old: Option<&mut [u64; 4]>) -> i64 {
    let old_ptr = match old {
        Some(slot) => slot.as_mut_ptr() as u64,
        None => 0,
    };
    sys(SYS_RT_SIGACTION, [sig, action.as_ptr() as u64, old_ptr, 8, 0, 0])
}

fn set_sigpipe(handler: u64, old: Option<&mut [u64; 4]>) -> i64 {
    let action = [handler, 0, 0, 0];
    set_sigaction(SIGPIPE, &action, old)
}

fn set_sigwinch(old: &mut [u64; 4]) -> i64 {
    let action = [sigwinch_handler as *const () as u64, SA_RESTORER, ptyrun_sigreturn as *const () as u64, 0];
    set_sigaction(SIGWINCH, &action, Some(old))
}

fn restore_sigaction(sig: u64, old: &[u64; 4]) -> i64 {
    set_sigaction(sig, old, None)
}

fn read_stdin_winsize() -> Option<[u16; 4]> {
    let mut ws = [0u16; 4];
    if ioctl(0, TIOCGWINSZ, ws.as_mut_ptr() as u64) == 0 && ws[0] > 0 && ws[1] > 0 { Some(ws) } else { None }
}

fn forward_winsize(master: i32) {
    if let Some(ws) = read_stdin_winsize() {
        let r = ioctl(master, TIOCSWINSZ, ws.as_ptr() as u64);
        if r < 0 {
            say_err!("ptyrun: TIOCSWINSZ failed (got {r})");
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

fn ioctl(fd: i32, cmd: u64, arg: u64) -> i64 {
    sys(SYS_IOCTL, [fd as u64, cmd, arg, 0, 0, 0])
}

fn openat(path: *const u8, flags: u64) -> i64 {
    sys(SYS_OPENAT, [AT_FDCWD, path as u64, flags, 0, 0, 0])
}

fn read_raw(fd: i32, buf: &mut [u8]) -> i64 {
    sys(SYS_READ, [fd as u64, buf.as_mut_ptr() as u64, buf.len() as u64, 0, 0, 0])
}

fn write_raw(fd: i32, buf: &[u8]) -> i64 {
    sys(SYS_WRITE, [fd as u64, buf.as_ptr() as u64, buf.len() as u64, 0, 0, 0])
}

fn poll_fds(fds: &mut [PollFd], timeout_ms: i32) -> Result<i64, Fail> {
    loop {
        let r = sys(SYS_POLL, [fds.as_mut_ptr() as u64, fds.len() as u64, timeout_ms as i64 as u64, 0, 0, 0]);
        if r == EINTR {
            continue;
        }
        return ok("poll", r);
    }
}

fn set_winsize(master: i32, rows: u16, cols: u16) -> R {
    let ws = [rows, cols, 0u16, 0u16];
    eq("TIOCSWINSZ", ioctl(master, TIOCSWINSZ, ws.as_ptr() as u64), 0)
}

fn wait_child(pid: i64) -> Result<i32, Fail> {
    let mut status = 0i32;
    loop {
        let r = sys(SYS_WAIT4, [pid as u64, &mut status as *mut i32 as u64, 0, 0, 0, 0]);
        if r == EINTR {
            continue;
        }
        eq("wait4", r, pid)?;
        return Ok(status);
    }
}

struct PtyChild {
    master: i32,
    pid: i64,
}

/// Open `/dev/ptmx`, unlock its slave, set the window size, and fork/exec
/// `argv[cmd_idx..]` on the slave as a session leader with it as its
/// controlling terminal. When `reset_sigpipe` is true, the child resets
/// `SIGPIPE` to `SIG_DFL` before `execve` (caught handlers such as `SIGWINCH`
/// are reset and `O_CLOEXEC` descriptors such as the winch pipe are closed by
/// `execve`; the child clears `WINCH_PIPE_W` immediately after `fork`).
///
/// # Safety
/// `argv` must hold at least `cmd_idx + 1` valid NUL-terminated C string
/// pointers followed by a NULL terminator.
unsafe fn spawn_on_pty(
    argv: *const *const u8,
    cmd_idx: usize,
    rows: u16,
    cols: u16,
    reset_sigpipe: bool,
) -> Result<PtyChild, Fail> {
    let master = ok("open /dev/ptmx", openat(b"/dev/ptmx\0".as_ptr(), O_RDWR | O_NOCTTY | O_NONBLOCK))? as i32;
    let mut ptn = -1i32;
    eq("TIOCGPTN", ioctl(master, TIOCGPTN, &mut ptn as *mut i32 as u64), 0)?;
    if !(0..256).contains(&ptn) {
        let _ = close(master);
        return Err(Fail { what: "PTY number out of range", got: ptn as i64 });
    }
    let unlock = 0i32;
    eq("TIOCSPTLCK", ioctl(master, TIOCSPTLCK, &unlock as *const i32 as u64), 0)?;
    set_winsize(master, rows, cols)?;

    let mut slave_path = [0u8; 20];
    slave_path[..9].copy_from_slice(b"/dev/pts/");
    let mut at = 9;
    for place in [100, 10, 1] {
        if ptn >= place || place == 1 {
            slave_path[at] = b'0' + (ptn / place % 10) as u8;
            at += 1;
        }
    }
    slave_path[at] = 0;

    // CLOEXEC pipe synchronizes child startup: EOF means the child opened the
    // slave (so `slave_seen` is true on the master) and completed `execve`.
    let err_pipe = pipe(O_CLOEXEC)?;

    let pid = ok("fork", sys(SYS_FORK, [0; 6]))?;
    if pid == 0 {
        WINCH_PIPE_W.store(-1, Ordering::Relaxed);
        let _ = close(err_pipe[0]);
        let fail_child = |code: u8, err: i64| -> ! {
            let msg = [code, (err.clamp(-127, 127) as i8) as u8];
            let _ = write_raw(err_pipe[1], &msg);
            let _ = close(err_pipe[1]);
            // SAFETY: child exits immediately without running parent destructors.
            unsafe { _exit(127) }
        };

        let r = sys(SYS_SETSID, [0; 6]);
        if r < 0 {
            fail_child(1, r);
        }
        let slave = openat(slave_path.as_ptr(), O_RDWR);
        if slave < 0 {
            fail_child(2, slave);
        }
        let slave = slave as i32;
        let r = ioctl(slave, TIOCSCTTY, 0);
        if r < 0 {
            fail_child(3, r);
        }
        let _ = close(master);
        for target in 0..=2 {
            if slave != target {
                let r = sys(SYS_DUP2, [slave as u64, target as u64, 0, 0, 0, 0]);
                if r < 0 {
                    fail_child(4, r);
                }
            }
        }
        if slave > 2 {
            let _ = close(slave);
        }
        if reset_sigpipe {
            let r = set_sigpipe(SIG_DFL, None);
            if r < 0 {
                fail_child(6, r);
            }
        }
        // SAFETY: caller guarantees `argv.add(cmd_idx)` is a valid NULL-terminated
        // array of C strings and musl's `environ` is a valid NULL-terminated envp.
        let (path, child_argv, child_envp) = unsafe { (*argv.add(cmd_idx), argv.add(cmd_idx), environ) };
        let r = sys(SYS_EXECVE, [path as u64, child_argv as u64, child_envp as u64, 0, 0, 0]);
        fail_child(5, r);
    }

    let _ = close(err_pipe[1]);
    let mut err_buf = [0u8; 2];
    let n = loop {
        let r = read_raw(err_pipe[0], &mut err_buf);
        if r == EINTR {
            continue;
        }
        break r;
    };
    let _ = close(err_pipe[0]);
    if n < 0 {
        let _ = close(master);
        let _ = wait_child(pid);
        return Err(Fail { what: "read startup pipe", got: n });
    }
    if n > 0 {
        let _ = close(master);
        let _ = wait_child(pid);
        let what = match err_buf[0] {
            1 => "child setsid",
            2 => "child open slave",
            3 => "child TIOCSCTTY",
            4 => "child dup2",
            5 => "child execve",
            6 => "child reset SIGPIPE",
            _ => "child startup",
        };
        return Err(Fail { what, got: (err_buf[1] as i8) as i64 });
    }

    Ok(PtyChild { master, pid })
}

struct ScreenBuf {
    buf: [u8; 32768],
    len: usize,
    cursor: usize,
}

impl ScreenBuf {
    fn new() -> Self {
        Self { buf: [0; 32768], len: 0, cursor: 0 }
    }

    fn push(&mut self, data: &[u8]) {
        if data.len() >= self.buf.len() {
            let start = data.len() - self.buf.len();
            self.buf.copy_from_slice(&data[start..]);
            self.len = self.buf.len();
            self.cursor = 0;
            return;
        }
        if self.len + data.len() > self.buf.len() {
            if self.cursor > 0 {
                self.buf.copy_within(self.cursor..self.len, 0);
                self.len -= self.cursor;
                self.cursor = 0;
            }
            if self.len + data.len() > self.buf.len() {
                let keep = (self.buf.len() - data.len()).min(4096).min(self.len);
                let start = self.len - keep;
                self.buf.copy_within(start..self.len, 0);
                self.len = keep;
                self.cursor = 0;
            }
        }
        self.buf[self.len..self.len + data.len()].copy_from_slice(data);
        self.len += data.len();
    }

    fn find_and_advance(&mut self, needle: &[u8]) -> bool {
        if needle.is_empty() {
            return true;
        }
        let hay = &self.buf[self.cursor..self.len];
        if hay.len() < needle.len() {
            return false;
        }
        for i in 0..=hay.len() - needle.len() {
            if &hay[i..i + needle.len()] == needle {
                self.cursor += i + needle.len();
                return true;
            }
        }
        false
    }

    fn dump_tail(&self) {
        let view = &self.buf[self.cursor..self.len];
        let start = view.len().saturating_sub(240);
        let slice = &view[start..];
        let mut out = [0u8; 400];
        let mut at = 0;
        for &b in slice {
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
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out[at] = b'\\';
                out[at + 1] = b'x';
                out[at + 2] = HEX[(b >> 4) as usize];
                out[at + 3] = HEX[(b & 0xf) as usize];
                at += 4;
            }
        }
        let text = core::str::from_utf8(&out[..at]).unwrap_or("");
        say_err!("pty-tail: {text}");
    }
}

/// Drain every byte currently buffered on the non-blocking PTY master.
/// Returns `Ok(true)` if the slave side is still open, `Ok(false)` on EOF/EIO.
fn drain_available(master: i32, screen: &mut ScreenBuf) -> Result<bool, Fail> {
    let mut chunk = [0u8; 2048];
    loop {
        let n = read_raw(master, &mut chunk);
        if n > 0 {
            screen.push(&chunk[..n as usize]);
        } else if n == 0 || n == EIO {
            return Ok(false);
        } else if n == EAGAIN {
            return Ok(true);
        } else if n == EINTR {
            continue;
        } else {
            return Err(Fail { what: "read master", got: n });
        }
    }
}

/// Search incrementally after each chunk read so a fast producer writing more
/// than `ScreenBuf`'s capacity cannot evict an earlier match before it is seen.
/// `ScreenBuf::push` retains a 4096-byte tail across evictions (well above the
/// 512-byte maximum needle length), preserving matches split across chunks.
fn expect_bytes(master: i32, screen: &mut ScreenBuf, needle: &[u8]) -> R {
    let mut chunk = [0u8; 2048];
    loop {
        if screen.find_and_advance(needle) {
            return Ok(());
        }
        let n = read_raw(master, &mut chunk);
        if n > 0 {
            screen.push(&chunk[..n as usize]);
            continue;
        }
        if n == EINTR {
            continue;
        }
        if n == 0 || n == EIO {
            screen.dump_tail();
            return Err(Fail { what: "expect reached EOF/HUP before match", got: n });
        }
        if n != EAGAIN {
            return Err(Fail { what: "read master", got: n });
        }
        let mut pfd = [PollFd { fd: master, events: POLLIN, revents: 0 }];
        poll_fds(&mut pfd, -1)?;
        if (pfd[0].revents & (POLLHUP | POLLERR)) != 0 && (pfd[0].revents & POLLIN) == 0 {
            let n = read_raw(master, &mut chunk);
            if n > 0 {
                screen.push(&chunk[..n as usize]);
                continue;
            }
            screen.dump_tail();
            return Err(Fail { what: "expect reached EOF/HUP before match", got: pfd[0].revents as i64 });
        }
    }
}

fn send_bytes(master: i32, screen: &mut ScreenBuf, bytes: &[u8]) -> R {
    let mut off = 0;
    while off < bytes.len() {
        let _ = drain_available(master, screen)?;
        let n = write_raw(master, &bytes[off..]);
        if n > 0 {
            off += n as usize;
            continue;
        }
        if n == EINTR {
            continue;
        }
        if n == EAGAIN {
            let mut pfd = [PollFd { fd: master, events: POLLIN | POLLOUT, revents: 0 }];
            poll_fds(&mut pfd, -1)?;
            if (pfd[0].revents & POLLIN) != 0 {
                let _ = drain_available(master, screen)?;
            }
            if (pfd[0].revents & (POLLHUP | POLLERR)) != 0 && (pfd[0].revents & POLLOUT) == 0 {
                return Err(Fail { what: "write master hit HUP", got: pfd[0].revents as i64 });
            }
            continue;
        }
        return Err(Fail { what: "write master", got: n });
    }
    Ok(())
}

fn drain_until_eof(master: i32, screen: &mut ScreenBuf) -> R {
    loop {
        if !drain_available(master, screen)? {
            return Ok(());
        }
        let mut pfd = [PollFd { fd: master, events: POLLIN, revents: 0 }];
        poll_fds(&mut pfd, -1)?;
        if !drain_available(master, screen)? {
            return Ok(());
        }
        if (pfd[0].revents & (POLLHUP | POLLERR)) != 0 && (pfd[0].revents & POLLIN) == 0 {
            return Ok(());
        }
    }
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode_escapes(src: &[u8], dst: &mut [u8]) -> Result<usize, Fail> {
    let mut i = 0;
    let mut out = 0;
    while i < src.len() {
        if out >= dst.len() {
            return Err(Fail { what: "escaped payload too long", got: src.len() as i64 });
        }
        if src[i] == b'\\' && i + 1 < src.len() {
            match src[i + 1] {
                b'r' => {
                    dst[out] = b'\r';
                    i += 2;
                }
                b'n' => {
                    dst[out] = b'\n';
                    i += 2;
                }
                b't' => {
                    dst[out] = b'\t';
                    i += 2;
                }
                b'e' => {
                    dst[out] = 0x1b;
                    i += 2;
                }
                b'\\' => {
                    dst[out] = b'\\';
                    i += 2;
                }
                b'x' if i + 3 < src.len() => {
                    let Some(hi) = hex_nibble(src[i + 2]) else {
                        return Err(Fail { what: "bad hex escape", got: src[i + 2] as i64 });
                    };
                    let Some(lo) = hex_nibble(src[i + 3]) else {
                        return Err(Fail { what: "bad hex escape", got: src[i + 3] as i64 });
                    };
                    dst[out] = (hi << 4) | lo;
                    i += 4;
                }
                other => return Err(Fail { what: "unknown escape", got: other as i64 }),
            }
            out += 1;
        } else {
            dst[out] = src[i];
            out += 1;
            i += 1;
        }
    }
    Ok(out)
}

fn parse_u16(s: &[u8]) -> Option<u16> {
    if s.is_empty() {
        return None;
    }
    let mut v = 0u32;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v * 10 + (b - b'0') as u32;
        if v > u16::MAX as u32 {
            return None;
        }
    }
    Some(v as u16)
}

fn read_script(path: *const u8, buf: &mut [u8]) -> Result<usize, Fail> {
    let fd = ok("open script", openat(path, O_RDONLY))? as i32;
    let mut len = 0;
    loop {
        if len == buf.len() {
            let _ = close(fd);
            return Err(Fail { what: "script exceeds buffer", got: len as i64 });
        }
        let n = read_raw(fd, &mut buf[len..]);
        if n == EINTR {
            continue;
        }
        if n < 0 {
            let _ = close(fd);
            return Err(Fail { what: "read script", got: n });
        }
        if n == 0 {
            break;
        }
        len += n as usize;
    }
    close(fd)?;
    Ok(len)
}

/// Run scripted PTY master steps against `argv[cmd_idx..]`.
///
/// # Safety
/// `script_path` must be a valid NUL-terminated C string and `argv[cmd_idx..]`
/// must be a valid NULL-terminated array of C strings.
unsafe fn run_script(script_path: *const u8, argv: *const *const u8, cmd_idx: usize) -> R {
    let mut script_buf = [0u8; 4096];
    let script_len = read_script(script_path, &mut script_buf)?;

    // SAFETY: forwarded from caller's `argv` contract.
    let child = unsafe { spawn_on_pty(argv, cmd_idx, DEFAULT_ROWS, DEFAULT_COLS, false)? };
    let mut screen = ScreenBuf::new();
    let mut waited = false;
    let mut step = 0usize;
    let mut payload = [0u8; 512];

    let result = (|| -> R {
        for raw_line in script_buf[..script_len].split(|&b| b == b'\n') {
            let line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
            if line.is_empty() || line[0] == b'#' {
                continue;
            }
            step += 1;
            if let Some(arg) = line.strip_prefix(b"expect ") {
                let n = decode_escapes(arg, &mut payload)?;
                expect_bytes(child.master, &mut screen, &payload[..n])?;
                let label = core::str::from_utf8(arg).unwrap_or("bytes");
                say!("ok step {step} expect {label}");
            } else if let Some(arg) = line.strip_prefix(b"send ") {
                let n = decode_escapes(arg, &mut payload)?;
                send_bytes(child.master, &mut screen, &payload[..n])?;
                say!("ok step {step} send");
            } else if let Some(arg) = line.strip_prefix(b"winsize ") {
                let mut parts = arg.split(|&b| b == b' ');
                let rows =
                    parts.next().and_then(parse_u16).ok_or(Fail { what: "bad winsize rows", got: step as i64 })?;
                let cols =
                    parts.next().and_then(parse_u16).ok_or(Fail { what: "bad winsize cols", got: step as i64 })?;
                set_winsize(child.master, rows, cols)?;
                say!("ok step {step} winsize {rows}x{cols}");
            } else if let Some(arg) = line.strip_prefix(b"wait_exit ") {
                let want = parse_u16(arg).ok_or(Fail { what: "bad wait_exit code", got: step as i64 })? as i32;
                drain_until_eof(child.master, &mut screen)?;
                let status = wait_child(child.pid)?;
                waited = true;
                if status & 0x7f != 0 {
                    return Err(Fail { what: "child killed by signal", got: (status & 0x7f) as i64 });
                }
                let code = (status >> 8) & 0xff;
                eq("child exit code", code as i64, want as i64)?;
                say!("ok step {step} wait_exit {code}");
            } else {
                return Err(Fail { what: "unknown script command", got: step as i64 });
            }
        }
        if !waited {
            drain_until_eof(child.master, &mut screen)?;
            let status = wait_child(child.pid)?;
            waited = true;
            if status != 0 {
                return Err(Fail { what: "child exit status", got: status as i64 });
            }
        }
        Ok(())
    })();

    let _ = close(child.master);
    if !waited {
        let _ = wait_child(child.pid);
    }
    result
}

struct PendingBuf {
    buf: [u8; 2048],
    pos: usize,
    len: usize,
}

impl PendingBuf {
    fn new() -> Self {
        Self { buf: [0; 2048], pos: 0, len: 0 }
    }

    fn is_empty(&self) -> bool {
        self.pos == self.len
    }

    fn pending(&self) -> &[u8] {
        &self.buf[self.pos..self.len]
    }

    fn set_len(&mut self, n: usize) {
        self.pos = 0;
        self.len = n;
    }

    fn advance(&mut self, n: usize) {
        self.pos += n;
        if self.pos >= self.len {
            self.pos = 0;
            self.len = 0;
        }
    }
}

struct RelaySetup {
    old_sigpipe: [u64; 4],
    ignored_sigpipe: bool,
    winch_pipe: Option<[i32; 2]>,
    old_sigwinch: [u64; 4],
    installed_sigwinch: bool,
    saved_termios: [u8; 36],
    have_tty: bool,
    saved_stdout_fl: Option<u64>,
}

/// Undo relay setup in reverse dependency order: stdout flags and stdin
/// termios first, then restore `SIGWINCH` before clearing `WINCH_PIPE_W` and
/// closing `winch_pipe` (so the handler cannot write to a closed/reused fd),
/// and finally restore `SIGPIPE`. Returns `false` if any restore failed.
fn undo_relay_setup(setup: &RelaySetup) -> bool {
    let mut restored = true;
    if let Some(fl) = setup.saved_stdout_fl {
        let r = sys(SYS_FCNTL, [1, F_SETFL, fl, 0, 0, 0]);
        if r < 0 {
            say_err!("ptyrun: restore stdout F_SETFL failed (got {r})");
            restored = false;
        }
    }
    if setup.have_tty {
        let r = ioctl(0, TCSETS, setup.saved_termios.as_ptr() as u64);
        if r < 0 {
            say_err!("ptyrun: restore stdin TCSETS failed (got {r})");
            restored = false;
        }
    }
    if setup.installed_sigwinch {
        let r = restore_sigaction(SIGWINCH, &setup.old_sigwinch);
        if r < 0 {
            say_err!("ptyrun: restore SIGWINCH failed (got {r})");
            restored = false;
        }
    }
    WINCH_PIPE_W.store(-1, Ordering::Release);
    if let Some([r, w]) = setup.winch_pipe {
        let _ = close(r);
        let _ = close(w);
    }
    if setup.ignored_sigpipe {
        let r = restore_sigaction(SIGPIPE, &setup.old_sigpipe);
        if r < 0 {
            say_err!("ptyrun: restore SIGPIPE failed (got {r})");
            restored = false;
        }
    }
    restored
}

/// Interactive relay mode: copy stdin's window size onto the PTY, forward
/// `SIGWINCH` resizes from stdin (`TIOCGWINSZ`) to the PTY master
/// (`TIOCSWINSZ`), switch stdin to raw mode if it is a tty, set `O_NONBLOCK`
/// on stdout and ignore `SIGPIPE` in the parent (resetting `SIGPIPE` to
/// `SIG_DFL` in the child before `execve`), and relay stdin/stdout <-> PTY
/// master in a single `poll` loop so neither master nor stdout writes can
/// block the relay. Restores stdout's flags, stdin's termios, `SIGWINCH`, and
/// `SIGPIPE` on exit.
///
/// # Safety
/// `argv[cmd_idx..]` must be a valid NULL-terminated array of C strings.
unsafe fn run_relay(argv: *const *const u8, cmd_idx: usize) -> Result<c_int, Fail> {
    let mut setup = RelaySetup {
        old_sigpipe: [0; 4],
        ignored_sigpipe: false,
        winch_pipe: None,
        old_sigwinch: [0; 4],
        installed_sigwinch: false,
        saved_termios: [0; 36],
        have_tty: false,
        saved_stdout_fl: None,
    };

    let sigpipe_r = set_sigpipe(SIG_IGN, Some(&mut setup.old_sigpipe));
    setup.ignored_sigpipe = sigpipe_r == 0;
    if !setup.ignored_sigpipe {
        say_err!("ptyrun: rt_sigaction SIGPIPE failed (got {sigpipe_r})");
    }

    let winch_pipe = match pipe(O_NONBLOCK | O_CLOEXEC) {
        Ok(p) => p,
        Err(e) => {
            let _ = undo_relay_setup(&setup);
            return Err(e);
        }
    };
    setup.winch_pipe = Some(winch_pipe);
    WINCH_PIPE_W.store(winch_pipe[1], Ordering::Release);
    let winch_r = set_sigwinch(&mut setup.old_sigwinch);
    setup.installed_sigwinch = winch_r == 0;
    if !setup.installed_sigwinch {
        say_err!("ptyrun: rt_sigaction SIGWINCH failed (got {winch_r}); resize forwarding disabled");
    }

    // Read stdin's window size after installing the SIGWINCH handler so no
    // resize is lost between startup and the relay poll loop.
    let (rows, cols) = match read_stdin_winsize() {
        Some(ws) => (ws[0], ws[1]),
        None => (DEFAULT_ROWS, DEFAULT_COLS),
    };

    setup.have_tty = ioctl(0, TCGETS, setup.saved_termios.as_mut_ptr() as u64) == 0;
    if setup.have_tty {
        let mut raw = setup.saved_termios;
        raw[..8].fill(0);
        raw[12..16].fill(0);
        raw[17 + 6] = 1; // VMIN = 1
        raw[17 + 5] = 0; // VTIME = 0
        let r = ioctl(0, TCSETS, raw.as_ptr() as u64);
        if r < 0 {
            say_err!("ptyrun: raw TCSETS failed (got {r})");
        }
    }

    // Set O_NONBLOCK on stdout before spawning the child (which rebinds fds
    // 0..=2 to the fresh slave) so stdout is already non-blocking once the
    // child starts, and spawn failure restores it via undo_relay_setup.
    let fl = sys(SYS_FCNTL, [1, F_GETFL, 0, 0, 0, 0]);
    if fl < 0 {
        say_err!("ptyrun: fcntl F_GETFL stdout failed (got {fl})");
    } else if (fl as u64 & O_NONBLOCK) == 0 {
        let r = sys(SYS_FCNTL, [1, F_SETFL, (fl as u64) | O_NONBLOCK, 0, 0, 0]);
        if r == 0 {
            setup.saved_stdout_fl = Some(fl as u64);
        } else {
            say_err!("ptyrun: fcntl F_SETFL O_NONBLOCK stdout failed (got {r})");
        }
    }

    // SAFETY: forwarded from caller's `argv` contract.
    let child = match unsafe { spawn_on_pty(argv, cmd_idx, rows, cols, setup.ignored_sigpipe) } {
        Ok(c) => c,
        Err(e) => {
            let _ = undo_relay_setup(&setup);
            return Err(e);
        }
    };

    let mut stdin_open = true;
    let mut stdin_at_bol = true;
    let mut master_open = true;
    let mut to_master = PendingBuf::new();
    let mut to_stdout = PendingBuf::new();
    let mut relay_err: Option<Fail> = None;

    while master_open || !to_stdout.is_empty() {
        let mut master_events = 0i16;
        if master_open && to_stdout.is_empty() {
            master_events |= POLLIN;
        }
        if master_open && !to_master.is_empty() {
            master_events |= POLLOUT;
        }
        let mut pfds = [
            PollFd { fd: if master_events != 0 { child.master } else { -1 }, events: master_events, revents: 0 },
            PollFd {
                fd: if stdin_open && master_open && to_master.is_empty() { 0 } else { -1 },
                events: POLLIN,
                revents: 0,
            },
            PollFd { fd: if !to_stdout.is_empty() { 1 } else { -1 }, events: POLLOUT, revents: 0 },
            PollFd {
                fd: if master_open && setup.installed_sigwinch { winch_pipe[0] } else { -1 },
                events: POLLIN,
                revents: 0,
            },
        ];
        if let Err(err) = poll_fds(&mut pfds, -1) {
            relay_err = Some(err);
            break;
        }

        if master_open && setup.installed_sigwinch && (pfds[3].revents & (POLLIN | POLLERR | POLLHUP | POLLNVAL)) != 0 {
            let mut drain = [0u8; 64];
            let mut drain_err = None;
            loop {
                let n = read_raw(winch_pipe[0], &mut drain);
                if n > 0 || n == EINTR {
                    continue;
                }
                if n < 0 && n != EAGAIN {
                    drain_err = Some(Fail { what: "relay read winch pipe", got: n });
                }
                break;
            }
            if let Some(err) = drain_err {
                relay_err = Some(err);
                break;
            }
            forward_winsize(child.master);
        }

        if !to_stdout.is_empty() && (pfds[2].revents & (POLLOUT | POLLERR | POLLHUP | POLLNVAL)) != 0 {
            let n = write_raw(1, to_stdout.pending());
            if n > 0 {
                to_stdout.advance(n as usize);
            } else if n != EINTR && n != EAGAIN {
                relay_err = Some(Fail { what: "relay write stdout", got: n });
                break;
            }
        }

        if master_open && !to_master.is_empty() && (pfds[0].revents & (POLLOUT | POLLERR | POLLHUP | POLLNVAL)) != 0 {
            let n = write_raw(child.master, to_master.pending());
            if n > 0 {
                to_master.advance(n as usize);
            } else if n == EIO {
                to_master.set_len(0);
            } else if n != EINTR && n != EAGAIN {
                relay_err = Some(Fail { what: "relay write master", got: n });
                break;
            }
        }

        if master_open && to_stdout.is_empty() && (pfds[0].revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL)) != 0 {
            let n = read_raw(child.master, &mut to_stdout.buf);
            if n > 0 {
                to_stdout.set_len(n as usize);
            } else if n == 0 || n == EIO {
                master_open = false;
                to_master.set_len(0);
            } else if n != EINTR && n != EAGAIN {
                relay_err = Some(Fail { what: "relay read master", got: n });
                break;
            }
        }

        if stdin_open
            && master_open
            && to_master.is_empty()
            && (pfds[1].revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL)) != 0
        {
            if (pfds[1].revents & (POLLIN | POLLHUP)) != 0 {
                let n = read_raw(0, &mut to_master.buf);
                if n > 0 {
                    let n = n as usize;
                    stdin_at_bol = to_master.buf[n - 1] == b'\n';
                    to_master.set_len(n);
                } else if n == 0 {
                    stdin_open = false;
                    let mut pty_termios = [0u8; 36];
                    if ioctl(child.master, TCGETS, pty_termios.as_mut_ptr() as u64) == 0 {
                        let lflag =
                            u32::from_ne_bytes([pty_termios[12], pty_termios[13], pty_termios[14], pty_termios[15]]);
                        let veof = pty_termios[17 + VEOF];
                        if (lflag & ICANON) != 0 && veof != 0 {
                            to_master.buf[0] = veof;
                            if stdin_at_bol {
                                to_master.set_len(1);
                            } else {
                                to_master.buf[1] = veof;
                                to_master.set_len(2);
                            }
                        }
                    }
                } else if n != EINTR && n != EAGAIN {
                    relay_err = Some(Fail { what: "relay read stdin", got: n });
                    break;
                }
            } else {
                relay_err = Some(Fail { what: "relay poll stdin", got: pfds[1].revents as i64 });
                break;
            }
        }
    }

    let _ = close(child.master);
    let restore_ok = undo_relay_setup(&setup);
    if let Some(err) = &relay_err {
        say_err!("FAIL ptyrun: {} (got {})", err.what, err.got);
    }
    let status = wait_child(child.pid);
    if relay_err.is_some() {
        match status {
            Ok(st) => say_err!("ptyrun: child wait status {st}"),
            Err(w) => say_err!("ptyrun: {} (got {})", w.what, w.got),
        }
        return Ok(1);
    }
    let status = status?;
    let code = if status & 0x7f != 0 { 128 + (status & 0x7f) } else { (status >> 8) & 0xff };
    if !restore_ok && code == 0 { Ok(1) } else { Ok(code) }
}

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const u8) -> c_int {
    if argc < 2 {
        say_err!("{USAGE}");
        return USAGE_EXIT;
    }
    // SAFETY: musl passes the kernel's `argc` valid NUL-terminated C strings.
    let arg1 = unsafe { common::arg(*argv.add(1)) };
    if arg1 == b"--script" {
        if argc < 5 {
            say_err!("{USAGE}");
            return USAGE_EXIT;
        }
        // SAFETY: `argc >= 5`, so indices 2, 3 and 4..argc are valid C strings.
        let (script_path, sep) = unsafe { (*argv.add(2), common::arg(*argv.add(3))) };
        if sep != b"--" {
            say_err!("{USAGE}");
            return USAGE_EXIT;
        }
        // SAFETY: `script_path` is `argv[2]` and `argv[4..]` is NULL-terminated.
        match unsafe { run_script(script_path, argv, 4) } {
            Ok(()) => 0,
            Err(Fail { what, got }) => {
                say_err!("FAIL ptyrun: {what} (got {got})");
                1
            }
        }
    } else {
        // SAFETY: `argc >= 2`, so `argv[1..]` is a valid NULL-terminated array.
        match unsafe { run_relay(argv, 1) } {
            Ok(code) => code,
            Err(Fail { what, got }) => {
                say_err!("FAIL ptyrun: {what} (got {got})");
                1
            }
        }
    }
}
