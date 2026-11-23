//! Mount table, short-held Lock 9 cache probe trait, and the Lock-9 tables
//! (`VfsTables`) behind it.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::cache::{DentryCache, DentryTarget, InodeCache};
use crate::error::VfsError;
use crate::fd::{LockFamily, VfsLock};
use crate::fs::{FileSystem, VfsInode};
use crate::types::{DevId, Ino, InodeType};

/// Summary of an active mount for `/proc/mounts` and the `mount` command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountInfo {
    pub dev_id: DevId,
    pub fs_name: &'static str,
    pub mount_path: String,
    pub read_only: bool,
}

impl MountInfo {
    /// `fs` mounted at `mount_path`, asking `fs` for its device, name and
    /// read-only flag: call it only with Lock 9 released.
    pub(crate) fn of(fs: &dyn FileSystem, mount_path: String) -> Self {
        Self { dev_id: fs.dev_id(), fs_name: fs.name(), mount_path, read_only: fs.read_only() }
    }
}

struct MountRecord {
    mount_id: u64,
    fs: Arc<dyn FileSystem>,
    fs_root_ino: Ino,
    parent_mountpoint: Option<Arc<dyn VfsInode>>,
    mount_path: String,
}

/// Global mount table tracking root and submounts, parent-mountpoint `..`
/// crossing, and device IDs.
pub struct MountTable {
    next_mount_id: u64,
    root: Option<MountRecord>,
    /// Keyed by `(mountpoint_dev, mountpoint_ino)`.
    mounts: BTreeMap<(DevId, Ino), MountRecord>,
    /// Index from mounted `fs.dev_id()` to `(mountpoint_dev, mountpoint_ino)`.
    dev_to_mp: BTreeMap<DevId, (DevId, Ino)>,
}

impl Default for MountTable {
    fn default() -> Self {
        Self::new()
    }
}

impl MountTable {
    /// Create an empty mount table.
    pub const fn new() -> Self {
        Self { next_mount_id: 1, root: None, mounts: BTreeMap::new(), dev_to_mp: BTreeMap::new() }
    }

    fn alloc_mount_id(&mut self) -> u64 {
        let id = self.next_mount_id;
        self.next_mount_id = self.next_mount_id.wrapping_add(1);
        id
    }

    /// Mount the global root filesystem `/` using a pre-fetched `fs_root_ino`
    /// so callers holding Lock 9 never invoke `FileSystem::root_inode()`.
    pub fn mount_root_with_ino(&mut self, fs: Arc<dyn FileSystem>, fs_root_ino: Ino) -> Result<(), VfsError> {
        if self.root.is_some() {
            return Err(VfsError::Busy);
        }
        let dev = fs.dev_id();
        if self.dev_to_mp.contains_key(&dev) {
            return Err(VfsError::Busy);
        }
        let mount_id = self.alloc_mount_id();
        self.root =
            Some(MountRecord { mount_id, fs, fs_root_ino, parent_mountpoint: None, mount_path: String::from("/") });
        Ok(())
    }

    /// Mount the global root filesystem `/`.
    pub fn mount_root(&mut self, fs: Arc<dyn FileSystem>) -> Result<(), VfsError> {
        let fs_root_ino = fs.root_inode().ino();
        self.mount_root_with_ino(fs, fs_root_ino)
    }

    /// Mount `fs` on directory `mountpoint` at `path` using a pre-fetched
    /// `fs_root_ino` so callers holding Lock 9 never invoke `FileSystem::root_inode()`.
    pub fn mount_with_ino(
        &mut self,
        mountpoint: Arc<dyn VfsInode>,
        path: &str,
        fs: Arc<dyn FileSystem>,
        fs_root_ino: Ino,
    ) -> Result<(), VfsError> {
        if self.root.is_none() {
            return Err(VfsError::NoEnt);
        }
        if mountpoint.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        let fs_dev = fs.dev_id();
        if self.root.as_ref().is_some_and(|r| r.fs.dev_id() == fs_dev) || self.dev_to_mp.contains_key(&fs_dev) {
            return Err(VfsError::Busy);
        }
        let mp_key = (mountpoint.dev_id(), mountpoint.ino());
        if self.mounts.contains_key(&mp_key) {
            return Err(VfsError::Busy);
        }
        let mount_id = self.alloc_mount_id();
        self.dev_to_mp.insert(fs_dev, mp_key);
        self.mounts.insert(
            mp_key,
            MountRecord {
                mount_id,
                fs,
                fs_root_ino,
                parent_mountpoint: Some(mountpoint),
                mount_path: String::from(path),
            },
        );
        Ok(())
    }

    /// Mount `fs` on directory `mountpoint` at `path`.
    pub fn mount(
        &mut self,
        mountpoint: Arc<dyn VfsInode>,
        path: &str,
        fs: Arc<dyn FileSystem>,
    ) -> Result<(), VfsError> {
        let fs_root_ino = fs.root_inode().ino();
        self.mount_with_ino(mountpoint, path, fs, fs_root_ino)
    }

    /// Unmount the filesystem mounted on `(mountpoint_dev, mountpoint_ino)`.
    /// Returns `Err(Busy)` if any child filesystem is mounted beneath it.
    pub fn umount(&mut self, mountpoint_dev: DevId, mountpoint_ino: Ino) -> Result<Arc<dyn FileSystem>, VfsError> {
        let mp_key = (mountpoint_dev, mountpoint_ino);
        let Some(rec) = self.mounts.get(&mp_key) else {
            return Err(VfsError::Inval);
        };
        let mounted_dev = rec.fs.dev_id();
        let has_child_mount = self.mounts.iter().any(|(&(parent_dev, _), _)| parent_dev == mounted_dev);
        if has_child_mount {
            return Err(VfsError::Busy);
        }
        let rec = self.mounts.remove(&mp_key).ok_or(VfsError::Inval)?;
        self.dev_to_mp.remove(&mounted_dev);
        Ok(rec.fs)
    }

    /// Return the global root filesystem, if mounted.
    pub fn root_fs(&self) -> Option<Arc<dyn FileSystem>> {
        self.root.as_ref().map(|r| Arc::clone(&r.fs))
    }

    /// Look up a mounted filesystem by its `DevId`.
    pub fn fs_by_dev(&self, dev: DevId) -> Option<Arc<dyn FileSystem>> {
        if let Some(root) = &self.root
            && root.fs.dev_id() == dev
        {
            return Some(Arc::clone(&root.fs));
        }
        let mp_key = self.dev_to_mp.get(&dev)?;
        self.mounts.get(mp_key).map(|r| Arc::clone(&r.fs))
    }

    /// If `(dev, ino)` is a mountpoint, return the mounted filesystem.
    pub fn mount_at(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn FileSystem>> {
        self.mounts.get(&(dev, ino)).map(|r| Arc::clone(&r.fs))
    }

    /// If `(dev, ino)` is a mountpoint, return the mounted filesystem and its unique `mount_id`.
    pub fn mount_at_with_id(&self, dev: DevId, ino: Ino) -> Option<(Arc<dyn FileSystem>, u64)> {
        self.mounts.get(&(dev, ino)).map(|r| (Arc::clone(&r.fs), r.mount_id))
    }

    /// True if `(mp_dev, mp_ino)` currently hosts the exact mount instance `mount_id`.
    pub fn has_mount_id(&self, mp_dev: DevId, mp_ino: Ino, mount_id: u64) -> bool {
        self.mounts.get(&(mp_dev, mp_ino)).is_some_and(|r| r.mount_id == mount_id)
    }

    /// True if `dev` is currently mounted (either as global `/` or a submount).
    pub fn is_dev_mounted(&self, dev: DevId) -> bool {
        self.root.as_ref().is_some_and(|r| r.fs.dev_id() == dev) || self.dev_to_mp.contains_key(&dev)
    }

    /// Return the underlying parent mountpoint directory for `mounted_dev`.
    pub fn parent_mount(&self, mounted_dev: DevId) -> Option<Arc<dyn VfsInode>> {
        let mp_key = self.dev_to_mp.get(&mounted_dev)?;
        self.mounts.get(mp_key).and_then(|r| r.parent_mountpoint.as_ref().map(Arc::clone))
    }

    /// If `(dev, ino)` is the root inode of a non-root mount, return its parent
    /// mountpoint directory for `..` crossing.
    pub fn parent_mount_if_root(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        let mp_key = self.dev_to_mp.get(&dev)?;
        let rec = self.mounts.get(mp_key)?;
        if rec.fs_root_ino == ino { rec.parent_mountpoint.as_ref().map(Arc::clone) } else { None }
    }

    /// True if `(dev, ino)` is a mountpoint for a mounted child filesystem.
    pub fn is_mountpoint(&self, dev: DevId, ino: Ino) -> bool {
        self.mounts.contains_key(&(dev, ino))
    }

    /// True if `(dev, ino)` is the global root `/` inode.
    pub fn is_global_root(&self, dev: DevId, ino: Ino) -> bool {
        self.root.as_ref().is_some_and(|r| r.fs.dev_id() == dev && r.fs_root_ino == ino)
    }

    /// Snapshot all mounted filesystems so `sync()` can release Lock 9 before I/O.
    pub fn snapshot_filesystems(&self) -> Vec<Arc<dyn FileSystem>> {
        let mut out = Vec::with_capacity(self.mounts.len() + usize::from(self.root.is_some()));
        if let Some(root) = &self.root {
            out.push(Arc::clone(&root.fs));
        }
        for rec in self.mounts.values() {
            out.push(Arc::clone(&rec.fs));
        }
        out
    }

    /// Every mount's filesystem and path, `/` first, for `/proc/mounts` and
    /// `mount`: the caller asks the filesystems for the rest after releasing
    /// Lock 9. As with [`snapshot_filesystems`](Self::snapshot_filesystems),
    /// if `umount` runs meanwhile, the caller may drop a filesystem's last
    /// reference.
    pub fn snapshot_mounts(&self) -> Vec<(Arc<dyn FileSystem>, String)> {
        let mut out = Vec::with_capacity(self.mounts.len() + usize::from(self.root.is_some()));
        for rec in self.root.iter().chain(self.mounts.values()) {
            out.push((Arc::clone(&rec.fs), rec.mount_path.clone()));
        }
        out
    }
}

/// Short-held Lock 9 probe interface used by `resolve` and `ops`.
///
/// Every method acquires only Lock 9 (`MountTable`, `DentryCache`, and/or
/// `InodeCache` in strict `MountTable -> DentryCache -> InodeCache` sub-order),
/// returns an owned `Arc` or `Copy` value, and releases the Lock 9 guard(s)
/// before returning so Locks 4–8 are never acquired while holding Lock 9.
///
/// No method taking an inode inserts one whose [`VfsInode::cacheable`] is
/// false into either cache: each one that would hands it back as its
/// success value (`Ok(inode)`, `Some(inode)` or `inode`), unified with
/// nothing. (`dentry_insert` and `dentry_insert_if_gen` take a bare
/// [`DentryTarget`] and can't check. `ops`'s re-publish of a created inode
/// calls them only when the inode was unified with an already cached one,
/// which a non-cacheable inode never is; `link_at` is safe only because no
/// writable filesystem has non-cacheable inodes.)
pub trait VfsCacheOps {
    fn root_inode(&self) -> Result<Arc<dyn VfsInode>, VfsError>;
    fn is_global_root(&self, dev: DevId, ino: Ino) -> bool;
    fn mount_at(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn FileSystem>>;
    fn mount_at_with_id(&self, dev: DevId, ino: Ino) -> Option<(Arc<dyn FileSystem>, u64)>;
    fn parent_mount(&self, mounted_dev: DevId) -> Option<Arc<dyn VfsInode>>;
    fn parent_mount_if_root(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>>;
    fn is_mountpoint(&self, dev: DevId, ino: Ino) -> bool;
    fn fs_read_only(&self, dev: DevId) -> bool;
    fn fs_by_dev(&self, dev: DevId) -> Option<Arc<dyn FileSystem>>;

    fn cache_gens(&self) -> (u64, u64);
    fn dentry_lookup_inode(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &[u8],
    ) -> (Option<Arc<dyn VfsInode>>, u64, u64);
    /// Unify a looked-up inode in `InodeCache` and publish its dentry under Lock 9.
    /// Implementations must insert into `DentryCache` only when
    /// `!mounts.is_mountpoint(parent_dev, parent_ino)` (so an active mountpoint's
    /// shadowed children never pollute `DentryCache`), and must return a
    /// mounted inode whose [`VfsInode::cacheable`] is false as `Ok(inode)`,
    /// inserting it into neither cache (`Err` would make callers retry).
    fn publish_lookup_inode(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &[u8],
        inode: Arc<dyn VfsInode>,
        expected_dentry_gen: u64,
        expected_inode_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>>;
    /// Unify a newly created inode in `InodeCache` and publish its dentry
    /// under Lock 9. Implementations must insert into `DentryCache` only when
    /// `!mounts.is_mountpoint(parent_dev, parent_ino)` and
    /// `(inode.dev_id(), inode.ino())` was not already cached in `InodeCache`
    /// prior to `get_or_insert_created_if_fresh`, so callers outside Lock 9
    /// can verify a pre-existing cached `Arc` is alive before caching its dentry.
    fn publish_created_inode(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &[u8],
        inode: Arc<dyn VfsInode>,
        expected_dentry_gen: u64,
        expected_inode_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>>;

    fn dentry_gen(&self) -> u64;
    fn dentry_get(&self, parent_dev: DevId, parent_ino: Ino, name: &[u8]) -> Option<DentryTarget>;
    fn dentry_get_with_gen(&self, parent_dev: DevId, parent_ino: Ino, name: &[u8]) -> (Option<DentryTarget>, u64);
    fn dentry_insert(&self, parent_dev: DevId, parent_ino: Ino, name: &[u8], target: DentryTarget);
    /// Insert into `DentryCache` if `invalidation_gen == expected_gen` and
    /// `!mounts.is_mountpoint(parent_dev, parent_ino)` (acquiring `MountTable`
    /// before `DentryCache` per Lock-9 sub-order).
    fn dentry_insert_if_gen(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &[u8],
        target: DentryTarget,
        expected_gen: u64,
    ) -> bool;
    fn dentry_invalidate(&self, parent_dev: DevId, parent_ino: Ino, name: &[u8]);
    fn dentry_invalidate_dir(&self, parent_dev: DevId, parent_ino: Ino);

    fn inode_get(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>>;
    fn inode_get_if_fresh(&self, dev: DevId, ino: Ino, since_gen: u64) -> Option<Arc<dyn VfsInode>>;
    fn inode_get_or_insert(&self, inode: Arc<dyn VfsInode>) -> Arc<dyn VfsInode>;
    fn inode_get_or_insert_if_mounted(
        &self,
        mp_dev: DevId,
        mp_ino: Ino,
        mount_id: u64,
        inode: Arc<dyn VfsInode>,
    ) -> Option<Arc<dyn VfsInode>>;
    /// Unify `inode` in `InodeCache` if fresh (`InodeCache::get_or_insert_if_fresh`).
    /// Like [`Self::publish_lookup_inode`], a mounted inode whose
    /// [`VfsInode::cacheable`] is false comes back as `Ok(inode)`, uncached.
    fn inode_get_or_insert_if_fresh(
        &self,
        inode: Arc<dyn VfsInode>,
        since_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>>;
    fn inode_remove(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>>;
    fn inode_remove_if_fresh(&self, dev: DevId, ino: Ino, since_gen: u64) -> Option<(Option<Arc<dyn VfsInode>>, u64)>;
    fn inode_begin_removal(&self, dev: DevId, ino: Ino, since_gen: u64) -> (Option<Arc<dyn VfsInode>>, u64);
    fn inode_end_removal(&self, dev: DevId, ino: Ino) -> u64;
    fn inode_evict_detached(&self, dev: DevId, ino: Ino) -> bool;
    fn inode_remove_if_same(&self, dev: DevId, ino: Ino, expected: &Arc<dyn VfsInode>) -> Option<Arc<dyn VfsInode>>;
    fn inode_record_removal(&self, dev: DevId, ino: Ino) -> u64;
    fn inode_record_removal_if_absent(&self, dev: DevId, ino: Ino) -> Result<u64, Arc<dyn VfsInode>>;
}

/// The Lock-9 VFS lookup tables (#264): the kernel's (`vfs::KernelVfsTables`,
/// over `SleepMutex`) and the host tests' alike, generic over the
/// [`LockFamily`] that guards each table.
///
/// Strict sub-order `MountTable -> DentryCache -> InodeCache`. Never holds
/// any of the three across `FileSystem::root_inode()`, `read_only()` and
/// `name()` (`fs_read_only`, `list_mounts`), Locks 1–8, or the drops of the
/// inodes `umount` evicts. Some calls into the filesystems still run under
/// them: `VfsInode::dev_id`, `ino` and `cacheable` and `FileSystem::dev_id`
/// (fixed state, by design), `VfsInode::is_detached` (an atomic load in
/// ext4 and tmpfs, but not fixed state: a detach sets it), and #320's
/// pre-existing cases
/// (`MountTable::mount_with_ino`'s `inode_type()` and its error paths'
/// `Arc` drops, the unmounted mountpoint's `Arc` dropped inside
/// `MountTable::umount`, the `Arc` each of `InodeCache`'s `get_or_insert*`
/// is handed and drops when it finds a live entry, and the stale (detached
/// or `ino`-mismatched) entries `InodeCache` drops in `get_if_fresh`, every
/// `get_or_insert*`, `end_removal`, `evict_detached` and
/// `record_removal_if_absent`).
pub struct VfsTables<F: LockFamily> {
    mounts: F::Lock<MountTable>,
    dentries: F::Lock<DentryCache>,
    inodes: F::Lock<InodeCache>,
}

impl<F: LockFamily> VfsTables<F> {
    /// Tables over the given locks, which must hold empty tables (seeded
    /// ones would bypass the invariants `mount_root`, `mount` and `umount`
    /// keep across the three, such as the root inode being cached): the way
    /// to a `static`, since [`VfsLock::new`] isn't `const` (the kernel
    /// passes locks built by `SleepMutex::new`, a `const fn`, around
    /// [`MountTable::new`], [`DentryCache::with_default_capacity`] and
    /// [`InodeCache::new`], the constructors [`new`](Self::new) uses).
    pub const fn from_locks(
        mounts: F::Lock<MountTable>,
        dentries: F::Lock<DentryCache>,
        inodes: F::Lock<InodeCache>,
    ) -> Self {
        Self { mounts, dentries, inodes }
    }

    /// Empty tables: [`DentryCache::DEFAULT_CAPACITY`] names and
    /// [`InodeCache::DEFAULT_REMOVAL_HISTORY`] removals remembered, from the
    /// same argument-free constructors the kernel's `static` hands
    /// [`from_locks`](Self::from_locks), so neither names a capacity.
    pub fn new() -> Self {
        Self::from_locks(
            F::Lock::new(MountTable::new()),
            F::Lock::new(DentryCache::with_default_capacity()),
            F::Lock::new(InodeCache::new()),
        )
    }

    /// Mount `fs` at `/` and seed its root inode into `InodeCache`.
    pub fn mount_root(&self, fs: Arc<dyn FileSystem>) -> Result<(), VfsError> {
        let root = fs.root_inode();
        let root_ino = root.ino();
        self.mounts.lock().mount_root_with_ino(fs, root_ino)?;
        let _ = self.inodes.lock().get_or_insert(root);
        Ok(())
    }

    /// Mount `fs` at `mountpoint` (`path`), invalidating stale dentries at `mountpoint`.
    pub fn mount(&self, mountpoint: Arc<dyn VfsInode>, path: &str, fs: Arc<dyn FileSystem>) -> Result<(), VfsError> {
        let mp_dev = mountpoint.dev_id();
        let mp_ino = mountpoint.ino();
        let root = fs.root_inode();
        let root_ino = root.ino();
        let mut mounts = self.mounts.lock();
        let mut dentries = self.dentries.lock();
        let mut inodes = self.inodes.lock();
        mounts.mount_with_ino(mountpoint, path, fs, root_ino)?;
        dentries.invalidate_dir(mp_dev, mp_ino);
        let _ = inodes.get_or_insert(root);
        Ok(())
    }

    /// Unmount the filesystem mounted on `(mountpoint_dev, mountpoint_ino)`
    /// and purge its entries from `DentryCache` and `InodeCache`, dropping
    /// the evicted inodes after the locks are released.
    pub fn umount(&self, mountpoint_dev: DevId, mountpoint_ino: Ino) -> Result<Arc<dyn FileSystem>, VfsError> {
        let (fs, evicted) = {
            let mut mounts = self.mounts.lock();
            let mut dentries = self.dentries.lock();
            let mut inodes = self.inodes.lock();
            let fs = mounts.umount(mountpoint_dev, mountpoint_ino)?;
            let dev = fs.dev_id();
            dentries.invalidate_fs(dev);
            dentries.invalidate_dir(mountpoint_dev, mountpoint_ino);
            let evicted = inodes.evict_fs(dev);
            (fs, evicted)
        };
        drop(evicted);
        Ok(fs)
    }

    /// Every mounted filesystem, the lock released before the caller uses them.
    pub fn snapshot_filesystems(&self) -> Vec<Arc<dyn FileSystem>> {
        self.mounts.lock().snapshot_filesystems()
    }

    /// Every cached inode, the lock released before the caller uses them.
    pub fn snapshot_inodes(&self) -> Vec<Arc<dyn VfsInode>> {
        self.inodes.lock().snapshot_all()
    }

    /// How many names `DentryCache` holds.
    pub fn dentry_count(&self) -> usize {
        self.dentries.lock().len()
    }

    /// Every active mount, `/` first; the filesystems are asked for their
    /// device, name and read-only flag after the lock is released.
    pub fn list_mounts(&self) -> Vec<MountInfo> {
        let mounts = self.mounts.lock().snapshot_mounts();
        mounts.into_iter().map(|(fs, path)| MountInfo::of(fs.as_ref(), path)).collect()
    }

    /// Every cached name.
    #[cfg(test)]
    pub(crate) fn snapshot_dentries(&self) -> Vec<(DevId, Ino, Vec<u8>, DentryTarget)> {
        self.dentries.lock().snapshot_all()
    }

    /// Whether a filesystem with `dev` is mounted.
    #[cfg(test)]
    pub(crate) fn is_dev_mounted(&self, dev: DevId) -> bool {
        self.mounts.lock().is_dev_mounted(dev)
    }
}

impl<F: LockFamily> Default for VfsTables<F> {
    fn default() -> Self {
        Self::new()
    }
}

impl<F: LockFamily> VfsCacheOps for VfsTables<F> {
    fn root_inode(&self) -> Result<Arc<dyn VfsInode>, VfsError> {
        let root_fs = self.mounts.lock().root_fs().ok_or(VfsError::NoEnt)?;
        Ok(root_fs.root_inode())
    }

    fn is_global_root(&self, dev: DevId, ino: Ino) -> bool {
        self.mounts.lock().is_global_root(dev, ino)
    }

    fn mount_at(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn FileSystem>> {
        self.mounts.lock().mount_at(dev, ino)
    }

    fn mount_at_with_id(&self, dev: DevId, ino: Ino) -> Option<(Arc<dyn FileSystem>, u64)> {
        self.mounts.lock().mount_at_with_id(dev, ino)
    }

    fn parent_mount(&self, mounted_dev: DevId) -> Option<Arc<dyn VfsInode>> {
        self.mounts.lock().parent_mount(mounted_dev)
    }

    fn parent_mount_if_root(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        self.mounts.lock().parent_mount_if_root(dev, ino)
    }

    fn is_mountpoint(&self, dev: DevId, ino: Ino) -> bool {
        self.mounts.lock().is_mountpoint(dev, ino)
    }

    fn fs_read_only(&self, dev: DevId) -> bool {
        // The guard is released before `read_only` runs, as `root_inode`
        // does before `root_inode()` (`fs_by_dev` still asks each mounted
        // filesystem's `dev_id()` under it; the struct doc lists what runs
        // under the guards).
        let fs = self.mounts.lock().fs_by_dev(dev);
        fs.is_some_and(|fs| fs.read_only())
    }

    fn fs_by_dev(&self, dev: DevId) -> Option<Arc<dyn FileSystem>> {
        self.mounts.lock().fs_by_dev(dev)
    }

    fn cache_gens(&self) -> (u64, u64) {
        let dentries = self.dentries.lock();
        let inodes = self.inodes.lock();
        (dentries.invalidation_gen(), inodes.removal_gen())
    }

    fn dentry_lookup_inode(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &[u8],
    ) -> (Option<Arc<dyn VfsInode>>, u64, u64) {
        let dentries = self.dentries.lock();
        let inodes = self.inodes.lock();
        let (target_opt, dentry_gen) = dentries.get_with_gen(parent_dev, parent_ino, name);
        let inode_gen = inodes.removal_gen();
        let inode = target_opt.and_then(|t| inodes.get(t.dev, t.ino));
        (inode, dentry_gen, inode_gen)
    }

    fn publish_lookup_inode(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &[u8],
        inode: Arc<dyn VfsInode>,
        expected_dentry_gen: u64,
        expected_inode_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
        let mounts = self.mounts.lock();
        let mut dentries = self.dentries.lock();
        let mut inodes = self.inodes.lock();
        if !mounts.is_dev_mounted(inode.dev_id()) {
            return Err(inode);
        }
        if !inode.cacheable() {
            return Ok(inode);
        }
        let unified = inodes.get_or_insert_if_fresh(inode, expected_inode_gen)?;
        if !mounts.is_mountpoint(parent_dev, parent_ino) {
            let _ = dentries.insert_if_gen(
                parent_dev,
                parent_ino,
                name,
                DentryTarget { dev: unified.dev_id(), ino: unified.ino() },
                expected_dentry_gen,
            );
        }
        Ok(unified)
    }

    fn publish_created_inode(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &[u8],
        inode: Arc<dyn VfsInode>,
        expected_dentry_gen: u64,
        expected_inode_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
        let mounts = self.mounts.lock();
        let mut dentries = self.dentries.lock();
        let mut inodes = self.inodes.lock();
        if !mounts.is_dev_mounted(inode.dev_id()) {
            return Err(inode);
        }
        if !inode.cacheable() {
            return Ok(inode);
        }
        let was_cached = inodes.get(inode.dev_id(), inode.ino()).is_some();
        let unified = inodes.get_or_insert_created_if_fresh(inode, expected_inode_gen)?;
        if !was_cached && !mounts.is_mountpoint(parent_dev, parent_ino) {
            let _ = dentries.insert_if_gen(
                parent_dev,
                parent_ino,
                name,
                DentryTarget { dev: unified.dev_id(), ino: unified.ino() },
                expected_dentry_gen,
            );
        }
        Ok(unified)
    }

    fn dentry_gen(&self) -> u64 {
        self.dentries.lock().invalidation_gen()
    }

    fn dentry_get(&self, parent_dev: DevId, parent_ino: Ino, name: &[u8]) -> Option<DentryTarget> {
        self.dentries.lock().get(parent_dev, parent_ino, name)
    }

    fn dentry_get_with_gen(&self, parent_dev: DevId, parent_ino: Ino, name: &[u8]) -> (Option<DentryTarget>, u64) {
        self.dentries.lock().get_with_gen(parent_dev, parent_ino, name)
    }

    fn dentry_insert(&self, parent_dev: DevId, parent_ino: Ino, name: &[u8], target: DentryTarget) {
        self.dentries.lock().insert(parent_dev, parent_ino, name, target);
    }

    fn dentry_insert_if_gen(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &[u8],
        target: DentryTarget,
        expected_gen: u64,
    ) -> bool {
        let mounts = self.mounts.lock();
        let mut dentries = self.dentries.lock();
        if mounts.is_mountpoint(parent_dev, parent_ino) {
            return false;
        }
        dentries.insert_if_gen(parent_dev, parent_ino, name, target, expected_gen)
    }

    fn dentry_invalidate(&self, parent_dev: DevId, parent_ino: Ino, name: &[u8]) {
        self.dentries.lock().invalidate(parent_dev, parent_ino, name);
    }

    fn dentry_invalidate_dir(&self, parent_dev: DevId, parent_ino: Ino) {
        self.dentries.lock().invalidate_dir(parent_dev, parent_ino);
    }

    fn inode_get(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        self.inodes.lock().get(dev, ino)
    }

    fn inode_get_if_fresh(&self, dev: DevId, ino: Ino, since_gen: u64) -> Option<Arc<dyn VfsInode>> {
        self.inodes.lock().get_if_fresh(dev, ino, since_gen)
    }

    fn inode_get_or_insert(&self, inode: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
        if !inode.cacheable() {
            return inode;
        }
        self.inodes.lock().get_or_insert(inode)
    }

    fn inode_get_or_insert_if_mounted(
        &self,
        mp_dev: DevId,
        mp_ino: Ino,
        mount_id: u64,
        inode: Arc<dyn VfsInode>,
    ) -> Option<Arc<dyn VfsInode>> {
        let mounts = self.mounts.lock();
        let mut inodes = self.inodes.lock();
        if !mounts.has_mount_id(mp_dev, mp_ino, mount_id) {
            return None;
        }
        if !inode.cacheable() {
            return Some(inode);
        }
        Some(inodes.get_or_insert(inode))
    }

    fn inode_get_or_insert_if_fresh(
        &self,
        inode: Arc<dyn VfsInode>,
        since_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
        let mounts = self.mounts.lock();
        let mut inodes = self.inodes.lock();
        if !mounts.is_dev_mounted(inode.dev_id()) {
            return Err(inode);
        }
        if !inode.cacheable() {
            return Ok(inode);
        }
        inodes.get_or_insert_if_fresh(inode, since_gen)
    }

    fn inode_remove(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        self.inodes.lock().remove(dev, ino)
    }

    fn inode_remove_if_fresh(&self, dev: DevId, ino: Ino, since_gen: u64) -> Option<(Option<Arc<dyn VfsInode>>, u64)> {
        self.inodes.lock().remove_if_fresh(dev, ino, since_gen)
    }

    fn inode_begin_removal(&self, dev: DevId, ino: Ino, since_gen: u64) -> (Option<Arc<dyn VfsInode>>, u64) {
        self.inodes.lock().begin_removal_if_fresh(dev, ino, since_gen)
    }

    fn inode_end_removal(&self, dev: DevId, ino: Ino) -> u64 {
        self.inodes.lock().end_removal(dev, ino)
    }

    fn inode_evict_detached(&self, dev: DevId, ino: Ino) -> bool {
        self.inodes.lock().evict_detached(dev, ino)
    }

    fn inode_remove_if_same(&self, dev: DevId, ino: Ino, expected: &Arc<dyn VfsInode>) -> Option<Arc<dyn VfsInode>> {
        self.inodes.lock().remove_if_same(dev, ino, expected)
    }

    fn inode_record_removal(&self, dev: DevId, ino: Ino) -> u64 {
        self.inodes.lock().record_removal(dev, ino)
    }

    fn inode_record_removal_if_absent(&self, dev: DevId, ino: Ino) -> Result<u64, Arc<dyn VfsInode>> {
        self.inodes.lock().record_removal_if_absent(dev, ino)
    }
}
