//! In-memory POSIX filesystem (`tmpfs`) backed by unified `PageCache` frames
//! and bounded by an atomic per-mount memory quota (default 32 MiB).

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use lumen_vfs::{
    AdvisoryLocks, DevId, DirEntry, Escaped, FileSystem, Ino, InodeType, Metadata, RenameFlags, SetTime, Timespec,
    VfsError, VfsInode, mode_after_chown, next_detached_ino, next_lock_id,
};

use crate::sync::{SleepMutex, SleepMutexGuard};
use crate::time;

use super::page_cache::{self, CachePage, PAGE_SIZE, PageCache, QuotaCounter};

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
        let shared = Arc::new(TmpfsShared {
            dev,
            quota: Arc::new(QuotaCounter::new("tmpfs", quota_pages)),
            next_ino: AtomicU64::new(2),
        });
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
                symlink_target: Vec::new(),
                children: Children::new(),
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
    symlink_target: Vec<u8>,
    children: Children,
    parent: Option<Weak<TmpfsInode>>,
    locks: AdvisoryLocks,
}

/// `readdir`'s cursor for the first entry after `.` (0) and `..` (1).
const FIRST_CHILD_CURSOR: u64 = 2;

/// A directory's entries, each under the sequence number it was added
/// with: `readdir`'s cursor. An entry keeps its number until it is removed
/// (replacing what a name points to, a `rename` over it or a
/// `RENAME_EXCHANGE`, keeps the name's number), and a new name takes the
/// next number, never one used before; so a cursor between two calls still
/// starts where it did, whatever was added or removed meanwhile.
struct Children {
    /// The entries by number: what `readdir` walks.
    by_seq: BTreeMap<u64, (Vec<u8>, Arc<TmpfsInode>)>,
    /// Each name's number, for lookups; always the same names as `by_seq`.
    by_name: BTreeMap<Vec<u8>, u64>,
    /// The next name's number, from [`FIRST_CHILD_CURSOR`].
    next_seq: u64,
}

impl Children {
    fn new() -> Self {
        Self { by_seq: BTreeMap::new(), by_name: BTreeMap::new(), next_seq: FIRST_CHILD_CURSOR }
    }

    fn get(&self, name: &[u8]) -> Option<&Arc<TmpfsInode>> {
        let seq = self.by_name.get(name)?;
        let entry = self.by_seq.get(seq);
        debug_assert!(
            entry.is_some_and(|(n, _)| n == name),
            "tmpfs: {:?}'s number {seq} lists another entry",
            Escaped(name)
        );
        entry.map(|(_, child)| child)
    }

    fn contains_key(&self, name: &[u8]) -> bool {
        self.by_name.contains_key(name)
    }

    fn is_empty(&self) -> bool {
        debug_assert_eq!(self.by_seq.len(), self.by_name.len());
        self.by_seq.is_empty()
    }

    fn values(&self) -> impl Iterator<Item = &Arc<TmpfsInode>> {
        self.by_seq.values().map(|(_, child)| child)
    }

    /// Point `name` at `child`: an existing name keeps its number, a new one
    /// takes the next.
    fn insert(&mut self, name: Vec<u8>, child: Arc<TmpfsInode>) {
        if let Some(&seq) = self.by_name.get(&name) {
            if let Some(slot) = self.by_seq.get_mut(&seq) {
                slot.1 = child;
                return;
            }
            debug_assert!(false, "tmpfs: {:?}'s number {seq} has no entry", Escaped(&name));
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.by_name.insert(name.clone(), seq);
        self.by_seq.insert(seq, (name, child));
    }

    fn remove(&mut self, name: &[u8]) -> Option<Arc<TmpfsInode>> {
        let seq = self.by_name.remove(name)?;
        let entry = self.by_seq.remove(&seq);
        debug_assert!(entry.is_some(), "tmpfs: {:?}'s number {seq} had no entry", Escaped(name));
        entry.map(|(_, child)| child)
    }

    /// The entries numbered `from` or more, in number order (the order they
    /// were added in): `(number, name, inode)`.
    fn listed_from(&self, from: u64) -> impl Iterator<Item = (u64, &[u8], &Arc<TmpfsInode>)> {
        self.by_seq.range(from..).map(|(&seq, (name, child))| (seq, name.as_slice(), child))
    }
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
    /// `write_at` at `offset`, or at the end (`append`, `None`): the size is
    /// read under the same hold of `meta` (Lock 4) the write is made in.
    /// Returns where the write began and how much was written.
    fn write_locked(&self, offset: Option<u64>, buf: &[u8]) -> Result<(u64, usize), VfsError> {
        if self.inode_type == InodeType::Dir {
            return Err(VfsError::IsDir);
        }
        if self.inode_type != InodeType::File {
            return Err(VfsError::Inval);
        }
        let mut meta = self.meta.lock();
        let offset = offset.unwrap_or(meta.size);
        if buf.is_empty() {
            return Ok((offset, 0));
        }
        let done = self.pages.lock().write_from(offset, buf, meta.size, |_| CachePage::new_zeroed())?;
        let written_end = offset + done as u64;
        let now = time::realtime_timespec();
        if written_end > meta.size {
            meta.size = written_end;
        }
        meta.mtime = now;
        meta.ctime = now;
        Ok((offset, done))
    }

    fn new_child(
        shared: &Arc<TmpfsShared>,
        inode_type: InodeType,
        mode: u16,
        nlink: u32,
        symlink_target: Vec<u8>,
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
                children: Children::new(),
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

    fn validate_name(name: &[u8]) -> Result<(), VfsError> {
        if name.is_empty() || name.contains(&b'/') || name.contains(&0) {
            return Err(VfsError::Inval);
        }
        if name.len() > lumen_vfs::NAME_MAX {
            return Err(VfsError::NameTooLong);
        }
        Ok(())
    }

    /// This file's page-cache lock (Lock 5), for
    /// `page_cache::MappedPage::copy_to_frame`.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: file-backed MAP_PRIVATE CoW (#255)
    pub(super) fn page_cache_lock(&self) -> &SleepMutex<PageCache> {
        &self.pages
    }

    /// This file's page `index`, a zero page allocated for a hole, and its
    /// size read with it: `tmpfs`'s side of [`page_cache::mmap_page`] (its
    /// contract). The hole's page
    /// counts against the mount's quota, as a write's would.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: file-backed mmap (#255)
    pub(super) fn mmap_page(&self, index: u64) -> Result<(Arc<CachePage>, u64), VfsError> {
        if self.inode_type != InodeType::File {
            return Err(VfsError::NoDev);
        }
        let meta = self.meta.lock();
        let file_size = meta.size;
        page_cache::mmap_page_in_file(index, file_size)?;
        let mut pages = self.pages.lock();
        let page = match pages.get(index) {
            Some(page) => Arc::clone(page),
            None => {
                let page = Arc::new(CachePage::new_zeroed()?);
                pages.insert_new(index, Arc::clone(&page))?;
                page
            }
        };
        page.touch();
        Ok((page, file_size))
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
    // Depth-first search from the root (`cur`) for `target_lock_id`.
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

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        let meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        if name == b"." {
            return self.self_arc().map(|a| a as Arc<dyn VfsInode>).ok_or(VfsError::NoEnt);
        }
        if name == b".." {
            let p = meta.parent.as_ref().and_then(Weak::upgrade).ok_or(VfsError::NoEnt)?;
            return Ok(p as Arc<dyn VfsInode>);
        }
        meta.children.get(name).map(|c| Arc::clone(c) as Arc<dyn VfsInode>).ok_or(VfsError::NoEnt)
    }

    fn create(&self, name: &[u8], mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        if name == b"." || name == b".." {
            return Err(VfsError::Exist);
        }
        let mut meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        if meta.children.contains_key(name) {
            return Err(VfsError::Exist);
        }
        let child = Self::new_child(&self.shared, InodeType::File, mode, 1, Vec::new(), None);
        meta.children.insert(name.to_vec(), Arc::clone(&child));
        let now = time::realtime_timespec();
        meta.mtime = now;
        meta.ctime = now;
        Ok(child as Arc<dyn VfsInode>)
    }

    fn mkdir(&self, name: &[u8], mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        if name == b"." || name == b".." {
            return Err(VfsError::Exist);
        }
        let self_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        let mut meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        if meta.children.contains_key(name) {
            return Err(VfsError::Exist);
        }
        let child = Self::new_child(&self.shared, InodeType::Dir, mode, 2, Vec::new(), Some(Arc::downgrade(&self_arc)));
        meta.children.insert(name.to_vec(), Arc::clone(&child));
        meta.nlink = meta.nlink.saturating_add(1);
        let now = time::realtime_timespec();
        meta.mtime = now;
        meta.ctime = now;
        Ok(child as Arc<dyn VfsInode>)
    }

    fn unlink(&self, name: &[u8]) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        if name == b"." || name == b".." {
            return Err(VfsError::IsDir);
        }
        let self_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        let child_arc = {
            let meta = self_arc.meta.lock();
            if self_arc.is_detached() || meta.nlink == 0 {
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
            if self_arc.is_detached() || p_meta.nlink == 0 || !p_meta.children.contains_key(name) {
                return Err(VfsError::NoEnt);
            }
            p_meta.children.remove(name);
            p_meta.mtime = now;
            p_meta.ctime = now;
        }
        let c_meta = tmpfs_guard_mut(&mut guards, &child_arc);
        if c_meta.nlink <= 1 {
            Self::detach_if_open(&child_arc, c_meta);
            c_meta.ctime = now;
        } else {
            c_meta.nlink -= 1;
            c_meta.ctime = now;
        }
        Ok(())
    }

    fn rmdir(&self, name: &[u8]) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        Self::validate_name(name)?;
        if name == b"." {
            return Err(VfsError::Inval);
        }
        if name == b".." {
            return Err(VfsError::NotEmpty);
        }
        let self_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        let child_arc = {
            let meta = self_arc.meta.lock();
            if self_arc.is_detached() || meta.nlink == 0 {
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
            if self_arc.is_detached() || p_meta.nlink == 0 || !p_meta.children.contains_key(name) {
                return Err(VfsError::NoEnt);
            }
            p_meta.children.remove(name);
            p_meta.nlink = p_meta.nlink.saturating_sub(1);
            p_meta.mtime = now;
            p_meta.ctime = now;
        }
        let c_meta = tmpfs_guard_mut(&mut guards, &child_arc);
        Self::detach_if_open(&child_arc, c_meta);
        c_meta.ctime = now;
        Ok(())
    }

    fn link(&self, name: &[u8], target: &Arc<dyn VfsInode>) -> Result<(), VfsError> {
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
        if name == b"." || name == b".." {
            return Err(VfsError::Exist);
        }
        let self_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        // Locate target `Arc<TmpfsInode>` from root tree.
        let target_arc = find_tmpfs_non_dir(&self_arc, target.lock_id()).ok_or(VfsError::NoEnt)?;
        let mut guards = lock_tmpfs_ordered(&[&self_arc, &target_arc]);
        if self_arc.is_detached()
            || target_arc.is_detached()
            || tmpfs_guard_mut(&mut guards, &self_arc).nlink == 0
            || tmpfs_guard_mut(&mut guards, &target_arc).nlink == 0
        {
            return Err(VfsError::NoEnt);
        }
        let now = time::realtime_timespec();
        {
            let p_meta = tmpfs_guard_mut(&mut guards, &self_arc);
            if p_meta.children.contains_key(name) {
                return Err(VfsError::Exist);
            }
            p_meta.children.insert(name.to_vec(), Arc::clone(&target_arc));
            p_meta.mtime = now;
            p_meta.ctime = now;
        }
        let t_meta = tmpfs_guard_mut(&mut guards, &target_arc);
        t_meta.nlink = t_meta.nlink.saturating_add(1);
        t_meta.ctime = now;
        Ok(())
    }

    fn symlink(&self, name: &[u8], target: &[u8]) -> Result<Arc<dyn VfsInode>, VfsError> {
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
        if name == b"." || name == b".." {
            return Err(VfsError::Exist);
        }
        let mut meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        if meta.children.contains_key(name) {
            return Err(VfsError::Exist);
        }
        let child = Self::new_child(&self.shared, InodeType::Symlink, 0o777, 1, target.to_vec(), None);
        meta.children.insert(name.to_vec(), Arc::clone(&child));
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
        Ok(meta.symlink_target.clone())
    }

    fn rename(
        &self,
        old_name: &[u8],
        new_dir: &Arc<dyn VfsInode>,
        new_name: &[u8],
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
        if old_name == b"." || old_name == b".." || new_name == b"." || new_name == b".." {
            return Err(VfsError::Inval);
        }

        let old_dir_arc = find_tmpfs_dir_arc(self, self.lock_id, self.ino()).ok_or(VfsError::NoEnt)?;
        let new_dir_arc = find_tmpfs_dir_arc(self, new_dir.lock_id(), new_dir.ino()).ok_or(VfsError::NoEnt)?;

        let (old_child_arc, new_child_opt) = {
            let mut dir_guards = lock_tmpfs_ordered(&[&old_dir_arc, &new_dir_arc]);
            if old_dir_arc.is_detached()
                || new_dir_arc.is_detached()
                || tmpfs_guard_mut(&mut dir_guards, &old_dir_arc).nlink == 0
                || tmpfs_guard_mut(&mut dir_guards, &new_dir_arc).nlink == 0
            {
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
        if old_dir_arc.is_detached()
            || new_dir_arc.is_detached()
            || tmpfs_guard_mut(&mut guards, &old_dir_arc).nlink == 0
            || tmpfs_guard_mut(&mut guards, &new_dir_arc).nlink == 0
        {
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
                old_p.children.insert(old_name.to_vec(), Arc::clone(new_child_arc));
                old_p.mtime = now;
                old_p.ctime = now;
            }
            {
                let new_p = tmpfs_guard_mut(&mut guards, &new_dir_arc);
                new_p.children.insert(new_name.to_vec(), Arc::clone(&old_child_arc));
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
        tmpfs_guard_mut(&mut guards, &new_dir_arc).children.insert(new_name.to_vec(), Arc::clone(&old_child_arc));

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

    /// The cursor is `.` (0), `..` (1), then each entry's sequence number
    /// ([`Children`]): stable as entries come and go, so a listing across
    /// removals lists every entry that stayed exactly once, in the order
    /// they were added.
    fn readdir(&self, cursor: u64, out: &mut Vec<DirEntry>, max_entries: usize) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        let meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 || max_entries == 0 {
            return Ok(());
        }
        let parent_ino = meta.parent.as_ref().and_then(Weak::upgrade).map_or(self.ino(), |p| p.ino());
        let dots = [(0, self.ino(), InodeType::Dir, &b"."[..]), (1, parent_ino, InodeType::Dir, &b".."[..])];
        let children = meta
            .children
            .listed_from(cursor.max(FIRST_CHILD_CURSOR))
            .map(|(seq, name, child)| (seq, child.ino(), child.inode_type, name));
        for (seq, ino, inode_type, name) in dots.into_iter().filter(|&(seq, ..)| seq >= cursor).chain(children) {
            if out.len() >= max_entries {
                break;
            }
            out.push(DirEntry { ino, inode_type, name: name.to_vec(), next_cursor: seq + 1 });
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
        self.pages.lock().resize(meta.size, new_size);
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
        // In the same hold (`VfsInode::set_owner`'s contract).
        meta.mode = mode_after_chown(self.inode_type, meta.mode);
        meta.ctime = time::realtime_timespec();
        Ok(())
    }

    fn set_times(&self, atime: SetTime, mtime: SetTime) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        let now = time::realtime_timespec();
        meta.atime = atime.resolve(meta.atime, now);
        meta.mtime = mtime.resolve(meta.mtime, now);
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
        // A hole reads as zeros without a frame.
        self.pages.lock().read_into(offset, buf, meta.size, |_| Ok(None))
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
        self.write_locked(Some(offset), buf).map(|(_, n)| n)
    }

    fn append(&self, buf: &[u8]) -> Result<(u64, usize), VfsError> {
        self.write_locked(None, buf)
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

    /// For `vfs::mmap_page`.
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
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

crate::ktest!(
    fn tmpfs_mmap_page_hands_out_the_cached_page() {
        // `vfs::mmap_page` on tmpfs: the page holding the file's bytes, the
        // same `Arc` again, coherent with `read_at`/`write_at` both ways; a
        // hole gets a zero page from the quota (`NoSpc` past it), which a
        // `read_at` of a hole never takes; `Range` past EOF, `NoDev` for a
        // directory. (The pages' bytes are used here without Lock 5, as a
        // mapping would: nothing else uses this mount.)
        use crate::vfs::mmap_page;
        use crate::{kassert, kassert_eq};
        let fs = Tmpfs::new(DevId::makedev(0, 125), 4 * PAGE_SIZE);
        let root = fs.root_inode();
        let f = root.create(b"map.bin", 0o644).map_err(|_| "create")?;
        let data: Vec<u8> = (0..PAGE_SIZE + 100).map(|i| (i % 251) as u8 | 1).collect();
        kassert_eq!(f.write_at(0, &data).map_err(|_| "write")?, data.len());
        kassert_eq!(fs.used_pages(), 2);
        let first = mmap_page(&f, 0).map_err(|_| "map page 0")?;
        kassert_eq!(first.file_size(), data.len() as u64);
        let mut got = [0u8; PAGE_SIZE];
        first.page().read_bytes(0, &mut got);
        kassert!(got[..] == data[..PAGE_SIZE]);
        let last = mmap_page(&f, 1).map_err(|_| "map page 1")?;
        last.page().read_bytes(0, &mut got);
        kassert!(got[..100] == data[PAGE_SIZE..]);
        kassert!(got[100..].iter().all(|&b| b == 0)); // past EOF: zero
        let again = mmap_page(&f, 0).map_err(|_| "map page 0 again")?;
        kassert!(Arc::ptr_eq(again.page(), first.page()));
        kassert_eq!(fs.used_pages(), 2); // no page added for either
        // Coherent both ways: a write shows in the page, a store in the
        // page shows in a read.
        kassert_eq!(f.write_at(5, b"written").map_err(|_| "write_at")?, 7);
        first.page().read_bytes(5, &mut got[..7]);
        kassert!(&got[..7] == b"written");
        first.page().write_bytes(20, b"stored");
        let mut back = [0u8; 6];
        kassert_eq!(f.read_at(20, &mut back).map_err(|_| "read_at")?, 6);
        kassert!(&back == b"stored");
        // Holes: a read leaves them be, a mapping takes a page each until
        // the quota (4 pages) refuses.
        f.truncate(5 * PAGE_SIZE as u64).map_err(|_| "truncate")?;
        kassert_eq!(f.read_at(2 * PAGE_SIZE as u64, &mut back).map_err(|_| "read hole")?, 6);
        kassert_eq!(fs.used_pages(), 2);
        let hole = mmap_page(&f, 3).map_err(|_| "map hole")?;
        kassert_eq!(fs.used_pages(), 3);
        hole.page().read_bytes(0, &mut got);
        kassert!(got.iter().all(|&b| b == 0));
        kassert!(Arc::ptr_eq(mmap_page(&f, 3).map_err(|_| "map hole again")?.page(), hole.page()));
        mmap_page(&f, 2).map_err(|_| "map last page of the quota")?;
        kassert_eq!(fs.used_pages(), 4);
        kassert_eq!(mmap_page(&f, 4).err(), Some(VfsError::NoSpc));
        // Past EOF there is no page: `1 << 52` pages is 2^64 bytes, which a
        // plain multiplication wraps to offset 0. Nor in a directory or a
        // symlink.
        kassert_eq!(mmap_page(&f, 5).err(), Some(VfsError::Range));
        kassert_eq!(mmap_page(&f, 1 << 52).err(), Some(VfsError::Range));
        kassert_eq!(mmap_page(&f, u64::MAX).err(), Some(VfsError::Range));
        kassert_eq!(mmap_page(&root, 0).err(), Some(VfsError::NoDev));
        let link = root.symlink(b"link", b"map.bin").map_err(|_| "symlink")?;
        kassert_eq!(mmap_page(&link, 0).err(), Some(VfsError::NoDev));
        Ok(())
    }
);

crate::ktest!(
    fn tmpfs_open_racing_unlink_keeps_the_data_or_fails_cleanly() {
        // #262 on tmpfs (`/tmp`, and the root without an ext4 disk): an
        // unlink landing between an open's path lookup and the open file
        // description it builds. With the open's reference already up the
        // unlink detaches; without it, the file is unlinked with no opener
        // (`nlink == 0`, not detached) and the descriptor is refused.
        use crate::vfs::{KernelVfsLock, KernelVfsTables, read_all_inode};
        use crate::{kassert, kassert_eq};
        use lumen_vfs::{OpenFile, OpenFlags, open_inode, open_inode_guarded, unlink_at};
        // Each failure's `VfsError` goes to the TAP diagnostics.
        fn why(what: &'static str) -> impl FnOnce(VfsError) -> &'static str {
            move |e| {
                crate::ktest::diag(format_args!("{what}: {e:?}"));
                what
            }
        }
        let fs = Tmpfs::new(DevId::makedev(0, 122), 64 * PAGE_SIZE);
        let tables = KernelVfsTables::new();
        tables.mount_root(Arc::clone(&fs) as Arc<dyn FileSystem>).map_err(why("mount_root"))?;
        let data: Vec<u8> = (0..2 * PAGE_SIZE + 100).map(|i| (i as u8).wrapping_mul(31) | 1).collect();
        for name in ["kept", "freed"] {
            let f = fs.root_inode().create(name.as_bytes(), 0o644).map_err(why("create"))?;
            kassert_eq!(f.write_at(0, &data).map_err(why("write"))?, data.len());
        }
        let rd = OpenFlags::from_bits_truncate(OpenFlags::O_RDONLY);

        let opened = open_inode_guarded(&tables, None, b"/kept", rd, 0, 0).map_err(why("open kept"))?;
        unlink_at(&tables, None, b"/kept", false).map_err(why("unlink kept"))?;
        let of = OpenFile::<KernelVfsLock<u64>>::from_opened(opened).map_err(why("OpenFile::from_opened"))?;
        kassert!(of.inode.is_detached());
        kassert_eq!(of.inode.stat().map_err(why("stat kept"))?.nlink, 0);
        kassert_eq!(of.inode.open_count(), 1);
        kassert_eq!(read_all_inode(&of.inode).map_err(why("read kept"))?, data);
        let kept = Arc::clone(&of.inode);
        drop(of);
        kassert_eq!(kept.open_count(), 0);

        let (inode, path) = open_inode(&tables, None, b"/freed", rd, 0, 0).map_err(why("open freed"))?;
        kassert_eq!(inode.open_count(), 0);
        unlink_at(&tables, None, b"/freed", false).map_err(why("unlink freed"))?;
        kassert!(!inode.is_detached());
        kassert_eq!(inode.stat().map_err(why("stat freed"))?.nlink, 0);
        let res =
            OpenFile::<KernelVfsLock<u64>>::new_unchecked_flags(Arc::clone(&inode), Arc::from(path.as_slice()), rd);
        kassert_eq!(res.err(), Some(VfsError::NoEnt));
        kassert_eq!(inode.open_count(), 0);
        Ok(())
    }
);
