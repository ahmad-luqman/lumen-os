//! Independent `flock(2)` and `fcntl(2)` POSIX byte-range advisory locks.
//!
//! [`AdvisoryLocks`] is embedded per inode inside Lock 4 (`VfsInode` mutex)
//! in the kernel. [`PosixWaitGraph`] is kernel-wide (one for every inode,
//! behind its own spinlock, never under Lock 4).

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

/// Result of a `flock(2)` attempt that didn't fail.
///
/// `released` says whether the attempt gave up (or weakened) a lock another
/// waiter may be blocked on, so the caller knows to wake its waiters: an
/// unlock that removed a lock, an `LOCK_EX` → `LOCK_SH` downgrade, or a
/// blocking request dropping the lock its description held before it waits.
/// Taking a new lock, an upgrade, re-taking the same kind, or unlocking
/// nothing releases nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "a release must be followed by waking the blocked lockers"]
pub enum FlockOutcome {
    /// Lock acquired, converted or unlocked immediately.
    Granted { released: bool },
    /// Blocking request (no `LOCK_NB`) that conflicts: wait and retry. As
    /// Linux's `flock_lock_inode`, the lock this description held (if any)
    /// was dropped first (`released`), so two holders of `LOCK_SH` both
    /// asking for `LOCK_EX` can't wait for each other for good.
    WouldBlock { released: bool },
}

/// Result of an `F_SETLK`/`F_SETLKW` attempt that didn't fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "a release must be followed by waking the blocked lockers"]
pub enum RecordLockOutcome {
    /// Lock taken, changed or given up. `released`: the request gave up or
    /// weakened a range this pid held (an `F_UNLCK` over a held range, an
    /// `F_RDLCK` over a held write lock), so waiters may now get theirs.
    Granted { released: bool },
    /// `F_SETLKW` conflicts: wait and retry. `holder` is the pid owning the
    /// first conflicting lock (the one `F_GETLK` would report).
    WouldBlock { holder: u32 },
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
    /// `file_id` intact (a deviation: Linux drops it first, as it does for a
    /// blocking request, which this does too: see [`FlockOutcome::WouldBlock`]).
    pub fn flock(&mut self, file_id: OpenFileId, op: FlockOp) -> Result<FlockOutcome, VfsError> {
        let Some(req_kind) = op.kind else {
            return Ok(FlockOutcome::Granted { released: self.release_flock(file_id) });
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
            return Ok(FlockOutcome::WouldBlock { released: self.release_flock(file_id) });
        }

        let released = if let Some((_, kind)) = self.flocks.iter_mut().find(|(id, _)| *id == file_id) {
            let downgrade = *kind == FlockKind::Exclusive && req_kind == FlockKind::Shared;
            *kind = req_kind;
            downgrade
        } else {
            self.flocks.push((file_id, req_kind));
            false
        };
        Ok(FlockOutcome::Granted { released })
    }

    /// Release any `flock(2)` held by `file_id` (e.g. on last FD close).
    /// Returns `true` if a lock was present and removed: the caller must
    /// then wake the blocked lockers.
    #[must_use = "a release must be followed by waking the blocked lockers"]
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

        match self.first_conflict(pid, start, end, req_kind) {
            Some((other_pid, lock)) => {
                spec.l_type = lock.kind.to_raw();
                spec.l_whence = fcntl_const::SEEK_SET;
                spec.l_start = lock.start;
                spec.l_len = if lock.end == i64::MAX { 0 } else { lock.end - lock.start + 1 };
                spec.l_pid = other_pid as i32;
            }
            None => spec.l_type = fcntl_const::F_UNLCK,
        }
        Ok(())
    }

    /// The first lock (in pid order, then by start) another pid than `pid`
    /// holds over `[start, end]` that conflicts with `kind`, with its owner.
    fn first_conflict(&self, pid: u32, start: i64, end: i64, kind: RangeLockKind) -> Option<(u32, PosixRangeLock)> {
        self.posix.iter().filter(|&(&other_pid, _)| other_pid != pid).find_map(|(&other_pid, locks)| {
            locks
                .iter()
                .find(|lock| lock.overlaps(start, end) && kind.conflicts_with(lock.kind))
                .map(|&l| (other_pid, l))
        })
    }

    /// Apply `F_SETLK` (`blocking = false`) or `F_SETLKW` (`blocking = true`).
    ///
    /// Checked in Linux's order (`flock64_to_posix_lock`, then
    /// `check_fmode_for_setlk`): the range first (`Inval` for a bad
    /// `l_whence` or a start before 0, `Overflow` past the largest offset),
    /// then the lock type (`Inval`), then the access mode (`BadF`;
    /// `F_UNLCK` needs none). Callers must reject `O_PATH` descriptors with
    /// `-EBADF` beforehand.
    pub fn setlk(
        &mut self,
        pid: u32,
        spec: FlockSpec,
        ctx: LockContext,
        blocking: bool,
    ) -> Result<RecordLockOutcome, VfsError> {
        let (start, end) = normalize_range(&spec, ctx.cur_offset, ctx.file_size)?;
        let lock_type = PosixLockType::from_raw(spec.l_type)?;
        match lock_type {
            PosixLockType::Read if !ctx.readable => return Err(VfsError::BadF),
            PosixLockType::Write if !ctx.writable => return Err(VfsError::BadF),
            _ => {}
        }

        // Whether the request weakens what `pid` holds over the range: an
        // unlock of a held range, a read lock over a held write lock.
        let released = self.posix.get(&pid).is_some_and(|locks| {
            locks.iter().any(|lock| {
                lock.overlaps(start, end)
                    && match lock_type {
                        PosixLockType::Unlock => true,
                        PosixLockType::Read => lock.kind == RangeLockKind::Write,
                        PosixLockType::Write => false,
                    }
            })
        });

        let req_kind = match lock_type {
            PosixLockType::Unlock => {
                if let Some(locks) = self.posix.get_mut(&pid) {
                    subtract_range(locks, start, end);
                    if locks.is_empty() {
                        self.posix.remove(&pid);
                    }
                }
                return Ok(RecordLockOutcome::Granted { released });
            }
            PosixLockType::Read => RangeLockKind::Read,
            PosixLockType::Write => RangeLockKind::Write,
        };

        if let Some((holder, _)) = self.first_conflict(pid, start, end, req_kind) {
            if !blocking {
                return Err(VfsError::Again);
            }
            return Ok(RecordLockOutcome::WouldBlock { holder });
        }

        let locks = self.posix.entry(pid).or_default();
        subtract_range(locks, start, end);
        locks.push(PosixRangeLock { start, end, kind: req_kind });
        sort_and_coalesce(locks);

        Ok(RecordLockOutcome::Granted { released })
    }

    /// Release **all** POSIX record locks held by `pid` on this inode
    /// (mandated by POSIX when `pid` closes any FD referring to this inode).
    /// Returns `true` if any locks were removed: the caller must then wake
    /// the blocked lockers.
    #[must_use = "a release must be followed by waking the blocked lockers"]
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

/// `spec`'s range as inclusive `[start, end]` offsets, as Linux's
/// `flock_to_posix_lock` computes it: a bad `l_whence` or a range starting
/// before offset 0 is `Inval`; one reaching past the largest offset
/// (`i64::MAX`) is `Overflow` (`EOVERFLOW`, as Linux and POSIX).
fn normalize_range(spec: &FlockSpec, cur_offset: u64, file_size: u64) -> Result<(i64, i64), VfsError> {
    let base = match spec.l_whence {
        fcntl_const::SEEK_SET => 0i64,
        fcntl_const::SEEK_CUR => i64::try_from(cur_offset).map_err(|_| VfsError::Overflow)?,
        fcntl_const::SEEK_END => i64::try_from(file_size).map_err(|_| VfsError::Overflow)?,
        _ => return Err(VfsError::Inval),
    };

    // `base >= 0`, so only a positive `l_start` can overflow.
    let anchor = base.checked_add(spec.l_start).ok_or(VfsError::Overflow)?;
    if anchor < 0 {
        return Err(VfsError::Inval);
    }

    if spec.l_len == 0 {
        Ok((anchor, i64::MAX))
    } else if spec.l_len > 0 {
        let end = anchor.checked_add(spec.l_len - 1).ok_or(VfsError::Overflow)?;
        Ok((anchor, end))
    } else {
        // `anchor >= 0` and `l_len < 0`: no overflow, maybe below 0.
        let start = anchor + spec.l_len;
        if start < 0 {
            return Err(VfsError::Inval);
        }
        Ok((start, anchor - 1))
    }
}

/// How many other processes' records [`PosixWaitGraph::wait_for`] follows
/// looking for a cycle back to the caller: a cycle through up to this many
/// other processes is found, one through more isn't (the search is then
/// [`WaitSearch::Truncated`]). Linux's `posix_locks_deadlock` checks
/// `i++ > MAX_DEADLK_ITERATIONS` (10) before each comparison, so it
/// compares 11 blockers: the same reach.
pub const MAX_DEADLOCK_CHAIN: usize = 11;

/// `F_SETLKW` would wait in a cycle: `EDEADLK`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadlock;

/// What [`PosixWaitGraph::wait_for`] found, short of a cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "a truncated search may hide a deadlock: say so"]
pub enum WaitSearch {
    /// The chain ended (no record, or a stale one) without reaching the
    /// caller: recorded.
    Clear,
    /// [`MAX_DEADLOCK_CHAIN`] records were followed and the chain went on:
    /// recorded, but a cycle through more processes would hang every one of
    /// them unseen.
    Truncated,
    /// The caller's own view is stale (`seen != now`): neither checked nor
    /// recorded (its old record, if any, removed); its wait ends at once
    /// and it retries.
    StaleCaller,
}

/// A thread blocked in `F_SETLKW`: who it waits for, and the release count
/// its wait ends past.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WaitRecord {
    holder: u32,
    seen: u64,
}

/// A waiting thread's identity, ordered by process and then by thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Waiter {
    pub tgid: u32,
    pub tid: u32,
}

/// Who waits for whom among the threads blocked in `F_SETLKW`, for
/// `EDEADLK`: each waiter's `(tgid, tid)` and the holder it waits for (the
/// owner of the first conflicting lock, [`RecordLockOutcome::WouldBlock`]'s
/// `holder`). Like Linux's `posix_locks_deadlock`, which follows each
/// waiter's single `fl_blocker`, a cycle through a second holder of the
/// same range isn't seen. From a holder process the search follows its
/// lowest-tid current record, so a cycle through another sibling's edge
/// isn't seen either. Lumen chooses the lowest tid; Linux also follows
/// one blocked lock per owner.
///
/// One graph for every inode: the kernel keeps it behind a spinlock (not
/// under any inode's Lock 4), with a count of lock releases. A waiter reads
/// the count (`seen`) before its try, and its wait ends once the count
/// moves past it. Each record is stamped with its waiter's `seen`, and is
/// followed only while that equals the caller's `now` (the count when it
/// checks, passed in): a record made before the last release (its waiter
/// woken since, maybe about to get its lock) is never followed.
#[derive(Debug, Default)]
pub struct PosixWaitGraph {
    waits: BTreeMap<Waiter, WaitRecord>,
}

impl PosixWaitGraph {
    pub const fn new() -> Self {
        Self { waits: BTreeMap::new() }
    }

    /// Thread `waiter` is about to wait for `holder`, its try
    /// having failed with the release count at `seen` (normally `now`):
    /// record it, unless following `holder`'s lowest-tid current record,
    /// its holder's lowest-tid current record, and so
    /// on through at most [`MAX_DEADLOCK_CHAIN`] records leads back to
    /// its tgid (`Err(Deadlock)`, nothing recorded). `Ok` says whether the
    /// search reached the end of the chain ([`WaitSearch::Clear`]) or was
    /// cut off ([`WaitSearch::Truncated`]).
    ///
    /// A caller whose own view is stale (`seen != now`: a release came
    /// after its try) is neither checked nor recorded, and its old record
    /// goes ([`WaitSearch::StaleCaller`]): its wait ends at once and it
    /// retries with a fresh view. Checking it could follow a record made
    /// after that release (by the very holder it saw, now waiting on it in
    /// turn) and report a cycle that its fresh try wouldn't be in.
    pub fn wait_for(&mut self, waiter: Waiter, holder: u32, seen: u64, now: u64) -> Result<WaitSearch, Deadlock> {
        if seen != now {
            self.waits.remove(&waiter);
            return Ok(WaitSearch::StaleCaller);
        }
        // Follow the lowest-numbered currently waiting thread of the
        // holding process, like Linux's first blocked lock for an owner.
        let current = |waits: &BTreeMap<Waiter, WaitRecord>, p: u32| {
            waits
                .range(Waiter { tgid: p, tid: 0 }..=Waiter { tgid: p, tid: u32::MAX })
                .find(|(_, w)| w.seen == now)
                .map(|(_, w)| w.holder)
        };
        // Compare `holder`, then each pid reached by following a record.
        let mut next = holder;
        let mut followed = 0;
        let search = loop {
            if next == waiter.tgid {
                return Err(Deadlock);
            }
            match current(&self.waits, next) {
                None => break WaitSearch::Clear,
                Some(_) if followed == MAX_DEADLOCK_CHAIN => break WaitSearch::Truncated,
                Some(further) => {
                    next = further;
                    followed += 1;
                }
            }
        };
        self.waits.insert(waiter, WaitRecord { holder, seen });
        Ok(search)
    }

    /// Only this thread no longer waits (its lock call returned).
    pub fn done(&mut self, waiter: Waiter) {
        self.waits.remove(&waiter);
    }

    /// Whether this thread has a record, including a stale one (for tests).
    pub fn is_recorded(&self, waiter: Waiter) -> bool {
        self.waits.contains_key(&waiter)
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
