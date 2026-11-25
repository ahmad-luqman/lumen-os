//! Open file descriptions (`OpenFile`) and per-process 1024-entry `FdTable`.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::{RefCell, RefMut};
use core::fmt;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::error::VfsError;
use crate::fs::{OpenGuard, VfsInode};
use crate::ops::Opened;
use crate::types::{AccessMode, InodeType, OpenFlags};
use crate::{MAX_FDS, next_open_file_id};

/// Synchronization abstraction for `OpenFile::pos` (Lock 0),
/// `OpenFile::offset` and `dir_cursor` (Lock 2)
/// and, through a [`LockFamily`], the `VfsTables` (Lock 9).
///
/// The kernel implements this over `SleepMutex<T>`; single-threaded host tests
/// can use [`LocalLock<T>`] with zero `unsafe`.
pub trait VfsLock<T> {
    type Guard<'a>: Deref<Target = T> + DerefMut
    where
        Self: 'a,
        T: 'a;

    fn new(val: T) -> Self;
    fn lock(&self) -> Self::Guard<'_>;
    fn get_mut(&mut self) -> &mut T;
}

/// A family of [`VfsLock`]s, one lock type for any `T`: what
/// [`VfsTables`](crate::VfsTables) is generic over (#264). The kernel's is
/// `vfs::KernelLocks` (`SleepMutex`); the host tests' is `TestLocks`, a
/// `std` mutex recording which Lock-9 tables a thread holds and aborting
/// the test binary on an out-of-order or repeated take.
pub trait LockFamily {
    type Lock<T>: VfsLock<T>;
}

/// Zero-`unsafe` single-threaded `VfsLock` backed by `core::cell::RefCell`.
#[derive(Debug, Default)]
pub struct LocalLock<T>(RefCell<T>);

impl<T> VfsLock<T> for LocalLock<T> {
    type Guard<'a>
        = RefMut<'a, T>
    where
        T: 'a;

    fn new(val: T) -> Self {
        Self(RefCell::new(val))
    }

    fn lock(&self) -> Self::Guard<'_> {
        self.0.borrow_mut()
    }

    fn get_mut(&mut self) -> &mut T {
        self.0.get_mut()
    }
}

/// Unique identifier for an open file description (used as the `flock(2)` key).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpenFileId(pub u64);

/// Open file description shared across `dup` / `F_DUPFD*` / `fork` descriptors.
pub struct OpenFile<L: VfsLock<u64> = LocalLock<u64>> {
    pub id: OpenFileId,
    /// The path it was opened by, its bytes (#268): what `/proc/<pid>/fd`
    /// reads back, never a lossy copy (that would name another file).
    pub path: Arc<[u8]>,
    access_mode: AccessMode,
    is_path: bool,
    status_flags: AtomicU32,
    fd_refs: AtomicU32,
    /// Serializes a whole position-based call, including user copies (Lock 0).
    /// Its value is unused; positional I/O and streams do not take it.
    pub(crate) pos: L,
    pub offset: L,
    /// A directory's position (Lock 2): the `readdir` cursor `getdents64`
    /// resumes at, `0` at the start; `lseek` sets it. Opaque: `0` or a
    /// [`DirEntry::next_cursor`](crate::DirEntry::next_cursor) the
    /// directory returned, or whatever `lseek` was given (any value: the
    /// contract on [`VfsInode::readdir`](crate::VfsInode::readdir)).
    pub dir_cursor: L,
    open_guard: OpenGuard,
}

impl<L: VfsLock<u64>> OpenFile<L> {
    /// Construct a new open file description with `fd_refs = 1` on an inode
    /// and flags no open checked, taking a checked open reference
    /// ([`OpenGuard::try_new`]).
    ///
    /// **Not for `openat`:** `flags` get none of the open's checks (`EROFS`,
    /// `EISDIR`, `O_PATH`'s rules), so nothing stops a caller from building
    /// an `O_RDWR` descriptor on an inode looked up read-only. File
    /// descriptors use [`open_inode_guarded`](crate::ops::open_inode_guarded)
    /// and [`OpenFile::from_opened`], which also can't lose the race below.
    ///
    /// `ENOENT` if `inode` was freed by an unlink since it was looked up
    /// (between [`open_inode`](crate::ops::open_inode) returning and this
    /// call, say): only an inode that reports `nlink >= 1`, or is detached,
    /// can be opened. May sleep, as `try_new` does (the inode's Lock 4 and,
    /// on ext4 and tmpfs, Lock 5 through `stat()`): never call it holding
    /// this inode's Lock 4 (or another inode's Lock 4 out of `lock_id`
    /// order), any of Locks 5 to 9, or a spinlock.
    pub fn new_unchecked_flags(inode: Arc<dyn VfsInode>, path: Arc<[u8]>, flags: OpenFlags) -> Result<Self, VfsError> {
        flags.access_mode()?;
        Self::build(OpenGuard::try_new(inode)?, path, flags)
    }

    /// Construct a new open file description with `fd_refs = 1` from what
    /// [`open_inode_guarded`](crate::ops::open_inode_guarded) opened: it
    /// takes that open's reference over (no second count) and its checked
    /// flags. Never sleeps.
    pub fn from_opened(opened: Opened) -> Result<Self, VfsError> {
        let (guard, path, flags) = opened.into_parts();
        Self::build(guard, path, flags)
    }

    fn build(guard: OpenGuard, path: Arc<[u8]>, flags: OpenFlags) -> Result<Self, VfsError> {
        let access_mode = flags.access_mode()?;
        let is_path = flags.is_path();
        let status_bits = if is_path { 0 } else { flags.status_bits() };
        Ok(Self {
            id: next_open_file_id(),
            path,
            access_mode,
            is_path,
            status_flags: AtomicU32::new(status_bits),
            fd_refs: AtomicU32::new(1),
            pos: L::new(0),
            offset: L::new(0),
            dir_cursor: L::new(0),
            open_guard: guard,
        })
    }

    /// Hold the position across a whole read, write, vector I/O, seek or
    /// directory listing of a regular file (including procfs) or directory.
    /// Streams and devices have no atomic position and return no guard.
    /// Take this before other VFS locks; never for pread or pwrite.
    #[must_use = "hold the position guard for the whole call"]
    pub fn lock_pos(&self) -> Option<L::Guard<'_>> {
        matches!(self.inode().inode_type(), InodeType::File | InodeType::Dir).then(|| self.pos.lock())
    }

    /// The inode this description is open on: always the one its open
    /// reference ([`OpenFile::open_guard`]) holds, so a description can
    /// never point at an inode it doesn't keep open (#264).
    pub fn inode(&self) -> &Arc<dyn VfsInode> {
        self.open_guard.inode()
    }

    /// The open reference this description holds on its inode: clone it
    /// for another holder (`fchdir`'s cwd, a file mapping) that must not
    /// be refused the way a fresh [`OpenGuard::try_new`] can be: a procfs
    /// inode whose process has ended (its `stat()` says `ENOENT`).
    pub fn open_guard(&self) -> &OpenGuard {
        &self.open_guard
    }

    /// The access mode the open checked (meaningless for `O_PATH`).
    pub fn access_mode(&self) -> AccessMode {
        self.access_mode
    }

    /// True if opened with `O_PATH`.
    pub fn is_path(&self) -> bool {
        self.is_path
    }

    /// Current mutable status flags (`O_APPEND | O_NONBLOCK`).
    pub fn status_flags(&self) -> u32 {
        self.status_flags.load(Ordering::Relaxed)
    }

    /// Update `O_APPEND | O_NONBLOCK` (`fcntl(F_SETFL)`). Returns `-EBADF` on `O_PATH`.
    pub fn set_status_flags(&self, flags: u32) -> Result<(), VfsError> {
        if self.is_path {
            return Err(VfsError::BadF);
        }
        let masked = flags & (OpenFlags::O_APPEND | OpenFlags::O_NONBLOCK);
        self.status_flags.store(masked, Ordering::Relaxed);
        Ok(())
    }

    /// Toggle `O_NONBLOCK` (`ioctl(FIONBIO)`). Returns `-EBADF` on `O_PATH`.
    pub fn set_nonblock(&self, nonblock: bool) -> Result<(), VfsError> {
        if self.is_path {
            return Err(VfsError::BadF);
        }
        if nonblock {
            self.status_flags.fetch_or(OpenFlags::O_NONBLOCK, Ordering::Relaxed);
        } else {
            self.status_flags.fetch_and(!OpenFlags::O_NONBLOCK, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Flags returned by `fcntl(F_GETFL)`.
    pub fn get_fl(&self) -> u32 {
        if self.is_path { OpenFlags::O_PATH } else { self.access_mode.to_raw() | self.status_flags() }
    }

    /// Return `Ok(())` if not opened with `O_PATH`, else `Err(BadF)`.
    pub fn require_non_path(&self) -> Result<(), VfsError> {
        if self.is_path { Err(VfsError::BadF) } else { Ok(()) }
    }

    /// True if readable (`O_RDONLY` or `O_RDWR`, and not `O_PATH`).
    pub fn is_readable(&self) -> bool {
        !self.is_path && matches!(self.access_mode, AccessMode::ReadOnly | AccessMode::ReadWrite)
    }

    /// True if writable (`O_WRONLY` or `O_RDWR`, and not `O_PATH`).
    pub fn is_writable(&self) -> bool {
        !self.is_path && matches!(self.access_mode, AccessMode::WriteOnly | AccessMode::ReadWrite)
    }

    /// Return `Ok(())` if readable, else `Err(BadF)`.
    pub fn require_readable(&self) -> Result<(), VfsError> {
        if self.is_readable() { Ok(()) } else { Err(VfsError::BadF) }
    }

    /// Return `Ok(())` if writable, else `Err(BadF)`.
    pub fn require_writable(&self) -> Result<(), VfsError> {
        if self.is_writable() { Ok(()) } else { Err(VfsError::BadF) }
    }

    /// Increment descriptor reference count (called when duplicating into an FD slot).
    pub fn inc_fd_ref(&self) {
        self.fd_refs.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement descriptor reference count, returning the new count.
    ///
    /// When this reaches `0`, the caller must release `flock(2)` locks for
    /// `self.id` outside every spinlock (the kernel's descriptor-table lock
    /// among them).
    pub fn dec_fd_ref(&self) -> u32 {
        self.fd_refs.fetch_sub(1, Ordering::AcqRel) - 1
    }

    /// Current FD reference count.
    pub fn fd_ref_count(&self) -> u32 {
        self.fd_refs.load(Ordering::Acquire)
    }
}

impl<L: VfsLock<u64>> fmt::Debug for OpenFile<L> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenFile")
            .field("id", &self.id)
            .field("dev", &self.inode().dev_id())
            .field("ino", &self.inode().ino())
            .field("path", &crate::Escaped(&self.path))
            .field("access_mode", &self.access_mode)
            .field("is_path", &self.is_path)
            .finish()
    }
}

/// Entry in a per-process [`FdTable`].
#[derive(Clone, Debug)]
pub struct FdEntry<F> {
    pub file: F,
    pub cloexec: bool,
}

/// A slot reserved in one table: consumed by `install` or `cancel` on that
/// same table. Not cloneable; neither an open descriptor nor a free slot.
/// Dropping this token leaves its slot reserved for the table's life.
/// The kernel uses its RAII `ReservedFd` wrapper, which cancels on drop.
#[derive(Debug)]
#[must_use = "install or cancel the reserved descriptor"]
pub struct FdReservation {
    fd: i32,
    id: u64,
}

impl FdReservation {
    /// The reserved descriptor number (not open until installed).
    pub fn fd(&self) -> i32 {
        self.fd
    }
}

static NEXT_RESERVATION: AtomicU64 = AtomicU64::new(1);

/// Per-process file descriptor table with capacity [`MAX_FDS`] (1024),
/// lazily grown and extract-and-drop friendly for IRQ-off `SpinLock` callers.
#[derive(Debug)]
pub struct FdTable<F> {
    slots: Vec<Option<FdEntry<F>>>,
    reserved: BTreeMap<i32, u64>,
}

impl<F: Clone> Clone for FdTable<F> {
    /// Copy only open entries: another thread's in-flight open belongs to
    /// the original table, never its fork copy.
    fn clone(&self) -> Self {
        Self { slots: self.slots.clone(), reserved: BTreeMap::new() }
    }
}

impl<F> Default for FdTable<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F> FdTable<F> {
    /// Create an empty descriptor table without pre-allocating 1024 slots.
    pub const fn new() -> Self {
        Self { slots: Vec::new(), reserved: BTreeMap::new() }
    }

    /// Borrow descriptor `fd`, if open.
    pub fn get(&self, fd: i32) -> Option<&FdEntry<F>> {
        let idx = usize::try_from(fd).ok()?;
        self.slots.get(idx).and_then(Option::as_ref)
    }

    /// Mutably borrow descriptor `fd`, if open.
    pub fn get_mut(&mut self, fd: i32) -> Option<&mut FdEntry<F>> {
        let idx = usize::try_from(fd).ok()?;
        self.slots.get_mut(idx).and_then(Option::as_mut)
    }

    /// Allocate the lowest free FD in `[0, MAX_FDS)`.
    pub fn alloc(&mut self, file: F, cloexec: bool) -> Result<i32, VfsError> {
        self.alloc_from(0, file, cloexec)
    }

    /// Allocate the lowest free FD `>= min_fd` in `[0, MAX_FDS)` (`F_DUPFD` / `F_DUPFD_CLOEXEC`).
    ///
    /// Returns `Err(Inval)` if `min_fd < 0 || min_fd >= MAX_FDS as i32`,
    /// or `Err(MFile)` if every slot in `[min_fd, MAX_FDS)` is open or reserved.
    pub fn alloc_from(&mut self, min_fd: i32, file: F, cloexec: bool) -> Result<i32, VfsError> {
        let fd = self.lowest_free(min_fd)?;
        self.slots.resize_with(self.slots.len().max(fd as usize + 1), || None);
        self.slots[fd as usize] = Some(FdEntry { file, cloexec });
        Ok(fd)
    }

    fn lowest_free(&self, from: i32) -> Result<i32, VfsError> {
        if from < 0 || from as usize >= MAX_FDS {
            return Err(VfsError::Inval);
        }
        (from..MAX_FDS as i32)
            .find(|&fd| self.get(fd).is_none() && !self.reserved.contains_key(&fd))
            .ok_or(VfsError::MFile)
    }

    /// Reserve the lowest free slot at or above `from`, before doing work
    /// outside the table lock. `Inval` for an out-of-range minimum,
    /// `MFile` if none is free. Reserved slots are skipped by all open-FD
    /// operations, drains and clones, and cannot be displaced by dup2.
    pub fn reserve(&mut self, from: i32) -> Result<FdReservation, VfsError> {
        let fd = self.lowest_free(from)?;
        self.slots.resize_with(self.slots.len().max(fd as usize + 1), || None);
        let id = NEXT_RESERVATION.fetch_add(1, Ordering::Relaxed);
        assert_ne!(id, 0, "FD reservation identity exhausted");
        self.reserved.insert(fd, id);
        Ok(FdReservation { fd, id })
    }

    /// Install an entry in its reserved slot, returning its descriptor.
    /// Panics for a token from a different table; nothing is displaced.
    pub fn install(&mut self, r: FdReservation, file: F, cloexec: bool) -> i32 {
        self.cancel_reservation(&r);
        assert!(self.slots[r.fd as usize].is_none());
        self.slots[r.fd as usize] = Some(FdEntry { file, cloexec });
        r.fd
    }

    /// Return a reserved slot to the free set. Panics for a token from a
    /// different table. Draining open entries does not cancel reservations.
    pub fn cancel(&mut self, r: FdReservation) {
        self.cancel_reservation(&r);
    }

    fn cancel_reservation(&mut self, r: &FdReservation) {
        assert_eq!(self.reserved.get(&r.fd), Some(&r.id), "FD reservation belongs to another table");
        self.reserved.remove(&r.fd);
    }

    /// Insert or replace `fd` directly (`dup2` / `dup3` / stdio seeding).
    ///
    /// Returns `Err(Busy)` for a reserved slot, `Err(BadF)` outside the table.
    /// Returns the displaced `FdEntry` (if any) so the caller can drop it
    /// outside every spinlock (the kernel's descriptor-table lock among them).
    pub fn insert_at(&mut self, fd: i32, file: F, cloexec: bool) -> Result<Option<FdEntry<F>>, VfsError> {
        let Ok(idx) = usize::try_from(fd) else {
            return Err(VfsError::BadF);
        };
        if idx >= MAX_FDS {
            return Err(VfsError::BadF);
        }
        if self.reserved.contains_key(&fd) {
            return Err(VfsError::Busy);
        }
        if idx >= self.slots.len() {
            self.slots.resize_with(idx + 1, || None);
        }
        Ok(self.slots[idx].replace(FdEntry { file, cloexec }))
    }

    /// Remove and return `fd` (`close`), or `Err(BadF)` if invalid/unallocated.
    pub fn remove(&mut self, fd: i32) -> Result<FdEntry<F>, VfsError> {
        let Ok(idx) = usize::try_from(fd) else {
            return Err(VfsError::BadF);
        };
        self.slots.get_mut(idx).and_then(Option::take).ok_or(VfsError::BadF)
    }

    /// Query `FD_CLOEXEC` for `fd`.
    pub fn get_cloexec(&self, fd: i32) -> Result<bool, VfsError> {
        self.get(fd).map(|e| e.cloexec).ok_or(VfsError::BadF)
    }

    /// Set `FD_CLOEXEC` for `fd`.
    pub fn set_cloexec(&mut self, fd: i32, cloexec: bool) -> Result<(), VfsError> {
        let entry = self.get_mut(fd).ok_or(VfsError::BadF)?;
        entry.cloexec = cloexec;
        Ok(())
    }

    /// Remove and return all entries marked `cloexec` (for `execve`).
    pub fn drain_cloexec(&mut self) -> Vec<FdEntry<F>> {
        let mut out = Vec::new();
        for slot in &mut self.slots {
            if slot.as_ref().is_some_and(|e| e.cloexec)
                && let Some(entry) = slot.take()
            {
                out.push(entry);
            }
        }
        out
    }

    /// Remove and return all open entries (for `exit_group`).
    pub fn drain_all(&mut self) -> Vec<FdEntry<F>> {
        let mut out = Vec::new();
        for slot in &mut self.slots {
            if let Some(entry) = slot.take() {
                out.push(entry);
            }
        }
        out
    }

    /// Iterate over `(fd, entry)` for all open descriptors.
    pub fn iter_open(&self) -> impl Iterator<Item = (i32, &FdEntry<F>)> {
        self.slots.iter().enumerate().filter_map(|(i, slot)| slot.as_ref().map(|e| (i as i32, e)))
    }

    /// Number of reserved slots awaiting installation or cancellation.
    pub fn reserved_count(&self) -> usize {
        self.reserved.len()
    }

    /// Number of currently open descriptors.
    pub fn open_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }
}
