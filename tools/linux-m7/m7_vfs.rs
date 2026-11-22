//! `m7_vfs`: the POSIX VFS exit scenarios' ring-3 program (#254,
//! `tests/scenarios/test_fs.py`), a `#![no_std]` dynamically linked musl
//! x86-64 executable built as `m7_test` is.
//!
//! musl starts it (`Scrt1.o`, `__libc_start_main`) and ends it (`_exit`);
//! every check calls the kernel itself (`syscall`, by number), so each
//! claim is about the syscall it names (`stat` 4, `newfstatat` 262, `statx`
//! 332, ...) and its raw `-errno`, not a libc wrapper's choice.
//!
//!   m7_vfs posix <ext4 dir> <tmpfs dir>
//!   m7_vfs locks <dir>
//!   m7_vfs lock-hold <file>
//!   m7_vfs lock-probe <file> <holder pid> <probe>...
//!
//! `posix` and `locks` make their directories (which must not exist), print
//! `ok <check>` for each check passed, remove everything they made and exit
//! 0; the first failed check prints `FAIL <check>: <what> (got <value>)`
//! and exits with the check's number (from 1).
//!
//! `lock-hold` takes POSIX record locks on `<file>` one step per byte read
//! from its stdin (a key on the virtual terminal it runs on), printing
//! `hold: <step>` after each, and exits still holding one; `lock-probe`, a second process meanwhile,
//! checks what `F_GETLK` reports of them and that `F_SETLK` conflicts. A
//! probe is `<t><start>+<len>=<expect>`: a request of type `<t>` (`r` or
//! `w`) for that range, expecting `unlck` or the holder's lock
//! `<t><start>+<len>` (its pid the given one), with `F_SETLK` refused
//! `EAGAIN` (or taken and given back for `unlck`).

#![no_std]
#![no_main]

use core::arch::asm;
use core::ffi::c_int;
use core::fmt::{self, Write as _};

// SAFETY: musl C ABI symbols resolved at runtime by `/lib/ld-musl-x86_64.so.1`
// via `libc.musl-x86_64.so.1`.
unsafe extern "C" {
    fn _exit(status: c_int) -> !;
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    // SAFETY: `_exit` terminates the process immediately without unwinding.
    unsafe { _exit(127) }
}

/// libcore is built to unwind, so its objects name this symbol; with
/// `panic=abort` nothing ever calls it.
#[unsafe(no_mangle)]
extern "C" fn rust_eh_personality() {}

// -- syscalls ------------------------------------------------------------------

mod nr {
    pub const READ: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const OPEN: u64 = 2;
    pub const CLOSE: u64 = 3;
    pub const STAT: u64 = 4;
    pub const FSTAT: u64 = 5;
    pub const LSTAT: u64 = 6;
    pub const LSEEK: u64 = 8;
    pub const IOCTL: u64 = 16;
    pub const PREAD64: u64 = 17;
    pub const PWRITE64: u64 = 18;
    pub const GETPID: u64 = 39;
    pub const FCNTL: u64 = 72;
    pub const FLOCK: u64 = 73;
    pub const GETCWD: u64 = 79;
    pub const CHDIR: u64 = 80;
    pub const FCHDIR: u64 = 81;
    pub const RENAME: u64 = 82;
    pub const MKDIR: u64 = 83;
    pub const RMDIR: u64 = 84;
    pub const LINK: u64 = 86;
    pub const UNLINK: u64 = 87;
    pub const SYMLINK: u64 = 88;
    pub const READLINK: u64 = 89;
    pub const CHMOD: u64 = 90;
    pub const FCHMOD: u64 = 91;
    pub const CHOWN: u64 = 92;
    pub const FCHOWN: u64 = 93;
    pub const LCHOWN: u64 = 94;
    pub const UMASK: u64 = 95;
    pub const GETDENTS64: u64 = 217;
    pub const OPENAT: u64 = 257;
    pub const FCHOWNAT: u64 = 260;
    pub const NEWFSTATAT: u64 = 262;
    pub const UNLINKAT: u64 = 263;
    pub const RENAMEAT: u64 = 264;
    pub const LINKAT: u64 = 265;
    pub const READLINKAT: u64 = 267;
    pub const FCHMODAT: u64 = 268;
    pub const UTIMENSAT: u64 = 280;
    pub const RENAMEAT2: u64 = 316;
    pub const STATX: u64 = 332;
}

const O_RDONLY: u64 = 0;
const O_WRONLY: u64 = 1;
const O_RDWR: u64 = 2;
const O_CREAT: u64 = 0o100;
const O_EXCL: u64 = 0o200;
const O_TRUNC: u64 = 0o1000;
const O_APPEND: u64 = 0o2000;
const O_DIRECTORY: u64 = 0o200000;
const O_NOFOLLOW: u64 = 0o400000;
const O_PATH: u64 = 0o10000000;

const AT_FDCWD: i64 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_REMOVEDIR: u64 = 0x200;
const AT_SYMLINK_FOLLOW: u64 = 0x400;
const AT_EMPTY_PATH: u64 = 0x1000;

const EPERM: i64 = 1;
const ENOENT: i64 = 2;
const EBADF: i64 = 9;
const EAGAIN: i64 = 11;
const EACCES: i64 = 13;
const EEXIST: i64 = 17;
const EXDEV: i64 = 18;
const ENOTDIR: i64 = 20;
const EISDIR: i64 = 21;
const EINVAL: i64 = 22;
const ENOTTY: i64 = 25;
const ERANGE: i64 = 34;
const ENOTEMPTY: i64 = 39;
const ELOOP: i64 = 40;

const SEEK_SET: u64 = 0;
const SEEK_CUR: u64 = 1;
const TCGETS: u64 = 0x5401;
const TIOCGWINSZ: u64 = 0x5413;

const F_DUPFD: u64 = 0;
const F_GETLK: u64 = 5;
const F_SETLK: u64 = 6;
const F_RDLCK: i16 = 0;
const F_WRLCK: i16 = 1;
const F_UNLCK: i16 = 2;

const LOCK_SH: u64 = 1;
const LOCK_EX: u64 = 2;
const LOCK_NB: u64 = 4;
const LOCK_UN: u64 = 8;

const RENAME_NOREPLACE: u64 = 1;
const RENAME_EXCHANGE: u64 = 2;

const UTIME_NOW: i64 = (1 << 30) - 1;
const UTIME_OMIT: i64 = (1 << 30) - 2;

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;

const STATX_BASIC_STATS: u64 = 0x7ff;

/// A raw `syscall` with up to six arguments: the kernel's return value
/// (`-errno` on failure).
fn sys(n: u64, a: [u64; 6]) -> i64 {
    let ret: i64;
    // SAFETY: the Linux x86-64 syscall convention: the number in rax, the
    // arguments in rdi, rsi, rdx, r10, r8, r9; rcx and r11 clobbered. Every
    // pointer passed points at memory this program owns for the call.
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

fn s0(n: u64) -> i64 {
    sys(n, [0; 6])
}
fn s1(n: u64, a: u64) -> i64 {
    sys(n, [a, 0, 0, 0, 0, 0])
}
fn s2(n: u64, a: u64, b: u64) -> i64 {
    sys(n, [a, b, 0, 0, 0, 0])
}
fn s3(n: u64, a: u64, b: u64, c: u64) -> i64 {
    sys(n, [a, b, c, 0, 0, 0])
}
fn s4(n: u64, a: u64, b: u64, c: u64, d: u64) -> i64 {
    sys(n, [a, b, c, d, 0, 0])
}
fn s5(n: u64, a: u64, b: u64, c: u64, d: u64, e: u64) -> i64 {
    sys(n, [a, b, c, d, e, 0])
}

fn ptr<T: ?Sized>(x: &T) -> u64 {
    x as *const T as *const u8 as u64
}
fn mptr<T: ?Sized>(x: &mut T) -> u64 {
    x as *mut T as *mut u8 as u64
}

// -- output --------------------------------------------------------------------

/// A line built in a fixed buffer, then written to stdout in one `write`.
struct Line {
    buf: [u8; 512],
    len: usize,
}

impl Line {
    fn new() -> Self {
        Self { buf: [0; 512], len: 0 }
    }
    fn send(&mut self) {
        let mut off = 0;
        while off < self.len {
            let n = s3(nr::WRITE, 1, ptr(&self.buf[off..self.len]), (self.len - off) as u64);
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

macro_rules! say {
    ($($arg:tt)*) => {{
        let mut line = Line::new();
        let _ = writeln!(line, $($arg)*);
        line.send();
    }};
}

/// Bytes as text for a message (lossy: one `?` per byte that isn't ASCII).
struct Show<'a>(&'a [u8]);

impl fmt::Display for Show<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in self.0 {
            f.write_char(if (0x20..0x7f).contains(&b) { b as char } else { '?' })?;
        }
        Ok(())
    }
}

// -- paths -----------------------------------------------------------------------

/// A NUL-terminated path joined from parts.
struct P {
    buf: [u8; 256],
    len: usize,
}

impl P {
    fn new(parts: &[&[u8]]) -> Self {
        let mut p = Self { buf: [0; 256], len: 0 };
        for part in parts {
            let n = part.len().min(255 - p.len);
            p.buf[p.len..p.len + n].copy_from_slice(&part[..n]);
            p.len += n;
        }
        p
    }
    fn at(&self) -> u64 {
        ptr(&self.buf[..])
    }
    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// `dir/name`, NUL-terminated.
fn j(dir: &[u8], name: &[u8]) -> P {
    P::new(&[dir, b"/", name])
}

// -- checks ------------------------------------------------------------------------

/// A failed expectation: what was expected, and what came instead.
struct Fail {
    what: &'static str,
    got: i64,
}

type R = Result<(), Fail>;

/// `got == want`, or a [`Fail`] saying `what`.
fn eq(what: &'static str, got: i64, want: i64) -> R {
    if got == want { Ok(()) } else { Err(Fail { what, got }) }
}

/// `cond`, or a [`Fail`] saying `what` with `got`.
fn ensure(what: &'static str, cond: bool, got: i64) -> R {
    if cond { Ok(()) } else { Err(Fail { what, got }) }
}

/// A syscall that returns a descriptor or a count: its value, or a [`Fail`].
fn ok(what: &'static str, ret: i64) -> Result<i64, Fail> {
    if ret >= 0 { Ok(ret) } else { Err(Fail { what, got: ret }) }
}

/// Run the checks in order, printing `ok <name>` for each that passes;
/// the first that fails prints why and ends the program with its number.
fn run_checks(checks: &[(&str, &dyn Fn() -> R)]) -> c_int {
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

// -- file helpers --------------------------------------------------------------------

fn open(path: &P, flags: u64, mode: u64) -> i64 {
    s3(nr::OPEN, path.at(), flags, mode)
}

fn close(fd: i64) -> i64 {
    s1(nr::CLOSE, fd as u64)
}

/// Create (or truncate) `path` holding `data`.
fn put(path: &P, data: &[u8]) -> R {
    let fd = ok("open O_CREAT|O_TRUNC", open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o644))?;
    let n = s3(nr::WRITE, fd as u64, ptr(data), data.len() as u64);
    close(fd);
    eq("write whole", n, data.len() as i64)
}

/// `path`'s first bytes equal `want` and it ends there.
fn holds(path: &P, want: &[u8]) -> R {
    let fd = ok("open to read", open(path, O_RDONLY, 0))?;
    let r = reads(fd, want);
    close(fd);
    r
}

/// Reading `fd` from its offset gives exactly `want`, then the end.
fn reads(fd: i64, want: &[u8]) -> R {
    let mut buf = [0u8; 128];
    let n = ok("read", s3(nr::READ, fd as u64, mptr(&mut buf[..]), buf.len() as u64))?;
    eq("read length", n, want.len() as i64)?;
    ensure("read contents", &buf[..n as usize] == want, n)
}

/// `pread64(fd, len, at)` gives exactly `want`.
fn preads(fd: i64, at: u64, want: &[u8]) -> R {
    let mut buf = [0u8; 128];
    let n = ok("pread64", s4(nr::PREAD64, fd as u64, mptr(&mut buf[..]), want.len() as u64, at))?;
    eq("pread64 length", n, want.len() as i64)?;
    ensure("pread64 contents", &buf[..n as usize] == want, n)
}

/// Linux x86-64's `struct stat` (144 bytes), the fields read here.
#[derive(Clone, Copy)]
struct Stat {
    raw: [u8; 144],
}

impl Stat {
    fn new() -> Self {
        Self { raw: [0; 144] }
    }
    fn u64_at(&self, o: usize) -> u64 {
        u64::from_le_bytes(self.raw[o..o + 8].try_into().unwrap())
    }
    fn u32_at(&self, o: usize) -> u32 {
        u32::from_le_bytes(self.raw[o..o + 4].try_into().unwrap())
    }
    fn dev(&self) -> u64 {
        self.u64_at(0)
    }
    fn ino(&self) -> u64 {
        self.u64_at(8)
    }
    fn nlink(&self) -> u64 {
        self.u64_at(16)
    }
    fn mode(&self) -> u32 {
        self.u32_at(24)
    }
    fn kind(&self) -> u32 {
        self.mode() & S_IFMT
    }
    fn perm(&self) -> u32 {
        self.mode() & 0o7777
    }
    fn uid(&self) -> u32 {
        self.u32_at(28)
    }
    fn gid(&self) -> u32 {
        self.u32_at(32)
    }
    fn size(&self) -> i64 {
        self.u64_at(48) as i64
    }
    fn atime(&self) -> (i64, i64) {
        (self.u64_at(72) as i64, self.u64_at(80) as i64)
    }
    fn mtime(&self) -> (i64, i64) {
        (self.u64_at(88) as i64, self.u64_at(96) as i64)
    }
}

fn stat(path: &P) -> Result<Stat, i64> {
    let mut st = Stat::new();
    match s2(nr::STAT, path.at(), mptr(&mut st.raw)) {
        0 => Ok(st),
        e => Err(e),
    }
}

fn lstat(path: &P) -> Result<Stat, i64> {
    let mut st = Stat::new();
    match s2(nr::LSTAT, path.at(), mptr(&mut st.raw)) {
        0 => Ok(st),
        e => Err(e),
    }
}

fn fstat(fd: i64) -> Result<Stat, i64> {
    let mut st = Stat::new();
    match s2(nr::FSTAT, fd as u64, mptr(&mut st.raw)) {
        0 => Ok(st),
        e => Err(e),
    }
}

fn fstatat(dirfd: i64, path: u64, flags: u64) -> Result<Stat, i64> {
    let mut st = Stat::new();
    match s4(nr::NEWFSTATAT, dirfd as u64, path, mptr(&mut st.raw), flags) {
        0 => Ok(st),
        e => Err(e),
    }
}

/// A `stat`-family result, or a [`Fail`] naming the call.
fn st(what: &'static str, r: Result<Stat, i64>) -> Result<Stat, Fail> {
    r.map_err(|got| Fail { what, got })
}

/// The `-errno` a `stat`-family call failed with (0 if it succeeded).
fn st_err(r: Result<Stat, i64>) -> i64 {
    r.err().unwrap_or(0)
}

/// `getcwd` into `buf`: the path (without its NUL), or the `-errno`.
fn getcwd(buf: &mut [u8; 256]) -> Result<&[u8], i64> {
    let n = s2(nr::GETCWD, mptr(&mut buf[..]), buf.len() as u64);
    if n < 0 {
        return Err(n);
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(0);
    Ok(&buf[..len])
}

fn cwd_is(what: &'static str, want: &[u8]) -> R {
    let mut buf = [0u8; 256];
    match getcwd(&mut buf) {
        Ok(cwd) => ensure(what, cwd == want, cwd.len() as i64),
        Err(e) => Err(Fail { what, got: e }),
    }
}

/// One `linux_dirent64` entry's name and type.
struct Entry {
    name: [u8; 64],
    len: usize,
    kind: u8,
    ino: u64,
}

/// The entries `getdents64` lists on `fd`, `chunk` bytes a call, into
/// `out`: how many (or a [`Fail`]).
fn list(fd: i64, chunk: usize, out: &mut [Entry]) -> Result<usize, Fail> {
    let mut buf = [0u8; 1024];
    let mut count = 0;
    loop {
        let n = ok("getdents64", s3(nr::GETDENTS64, fd as u64, mptr(&mut buf[..]), chunk as u64))?;
        if n == 0 {
            return Ok(count);
        }
        let mut off = 0usize;
        while off < n as usize {
            let reclen = u16::from_le_bytes([buf[off + 16], buf[off + 17]]) as usize;
            ensure("a dirent's reclen", reclen >= 20 && off + reclen <= n as usize, reclen as i64)?;
            let ino = u64::from_le_bytes(buf[off..off + 8].try_into().unwrap());
            let kind = buf[off + 18];
            let name = &buf[off + 19..off + reclen];
            let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
            ensure("room for every entry", count < out.len(), count as i64)?;
            let e = &mut out[count];
            let keep = len.min(64);
            e.name[..keep].copy_from_slice(&name[..keep]);
            e.len = keep;
            e.kind = kind;
            e.ino = ino;
            count += 1;
            off += reclen;
        }
    }
}

fn no_entries<const N: usize>() -> [Entry; N] {
    core::array::from_fn(|_| Entry { name: [0; 64], len: 0, kind: 0, ino: 0 })
}

/// Remove `path` and everything under it (depth-first, through
/// `getdents64` and `unlinkat`); a missing path is fine.
fn rm_tree(path: &P) -> R {
    let st = match lstat(path) {
        Ok(st) => st,
        Err(e) if e == -ENOENT => return Ok(()),
        Err(e) => return Err(Fail { what: "lstat to remove", got: e }),
    };
    if st.kind() != S_IFDIR {
        return eq("unlinkat", s3(nr::UNLINKAT, AT_FDCWD as u64, path.at(), 0), 0);
    }
    let fd = ok("open a directory to remove", open(path, O_RDONLY | O_DIRECTORY, 0))?;
    let mut entries = no_entries::<96>();
    let listed = list(fd, 1024, &mut entries);
    close(fd);
    for e in &entries[..listed?] {
        let name = &e.name[..e.len];
        if name == b"." || name == b".." {
            continue;
        }
        rm_tree(&j(path.bytes(), name))?;
    }
    eq("unlinkat AT_REMOVEDIR", s3(nr::UNLINKAT, AT_FDCWD as u64, path.at(), AT_REMOVEDIR), 0)
}

// -- mode posix ------------------------------------------------------------------------

fn posix(ext: &[u8], tmp: &[u8]) -> c_int {
    let checks: [(&str, &dyn Fn() -> R); 15] = [
        ("setup", &|| setup(ext, tmp)),
        ("symlinks-and-eloop", &|| symlinks(ext)),
        ("trailing-slash-nofollow", &|| trailing_slash(ext)),
        ("o-path", &|| o_path(ext)),
        ("hard-links", &|| hard_links(ext)),
        ("unlink-while-open-ext4", &|| unlink_open(ext)),
        ("unlink-while-open-tmpfs", &|| unlink_open(tmp)),
        ("rmdir-while-open", &|| rmdir_open(ext)),
        ("getdents64", &|| getdents(ext).and_then(|()| getdents(tmp))),
        ("pread64-pwrite64", &|| positional(ext).and_then(|()| positional(tmp))),
        ("stat-lstat-newfstatat-statx", &|| stat_family(ext, tmp)),
        ("chmod-chown-umask-utimensat", &|| metadata(ext)),
        ("chdir-fchdir-getcwd", &|| cwd(ext, tmp)),
        ("exdev-and-enotty", &|| exdev_enotty(ext, tmp)),
        ("cleanup", &|| cleanup(&[ext, tmp])),
    ];
    run_checks(&checks)
}

fn setup(ext: &[u8], tmp: &[u8]) -> R {
    for dir in [ext, tmp] {
        eq("mkdir the test directory", s2(nr::MKDIR, P::new(&[dir]).at(), 0o755), 0)?;
    }
    let ext_st = st("stat the ext4 directory", stat(&P::new(&[ext])))?;
    let tmp_st = st("stat the tmpfs directory", stat(&P::new(&[tmp])))?;
    ensure("the directories on two filesystems", ext_st.dev() != tmp_st.dev(), ext_st.dev() as i64)
}

fn cleanup(dirs: &[&[u8]]) -> R {
    for dir in dirs {
        let p = P::new(&[dir]);
        rm_tree(&p)?;
        eq("gone after cleanup", st_err(lstat(&p)), -ENOENT)?;
    }
    Ok(())
}

fn symlinks(d: &[u8]) -> R {
    put(&j(d, b"f"), b"hello")?;
    eq("symlink", s2(nr::SYMLINK, P::new(&[b"f"]).at(), j(d, b"l").at()), 0)?;
    let mut buf = [0u8; 64];
    let n = s3(nr::READLINK, j(d, b"l").at(), mptr(&mut buf[..]), buf.len() as u64);
    eq("readlink's length", n, 1)?;
    ensure("readlink's target", buf[0] == b'f', buf[0] as i64)?;
    holds(&j(d, b"l"), b"hello")?;
    // A loop of two links: ELOOP for open and stat.
    eq("symlink loop1", s2(nr::SYMLINK, P::new(&[b"loop2"]).at(), j(d, b"loop1").at()), 0)?;
    eq("symlink loop2", s2(nr::SYMLINK, P::new(&[b"loop1"]).at(), j(d, b"loop2").at()), 0)?;
    eq("open a loop", open(&j(d, b"loop1"), O_RDONLY, 0), -ELOOP)?;
    eq("stat a loop", st_err(stat(&j(d, b"loop1"))), -ELOOP)?;
    // A chain of links: 40 followed resolve (Linux's MAXSYMLINKS), 41 don't.
    let mut prev = P::new(&[b"f"]);
    for i in 1..=41u8 {
        let name = P::new(&[b"c", &[b'0' + i / 10, b'0' + i % 10]]);
        eq("symlink a chain link", s2(nr::SYMLINK, prev.at(), j(d, name.bytes()).at()), 0)?;
        prev = name;
    }
    holds(&j(d, b"c40"), b"hello")?;
    eq("41 links in a chain", open(&j(d, b"c41"), O_RDONLY, 0), -ELOOP)?;
    // lstat doesn't follow: the loop's link itself.
    eq("lstat a loop's link", st("lstat loop1", lstat(&j(d, b"loop1")))?.kind() as i64, S_IFLNK as i64)?;
    // O_NOFOLLOW on a final symlink: ELOOP; through a symlink in the middle
    // of a path, followed.
    eq("O_NOFOLLOW on a symlink", open(&j(d, b"l"), O_RDONLY | O_NOFOLLOW, 0), -ELOOP)?;
    eq("symlink to the directory", s2(nr::SYMLINK, P::new(&[d]).at(), j(d, b"self").at()), 0)?;
    let fd = ok("O_NOFOLLOW through a middle symlink", open(&P::new(&[d, b"/self/f"]), O_RDONLY | O_NOFOLLOW, 0))?;
    let r = reads(fd, b"hello");
    close(fd);
    r?;
    // A dangling link: ENOENT through it, but readlink still works.
    eq("symlink dangling", s2(nr::SYMLINK, P::new(&[b"nowhere"]).at(), j(d, b"dangle").at()), 0)?;
    eq("open a dangling link", open(&j(d, b"dangle"), O_RDONLY, 0), -ENOENT)?;
    eq("readlink on a regular file", s3(nr::READLINK, j(d, b"f").at(), mptr(&mut buf[..]), 64), -EINVAL)?;
    // O_CREAT|O_EXCL on a dangling link: EEXIST (never creates through it).
    eq("O_CREAT|O_EXCL on a dangling link", open(&j(d, b"dangle"), O_CREAT | O_EXCL | O_WRONLY, 0o644), -EEXIST)?;
    eq("nothing made through it", st_err(lstat(&j(d, b"nowhere"))), -ENOENT)
}

fn trailing_slash(d: &[u8]) -> R {
    eq("mkdir", s2(nr::MKDIR, j(d, b"td").at(), 0o755), 0)?;
    eq("symlink to a directory", s2(nr::SYMLINK, P::new(&[b"td"]).at(), j(d, b"ltd").at()), 0)?;
    // `f` and `l` (-> f) are from symlinks-and-eloop.
    eq("a file with a trailing slash", open(&P::new(&[d, b"/f/"]), O_RDONLY, 0), -ENOTDIR)?;
    eq(
        "a link to a file, trailing slash, O_NOFOLLOW",
        open(&P::new(&[d, b"/l/"]), O_RDONLY | O_NOFOLLOW, 0),
        -ENOTDIR,
    )?;
    // A trailing slash follows the final symlink even under O_NOFOLLOW.
    let fd = ok(
        "a link to a directory, trailing slash, O_NOFOLLOW",
        open(&P::new(&[d, b"/ltd/"]), O_RDONLY | O_NOFOLLOW, 0),
    )?;
    let kind = fstat(fd).map(|s| s.kind() as i64);
    close(fd);
    eq("which opens the directory", kind.unwrap_or(-1), S_IFDIR as i64)?;
    eq("without the slash, O_NOFOLLOW", open(&j(d, b"ltd"), O_RDONLY | O_NOFOLLOW, 0), -ELOOP)?;
    // lstat with a trailing slash follows too; without it, the link.
    eq("lstat with a trailing slash", st("lstat ltd/", lstat(&P::new(&[d, b"/ltd/"])))?.kind() as i64, S_IFDIR as i64)?;
    eq("lstat without it", st("lstat ltd", lstat(&j(d, b"ltd")))?.kind() as i64, S_IFLNK as i64)?;
    // O_CREAT on a free name with a trailing slash: EISDIR, nothing made.
    eq("O_CREAT name/", open(&P::new(&[d, b"/newf/"]), O_CREAT | O_WRONLY, 0o644), -EISDIR)?;
    eq("nothing made", st_err(lstat(&j(d, b"newf"))), -ENOENT)?;
    // O_DIRECTORY on a file.
    eq("O_DIRECTORY on a file", open(&j(d, b"f"), O_RDONLY | O_DIRECTORY, 0), -ENOTDIR)
}

fn o_path(d: &[u8]) -> R {
    let mut buf = [0u8; 16];
    let fd = ok("O_PATH on a file", open(&j(d, b"f"), O_PATH, 0))?;
    let r = (|| {
        eq("read on O_PATH", s3(nr::READ, fd as u64, mptr(&mut buf[..]), 16), -EBADF)?;
        eq("write on O_PATH", s3(nr::WRITE, fd as u64, ptr(&buf[..]), 1), -EBADF)?;
        eq("pread64 on O_PATH", s4(nr::PREAD64, fd as u64, mptr(&mut buf[..]), 16, 0), -EBADF)?;
        eq("ioctl on O_PATH", s3(nr::IOCTL, fd as u64, TCGETS, mptr(&mut buf[..])), -EBADF)?;
        let s = st("fstat on O_PATH", fstat(fd))?;
        eq("its type", s.kind() as i64, S_IFREG as i64)?;
        eq("its size", s.size(), 5)
    })();
    close(fd);
    r?;
    // O_PATH|O_NOFOLLOW on a symlink: the link itself; readlinkat with an
    // empty path reads it.
    let fd = ok("O_PATH|O_NOFOLLOW on a symlink", open(&j(d, b"l"), O_PATH | O_NOFOLLOW, 0))?;
    let r = (|| {
        eq("fstat: the link", st("fstat link", fstat(fd))?.kind() as i64, S_IFLNK as i64)?;
        let mut tbuf = [0u8; 16];
        let n = s4(nr::READLINKAT, fd as u64, ptr(&b"\0"[..]), mptr(&mut tbuf[..]), 16);
        eq("readlinkat(fd, \"\")", n, 1)?;
        ensure("its target", tbuf[0] == b'f', tbuf[0] as i64)
    })();
    close(fd);
    r?;
    // An O_PATH directory as a dirfd.
    let fd = ok("O_PATH on a directory", open(&P::new(&[d]), O_PATH | O_DIRECTORY, 0))?;
    let r = (|| {
        let s = st("newfstatat(opath dirfd, \"f\")", fstatat(fd, ptr(&b"f\0"[..]), 0))?;
        eq("relative to it", s.size(), 5)?;
        let f2 = ok("openat(opath dirfd, \"f\")", s4(nr::OPENAT, fd as u64, ptr(&b"f\0"[..]), O_RDONLY, 0))?;
        let r = reads(f2, b"hello");
        close(f2);
        r
    })();
    close(fd);
    r
}

fn hard_links(d: &[u8]) -> R {
    put(&j(d, b"hf"), b"hello")?;
    eq("link", s2(nr::LINK, j(d, b"hf").at(), j(d, b"hl").at()), 0)?;
    let a = st("stat hf", stat(&j(d, b"hf")))?;
    let b = st("stat hl", stat(&j(d, b"hl")))?;
    eq("the same inode", b.ino() as i64, a.ino() as i64)?;
    eq("two links", a.nlink() as i64, 2)?;
    // Written through one name, read through the other.
    let fd = ok("open hl O_APPEND", open(&j(d, b"hl"), O_WRONLY | O_APPEND, 0))?;
    let n = s3(nr::WRITE, fd as u64, ptr(&b" world"[..]), 6);
    close(fd);
    eq("append", n, 6)?;
    holds(&j(d, b"hf"), b"hello world")?;
    eq("link onto a name in use", s2(nr::LINK, j(d, b"hf").at(), j(d, b"hl").at()), -EEXIST)?;
    eq("link a directory", s2(nr::LINK, j(d, b"td").at(), j(d, b"tdl").at()), -EPERM)?;
    // linkat follows a symlink only with AT_SYMLINK_FOLLOW.
    eq("symlink to hf", s2(nr::SYMLINK, P::new(&[b"hf"]).at(), j(d, b"hs").at()), 0)?;
    let at = AT_FDCWD as u64;
    eq(
        "linkat AT_SYMLINK_FOLLOW",
        s5(nr::LINKAT, at, j(d, b"hs").at(), at, j(d, b"hfollow").at(), AT_SYMLINK_FOLLOW),
        0,
    )?;
    eq("which links the file", st("lstat hfollow", lstat(&j(d, b"hfollow")))?.ino() as i64, a.ino() as i64)?;
    eq("linkat without it", s5(nr::LINKAT, at, j(d, b"hs").at(), at, j(d, b"hlink").at(), 0), 0)?;
    eq("which links the symlink", st("lstat hlink", lstat(&j(d, b"hlink")))?.kind() as i64, S_IFLNK as i64)?;
    eq("three links now", st("stat hf", stat(&j(d, b"hf")))?.nlink() as i64, 3)?;
    // Unlinking one name leaves the others.
    eq("unlink hf", s1(nr::UNLINK, j(d, b"hf").at()), 0)?;
    eq("hf gone", st_err(stat(&j(d, b"hf"))), -ENOENT)?;
    eq("hl's links", st("stat hl", stat(&j(d, b"hl")))?.nlink() as i64, 2)?;
    holds(&j(d, b"hl"), b"hello world")?;
    eq("unlink a directory", s1(nr::UNLINK, j(d, b"td").at()), -EISDIR)?;
    // Renaming a name onto another link of its inode does nothing.
    eq("link hl2", s2(nr::LINK, j(d, b"hl").at(), j(d, b"hl2").at()), 0)?;
    eq("rename onto its own hard link", s2(nr::RENAME, j(d, b"hl").at(), j(d, b"hl2").at()), 0)?;
    eq("both names kept", st("stat hl", stat(&j(d, b"hl")))?.nlink() as i64, 3)?;
    eq("unlink hl2", s1(nr::UNLINK, j(d, b"hl2").at()), 0)
}

fn unlink_open(d: &[u8]) -> R {
    let p = j(d, b"uo");
    put(&p, b"data")?;
    let fd = ok("open uo", open(&p, O_RDWR, 0))?;
    let r = (|| {
        // (Its st_ino isn't compared: ext4 gives a file unlinked while open
        // a synthetic one, ARCHITECTURE section 9's Gate 6.)
        eq("unlink while open", s1(nr::UNLINK, p.at()), 0)?;
        eq("the name gone", st_err(stat(&p)), -ENOENT)?;
        preads(fd, 0, b"data")?;
        let s = st("fstat after unlink", fstat(fd))?;
        eq("no links", s.nlink() as i64, 0)?;
        eq("its size", s.size(), 4)?;
        eq("pwrite64 to the unlinked file", s4(nr::PWRITE64, fd as u64, ptr(&b"more"[..]), 4, 4), 4)?;
        preads(fd, 0, b"datamore")?;
        // A new file under the old name is another inode.
        put(&p, b"new")?;
        preads(fd, 0, b"datamore")?;
        holds(&p, b"new")
    })();
    close(fd);
    r?;
    eq("unlink the new one", s1(nr::UNLINK, p.at()), 0)
}

fn rmdir_open(d: &[u8]) -> R {
    let p = j(d, b"rd");
    eq("mkdir", s2(nr::MKDIR, p.at(), 0o755), 0)?;
    put(&j(p.bytes(), b"x"), b"x")?;
    eq("rmdir a directory with a file", s1(nr::RMDIR, p.at()), -ENOTEMPTY)?;
    eq("unlink its file", s1(nr::UNLINK, j(p.bytes(), b"x").at()), 0)?;
    let fd = ok("open the directory", open(&p, O_RDONLY | O_DIRECTORY, 0))?;
    let r = (|| {
        eq("rmdir while open", s1(nr::RMDIR, p.at()), 0)?;
        eq("the name gone", st_err(stat(&p)), -ENOENT)?;
        let s = st("fstat after rmdir", fstat(fd))?;
        eq("still a directory", s.kind() as i64, S_IFDIR as i64)?;
        eq("no links", s.nlink() as i64, 0)?;
        // A removed directory lists as empty (Linux says ENOENT: a
        // deliberate difference, ARCHITECTURE section 9's `readdir`), and
        // nothing can be made in it.
        let mut buf = [0u8; 256];
        eq("getdents64 on it", s3(nr::GETDENTS64, fd as u64, mptr(&mut buf[..]), 256), 0)?;
        eq("openat O_CREAT in it", s4(nr::OPENAT, fd as u64, ptr(&b"y\0"[..]), O_CREAT | O_WRONLY, 0o644), -ENOENT)?;
        // Its cwd: getcwd says ENOENT, as Linux.
        eq("fchdir into it", s1(nr::FCHDIR, fd as u64), 0)?;
        let mut buf = [0u8; 256];
        let cwd = getcwd(&mut buf).err().unwrap_or(0);
        eq("back to /", s1(nr::CHDIR, ptr(&b"/\0"[..])), 0)?;
        eq("getcwd in a removed directory", cwd, -ENOENT)
    })();
    close(fd);
    r
}

fn getdents(d: &[u8]) -> R {
    let g = j(d, b"g");
    eq("mkdir g", s2(nr::MKDIR, g.at(), 0o755), 0)?;
    // Making a name in a directory updates its mtime.
    let old_times: [i64; 4] = [1_000_000, 0, 1_000_000, 0];
    eq("utimensat g", s4(nr::UTIMENSAT, AT_FDCWD as u64, g.at(), ptr(&old_times), 0), 0)?;
    put(&j(g.bytes(), b"a"), b"a")?;
    let mtime = st("stat g", stat(&g))?.mtime().0;
    ensure("a create updates the directory's mtime", mtime > 1_600_000_000, mtime)?;
    put(&j(g.bytes(), b"bb"), b"b")?;
    eq("mkdir g/sub", s2(nr::MKDIR, j(g.bytes(), b"sub").at(), 0o755), 0)?;
    eq("symlink g/ln", s2(nr::SYMLINK, P::new(&[b"a"]).at(), j(g.bytes(), b"ln").at()), 0)?;
    let want: [(&[u8], u8); 6] =
        [(b".", DT_DIR), (b"..", DT_DIR), (b"a", DT_REG), (b"bb", DT_REG), (b"sub", DT_DIR), (b"ln", DT_LNK)];
    let fd = ok("open g", open(&g, O_RDONLY | O_DIRECTORY, 0))?;
    let r = (|| {
        // A buffer too small for any entry: EINVAL.
        let mut tiny = [0u8; 8];
        eq("getdents64 into 8 bytes", s3(nr::GETDENTS64, fd as u64, mptr(&mut tiny[..]), 8), -EINVAL)?;
        // Listed whole, then again a few entries a call (the cursor going
        // on), after a rewind: each name once, with its type.
        for chunk in [1024usize, 40] {
            eq("rewind", s3(nr::LSEEK, fd as u64, 0, SEEK_SET), 0)?;
            let mut got = no_entries::<16>();
            let n = list(fd, chunk, &mut got)?;
            eq("how many entries", n as i64, want.len() as i64)?;
            for (name, kind) in want {
                let Some(found) = got[..n].iter().find(|e| &e.name[..e.len] == name) else {
                    return Err(Fail { what: "a name listed", got: name.len() as i64 });
                };
                eq("its d_type", found.kind as i64, kind as i64)?;
                // d_ino is the inode lstat sees under that name.
                let ino = st("lstat a listed name", lstat(&j(g.bytes(), name)))?.ino();
                eq("its d_ino", found.ino as i64, ino as i64)?;
                let times = got[..n].iter().filter(|e| &e.name[..e.len] == name).count();
                eq("listed once", times as i64, 1)?;
            }
        }
        Ok(())
    })();
    close(fd);
    r?;
    // A subdirectory counts in its parent's links (its ".."), and moves
    // that link with it: a rename to another parent repoints its "..".
    eq("g's links with one subdirectory", st("stat g", stat(&g))?.nlink() as i64, 3)?;
    let d_links = st("stat the test directory", stat(&P::new(&[d])))?.nlink();
    eq("move g/sub up", s2(nr::RENAME, j(g.bytes(), b"sub").at(), j(d, b"moved").at()), 0)?;
    eq("g's links after", st("stat g", stat(&g))?.nlink() as i64, 2)?;
    eq(
        "the new parent's links",
        st("stat the test directory", stat(&P::new(&[d])))?.nlink() as i64,
        d_links as i64 + 1,
    )?;
    let dd = st("stat moved/..", stat(&P::new(&[d, b"/moved/.."])))?;
    eq(
        "moved/.. is the new parent",
        dd.ino() as i64,
        st("stat the test directory", stat(&P::new(&[d])))?.ino() as i64,
    )?;
    let mfd = ok("open moved", open(&j(d, b"moved"), O_RDONLY | O_DIRECTORY, 0))?;
    let mut got = no_entries::<4>();
    let n = list(mfd, 1024, &mut got);
    close(mfd);
    let n = n?;
    let Some(dotdot) = got[..n].iter().find(|e| &e.name[..e.len] == b"..") else {
        return Err(Fail { what: "moved lists ..", got: n as i64 });
    };
    eq("moved's .. listed as the new parent", dotdot.ino as i64, dd.ino() as i64)?;
    let f = ok("open a file", open(&j(g.bytes(), b"a"), O_RDONLY, 0))?;
    let mut buf = [0u8; 256];
    let n = s3(nr::GETDENTS64, f as u64, mptr(&mut buf[..]), 256);
    close(f);
    eq("getdents64 on a file", n, -ENOTDIR)
}

fn positional(d: &[u8]) -> R {
    let p = j(d, b"pp");
    let fd = ok("open pp", open(&p, O_CREAT | O_RDWR | O_TRUNC, 0o644))?;
    let r = (|| {
        let old_times: [i64; 4] = [1_000_000, 0, 1_000_000, 0];
        eq("utimensat pp", s4(nr::UTIMENSAT, fd as u64, 0, ptr(&old_times), 0), 0)?;
        eq("pwrite64 past the end", s4(nr::PWRITE64, fd as u64, ptr(&b"XYZ"[..]), 3, 5000), 3)?;
        let mtime = st("fstat", fstat(fd))?.mtime().0;
        ensure("pwrite64 updates the mtime", mtime > 1_600_000_000, mtime)?;
        eq("the offset untouched", s3(nr::LSEEK, fd as u64, 0, SEEK_CUR), 0)?;
        eq("the size", st("fstat", fstat(fd))?.size(), 5003)?;
        preads(fd, 5000, b"XYZ")?;
        preads(fd, 4094, &[0, 0, 0, 0])?;
        let mut buf = [0u8; 8];
        eq("pread64 at the end", s4(nr::PREAD64, fd as u64, mptr(&mut buf[..]), 8, 5003), 0)?;
        eq("pread64 at a negative offset", s4(nr::PREAD64, fd as u64, mptr(&mut buf[..]), 8, -1i64 as u64), -EINVAL)?;
        eq("pwrite64 at a negative offset", s4(nr::PWRITE64, fd as u64, ptr(&buf[..]), 1, -1i64 as u64), -EINVAL)?;
        eq("write moves the offset", s3(nr::WRITE, fd as u64, ptr(&b"ab"[..]), 2), 2)?;
        eq("to 2", s3(nr::LSEEK, fd as u64, 0, SEEK_CUR), 2)?;
        preads(fd, 0, b"ab\0")?;
        eq("still 2", s3(nr::LSEEK, fd as u64, 0, SEEK_CUR), 2)
    })();
    close(fd);
    r?;
    let mut buf = [0u8; 8];
    let ro = ok("open pp read-only", open(&p, O_RDONLY, 0))?;
    let n = s4(nr::PWRITE64, ro as u64, ptr(&buf[..]), 1, 0);
    close(ro);
    eq("pwrite64 on a read-only fd", n, -EBADF)?;
    let dfd = ok("open the directory", open(&P::new(&[d]), O_RDONLY | O_DIRECTORY, 0))?;
    let n = s4(nr::PREAD64, dfd as u64, mptr(&mut buf[..]), 8, 0);
    close(dfd);
    eq("pread64 on a directory", n, -EISDIR)
}

/// Linux's `struct statx` (256 bytes), the fields read here.
struct Statx {
    raw: [u8; 256],
}

impl Statx {
    fn u32_at(&self, o: usize) -> u32 {
        u32::from_le_bytes(self.raw[o..o + 4].try_into().unwrap())
    }
    fn u64_at(&self, o: usize) -> u64 {
        u64::from_le_bytes(self.raw[o..o + 8].try_into().unwrap())
    }
    fn mask(&self) -> u32 {
        self.u32_at(0)
    }
    fn nlink(&self) -> u32 {
        self.u32_at(16)
    }
    fn uid(&self) -> u32 {
        self.u32_at(20)
    }
    fn mode(&self) -> u32 {
        u16::from_le_bytes([self.raw[28], self.raw[29]]) as u32
    }
    fn ino(&self) -> u64 {
        self.u64_at(32)
    }
    fn size(&self) -> u64 {
        self.u64_at(40)
    }
    fn mtime_sec(&self) -> i64 {
        self.u64_at(112) as i64
    }
}

fn statx(dirfd: i64, path: u64, flags: u64) -> Result<Statx, Fail> {
    let mut sx = Statx { raw: [0xa5; 256] };
    let r = s5(nr::STATX, dirfd as u64, path, flags, STATX_BASIC_STATS, mptr(&mut sx.raw));
    eq("statx", r, 0)?;
    Ok(sx)
}

fn stat_family(ext: &[u8], tmp: &[u8]) -> R {
    let p = j(ext, b"sf");
    put(&p, b"0123456789")?;
    eq("symlink to sf", s2(nr::SYMLINK, P::new(&[b"sf"]).at(), j(ext, b"sl").at()), 0)?;
    let s = st("stat", stat(&p))?;
    eq("stat: a file", s.kind() as i64, S_IFREG as i64)?;
    eq("stat: its size", s.size(), 10)?;
    eq("stat: one link", s.nlink() as i64, 1)?;
    let fd = ok("open sf", open(&p, O_RDONLY, 0))?;
    let r = (|| {
        let f = st("fstat", fstat(fd))?;
        let at = st("newfstatat", fstatat(AT_FDCWD, p.at(), 0))?;
        let empty = st("newfstatat AT_EMPTY_PATH", fstatat(fd, ptr(&b"\0"[..]), AT_EMPTY_PATH))?;
        for (what, o) in [("fstat agrees", f), ("newfstatat agrees", at), ("AT_EMPTY_PATH agrees", empty)] {
            ensure(what, o.ino() == s.ino() && o.dev() == s.dev(), o.ino() as i64)?;
            ensure(what, o.mode() == s.mode() && o.size() == s.size() && o.nlink() == s.nlink(), o.mode() as i64)?;
            ensure(what, o.mtime() == s.mtime(), o.mtime().0)?;
        }
        eq("newfstatat empty path without AT_EMPTY_PATH", st_err(fstatat(fd, ptr(&b"\0"[..]), 0)), -ENOENT)?;
        let sx = statx(AT_FDCWD, p.at(), 0)?;
        eq(
            "statx's mask has the basic stats",
            (sx.mask() as u64 & STATX_BASIC_STATS) as i64,
            STATX_BASIC_STATS as i64,
        )?;
        ensure("statx agrees with stat", sx.ino() == s.ino() && sx.size() == s.size() as u64, sx.ino() as i64)?;
        ensure("statx's mode and links", sx.mode() == s.mode() && sx.nlink() as u64 == s.nlink(), sx.mode() as i64)?;
        ensure("statx's owner and mtime", sx.uid() == s.uid() && sx.mtime_sec() == s.mtime().0, sx.uid() as i64)?;
        let sx = statx(fd, ptr(&b"\0"[..]), AT_EMPTY_PATH)?;
        eq("statx AT_EMPTY_PATH", sx.ino() as i64, s.ino() as i64)
    })();
    close(fd);
    r?;
    // The symlink: lstat and AT_SYMLINK_NOFOLLOW see it, stat follows it.
    let l = st("lstat", lstat(&j(ext, b"sl")))?;
    eq("lstat: a link", l.kind() as i64, S_IFLNK as i64)?;
    eq("lstat: its size is its target's length", l.size(), 2)?;
    ensure("lstat: its own inode", l.ino() != s.ino(), l.ino() as i64)?;
    eq("stat follows it", st("stat sl", stat(&j(ext, b"sl")))?.ino() as i64, s.ino() as i64)?;
    let n = st("newfstatat NOFOLLOW", fstatat(AT_FDCWD, j(ext, b"sl").at(), AT_SYMLINK_NOFOLLOW))?;
    eq("newfstatat AT_SYMLINK_NOFOLLOW", n.ino() as i64, l.ino() as i64)?;
    eq(
        "newfstatat follows by default",
        st("newfstatat sl", fstatat(AT_FDCWD, j(ext, b"sl").at(), 0))?.ino() as i64,
        s.ino() as i64,
    )?;
    let sx = statx(AT_FDCWD, j(ext, b"sl").at(), AT_SYMLINK_NOFOLLOW)?;
    eq("statx AT_SYMLINK_NOFOLLOW: a link", (sx.mode() & S_IFMT) as i64, S_IFLNK as i64)?;
    // Relative to a directory fd.
    let dfd = ok("open the directory", open(&P::new(&[ext]), O_RDONLY | O_DIRECTORY, 0))?;
    let rel = fstatat(dfd, ptr(&b"sf\0"[..]), 0);
    let relx = statx(dfd, ptr(&b"sf\0"[..]), 0);
    close(dfd);
    eq("newfstatat relative to a dirfd", st("newfstatat dirfd", rel)?.ino() as i64, s.ino() as i64)?;
    eq("statx relative to a dirfd", relx?.ino() as i64, s.ino() as i64)?;
    // Directories, on two filesystems.
    let de = st("stat the ext4 directory", stat(&P::new(&[ext])))?;
    let dt = st("stat the tmpfs directory", stat(&P::new(&[tmp])))?;
    eq("a directory", de.kind() as i64, S_IFDIR as i64)?;
    ensure("two devices", de.dev() != dt.dev(), de.dev() as i64)?;
    eq("stat \"\"", st_err(stat(&P::new(&[b""]))), -ENOENT)?;
    eq("stat a missing name", st_err(stat(&j(ext, b"missing"))), -ENOENT)?;
    eq("stat through a file", st_err(stat(&P::new(&[ext, b"/sf/x"]))), -ENOTDIR)
}

fn metadata(d: &[u8]) -> R {
    // umask: the default 022, then 027 applied to O_CREAT and mkdir.
    let old = s1(nr::UMASK, 0o027);
    eq("umask's default", old, 0o022)?;
    let m = j(d, b"m");
    let fd = ok("open O_CREAT 0666", open(&m, O_CREAT | O_WRONLY, 0o666))?;
    let r = (|| {
        eq("under umask 027", st("stat m", stat(&m))?.perm() as i64, 0o640)?;
        eq("mkdir 0777", s2(nr::MKDIR, j(d, b"md").at(), 0o777), 0)?;
        eq("under umask 027", st("stat md", stat(&j(d, b"md")))?.perm() as i64, 0o750)?;
        eq("umask returns the old mask", s1(nr::UMASK, 0o022), 0o027)?;
        // chmod family, special bits too.
        eq("chmod 4755", s2(nr::CHMOD, m.at(), 0o4755), 0)?;
        eq("chmod's mode", st("stat m", stat(&m))?.perm() as i64, 0o4755)?;
        eq("fchmod 600", s2(nr::FCHMOD, fd as u64, 0o600), 0)?;
        eq("fchmod's mode", st("fstat", fstat(fd))?.perm() as i64, 0o600)?;
        eq("fchmodat 644", s3(nr::FCHMODAT, AT_FDCWD as u64, m.at(), 0o644), 0)?;
        eq("fchmodat's mode", st("stat m", stat(&m))?.perm() as i64, 0o644)?;
        // chmod through a symlink changes the file, not the link.
        eq("symlink ml", s2(nr::SYMLINK, P::new(&[b"m"]).at(), j(d, b"ml").at()), 0)?;
        let link_mode = st("lstat ml", lstat(&j(d, b"ml")))?.perm();
        eq("chmod through the link", s2(nr::CHMOD, j(d, b"ml").at(), 0o600), 0)?;
        eq("the file's mode", st("stat m", stat(&m))?.perm() as i64, 0o600)?;
        eq("the link's mode", st("lstat ml", lstat(&j(d, b"ml")))?.perm() as i64, link_mode as i64)?;
        // chown family: -1 leaves a field; lchown and AT_SYMLINK_NOFOLLOW the link.
        eq("chown 1000:2000", s3(nr::CHOWN, m.at(), 1000, 2000), 0)?;
        let s = st("stat m", stat(&m))?;
        ensure("chown's owner", (s.uid(), s.gid()) == (1000, 2000), s.uid() as i64)?;
        eq("fchown -1:3000", s3(nr::FCHOWN, fd as u64, u32::MAX as u64, 3000), 0)?;
        let s = st("fstat", fstat(fd))?;
        ensure("fchown's owner", (s.uid(), s.gid()) == (1000, 3000), s.gid() as i64)?;
        eq("lchown 5:6", s3(nr::LCHOWN, j(d, b"ml").at(), 5, 6), 0)?;
        let l = st("lstat ml", lstat(&j(d, b"ml")))?;
        ensure("lchown: the link's owner", (l.uid(), l.gid()) == (5, 6), l.uid() as i64)?;
        let s = st("stat m", stat(&m))?;
        ensure("lchown: the file's owner unchanged", (s.uid(), s.gid()) == (1000, 3000), s.uid() as i64)?;
        let at = AT_FDCWD as u64;
        eq("fchownat NOFOLLOW 7:8", s5(nr::FCHOWNAT, at, j(d, b"ml").at(), 7, 8, AT_SYMLINK_NOFOLLOW), 0)?;
        let l = st("lstat ml", lstat(&j(d, b"ml")))?;
        ensure("fchownat: the link's owner", (l.uid(), l.gid()) == (7, 8), l.uid() as i64)?;
        eq("fchownat following 9:10", s5(nr::FCHOWNAT, at, j(d, b"ml").at(), 9, 10, 0), 0)?;
        let s = st("stat m", stat(&m))?;
        ensure("fchownat: the file's owner", (s.uid(), s.gid()) == (9, 10), s.uid() as i64)?;
        // utimensat: set, omit, now; by path, by fd (path NULL), on a link.
        let times: [i64; 4] = [1_000_000, 111, 2_000_000, 222];
        eq("utimensat set", s4(nr::UTIMENSAT, at, m.at(), ptr(&times), 0), 0)?;
        let s = st("stat m", stat(&m))?;
        ensure("utimensat's atime", s.atime() == (1_000_000, 111), s.atime().0)?;
        ensure("utimensat's mtime", s.mtime() == (2_000_000, 222), s.mtime().0)?;
        let times: [i64; 4] = [0, UTIME_OMIT, 3_000_000, 0];
        eq("utimensat omit atime", s4(nr::UTIMENSAT, at, m.at(), ptr(&times), 0), 0)?;
        let s = st("stat m", stat(&m))?;
        ensure("UTIME_OMIT keeps atime", s.atime() == (1_000_000, 111), s.atime().0)?;
        ensure("and sets mtime", s.mtime() == (3_000_000, 0), s.mtime().0)?;
        let times: [i64; 4] = [0, UTIME_NOW, 0, UTIME_OMIT];
        eq("utimensat(fd, NULL) now", s4(nr::UTIMENSAT, fd as u64, 0, ptr(&times), 0), 0)?;
        let s = st("fstat", fstat(fd))?;
        // After 2020 (the RTC's date), and the mtime left alone.
        ensure("UTIME_NOW", s.atime().0 > 1_600_000_000, s.atime().0)?;
        ensure("UTIME_OMIT keeps mtime", s.mtime() == (3_000_000, 0), s.mtime().0)?;
        let times: [i64; 4] = [4_000_000, 0, 5_000_000, 0];
        eq("utimensat NOFOLLOW", s4(nr::UTIMENSAT, at, j(d, b"ml").at(), ptr(&times), AT_SYMLINK_NOFOLLOW), 0)?;
        eq("the link's mtime", st("lstat ml", lstat(&j(d, b"ml")))?.mtime().0, 5_000_000)?;
        eq("the file's mtime unchanged", st("stat m", stat(&m))?.mtime().0, 3_000_000)?;
        eq("utimensat times NULL", s4(nr::UTIMENSAT, at, m.at(), 0, 0), 0)?;
        let s = st("stat m", stat(&m))?;
        ensure("both now", s.atime().0 > 1_600_000_000 && s.mtime().0 > 1_600_000_000, s.mtime().0)?;
        let bad: [i64; 4] = [0, 1_000_000_000, 0, 0];
        eq("utimensat a bad nsec", s4(nr::UTIMENSAT, at, m.at(), ptr(&bad), 0), -EINVAL)
    })();
    close(fd);
    s1(nr::UMASK, 0o022);
    r
}

fn cwd(ext: &[u8], tmp: &[u8]) -> R {
    let r = (|| {
        cwd_is("getcwd at start", b"/")?;
        eq("chdir to the ext4 directory", s1(nr::CHDIR, P::new(&[ext]).at()), 0)?;
        cwd_is("getcwd after chdir", ext)?;
        // `sf` is from the stat check: relative paths start at the cwd.
        holds(&P::new(&[b"sf"]), b"0123456789")?;
        eq("mkdir relative", s2(nr::MKDIR, P::new(&[b"cd"]).at(), 0o755), 0)?;
        eq("symlink to it", s2(nr::SYMLINK, P::new(&[b"cd"]).at(), P::new(&[b"lcd"]).at()), 0)?;
        // Through a symlink: getcwd names the directory, not the link.
        eq("chdir through a symlink", s1(nr::CHDIR, P::new(&[b"lcd"]).at()), 0)?;
        cwd_is("getcwd through the link", j(ext, b"cd").bytes())?;
        // Renaming the cwd's directory: getcwd follows it.
        eq("rename the cwd", s2(nr::RENAME, j(ext, b"cd").at(), j(ext, b"cd2").at()), 0)?;
        cwd_is("getcwd after renaming it", j(ext, b"cd2").bytes())?;
        eq("rmdir .", s1(nr::RMDIR, P::new(&[b"."]).at()), -EINVAL)?;
        eq("chdir ..", s1(nr::CHDIR, P::new(&[b".."]).at()), 0)?;
        cwd_is("getcwd after ..", ext)?;
        // fchdir to another filesystem, by a plain and an O_PATH fd.
        let tfd = ok("open the tmpfs directory", open(&P::new(&[tmp]), O_RDONLY | O_DIRECTORY, 0))?;
        let r = s1(nr::FCHDIR, tfd as u64);
        close(tfd);
        eq("fchdir", r, 0)?;
        cwd_is("getcwd after fchdir", tmp)?;
        let here = st("stat .", stat(&P::new(&[b"."])))?;
        eq("\".\" is the tmpfs directory", here.ino() as i64, st("stat tmp", stat(&P::new(&[tmp])))?.ino() as i64)?;
        let pfd = ok("O_PATH on the ext4 directory", open(&P::new(&[ext]), O_PATH | O_DIRECTORY, 0))?;
        let r = s1(nr::FCHDIR, pfd as u64);
        close(pfd);
        eq("fchdir by an O_PATH fd", r, 0)?;
        cwd_is("getcwd after it", ext)?;
        let ffd = ok("open a file", open(&P::new(&[b"sf"]), O_RDONLY, 0))?;
        let r = s1(nr::FCHDIR, ffd as u64);
        close(ffd);
        eq("fchdir to a file", r, -ENOTDIR)?;
        eq("chdir to a file", s1(nr::CHDIR, P::new(&[b"sf"]).at()), -ENOTDIR)?;
        eq("chdir to a missing name", s1(nr::CHDIR, P::new(&[b"missing"]).at()), -ENOENT)?;
        let mut small = [0u8; 2];
        eq("getcwd into 2 bytes", s2(nr::GETCWD, mptr(&mut small[..]), 2), -ERANGE)?;
        cwd_is("the cwd kept after the failures", ext)
    })();
    s1(nr::CHDIR, ptr(&b"/\0"[..]));
    r?;
    cwd_is("back at /", b"/")
}

fn exdev_enotty(ext: &[u8], tmp: &[u8]) -> R {
    put(&j(tmp, b"x"), b"x")?;
    put(&j(ext, b"y"), b"y")?;
    eq("rename tmpfs -> ext4", s2(nr::RENAME, j(tmp, b"x").at(), j(ext, b"x").at()), -EXDEV)?;
    eq("link tmpfs -> ext4", s2(nr::LINK, j(tmp, b"x").at(), j(ext, b"x").at()), -EXDEV)?;
    let at = AT_FDCWD as u64;
    eq(
        "renameat2 RENAME_EXCHANGE across",
        s5(nr::RENAMEAT2, at, j(ext, b"y").at(), at, j(tmp, b"x").at(), RENAME_EXCHANGE),
        -EXDEV,
    )?;
    holds(&j(tmp, b"x"), b"x")?;
    holds(&j(ext, b"y"), b"y")?;
    eq("nothing made on ext4", st_err(lstat(&j(ext, b"x"))), -ENOENT)?;
    eq("rename within tmpfs", s2(nr::RENAME, j(tmp, b"x").at(), j(tmp, b"x2").at()), 0)?;
    // ENOTTY: a tty ioctl on a regular file; the console answers it.
    let fd = ok("open a file", open(&j(ext, b"y"), O_RDONLY, 0))?;
    let mut buf = [0u8; 64];
    let tcgets = s3(nr::IOCTL, fd as u64, TCGETS, mptr(&mut buf[..]));
    let winsz = s3(nr::IOCTL, fd as u64, TIOCGWINSZ, mptr(&mut buf[..]));
    close(fd);
    eq("TCGETS on a file", tcgets, -ENOTTY)?;
    eq("TIOCGWINSZ on a file", winsz, -ENOTTY)?;
    let dfd = ok("open a directory", open(&P::new(&[ext]), O_RDONLY | O_DIRECTORY, 0))?;
    let r = s3(nr::IOCTL, dfd as u64, TCGETS, mptr(&mut buf[..]));
    close(dfd);
    eq("TCGETS on a directory", r, -ENOTTY)?;
    eq("TCGETS on the console", s3(nr::IOCTL, 1, TCGETS, mptr(&mut buf[..])), 0)
}

// -- mode locks --------------------------------------------------------------------------

fn locks(d: &[u8]) -> c_int {
    let checks: [(&str, &dyn Fn() -> R); 6] = [
        ("setup", &|| eq("mkdir the test directory", s2(nr::MKDIR, P::new(&[d]).at(), 0o755), 0)),
        ("flock-open-file-descriptions", &|| flock(d)),
        ("renameat2-noreplace", &|| noreplace(d)),
        ("renameat2-exchange", &|| exchange(d)),
        ("rename-over-an-open-file", &|| overwrite_open(d)),
        ("cleanup", &|| cleanup(&[d])),
    ];
    run_checks(&checks)
}

fn flk(fd: i64, op: u64) -> i64 {
    s2(nr::FLOCK, fd as u64, op)
}

fn flock(d: &[u8]) -> R {
    let p = j(d, b"fl");
    put(&p, b"x")?;
    // Two open file descriptions of one file, in one process: their flocks
    // conflict. A dup shares its description's lock.
    let a = ok("open a", open(&p, O_RDWR, 0))?;
    let b = ok("open b", open(&p, O_RDWR, 0))?;
    let r = (|| {
        eq("LOCK_EX on a", flk(a, LOCK_EX), 0)?;
        eq("LOCK_EX|LOCK_NB on b", flk(b, LOCK_EX | LOCK_NB), -EAGAIN)?;
        eq("LOCK_SH|LOCK_NB on b", flk(b, LOCK_SH | LOCK_NB), -EAGAIN)?;
        let a2 = ok("F_DUPFD a", s3(nr::FCNTL, a as u64, F_DUPFD, 0))?;
        let r = (|| {
            eq("LOCK_EX|LOCK_NB on a's dup", flk(a2, LOCK_EX | LOCK_NB), 0)?;
            eq("LOCK_UN on a", flk(a, LOCK_UN), 0)?;
            eq("then LOCK_EX|LOCK_NB on b", flk(b, LOCK_EX | LOCK_NB), 0)?;
            eq("now a's conflicts", flk(a, LOCK_SH | LOCK_NB), -EAGAIN)?;
            eq("LOCK_UN on b", flk(b, LOCK_UN), 0)?;
            // Shared with shared; exclusive against either.
            eq("LOCK_SH on a", flk(a, LOCK_SH | LOCK_NB), 0)?;
            eq("LOCK_SH on b", flk(b, LOCK_SH | LOCK_NB), 0)?;
            eq("LOCK_EX on b against a's shared", flk(b, LOCK_EX | LOCK_NB), -EAGAIN)
        })();
        // Closing a dup keeps the description's lock (a is still open).
        close(a2);
        r?;
        eq("LOCK_EX on b after a's dup closed", flk(b, LOCK_EX | LOCK_NB), -EAGAIN)?;
        eq("a bad operation", flk(a, 0), -EINVAL)?;
        eq("a bad fd", flk(999, LOCK_SH), -EBADF)?;
        Ok(())
    })();
    // The last close of a's description gives its lock up.
    close(a);
    let after = flk(b, LOCK_EX | LOCK_NB);
    close(b);
    r?;
    eq("LOCK_EX on b after a's last close", after, 0)
}

fn rename2(old: &P, new: &P, flags: u64) -> i64 {
    s5(nr::RENAMEAT2, AT_FDCWD as u64, old.at(), AT_FDCWD as u64, new.at(), flags)
}

fn noreplace(d: &[u8]) -> R {
    let (a, b, c) = (j(d, b"ra"), j(d, b"rb"), j(d, b"rc"));
    put(&a, b"A")?;
    put(&b, b"B")?;
    eq("RENAME_NOREPLACE onto a name in use", rename2(&a, &b, RENAME_NOREPLACE), -EEXIST)?;
    holds(&a, b"A")?;
    holds(&b, b"B")?;
    eq("RENAME_NOREPLACE to a free name", rename2(&a, &c, RENAME_NOREPLACE), 0)?;
    eq("the old name gone", st_err(lstat(&a)), -ENOENT)?;
    holds(&c, b"A")?;
    eq("NOREPLACE|EXCHANGE", rename2(&c, &b, RENAME_NOREPLACE | RENAME_EXCHANGE), -EINVAL)?;
    eq("an unknown flag", rename2(&c, &b, 8), -EINVAL)?;
    // A directory onto a directory in use, too.
    eq("mkdir n1", s2(nr::MKDIR, j(d, b"n1").at(), 0o755), 0)?;
    eq("mkdir n2", s2(nr::MKDIR, j(d, b"n2").at(), 0o755), 0)?;
    eq("RENAME_NOREPLACE onto an empty directory", rename2(&j(d, b"n1"), &j(d, b"n2"), RENAME_NOREPLACE), -EEXIST)
}

fn exchange(d: &[u8]) -> R {
    // `rb` holds "B" and `rc` "A" (from renameat2-noreplace).
    let (b, c) = (j(d, b"rb"), j(d, b"rc"));
    let ib = st("stat rb", stat(&b))?.ino();
    eq("RENAME_EXCHANGE", rename2(&c, &b, RENAME_EXCHANGE), 0)?;
    holds(&c, b"B")?;
    holds(&b, b"A")?;
    eq("the inode moved with its data", st("stat rc", stat(&c))?.ino() as i64, ib as i64)?;
    eq("RENAME_EXCHANGE with a free name", rename2(&c, &j(d, b"free"), RENAME_EXCHANGE), -ENOENT)?;
    // A file and a directory trade places; the directory keeps its contents.
    let dir = j(d, b"xd");
    eq("mkdir xd", s2(nr::MKDIR, dir.at(), 0o755), 0)?;
    put(&j(dir.bytes(), b"inside"), b"in")?;
    eq("RENAME_EXCHANGE a file and a directory", rename2(&b, &dir, RENAME_EXCHANGE), 0)?;
    holds(&dir, b"A")?;
    eq("rb is the directory", st("stat rb", stat(&b))?.kind() as i64, S_IFDIR as i64)?;
    holds(&P::new(&[b.bytes(), b"/inside"]), b"in")?;
    // renameat relative to directory fds.
    let dfd = ok("open the directory", open(&P::new(&[d]), O_RDONLY | O_DIRECTORY, 0))?;
    let r = s4(nr::RENAMEAT, dfd as u64, ptr(&b"rc\0"[..]), dfd as u64, ptr(&b"rc2\0"[..]));
    close(dfd);
    eq("renameat by dirfds", r, 0)?;
    holds(&j(d, b"rc2"), b"B")
}

fn overwrite_open(d: &[u8]) -> R {
    let (old, new) = (j(d, b"old"), j(d, b"new"));
    put(&old, b"old-contents")?;
    let fd = ok("open old", open(&old, O_RDONLY, 0))?;
    let r = (|| {
        let ino = st("fstat", fstat(fd))?.ino();
        put(&new, b"new")?;
        // rename replaces the name at once: every open sees one file or the
        // other, and the open descriptor keeps the old inode and its data.
        eq("rename over an open file", s2(nr::RENAME, new.at(), old.at()), 0)?;
        holds(&old, b"new")?;
        eq("the new name gone", st_err(lstat(&new)), -ENOENT)?;
        preads(fd, 0, b"old-contents")?;
        reads(fd, b"old-contents")?;
        // The descriptor's inode has no links left (its st_ino may change:
        // see unlink-while-open), and the name is the other inode.
        let s = st("fstat after", fstat(fd))?;
        eq("the old file has no links", s.nlink() as i64, 0)?;
        eq("and its size", s.size(), 12)?;
        ensure("the name's another inode", st("stat old", stat(&old))?.ino() != ino, ino as i64)
    })();
    close(fd);
    r?;
    // The type rules.
    let (dd, full, f) = (j(d, b"od"), j(d, b"ofull"), j(d, b"of"));
    eq("mkdir od", s2(nr::MKDIR, dd.at(), 0o755), 0)?;
    eq("mkdir ofull", s2(nr::MKDIR, full.at(), 0o755), 0)?;
    put(&j(full.bytes(), b"x"), b"x")?;
    put(&f, b"f")?;
    eq("a directory over a non-empty one", s2(nr::RENAME, dd.at(), full.at()), -ENOTEMPTY)?;
    eq("a directory over a file", s2(nr::RENAME, dd.at(), f.at()), -ENOTDIR)?;
    eq("a file over a directory", s2(nr::RENAME, f.at(), dd.at()), -EISDIR)?;
    eq("a directory into itself", s2(nr::RENAME, dd.at(), j(dd.bytes(), b"in").at()), -EINVAL)?;
    eq("a directory over an empty one", s2(nr::RENAME, full.at(), dd.at()), 0)?;
    holds(&j(dd.bytes(), b"x"), b"x")
}

// -- modes lock-hold and lock-probe --------------------------------------------------------

/// Linux x86-64's `struct flock` (32 bytes).
#[repr(C)]
#[derive(Clone, Copy)]
struct Flock {
    l_type: i16,
    l_whence: i16,
    _pad: i32,
    l_start: i64,
    l_len: i64,
    l_pid: i32,
    _pad2: i32,
}

impl Flock {
    fn new(l_type: i16, l_start: i64, l_len: i64) -> Self {
        Self { l_type, l_whence: SEEK_SET as i16, _pad: 0, l_start, l_len, l_pid: 0, _pad2: 0 }
    }
}

fn fcntl_lock(fd: i64, cmd: u64, fl: &mut Flock) -> i64 {
    s3(nr::FCNTL, fd as u64, cmd, mptr(fl))
}

/// Wait for a byte on stdin (a key on the terminal): false at the end of
/// input or an error.
fn wait_step() -> bool {
    let mut b = [0u8; 4];
    s3(nr::READ, 0, mptr(&mut b[..]), b.len() as u64) > 0
}

fn lock_hold(path: &[u8]) -> c_int {
    let p = P::new(&[path]);
    let fd = open(&p, O_RDWR | O_CREAT, 0o644);
    if fd < 0 {
        say!("hold: FAIL open (got {fd})");
        return 1;
    }
    let pid = s0(nr::GETPID);
    say!("hold: pid={pid} ready");
    let mut other = -1i64;
    let steps: [&str; 6] = ["locked", "split", "coalesced", "released", "relocked", "done"];
    for (i, name) in steps.iter().enumerate() {
        if !wait_step() {
            say!("hold: FAIL no key for step {name}");
            return 2;
        }
        let r = match i {
            // A write lock on [0, 100) and a read lock on [200, 210).
            0 => {
                let r = fcntl_lock(fd, F_SETLK, &mut Flock::new(F_WRLCK, 0, 100));
                if r < 0 { r } else { fcntl_lock(fd, F_SETLK, &mut Flock::new(F_RDLCK, 200, 10)) }
            }
            // Unlocking [40, 60) splits the write lock in two.
            1 => fcntl_lock(fd, F_SETLK, &mut Flock::new(F_UNLCK, 40, 20)),
            // Locking it again joins them into one.
            2 => {
                // Another descriptor of the file, opened now and kept.
                other = open(&p, O_RDWR, 0);
                if other < 0 { other } else { fcntl_lock(fd, F_SETLK, &mut Flock::new(F_WRLCK, 40, 20)) }
            }
            // Closing any descriptor of the file, not the one that locked,
            // gives up all the process's locks on it.
            3 => close(other),
            // Locked again, to be given up by the exit.
            4 => fcntl_lock(fd, F_SETLK, &mut Flock::new(F_WRLCK, 0, 100)),
            _ => 0,
        };
        if r < 0 {
            say!("hold: FAIL step {name} (got {r})");
            return 3;
        }
        // A process's own locks never conflict with it: F_UNLCK.
        let mut own = Flock::new(F_WRLCK, 0, 0);
        let g = fcntl_lock(fd, F_GETLK, &mut own);
        if g != 0 || own.l_type != F_UNLCK {
            say!("hold: FAIL own F_GETLK after {name} (got {g}, type {})", own.l_type);
            return 4;
        }
        say!("hold: {name}");
    }
    // Ending with `fd` open and [0, 100) locked: the exit releases it.
    0
}

/// Parse a decimal number at the start of `s`: the number and the rest.
fn number(s: &[u8]) -> Option<(i64, &[u8])> {
    let digits = s.iter().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let mut v = 0i64;
    for &b in &s[..digits] {
        v = v.checked_mul(10)?.checked_add((b - b'0') as i64)?;
    }
    Some((v, &s[digits..]))
}

/// `<t><start>+<len>`: a lock's type and range, and what follows it.
fn range(s: &[u8]) -> Option<(i16, i64, i64, &[u8])> {
    let t = match s.first()? {
        b'r' => F_RDLCK,
        b'w' => F_WRLCK,
        _ => return None,
    };
    let (start, rest) = number(&s[1..])?;
    let rest = rest.strip_prefix(b"+")?;
    let (len, rest) = number(rest)?;
    Some((t, start, len, rest))
}

fn lock_probe(path: &[u8], pid: &[u8], probes: &[&[u8]]) -> c_int {
    let Some((pid, b"")) = number(pid) else {
        say!("probe: FAIL bad pid {}", Show(pid));
        return 1;
    };
    let fd = open(&P::new(&[path]), O_RDWR, 0);
    if fd < 0 {
        say!("probe: FAIL open (got {fd})");
        return 1;
    }
    for (i, &probe) in probes.iter().enumerate() {
        let n = (i + 2) as c_int;
        let Some((t, start, len, rest)) = range(probe) else {
            say!("probe: FAIL bad probe {}", Show(probe));
            return n;
        };
        let Some(expect) = rest.strip_prefix(b"=") else {
            say!("probe: FAIL bad probe {}", Show(probe));
            return n;
        };
        let want = if expect == b"unlck" {
            None
        } else if let Some((wt, ws, wl, b"")) = range(expect) {
            Some((wt, ws, wl))
        } else {
            say!("probe: FAIL bad probe {}", Show(probe));
            return n;
        };
        let mut fl = Flock::new(t, start, len);
        let g = fcntl_lock(fd, F_GETLK, &mut fl);
        let seen_ok = g == 0
            && match want {
                None => fl.l_type == F_UNLCK,
                Some((wt, ws, wl)) => {
                    fl.l_type == wt && fl.l_start == ws && fl.l_len == wl && fl.l_pid as i64 == pid && fl.l_whence == 0
                }
            };
        if !seen_ok {
            say!(
                "probe: FAIL {}: F_GETLK returned {g}: type {} start {} len {} pid {} whence {}",
                Show(probe),
                fl.l_type,
                fl.l_start,
                fl.l_len,
                fl.l_pid,
                fl.l_whence
            );
            return n;
        }
        let set = fcntl_lock(fd, F_SETLK, &mut Flock::new(t, start, len));
        let set_ok = match want {
            Some(_) => set == -EAGAIN || set == -EACCES,
            None => set == 0 && fcntl_lock(fd, F_SETLK, &mut Flock::new(F_UNLCK, start, len)) == 0,
        };
        if !set_ok {
            say!("probe: FAIL {}: F_SETLK returned {set}", Show(probe));
            return n;
        }
        say!("ok probe {}", Show(probe));
    }
    close(fd);
    0
}

// -- main ----------------------------------------------------------------------------------

/// The NUL-terminated C string at `p` as bytes.
///
/// # Safety
/// `p` points at a NUL-terminated string that outlives the program.
unsafe fn arg(p: *const u8) -> &'static [u8] {
    let mut n = 0;
    // SAFETY: the caller's contract: readable up to and including its NUL.
    while unsafe { *p.add(n) } != 0 {
        n += 1;
    }
    // SAFETY: the `n` bytes before the NUL are readable for the program's life.
    unsafe { core::slice::from_raw_parts(p, n) }
}

const USAGE: &str =
    "usage: m7_vfs posix <ext4 dir> <tmpfs dir> | locks <dir> | lock-hold <file> | lock-probe <file> <pid> <probe>...";

#[unsafe(no_mangle)]
pub extern "C" fn main(argc: c_int, argv: *const *const u8) -> c_int {
    let mut args: [&[u8]; 16] = [b""; 16];
    let argc = (argc.max(0) as usize).min(args.len());
    for (i, slot) in args[..argc].iter_mut().enumerate() {
        // SAFETY: the kernel's initial stack holds `argc` valid pointers to
        // NUL-terminated strings at `argv`, kept for the program's life.
        *slot = unsafe { arg(*argv.add(i)) };
    }
    let args = &args[..argc];
    let code = match args {
        [_, b"posix", ext, tmp] => posix(ext, tmp),
        [_, b"locks", dir] => locks(dir),
        [_, b"lock-hold", file] => lock_hold(file),
        [_, b"lock-probe", file, pid, probes @ ..] if !probes.is_empty() => lock_probe(file, pid, probes),
        _ => {
            say!("{USAGE}");
            64
        }
    };
    code
}
