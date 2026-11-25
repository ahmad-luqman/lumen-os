//! Descriptor-table and working-directory ownership.

use super::*;

/// Shared access leases cover descriptor-table operations and whole open
/// reservations. An exclusive lease orders replacement after all of them.
pub(super) struct FilesAccess {
    state: AtomicUsize,
    seq: AtomicU64,
    waiters: sched::WaitQueue,
}
const FILES_EXCLUSIVE: usize = usize::MAX;

impl FilesAccess {
    pub(super) fn new() -> Self {
        Self { state: AtomicUsize::new(0), seq: AtomicU64::new(0), waiters: sched::WaitQueue::new() }
    }

    pub(super) fn acquire(&self, exclusive: bool) -> FilesAccessGuard<'_> {
        loop {
            let seen = self.seq.load(Ordering::SeqCst);
            let state = self.state.load(Ordering::Acquire);
            if state != FILES_EXCLUSIVE && (!exclusive || state == 0) {
                let next = if exclusive { FILES_EXCLUSIVE } else { state.checked_add(1).expect("fd leases overflow") };
                if self.state.compare_exchange(state, next, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    return FilesAccessGuard { access: self, exclusive };
                }
                continue;
            }
            self.waiters.wait_until(|| self.seq.load(Ordering::SeqCst) != seen);
        }
    }

    pub(super) fn read(&self) -> FilesAccessGuard<'_> {
        self.acquire(false)
    }
    pub(super) fn write(&self) -> FilesAccessGuard<'_> {
        self.acquire(true)
    }

    pub(super) fn release(&self, exclusive: bool) {
        if exclusive {
            assert_eq!(self.state.swap(0, Ordering::AcqRel), FILES_EXCLUSIVE);
        } else {
            let before = self.state.fetch_sub(1, Ordering::AcqRel);
            assert!(before > 0 && before != FILES_EXCLUSIVE);
        }
        self.seq.fetch_add(1, Ordering::SeqCst);
        self.waiters.wake_all();
    }
}

pub(super) struct FilesAccessGuard<'a> {
    access: &'a FilesAccess,
    exclusive: bool,
}
impl Drop for FilesAccessGuard<'_> {
    fn drop(&mut self) {
        self.access.release(self.exclusive);
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
    pub(super) fn new(table: FdTable<FileRef>) -> Self {
        Self { table: SpinLock::new(table), users: AtomicUsize::new(1) }
    }

    /// Copy open descriptors, preserving cloexec and counting each slot.
    /// Reservations belong to the original table and are never inherited.
    pub(super) fn fork_copy(&self) -> Self {
        let table = self.table.lock();
        let copy = table.clone();
        for (_, entry) in copy.iter_open() {
            entry.file.inc_fd_ref();
        }
        Self::new(copy)
    }

    pub(super) fn with_table<R>(&self, f: impl FnOnce(&mut FdTable<FileRef>) -> R) -> R {
        f(&mut self.table.lock())
    }

    pub(super) fn reserve(self: &Arc<Self>, owner: Arc<Process>, from: i32) -> Result<ReservedFd, VfsError> {
        let reservation = self.table.lock().reserve(from)?;
        Ok(ReservedFd { files: Arc::clone(self), owner, reservation: Some(reservation) })
    }

    /// Another process shares it (C1's `clone(CLONE_FILES)`).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // `procs_lookup_and_lifetime`; C1's `clone` next
    pub(super) fn share(self: &Arc<Self>) -> Arc<Self> {
        let before = self.users.fetch_add(1, Ordering::AcqRel);
        assert_ne!(before, 0, "a descriptor table shared after its last user left");
        Arc::clone(self)
    }

    /// Process `pid` stops sharing it: the last one closes every descriptor
    /// (`release_fd`, outside every spinlock). Panics if no process shared
    /// it. Takes inodes' locks: never call it holding a spinlock.
    pub(super) fn leave(self: Arc<Self>, pid: u32) {
        let before = self.users.fetch_sub(1, Ordering::AcqRel);
        assert_ne!(before, 0, "pid {pid} left a descriptor table nobody shared");
        if before == 1 {
            let entries = {
                let mut table = self.table.lock();
                debug_assert_eq!(table.reserved_count(), 0, "last user left with an open in flight");
                table.drain_all()
            };
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

impl Drop for Files {
    fn drop(&mut self) {
        let users = *self.users.get_mut();
        debug_assert_eq!(self.table.get_mut().reserved_count(), 0, "dropped table has reserved slots");
        debug_assert_eq!(
            self.table.get_mut().open_count(),
            0,
            "a descriptor table dropped with {users} users and descriptors open (no last `leave`)"
        );
    }
}

/// A process's working directory and umask, shared by the processes
/// `clone(CLONE_FS)` links (M8). Only its owning processes' threads may
/// clone this Arc; procfs must not retain it. Thus its last Cwd is always
/// dropped in thread context, outside every spinlock.
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
    pub(super) fn with_inner<R>(&self, f: impl FnOnce(&mut FsInner) -> R) -> R {
        f(&mut self.inner.lock())
    }

    pub(super) fn new(cwd: Cwd) -> Self {
        Self { inner: SpinLock::new(FsInner { cwd, umask: DEFAULT_UMASK }) }
    }
    pub(super) fn fork_copy(&self) -> Result<Self, VfsError> {
        let (inode, path, umask) = {
            let inner = self.inner.lock();
            (Arc::clone(inner.cwd.inode()), Arc::clone(inner.cwd.path()), inner.umask)
        };
        let cwd = Cwd::new(OpenGuard::try_new(inode)?, &path)?;
        Ok(Self { inner: SpinLock::new(FsInner { cwd, umask }) })
    }
}

impl FsInner {
    pub(super) fn cwd(&self) -> &Cwd {
        &self.cwd
    }
    pub(super) fn umask(&self) -> u16 {
        self.umask
    }

    /// Make `new`'s `0o777` bits the umask: the mask it replaces.
    pub(super) fn swap_umask(&mut self, new: u16) -> u16 {
        core::mem::replace(&mut self.umask, new & 0o777)
    }

    /// Make `cwd` the working directory: the one it replaces, for the
    /// caller to drop with the lock released.
    pub(super) fn swap_cwd(&mut self, cwd: Cwd) -> Cwd {
        core::mem::replace(&mut self.cwd, cwd)
    }
}

/// A slot kept in the exact `Files` table that reserved it. Any failed
/// open cancels it on drop; installation consumes it. No table lock is
/// held while the caller opens the file or copies user memory.
pub struct ReservedFd {
    owner: Arc<Process>,
    files: Arc<Files>,
    reservation: Option<FdReservation>,
}

impl ReservedFd {
    /// The reserved descriptor number (not open until installed).
    #[cfg(feature = "ktest")]
    pub fn fd(&self) -> i32 {
        self.reservation.as_ref().expect("unconsumed reservation").fd()
    }

    /// Install a fresh description, whose one FD reference becomes this
    /// slot. The reserved descriptor cannot have been taken by a dup.
    pub fn install(mut self, file: KernelFile, cloexec: bool) -> i32 {
        let file = Arc::new(file);
        let r = self.reservation.take().expect("unconsumed reservation");
        self.files.table.lock().install(r, file, cloexec)
    }
}

impl Drop for ReservedFd {
    fn drop(&mut self) {
        if let Some(r) = self.reservation.take() {
            self.files.table.lock().cancel(r);
        }
        self.owner.files_access.release(false);
    }
}
