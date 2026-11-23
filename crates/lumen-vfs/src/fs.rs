//! Core filesystem and inode traits, plus lock-free RAII open-count guards.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;

use crate::error::VfsError;
use crate::locks::AdvisoryLocks;
use crate::types::{DevId, DirEntry, Ino, InodeType, Metadata, RenameFlags, SetTime};

/// Mounted filesystem interface (`ext4`, `tmpfs`, `devfs`, `procfs`).
pub trait FileSystem: Send + Sync {
    /// Canonical `DevId` (`makedev(major, minor)`) matching `stat.st_dev`.
    ///
    /// Called under Lock 9 (the mount table's checks, #320): must not lock
    /// or block, and answers from fixed state.
    fn dev_id(&self) -> DevId;

    /// Human-readable filesystem type name (e.g. `"ext4"`, `"tmpfs"`, `"devfs"`, `"procfs"`).
    fn name(&self) -> &'static str;

    /// Root directory inode of this filesystem.
    fn root_inode(&self) -> Arc<dyn VfsInode>;

    /// Flush filesystem metadata and dirty state.
    ///
    /// Logs its own failures at `warn` (ext4: each inode and the block
    /// cache that failed). Nothing above it reports them: `sync(2)` returns
    /// 0 whatever this returns, and the kernel's `vfs::sync_all` logs a
    /// failed filesystem only at `debug`, so these lines are the only
    /// report of a failed global sync.
    fn sync(&self) -> Result<(), VfsError>;

    /// True if the filesystem is mounted read-only.
    fn read_only(&self) -> bool;

    /// Execute `f` while holding this filesystem's Lock 3 (`rename_lock`).
    ///
    /// **All concurrent multi-threaded writable filesystems MUST override this
    /// method** to acquire their per-FS Lock 3 (`SleepMutex<()>`), serializing
    /// all 6 namespace-mutating operations (`open_inode(O_CREAT)`, `mkdir_at`,
    /// `unlink_at`, `link_at`, `symlink_at`, `rename_at2`) against directory-entry
    /// TOCTOU races. The default no-op implementation is strictly for read-only /
    /// pseudo filesystems or single-writer / non-overlapping host tests.
    fn with_rename_lock(&self, f: &mut dyn FnMut() -> Result<(), VfsError>) -> Result<(), VfsError> {
        f()
    }
}

/// Virtual filesystem inode interface.
///
/// Implementations must ensure `Drop` NEVER acquires a sleeping lock (Lock 10
/// `SpinLock` drop paths and `AddressSpace::drop` rely on lock-free `Drop`).
pub trait VfsInode: Send + Sync {
    /// Immutable creation-time monotonic ID used for Lock 4 total ordering.
    /// Never changes even if `ino()` transitions to a synthetic detached ID.
    fn lock_id(&self) -> u64;

    /// Filesystem device ID (`stat.st_dev`).
    ///
    /// Called under Lock 9 (`InodeCache`'s keys, #320): must not lock or
    /// block, and answers from fixed state.
    fn dev_id(&self) -> DevId;

    /// Current `st_ino` (may transition to `(1 << 63) | seq` on detached unlinks).
    ///
    /// Called under Lock 9 (`InodeCache` checks a cached entry's number,
    /// #320): must not lock or block (an atomic load, as the detach that
    /// changes it stores it).
    fn ino(&self) -> Ino;

    /// File type (`File`, `Dir`, `Symlink`, `CharDevice`, ...).
    fn inode_type(&self) -> InodeType;

    /// Explicit atomic open-reference count (`OpenFile`, `cwd`, `mapped_files`),
    /// distinct from `Arc<dyn VfsInode>` cache strong refs.
    ///
    /// Gate 6 contract (ARCHITECTURE section 9): an `unlink`, `rmdir` or
    /// rename-overwrite that removes the last link reads `open_count()` and
    /// then either frees the inode (`stat().nlink == 0`, not detached) or
    /// detaches it (`is_detached()`), all within one hold of the lock that
    /// `stat()` takes. [`OpenGuard::try_new`] raises the count first and then
    /// calls `stat()`, so either the remover sees the new reference and
    /// detaches, or the opener sees the freed inode and fails with `ENOENT`.
    fn open_count(&self) -> u32;

    /// Increment explicit open-reference count.
    fn inc_open_count(&self);

    /// Decrement explicit open-reference count, returning the new value.
    fn dec_open_count(&self) -> u32;

    /// True if this inode has been detached-unlinked (`st_nlink == 0` while open).
    ///
    /// Called under Lock 9 (`InodeCache` checks whether a cached entry is
    /// stale, #320): must not lock or block (an atomic load, as the detach
    /// stores it).
    fn is_detached(&self) -> bool {
        false
    }

    /// Host tests only: runs where `open_inode` decides the read-only
    /// mount's `-EROFS` (after the lookup and Lock 3, before the open
    /// reference), for the Gate tests' hooks.
    #[cfg(test)]
    fn at_rofs_check(&self) {}

    /// True for a terminal (`devfs`'s `tty` and `console`): a descriptor on
    /// one reads and writes the calling process's console, takes the tty
    /// `ioctl`s (`-ENOTTY` on anything else), polls on the console's
    /// readiness and can't seek (`-ESPIPE`). Answers from fixed state,
    /// without locking.
    fn is_tty(&self) -> bool {
        false
    }

    /// False for an inode whose existence the VFS can't hold on to, so it
    /// must never enter `DentryCache` or `InodeCache` (e.g. `procfs`'s
    /// `/proc/<pid>` and everything under it, which go when the process or
    /// its fd does). A lookup of such a name always asks the filesystem
    /// again.
    ///
    /// A non-cacheable directory's children must be non-cacheable too: a
    /// dentry is keyed by its parent's `(dev, ino)`, so a cached child's
    /// dentry would outlive its parent, one per parent for good. A
    /// `debug_assert!` in the resolver checks it on a plain lookup (the
    /// kernel's builds keep debug assertions on); `open(O_CREAT)` of an
    /// existing name and a created inode's re-publish don't check.
    ///
    /// The tables never refuse a mounted non-cacheable inode as stale (the
    /// `Err` a cache generation's change gives), so the VFS's race retries,
    /// which look a name up again when an unlink or eviction raced its
    /// caching, never run for it: a writable filesystem with non-cacheable
    /// inodes must not rely on them.
    ///
    /// Called under Lock 9: must not lock or block, and answers from fixed
    /// state (never changing for one inode).
    fn cacheable(&self) -> bool {
        true
    }

    /// Magic symlink target override (e.g. `/proc/<pid>/fd/*` and `/proc/<pid>/exe`).
    /// When `Some`, path traversal jumps directly to the returned inode while
    /// `readlink()` continues to return the link's path bytes.
    fn resolve_magic_link(&self) -> Option<Arc<dyn VfsInode>> {
        None
    }

    fn stat(&self) -> Result<Metadata, VfsError>;
    /// Names, here and in `create` to `rename`, are bytes (#268): any but
    /// `/` and NUL, UTF-8 or not; `symlink`'s target is any bytes but NUL.
    /// A filesystem stores and compares them as given, never through a
    /// lossy conversion.
    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn VfsInode>, VfsError>;
    fn create(&self, name: &[u8], mode: u16) -> Result<Arc<dyn VfsInode>, VfsError>;
    fn mkdir(&self, name: &[u8], mode: u16) -> Result<Arc<dyn VfsInode>, VfsError>;
    fn unlink(&self, name: &[u8]) -> Result<(), VfsError>;
    fn rmdir(&self, name: &[u8]) -> Result<(), VfsError>;
    fn link(&self, name: &[u8], target: &Arc<dyn VfsInode>) -> Result<(), VfsError>;
    fn symlink(&self, name: &[u8], target: &[u8]) -> Result<Arc<dyn VfsInode>, VfsError>;
    fn readlink(&self) -> Result<Vec<u8>, VfsError>;
    fn rename(
        &self,
        old_name: &[u8],
        new_dir: &Arc<dyn VfsInode>,
        new_name: &[u8],
        flags: RenameFlags,
    ) -> Result<(), VfsError>;
    /// Push up to `max_entries` of this directory's entries onto `out`,
    /// starting with the first at or after `cursor` (`0`: the start; `.`
    /// and `..` are listed as entries). With room for one (`max_entries`
    /// above 0 and `out` not already that long), nothing pushed means the
    /// end.
    ///
    /// **The cursor** is the filesystem's own, opaque to callers: `0`, or a
    /// [`DirEntry::next_cursor`] this directory returned, or any value at
    /// all (`lseek` lets user space set one): a filesystem must take any
    /// cursor without harm, listing from the first entry at or past it
    /// as it orders them. Each entry's
    /// `next_cursor` is greater than `cursor` and than the one before it,
    /// and resumes with the entry after it. It must stay **stable as other
    /// entries come and go**: a listing resumed after entries were removed
    /// or added lists every entry that stayed in the directory throughout
    /// exactly once (POSIX leaves open only the ones removed or added
    /// meanwhile). `getdents64` keeps it in `OpenFile::dir_cursor` between
    /// calls, and `lseek` and `d_off` hand it to user space. An index into
    /// the current listing is not enough: a removal before it would shift
    /// the rest and skip an entry (busybox `rm -r`'s loop would leave one
    /// behind). tmpfs numbers entries as they are added, ext4 uses their
    /// byte positions (with two exceptions, ARCHITECTURE section 9), procfs
    /// keys them by pid or fd.
    ///
    /// On an `Err`, the entries already pushed are valid: a caller may hand
    /// them on and report the error on its next call. A directory removed
    /// while open (detached) lists as empty; a `/proc/<pid>` whose process
    /// has ended says `ENOENT`.
    fn readdir(&self, cursor: u64, out: &mut Vec<DirEntry>, max_entries: usize) -> Result<(), VfsError>;
    fn truncate(&self, size: u64) -> Result<(), VfsError>;
    fn set_mode(&self, mode: u16) -> Result<(), VfsError>;
    /// Set the owner and group (`None` leaves either as it is), update
    /// `ctime`, and leave the mode [`mode_after_chown`](crate::mode_after_chown)
    /// gives (setuid dropped on anything but a directory, setgid too with
    /// group-execute), all in one hold of the inode's metadata lock and, on
    /// disk, one inode write: a failure changes nothing, and no `set_mode`
    /// can land between the owner change and the mode's.
    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), VfsError>;
    fn set_times(&self, atime: SetTime, mtime: SetTime) -> Result<(), VfsError>;
    /// Read up to `buf.len()` bytes at `offset`, as `pread(2)`: how many
    /// were read, 0 at or past the end. A count short of what lies before
    /// the end is not EOF alone: an error past the first page read returns
    /// the bytes before it, and the next read, starting there, returns the
    /// error if it happens again (the kernel's page cache, as Linux's
    /// `filemap_read`).
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError>;
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError>;

    /// Write `buf` at the end of the file (`O_APPEND`): the end read and the
    /// write made in one hold of the inode's lock (Lock 4), so two appenders
    /// on separate open file descriptions can't both write at the same old
    /// end. Returns where the write began and how much was written (a short
    /// write as `write_at`'s).
    ///
    /// Every filesystem with regular files must override it (ext4 and tmpfs
    /// do). The default is [`append_without_end`]: `EINVAL` for a regular
    /// file (never a write at 0 over its first bytes), and for an inode
    /// without an end (a device, which ignores the offset) `write_at(0,
    /// buf)` at position 0, so the caller's offset becomes `n`.
    fn append(&self, buf: &[u8]) -> Result<(u64, usize), VfsError> {
        append_without_end(self, buf)
    }
    fn read_page(&self, page_index: u64, buf: &mut [u8; 4096]) -> Result<usize, VfsError>;
    fn write_page(&self, page_index: u64, buf: &[u8]) -> Result<(), VfsError>;
    fn sync(&self, data_only: bool) -> Result<(), VfsError>;

    /// Execute `f` with mutable access to this inode's Lock-4-protected `AdvisoryLocks`.
    ///
    /// `Err` only if `f` did not run (a filesystem keeping no lock state):
    /// once `f` has run, its changes stand and the call is `Ok`, so a caller
    /// never loses a release `f` made (and must wake waiters for).
    fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError>;

    /// The concrete inode, which this crate's types can't name: the
    /// kernel's `vfs::mmap_page` and `page_cache_lock` downcast to the
    /// `ext4` and `tmpfs` inodes for their cached pages, and
    /// `tmpfs::quota_use` to a `tmpfs` root for its mount's quota. `None`
    /// (the default) for every other filesystem.
    ///
    /// A new filesystem with a page cache that overrides this also needs
    /// its own branch in `mmap_page` and `page_cache_lock`, or its inodes
    /// are still refused with `NoDev`.
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        None
    }
}

impl fmt::Debug for dyn FileSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileSystem")
            .field("dev_id", &self.dev_id())
            .field("name", &self.name())
            .field("read_only", &self.read_only())
            .finish()
    }
}

impl fmt::Debug for dyn VfsInode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VfsInode")
            .field("dev_id", &self.dev_id())
            .field("ino", &self.ino())
            .field("lock_id", &self.lock_id())
            .field("type", &self.inode_type())
            .finish()
    }
}

/// [`VfsInode::append`]'s default: `EINVAL` for a regular file, whose
/// filesystem must override `append`; for anything else (a device: no end,
/// the offset ignored) `write_at(0, buf)`, reported at position 0.
pub fn append_without_end<I: VfsInode + ?Sized>(inode: &I, buf: &[u8]) -> Result<(u64, usize), VfsError> {
    if inode.inode_type() == InodeType::File {
        return Err(VfsError::Inval);
    }
    inode.write_at(0, buf).map(|n| (0, n))
}

/// RAII open-reference guard held by `OpenFile`, `cwd`, and `mapped_files`.
///
/// `Drop` performs only an atomic decrement (`dec_open_count`) — never a
/// sleeping lock.
pub struct OpenGuard {
    inode: Arc<dyn VfsInode>,
}

/// Why [`OpenGuard::try_new`] refused a reference, kept apart inside the
/// crate so an `O_CREAT` open retries only a lost race, never a `stat()`
/// error that happens to be `ENOENT` (a vanished procfs entry).
pub(crate) enum Refused {
    /// An unlink freed the inode before the count went up (`nlink == 0`,
    /// not detached): `ENOENT` to callers.
    Freed,
    /// `stat()` failed: its error, unchanged.
    Stat(VfsError),
}

impl From<Refused> for VfsError {
    fn from(r: Refused) -> Self {
        match r {
            Refused::Freed => VfsError::NoEnt,
            Refused::Stat(e) => e,
        }
    }
}

impl OpenGuard {
    /// Acquire an open reference on `inode` without checking that it is
    /// still linked or detached.
    ///
    /// Only where nothing can free the inode before the count goes up: an
    /// inode some other reference already keeps open (a clone, a `cwd`
    /// inherited by `fork`), a file just created under its filesystem's
    /// Lock 3 on a filesystem with a rename lock (the `O_CREAT` path of
    /// [`open_inode_guarded`](crate::ops::open_inode_guarded)), or tests. On
    /// an inode found by a path lookup, an `unlink` landing between the
    /// lookup and this increment frees the inode under the new reference.
    /// Opens go through [`OpenGuard::try_new`] (or `open_inode_guarded`).
    pub fn new_unchecked(inode: Arc<dyn VfsInode>) -> Self {
        inode.inc_open_count();
        Self { inode }
    }

    /// Acquire an open reference on `inode`, then check that the inode
    /// hasn't been freed: `ENOENT` (the reference given back) if
    /// `stat().nlink == 0` and it isn't detached, or `stat()`'s own error,
    /// passed through unchanged. Only an inode that reports `nlink >= 1`, or
    /// is detached, can be opened.
    ///
    /// The count is raised before the check, and Gate 6 reads the count and
    /// frees or detaches under the lock `stat()` takes, so an `unlink` racing
    /// this call either detaches the inode (the data stays readable through
    /// the guard) or has already freed it (this returns `ENOENT`); never a
    /// guard on a freed inode.
    ///
    /// May sleep: through `stat()` it takes the inode's Lock 4 and, on ext4
    /// and tmpfs, its Lock 5 (pages). Never call it holding this inode's
    /// Lock 4 (or another inode's Lock 4 out of `lock_id` order), any of
    /// Locks 5 to 9, or a spinlock.
    pub fn try_new(inode: Arc<dyn VfsInode>) -> Result<Self, VfsError> {
        Self::try_new_or_refused(inode).map_err(VfsError::from)
    }

    /// [`OpenGuard::try_new`] saying why it refused.
    pub(crate) fn try_new_or_refused(inode: Arc<dyn VfsInode>) -> Result<Self, Refused> {
        inode.inc_open_count();
        // On an error `guard` drops and gives the reference back.
        let guard = Self { inode };
        let meta = guard.inode.stat().map_err(Refused::Stat)?;
        // `is_detached` after `stat`, never before: a detach sets both under
        // the lock `stat` took, so a zero `nlink` read there comes with its
        // `detached` flag, while a flag read first could predate a detach
        // that `stat` then sees as `nlink == 0`.
        if meta.nlink == 0 && !guard.inode.is_detached() {
            return Err(Refused::Freed);
        }
        Ok(guard)
    }

    /// Borrow the guarded inode.
    pub fn inode(&self) -> &Arc<dyn VfsInode> {
        &self.inode
    }
}

/// A clone is another open reference on the same inode, never refused
/// (unlike [`OpenGuard::try_new`]): the inode is already open.
impl Clone for OpenGuard {
    fn clone(&self) -> Self {
        Self::new_unchecked(Arc::clone(&self.inode))
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        // This guard's own reference is still counted, so the count is at
        // least 1 here unless a reference was given back twice.
        debug_assert!(
            self.inode.open_count() > 0,
            "open count of inode {:?}:{} already 0 before an OpenGuard's drop",
            self.inode.dev_id(),
            self.inode.ino()
        );
        let _ = self.inode.dec_open_count();
    }
}
