//! Descriptor-table and working-directory ownership.

use super::*;

/// Shared access leases cover descriptor-table operations and whole open
/// reservations. An exclusive lease orders replacement after all of them.
/// Nested shared leases are required: openat holds one while with_files
/// may take another. Writers have no priority and can starve under readers.
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

    fn acquire(&self, exclusive: bool) {
        loop {
            let seen = self.seq.load(Ordering::SeqCst);
            let state = self.state.load(Ordering::Acquire);
            if state != FILES_EXCLUSIVE && (!exclusive || state == 0) {
                let next = if exclusive { FILES_EXCLUSIVE } else { state.checked_add(1).expect("fd leases overflow") };
                if self.state.compare_exchange(state, next, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    return;
                }
                continue;
            }
            self.waiters.wait_until(|| self.seq.load(Ordering::SeqCst) != seen);
        }
    }

    pub(super) fn read(&self) -> FilesAccessGuard<'_> {
        self.acquire(false);
        FilesAccessGuard { access: self, exclusive: false }
    }
    pub(super) fn write(&self) -> FilesAccessGuard<'_> {
        self.acquire(true);
        FilesAccessGuard { access: self, exclusive: true }
    }

    fn release(&self, exclusive: bool) {
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
    table: SpinLock<Shared>,
    /// Shared by all table users: descriptor removal/release versus leave.
    close_access: FilesAccess,
}

/// Descriptor slots and their process owners change under the same leaf lock.
struct Shared {
    fds: FdTable<FileRef>,
    /// Zero reserves membership for a process not linked to a pid yet.
    members: Vec<u32>,
}

impl Files {
    pub(super) fn new(fds: FdTable<FileRef>) -> Self {
        Self { table: SpinLock::new(Shared { fds, members: alloc::vec![0] }), close_access: FilesAccess::new() }
    }

    /// Reserve an empty exec table before the point of no return.
    pub(super) fn prepare_exec() -> Result<Self, VfsError> {
        let fds = FdTable::try_preallocated()?;
        let mut members = Vec::new();
        members.try_reserve_exact(1).map_err(|_| VfsError::NoMem)?;
        Ok(Self { table: SpinLock::new(Shared { fds, members }), close_access: FilesAccess::new() })
    }

    /// Fill a prepared empty table without allocation, preserving open descriptions and CLOEXEC.
    pub(super) fn copy_exec_into(&self, prepared: &Self, member: u32) {
        let source = self.table.lock();
        let mut target = prepared.table.lock();
        assert!(target.members.is_empty() && target.fds.open_count() == 0);
        for (fd, entry) in source.fds.iter_open() {
            target.fds.insert_at(fd, Arc::clone(&entry.file), entry.cloexec).expect("preallocated slot");
            entry.file.inc_fd_ref();
        }
        target.members.push(member);
    }

    /// Copy open descriptors and cloexec, counting each slot for member.
    /// Reservations belong to the original table and are never inherited.
    pub(super) fn fork_copy(&self, member: u32) -> Self {
        let table = self.table.lock();
        let copy = table.fds.clone();
        for (_, entry) in copy.iter_open() {
            entry.file.inc_fd_ref();
        }
        Self {
            table: SpinLock::new(Shared { fds: copy, members: alloc::vec![member] }),
            close_access: FilesAccess::new(),
        }
    }

    pub(super) fn with_table<R>(&self, f: impl FnOnce(&mut FdTable<FileRef>) -> R) -> R {
        f(&mut self.table.lock().fds)
    }

    fn reserve(self: &Arc<Self>, lease: FilesLease, limit: usize) -> Result<ReservedFd, VfsError> {
        let reservation = self.table.lock().fds.reserve_below(limit)?;
        Ok(ReservedFd { files: Arc::clone(self), _lease: lease, reservation: Some(reservation) })
    }

    /// Hold from before removing a descriptor until its inode lock release
    /// finishes. The caller must drop this lease before calling leave.
    pub(super) fn closing(&self) -> FilesAccessGuard<'_> {
        self.close_access.read()
    }

    /// Reserve membership for another process before its pid is linked.
    pub(super) fn share(self: &Arc<Self>) -> Arc<Self> {
        let mut table = self.table.lock();
        assert!(!table.members.is_empty(), "a descriptor table shared after its last user left");
        table.members.push(0);
        Arc::clone(self)
    }

    /// Replace one reserved membership with its assigned pid, without allocating.
    pub(super) fn enrol(&self, pid: u32) {
        assert_ne!(pid, 0);
        let mut table = self.table.lock();
        *table.members.iter_mut().find(|member| **member == 0).expect("unlinked Files member") = pid;
    }

    /// Release all current members' POSIX locks while the caller holds the
    /// inode lock. Membership cannot outlive reaping/pid reuse, and no new
    /// lock on this inode can be acquired during this allocation-free scan.
    pub(super) fn release_members(&self, locks: &mut lumen_vfs::AdvisoryLocks) -> bool {
        let table = self.table.lock();
        let mut freed = false;
        for &pid in &table.members {
            if pid != 0 {
                freed |= locks.release_posix_for_pid(pid);
            }
        }
        freed
    }

    /// Release the departing process's POSIX locks even when other users
    /// keep this table. Snapshot descriptions under the leaf, then take inode
    /// locks with no spinlock held. Unshare does not perform this exit sweep.
    pub(super) fn release_posix_for_pid(&self, pid: u32) {
        // An unlinked process never acquired locks.
        if pid == 0 {
            return;
        }
        let files: Vec<_> = self.table.lock().fds.iter_open().map(|(_, entry)| Arc::clone(&entry.file)).collect();
        for file in files {
            release_locks(&file, LockOwners::Pid(pid), false);
        }
    }

    /// Remove exactly one membership; the last member drains descriptors
    /// and flock outside the table lock. pid is only for member removal and
    /// the panic message: exit releases its POSIX locks before calling leave.
    /// Takes inode locks: never call with a spinlock or closing lease held.
    pub(super) fn leave(self: Arc<Self>, pid: u32) {
        // Either an in-flight close scans this pid before leave removes it,
        // or leave wins before removal and exit's preceding sweep saw the fd.
        // This lease is table-wide; a process's files_access only orders swaps.
        let _exclusive = self.close_access.write();
        let entries = {
            let mut table = self.table.lock();
            let member = table.members.iter().position(|&member| member == pid);
            debug_assert!(member.is_some(), "pid {pid} left a descriptor table it wasn't using");
            let Some(member) = member else { return };
            table.members.swap_remove(member);
            if !table.members.is_empty() {
                return;
            }
            debug_assert_eq!(table.fds.reserved_count(), 0, "last user left with an open in flight");
            table.fds.drain_all()
        };
        for entry in entries {
            release_fd(&self, entry.file);
        }
    }

    /// The processes sharing it, including reserved unlinked members.
    pub fn users(&self) -> usize {
        self.table.lock().members.len()
    }

    /// How many descriptors are open.
    #[cfg(feature = "ktest")]
    pub fn open_count(&self) -> usize {
        self.table.lock().fds.open_count()
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        let table = self.table.get_mut();
        assert!(table.members.is_empty(), "Files dropped before its last leave");
        debug_assert_eq!(table.fds.reserved_count(), 0, "dropped table has reserved slots");
        debug_assert_eq!(table.fds.open_count(), 0, "dropped table has open descriptors");
    }
}

impl Process {
    /// Reserve the lowest free slot below this process's soft `RLIMIT_NOFILE`
    /// while its owned lease prevents table replacement (`EMFILE` if none is free).
    pub(super) fn reserve_fd(self: Arc<Self>) -> Result<ReservedFd, VfsError> {
        let lease = FilesLease::read(self);
        let limit = lease.owner.nofile_soft();
        let files = lease.owner.files.lock().clone().expect("a running process has its descriptors");
        files.reserve(lease, limit)
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
/// held while the caller opens the file or copies user memory. Its owned
/// shared files_access lease prevents table replacement until install/cancel.
pub struct ReservedFd {
    _lease: FilesLease,
    files: Arc<Files>,
    reservation: Option<FdReservation>,
}

impl ReservedFd {
    /// The reserved descriptor number (not open until installed).
    pub fn fd(&self) -> i32 {
        self.reservation.as_ref().expect("unconsumed reservation").fd()
    }

    /// Install a fresh description, whose one FD reference becomes this
    /// slot. The reserved descriptor cannot have been taken by a dup.
    pub fn install(self, file: KernelFile, cloexec: bool) -> i32 {
        self.install_inflight(crate::net::sock::InFlightFile::from_fresh(file), cloexec)
    }

    /// Install an [`crate::net::sock::InFlightFile`], moving its counted FD/end
    /// reference directly into this slot with no counter traffic.
    pub fn install_inflight(mut self, file: crate::net::sock::InFlightFile, cloexec: bool) -> i32 {
        let file = file.into_file_ref();
        let r = self.reservation.take().expect("unconsumed reservation");
        self.files.table.lock().fds.install(r, file, cloexec)
    }
}

impl Drop for ReservedFd {
    fn drop(&mut self) {
        if let Some(r) = self.reservation.take() {
            self.files.table.lock().fds.cancel(r);
        }
    }
}

/// Owns one shared access count independently of its creator's stack.
struct FilesLease {
    owner: Arc<Process>,
}

impl FilesLease {
    fn read(owner: Arc<Process>) -> Self {
        owner.files_access.acquire(false);
        Self { owner }
    }
}

impl Drop for FilesLease {
    fn drop(&mut self) {
        self.owner.files_access.release(false);
    }
}
