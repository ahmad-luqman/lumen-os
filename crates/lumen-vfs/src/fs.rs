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
    /// again. A non-cacheable directory's children must be non-cacheable
    /// too: a dentry is keyed by its parent's `(dev, ino)`, so a cached
    /// child would keep its parent's key alive. Called under Lock 9: must
    /// not lock or block, and answers from fixed state (never changing for
    /// one inode).
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
    /// Acquire an open reference on `inode`.
    pub fn new(inode: Arc<dyn VfsInode>) -> Self {
        inode.inc_open_count();
        Self { inode }
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
        let _ = self.inode.dec_open_count();
    }
}
