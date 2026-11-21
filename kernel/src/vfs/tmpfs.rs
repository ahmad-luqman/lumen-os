//! In-memory POSIX filesystem (`tmpfs`) backed by unified `PageCache` frames
//! and bounded by an atomic per-mount memory quota (default 32 MiB).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use lumen_vfs::{
    AdvisoryLocks, DevId, DirEntry, FileSystem, Ino, InodeType, Metadata, RenameFlags, SetTime, Timespec, VfsError,
    VfsInode, next_detached_ino, next_lock_id,
};

use crate::sync::{SleepMutex, SleepMutexGuard};
use crate::time;

use super::page_cache::{CachePage, PAGE_SIZE, PageCache, QuotaCounter};

struct TmpfsShared {
    dev: DevId,
    quota: Arc<QuotaCounter>,
    next_ino: AtomicU64,
}

/// In-memory `tmpfs` filesystem (`0:2` on `/tmp`, or `0:1` on diskless root `/`).
pub struct Tmpfs {
    shared: Arc<TmpfsShared>,
    rename_lock: SleepMutex<()>,
    root: Arc<TmpfsInode>,
}

impl Tmpfs {
    pub fn new(dev: DevId, quota_bytes: usize) -> Arc<Self> {
        let quota_pages = quota_bytes / PAGE_SIZE;
        let shared =
            Arc::new(TmpfsShared { dev, quota: Arc::new(QuotaCounter::new(quota_pages)), next_ino: AtomicU64::new(2) });
        let now = time::realtime_timespec();
        let root = Arc::new_cyclic(|weak_self| TmpfsInode {
            lock_id: next_lock_id(),
            shared: Arc::clone(&shared),
            self_weak: Weak::clone(weak_self),
            ino: AtomicU64::new(1),
            inode_type: InodeType::Dir,
            open_count: AtomicU32::new(0),
            detached: AtomicBool::new(false),
            meta: SleepMutex::new(TmpfsMeta {
                mode: 0o755,
                nlink: 2,
                uid: 0,
                gid: 0,
                size: 0,
                atime: now,
                mtime: now,
                ctime: now,
                symlink_target: String::new(),
                children: BTreeMap::new(),
                parent: Some(Weak::clone(weak_self)),
                locks: AdvisoryLocks::new(),
            }),
            pages: SleepMutex::new(PageCache::new_tmpfs(Arc::clone(&shared.quota))),
        });
        Arc::new(Self { shared, rename_lock: SleepMutex::new(()), root })
    }

    #[allow(dead_code)]
    pub fn used_pages(&self) -> usize {
        self.shared.quota.used_pages()
    }
}

impl FileSystem for Tmpfs {
    fn dev_id(&self) -> DevId {
        self.shared.dev
    }

    fn name(&self) -> &'static str {
        "tmpfs"
    }

    fn root_inode(&self) -> Arc<dyn VfsInode> {
        Arc::clone(&self.root) as Arc<dyn VfsInode>
    }

    fn sync(&self) -> Result<(), VfsError> {
        Ok(())
    }

    fn read_only(&self) -> bool {
        false
    }

    fn with_rename_lock(&self, f: &mut dyn FnMut() -> Result<(), VfsError>) -> Result<(), VfsError> {
        let _guard = self.rename_lock.lock();
        f()
    }
}

struct TmpfsMeta {
    mode: u16,
    nlink: u32,
    uid: u32,
    gid: u32,
    size: u64,
    atime: Timespec,
    mtime: Timespec,
    ctime: Timespec,
    symlink_target: String,
    children: BTreeMap<String, Arc<TmpfsInode>>,
    parent: Option<Weak<TmpfsInode>>,
    locks: AdvisoryLocks,
}

/// Inode belonging to a [`Tmpfs`] mount.
///
/// `Drop` never acquires any sleeping lock.
pub struct TmpfsInode {
    lock_id: u64,
    shared: Arc<TmpfsShared>,
    self_weak: Weak<TmpfsInode>,
    ino: AtomicU64,
    inode_type: InodeType,
    open_count: AtomicU32,
    detached: AtomicBool,
    meta: SleepMutex<TmpfsMeta>,
    pages: SleepMutex<PageCache>,
}

impl TmpfsInode {
    fn new_child(
        shared: &Arc<TmpfsShared>,
        inode_type: InodeType,
        mode: u16,
        nlink: u32,
        symlink_target: String,
        parent: Option<Weak<TmpfsInode>>,
    ) -> Arc<Self> {
        let now = time::realtime_timespec();
        let ino = shared.next_ino.fetch_add(1, Ordering::Relaxed);
        Arc::new_cyclic(|weak_self| Self {
            lock_id: next_lock_id(),
            shared: Arc::clone(shared),
            self_weak: Weak::clone(weak_self),
            ino: AtomicU64::new(ino),
            inode_type,
            open_count: AtomicU32::new(0),
            detached: AtomicBool::new(false),
            meta: SleepMutex::new(TmpfsMeta {
                mode: mode & 0o7777,
                nlink,
                uid: 0,
                gid: 0,
                size: symlink_target.len() as u64,
                atime: now,
                mtime: now,
                ctime: now,
                symlink_target,
                children: BTreeMap::new(),
                parent,
                locks: AdvisoryLocks::new(),
            }),
            pages: SleepMutex::new(PageCache::new_tmpfs(Arc::clone(&shared.quota))),
        })
    }

    fn detach_if_open(target: &Arc<TmpfsInode>, meta: &mut TmpfsMeta) {
        meta.nlink = 0;
        if target.open_count() > 0 && !target.is_detached() {
            target.detached.store(true, Ordering::Release);
            target.ino.store(next_detached_ino(), Ordering::Release);
        }
    }

    fn self_arc(&self) -> Option<Arc<TmpfsInode>> {
        self.self_weak.upgrade()
    }

    fn validate_name(name: &str) -> Result<(), VfsError> {
        if name.is_empty() || name.contains('/') || name.as_bytes().contains(&0) {
            return Err(VfsError::Inval);
        }
        if name.len() > lumen_vfs::NAME_MAX {
            return Err(VfsError::NameTooLong);
        }
        Ok(())
    }
}

struct LockedTmpfs<'a> {
    inode: &'a Arc<TmpfsInode>,
    guard: SleepMutexGuard<'a, TmpfsMeta>,
}

fn lock_tmpfs_ordered<'a>(inodes: &[&'a Arc<TmpfsInode>]) -> Vec<LockedTmpfs<'a>> {
    let mut unique: Vec<&'a Arc<TmpfsInode>> = Vec::with_capacity(inodes.len());
    for &ino in inodes {
        if !unique.iter().any(|x| x.lock_id == ino.lock_id) {
            unique.push(ino);
        }
    }
    unique.sort_by_key(|x| (x.inode_type != InodeType::Dir, x.lock_id));
    unique.into_iter().map(|inode| LockedTmpfs { inode, guard: inode.meta.lock() }).collect()
}

fn tmpfs_guard_mut<'a, 'b>(guards: &'b mut [LockedTmpfs<'a>], target: &Arc<TmpfsInode>) -> &'b mut TmpfsMeta {
    &mut guards.iter_mut().find(|g| g.inode.lock_id == target.lock_id).expect("locked tmpfs inode").guard
}

/// Resolve an `Arc<dyn VfsInode>` to its concrete `Arc<TmpfsInode>` by walking
/// up to root and scanning the directory tree.
fn find_tmpfs_dir_arc(known_dir: &TmpfsInode, target_lock_id: u64, target_ino: Ino) -> Option<Arc<TmpfsInode>> {
    let mut cur = known_dir.self_arc()?;
    if cur.lock_id == target_lock_id {
        return Some(cur);
    }
    loop {
        if cur.lock_id == target_lock_id {
            return Some(cur);
        }
        let next = {
            let m = cur.meta.lock();
            m.parent.as_ref().and_then(Weak::upgrade)
        };
        match next {
            Some(p) if p.lock_id != cur.lock_id => cur = p,
            _ => break,
        }
    }
    // BFS from root (`cur`) to locate `target_lock_id`.
    let mut stack = alloc::vec![cur];
    while let Some(node) = stack.pop() {
        if node.lock_id == target_lock_id {
            return Some(node);
        }
        let m = node.meta.lock();
        for child in m.children.values() {
            if child.inode_type == InodeType::Dir {
                if child.lock_id == target_lock_id || child.ino() == target_ino {
                    return Some(Arc::clone(child));
                }
                stack.push(Arc::clone(child));
            }
        }
    }
    None
}

impl VfsInode for TmpfsInode {
    fn lock_id(&self) -> u64 {
        self.lock_id
    }

    fn dev_id(&self) -> DevId {
        self.shared.dev
    }

    fn ino(&self) -> Ino {
        self.ino.load(Ordering::Acquire)
    }

    fn inode_type(&self) -> InodeType {
        self.inode_type
    }

    fn open_count(&self) -> u32 {
        self.open_count.load(Ordering::Acquire)
    }

    fn inc_open_count(&self) {
        self.open_count.fetch_add(1, Ordering::AcqRel);
    }

    fn dec_open_count(&self) -> u32 {
        self.open_count.fetch_sub(1, Ordering::AcqRel) - 1
    }

    fn is_detached(&self) -> bool {
        self.detached.load(Ordering::Acquire)
    }

    fn stat(&self) -> Result<Metadata, VfsError> {
        let meta = self.meta.lock();
        let blocks = if self.inode_type == InodeType::File {
            let pages = self.pages.lock();
            (pages.len() as u64) * 8
        } else {
            0
        };
        Ok(Metadata {
            dev: self.shared.dev,
            ino: self.ino(),
            inode_type: self.inode_type,
            mode: meta.mode,
            nlink: meta.nlink,
            uid: meta.uid,
            gid: meta.gid,
            rdev: DevId::default(),
            size: meta.size,
            blksize: PAGE_SIZE as u32,
            blocks,
            atime: meta.atime,
            mtime: meta.mtime,
            ctime: meta.ctime,
        })
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        let meta = self.meta.lock();
        if self.is_detached() {
            return Err(VfsError::NoEnt);
        }
        if name == "." {
            return self.self_arc().map(|a| a as Arc<dyn VfsInode>).ok_or(VfsError::NoEnt);
        }
        if name == ".." {
            let p = meta.parent.as_ref().and_then(Weak::upgrade).ok_or(VfsError::NoEnt)?;
            return Ok(p as Arc<dyn VfsInode>);
        }
        meta.children.get(name).map(|c| Arc::clone(c) as Arc<dyn VfsInode>).ok_or(VfsError::NoEnt)
    }

    fn create(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        if name == "." || name == ".." {
            return Err(VfsError::Exist);
        }
        let mut meta = self.meta.lock();
        if self.is_detached() {
            return Err(VfsError::NoEnt);
        }
        if meta.children.contains_key(name) {
            return Err(VfsError::Exist);
        }
        let child = Self::new_child(&self.shared, InodeType::File, mode, 1, String::new(), None);
        meta.children.insert(String::from(name), Arc::clone(&child));
        let now = time::realtime_timespec();
        meta.mtime = now;
        meta.ctime = now;
        Ok(child as Arc<dyn VfsInode>)
    }

    fn mkdir(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        if name == "." || name == ".." {
            return Err(VfsError::Exist);
        }
        let self_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        let mut meta = self.meta.lock();
        if self.is_detached() {
            return Err(VfsError::NoEnt);
        }
        if meta.children.contains_key(name) {
            return Err(VfsError::Exist);
        }
        let child =
            Self::new_child(&self.shared, InodeType::Dir, mode, 2, String::new(), Some(Arc::downgrade(&self_arc)));
        meta.children.insert(String::from(name), Arc::clone(&child));
        meta.nlink = meta.nlink.saturating_add(1);
        let now = time::realtime_timespec();
        meta.mtime = now;
        meta.ctime = now;
        Ok(child as Arc<dyn VfsInode>)
    }

    fn unlink(&self, name: &str) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        if name == "." || name == ".." {
            return Err(VfsError::IsDir);
        }
        let self_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        let child_arc = {
            let meta = self_arc.meta.lock();
            if self_arc.is_detached() {
                return Err(VfsError::NoEnt);
            }
            let c = meta.children.get(name).ok_or(VfsError::NoEnt)?;
            if c.inode_type == InodeType::Dir {
                return Err(VfsError::IsDir);
            }
            Arc::clone(c)
        };

        let mut guards = lock_tmpfs_ordered(&[&self_arc, &child_arc]);
        let now = time::realtime_timespec();
        {
            let p_meta = tmpfs_guard_mut(&mut guards, &self_arc);
            if self_arc.is_detached() || !p_meta.children.contains_key(name) {
                return Err(VfsError::NoEnt);
            }
            p_meta.children.remove(name);
            p_meta.mtime = now;
            p_meta.ctime = now;
        }
        let c_meta = tmpfs_guard_mut(&mut guards, &child_arc);
        if c_meta.nlink <= 1 {
            Self::detach_if_open(&child_arc, c_meta);
        } else {
            c_meta.nlink -= 1;
            c_meta.ctime = now;
        }
        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        if name == "." {
            return Err(VfsError::Inval);
        }
        if name == ".." {
            return Err(VfsError::NotEmpty);
        }
        let self_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        let child_arc = {
            let meta = self_arc.meta.lock();
            if self_arc.is_detached() {
                return Err(VfsError::NoEnt);
            }
            let c = meta.children.get(name).ok_or(VfsError::NoEnt)?;
            if c.inode_type != InodeType::Dir {
                return Err(VfsError::NotDir);
            }
            Arc::clone(c)
        };

        let mut guards = lock_tmpfs_ordered(&[&self_arc, &child_arc]);
        if !tmpfs_guard_mut(&mut guards, &child_arc).children.is_empty() {
            return Err(VfsError::NotEmpty);
        }
        let now = time::realtime_timespec();
        {
            let p_meta = tmpfs_guard_mut(&mut guards, &self_arc);
            if self_arc.is_detached() || !p_meta.children.contains_key(name) {
                return Err(VfsError::NoEnt);
            }
            p_meta.children.remove(name);
            p_meta.nlink = p_meta.nlink.saturating_sub(1);
            p_meta.mtime = now;
            p_meta.ctime = now;
        }
        let c_meta = tmpfs_guard_mut(&mut guards, &child_arc);
        Self::detach_if_open(&child_arc, c_meta);
        Ok(())
    }

    fn link(&self, name: &str, target: &Arc<dyn VfsInode>) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if target.dev_id() != self.shared.dev {
            return Err(VfsError::XDev);
        }
        if target.inode_type() == InodeType::Dir {
            return Err(VfsError::Perm);
        }
        if target.is_detached() {
            return Err(VfsError::NoEnt);
        }
        Self::validate_name(name)?;
        if name == "." || name == ".." {
            return Err(VfsError::Exist);
        }
        let self_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        // Locate target `Arc<TmpfsInode>` from root tree.
        let target_arc = find_tmpfs_non_dir(&self_arc, target.lock_id()).ok_or(VfsError::NoEnt)?;
        let mut guards = lock_tmpfs_ordered(&[&self_arc, &target_arc]);
        if self_arc.is_detached() || target_arc.is_detached() {
            return Err(VfsError::NoEnt);
        }
        let now = time::realtime_timespec();
        {
            let p_meta = tmpfs_guard_mut(&mut guards, &self_arc);
            if p_meta.children.contains_key(name) {
                return Err(VfsError::Exist);
            }
            p_meta.children.insert(String::from(name), Arc::clone(&target_arc));
            p_meta.mtime = now;
            p_meta.ctime = now;
        }
        let t_meta = tmpfs_guard_mut(&mut guards, &target_arc);
        t_meta.nlink = t_meta.nlink.saturating_add(1);
        t_meta.ctime = now;
        Ok(())
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        if target.is_empty() {
            return Err(VfsError::NoEnt);
        }
        if target.len() >= lumen_vfs::PATH_MAX {
            return Err(VfsError::NameTooLong);
        }
        if name == "." || name == ".." {
            return Err(VfsError::Exist);
        }
        let mut meta = self.meta.lock();
        if self.is_detached() {
            return Err(VfsError::NoEnt);
        }
        if meta.children.contains_key(name) {
            return Err(VfsError::Exist);
        }
        let child = Self::new_child(&self.shared, InodeType::Symlink, 0o777, 1, String::from(target), None);
        meta.children.insert(String::from(name), Arc::clone(&child));
        let now = time::realtime_timespec();
        meta.mtime = now;
        meta.ctime = now;
        Ok(child as Arc<dyn VfsInode>)
    }

    fn readlink(&self) -> Result<Vec<u8>, VfsError> {
        if self.inode_type != InodeType::Symlink {
            return Err(VfsError::Inval);
        }
        let meta = self.meta.lock();
        Ok(meta.symlink_target.as_bytes().to_vec())
    }

    fn rename(
        &self,
        old_name: &str,
        new_dir: &Arc<dyn VfsInode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<(), VfsError> {
        let flags = flags.validate()?;
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if new_dir.dev_id() != self.shared.dev {
            return Err(VfsError::XDev);
        }
        if new_dir.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(old_name)?;
        Self::validate_name(new_name)?;
        if old_name == "." || old_name == ".." || new_name == "." || new_name == ".." {
            return Err(VfsError::Inval);
        }

        let old_dir_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        let new_dir_arc = find_tmpfs_dir_arc(self, new_dir.lock_id(), new_dir.ino()).ok_or(VfsError::NoEnt)?;

        let (old_child_arc, new_child_opt) = {
            let mut dir_guards = lock_tmpfs_ordered(&[&old_dir_arc, &new_dir_arc]);
            if old_dir_arc.is_detached() || new_dir_arc.is_detached() {
                return Err(VfsError::NoEnt);
            }
            let old_c = tmpfs_guard_mut(&mut dir_guards, &old_dir_arc)
                .children
                .get(old_name)
                .cloned()
                .ok_or(VfsError::NoEnt)?;
            let new_c = tmpfs_guard_mut(&mut dir_guards, &new_dir_arc).children.get(new_name).cloned();
            (old_c, new_c)
        };

        if flags.has_noreplace() && new_child_opt.is_some() {
            return Err(VfsError::Exist);
        }
        if flags.has_exchange() && new_child_opt.is_none() {
            return Err(VfsError::NoEnt);
        }
        if old_dir_arc.lock_id == new_dir_arc.lock_id && old_name == new_name {
            return Ok(());
        }

        let mut to_lock = alloc::vec![&old_dir_arc, &new_dir_arc, &old_child_arc];
        if let Some(ref nc) = new_child_opt {
            to_lock.push(nc);
        }
        let mut guards = lock_tmpfs_ordered(&to_lock);
        if old_dir_arc.is_detached() || new_dir_arc.is_detached() {
            return Err(VfsError::NoEnt);
        }

        let now = time::realtime_timespec();
        let same_parent = old_dir_arc.lock_id == new_dir_arc.lock_id;
        let old_is_dir = old_child_arc.inode_type == InodeType::Dir;

        if flags.has_exchange() {
            let new_child_arc = new_child_opt.as_ref().expect("checked Some for EXCHANGE");
            let new_is_dir = new_child_arc.inode_type == InodeType::Dir;
            {
                let old_p = tmpfs_guard_mut(&mut guards, &old_dir_arc);
                old_p.children.insert(String::from(old_name), Arc::clone(new_child_arc));
                old_p.mtime = now;
                old_p.ctime = now;
            }
            {
                let new_p = tmpfs_guard_mut(&mut guards, &new_dir_arc);
                new_p.children.insert(String::from(new_name), Arc::clone(&old_child_arc));
                new_p.mtime = now;
                new_p.ctime = now;
            }
            if !same_parent {
                if old_is_dir {
                    tmpfs_guard_mut(&mut guards, &old_child_arc).parent = Some(Arc::downgrade(&new_dir_arc));
                    tmpfs_guard_mut(&mut guards, &old_dir_arc).nlink =
                        tmpfs_guard_mut(&mut guards, &old_dir_arc).nlink.saturating_sub(1);
                    tmpfs_guard_mut(&mut guards, &new_dir_arc).nlink =
                        tmpfs_guard_mut(&mut guards, &new_dir_arc).nlink.saturating_add(1);
                }
                if new_is_dir {
                    tmpfs_guard_mut(&mut guards, new_child_arc).parent = Some(Arc::downgrade(&old_dir_arc));
                    tmpfs_guard_mut(&mut guards, &new_dir_arc).nlink =
                        tmpfs_guard_mut(&mut guards, &new_dir_arc).nlink.saturating_sub(1);
                    tmpfs_guard_mut(&mut guards, &old_dir_arc).nlink =
                        tmpfs_guard_mut(&mut guards, &old_dir_arc).nlink.saturating_add(1);
                }
            }
            tmpfs_guard_mut(&mut guards, &old_child_arc).ctime = now;
            tmpfs_guard_mut(&mut guards, new_child_arc).ctime = now;
            return Ok(());
        }

        if let Some(ref nc) = new_child_opt {
            if nc.lock_id == old_child_arc.lock_id {
                return Ok(());
            }
            let new_is_dir = nc.inode_type == InodeType::Dir;
            if old_is_dir && !new_is_dir {
                return Err(VfsError::NotDir);
            }
            if !old_is_dir && new_is_dir {
                return Err(VfsError::IsDir);
            }
            if new_is_dir && !tmpfs_guard_mut(&mut guards, nc).children.is_empty() {
                return Err(VfsError::NotEmpty);
            }
            let nc_meta = tmpfs_guard_mut(&mut guards, nc);
            if new_is_dir || nc_meta.nlink <= 1 {
                Self::detach_if_open(nc, nc_meta);
            } else {
                nc_meta.nlink -= 1;
                nc_meta.ctime = now;
            }
            if new_is_dir {
                let np = tmpfs_guard_mut(&mut guards, &new_dir_arc);
                np.nlink = np.nlink.saturating_sub(1);
            }
        }

        tmpfs_guard_mut(&mut guards, &old_dir_arc).children.remove(old_name);
        tmpfs_guard_mut(&mut guards, &new_dir_arc).children.insert(String::from(new_name), Arc::clone(&old_child_arc));

        if old_is_dir && !same_parent {
            tmpfs_guard_mut(&mut guards, &old_child_arc).parent = Some(Arc::downgrade(&new_dir_arc));
            tmpfs_guard_mut(&mut guards, &old_dir_arc).nlink =
                tmpfs_guard_mut(&mut guards, &old_dir_arc).nlink.saturating_sub(1);
            tmpfs_guard_mut(&mut guards, &new_dir_arc).nlink =
                tmpfs_guard_mut(&mut guards, &new_dir_arc).nlink.saturating_add(1);
        }
        tmpfs_guard_mut(&mut guards, &old_dir_arc).mtime = now;
        tmpfs_guard_mut(&mut guards, &old_dir_arc).ctime = now;
        tmpfs_guard_mut(&mut guards, &new_dir_arc).mtime = now;
        tmpfs_guard_mut(&mut guards, &new_dir_arc).ctime = now;
        tmpfs_guard_mut(&mut guards, &old_child_arc).ctime = now;
        Ok(())
    }

    fn readdir(&self, cursor: u64, out: &mut Vec<DirEntry>, max_entries: usize) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        let meta = self.meta.lock();
        if self.is_detached() || max_entries == 0 {
            return Ok(());
        }
        let parent_ino = meta.parent.as_ref().and_then(Weak::upgrade).map_or(self.ino(), |p| p.ino());
        for (idx, (ino, inode_type, name)) in [(self.ino(), InodeType::Dir, "."), (parent_ino, InodeType::Dir, "..")]
            .into_iter()
            .chain(meta.children.iter().map(|(name, child)| (child.ino(), child.inode_type, name.as_str())))
            .enumerate()
        {
            if out.len() >= max_entries {
                break;
            }
            let cur_idx = idx as u64;
            if cur_idx >= cursor {
                out.push(DirEntry { ino, inode_type, name: String::from(name), next_cursor: cur_idx + 1 });
            }
        }
        Ok(())
    }

    fn truncate(&self, new_size: u64) -> Result<(), VfsError> {
        if self.inode_type == InodeType::Dir {
            return Err(VfsError::IsDir);
        }
        if self.inode_type != InodeType::File {
            return Err(VfsError::Inval);
        }
        let mut meta = self.meta.lock();
        let mut pages = self.pages.lock();
        let old_size = meta.size;
        if new_size < old_size {
            pages.remove_from(new_size.div_ceil(PAGE_SIZE as u64));
            let tail_rem = (new_size % PAGE_SIZE as u64) as usize;
            if tail_rem != 0
                && let Some(page) = pages.get(new_size / PAGE_SIZE as u64)
            {
                page.zero_range(tail_rem, PAGE_SIZE);
            }
        } else if new_size > old_size {
            let tail_rem = (old_size % PAGE_SIZE as u64) as usize;
            if tail_rem != 0
                && let Some(page) = pages.get(old_size / PAGE_SIZE as u64)
            {
                page.zero_range(tail_rem, PAGE_SIZE);
            }
        }
        let now = time::realtime_timespec();
        meta.size = new_size;
        meta.mtime = now;
        meta.ctime = now;
        Ok(())
    }

    fn set_mode(&self, mode: u16) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        meta.mode = mode & 0o7777;
        meta.ctime = time::realtime_timespec();
        Ok(())
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        if let Some(u) = uid {
            meta.uid = u;
        }
        if let Some(g) = gid {
            meta.gid = g;
        }
        meta.ctime = time::realtime_timespec();
        Ok(())
    }

    fn set_times(&self, atime: SetTime, mtime: SetTime) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        let now = time::realtime_timespec();
        match atime {
            SetTime::Omit => {}
            SetTime::Now => meta.atime = now,
            SetTime::Set(ts) => meta.atime = ts,
        }
        match mtime {
            SetTime::Omit => {}
            SetTime::Now => meta.mtime = now,
            SetTime::Set(ts) => meta.mtime = ts,
        }
        meta.ctime = now;
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        if self.inode_type == InodeType::Dir {
            return Err(VfsError::IsDir);
        }
        if self.inode_type != InodeType::File {
            return Err(VfsError::Inval);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let meta = self.meta.lock();
        if offset >= meta.size {
            return Ok(0);
        }
        let to_read = buf.len().min((meta.size - offset) as usize);
        let pages = self.pages.lock();
        let mut done = 0usize;
        while done < to_read {
            let cur_off = offset + done as u64;
            let page_idx = cur_off / PAGE_SIZE as u64;
            let in_page = (cur_off % PAGE_SIZE as u64) as usize;
            let chunk = (PAGE_SIZE - in_page).min(to_read - done);
            if let Some(page) = pages.get(page_idx) {
                page.read_bytes(in_page, &mut buf[done..done + chunk]);
            } else {
                // Sparse miss: zero without allocating a frame.
                buf[done..done + chunk].fill(0);
            }
            done += chunk;
        }
        Ok(to_read)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
        if self.inode_type == InodeType::Dir {
            return Err(VfsError::IsDir);
        }
        if self.inode_type != InodeType::File {
            return Err(VfsError::Inval);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let mut meta = self.meta.lock();
        let mut pages = self.pages.lock();
        let old_size = meta.size;
        let end_off = offset.checked_add(buf.len() as u64).ok_or(VfsError::FBig)?;
        if offset > old_size && !old_size.is_multiple_of(PAGE_SIZE as u64) {
            let tail_page = old_size / PAGE_SIZE as u64;
            let tail_off = (old_size % PAGE_SIZE as u64) as usize;
            if let Some(page) = pages.get(tail_page) {
                page.zero_range(tail_off, PAGE_SIZE);
            }
        }
        let mut done = 0usize;
        while done < buf.len() {
            let cur_off = offset + done as u64;
            let page_idx = cur_off / PAGE_SIZE as u64;
            let in_page = (cur_off % PAGE_SIZE as u64) as usize;
            let chunk = (PAGE_SIZE - in_page).min(buf.len() - done);
            if let Some(page) = pages.get(page_idx) {
                page.write_bytes(in_page, &buf[done..done + chunk]);
            } else {
                let cp = Arc::new(CachePage::new_zeroed().ok_or(VfsError::NoMem)?);
                cp.write_bytes(in_page, &buf[done..done + chunk]);
                pages.insert_new(page_idx, cp)?;
            }
            done += chunk;
        }
        let now = time::realtime_timespec();
        if end_off > meta.size {
            meta.size = end_off;
        }
        meta.mtime = now;
        meta.ctime = now;
        Ok(buf.len())
    }

    fn read_page(&self, page_index: u64, buf: &mut [u8; 4096]) -> Result<usize, VfsError> {
        let off = page_index.checked_mul(PAGE_SIZE as u64).ok_or(VfsError::Inval)?;
        self.read_at(off, buf)
    }

    fn write_page(&self, page_index: u64, buf: &[u8]) -> Result<(), VfsError> {
        let off = page_index.checked_mul(PAGE_SIZE as u64).ok_or(VfsError::Inval)?;
        let _ = self.write_at(off, buf)?;
        Ok(())
    }

    fn sync(&self, _data_only: bool) -> Result<(), VfsError> {
        Ok(())
    }

    fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        f(&mut meta.locks);
        Ok(())
    }
}

fn find_tmpfs_non_dir(known_dir: &Arc<TmpfsInode>, target_lock_id: u64) -> Option<Arc<TmpfsInode>> {
    let mut cur = Arc::clone(known_dir);
    loop {
        let next = {
            let m = cur.meta.lock();
            m.parent.as_ref().and_then(Weak::upgrade)
        };
        match next {
            Some(p) if p.lock_id != cur.lock_id => cur = p,
            _ => break,
        }
    }
    let mut stack = alloc::vec![cur];
    while let Some(node) = stack.pop() {
        let m = node.meta.lock();
        for child in m.children.values() {
            if child.lock_id == target_lock_id {
                return Some(Arc::clone(child));
            }
            if child.inode_type == InodeType::Dir {
                stack.push(Arc::clone(child));
            }
        }
    }
    None
}
