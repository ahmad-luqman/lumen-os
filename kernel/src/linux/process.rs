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
//! Lock discipline (ARCHITECTURE section 9, level 10): `PROCS`, then a
//! `Process::signal`, then the leaves (a process's `mm`, `files`, `fs`,
//! `exe_path`, `console` and `exit` slots, `Files::table`,
//! `FsState::inner`), never two leaves at once. All are IRQ-off
//! [`SpinLock`]s: never held across `copy_from_user`, `copy_to_user`,
//! blocking waits, or `AddressSpace` operations; nor across anything that
//! takes a VFS lock (an open, `stat`, an inode's lock state): an open file
//! or cwd leaves its lock as a clone or an extracted entry, used and
//! dropped after.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::marker::PhantomData;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use log::warn;
use lumen_proc::{ExitAction, ExitError, ExitStatus, INIT_PID, ProcState, ProcTree, RusageSlot};
use lumen_signal::{Blocked, SigSet, Signal};
use lumen_vfs::{Escaped, FdTable, InodeType, OpenFile, OpenFlags, OpenGuard, VfsError, VfsInode, open_inode_guarded};

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
    OpenFile::from_opened(opened)
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
        let opened = Self::open_from(b"/dev/console");
        if let Err(e) = &opened {
            log::error!(target: "linux", "cannot open a process's stdio (/dev/console) and working directory (/): {e:?}");
        }
        opened
    }

    /// [`Stdio::open`] with `console` for the console's path, not logged:
    /// the seam the refusal's ktest opens a missing path through.
    fn open_from(console: &[u8]) -> Result<Self, VfsError> {
        let mut fds = FdTable::new();
        for (fd, flags) in [(0, OpenFlags::O_RDONLY), (1, OpenFlags::O_WRONLY), (2, OpenFlags::O_WRONLY)] {
            let file = Arc::new(open_kernel_file(console, flags, 0)?);
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

/// `SIG_IGN` as a `sa_handler` value (Linux's `(void (*)(int))1`).
const SIG_IGN: u64 = 1;

/// The largest pid (and tid) `PROCS`' tree hands out: pids run
/// `2..=PID_MAX`, as Linux's default `pid_max` hands out `1..32767`
/// (lumen-proc wraps to 2).
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
    /// Its working directory and umask.
    fs: SpinLock<Option<Arc<FsState>>>,
    /// Its signal dispositions, and each thread's mask.
    signal: SpinLock<SignalState>,
    /// The executable's path for `/proc/<pid>/exe`: set by `linux /path`
    /// (the path as typed) and by `vfstest dev-tmp-proc`'s checks (and the
    /// ktests running them).
    exe_path: SpinLock<Option<String>>,
    /// Its `RusageSlot`'s value in `PROCS`' tree.
    usage_slot: u64,
    /// M7's console state (until the console is a tty).
    console: SpinLock<ConsoleState>,
    allow_test_syscalls: bool,
    /// How its run ended: the first [`UserExit`] recorded wins (C1 and C3
    /// replace it with the group exit, `SignalState::group_exit`).
    exit: SpinLock<Option<UserExit>>,
}

impl Process {
    fn new(
        mm: Option<Arc<AddressSpace>>,
        files: Option<Arc<Files>>,
        fs: Option<Arc<FsState>>,
        signal: SignalState,
        console: ConsoleState,
        allow_test_syscalls: bool,
        exe_path: Option<String>,
    ) -> Self {
        Self {
            pid: AtomicU32::new(0),
            mm: SpinLock::new(mm),
            files: SpinLock::new(files),
            fs: SpinLock::new(fs),
            signal: SpinLock::new(signal),
            exe_path: SpinLock::new(exe_path),
            usage_slot: NEXT_USAGE_SLOT.fetch_add(1, Ordering::Relaxed),
            console: SpinLock::new(console),
            allow_test_syscalls,
            exit: SpinLock::new(None),
        }
    }

    /// Store its pid, once: panics if it already has one.
    fn set_pid(&self, pid: u32) {
        if let Err(had) = self.pid.compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire) {
            panic!("process {had} given a second pid, {pid}");
        }
    }

    /// Its pid (the thread group id): set before anything can read it.
    pub fn pid(&self) -> u32 {
        let pid = self.pid.load(Ordering::Acquire);
        debug_assert_ne!(pid, 0, "a process's pid read before it was set");
        pid
    }

    /// Its parent's pid, or 0 without one (a root `linux` started, as M6's
    /// `getppid`). Takes `PROCS`: never call it holding a process lock.
    pub fn ppid(&self) -> u32 {
        PROCS.lock().tree.parent(self.pid()).flatten().unwrap_or(0)
    }

    /// Whether its run may make `usertest`'s test syscalls.
    pub fn allow_test_syscalls(&self) -> bool {
        self.allow_test_syscalls
    }

    /// Its address space (a clone, dropped by the caller with no lock
    /// held), until its takedown.
    fn address_space(&self) -> Option<Arc<AddressSpace>> {
        self.mm.lock().clone()
    }

    /// `f` on its descriptor table, under the table's lock (a leaf but for
    /// the heap: nothing in `f` may take a lock or close a file); `None`
    /// after its takedown.
    fn with_files<R>(&self, f: impl FnOnce(&mut FdTable<FileRef>) -> R) -> Option<R> {
        let files = self.files.lock().clone()?;
        let mut table = files.table.lock();
        Some(f(&mut table))
    }

    /// `f` on its working directory and umask, under their lock (a leaf
    /// but for the heap: nothing in `f` may take a lock or drop a working
    /// directory); `None` after its takedown.
    fn with_fs<R>(&self, f: impl FnOnce(&mut FsInner) -> R) -> Option<R> {
        let fs = self.fs.lock().clone()?;
        let mut inner = fs.inner.lock();
        Some(f(&mut inner))
    }

    /// Whether its `SIGCHLD` disposition is `SIG_IGN` or carries
    /// `SA_NOCLDWAIT`: its children's zombies leave at once
    /// (`ProcTree::exit`'s `autoreap`). Takes `Process::signal`: callable
    /// under `PROCS`.
    fn autoreaps(&self) -> bool {
        let chld = self.signal.lock().action(Signal::CHLD);
        chld.handler == SIG_IGN || chld.flags & u64::from(general::SA_NOCLDWAIT) != 0
    }

    /// Exit step 3, in thread context, its CPU already off its address
    /// space: give up its user of the space (torn down at the last,
    /// `AddressSpace::drop_user`), its share of the descriptors (the last
    /// sharer closes them all, `Files::leave`) and its working directory.
    /// Each slot is emptied under its lock and released after.
    fn release_resources(&self) {
        let mm = self.mm.lock().take();
        if let Some(mm) = mm {
            mm.drop_user();
        }
        let files = self.files.lock().take();
        if let Some(files) = files {
            files.leave(self.pid());
        }
        let fs = self.fs.lock().take();
        drop(fs);
    }
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

/// A descriptor table, shared by the processes `clone(CLONE_FILES)` links
/// (M8).
pub struct Files {
    table: SpinLock<FdTable<FileRef>>,
    /// The processes sharing it: the one that brings it to 0 closes every
    /// descriptor (`Files::leave`), so the last `Arc` (a procfs reader's,
    /// say) frees memory only.
    users: AtomicUsize,
}

impl Files {
    fn new(table: FdTable<FileRef>) -> Self {
        Self { table: SpinLock::new(table), users: AtomicUsize::new(1) }
    }

    /// Another process shares it (C1's `clone(CLONE_FILES)`).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // `procs_lookup_and_lifetime`; C1's `clone` next
    pub fn share(self: &Arc<Self>) -> Arc<Self> {
        let before = self.users.fetch_add(1, Ordering::AcqRel);
        assert_ne!(before, 0, "a descriptor table shared after its last user left");
        Arc::clone(self)
    }

    /// Process `pid` stops sharing it: the last one closes every descriptor
    /// (`release_fd`, outside every spinlock). Panics if no process shared
    /// it. Takes inodes' locks: never call it holding a spinlock.
    fn leave(self: Arc<Self>, pid: u32) {
        let before = self.users.fetch_sub(1, Ordering::AcqRel);
        assert_ne!(before, 0, "pid {pid} left a descriptor table nobody shared");
        if before == 1 {
            let entries = self.table.lock().drain_all();
            for entry in entries {
                release_fd(pid, entry.file);
            }
        }
    }

    /// The processes sharing it.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // `procs_lookup_and_lifetime`; C1's `fork` next
    pub fn users(&self) -> usize {
        self.users.load(Ordering::Acquire)
    }

    /// How many descriptors are open.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // `procs_lookup_and_lifetime`
    pub fn open_count(&self) -> usize {
        self.table.lock().open_count()
    }
}

/// A process's working directory and umask, shared by the processes
/// `clone(CLONE_FS)` links (M8).
pub struct FsState {
    inner: SpinLock<FsInner>,
}

/// What [`FsState`] holds.
pub struct FsInner {
    /// The working directory (`set_current_cwd` swaps it).
    cwd: Cwd,
    /// The file mode creation mask (`0o777` bits only).
    umask: u16,
}

impl FsState {
    fn new(cwd: Cwd) -> Self {
        Self { inner: SpinLock::new(FsInner { cwd, umask: DEFAULT_UMASK }) }
    }
}

/// A process's signal dispositions and its threads' masks. M8's delivery
/// (pending signals, stops, the alternate stack) adds to it.
pub struct SignalState {
    /// By signal number - 1.
    actions: [KernelSigaction; 64],
    /// By tid.
    threads: BTreeMap<u32, ThreadSignals>,
}

impl SignalState {
    fn new() -> Self {
        Self { actions: [KernelSigaction::default(); 64], threads: BTreeMap::new() }
    }

    /// `signal`'s disposition.
    pub fn action(&self, signal: Signal) -> KernelSigaction {
        self.actions[signal.index()]
    }

    /// Install `action` for `signal`, its mask stripped of `SIGKILL` and
    /// `SIGSTOP`: the action it replaces, or `None` (nothing changed) for
    /// `SIGKILL` or `SIGSTOP`, whose dispositions never change.
    pub fn set_action(&mut self, signal: Signal, mut action: KernelSigaction) -> Option<KernelSigaction> {
        if signal.is_unblockable() {
            return None;
        }
        action.mask = SigSet(action.mask).unblockable_stripped().0;
        Some(core::mem::replace(&mut self.actions[signal.index()], action))
    }

    /// Thread `tid`'s signal state: panics if it isn't one of this
    /// process's live threads.
    fn thread_mut(&mut self, tid: u32) -> &mut ThreadSignals {
        self.threads.get_mut(&tid).unwrap_or_else(|| panic!("thread {tid} has no signal state"))
    }
}

/// One thread's signal state.
pub struct ThreadSignals {
    /// The signals it blocks (`rt_sigprocmask`): never `SIGKILL` or
    /// `SIGSTOP`.
    blocked: Blocked,
}

impl ThreadSignals {
    fn new(blocked: Blocked) -> Self {
        Self { blocked }
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
    /// Its scheduler thread's id (`sched::threads()`' id).
    kernel_thread: u64,
}

impl UserThread {
    fn new(process: Arc<Process>, kernel_thread: u64) -> Self {
        Self { tid: AtomicU32::new(0), process, clear_child_tid: AtomicU64::new(0), kernel_thread }
    }

    /// Store its tid, once: panics if it already has one.
    fn set_tid(&self, tid: u32) {
        if let Err(had) = self.tid.compare_exchange(0, tid, Ordering::AcqRel, Ordering::Acquire) {
            panic!("thread {had} given a second tid, {tid}");
        }
    }

    /// Its tid: set before anything can read it.
    pub fn tid(&self) -> u32 {
        let tid = self.tid.load(Ordering::Acquire);
        debug_assert_ne!(tid, 0, "a thread's tid read before it was set");
        tid
    }

    /// Its process.
    pub fn process(&self) -> &Arc<Process> {
        &self.process
    }

    /// Record `set_tid_address`'s pointer.
    pub fn set_clear_child_tid(&self, tidptr: u64) {
        self.clear_child_tid.store(tidptr, Ordering::Relaxed);
    }

    /// `set_tid_address`'s pointer (0 if none was set).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // the identity ktests; C1's exit next
    pub fn clear_child_tid(&self) -> u64 {
        self.clear_child_tid.load(Ordering::Relaxed)
    }
}

/// Every process by pid and every user thread by tid, with the process
/// tree (`PROCS`). Its methods are the only writers of its fields.
struct ProcTable {
    tree: ProcTree,
    /// Live processes and zombies, by pid.
    procs: BTreeMap<u32, Arc<Process>>,
    /// Live user threads, by tid.
    threads: BTreeMap<u32, Arc<UserThread>>,
}

impl ProcTable {
    /// Link `init` as pid 1 (`ProcTree::spawn_init`), or the tree's refusal
    /// (nothing changed).
    fn link_init(&mut self, init: &Arc<Process>) -> Result<(), lumen_proc::SpawnInitError> {
        self.tree.spawn_init(RusageSlot::new(init.usage_slot))?;
        let displaced = self.procs.insert(INIT_PID, Arc::clone(init));
        assert!(displaced.is_none(), "pid 1 already in PROCS");
        Ok(())
    }

    /// Link `process` and its leader `thread` as a new root (`spawn_root`,
    /// no parent): their pid and tid, or `None` (nothing changed) when no
    /// pid is free.
    fn link_root(&mut self, process: &Arc<Process>, thread: &Arc<UserThread>) -> Option<u32> {
        let pid = self.tree.spawn_root(RusageSlot::new(process.usage_slot)).ok()?;
        process.set_pid(pid);
        self.insert_thread(pid, thread);
        let displaced = self.procs.insert(pid, Arc::clone(process));
        assert!(displaced.is_none(), "pid {pid} handed out while in PROCS");
        Some(pid)
    }

    /// Link `thread`, a non-leader thread of its (linked) process, with a
    /// tid from the tree (`ProcTree::add_thread`): the tid, or the tree's
    /// refusal (nothing changed).
    #[cfg(feature = "ktest")] // M8's `clone(CLONE_THREAD)` (C3) next
    fn link_thread(&mut self, thread: &Arc<UserThread>) -> Result<u32, lumen_proc::Error> {
        let pid = thread.process().pid();
        let tid = self.tree.add_thread(pid)?;
        self.insert_thread(tid, thread);
        Ok(tid)
    }

    /// Give `thread` tid `tid` and put it in `threads` and in its process's
    /// signal state (`Process::signal` under `PROCS`, section 9's order).
    fn insert_thread(&mut self, tid: u32, thread: &Arc<UserThread>) {
        thread.set_tid(tid);
        let blocked = thread.process().signal.lock().threads.insert(tid, ThreadSignals::new(Blocked::EMPTY));
        assert!(blocked.is_none(), "tid {tid} already had signal state");
        let displaced = self.threads.insert(tid, Arc::clone(thread));
        assert!(displaced.is_none(), "tid {tid} handed out while in PROCS");
    }

    /// Unlink `thread`: out of `threads`, its process's signal state and,
    /// for a non-leader, the tree. Returns `PROCS`' reference, for the
    /// caller to drop with `PROCS` released. Panics if it isn't linked.
    fn unlink_thread(&mut self, thread: &Arc<UserThread>) -> Arc<UserThread> {
        let (pid, tid) = (thread.process().pid(), thread.tid());
        if tid != pid
            && let Err(e) = self.tree.remove_thread(pid, tid)
        {
            panic!("thread {tid} of {pid} unlinked: {e}");
        }
        let signals = thread.process().signal.lock().threads.remove(&tid);
        assert!(signals.is_some(), "thread {tid} had no signal state");
        let gone = self.threads.remove(&tid).unwrap_or_else(|| panic!("thread {tid} wasn't in PROCS"));
        assert!(Arc::ptr_eq(&gone, thread), "tid {tid} in PROCS is another thread");
        gone
    }

    /// `pid`, with no thread left, ends with `status` in the tree, its
    /// children going to pid 1, and every action B1 can reach carried out:
    /// the processes that left `PROCS` (a root, a child of an autoreaping
    /// parent, and their released zombies), for the caller to drop with
    /// `PROCS` released. A process whose zombie waits for its parent's wait
    /// (C1) stays.
    fn exit_in_tree(&mut self, pid: u32, status: ExitStatus) -> Result<Vec<Arc<Process>>, ExitError> {
        let Self { tree, procs, .. } = self;
        let autoreap = |parent: u32| procs.get(&parent).is_some_and(|p| p.autoreaps());
        let outcome = tree.exit(pid, status, Some(INIT_PID), autoreap)?;
        let mut gone = Vec::new();
        for action in outcome {
            match action {
                ExitAction::Released { child, .. } | ExitAction::Notify { child, released: Some(_), .. } => {
                    let process = procs.remove(&child).unwrap_or_else(|| panic!("pid {child} released, not in PROCS"));
                    gone.push(process);
                }
                // A zombie waits for its parent's wait (M8's `wait4`).
                ExitAction::Notify { released: None, .. } => {}
                // Nothing to signal before M8's signal delivery.
                ExitAction::HangupThenContinue { .. } => {}
            }
        }
        Ok(gone)
    }

    /// Check that the table agrees with itself and its tree (ktests):
    /// each thread's tid is its key, its process is the one `procs` holds
    /// for its pid, the tid is the pid or one of the tree's threads of it,
    /// its process's signal state has it; each `procs` key is in the tree;
    /// and the tree's own invariants.
    #[cfg(feature = "ktest")]
    fn check(&self) -> Result<(), &'static str> {
        for (&tid, thread) in &self.threads {
            let pid = thread.process().pid();
            if thread.tid() != tid {
                return Err("a thread's tid isn't its key");
            }
            if !self.procs.get(&pid).is_some_and(|p| Arc::ptr_eq(p, thread.process())) {
                return Err("a thread's process isn't PROCS' process for its pid");
            }
            if tid != pid && !self.tree.threads(pid).contains(&tid) {
                return Err("a non-leader tid isn't in the tree");
            }
            if !thread.process().signal.lock().threads.contains_key(&tid) {
                return Err("a thread has no signal state");
            }
        }
        if !self.procs.keys().all(|&pid| self.tree.contains(pid)) {
            return Err("a process in PROCS isn't in the tree");
        }
        self.tree.check_invariants();
        Ok(())
    }
}

/// The kernel's process table (section 9's Lock 10a): taken before any
/// `Process::signal`, which may nest under it, and before the leaf locks.
/// The tree allocates under it.
static PROCS: SpinLock<ProcTable> =
    SpinLock::new(ProcTable { tree: ProcTree::new(PID_MAX), procs: BTreeMap::new(), threads: BTreeMap::new() });

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
    let _ = signal.set_action(Signal::CHLD, ignore);
    let init = Arc::new(Process::new(
        Some(Arc::new(mm)),
        None,
        None,
        signal,
        ConsoleState::new(ConsoleIo::Capture, Vec::new()),
        false,
        None,
    ));
    init.set_pid(INIT_PID);
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
pub fn lookup(pid: u32) -> Option<Arc<Process>> {
    PROCS.lock().procs.get(&pid).cloned()
}

/// The live user thread `tid`.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // `procs_lookup_and_lifetime`; C5's `tkill` next
pub fn lookup_thread(tid: u32) -> Option<Arc<UserThread>> {
    PROCS.lock().threads.get(&tid).cloned()
}

/// How a process's run ended, as the process tree records it: an exit's
/// status byte, or the signal Linux's `traps.c` kills it with for a fault
/// (ARCHITECTURE section 6; M8 delivers those as signals).
fn exit_status(exit: &UserExit) -> ExitStatus {
    let signal = match *exit {
        UserExit::Exited(code) => return ExitStatus::exited(code),
        UserExit::Fault { vector, .. } => match vector {
            0 | 9 | 16 | 19 => Signal::FPE,
            1 | 3 => Signal::TRAP,
            6 => Signal::ILL,
            11 | 12 | 17 => Signal::BUS,
            _ => Signal::SEGV,
        },
        UserExit::NonCanonicalRip(_) => Signal::SEGV,
        UserExit::BusError { .. } => Signal::BUS,
    };
    ExitStatus::Killed { signal, core: false }
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
    exe_path: Option<String>,
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
    let thread = Arc::new(UserThread::new(Arc::clone(&process), current_kernel_tid()));
    let spawned = PROCS.lock().link_root(&process, &thread);
    let pid = spawned.unwrap_or_else(|| panic!("no pid is free for a new process"));
    let prev = sched::set_current_user_thread(Some(thread));
    debug_assert!(prev.is_none(), "a Linux process registered twice on thread {}", current_kernel_tid());
    pid
}

/// What a process's takedown hands back: its pid, how its run ended, and
/// what its console captured (`ConsoleIo::Capture`).
pub struct Ended {
    pub pid: u32,
    pub exit: Option<UserExit>,
    pub stdout: Vec<u8>,
}

/// Take the current thread's process down after ring 3 exits
/// (`take_current_if_any`): panics if it has none.
pub fn take_current() -> Ended {
    take_current_if_any().expect("no Linux process to take down after ring-3 exit")
}

/// How long the takedown waits between tries of an exit the heap refused
/// (`ExitError::NoMem`).
const EXIT_RETRY_NS: u64 = 1_000_000;

/// Take the current thread's process down, if it has one (`None` on a user
/// thread without one; panics on a kernel thread): the one way a process is
/// taken down ([`take_current`] and [`RegisteredProcess`] both come here).
/// The process must have no other thread. B1's order, after the design's
/// exit steps 2 and 3:
/// 1. the thread leaves its scheduler state, and its CPU leaves the
///    address space (`sched::detach_user_space`: `CR3` back on the
///    kernel's tables);
/// 2. the process gives up its address space (torn down at its last user),
///    its descriptors (each closed, its locks released as `close` does,
///    once no other process shares the table) and its working directory;
/// 3. the thread leaves `PROCS`, and the process exits in the tree with
///    how its run ended (`SIGKILL` if nothing was recorded), pid 1 its
///    reaper, leaving `PROCS` once the tree releases it.
fn take_current_if_any() -> Option<Ended> {
    let thread = sched::set_current_user_thread(None)?;
    drop(sched::detach_user_space());
    let process = Arc::clone(thread.process());
    let pid = process.pid();
    let exit = process.exit.lock().take();
    process.release_resources();
    let status = exit.as_ref().map_or(ExitStatus::Killed { signal: Signal::KILL, core: false }, exit_status);
    let mut last_warned: Option<u64> = None;
    let (unlinked, gone) = loop {
        let mut t = PROCS.lock();
        // Unlinked once: a retry finds it gone.
        let unlinked = t.threads.contains_key(&thread.tid()).then(|| t.unlink_thread(&thread));
        debug_assert!(
            t.tree.threads(pid).is_empty() && !t.threads.values().any(|th| Arc::ptr_eq(th.process(), &process)),
            "pid {pid} taken down with other threads still live"
        );
        match t.exit_in_tree(pid, status) {
            Ok(gone) => {
                // It left PROCS exactly when the tree released it (a root
                // does; C1's zombie waiting for its parent's wait doesn't).
                debug_assert_eq!(
                    gone.iter().any(|g| Arc::ptr_eq(g, &process)),
                    !t.procs.contains_key(&pid),
                    "pid {pid} left PROCS without its release, or stayed after it"
                );
                break (unlinked, gone);
            }
            Err(ExitError::NoMem) => {
                drop(t);
                drop(unlinked);
                let now = crate::time::now_ns();
                if last_warned.is_none_or(|at| now.saturating_sub(at) >= 1_000_000_000) {
                    warn!(target: "linux", "pid {pid}'s exit is waiting for memory");
                    last_warned = Some(now);
                }
                let _ = sched::sleep(EXIT_RETRY_NS);
            }
            Err(ExitError::Bug(bug)) => panic!("pid {pid}'s exit: {bug}"),
        }
    };
    drop((unlinked, gone));
    let stdout = core::mem::take(&mut process.console.lock().stdout);
    Some(Ended { pid, exit, stdout })
}

/// A process the kernel's own checks register on the current user thread,
/// taken down (`take_current_if_any`) when this drops, so an early return
/// can't leave it in `PROCS`, where it would stay in `/proc` for good. It
/// starts with `ConsoleIo::Capture`, no test syscalls, no stdin and no
/// executable path. Real runs (`user.rs`) register and take their process
/// themselves.
///
/// - Its takedown also takes the thread's CPU off its address space and
///   gives the process's user of the space up (tearing it down, as the
///   only user): after it, the thread has no user memory. Never call
///   `sched::detach_user_space` while it lives (a debug assertion there).
/// - Never call [`take_current`] while it lives: its own takedown would
///   find no process, a panic.
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

    /// Take the current thread's process down: this one's, or a panic.
    fn take(&self) -> Ended {
        let ended = take_current_if_any().unwrap_or_else(|| {
            panic!("pid {}: thread {} has no process left to take down", self.pid, current_kernel_tid());
        });
        debug_assert_eq!(ended.pid, self.pid, "the thread's process (left) isn't this RegisteredProcess's (right)");
        ended
    }
}

impl Drop for RegisteredProcess {
    fn drop(&mut self) {
        drop(self.take());
    }
}

/// Give back one descriptor's slot on `file` (`close`, exit, `dup2`
/// displacing it): `pid`'s POSIX record locks on its inode go on any close
/// (as Linux), and its `flock` locks with the last slot of the open file
/// description; if any went, the blocked lockers are woken to retry
/// ([`locks_released`]). Takes the inode's Lock 4: never call it holding
/// any spinlock (`PROCS`, `Files::table`).
fn release_fd(pid: u32, file: FileRef) {
    let last = file.dec_fd_ref() == 0;
    let id = file.id;
    let mut freed = false;
    let released = file.inode().with_locks_mut(&mut |locks| {
        freed = locks.release_posix_for_pid(pid);
        if last {
            freed |= locks.release_flock(id);
        }
    });
    // Every filesystem keeps lock state today; one that refused would keep
    // this process's locks held.
    if let Err(e) = released {
        log::error!(target: "linux", "pid {pid}: closing {}: its locks weren't released: {e:?}", Escaped(&file.path));
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
/// attempt and this wait isn't missed). No signal ends the wait (M7 has no
/// signal delivery): a caller refuses a wait that would never end
/// beforehand (`F_SETLKW`'s `EDEADLK`), if it can tell.
pub fn wait_for_lock_release(seen: u64) {
    #[cfg(feature = "ktest")]
    LOCK_WAITING.fetch_add(1, Ordering::SeqCst);
    LOCK_WAITERS.wait_until(|| LOCK_RELEASES.load(Ordering::SeqCst) != seen);
    #[cfg(feature = "ktest")]
    LOCK_WAITING.fetch_sub(1, Ordering::SeqCst);
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

/// The current process's open file on `fd` (a clone, taken and released
/// under its table's lock), or `None` if `fd` isn't open.
pub fn current_file(fd: i32) -> Option<FileRef> {
    with_current_fds(|t| t.get(fd).map(|e| Arc::clone(&e.file)))
}

/// The current process's open files on each of `fds` (clones, all taken in
/// one hold of its table's lock; `None` where one isn't open), for `poll`.
pub fn current_files(fds: &[i32]) -> Vec<Option<FileRef>> {
    with_current_fds(|t| fds.iter().map(|&fd| t.get(fd).map(|e| Arc::clone(&e.file))).collect())
}

/// Whether the current process has a free descriptor slot. `openat` checks
/// before it opens, so a full table is `-EMFILE` before any file is created
/// (Linux reserves the slot; this only checks: from M8, another thread could
/// fill the table between this check and the install, after an `O_CREAT`).
pub fn current_has_free_fd() -> bool {
    with_current_fds(|t| t.open_count() < lumen_vfs::MAX_FDS)
}

/// Put `file`, a fresh open file description (its `fd_refs` 1, for this
/// slot), in the current process's lowest free slot: the descriptor, or
/// `EMFILE`.
pub fn install_current_fd(file: KernelFile, cloexec: bool) -> Result<i32, VfsError> {
    let file = Arc::new(file);
    // A clone goes in, so a refused one drops under the table's lock without
    // being the last reference; `file` drops after the lock is released.
    with_current_fds(|t| t.alloc(Arc::clone(&file), cloexec))
}

/// Put `file`, a fresh open file description, on descriptor `fd` of the
/// current process, closing what was there (its locks released outside the
/// table's lock). `EBADF` for an fd outside `0..lumen_vfs::MAX_FDS`:
/// procfs builds `/proc/<pid>/fd/<fd>`'s inode number from it and relies on
/// that range.
pub fn insert_current_fd(fd: i32, file: KernelFile) -> Result<(), VfsError> {
    place_current_fd(fd, Arc::new(file), false)
}

/// Make descriptor `new` share `old`'s open file description, close-on-exec
/// as `cloexec` says (`dup2`/`dup3`'s slot work, M8): its `fd_refs` counts
/// the new slot, and what `new` held is closed. `EBADF` if `old` isn't open
/// or `new` is out of range; `old == new` changes nothing.
///
/// `old`'s lookup and `new`'s install are two holds of the table's lock:
/// from M8, another thread could close `old` between them (the file stays
/// alive through this call's reference, so the dup still succeeds).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // M8's `dup2`/`dup3` call it; the ktests until then
pub fn dup_current_fd(old: i32, new: i32, cloexec: bool) -> Result<i32, VfsError> {
    let file = current_file(old).ok_or(VfsError::BadF)?;
    if old == new {
        return Ok(new);
    }
    file.inc_fd_ref();
    match place_current_fd(new, Arc::clone(&file), cloexec) {
        Ok(()) => Ok(new),
        Err(e) => {
            // The slot was never filled: give its count back.
            file.dec_fd_ref();
            Err(e)
        }
    }
}

/// Make the lowest free descriptor at or above `min` share `old`'s open
/// file description, close-on-exec as `cloexec` says (`fcntl(F_DUPFD)`,
/// `F_DUPFD_CLOEXEC`, `dup`): the descriptor. `EBADF` if `old` isn't open,
/// `EINVAL` if `min` is out of range, `EMFILE` if no slot at or above it is
/// free. Two holds of the table's lock, as [`dup_current_fd`].
pub fn dup_current_fd_from(old: i32, min: i32, cloexec: bool) -> Result<i32, VfsError> {
    let file = current_file(old).ok_or(VfsError::BadF)?;
    file.inc_fd_ref();
    // A clone goes in, so a refused one never drops the last reference
    // under the table's lock.
    let placed = with_current_fds(|t| t.alloc_from(min, Arc::clone(&file), cloexec));
    if placed.is_err() {
        file.dec_fd_ref();
    }
    placed
}

/// Put `file` (its `fd_refs` already counting this slot) on `fd`, releasing
/// what it displaces outside the table's lock.
fn place_current_fd(fd: i32, file: FileRef, cloexec: bool) -> Result<(), VfsError> {
    let process = current_process();
    let displaced = process.with_files(|t| t.insert_at(fd, Arc::clone(&file), cloexec));
    if let Some(old) = displaced.expect("a running process has its descriptors")? {
        release_fd(process.pid(), old.file);
    }
    Ok(())
}

/// Close descriptor `fd` of the current process (`close`): `EBADF` if it
/// isn't open. The entry is taken out under the table's lock and released
/// after.
pub fn close_current_fd(fd: i32) -> Result<(), VfsError> {
    let process = current_process();
    let removed = process.with_files(|t| t.remove(fd)).expect("a running process has its descriptors");
    release_fd(process.pid(), removed?.file);
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
    with_current_fs(|fs| (Arc::clone(fs.cwd.inode()), Arc::clone(fs.cwd.path())))
}

/// Make `cwd` the current process's working directory; the old one drops
/// after its lock is released (`chdir`, `fchdir`).
pub fn set_current_cwd(cwd: Cwd) {
    let old = with_current_fs(|fs| core::mem::replace(&mut fs.cwd, cwd));
    drop(old);
}

/// The current process's file mode creation mask.
pub fn current_umask() -> u16 {
    with_current_fs(|fs| fs.umask)
}

/// Set the current process's file mode creation mask to `new`'s `0o777`
/// bits (`umask`): the mask it replaces.
pub fn swap_current_umask(new: u16) -> u16 {
    with_current_fs(|fs| core::mem::replace(&mut fs.umask, new & 0o777))
}

/// The current process's disposition of `signal`.
pub fn current_action(signal: Signal) -> KernelSigaction {
    current_process().signal.lock().action(signal)
}

/// Install `action` as the current process's disposition of `signal`
/// ([`SignalState::set_action`]): the one it replaces, or `None` for
/// `SIGKILL` and `SIGSTOP`.
pub fn set_current_action(signal: Signal, action: KernelSigaction) -> Option<KernelSigaction> {
    current_process().signal.lock().set_action(signal, action)
}

/// The signals the current thread blocks.
pub fn current_blocked() -> Blocked {
    let thread = current_thread();
    let mut signal = thread.process().signal.lock();
    signal.thread_mut(thread.tid()).blocked
}

/// Make `blocked` the signals the current thread blocks.
pub fn set_current_blocked(blocked: Blocked) {
    let thread = current_thread();
    let mut signal = thread.process().signal.lock();
    signal.thread_mut(thread.tid()).blocked = blocked;
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
    PROCS.lock().procs.keys().copied().filter(|&pid| pid != INIT_PID).collect()
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
    PROCS.lock().threads.get(&pid).map(|t| t.kernel_thread)
}

/// Snapshot the executable path recorded for `pid`, if any.
pub fn process_exe_path(pid: u32) -> Option<String> {
    active(pid)?.exe_path.lock().clone()
}

/// Set the executable's path on the current process, as given (not
/// resolved: symlinks and `..` stay).
pub fn set_current_exe_path(path: &str) {
    let Some(thread) = try_current_thread() else {
        return;
    };
    let path = String::from(path);
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
    if unlinked && !path.ends_with(b" (deleted)") {
        path.extend_from_slice(b" (deleted)");
    }
    Some((Some(inode), path))
}

/// Record the process's terminal [`UserExit`] (the first one recorded
/// wins) and return `(pid, allow_test_syscalls)`.
pub fn record_exit(exit: UserExit) -> (u32, bool) {
    let thread = current_thread();
    let process = thread.process();
    let _ = process.exit.lock().get_or_insert(exit);
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
    let t = PROCS.lock();
    let mut rows: BTreeMap<u32, PsRow> = BTreeMap::new();
    for &pid in t.procs.keys() {
        let ppid = t.tree.parent(pid).expect("a process in PROCS is in the tree").unwrap_or(0);
        let state = t.tree.state(pid).expect("a process in PROCS is in the tree");
        rows.insert(pid, PsRow { pid, ppid, threads: Vec::new(), state });
    }
    for (&tid, thread) in &t.threads {
        let pid = thread.process().pid();
        let row = rows.get_mut(&pid).expect("a thread's process is in PROCS");
        let entry = (tid, thread.kernel_thread);
        if tid == pid { row.threads.insert(0, entry) } else { row.threads.push(entry) }
    }
    drop(t);
    rows.into_values().collect()
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
        ConsoleIo::Vt(vt) => write_vt_utf8(vt, bytes),
    }
}

/// Stream `bytes` onto virtual terminal `vt`, preserving incomplete trailing
/// multi-byte UTF-8 sequences across chunk/write boundaries and replacing only
/// genuinely invalid byte sequences with U+FFFD.
fn write_vt_utf8(vt: usize, bytes: &[u8]) {
    let (mut tail, mut tail_len) = with_current_console(|proc| {
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

    with_current_console(|proc| {
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
        ConsoleIo::Serial => with_current_console(|c| c.has_unread()) || serial_in::available(),
        ConsoleIo::Vt(vt) => {
            if with_current_console(|c| c.has_unread()) {
                return true;
            }
            while let Some(key) = crate::vt::read_key(vt) {
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
        ConsoleIo::Capture => with_current_console(|proc| {
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
    let first = with_current_console(ConsoleState::pop_unread).unwrap_or_else(|| {
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
            with_current_console(|proc| proc.push_unread_bytes(&[0x04]));
            break;
        }
        dst[n] = if b == b'\r' { b'\n' } else { b };
        n += 1;
    }
    n
}

fn read_vt(vt: usize, dst: &mut [u8]) -> usize {
    if let Some(first) = with_current_console(ConsoleState::pop_unread) {
        if first == 0x04 {
            return 0;
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
            with_current_console(|proc| proc.push_unread_bytes(&enc[n..len]));
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
    let spawned = sched::spawn_user_on(crate::arch::cpu_index(), "ktest-process", Arc::clone(&space), move || {
        let registered = RegisteredProcess::register(stdio);
        let result = f(registered.pid());
        // Its takedown takes this thread off the space and tears it down.
        drop(registered);
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

/// Link a non-leader thread of `process` into `PROCS` with a tid from its
/// tree (`ProcTable::link_thread`), as M8's `clone` will: the ktests'
/// second tid. It runs nowhere (its scheduler thread id is 0).
#[cfg(feature = "ktest")]
pub(crate) fn add_ktest_thread(process: &Arc<Process>) -> Result<Arc<UserThread>, &'static str> {
    let thread = Arc::new(UserThread::new(Arc::clone(process), 0));
    PROCS.lock().link_thread(&thread).map_err(|_| "ProcTree::add_thread refused")?;
    Ok(thread)
}

/// Unlink a thread [`add_ktest_thread`] linked (`ProcTable::unlink_thread`).
#[cfg(feature = "ktest")]
pub(crate) fn remove_ktest_thread(thread: &Arc<UserThread>) {
    let gone = PROCS.lock().unlink_thread(thread);
    drop(gone);
}

/// `process`'s disposition of `signal` (the ktests' view of another
/// thread's process).
#[cfg(feature = "ktest")]
pub(crate) fn ktest_process_action(process: &Process, signal: Signal) -> KernelSigaction {
    process.signal.lock().action(signal)
}

/// The signals `thread` blocks (the ktests' view of another thread's mask).
#[cfg(feature = "ktest")]
pub(crate) fn ktest_thread_blocked(thread: &UserThread) -> Blocked {
    thread.process().signal.lock().thread_mut(thread.tid()).blocked
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

        // How an end is recorded in the tree (Linux's `traps.c`).
        kassert_eq!(exit_status(&UserExit::Exited(256 + 3)), ExitStatus::Exited(3));
        let bus = UserExit::BusError { rip: 0, cr2: 0 };
        kassert_eq!(exit_status(&bus), ExitStatus::Killed { signal: Signal::BUS, core: false });
        for (vector, signal) in [(14, Signal::SEGV), (11, Signal::BUS), (9, Signal::FPE), (6, Signal::ILL)] {
            let fault = UserExit::Fault { vector, error_code: 0, rip: 0, cr2: 0 };
            kassert_eq!(exit_status(&fault), ExitStatus::Killed { signal, core: false });
        }

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
            shared.leave(pid);
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
            kassert!(!PROCS.lock().tree.is_busy(tid));
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
        use crate::kassert_eq;
        // A process's run (register, takedown, exit in the tree) gives back
        // every allocation it made: the heap's live count is the same before
        // and after three runs, once one run has grown the tables' nodes. A
        // leak shows on every try; another thread's allocation meanwhile
        // only on one, so the count is retaken up to three times.
        in_test_process(|_| ())?;
        let mut counts = (0, 0);
        for _ in 0..3 {
            let before = crate::mm::heap::stats().live;
            for _ in 0..3 {
                in_test_process(|_| ())?;
            }
            counts = (before, crate::mm::heap::stats().live);
            if counts.0 == counts.1 {
                break;
            }
        }
        kassert_eq!(counts.1, counts.0);
        Ok(())
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
