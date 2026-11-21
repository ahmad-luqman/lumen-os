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
    fn dev_id(&self) -> DevId;

    /// Human-readable filesystem type name (e.g. `"ext4"`, `"tmpfs"`, `"devfs"`, `"procfs"`).
    fn name(&self) -> &'static str;

    /// Root directory inode of this filesystem.
    fn root_inode(&self) -> Arc<dyn VfsInode>;

    /// Flush filesystem metadata and dirty state.
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
    fn dev_id(&self) -> DevId;

    /// Current `st_ino` (may transition to `(1 << 63) | seq` on detached unlinks).
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
    fn is_detached(&self) -> bool {
        false
    }

    /// True for `devfs` `S_ISCHR` nodes exempt from `-EROFS` for `O_WRONLY`/`O_RDWR`/`O_TRUNC`/`W_OK`.
    fn exempt_from_rofs(&self) -> bool {
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
    /// `readlink()` continues to return the textual symlink path.
    fn resolve_magic_link(&self) -> Option<Arc<dyn VfsInode>> {
        None
    }

    fn stat(&self) -> Result<Metadata, VfsError>;
    fn lookup(&self, name: &str) -> Result<Arc<dyn VfsInode>, VfsError>;
    fn create(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError>;
    fn mkdir(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError>;
    fn unlink(&self, name: &str) -> Result<(), VfsError>;
    fn rmdir(&self, name: &str) -> Result<(), VfsError>;
    fn link(&self, name: &str, target: &Arc<dyn VfsInode>) -> Result<(), VfsError>;
    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn VfsInode>, VfsError>;
    fn readlink(&self) -> Result<Vec<u8>, VfsError>;
    fn rename(
        &self,
        old_name: &str,
        new_dir: &Arc<dyn VfsInode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<(), VfsError>;
    fn readdir(&self, cursor: u64, out: &mut Vec<DirEntry>, max_entries: usize) -> Result<(), VfsError>;
    fn truncate(&self, size: u64) -> Result<(), VfsError>;
    fn set_mode(&self, mode: u16) -> Result<(), VfsError>;
    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), VfsError>;
    fn set_times(&self, atime: SetTime, mtime: SetTime) -> Result<(), VfsError>;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError>;
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError>;
    fn read_page(&self, page_index: u64, buf: &mut [u8; 4096]) -> Result<usize, VfsError>;
    fn write_page(&self, page_index: u64, buf: &[u8]) -> Result<(), VfsError>;
    fn sync(&self, data_only: bool) -> Result<(), VfsError>;

    /// Execute `f` with mutable access to this inode's Lock-4-protected `AdvisoryLocks`.
    fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError>;
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

/// RAII open-reference guard held by `OpenFile`, `cwd`, and `mapped_files`.
///
/// `Drop` performs only an atomic decrement (`dec_open_count`) — never a
/// sleeping lock.
pub struct OpenGuard {
    inode: Arc<dyn VfsInode>,
}

impl OpenGuard {
    /// Acquire an open reference on `inode` without checking that it is
    /// still linked or detached.
    ///
    /// Only where nothing can free the inode before the count goes up: an
    /// inode some other reference already keeps open (a clone, a `cwd`
    /// inherited by `fork`), a file just created under its filesystem's
    /// Lock 3 (the `O_CREAT` path of
    /// [`open_inode_guarded`](crate::ops::open_inode_guarded)), or tests. On
    /// an inode found by a path lookup, an `unlink` landing between the
    /// lookup and this increment frees the inode under the new reference.
    /// Opens go through [`OpenGuard::try_new`] (or `open_inode_guarded`).
    pub fn new(inode: Arc<dyn VfsInode>) -> Self {
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
    /// Lock 4, any of Locks 5 to 9, or a spinlock.
    pub fn try_new(inode: Arc<dyn VfsInode>) -> Result<Self, VfsError> {
        inode.inc_open_count();
        // On an error `guard` drops and gives the reference back.
        let guard = Self { inode };
        let meta = guard.inode.stat()?;
        // `is_detached` after `stat`, never before: a detach sets both under
        // the lock `stat` took, so a zero `nlink` read there comes with its
        // `detached` flag, while a flag read first could predate a detach
        // that `stat` then sees as `nlink == 0`.
        if meta.nlink == 0 && !guard.inode.is_detached() {
            return Err(VfsError::NoEnt);
        }
        Ok(guard)
    }

    /// Borrow the guarded inode.
    pub fn inode(&self) -> &Arc<dyn VfsInode> {
        &self.inode
    }
}

impl Clone for OpenGuard {
    fn clone(&self) -> Self {
        Self::new(Arc::clone(&self.inode))
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        // This guard's own reference is still counted, so the count is at
        // least 1 here unless a reference was given back twice.
        debug_assert!(self.inode.open_count() > 0, "open count already 0 before an OpenGuard's drop");
        let _ = self.inode.dec_open_count();
    }
}
