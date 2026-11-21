//! Comprehensive host unit tests for `lumen-vfs` backed by a genuine
//! in-memory POSIX filesystem implementation (`MemFs` / `MemInode`).

extern crate std;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::locks::{fcntl_const, flock_const};
use crate::*;

struct StdLock<T>(Mutex<T>);

impl<T> VfsLock<T> for StdLock<T> {
    type Guard<'a>
        = std::sync::MutexGuard<'a, T>
    where
        T: 'a;

    fn new(val: T) -> Self {
        Self(Mutex::new(val))
    }

    fn lock(&self) -> Self::Guard<'_> {
        self.0.lock().unwrap()
    }

    fn get_mut(&mut self) -> &mut T {
        self.0.get_mut().unwrap()
    }
}

type InodeRegistry = Arc<Mutex<BTreeMap<u64, Weak<MemInode>>>>;

struct MemInodeData {
    mode: u16,
    nlink: u32,
    uid: u32,
    gid: u32,
    rdev: DevId,
    data: Vec<u8>,
    symlink_target: String,
    children: BTreeMap<String, Arc<MemInode>>,
    parent: Option<Weak<MemInode>>,
    atime: Timespec,
    mtime: Timespec,
    ctime: Timespec,
}

struct MemInode {
    lock_id: u64,
    dev: DevId,
    ino: AtomicU64,
    inode_type: InodeType,
    open_count: AtomicU32,
    detached: AtomicBool,
    exempt_rofs: bool,
    self_weak: Weak<MemInode>,
    registry: InodeRegistry,
    next_ino: Arc<AtomicU64>,
    magic_target: Mutex<Option<Arc<dyn VfsInode>>>,
    locks: Mutex<AdvisoryLocks>,
    inner: Mutex<MemInodeData>,
}

impl MemInode {
    fn register(arc: &Arc<Self>) {
        arc.registry.lock().unwrap().insert(arc.lock_id, Arc::downgrade(arc));
    }

    fn new_dir(
        dev: DevId,
        ino: Ino,
        mode: u16,
        parent: Option<Weak<MemInode>>,
        registry: InodeRegistry,
        next_ino: Arc<AtomicU64>,
    ) -> Arc<Self> {
        let arc = Arc::new_cyclic(|weak| Self {
            lock_id: next_lock_id(),
            dev,
            ino: AtomicU64::new(ino),
            inode_type: InodeType::Dir,
            open_count: AtomicU32::new(0),
            detached: AtomicBool::new(false),
            exempt_rofs: false,
            self_weak: weak.clone(),
            registry,
            next_ino,
            magic_target: Mutex::new(None),
            locks: Mutex::new(AdvisoryLocks::new()),
            inner: Mutex::new(MemInodeData {
                mode,
                nlink: 2,
                uid: 0,
                gid: 0,
                rdev: DevId(0),
                data: Vec::new(),
                symlink_target: String::new(),
                children: BTreeMap::new(),
                parent,
                atime: Timespec::default(),
                mtime: Timespec::default(),
                ctime: Timespec::default(),
            }),
        });
        Self::register(&arc);
        arc
    }

    fn new_file(dev: DevId, ino: Ino, mode: u16, registry: InodeRegistry, next_ino: Arc<AtomicU64>) -> Arc<Self> {
        let arc = Arc::new_cyclic(|weak| Self {
            lock_id: next_lock_id(),
            dev,
            ino: AtomicU64::new(ino),
            inode_type: InodeType::File,
            open_count: AtomicU32::new(0),
            detached: AtomicBool::new(false),
            exempt_rofs: false,
            self_weak: weak.clone(),
            registry,
            next_ino,
            magic_target: Mutex::new(None),
            locks: Mutex::new(AdvisoryLocks::new()),
            inner: Mutex::new(MemInodeData {
                mode,
                nlink: 1,
                uid: 0,
                gid: 0,
                rdev: DevId(0),
                data: Vec::new(),
                symlink_target: String::new(),
                children: BTreeMap::new(),
                parent: None,
                atime: Timespec::default(),
                mtime: Timespec::default(),
                ctime: Timespec::default(),
            }),
        });
        Self::register(&arc);
        arc
    }

    fn new_char_dev(
        dev: DevId,
        ino: Ino,
        mode: u16,
        rdev: DevId,
        exempt_rofs: bool,
        registry: InodeRegistry,
        next_ino: Arc<AtomicU64>,
    ) -> Arc<Self> {
        let arc = Arc::new_cyclic(|weak| Self {
            lock_id: next_lock_id(),
            dev,
            ino: AtomicU64::new(ino),
            inode_type: InodeType::CharDevice,
            open_count: AtomicU32::new(0),
            detached: AtomicBool::new(false),
            exempt_rofs,
            self_weak: weak.clone(),
            registry,
            next_ino,
            magic_target: Mutex::new(None),
            locks: Mutex::new(AdvisoryLocks::new()),
            inner: Mutex::new(MemInodeData {
                mode,
                nlink: 1,
                uid: 0,
                gid: 0,
                rdev,
                data: Vec::new(),
                symlink_target: String::new(),
                children: BTreeMap::new(),
                parent: None,
                atime: Timespec::default(),
                mtime: Timespec::default(),
                ctime: Timespec::default(),
            }),
        });
        Self::register(&arc);
        arc
    }

    fn new_symlink(dev: DevId, ino: Ino, target: &str, registry: InodeRegistry, next_ino: Arc<AtomicU64>) -> Arc<Self> {
        let arc = Arc::new_cyclic(|weak| Self {
            lock_id: next_lock_id(),
            dev,
            ino: AtomicU64::new(ino),
            inode_type: InodeType::Symlink,
            open_count: AtomicU32::new(0),
            detached: AtomicBool::new(false),
            exempt_rofs: false,
            self_weak: weak.clone(),
            registry,
            next_ino,
            magic_target: Mutex::new(None),
            locks: Mutex::new(AdvisoryLocks::new()),
            inner: Mutex::new(MemInodeData {
                mode: 0o777,
                nlink: 1,
                uid: 0,
                gid: 0,
                rdev: DevId(0),
                data: Vec::new(),
                symlink_target: String::from(target),
                children: BTreeMap::new(),
                parent: None,
                atime: Timespec::default(),
                mtime: Timespec::default(),
                ctime: Timespec::default(),
            }),
        });
        Self::register(&arc);
        arc
    }

    fn alloc_ino(&self) -> Ino {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }

    fn resolve_mem(&self, lock_id: u64) -> Option<Arc<MemInode>> {
        self.registry.lock().unwrap().get(&lock_id).and_then(Weak::upgrade)
    }

    fn mark_detached_if_open(&self) {
        self.detached.store(true, Ordering::Release);
        self.ino.store(next_detached_ino(), Ordering::Release);
    }
}

struct MemFs {
    dev: DevId,
    name: &'static str,
    read_only: bool,
    next_ino: Arc<AtomicU64>,
    registry: InodeRegistry,
    root: Arc<MemInode>,
    rename_lock: Mutex<()>,
}

impl MemFs {
    fn new(dev: DevId, name: &'static str, read_only: bool) -> Arc<Self> {
        let registry: InodeRegistry = Arc::new(Mutex::new(BTreeMap::new()));
        let next_ino = Arc::new(AtomicU64::new(2));
        let root = MemInode::new_dir(dev, 1, 0o755, None, Arc::clone(&registry), Arc::clone(&next_ino));
        root.inner.lock().unwrap().parent = Some(Arc::downgrade(&root));
        Arc::new(Self { dev, name, read_only, next_ino, registry, root, rename_lock: Mutex::new(()) })
    }

    fn add_char_device(
        &self,
        parent: &Arc<MemInode>,
        name: &str,
        mode: u16,
        rdev: DevId,
        exempt_rofs: bool,
    ) -> Arc<MemInode> {
        let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
        let node = MemInode::new_char_dev(
            self.dev,
            ino,
            mode,
            rdev,
            exempt_rofs,
            Arc::clone(&self.registry),
            Arc::clone(&self.next_ino),
        );
        parent.inner.lock().unwrap().children.insert(String::from(name), Arc::clone(&node));
        node
    }

    fn add_magic_link(
        &self,
        parent: &Arc<MemInode>,
        name: &str,
        text_target: &str,
        target: Arc<dyn VfsInode>,
    ) -> Arc<MemInode> {
        let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
        let sym =
            MemInode::new_symlink(self.dev, ino, text_target, Arc::clone(&self.registry), Arc::clone(&self.next_ino));
        *sym.magic_target.lock().unwrap() = Some(target);
        parent.inner.lock().unwrap().children.insert(String::from(name), Arc::clone(&sym));
        sym
    }
}

impl FileSystem for MemFs {
    fn dev_id(&self) -> DevId {
        self.dev
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn root_inode(&self) -> Arc<dyn VfsInode> {
        self.root.clone()
    }

    fn sync(&self) -> Result<(), VfsError> {
        Ok(())
    }

    fn read_only(&self) -> bool {
        self.read_only
    }

    fn with_rename_lock(&self, f: &mut dyn FnMut() -> Result<(), VfsError>) -> Result<(), VfsError> {
        let _guard = self.rename_lock.lock().unwrap();
        f()
    }
}

impl VfsInode for MemInode {
    fn lock_id(&self) -> u64 {
        self.lock_id
    }

    fn dev_id(&self) -> DevId {
        self.dev
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

    fn exempt_from_rofs(&self) -> bool {
        self.exempt_rofs
    }

    fn resolve_magic_link(&self) -> Option<Arc<dyn VfsInode>> {
        self.magic_target.lock().unwrap().clone()
    }

    fn stat(&self) -> Result<Metadata, VfsError> {
        let g = self.inner.lock().unwrap();
        let size = match self.inode_type {
            InodeType::File => g.data.len() as u64,
            InodeType::Symlink => g.symlink_target.len() as u64,
            _ => 0,
        };
        Ok(Metadata {
            dev: self.dev,
            ino: self.ino(),
            inode_type: self.inode_type,
            mode: g.mode,
            nlink: g.nlink,
            uid: g.uid,
            gid: g.gid,
            rdev: g.rdev,
            size,
            blksize: 4096,
            blocks: size.div_ceil(512),
            atime: g.atime,
            mtime: g.mtime,
            ctime: g.ctime,
        })
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if self.is_detached() {
            return Err(VfsError::NoEnt);
        }
        let g = self.inner.lock().unwrap();
        if name == "." {
            return self.self_weak.upgrade().map(|s| s as Arc<dyn VfsInode>).ok_or(VfsError::NoEnt);
        }
        if name == ".." {
            return g.parent.as_ref().and_then(Weak::upgrade).map(|p| p as Arc<dyn VfsInode>).ok_or(VfsError::NoEnt);
        }
        g.children.get(name).map(|c| Arc::clone(c) as Arc<dyn VfsInode>).ok_or(VfsError::NoEnt)
    }

    fn create(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if self.is_detached() {
            return Err(VfsError::NoEnt);
        }
        let mut g = self.inner.lock().unwrap();
        if g.children.contains_key(name) {
            return Err(VfsError::Exist);
        }
        let file = MemInode::new_file(
            self.dev,
            self.alloc_ino(),
            mode & 0o7777,
            Arc::clone(&self.registry),
            Arc::clone(&self.next_ino),
        );
        g.children.insert(String::from(name), Arc::clone(&file));
        Ok(file)
    }

    fn mkdir(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if self.is_detached() {
            return Err(VfsError::NoEnt);
        }
        let mut g = self.inner.lock().unwrap();
        if g.children.contains_key(name) {
            return Err(VfsError::Exist);
        }
        let dir = MemInode::new_dir(
            self.dev,
            self.alloc_ino(),
            mode & 0o7777,
            Some(self.self_weak.clone()),
            Arc::clone(&self.registry),
            Arc::clone(&self.next_ino),
        );
        g.children.insert(String::from(name), Arc::clone(&dir));
        g.nlink += 1;
        Ok(dir)
    }

    fn unlink(&self, name: &str) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        let child = {
            let mut g = self.inner.lock().unwrap();
            let Some(c) = g.children.get(name) else {
                return Err(VfsError::NoEnt);
            };
            if c.inode_type == InodeType::Dir {
                return Err(VfsError::IsDir);
            }
            g.children.remove(name).unwrap()
        };
        let mut cg = child.inner.lock().unwrap();
        cg.nlink = cg.nlink.saturating_sub(1);
        if cg.nlink == 0 && child.open_count() > 0 {
            child.mark_detached_if_open();
        }
        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        let child = {
            let g = self.inner.lock().unwrap();
            let Some(c) = g.children.get(name) else {
                return Err(VfsError::NoEnt);
            };
            if c.inode_type != InodeType::Dir {
                return Err(VfsError::NotDir);
            }
            Arc::clone(c)
        };
        {
            let cg = child.inner.lock().unwrap();
            if !cg.children.is_empty() {
                return Err(VfsError::NotEmpty);
            }
        }
        let mut g = self.inner.lock().unwrap();
        g.children.remove(name);
        g.nlink = g.nlink.saturating_sub(1);
        let mut cg = child.inner.lock().unwrap();
        cg.nlink = 0;
        if child.open_count() > 0 {
            child.mark_detached_if_open();
        }
        Ok(())
    }

    fn link(&self, name: &str, target: &Arc<dyn VfsInode>) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if self.is_detached() {
            return Err(VfsError::NoEnt);
        }
        if target.inode_type() == InodeType::Dir {
            return Err(VfsError::Perm);
        }
        if target.dev_id() != self.dev {
            return Err(VfsError::XDev);
        }
        let target_mem = self.resolve_mem(target.lock_id()).ok_or(VfsError::NoEnt)?;
        {
            let mut g = self.inner.lock().unwrap();
            if g.children.contains_key(name) {
                return Err(VfsError::Exist);
            }
            g.children.insert(String::from(name), Arc::clone(&target_mem));
        }
        target_mem.inner.lock().unwrap().nlink += 1;
        Ok(())
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if self.is_detached() {
            return Err(VfsError::NoEnt);
        }
        let mut g = self.inner.lock().unwrap();
        if g.children.contains_key(name) {
            return Err(VfsError::Exist);
        }
        let sym = MemInode::new_symlink(
            self.dev,
            self.alloc_ino(),
            target,
            Arc::clone(&self.registry),
            Arc::clone(&self.next_ino),
        );
        g.children.insert(String::from(name), Arc::clone(&sym));
        Ok(sym)
    }

    fn readlink(&self) -> Result<Vec<u8>, VfsError> {
        if self.inode_type != InodeType::Symlink {
            return Err(VfsError::Inval);
        }
        Ok(self.inner.lock().unwrap().symlink_target.as_bytes().to_vec())
    }

    fn rename(
        &self,
        old_name: &str,
        new_dir: &Arc<dyn VfsInode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir || new_dir.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if new_dir.dev_id() != self.dev {
            return Err(VfsError::XDev);
        }
        let new_dir_mem = self.resolve_mem(new_dir.lock_id()).ok_or(VfsError::NoEnt)?;
        let same_dir = self.lock_id == new_dir_mem.lock_id;

        if same_dir {
            let mut g = self.inner.lock().unwrap();
            let old_child = g.children.get(old_name).cloned().ok_or(VfsError::NoEnt)?;
            let new_child = g.children.get(new_name).cloned();

            if flags.has_exchange() {
                let dst = new_child.ok_or(VfsError::NoEnt)?;
                g.children.insert(String::from(new_name), old_child);
                g.children.insert(String::from(old_name), dst);
                return Ok(());
            }
            if flags.has_noreplace() && new_child.is_some() {
                return Err(VfsError::Exist);
            }
            if let Some(ref dst) = new_child
                && dst.inode_type == InodeType::Dir
            {
                let dst_g = dst.inner.lock().unwrap();
                if !dst_g.children.is_empty() {
                    return Err(VfsError::NotEmpty);
                }
            }
            g.children.remove(old_name);
            if let Some(dst) = g.children.insert(String::from(new_name), old_child) {
                if dst.inode_type == InodeType::Dir {
                    g.nlink = g.nlink.saturating_sub(1);
                }
                let mut dg = dst.inner.lock().unwrap();
                dg.nlink = if dst.inode_type == InodeType::Dir { 0 } else { dg.nlink.saturating_sub(1) };
                if dg.nlink == 0 && dst.open_count() > 0 {
                    dst.mark_detached_if_open();
                }
            }
            return Ok(());
        }

        // Cross-directory within same FS: lock in ascending `lock_id` order.
        let (mut old_g, mut new_g) = if self.lock_id < new_dir_mem.lock_id {
            (self.inner.lock().unwrap(), new_dir_mem.inner.lock().unwrap())
        } else {
            let ng = new_dir_mem.inner.lock().unwrap();
            let og = self.inner.lock().unwrap();
            (og, ng)
        };

        let old_child = old_g.children.get(old_name).cloned().ok_or(VfsError::NoEnt)?;
        let new_child = new_g.children.get(new_name).cloned();

        if flags.has_exchange() {
            let dst = new_child.ok_or(VfsError::NoEnt)?;
            if old_child.inode_type == InodeType::Dir {
                old_child.inner.lock().unwrap().parent = Some(new_dir_mem.self_weak.clone());
                old_g.nlink = old_g.nlink.saturating_sub(1);
                new_g.nlink += 1;
            }
            if dst.inode_type == InodeType::Dir {
                dst.inner.lock().unwrap().parent = Some(self.self_weak.clone());
                new_g.nlink = new_g.nlink.saturating_sub(1);
                old_g.nlink += 1;
            }
            new_g.children.insert(String::from(new_name), old_child);
            old_g.children.insert(String::from(old_name), dst);
            return Ok(());
        }

        if flags.has_noreplace() && new_child.is_some() {
            return Err(VfsError::Exist);
        }
        if let Some(ref dst) = new_child
            && dst.inode_type == InodeType::Dir
        {
            let dst_g = dst.inner.lock().unwrap();
            if !dst_g.children.is_empty() {
                return Err(VfsError::NotEmpty);
            }
        }

        old_g.children.remove(old_name);
        if old_child.inode_type == InodeType::Dir {
            old_child.inner.lock().unwrap().parent = Some(new_dir_mem.self_weak.clone());
            old_g.nlink = old_g.nlink.saturating_sub(1);
            new_g.nlink += 1;
        }
        if let Some(dst) = new_g.children.insert(String::from(new_name), old_child) {
            if dst.inode_type == InodeType::Dir {
                new_g.nlink = new_g.nlink.saturating_sub(1);
            }
            let mut dg = dst.inner.lock().unwrap();
            dg.nlink = if dst.inode_type == InodeType::Dir { 0 } else { dg.nlink.saturating_sub(1) };
            if dg.nlink == 0 && dst.open_count() > 0 {
                dst.mark_detached_if_open();
            }
        }
        Ok(())
    }

    fn readdir(&self, cursor: u64, out: &mut Vec<DirEntry>, max_entries: usize) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if self.is_detached() {
            return Ok(());
        }
        let g = self.inner.lock().unwrap();
        let parent_ino = g.parent.as_ref().and_then(Weak::upgrade).map_or(self.ino(), |p| p.ino());

        let mut list: Vec<(Ino, InodeType, String)> = Vec::with_capacity(2 + g.children.len());
        list.push((self.ino(), InodeType::Dir, String::from(".")));
        list.push((parent_ino, InodeType::Dir, String::from("..")));
        for (name, child) in &g.children {
            list.push((child.ino(), child.inode_type, name.clone()));
        }

        let start = cursor as usize;
        for (idx, (ino, itype, name)) in list.into_iter().enumerate().skip(start) {
            if out.len() >= max_entries {
                break;
            }
            out.push(DirEntry { ino, inode_type: itype, name, next_cursor: (idx + 1) as u64 });
        }
        Ok(())
    }

    fn truncate(&self, size: u64) -> Result<(), VfsError> {
        if self.inode_type != InodeType::File {
            return Err(VfsError::IsDir);
        }
        self.inner.lock().unwrap().data.resize(size as usize, 0);
        Ok(())
    }

    fn set_mode(&self, mode: u16) -> Result<(), VfsError> {
        self.inner.lock().unwrap().mode = mode & 0o7777;
        Ok(())
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), VfsError> {
        let mut g = self.inner.lock().unwrap();
        if let Some(u) = uid {
            g.uid = u;
        }
        if let Some(gid_val) = gid {
            g.gid = gid_val;
        }
        Ok(())
    }

    fn set_times(&self, atime: SetTime, mtime: SetTime) -> Result<(), VfsError> {
        let mut g = self.inner.lock().unwrap();
        match atime {
            SetTime::Omit => {}
            SetTime::Now => g.atime = Timespec { sec: 100, nsec: 0 },
            SetTime::Set(ts) => g.atime = ts,
        }
        match mtime {
            SetTime::Omit => {}
            SetTime::Now => g.mtime = Timespec { sec: 100, nsec: 0 },
            SetTime::Set(ts) => g.mtime = ts,
        }
        Ok(())
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        if self.inode_type != InodeType::File {
            return Err(VfsError::IsDir);
        }
        let g = self.inner.lock().unwrap();
        let off = offset as usize;
        if off >= g.data.len() {
            return Ok(0);
        }
        let n = buf.len().min(g.data.len() - off);
        buf[..n].copy_from_slice(&g.data[off..off + n]);
        Ok(n)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
        if self.inode_type != InodeType::File {
            return Err(VfsError::IsDir);
        }
        let mut g = self.inner.lock().unwrap();
        let end = offset as usize + buf.len();
        if end > g.data.len() {
            g.data.resize(end, 0);
        }
        g.data[offset as usize..end].copy_from_slice(buf);
        Ok(buf.len())
    }

    fn read_page(&self, page_index: u64, buf: &mut [u8; 4096]) -> Result<usize, VfsError> {
        buf.fill(0);
        self.read_at(page_index * 4096, buf)
    }

    fn write_page(&self, page_index: u64, buf: &[u8]) -> Result<(), VfsError> {
        let _ = self.write_at(page_index * 4096, buf)?;
        Ok(())
    }

    fn sync(&self, _data_only: bool) -> Result<(), VfsError> {
        Ok(())
    }

    fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
        let mut locks = self.locks.lock().unwrap();
        f(&mut locks);
        Ok(())
    }
}

fn setup_vfs() -> (SimpleVfsTables, Arc<MemFs>) {
    let tables = SimpleVfsTables::new();
    let root_fs = MemFs::new(DevId::ROOT, "ext4", false);
    tables.mount_root(root_fs.clone()).unwrap();
    (tables, root_fs)
}

#[test]
fn dev_id_makedev_and_canonical_constants() {
    assert_eq!(DevId::ROOT.major(), 0);
    assert_eq!(DevId::ROOT.minor(), 1);
    assert_eq!(DevId::TMPFS.minor(), 2);
    assert_eq!(DevId::DEVFS.minor(), 3);
    assert_eq!(DevId::PROCFS.minor(), 4);
    assert_eq!(DevId::makedev(5, 1).raw(), 0x501);

    let big = DevId::makedev(0x12345, 0x6789ab);
    assert_eq!(big.major(), 0x12345);
    assert_eq!(big.minor(), 0x6789ab);
}

#[test]
fn errno_and_timespec_constants_match_linux_x86_64() {
    assert_eq!(VfsError::Perm.to_errno(), 1);
    assert_eq!(VfsError::NoEnt.to_errno(), 2);
    assert_eq!(VfsError::BadF.to_errno(), 9);
    assert_eq!(VfsError::Again.to_errno(), 11);
    assert_eq!(VfsError::Busy.to_errno(), 16);
    assert_eq!(VfsError::Exist.to_errno(), 17);
    assert_eq!(VfsError::XDev.to_errno(), 18);
    assert_eq!(VfsError::NotDir.to_errno(), 20);
    assert_eq!(VfsError::IsDir.to_errno(), 21);
    assert_eq!(VfsError::Inval.to_errno(), 22);
    assert_eq!(VfsError::MFile.to_errno(), 24);
    assert_eq!(VfsError::RoFs.to_errno(), 30);
    assert_eq!(VfsError::NameTooLong.to_errno(), 36);
    assert_eq!(VfsError::NotEmpty.to_errno(), 39);
    assert_eq!(VfsError::Loop.to_errno(), 40);
    assert_eq!(VfsError::OpNotSupp.to_errno(), 95);

    assert_eq!(SetTime::from_raw(0, SetTime::UTIME_NOW), Ok(SetTime::Now));
    assert_eq!(SetTime::from_raw(0, SetTime::UTIME_OMIT), Ok(SetTime::Omit));
    assert_eq!(SetTime::from_raw(12, 1_000_000_000), Err(VfsError::Inval));
    assert_eq!(SetTime::from_raw(12, -1), Err(VfsError::Inval));
}

#[test]
fn path_empty_slashes_and_limits() {
    let (env, root_fs) = setup_vfs();
    let dir = mkdir_at(&env, None, b"/dir", 0o755, 0).unwrap();
    let (file, _) =
        open_inode(&env, None, b"/dir/file", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();

    // 1. Empty path
    assert_eq!(resolve_path(&env, Some(&dir), b"", ResolveFlags::default()).unwrap_err(), VfsError::NoEnt);
    let empty_ok =
        resolve_path(&env, Some(&file), b"", ResolveFlags { empty_path: true, ..Default::default() }).unwrap();
    assert_eq!(empty_ok.inode.ino(), file.ino());
    assert_eq!(readlink_at(&env, Some(&file), b"").unwrap_err(), VfsError::Inval);

    // 2. Slash collapsing
    let r1 = resolve_path(&env, None, b"///", ResolveFlags::default()).unwrap();
    assert_eq!(r1.inode.ino(), root_fs.root_inode().ino());
    let r2 = resolve_path(&env, None, b"///dir///file", ResolveFlags::default()).unwrap();
    assert_eq!(r2.inode.ino(), file.ino());

    // 5. PATH_MAX / NAME_MAX
    let too_long_path = vec![b'a'; PATH_MAX];
    assert_eq!(resolve_path(&env, None, &too_long_path, ResolveFlags::default()).unwrap_err(), VfsError::NameTooLong);
    let mut long_comp = vec![b'/'];
    long_comp.extend_from_slice(&vec![b'x'; NAME_MAX + 1]);
    assert_eq!(resolve_path(&env, None, &long_comp, ResolveFlags::default()).unwrap_err(), VfsError::NameTooLong);
    let mut max_comp = vec![b'/'];
    max_comp.extend_from_slice(&vec![b'y'; NAME_MAX]);
    let _ = open_inode(&env, None, &max_comp, OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
}

#[test]
fn path_physical_dotdot_and_non_dir_traversal() {
    let (env, _) = setup_vfs();
    mkdir_at(&env, None, b"/a", 0o755, 0).unwrap();
    mkdir_at(&env, None, b"/x", 0o755, 0).unwrap();
    mkdir_at(&env, None, b"/x/y", 0o755, 0).unwrap();
    let (x_file, _) =
        open_inode(&env, None, b"/x/marker", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    symlink_at(&env, b"/x/y", None, b"/a/link").unwrap();

    // /a/link/.. resolves `..` in `/x/y`, landing in `/x` (not `/a`)!
    let resolved =
        resolve_path(&env, None, b"/a/link/../marker", ResolveFlags { follow_final: true, ..Default::default() })
            .unwrap();
    assert_eq!(resolved.inode.ino(), x_file.ino());

    // `/nonexistent/..` must fail with ENOENT (never lexical collapse).
    assert_eq!(resolve_path(&env, None, b"/nonexistent/..", ResolveFlags::default()).unwrap_err(), VfsError::NoEnt);

    // Relative path from regular file dirfd returns ENOTDIR.
    assert_eq!(resolve_path(&env, Some(&x_file), b"foo", ResolveFlags::default()).unwrap_err(), VfsError::NotDir);
    assert_eq!(resolve_path(&env, Some(&x_file), b".", ResolveFlags::default()).unwrap_err(), VfsError::NotDir);
}

#[test]
fn symlink_depth_relative_and_trailing_slash_rules() {
    let (env, _) = setup_vfs();
    mkdir_at(&env, None, b"/d", 0o755, 0).unwrap();
    let (file, _) =
        open_inode(&env, None, b"/d/target", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();

    // 7. Relative symlink resolves relative to symlink's parent (`/d`), not cwd (`/`).
    symlink_at(&env, b"target", None, b"/d/rel_link").unwrap();
    let r =
        resolve_path(&env, None, b"/d/rel_link", ResolveFlags { follow_final: true, ..Default::default() }).unwrap();
    assert_eq!(r.inode.ino(), file.ino());

    // 6. Symlink depth limit: 40 succeeds, 41 fails with ELOOP.
    mkdir_at(&env, None, b"/chain", 0o755, 0).unwrap();
    open_inode(&env, None, b"/chain/s0", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    for i in 1..=41 {
        let target = format!("s{}", i - 1);
        let name = format!("/chain/s{i}");
        symlink_at(&env, target.as_bytes(), None, name.as_bytes()).unwrap();
    }
    assert!(resolve_path(&env, None, b"/chain/s40", ResolveFlags { follow_final: true, ..Default::default() }).is_ok());
    assert_eq!(
        resolve_path(&env, None, b"/chain/s41", ResolveFlags { follow_final: true, ..Default::default() }).unwrap_err(),
        VfsError::Loop
    );

    // Self-loop -> ELOOP
    symlink_at(&env, b"loop_a", None, b"/loop_b").unwrap();
    symlink_at(&env, b"loop_b", None, b"/loop_a").unwrap();
    assert_eq!(
        resolve_path(&env, None, b"/loop_a", ResolveFlags { follow_final: true, ..Default::default() }).unwrap_err(),
        VfsError::Loop
    );

    // 8. Symlink target ending in `/` pointing to regular file -> ENOTDIR
    symlink_at(&env, b"target/", None, b"/d/file_slash").unwrap();
    assert_eq!(
        resolve_path(&env, None, b"/d/file_slash", ResolveFlags { follow_final: true, ..Default::default() })
            .unwrap_err(),
        VfsError::NotDir
    );

    // 9. Trailing `/` + O_NOFOLLOW / follow_final=false follows symlink and checks Dir
    symlink_at(&env, b"/d", None, b"/sym_to_dir").unwrap();
    symlink_at(&env, b"/d/target", None, b"/sym_to_file").unwrap();
    assert_eq!(
        open_inode(&env, None, b"/sym_to_file/", OpenFlags(OpenFlags::O_RDONLY | OpenFlags::O_NOFOLLOW), 0, 0)
            .unwrap_err(),
        VfsError::NotDir
    );
    let (opened_dir, _) =
        open_inode(&env, None, b"/sym_to_dir/", OpenFlags(OpenFlags::O_RDONLY | OpenFlags::O_NOFOLLOW), 0, 0).unwrap();
    assert_eq!(opened_dir.inode_type(), InodeType::Dir);

    // 10. Without trailing `/`, O_NOFOLLOW -> ELOOP; O_PATH | O_NOFOLLOW -> symlink fd
    assert_eq!(
        open_inode(&env, None, b"/sym_to_file", OpenFlags(OpenFlags::O_RDONLY | OpenFlags::O_NOFOLLOW), 0, 0)
            .unwrap_err(),
        VfsError::Loop
    );
    let (opath_sym, _) =
        open_inode(&env, None, b"/sym_to_file", OpenFlags(OpenFlags::O_PATH | OpenFlags::O_NOFOLLOW), 0, 0).unwrap();
    assert_eq!(opath_sym.inode_type(), InodeType::Symlink);
    assert_eq!(readlink_at(&env, Some(&opath_sym), b"").unwrap(), b"/d/target");

    // 11. O_CREAT | O_EXCL on dangling symlink -> EEXIST; open("foo/", O_CREAT) -> EISDIR
    symlink_at(&env, b"/does_not_exist", None, b"/dangling").unwrap();
    assert_eq!(
        open_inode(
            &env,
            None,
            b"/dangling",
            OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_EXCL | OpenFlags::O_RDWR),
            0o644,
            0
        )
        .unwrap_err(),
        VfsError::Exist
    );
    assert_eq!(
        open_inode(&env, None, b"/new_dir_name/", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap_err(),
        VfsError::IsDir
    );

    // 12. O_TMPFILE, O_DIRECTORY, O_WRONLY|O_TRUNC on dir
    assert_eq!(
        open_inode(&env, None, b"/d", OpenFlags(OpenFlags::O_TMPFILE | OpenFlags::O_RDWR), 0o644, 0).unwrap_err(),
        VfsError::OpNotSupp
    );
    assert_eq!(
        open_inode(&env, None, b"/d/target", OpenFlags(OpenFlags::O_RDONLY | OpenFlags::O_DIRECTORY), 0, 0)
            .unwrap_err(),
        VfsError::NotDir
    );
    assert_eq!(
        open_inode(&env, None, b"/d", OpenFlags(OpenFlags::O_WRONLY | OpenFlags::O_TRUNC), 0, 0).unwrap_err(),
        VfsError::IsDir
    );
}

#[test]
fn magic_symlinks_and_rofs_exempt_chardev() {
    let (env, _root_fs) = setup_vfs();
    let dev_mp = mkdir_at(&env, None, b"/dev", 0o755, 0).unwrap();
    let proc_mp = mkdir_at(&env, None, b"/proc", 0o755, 0).unwrap();

    let devfs = MemFs::new(DevId::DEVFS, "devfs", true);
    let _null_dev = devfs.add_char_device(&devfs.root, "null", 0o666, DevId::makedev(1, 3), true);
    let procfs = MemFs::new(DevId::PROCFS, "procfs", true);

    env.mount(dev_mp, "/dev", devfs).unwrap();
    env.mount(proc_mp, "/proc", procfs.clone()).unwrap();

    // `O_WRONLY | O_TRUNC` succeeds on devfs char device even though devfs is read-only.
    let (null_inode, _) =
        open_inode(&env, None, b"/dev/null", OpenFlags(OpenFlags::O_WRONLY | OpenFlags::O_TRUNC), 0, 0).unwrap();
    assert_eq!(null_inode.inode_type(), InodeType::CharDevice);
    assert_eq!(check_access(&env, &null_inode, AccessMask(AccessMask::W_OK), 0, 0), Ok(()));
    // Namespace mutation on rofs devfs returns EROFS.
    assert_eq!(
        open_inode(&env, None, b"/dev/newfile", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap_err(),
        VfsError::RoFs
    );

    // 13. Magic symlink `/proc/fd0` -> text `/unlinked/path`, live traversal -> target file
    let (live_file, _) =
        open_inode(&env, None, b"/real_file", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    live_file.write_at(0, b"hello").unwrap();
    procfs.add_magic_link(&procfs.root, "fd0", "/nonexistent/text", live_file.clone());

    assert_eq!(readlink_at(&env, None, b"/proc/fd0").unwrap(), b"/nonexistent/text");
    let (via_magic, _) = open_inode(&env, None, b"/proc/fd0", OpenFlags(OpenFlags::O_RDONLY), 0, 0).unwrap();
    assert_eq!(via_magic.ino(), live_file.ino());
}

#[test]
fn mount_table_dotdot_crossing_stacked_mounts_and_exdev() {
    let (env, root_fs) = setup_vfs();
    let mnt = mkdir_at(&env, None, b"/mnt", 0o755, 0).unwrap();
    let tmpfs = MemFs::new(DevId::TMPFS, "tmpfs", false);
    env.mount(mnt.clone(), "/mnt", tmpfs.clone()).unwrap();

    // 4. `..` at global `/` clamps to `/`
    let root_dotdot = resolve_path(&env, None, b"/../..", ResolveFlags::default()).unwrap();
    assert_eq!(root_dotdot.inode.ino(), root_fs.root_inode().ino());

    // `..` at `/mnt` crosses back to `/`
    let sub = mkdir_at(&env, None, b"/mnt/sub", 0o755, 0).unwrap();
    let back = resolve_path(&env, None, b"/mnt/sub/../..", ResolveFlags::default()).unwrap();
    assert_eq!(back.inode.ino(), root_fs.root_inode().ino());
    assert_eq!(back.inode.dev_id(), DevId::ROOT);

    // `compute_cwd_path` inside mountpoint `/mnt/sub`
    assert_eq!(compute_cwd_path(&env, &sub).unwrap(), "/mnt/sub");

    // Stacked mount over `/mnt`
    let mnt_top = resolve_path(&env, None, b"/mnt", ResolveFlags::default()).unwrap().inode;
    assert_eq!(mnt_top.dev_id(), DevId::TMPFS);
    let overlay_fs = MemFs::new(DevId::makedev(0, 99), "tmpfs2", false);
    env.mount(mnt_top.clone(), "/mnt", overlay_fs.clone()).unwrap();

    // `/mnt/..` across stacked mounts unwinds all the way to `/`
    let stacked_back = resolve_path(&env, None, b"/mnt/..", ResolveFlags::default()).unwrap();
    assert_eq!(stacked_back.inode.dev_id(), DevId::ROOT);
    assert_eq!(stacked_back.inode.ino(), root_fs.root_inode().ino());

    // Unmounting underlying mount fails EBUSY while child mount is stacked; unmounting top succeeds.
    assert_eq!(env.umount(mnt.dev_id(), mnt.ino()).unwrap_err(), VfsError::Busy);
    assert!(env.umount(mnt_top.dev_id(), mnt_top.ino()).is_ok());

    // 15. Cross-mount rename / link -> EXDEV
    open_inode(&env, None, b"/src", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    assert_eq!(rename_at2(&env, None, b"/src", None, b"/mnt/dst", RenameFlags(0)).unwrap_err(), VfsError::XDev);
    assert_eq!(link_at(&env, None, b"/src", None, b"/mnt/hl", true, false).unwrap_err(), VfsError::XDev);

    // Busy mountpoint cannot be rmdir'd or overwritten by rename
    assert_eq!(unlink_at(&env, None, b"/mnt", true).unwrap_err(), VfsError::Busy);
    mkdir_at(&env, None, b"/other", 0o755, 0).unwrap();
    assert_eq!(rename_at2(&env, None, b"/other", None, b"/mnt", RenameFlags(0)).unwrap_err(), VfsError::Busy);
}

#[test]
fn dentry_cache_rules_and_invalidation() {
    let mut dcache = DentryCache::new(2);
    // `.` and `..` must never be cached
    dcache.insert(DevId::ROOT, 1, ".", DentryTarget { dev: DevId::ROOT, ino: 1 });
    dcache.insert(DevId::ROOT, 1, "..", DentryTarget { dev: DevId::ROOT, ino: 1 });
    assert_eq!(dcache.len(), 0);
    assert_eq!(dcache.get(DevId::ROOT, 1, "."), None);
    assert_eq!(dcache.get(DevId::ROOT, 1, ".."), None);

    // Bounded eviction
    dcache.insert(DevId::ROOT, 1, "a", DentryTarget { dev: DevId::ROOT, ino: 2 });
    dcache.insert(DevId::ROOT, 1, "b", DentryTarget { dev: DevId::ROOT, ino: 3 });
    dcache.insert(DevId::ROOT, 1, "c", DentryTarget { dev: DevId::ROOT, ino: 4 });
    assert_eq!(dcache.len(), 2);
    assert_eq!(dcache.get(DevId::ROOT, 1, "a"), None);
    assert!(dcache.get(DevId::ROOT, 1, "b").is_some());
    assert!(dcache.get(DevId::ROOT, 1, "c").is_some());

    // 16. Immediate invalidation on unlink / rmdir / rename
    let (env, _) = setup_vfs();
    mkdir_at(&env, None, b"/dir", 0o755, 0).unwrap();
    open_inode(&env, None, b"/dir/f", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    assert!(resolve_path(&env, None, b"/dir/f", ResolveFlags::default()).is_ok());
    assert!(env.dentry_get(DevId::ROOT, 1, "dir").is_some());

    unlink_at(&env, None, b"/dir/f", false).unwrap();
    assert_eq!(resolve_path(&env, None, b"/dir/f", ResolveFlags::default()).unwrap_err(), VfsError::NoEnt);

    rename_at2(&env, None, b"/dir", None, b"/dir2", RenameFlags(0)).unwrap();
    assert_eq!(resolve_path(&env, None, b"/dir", ResolveFlags::default()).unwrap_err(), VfsError::NoEnt);
    assert!(resolve_path(&env, None, b"/dir2", ResolveFlags::default()).is_ok());
}

#[test]
fn unlink_rmdir_and_renameat2_error_matrix() {
    let (env, _) = setup_vfs();
    mkdir_at(&env, None, b"/d", 0o755, 0).unwrap();
    mkdir_at(&env, None, b"/d/sub", 0o755, 0).unwrap();
    open_inode(&env, None, b"/f", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();

    // 17. unlink error matrix
    assert_eq!(unlink_at(&env, None, b"/d", false).unwrap_err(), VfsError::IsDir);
    assert_eq!(unlink_at(&env, None, b"/d/", false).unwrap_err(), VfsError::IsDir);
    assert_eq!(unlink_at(&env, None, b"/f/", false).unwrap_err(), VfsError::NotDir);
    assert_eq!(unlink_at(&env, None, b"/.", false).unwrap_err(), VfsError::Inval);
    assert_eq!(unlink_at(&env, None, b"/..", false).unwrap_err(), VfsError::NotEmpty);

    // 18. rmdir error matrix
    assert_eq!(unlink_at(&env, None, b"/f", true).unwrap_err(), VfsError::NotDir);
    assert_eq!(unlink_at(&env, None, b"/d", true).unwrap_err(), VfsError::NotEmpty);
    assert!(unlink_at(&env, None, b"/d/sub/", true).is_ok());

    // 19. renameat2 error matrix & flags
    assert_eq!(
        rename_at2(&env, None, b"/f", None, b"/g", RenameFlags(RenameFlags::NOREPLACE | RenameFlags::EXCHANGE))
            .unwrap_err(),
        VfsError::Inval
    );
    assert_eq!(rename_at2(&env, None, b"/f", None, b"/g", RenameFlags(1 << 5)).unwrap_err(), VfsError::Inval);

    // rename(a, a)
    assert!(rename_at2(&env, None, b"/f", None, b"/f", RenameFlags(0)).is_ok());
    assert_eq!(
        rename_at2(&env, None, b"/f", None, b"/f", RenameFlags(RenameFlags::NOREPLACE)).unwrap_err(),
        VfsError::Exist
    );

    // RENAME_NOREPLACE and RENAME_EXCHANGE
    open_inode(&env, None, b"/g", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    assert_eq!(
        rename_at2(&env, None, b"/f", None, b"/g", RenameFlags(RenameFlags::NOREPLACE)).unwrap_err(),
        VfsError::Exist
    );
    assert_eq!(
        rename_at2(&env, None, b"/f", None, b"/missing", RenameFlags(RenameFlags::EXCHANGE)).unwrap_err(),
        VfsError::NoEnt
    );
    // RENAME_EXCHANGE swaps file `/f` and dir `/d`
    rename_at2(&env, None, b"/f", None, b"/d", RenameFlags(RenameFlags::EXCHANGE)).unwrap();
    assert_eq!(resolve_path(&env, None, b"/f", ResolveFlags::default()).unwrap().inode.inode_type(), InodeType::Dir);
    assert_eq!(resolve_path(&env, None, b"/d", ResolveFlags::default()).unwrap().inode.inode_type(), InodeType::File);
    // Swap back so `/f` is File and `/d` is Dir
    rename_at2(&env, None, b"/f", None, b"/d", RenameFlags(RenameFlags::EXCHANGE)).unwrap();

    // Dir-over-file -> ENOTDIR; file-over-dir -> EISDIR
    assert_eq!(rename_at2(&env, None, b"/d", None, b"/f", RenameFlags(0)).unwrap_err(), VfsError::NotDir);
    assert_eq!(rename_at2(&env, None, b"/f", None, b"/d", RenameFlags(0)).unwrap_err(), VfsError::IsDir);

    // Non-empty dir overwrite -> ENOTEMPTY
    mkdir_at(&env, None, b"/d/child", 0o755, 0).unwrap();
    mkdir_at(&env, None, b"/d_src", 0o755, 0).unwrap();
    assert_eq!(rename_at2(&env, None, b"/d_src", None, b"/d", RenameFlags(0)).unwrap_err(), VfsError::NotEmpty);

    // Ancestor-into-descendant -> EINVAL (both standard and EXCHANGE)
    mkdir_at(&env, None, b"/anc", 0o755, 0).unwrap();
    mkdir_at(&env, None, b"/anc/child", 0o755, 0).unwrap();
    assert_eq!(rename_at2(&env, None, b"/anc", None, b"/anc/child/dest", RenameFlags(0)).unwrap_err(), VfsError::Inval);
    open_inode(&env, None, b"/anc/child/file", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    assert_eq!(
        rename_at2(&env, None, b"/anc", None, b"/anc/child/file", RenameFlags(RenameFlags::EXCHANGE)).unwrap_err(),
        VfsError::Inval
    );
    assert_eq!(
        rename_at2(&env, None, b"/anc/child/file", None, b"/anc", RenameFlags(RenameFlags::EXCHANGE)).unwrap_err(),
        VfsError::Inval
    );
}

#[test]
fn open_unlinked_file_dir_and_compute_cwd_after_rename() {
    let (env, _) = setup_vfs();
    let (file_inode, _) =
        open_inode(&env, None, b"/open_file", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    let orig_ino = file_inode.ino();
    let orig_lock_id = file_inode.lock_id();
    file_inode.write_at(0, b"payload").unwrap();

    let open_guard = OpenGuard::new(Arc::clone(&file_inode));
    assert_eq!(file_inode.open_count(), 1);

    // 20. Unlink while open: dentry & InodeCache removed, synthetic ino, lock_id immutable, I/O continues
    unlink_at(&env, None, b"/open_file", false).unwrap();
    assert_eq!(resolve_path(&env, None, b"/open_file", ResolveFlags::default()).unwrap_err(), VfsError::NoEnt);
    assert!(env.inode_get(DevId::ROOT, orig_ino).is_none());
    assert!(file_inode.is_detached());
    assert_ne!(file_inode.ino(), orig_ino);
    assert_ne!(file_inode.ino() & DETACHED_INO_BIT, 0);
    assert_eq!(file_inode.lock_id(), orig_lock_id);
    assert_eq!(file_inode.stat().unwrap().nlink, 0);

    let mut buf = [0u8; 7];
    assert_eq!(file_inode.read_at(0, &mut buf).unwrap(), 7);
    assert_eq!(&buf, b"payload");
    drop(open_guard);
    assert_eq!(file_inode.open_count(), 0);

    // 21. compute_cwd_path updates after ancestor rename and returns ENOENT when cwd is rmdir'd
    mkdir_at(&env, None, b"/parent_old", 0o755, 0).unwrap();
    let cwd = mkdir_at(&env, None, b"/parent_old/work", 0o755, 0).unwrap();
    let cwd_guard = OpenGuard::new(Arc::clone(&cwd));
    assert_eq!(compute_cwd_path(&env, &cwd).unwrap(), "/parent_old/work");

    rename_at2(&env, None, b"/parent_old", None, b"/parent_new", RenameFlags(0)).unwrap();
    assert_eq!(compute_cwd_path(&env, &cwd).unwrap(), "/parent_new/work");

    unlink_at(&env, None, b"/parent_new/work", true).unwrap();
    assert_eq!(compute_cwd_path(&env, &cwd).unwrap_err(), VfsError::NoEnt);
    assert_eq!(cwd.lookup("anything").unwrap_err(), VfsError::NoEnt);
    let mut entries = Vec::new();
    cwd.readdir(0, &mut entries, 16).unwrap();
    assert!(entries.is_empty());
    drop(cwd_guard);
}

#[test]
fn fd_table_capacity_dup_and_opath_matrix() {
    let (env, _) = setup_vfs();
    let (file, _) =
        open_inode(&env, None, b"/fdtest", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    let (opath, _) = open_inode(&env, None, b"/fdtest", OpenFlags(OpenFlags::O_PATH), 0, 0).unwrap();

    // 23. O_PATH checks
    let of_path = Arc::new(
        OpenFile::<StdLock<u64>>::new(
            opath,
            Arc::from("/fdtest"),
            OpenFlags(OpenFlags::O_PATH | OpenFlags::O_WRONLY | OpenFlags::O_APPEND),
        )
        .unwrap(),
    );
    assert!(of_path.is_path);
    assert_eq!(of_path.require_non_path(), Err(VfsError::BadF));
    assert_eq!(of_path.require_readable(), Err(VfsError::BadF));
    assert_eq!(of_path.require_writable(), Err(VfsError::BadF));
    assert_eq!(of_path.set_status_flags(OpenFlags::O_APPEND), Err(VfsError::BadF));
    assert_eq!(of_path.set_nonblock(true), Err(VfsError::BadF));
    assert_eq!(of_path.get_fl(), OpenFlags::O_PATH);

    // Normal OpenFile
    let of_rw = Arc::new(
        OpenFile::<StdLock<u64>>::new(file, Arc::from("/fdtest"), OpenFlags(OpenFlags::O_RDWR | OpenFlags::O_APPEND))
            .unwrap(),
    );
    assert_eq!(of_rw.require_non_path(), Ok(()));
    assert_eq!(of_rw.require_readable(), Ok(()));
    assert_eq!(of_rw.require_writable(), Ok(()));
    assert_eq!(of_rw.get_fl(), OpenFlags::O_RDWR | OpenFlags::O_APPEND);
    of_rw.set_nonblock(true).unwrap();
    assert_eq!(of_rw.get_fl(), OpenFlags::O_RDWR | OpenFlags::O_APPEND | OpenFlags::O_NONBLOCK);
    of_rw.inc_fd_ref();
    assert_eq!(of_rw.fd_ref_count(), 2);
    assert_eq!(of_rw.dec_fd_ref(), 1);
    assert_eq!(of_rw.dec_fd_ref(), 0);

    // 22. FdTable 1024 slots & F_DUPFD / F_DUPFD_CLOEXEC
    let mut fds = FdTable::new();
    for i in 0..MAX_FDS {
        let fd = fds.alloc(Arc::clone(&of_rw), i % 2 == 0).unwrap();
        assert_eq!(fd, i as i32);
    }
    assert_eq!(fds.open_count(), MAX_FDS);
    assert_eq!(fds.alloc(Arc::clone(&of_rw), false).unwrap_err(), VfsError::MFile);
    assert_eq!(fds.alloc_from(-1, Arc::clone(&of_rw), false).unwrap_err(), VfsError::Inval);
    assert_eq!(fds.alloc_from(MAX_FDS as i32, Arc::clone(&of_rw), false).unwrap_err(), VfsError::Inval);

    let removed = fds.remove(500).unwrap();
    assert_eq!(removed.file.id, of_rw.id);
    let dup = fds.alloc_from(400, Arc::clone(&of_rw), true).unwrap();
    assert_eq!(dup, 500);
    assert!(fds.get_cloexec(500).unwrap());
    fds.set_cloexec(500, false).unwrap();
    assert!(!fds.get_cloexec(500).unwrap());

    let cloexec_drained = fds.drain_cloexec();
    assert!(!cloexec_drained.is_empty());
    let all_drained = fds.drain_all();
    assert_eq!(all_drained.len() + cloexec_drained.len(), MAX_FDS);
    assert_eq!(fds.open_count(), 0);
}

#[test]
fn advisory_locks_flock_and_posix_independent_namespaces() {
    let mut locks = AdvisoryLocks::new();
    let f1 = OpenFileId(1);
    let f2 = OpenFileId(2);

    // 24. flock(2): shared, conflict on exclusive, atomic upgrade/downgrade
    assert_eq!(locks.flock(f1, FlockOp::from_raw(flock_const::LOCK_SH).unwrap()).unwrap(), LockOutcome::Granted);
    assert_eq!(locks.flock(f2, FlockOp::from_raw(flock_const::LOCK_SH).unwrap()).unwrap(), LockOutcome::Granted);
    // Upgrade f1 while f2 holds SH fails with EAGAIN under LOCK_NB AND preserves f1's SH!
    assert_eq!(
        locks.flock(f1, FlockOp::from_raw(flock_const::LOCK_EX | flock_const::LOCK_NB).unwrap()).unwrap_err(),
        VfsError::Again
    );
    // Release f2, now f1 upgrades SH -> EX atomically
    assert!(locks.release_flock(f2));
    assert_eq!(
        locks.flock(f1, FlockOp::from_raw(flock_const::LOCK_EX | flock_const::LOCK_NB).unwrap()).unwrap(),
        LockOutcome::Granted
    );
    assert_eq!(
        locks.flock(f2, FlockOp::from_raw(flock_const::LOCK_SH | flock_const::LOCK_NB).unwrap()).unwrap_err(),
        VfsError::Again
    );
    // Downgrade EX -> SH allows f2 SH
    assert_eq!(locks.flock(f1, FlockOp::from_raw(flock_const::LOCK_SH).unwrap()).unwrap(), LockOutcome::Granted);
    assert_eq!(
        locks.flock(f2, FlockOp::from_raw(flock_const::LOCK_SH | flock_const::LOCK_NB).unwrap()).unwrap(),
        LockOutcome::Granted
    );

    // 25. fcntl POSIX locks are independent of flock!
    let rw_ctx = LockContext { cur_offset: 100, file_size: 200, readable: true, writable: true };
    let ro_ctx = LockContext { cur_offset: 0, file_size: 200, readable: true, writable: false };

    // Open-mode enforcement
    assert_eq!(
        locks
            .setlk(
                10,
                FlockSpec {
                    l_type: fcntl_const::F_WRLCK,
                    l_whence: fcntl_const::SEEK_SET,
                    l_start: 0,
                    l_len: 10,
                    l_pid: 0,
                },
                ro_ctx,
                false,
            )
            .unwrap_err(),
        VfsError::BadF
    );

    // Negative l_len normalization & invalid negative start
    assert_eq!(
        locks
            .setlk(
                10,
                FlockSpec {
                    l_type: fcntl_const::F_WRLCK,
                    l_whence: fcntl_const::SEEK_SET,
                    l_start: 5,
                    l_len: -10,
                    l_pid: 0,
                },
                rw_ctx,
                false,
            )
            .unwrap_err(),
        VfsError::Inval
    );
    // [100 - 20, 100) => [80, 99] via SEEK_CUR
    assert_eq!(
        locks
            .setlk(
                10,
                FlockSpec {
                    l_type: fcntl_const::F_WRLCK,
                    l_whence: fcntl_const::SEEK_CUR,
                    l_start: 0,
                    l_len: -20,
                    l_pid: 0,
                },
                rw_ctx,
                false,
            )
            .unwrap(),
        LockOutcome::Granted
    );

    // F_GETLK from pid 20 sees pid 10's lock [80, 99] (len 20)
    let mut probe = FlockSpec {
        l_type: fcntl_const::F_RDLCK,
        l_whence: fcntl_const::SEEK_SET,
        l_start: 90,
        l_len: 0, // to EOF (i64::MAX)
        l_pid: 0,
    };
    locks.getlk(20, &mut probe, 0, 200).unwrap();
    assert_eq!(probe.l_type, fcntl_const::F_WRLCK);
    assert_eq!(probe.l_start, 80);
    assert_eq!(probe.l_len, 20);
    assert_eq!(probe.l_pid, 10);

    // pid 10 splits its own lock [80, 99] by unlocking [85, 89]
    locks
        .setlk(
            10,
            FlockSpec {
                l_type: fcntl_const::F_UNLCK,
                l_whence: fcntl_const::SEEK_SET,
                l_start: 85,
                l_len: 5,
                l_pid: 0,
            },
            rw_ctx,
            false,
        )
        .unwrap();
    assert_eq!(locks.posix_lock_count(10), 2);

    // pid 10 re-locks [85, 89] with F_WRLCK -> coalesces back to 1 segment [80, 99]
    locks
        .setlk(
            10,
            FlockSpec {
                l_type: fcntl_const::F_WRLCK,
                l_whence: fcntl_const::SEEK_SET,
                l_start: 85,
                l_len: 5,
                l_pid: 0,
            },
            rw_ctx,
            false,
        )
        .unwrap();
    assert_eq!(locks.posix_lock_count(10), 1);

    // Overwrite middle [85, 89] with F_RDLCK -> splits into 3 segments (WR, RD, WR)
    locks
        .setlk(
            10,
            FlockSpec {
                l_type: fcntl_const::F_RDLCK,
                l_whence: fcntl_const::SEEK_SET,
                l_start: 85,
                l_len: 5,
                l_pid: 0,
            },
            rw_ctx,
            false,
        )
        .unwrap();
    assert_eq!(locks.posix_lock_count(10), 3);

    // Conflict for pid 20 on F_SETLK returns EAGAIN; F_SETLKW returns WouldBlock
    assert_eq!(
        locks
            .setlk(
                20,
                FlockSpec {
                    l_type: fcntl_const::F_WRLCK,
                    l_whence: fcntl_const::SEEK_SET,
                    l_start: 86,
                    l_len: 2,
                    l_pid: 0,
                },
                rw_ctx,
                false,
            )
            .unwrap_err(),
        VfsError::Again
    );
    assert_eq!(
        locks
            .setlk(
                20,
                FlockSpec {
                    l_type: fcntl_const::F_WRLCK,
                    l_whence: fcntl_const::SEEK_SET,
                    l_start: 86,
                    l_len: 2,
                    l_pid: 0,
                },
                rw_ctx,
                true,
            )
            .unwrap(),
        LockOutcome::WouldBlock
    );

    // Releasing POSIX locks for pid 10 clears all 3 segments while keeping flock intact
    assert!(locks.release_posix_for_pid(10));
    assert_eq!(locks.posix_lock_count(10), 0);
    assert_eq!(locks.flock_count(), 2);

    let mut clear_probe =
        FlockSpec { l_type: fcntl_const::F_WRLCK, l_whence: fcntl_const::SEEK_SET, l_start: 0, l_len: 0, l_pid: 0 };
    locks.getlk(20, &mut clear_probe, 0, 200).unwrap();
    assert_eq!(clear_probe.l_type, fcntl_const::F_UNLCK);
}

#[test]
fn hard_links_access_checks_and_symlink_creation_edges() {
    let (env, _) = setup_vfs();
    mkdir_at(&env, None, b"/sub", 0o755, 0).unwrap();
    let (orig, _) =
        open_inode(&env, None, b"/orig", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o640, 0).unwrap();
    orig.write_at(0, b"shared-data").unwrap();

    // Hard link increments nlink and shares inode; unlinking one preserves the other and InodeCache.
    link_at(&env, None, b"/orig", None, b"/sub/alias", false, false).unwrap();
    assert_eq!(orig.stat().unwrap().nlink, 2);
    unlink_at(&env, None, b"/orig", false).unwrap();
    assert_eq!(orig.stat().unwrap().nlink, 1);
    assert!(env.inode_get(DevId::ROOT, orig.ino()).is_some());
    let via_alias = resolve_path(&env, None, b"/sub/alias", ResolveFlags::default()).unwrap().inode;
    assert_eq!(via_alias.ino(), orig.ino());

    // Directory hard link rejected with EPERM.
    assert_eq!(link_at(&env, None, b"/sub", None, b"/sub_link", false, false).unwrap_err(), VfsError::Perm);

    // symlinkat empty target -> ENOENT; trailing slash on missing/existing -> ENOENT/EEXIST
    assert_eq!(symlink_at(&env, b"", None, b"/empty_sym").unwrap_err(), VfsError::NoEnt);
    assert_eq!(symlink_at(&env, b"/sub", None, b"/new_sym/").unwrap_err(), VfsError::NoEnt);
    assert_eq!(symlink_at(&env, b"/sub", None, b"/sub/").unwrap_err(), VfsError::Exist);

    // access(2) checks for root vs non-root uid/gid/other
    orig.set_owner(Some(1000), Some(1000)).unwrap();
    orig.set_mode(0o640).unwrap();
    assert_eq!(check_access(&env, &orig, AccessMask(AccessMask::R_OK | AccessMask::W_OK), 1000, 1000), Ok(()));
    assert_eq!(check_access(&env, &orig, AccessMask(AccessMask::W_OK), 2000, 1000), Err(VfsError::Acces));
    assert_eq!(check_access(&env, &orig, AccessMask(AccessMask::R_OK), 2000, 1000), Ok(()));
    assert_eq!(check_access(&env, &orig, AccessMask(AccessMask::R_OK), 2000, 2000), Err(VfsError::Acces));
    // Root X_OK requires at least one execute bit on non-dirs
    assert_eq!(check_access(&env, &orig, AccessMask(AccessMask::X_OK), 0, 0), Err(VfsError::Acces));
    orig.set_mode(0o641).unwrap();
    assert_eq!(check_access(&env, &orig, AccessMask(AccessMask::X_OK), 0, 0), Ok(()));
}

#[test]
fn lock9_is_never_held_during_inode_calls() {
    struct ProbeGuardEnv {
        mounts: Mutex<MountTable>,
        dentries: Mutex<DentryCache>,
        inodes: Mutex<InodeCache>,
    }

    impl ProbeGuardEnv {
        fn new() -> Self {
            Self {
                mounts: Mutex::new(MountTable::new()),
                dentries: Mutex::new(DentryCache::default()),
                inodes: Mutex::new(InodeCache::new()),
            }
        }

        fn assert_lock9_free(&self) {
            assert!(self.mounts.try_lock().is_ok(), "MountTable Lock 9 held across VfsInode call!");
            assert!(self.dentries.try_lock().is_ok(), "DentryCache Lock 9 held across VfsInode call!");
            assert!(self.inodes.try_lock().is_ok(), "InodeCache Lock 9 held across VfsInode call!");
        }
    }

    impl VfsCacheOps for ProbeGuardEnv {
        fn root_inode(&self) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.mounts.lock().unwrap().root_fs().map(|fs| fs.root_inode()).ok_or(VfsError::NoEnt)
        }
        fn is_global_root(&self, dev: DevId, ino: Ino) -> bool {
            self.mounts.lock().unwrap().is_global_root(dev, ino)
        }
        fn mount_at(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn FileSystem>> {
            self.mounts.lock().unwrap().mount_at(dev, ino)
        }
        fn parent_mount(&self, mounted_dev: DevId) -> Option<Arc<dyn VfsInode>> {
            self.mounts.lock().unwrap().parent_mount(mounted_dev)
        }
        fn parent_mount_if_root(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
            self.mounts.lock().unwrap().parent_mount_if_root(dev, ino)
        }
        fn is_mountpoint(&self, dev: DevId, ino: Ino) -> bool {
            self.mounts.lock().unwrap().is_mountpoint(dev, ino)
        }
        fn fs_read_only(&self, dev: DevId) -> bool {
            self.mounts.lock().unwrap().fs_by_dev(dev).is_some_and(|fs| fs.read_only())
        }
        fn fs_by_dev(&self, dev: DevId) -> Option<Arc<dyn FileSystem>> {
            self.mounts.lock().unwrap().fs_by_dev(dev)
        }
        fn dentry_get(&self, pdev: DevId, pino: Ino, name: &str) -> Option<DentryTarget> {
            self.dentries.lock().unwrap().get(pdev, pino, name)
        }
        fn dentry_insert(&self, pdev: DevId, pino: Ino, name: &str, target: DentryTarget) {
            self.dentries.lock().unwrap().insert(pdev, pino, name, target);
        }
        fn dentry_invalidate(&self, pdev: DevId, pino: Ino, name: &str) {
            self.dentries.lock().unwrap().invalidate(pdev, pino, name);
        }
        fn dentry_invalidate_dir(&self, pdev: DevId, pino: Ino) {
            self.dentries.lock().unwrap().invalidate_dir(pdev, pino);
        }
        fn inode_get(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
            self.inodes.lock().unwrap().get(dev, ino)
        }
        fn inode_get_or_insert(&self, inode: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
            self.inodes.lock().unwrap().get_or_insert(inode)
        }
        fn inode_remove(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
            self.inodes.lock().unwrap().remove(dev, ino)
        }
    }

    struct CheckingInode {
        inner: Arc<dyn VfsInode>,
        env: Arc<ProbeGuardEnv>,
    }

    impl VfsInode for CheckingInode {
        fn lock_id(&self) -> u64 {
            self.inner.lock_id()
        }
        fn dev_id(&self) -> DevId {
            self.inner.dev_id()
        }
        fn ino(&self) -> Ino {
            self.inner.ino()
        }
        fn inode_type(&self) -> InodeType {
            self.inner.inode_type()
        }
        fn open_count(&self) -> u32 {
            self.inner.open_count()
        }
        fn inc_open_count(&self) {
            self.inner.inc_open_count()
        }
        fn dec_open_count(&self) -> u32 {
            self.inner.dec_open_count()
        }
        fn stat(&self) -> Result<Metadata, VfsError> {
            self.env.assert_lock9_free();
            self.inner.stat()
        }
        fn lookup(&self, name: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.env.assert_lock9_free();
            self.inner.lookup(name)
        }
        fn create(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.env.assert_lock9_free();
            self.inner.create(name, mode)
        }
        fn mkdir(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.env.assert_lock9_free();
            self.inner.mkdir(name, mode)
        }
        fn unlink(&self, name: &str) -> Result<(), VfsError> {
            self.env.assert_lock9_free();
            self.inner.unlink(name)
        }
        fn rmdir(&self, name: &str) -> Result<(), VfsError> {
            self.env.assert_lock9_free();
            self.inner.rmdir(name)
        }
        fn link(&self, name: &str, target: &Arc<dyn VfsInode>) -> Result<(), VfsError> {
            self.env.assert_lock9_free();
            self.inner.link(name, target)
        }
        fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.env.assert_lock9_free();
            self.inner.symlink(name, target)
        }
        fn readlink(&self) -> Result<Vec<u8>, VfsError> {
            self.env.assert_lock9_free();
            self.inner.readlink()
        }
        fn rename(
            &self,
            old_name: &str,
            new_dir: &Arc<dyn VfsInode>,
            new_name: &str,
            flags: RenameFlags,
        ) -> Result<(), VfsError> {
            self.env.assert_lock9_free();
            self.inner.rename(old_name, new_dir, new_name, flags)
        }
        fn readdir(&self, cursor: u64, out: &mut Vec<DirEntry>, max_entries: usize) -> Result<(), VfsError> {
            self.env.assert_lock9_free();
            self.inner.readdir(cursor, out, max_entries)
        }
        fn truncate(&self, size: u64) -> Result<(), VfsError> {
            self.inner.truncate(size)
        }
        fn set_mode(&self, mode: u16) -> Result<(), VfsError> {
            self.inner.set_mode(mode)
        }
        fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), VfsError> {
            self.inner.set_owner(uid, gid)
        }
        fn set_times(&self, atime: SetTime, mtime: SetTime) -> Result<(), VfsError> {
            self.inner.set_times(atime, mtime)
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
            self.inner.read_at(offset, buf)
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
            self.inner.write_at(offset, buf)
        }
        fn read_page(&self, page_index: u64, buf: &mut [u8; 4096]) -> Result<usize, VfsError> {
            self.inner.read_page(page_index, buf)
        }
        fn write_page(&self, page_index: u64, buf: &[u8]) -> Result<(), VfsError> {
            self.inner.write_page(page_index, buf)
        }
        fn sync(&self, data_only: bool) -> Result<(), VfsError> {
            self.inner.sync(data_only)
        }
        fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
            self.inner.with_locks_mut(f)
        }
    }

    let root_fs = MemFs::new(DevId::ROOT, "ext4", false);
    let probe = Arc::new(ProbeGuardEnv::new());
    probe.mounts.lock().unwrap().mount_root(root_fs.clone()).unwrap();
    let checking_root: Arc<dyn VfsInode> =
        Arc::new(CheckingInode { inner: root_fs.root_inode(), env: Arc::clone(&probe) });

    mkdir_at(probe.as_ref(), Some(&checking_root), b"a", 0o755, 0).unwrap();
    open_inode(
        probe.as_ref(),
        Some(&checking_root),
        b"a/b",
        OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
        0o644,
        0,
    )
    .unwrap();
    symlink_at(probe.as_ref(), b"b", Some(&checking_root), b"a/s").unwrap();
    let _ = resolve_path(
        probe.as_ref(),
        Some(&checking_root),
        b"a/s",
        ResolveFlags { follow_final: true, ..Default::default() },
    )
    .unwrap();
    rename_at2(probe.as_ref(), Some(&checking_root), b"a/b", Some(&checking_root), b"a/c", RenameFlags(0)).unwrap();
    unlink_at(probe.as_ref(), Some(&checking_root), b"a/s", false).unwrap();
}

#[test]
fn dotdot_unifies_through_inode_cache_and_crosses_stacked_mounts() {
    /// Wrapper whose `lookup("..")` constructs a brand-new `Arc<FreshDotDotInode>`
    /// (mimicking an on-disk filesystem like ext4 returning an uncached inode on `..`).
    struct FreshDotDotInode {
        inner: Arc<dyn VfsInode>,
    }

    impl VfsInode for FreshDotDotInode {
        fn lock_id(&self) -> u64 {
            self.inner.lock_id()
        }
        fn dev_id(&self) -> DevId {
            self.inner.dev_id()
        }
        fn ino(&self) -> Ino {
            self.inner.ino()
        }
        fn inode_type(&self) -> InodeType {
            self.inner.inode_type()
        }
        fn open_count(&self) -> u32 {
            self.inner.open_count()
        }
        fn inc_open_count(&self) {
            self.inner.inc_open_count()
        }
        fn dec_open_count(&self) -> u32 {
            self.inner.dec_open_count()
        }
        fn is_detached(&self) -> bool {
            self.inner.is_detached()
        }
        fn stat(&self) -> Result<Metadata, VfsError> {
            self.inner.stat()
        }
        fn lookup(&self, name: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
            let child = self.inner.lookup(name)?;
            Ok(Arc::new(FreshDotDotInode { inner: child }))
        }
        fn create(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
            let child = self.inner.create(name, mode)?;
            Ok(Arc::new(FreshDotDotInode { inner: child }))
        }
        fn mkdir(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
            let child = self.inner.mkdir(name, mode)?;
            Ok(Arc::new(FreshDotDotInode { inner: child }))
        }
        fn unlink(&self, name: &str) -> Result<(), VfsError> {
            self.inner.unlink(name)
        }
        fn rmdir(&self, name: &str) -> Result<(), VfsError> {
            self.inner.rmdir(name)
        }
        fn link(&self, name: &str, target: &Arc<dyn VfsInode>) -> Result<(), VfsError> {
            self.inner.link(name, target)
        }
        fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
            let child = self.inner.symlink(name, target)?;
            Ok(Arc::new(FreshDotDotInode { inner: child }))
        }
        fn readlink(&self) -> Result<Vec<u8>, VfsError> {
            self.inner.readlink()
        }
        fn rename(
            &self,
            old_name: &str,
            new_dir: &Arc<dyn VfsInode>,
            new_name: &str,
            flags: RenameFlags,
        ) -> Result<(), VfsError> {
            self.inner.rename(old_name, new_dir, new_name, flags)
        }
        fn readdir(&self, cursor: u64, out: &mut Vec<DirEntry>, max_entries: usize) -> Result<(), VfsError> {
            self.inner.readdir(cursor, out, max_entries)
        }
        fn truncate(&self, size: u64) -> Result<(), VfsError> {
            self.inner.truncate(size)
        }
        fn set_mode(&self, mode: u16) -> Result<(), VfsError> {
            self.inner.set_mode(mode)
        }
        fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), VfsError> {
            self.inner.set_owner(uid, gid)
        }
        fn set_times(&self, atime: SetTime, mtime: SetTime) -> Result<(), VfsError> {
            self.inner.set_times(atime, mtime)
        }
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
            self.inner.read_at(offset, buf)
        }
        fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
            self.inner.write_at(offset, buf)
        }
        fn read_page(&self, page_index: u64, buf: &mut [u8; 4096]) -> Result<usize, VfsError> {
            self.inner.read_page(page_index, buf)
        }
        fn write_page(&self, page_index: u64, buf: &[u8]) -> Result<(), VfsError> {
            self.inner.write_page(page_index, buf)
        }
        fn sync(&self, data_only: bool) -> Result<(), VfsError> {
            self.inner.sync(data_only)
        }
        fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
            self.inner.with_locks_mut(f)
        }
    }

    struct FreshRootFs {
        inner: Arc<MemFs>,
    }

    impl FileSystem for FreshRootFs {
        fn dev_id(&self) -> DevId {
            self.inner.dev_id()
        }
        fn name(&self) -> &'static str {
            self.inner.name()
        }
        fn root_inode(&self) -> Arc<dyn VfsInode> {
            Arc::new(FreshDotDotInode { inner: self.inner.root_inode() })
        }
        fn sync(&self) -> Result<(), VfsError> {
            self.inner.sync()
        }
        fn read_only(&self) -> bool {
            self.inner.read_only()
        }
        fn with_rename_lock(&self, f: &mut dyn FnMut() -> Result<(), VfsError>) -> Result<(), VfsError> {
            self.inner.with_rename_lock(f)
        }
    }

    let env = SimpleVfsTables::new();
    let mem_fs = MemFs::new(DevId::ROOT, "fresh", false);
    let fresh_fs = Arc::new(FreshRootFs { inner: Arc::clone(&mem_fs) });
    env.mount_root(fresh_fs).unwrap();

    let dir = mkdir_at(&env, None, b"/dir", 0o755, 0).unwrap();
    let sub = mkdir_at(&env, None, b"/dir/sub", 0o755, 0).unwrap();

    // Verify `lookup("..")` on `sub` returns a distinct Arc pointer outside InodeCache.
    let raw_parent = sub.lookup("..").unwrap();
    assert!(!Arc::ptr_eq(&raw_parent, &dir));

    // `resolve_path("/dir/sub/..")` must unify through InodeCache to the exact `dir` Arc.
    let via_dotdot = resolve_path(&env, None, b"/dir/sub/..", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&via_dotdot, &dir));

    let guard = OpenGuard::new(via_dotdot);
    assert_eq!(dir.open_count(), 1);
    drop(guard);
    assert_eq!(dir.open_count(), 0);

    // `compute_cwd_path` also walks `..` cleanly.
    assert_eq!(compute_cwd_path(&env, &sub).unwrap(), "/dir/sub");
}

#[test]
fn overmounted_global_root_dotdot_clamps_and_stacked_step_up() {
    let (env, root_fs) = setup_vfs();
    open_inode(&env, None, b"/hidden_secret", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();

    // Prepare an obscured subdirectory under `/mnt/sub` before mounting over `/mnt`.
    let mnt = mkdir_at(&env, None, b"/mnt", 0o755, 0).unwrap();
    let obscured_sub = mkdir_at(&env, None, b"/mnt/sub", 0o755, 0).unwrap();
    let tmpfs = MemFs::new(DevId::TMPFS, "tmpfs", false);
    env.mount(mnt, "/mnt", tmpfs.clone()).unwrap();

    // Stepping `..` from obscured `/mnt/sub` lands on the mounted `tmpfs` root!
    let stepped = resolve_path(&env, Some(&obscured_sub), b"..", ResolveFlags::default()).unwrap().inode;
    assert_eq!(stepped.dev_id(), DevId::TMPFS);
    assert_eq!(stepped.ino(), tmpfs.root_inode().ino());

    // Now overmount global `/` itself with `overlay_root`.
    let overlay_root = MemFs::new(DevId::makedev(0, 88), "overlay", false);
    env.mount(root_fs.root_inode(), "/", overlay_root.clone()).unwrap();
    open_inode(&env, None, b"/visible", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    let overlay_sub = mkdir_at(&env, None, b"/sub", 0o755, 0).unwrap();

    // `/..` must stay clamped on `overlay_root`, never escaping to `root_fs`.
    let dotdot = resolve_path(&env, None, b"/..", ResolveFlags::default()).unwrap().inode;
    assert_eq!(dotdot.dev_id(), overlay_root.dev_id());
    assert_eq!(dotdot.ino(), overlay_root.root_inode().ino());
    assert_eq!(resolve_path(&env, None, b"/../hidden_secret", ResolveFlags::default()).unwrap_err(), VfsError::NoEnt);
    assert!(resolve_path(&env, None, b"/../visible", ResolveFlags::default()).is_ok());

    // `compute_cwd_path` on overmounted `/` and `/sub`.
    assert_eq!(compute_cwd_path(&env, &overlay_root.root_inode()).unwrap(), "/");
    assert_eq!(compute_cwd_path(&env, &overlay_sub).unwrap(), "/sub");
}

#[test]
fn dentry_cache_churn_bounded_order_and_no_premature_eviction() {
    // Challenger 1 Bug #5 reproducer: stale `order` entries must never evict freshly re-inserted keys.
    let mut dc = DentryCache::new(2);
    let t1 = DentryTarget { dev: DevId::ROOT, ino: 10 };
    let t2 = DentryTarget { dev: DevId::ROOT, ino: 20 };
    let t3 = DentryTarget { dev: DevId::ROOT, ino: 30 };
    let t4 = DentryTarget { dev: DevId::ROOT, ino: 40 };

    dc.insert(DevId::ROOT, 1, "a", t1);
    dc.insert(DevId::ROOT, 1, "b", t2);
    dc.invalidate(DevId::ROOT, 1, "a");
    dc.insert(DevId::ROOT, 1, "a", t3);
    // In-place update preserves generation without growing `order`.
    let t3_updated = DentryTarget { dev: DevId::ROOT, ino: 31 };
    dc.insert(DevId::ROOT, 1, "a", t3_updated);
    dc.insert(DevId::ROOT, 1, "c", t4);

    assert_eq!(dc.get(DevId::ROOT, 1, "a"), Some(t3_updated));
    assert_eq!(dc.get(DevId::ROOT, 1, "b"), None);
    assert_eq!(dc.get(DevId::ROOT, 1, "c"), Some(t4));

    // Repeated create+invalidate churn below `max_entries` must keep `order` strictly bounded.
    let mut churn = DentryCache::new(8);
    for i in 0..5_000u64 {
        let name = format!("tmp_{i}");
        churn.insert(DevId::ROOT, 1, &name, DentryTarget { dev: DevId::ROOT, ino: i + 100 });
        churn.invalidate(DevId::ROOT, 1, &name);
        assert!(churn.order_len() <= 16, "order_len leaked to {}", churn.order_len());
    }
    assert_eq!(churn.len(), 0);

    // `invalidate_dir` and `invalidate_fs` + re-insertion also avoid premature eviction.
    let mut dc2 = DentryCache::new(2);
    dc2.insert(DevId::ROOT, 1, "x", t1);
    dc2.insert(DevId::ROOT, 2, "y", t2);
    dc2.invalidate_dir(DevId::ROOT, 1);
    dc2.insert(DevId::ROOT, 1, "x", t3);
    dc2.insert(DevId::ROOT, 3, "z", t4);
    assert_eq!(dc2.get(DevId::ROOT, 1, "x"), Some(t3));
    assert_eq!(dc2.get(DevId::ROOT, 2, "y"), None);
    assert_eq!(dc2.get(DevId::ROOT, 3, "z"), Some(t4));

    dc2.invalidate_fs(DevId::ROOT);
    assert_eq!(dc2.len(), 0);
}

#[test]
fn open_creat_dangling_symlinks_and_existing_dir_edges() {
    let (env, _) = setup_vfs();
    mkdir_at(&env, None, b"/sub", 0o755, 0).unwrap();
    let existing_dir = mkdir_at(&env, None, b"/existing_dir", 0o755, 0).unwrap();
    let (existing_file, _) =
        open_inode(&env, None, b"/sub/existing_file", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    existing_file.write_at(0, b"old-data").unwrap();

    // 1. Single dangling symlink without O_EXCL creates target.
    symlink_at(&env, b"/new_via_sym", None, b"/dangling").unwrap();
    let (created, _) =
        open_inode(&env, None, b"/dangling", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    created.write_at(0, b"via-sym").unwrap();
    let resolved_target = resolve_path(&env, None, b"/new_via_sym", ResolveFlags::default()).unwrap().inode;
    assert_eq!(resolved_target.ino(), created.ino());

    // 2. Relative dangling symlink resolves relative to symlink's parent `/sub`, not `/`.
    symlink_at(&env, b"rel_created", None, b"/sub/rel_dangling").unwrap();
    let (rel_created, _) =
        open_inode(&env, None, b"/sub/rel_dangling", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    assert_eq!(
        resolve_path(&env, None, b"/sub/rel_created", ResolveFlags::default()).unwrap().inode.ino(),
        rel_created.ino()
    );
    assert_eq!(resolve_path(&env, None, b"/rel_created", ResolveFlags::default()).unwrap_err(), VfsError::NoEnt);

    // 3. Chained dangling symlinks across directories.
    symlink_at(&env, b"sub/s2", None, b"/s1").unwrap();
    symlink_at(&env, b"../chain_final", None, b"/sub/s2").unwrap();
    let (chain_final, _) =
        open_inode(&env, None, b"/s1", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    assert_eq!(
        resolve_path(&env, None, b"/chain_final", ResolveFlags::default()).unwrap().inode.ino(),
        chain_final.ino()
    );

    // 4. Symlink to existing file with O_CREAT | O_RDWR | O_TRUNC opens & truncates.
    symlink_at(&env, b"/sub/existing_file", None, b"/sym_existing").unwrap();
    let (opened_existing, _) = open_inode(
        &env,
        None,
        b"/sym_existing",
        OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .unwrap();
    assert_eq!(opened_existing.ino(), existing_file.ino());
    assert_eq!(opened_existing.stat().unwrap().size, 0);

    // 5. Symlink depth 40 succeeds, 41 returns ELOOP under O_CREAT.
    mkdir_at(&env, None, b"/c", 0o755, 0).unwrap();
    symlink_at(&env, b"final_target", None, b"/c/l1").unwrap();
    for i in 2..=41 {
        let tgt = format!("l{}", i - 1);
        let name = format!("/c/l{i}");
        symlink_at(&env, tgt.as_bytes(), None, name.as_bytes()).unwrap();
    }
    assert_eq!(
        open_inode(&env, None, b"/c/l41", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap_err(),
        VfsError::Loop
    );
    let (l40_file, _) =
        open_inode(&env, None, b"/c/l40", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    assert_eq!(
        resolve_path(&env, None, b"/c/final_target", ResolveFlags::default()).unwrap().inode.ino(),
        l40_file.ino()
    );

    // 6. Dangling symlink target ending in `/` -> EISDIR; target ending in `/` pointing to file -> ENOTDIR.
    symlink_at(&env, b"/sub/missing_dir/", None, b"/sym_missing_slash").unwrap();
    assert_eq!(
        open_inode(&env, None, b"/sym_missing_slash", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap_err(),
        VfsError::IsDir
    );
    symlink_at(&env, b"/sub/existing_file/", None, b"/sym_file_slash").unwrap();
    assert_eq!(
        open_inode(&env, None, b"/sym_file_slash", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap_err(),
        VfsError::NotDir
    );

    // 7. O_CREAT | O_EXCL on dangling symlink -> EEXIST; O_CREAT | O_NOFOLLOW -> ELOOP; O_PATH | O_CREAT | O_NOFOLLOW -> symlink fd.
    symlink_at(&env, b"/never_created", None, b"/dangling2").unwrap();
    assert_eq!(
        open_inode(
            &env,
            None,
            b"/dangling2",
            OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_EXCL | OpenFlags::O_RDWR),
            0o644,
            0
        )
        .unwrap_err(),
        VfsError::Exist
    );
    assert_eq!(
        open_inode(
            &env,
            None,
            b"/dangling2",
            OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_NOFOLLOW | OpenFlags::O_RDWR),
            0o644,
            0
        )
        .unwrap_err(),
        VfsError::Loop
    );
    let (opath_dangling, _) = open_inode(
        &env,
        None,
        b"/dangling2",
        OpenFlags(OpenFlags::O_PATH | OpenFlags::O_CREAT | OpenFlags::O_NOFOLLOW),
        0o644,
        0,
    )
    .unwrap();
    assert_eq!(opath_dangling.inode_type(), InodeType::Symlink);

    // 8. O_CREAT | O_DIRECTORY on dangling symlink -> ENOTDIR; O_CREAT into ROFS via symlink -> EROFS.
    assert_eq!(
        open_inode(
            &env,
            None,
            b"/dangling2",
            OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_DIRECTORY | OpenFlags::O_RDWR),
            0o644,
            0
        )
        .unwrap_err(),
        VfsError::NotDir
    );
    let ro_mp = mkdir_at(&env, None, b"/ro", 0o755, 0).unwrap();
    let ro_fs = MemFs::new(DevId::makedev(0, 70), "rofs", true);
    env.mount(ro_mp, "/ro", ro_fs).unwrap();
    symlink_at(&env, b"/ro/newfile", None, b"/sym_to_ro").unwrap();
    assert_eq!(
        open_inode(&env, None, b"/sym_to_ro", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap_err(),
        VfsError::RoFs
    );

    // 9. O_CREAT | O_RDONLY on existing dir, `/.`, and symlink-to-dir -> EISDIR; with O_EXCL -> EEXIST.
    symlink_at(&env, b"/existing_dir", None, b"/sym_to_existing_dir").unwrap();
    assert_eq!(
        open_inode(&env, None, b"/existing_dir", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0)
            .unwrap_err(),
        VfsError::IsDir
    );
    assert_eq!(
        open_inode(&env, Some(&existing_dir), b".", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0)
            .unwrap_err(),
        VfsError::IsDir
    );
    assert_eq!(
        open_inode(&env, None, b"/sym_to_existing_dir", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0)
            .unwrap_err(),
        VfsError::IsDir
    );
    assert_eq!(
        open_inode(
            &env,
            None,
            b"/existing_dir",
            OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_EXCL | OpenFlags::O_RDONLY),
            0o644,
            0
        )
        .unwrap_err(),
        VfsError::Exist
    );

    // 10. Stacked mounts: `open("/mnt", O_RDONLY)` crosses all layers; `open("/mnt", O_CREAT | O_RDONLY)` returns EISDIR.
    let mnt = mkdir_at(&env, None, b"/mnt", 0o755, 0).unwrap();
    let fs2 = MemFs::new(DevId::TMPFS, "tmpfs2", false);
    let fs3 = MemFs::new(DevId::makedev(0, 77), "tmpfs3", false);
    env.mount(mnt, "/mnt", fs2.clone()).unwrap();
    env.mount(fs2.root_inode(), "/mnt", fs3.clone()).unwrap();
    let (opened_mnt, _) = open_inode(&env, None, b"/mnt", OpenFlags(OpenFlags::O_RDONLY), 0, 0).unwrap();
    assert_eq!(opened_mnt.dev_id(), fs3.dev_id());
    assert_eq!(
        open_inode(&env, None, b"/mnt", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0).unwrap_err(),
        VfsError::IsDir
    );
}

#[test]
fn symlink_at_trailing_slash_preserves_intermediate_enotdir() {
    let (env, _) = setup_vfs();
    open_inode(&env, None, b"/existing_file", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();

    assert_eq!(symlink_at(&env, b"t", None, b"/existing_file/sub/").unwrap_err(), VfsError::NotDir);
    assert_eq!(symlink_at(&env, b"t", None, b"/missing/sub/").unwrap_err(), VfsError::NoEnt);
    assert_eq!(symlink_at(&env, b"t", None, b"/existing_file/").unwrap_err(), VfsError::Exist);
    assert_eq!(symlink_at(&env, b"t", None, b"/nonexistent/").unwrap_err(), VfsError::NoEnt);
}

#[test]
fn post_mutation_dentry_invalidation_and_fd_ref_sync() {
    struct SpyEnv {
        inner: SimpleVfsTables,
        invalidations: AtomicU32,
        dir_invalidations: AtomicU32,
    }

    impl SpyEnv {
        fn new(root: Arc<MemFs>) -> Self {
            let inner = SimpleVfsTables::new();
            inner.mount_root(root).unwrap();
            Self { inner, invalidations: AtomicU32::new(0), dir_invalidations: AtomicU32::new(0) }
        }
        fn take_counts(&self) -> (u32, u32) {
            (self.invalidations.swap(0, Ordering::AcqRel), self.dir_invalidations.swap(0, Ordering::AcqRel))
        }
    }

    impl VfsCacheOps for SpyEnv {
        fn root_inode(&self) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.inner.root_inode()
        }
        fn is_global_root(&self, dev: DevId, ino: Ino) -> bool {
            self.inner.is_global_root(dev, ino)
        }
        fn mount_at(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn FileSystem>> {
            self.inner.mount_at(dev, ino)
        }
        fn parent_mount(&self, mounted_dev: DevId) -> Option<Arc<dyn VfsInode>> {
            self.inner.parent_mount(mounted_dev)
        }
        fn parent_mount_if_root(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
            self.inner.parent_mount_if_root(dev, ino)
        }
        fn is_mountpoint(&self, dev: DevId, ino: Ino) -> bool {
            self.inner.is_mountpoint(dev, ino)
        }
        fn fs_read_only(&self, dev: DevId) -> bool {
            self.inner.fs_read_only(dev)
        }
        fn fs_by_dev(&self, dev: DevId) -> Option<Arc<dyn FileSystem>> {
            self.inner.fs_by_dev(dev)
        }
        fn dentry_get(&self, pdev: DevId, pino: Ino, name: &str) -> Option<DentryTarget> {
            self.inner.dentry_get(pdev, pino, name)
        }
        fn dentry_insert(&self, pdev: DevId, pino: Ino, name: &str, target: DentryTarget) {
            self.inner.dentry_insert(pdev, pino, name, target)
        }
        fn dentry_invalidate(&self, pdev: DevId, pino: Ino, name: &str) {
            self.invalidations.fetch_add(1, Ordering::AcqRel);
            self.inner.dentry_invalidate(pdev, pino, name);
        }
        fn dentry_invalidate_dir(&self, pdev: DevId, pino: Ino) {
            self.dir_invalidations.fetch_add(1, Ordering::AcqRel);
            self.inner.dentry_invalidate_dir(pdev, pino);
        }
        fn inode_get(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
            self.inner.inode_get(dev, ino)
        }
        fn inode_get_or_insert(&self, inode: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
            self.inner.inode_get_or_insert(inode)
        }
        fn inode_remove(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
            self.inner.inode_remove(dev, ino)
        }
    }

    let root_fs = MemFs::new(DevId::ROOT, "ext4", false);
    let spy = SpyEnv::new(root_fs);

    let (file, _) =
        open_inode(&spy, None, b"/f1", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    mkdir_at(&spy, None, b"/d1", 0o755, 0).unwrap();
    mkdir_at(&spy, None, b"/d2", 0o755, 0).unwrap();
    let _ = spy.take_counts();

    // unlink invokes both pre- and post-mutation `dentry_invalidate` (2 calls).
    unlink_at(&spy, None, b"/f1", false).unwrap();
    assert_eq!(spy.take_counts(), (2, 0));

    // rmdir invokes both pre- and post-mutation `dentry_invalidate` and `dentry_invalidate_dir` (2, 2).
    unlink_at(&spy, None, b"/d1", true).unwrap();
    assert_eq!(spy.take_counts(), (2, 2));

    // non-EXCHANGE rename over existing dir invokes pre- and post-mutation invalidation on both paths and overwritten dir (4, 2).
    mkdir_at(&spy, None, b"/d1", 0o755, 0).unwrap();
    let _ = spy.take_counts();
    rename_at2(&spy, None, b"/d1", None, b"/d2", RenameFlags(0)).unwrap();
    assert_eq!(spy.take_counts(), (4, 2));

    // `OpenFile::dec_fd_ref` AcqRel reference counting transitions cleanly to 0.
    let of = OpenFile::<StdLock<u64>>::new(file, Arc::from("/f1"), OpenFlags(OpenFlags::O_RDWR)).unwrap();
    of.inc_fd_ref();
    of.inc_fd_ref();
    assert_eq!(of.fd_ref_count(), 3);
    assert_eq!(of.dec_fd_ref(), 2);
    assert_eq!(of.dec_fd_ref(), 1);
    assert_eq!(of.dec_fd_ref(), 0);
}

#[test]
fn unchallenged_edges_tmpfile_link_empty_path_and_access() {
    let (env, _) = setup_vfs();
    let dir = mkdir_at(&env, None, b"/dir", 0o755, 0).unwrap();

    // 1. O_TMPFILE -> EOPNOTSUPP across access modes and flags.
    for extra in [OpenFlags::O_RDONLY, OpenFlags::O_RDWR, OpenFlags::O_WRONLY | OpenFlags::O_EXCL] {
        assert_eq!(
            open_inode(&env, None, b"/dir", OpenFlags(OpenFlags::O_TMPFILE | extra), 0o600, 0).unwrap_err(),
            VfsError::OpNotSupp
        );
    }

    // 2. link_at with `empty_path: true` on live vs unlinked FDs.
    let (live_file, _) =
        open_inode(&env, None, b"/dir/live", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    let guard = OpenGuard::new(Arc::clone(&live_file));
    link_at(&env, Some(&live_file), b"", None, b"/dir/hardlink", false, true).unwrap();
    assert_eq!(live_file.stat().unwrap().nlink, 2);

    // Unlink both links while `guard` keeps inode open & detached; `link_at(..., empty_path: true)` must fail ENOENT.
    unlink_at(&env, None, b"/dir/live", false).unwrap();
    unlink_at(&env, None, b"/dir/hardlink", false).unwrap();
    assert!(live_file.is_detached());
    assert_eq!(live_file.stat().unwrap().nlink, 0);
    assert_eq!(
        link_at(&env, Some(&live_file), b"", None, b"/dir/resurrect", false, true).unwrap_err(),
        VfsError::NoEnt
    );
    drop(guard);

    // 3. check_access uid/gid/mode edges.
    let (acc_file, _) =
        open_inode(&env, None, b"/dir/acc", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o077, 0).unwrap();
    acc_file.set_owner(Some(1000), Some(1000)).unwrap();

    // Invalid mask -> EINVAL.
    assert_eq!(check_access(&env, &acc_file, AccessMask(1 << 4), 1000, 1000), Err(VfsError::Inval));
    // F_OK succeeds even with 0000 mode.
    acc_file.set_mode(0o000).unwrap();
    assert_eq!(check_access(&env, &acc_file, AccessMask(AccessMask::F_OK), 2000, 2000), Ok(()));
    // Owner bits take precedence over group/other even when group/other are 7.
    acc_file.set_mode(0o077).unwrap();
    assert_eq!(check_access(&env, &acc_file, AccessMask(AccessMask::R_OK), 1000, 1000), Err(VfsError::Acces));
    assert_eq!(check_access(&env, &acc_file, AccessMask(AccessMask::W_OK), 1000, 2000), Err(VfsError::Acces));
    assert_eq!(check_access(&env, &acc_file, AccessMask(AccessMask::X_OK), 1000, 2000), Err(VfsError::Acces));
    // Group bits take precedence over other bits when uid != owner && gid == file.gid.
    acc_file.set_mode(0o707).unwrap();
    assert_eq!(check_access(&env, &acc_file, AccessMask(AccessMask::R_OK), 2000, 1000), Err(VfsError::Acces));
    assert_eq!(check_access(&env, &acc_file, AccessMask(AccessMask::R_OK), 2000, 2000), Ok(()));
    // Root bypasses R_OK/W_OK on 0000 file and X_OK on 0000 dir, but fails X_OK on 0000 file.
    acc_file.set_mode(0o000).unwrap();
    dir.set_mode(0o000).unwrap();
    assert_eq!(check_access(&env, &acc_file, AccessMask(AccessMask::R_OK | AccessMask::W_OK), 0, 0), Ok(()));
    assert_eq!(check_access(&env, &acc_file, AccessMask(AccessMask::X_OK), 0, 0), Err(VfsError::Acces));
    assert_eq!(check_access(&env, &dir, AccessMask(AccessMask::X_OK), 0, 0), Ok(()));
    // ROFS blocks W_OK even for uid == 0 unless exempt.
    let ro_mp = mkdir_at(&env, None, b"/ro_acc", 0o755, 0).unwrap();
    let ro_fs = MemFs::new(DevId::DEVFS, "devfs", true);
    let ro_reg = MemInode::new_char_dev(
        DevId::DEVFS,
        99,
        0o666,
        DevId::makedev(1, 3),
        false,
        Arc::clone(&ro_fs.registry),
        Arc::clone(&ro_fs.next_ino),
    );
    ro_fs.root.inner.lock().unwrap().children.insert(String::from("non_exempt"), ro_reg);
    let ro_exempt: Arc<dyn VfsInode> = ro_fs.add_char_device(&ro_fs.root, "exempt", 0o666, DevId::makedev(1, 3), true);
    env.mount(ro_mp, "/ro_acc", ro_fs).unwrap();
    let non_exempt = resolve_path(&env, None, b"/ro_acc/non_exempt", ResolveFlags::default()).unwrap().inode;
    assert_eq!(check_access(&env, &non_exempt, AccessMask(AccessMask::F_OK), 0, 0), Ok(()));
    assert_eq!(check_access(&env, &non_exempt, AccessMask(AccessMask::W_OK), 0, 0), Err(VfsError::RoFs));
    assert_eq!(check_access(&env, &ro_exempt, AccessMask(AccessMask::W_OK), 0, 0), Ok(()));
}

#[test]
fn open_creat_chained_trailing_slash_magic_depth_and_sym_dir_edges() {
    let (env, _) = setup_vfs();
    let sub = mkdir_at(&env, None, b"/sub", 0o755, 0).unwrap();
    let (existing_file, _) =
        open_inode(&env, None, b"/sub/existing_file", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();

    // 1. Chained trailing-slash symlinks under O_CREAT must preserve trailing `/`.
    symlink_at(&env, b"s2_missing/", None, b"/s1_missing").unwrap();
    symlink_at(&env, b"missing", None, b"/s2_missing").unwrap();
    assert_eq!(
        open_inode(&env, None, b"/s1_missing", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap_err(),
        VfsError::IsDir
    );
    assert_eq!(resolve_path(&env, None, b"/missing", ResolveFlags::default()).unwrap_err(), VfsError::NoEnt);

    symlink_at(&env, b"s2_file/", None, b"/s1_file").unwrap();
    symlink_at(&env, b"/sub/existing_file", None, b"/s2_file").unwrap();
    assert_eq!(
        open_inode(&env, None, b"/s1_file", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap_err(),
        VfsError::NotDir
    );
    assert_eq!(
        open_inode(&env, None, b"/s1_file", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0).unwrap_err(),
        VfsError::NotDir
    );

    symlink_at(&env, b"s2_dir/", None, b"/s1_dir").unwrap();
    symlink_at(&env, b"/sub", None, b"/s2_dir").unwrap();
    assert_eq!(
        open_inode(&env, None, b"/s1_dir", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0).unwrap_err(),
        VfsError::IsDir
    );

    // 2. Magic symlink to regular file with trailing `/` under O_CREAT -> ENOTDIR; to dir -> EISDIR.
    let proc_mp = mkdir_at(&env, None, b"/proc", 0o755, 0).unwrap();
    let proc_fs = MemFs::new(DevId::PROCFS, "procfs", false);
    proc_fs.add_magic_link(&proc_fs.root, "fd0", "/sub/existing_file", Arc::clone(&existing_file));
    proc_fs.add_magic_link(&proc_fs.root, "fd_dir", "/sub", Arc::clone(&sub));
    env.mount(proc_mp, "/proc", proc_fs).unwrap();

    symlink_at(&env, b"/proc/fd0/", None, b"/outer_magic_file").unwrap();
    assert_eq!(
        open_inode(&env, None, b"/outer_magic_file", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0)
            .unwrap_err(),
        VfsError::NotDir
    );
    assert_eq!(
        open_inode(&env, None, b"/outer_magic_file", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap_err(),
        VfsError::NotDir
    );

    symlink_at(&env, b"/proc/fd_dir/", None, b"/outer_magic_dir").unwrap();
    assert_eq!(
        open_inode(&env, None, b"/outer_magic_dir", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0)
            .unwrap_err(),
        VfsError::IsDir
    );

    // 3. Symlink prefix depth 21..=40 with `/.` and `/..` under O_CREAT -> EISDIR (and 41 -> ELOOP).
    mkdir_at(&env, None, b"/chain", 0o755, 0).unwrap();
    mkdir_at(&env, None, b"/chain/real", 0o755, 0).unwrap();
    symlink_at(&env, b"real", None, b"/chain/d1").unwrap();
    for i in 2..=41 {
        let tgt = format!("d{}", i - 1);
        let name = format!("/chain/d{i}");
        symlink_at(&env, tgt.as_bytes(), None, name.as_bytes()).unwrap();
    }
    for path in [b"/chain/d21/." as &[u8], b"/chain/d40/.", b"/chain/d21/..", b"/chain/d40/.."] {
        assert_eq!(
            open_inode(&env, None, path, OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0).unwrap_err(),
            VfsError::IsDir
        );
        assert_eq!(
            open_inode(
                &env,
                None,
                path,
                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_EXCL | OpenFlags::O_RDONLY),
                0o644,
                0
            )
            .unwrap_err(),
            VfsError::Exist
        );
    }
    for path in [b"/chain/d41/." as &[u8], b"/chain/d41/.."] {
        assert_eq!(
            open_inode(&env, None, path, OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0).unwrap_err(),
            VfsError::Loop
        );
    }
    assert_eq!(
        open_inode(&env, None, b"/", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0).unwrap_err(),
        VfsError::IsDir
    );
    assert_eq!(
        open_inode(&env, None, b"/", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_EXCL | OpenFlags::O_RDONLY), 0o644, 0)
            .unwrap_err(),
        VfsError::Exist
    );

    // 4. Intermediate symlinked directory + existing regular file with trailing `/` -> EISDIR (consistent with real dir),
    // while symlink target with trailing `/` to existing regular file still returns ENOTDIR.
    mkdir_at(&env, None, b"/real_dir", 0o755, 0).unwrap();
    open_inode(&env, None, b"/real_dir/f", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    symlink_at(&env, b"/real_dir", None, b"/sym_dir").unwrap();
    symlink_at(&env, b"/real_dir/f/", None, b"/sym_to_file_slash").unwrap();

    assert_eq!(
        open_inode(&env, None, b"/real_dir/f/", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0)
            .unwrap_err(),
        VfsError::IsDir
    );
    assert_eq!(
        open_inode(&env, None, b"/sym_dir/f/", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0)
            .unwrap_err(),
        VfsError::IsDir
    );
    assert_eq!(
        open_inode(&env, None, b"/sym_to_file_slash", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0)
            .unwrap_err(),
        VfsError::NotDir
    );
}
