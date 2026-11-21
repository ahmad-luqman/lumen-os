//! Independent `flock(2)` and `fcntl(2)` POSIX byte-range advisory locks.
//!
//! Embedded per-inode inside Lock 4 (`VfsInode` mutex) in the kernel.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::error::VfsError;
use crate::fd::OpenFileId;

/// `flock(2)` operation constants.
pub mod flock_const {
    pub const LOCK_SH: i32 = 1;
    pub const LOCK_EX: i32 = 2;
    pub const LOCK_NB: i32 = 4;
    pub const LOCK_UN: i32 = 8;
}

/// `fcntl(2)` command and `struct flock` constants.
pub mod fcntl_const {
    pub const F_DUPFD: i32 = 0;
    pub const F_GETFD: i32 = 1;
    pub const F_SETFD: i32 = 2;
    pub const F_GETFL: i32 = 3;
    pub const F_SETFL: i32 = 4;
    pub const F_GETLK: i32 = 5;
    pub const F_SETLK: i32 = 6;
    pub const F_SETLKW: i32 = 7;
    pub const F_DUPFD_CLOEXEC: i32 = 1030;

    pub const FD_CLOEXEC: u32 = 1;

    pub const F_RDLCK: i16 = 0;
    pub const F_WRLCK: i16 = 1;
    pub const F_UNLCK: i16 = 2;

    pub const SEEK_SET: i16 = 0;
    pub const SEEK_CUR: i16 = 1;
    pub const SEEK_END: i16 = 2;
}

/// Lock mode for `flock(2)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlockKind {
    Shared,
    Exclusive,
}

/// Decoded `flock(2)` request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlockOp {
    /// `Some(kind)` to acquire/convert, `None` for `LOCK_UN`.
    pub kind: Option<FlockKind>,
    /// True if `LOCK_NB` was set.
    pub nonblock: bool,
}

impl FlockOp {
    /// Decode Linux `flock(2)` `operation` integer.
    pub const fn from_raw(raw: i32) -> Result<Self, VfsError> {
        let valid_bits = flock_const::LOCK_SH | flock_const::LOCK_EX | flock_const::LOCK_UN | flock_const::LOCK_NB;
        if (raw & !valid_bits) != 0 {
            return Err(VfsError::Inval);
        }
        let nonblock = (raw & flock_const::LOCK_NB) != 0;
        let cmd = raw & !flock_const::LOCK_NB;
        let kind = match cmd {
            flock_const::LOCK_SH => Some(FlockKind::Shared),
            flock_const::LOCK_EX => Some(FlockKind::Exclusive),
            flock_const::LOCK_UN => None,
            _ => return Err(VfsError::Inval),
        };
        Ok(Self { kind, nonblock })
    }
}

/// Lock type for `fcntl` POSIX record locks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PosixLockType {
    Read,
    Write,
    Unlock,
}

impl PosixLockType {
    pub const fn from_raw(raw: i16) -> Result<Self, VfsError> {
        match raw {
            fcntl_const::F_RDLCK => Ok(Self::Read),
            fcntl_const::F_WRLCK => Ok(Self::Write),
            fcntl_const::F_UNLCK => Ok(Self::Unlock),
            _ => Err(VfsError::Inval),
        }
    }

    pub const fn to_raw(self) -> i16 {
        match self {
            Self::Read => fcntl_const::F_RDLCK,
            Self::Write => fcntl_const::F_WRLCK,
            Self::Unlock => fcntl_const::F_UNLCK,
        }
    }
}

/// Portable representation of Linux `struct flock`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlockSpec {
    pub l_type: i16,
    pub l_whence: i16,
    pub l_start: i64,
    pub l_len: i64,
    pub l_pid: i32,
}

/// File descriptor state needed for `fcntl` `F_SETLK` / `F_SETLKW` validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LockContext {
    pub cur_offset: u64,
    pub file_size: u64,
    pub readable: bool,
    pub writable: bool,
}

/// Result of a non-failing lock attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockOutcome {
    /// Lock acquired or unlocked immediately.
    Granted,
    /// Blocking lock requested (`!LOCK_NB` or `F_SETLKW`) but conflicted.
    WouldBlock,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RangeLockKind {
    Read,
    Write,
}

impl RangeLockKind {
    const fn conflicts_with(self, other: Self) -> bool {
        matches!((self, other), (Self::Write, _) | (_, Self::Write))
    }

    const fn to_raw(self) -> i16 {
        match self {
            Self::Read => fcntl_const::F_RDLCK,
            Self::Write => fcntl_const::F_WRLCK,
        }
    }
}

/// Inclusive byte range `[start, end]` with `0 <= start <= end <= i64::MAX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PosixRangeLock {
    start: i64,
    end: i64,
    kind: RangeLockKind,
}

impl PosixRangeLock {
    const fn overlaps(self, start: i64, end: i64) -> bool {
        self.start <= end && start <= self.end
    }
}

/// Per-inode advisory lock table maintaining independent `flock(2)` and
/// `fcntl(2)` POSIX byte-range lock namespaces.
#[derive(Debug, Default)]
pub struct AdvisoryLocks {
    flocks: Vec<(OpenFileId, FlockKind)>,
    posix: BTreeMap<u32, Vec<PosixRangeLock>>,
}

impl AdvisoryLocks {
    /// Create an empty advisory lock table.
    pub const fn new() -> Self {
        Self { flocks: Vec::new(), posix: BTreeMap::new() }
    }

    /// Apply a `flock(2)` operation for `file_id`.
    ///
    /// Callers must reject `O_PATH` descriptors with `-EBADF` beforehand.
    /// When `op.nonblock` is true and another `OpenFileId` conflicts, returns
    /// `Err(VfsError::Again)` while leaving any existing lock held by
    /// `file_id` intact.
    pub fn flock(&mut self, file_id: OpenFileId, op: FlockOp) -> Result<LockOutcome, VfsError> {
        let Some(req_kind) = op.kind else {
            self.release_flock(file_id);
            return Ok(LockOutcome::Granted);
        };

        let conflict = self.flocks.iter().any(|&(other_id, other_kind)| match (req_kind, other_kind) {
            _ if other_id == file_id => false,
            (FlockKind::Exclusive, _) | (_, FlockKind::Exclusive) => true,
            (FlockKind::Shared, FlockKind::Shared) => false,
        });

        if conflict {
            if op.nonblock {
                return Err(VfsError::Again);
            }
            return Ok(LockOutcome::WouldBlock);
        }

        if let Some((_, kind)) = self.flocks.iter_mut().find(|(id, _)| *id == file_id) {
            *kind = req_kind;
        } else {
            self.flocks.push((file_id, req_kind));
        }
        Ok(LockOutcome::Granted)
    }

    /// Release any `flock(2)` held by `file_id` (e.g. on last FD close).
    /// Returns `true` if a lock was present and removed.
    pub fn release_flock(&mut self, file_id: OpenFileId) -> bool {
        if let Some(pos) = self.flocks.iter().position(|(id, _)| *id == file_id) {
            self.flocks.swap_remove(pos);
            true
        } else {
            false
        }
    }

    /// Inspect `F_GETLK` for `pid`. Updates `spec` in place with either the
    /// first conflicting lock or `l_type = F_UNLCK`.
    pub fn getlk(&self, pid: u32, spec: &mut FlockSpec, cur_offset: u64, file_size: u64) -> Result<(), VfsError> {
        let req_kind = match PosixLockType::from_raw(spec.l_type)? {
            PosixLockType::Read => RangeLockKind::Read,
            PosixLockType::Write => RangeLockKind::Write,
            PosixLockType::Unlock => return Err(VfsError::Inval),
        };
        let (start, end) = normalize_range(spec, cur_offset, file_size)?;

        for (&other_pid, locks) in &self.posix {
            if other_pid == pid {
                continue;
            }
            for &lock in locks {
                if lock.overlaps(start, end) && req_kind.conflicts_with(lock.kind) {
                    spec.l_type = lock.kind.to_raw();
                    spec.l_whence = fcntl_const::SEEK_SET;
                    spec.l_start = lock.start;
                    spec.l_len = if lock.end == i64::MAX { 0 } else { lock.end - lock.start + 1 };
                    spec.l_pid = other_pid as i32;
                    return Ok(());
                }
            }
        }

        spec.l_type = fcntl_const::F_UNLCK;
        Ok(())
    }

    /// Apply `F_SETLK` (`blocking = false`) or `F_SETLKW` (`blocking = true`).
    ///
    /// Callers must reject `O_PATH` descriptors with `-EBADF` beforehand.
    pub fn setlk(
        &mut self,
        pid: u32,
        spec: FlockSpec,
        ctx: LockContext,
        blocking: bool,
    ) -> Result<LockOutcome, VfsError> {
        let lock_type = PosixLockType::from_raw(spec.l_type)?;
        match lock_type {
            PosixLockType::Read if !ctx.readable => return Err(VfsError::BadF),
            PosixLockType::Write if !ctx.writable => return Err(VfsError::BadF),
            _ => {}
        }

        let (start, end) = normalize_range(&spec, ctx.cur_offset, ctx.file_size)?;

        let req_kind = match lock_type {
            PosixLockType::Unlock => {
                if let Some(locks) = self.posix.get_mut(&pid) {
                    subtract_range(locks, start, end);
                    if locks.is_empty() {
                        self.posix.remove(&pid);
                    }
                }
                return Ok(LockOutcome::Granted);
            }
            PosixLockType::Read => RangeLockKind::Read,
            PosixLockType::Write => RangeLockKind::Write,
        };

        let conflict = self.posix.iter().any(|(&other_pid, locks)| {
            other_pid != pid && locks.iter().any(|lock| lock.overlaps(start, end) && req_kind.conflicts_with(lock.kind))
        });

        if conflict {
            if !blocking {
                return Err(VfsError::Again);
            }
            return Ok(LockOutcome::WouldBlock);
        }

        let locks = self.posix.entry(pid).or_default();
        subtract_range(locks, start, end);
        locks.push(PosixRangeLock { start, end, kind: req_kind });
        sort_and_coalesce(locks);

        Ok(LockOutcome::Granted)
    }

    /// Release **all** POSIX record locks held by `pid` on this inode
    /// (mandated by POSIX when `pid` closes any FD referring to this inode).
    /// Returns `true` if any locks were removed.
    pub fn release_posix_for_pid(&mut self, pid: u32) -> bool {
        self.posix.remove(&pid).is_some()
    }

    /// Count of active `flock(2)` holders (diagnostic / test helper).
    pub fn flock_count(&self) -> usize {
        self.flocks.len()
    }

    /// Count of POSIX record lock segments held by `pid` (diagnostic / test helper).
    pub fn posix_lock_count(&self, pid: u32) -> usize {
        self.posix.get(&pid).map_or(0, Vec::len)
    }
}

fn normalize_range(spec: &FlockSpec, cur_offset: u64, file_size: u64) -> Result<(i64, i64), VfsError> {
    let base = match spec.l_whence {
        fcntl_const::SEEK_SET => 0i64,
        fcntl_const::SEEK_CUR => i64::try_from(cur_offset).map_err(|_| VfsError::Inval)?,
        fcntl_const::SEEK_END => i64::try_from(file_size).map_err(|_| VfsError::Inval)?,
        _ => return Err(VfsError::Inval),
    };

    let anchor = base.checked_add(spec.l_start).ok_or(VfsError::Inval)?;

    if spec.l_len == 0 {
        if anchor < 0 {
            return Err(VfsError::Inval);
        }
        Ok((anchor, i64::MAX))
    } else if spec.l_len > 0 {
        if anchor < 0 {
            return Err(VfsError::Inval);
        }
        let end = anchor.checked_add(spec.l_len - 1).ok_or(VfsError::Inval)?;
        Ok((anchor, end))
    } else {
        let start = anchor.checked_add(spec.l_len).ok_or(VfsError::Inval)?;
        if start < 0 {
            return Err(VfsError::Inval);
        }
        let end = anchor - 1;
        Ok((start, end))
    }
}

fn subtract_range(locks: &mut Vec<PosixRangeLock>, u_start: i64, u_end: i64) {
    let mut next = Vec::with_capacity(locks.len() + 1);
    for lock in locks.drain(..) {
        if !lock.overlaps(u_start, u_end) {
            next.push(lock);
            continue;
        }
        if lock.start < u_start {
            next.push(PosixRangeLock { start: lock.start, end: u_start - 1, kind: lock.kind });
        }
        if u_end < lock.end {
            next.push(PosixRangeLock { start: u_end + 1, end: lock.end, kind: lock.kind });
        }
    }
    *locks = next;
}

fn sort_and_coalesce(locks: &mut Vec<PosixRangeLock>) {
    locks.sort_unstable_by_key(|l| l.start);
    let mut merged: Vec<PosixRangeLock> = Vec::with_capacity(locks.len());
    for lock in locks.drain(..) {
        if let Some(prev) = merged.last_mut()
            && prev.kind == lock.kind
            && prev.end.saturating_add(1) >= lock.start
        {
            if lock.end > prev.end {
                prev.end = lock.end;
            }
            continue;
        }
        merged.push(lock);
    }
    *locks = merged;
}
