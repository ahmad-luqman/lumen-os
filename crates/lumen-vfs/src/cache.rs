//! Lock-9 inode and dentry caches.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use crate::fs::VfsInode;
use crate::types::{DevId, Ino};

/// Target `(dev, ino)` stored in [`DentryCache`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DentryTarget {
    pub dev: DevId,
    pub ino: Ino,
}

struct CachedInode {
    inode: Arc<dyn VfsInode>,
    inserted_at_gen: u64,
}

/// A removed directory's parent, as Linux keeps a deleted dentry's
/// `d_parent` (#318): `..` from a directory no longer in its filesystem
/// (which has dropped its own `..` with it) still finds the directory it
/// was removed from.
///
/// Keyed by the `Arc` itself, never `(dev, ino)`: a directory removed while
/// open is detached under a synthetic number and a freed one's number is
/// reused. The `Weak` keeps the allocation, so its address is never another
/// inode's while the record lasts. A record whose directory has no strong
/// reference left is dead, and dropped by [`InodeCache::prune_removed_dirs`]
/// (which `VfsTables` runs on each new record and each `..` its filesystem
/// answers `ENOENT`); [`InodeCache::evict_fs`] drops a filesystem's records,
/// live or dead.
struct RemovedDir {
    dir: Weak<dyn VfsInode>,
    /// Held strongly, as `d_parent` holds its dentry: the parent stays
    /// reachable through `..` if it is removed in turn while held open
    /// (detached). One freed by its removal is held too, but `..` refuses
    /// it (`resolve::removed_dir_parent`).
    parent: Arc<dyn VfsInode>,
}

impl RemovedDir {
    fn is(&self, dir: &Arc<dyn VfsInode>) -> bool {
        core::ptr::addr_eq(self.dir.as_ptr(), Arc::as_ptr(dir))
    }
}

/// Active inode cache keyed by `(DevId, Ino)`.
///
/// Ensures hard links and multiple open descriptors share the same
/// `Arc<dyn VfsInode>`, `PageCache`, and `AdvisoryLocks`.
///
/// Tracks a bounded per-`(DevId, Ino)` recent-removal history (`removal_gen` /
/// `recent_removals` / `removal_order`), per-`DevId` overflow watermarks
/// (`dev_overflow_gen`), and per-filesystem `evict_fs` generation
/// (`fs_evict_gen`) so concurrent `unlink`/`rmdir`/`rename`/`umount` races can
/// be detected per key without cross-FS overflow or unrelated invalidations
/// causing `InodeCache` skips or zombie leaks.
///
/// Also keeps each removed directory's parent for `..` (#318, [`RemovedDir`]).
pub struct InodeCache {
    inodes: BTreeMap<(DevId, Ino), CachedInode>,
    removal_gen: u64,
    oldest_tracked_gen: u64,
    max_removal_history: usize,
    recent_removals: BTreeMap<(DevId, Ino), u64>,
    removal_order: VecDeque<((DevId, Ino), u64)>,
    dev_overflow_gen: BTreeMap<DevId, u64>,
    fs_evict_gen: BTreeMap<DevId, u64>,
    pending_removals: BTreeMap<(DevId, Ino), usize>,
    removed_dirs: Vec<RemovedDir>,
}

impl InodeCache {
    /// Default bounded per-key removal tracking capacity.
    pub const DEFAULT_REMOVAL_HISTORY: usize = 4096;

    /// Create an empty inode cache with [`Self::DEFAULT_REMOVAL_HISTORY`].
    pub const fn new() -> Self {
        Self::with_removal_history(Self::DEFAULT_REMOVAL_HISTORY)
    }

    /// Create an empty inode cache tracking up to `max_removal_history`
    /// distinct recent `(DevId, Ino)` removals before falling back
    /// conservatively via per-`DevId` `dev_overflow_gen` (and global
    /// `oldest_tracked_gen`).
    pub const fn with_removal_history(max_removal_history: usize) -> Self {
        Self {
            inodes: BTreeMap::new(),
            removal_gen: 0,
            oldest_tracked_gen: 0,
            max_removal_history,
            recent_removals: BTreeMap::new(),
            removal_order: VecDeque::new(),
            dev_overflow_gen: BTreeMap::new(),
            fs_evict_gen: BTreeMap::new(),
            pending_removals: BTreeMap::new(),
            removed_dirs: Vec::new(),
        }
    }

    #[inline]
    fn gen_gt(a: u64, b: u64) -> bool {
        a != b && a.wrapping_sub(b) < (1u64 << 63)
    }

    /// Monotonic removal generation incremented on every removal or FS eviction.
    pub fn removal_gen(&self) -> u64 {
        self.removal_gen
    }

    /// Oldest removal generation beyond which untracked removals may have
    /// been evicted from the bounded recent-removal history.
    pub fn oldest_tracked_gen(&self) -> u64 {
        self.oldest_tracked_gen
    }

    /// Record a removal for `(dev, ino)` and return the new monotonic `removal_gen`.
    pub fn record_removal(&mut self, dev: DevId, ino: Ino) -> u64 {
        self.removal_gen = self.removal_gen.wrapping_add(1);
        let gen_id = self.removal_gen;
        if self.max_removal_history == 0 {
            self.oldest_tracked_gen = gen_id;
            self.dev_overflow_gen.insert(dev, gen_id);
            return gen_id;
        }
        let key = (dev, ino);
        self.recent_removals.insert(key, gen_id);
        self.removal_order.push_back((key, gen_id));
        while self.recent_removals.len() > self.max_removal_history {
            let Some((old_key, old_gen)) = self.removal_order.pop_front() else {
                self.recent_removals.clear();
                break;
            };
            if self.recent_removals.get(&old_key).is_some_and(|&cur_g| cur_g == old_gen) {
                self.recent_removals.remove(&old_key);
                if Self::gen_gt(old_gen, self.oldest_tracked_gen) {
                    self.oldest_tracked_gen = old_gen;
                }
                let dev_overflow = self.dev_overflow_gen.entry(old_key.0).or_insert(0);
                if Self::gen_gt(old_gen, *dev_overflow) {
                    *dev_overflow = old_gen;
                }
            }
        }
        if self.removal_order.len() >= self.max_removal_history.saturating_mul(2) {
            let recent = &self.recent_removals;
            self.removal_order.retain(|(k, g)| recent.get(k).is_some_and(|&cur_g| cur_g == *g));
        }
        gen_id
    }

    /// If `(dev, ino)` is absent from the cache (or detached / `ino`-mismatched,
    /// in which case it is evicted first), atomically record a removal generation
    /// and return `Ok(gen_id)`. Otherwise return `Err(Arc::clone(&existing))` so
    /// the caller can check `is_inode_dead` outside Lock 9.
    pub fn record_removal_if_absent(&mut self, dev: DevId, ino: Ino) -> Result<u64, Arc<dyn VfsInode>> {
        let key = (dev, ino);
        if let Some(existing) = self.inodes.get(&key) {
            if !existing.inode.is_detached() && existing.inode.ino() == ino {
                return Err(Arc::clone(&existing.inode));
            }
            let _ = self.inodes.remove(&key);
        }
        Ok(self.record_removal(dev, ino))
    }

    fn was_removed_since(&self, dev: DevId, ino: Ino, since_gen: u64) -> bool {
        if self.removal_gen == since_gen {
            return false;
        }
        if self.fs_evict_gen.get(&dev).is_some_and(|&g| Self::gen_gt(g, since_gen)) {
            return true;
        }
        if self.dev_overflow_gen.get(&dev).is_some_and(|&g| Self::gen_gt(g, since_gen)) {
            return true;
        }
        self.recent_removals.get(&(dev, ino)).is_some_and(|&g| Self::gen_gt(g, since_gen))
    }

    /// Look up a live (non-detached, `ino`-matching) inode by `(dev, ino)`.
    pub fn get(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        self.inodes
            .get(&(dev, ino))
            .filter(|c| !c.inode.is_detached() && c.inode.ino() == ino)
            .map(|c| Arc::clone(&c.inode))
    }

    /// Look up a cached inode only if `(dev, ino)` has no in-flight removal and
    /// has not been removed or its filesystem evicted since `since_gen`. Eagerly
    /// evicts any detached or `ino`-mismatched cached entry.
    pub fn get_if_fresh(&mut self, dev: DevId, ino: Ino, since_gen: u64) -> Option<Arc<dyn VfsInode>> {
        let key = (dev, ino);
        if self.inodes.get(&key).is_some_and(|c| c.inode.is_detached() || c.inode.ino() != ino) {
            let _ = self.inodes.remove(&key);
            let _ = self.record_removal(dev, ino);
            return None;
        }
        if self.pending_removals.contains_key(&key) || self.was_removed_since(dev, ino, since_gen) {
            return None;
        }
        self.inodes.get(&key).map(|c| Arc::clone(&c.inode))
    }

    /// Return an existing live cached inode for `(inode.dev_id(), inode.ino())`,
    /// or insert and return `inode` (skipping detached inodes and eagerly
    /// evicting any stale detached/`ino`-mismatched entry).
    pub fn get_or_insert(&mut self, inode: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
        if inode.is_detached() {
            return inode;
        }
        let key = (inode.dev_id(), inode.ino());
        if let Some(existing) = self.inodes.get(&key) {
            if !existing.inode.is_detached() && existing.inode.ino() == key.1 {
                return Arc::clone(&existing.inode);
            }
            let _ = self.inodes.remove(&key);
            let _ = self.record_removal(key.0, key.1);
        }
        self.inodes.insert(key, CachedInode { inode: Arc::clone(&inode), inserted_at_gen: self.removal_gen });
        inode
    }

    /// Unify `inode` into the cache only if it is non-detached, `(dev, ino)`
    /// has no in-flight removal and was not removed/evicted since `since_gen`,
    /// and any existing entry is live and `ino`-matching. Returns `Err(inode)`
    /// on race/staleness so the caller can retry or return `inode` uncached
    /// without aliasing.
    pub fn get_or_insert_if_fresh(
        &mut self,
        inode: Arc<dyn VfsInode>,
        since_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
        if inode.is_detached() {
            return Err(inode);
        }
        let key = (inode.dev_id(), inode.ino());
        if self.pending_removals.contains_key(&key) || self.was_removed_since(key.0, key.1, since_gen) {
            return Err(inode);
        }
        if let Some(existing) = self.inodes.get(&key) {
            if !existing.inode.is_detached() && existing.inode.ino() == key.1 {
                return Ok(Arc::clone(&existing.inode));
            }
            let _ = self.inodes.remove(&key);
            let _ = self.record_removal(key.0, key.1);
            return Err(inode);
        }
        self.inodes.insert(key, CachedInode { inode: Arc::clone(&inode), inserted_at_gen: self.removal_gen });
        Ok(inode)
    }

    /// Unify a newly created `inode` into the cache when `(dev, ino)` has not
    /// been removed/evicted since `since_gen`. Unlike
    /// [`Self::get_or_insert_if_fresh`], allows unification when `since_gen`
    /// already reflects an in-flight `begin_removal` whose freed `ino` was
    /// reallocated to this creator.
    pub fn get_or_insert_created_if_fresh(
        &mut self,
        inode: Arc<dyn VfsInode>,
        since_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
        if inode.is_detached() {
            return Err(inode);
        }
        let key = (inode.dev_id(), inode.ino());
        if self.was_removed_since(key.0, key.1, since_gen) {
            return Err(inode);
        }
        if let Some(existing) = self.inodes.get(&key) {
            if !existing.inode.is_detached() && existing.inode.ino() == key.1 {
                return Ok(Arc::clone(&existing.inode));
            }
            let _ = self.inodes.remove(&key);
            let _ = self.record_removal(key.0, key.1);
            return Err(inode);
        }
        self.inodes.insert(key, CachedInode { inode: Arc::clone(&inode), inserted_at_gen: self.removal_gen });
        Ok(inode)
    }

    /// Mark `(dev, ino)` as undergoing an in-flight removal, evict any cached
    /// entry unless `(dev, ino)` itself was already removed/evicted since
    /// `since_gen` (or `dev` overflowed and the cached entry was inserted after
    /// `since_gen`), and record a pre-mutation removal generation. Must be
    /// paired with [`Self::end_removal`] after the filesystem mutation finishes.
    pub fn begin_removal_if_fresh(&mut self, dev: DevId, ino: Ino, since_gen: u64) -> (Option<Arc<dyn VfsInode>>, u64) {
        let key = (dev, ino);
        *self.pending_removals.entry(key).or_insert(0) += 1;

        let known_key_removed = self.fs_evict_gen.get(&dev).is_some_and(|&g| Self::gen_gt(g, since_gen))
            || self.recent_removals.get(&key).is_some_and(|&g| Self::gen_gt(g, since_gen));
        let dev_overflowed = self.dev_overflow_gen.get(&dev).is_some_and(|&g| Self::gen_gt(g, since_gen));

        let should_evict_existing = match self.inodes.get(&key) {
            None => false,
            Some(existing) => {
                if existing.inode.is_detached() || existing.inode.ino() != ino {
                    true
                } else if known_key_removed {
                    false
                } else if !dev_overflowed {
                    true
                } else {
                    !Self::gen_gt(existing.inserted_at_gen, since_gen)
                }
            }
        };

        if should_evict_existing {
            let removed = self.inodes.remove(&key).map(|c| c.inode);
            let gen_id = self.record_removal(dev, ino);
            return (removed, gen_id);
        }
        if !self.inodes.contains_key(&key) && !known_key_removed {
            let gen_id = self.record_removal(dev, ino);
            return (None, gen_id);
        }
        (None, self.removal_gen)
    }

    /// Complete an in-flight removal for `(dev, ino)` started by
    /// [`Self::begin_removal_if_fresh`], evicting any detached entry and
    /// recording a post-mutation removal generation without evicting a live
    /// newly created inode reusing `(dev, ino)`.
    pub fn end_removal(&mut self, dev: DevId, ino: Ino) -> u64 {
        let key = (dev, ino);
        if let Some(cnt) = self.pending_removals.get_mut(&key) {
            if *cnt <= 1 {
                self.pending_removals.remove(&key);
            } else {
                *cnt -= 1;
            }
        }
        if self.inodes.get(&key).is_some_and(|c| c.inode.is_detached() || c.inode.ino() != ino) {
            let _ = self.inodes.remove(&key);
        }
        self.record_removal(dev, ino)
    }

    /// Evict `(dev, ino)` if its cached entry is detached or `ino`-mismatched.
    pub fn evict_detached(&mut self, dev: DevId, ino: Ino) -> bool {
        let key = (dev, ino);
        if self.inodes.get(&key).is_some_and(|c| c.inode.is_detached() || c.inode.ino() != ino) {
            let _ = self.inodes.remove(&key);
            let _ = self.record_removal(dev, ino);
            return true;
        }
        false
    }

    /// Evict `(dev, ino)` only if the currently cached entry is the exact same
    /// `Arc` instance as `expected`.
    pub fn remove_if_same(&mut self, dev: DevId, ino: Ino, expected: &Arc<dyn VfsInode>) -> Option<Arc<dyn VfsInode>> {
        let key = (dev, ino);
        if self.inodes.get(&key).is_some_and(|c| Arc::ptr_eq(&c.inode, expected)) {
            let removed = self.inodes.remove(&key).map(|c| c.inode);
            let _ = self.record_removal(dev, ino);
            return removed;
        }
        None
    }

    /// Unconditionally record a removal for `(dev, ino)` and remove any
    /// cached inode for `(dev, ino)`.
    pub fn remove(&mut self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        let _ = self.record_removal(dev, ino);
        self.inodes.remove(&(dev, ino)).map(|c| c.inode)
    }

    /// Record a removal and evict `(dev, ino)` unless `(dev, ino)` itself was
    /// already removed or evicted since `since_gen` (or `dev` overflowed and
    /// the cached entry was inserted after `since_gen`).
    pub fn remove_if_fresh(
        &mut self,
        dev: DevId,
        ino: Ino,
        since_gen: u64,
    ) -> Option<(Option<Arc<dyn VfsInode>>, u64)> {
        let key = (dev, ino);
        let known_key_removed = self.fs_evict_gen.get(&dev).is_some_and(|&g| Self::gen_gt(g, since_gen))
            || self.recent_removals.get(&key).is_some_and(|&g| Self::gen_gt(g, since_gen));
        let dev_overflowed = self.dev_overflow_gen.get(&dev).is_some_and(|&g| Self::gen_gt(g, since_gen));

        let should_evict_existing = match self.inodes.get(&key) {
            None => false,
            Some(existing) => {
                if existing.inode.is_detached() || existing.inode.ino() != ino {
                    true
                } else if known_key_removed {
                    false
                } else if !dev_overflowed {
                    true
                } else {
                    !Self::gen_gt(existing.inserted_at_gen, since_gen)
                }
            }
        };

        if should_evict_existing {
            let removed = self.inodes.remove(&key).map(|c| c.inode);
            let gen_id = self.record_removal(dev, ino);
            return Some((removed, gen_id));
        }
        if !self.inodes.contains_key(&key) && !known_key_removed && !dev_overflowed {
            let gen_id = self.record_removal(dev, ino);
            return Some((None, gen_id));
        }
        None
    }

    /// Evict all cached inodes belonging to filesystem `dev` and bump
    /// `fs_evict_gen`, and drop its removed directories' records (#318),
    /// their parents returned with the evicted inodes.
    pub fn evict_fs(&mut self, dev: DevId) -> Vec<Arc<dyn VfsInode>> {
        self.removal_gen = self.removal_gen.wrapping_add(1);
        self.fs_evict_gen.insert(dev, self.removal_gen);
        let keys: Vec<(DevId, Ino)> = self.inodes.keys().copied().filter(|&(d, _)| d == dev).collect();
        let mut evicted = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(c) = self.inodes.remove(&key) {
                evicted.push(c.inode);
            }
        }
        // A removed directory and its parent are on one filesystem (`rmdir`
        // and `rename` never cross one), so the parent's device is both's.
        self.removed_dirs.retain(|r| {
            if r.parent.dev_id() == dev {
                evicted.push(Arc::clone(&r.parent));
                false
            } else {
                true
            }
        });
        evicted
    }

    /// Remember `parent` as `dir`'s for `..` once `dir` is removed (#318;
    /// [`RemovedDir`]), replacing any record of `dir` already there, and
    /// prune the dead records ([`Self::prune_removed_dirs`]). Called before
    /// the filesystem removes `dir`, under its rename lock (ext4's and
    /// tmpfs's; so `dir` can't move meanwhile), and undone by
    /// [`Self::forget_removed_dir`] if the removal fails with `dir` still
    /// linked. Returns the parents of the records it dropped, for the
    /// caller to drop after releasing Lock 9.
    #[must_use = "the dropped records' parents go after Lock 9 is released"]
    pub fn record_removed_dir(&mut self, dir: &Arc<dyn VfsInode>, parent: Arc<dyn VfsInode>) -> Vec<Arc<dyn VfsInode>> {
        let mut dropped = self.prune_removed_dirs();
        dropped.extend(self.forget_removed_dir(dir));
        self.removed_dirs.push(RemovedDir { dir: Arc::downgrade(dir), parent });
        dropped
    }

    /// Drop the records whose directory has no strong reference left,
    /// returning their parents for the caller to drop after releasing Lock
    /// 9. A parent that was itself a removed directory kept alive only by
    /// such a record dies with that drop, so its own record is dead only
    /// afterwards: the caller prunes again until nothing is dropped.
    #[must_use = "the dropped records' parents go after Lock 9 is released"]
    pub fn prune_removed_dirs(&mut self) -> Vec<Arc<dyn VfsInode>> {
        let mut dropped = Vec::new();
        self.removed_dirs.retain(|r| {
            if r.dir.strong_count() == 0 {
                dropped.push(Arc::clone(&r.parent));
                false
            } else {
                true
            }
        });
        dropped
    }

    /// Drop the record [`Self::record_removed_dir`] made of `dir`, returning
    /// its parent for the caller to drop after releasing Lock 9.
    #[must_use = "the record's parent goes after Lock 9 is released"]
    pub fn forget_removed_dir(&mut self, dir: &Arc<dyn VfsInode>) -> Option<Arc<dyn VfsInode>> {
        let at = self.removed_dirs.iter().position(|r| r.is(dir))?;
        Some(self.removed_dirs.swap_remove(at).parent)
    }

    /// The directory `dir` was removed from, if a record of it is kept: live,
    /// detached, or freed by now (the caller checks, as
    /// `resolve::removed_dir_parent` does).
    pub fn removed_dir_parent(&self, dir: &Arc<dyn VfsInode>) -> Option<Arc<dyn VfsInode>> {
        self.removed_dirs.iter().find(|r| r.is(dir)).map(|r| Arc::clone(&r.parent))
    }

    /// How many removed directories' records are kept, dead ones included.
    #[cfg(test)]
    pub(crate) fn removed_dirs_len(&self) -> usize {
        self.removed_dirs.len()
    }

    /// Snapshot all currently cached inodes so `sync()` can release Lock 9
    /// before acquiring Locks 4–8.
    pub fn snapshot_all(&self) -> Vec<Arc<dyn VfsInode>> {
        self.inodes.values().map(|c| Arc::clone(&c.inode)).collect()
    }

    /// Number of cached inodes.
    pub fn len(&self) -> usize {
        self.inodes.len()
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.inodes.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn removal_order_len(&self) -> usize {
        self.removal_order.len()
    }
}

impl Default for InodeCache {
    fn default() -> Self {
        Self::new()
    }
}

type DentryKey = (DevId, Ino, Box<[u8]>);

/// Bounded dentry cache keyed by `(parent_dev, parent_ino, component_name)`.
///
/// Never caches `"."` or `".."`, and provides immediate invalidation on
/// `unlink`, `rmdir`, `rename`, `mount`, and `umount`.
pub struct DentryCache {
    max_entries: usize,
    next_gen: u64,
    invalidation_gen: u64,
    entries: BTreeMap<DentryKey, (DentryTarget, u64)>,
    order: VecDeque<(DentryKey, u64)>,
}

impl DentryCache {
    /// Default bounded capacity.
    pub const DEFAULT_CAPACITY: usize = 4096;

    /// Create a dentry cache bounded to `max_entries`.
    pub const fn new(max_entries: usize) -> Self {
        Self { max_entries, next_gen: 0, invalidation_gen: 0, entries: BTreeMap::new(), order: VecDeque::new() }
    }

    /// Create a dentry cache bounded to [`Self::DEFAULT_CAPACITY`].
    pub const fn with_default_capacity() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }

    fn compact_order(&mut self) {
        let entries = &self.entries;
        self.order.retain(|(k, g)| entries.get(k).is_some_and(|&(_, cur_g)| cur_g == *g));
    }

    /// Monotonic invalidation generation incremented on every invalidation or clear.
    pub fn invalidation_gen(&self) -> u64 {
        self.invalidation_gen
    }

    /// Look up a cached child target. Always returns `None` for `.` and `..`.
    pub fn get(&self, parent_dev: DevId, parent_ino: Ino, name: &[u8]) -> Option<DentryTarget> {
        if name == b"." || name == b".." || name.is_empty() {
            return None;
        }
        let key: DentryKey = (parent_dev, parent_ino, Box::from(name));
        self.entries.get(&key).map(|&(target, _)| target)
    }

    /// Atomically read a cached child target together with the current
    /// invalidation generation.
    pub fn get_with_gen(&self, parent_dev: DevId, parent_ino: Ino, name: &[u8]) -> (Option<DentryTarget>, u64) {
        (self.get(parent_dev, parent_ino, name), self.invalidation_gen)
    }

    /// Insert or update a dentry mapping only if `invalidation_gen` still matches
    /// `expected_gen`. Returns `true` if `expected_gen` matched.
    pub fn insert_if_gen(
        &mut self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &[u8],
        target: DentryTarget,
        expected_gen: u64,
    ) -> bool {
        if self.invalidation_gen != expected_gen {
            return false;
        }
        self.insert(parent_dev, parent_ino, name, target);
        true
    }

    /// Insert or update a dentry mapping. Never caches `.`, `..`, or empty names.
    pub fn insert(&mut self, parent_dev: DevId, parent_ino: Ino, name: &[u8], target: DentryTarget) {
        if self.max_entries == 0 || name == b"." || name == b".." || name.is_empty() {
            return;
        }
        let key: DentryKey = (parent_dev, parent_ino, Box::from(name));
        if let Some(existing) = self.entries.get_mut(&key) {
            existing.0 = target;
            return;
        }
        while self.entries.len() >= self.max_entries {
            let Some((oldest_key, oldest_gen)) = self.order.pop_front() else {
                self.entries.clear();
                break;
            };
            if self.entries.get(&oldest_key).is_some_and(|&(_, cur_g)| cur_g == oldest_gen) {
                self.entries.remove(&oldest_key);
            }
        }
        if self.order.len() >= self.max_entries.saturating_mul(2) {
            self.compact_order();
        }
        let gen_id = self.next_gen;
        self.next_gen = self.next_gen.wrapping_add(1);
        self.order.push_back((key.clone(), gen_id));
        self.entries.insert(key, (target, gen_id));
    }

    /// Invalidate a single `(parent_dev, parent_ino, name)` entry.
    pub fn invalidate(&mut self, parent_dev: DevId, parent_ino: Ino, name: &[u8]) {
        if name == b"." || name == b".." || name.is_empty() {
            return;
        }
        self.invalidation_gen = self.invalidation_gen.wrapping_add(1);
        let key: DentryKey = (parent_dev, parent_ino, Box::from(name));
        self.entries.remove(&key);
    }

    /// Invalidate all cached children under `(parent_dev, parent_ino)`.
    pub fn invalidate_dir(&mut self, parent_dev: DevId, parent_ino: Ino) {
        self.invalidation_gen = self.invalidation_gen.wrapping_add(1);
        let keys: Vec<DentryKey> =
            self.entries.keys().filter(|(d, i, _)| *d == parent_dev && *i == parent_ino).cloned().collect();
        for key in keys {
            self.entries.remove(&key);
        }
    }

    /// Invalidate all entries whose parent or target belongs to filesystem `dev`.
    pub fn invalidate_fs(&mut self, dev: DevId) {
        self.invalidation_gen = self.invalidation_gen.wrapping_add(1);
        let keys: Vec<DentryKey> = self
            .entries
            .iter()
            .filter(|((pd, _, _), (target, _))| *pd == dev || target.dev == dev)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            self.entries.remove(&key);
        }
    }

    /// Number of currently cached dentries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Remove all cached dentries.
    pub fn clear(&mut self) {
        self.invalidation_gen = self.invalidation_gen.wrapping_add(1);
        self.entries.clear();
        self.order.clear();
    }

    #[cfg(test)]
    pub(crate) fn order_len(&self) -> usize {
        self.order.len()
    }

    #[cfg(test)]
    pub(crate) fn snapshot_all(&self) -> Vec<(DevId, Ino, Vec<u8>, DentryTarget)> {
        self.entries.iter().map(|((pdev, pino, name), &(target, _))| (*pdev, *pino, name.to_vec(), target)).collect()
    }
}

impl Default for DentryCache {
    fn default() -> Self {
        Self::with_default_capacity()
    }
}
