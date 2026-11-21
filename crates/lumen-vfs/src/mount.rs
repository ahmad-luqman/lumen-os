//! Mount table, short-held Lock 9 cache probe trait, and host-test tables.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::cache::{DentryCache, DentryTarget, InodeCache};
use crate::error::VfsError;
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

    /// Snapshot mount metadata for `/proc/mounts` and `mount` output.
    pub fn list_mounts(&self) -> Vec<MountInfo> {
        let mut out = Vec::with_capacity(self.mounts.len() + usize::from(self.root.is_some()));
        if let Some(root) = &self.root {
            out.push(MountInfo {
                dev_id: root.fs.dev_id(),
                fs_name: root.fs.name(),
                mount_path: root.mount_path.clone(),
                read_only: root.fs.read_only(),
            });
        }
        for rec in self.mounts.values() {
            out.push(MountInfo {
                dev_id: rec.fs.dev_id(),
                fs_name: rec.fs.name(),
                mount_path: rec.mount_path.clone(),
                read_only: rec.fs.read_only(),
            });
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
        name: &str,
    ) -> (Option<Arc<dyn VfsInode>>, u64, u64);
    fn publish_lookup_inode(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &str,
        inode: Arc<dyn VfsInode>,
        expected_dentry_gen: u64,
        expected_inode_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>>;
    /// Unify a newly created inode in `InodeCache` and publish its dentry
    /// under Lock 9. Implementations must insert into `DentryCache` only when
    /// `(inode.dev_id(), inode.ino())` was not already cached in `InodeCache`
    /// prior to `get_or_insert_created_if_fresh`, so callers outside Lock 9
    /// can verify a pre-existing cached `Arc` is alive before caching its dentry.
    fn publish_created_inode(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &str,
        inode: Arc<dyn VfsInode>,
        expected_dentry_gen: u64,
        expected_inode_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>>;

    fn dentry_gen(&self) -> u64;
    fn dentry_get(&self, parent_dev: DevId, parent_ino: Ino, name: &str) -> Option<DentryTarget>;
    fn dentry_get_with_gen(&self, parent_dev: DevId, parent_ino: Ino, name: &str) -> (Option<DentryTarget>, u64);
    fn dentry_insert(&self, parent_dev: DevId, parent_ino: Ino, name: &str, target: DentryTarget);
    fn dentry_insert_if_gen(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &str,
        target: DentryTarget,
        expected_gen: u64,
    ) -> bool;
    fn dentry_invalidate(&self, parent_dev: DevId, parent_ino: Ino, name: &str);
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
}

/// Single-threaded `RefCell`-backed VFS lookup tables for host unit tests.
#[derive(Default)]
pub struct SimpleVfsTables {
    pub mounts: RefCell<MountTable>,
    pub dentries: RefCell<DentryCache>,
    pub inodes: RefCell<InodeCache>,
}

impl SimpleVfsTables {
    /// Create empty VFS tables with default dentry cache capacity.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mount `fs` at `/` and seed its root inode into `InodeCache`.
    pub fn mount_root(&self, fs: Arc<dyn FileSystem>) -> Result<(), VfsError> {
        let root = fs.root_inode();
        let root_ino = root.ino();
        self.mounts.borrow_mut().mount_root_with_ino(fs, root_ino)?;
        let _ = self.inodes.borrow_mut().get_or_insert(root);
        Ok(())
    }

    /// Mount `fs` at `mountpoint` (`path`), invalidating stale dentries at `mountpoint`.
    pub fn mount(&self, mountpoint: Arc<dyn VfsInode>, path: &str, fs: Arc<dyn FileSystem>) -> Result<(), VfsError> {
        let mp_dev = mountpoint.dev_id();
        let mp_ino = mountpoint.ino();
        let root = fs.root_inode();
        let root_ino = root.ino();
        let mut mounts = self.mounts.borrow_mut();
        let mut dentries = self.dentries.borrow_mut();
        let mut inodes = self.inodes.borrow_mut();
        mounts.mount_with_ino(mountpoint, path, fs, root_ino)?;
        dentries.invalidate_dir(mp_dev, mp_ino);
        let _ = inodes.get_or_insert(root);
        Ok(())
    }

    /// Unmount the filesystem mounted on `(mountpoint_dev, mountpoint_ino)` and
    /// purge its entries from `DentryCache` and `InodeCache`.
    pub fn umount(&self, mountpoint_dev: DevId, mountpoint_ino: Ino) -> Result<Arc<dyn FileSystem>, VfsError> {
        let mut mounts = self.mounts.borrow_mut();
        let mut dentries = self.dentries.borrow_mut();
        let mut inodes = self.inodes.borrow_mut();
        let fs = mounts.umount(mountpoint_dev, mountpoint_ino)?;
        let dev = fs.dev_id();
        dentries.invalidate_fs(dev);
        dentries.invalidate_dir(mountpoint_dev, mountpoint_ino);
        let _ = inodes.evict_fs(dev);
        Ok(fs)
    }
}

impl VfsCacheOps for SimpleVfsTables {
    fn root_inode(&self) -> Result<Arc<dyn VfsInode>, VfsError> {
        let root_fs = self.mounts.borrow().root_fs().ok_or(VfsError::NoEnt)?;
        Ok(root_fs.root_inode())
    }

    fn is_global_root(&self, dev: DevId, ino: Ino) -> bool {
        self.mounts.borrow().is_global_root(dev, ino)
    }

    fn mount_at(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn FileSystem>> {
        self.mounts.borrow().mount_at(dev, ino)
    }

    fn mount_at_with_id(&self, dev: DevId, ino: Ino) -> Option<(Arc<dyn FileSystem>, u64)> {
        self.mounts.borrow().mount_at_with_id(dev, ino)
    }

    fn parent_mount(&self, mounted_dev: DevId) -> Option<Arc<dyn VfsInode>> {
        self.mounts.borrow().parent_mount(mounted_dev)
    }

    fn parent_mount_if_root(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        self.mounts.borrow().parent_mount_if_root(dev, ino)
    }

    fn is_mountpoint(&self, dev: DevId, ino: Ino) -> bool {
        self.mounts.borrow().is_mountpoint(dev, ino)
    }

    fn fs_read_only(&self, dev: DevId) -> bool {
        self.mounts.borrow().fs_by_dev(dev).is_some_and(|fs| fs.read_only())
    }

    fn fs_by_dev(&self, dev: DevId) -> Option<Arc<dyn FileSystem>> {
        self.mounts.borrow().fs_by_dev(dev)
    }

    fn cache_gens(&self) -> (u64, u64) {
        let dentries = self.dentries.borrow();
        let inodes = self.inodes.borrow();
        (dentries.invalidation_gen(), inodes.removal_gen())
    }

    fn dentry_lookup_inode(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &str,
    ) -> (Option<Arc<dyn VfsInode>>, u64, u64) {
        let dentries = self.dentries.borrow();
        let inodes = self.inodes.borrow();
        let (target_opt, dentry_gen) = dentries.get_with_gen(parent_dev, parent_ino, name);
        let inode_gen = inodes.removal_gen();
        let inode = target_opt.and_then(|t| inodes.get(t.dev, t.ino));
        (inode, dentry_gen, inode_gen)
    }

    fn publish_lookup_inode(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &str,
        inode: Arc<dyn VfsInode>,
        expected_dentry_gen: u64,
        expected_inode_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
        let mounts = self.mounts.borrow();
        let mut dentries = self.dentries.borrow_mut();
        let mut inodes = self.inodes.borrow_mut();
        if !mounts.is_dev_mounted(inode.dev_id()) {
            return Err(inode);
        }
        let unified = inodes.get_or_insert_if_fresh(inode, expected_inode_gen)?;
        let _ = dentries.insert_if_gen(
            parent_dev,
            parent_ino,
            name,
            DentryTarget { dev: unified.dev_id(), ino: unified.ino() },
            expected_dentry_gen,
        );
        Ok(unified)
    }

    fn publish_created_inode(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &str,
        inode: Arc<dyn VfsInode>,
        expected_dentry_gen: u64,
        expected_inode_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
        let mounts = self.mounts.borrow();
        let mut dentries = self.dentries.borrow_mut();
        let mut inodes = self.inodes.borrow_mut();
        if !mounts.is_dev_mounted(inode.dev_id()) {
            return Err(inode);
        }
        let was_cached = inodes.get(inode.dev_id(), inode.ino()).is_some();
        let unified = inodes.get_or_insert_created_if_fresh(inode, expected_inode_gen)?;
        if !was_cached {
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
        self.dentries.borrow().invalidation_gen()
    }

    fn dentry_get(&self, parent_dev: DevId, parent_ino: Ino, name: &str) -> Option<DentryTarget> {
        self.dentries.borrow().get(parent_dev, parent_ino, name)
    }

    fn dentry_get_with_gen(&self, parent_dev: DevId, parent_ino: Ino, name: &str) -> (Option<DentryTarget>, u64) {
        self.dentries.borrow().get_with_gen(parent_dev, parent_ino, name)
    }

    fn dentry_insert(&self, parent_dev: DevId, parent_ino: Ino, name: &str, target: DentryTarget) {
        self.dentries.borrow_mut().insert(parent_dev, parent_ino, name, target);
    }

    fn dentry_insert_if_gen(
        &self,
        parent_dev: DevId,
        parent_ino: Ino,
        name: &str,
        target: DentryTarget,
        expected_gen: u64,
    ) -> bool {
        self.dentries.borrow_mut().insert_if_gen(parent_dev, parent_ino, name, target, expected_gen)
    }

    fn dentry_invalidate(&self, parent_dev: DevId, parent_ino: Ino, name: &str) {
        self.dentries.borrow_mut().invalidate(parent_dev, parent_ino, name);
    }

    fn dentry_invalidate_dir(&self, parent_dev: DevId, parent_ino: Ino) {
        self.dentries.borrow_mut().invalidate_dir(parent_dev, parent_ino);
    }

    fn inode_get(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        self.inodes.borrow().get(dev, ino)
    }

    fn inode_get_if_fresh(&self, dev: DevId, ino: Ino, since_gen: u64) -> Option<Arc<dyn VfsInode>> {
        self.inodes.borrow_mut().get_if_fresh(dev, ino, since_gen)
    }

    fn inode_get_or_insert(&self, inode: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
        self.inodes.borrow_mut().get_or_insert(inode)
    }

    fn inode_get_or_insert_if_mounted(
        &self,
        mp_dev: DevId,
        mp_ino: Ino,
        mount_id: u64,
        inode: Arc<dyn VfsInode>,
    ) -> Option<Arc<dyn VfsInode>> {
        let mounts = self.mounts.borrow();
        let mut inodes = self.inodes.borrow_mut();
        if !mounts.has_mount_id(mp_dev, mp_ino, mount_id) {
            return None;
        }
        Some(inodes.get_or_insert(inode))
    }

    fn inode_get_or_insert_if_fresh(
        &self,
        inode: Arc<dyn VfsInode>,
        since_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
        let mounts = self.mounts.borrow();
        let mut inodes = self.inodes.borrow_mut();
        if !mounts.is_dev_mounted(inode.dev_id()) {
            return Err(inode);
        }
        inodes.get_or_insert_if_fresh(inode, since_gen)
    }

    fn inode_remove(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        self.inodes.borrow_mut().remove(dev, ino)
    }

    fn inode_remove_if_fresh(&self, dev: DevId, ino: Ino, since_gen: u64) -> Option<(Option<Arc<dyn VfsInode>>, u64)> {
        self.inodes.borrow_mut().remove_if_fresh(dev, ino, since_gen)
    }

    fn inode_begin_removal(&self, dev: DevId, ino: Ino, since_gen: u64) -> (Option<Arc<dyn VfsInode>>, u64) {
        self.inodes.borrow_mut().begin_removal_if_fresh(dev, ino, since_gen)
    }

    fn inode_end_removal(&self, dev: DevId, ino: Ino) -> u64 {
        self.inodes.borrow_mut().end_removal(dev, ino)
    }

    fn inode_evict_detached(&self, dev: DevId, ino: Ino) -> bool {
        self.inodes.borrow_mut().evict_detached(dev, ino)
    }

    fn inode_remove_if_same(&self, dev: DevId, ino: Ino, expected: &Arc<dyn VfsInode>) -> Option<Arc<dyn VfsInode>> {
        self.inodes.borrow_mut().remove_if_same(dev, ino, expected)
    }

    fn inode_record_removal(&self, dev: DevId, ino: Ino) -> u64 {
        self.inodes.borrow_mut().record_removal(dev, ino)
    }
}
