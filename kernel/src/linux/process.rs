//! Per-process Linux x86-64 state, console I/O bridge, and exit tracking (M6).
//!
//! Each ring-3 run registers a [`ProcessState`] keyed by its kernel scheduler
//! thread id in [`ACTIVE_PROCS`] and removes it cleanly when the thread exits
//! or faults. Lock discipline: `ACTIVE_PROCS` is an IRQ-off [`SpinLock`] and
//! must NEVER be held across `copy_from_user`, `copy_to_user`, blocking waits,
//! or `AddressSpace` operations.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicU32, Ordering};

use lumen_vfs::{VfsError, VfsInode};

use crate::keyboard::{Key as KbdKey, KeyCode};
use crate::klog;
use crate::linux::general;
use crate::mm::user_vm::AddressSpace;
use crate::sched;
use crate::serial_in;
use crate::sync::SpinLock;
use crate::term;
use crate::user::UserExit;

/// Maximum open file descriptors per M6 process.
pub const MAX_FDS: usize = 64;

/// Maximum captured stdout bytes for `usertest` runs.
const CAPTURE_STDOUT_LIMIT: usize = 4096;

/// Descriptor kind stored in the M6 per-process FD table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FdKind {
    Closed,
    Stdin,
    Stdout,
    Stderr,
}

impl FdKind {
    #[inline]
    pub const fn is_open(self) -> bool {
        !matches!(self, FdKind::Closed)
    }

    #[inline]
    pub const fn is_readable(self) -> bool {
        matches!(self, FdKind::Stdin)
    }

    #[inline]
    pub const fn is_writable(self) -> bool {
        matches!(self, FdKind::Stdout | FdKind::Stderr)
    }
}

/// Where a ring-3 process's console fds (0, 1, 2) connect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleIo {
    /// `usertest` capture mode: stdout captured into `ProcessState::stdout`,
    /// stdin served from pre-seeded `ProcessState::stdin`.
    Capture,
    /// Interactive serial console (`serial_in` + `klog::LockedSerial`).
    Serial,
    /// Interactive virtual terminal (`vt::wait_key` + `term::write_str_to`).
    Vt(usize),
}

/// POD representation of Linux x86-64 `struct kernel_sigaction` (32 bytes,
/// zero padding, `Send`/`Sync` safe without raw function pointers).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KernelSigaction {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

const _: () = {
    assert!(size_of::<KernelSigaction>() == size_of::<general::kernel_sigaction>());
    assert!(size_of::<KernelSigaction>() == 32);
    assert!(offset_of!(KernelSigaction, handler) == offset_of!(general::kernel_sigaction, sa_handler_kernel));
    assert!(offset_of!(KernelSigaction, flags) == offset_of!(general::kernel_sigaction, sa_flags));
    assert!(offset_of!(KernelSigaction, restorer) == offset_of!(general::kernel_sigaction, sa_restorer));
    assert!(offset_of!(KernelSigaction, mask) == offset_of!(general::kernel_sigaction, sa_mask));
};

/// Default console `struct termios` returned by `TCGETS` (canonical mode,
/// CR->NL mapping, standard control characters).
pub fn default_termios() -> general::termios {
    let mut c_cc = [0u8; 19];
    c_cc[general::VINTR as usize] = 3; // ^C
    c_cc[general::VQUIT as usize] = 28; // ^\
    c_cc[general::VERASE as usize] = 127; // DEL
    c_cc[general::VKILL as usize] = 21; // ^U
    c_cc[general::VEOF as usize] = 4; // ^D
    c_cc[general::VTIME as usize] = 0;
    c_cc[general::VMIN as usize] = 1;
    c_cc[general::VSTART as usize] = 17; // ^Q
    c_cc[general::VSTOP as usize] = 19; // ^S
    c_cc[general::VSUSP as usize] = 26; // ^Z
    general::termios {
        c_iflag: general::ICRNL | general::IUTF8,
        c_oflag: general::OPOST | general::ONLCR,
        c_cflag: general::B38400 | general::CS8 | general::CREAD,
        c_lflag: general::ISIG | general::ICANON | general::ECHO | general::ECHOE | general::ECHOK,
        c_line: 0,
        c_cc,
    }
}

/// Per-process Linux state owned by one active ring-3 thread.
pub struct ProcessState {
    pub pid: u32,
    pub ppid: u32,
    pub tid: u32,
    pub fds: [FdKind; MAX_FDS],
    pub sigactions: [KernelSigaction; 64],
    pub sigmask: u64,
    pub clear_child_tid: u64,
    pub termios: general::termios,
    pub io: ConsoleIo,
    pub allow_test_syscalls: bool,
    pub exit: Option<UserExit>,
    pub stdout: Vec<u8>,
    /// The process's address space, for `/proc/<pid>/maps`.
    pub address_space: Option<Arc<AddressSpace>>,
    /// The executable's path for `/proc/<pid>/exe`: set by `linux /path`
    /// (the path as typed) and by `vfstest dev-tmp-proc`'s checks (and the
    /// ktests running them).
    pub exe_path: Option<String>,
    /// VFS files open on descriptors, with the path each was opened by, for
    /// `/proc/<pid>/fd`: a stand-in for the 1024-entry FD table that #254
    /// wires into the syscalls (only `vfstest dev-tmp-proc`'s checks, and the
    /// ktests running them, fill it today). Keys are in `0..lumen_vfs::MAX_FDS`
    /// (`register_current_vfs_fd` refuses others).
    pub vfs_fds: BTreeMap<i32, (Arc<dyn VfsInode>, String)>,
    stdin: Vec<u8>,
    stdin_pos: usize,
    /// Small lookahead / UTF-8 tail buffer for stdin (up to 4 bytes).
    unread: [u8; 4],
    unread_pos: u8,
    unread_len: u8,
    /// Incomplete trailing UTF-8 bytes pending across `write`/`writev` chunks on a VT.
    vt_utf8: [u8; 4],
    vt_utf8_len: u8,
}

impl ProcessState {
    fn new(
        pid: u32,
        io: ConsoleIo,
        allow_test_syscalls: bool,
        stdin: Vec<u8>,
        address_space: Option<Arc<AddressSpace>>,
        exe_path: Option<String>,
    ) -> Self {
        let mut fds = [FdKind::Closed; MAX_FDS];
        fds[0] = FdKind::Stdin;
        fds[1] = FdKind::Stdout;
        fds[2] = FdKind::Stderr;
        Self {
            pid,
            ppid: 0,
            tid: pid,
            fds,
            sigactions: [KernelSigaction::default(); 64],
            sigmask: 0,
            clear_child_tid: 0,
            termios: default_termios(),
            io,
            allow_test_syscalls,
            exit: None,
            stdout: Vec::new(),
            address_space,
            exe_path,
            vfs_fds: BTreeMap::new(),
            stdin,
            stdin_pos: 0,
            unread: [0; 4],
            unread_pos: 0,
            unread_len: 0,
            vt_utf8: [0; 4],
            vt_utf8_len: 0,
        }
    }

    #[inline]
    pub fn fd_kind(&self, fd: u64) -> FdKind {
        if fd < MAX_FDS as u64 { self.fds[fd as usize] } else { FdKind::Closed }
    }

    #[inline]
    fn has_unread(&self) -> bool {
        self.unread_pos < self.unread_len
    }

    #[inline]
    fn pop_unread(&mut self) -> Option<u8> {
        if self.unread_pos < self.unread_len {
            let b = self.unread[self.unread_pos as usize];
            self.unread_pos += 1;
            if self.unread_pos == self.unread_len {
                self.unread_pos = 0;
                self.unread_len = 0;
            }
            Some(b)
        } else {
            None
        }
    }

    fn push_unread_bytes(&mut self, bytes: &[u8]) {
        debug_assert!(!self.has_unread(), "push_unread_bytes called with unread bytes pending");
        let n = bytes.len().min(self.unread.len());
        self.unread[..n].copy_from_slice(&bytes[..n]);
        self.unread_pos = 0;
        self.unread_len = n as u8;
    }
}

static ACTIVE_PROCS: SpinLock<BTreeMap<u64, ProcessState>> = SpinLock::new(BTreeMap::new());
static NEXT_PID: AtomicU32 = AtomicU32::new(1);

#[inline]
fn current_kernel_tid() -> u64 {
    sched::running_here().expect("ring-3 syscall/exception requires a running thread").0
}

/// Register a fresh [`ProcessState`] for the current user thread, with the
/// executable's path for `/proc/<pid>/exe` if it was loaded by path, and
/// return its PID.
pub fn register_current(io: ConsoleIo, allow_test_syscalls: bool, stdin: Vec<u8>, exe_path: Option<String>) -> u32 {
    let ktid = current_kernel_tid();
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    let address_space = sched::current_address_space();
    let state = ProcessState::new(pid, io, allow_test_syscalls, stdin, address_space, exe_path);
    let prev = ACTIVE_PROCS.lock().insert(ktid, state);
    debug_assert!(prev.is_none(), "ring-3 process registered twice on thread {ktid}");
    pid
}

/// Remove and return the current thread's [`ProcessState`] after ring-3 exits.
pub fn take_current() -> ProcessState {
    let ktid = current_kernel_tid();
    ACTIVE_PROCS.lock().remove(&ktid).expect("active process state missing after ring-3 exit")
}

/// Non-panicking lookup of the current thread's [`ConsoleIo`], returning `None`
/// if called outside an active ring-3 [`ProcessState`] (e.g., from a kernel thread).
pub fn try_current_io() -> Option<ConsoleIo> {
    let ktid = sched::running_here()?.0;
    ACTIVE_PROCS.lock().get(&ktid).map(|p| p.io)
}

/// Non-panicking lookup of the current thread's Linux PID.
pub fn current_pid() -> Option<u32> {
    let ktid = sched::running_here()?.0;
    ACTIVE_PROCS.lock().get(&ktid).map(|p| p.pid)
}

/// Snapshot all active Linux PIDs (sorted).
pub fn active_pids() -> Vec<u32> {
    let guard = ACTIVE_PROCS.lock();
    let mut pids: Vec<u32> = guard.values().map(|p| p.pid).collect();
    pids.sort_unstable();
    pids
}

/// True if `pid` corresponds to an active [`ProcessState`].
pub fn has_pid(pid: u32) -> bool {
    ACTIVE_PROCS.lock().values().any(|p| p.pid == pid)
}

/// Clone the `Arc<AddressSpace>` for `pid` and release `ACTIVE_PROCS` before returning.
pub fn process_address_space(pid: u32) -> Option<Arc<AddressSpace>> {
    let guard = ACTIVE_PROCS.lock();
    guard.values().find(|p| p.pid == pid).and_then(|p| p.address_space.as_ref().map(Arc::clone))
}

/// Snapshot the executable path recorded for `pid`, if any.
pub fn process_exe_path(pid: u32) -> Option<String> {
    let guard = ACTIVE_PROCS.lock();
    guard.values().find(|p| p.pid == pid).and_then(|p| p.exe_path.clone())
}

/// Set the executable's path on the current [`ProcessState`], as given (not
/// resolved: symlinks and `..` stay).
pub fn set_current_exe_path(path: &str) {
    let Some(ktid) = sched::running_here().map(|id| id.0) else {
        return;
    };
    let path = String::from(path);
    let mut guard = ACTIVE_PROCS.lock();
    if let Some(proc) = guard.get_mut(&ktid) {
        proc.exe_path = Some(path);
    }
}

/// Attach a VFS inode/path to `fd` on the current [`ProcessState`] (extracting
/// any displaced entry so `Arc` drops outside `ACTIVE_PROCS`). `EBADF` for an
/// fd outside `0..lumen_vfs::MAX_FDS`: procfs builds `/proc/<pid>/fd/<fd>`'s
/// inode number from it and relies on that range.
pub fn register_current_vfs_fd(fd: i32, inode: Arc<dyn VfsInode>, path: String) -> Result<(), VfsError> {
    if usize::try_from(fd).map_or(true, |f| f >= lumen_vfs::MAX_FDS) {
        return Err(VfsError::BadF);
    }
    let Some(ktid) = sched::running_here().map(|id| id.0) else {
        return Ok(());
    };
    let displaced = {
        let mut guard = ACTIVE_PROCS.lock();
        guard.get_mut(&ktid).and_then(|proc| proc.vfs_fds.insert(fd, (inode, path)))
    };
    drop(displaced);
    Ok(())
}

/// Detach a VFS descriptor from the current [`ProcessState`], dropping its
/// `Arc<dyn VfsInode>` outside `ACTIVE_PROCS`.
pub fn unregister_current_vfs_fd(fd: i32) {
    let Some(ktid) = sched::running_here().map(|id| id.0) else {
        return;
    };
    let removed = {
        let mut guard = ACTIVE_PROCS.lock();
        guard.get_mut(&ktid).and_then(|proc| proc.vfs_fds.remove(&fd))
    };
    drop(removed);
}

/// Return sorted open descriptor numbers for `pid`.
pub fn process_open_fds(pid: u32) -> Vec<i32> {
    let guard = ACTIVE_PROCS.lock();
    let Some(proc) = guard.values().find(|p| p.pid == pid) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (idx, kind) in proc.fds.iter().enumerate() {
        let fd = idx as i32;
        if kind.is_open() || proc.vfs_fds.contains_key(&fd) {
            out.push(fd);
        }
    }
    for &fd in proc.vfs_fds.keys() {
        let open_in_array = usize::try_from(fd).ok().is_some_and(|i| i < MAX_FDS && proc.fds[i].is_open());
        if !open_in_array && !out.contains(&fd) {
            out.push(fd);
        }
    }
    out.sort_unstable();
    out
}

/// Snapshot `(Option<Arc<dyn VfsInode>>, display_path)` for `/proc/<pid>/fd/<fd>`.
///
/// Releases `ACTIVE_PROCS` before inspecting `VfsInode::stat()` or resolving `/dev/console`.
pub fn process_fd_target(pid: u32, fd: i32) -> Option<(Option<Arc<dyn VfsInode>>, String)> {
    enum Snapshot {
        Vfs(Arc<dyn VfsInode>, String),
        Console,
    }

    let snap = {
        let guard = ACTIVE_PROCS.lock();
        let proc = guard.values().find(|p| p.pid == pid)?;
        if let Some((inode, path)) = proc.vfs_fds.get(&fd) {
            Snapshot::Vfs(Arc::clone(inode), path.clone())
        } else if fd >= 0 && (fd as usize) < MAX_FDS && proc.fds[fd as usize].is_open() {
            Snapshot::Console
        } else {
            return None;
        }
    };

    match snap {
        Snapshot::Vfs(inode, mut path) => {
            let unlinked = inode.is_detached() || inode.stat().ok().is_some_and(|m| m.nlink == 0);
            if unlinked && !path.ends_with(" (deleted)") {
                path.push_str(" (deleted)");
            }
            Some((Some(inode), path))
        }
        Snapshot::Console => {
            let inode = crate::vfs::resolve_path_from_root("/dev/console").ok();
            Some((inode, String::from("/dev/console")))
        }
    }
}

/// Run a short non-blocking closure with immutable access to the current process state.
pub fn with_current<R>(f: impl FnOnce(&ProcessState) -> R) -> R {
    let ktid = current_kernel_tid();
    let guard = ACTIVE_PROCS.lock();
    let proc = guard.get(&ktid).expect("ring-3 syscall with no active ProcessState");
    f(proc)
}

/// Run a short non-blocking closure with mutable access to the current process state.
pub fn with_current_mut<R>(f: impl FnOnce(&mut ProcessState) -> R) -> R {
    let ktid = current_kernel_tid();
    let mut guard = ACTIVE_PROCS.lock();
    let proc = guard.get_mut(&ktid).expect("ring-3 syscall with no active ProcessState");
    f(proc)
}

/// Record the process's terminal [`UserExit`] and return `(pid, allow_test_syscalls)`.
pub fn record_exit(exit: UserExit) -> (u32, bool) {
    with_current_mut(|proc| {
        proc.exit = Some(exit);
        (proc.pid, proc.allow_test_syscalls)
    })
}

/// Write `bytes` (already copied from user space) to the process's console target.
///
/// Never called while holding `ACTIVE_PROCS` for `Serial` or `Vt`; for `Capture`
/// it acquires `ACTIVE_PROCS` briefly to append up to `CAPTURE_STDOUT_LIMIT`.
pub fn write_console(io: ConsoleIo, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    match io {
        ConsoleIo::Capture => {
            with_current_mut(|proc| {
                let room = CAPTURE_STDOUT_LIMIT.saturating_sub(proc.stdout.len());
                proc.stdout.extend_from_slice(&bytes[..bytes.len().min(room)]);
            });
        }
        ConsoleIo::Serial => {
            klog::with_line_lock(|| {
                let _ = klog::burst_held(bytes);
            });
        }
        ConsoleIo::Vt(vt) => write_vt_utf8(vt, bytes),
    }
}

/// Stream `bytes` onto virtual terminal `vt`, preserving incomplete trailing
/// multi-byte UTF-8 sequences across chunk/write boundaries and replacing only
/// genuinely invalid byte sequences with U+FFFD.
fn write_vt_utf8(vt: usize, bytes: &[u8]) {
    let (mut tail, mut tail_len) = with_current_mut(|proc| {
        let t = proc.vt_utf8;
        let l = proc.vt_utf8_len as usize;
        proc.vt_utf8_len = 0;
        (t, l)
    });

    let mut scratch = [0u8; 516];
    for chunk in bytes.chunks(512) {
        scratch[..tail_len].copy_from_slice(&tail[..tail_len]);
        let total = tail_len + chunk.len();
        scratch[tail_len..total].copy_from_slice(chunk);
        tail_len = 0;

        let mut rest = &scratch[..total];
        while !rest.is_empty() {
            match core::str::from_utf8(rest) {
                Ok(s) => {
                    let _ = term::write_str_to(vt, s);
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0
                        && let Ok(s) = core::str::from_utf8(&rest[..valid])
                    {
                        let _ = term::write_str_to(vt, s);
                    }
                    match e.error_len() {
                        Some(bad_len) => {
                            let _ = term::write_str_to(vt, "\u{fffd}");
                            rest = &rest[valid + bad_len..];
                        }
                        None => {
                            // Incomplete multi-byte UTF-8 sequence (1..=3 bytes) at chunk end.
                            let rem = &rest[valid..];
                            let n = rem.len().min(tail.len());
                            tail[..n].copy_from_slice(&rem[..n]);
                            tail_len = n;
                            break;
                        }
                    }
                }
            }
        }
    }

    with_current_mut(|proc| {
        proc.vt_utf8 = tail;
        proc.vt_utf8_len = tail_len as u8;
    });
}

/// Translate a framebuffer VT keyboard event into VT100/UTF-8 input bytes
/// (up to 4 bytes). Returns `0` for keys that emit no terminal bytes (such as
/// bare modifiers or unhandled function keys).
fn vt_key_to_bytes(key: KbdKey, out: &mut [u8; 4]) -> usize {
    match key {
        KbdKey::Char(ch) => {
            let mapped = if ch == '\r' { '\n' } else { ch };
            let bytes = mapped.encode_utf8(out).as_bytes();
            bytes.len()
        }
        KbdKey::Raw { code, .. } => {
            let seq: &[u8] = match code {
                KeyCode::ArrowUp => b"\x1b[A",
                KeyCode::ArrowDown => b"\x1b[B",
                KeyCode::ArrowRight => b"\x1b[C",
                KeyCode::ArrowLeft => b"\x1b[D",
                KeyCode::Home => b"\x1b[H",
                KeyCode::End => b"\x1b[F",
                KeyCode::Delete => b"\x1b[3~",
                KeyCode::Insert => b"\x1b[2~",
                KeyCode::PageUp => b"\x1b[5~",
                KeyCode::PageDown => b"\x1b[6~",
                KeyCode::Escape => b"\x1b",
                _ => return 0,
            };
            out[..seq.len()].copy_from_slice(seq);
            seq.len()
        }
    }
}

/// Whether stdin currently has data available without blocking (or EOF in capture mode).
pub fn stdin_ready(io: ConsoleIo) -> bool {
    match io {
        ConsoleIo::Capture => true,
        ConsoleIo::Serial => with_current(ProcessState::has_unread) || serial_in::available(),
        ConsoleIo::Vt(vt) => {
            if with_current(ProcessState::has_unread) {
                return true;
            }
            while let Some(key) = crate::vt::read_key(vt) {
                let mut enc = [0u8; 4];
                let n = vt_key_to_bytes(key, &mut enc);
                if n > 0 {
                    with_current_mut(|p| p.push_unread_bytes(&enc[..n]));
                    return true;
                }
            }
            false
        }
    }
}

/// Block briefly waiting for stdin input up to optional monotonic `deadline` (ns).
/// Must be called with interrupts on and no `SpinLock` held.
pub fn wait_stdin_step(io: ConsoleIo, deadline: Option<u64>) {
    match io {
        ConsoleIo::Serial => serial_in::wait_until(deadline),
        ConsoleIo::Vt(_) => {
            let slice = match deadline {
                Some(dl) => dl.saturating_sub(crate::time::now_ns()).min(5_000_000),
                None => 5_000_000,
            };
            if slice > 0 {
                let _ = crate::sched::sleep(slice);
            }
        }
        ConsoleIo::Capture => {}
    }
}

/// Read up to `dst.len()` bytes from the process's console stdin, blocking until
/// at least 1 byte arrives (or returning `0` on EOF).
///
/// Must be called with interrupts enabled and no `SpinLock` held.
pub fn read_console(io: ConsoleIo, dst: &mut [u8]) -> usize {
    if dst.is_empty() {
        return 0;
    }
    match io {
        ConsoleIo::Capture => with_current_mut(|proc| {
            let avail = &proc.stdin[proc.stdin_pos..];
            if avail.is_empty() {
                return 0;
            }
            let mut n = 0;
            for &b in avail {
                if n >= dst.len() {
                    break;
                }
                dst[n] = b;
                n += 1;
                if b == b'\n' {
                    break;
                }
            }
            proc.stdin_pos += n;
            n
        }),
        ConsoleIo::Serial => read_serial(dst),
        ConsoleIo::Vt(vt) => read_vt(vt, dst),
    }
}

fn read_serial(dst: &mut [u8]) -> usize {
    let first = with_current_mut(ProcessState::pop_unread).unwrap_or_else(|| {
        loop {
            if let Some(b) = serial_in::read_byte() {
                return b;
            }
            serial_in::wait();
        }
    });

    // Ctrl-D at start of read -> EOF (0 bytes).
    if first == 0x04 {
        return 0;
    }

    dst[0] = if first == b'\r' { b'\n' } else { first };
    let mut n = 1usize;

    // Drain any immediately queued bytes up to `dst.len()` or `\n`, never
    // blocking past the first available chunk (standard tty short-read).
    while n < dst.len() && dst[n - 1] != b'\n' {
        let Some(b) = serial_in::read_byte() else {
            break;
        };
        if b == 0x04 {
            with_current_mut(|proc| proc.push_unread_bytes(&[0x04]));
            break;
        }
        dst[n] = if b == b'\r' { b'\n' } else { b };
        n += 1;
    }
    n
}

fn read_vt(vt: usize, dst: &mut [u8]) -> usize {
    if let Some(first) = with_current_mut(ProcessState::pop_unread) {
        if first == 0x04 {
            return 0;
        }
        dst[0] = first;
        let mut n = 1;
        while n < dst.len() {
            let next = with_current_mut(|proc| {
                if proc.has_unread() && proc.unread[proc.unread_pos as usize] == 0x04 {
                    return None;
                }
                proc.pop_unread()
            });
            let Some(b) = next else {
                break;
            };
            dst[n] = b;
            n += 1;
        }
        return n;
    }
    loop {
        let key = crate::vt::wait_key(vt);
        let mut enc = [0u8; 4];
        let len = vt_key_to_bytes(key, &mut enc);
        if len == 0 {
            continue;
        }
        if len == 1 && enc[0] == 0x04 {
            return 0;
        }
        let n = len.min(dst.len());
        dst[..n].copy_from_slice(&enc[..n]);
        if n < len {
            with_current_mut(|proc| proc.push_unread_bytes(&enc[n..len]));
        }
        return n;
    }
}
