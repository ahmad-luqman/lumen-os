//! Pure `no_std + alloc` POSIX VFS core for the Lumen kernel.
//!
//! Provides filesystem/inode traits, Lock-9 inode/dentry/mount tables with a
//! short-probe `VfsCacheOps` boundary (never held across Locks 4–8), physical
//! path & symlink resolution, `OpenFile` / 1024-FD `FdTable`, high-level
//! namespace operations (`open`, `mkdir`, `unlink`, `rmdir`, `link`, `symlink`,
//! `readlink`, `renameat2`), and independent `flock(2)` + `fcntl` POSIX record
//! advisory locks.
//!
//! Clean-room MIT implementation conceptually adapted from Starry / `axfs_vfs`.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod cache;
pub mod error;
pub mod fd;
pub mod fs;
pub mod locks;
pub mod mount;
pub mod ops;
pub mod procfs;
pub mod resolve;
pub mod types;

#[cfg(test)]
mod tests;

use core::sync::atomic::{AtomicU64, Ordering};

pub use cache::{DentryCache, DentryTarget, InodeCache};
pub use error::VfsError;
pub use fd::{FdEntry, FdTable, LocalLock, OpenFile, OpenFileId, VfsLock};
pub use fs::{FileSystem, OpenGuard, VfsInode};
pub use locks::{
    AdvisoryLocks, Deadlock, FlockKind, FlockOp, FlockOutcome, FlockSpec, LockContext, PosixLockType, PosixWaitGraph,
    RecordLockOutcome, WaitSearch,
};
pub use mount::{MountInfo, MountTable, SimpleVfsTables, VfsCacheOps};
pub use ops::{
    Opened, link_at, mkdir_at, open_inode, open_inode_guarded, readlink_at, rename_at2, symlink_at, unlink_at,
};
pub use resolve::{
    ParentTarget, ResolveFlags, ResolvedPath, check_access, compute_cwd_path, resolve_parent, resolve_path,
};
pub use types::{
    AccessMask, AccessMode, AtFlags, DevId, DirEntry, Ino, InodeType, Metadata, OpenFlags, RenameFlags, SeekFrom,
    SetTime, Timespec, mode_after_chown,
};

/// Maximum POSIX path length including NUL (`PATH_MAX`).
pub const PATH_MAX: usize = 4096;
/// Maximum single filename component length (`NAME_MAX`).
pub const NAME_MAX: usize = 255;
/// Maximum symbolic link traversals per lookup (`MAXSYMLINKS` / Linux `40`).
pub const MAX_SYMLINK_DEPTH: usize = 40;
/// Per-process open file descriptor capacity (`RLIMIT_NOFILE`).
pub const MAX_FDS: usize = 1024;
/// High-bit tag applied to synthetic `st_ino` values for detached open-unlinked inodes.
pub const DETACHED_INO_BIT: u64 = 1u64 << 63;

static NEXT_LOCK_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_OPEN_FILE_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_DETACHED_INO_SEQ: AtomicU64 = AtomicU64::new(1);

/// Allocate an immutable, globally unique, monotonically increasing `lock_id`
/// for Lock 4 total ordering across inodes.
pub fn next_lock_id() -> u64 {
    NEXT_LOCK_ID.fetch_add(1, Ordering::Relaxed)
}

/// Allocate a globally unique [`OpenFileId`] for `flock(2)` ownership.
pub fn next_open_file_id() -> OpenFileId {
    OpenFileId(NEXT_OPEN_FILE_ID.fetch_add(1, Ordering::Relaxed))
}

/// Allocate a synthetic non-colliding `st_ino` (`(1 << 63) | seq`) for an
/// open-unlinked inode detached from `InodeCache`.
pub fn next_detached_ino() -> Ino {
    DETACHED_INO_BIT | NEXT_DETACHED_INO_SEQ.fetch_add(1, Ordering::Relaxed)
}
