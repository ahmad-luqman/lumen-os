//! Linux processes and threads (M8's split, #372), the console I/O bridge
//! and exit tracking (M6), and the VFS-backed FD table, working directory
//! and umask (M7, #254).
//!
//! A [`Process`] is a Linux thread group: one pid and what its threads
//! share (address space, descriptors, working directory and umask,
//! dispositions, console state). A [`UserThread`] is one ring-3 thread's
//! Linux identity, held in its scheduler thread's user state
//! (`sched::current_user_thread`), so a syscall finds its process without a
//! table lookup. `PROCS` holds every process by pid and every user thread
//! by tid, with lumen-proc's [`ProcTree`]; pid 1 is the kernel's
//! (`start_init`, from `linux::init`), a process with no threads. Each
//! `linux` run registers a root process on its thread
//! ([`register_current`]) and takes it down when ring 3 exits or faults
//! ([`take_current`]).
//!
//! Each type's fields are written only through its own methods
//! (`Process::record_exit`, `SignalState::add_thread`, `FsInner::swap_umask`,
//! `ProcTable::link_root` and the like), though Rust's privacy reaches the
//! file; child modules enforce the table, file and signal owners' privacy.
//!
//! Lock discipline (ARCHITECTURE section 9, level 10): `PROCS`, then a
//! `Process::signal`, then the leaves (a process's `mm`, `files`, `fs`,
//! `exe_path`, `console`, `exit` and `vfork_done` slots, `Files::table`,
//! `FsState::inner`), never two leaves at once. All are IRQ-off
//! [`SpinLock`]s: never held across `copy_from_user`, `copy_to_user`,
//! blocking waits, or `AddressSpace` operations; nor across anything that
//! takes a VFS lock (an open, `stat`, an inode's lock state): an open file
//! or cwd leaves its lock as a clone or an extracted entry, used and
//! dropped after. `files_access` leases precede these locks and may sleep;
//! never acquire a lease while holding a spinlock or with interrupts off.
//! Close takes the process lease, then Files::close_access shared; leave
//! takes only close_access exclusive, with no shared lease or inode lock held.

mod clone;
mod files;
pub(crate) use clone::{ProcessClone, clone_flags};
mod signal;
mod table;
#[path = "timer.rs"]
pub mod timer;

use files::FilesAccess;
pub use files::{Files, FsInner, FsState, ReservedFd};
#[cfg(feature = "ktest")]
pub use signal::send_thread;
pub use signal::{
    Delivery, HandlerDelivery, Rlimit, RlimitKind, SignalState, current_pending, send_current_signal,
    send_process_checked, send_thread_checked,
};
use table::ProcTable;

use alloc::collections::BTreeMap;
#[cfg(feature = "ktest")]
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use log::{info, warn};
use lumen_proc::{ExitAction, ExitError, ExitStatus, INIT_PID, ProcState, ProcTree, RusageSlot};
#[cfg(feature = "ktest")]
use lumen_signal::SigSet;
use lumen_signal::{Blocked, Signal};
use lumen_vfs::{
    Escaped, FdReservation, FdTable, InodeType, OpenFile, OpenFlags, OpenGuard, VfsError, VfsInode, open_inode_guarded,
};

use crate::keyboard::{Key as KbdKey, KeyCode};
use crate::klog;
use crate::linux::{errno, general};
use crate::mm::user_vm::AddressSpace;
use crate::sched;
use crate::serial_in;
use crate::sync::SpinLock;
use crate::term;
use crate::user::UserExit;
use crate::vfs::KernelVfsLock;

/// An open file description as the kernel's FD tables hold it (Lock 2 its
/// offset and directory cursor, Lock 0 its whole-call position), together with
/// its combined end reference count and optional pipe/FIFO end ownership.
pub struct KernelFile {
    inner: OpenFile<KernelVfsLock<u64>>,
    /// Active in-flight syscall references (`Fd::file`, `poll`).
    #[cfg(feature = "ktest")]
    io_refs: AtomicU32,
    /// Combined descriptor + in-flight syscall count (`fd_refs + io_refs`),
    /// starting at 1. Transitioning 1 -> 0 runs pipe/FIFO end release once.
    end_refs: AtomicU32,
    /// Shared pipe state for anonymous pipes and opened (non-`O_PATH`) FIFOs.
    pipe: Option<Arc<crate::linux::pipe::PipeObject>>,
    /// Zero except for a nonblocking FIFO reader opened with no writer,
    /// which snapshots w_opens. Otherwise w_opens >= 1 when open returns.
    /// Copied from PipeEnd so polling need not take the pipe_end lock.
    writer_generation: u64,
    /// End ownership released once when `end_refs` reaches 0.
    pipe_end: SpinLock<Option<crate::linux::pipe::PipeEnd>>,
    tty: Option<crate::vfs::devpts::TtyEnd>,
}

impl KernelFile {
    fn from_open_file(inner: OpenFile<KernelVfsLock<u64>>) -> Self {
        Self {
            inner,
            #[cfg(feature = "ktest")]
            io_refs: AtomicU32::new(0),
            end_refs: AtomicU32::new(1),
            pipe: None,
            writer_generation: 0,
            pipe_end: SpinLock::new(None),
            tty: None,
        }
    }

    /// Wrap an already-checked VFS open (`open_inode_guarded`).
    pub fn from_opened(opened: lumen_vfs::Opened) -> Result<Self, VfsError> {
        Ok(Self::from_open_file(OpenFile::from_opened(opened)?))
    }

    /// Construct a fresh open file description without open-time checks (ktests).
    #[cfg(feature = "ktest")]
    pub fn new_unchecked_flags(inode: Arc<dyn VfsInode>, path: Arc<[u8]>, flags: OpenFlags) -> Result<Self, VfsError> {
        Ok(Self::from_open_file(OpenFile::new_unchecked_flags(inode, path, flags)?))
    }

    /// Construct an open file description for one end of an anonymous pipe.
    pub fn new_pipe_end(
        inode: Arc<dyn VfsInode>,
        path: Arc<[u8]>,
        flags: OpenFlags,
        end: crate::linux::pipe::PipeEnd,
    ) -> Result<Self, VfsError> {
        let mut file = Self::from_open_file(OpenFile::new_unchecked_flags(inode, path, flags)?);
        file.attach_pipe_end(end);
        Ok(file)
    }

    /// Attach an opened FIFO/pipe end before installing into an FD table.
    pub fn attach_pipe_end(&mut self, end: crate::linux::pipe::PipeEnd) {
        debug_assert!(self.pipe.is_none() && self.pipe_end.get_mut().is_none());
        self.writer_generation = end.writer_generation();
        self.pipe = Some(Arc::clone(end.pipe()));
        *self.pipe_end.get_mut() = Some(end);
    }

    pub(crate) fn new_tty_end(
        inode: Arc<dyn VfsInode>,
        path: Arc<[u8]>,
        flags: OpenFlags,
        end: crate::vfs::devpts::TtyEnd,
    ) -> Result<Self, VfsError> {
        let mut file = Self::from_open_file(OpenFile::new_unchecked_flags(inode, path, flags)?);
        file.tty = Some(end);
        Ok(file)
    }

    pub(crate) fn tty(&self) -> Option<&crate::vfs::devpts::TtyEnd> {
        self.tty.as_ref()
    }

    pub fn is_nonblock(&self) -> bool {
        self.status_flags() & OpenFlags::O_NONBLOCK != 0
    }

    pub fn writer_generation(&self) -> u64 {
        self.writer_generation
    }

    /// Borrow the underlying `PipeObject` if this is a pipe or opened FIFO.
    pub fn pipe(&self) -> Option<&Arc<crate::linux::pipe::PipeObject>> {
        self.pipe.as_ref()
    }

    /// Count of descriptors sharing this description, raising both `fd_refs`
    /// and the combined end-lifetime count (`dup`, `fork`, `exec`).
    /// Caller holds the FD table lock with an existing counted descriptor.
    pub fn inc_fd_ref(&self) {
        self.inner.inc_fd_ref();
        let prev = self.end_refs.fetch_add(1, Ordering::Relaxed);
        debug_assert!(prev > 0, "inc_fd_ref after last pipe end release");
    }

    /// Release one descriptor reference; if neither descriptors nor in-flight
    /// syscalls remain, releases any attached pipe/FIFO end in thread context.
    /// May take Lock 0: never hold locks 1–10 or OpenFile::pos here.
    pub fn dec_fd_ref(&self) -> u32 {
        let rem = self.inner.dec_fd_ref();
        if self.end_refs.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.release_pipe_end();
        }
        rem
    }

    /// Current in-flight syscall reference count.
    #[cfg(feature = "ktest")]
    pub fn io_refs(&self) -> u32 {
        self.io_refs.load(Ordering::Acquire)
    }

    fn inc_io_ref(&self) {
        let prev = self.end_refs.fetch_add(1, Ordering::Relaxed);
        debug_assert!(prev > 0, "inc_io_ref after last pipe end release");
        #[cfg(feature = "ktest")]
        self.io_refs.fetch_add(1, Ordering::Relaxed);
    }

    fn dec_io_ref(&self) {
        #[cfg(feature = "ktest")]
        self.io_refs.fetch_sub(1, Ordering::Relaxed);
        if self.end_refs.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.release_pipe_end();
        }
    }

    fn release_pipe_end(&self) {
        let end = self.pipe_end.lock().take();
        if let Some(end) = end {
            end.release();
        }
        if let Some(end) = &self.tty {
            end.release();
        }
    }
}

impl core::ops::Deref for KernelFile {
    type Target = OpenFile<KernelVfsLock<u64>>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl core::ops::DerefMut for KernelFile {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Drop for KernelFile {
    fn drop(&mut self) {
        if let Some(end) = self.pipe_end.get_mut().take() {
            // Only a never-installed description retains its initial reference.
            debug_assert_eq!(*self.end_refs.get_mut(), 1);
            debug_assert_eq!(self.inner.fd_ref_count(), 1);
            end.release();
        }
    }
}

/// A descriptor's open file description: shared by the slots `dup` (M8)
/// fills, its `fd_refs` counting them.
pub type FileRef = Arc<KernelFile>;

/// An in-flight syscall reference to an open file description (`Fd::file`,
/// `poll`/`select`): raises `KernelFile::end_refs` under `Files::table` and releases it
/// on drop, running pipe/FIFO end release if all descriptors closed meanwhile.
pub struct IoFile(FileRef);

impl IoFile {
    /// Caller holds the FD table lock over an existing descriptor.
    fn new(file: &FileRef) -> Self {
        file.inc_io_ref();
        Self(Arc::clone(file))
    }
}

impl core::ops::Deref for IoFile {
    type Target = KernelFile;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// Dropping may take Lock 0: no locks 1–10 or OpenFile::pos may be held.
impl Drop for IoFile {
    fn drop(&mut self) {
        self.0.dec_io_ref();
    }
}

/// A new process's file mode creation mask (`umask`).
pub const DEFAULT_UMASK: u16 = 0o022;

/// Maximum captured stdout bytes for `usertest` runs.
const CAPTURE_STDOUT_LIMIT: usize = 4096;

/// A process's working directory: an open reference on a directory inode
/// (so an `rmdir` detaches it rather than freeing it) and its path, as
/// `chdir`/`fchdir` computed it from the inode when they set it (`/` for a
/// new process). `getcwd` computes the path afresh from the inode
/// (`compute_cwd_path`); the stored one is what a relative open's
/// `/proc/<pid>/fd` path is joined to.
pub struct Cwd {
    guard: OpenGuard,
    path: Arc<[u8]>,
}

impl Cwd {
    /// The working directory `guard` holds open, its path `path`: `ENOTDIR`
    /// unless it is a directory.
    pub fn new(guard: OpenGuard, path: &[u8]) -> Result<Self, VfsError> {
        if guard.inode().inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Ok(Self { guard, path: Arc::from(path) })
    }

    pub fn inode(&self) -> &Arc<dyn VfsInode> {
        self.guard.inode()
    }

    pub fn path(&self) -> &Arc<[u8]> {
        &self.path
    }
}

/// Open `path` (from `/`) into a fresh open file description with `flags`
/// and, if it creates the file, `mode`: the guarded open `openat` makes,
/// for kernel callers (a process's stdio, `vfstest`, the ktests). May sleep.
pub fn open_kernel_file(path: &[u8], flags: u32, mode: u16) -> Result<KernelFile, VfsError> {
    let opened = open_inode_guarded(crate::vfs::tables(), None, path, OpenFlags::from_bits(flags), mode, 0)?;
    KernelFile::from_opened(opened)
}

/// What a process starts with, opened before it is registered: fd 0 on
/// `/dev/console` read-only, fds 1 and 2 write-only (so `read(1)` and
/// `write(0)` are `-EBADF`, as in M6), and `/` as its working directory.
/// A process never starts without all four: an empty stdio slot would be
/// the first `open`'s, and output meant for the console would go to a file.
pub struct Stdio {
    fds: FdTable<FileRef>,
    cwd: Cwd,
}

impl Stdio {
    /// Open the console three times and `/`: the first error, logged (the
    /// VFS mounts `/dev` at boot, so only a broken boot sees one). Opens
    /// files, so may sleep: never call it holding a spinlock.
    pub fn open() -> Result<Self, VfsError> {
        Self::open_console(ConsoleIo::Capture)
    }

    /// Open each stdio description once, binding physical consoles to their tty.
    pub(crate) fn open_console(io: ConsoleIo) -> Result<Self, VfsError> {
        let opened = match crate::tty::console(io) {
            Some(tty) => Self::open_bound(b"/dev/console", Some(tty)),
            None => Self::open_from(b"/dev/console"),
        };
        if let Err(e) = &opened {
            log::error!(target: "linux", "cannot open a process's stdio (/dev/console) and working directory (/): {e:?}");
        }
        opened
    }

    /// [`Stdio::open`] with `console` for the console's path, not logged:
    /// the seam the refusal's ktest opens a missing path through.
    fn open_from(console: &[u8]) -> Result<Self, VfsError> {
        Self::open_bound(console, None)
    }

    fn open_bound(console: &[u8], tty: Option<Arc<crate::tty::Tty>>) -> Result<Self, VfsError> {
        let mut fds = FdTable::new();
        for (fd, flags) in [(0, OpenFlags::O_RDONLY), (1, OpenFlags::O_WRONLY), (2, OpenFlags::O_WRONLY)] {
            let file = open_kernel_file(console, flags, 0)?;
            let file = match &tty {
                Some(tty) => crate::vfs::devpts::console_file(file, Arc::clone(tty)).map_err(|_| VfsError::Io)?,
                None => file,
            };
            let file = Arc::new(file);
            // A fresh table: nothing is displaced.
            drop(fds.insert_at(fd, file, false)?);
        }
        let root = crate::vfs::resolve_path_from_root("/")?;
        Ok(Self { fds, cwd: Cwd::new(OpenGuard::try_new(root)?, b"/")? })
    }
}

/// Where a ring-3 process's console fds (0, 1, 2) connect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleIo {
    /// `usertest` capture mode: stdout captured into `ConsoleState::stdout`,
    /// stdin served from pre-seeded `ConsoleState::stdin`.
    Capture,
    /// Interactive serial console (`serial_in` + `klog::LockedSerial`).
    Serial,
    /// Interactive virtual terminal, validated before console ownership starts.
    Vt(VtId),
}

/// A console VT index checked before constructing a physical console route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VtId(usize);

impl VtId {
    pub fn new(index: usize) -> Option<Self> {
        (index < crate::term::VTS).then_some(Self(index))
    }

    pub fn index(self) -> usize {
        self.0
    }
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

/// `SIG_IGN` as a `sa_handler` value (Linux's `(void (*)(int))1`).
const SIG_IGN: u64 = 1;

/// The largest pid (and tid) `PROCS`' tree hands out: pids and tids run
/// `2..=PID_MAX` (pid 1 is `start_init`'s), as Linux's default `pid_max`
/// hands out `1..32767`; lumen-proc wraps back to 2, Linux to 300.
pub const PID_MAX: u32 = 32767;

/// A Linux thread group: one pid and what its threads share. Its `mm`,
/// `files` and `fs` slots are emptied by its takedown (`take_current`),
/// in thread context, so dropping the last reference anywhere frees memory
/// only (a debug assertion in `Drop`, pid 1 aside).
pub struct Process {
    /// Stored once (`set_pid`): a root's in the `PROCS` hold that
    /// allocates it, before its thread enters ring 3; pid 1's by
    /// `start_init` before it is linked.
    pid: AtomicU32,
    /// Its address space, whose one process user it holds (B4's
    /// `AddressSpace::drop_user` at its takedown): the `Arc` its thread also
    /// holds in its scheduler state. Pid 1's is an empty placeholder that is
    /// never loaded.
    mm: SpinLock<Option<Arc<AddressSpace>>>,
    /// Its descriptors.
    files: SpinLock<Option<Arc<Files>>>,
    /// Shared leases span fd operations/reservations; an exclusive lease orders table replacement.
    files_access: FilesAccess,
    /// Its working directory and umask.
    fs: SpinLock<Option<Arc<FsState>>>,
    /// Its signal dispositions, and each thread's mask.
    signal: SpinLock<SignalState>,
    /// The executable's canonical byte path for `/proc/<pid>/exe`, when known.
    /// A snapshot: subsequent renames can leave it stale.
    exe_path: SpinLock<Option<Arc<[u8]>>>,
    /// Its `RusageSlot`'s value in `PROCS`' tree.
    usage_slot: u64,
    dead_ticks: [AtomicU64; 2],
    children_ticks: [AtomicU64; 2],
    /// The exited leader's nice, retained until the process is reaped.
    exited_leader_nice: SpinLock<Option<i8>>,
    /// M7's console state (until the console is a tty).
    console: SpinLock<ConsoleState>,
    allow_test_syscalls: bool,
    /// The recorded exit diagnostic/status. A pending fatal signal under
    /// `signal` overrides an ordinary exit until the last thread unlinks.
    exit: SpinLock<Option<UserExit>>,
    /// Ordinary leader exit, used only when no group-end decision wins.
    leader_exit: SpinLock<Option<UserExit>>,
    live_threads: AtomicUsize,
    exec_wait: sched::WaitQueue,
    /// A shell/root runner remains alive after its Linux thread leaves, so
    /// its join reports the whole group's outcome and captured output.
    run_waiting: core::sync::atomic::AtomicBool,
    run_result: SpinLock<Option<Ended>>,
    run_wait: sched::WaitQueue,
    /// Waiters for child state changes (wait4/waitid).
    child_events: sched::WaitQueue,
    /// Published before a child-event wake to close the check/enqueue race.
    child_seq: AtomicU64,
    /// Mirror of group_stop, published under signal before any park or wake.
    stopped: core::sync::atomic::AtomicBool,
    /// Advances on cancellation so a new stop cannot hide a previous continue.
    stop_seq: AtomicU64,
    stop_wait: sched::WaitQueue,
    /// Taken once when a vfork child releases its parent.
    vfork_done: SpinLock<Option<Arc<VforkDone>>>,
}

impl Process {
    fn new(
        mm: Option<Arc<AddressSpace>>,
        files: Option<Arc<Files>>,
        fs: Option<Arc<FsState>>,
        signal: SignalState,
        console: ConsoleState,
        allow_test_syscalls: bool,
        exe_path: Option<Arc<[u8]>>,
    ) -> Self {
        Self {
            pid: AtomicU32::new(0),
            mm: SpinLock::new(mm),
            files: SpinLock::new(files),
            files_access: FilesAccess::new(),
            fs: SpinLock::new(fs),
            signal: SpinLock::new(signal),
            exe_path: SpinLock::new(exe_path),
            usage_slot: NEXT_USAGE_SLOT.fetch_add(1, Ordering::Relaxed),
            dead_ticks: [const { AtomicU64::new(0) }; 2],
            children_ticks: [const { AtomicU64::new(0) }; 2],
            exited_leader_nice: SpinLock::new(None),
            console: SpinLock::new(console),
            allow_test_syscalls,
            exit: SpinLock::new(None),
            leader_exit: SpinLock::new(None),
            live_threads: AtomicUsize::new(0),
            exec_wait: sched::WaitQueue::new(),
            run_waiting: core::sync::atomic::AtomicBool::new(false),
            run_result: SpinLock::new(None),
            run_wait: sched::WaitQueue::new(),
            child_events: sched::WaitQueue::new(),
            child_seq: AtomicU64::new(0),
            stopped: core::sync::atomic::AtomicBool::new(false),
            stop_seq: AtomicU64::new(0),
            stop_wait: sched::WaitQueue::new(),
            vfork_done: SpinLock::new(None),
        }
    }

    /// Store its pid, once: panics if it already has one.
    fn set_pid(&self, pid: u32) {
        if let Err(had) = self.pid.compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire) {
            panic!("process {had} given a second pid, {pid}");
        }
        let files = self.files.lock().clone();
        if let Some(files) = files {
            files.enrol(pid);
        }
    }

    /// Its pid (the thread group id): set before anything can read it.
    pub fn pid(&self) -> u32 {
        let pid = self.pid.load(Ordering::Acquire);
        debug_assert_ne!(pid, 0, "a process's pid read before it was set");
        pid
    }

    /// Read or atomically replace one per-process resource limit; returns the
    /// previous pair, or `EPERM` if a `Nofile` write exceeds [`lumen_vfs::MAX_FDS`].
    pub fn rlimit(&self, kind: RlimitKind, new: Option<Rlimit>) -> Result<Rlimit, u32> {
        self.signal.lock().rlimits.exchange(kind, new)
    }

    /// Current soft `RLIMIT_NOFILE`, bounded by [`lumen_vfs::MAX_FDS`].
    pub fn nofile_soft(&self) -> usize {
        let soft = self.signal.lock().rlimits.nofile().soft();
        debug_assert!(soft <= lumen_vfs::MAX_FDS as u64);
        soft as usize
    }

    /// CPU ticks accumulated by children reaped by this process.
    pub fn children_time(&self) -> [u64; 2] {
        self.children_ticks.each_ref().map(|t| t.load(Ordering::Relaxed))
    }

    /// Its parent's pid, or 0 without one (a root `linux` started, as M6's
    /// `getppid`). Takes `PROCS`: never call it holding a process lock.
    pub fn ppid(&self) -> u32 {
        PROCS.lock().parent(self.pid())
    }

    /// Whether its run may make `usertest`'s test syscalls.
    pub fn allow_test_syscalls(&self) -> bool {
        self.allow_test_syscalls
    }

    /// Record the first group-end decision and its exit diagnostic under siglock.
    /// An earlier fatal signal wins; a later signal cannot replace a recorded exit.
    pub fn record_exit(&self, exit: UserExit) {
        let tid =
            try_current_thread().filter(|t| core::ptr::eq(t.process().as_ref(), self)).map_or(self.pid(), |t| t.tid());
        self.record_group_exit(tid, exit);
    }

    fn record_group_exit(&self, tid: u32, exit: UserExit) {
        let mut signal = self.signal.lock();
        // do_group_exit rechecks group_exec_task under siglock: a sibling
        // reaching here after de_thread ends alone, even for a fatal dequeue.
        if !signal.ending() && signal.exec_waiter.is_some_and(|caller| caller != tid) {
            return;
        }
        if let Some(sig) = exit.signal() {
            signal.record_signal(sig);
        } else {
            signal.record_exit();
        }
        let _ = signal.cancel_stop(self);
        let exit = signal
            .fatal_signal()
            .map_or(exit, |sig| if exit.signal() == Some(sig) { exit } else { UserExit::Signaled(sig) });
        self.exit.lock().get_or_insert(exit);
        signal.notify_group_exit(tid);
    }

    /// Take the recorded outcome once during takedown, without reconciling other state.
    fn take_exit(&self) -> Option<UserExit> {
        let group = self.exit.lock().take();
        group.or_else(|| self.leader_exit.lock().take())
    }

    /// Its disposition of `signal`.
    pub fn action(&self, signal: Signal) -> KernelSigaction {
        self.signal.lock().action(signal)
    }

    pub(crate) fn controlling_tty(&self) -> Option<Arc<crate::tty::Tty>> {
        self.signal.lock().ctty.clone()
    }

    /// Replace the association under the signal lock; callers hold PROCS,
    /// and terminal-driven changes additionally hold the TTY lock.
    pub(crate) fn replace_controlling_tty(&self, tty: Option<Arc<crate::tty::Tty>>) {
        self.signal.lock().ctty = tty;
    }

    /// Its address space (a clone, dropped by the caller with no lock
    /// held), until its takedown.
    fn address_space(&self) -> Option<Arc<AddressSpace>> {
        self.mm.lock().clone()
    }

    /// `f` on its descriptor table, under the table's lock (a leaf but for
    /// the heap: nothing in `f` may take a lock or close a file); `None`
    /// after its takedown. Acquiring its lease may block: never call with
    /// interrupts off or any spinlock held.
    fn with_files<R>(&self, f: impl FnOnce(&mut FdTable<FileRef>) -> R) -> Option<R> {
        let _access = self.files_access.read();
        let files = self.files.lock().clone()?;
        Some(files.with_table(f))
    }

    /// `f` on its working directory and umask, under their lock (a leaf
    /// but for the heap: nothing in `f` may take a lock or drop a working
    /// directory); `None` after its takedown.
    fn with_fs<R>(&self, f: impl FnOnce(&mut FsInner) -> R) -> Option<R> {
        let fs = self.fs.lock().clone()?;
        Some(fs.with_inner(f))
    }

    /// Whether its `SIGCHLD` disposition is `SIG_IGN` or carries
    /// `SA_NOCLDWAIT`: its children's zombies leave at once
    /// (`ProcTree::exit`'s `autoreap`). Takes `Process::signal`: callable
    /// under `PROCS`.
    fn autoreaps(&self) -> bool {
        let chld = self.action(Signal::CHLD);
        chld.handler == SIG_IGN || chld.flags & u64::from(general::SA_NOCLDWAIT) != 0
    }

    /// Exit step 3, in thread context, its last thread already off its
    /// address space: give up its user of the space (torn down at the
    /// last, `AddressSpace::drop_user`), its POSIX record locks regardless of
    /// table users, its share of descriptors (the last sharer closes them
    /// all and releases flock, `Files::leave`) and its working
    /// directory, and forget its executable's path (Linux's
    /// `/proc/<pid>/exe` is `ENOENT` once `mm` is gone). Each slot is
    /// emptied under its lock and released after.
    fn release_resources(&self) {
        // A refused fork may retire an unlinked process (pid 0, no timers).
        self.signal.lock().timers.clear(self.pid.load(Ordering::Acquire), true);
        let mm = self.mm.lock().take();
        if let Some(mm) = mm {
            mm.drop_user();
        }
        let files = self.files.lock().take();
        if let Some(files) = files {
            let pid = self.pid.load(Ordering::Acquire);
            files.release_posix_for_pid(pid);
            files.leave(pid);
        }
        let fs = self.fs.lock().take();
        drop(fs);
        let exe = self.exe_path.lock().take();
        drop(exe);
    }

    fn inherit_signals(&self, parent: &SignalState) {
        *self.signal.lock() = parent.fork_copy();
    }

    /// Publish a child state change before waking every waiter. The tree
    /// change must precede this publication; readers recheck it under PROCS.
    fn child_changed(&self) {
        self.child_seq.fetch_add(1, Ordering::SeqCst);
        self.child_events.wake_all();
    }

    /// Release a vfork parent after detaching from the shared address space.
    /// Exit calls this after detach; exec calls it at its point of
    /// no return before installing its new mm.
    pub fn complete_vfork(&self) {
        let done = self.vfork_done.lock().take();
        if let Some(done) = done {
            done.complete();
        }
    }

    fn copy_for_fork(parent: &Arc<Self>, flags: u32) -> Result<NewProcess, u32> {
        let console = parent.console.lock().fork_copy();
        let exe = parent.exe_path.lock().clone();
        let new = NewProcess::empty(console, parent.allow_test_syscalls, exe);
        let fs = parent.fs.lock().clone().expect("live process's fs");
        let child_fs =
            if flags & general::CLONE_FS != 0 { fs } else { Arc::new(fs.fork_copy().map_err(|e| e.to_errno())?) };
        *new.process.fs.lock() = Some(child_fs);
        let files_access = parent.files_access.read();
        let files = parent.files.lock().clone().expect("live process's files");
        let child_files = if flags & general::CLONE_FILES != 0 { files.share() } else { Arc::new(files.fork_copy(0)) };
        *new.process.files.lock() = Some(child_files);
        drop(files_access);
        let mm = parent.address_space().expect("live process's mm");
        assert!(Arc::ptr_eq(&mm, &sched::current_address_space().expect("user space")));
        let child_mm = if flags & general::CLONE_VM != 0 {
            assert!(mm.add_user(), "fork from a retired space");
            mm
        } else {
            mm.fork_copy().map_err(|e| e.to_errno())?
        };
        *new.process.mm.lock() = Some(child_mm);
        if flags & general::CLONE_VFORK != 0 {
            *new.process.vfork_done.lock() = Some(Arc::new(VforkDone::new()));
        }
        Ok(new)
    }

    /// Whether its `mm`, `files` and `fs` slots are all empty (its
    /// takedown's step 3 is done).
    fn released(&self) -> bool {
        self.mm.lock().is_none() && self.files.lock().is_none() && self.fs.lock().is_none()
    }
}

/// Replace both references to the caller's mm with one writer. The
/// caller must be this process's last thread and transfer one process
/// user of the new space. Old mappings are retired in thread context.
pub fn replace_current_mm(space: Arc<AddressSpace>) {
    let process = current_process();
    assert!(!PROCS.lock().has_extra_threads(process.pid()), "exec needs one thread");
    let old = {
        let _irq = crate::arch::irq::push_off();
        let old = sched::replace_user_space(Arc::clone(&space));
        let recorded = process.mm.lock().replace(space).expect("live process's mm");
        assert!(Arc::ptr_eq(&old, &recorded), "process and scheduler disagree on mm");
        old
    };
    old.drop_user();
}

/// Mark that the current process has performed an exec, even without an identity.
pub fn mark_exec() {
    let pid = current_process().pid();
    PROCS.lock().exec(pid);
}

/// Serialize exec against group actions, wait for detached siblings, and take
/// the leader's tid. A real fatal send still aborts the caller's killable wait.
pub(crate) fn de_thread() -> Result<(), u32> {
    #[cfg(feature = "ktest")]
    delay_window(&EXEC_DELAY);
    let caller = current_thread();
    let process = caller.process();
    process.signal.lock().begin_exec(process, caller.tid())?;
    let wake = process.exec_wait.wait_killable(None, || process.live_threads.load(Ordering::SeqCst) == 1);
    #[cfg(feature = "ktest")]
    delay_window(&EXEC_WAKE_DELAY);
    let mut table = PROCS.lock();
    let mut state = process.signal.lock();
    assert_eq!(state.exec_waiter, Some(caller.tid()));
    state.exec_waiter = None;
    if wake == sched::Wake::Interrupted || state.ending() || sched::with_current(|t| t.fatal_pending()).unwrap_or(false)
    {
        return Err(errno::EAGAIN);
    }
    // Linux begin_new_exec runs de_thread/exchange_tids before exec_mmap's
    // robust-list cleanup. Like handle_futex_death(task_pid_vnr(curr)), our
    // later release_user_sync compares owner bits against the new leader tid;
    // a mutex still naming this caller's old tid is not marked OWNER_DIED.
    table.take_leader_tid(&caller, &mut state);
    Ok(())
}

/// Deterministic one-shot pause with no elapsed-time assertion.
#[cfg(feature = "ktest")]
pub(super) struct TransitionDelay {
    tid: u32,
    reached: VforkDone,
    release: VforkDone,
}

#[cfg(feature = "ktest")]
pub(super) static EXEC_DELAY: SpinLock<Option<Arc<TransitionDelay>>> = SpinLock::new(None);
#[cfg(feature = "ktest")]
static EXEC_WAKE_DELAY: SpinLock<Option<Arc<TransitionDelay>>> = SpinLock::new(None);
#[cfg(feature = "ktest")]
static EXEC_FILES_DELAY: SpinLock<Option<Arc<TransitionDelay>>> = SpinLock::new(None);
#[cfg(feature = "ktest")]
static CLONE_DELAY: SpinLock<Option<Arc<TransitionDelay>>> = SpinLock::new(None);

#[cfg(feature = "ktest")]
pub(super) fn delay_window(slot: &SpinLock<Option<Arc<TransitionDelay>>>) {
    let pause = slot.lock().as_ref().filter(|p| p.tid == current_thread().tid()).cloned();
    if let Some(pause) = pause {
        pause.reached.complete();
        pause.release.waiters.wait_until(|| pause.release.done.load(Ordering::SeqCst));
    }
}

#[cfg(feature = "ktest")]
fn clone_window() {
    delay_window(&CLONE_DELAY);
}

/// Publish the replacement image's raw identity, clearing any old path if unnamed.
pub fn set_exe_path(path: Option<Arc<[u8]>>) {
    let old = core::mem::replace(&mut *current_process().exe_path.lock(), path);
    drop(old);
}

/// Close open CLOEXEC slots through release_fd; reset handled dispositions,
/// alternate stack and fault/frame diagnostics, preserving masks and pending signals.
pub fn finish_exec() {
    let process = current_process();
    let mut next = 0;
    while let Some(fd) = process.with_files(|table| table.next_cloexec(next)).flatten() {
        close_current_fd(fd).expect("exec owns its unshared descriptor table");
        next = fd as usize + 1;
    }
    process.exec_signals(current_thread().tid());
}

/// Storage reserved before teardown, whether or not the table is shared yet.
pub struct PreparedFiles(Arc<Files>);

/// Allocate all descriptor-copy storage before replacing the address space.
pub fn prepare_exec_files() -> Result<PreparedFiles, u32> {
    #[cfg(feature = "ktest")]
    delay_window(&EXEC_FILES_DELAY);
    // A sibling can fork a Files sharer and exit during preparation. Only
    // after de_thread can commit_unshare decide that no copy is needed.
    Ok(PreparedFiles(Arc::new(Files::prepare_exec().map_err(|e| e.to_errno())?)))
}

/// Copy descriptors using reserved storage, then retire the old table.
/// The copy does not allocate; retirement may allocate its drain vector if
/// the last other sharer has left. No page-table or frame-allocator lock is
/// held. The exclusive lease waits for reservations without spinlocks held.
pub fn commit_unshare(prepared: PreparedFiles) {
    let process = current_process();
    let access = process.files_access.write();
    let old = process.files.lock().clone().expect("live process's files");
    if old.users() > 1 {
        old.copy_exec_into(&prepared.0, process.pid());
        let replaced = process.files.lock().replace(prepared.0).expect("live process's files");
        drop(access);
        replaced.leave(process.pid());
    }
}

/// Ktest helper: reserve the replacement before waiting for open reservations.
#[cfg(feature = "ktest")]
pub fn unshare_current_files() -> Result<(), u32> {
    commit_unshare(prepare_exec_files()?);
    Ok(())
}

#[cfg(feature = "ktest")]
pub(crate) fn with_test_shared_files<R>(f: impl FnOnce() -> R) -> R {
    let files = current_process().files.lock().clone().expect("live files");
    let shared = files.share();
    let result = f();
    shared.leave(0);
    result
}

impl Drop for Process {
    fn drop(&mut self) {
        let pid = *self.pid.get_mut();
        debug_assert!(
            pid == INIT_PID
                || (self.mm.get_mut().is_none() && self.files.get_mut().is_none() && self.fs.get_mut().is_none()),
            "pid {pid} dropped before its takedown emptied mm, files and fs"
        );
    }
}

/// An unlinked process owns resources even without a pid. A refusal must
/// release them in its creator's thread, before Process's ordinary Drop.
struct NewProcess {
    process: Arc<Process>,
    armed: bool,
}

impl NewProcess {
    fn empty(console: ConsoleState, allow_test_syscalls: bool, exe: Option<Arc<[u8]>>) -> Self {
        Self {
            process: Arc::new(Process::new(None, None, None, SignalState::new(), console, allow_test_syscalls, exe)),
            armed: true,
        }
    }

    fn linked(mut self) {
        self.armed = false;
    }
}

impl Drop for NewProcess {
    fn drop(&mut self) {
        if self.armed {
            self.process.release_resources();
        }
    }
}

/// A child cannot run until its pid and parent tid store are published.
#[repr(u32)]
enum Gate {
    Wait,
    Start,
    Cancel,
}

/// A one-shot start/cancel decision; only the creator opens it.
struct StartGate {
    state: AtomicU32,
    waiters: sched::WaitQueue,
}

impl StartGate {
    fn new() -> Self {
        Self { state: AtomicU32::new(Gate::Wait as u32), waiters: sched::WaitQueue::new() }
    }

    fn open(&self, decision: Gate) {
        self.state.store(decision as u32, Ordering::Release);
        self.waiters.wake_all();
    }

    fn started(&self) -> bool {
        self.waiters.wait_until(|| self.state.load(Ordering::Acquire) != Gate::Wait as u32);
        self.state.load(Ordering::Acquire) == Gate::Start as u32
    }
}

/// Drop before NewProcess: cancellation joins before its resources retire.
struct ParkedChild {
    gate: Arc<StartGate>,
    handle: Option<sched::JoinHandle>,
}

impl ParkedChild {
    /// The child's run closure starts only after publication. Cancellation
    /// detaches its scheduler state without releasing the creator's resources.
    fn spawn(
        name: impl Into<sched::TaskName>,
        space: Arc<AddressSpace>,
        linux: Arc<UserThread>,
        fs_base: u64,
        run: impl FnOnce() + Send + 'static,
    ) -> Result<Self, sched::SpawnError> {
        let gate = Arc::new(StartGate::new());
        let start = Arc::clone(&gate);
        let handle = sched::spawn_fork_child(name, space, linux, fs_base, move || {
            let started = start.started();
            drop(start);
            if started {
                run();
            } else {
                drop(sched::set_current_user_thread(None));
                drop(sched::detach_user_space());
            }
        })?;
        Ok(Self { gate, handle: Some(handle) })
    }

    fn start(mut self) -> sched::JoinHandle {
        self.gate.open(Gate::Start);
        self.handle.take().expect("parked child's handle")
    }
}

impl Drop for ParkedChild {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.gate.open(Gate::Cancel);
            handle.join();
        }
    }
}

/// One-shot completion shared by a vfork child and its sleeping parent.
pub struct VforkDone {
    done: core::sync::atomic::AtomicBool,
    waiters: sched::WaitQueue,
}

impl VforkDone {
    fn new() -> Self {
        Self { done: core::sync::atomic::AtomicBool::new(false), waiters: sched::WaitQueue::new() }
    }

    fn complete(&self) {
        self.done.store(true, Ordering::SeqCst);
        self.waiters.wake_all();
    }

    fn wait(&self) {
        let wake = self.waiters.wait_killable(None, || self.done.load(Ordering::SeqCst));
        // Only fatal_pending interrupts this wait; C5 ends the caller before
        // returning to user mode, even if the child still holds its completion.
        debug_assert!(wake != sched::Wake::Interrupted || sched::with_current(|t| t.fatal_pending()).unwrap_or(false));
    }
}

/// One ring-3 thread's Linux identity: held by its scheduler thread's user
/// state (`sched::current_user_thread`) and by `PROCS` while it lives.
pub struct UserThread {
    /// Stored once (`set_tid`), in the `PROCS` hold that allocates it,
    /// before the thread enters ring 3. The leader's is the pid.
    tid: AtomicU32,
    process: Arc<Process>,
    /// `set_tid_address`'s pointer (M8: `CLONE_CHILD_CLEARTID`'s too).
    clear_child_tid: AtomicU64,
    /// The registered robust-list head, cleared at exec and exit.
    robust_list: AtomicU64,
    /// Its scheduler thread's id (`sched::threads()`' id).
    kernel_thread: AtomicU64,
    /// Shared notification target: bound before a real thread becomes runnable.
    kernel_handle: Arc<SpinLock<Weak<sched::Thread>>>,
    /// Only explicit ktest stand-ins may have no scheduler notification target.
    synthetic: bool,
    /// Owns register_current's kernel continuation and result slot.
    run_owner: bool,
    /// Completion for CLONE_THREAD | CLONE_VFORK, owned by this task alone.
    vfork_done: SpinLock<Option<Arc<VforkDone>>>,
    exit: SpinLock<Option<UserExit>>,
}

impl UserThread {
    fn new(process: Arc<Process>) -> Self {
        Self {
            tid: AtomicU32::new(0),
            process,
            clear_child_tid: AtomicU64::new(0),
            robust_list: AtomicU64::new(0),
            kernel_thread: AtomicU64::new(0),
            kernel_handle: Arc::new(SpinLock::new(Weak::new())),
            synthetic: false,
            run_owner: false,
            vfork_done: SpinLock::new(None),
            exit: SpinLock::new(None),
        }
    }

    fn for_current(process: Arc<Process>) -> Self {
        let thread = Self::new(process);
        thread.bind_kernel_thread(&sched::current().expect("current scheduler thread"));
        thread
    }

    #[cfg(feature = "ktest")]
    fn unbound_for_ktest(process: Arc<Process>) -> Self {
        Self { synthetic: true, ..Self::new(process) }
    }

    /// Bind the scheduler identity and notification target before the child is runnable.
    pub(crate) fn bind_kernel_thread(&self, kernel: &Arc<sched::Thread>) {
        assert_eq!(self.kernel_thread.swap(kernel.id(), Ordering::AcqRel), 0);
        *self.kernel_handle.lock() = Arc::downgrade(kernel);
    }

    /// Store its tid, once: panics if it already has one. (C3's
    /// `de_thread`, which gives the exec'ing thread the leader's tid, gets
    /// its own exchange: this stays set-once.)
    fn set_tid(&self, tid: u32) {
        if let Err(had) = self.tid.compare_exchange(0, tid, Ordering::AcqRel, Ordering::Acquire) {
            panic!("thread {had} given a second tid, {tid}");
        }
    }

    /// Only the table's sole-survivor exec transition may replace a tid.
    fn exchange_tid(&self, old: u32, pid: u32) {
        assert_eq!(self.tid.compare_exchange(old, pid, Ordering::AcqRel, Ordering::Acquire), Ok(old));
    }

    pub(crate) fn complete_vfork(&self) {
        if let Some(done) = self.vfork_done.lock().take() {
            done.complete();
        }
    }

    /// Its tid: set before anything can read it.
    pub fn tid(&self) -> u32 {
        let tid = self.tid.load(Ordering::Acquire);
        debug_assert_ne!(tid, 0, "a thread's tid read before it was set");
        tid
    }

    /// A live scheduler handle, if this thread has one.
    pub fn scheduler_thread(&self) -> Option<Arc<sched::Thread>> {
        self.kernel_handle.lock().upgrade()
    }

    /// Its process.
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// The signals it blocks.
    pub fn blocked(&self) -> Blocked {
        self.process.signal.lock().blocked(self.tid())
    }

    /// Make `blocked` the signals it blocks.
    pub fn set_blocked(&self, blocked: Blocked) {
        self.process.signal.lock().set_blocked(self.tid(), blocked);
    }

    /// Store an opaque robust-list head for cleanup on the current mm.
    pub fn set_robust_list(&self, head: u64) {
        self.robust_list.store(head, Ordering::Relaxed);
    }

    /// The registered robust-list head, or zero when none is registered.
    pub fn robust_list(&self) -> u64 {
        self.robust_list.load(Ordering::Relaxed)
    }

    /// Take the registered head exactly once for exit or exec cleanup.
    pub(crate) fn take_robust_list(&self) -> u64 {
        self.robust_list.swap(0, Ordering::Relaxed)
    }

    /// Record `set_tid_address`'s pointer.
    pub fn set_clear_child_tid(&self, tidptr: u64) {
        self.clear_child_tid.store(tidptr, Ordering::Relaxed);
    }

    /// `set_tid_address`'s pointer (0 if none was set).
    pub fn clear_child_tid(&self) -> u64 {
        self.clear_child_tid.load(Ordering::Relaxed)
    }
}

/// The kernel's process table (section 9's Lock 10a): taken before any
/// `Process::signal`, which may nest under it, and before the leaf locks.
/// The tree allocates under it.
static PROCS: SpinLock<ProcTable> = SpinLock::new(ProcTable::new(PID_MAX));

/// Each process's `RusageSlot` value: any unique number, taken before its
/// pid exists.
static NEXT_USAGE_SLOT: AtomicU64 = AtomicU64::new(0);

/// Start pid 1 (`linux::init`, at boot): a kernel-owned process with no
/// threads, an empty placeholder address space and no descriptors, whose
/// `SIGCHLD` is `SIG_IGN`, so the zombies of its children (orphans, and a
/// root's children once the root has ended) go at once. Every exit names
/// it as the reaper.
///
/// Panics if it can't (no frame for its placeholder address space at boot,
/// or a second call).
pub(crate) fn start_init() {
    let mm = AddressSpace::new().unwrap_or_else(|e| panic!("pid 1's address space: {e}"));
    let mut signal = SignalState::new();
    let ignore = KernelSigaction { handler: SIG_IGN, ..KernelSigaction::default() };
    signal.set_action(Signal::CHLD, ignore).expect("SIGCHLD's disposition can change");
    let init = Arc::new(Process::new(
        Some(Arc::new(mm)),
        None,
        None,
        signal,
        ConsoleState::new(ConsoleIo::Capture, Vec::new()),
        false,
        None,
    ));
    let started = PROCS.lock().link_init(&init);
    if let Err(e) = started {
        panic!("pid 1 can't start: {e:?}");
    }
}

#[inline]
fn current_kernel_tid() -> u64 {
    sched::running_here().expect("ring-3 syscall/exception requires a running thread").0
}

/// The current Linux thread, if the running thread is a registered user
/// thread (`None` on a kernel thread, a ktest's `UserSpaceGuard`, or before
/// registration and after takedown).
pub fn try_current_thread() -> Option<Arc<UserThread>> {
    sched::current_user_thread()
}

/// The current Linux thread: panics outside a registered user thread (a
/// syscall or fault handler always runs on one).
pub fn current_thread() -> Arc<UserThread> {
    try_current_thread().expect("ring-3 syscall with no Linux thread")
}

/// The current thread's process (see [`current_thread`]).
pub fn current_process() -> Arc<Process> {
    Arc::clone(current_thread().process())
}

/// The process `pid`, live or a zombie, while `PROCS` holds it.
///
/// Procfs currently resolves numeric pids anew on each access: an already
/// open /proc/<pid>/... description can therefore observe a recycled pid.
/// Unlike Linux's pinned struct pid, these descriptions do not pin identity.
pub fn lookup(pid: u32) -> Option<Arc<Process>> {
    PROCS.lock().lookup(pid)
}

/// The live user thread `tid`.
pub fn lookup_thread(tid: u32) -> Option<Arc<UserThread>> {
    PROCS.lock().lookup_thread(tid)
}

/// The tree's exit status, with fault diagnostics using the delivery mapping.
fn exit_status(exit: &UserExit) -> ExitStatus {
    match *exit {
        UserExit::Exited(code) => ExitStatus::exited(code),
        _ => ExitStatus::Killed { signal: exit.signal().expect("signal exit"), core: false },
    }
}

/// Register a fresh process for the current user thread, as a root (no
/// parent: `getppid` 0), starting with `stdio` (opened by the caller first,
/// so a failure stops the run before it starts), with the executable's path
/// for `/proc/<pid>/exe` if it was loaded by path, and return its pid. The
/// process takes over the address space's one process user.
///
/// Panics if no pid is free (every pid up to [`PID_MAX`] in use).
pub fn register_current(
    io: ConsoleIo,
    allow_test_syscalls: bool,
    stdin: Vec<u8>,
    exe_path: Option<Arc<[u8]>>,
    stdio: Stdio,
) -> u32 {
    let process = Arc::new(Process::new(
        sched::current_address_space(),
        Some(Arc::new(Files::new(stdio.fds))),
        Some(Arc::new(FsState::new(stdio.cwd))),
        SignalState::new(),
        ConsoleState::new(io, stdin),
        allow_test_syscalls,
        exe_path,
    ));
    let thread = Arc::new(UserThread { run_owner: true, ..UserThread::for_current(process) });
    let spawned = PROCS.lock().link_root(&thread);
    let pid = spawned.unwrap_or_else(|| panic!("no pid is free for a new process"));
    let prev = sched::set_current_user_thread(Some(thread));
    debug_assert!(prev.is_none(), "a Linux process registered twice on thread {}", current_kernel_tid());
    pid
}

/// Translate only user errors; an inconsistent tree is a kernel bug.
fn tree_errno(error: lumen_proc::Error) -> u32 {
    match error {
        lumen_proc::Error::User(error) => error.to_errno(),
        lumen_proc::Error::Bug(bug) => panic!("process tree: {bug}"),
    }
}

/// Clone the calling process or thread. All fallible work precedes linking its tid;
/// the child cannot enter user mode before the parent's tid store.
pub(crate) fn fork_current(
    request: ProcessClone,
    newsp: u64,
    parent_tid: u64,
    child_tid: u64,
    tls: u64,
    frame: &crate::arch::syscall::SyscallFrame,
) -> Result<u64, u32> {
    let caller = current_thread();
    if !request.is_thread() && caller.process().console.lock().io == ConsoleIo::Capture {
        info!(target: "linux", "fork: capture console unsupported until children have a shared console (C7/C10)");
        return Err(errno::ENOSYS);
    }
    // Fail fast on RLIMIT_NPROC (or a pending exit/exec/stop) before copying
    // mm/files/fs or allocating a child kernel stack. link_child/link_thread
    // recheck atomically under PROCS when publishing the child.
    {
        let table = PROCS.lock();
        let signal = caller.process().signal.lock();
        table.check_nproc(&signal)?;
    }
    let flags = request.bits();
    let new = if request.is_thread() { None } else { Some(Process::copy_for_fork(caller.process(), flags)?) };
    let process = new.as_ref().map_or(caller.process(), |new| &new.process);
    let child = Arc::new(UserThread::new(Arc::clone(process)));
    if request.is_thread() && flags & general::CLONE_VFORK != 0 {
        *child.vfork_done.lock() = Some(Arc::new(VforkDone::new()));
    }
    if flags & general::CLONE_CHILD_CLEARTID != 0 {
        child.set_clear_child_tid(child_tid);
    }
    let mut registers = frame.clone();
    registers.rax = 0;
    if newsp != 0 {
        registers.rsp = newsp;
    }
    let fs_base = if flags & general::CLONE_SETTLS != 0 { tls } else { sched::current_fs_base() };
    let space = process.address_space().expect("child's space");
    let done = if request.is_thread() { child.vfork_done.lock().clone() } else { process.vfork_done.lock().clone() };
    // ParkedChild is declared after NewProcess, so a refusal joins first.
    let parked = ParkedChild::spawn(
        sched::with_current(|t| t.task_name()).expect("forking thread"),
        space,
        Arc::clone(&child),
        fs_base,
        move || {
            if flags & general::CLONE_CHILD_SETTID != 0 {
                let tid = current_thread().tid();
                // Linux ignores this put_user's failure.
                let _ = crate::user::copy_to_user(child_tid, &tid.to_ne_bytes());
            }
            // SAFETY: the scheduler installed this child's identity and space;
            // its copied frame is resumed with user selectors and no lock held.
            unsafe { crate::arch::syscall::enter_fork(registers) };
            let ended = take_current();
            assert!(ended.exit.is_some(), "fork child exited without recording UserExit");
        },
    )
    .map_err(|error| match error {
        sched::SpawnError::NoMemory | sched::SpawnError::Stack(crate::mm::stack::StackError::OutOfMemory) => {
            errno::ENOMEM
        }
        _ => errno::EAGAIN,
    })?;
    let signals = if request.is_thread() {
        let mut state = caller.process().signal.lock();
        let init = state.thread_init(caller.tid(), request);
        Some(state.prepare_thread(&child, init)?)
    } else {
        None
    };
    #[cfg(feature = "ktest")]
    clone_window();
    let pid = if let Some(signals) = signals {
        let mut table = PROCS.lock();
        let mut state = caller.process().signal.lock();
        table.link_thread(&child, signals, &mut state)?
    } else {
        PROCS.lock().link_child(&caller, &child, request)?
    };
    if let Some(new) = new {
        new.linked();
    }
    if flags & general::CLONE_PARENT_SETTID != 0 {
        // Linux ignores this put_user's failure.
        let _ = crate::user::copy_to_user(parent_tid, &pid.to_ne_bytes());
    }
    drop(parked.start());
    if let Some(done) = done {
        done.wait();
    }
    Ok(u64::from(pid))
}

/// Validate wait4 options and select the caller's children by pid or group;
/// process-group selection is snapshotted under PROCS.
pub fn wait4_request(pid: i32, options: u32) -> Result<lumen_proc::WaitRequest, u32> {
    PROCS.lock().wait4_request(current_process().pid(), pid, options)
}

/// Validate waitid idtype, id and event options for the caller; group
/// selection is snapshotted under PROCS.
pub fn waitid_request(idtype: i32, id: i32, options: u32) -> Result<lumen_proc::WaitRequest, u32> {
    PROCS.lock().waitid_request(current_process().pid(), idtype, id, options)
}

/// Start a new session and process group led by the caller; returns the new sid.
pub fn setsid() -> Result<u32, u32> {
    let caller = current_process();
    let mut table = PROCS.lock();
    let sid = table.setsid(caller.pid())?;
    caller.replace_controlling_tty(None);
    Ok(sid)
}

/// Run terminal identity checks with the tree pinned; only signal locks may nest.
pub(crate) fn with_tty_processes<R>(f: impl FnOnce(&mut ProcTree, &BTreeMap<u32, Arc<Process>>) -> R) -> R {
    PROCS.lock().with_tty_processes(f)
}

/// A kernel-generated terminal signal, also valid during process teardown.
pub(crate) fn tty_signal(target: i32, signal: Signal) {
    use lumen_signal::pending::{SenderClass, SigInfo};
    let _ = PROCS.lock().send_process(
        INIT_PID,
        target,
        Ok(Some(SigInfo::new(signal, general::SI_KERNEL as i32))),
        SenderClass::SiUser,
    );
}

/// Move `pid` (0: caller) into process group `pgid` (0: target's pid).
pub fn setpgid(pid: i32, pgid: i32) -> Result<(), u32> {
    let caller = current_process().pid();
    PROCS.lock().setpgid(caller, pid, pgid)
}

/// Return the process group of `pid` (0: caller).
pub fn getpgid(pid: i32) -> Result<u32, u32> {
    let caller = current_process().pid();
    PROCS.lock().getpgid(caller, pid)
}

/// Return the session id of `pid` (0: caller).
pub fn getsid(pid: i32) -> Result<u32, u32> {
    let caller = current_process().pid();
    PROCS.lock().getsid(caller, pid)
}

/// Wait for a matching child state change; WNOHANG returns None. A reap
/// removes both tree and process-table entries before user copyout, even
/// when that copy faults. No process lock is held while sleeping.
#[cfg(feature = "ktest")]
pub fn wait_child(request: lumen_proc::WaitRequest) -> Result<Option<lumen_proc::Report>, u32> {
    wait_child_with_usage(request).map(|result| result.map(|(report, _)| report))
}

/// Wait and snapshot the reported child's own and reaped-descendant ticks before removing it.
pub fn wait_child_with_usage(request: lumen_proc::WaitRequest) -> Result<Option<(lumen_proc::Report, [u64; 2])>, u32> {
    let parent = current_process();
    loop {
        let seen = parent.child_seq.load(Ordering::SeqCst);
        let table::WaitResult { ready, gone, ticks } = PROCS.lock().wait(&parent, request)?;
        drop(gone);
        match ready {
            lumen_proc::WaitReady::Reported(report) => return Ok(Some((report, ticks))),
            lumen_proc::WaitReady::NoneReady if request.options().nohang() => return Ok(None),
            lumen_proc::WaitReady::NoneReady => {}
        }
        if parent.child_events.wait_interruptible(None, || parent.child_seq.load(Ordering::SeqCst) != seen)
            == sched::Wake::Interrupted
        {
            return Err(crate::linux::signal::ERESTARTSYS);
        }
    }
}

/// What a process's takedown hands back: its pid, how its run ended, and
/// what its console captured (`ConsoleIo::Capture`).
pub struct Ended {
    pub pid: u32,
    pub exit: Option<UserExit>,
    pub stdout: Vec<u8>,
}

/// How long the takedown waits between tries of an exit the heap refused
/// (`ExitError::NoMem`).
const EXIT_RETRY_NS: u64 = 1_000_000;

/// Whether a takedown waiting for memory, last warned at `last` (`None`:
/// never), warns again at `now`: at most once a second.
fn warn_due(last: Option<u64>, now: u64) -> bool {
    last.is_none_or(|at| now.saturating_sub(at) >= lumen_time::NS_PER_SEC)
}

/// End the current Linux thread; the last thread releases the process and
/// publishes its wait status. Panics if the running thread has no Linux
/// identity. A root runner waits for the group's result after unlinking,
/// allowing a sibling's exec to complete even when the original leader exits.
/// The design's exit steps 2 and 3, then the tree:
/// 1. release robust futexes, then clear and wake clear_child_tid if the mm
///    has another thread or process user; take its `Arc<UserThread>` out of scheduler state,
///    and its CPU leaves the address space (`sched::detach_user_space`:
///    `CR3` back on the kernel's tables); complete_vfork then releases its parent;
/// 2. in one `PROCS` hold it leaves `PROCS` (and its process's signal
///    state), retargeting pending signals and completing stop participation;
/// 3. the last thread gives up its address space (torn down at its last user),
///    its POSIX locks even with a shared table, its descriptor-table user
///    (last user closes descriptors and releases flock), its working directory and
///    its executable's path;
/// 4. it exits in the tree with how its run ended (`SIGKILL` if nothing
///    was recorded), pid 1 its reaper, leaving `PROCS` once the tree
///    releases it; an exit the heap refuses is retried, every millisecond.
pub fn take_current() -> Ended {
    let current = current_thread();
    super::futex::release_user_sync(
        &current,
        current.process().live_threads.load(Ordering::SeqCst) > 1
            || sched::current_address_space().is_some_and(|mm| mm.users() > 1),
    );
    drop(current);
    let thread = sched::set_current_user_thread(None).expect("no Linux process to take down after ring-3 exit");
    drop(sched::detach_user_space());
    thread.complete_vfork();
    let process = Arc::clone(thread.process());
    let pid = process.pid();
    let run_owner = thread.run_owner;
    let (unlinked, last) = {
        let mut t = PROCS.lock();
        let unlinked = t.unlink_thread(&thread);
        let last = t.no_thread_left(pid);
        if !last && run_owner {
            process.run_waiting.store(true, Ordering::Release);
        }
        if last {
            // Zombie sends succeed without queueing. Preserve the leader's
            // ordinary status without turning earlier thread exits into group exits.
            let mut state = process.signal.lock();
            if let Some(sig) = state.fatal_signal() {
                process.exit.lock().get_or_insert(UserExit::Signaled(sig));
            }
            state.record_exit();
        }
        (unlinked, last)
    };
    let own_exit = thread.exit.lock().take().unwrap_or(UserExit::Exited(0));
    if thread.tid() == pid || last {
        process.complete_vfork();
    }
    drop((unlinked, thread));
    if !last {
        if run_owner {
            process.run_wait.wait_until(|| process.run_result.lock().is_some());
            return process.run_result.lock().take().expect("last thread published the root's result");
        }
        return Ended { pid, exit: Some(own_exit), stdout: Vec::new() };
    }
    let exit = process.take_exit();
    if let Some(tty) = process.controlling_tty() {
        tty.leader_exit(pid);
    }
    process.release_resources();
    debug_assert!(process.released(), "pid {pid} exits in the tree before giving up mm, files and fs");
    let status = exit.as_ref().map_or(ExitStatus::Killed { signal: Signal::KILL, core: false }, exit_status);
    let mut last_warned: Option<u64> = None;
    let gone = loop {
        let tried = PROCS.lock().exit_in_tree(pid, status);
        match tried {
            Ok(gone) => break gone,
            Err(ExitError::NoMem) => {
                let now = crate::time::now_ns();
                if warn_due(last_warned, now) {
                    warn!(target: "linux", "pid {pid}'s exit is waiting for memory");
                    last_warned = Some(now);
                }
                crate::time::sleep(EXIT_RETRY_NS);
            }
            Err(ExitError::Bug(bug)) => panic!("pid {pid}'s exit: {bug}"),
        }
    };
    if last_warned.is_some() {
        info!(target: "linux", "pid {pid}'s exit went through after waiting for memory");
    }
    drop(gone);
    let stdout = core::mem::take(&mut process.console.lock().stdout);
    if process.run_waiting.load(Ordering::Acquire) {
        *process.run_result.lock() = Some(Ended { pid, exit, stdout });
        process.run_wait.wake_all();
        return Ended { pid, exit, stdout: Vec::new() };
    }
    Ended { pid, exit, stdout }
}

/// A process the kernel's own checks register on the current user thread,
/// taken down ([`take_current`]) when this drops, so an early return
/// can't leave it in `PROCS`, where it would stay in `/proc` for good. It
/// starts with `ConsoleIo::Capture`, no test syscalls, no stdin and no
/// executable path. Real runs (`user.rs`) register and take their process
/// themselves.
///
/// - Its takedown also takes the thread's CPU off its address space and
///   gives the process's user of the space up (tearing it down, as the
///   only user): after it, the thread has no user memory. Never call
///   `sched::detach_user_space` while it lives (a debug assertion there).
/// - Never call [`take_current`] yourself while it lives: that takes its
///   process down, and its own takedown (at its drop or `end`) then finds
///   none and panics.
/// - Bind it to a named variable: `#[must_use]` catches a bare call, not
///   `let _ = RegisteredProcess::register(..)`, which drops it (and takes
///   the process down) at once.
/// - Not `Send`: taking it down finds the process by the running thread.
#[must_use = "dropping it takes its process down again at once"]
pub(crate) struct RegisteredProcess {
    pid: u32,
    _on_this_thread: PhantomData<*const ()>,
}

impl RegisteredProcess {
    /// Register a process on the current user thread with `stdio`
    /// ([`register_current`]).
    pub(crate) fn register(stdio: Stdio) -> Self {
        let pid = register_current(ConsoleIo::Capture, false, Vec::new(), None, stdio);
        Self { pid, _on_this_thread: PhantomData }
    }

    /// Its pid.
    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    /// Take the process down now and return what it [`Ended`] with (what
    /// its console captured, say).
    pub(crate) fn end(self) -> Ended {
        let ended = self.take();
        // Taken, so `Drop` must not take it again; nothing else in `self`
        // needs dropping.
        core::mem::forget(self);
        ended
    }

    /// Take the current thread's process down: this one's (a debug
    /// assertion), or a panic if there is none.
    fn take(&self) -> Ended {
        let ended = take_current();
        debug_assert_eq!(ended.pid, self.pid, "the thread's process (left) isn't this RegisteredProcess's (right)");
        ended
    }
}

impl Drop for RegisteredProcess {
    fn drop(&mut self) {
        drop(self.take());
    }
}

/// Release a removed/displaced descriptor from its original Files table.
/// Every current member loses its POSIX locks on this inode; the last FD
/// reference also releases flock. Exit separately sweeps its departing pid
/// before leave drains the last table user's descriptors. No spinlock may
/// be held: the inode lock precedes the Files leaf used to read members.
fn release_fd(files: &Files, file: FileRef) {
    let last = file.dec_fd_ref() == 0;
    release_locks(&file, LockOwners::Members(files), last);
}

enum LockOwners<'a> {
    Members(&'a Files),
    Pid(u32),
}

/// Release record locks and optionally flock under one inode lock, then
/// wake lockers with all inode/table locks released.
fn release_locks(file: &FileRef, owners: LockOwners<'_>, flock: bool) {
    let mut freed = false;
    let released = file.inode().with_locks_mut(&mut |locks| {
        freed = match owners {
            LockOwners::Members(files) => files.release_members(locks),
            LockOwners::Pid(pid) => locks.release_posix_for_pid(pid),
        };
        if flock {
            freed |= locks.release_flock(file.id);
        }
    });
    if let Err(error) = released {
        log::error!(target: "linux", "releasing locks on {}: {error:?}", Escaped(&file.path));
    }
    if freed {
        locks_released();
    }
}

/// How many times an advisory lock may have been given up (see
/// [`locks_released`]): a blocking `F_SETLKW` or `flock` reads it before
/// each attempt and waits for it to change ([`wait_for_lock_release`]).
static LOCK_RELEASES: AtomicU64 = AtomicU64::new(0);

/// The threads in a blocking `F_SETLKW` or `flock`, waiting for a lock to
/// be given up: one queue for every inode (a release anywhere wakes them
/// all, and each retries its own lock), so no `VfsInode` needs a queue of
/// its own.
static LOCK_WAITERS: sched::WaitQueue = sched::WaitQueue::new();

/// How many threads are in [`wait_for_lock_release`] now (the lock
/// ktests' sign that a blocking call is waiting).
#[cfg(feature = "ktest")]
static LOCK_WAITING: AtomicU32 = AtomicU32::new(0);

/// The release count a blocking lock call reads before each attempt.
pub fn lock_releases() -> u64 {
    LOCK_RELEASES.load(Ordering::SeqCst)
}

/// Say that an advisory lock (`flock` or a POSIX record lock) may have been
/// given up or weakened, waking every blocked locker to retry. Call it
/// after the inode's lock state is released, never under it or under a
/// spinlock (`PROCS`, `Files::table`): `wake_all` takes the wait queue's
/// lock and each woken thread's run-queue lock.
pub fn locks_released() {
    LOCK_RELEASES.fetch_add(1, Ordering::SeqCst);
    LOCK_WAITERS.wake_all();
}

/// Block until [`locks_released`] has been called since [`lock_releases`]
/// read `seen` (at once if it already has, so a release between a failed
/// attempt and this wait isn't missed): `Woken`; or until the thread has a
/// signal to look at: `Interrupted` (B2: the caller returns
/// `-ERESTARTSYS`, as Linux's `locks_lock_inode_wait`). A caller refuses a
/// wait that only a signal would end beforehand (`F_SETLKW`'s `EDEADLK`),
/// if it can tell.
pub fn wait_for_lock_release(seen: u64) -> sched::Wake {
    #[cfg(feature = "ktest")]
    LOCK_WAITING.fetch_add(1, Ordering::SeqCst);
    let woke = LOCK_WAITERS.wait_interruptible(None, || LOCK_RELEASES.load(Ordering::SeqCst) != seen);
    #[cfg(feature = "ktest")]
    LOCK_WAITING.fetch_sub(1, Ordering::SeqCst);
    woke
}

/// How many threads are blocked in a lock call now (ktests).
#[cfg(feature = "ktest")]
pub(crate) fn lock_waiters() -> u32 {
    LOCK_WAITING.load(Ordering::SeqCst)
}

/// Run a short non-blocking closure on the current process's descriptor
/// table, under its `Files::table` lock (a leaf but for the heap: nothing
/// in `f` may take a lock or close a file).
fn with_current_fds<R>(f: impl FnOnce(&mut FdTable<FileRef>) -> R) -> R {
    current_process().with_files(f).expect("a running process has its descriptors")
}

/// The current process's open file on `fd` for an in-flight syscall, raising
/// `KernelFile::end_refs` under `Files::table` and releasing it on drop.
pub fn current_io_file(fd: i32) -> Option<IoFile> {
    with_current_fds(|t| t.get(fd).map(|e| IoFile::new(&e.file)))
}

/// The current process's open files on each of `fds` for `poll`/`select`,
/// raising `KernelFile::end_refs` under one hold of `Files::table`.
pub fn current_io_files(fds: &[i32]) -> Vec<Option<IoFile>> {
    with_current_fds(|t| fds.iter().map(|&fd| t.get(fd).map(|e| IoFile::new(&e.file))).collect())
}

/// The current process's open file on `fd` (a metadata/test clone without
/// raising `end_refs` or keeping a pipe end open), or `None` if closed.
/// Its last drop can take Lock 0; never drop under locks 1–10 or `pos`.
#[cfg(feature = "ktest")]
pub fn current_file(fd: i32) -> Option<FileRef> {
    with_current_fds(|t| t.get(fd).map(|e| Arc::clone(&e.file)))
}

/// Reserve the lowest free descriptor below the current process's soft
/// `RLIMIT_NOFILE` limit (`EMFILE` if every slot below that soft limit is open
/// or reserved) before an open or pipe can create or truncate anything. Its
/// guard cancels the slot on every failure path.
pub fn reserve_current_fd() -> Result<ReservedFd, VfsError> {
    current_process().reserve_fd()
}

/// Put `file`, a fresh open file description (its `fd_refs` 1, for this
/// slot), in the current process's lowest free slot below its soft
/// `RLIMIT_NOFILE`: the descriptor, or `EMFILE`.
#[cfg(feature = "ktest")]
pub fn install_current_fd(file: KernelFile, cloexec: bool) -> Result<i32, VfsError> {
    Ok(reserve_current_fd()?.install(file, cloexec))
}

/// Put `file`, a fresh open file description, on descriptor `fd` of the
/// current process, closing what was there (its locks released outside the
/// table's lock). `EBUSY` for a reserved slot; `EBADF` for an fd outside
/// `0..lumen_vfs::MAX_FDS`: procfs builds `/proc/<pid>/fd/<fd>`'s inode
/// number from it and relies on that range.
pub fn insert_current_fd(fd: i32, file: KernelFile) -> Result<(), VfsError> {
    place_current_fd(fd, Arc::new(file), false)
}

/// Make descriptor `new` share `old`'s open file description, close-on-exec
/// as `cloexec` says (`dup2`/`dup3`'s slot work, M8): its `fd_refs` counts
/// the new slot, and what `new` held is closed. `EBADF` if `old` isn't open
/// or `new` is outside `0..RLIMIT_NOFILE`; `old == new` changes nothing
/// (succeeding for an already-open descriptor even if `>= RLIMIT_NOFILE`,
/// matching Linux `ksys_dup2`).
///
/// Lookup, reference counting and installation use one table hold;
/// displaced entries are released after it. A reserved target is `EBUSY`.
pub fn dup_current_fd(old: i32, new: i32, cloexec: bool) -> Result<i32, VfsError> {
    let process = current_process();
    let _access = process.files_access.read();
    let limit = process.nofile_soft();
    let files = process.files.lock().clone().expect("a running process has its descriptors");
    let _closing = files.closing();
    let displaced = files.with_table(|t| {
        let file = t.get(old).map(|e| Arc::clone(&e.file)).ok_or(VfsError::BadF)?;
        #[cfg(feature = "ktest")]
        dup_lookup_window();
        if old == new {
            return Ok::<_, VfsError>(None);
        }
        // Even on refusal the source slot keeps this clone from being
        // the last Arc dropped under the table lock.
        let displaced = t.insert_at_below(new, limit, Arc::clone(&file), cloexec)?;
        file.inc_fd_ref();
        Ok(displaced)
    })?;
    if let Some(old) = displaced {
        release_fd(&files, old.file);
    }
    Ok(new)
}

fn dup_into(
    old: i32,
    alloc: impl FnOnce(&mut FdTable<FileRef>, usize, FileRef) -> Result<i32, VfsError>,
) -> Result<i32, VfsError> {
    let process = current_process();
    let limit = process.nofile_soft();
    process
        .with_files(|t| {
            let file = t.get(old).map(|e| Arc::clone(&e.file)).ok_or(VfsError::BadF)?;
            #[cfg(feature = "ktest")]
            dup_lookup_window();
            let new = alloc(t, limit, Arc::clone(&file))?;
            file.inc_fd_ref();
            Ok(new)
        })
        .expect("a running process has its descriptors")
}

/// Duplicate `old` into the lowest free slot below `RLIMIT_NOFILE` (`dup`),
/// under one hold of the table. `EBADF` for a closed source; `EMFILE` when no
/// slot below the soft limit is free (including when the limit is 0).
pub fn dup_current_fd_lowest(old: i32) -> Result<i32, VfsError> {
    dup_into(old, |t, limit, file| t.alloc_below(limit, file, false))
}

/// Duplicate `old` into the lowest free slot at or above `min` and below
/// `RLIMIT_NOFILE` (`F_DUPFD` / `F_DUPFD_CLOEXEC`), under one hold of the
/// table. `EBADF` for a closed source, `EINVAL` for `min < 0` or
/// `min >= RLIMIT_NOFILE`, `EMFILE` when no slot in `[min, RLIMIT_NOFILE)` is
/// free.
pub fn dup_current_fd_from(old: i32, min: i32, cloexec: bool) -> Result<i32, VfsError> {
    dup_into(old, |t, limit, file| t.alloc_from_below(min, limit, file, cloexec))
}

/// Put `file` (its `fd_refs` already counting this slot) on `fd`, releasing
/// what it displaces outside the table's lock.
fn place_current_fd(fd: i32, file: FileRef, cloexec: bool) -> Result<(), VfsError> {
    let process = current_process();
    let _access = process.files_access.read();
    let files = process.files.lock().clone().expect("a running process has its descriptors");
    let _closing = files.closing();
    let displaced = files.with_table(|t| t.insert_at(fd, Arc::clone(&file), cloexec));
    if let Some(old) = displaced? {
        release_fd(&files, old.file);
    }
    Ok(())
}

/// Close descriptor `fd` of the current process (`close`): `EBADF` if it
/// isn't open. The entry is taken out under the table's lock and released
/// after.
pub fn close_current_fd(fd: i32) -> Result<(), VfsError> {
    let process = current_process();
    let _access = process.files_access.read();
    let files = process.files.lock().clone().expect("a running process has its descriptors");
    let _closing = files.closing();
    let removed = files.with_table(|t| t.remove(fd));
    release_fd(&files, removed?.file);
    Ok(())
}

/// Whether descriptor `fd` of the current process is close-on-exec, or
/// `EBADF`.
pub fn current_cloexec(fd: i32) -> Result<bool, VfsError> {
    with_current_fds(|t| t.get_cloexec(fd))
}

/// Set or clear descriptor `fd`'s close-on-exec flag (`FIOCLEX`,
/// `FIONCLEX`), or `EBADF`.
pub fn set_current_cloexec(fd: i32, cloexec: bool) -> Result<(), VfsError> {
    with_current_fds(|t| t.set_cloexec(fd, cloexec))
}

/// Run a short non-blocking closure on the current process's working
/// directory and umask, under its `FsState` lock (a leaf but for the heap:
/// nothing in `f` may take a lock or drop a working directory).
fn with_current_fs<R>(f: impl FnOnce(&mut FsInner) -> R) -> R {
    current_process().with_fs(f).expect("a running process has its working directory")
}

/// The current process's working directory: its inode and its stored path
/// (clones).
pub fn current_cwd() -> (Arc<dyn VfsInode>, Arc<[u8]>) {
    with_current_fs(|fs| (Arc::clone(fs.cwd().inode()), Arc::clone(fs.cwd().path())))
}

/// Make `cwd` the current process's working directory; the old one drops
/// after its lock is released (`chdir`, `fchdir`).
pub fn set_current_cwd(cwd: Cwd) {
    let old = with_current_fs(|fs| fs.swap_cwd(cwd));
    drop(old);
}

/// The current process's file mode creation mask.
pub fn current_umask() -> u16 {
    with_current_fs(|fs| fs.umask())
}

/// Set the current process's file mode creation mask to `new`'s `0o777`
/// bits (`umask`): the mask it replaces.
pub fn swap_current_umask(new: u16) -> u16 {
    with_current_fs(|fs| fs.swap_umask(new))
}

/// The current process's disposition of `signal`.
pub fn current_action(signal: Signal) -> KernelSigaction {
    current_thread().process().action(signal)
}

/// Install `action` as the current process's disposition of `signal`
/// ([`SignalState::set_action`]): the one it replaces, or `None` for
/// `SIGKILL` and `SIGSTOP`.
pub fn set_current_action(signal: Signal, action: KernelSigaction) -> Option<KernelSigaction> {
    current_process().signal.lock().set_action(signal, action)
}

/// The signals the current thread blocks.
pub fn current_blocked() -> Blocked {
    current_thread().blocked()
}

/// Make `blocked` the signals the current thread blocks.
pub fn set_current_blocked(blocked: Blocked) {
    current_thread().set_blocked(blocked);
}

/// Run a short non-blocking closure on the current process's console
/// state, under its lock (a leaf).
pub fn with_current_console<R>(f: impl FnOnce(&mut ConsoleState) -> R) -> R {
    let thread = current_thread();
    let mut console = thread.process().console.lock();
    f(&mut console)
}

/// Non-panicking lookup of the current thread's [`ConsoleIo`], returning `None`
/// if called outside a registered Linux thread (e.g., from a kernel thread).
pub fn try_current_io() -> Option<ConsoleIo> {
    try_current_thread().map(|t| t.process().console.lock().io)
}

/// Non-panicking lookup of the current thread's Linux pid.
pub fn try_current_pid() -> Option<u32> {
    try_current_thread().map(|t| t.process().pid())
}

/// The pids of the processes running user code, and (from C1) zombies
/// (sorted): every process in `PROCS` but pid 1, the kernel's, which has
/// no threads, descriptors or working directory (`/proc` lists these).
pub fn active_pids() -> Vec<u32> {
    PROCS.lock().active_pids()
}

/// True if `pid` is one of [`active_pids`].
pub fn has_pid(pid: u32) -> bool {
    active(pid).is_some()
}

/// The process `pid` if it is one of [`active_pids`] (a clone, `PROCS`
/// released before returning).
fn active(pid: u32) -> Option<Arc<Process>> {
    if pid == INIT_PID {
        return None;
    }
    lookup(pid)
}

/// Clone the `Arc<AddressSpace>` for `pid`, every lock released before returning.
#[cfg(feature = "ktest")]
pub fn process_address_space(pid: u32) -> Option<Arc<AddressSpace>> {
    active(pid)?.address_space()
}

/// `/proc/<pid>/maps`' text for `pid`: `None` unless it is one of
/// [`active_pids`]; empty while it has no address space (its takedown, a
/// zombie), as Linux's for a process with no `mm`. Formatted under the
/// space's Lock 1, every process lock released.
pub fn process_maps(pid: u32) -> Option<Vec<u8>> {
    let space = active(pid)?.address_space();
    Some(space.map_or_else(Vec::new, |space| space.format_maps()))
}

/// The kernel thread `pid`'s leader runs on (its id in `sched::threads()`,
/// which the shell's `threads` lists), while it runs.
#[cfg(feature = "ktest")]
pub fn process_thread_id(pid: u32) -> Option<u64> {
    PROCS.lock().lookup_thread(pid).map(|t| t.kernel_thread.load(Ordering::Acquire))
}

/// Snapshot the executable path recorded for `pid`, if any.
pub fn process_exe_path(pid: u32) -> Option<Arc<[u8]>> {
    active(pid)?.exe_path.lock().clone()
}

/// Set the executable's path on the current process, as given (not
/// resolved: symlinks and `..` stay).
pub fn set_current_exe_path(path: &str) {
    let Some(thread) = try_current_thread() else {
        return;
    };
    let path = Arc::from(path.as_bytes());
    let old = thread.process().exe_path.lock().replace(path);
    drop(old);
}

/// Return sorted open descriptor numbers for `pid`.
pub fn process_open_fds(pid: u32) -> Vec<i32> {
    active(pid).and_then(|p| p.with_files(|t| t.iter_open().map(|(fd, _)| fd).collect())).unwrap_or_default()
}

/// What `/proc/<pid>/fd/<fd>` points at: the open file's inode and the path
/// it shows, its bytes.
pub type FdTarget = (Option<Arc<dyn VfsInode>>, Vec<u8>);

/// Snapshot `/proc/<pid>/fd/<fd>`'s [`FdTarget`], ` (deleted)` added to the
/// path once unlinked.
///
/// Releases every lock before inspecting `VfsInode::stat()` or dropping
/// the file it cloned.
pub fn process_fd_target(pid: u32, fd: i32) -> Option<FdTarget> {
    let file = active(pid)?.with_files(|t| t.get(fd).map(|e| Arc::clone(&e.file)))??;
    let inode = Arc::clone(file.inode());
    let mut path = file.path.to_vec();
    drop(file);
    let unlinked = inode.is_detached() || inode.stat().ok().is_some_and(|m| m.nlink == 0);
    if unlinked && !path.ends_with(lumen_vfs::DELETED_SUFFIX) {
        path.extend_from_slice(lumen_vfs::DELETED_SUFFIX);
    }
    Some((Some(inode), path))
}

/// Record the process's terminal [`UserExit`] (the first one recorded
/// wins) and return `(pid, allow_test_syscalls)`.
pub fn record_exit(exit: UserExit) -> (u32, bool) {
    let thread = current_thread();
    let process = thread.process();
    thread.exit.lock().get_or_insert(exit);
    process.record_exit(exit);
    (process.pid(), process.allow_test_syscalls)
}

/// End only the caller; retain the leader's code for the eventual last exit.
pub fn record_thread_exit(exit: UserExit) -> (u32, bool) {
    let thread = current_thread();
    let process = thread.process();
    let state = process.signal.lock();
    thread.exit.lock().get_or_insert(exit);
    if thread.tid() == process.pid() && state.exec_waiter.is_none() {
        process.leader_exit.lock().get_or_insert(exit);
    }
    (process.pid(), process.allow_test_syscalls)
}

/// One process as `ps` lists it ([`ps_rows`]).
pub struct PsRow {
    pub pid: u32,
    pub ppid: u32,
    /// Its threads: each tid with its scheduler thread's id, the leader's
    /// first, then the others by tid.
    pub threads: Vec<(u32, u64)>,
    /// Its state in the process tree.
    pub state: ProcState,
}

/// Every process in `PROCS`, pid 1 included, by pid: the `ps` command's
/// rows (read in one hold of `PROCS`).
pub fn ps_rows() -> Vec<PsRow> {
    PROCS.lock().ps_rows()
}

/// A process's M7 console state: where its console fds connect, the
/// terminal settings `TCGETS` reports, and the console bridge's buffers.
pub struct ConsoleState {
    pub io: ConsoleIo,
    pub termios: general::termios,
    /// What `ConsoleIo::Capture` captured (at most `CAPTURE_STDOUT_LIMIT`
    /// bytes).
    stdout: Vec<u8>,
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

impl ConsoleState {
    fn new(io: ConsoleIo, stdin: Vec<u8>) -> Self {
        Self {
            io,
            termios: default_termios(),
            stdout: Vec::new(),
            stdin,
            stdin_pos: 0,
            unread: [0; 4],
            unread_pos: 0,
            unread_len: 0,
            vt_utf8: [0; 4],
            vt_utf8_len: 0,
        }
    }

    fn fork_copy(&self) -> Self {
        let mut child = Self::new(self.io, self.stdin.clone());
        child.termios = self.termios;
        child.stdin_pos = self.stdin_pos;
        child
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

/// Write `bytes` (already copied from user space) to the process's console target.
///
/// Never called while holding a spinlock; for `Capture` it takes the
/// process's console state briefly to append up to `CAPTURE_STDOUT_LIMIT`.
pub fn write_console(io: ConsoleIo, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }
    match io {
        ConsoleIo::Capture => {
            with_current_console(|proc| {
                let room = CAPTURE_STDOUT_LIMIT.saturating_sub(proc.stdout.len());
                proc.stdout.extend_from_slice(&bytes[..bytes.len().min(room)]);
            });
        }
        ConsoleIo::Serial => {
            klog::with_line_lock(|| {
                let _ = klog::burst_held(bytes);
            });
        }
        ConsoleIo::Vt(vt) => write_vt_utf8(vt.index(), bytes),
    }
}

/// Stream `bytes` onto virtual terminal `vt`, preserving incomplete trailing
/// multi-byte UTF-8 sequences across chunk/write boundaries and replacing only
/// genuinely invalid byte sequences with U+FFFD.
fn write_vt_utf8(vt: usize, bytes: &[u8]) {
    let (tail, tail_len) = with_current_console(|proc| {
        let t = proc.vt_utf8;
        let l = proc.vt_utf8_len as usize;
        proc.vt_utf8_len = 0;
        (t, l)
    });

    let (tail, tail_len) = write_vt_utf8_stream(vt, bytes, tail, tail_len);
    with_current_console(|proc| {
        proc.vt_utf8 = tail;
        proc.vt_utf8_len = tail_len as u8;
    });
}

/// Stream console output with caller-owned UTF-8 state, preserving split codepoints.
/// Invalid input produces replacement characters; incomplete tails are returned.
fn write_vt_utf8_stream(vt: usize, bytes: &[u8], mut tail: [u8; 4], mut tail_len: usize) -> ([u8; 4], usize) {
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

    (tail, tail_len)
}

/// Translate a framebuffer VT keyboard event into VT100/UTF-8 input bytes
/// (up to 4 bytes). Returns `0` for keys that emit no terminal bytes (such as
/// bare modifiers or unhandled function keys).
pub(crate) fn vt_key_to_bytes(key: KbdKey, out: &mut [u8; 4]) -> usize {
    match key {
        KbdKey::Enter => {
            out[0] = b'\n';
            1
        }
        KbdKey::Backspace => {
            out[0] = 0x08;
            1
        }
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
        ConsoleIo::Serial => with_current_console(|c| c.has_unread()) || serial_in::available(),
        ConsoleIo::Vt(vt) => {
            if with_current_console(|c| c.has_unread()) {
                return true;
            }
            while let Some(key) = crate::vt::read_key(vt.index()) {
                let mut enc = [0u8; 4];
                let n = vt_key_to_bytes(key, &mut enc);
                if n > 0 {
                    with_current_console(|p| p.push_unread_bytes(&enc[..n]));
                    return true;
                }
            }
            false
        }
    }
}

/// Read up to `dst.len()` bytes from the process's console stdin, blocking until
/// at least 1 byte arrives (or returning `0` on EOF).
///
/// Must be called with interrupts enabled and no `SpinLock` held.
pub fn read_console(io: ConsoleIo, dst: &mut [u8]) -> usize {
    read_console_inner(io, dst, false).expect("uninterruptible console read")
}

/// A console read interrupted before its first byte returns ERESTARTSYS.
/// Once a byte has been consumed, the short read wins over a pending signal.
pub fn read_console_interruptible(io: ConsoleIo, dst: &mut [u8]) -> Result<usize, u32> {
    read_console_inner(io, dst, true)
}

fn read_console_inner(io: ConsoleIo, dst: &mut [u8], interruptible: bool) -> Result<usize, u32> {
    if dst.is_empty() {
        return Ok(0);
    }
    match io {
        ConsoleIo::Capture => Ok(with_current_console(|proc| {
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
        })),
        ConsoleIo::Serial => read_serial(dst, interruptible),
        ConsoleIo::Vt(vt) => read_vt(vt, dst, interruptible),
    }
}

struct ConsoleReadWait {
    queue: sched::WaitQueue,
    fired: core::sync::atomic::AtomicBool,
}
impl lumen_vfs::PollWaker for ConsoleReadWait {
    fn wake(&self, _: lumen_vfs::PollMask) {
        self.fired.store(true, Ordering::SeqCst);
        self.queue.wake_all();
    }
}

/// Subscribe before rechecking input, so an IRQ cannot fall between the
/// empty read and the park. VT input retains its existing periodic recheck.
fn wait_console(io: ConsoleIo) -> Result<(), u32> {
    let wait =
        Arc::new(ConsoleReadWait { queue: sched::WaitQueue::new(), fired: core::sync::atomic::AtomicBool::new(false) });
    let _guard = if io == ConsoleIo::Serial {
        Some(serial_in::subscribe_poll(Arc::clone(&wait) as Arc<dyn lumen_vfs::PollWaker>))
    } else {
        None
    };
    if stdin_ready(io) {
        return Ok(());
    }
    let deadline = Some(crate::time::now_ns().saturating_add(5_000_000));
    // The serial interruption fixture controls both wake sources. Remove the
    // safety deadline there so a host stall cannot masquerade as its second
    // read and cause the rescue byte to beat the pending signal.
    #[cfg(feature = "ktest")]
    let deadline =
        if io == ConsoleIo::Serial && SERIAL_READ_PHASE.load(Ordering::SeqCst) != 0 { None } else { deadline };
    if wait.queue.wait_interruptible(deadline, || wait.fired.load(Ordering::SeqCst)) == sched::Wake::Interrupted {
        Err(super::signal::ERESTARTSYS)
    } else {
        Ok(())
    }
}

fn read_serial(dst: &mut [u8], interruptible: bool) -> Result<usize, u32> {
    let first = if let Some(byte) = with_current_console(ConsoleState::pop_unread) {
        byte
    } else {
        loop {
            if let Some(b) = serial_in::read_byte() {
                break b;
            }
            #[cfg(feature = "ktest")]
            serial_read_window();
            if interruptible {
                wait_console(ConsoleIo::Serial)?;
            } else {
                serial_in::wait();
            }
        }
    };

    // Ctrl-D at start of read -> EOF (0 bytes).
    if first == 0x04 {
        return Ok(0);
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
            with_current_console(|proc| proc.push_unread_bytes(&[0x04]));
            break;
        }
        dst[n] = if b == b'\r' { b'\n' } else { b };
        n += 1;
    }
    Ok(n)
}

#[cfg(feature = "ktest")]
static SERIAL_READ_PHASE: AtomicU32 = AtomicU32::new(0);

/// Pause the first empty read before choosing its wait, then record a
/// second empty read so the test can supply data (also rescuing a mutant).
#[cfg(feature = "ktest")]
fn serial_read_window() {
    if SERIAL_READ_PHASE.compare_exchange(1, 2, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
        while SERIAL_READ_PHASE.load(Ordering::SeqCst) == 2 {
            crate::time::sleep(1_000_000);
        }
    } else {
        let _ = SERIAL_READ_PHASE.compare_exchange(3, 4, Ordering::SeqCst, Ordering::SeqCst);
    }
}

fn read_vt(vt: VtId, dst: &mut [u8], interruptible: bool) -> Result<usize, u32> {
    loop {
        if let Some(first) = with_current_console(ConsoleState::pop_unread) {
            if first == 0x04 {
                return Ok(0);
            }
            dst[0] = first;
            let mut n = 1;
            while n < dst.len() {
                let next = with_current_console(|proc| {
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
            return Ok(n);
        }
        let key = if interruptible {
            match crate::vt::read_key(vt.index()) {
                Some(key) => key,
                None => {
                    wait_console(ConsoleIo::Vt(vt))?;
                    continue;
                }
            }
        } else {
            crate::vt::wait_key(vt.index())
        };
        let mut enc = [0u8; 4];
        let len = vt_key_to_bytes(key, &mut enc);
        if len == 0 {
            continue;
        }
        if len == 1 && enc[0] == 0x04 {
            return Ok(0);
        }
        let n = len.min(dst.len());
        dst[..n].copy_from_slice(&enc[..n]);
        if n < len {
            with_current_console(|proc| proc.push_unread_bytes(&enc[n..len]));
        }
        return Ok(n);
    }
}

/// Where [`in_test_process`] maps user memory for a ktest (read-write).
#[cfg(feature = "ktest")]
pub(crate) const KTEST_USER_VA: u64 = 0x0000_0000_0060_0000;

/// How much user memory [`in_test_process`] maps at [`KTEST_USER_VA`].
#[cfg(feature = "ktest")]
pub(crate) const KTEST_USER_LEN: u64 = 4 * 4096;

/// Run `f` as a Linux process for a ktest: on a pinned user thread with its own
/// address space ([`KTEST_USER_LEN`] bytes mapped at [`KTEST_USER_VA`], so
/// handlers can copy to and from user memory) and a [`RegisteredProcess`],
/// handed its pid. `f`'s result comes back once the process is taken down,
/// whatever `f` returned.
#[cfg(feature = "ktest")]
pub(crate) fn in_test_process<R: Send + 'static>(f: impl FnOnce(u32) -> R + Send + 'static) -> Result<R, &'static str> {
    use lumen_vm::Prot;

    let space = Arc::new(AddressSpace::new().map_err(|_| "out of frames for AddressSpace")?);
    if space.map_eager_with_bytes(KTEST_USER_VA, KTEST_USER_LEN, Prot::READ | Prot::WRITE, &[]).is_err() {
        space.teardown();
        return Err("map the test process's user memory");
    }
    let Ok(stdio) = Stdio::open() else {
        space.teardown();
        return Err("open the test process's stdio");
    };
    let slot: Arc<SpinLock<Option<R>>> = Arc::new(SpinLock::new(None));
    let out = Arc::clone(&slot);
    let spawned =
        sched::spawn_user_pinned_on(crate::arch::cpu_index(), "ktest-process", Arc::clone(&space), move || {
            let registered = RegisteredProcess::register(stdio);
            let result = f(registered.pid());
            // Its takedown takes this thread off the space and tears it down.
            drop(registered);
            *out.lock() = Some(result);
        });
    match spawned {
        Ok(handle) => {
            handle.join();
            // Its takedown gave up the space's one user and tore it down.
            if space.users() != 0 {
                return Err("the test process's takedown left its space a user");
            }
            slot.lock().take().ok_or("the test process ended without a result")
        }
        Err(_) => {
            // No process adopted the space: its one user is still ours.
            space.teardown();
            Err("spawn the test process")
        }
    }
}

/// Run `f` on this kernel thread inside `space` (a `UserSpaceGuard`), a
/// process's space that the process may give up meanwhile, holding a
/// process user of it for the whole time: `None` (`f` not run) if the space
/// is already retired. Without the user, the process's takedown could tear
/// the space down while this thread is preempted on the same CPU with its
/// saved state still naming the space: `schedule` would load that root
/// again when it switches back, its user half cleared and its tables freed
/// under it, and `teardown`'s `CR3` assertion checks only the CPU that tears
/// down. Whichever side drops the last user tears the space down, here only
/// after the guard has switched `CR3` back.
#[cfg(feature = "ktest")]
pub(crate) fn with_process_user<R>(space: &Arc<AddressSpace>, f: impl FnOnce() -> R) -> Option<R> {
    if !space.add_user() {
        return None;
    }
    let result = {
        let _active = sched::UserSpaceGuard::enter(Arc::clone(space));
        f()
    };
    space.drop_user();
    Some(result)
}

/// Link a non-leader thread of `process` into `PROCS` with a tid from its
/// tree (`ProcTable::link_thread`), as M8's `clone` will: the ktests'
/// second tid. It runs nowhere (its scheduler thread id is 0).
#[cfg(feature = "ktest")]
pub(crate) fn add_ktest_thread(process: &Arc<Process>) -> Result<Arc<UserThread>, &'static str> {
    let thread = Arc::new(UserThread::unbound_for_ktest(Arc::clone(process)));
    PROCS.lock().link_ktest_thread(&thread).map_err(|_| "ProcTree::add_thread refused")?;
    Ok(thread)
}

/// Unlink a thread [`add_ktest_thread`] linked (`ProcTable::unlink_thread`).
#[cfg(feature = "ktest")]
pub(crate) fn remove_ktest_thread(thread: &Arc<UserThread>) {
    let gone = PROCS.lock().unlink_thread(thread);
    drop(gone);
}

crate::ktest!(
    fn procs_lookup_and_lifetime() {
        use crate::shell::sysinfo::{DASH, int};
        use crate::{kassert, kassert_eq};
        use lumen_shell::pipe::Value;

        PROCS.lock().check()?;
        // Pid 1: the kernel's, in PROCS with no threads, its placeholder
        // space but no descriptors or working directory, its SIGCHLD
        // ignored (its children's zombies go at once); `ps` lists it, /proc
        // doesn't.
        let init = lookup(INIT_PID).ok_or("pid 1 isn't in PROCS")?;
        kassert_eq!(init.pid(), INIT_PID);
        kassert!(init.address_space().is_some() && init.files.lock().is_none() && init.fs.lock().is_none());
        kassert!(init.autoreaps());
        kassert!(lookup_thread(INIT_PID).is_none());
        kassert!(!has_pid(INIT_PID) && !active_pids().contains(&INIT_PID));
        kassert!(ps_rows().iter().any(|r| r.pid == INIT_PID && r.threads.is_empty() && r.ppid == 0));
        let ps = crate::shell::sysinfo::ps_table();
        let row = ps.rows().iter().find(|r| r.first() == Some(&int(INIT_PID))).ok_or("`ps` lacks pid 1")?;
        kassert_eq!(&row[1..4], [int(0), DASH, DASH]);
        drop(init);

        type Seen = (u32, u32, Arc<Process>, Arc<Files>, Arc<AddressSpace>, (u32, bool, bool, bool));
        let (pid, tid, held, files, space, inner) = in_test_process(|pid| -> Result<Seen, &'static str> {
            // By pid and by tid, the running thread's own process and thread.
            let me = current_thread();
            let process = lookup(pid).ok_or("its process isn't in PROCS")?;
            kassert!(pid != INIT_PID);
            kassert_eq!(me.tid(), pid);
            kassert!(Arc::ptr_eq(me.process(), &process));
            kassert!(lookup_thread(pid).is_some_and(|t| Arc::ptr_eq(&t, &me)));
            kassert_eq!((process.pid(), process.ppid()), (pid, 0));
            kassert_eq!(try_current_pid(), Some(pid));
            me.set_clear_child_tid(0x1234);
            kassert_eq!(current_thread().clear_child_tid(), 0x1234);
            // While it runs, its slots are full: the space its thread runs
            // in (the one process user), its descriptors and its directory.
            let space = process.address_space().ok_or("no address space")?;
            let running = sched::current_address_space().ok_or("the thread has no space")?;
            kassert!(Arc::ptr_eq(&space, &running));
            kassert_eq!(space.users(), 1);
            kassert!(process.fs.lock().is_some());
            let files = process.files.lock().clone().ok_or("no descriptors")?;
            kassert_eq!((files.users(), files.open_count()), (1, 3));
            // A second sharer leaving closes nothing: only the last does.
            let shared = files.share();
            kassert_eq!(files.users(), 2);
            shared.leave(0);
            kassert_eq!((files.users(), files.open_count()), (1, 3));

            // A second thread: its tid finds it and the same process; a tid
            // is no pid. Unlinked before the takedown whatever the checks
            // say.
            let extra = add_ktest_thread(&process)?;
            let tid = extra.tid();
            let checked = (|| {
                PROCS.lock().check()?;
                kassert!(tid != pid && tid != INIT_PID);
                kassert!(lookup_thread(tid).is_some_and(|t| Arc::ptr_eq(&t, &extra)));
                kassert!(Arc::ptr_eq(extra.process(), &process));
                kassert!(lookup(tid).is_none() && !has_pid(tid));
                let row = ps_rows().into_iter().find(|r| r.pid == pid).ok_or("ps lacks the process")?;
                let tids: Vec<u32> = row.threads.iter().map(|&(t, _)| t).collect();
                kassert_eq!(tids, [pid, tid]);
                // `ps`: the leader's tid first, its state the running
                // thread's (the extra one runs nowhere: id 0 is no thread's).
                let ps = crate::shell::sysinfo::ps_table();
                let row = ps.rows().iter().find(|r| r.first() == Some(&int(pid))).ok_or("`ps` lacks the process")?;
                kassert_eq!(row[2], Value::Text(alloc::format!("{pid},{tid}")));
                kassert_eq!(row[3], Value::Text(String::from("running,gone")));
                // A second process at once, on its own thread: its pid and
                // tid find it, not this one.
                let inner = in_test_process(move |inner| {
                    let mine = current_process();
                    let by_pid = lookup(inner).is_some_and(|p| Arc::ptr_eq(&p, &mine));
                    let by_tid = lookup_thread(inner).is_some_and(|t| Arc::ptr_eq(t.process(), &mine));
                    let outer = lookup(pid).is_some_and(|p| !Arc::ptr_eq(&p, &mine))
                        && lookup_thread(tid).is_some_and(|t| !Arc::ptr_eq(t.process(), &mine));
                    (inner, by_pid, by_tid, outer)
                })?;
                Ok(inner)
            })();
            remove_ktest_thread(&extra);
            let inner: (u32, bool, bool, bool) = checked?;
            kassert!(lookup_thread(tid).is_none());
            kassert!(!PROCS.lock().is_busy(tid));
            PROCS.lock().check()?;
            Ok((pid, tid, process, files, space, inner))
        })??;

        let (inner_pid, by_pid, by_tid, outer) = inner;
        kassert!(inner_pid != pid && inner_pid != tid && inner_pid != INIT_PID);
        kassert!(by_pid && by_tid && outer);
        kassert!(lookup(inner_pid).is_none());

        // Taken down: gone from PROCS by pid and tid, its slots emptied, its
        // space's one user given up (torn down: the test's `Arc` only
        // observes it), its descriptors closed (it was their only user),
        // and the test's reference the last one.
        PROCS.lock().check()?;
        kassert!(lookup(pid).is_none() && lookup_thread(pid).is_none() && !has_pid(pid));
        kassert!(!ps_rows().iter().any(|r| r.pid == pid));
        kassert!(held.mm.lock().is_none() && held.files.lock().is_none() && held.fs.lock().is_none());
        kassert_eq!(space.users(), 0);
        kassert_eq!((files.users(), files.open_count()), (0, 0));
        let weak = Arc::downgrade(&held);
        drop(held);
        kassert!(weak.upgrade().is_none());
        Ok(())
    }
);

crate::ktest!(
    fn procs_leave_no_heap_behind() {
        use crate::kassert;
        // A process's run (register, takedown, exit in the tree) gives back
        // every allocation it made: the heap's live count and bytes in use
        // are the same before and after 16 runs, once one run has grown the
        // tables' nodes (16, so even a leak of a table entry alone forces
        // node allocations). A leak shows on every try; another thread's
        // allocation meanwhile only on one, so the counts are retaken up to
        // three times.
        in_test_process(|_| ())?;
        let mut deltas = Vec::new();
        for _ in 0..3 {
            let before = crate::mm::heap::stats();
            for _ in 0..16 {
                in_test_process(|_| ())?;
            }
            let after = crate::mm::heap::stats();
            let delta = (after.live as i64 - before.live as i64, after.used as i64 - before.used as i64);
            deltas.push(delta);
            if delta == (0, 0) {
                break;
            }
        }
        if deltas.last() != Some(&(0, 0)) {
            crate::ktest::diag(format_args!("(live, used) deltas per try of 16 runs: {deltas:?}"));
        }
        kassert!(deltas.last() == Some(&(0, 0)));
        Ok(())
    }
);

crate::ktest!(
    fn exit_status_follows_traps_c() {
        // Each end as the tree records it: an exit's status byte, or
        // Linux's `traps.c` signal for the vector (ARCHITECTURE section 6).
        use crate::kassert_eq;
        kassert_eq!(exit_status(&UserExit::Exited(256 + 3)), ExitStatus::Exited(3));
        let killed = |signal| ExitStatus::Killed { signal, core: false };
        for (vector, signal) in [
            (0, Signal::FPE),
            (1, Signal::TRAP),
            (3, Signal::TRAP),
            (6, Signal::ILL),
            (9, Signal::FPE),
            (10, Signal::SEGV),
            (11, Signal::BUS),
            (12, Signal::BUS),
            (13, Signal::SEGV),
            (14, Signal::SEGV),
            (16, Signal::FPE),
            (17, Signal::BUS),
            (19, Signal::FPE),
            (21, Signal::SEGV),
        ] {
            let fault = UserExit::Fault { vector, error_code: 0, rip: 0, cr2: 0 };
            kassert_eq!((vector, exit_status(&fault)), (vector, killed(signal)));
        }
        kassert_eq!(exit_status(&UserExit::NonCanonicalRip(1 << 47)), killed(Signal::SEGV));
        kassert_eq!(exit_status(&UserExit::BusError { rip: 0, cr2: 0 }), killed(Signal::BUS));
        Ok(())
    }
);

crate::ktest!(
    fn takedown_warns_at_most_once_a_second() {
        use crate::kassert;
        kassert!(warn_due(None, 0));
        kassert!(!warn_due(Some(5), 5 + lumen_time::NS_PER_SEC - 1));
        kassert!(warn_due(Some(5), 5 + lumen_time::NS_PER_SEC));
        // A clock read before the last warning's (never later) waits.
        kassert!(!warn_due(Some(10), 3));
        Ok(())
    }
);

crate::ktest!(
    fn signal_state_refuses_kill_and_stop() {
        // `set_action` changes no disposition of SIGKILL or SIGSTOP (the
        // syscall refuses them first; this is the state's own guard), and
        // strips both from another action's mask.
        use crate::kassert_eq;
        let mut state = SignalState::new();
        let act = KernelSigaction { handler: 0x1000, flags: 0, restorer: 0, mask: u64::MAX };
        for signal in [Signal::KILL, Signal::STOP] {
            kassert_eq!(state.set_action(signal, act), None);
            kassert_eq!(state.action(signal), KernelSigaction::default());
        }
        kassert_eq!(state.set_action(Signal::USR1, act), Some(KernelSigaction::default()));
        kassert_eq!(state.action(Signal::USR1).mask, SigSet::ALL.unblockable_stripped().0);
        Ok(())
    }
);

crate::ktest!(
    fn process_state_owner_methods() {
        // `record_exit` keeps the first end; `/proc/<pid>/maps` reads empty
        // while the process has no address space (its takedown, a zombie),
        // as Linux's; the umask keeps only its `0o777` bits.
        use crate::{kassert, kassert_eq};
        in_test_process(|pid| {
            let process = current_process();
            record_exit(UserExit::Exited(3));
            record_exit(UserExit::Exited(4));
            kassert_eq!(*process.exit.lock(), Some(UserExit::Exited(3)));

            kassert!(process_maps(pid).is_some_and(|m| !m.is_empty()));
            let mm = process.mm.lock().take();
            let empty = process_maps(pid);
            *process.mm.lock() = mm;
            kassert_eq!(empty, Some(Vec::new()));
            kassert_eq!(process_maps(INIT_PID), None);

            let old = swap_current_umask(0o7777);
            kassert_eq!((old, current_umask()), (DEFAULT_UMASK, 0o777));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn in_test_process_takes_its_process_down() {
        use crate::kassert;
        let (pid, listed) = in_test_process(|pid| (pid, active_pids().contains(&pid)))?;
        kassert!(listed);
        kassert!(!active_pids().contains(&pid));
        Ok(())
    }
);

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
                kassert!(file.inode().is_tty());
                kassert_eq!(&*file.path, b"/dev/console");
                kassert_eq!((file.is_readable(), file.is_writable()), (readable, writable));
                kassert_eq!(current_cloexec(fd), Ok(false));
            }
            // `/` is the working directory, with the default umask.
            let (cwd, path) = current_cwd();
            let root = crate::vfs::resolve_path_from_root("/").map_err(|_| "resolve /")?;
            kassert!(Arc::ptr_eq(&cwd, &root));
            kassert_eq!(&*path, b"/");
            kassert_eq!(current_umask(), 0o022);
            // A missing console refuses the whole set (the run's refusal).
            kassert!(matches!(Stdio::open_from(b"/dev/no-such-console"), Err(VfsError::NoEnt)));
            // A working directory must be a directory.
            let console = crate::vfs::resolve_path_from_root("/dev/console").map_err(|_| "resolve the console")?;
            let guard = OpenGuard::try_new(console).map_err(|_| "guard")?;
            kassert!(matches!(Cwd::new(guard, b"/dev/console"), Err(VfsError::NotDir)));
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
        fn open(path: &[u8]) -> Result<KernelFile, &'static str> {
            open_kernel_file(path, OpenFlags::O_RDWR | OpenFlags::O_CREAT, 0o644).map_err(|_| "open")
        }
        // The `flock` and `pid`'s POSIX record-lock counts on `file`'s inode.
        fn counts(file: &FileRef, pid: u32) -> (usize, usize) {
            let mut got = (usize::MAX, usize::MAX);
            let _ = file.inode().with_locks_mut(&mut |l| got = (l.flock_count(), l.posix_lock_count(pid)));
            got
        }
        fn posix_lock(file: &FileRef, pid: u32) -> Result<(), VfsError> {
            let spec = FlockSpec { l_type: general::F_WRLCK as i16, l_whence: 0, l_start: 0, l_len: 10, l_pid: 0 };
            let ctx = LockContext { cur_offset: 0, file_size: 0, readable: true, writable: true };
            let mut res = Err(VfsError::Io);
            file.inode().with_locks_mut(&mut |l| res = l.setlk(pid, spec, ctx, false).map(drop))?;
            res
        }
        fn flock_ex(file: &FileRef) -> Result<(), VfsError> {
            let ex = FlockOp { kind: Some(FlockKind::Exclusive), nonblock: true };
            let mut res = Err(VfsError::Io);
            file.inode().with_locks_mut(&mut |l| res = l.flock(file.id, ex).map(drop))?;
            res
        }

        let (pid, held) = in_test_process(|pid| -> Result<(u32, FileRef), &'static str> {
            // A holds an exclusive `flock` and sits on fds 3 and 4 (a `dup`:
            // one open file description, two slots).
            insert_current_fd(3, open(PATH)?).map_err(|_| "install fd 3")?;
            let a = current_file(3).ok_or("fd 3")?;
            flock_ex(&a).map_err(|_| "flock A")?;
            kassert_eq!(dup_current_fd(3, 4, false), Ok(4));
            kassert_eq!(current_cloexec(4), Ok(false));
            kassert_eq!(a.fd_ref_count(), 2);
            // `dup` onto itself or from a closed fd changes nothing.
            kassert_eq!(dup_current_fd(4, 4, true), Ok(4));
            kassert_eq!(dup_current_fd(77, 8, false), Err(VfsError::BadF));
            kassert_eq!(dup_current_fd(3, lumen_vfs::MAX_FDS as i32, false), Err(VfsError::BadF));
            kassert_eq!(a.fd_ref_count(), 2);
            // The lowest free slot at or above a minimum (`F_DUPFD_CLOEXEC`):
            // 10 here; a minimum out of range or a closed `old` refused,
            // counting nothing.
            kassert_eq!(dup_current_fd_from(3, 10, true), Ok(10));
            kassert_eq!(current_cloexec(10), Ok(true));
            kassert_eq!(a.fd_ref_count(), 3);
            kassert_eq!(dup_current_fd_from(3, -1, false), Err(VfsError::Inval));
            kassert_eq!(dup_current_fd_from(3, lumen_vfs::MAX_FDS as i32, false), Err(VfsError::Inval));
            kassert_eq!(dup_current_fd_from(77, 0, false), Err(VfsError::BadF));
            kassert_eq!(a.fd_ref_count(), 3);
            kassert_eq!(close_current_fd(10), Ok(()));
            kassert_eq!(a.fd_ref_count(), 2);
            // B, another open of the same file, on fd 5 holds a POSIX lock;
            // a file on another inode on fd 6.
            kassert_eq!(install_current_fd(open(PATH)?, false), Ok(5));
            let b = current_file(5).ok_or("fd 5")?;
            kassert_eq!(install_current_fd(open(OTHER)?, false), Ok(6));
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

#[cfg(feature = "ktest")]
const DUP_IDLE: u32 = 0;
#[cfg(feature = "ktest")]
const DUP_ARMED: u32 = 1;
#[cfg(feature = "ktest")]
const DUP_LOOKED_UP: u32 = 2;
#[cfg(feature = "ktest")]
const DUP_SOURCE_CLOSED: u32 = 3;

/// The dup/close test's rendezvous. The correct path only publishes under
/// its spinlock and cannot be preempted there on this CPU. A lookup moved
/// outside that hold lets the close finish here, exposing the lost flock
/// before the duplicate can be installed.
#[cfg(feature = "ktest")]
static DUP_LOOKUP_PHASE: AtomicU32 = AtomicU32::new(DUP_IDLE);

#[cfg(feature = "ktest")]
fn dup_lookup_window() {
    if DUP_LOOKUP_PHASE.compare_exchange(DUP_ARMED, DUP_LOOKED_UP, Ordering::SeqCst, Ordering::SeqCst).is_ok()
        && crate::arch::irq::depth() == 0
    {
        while DUP_LOOKUP_PHASE.load(Ordering::SeqCst) != DUP_SOURCE_CLOSED {
            crate::time::sleep(1_000_000);
        }
    }
}

#[cfg(feature = "ktest")]
const OPEN_IDLE: u32 = 0;
#[cfg(feature = "ktest")]
const OPEN_ARMED: u32 = 1;
#[cfg(feature = "ktest")]
const OPEN_PAUSED: u32 = 2;
#[cfg(feature = "ktest")]
const OPEN_RELEASED: u32 = 3;
#[cfg(feature = "ktest")]
static OPEN_INSTALL_PHASE: AtomicU32 = AtomicU32::new(OPEN_IDLE);

/// Pause one open after its filesystem work and before installing its slot.
#[cfg(feature = "ktest")]
pub(crate) fn open_install_window() {
    if OPEN_INSTALL_PHASE.compare_exchange(OPEN_ARMED, OPEN_PAUSED, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
        while OPEN_INSTALL_PHASE.load(Ordering::SeqCst) != OPEN_RELEASED {
            crate::time::sleep(1_000_000);
        }
    }
}

/// A ktest's additional thread in the current process. The caller must
/// join it before its registered process leaves (and release any locks
/// the thread is waiting for before joining).
#[cfg(feature = "ktest")]
pub(crate) struct KtestThread<R> {
    handle: sched::JoinHandle,
    result: Arc<SpinLock<Option<Result<R, &'static str>>>>,
}

#[cfg(feature = "ktest")]
impl<R> KtestThread<R> {
    pub(crate) fn thread(&self) -> &sched::Thread {
        self.handle.thread()
    }

    pub(crate) fn finished(&self) -> bool {
        self.handle.is_finished()
    }

    pub(crate) fn blocked(&self) -> bool {
        let id = self.handle.thread().id();
        sched::threads().iter().any(|t| t.id == id && t.state == sched::State::Blocked)
    }

    /// Wait until this thread blocks or finishes; return whether it blocked.
    pub(crate) fn settle(&self) -> bool {
        while !self.blocked() && !self.finished() {
            crate::time::sleep(1_000_000);
        }
        self.blocked()
    }

    pub(crate) fn join(self) -> Result<R, &'static str> {
        self.handle.join();
        self.result.lock().take().ok_or("test thread ended without a result")?
    }
}

/// Run a ktest closure as another Linux thread of the current process,
/// on this CPU (B6 adds migratable user threads). Each gets
/// its own tid and must use a disjoint user buffer. Its identity is
/// unlinked before completion, even when the closure returns an error.
#[cfg(feature = "ktest")]
pub(crate) fn spawn_test_thread<R: Send + 'static>(
    f: impl FnOnce(u32) -> R + Send + 'static,
) -> Result<KtestThread<R>, &'static str> {
    let process = current_process();
    let space = process.address_space().ok_or("test process has no address space")?;
    let result = Arc::new(SpinLock::new(None));
    let out = Arc::clone(&result);
    let handle = sched::spawn_on(crate::arch::cpu_index(), "hazard-thread", move || {
        let ran = with_process_user(&space, || {
            let thread = Arc::new(UserThread::for_current(process));
            PROCS.lock().link_ktest_thread(&thread).map_err(|_| "link the test thread")?;
            let tid = thread.tid();
            let prev = sched::set_current_user_thread(Some(Arc::clone(&thread)));
            assert!(prev.is_none());
            let value = f(tid);
            drop(sched::set_current_user_thread(None));
            remove_ktest_thread(&thread);
            Ok(value)
        })
        .ok_or("test address space already retired")
        .and_then(|r| r);
        *out.lock() = Some(ran);
    })
    .map_err(|_| "spawn the test thread")?;
    Ok(KtestThread { handle, result })
}

/// One-shot pauses at the three position races. Only the armed open
/// description is delayed, with all Lock-2 and spinlock guards released.
#[cfg(feature = "ktest")]
pub(crate) mod position_window {
    use super::{KernelFile, KtestThread, SpinLock};
    use lumen_vfs::OpenFileId;

    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Point {
        Snapshot,
        ReadCopy,
        DirCopy,
    }
    struct State {
        target: Option<(OpenFileId, Point)>,
        held: bool,
        go: bool,
    }
    static WINDOW: SpinLock<State> = SpinLock::new(State { target: None, held: false, go: false });
    pub(super) struct Armed;

    pub(super) fn arm(file: &KernelFile, point: Point) -> Armed {
        *WINDOW.lock() = State { target: Some((file.id, point)), held: false, go: false };
        Armed
    }

    impl Armed {
        pub(super) fn wait<R>(&self, thread: &KtestThread<R>) -> Result<(), &'static str> {
            while !WINDOW.lock().held {
                if thread.finished() {
                    return Err("position call missed its pause");
                }
                crate::time::sleep(1_000_000);
            }
            Ok(())
        }
    }

    impl Drop for Armed {
        fn drop(&mut self) {
            let mut w = WINDOW.lock();
            w.target = None;
            w.go = true;
        }
    }

    pub(crate) fn hold(file: &KernelFile, point: Point) {
        {
            let mut w = WINDOW.lock();
            if w.target != Some((file.id, point)) {
                return;
            }
            w.target = None;
            w.held = true;
        }
        while !WINDOW.lock().go {
            crate::time::sleep(1_000_000);
        }
    }
}

#[cfg(feature = "ktest")]
fn hazard_call(nr: u32, rdi: u64, rsi: u64, rdx: u64, r10: u64) -> u64 {
    let mut frame = crate::arch::syscall::SyscallFrame { rax: u64::from(nr), rdi, rsi, rdx, r10, ..Default::default() };
    let _ = super::syscall::dispatch(&mut frame);
    frame.rax
}

/// Run the first syscall into its armed pause, then let a second contend.
/// The second either blocks or finishes; no time ceiling decides which.
/// Both threads are joined and the pause released on every failure path.
#[cfg(feature = "ktest")]
fn position_race(
    file: &KernelFile,
    point: position_window::Point,
    first: impl FnOnce(u32) -> Result<Vec<u8>, &'static str> + Send + 'static,
    second: impl FnOnce(u32) -> Result<Vec<u8>, &'static str> + Send + 'static,
) -> Result<Vec<u8>, &'static str> {
    let pause = position_window::arm(file, point);
    let a = spawn_test_thread(first)?;
    let b = match pause.wait(&a).and_then(|()| spawn_test_thread(second)) {
        Ok(b) => b,
        Err(e) => {
            drop(pause);
            let _ = a.join();
            return Err(e);
        }
    };
    let blocked = b.settle();
    drop(pause);
    let a = a.join();
    let b = b.join();
    let mut all = a??;
    all.extend(b??);
    crate::kassert!(blocked);
    Ok(all)
}

#[cfg(feature = "ktest")]
fn hazard_read(fd: u64, buf: u64, count: u64) -> Result<Vec<u8>, &'static str> {
    let n = hazard_call(general::__NR_read, fd, buf, count, 0);
    if n > count {
        return Err("shared read failed");
    }
    let mut bytes = alloc::vec![0; n as usize];
    crate::user::copy_from_user(&mut bytes, buf).map_err(|_| "read back shared bytes")?;
    Ok(bytes)
}

crate::ktest!(
    fn shared_description_offsets_move_once() {
        use crate::{kassert, kassert_eq};
        use lumen_vfs::{FileSystem, VfsLock};
        in_test_process(|_| {
            let fs = crate::vfs::tmpfs::Tmpfs::new(lumen_vfs::DevId::makedev(0, 379), 64 * 4096);
            let inode = fs.root_inode().create(b"position", 0o600).map_err(|_| "create position file")?;
            let pattern: Vec<u8> = (0..8192).map(|i| (i % 251) as u8).collect();
            kassert_eq!(inode.write_at(0, &pattern), Ok(pattern.len()));
            let regular = KernelFile::new_unchecked_flags(
                inode,
                Arc::from(&b"/position"[..]),
                OpenFlags::from_bits(OpenFlags::O_RDWR),
            )
            .map_err(|_| "open position file")?;
            let proc = open_kernel_file(b"/proc/self/maps", OpenFlags::O_RDONLY, 0).map_err(|_| "open maps")?;
            for opened in [regular, proc] {
                let fd = install_current_fd(opened, false).map_err(|_| "install shared file")? as u64;
                let file = current_file(fd as i32).ok_or("shared file")?;
                let mut expected = alloc::vec![0; 16384];
                let n = file.inode().read_at(0, &mut expected).map_err(|_| "read expected bytes")?;
                expected.truncate(n);
                kassert!(n > 1);
                let point = if file.inode().dev_id() == lumen_vfs::DevId::PROCFS {
                    position_window::Point::Snapshot
                } else {
                    position_window::Point::ReadCopy
                };
                let mut got = position_race(
                    &file,
                    point,
                    move |_| hazard_read(fd, KTEST_USER_VA, 1),
                    move |_| hazard_read(fd, KTEST_USER_VA + 4096, 1),
                )?;
                kassert_eq!(got.as_slice(), &expected[..2]);
                got.extend(hazard_read(fd, KTEST_USER_VA, 16384)?);
                kassert_eq!(got, expected);
                kassert_eq!(*file.offset.lock(), n as u64);

                // A failed copy's tentative byte must be given back
                // before the contender reads, on regular and procfs files.
                kassert_eq!(hazard_call(general::__NR_lseek, fd, 0, 0, 0), 0);
                let mut got = position_race(
                    &file,
                    position_window::Point::ReadCopy,
                    move |_| {
                        kassert_eq!(
                            hazard_call(general::__NR_read, fd, 0, 1, 0) as i64,
                            -(super::errno::EFAULT as i64)
                        );
                        Ok(Vec::new())
                    },
                    move |_| hazard_read(fd, KTEST_USER_VA + 4096, 1),
                )?;
                kassert_eq!(got.as_slice(), &expected[..1]);
                got.extend(hazard_read(fd, KTEST_USER_VA, 16384)?);
                kassert_eq!(got, expected);
                kassert_eq!(*file.offset.lock(), n as u64);
                if file.is_writable() {
                    // Each public position-based handler must contend on
                    // this description, vector calls included. Positional
                    // I/O must finish while the same mutex is still held.
                    let buf = KTEST_USER_VA + 4096;
                    let iov = KTEST_USER_VA + 12288;
                    let mut raw = Vec::new();
                    for addr in [buf, buf + 1] {
                        raw.extend_from_slice(&addr.to_le_bytes());
                        raw.extend_from_slice(&1u64.to_le_bytes());
                    }
                    crate::user::copy_to_user(iov, &raw).map_err(|_| "copy shared iovecs")?;
                    for (nr, arg, count, delta, moved, atomic) in [
                        (general::__NR_readv, iov, 2, 0, 2, true),
                        (general::__NR_write, buf, 2, 0, 2, true),
                        (general::__NR_writev, iov, 2, 0, 2, true),
                        (general::__NR_lseek, 7, u64::from(general::SEEK_SET), 0, 7, true),
                        (general::__NR_pread64, buf, 2, 10, 2, false),
                        (general::__NR_pwrite64, buf, 2, 10, 2, false),
                    ] {
                        kassert_eq!(hazard_call(general::__NR_lseek, fd, 0, 0, 0), 0);
                        let pos = file.lock_pos().expect("regular file position");
                        let worker = spawn_test_thread(move |_| hazard_call(nr, fd, arg, count, delta))?;
                        let blocked = worker.settle();
                        drop(pos);
                        let ret = worker.join()?;
                        kassert_eq!((nr, blocked), (nr, atomic));
                        kassert_eq!(ret, moved);
                        kassert_eq!(*file.offset.lock(), if atomic { moved } else { 0 });
                    }
                }
                close_current_fd(fd as i32).map_err(|_| "close shared file")?;
            }
            Ok(())
        })?
    }
);

#[cfg(feature = "ktest")]
fn hazard_list(fd: u64, buf: u64) -> Result<Vec<u8>, &'static str> {
    let n = hazard_call(general::__NR_getdents64, fd, buf, 24, 0);
    if n == 0 {
        return Ok(Vec::new());
    }
    crate::kassert_eq!(n, 24);
    let mut raw = [0; 24];
    crate::user::copy_from_user(&mut raw, buf).map_err(|_| "read directory record")?;
    crate::kassert_eq!(u16::from_le_bytes([raw[16], raw[17]]), 24);
    let end = raw[19..].iter().position(|&b| b == 0).ok_or("dirent lacks NUL")?;
    let mut name = raw[19..19 + end].to_vec();
    name.push(0);
    Ok(name)
}

crate::ktest!(
    fn shared_description_getdents_lists_once() {
        use crate::kassert_eq;
        use lumen_vfs::FileSystem;
        in_test_process(|_| {
            let fs = crate::vfs::tmpfs::Tmpfs::new(lumen_vfs::DevId::makedev(0, 380), 64 * 4096);
            let root = fs.root_inode();
            for name in [b"a", b"b", b"c"] {
                root.create(name, 0o600).map_err(|_| "create listed file")?;
            }
            let opened = KernelFile::new_unchecked_flags(
                root,
                Arc::from(&b"/listing"[..]),
                OpenFlags::from_bits(OpenFlags::O_RDONLY),
            )
            .map_err(|_| "open directory")?;
            let fd = install_current_fd(opened, false).map_err(|_| "install directory")? as u64;
            let file = current_file(fd as i32).ok_or("directory fd")?;
            for fault in [false, true] {
                kassert_eq!(hazard_call(general::__NR_lseek, fd, 0, 0, 0), 0);
                let mut got = position_race(
                    &file,
                    position_window::Point::DirCopy,
                    move |_| {
                        if fault {
                            kassert_eq!(
                                hazard_call(general::__NR_getdents64, fd, 0, 24, 0) as i64,
                                -(super::errno::EFAULT as i64)
                            );
                            Ok(Vec::new())
                        } else {
                            hazard_list(fd, KTEST_USER_VA)
                        }
                    },
                    move |_| hazard_list(fd, KTEST_USER_VA + 4096),
                )?;
                // Bounded by the five names plus EOF; a broken cursor
                // cannot turn the test into an unbounded allocation.
                for _ in 0..6 {
                    let next = hazard_list(fd, KTEST_USER_VA)?;
                    if next.is_empty() {
                        break;
                    }
                    got.extend(next);
                }
                kassert_eq!(got.as_slice(), b".\0..\0a\0b\0c\0");
                kassert_eq!(hazard_list(fd, KTEST_USER_VA)?, Vec::<u8>::new());
            }
            Ok(())
        })?
    }
);

/// A successful dup racing the source's close must keep the description's
/// flock held: close either precedes lookup (EBADF) or follows duplication
/// (another slot still owns the flock). Force the latter at the lookup.
#[cfg(feature = "ktest")]
fn duplicate_racing_close_keeps_flock() -> crate::ktest::TestResult {
    use crate::{kassert, kassert_eq};
    use lumen_vfs::{FileSystem, FlockKind, FlockOp, FlockOutcome};
    let fs = crate::vfs::tmpfs::Tmpfs::new(lumen_vfs::DevId::makedev(0, 381), 64 * 4096);
    let inode = fs.root_inode().create(b"dup", 0o600).map_err(|_| "create dup file")?;
    for lowest in [false, true] {
        let file = KernelFile::new_unchecked_flags(
            Arc::clone(&inode),
            Arc::from(&b"/dup"[..]),
            OpenFlags::from_bits(OpenFlags::O_RDWR),
        )
        .map_err(|_| "open dup file")?;
        let old = install_current_fd(file, false).map_err(|_| "install dup source")?;
        let file = current_file(old).ok_or("dup source")?;
        let mut locked = Err(VfsError::Io);
        inode
            .with_locks_mut(&mut |locks| {
                locked = locks.flock(file.id, FlockOp { kind: Some(FlockKind::Exclusive), nonblock: true });
            })
            .map_err(|_| "lock dup source")?;
        kassert_eq!(locked, Ok(FlockOutcome::Granted { released: false }));
        DUP_LOOKUP_PHASE.store(DUP_ARMED, Ordering::SeqCst);
        let closer = spawn_test_thread(move |_| {
            while DUP_LOOKUP_PHASE.load(Ordering::SeqCst) < DUP_LOOKED_UP {
                crate::time::sleep(1_000_000);
            }
            let ret = close_current_fd(old);
            DUP_LOOKUP_PHASE.store(DUP_SOURCE_CLOSED, Ordering::SeqCst);
            ret
        });
        let closer = match closer {
            Ok(c) => c,
            Err(e) => {
                DUP_LOOKUP_PHASE.store(DUP_IDLE, Ordering::SeqCst);
                return Err(e);
            }
        };
        let duplicated = if lowest { dup_current_fd_from(old, 100, true) } else { dup_current_fd(old, 100, false) };
        // Release the closer even if a regression refused before lookup.
        DUP_LOOKUP_PHASE.fetch_max(DUP_LOOKED_UP, Ordering::SeqCst);
        let closed = closer.join();
        DUP_LOOKUP_PHASE.store(DUP_IDLE, Ordering::SeqCst);
        kassert_eq!(closed?, Ok(()));
        kassert_eq!(duplicated, Ok(100));
        kassert_eq!(file.fd_ref_count(), 1);
        kassert!(current_file(old).is_none());
        let mut flocks = 0;
        inode.with_locks_mut(&mut |l| flocks = l.flock_count()).map_err(|_| "count retained flock")?;
        kassert_eq!(flocks, 1);
        kassert_eq!(close_current_fd(100), Ok(()));
        inode.with_locks_mut(&mut |l| flocks = l.flock_count()).map_err(|_| "count released flock")?;
        kassert_eq!(flocks, 0);
    }
    Ok(())
}

crate::ktest!(
    fn open_reservation_survives_until_install() {
        use crate::{kassert, kassert_eq};
        in_test_process(|_| {
            let path = c"/tmp/b8-open-reservation";
            let r = reserve_current_fd().map_err(|_| "find next open slot")?.fd();
            crate::user::copy_to_user(KTEST_USER_VA, path.to_bytes_with_nul()).map_err(|_| "copy paused open path")?;
            OPEN_INSTALL_PHASE.store(OPEN_ARMED, Ordering::SeqCst);
            let opener = spawn_test_thread(move |_| {
                hazard_call(
                    general::__NR_openat,
                    general::AT_FDCWD as u64,
                    KTEST_USER_VA,
                    u64::from(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                    0o600,
                )
            });
            let opener = match opener {
                Ok(opener) => opener,
                Err(e) => {
                    OPEN_INSTALL_PHASE.store(OPEN_IDLE, Ordering::SeqCst);
                    return Err(e);
                }
            };
            while OPEN_INSTALL_PHASE.load(Ordering::SeqCst) == OPEN_ARMED && !opener.finished() {
                crate::time::sleep(1_000_000);
            }
            let paused = OPEN_INSTALL_PHASE.load(Ordering::SeqCst) == OPEN_PAUSED;
            let exact = dup_current_fd(0, r, false);
            let lowest = dup_current_fd_from(0, 0, false);
            OPEN_INSTALL_PHASE.store(OPEN_RELEASED, Ordering::SeqCst);
            let opened = opener.join();
            OPEN_INSTALL_PHASE.store(OPEN_IDLE, Ordering::SeqCst);
            let unlinked = lumen_vfs::unlink_at(crate::vfs::tables(), None, path.to_bytes(), false);
            kassert!(paused);
            // If `openat` dropped its reservation before opening, this would be `Ok(r)`.
            kassert_eq!(exact, Err(VfsError::Busy));
            kassert_eq!(lowest, Ok(r + 1));
            kassert_eq!(opened?, r as u64);
            kassert_eq!(unlinked, Ok(()));
            kassert_eq!(close_current_fd(r), Ok(()));
            kassert_eq!(close_current_fd(r + 1), Ok(()));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn dup_and_openat_reserve_their_slot() {
        use crate::{kassert, kassert_eq};
        in_test_process(|pid| {
            let slot = reserve_current_fd().map_err(|_| "reserve descriptor")?;
            let fd = slot.fd();
            kassert_eq!(fd, 3);
            let source = current_file(0).ok_or("source descriptor")?;
            let refs = source.fd_ref_count();
            kassert_eq!(dup_current_fd(0, fd, false), Err(VfsError::Busy));
            kassert_eq!(source.fd_ref_count(), refs);
            kassert!(current_file(fd).is_none());
            kassert_eq!(close_current_fd(fd), Err(VfsError::BadF));
            kassert!(!process_open_fds(pid).contains(&fd));
            kassert_eq!(
                hazard_call(general::__NR_fstat, fd as u64, KTEST_USER_VA, 0, 0) as i64,
                -(super::errno::EBADF as i64)
            );
            kassert_eq!(dup_current_fd_from(0, 0, true), Ok(4));
            let null = open_kernel_file(b"/dev/null", OpenFlags::O_RDONLY, 0).map_err(|_| "open reserved file")?;
            kassert_eq!(slot.install(null, true), fd);
            kassert_eq!(current_cloexec(fd), Ok(true));
            kassert_eq!(dup_current_fd(0, fd, false), Ok(fd));
            kassert_eq!(source.fd_ref_count(), refs + 2);
            kassert_eq!(current_cloexec(fd), Ok(false));
            kassert_eq!(dup_current_fd(fd, fd, true), Ok(fd));
            kassert_eq!(current_cloexec(fd), Ok(false));
            // Failed path lookups cancel their reservation, leaving the
            // same lowest slot free.
            let path = b"/tmp/b8-must-not-exist\0";
            crate::user::copy_to_user(KTEST_USER_VA, path).map_err(|_| "copy open path")?;
            for _ in 0..2 {
                kassert_eq!(
                    hazard_call(general::__NR_openat, general::AT_FDCWD as u64, KTEST_USER_VA, 0, 0) as i64,
                    -(super::errno::ENOENT as i64)
                );
                let free = reserve_current_fd().map_err(|_| "reserve after failed open")?;
                kassert_eq!(free.fd(), 5);
            }
            // Fill with reservations, not opens: open_count alone cannot
            // detect that no slot is free.
            let mut held = Vec::new();
            for _ in 5..lumen_vfs::MAX_FDS {
                held.push(reserve_current_fd().map_err(|_| "fill reservations")?);
            }
            kassert_eq!(
                hazard_call(
                    general::__NR_openat,
                    general::AT_FDCWD as u64,
                    KTEST_USER_VA,
                    u64::from(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                    0o600
                ) as i64,
                -(super::errno::EMFILE as i64)
            );
            kassert!(matches!(crate::vfs::resolve_path_from_root("/tmp/b8-must-not-exist"), Err(VfsError::NoEnt)));
            drop(held);
            kassert_eq!(reserve_current_fd().map_err(|_| "cancelled slots reusable")?.fd(), 5);
            duplicate_racing_close_keeps_flock()
        })?
    }
);

crate::ktest!(
    fn fork_refusal_releases_unlinked_resources() {
        use crate::{kassert, kassert_eq};
        in_test_process(|_| {
            let parent = current_process();
            let mm = parent.address_space().ok_or("parent mm")?;
            let file = current_file(0).ok_or("parent stdin")?;
            let before = file.fd_ref_count();
            // A private two-pid table forces the same fork refusal as a
            // full production table, without exhausting the whole kernel.
            let root = NewProcess::empty(ConsoleState::new(ConsoleIo::Capture, Vec::new()), false, None);
            let leader = Arc::new(UserThread::unbound_for_ktest(Arc::clone(&root.process)));
            let mut table = ProcTable::new(2);
            kassert_eq!(table.link_root(&leader), Some(2));
            let files = parent.files.lock().clone().ok_or("parent files")?;
            for flags in [general::CLONE_VM, general::CLONE_VM | general::CLONE_FILES] {
                for _ in 0..8 {
                    let child = Process::copy_for_fork(&parent, flags).map_err(|_| "copy child")?;
                    let held = Arc::clone(&child.process);
                    let thread = Arc::new(UserThread::unbound_for_ktest(Arc::clone(&held)));
                    let shared = flags & general::CLONE_FILES != 0;
                    kassert_eq!(mm.users(), 2);
                    kassert_eq!(files.users(), if shared { 2 } else { 1 });
                    kassert_eq!(file.fd_ref_count(), before + u32::from(!shared));
                    kassert_eq!(
                        table.link_child(&leader, &thread, clone_flags(flags | general::SIGCHLD, 0).unwrap()),
                        Err(errno::EAGAIN)
                    );
                    drop(child);
                    kassert!(held.released());
                    kassert_eq!(mm.users(), 1);
                    kassert_eq!(files.users(), 1);
                    kassert_eq!(file.fd_ref_count(), before);
                }
            }
            drop(table);
            kassert_eq!(
                fork_current(ProcessClone::FORK, 0, 0, 0, 0, &crate::arch::syscall::SyscallFrame::default()),
                Err(errno::ENOSYS)
            );
            with_current_console(|console| console.io = ConsoleIo::Serial);
            // Also refuse after a real scheduler thread was made: the
            // pending-signal recheck must cancel and join its start gate.
            let kernel = sched::current().ok_or("current thread")?;
            kernel.set_sigpending();
            let name = kernel.name();
            let named_before = sched::threads().iter().filter(|t| t.name == name).count();
            let result = fork_current(
                clone_flags(general::SIGCHLD | general::CLONE_VM, 0).expect("valid process flags"),
                0,
                0,
                0,
                0,
                &crate::arch::syscall::SyscallFrame::default(),
            );
            sched::clear_current_sigpending();
            kassert_eq!(result, Err(lumen_signal::restart::RestartCode::NoIntr as u32));
            kassert_eq!(mm.users(), 1);
            kassert_eq!(file.fd_ref_count(), before);
            kassert_eq!(sched::threads().iter().filter(|t| t.name == name).count(), named_before);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn fork_files_count_slots_and_skip_reservations() {
        use crate::{kassert, kassert_eq};
        in_test_process(|_| {
            let file = current_file(0).ok_or("stdin")?;
            kassert_eq!(dup_current_fd(0, 9, true), Ok(9));
            let reserved = reserve_current_fd().map_err(|_| "reserve")?;
            let fd = reserved.fd();
            let files = current_process().files.lock().clone().ok_or("files")?;
            let copy = Arc::new(files.fork_copy(0));
            kassert_eq!(file.fd_ref_count(), 4);
            kassert_eq!(copy.users(), 1);
            kassert_eq!(copy.with_table(|t| t.get_cloexec(9)), Ok(true));
            kassert!(copy.with_table(|t| t.get(fd).is_none()));
            kassert_eq!(copy.with_table(|t| t.reserved_count()), 0);
            copy.leave(0);
            kassert_eq!(file.fd_ref_count(), 2);
            let sharer = files.share();
            kassert_eq!(files.users(), 2);
            kassert_eq!(file.fd_ref_count(), 2);
            sharer.leave(0);
            kassert_eq!(files.users(), 1);
            kassert_eq!(file.fd_ref_count(), 2);
            drop(reserved);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn files_unshare_waits_for_open_reservations() {
        use crate::{kassert, kassert_eq};
        in_test_process(|_| {
            let process = current_process();
            let original = process.files.lock().clone().ok_or("files")?;
            let shared = original.share();
            let reservation = reserve_current_fd().map_err(|_| "reserve")?;
            let fd = reservation.fd();
            let swapper = spawn_test_thread(|_| unshare_current_files().expect("unshare"))?;
            let blocked = swapper.settle();
            let opened = open_kernel_file(b"/dev/null", OpenFlags::O_RDONLY, 0).map_err(|_| "open")?;
            let installed = reservation.install(opened, true);
            swapper.join()?;
            kassert!(blocked);
            kassert_eq!(installed, fd);
            let new = process.files.lock().clone().ok_or("new files")?;
            kassert!(!Arc::ptr_eq(&shared, &new));
            kassert_eq!(current_cloexec(fd), Ok(true));
            let file = current_file(fd).ok_or("open's returned fd missing after swap")?;
            kassert_eq!(file.fd_ref_count(), 2);
            shared.leave(0);
            kassert_eq!(file.fd_ref_count(), 1);
            kassert_eq!(close_current_fd(fd), Ok(()));
            kassert_eq!(file.fd_ref_count(), 0);
            // A cancelled reservation releases the exclusive waiter too.
            let shared = new.share();
            let cancelled = reserve_current_fd().map_err(|_| "reserve cancelled")?;
            let swapper = spawn_test_thread(|_| unshare_current_files().expect("unshare"))?;
            let blocked = swapper.settle();
            drop(cancelled);
            swapper.join()?;
            shared.leave(0);
            kassert!(blocked);
            kassert_eq!(reserve_current_fd().map_err(|_| "cancelled slot free")?.fd(), fd);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn replace_mm_keeps_process_and_thread_in_agreement() {
        use crate::{kassert, kassert_eq};
        in_test_process(|pid| {
            let process = current_process();
            let old = process.address_space().ok_or("old mm")?;
            let new = Arc::new(AddressSpace::new().map_err(|_| "new mm")?);
            if new
                .map_eager_with_bytes(KTEST_USER_VA, 4096, lumen_vm::Prot::READ | lumen_vm::Prot::WRITE, &[0x5a])
                .is_err()
            {
                new.drop_user();
                return Err("map new mm");
            }
            replace_current_mm(Arc::clone(&new));
            kassert_eq!(old.users(), 0);
            kassert!(Arc::ptr_eq(&new, &sched::current_address_space().ok_or("scheduler mm")?));
            kassert!(Arc::ptr_eq(&new, &process_address_space(pid).ok_or("process mm")?));
            let mut byte = [0];
            crate::user::copy_from_user(&mut byte, KTEST_USER_VA).map_err(|_| "read new mapping")?;
            kassert_eq!(byte, [0x5a]);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn wait_child_interrupts_and_wakes_on_exit() {
        use crate::{kassert, kassert_eq};
        fn retire(thread: Arc<UserThread>) {
            let unlinked = PROCS.lock().unlink_thread(&thread);
            drop(unlinked);
            thread.process().release_resources();
            let gone = PROCS.lock().exit_in_tree(thread.process().pid(), ExitStatus::exited(37)).expect("exit");
            drop(gone);
        }
        in_test_process(|_| {
            // A linked child held in kernel context lets us control exactly
            // when its exit follows the parent's blocking wait.
            let caller = current_thread();
            let new = Process::copy_for_fork(caller.process(), general::CLONE_VM).map_err(|_| "copy child")?;
            let child = Arc::new(UserThread::unbound_for_ktest(Arc::clone(&new.process)));
            let pid = PROCS
                .lock()
                .link_child(&caller, &child, clone_flags(general::CLONE_VM | general::SIGCHLD, 0).unwrap())
                .map_err(|_| "link child")?;
            new.linked();
            let request = wait4_request(pid as i32, 0).expect("valid wait");
            let kernel = sched::current().expect("current thread");
            kernel.set_sigpending();
            let interrupted = wait_child(request);
            sched::clear_current_sigpending();
            let finished = Arc::new(core::sync::atomic::AtomicBool::new(false));
            let observed = Arc::new(core::sync::atomic::AtomicBool::new(false));
            let (end, saw, exiting) = (Arc::clone(&finished), Arc::clone(&observed), Arc::clone(&child));
            // How many waiters the exit's own wake left on the parent's
            // queue: the worker's second wake_all frees a parent the exit
            // forgot (a clean `not ok`, not a hang). The parent can't queue
            // on child_events again: its next wait reaps, the last ECHILDs.
            let rescued = Arc::new(core::sync::atomic::AtomicUsize::new(0));
            let (parent, left) = (Arc::clone(caller.process()), Arc::clone(&rescued));
            let worker = match sched::spawn("wait-exit", move || {
                while kernel.state() != sched::State::Blocked && !end.load(Ordering::SeqCst) {
                    sched::yield_now();
                }
                saw.store(kernel.state() == sched::State::Blocked, Ordering::SeqCst);
                retire(exiting);
                left.store(parent.child_events.wake_all(), Ordering::SeqCst);
            }) {
                Ok(worker) => worker,
                Err(_) => {
                    retire(child);
                    let _ = wait_child(request);
                    return Err("spawn exit worker");
                }
            };
            let report = wait_child(request);
            finished.store(true, Ordering::SeqCst);
            worker.join();
            // Always reap, including a broken early-return path, before
            // assertions can end the fixture.
            let remaining = wait_child(request);
            kassert!(matches!(interrupted, Err(crate::linux::signal::ERESTARTSYS)));
            kassert!(observed.load(Ordering::SeqCst));
            kassert_eq!(rescued.load(Ordering::SeqCst), 0);
            let report = report.map_err(|_| "blocking wait")?.ok_or("blocking wait returned no event")?;
            kassert_eq!(report.pid(), pid);
            kassert_eq!(report.status().word(), 37 << 8);
            kassert!(matches!(remaining, Err(errno::ECHILD)));
            Ok(())
        })?
    }
);

#[cfg(feature = "ktest")]
#[derive(Clone, Copy)]
enum LockCleanup {
    Exit,
    Close,
    Dup,
    Replace,
}

#[cfg(feature = "ktest")]
fn shared_files_lock_cleanup(cleanup: LockCleanup) -> crate::ktest::TestResult {
    use crate::{kassert, kassert_eq};
    fn lock(fd: i32, cmd: u32) -> Result<(u64, i16, i32), &'static str> {
        let mut raw = [0u8; 32];
        raw[0..2].copy_from_slice(&(general::F_WRLCK as i16).to_ne_bytes());
        raw[16..24].copy_from_slice(&10i64.to_ne_bytes());
        crate::user::copy_to_user(KTEST_USER_VA, &raw).map_err(|_| "write flock")?;
        let result = hazard_call(general::__NR_fcntl, fd as u64, u64::from(cmd), KTEST_USER_VA, 0);
        crate::user::copy_from_user(&mut raw, KTEST_USER_VA).map_err(|_| "read flock")?;
        Ok((
            result,
            i16::from_ne_bytes(raw[0..2].try_into().unwrap()),
            i32::from_ne_bytes(raw[24..28].try_into().unwrap()),
        ))
    }
    in_test_process(move |_| {
        let file = open_kernel_file(b"/tmp/c1-exit-lock", OpenFlags::O_CREAT | OpenFlags::O_RDWR, 0o600)
            .map_err(|_| "open")?;
        let fd = install_current_fd(file, false).map_err(|_| "install")?;
        kassert_eq!(fd, 3);
        let probe = dup_current_fd_from(fd, 0, false).map_err(|_| "probe descriptor")?;
        // Allocate the replacement before the child runs, so failures cannot
        // strand a child behind its exit handshake.
        let replacement = open_kernel_file(b"/dev/null", OpenFlags::O_RDWR, 0).map_err(|_| "replacement")?;
        let caller = current_thread();
        let files = caller.process().files.lock().clone().ok_or("files")?;
        let new = Process::copy_for_fork(caller.process(), general::CLONE_VM | general::CLONE_FILES)
            .map_err(|_| "copy child")?;
        let child = Arc::new(UserThread::unbound_for_ktest(Arc::clone(&new.process)));
        let locked = Arc::new(VforkDone::new());
        let finish = Arc::new(VforkDone::new());
        let result = Arc::new(SpinLock::new(None));
        let (ready, end, out) = (Arc::clone(&locked), Arc::clone(&finish), Arc::clone(&result));
        let parked =
            ParkedChild::spawn("lock-child", new.process.address_space().unwrap(), Arc::clone(&child), 0, move || {
                *out.lock() = Some(lock(fd, general::F_SETLK));
                ready.complete();
                end.wait();
                record_exit(UserExit::Exited(0));
                let ended = take_current();
                assert!(ended.exit.is_some());
            })
            .map_err(|_| "spawn child")?;
        let pid = PROCS
            .lock()
            .link_child(&caller, &child, clone_flags(general::CLONE_VM | general::SIGCHLD, 0).unwrap())
            .map_err(|_| "link child")?;
        new.linked();
        let shared_users = files.users();
        let handle = parked.start();
        locked.wait();
        let before = lock(probe, general::F_GETLK);
        let released = match cleanup {
            LockCleanup::Exit => Ok(()),
            LockCleanup::Close => close_current_fd(fd),
            LockCleanup::Dup => dup_current_fd(0, fd, false).map(|_| ()),
            LockCleanup::Replace => insert_current_fd(fd, replacement),
        };
        // Capture the result while the child is alive, before its exit sweep
        // could hide a close that only released the parent's locks.
        let after_close = lock(probe, general::F_GETLK);
        let alive = !handle.is_finished();
        finish.complete();
        handle.join();
        let request = wait4_request(pid as i32, 0).map_err(|_| "wait request")?;
        kassert!(wait_child(request).map_err(|_| "reap")?.is_some());
        kassert_eq!(shared_users, 2);
        kassert_eq!(result.lock().take().ok_or("child result")??.0, 0);
        kassert_eq!(before?, (0, general::F_WRLCK as i16, pid as i32));
        kassert!(alive);
        kassert_eq!(released, Ok(()));
        kassert_eq!(files.users(), 1);
        if !matches!(cleanup, LockCleanup::Exit) {
            kassert_eq!(after_close?.1, general::F_UNLCK as i16);
        } else {
            kassert!(current_file(fd).is_some());
            kassert_eq!(after_close?.1, general::F_WRLCK as i16);
        }
        let after_exit = lock(probe, general::F_GETLK)?;
        kassert_eq!(after_exit.0, 0);
        kassert_eq!(after_exit.1, general::F_UNLCK as i16);
        if current_file(fd).is_some() {
            close_current_fd(fd).map_err(|_| "close")?;
        }
        close_current_fd(probe).map_err(|_| "close probe")?;
        Ok(())
    })?
}

crate::ktest!(
    fn shared_files_exit_releases_its_posix_locks() {
        shared_files_lock_cleanup(LockCleanup::Exit)
    }
);

crate::ktest!(
    fn shared_files_close_releases_all_members_posix_locks() {
        for cleanup in [LockCleanup::Close, LockCleanup::Dup, LockCleanup::Replace] {
            shared_files_lock_cleanup(cleanup)?;
        }
        Ok(())
    }
);

crate::ktest!(
    fn shared_files_leave_waits_for_inflight_close() {
        use crate::{kassert, kassert_eq};
        use lumen_vfs::locks::fcntl_const;
        use lumen_vfs::{FlockSpec, LockContext, RecordLockOutcome};
        in_test_process(|pid| {
            // A stand-in process member exercises the exact sweep/leave path
            // without needing a second user program or a production pause hook.
            const MEMBER: u32 = 0x7fff_ffff;
            let opened = open_kernel_file(b"/tmp/c1-close-leave", OpenFlags::O_CREAT | OpenFlags::O_RDWR, 0o600)
                .map_err(|_| "open")?;
            let fd = install_current_fd(opened, false).map_err(|_| "install")?;
            let file = current_file(fd).ok_or("file")?;
            let files = current_process().files.lock().clone().ok_or("files")?;
            let shared = files.share();
            shared.enrol(MEMBER);
            let spec = FlockSpec {
                l_type: fcntl_const::F_WRLCK,
                l_whence: fcntl_const::SEEK_SET,
                l_start: 0,
                l_len: 10,
                l_pid: 0,
            };
            let ctx = LockContext { cur_offset: 0, file_size: 0, readable: true, writable: true };
            let mut closer = None;
            let mut leaver = None;
            let mut removed = false;
            let mut close_blocked = false;
            let mut leave_blocked = false;
            let mut members_while_blocked = 0;
            let mut outcome = Ok(());
            let held = file.inode().with_locks_mut(&mut |locks| {
                outcome = (|| {
                    kassert_eq!(
                        locks.setlk(MEMBER, spec, ctx, false),
                        Ok(RecordLockOutcome::Granted { released: false })
                    );
                    let mut before = spec;
                    locks.getlk(pid, &mut before, 0, 0).map_err(|_| "initial GETLK")?;
                    kassert_eq!(before.l_pid, MEMBER as i32);
                    closer = Some(spawn_test_thread(move |_| close_current_fd(fd))?);
                    close_blocked = closer.as_ref().unwrap().settle();
                    removed = current_file(fd).is_none();
                    if !removed {
                        return Ok(());
                    }
                    // The inode's sleeping lock pins the close after removal.
                    // The exit sweep must miss this inode (no fd remains), then
                    // leave must wait until the closer has scanned its member.
                    let leaving = Arc::clone(&shared);
                    leaver = Some(spawn_test_thread(move |_| {
                        leaving.release_posix_for_pid(MEMBER);
                        leaving.leave(MEMBER);
                    })?);
                    leave_blocked = leaver.as_ref().unwrap().settle();
                    members_while_blocked = files.users();
                    Ok(())
                })();
            });
            // Release the inode lock and join before any failing assertion.
            let closed = closer.map(|thread| thread.join()).transpose();
            if let Some(thread) = leaver {
                thread.join()?;
            } else {
                shared.release_posix_for_pid(MEMBER);
                Arc::clone(&shared).leave(MEMBER);
            }
            let mut after = spec;
            let mut queried = Err(VfsError::Io);
            file.inode()
                .with_locks_mut(&mut |locks| {
                    queried = locks.getlk(pid, &mut after, 0, 0);
                    // Clean up a broken leave/close implementation too, after
                    // capturing its result so it cannot hide the regression.
                    let _ = locks.release_posix_for_pid(MEMBER); // no locker waits on this fixture inode
                })
                .map_err(|_| "final inode lock")?;
            held.map_err(|_| "held inode lock")?;
            outcome?;
            kassert_eq!(closed?, Some(Ok(())));
            kassert!(close_blocked && removed);
            kassert!(leave_blocked);
            kassert_eq!(members_while_blocked, 2);
            kassert_eq!(files.users(), 1);
            kassert_eq!(queried, Ok(()));
            kassert_eq!(after.l_type, fcntl_const::F_UNLCK);
            Ok(())
        })?
    }
);

/// Link a real child behind its start gate, sharing the fixture's VM.
#[cfg(feature = "ktest")]
fn ktest_fork_child(run: impl FnOnce() + Send + 'static) -> Result<(u32, ParkedChild), &'static str> {
    let caller = current_thread();
    let request = clone_flags(general::CLONE_VM | general::SIGCHLD, 0).unwrap();
    let new = Process::copy_for_fork(caller.process(), request.bits()).map_err(|_| "copy child")?;
    let child = Arc::new(UserThread::new(Arc::clone(&new.process)));
    let parked = ParkedChild::spawn("signal-child", new.process.address_space().unwrap(), Arc::clone(&child), 0, run)
        .map_err(|_| "spawn child")?;
    let pid = PROCS.lock().link_child(&caller, &child, request).map_err(|_| "link child")?;
    new.linked();
    Ok((pid, parked))
}

#[cfg(feature = "ktest")]
fn ktest_reap(pid: u32, handle: sched::JoinHandle) -> Result<i32, &'static str> {
    handle.join();
    let report =
        wait_child(wait4_request(pid as i32, 0).map_err(|_| "request")?).map_err(|_| "wait")?.ok_or("no child exit")?;
    Ok(report.status().word())
}

/// Run a child that inherits the current fixture's session and controlling
/// terminal. Its exit and reap complete before returning the closure's result.
#[cfg(feature = "ktest")]
pub(crate) fn in_test_child(
    f: impl FnOnce(u32) -> crate::ktest::TestResult + Send + 'static,
) -> crate::ktest::TestResult {
    let result = Arc::new(SpinLock::new(None));
    let out = Arc::clone(&result);
    let (pid, parked) = ktest_fork_child(move || {
        let value = f(current_process().pid());
        record_thread_exit(UserExit::Exited(0));
        let _ = take_current();
        *out.lock() = Some(value);
    })?;
    let status = ktest_reap(pid, parked.start())?;
    if status != 0 {
        return Err("test child exit status");
    }
    result.lock().take().ok_or("test child result")?
}

crate::ktest!(
    fn fork_inherits_affinity_and_nice() {
        use crate::{kassert, kassert_eq};
        let space = Arc::new(AddressSpace::new().map_err(|_| "parent space")?);
        let stdio = Stdio::open().map_err(|_| "parent stdio")?;
        let result = Arc::new(SpinLock::new(None));
        let out = result.clone();
        // A separate migratable parent: never change the ktest runner's pin.
        let parent = sched::spawn_test_migratable("fork-affinity", move || {
            let _space = sched::UserSpaceGuard::enter(space);
            let _registered = RegisteredProcess::register(stdio);
            *out.lock() = Some((|| {
                let here = crate::arch::cpu_index();
                let there = sched::running_cpus().into_iter().find(|&cpu| cpu != here).ok_or("second CPU")?;
                let mask = (1 << here) | (1 << there);
                let me = sched::current().expect("parent");
                kassert_eq!(me.set_affinity(mask), Ok(()));
                me.set_nice(5);
                let seen = Arc::new(SpinLock::new(None));
                let child_out = seen.clone();
                let (pid, parked) = ktest_fork_child(move || {
                    let child = sched::current().expect("child");
                    let inherited = (child.affinity(), child.nice());
                    let changed = child.set_affinity(1 << there);
                    sched::yield_now();
                    *child_out.lock() = Some((inherited, changed, crate::arch::cpu_index()));
                    record_exit(UserExit::Exited(0));
                    let _ = take_current();
                })?;
                kassert_eq!(ktest_reap(pid, parked.start())?, 0);
                let (inherited, changed, cpu) = seen.lock().take().ok_or("child result")?;
                kassert_eq!(inherited, (mask, 5));
                kassert_eq!(changed, Ok(()));
                kassert_eq!(cpu, there);
                kassert!(mask & (1 << cpu) != 0);
                Ok(())
            })());
        })?;
        sched::join_bounded(parent)?;
        result.lock().take().ok_or("parent result")?
    }
);

crate::ktest!(
    fn fork_child_fault_is_delivered_and_reaped() {
        use crate::{
            arch::{
                gdt,
                syscall::{self, SyscallFrame},
            },
            kassert_eq,
        };
        in_test_process(|_| {
            const CODE: u64 = 0x800000;
            let space = current_process().address_space().ok_or("child space")?;
            space
                .map_eager_with_bytes(CODE, 4096, lumen_vm::Prot::READ | lumen_vm::Prot::EXEC, &[0x0f, 0x0b])
                .map_err(|_| "map UD2")?;
            let result = Arc::new(SpinLock::new(None));
            let out = Arc::clone(&result);
            let (pid, parked) = ktest_fork_child(move || {
                let frame = SyscallFrame {
                    rip: CODE,
                    rsp: KTEST_USER_VA + KTEST_USER_LEN,
                    cs: u64::from(gdt::USER_CODE),
                    ss: u64::from(gdt::USER_DATA),
                    rflags: 0x202,
                    ..Default::default()
                };
                // SAFETY: the child owns the active address space; entry and stack
                // are mapped, interrupts are enabled and no lock is held.
                unsafe {
                    syscall::enter_fork(frame);
                }
                *out.lock() = take_current().exit;
            })?;
            kassert_eq!(ktest_reap(pid, parked.start())?, Signal::ILL.number() as i32);
            kassert_eq!(*result.lock(), Some(UserExit::Fault { vector: 6, error_code: 0, rip: CODE, cr2: 0 }));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn kill_wakes_fork_child_and_wins_over_exit() {
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::AtomicBool;
        use lumen_signal::pending::{SenderClass, SigInfo};
        in_test_process(|_| {
            let queue = Arc::new(sched::WaitQueue::new());
            let rescue = Arc::new(AtomicBool::new(false));
            let result = Arc::new(SpinLock::new(None));
            let (q, stop, out) = (Arc::clone(&queue), Arc::clone(&rescue), Arc::clone(&result));
            let (pid, parked) = ktest_fork_child(move || {
                let wake = q.wait_killable(None, || stop.load(Ordering::SeqCst));
                *out.lock() = Some(wake);
                // A pending fatal signal must override an ordinary exit racing it.
                record_exit(UserExit::Exited(42));
                let _ = take_current();
            })?;
            let handle = parked.start();
            while handle.thread().state() != sched::State::Blocked && !handle.is_finished() {
                crate::time::sleep(1_000_000);
            }
            // Both threads run on this CPU. Observe kick before the child runs;
            // rescue a broken notifier so assertions never strand the child.
            let was_on = crate::arch::irq::disable();
            let sent = send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::KILL, 0))), SenderClass::SiUser);
            let woke = handle.thread().state() == sched::State::Ready;
            if !woke {
                rescue.store(true, Ordering::SeqCst);
                queue.wake_all();
            }
            crate::arch::irq::restore(was_on);
            let status = ktest_reap(pid, handle)?;
            kassert_eq!(sent, Ok(()));
            kassert!(woke);
            kassert_eq!(*result.lock(), Some(sched::Wake::Interrupted));
            kassert_eq!(status, Signal::KILL.number() as i32);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn recorded_exit_wins_over_later_signals() {
        use crate::{kassert, kassert_eq};
        use lumen_signal::pending::{SenderClass, SigInfo};
        in_test_process(|_| {
            let recorded = Arc::new(VforkDone::new());
            let finish = Arc::new(VforkDone::new());
            let (ready, end) = (Arc::clone(&recorded), Arc::clone(&finish));
            let (pid, parked) = ktest_fork_child(move || {
                record_exit(UserExit::Exited(0));
                ready.complete();
                end.wait();
                let _ = take_current();
            })?;
            let handle = parked.start();
            recorded.wait();
            let killed = send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::KILL, 0))), SenderClass::SiUser);
            let rt = SigInfo::new(Signal::new(35).unwrap(), -1);
            let queued = (0..1025).all(|_| send_process_checked(pid as i32, Ok(Some(rt)), SenderClass::Other).is_ok());
            finish.complete();
            while !handle.is_finished() {
                crate::time::sleep(1_000_000);
            }
            // A zombie remains a valid process target, but queues no signal.
            let zombie = (0..1025).all(|_| send_process_checked(pid as i32, Ok(Some(rt)), SenderClass::Other).is_ok());
            let status = ktest_reap(pid, handle)?;
            kassert_eq!(killed, Ok(()));
            kassert!(queued && zombie);
            kassert_eq!(status, 0);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn fork_pending_signal_precedes_first_user_instruction() {
        use crate::{
            arch::{
                gdt,
                syscall::{self, SyscallFrame},
            },
            kassert_eq,
        };
        use lumen_signal::pending::{SenderClass, SigInfo};
        in_test_process(|_| {
            const CODE: u64 = 0x800000;
            // Entry exits 42; the handler exits 0. No instruction at entry
            // may run before the already-pending handler gets control.
            let mut code = [0x90; 32];
            code[..12].copy_from_slice(&[0xbf, 42, 0, 0, 0, 0xb8, 60, 0, 0, 0, 0x0f, 0x05]);
            code[16..28].copy_from_slice(&[0xbf, 0, 0, 0, 0, 0xb8, 60, 0, 0, 0, 0x0f, 0x05]);
            current_process()
                .address_space()
                .unwrap()
                .map_eager_with_bytes(CODE, 4096, lumen_vm::Prot::READ | lumen_vm::Prot::EXEC, &code)
                .map_err(|_| "map program")?;
            set_current_action(
                Signal::USR1,
                KernelSigaction {
                    handler: CODE + 16,
                    restorer: CODE,
                    flags: u64::from(general::SA_RESTORER),
                    ..Default::default()
                },
            );
            let (pid, parked) = ktest_fork_child(move || {
                let frame = SyscallFrame {
                    rip: CODE,
                    rsp: KTEST_USER_VA + KTEST_USER_LEN,
                    cs: u64::from(gdt::USER_CODE),
                    ss: u64::from(gdt::USER_DATA),
                    rflags: 0x202,
                    ..Default::default()
                };
                // SAFETY: the child's shared fixture VM maps code and stack;
                // its identity is bound and no lock is held with interrupts on.
                unsafe {
                    syscall::enter_fork(frame);
                }
                let _ = take_current();
            })?;
            let sent = send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::USR1, 0))), SenderClass::SiUser);
            let status = ktest_reap(pid, parked.start())?;
            kassert_eq!(sent, Ok(()));
            kassert_eq!(status, 0);
            Ok(())
        })?
    }
);

/// The live processes counted by sysinfo, including init but excluding zombies.
pub fn live_process_count() -> usize {
    PROCS.lock().live_process_count()
}

/// Snapshot a process's live and exited threads under the table's lifetime lock.
pub fn process_time(pid: u32) -> [u64; 2] {
    PROCS.lock().process_time(pid)
}

/// Read the highest priority of the selected threads, optionally setting each one's nice.
pub fn priority(which: i32, who: i32, nice: Option<i32>) -> Result<u64, u32> {
    let caller = current_thread();
    PROCS.lock().priority(&caller, which, who, nice)
}

crate::ktest!(
    fn serial_read_interrupts_and_restarts_after_sigreturn() {
        use crate::arch::syscall::SyscallFrame;
        use crate::linux::{signal as delivery, syscall};
        use crate::{kassert, kassert_eq};
        use lumen_signal::pending::{SenderClass, SigInfo};
        use lumen_signal::restart::{RestartCode, SyscallExit};
        in_test_process(|_| {
            struct Reset;
            impl Drop for Reset {
                fn drop(&mut self) {
                    SERIAL_READ_PHASE.store(0, Ordering::SeqCst);
                }
            }
            let _reset = Reset;
            with_current_console(|c| c.io = ConsoleIo::Serial);
            kassert!(!stdin_ready(ConsoleIo::Serial));
            for restart in [false, true] {
                set_current_action(
                    Signal::INT,
                    KernelSigaction {
                        handler: 0x400100,
                        restorer: 0x400200,
                        flags: u64::from(general::SA_RESTORER | if restart { general::SA_RESTART } else { 0 }),
                        ..Default::default()
                    },
                );
                let tid = Arc::new(AtomicU32::new(0));
                let child_tid = Arc::clone(&tid);
                SERIAL_READ_PHASE.store(1, Ordering::SeqCst);
                let worker = spawn_test_thread(move |tid| {
                    child_tid.store(tid, Ordering::SeqCst);
                    let mut frame = SyscallFrame {
                        rax: u64::from(general::__NR_read),
                        rdi: 0,
                        rsi: KTEST_USER_VA,
                        rdx: 1,
                        rip: 0x400002,
                        rsp: KTEST_USER_VA + KTEST_USER_LEN,
                        cs: 0x23,
                        ss: 0x1b,
                        rflags: 0x202,
                        ..Default::default()
                    };
                    syscall::dispatch(&mut frame);
                    kassert_eq!(frame.rax, delivery::restart_result(RestartCode::Sys));
                    kassert!(delivery::exit_to_user(&mut frame, SyscallExit::new(u64::from(general::__NR_read))));
                    kassert_eq!(frame.rip, 0x400100);
                    frame.rsp += 8;
                    frame.r11 = 0x202;
                    delivery::frame::restore(&mut frame).map_err(|_| "serial read sigreturn")?;
                    if restart {
                        kassert_eq!(frame.rax, u64::from(general::__NR_read));
                        kassert_eq!(frame.rip, 0x400000);
                        // Execute the rewound read again. It finds no input;
                        // only after that is observed does the parent inject z.
                        frame.rip += 2;
                        syscall::dispatch(&mut frame);
                        kassert_eq!(frame.rax, 1);
                        let mut byte = [0];
                        crate::user::copy_from_user(&mut byte, KTEST_USER_VA).map_err(|_| "serial read byte")?;
                        kassert_eq!(byte, [b'z']);
                    } else {
                        kassert_eq!(frame.rax, (-(errno::EINTR as i64)) as u64);
                        kassert_eq!(frame.rip, 0x400002);
                    }
                    kassert!(!delivery::exit_to_user(&mut frame, None));
                    Ok(())
                })?;
                while SERIAL_READ_PHASE.load(Ordering::SeqCst) != 2 && !worker.finished() {
                    crate::time::sleep(1_000_000);
                }
                let empty = !serial_in::available();
                let reached = SERIAL_READ_PHASE.load(Ordering::SeqCst) == 2;
                let sent = send_thread(
                    None,
                    tid.load(Ordering::SeqCst),
                    Some(SigInfo::new(Signal::INT, 0)),
                    SenderClass::SiUser,
                );
                SERIAL_READ_PHASE.store(3, Ordering::SeqCst);
                while SERIAL_READ_PHASE.load(Ordering::SeqCst) != 4 && !worker.finished() {
                    crate::time::sleep(1_000_000);
                }
                let second_empty = SERIAL_READ_PHASE.load(Ordering::SeqCst) == 4;
                if second_empty {
                    // Also rescues an uninterruptible first read: that mutant
                    // returns a byte and fails the ERESTARTSYS assertion.
                    serial_in::inject_for_test(b'z');
                }
                let result = worker.join()?;
                SERIAL_READ_PHASE.store(0, Ordering::SeqCst);
                let leftover = serial_in::read_byte();
                result?;
                kassert!(reached && empty);
                kassert_eq!(sent, Ok(()));
                kassert_eq!(second_empty, restart);
                kassert_eq!(leftover, None);
                kassert_eq!(serial_in::poll_subscribers(), 0);
            }
            Ok(())
        })?
    }
);

#[cfg(feature = "ktest")]
fn ktest_sibling(run: impl FnOnce() + Send + 'static) -> Result<(Arc<UserThread>, ParkedChild), &'static str> {
    let caller = current_thread();
    let thread = Arc::new(UserThread::new(Arc::clone(caller.process())));
    let parked = ParkedChild::spawn(
        "c3-sibling",
        caller.process().address_space().unwrap(),
        Arc::clone(&thread),
        0,
        move || {
            run();
            let _ = take_current();
        },
    )
    .map_err(|_| "spawn sibling")?;
    let mut table = PROCS.lock();
    let mut state = caller.process().signal.lock();
    let init = state.thread_init(caller.tid(), clone_flags(0x10f00, 0).unwrap());
    let signals = state.prepare_thread(&thread, init).map_err(|_| "sibling signals")?;
    table.link_thread(&thread, signals, &mut state).map_err(|_| "link sibling")?;
    Ok((thread, parked))
}

#[cfg(feature = "ktest")]
fn transition_delay(tid: u32) -> Arc<TransitionDelay> {
    Arc::new(TransitionDelay { tid, reached: VforkDone::new(), release: VforkDone::new() })
}

#[cfg(feature = "ktest")]
fn ktest_exec() -> u64 {
    crate::user::copy_to_user(KTEST_USER_VA, b"/tmp/c3-exec\0").expect("exec path");
    hazard_call(general::__NR_execve, KTEST_USER_VA, 0, 0, 0)
}

crate::ktest!(
    fn clone_and_join_reuses_signal_slots() {
        use crate::arch::gdt;
        use crate::{kassert, kassert_eq};
        in_test_process(|_| {
            const CODE: u64 = 0x800000;
            let process = current_process();
            process
                .address_space()
                .unwrap()
                .map_eager_with_bytes(
                    CODE,
                    4096,
                    lumen_vm::Prot::READ | lumen_vm::Prot::EXEC,
                    // mov eax, __NR_exit; xor edi, edi; syscall; ud2
                    &[0xb8, 60, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05, 0x0f, 0x0b],
                )
                .map_err(|_| "map thread exit program")?;
            let frame = crate::arch::syscall::SyscallFrame {
                rip: CODE,
                rsp: KTEST_USER_VA + KTEST_USER_LEN,
                cs: u64::from(gdt::USER_CODE),
                ss: u64::from(gdt::USER_DATA),
                rflags: 0x202,
                ..Default::default()
            };
            let request = clone_flags(
                general::CLONE_THREAD
                    | general::CLONE_VM
                    | general::CLONE_SIGHAND
                    | general::CLONE_FILES
                    | general::CLONE_FS
                    | general::CLONE_CHILD_CLEARTID,
                0,
            )
            .unwrap();
            kassert_eq!(process.signal.lock().slot_count(), 1);
            for _ in 0..128 {
                crate::user::copy_to_user(KTEST_USER_VA, &1u32.to_ne_bytes()).map_err(|_| "set join word")?;
                let tid = fork_current(request, 0, 0, KTEST_USER_VA, 0, &frame).map_err(|_| "clone thread")?;
                // Wait beyond clear_child_tid: unlinking also releases the signal
                // slot. Confirm that effect before starting the next clone.
                while lookup_thread(tid as u32).is_some() {
                    crate::time::sleep(1_000_000);
                }
                let mut cleared = [0xff; 4];
                crate::user::copy_from_user(&mut cleared, KTEST_USER_VA).map_err(|_| "read join word")?;
                kassert_eq!(u32::from_ne_bytes(cleared), 0);
                kassert_eq!(process.live_threads.load(Ordering::SeqCst), 1);
                kassert_eq!(process.signal.lock().slot_count(), 2);
                kassert!(process.signal.lock().has_thread(current_thread().tid()));
            }
            PROCS.lock().check()?;
            Ok(())
        })?
    }
);

crate::ktest!(
    fn exec_reserves_private_files_before_teardown() {
        use crate::{kassert, kassert_eq};
        let _image = crate::linux::exec::ExecFixture::new("/tmp/c3-exec", &crate::mm::user_vm::build_relro_test_elf())?;
        for with_sibling in [false, true] {
            in_test_process(move |_| {
                let process = current_process();
                let old_mm = process.address_space().unwrap();
                let old_files = process.files.lock().clone().unwrap();
                // Positive control: the same ELF, argv and env load under
                // this threshold. Files' 1024-slot reservation is larger.
                let exec = crate::linux::exec::Exec::from_path("/tmp/c3-exec").map_err(|_| "plan")?;
                crate::mm::heap::refuse_from(16 * 1024);
                let loaded = exec.load(&[b""], &[], b"/tmp/c3-exec");
                crate::mm::heap::refuse_from(usize::MAX);
                loaded.map_err(|_| "load below Files threshold")?.0.teardown();

                let finish = Arc::new(VforkDone::new());
                let sibling = if with_sibling {
                    let done = Arc::clone(&finish);
                    let (thread, parked) = ktest_sibling(move || {
                        done.wait();
                        record_thread_exit(UserExit::Exited(0));
                    })?;
                    let handle = parked.start();
                    while handle.thread().state() != sched::State::Blocked {
                        sched::yield_now();
                    }
                    Some((thread, handle))
                } else {
                    None
                };
                let users_before = old_files.users();
                let threads_before = process.live_threads.load(Ordering::SeqCst);
                crate::mm::heap::refuse_from(16 * 1024);
                let ret = ktest_exec();
                crate::mm::heap::refuse_from(usize::MAX);
                let threads_after = process.live_threads.load(Ordering::SeqCst);
                let sibling_killed = sibling.as_ref().is_some_and(|(_, h)| h.thread().fatal_pending());
                // Clean up before asserting, including if a mutant ran exec.
                finish.complete();
                if let Some((_, handle)) = sibling {
                    handle.join();
                }
                kassert_eq!(users_before, 1);
                kassert_eq!(threads_before, 1 + usize::from(with_sibling));
                kassert_eq!(ret as i64, -i64::from(errno::ENOMEM));
                kassert_eq!(threads_after, threads_before);
                kassert!(!sibling_killed);
                kassert_eq!(process.signal.lock().exec_waiter, None);
                kassert!(Arc::ptr_eq(&old_mm, &process.address_space().unwrap()));
                kassert!(Arc::ptr_eq(&old_files, &process.files.lock().clone().unwrap()));
                Ok(())
            })??;
        }
        Ok(())
    }
);

crate::ktest!(
    fn exec_unshares_files_after_a_sibling_clones_and_exits() {
        use crate::{arch::gdt, kassert, kassert_eq};
        use lumen_signal::pending::{SenderClass, SigInfo};
        let _image = crate::linux::exec::ExecFixture::new("/tmp/c3-exec", &crate::mm::user_vm::build_relro_test_elf())?;
        in_test_process(|pid| {
            const CODE: u64 = 0x800000;
            let process = current_process();
            // The real process clone waits in pause until cleanup kills it.
            process
                .address_space()
                .unwrap()
                .map_eager_with_bytes(
                    CODE,
                    4096,
                    lumen_vm::Prot::READ | lumen_vm::Prot::EXEC,
                    &[0xb8, 34, 0, 0, 0, 0x0f, 0x05, 0xeb, 0xf7],
                )
                .map_err(|_| "map clone program")?;
            with_current_console(|c| c.io = ConsoleIo::Serial);
            let file = open_kernel_file(b"/dev/null", OpenFlags::O_RDWR, 0).map_err(|_| "open")?;
            let fd = install_current_fd(file, true).map_err(|_| "cloexec descriptor")?;
            let original = process.files.lock().clone().unwrap();
            let delay = transition_delay(pid);
            let pause = Arc::clone(&delay);
            let cloned = Arc::new(SpinLock::new(None));
            let out = Arc::clone(&cloned);
            let (_, parked) = ktest_sibling(move || {
                pause.reached.wait();
                let request = clone_flags(general::CLONE_FILES | general::SIGCHLD, 0).unwrap();
                let frame = crate::arch::syscall::SyscallFrame {
                    rip: CODE,
                    rsp: KTEST_USER_VA + KTEST_USER_LEN,
                    cs: u64::from(gdt::USER_CODE),
                    ss: u64::from(gdt::USER_DATA),
                    rflags: 0x202,
                    ..Default::default()
                };
                *out.lock() = Some(fork_current(request, 0, 0, 0, 0, &frame));
                record_thread_exit(UserExit::Exited(0));
            })?;
            let before = (original.users(), process.live_threads.load(Ordering::SeqCst));
            *EXEC_FILES_DELAY.lock() = Some(Arc::clone(&delay));
            let sibling = parked.start();
            let (p, old, pause) = (Arc::clone(&process), Arc::clone(&original), Arc::clone(&delay));
            let changed = Arc::new(SpinLock::new(None));
            let observed = Arc::clone(&changed);
            let controller = sched::spawn("c3-files-race", move || {
                sibling.join();
                *observed.lock() = Some((old.users(), p.live_threads.load(Ordering::SeqCst)));
                pause.release.complete();
            })
            .map_err(|_| "files controller")?;
            let ret = ktest_exec();
            controller.join();
            *EXEC_FILES_DELAY.lock() = None;
            let child_pid = cloned.lock().take().ok_or("clone result")?.map_err(|_| "sibling clone")? as u32;
            let child = lookup(child_pid).ok_or("cloned child")?;
            let private = !Arc::ptr_eq(&original, &process.files.lock().clone().unwrap());
            let child_kept_fd = child.with_files(|t| t.get(fd).is_some());
            let exec_closed_fd = process.with_files(|t| t.get(fd).is_none());
            let remaining_users = original.users();
            send_process_checked(child_pid as i32, Ok(Some(SigInfo::new(Signal::KILL, 0))), SenderClass::SiUser)
                .map_err(|_| "kill clone")?;
            let reaped = wait_child(wait4_request(child_pid as i32, 0).unwrap()).map_err(|_| "reap clone")?;
            kassert_eq!(before, (1, 2));
            kassert_eq!(*changed.lock(), Some((2, 1)));
            kassert_eq!(ret, 0);
            kassert!(private);
            kassert_eq!(remaining_users, 1);
            kassert_eq!(child_kept_fd, Some(true));
            kassert_eq!(exec_closed_fd, Some(true));
            kassert!(reaped.is_some());
            Ok(())
        })?
    }
);

crate::ktest!(
    fn de_thread_fatal_send_during_wait_refuses_and_clears_waiter() {
        use crate::{kassert, kassert_eq};
        use lumen_signal::pending::{SenderClass, SigInfo};
        let _image = crate::linux::exec::ExecFixture::new("/tmp/c3-exec", &crate::mm::user_vm::build_relro_test_elf())?;
        in_test_process(|pid| {
            let process = current_process();
            let old_mm = process.address_space().unwrap();
            let caller = sched::current().unwrap();
            let finish = Arc::new(VforkDone::new());
            let held = Arc::clone(&finish);
            let (_, parked) = ktest_sibling(move || {
                // Ignore de_thread's fatal kick until the controller has
                // observed the caller asleep and sent its own SIGKILL.
                held.waiters.wait_until(|| held.done.load(Ordering::SeqCst));
                record_thread_exit(UserExit::Exited(0));
            })?;
            let sibling = parked.start();
            let delay = transition_delay(pid);
            *EXEC_WAKE_DELAY.lock() = Some(Arc::clone(&delay));
            let (p, pause) = (Arc::clone(&process), Arc::clone(&delay));
            let result = Arc::new(SpinLock::new(None));
            let out = Arc::clone(&result);
            let controller = sched::spawn("c3-exec-kill", move || {
                while p.signal.lock().exec_waiter != Some(pid) || caller.state() != sched::State::Blocked {
                    sched::yield_now();
                }
                let live_at_send = p.live_threads.load(Ordering::SeqCst);
                let sent = send_thread(Some(pid), pid, Some(SigInfo::new(Signal::KILL, 0)), SenderClass::SiUser);
                pause.reached.wait();
                finish.complete();
                sibling.join();
                // Keep the mutant's tid-exchange precondition valid: it
                // must fail the errno assertion, not an incidental panic.
                let live_after_exit = p.live_threads.load(Ordering::SeqCst);
                *out.lock() = Some((live_at_send, sent, live_after_exit));
                pause.release.complete();
            })
            .map_err(|_| "kill controller")?;
            let ret = ktest_exec();
            controller.join();
            *EXEC_WAKE_DELAY.lock() = None;
            kassert_eq!(*result.lock(), Some((2, Ok(()), 1)));
            kassert_eq!(ret as i64, -i64::from(errno::EAGAIN));
            kassert_eq!(process.signal.lock().exec_waiter, None);
            kassert!(process.signal.lock().ending());
            kassert!(Arc::ptr_eq(&old_mm, &process.address_space().unwrap()));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn clone_racing_a_group_exit_never_escapes() {
        use crate::{kassert, kassert_eq};
        use lumen_signal::pending::{SenderClass, SigInfo};
        for race in 0..3 {
            in_test_process(move |pid| {
                let process = current_process();
                let delay = transition_delay(pid);
                *CLONE_DELAY.lock() = Some(Arc::clone(&delay));
                let (p, pause) = (Arc::clone(&process), Arc::clone(&delay));
                let kernel = sched::current().unwrap();
                let controller = sched::spawn("c3-clone-race", move || {
                    pause.reached.wait();
                    match race {
                        0 => p.record_group_exit(pid, UserExit::Exited(37)),
                        1 => p.signal.lock().exec_waiter = Some(pid),
                        _ => {
                            let mut state = p.signal.lock();
                            state.send(Some(pid), SigInfo::new(Signal::STOP, 0), SenderClass::SiUser).unwrap();
                            state.ktest_dequeue_stop(pid, &p);
                            kernel.clear_sigpending_for_test();
                            drop(state);
                        }
                    }
                    pause.release.complete();
                })
                .map_err(|_| "controller")?;
                let result = fork_current(clone_flags(0x10f00, 0).unwrap(), 0, 0, 0, 0, &Default::default());
                controller.join();
                *CLONE_DELAY.lock() = None;
                process.signal.lock().exec_waiter = None;
                let _ = process.signal.lock().cancel_stop(&process);
                kassert_eq!(
                    result,
                    Err(if race == 0 { errno::EINTR } else { lumen_signal::restart::RestartCode::NoIntr as u32 })
                );
                kassert_eq!(process.live_threads.load(Ordering::SeqCst), 1);
                kassert_eq!(process.address_space().unwrap().users(), 1);
                PROCS.lock().check()?;
                Ok(())
            })??;
        }
        in_test_process(|pid| {
            let mask = Blocked::new(SigSet::of(&[Signal::USR1]));
            set_current_blocked(mask);
            current_thread()
                .configure_alt_stack(0, Some((KTEST_USER_VA, 8192, 0)))
                .map_err(|_| "parent alternate stack")?;
            let (child, parked) = ktest_sibling(|| {
                record_exit(UserExit::Exited(99));
            })?;
            kassert_eq!(child.blocked(), mask);
            kassert_eq!(child.alt_stack(), lumen_signal::altstack::AltStack::DISABLED);
            current_process().record_group_exit(pid, UserExit::Exited(37));
            kassert!(child.scheduler_thread().unwrap().fatal_pending());
            parked.start().join();
            kassert_eq!(*current_process().exit.lock(), Some(UserExit::Exited(37)));
            kassert!(lookup_thread(child.tid()).is_none());
            Ok(())
        })?
    }
);

crate::ktest!(
    fn exit_group_during_de_thread_ends_only_itself() {
        use crate::{kassert, kassert_eq};
        let _image = crate::linux::exec::ExecFixture::new("/tmp/c3-exec", &crate::mm::user_vm::build_relro_test_elf())?;
        for fatal_dequeue in [false, true] {
            in_test_process(move |pid| {
                let done = Arc::new(VforkDone::new());
                let waiting = Arc::clone(&done);
                let result = Arc::new(SpinLock::new(None));
                let out = Arc::clone(&result);
                let (child, parked) = ktest_sibling(move || {
                    if fatal_dequeue {
                        let me = current_thread();
                        me.set_blocked(Blocked::new(SigSet::of(&[Signal::TERM])));
                        send_thread(
                            None,
                            me.tid(),
                            Some(lumen_signal::pending::SigInfo::new(Signal::TERM, 0)),
                            lumen_signal::pending::SenderClass::SiUser,
                        )
                        .unwrap();
                        me.set_blocked(Blocked::EMPTY);
                        assert!(matches!(me.process().next_signal(me.tid()), Delivery::Fatal(Signal::TERM, None)));
                    }
                    waiting.wait(); // The actual vfork TASK_KILLABLE wait.
                    let me = current_thread();
                    let exec = me.process().signal.lock().exec_waiter;
                    record_exit(if fatal_dequeue { UserExit::Signaled(Signal::TERM) } else { UserExit::Exited(42) });
                    *out.lock() = Some((
                        exec,
                        me.process().signal.lock().ending(),
                        sched::with_current(|t| t.fatal_pending()).unwrap(),
                    ));
                })?;
                let handle = parked.start();
                while handle.thread().state() != sched::State::Blocked {
                    sched::yield_now();
                }
                let delay = transition_delay(pid);
                *EXEC_DELAY.lock() = Some(Arc::clone(&delay));
                let pause = Arc::clone(&delay);
                let controller = sched::spawn("c3-exec-race", move || {
                    pause.reached.wait();
                    pause.release.complete();
                })
                .map_err(|_| "controller")?;
                let ret = ktest_exec();
                controller.join();
                *EXEC_DELAY.lock() = None;
                done.complete(); // cleanup even if exec was refused
                sched::join_bounded(handle)?;
                kassert_eq!(ret, 0);
                kassert_eq!(*result.lock(), Some((Some(pid), false, true)));
                kassert!(lookup_thread(child.tid()).is_none());
                kassert!(!current_process().signal.lock().ending());
                kassert_eq!(current_thread().tid(), pid);
                Ok(())
            })??;
        }
        Ok(())
    }
);

crate::ktest!(
    fn de_thread_refuses_a_racing_group_stop() {
        use crate::{kassert, kassert_eq};
        use lumen_signal::pending::{SenderClass, SigInfo};
        let _image = crate::linux::exec::ExecFixture::new("/tmp/c3-exec", &crate::mm::user_vm::build_relro_test_elf())?;
        // Isolate the exec guard from begin_exec's independent clearing of
        // the dequeue mark: either protection alone must retain its contract.
        in_job_control_process(|pid| {
            let process = current_process();
            send_thread(None, pid, Some(SigInfo::new(Signal::TSTP, 0)), SenderClass::SiUser).map_err(|_| "job stop")?;
            kassert!(matches!(process.next_signal(pid), Delivery::Stop(Signal::TSTP)));
            process.signal.lock().exec_waiter = Some(pid);
            process.start_job_stop(pid, Signal::TSTP);
            let stopped = process.stopped.load(Ordering::SeqCst);
            process.signal.lock().exec_waiter = None;
            process.signal.lock().cancel_stop(&process);
            kassert!(!stopped);
            Ok(())
        })?;
        for continued in [false, true] {
            in_job_control_process(move |pid| {
                let start = Arc::new(VforkDone::new());
                let go = Arc::clone(&start);
                let (child, parked) = ktest_sibling(move || {
                    go.wait();
                    let me = current_thread();
                    let next = me.process().next_signal(me.tid());
                    assert!(matches!(next, Delivery::Stop(Signal::TSTP)));
                    me.process().start_job_stop(me.tid(), Signal::TSTP);
                })?;
                let gap = transition_delay(child.tid());
                *signal::STOP_GAP_DELAY.lock() = Some(Arc::clone(&gap));
                let sent = send_thread(None, child.tid(), Some(SigInfo::new(Signal::TSTP, 0)), SenderClass::SiUser);
                let handle = parked.start();
                start.complete();
                gap.reached.wait();
                if continued {
                    send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::CONT, 0))), SenderClass::SiUser)
                        .map_err(|_| "continue")?;
                    gap.release.complete();
                    handle.join();
                    *signal::STOP_GAP_DELAY.lock() = None;
                    kassert!(!current_process().stopped.load(Ordering::SeqCst));
                    kassert_eq!(ktest_exec(), 0);
                } else {
                    let process = current_process();
                    let delay = transition_delay(pid);
                    *EXEC_DELAY.lock() = Some(Arc::clone(&delay));
                    let (p, exec, stop) = (Arc::clone(&process), Arc::clone(&delay), Arc::clone(&gap));
                    let controller = sched::spawn("c3-stop-gap", move || {
                        exec.reached.wait();
                        exec.release.complete();
                        while p.signal.lock().exec_waiter.is_none() {
                            sched::yield_now();
                        }
                        stop.release.complete();
                    })
                    .map_err(|_| "stop controller")?;
                    let ret = ktest_exec();
                    controller.join();
                    handle.join();
                    *EXEC_DELAY.lock() = None;
                    *signal::STOP_GAP_DELAY.lock() = None;
                    kassert_eq!(ret, 0);
                    kassert!(!process.stopped.load(Ordering::SeqCst));
                    kassert_eq!(process.signal.lock().stop_remaining(), None);
                    kassert!(!process.signal.lock().ending());
                }
                kassert_eq!(sent, Ok(()));
                Ok(())
            })?;
        }
        in_test_process(|pid| {
            let process = current_process();
            send_thread(None, pid, Some(SigInfo::new(Signal::STOP, 0)), SenderClass::SiUser).map_err(|_| "stop")?;
            kassert!(matches!(process.next_signal(pid), Delivery::Stop(Signal::STOP)));
            let stop = process.signal.lock().participate_stop(pid).ok_or("completed stop")?;
            PROCS.lock().complete_stop(&process, stop);
            kassert_eq!(process.signal.lock().stop_remaining(), Some(0));
            kassert_eq!(ktest_exec(), 0);
            kassert!(!process.stopped.load(Ordering::SeqCst));
            kassert_eq!(process.signal.lock().stop_remaining(), None);
            kassert!(!matches!(ps_rows().into_iter().find(|p| p.pid == pid).unwrap().state, ProcState::Stopped { .. }));
            Ok(())
        })?
    }
);

/// A child in its own group, with a live parent in another group of the
/// same session: job-control stops exercise the real orphan check.
#[cfg(feature = "ktest")]
fn in_job_control_process(
    f: impl FnOnce(u32) -> crate::ktest::TestResult + Send + 'static,
) -> crate::ktest::TestResult {
    in_test_process(move |parent| {
        let result = Arc::new(SpinLock::new(None));
        let out = Arc::clone(&result);
        let (pid, parked) = ktest_fork_child(move || {
            *out.lock() = Some(f(current_process().pid()));
            record_thread_exit(UserExit::Exited(0));
            let _ = take_current();
        })?;
        PROCS.lock().child_group_for_ktest(parent, pid);
        let _ = ktest_reap(pid, parked.start())?;
        result.lock().take().ok_or("job-control child result")?
    })?
}

crate::ktest!(
    fn exit_retargets_signals_and_completes_a_stop() {
        use crate::{kassert, kassert_eq};
        use lumen_signal::pending::{SenderClass, SigInfo};
        let _image = crate::linux::exec::ExecFixture::new("/tmp/c3-exec", &crate::mm::user_vm::build_relro_test_elf())?;
        for exec in [false, true] {
            in_test_process(move |pid| {
                let process = current_process();
                set_current_action(Signal::USR1, KernelSigaction { handler: 0x400000, ..Default::default() });
                let done = Arc::new(VforkDone::new());
                let waiting = Arc::clone(&done);
                let (child, parked) = ktest_sibling(move || {
                    waiting.wait();
                    record_thread_exit(UserExit::Exited(0));
                })?;
                let handle = parked.start();
                while handle.thread().state() != sched::State::Blocked {
                    sched::yield_now();
                }
                process.signal.lock().prefer_thread(child.tid());
                send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::USR1, 0))), SenderClass::SiUser)
                    .map_err(|_| "shared signal")?;
                kassert!(!sched::current_sigpending());
                kassert!(child.scheduler_thread().unwrap().sigpending());
                if exec {
                    kassert_eq!(ktest_exec(), 0);
                } else {
                    done.complete();
                }
                handle.join();
                kassert!(sched::current_sigpending());
                set_current_action(Signal::USR1, KernelSigaction { handler: 0x400000, ..Default::default() });
                kassert!(matches!(process.next_signal(pid), Delivery::Handler(_)));
                kassert!(matches!(process.next_signal(pid), Delivery::None));
                Ok(())
            })??;
        }
        in_test_process(|_| {
            set_current_blocked(Blocked::new(SigSet::of(&[Signal::CHLD])));
            let done = Arc::new(VforkDone::new());
            let end = Arc::clone(&done);
            let (pid, parked) = ktest_fork_child(move || {
                end.wait();
                record_thread_exit(UserExit::Exited(0));
                let _ = take_current();
            })?;
            let child = lookup(pid).ok_or("stop child")?;
            let extra = add_ktest_thread(&child)?;
            // The child leader will participate through next_signal before
            // being allowed to finish; the synthetic last participant exits.
            send_thread(None, pid, Some(SigInfo::new(Signal::STOP, 0)), SenderClass::SiUser)
                .map_err(|_| "stop child")?;
            let mut state = child.signal.lock();
            state.ktest_dequeue_stop(pid, &child);
            let _ = state.participate_stop(pid);
            drop(state);
            remove_ktest_thread(&extra);
            let report = wait_child(wait4_request(pid as i32, general::WUNTRACED | general::WNOHANG).unwrap())
                .map_err(|_| "stop report")?;
            let info = current_thread().wait_signal(SigSet::of(&[Signal::CHLD]), Some(crate::time::now_ns()));
            send_process_checked(pid as i32, Ok(Some(SigInfo::new(Signal::CONT, 0))), SenderClass::SiUser)
                .map_err(|_| "continue")?;
            done.complete();
            kassert_eq!(ktest_reap(pid, parked.start())?, 0);
            kassert!(report.is_some_and(|r| r.status() == lumen_proc::WaitStatus::Stopped(Signal::STOP)));
            let info = info.map_err(|_| "CLD_STOPPED")?;
            kassert_eq!(i32::from_ne_bytes(info.bytes()[8..12].try_into().unwrap()), 5);
            Ok(())
        })?
    }
);

crate::ktest!(
    fn rlimit_nproc_enforces_fork_clone_and_reap() {
        use crate::{kassert, kassert_eq};
        in_test_process(|_| {
            let process = current_process();
            kassert_eq!(process.rlimit(RlimitKind::Nproc, None), Ok(Rlimit::INFINITY));
            // Kernel callers cannot bypass the NOFILE hard ceiling either.
            kassert_eq!(
                process.rlimit(RlimitKind::Nofile, Some(Rlimit::new(0, lumen_vfs::MAX_FDS as u64 + 1).unwrap())),
                Err(errno::EPERM)
            );

            let base = PROCS.lock().nproc_task_count() as u64;
            if base != 1 {
                return Err("expected baseline nproc_task_count == 1 (only this test process, excluding pid 1)");
            }

            // Soft limit 0 refuses both fork (link_child) and thread clone (fork_current & link_thread) with EAGAIN.
            kassert_eq!(process.rlimit(RlimitKind::Nproc, Some(Rlimit::new(0, 0).unwrap())), Ok(Rlimit::INFINITY));
            let thread_req = clone_flags(0x10f00, 0).unwrap();
            kassert_eq!(fork_current(thread_req, 0, 0, 0, 0, &Default::default()), Err(errno::EAGAIN));

            let caller = current_thread();
            for _ in 0..2 {
                let dummy = Arc::new(UserThread::unbound_for_ktest(Arc::clone(&process)));
                let refused_thread = {
                    let mut table = PROCS.lock();
                    let mut state = process.signal.lock();
                    let init = state.thread_init(caller.tid(), thread_req);
                    let signals = state.prepare_thread(&dummy, init).map_err(|_| "prepare thread")?;
                    table.link_thread(&dummy, signals, &mut state)
                };
                kassert_eq!(refused_thread, Err(errno::EAGAIN));
                // Cancelled slot is reused on every refused attempt rather than leaking.
                kassert_eq!(process.signal.lock().slot_count(), 2);
            }
            PROCS.lock().check()?;

            let new = Process::copy_for_fork(&process, ProcessClone::FORK.bits()).map_err(|_| "copy fork")?;
            let dummy_child = Arc::new(UserThread::unbound_for_ktest(Arc::clone(&new.process)));
            kassert_eq!(PROCS.lock().link_child(&caller, &dummy_child, ProcessClone::FORK), Err(errno::EAGAIN));
            drop(new);

            // Finite limit (base + 1) allows one child, refuses a second while live OR
            // an unreaped zombie, and allows another once reaped.
            kassert!(process.rlimit(RlimitKind::Nproc, Some(Rlimit::new(base + 1, 10).unwrap())).is_ok());
            let release = Arc::new(VforkDone::new());
            let wait = Arc::clone(&release);
            let (child_pid, parked) = ktest_fork_child(move || {
                wait.wait();
                record_thread_exit(UserExit::Exited(0));
                let _ = take_current();
            })?;
            let handle = parked.start();

            // While child is live (count == base + 1): both thread clone and process fork fail with EAGAIN.
            kassert_eq!(fork_current(thread_req, 0, 0, 0, 0, &Default::default()), Err(errno::EAGAIN));
            let new = Process::copy_for_fork(&process, ProcessClone::FORK.bits()).map_err(|_| "copy live")?;
            let extra = Arc::new(UserThread::unbound_for_ktest(Arc::clone(&new.process)));
            kassert_eq!(PROCS.lock().link_child(&caller, &extra, ProcessClone::FORK), Err(errno::EAGAIN));
            drop(new);

            // Let child exit without reaping: zombie still counts against RLIMIT_NPROC.
            release.complete();
            handle.join();
            kassert_eq!(fork_current(thread_req, 0, 0, 0, 0, &Default::default()), Err(errno::EAGAIN));
            let new = Process::copy_for_fork(&process, ProcessClone::FORK.bits()).map_err(|_| "copy zombie")?;
            let extra = Arc::new(UserThread::unbound_for_ktest(Arc::clone(&new.process)));
            kassert_eq!(PROCS.lock().link_child(&caller, &extra, ProcessClone::FORK), Err(errno::EAGAIN));
            drop(new);

            // Reaping the zombie drops count back to base.
            let report = wait_child(wait4_request(child_pid as i32, 0).map_err(|_| "wait4 req")?)
                .map_err(|_| "wait4")?
                .ok_or("reaped child")?;
            kassert_eq!(report.pid(), child_pid);
            kassert_eq!(PROCS.lock().nproc_task_count() as u64, base);

            // Under the same finite limit (base + 1), a sibling thread succeeds, blocks the
            // next fork with EAGAIN, and frees its slot on exit/join so a fork succeeds after.
            let thread_done = Arc::new(VforkDone::new());
            let thread_wait = Arc::clone(&thread_done);
            let (_sibling, sibling_parked) = ktest_sibling(move || {
                thread_wait.wait();
                record_thread_exit(UserExit::Exited(0));
            })?;
            let sibling_handle = sibling_parked.start();
            kassert_eq!(PROCS.lock().nproc_task_count() as u64, base + 1);
            let new = Process::copy_for_fork(&process, ProcessClone::FORK.bits()).map_err(|_| "copy with sibling")?;
            let extra = Arc::new(UserThread::unbound_for_ktest(Arc::clone(&new.process)));
            kassert_eq!(PROCS.lock().link_child(&caller, &extra, ProcessClone::FORK), Err(errno::EAGAIN));
            drop(new);
            kassert_eq!(fork_current(thread_req, 0, 0, 0, 0, &Default::default()), Err(errno::EAGAIN));
            thread_done.complete();
            sibling_handle.join();
            kassert_eq!(PROCS.lock().nproc_task_count() as u64, base);

            let (second_pid, second_parked) = ktest_fork_child(move || {
                record_thread_exit(UserExit::Exited(0));
                let _ = take_current();
            })?;
            kassert!(second_pid != 0);
            kassert_eq!(ktest_reap(second_pid, second_parked.start())?, 0);

            // Soft limit == base refuses even a single thread or child (caller already counts as base).
            kassert!(process.rlimit(RlimitKind::Nproc, Some(Rlimit::new(base, 10).unwrap())).is_ok());
            kassert_eq!(fork_current(thread_req, 0, 0, 0, 0, &Default::default()), Err(errno::EAGAIN));
            Ok(())
        })?
    }
);

crate::ktest!(
    fn rlimit_nproc_and_nofile_inherit_across_fork_and_exec() {
        use crate::kassert_eq;
        let _image = crate::linux::exec::ExecFixture::new("/tmp/c3-exec", &crate::mm::user_vm::build_relro_test_elf())?;
        in_test_process(|_| {
            let parent = current_process();
            let parent_nproc = Rlimit::new(4, 8).unwrap();
            let parent_nofile = Rlimit::new(16, 32).unwrap();
            kassert_eq!(parent.rlimit(RlimitKind::Nproc, Some(parent_nproc)), Ok(Rlimit::INFINITY));
            kassert_eq!(parent.rlimit(RlimitKind::Nofile, Some(parent_nofile)), Ok(Rlimit::DEFAULT_NOFILE));

            // Child inherits both limits; modifying child's limits leaves parent untouched.
            let observed = Arc::new(SpinLock::new(None));
            let out = Arc::clone(&observed);
            let (child_pid, parked) = ktest_fork_child(move || {
                let me = current_process();
                let seen = (me.rlimit(RlimitKind::Nproc, None).unwrap(), me.rlimit(RlimitKind::Nofile, None).unwrap());
                me.rlimit(RlimitKind::Nproc, Some(Rlimit::new(2, 4).unwrap())).unwrap();
                me.rlimit(RlimitKind::Nofile, Some(Rlimit::new(6, 12).unwrap())).unwrap();
                *out.lock() = Some(seen);
                record_thread_exit(UserExit::Exited(0));
                let _ = take_current();
            })?;
            kassert_eq!(ktest_reap(child_pid, parked.start())?, 0);
            kassert_eq!(observed.lock().take(), Some((parent_nproc, parent_nofile)));
            kassert_eq!(parent.rlimit(RlimitKind::Nproc, None), Ok(parent_nproc));
            kassert_eq!(parent.rlimit(RlimitKind::Nofile, None), Ok(parent_nofile));

            // Both limits survive execve.
            kassert_eq!(ktest_exec(), 0);
            kassert_eq!(parent.rlimit(RlimitKind::Nproc, None), Ok(parent_nproc));
            kassert_eq!(parent.rlimit(RlimitKind::Nofile, None), Ok(parent_nofile));
            Ok(())
        })?
    }
);
