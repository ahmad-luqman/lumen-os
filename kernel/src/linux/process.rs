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

use lumen_vfs::{FdTable, OpenFile, OpenFlags, OpenGuard, VfsError, VfsInode, open_inode_guarded};

use crate::keyboard::{Key as KbdKey, KeyCode};
use crate::klog;
use crate::linux::general;
use crate::mm::user_vm::AddressSpace;
use crate::sched;
use crate::serial_in;
use crate::sync::SpinLock;
use crate::term;
use crate::user::UserExit;
use crate::vfs::KernelVfsLock;

/// An open file description as the kernel's FD tables hold it (Lock 2 its
/// offset and directory cursor).
pub type KernelFile = OpenFile<KernelVfsLock<u64>>;

/// A descriptor's open file description: shared by the slots `dup` (M8)
/// fills, its `fd_refs` counting them.
pub type FileRef = Arc<KernelFile>;

/// A new process's file mode creation mask (`umask`).
pub const DEFAULT_UMASK: u16 = 0o022;

/// Maximum captured stdout bytes for `usertest` runs.
const CAPTURE_STDOUT_LIMIT: usize = 4096;

/// A process's working directory: an open reference on its inode (so an
/// `rmdir` detaches it rather than freeing it) and the path it was set by.
/// `getcwd` computes the path afresh from the inode (`compute_cwd_path`);
/// the stored one names it for a relative open's `/proc/<pid>/fd` path.
pub struct Cwd {
    guard: OpenGuard,
    path: String,
}

impl Cwd {
    pub fn new(guard: OpenGuard, path: String) -> Self {
        Self { guard, path }
    }

    pub fn inode(&self) -> &Arc<dyn VfsInode> {
        self.guard.inode()
    }

    pub fn path(&self) -> &str {
        &self.path
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
    /// Open descriptors (`lumen_vfs::MAX_FDS` slots): 0, 1 and 2 on
    /// `/dev/console` at the start.
    pub fds: FdTable<FileRef>,
    /// The working directory (`None` only if `/` couldn't be opened).
    pub cwd: Option<Cwd>,
    /// The file mode creation mask (`0o777` bits).
    pub umask: u16,
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
    #[allow(clippy::too_many_arguments)]
    fn new(
        pid: u32,
        io: ConsoleIo,
        allow_test_syscalls: bool,
        stdin: Vec<u8>,
        address_space: Option<Arc<AddressSpace>>,
        exe_path: Option<String>,
        fds: FdTable<FileRef>,
        cwd: Option<Cwd>,
    ) -> Self {
        Self {
            pid,
            ppid: 0,
            tid: pid,
            fds,
            cwd,
            umask: DEFAULT_UMASK,
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

/// Open `/dev/console` for a new process's console descriptor, with `flags`
/// (the access mode). The guarded open, as `openat`'s.
fn open_console(flags: u32) -> Result<FileRef, VfsError> {
    let opened = open_inode_guarded(crate::vfs::tables(), None, b"/dev/console", OpenFlags::from_bits(flags), 0, 0)?;
    Ok(Arc::new(OpenFile::from_opened(opened)?))
}

/// What a new process starts with: fd 0 on `/dev/console` read-only, fds 1
/// and 2 write-only (so `read(1)` and `write(0)` are `-EBADF`, as in M6),
/// and `/` as its working directory. Opens files, so may sleep: never call
/// it holding `ACTIVE_PROCS`. A failure is logged and leaves that slot
/// closed (the VFS mounts `/dev` at boot, before anything runs).
fn initial_files() -> (FdTable<FileRef>, Option<Cwd>) {
    let mut fds = FdTable::new();
    for (fd, flags) in [(0, OpenFlags::O_RDONLY), (1, OpenFlags::O_WRONLY), (2, OpenFlags::O_WRONLY)] {
        match open_console(flags) {
            // A fresh table: nothing is displaced.
            Ok(file) => drop(fds.insert_at(fd, file, false)),
            Err(e) => log::warn!(target: "linux", "fd {fd}: cannot open /dev/console: {e:?}"),
        }
    }
    let cwd = match crate::vfs::resolve_path_from_root("/").and_then(OpenGuard::try_new) {
        Ok(guard) => Some(Cwd::new(guard, String::from("/"))),
        Err(e) => {
            log::warn!(target: "linux", "cannot open / as the working directory: {e:?}");
            None
        }
    };
    (fds, cwd)
}

/// Register a fresh [`ProcessState`] for the current user thread, with the
/// executable's path for `/proc/<pid>/exe` if it was loaded by path, and
/// return its PID.
pub fn register_current(io: ConsoleIo, allow_test_syscalls: bool, stdin: Vec<u8>, exe_path: Option<String>) -> u32 {
    let ktid = current_kernel_tid();
    let pid = NEXT_PID.fetch_add(1, Ordering::Relaxed);
    let address_space = sched::current_address_space();
    let (fds, cwd) = initial_files();
    let state = ProcessState::new(pid, io, allow_test_syscalls, stdin, address_space, exe_path, fds, cwd);
    let prev = ACTIVE_PROCS.lock().insert(ktid, state);
    debug_assert!(prev.is_none(), "ring-3 process registered twice on thread {ktid}");
    pid
}

/// Remove and return the current thread's [`ProcessState`] after ring-3
/// exits, its descriptors closed (each lock released as `close` does, outside
/// `ACTIVE_PROCS`); its working directory goes when it drops.
pub fn take_current() -> ProcessState {
    let ktid = current_kernel_tid();
    let mut proc = ACTIVE_PROCS.lock().remove(&ktid).expect("active process state missing after ring-3 exit");
    for entry in proc.fds.drain_all() {
        release_fd(proc.pid, entry.file);
    }
    proc
}

/// Give back one descriptor's slot on `file`: `pid`'s POSIX record locks on
/// its inode go on any close (as Linux), and its `flock` locks with the last
/// slot of the open file description. Takes the inode's Lock 4: never call
/// it holding `ACTIVE_PROCS` (or any spinlock).
fn release_fd(pid: u32, file: FileRef) {
    let last = file.dec_fd_ref() == 0;
    let id = file.id;
    // An inode without lock state has nothing to release.
    let _ = file.inode.with_locks_mut(&mut |locks| {
        locks.release_posix_for_pid(pid);
        if last {
            locks.release_flock(id);
        }
    });
}

/// The current process's open file on `fd` (a clone, taken and released
/// under `ACTIVE_PROCS`), or `None` if `fd` isn't open.
pub fn current_file(fd: i32) -> Option<FileRef> {
    with_current(|p| p.fds.get(fd).map(|e| Arc::clone(&e.file)))
}

/// Whether the current process has a free descriptor slot (`openat` checks
/// before it opens, as Linux reserves the slot first: a full table is
/// `-EMFILE` before any file is created).
pub fn current_has_free_fd() -> bool {
    with_current(|p| p.fds.open_count() < lumen_vfs::MAX_FDS)
}

/// Put `file` (a fresh open file description, its `fd_refs` 1) in the
/// current process's lowest free slot: the descriptor, or `EMFILE`.
pub fn install_current_fd(file: FileRef, cloexec: bool) -> Result<i32, VfsError> {
    // A clone goes in, so a refused one drops under `ACTIVE_PROCS` without
    // being the last reference; `file` drops after the lock is released.
    with_current_mut(|p| p.fds.alloc(Arc::clone(&file), cloexec))
}

/// Put `file` on descriptor `fd` of the current process, closing what was
/// there (its locks released outside `ACTIVE_PROCS`). `file`'s `fd_refs` must
/// already count this slot (a fresh open file's 1 does). `EBADF` for an fd
/// outside `0..lumen_vfs::MAX_FDS`: procfs builds `/proc/<pid>/fd/<fd>`'s
/// inode number from it and relies on that range.
pub fn insert_current_fd(fd: i32, file: FileRef) -> Result<(), VfsError> {
    let (pid, displaced) = with_current_mut(|p| (p.pid, p.fds.insert_at(fd, Arc::clone(&file), false)));
    if let Some(old) = displaced? {
        release_fd(pid, old.file);
    }
    Ok(())
}

/// Close descriptor `fd` of the current process (`close`): `EBADF` if it
/// isn't open. The entry is taken out under `ACTIVE_PROCS` and released after.
pub fn close_current_fd(fd: i32) -> Result<(), VfsError> {
    let (pid, removed) = with_current_mut(|p| (p.pid, p.fds.remove(fd)));
    release_fd(pid, removed?.file);
    Ok(())
}

/// The current process's working directory: its inode and the path it was
/// set by (clones), or `None` if it has none.
pub fn current_cwd() -> Option<(Arc<dyn VfsInode>, String)> {
    with_current(|p| p.cwd.as_ref().map(|c| (Arc::clone(c.inode()), String::from(c.path()))))
}

/// Make `cwd` the current process's working directory; the old one drops
/// after `ACTIVE_PROCS` is released. (`chdir` and `fchdir` once they land;
/// the ktests until then.)
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // `chdir`/`fchdir` (#254's wave 3) call it
pub fn set_current_cwd(cwd: Cwd) {
    let old = with_current_mut(|p| p.cwd.replace(cwd));
    drop(old);
}

/// The current process's file mode creation mask.
pub fn current_umask() -> u16 {
    with_current(|p| p.umask)
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

/// Return sorted open descriptor numbers for `pid`.
pub fn process_open_fds(pid: u32) -> Vec<i32> {
    let guard = ACTIVE_PROCS.lock();
    guard.values().find(|p| p.pid == pid).map(|p| p.fds.iter_open().map(|(fd, _)| fd).collect()).unwrap_or_default()
}

/// Snapshot `(Option<Arc<dyn VfsInode>>, display_path)` for `/proc/<pid>/fd/<fd>`:
/// the open file's inode and the path it was opened by, ` (deleted)` added
/// once unlinked.
///
/// Releases `ACTIVE_PROCS` before inspecting `VfsInode::stat()` or dropping
/// the file it cloned.
pub fn process_fd_target(pid: u32, fd: i32) -> Option<(Option<Arc<dyn VfsInode>>, String)> {
    let file = {
        let guard = ACTIVE_PROCS.lock();
        let proc = guard.values().find(|p| p.pid == pid)?;
        Arc::clone(&proc.fds.get(fd)?.file)
    };
    let inode = Arc::clone(&file.inode);
    let mut path = String::from(&*file.path);
    drop(file);
    let unlinked = inode.is_detached() || inode.stat().ok().is_some_and(|m| m.nlink == 0);
    if unlinked && !path.ends_with(" (deleted)") {
        path.push_str(" (deleted)");
    }
    Some((Some(inode), path))
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

/// Where [`in_test_process`] maps user memory for a ktest (read-write).
#[cfg(feature = "ktest")]
pub(crate) const KTEST_USER_VA: u64 = 0x0000_0000_0060_0000;

/// How much user memory [`in_test_process`] maps at [`KTEST_USER_VA`].
#[cfg(feature = "ktest")]
pub(crate) const KTEST_USER_LEN: u64 = 4 * 4096;

/// Run `f` as a Linux process for a ktest: on a user thread with its own
/// address space ([`KTEST_USER_LEN`] bytes mapped at [`KTEST_USER_VA`], so
/// handlers can copy to and from user memory) and a registered
/// [`ProcessState`] (`ConsoleIo::Capture`), handed its pid. The process is
/// taken down (`take_current`, closing its descriptors) when `f` returns,
/// whatever it returned, and `f`'s result comes back.
#[cfg(feature = "ktest")]
pub(crate) fn in_test_process<R: Send + 'static>(f: impl FnOnce(u32) -> R + Send + 'static) -> Result<R, &'static str> {
    use lumen_vm::Prot;

    let space = Arc::new(AddressSpace::new().map_err(|_| "out of frames for AddressSpace")?);
    if space.map_eager_with_bytes(KTEST_USER_VA, KTEST_USER_LEN, Prot::READ | Prot::WRITE, &[]).is_err() {
        space.teardown();
        return Err("map the test process's user memory");
    }
    let slot: Arc<SpinLock<Option<R>>> = Arc::new(SpinLock::new(None));
    let out = Arc::clone(&slot);
    let spawned = sched::spawn_user_on(crate::arch::cpu_index(), "ktest-process", Arc::clone(&space), move || {
        let pid = register_current(ConsoleIo::Capture, false, Vec::new(), None);
        let result = f(pid);
        drop(take_current());
        drop(sched::detach_user_space());
        *out.lock() = Some(result);
    });
    let result = match spawned {
        Ok(handle) => {
            handle.join();
            slot.lock().take().ok_or("the test process ended without a result")
        }
        Err(_) => Err("spawn the test process"),
    };
    space.teardown();
    result
}

crate::ktest!(
    fn process_starts_with_console_fds_and_root_cwd() {
        use crate::{kassert, kassert_eq};
        in_test_process(|pid| {
            // fds 0, 1 and 2 on `/dev/console`: 0 read-only, 1 and 2
            // write-only (M6's `EBADF` directions), none close-on-exec;
            // nothing else open.
            kassert_eq!(process_open_fds(pid), [0, 1, 2]);
            for (fd, readable, writable) in [(0, true, false), (1, false, true), (2, false, true)] {
                let file = current_file(fd).ok_or("a console fd isn't open")?;
                kassert!(file.inode.is_tty());
                kassert_eq!(&*file.path, "/dev/console");
                kassert_eq!((file.is_readable(), file.is_writable()), (readable, writable));
                kassert_eq!(with_current(|p| p.fds.get_cloexec(fd)), Ok(false));
            }
            // `/` is the working directory, with the default umask.
            let (cwd, path) = current_cwd().ok_or("no working directory")?;
            let root = crate::vfs::resolve_path_from_root("/").map_err(|_| "resolve /")?;
            kassert!(Arc::ptr_eq(&cwd, &root));
            kassert_eq!(path.as_str(), "/");
            kassert_eq!(current_umask(), 0o022);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn process_close_releases_flock_on_last_slot_and_posix_locks_on_any() {
        use crate::{kassert, kassert_eq};
        use lumen_vfs::{FlockKind, FlockOp, FlockSpec, LockContext};

        const PATH: &[u8] = b"/tmp/process-ktest-locks";
        const OTHER: &[u8] = b"/tmp/process-ktest-locks-other";
        fn open(path: &[u8]) -> Result<FileRef, VfsError> {
            let flags = OpenFlags::from_bits(OpenFlags::O_RDWR | OpenFlags::O_CREAT);
            let opened = open_inode_guarded(crate::vfs::tables(), None, path, flags, 0o644, 0)?;
            Ok(Arc::new(OpenFile::from_opened(opened)?))
        }
        // The `flock` and `pid`'s POSIX record-lock counts on `file`'s inode.
        fn counts(file: &FileRef, pid: u32) -> (usize, usize) {
            let mut got = (usize::MAX, usize::MAX);
            let _ = file.inode.with_locks_mut(&mut |l| got = (l.flock_count(), l.posix_lock_count(pid)));
            got
        }
        fn posix_lock(file: &FileRef, pid: u32) -> Result<(), VfsError> {
            let spec = FlockSpec { l_type: general::F_WRLCK as i16, l_whence: 0, l_start: 0, l_len: 10, l_pid: 0 };
            let ctx = LockContext { cur_offset: 0, file_size: 0, readable: true, writable: true };
            let mut res = Err(VfsError::Io);
            file.inode.with_locks_mut(&mut |l| res = l.setlk(pid, spec, ctx, false).map(drop))?;
            res
        }
        fn flock_ex(file: &FileRef) -> Result<(), VfsError> {
            let ex = FlockOp { kind: Some(FlockKind::Exclusive), nonblock: true };
            let mut res = Err(VfsError::Io);
            file.inode.with_locks_mut(&mut |l| res = l.flock(file.id, ex).map(drop))?;
            res
        }

        let (pid, held) = in_test_process(|pid| -> Result<(u32, FileRef), &'static str> {
            // A holds an exclusive `flock` and sits on fds 3 and 4 (a `dup`:
            // one open file description, two slots).
            let a = open(PATH).map_err(|_| "open A")?;
            flock_ex(&a).map_err(|_| "flock A")?;
            insert_current_fd(3, Arc::clone(&a)).map_err(|_| "install fd 3")?;
            a.inc_fd_ref();
            insert_current_fd(4, Arc::clone(&a)).map_err(|_| "install fd 4")?;
            // B, another open of the same file, on fd 5 holds a POSIX lock;
            // a file on another inode on fd 6.
            let b = open(PATH).map_err(|_| "open B")?;
            kassert_eq!(install_current_fd(Arc::clone(&b), false), Ok(5));
            kassert_eq!(install_current_fd(open(OTHER).map_err(|_| "open other")?, false), Ok(6));
            posix_lock(&b, pid).map_err(|_| "take the POSIX lock")?;
            kassert_eq!(counts(&a, pid), (1, 1));

            // Closing another inode's fd releases nothing here.
            kassert_eq!(close_current_fd(6), Ok(()));
            kassert_eq!(counts(&a, pid), (1, 1));
            // Closing fd 3 (A's first slot) drops the POSIX lock B took (any
            // close of the inode), not A's `flock` (fd 4 still holds A).
            kassert_eq!(close_current_fd(3), Ok(()));
            kassert_eq!(counts(&a, pid), (1, 0));
            kassert_eq!(a.fd_ref_count(), 1);
            // Its last slot closed, A's `flock` goes too.
            posix_lock(&b, pid).map_err(|_| "take the POSIX lock again")?;
            kassert_eq!(close_current_fd(4), Ok(()));
            kassert_eq!(counts(&a, pid), (0, 0));
            kassert_eq!(close_current_fd(4), Err(VfsError::BadF));

            // B keeps a `flock` and a POSIX lock to the end: exit closes it.
            flock_ex(&b).map_err(|_| "flock B")?;
            posix_lock(&b, pid).map_err(|_| "take the POSIX lock at exit")?;
            kassert_eq!(counts(&b, pid), (1, 1));
            Ok((pid, b))
        })??;
        // The process's exit released both.
        kassert_eq!(counts(&held, pid), (0, 0));
        drop(held);
        for path in [PATH, OTHER] {
            kassert!(lumen_vfs::unlink_at(crate::vfs::tables(), None, path, false).is_ok());
        }
        Ok(())
    }
);
