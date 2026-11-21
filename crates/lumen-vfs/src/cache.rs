//! Lock-9 inode and dentry caches.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::fs::VfsInode;
use crate::types::{DevId, Ino};

/// Target `(dev, ino)` stored in [`DentryCache`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DentryTarget {
    pub dev: DevId,
    pub ino: Ino,
}

/// Active inode cache keyed by `(DevId, Ino)`.
///
/// Ensures hard links and multiple open descriptors share the same
/// `Arc<dyn VfsInode>`, `PageCache`, and `AdvisoryLocks`.
#[derive(Default)]
pub struct InodeCache {
    inodes: BTreeMap<(DevId, Ino), Arc<dyn VfsInode>>,
}

impl InodeCache {
    /// Create an empty inode cache.
    pub const fn new() -> Self {
        Self { inodes: BTreeMap::new() }
    }

    /// Look up an inode by `(dev, ino)`.
    pub fn get(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        self.inodes.get(&(dev, ino)).cloned()
    }

    /// Return an existing cached inode for `(inode.dev_id(), inode.ino())`, or
    /// insert and return `inode` (skipping detached inodes).
    pub fn get_or_insert(&mut self, inode: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
        if inode.is_detached() {
            return inode;
        }
        let key = (inode.dev_id(), inode.ino());
        if let Some(existing) = self.inodes.get(&key) {
            return Arc::clone(existing);
        }
        self.inodes.insert(key, Arc::clone(&inode));
        inode
    }

    /// Remove and return the cached inode for `(dev, ino)`, if present.
    pub fn remove(&mut self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        self.inodes.remove(&(dev, ino))
    }

    /// Evict all cached inodes belonging to filesystem `dev`.
    pub fn evict_fs(&mut self, dev: DevId) -> Vec<Arc<dyn VfsInode>> {
        let keys: Vec<(DevId, Ino)> = self.inodes.keys().copied().filter(|&(d, _)| d == dev).collect();
        let mut evicted = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(inode) = self.inodes.remove(&key) {
                evicted.push(inode);
            }
        }
        evicted
    }

    /// Snapshot all currently cached inodes so `sync()` can release Lock 9
    /// before acquiring Locks 4–8.
    pub fn snapshot_all(&self) -> Vec<Arc<dyn VfsInode>> {
        self.inodes.values().cloned().collect()
    }

    /// Number of cached inodes.
    pub fn len(&self) -> usize {
        self.inodes.len()
    }

    /// True if empty.
    pub fn is_empty(&self) -> bool {
        self.inodes.is_empty()
    }
}

type DentryKey = (DevId, Ino, Box<str>);

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

    fn compact_order(&mut self) {
        let entries = &self.entries;
        self.order.retain(|(k, g)| entries.get(k).is_some_and(|&(_, cur_g)| cur_g == *g));
    }

    /// Monotonic invalidation generation incremented on every invalidation or clear.
    pub fn invalidation_gen(&self) -> u64 {
        self.invalidation_gen
    }

    /// Look up a cached child target. Always returns `None` for `.` and `..`.
    pub fn get(&self, parent_dev: DevId, parent_ino: Ino, name: &str) -> Option<DentryTarget> {
        if name == "." || name == ".." || name.is_empty() {
            return None;
        }
        let key: DentryKey = (parent_dev, parent_ino, Box::from(name));
        self.entries.get(&key).map(|&(target, _)| target)
    }

    /// Atomically read a cached child target together with the current
    /// invalidation generation.
    pub fn get_with_gen(&self, parent_dev: DevId, parent_ino: Ino, name: &str) -> (Option<DentryTarget>, u64) {
        (self.get(parent_dev, parent_ino, name), self.invalidation_gen)
    }

    /// Insert or update a dentry mapping only if `invalidation_gen` still matches
    /// `expected_gen`. Returns `true` if `expected_gen` matched.
    pub fn insert_if_gen(
        &mut self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &str,
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
    pub fn insert(&mut self, parent_dev: DevId, parent_ino: Ino, name: &str, target: DentryTarget) {
        if self.max_entries == 0 || name == "." || name == ".." || name.is_empty() {
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
    pub fn invalidate(&mut self, parent_dev: DevId, parent_ino: Ino, name: &str) {
        if name == "." || name == ".." || name.is_empty() {
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
}

impl Default for DentryCache {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAPACITY)
    }
}
