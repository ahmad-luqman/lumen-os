//! Open file descriptions (`OpenFile`) and per-process 1024-entry `FdTable`.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::{RefCell, RefMut};
use core::fmt;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

use crate::error::VfsError;
use crate::fs::{OpenGuard, VfsInode};
use crate::ops::Opened;
use crate::types::{AccessMode, OpenFlags};
use crate::{MAX_FDS, next_open_file_id};

/// Synchronization abstraction for `OpenFile::offset` and `dir_cursor` (Lock 2).
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
    pub inode: Arc<dyn VfsInode>,
    pub path: Arc<str>,
    access_mode: AccessMode,
    is_path: bool,
    status_flags: AtomicU32,
    fd_refs: AtomicU32,
    pub offset: L,
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
    pub fn new_unchecked_flags(inode: Arc<dyn VfsInode>, path: Arc<str>, flags: OpenFlags) -> Result<Self, VfsError> {
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

    fn build(guard: OpenGuard, path: Arc<str>, flags: OpenFlags) -> Result<Self, VfsError> {
        let access_mode = flags.access_mode()?;
        let is_path = flags.is_path();
        let status_bits = if is_path { 0 } else { flags.status_bits() };
        Ok(Self {
            id: next_open_file_id(),
            inode: Arc::clone(guard.inode()),
            path,
            access_mode,
            is_path,
            status_flags: AtomicU32::new(status_bits),
            fd_refs: AtomicU32::new(1),
            offset: L::new(0),
            dir_cursor: L::new(0),
            open_guard: guard,
        })
    }

    /// The open reference this description holds on its inode: clone it
    /// for another holder (a file mapping, `AddressSpace::mmap_file`) that
    /// must keep the inode open as long as the description does, with no
    /// fresh [`OpenGuard::try_new`] that could be refused.
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
    /// `self.id` outside `ACTIVE_PROCS`.
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
            .field("dev", &self.inode.dev_id())
            .field("ino", &self.inode.ino())
            .field("path", &self.path)
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

/// Per-process file descriptor table with capacity [`MAX_FDS`] (1024),
/// lazily grown and extract-and-drop friendly for IRQ-off `SpinLock` callers.
#[derive(Clone, Debug)]
pub struct FdTable<F> {
    slots: Vec<Option<FdEntry<F>>>,
}

impl<F> Default for FdTable<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F> FdTable<F> {
    /// Create an empty descriptor table without pre-allocating 1024 slots.
    pub const fn new() -> Self {
        Self { slots: Vec::new() }
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
    /// or `Err(MFile)` if all slots in `[min_fd, MAX_FDS)` are occupied.
    pub fn alloc_from(&mut self, min_fd: i32, file: F, cloexec: bool) -> Result<i32, VfsError> {
        if min_fd < 0 || min_fd as usize >= MAX_FDS {
            return Err(VfsError::Inval);
        }
        let start = min_fd as usize;
        for idx in start..MAX_FDS {
            if idx >= self.slots.len() {
                self.slots.resize_with(idx + 1, || None);
            }
            if self.slots[idx].is_none() {
                self.slots[idx] = Some(FdEntry { file, cloexec });
                return Ok(idx as i32);
            }
        }
        Err(VfsError::MFile)
    }

    /// Insert or replace `fd` directly (`dup2` / `dup3` / stdio seeding).
    ///
    /// Returns the displaced `FdEntry` (if any) so the caller can drop it
    /// outside `ACTIVE_PROCS`.
    pub fn insert_at(&mut self, fd: i32, file: F, cloexec: bool) -> Result<Option<FdEntry<F>>, VfsError> {
        let Ok(idx) = usize::try_from(fd) else {
            return Err(VfsError::BadF);
        };
        if idx >= MAX_FDS {
            return Err(VfsError::BadF);
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

    /// Number of currently open descriptors.
    pub fn open_count(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }
}
