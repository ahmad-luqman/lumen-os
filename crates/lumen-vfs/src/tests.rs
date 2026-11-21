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

    let open_guard = OpenGuard::new_unchecked(Arc::clone(&file_inode));
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
    let cwd_guard = OpenGuard::new_unchecked(Arc::clone(&cwd));
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

    // Leaves out `SimpleVfsTables`'s `VfsInode::cacheable` checks: no
    // filesystem mounted on these tables has a non-cacheable inode (#264's
    // generic `KernelVfsTables` could let these share the check).
    impl VfsCacheOps for ProbeGuardEnv {
        fn root_inode(&self) -> Result<Arc<dyn VfsInode>, VfsError> {
            let root_fs = self.mounts.lock().unwrap().root_fs().ok_or(VfsError::NoEnt)?;
            Ok(root_fs.root_inode())
        }
        fn is_global_root(&self, dev: DevId, ino: Ino) -> bool {
            self.mounts.lock().unwrap().is_global_root(dev, ino)
        }
        fn mount_at(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn FileSystem>> {
            self.mounts.lock().unwrap().mount_at(dev, ino)
        }
        fn mount_at_with_id(&self, dev: DevId, ino: Ino) -> Option<(Arc<dyn FileSystem>, u64)> {
            self.mounts.lock().unwrap().mount_at_with_id(dev, ino)
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
        fn cache_gens(&self) -> (u64, u64) {
            let dentries = self.dentries.lock().unwrap();
            let inodes = self.inodes.lock().unwrap();
            (dentries.invalidation_gen(), inodes.removal_gen())
        }
        fn dentry_lookup_inode(&self, pdev: DevId, pino: Ino, name: &str) -> (Option<Arc<dyn VfsInode>>, u64, u64) {
            let dentries = self.dentries.lock().unwrap();
            let inodes = self.inodes.lock().unwrap();
            let (target_opt, dentry_gen) = dentries.get_with_gen(pdev, pino, name);
            let inode_gen = inodes.removal_gen();
            let inode = target_opt.and_then(|t| inodes.get(t.dev, t.ino));
            (inode, dentry_gen, inode_gen)
        }
        fn publish_lookup_inode(
            &self,
            pdev: DevId,
            pino: Ino,
            name: &str,
            inode: Arc<dyn VfsInode>,
            expected_dentry_gen: u64,
            expected_inode_gen: u64,
        ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
            let mounts = self.mounts.lock().unwrap();
            let mut dentries = self.dentries.lock().unwrap();
            let mut inodes = self.inodes.lock().unwrap();
            if !mounts.is_dev_mounted(inode.dev_id()) {
                return Err(inode);
            }
            let unified = inodes.get_or_insert_if_fresh(inode, expected_inode_gen)?;
            if !mounts.is_mountpoint(pdev, pino) {
                let _ = dentries.insert_if_gen(
                    pdev,
                    pino,
                    name,
                    DentryTarget { dev: unified.dev_id(), ino: unified.ino() },
                    expected_dentry_gen,
                );
            }
            Ok(unified)
        }
        fn publish_created_inode(
            &self,
            pdev: DevId,
            pino: Ino,
            name: &str,
            inode: Arc<dyn VfsInode>,
            expected_dentry_gen: u64,
            expected_inode_gen: u64,
        ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
            let mounts = self.mounts.lock().unwrap();
            let mut dentries = self.dentries.lock().unwrap();
            let mut inodes = self.inodes.lock().unwrap();
            if !mounts.is_dev_mounted(inode.dev_id()) {
                return Err(inode);
            }
            let was_cached = inodes.get(inode.dev_id(), inode.ino()).is_some();
            let unified = inodes.get_or_insert_created_if_fresh(inode, expected_inode_gen)?;
            if !was_cached && !mounts.is_mountpoint(pdev, pino) {
                let _ = dentries.insert_if_gen(
                    pdev,
                    pino,
                    name,
                    DentryTarget { dev: unified.dev_id(), ino: unified.ino() },
                    expected_dentry_gen,
                );
            }
            Ok(unified)
        }
        fn dentry_gen(&self) -> u64 {
            self.dentries.lock().unwrap().invalidation_gen()
        }
        fn dentry_get(&self, pdev: DevId, pino: Ino, name: &str) -> Option<DentryTarget> {
            self.dentries.lock().unwrap().get(pdev, pino, name)
        }
        fn dentry_get_with_gen(&self, pdev: DevId, pino: Ino, name: &str) -> (Option<DentryTarget>, u64) {
            self.dentries.lock().unwrap().get_with_gen(pdev, pino, name)
        }
        fn dentry_insert(&self, pdev: DevId, pino: Ino, name: &str, target: DentryTarget) {
            self.dentries.lock().unwrap().insert(pdev, pino, name, target);
        }
        fn dentry_insert_if_gen(
            &self,
            pdev: DevId,
            pino: Ino,
            name: &str,
            target: DentryTarget,
            expected_gen: u64,
        ) -> bool {
            let mounts = self.mounts.lock().unwrap();
            let mut dentries = self.dentries.lock().unwrap();
            if mounts.is_mountpoint(pdev, pino) {
                return false;
            }
            dentries.insert_if_gen(pdev, pino, name, target, expected_gen)
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
        fn inode_get_if_fresh(&self, dev: DevId, ino: Ino, since_gen: u64) -> Option<Arc<dyn VfsInode>> {
            self.inodes.lock().unwrap().get_if_fresh(dev, ino, since_gen)
        }
        fn inode_get_or_insert(&self, inode: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
            self.inodes.lock().unwrap().get_or_insert(inode)
        }
        fn inode_get_or_insert_if_mounted(
            &self,
            mp_dev: DevId,
            mp_ino: Ino,
            mount_id: u64,
            inode: Arc<dyn VfsInode>,
        ) -> Option<Arc<dyn VfsInode>> {
            let mounts = self.mounts.lock().unwrap();
            let mut inodes = self.inodes.lock().unwrap();
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
            let mounts = self.mounts.lock().unwrap();
            let mut inodes = self.inodes.lock().unwrap();
            if !mounts.is_dev_mounted(inode.dev_id()) {
                return Err(inode);
            }
            inodes.get_or_insert_if_fresh(inode, since_gen)
        }
        fn inode_remove(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
            self.inodes.lock().unwrap().remove(dev, ino)
        }
        fn inode_remove_if_fresh(
            &self,
            dev: DevId,
            ino: Ino,
            since_gen: u64,
        ) -> Option<(Option<Arc<dyn VfsInode>>, u64)> {
            self.inodes.lock().unwrap().remove_if_fresh(dev, ino, since_gen)
        }
        fn inode_begin_removal(&self, dev: DevId, ino: Ino, since_gen: u64) -> (Option<Arc<dyn VfsInode>>, u64) {
            self.inodes.lock().unwrap().begin_removal_if_fresh(dev, ino, since_gen)
        }
        fn inode_end_removal(&self, dev: DevId, ino: Ino) -> u64 {
            self.inodes.lock().unwrap().end_removal(dev, ino)
        }
        fn inode_evict_detached(&self, dev: DevId, ino: Ino) -> bool {
            self.inodes.lock().unwrap().evict_detached(dev, ino)
        }
        fn inode_remove_if_same(
            &self,
            dev: DevId,
            ino: Ino,
            expected: &Arc<dyn VfsInode>,
        ) -> Option<Arc<dyn VfsInode>> {
            self.inodes.lock().unwrap().remove_if_same(dev, ino, expected)
        }
        fn inode_record_removal(&self, dev: DevId, ino: Ino) -> u64 {
            self.inodes.lock().unwrap().record_removal(dev, ino)
        }
        fn inode_record_removal_if_absent(&self, dev: DevId, ino: Ino) -> Result<u64, Arc<dyn VfsInode>> {
            self.inodes.lock().unwrap().record_removal_if_absent(dev, ino)
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

    let guard = OpenGuard::new_unchecked(via_dotdot);
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
        fn mount_at_with_id(&self, dev: DevId, ino: Ino) -> Option<(Arc<dyn FileSystem>, u64)> {
            self.inner.mount_at_with_id(dev, ino)
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
        fn cache_gens(&self) -> (u64, u64) {
            self.inner.cache_gens()
        }
        fn dentry_lookup_inode(&self, pdev: DevId, pino: Ino, name: &str) -> (Option<Arc<dyn VfsInode>>, u64, u64) {
            self.inner.dentry_lookup_inode(pdev, pino, name)
        }
        fn publish_lookup_inode(
            &self,
            pdev: DevId,
            pino: Ino,
            name: &str,
            inode: Arc<dyn VfsInode>,
            expected_dentry_gen: u64,
            expected_inode_gen: u64,
        ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
            self.inner.publish_lookup_inode(pdev, pino, name, inode, expected_dentry_gen, expected_inode_gen)
        }
        fn publish_created_inode(
            &self,
            pdev: DevId,
            pino: Ino,
            name: &str,
            inode: Arc<dyn VfsInode>,
            expected_dentry_gen: u64,
            expected_inode_gen: u64,
        ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
            self.inner.publish_created_inode(pdev, pino, name, inode, expected_dentry_gen, expected_inode_gen)
        }
        fn dentry_gen(&self) -> u64 {
            self.inner.dentry_gen()
        }
        fn dentry_get(&self, pdev: DevId, pino: Ino, name: &str) -> Option<DentryTarget> {
            self.inner.dentry_get(pdev, pino, name)
        }
        fn dentry_get_with_gen(&self, pdev: DevId, pino: Ino, name: &str) -> (Option<DentryTarget>, u64) {
            self.inner.dentry_get_with_gen(pdev, pino, name)
        }
        fn dentry_insert(&self, pdev: DevId, pino: Ino, name: &str, target: DentryTarget) {
            self.inner.dentry_insert(pdev, pino, name, target)
        }
        fn dentry_insert_if_gen(
            &self,
            pdev: DevId,
            pino: Ino,
            name: &str,
            target: DentryTarget,
            expected_gen: u64,
        ) -> bool {
            self.inner.dentry_insert_if_gen(pdev, pino, name, target, expected_gen)
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
        fn inode_get_if_fresh(&self, dev: DevId, ino: Ino, since_gen: u64) -> Option<Arc<dyn VfsInode>> {
            self.inner.inode_get_if_fresh(dev, ino, since_gen)
        }
        fn inode_get_or_insert(&self, inode: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
            self.inner.inode_get_or_insert(inode)
        }
        fn inode_get_or_insert_if_mounted(
            &self,
            mp_dev: DevId,
            mp_ino: Ino,
            mount_id: u64,
            inode: Arc<dyn VfsInode>,
        ) -> Option<Arc<dyn VfsInode>> {
            self.inner.inode_get_or_insert_if_mounted(mp_dev, mp_ino, mount_id, inode)
        }
        fn inode_get_or_insert_if_fresh(
            &self,
            inode: Arc<dyn VfsInode>,
            since_gen: u64,
        ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
            self.inner.inode_get_or_insert_if_fresh(inode, since_gen)
        }
        fn inode_remove(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
            self.inner.inode_remove(dev, ino)
        }
        fn inode_remove_if_fresh(
            &self,
            dev: DevId,
            ino: Ino,
            since_gen: u64,
        ) -> Option<(Option<Arc<dyn VfsInode>>, u64)> {
            self.inner.inode_remove_if_fresh(dev, ino, since_gen)
        }
        fn inode_begin_removal(&self, dev: DevId, ino: Ino, since_gen: u64) -> (Option<Arc<dyn VfsInode>>, u64) {
            self.inner.inode_begin_removal(dev, ino, since_gen)
        }
        fn inode_end_removal(&self, dev: DevId, ino: Ino) -> u64 {
            self.inner.inode_end_removal(dev, ino)
        }
        fn inode_evict_detached(&self, dev: DevId, ino: Ino) -> bool {
            self.inner.inode_evict_detached(dev, ino)
        }
        fn inode_remove_if_same(
            &self,
            dev: DevId,
            ino: Ino,
            expected: &Arc<dyn VfsInode>,
        ) -> Option<Arc<dyn VfsInode>> {
            self.inner.inode_remove_if_same(dev, ino, expected)
        }
        fn inode_record_removal(&self, dev: DevId, ino: Ino) -> u64 {
            self.inner.inode_record_removal(dev, ino)
        }
        fn inode_record_removal_if_absent(&self, dev: DevId, ino: Ino) -> Result<u64, Arc<dyn VfsInode>> {
            self.inner.inode_record_removal_if_absent(dev, ino)
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

    // `/f1` was unlinked with no open reference: an open file description
    // for it is refused (#262), and gives its reference back.
    assert_eq!(
        OpenFile::<StdLock<u64>>::new(Arc::clone(&file), Arc::from("/f1"), OpenFlags(OpenFlags::O_RDWR)).unwrap_err(),
        VfsError::NoEnt
    );
    assert_eq!(file.open_count(), 0);

    // `OpenFile::dec_fd_ref` AcqRel reference counting transitions cleanly to 0.
    let (live, _) =
        open_inode(&spy, None, b"/f2", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    let of = OpenFile::<StdLock<u64>>::new(live, Arc::from("/f2"), OpenFlags(OpenFlags::O_RDWR)).unwrap();
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
    let guard = OpenGuard::new_unchecked(Arc::clone(&live_file));
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

#[test]
fn lookup_component_does_not_resurrect_dentry_across_concurrent_rename_unlink_or_mount() {
    use alloc::boxed::Box;

    struct SyncTestEnv {
        mounts: Mutex<MountTable>,
        dentries: Mutex<DentryCache>,
        inodes: Mutex<InodeCache>,
    }

    impl SyncTestEnv {
        fn new(root: Arc<MemFs>) -> Arc<Self> {
            let env = Arc::new(Self {
                mounts: Mutex::new(MountTable::new()),
                dentries: Mutex::new(DentryCache::default()),
                inodes: Mutex::new(InodeCache::new()),
            });
            let root_inode = root.root_inode();
            let root_ino = root_inode.ino();
            env.mounts.lock().unwrap().mount_root_with_ino(root, root_ino).unwrap();
            let _ = env.inodes.lock().unwrap().get_or_insert(root_inode);
            env
        }

        fn mount(&self, mountpoint: Arc<dyn VfsInode>, path: &str, fs: Arc<dyn FileSystem>) -> Result<(), VfsError> {
            let mp_dev = mountpoint.dev_id();
            let mp_ino = mountpoint.ino();
            let root = fs.root_inode();
            let root_ino = root.ino();
            let mut mounts = self.mounts.lock().unwrap();
            let mut dentries = self.dentries.lock().unwrap();
            let mut inodes = self.inodes.lock().unwrap();
            mounts.mount_with_ino(mountpoint, path, fs, root_ino)?;
            dentries.invalidate_dir(mp_dev, mp_ino);
            let _ = inodes.get_or_insert(root);
            Ok(())
        }
    }

    // Leaves out `SimpleVfsTables`'s `VfsInode::cacheable` checks: no
    // filesystem mounted on these tables has a non-cacheable inode (#264's
    // generic `KernelVfsTables` could let these share the check).
    impl VfsCacheOps for SyncTestEnv {
        fn root_inode(&self) -> Result<Arc<dyn VfsInode>, VfsError> {
            let root_fs = self.mounts.lock().unwrap().root_fs().ok_or(VfsError::NoEnt)?;
            Ok(root_fs.root_inode())
        }
        fn is_global_root(&self, dev: DevId, ino: Ino) -> bool {
            self.mounts.lock().unwrap().is_global_root(dev, ino)
        }
        fn mount_at(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn FileSystem>> {
            self.mounts.lock().unwrap().mount_at(dev, ino)
        }
        fn mount_at_with_id(&self, dev: DevId, ino: Ino) -> Option<(Arc<dyn FileSystem>, u64)> {
            self.mounts.lock().unwrap().mount_at_with_id(dev, ino)
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
        fn cache_gens(&self) -> (u64, u64) {
            let dentries = self.dentries.lock().unwrap();
            let inodes = self.inodes.lock().unwrap();
            (dentries.invalidation_gen(), inodes.removal_gen())
        }
        fn dentry_lookup_inode(&self, pdev: DevId, pino: Ino, name: &str) -> (Option<Arc<dyn VfsInode>>, u64, u64) {
            let dentries = self.dentries.lock().unwrap();
            let inodes = self.inodes.lock().unwrap();
            let (target_opt, dentry_gen) = dentries.get_with_gen(pdev, pino, name);
            let inode_gen = inodes.removal_gen();
            let inode = target_opt.and_then(|t| inodes.get(t.dev, t.ino));
            (inode, dentry_gen, inode_gen)
        }
        fn publish_lookup_inode(
            &self,
            pdev: DevId,
            pino: Ino,
            name: &str,
            inode: Arc<dyn VfsInode>,
            expected_dentry_gen: u64,
            expected_inode_gen: u64,
        ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
            let mounts = self.mounts.lock().unwrap();
            let mut dentries = self.dentries.lock().unwrap();
            let mut inodes = self.inodes.lock().unwrap();
            if !mounts.is_dev_mounted(inode.dev_id()) {
                return Err(inode);
            }
            let unified = inodes.get_or_insert_if_fresh(inode, expected_inode_gen)?;
            if !mounts.is_mountpoint(pdev, pino) {
                let _ = dentries.insert_if_gen(
                    pdev,
                    pino,
                    name,
                    DentryTarget { dev: unified.dev_id(), ino: unified.ino() },
                    expected_dentry_gen,
                );
            }
            Ok(unified)
        }
        fn publish_created_inode(
            &self,
            pdev: DevId,
            pino: Ino,
            name: &str,
            inode: Arc<dyn VfsInode>,
            expected_dentry_gen: u64,
            expected_inode_gen: u64,
        ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
            let mounts = self.mounts.lock().unwrap();
            let mut dentries = self.dentries.lock().unwrap();
            let mut inodes = self.inodes.lock().unwrap();
            if !mounts.is_dev_mounted(inode.dev_id()) {
                return Err(inode);
            }
            let was_cached = inodes.get(inode.dev_id(), inode.ino()).is_some();
            let unified = inodes.get_or_insert_created_if_fresh(inode, expected_inode_gen)?;
            if !was_cached && !mounts.is_mountpoint(pdev, pino) {
                let _ = dentries.insert_if_gen(
                    pdev,
                    pino,
                    name,
                    DentryTarget { dev: unified.dev_id(), ino: unified.ino() },
                    expected_dentry_gen,
                );
            }
            Ok(unified)
        }
        fn dentry_gen(&self) -> u64 {
            self.dentries.lock().unwrap().invalidation_gen()
        }
        fn dentry_get(&self, pdev: DevId, pino: Ino, name: &str) -> Option<DentryTarget> {
            self.dentries.lock().unwrap().get(pdev, pino, name)
        }
        fn dentry_get_with_gen(&self, pdev: DevId, pino: Ino, name: &str) -> (Option<DentryTarget>, u64) {
            self.dentries.lock().unwrap().get_with_gen(pdev, pino, name)
        }
        fn dentry_insert(&self, pdev: DevId, pino: Ino, name: &str, target: DentryTarget) {
            self.dentries.lock().unwrap().insert(pdev, pino, name, target);
        }
        fn dentry_insert_if_gen(
            &self,
            pdev: DevId,
            pino: Ino,
            name: &str,
            target: DentryTarget,
            expected_gen: u64,
        ) -> bool {
            let mounts = self.mounts.lock().unwrap();
            let mut dentries = self.dentries.lock().unwrap();
            if mounts.is_mountpoint(pdev, pino) {
                return false;
            }
            dentries.insert_if_gen(pdev, pino, name, target, expected_gen)
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
        fn inode_get_if_fresh(&self, dev: DevId, ino: Ino, since_gen: u64) -> Option<Arc<dyn VfsInode>> {
            self.inodes.lock().unwrap().get_if_fresh(dev, ino, since_gen)
        }
        fn inode_get_or_insert(&self, inode: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
            self.inodes.lock().unwrap().get_or_insert(inode)
        }
        fn inode_get_or_insert_if_mounted(
            &self,
            mp_dev: DevId,
            mp_ino: Ino,
            mount_id: u64,
            inode: Arc<dyn VfsInode>,
        ) -> Option<Arc<dyn VfsInode>> {
            let mounts = self.mounts.lock().unwrap();
            let mut inodes = self.inodes.lock().unwrap();
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
            let mounts = self.mounts.lock().unwrap();
            let mut inodes = self.inodes.lock().unwrap();
            if !mounts.is_dev_mounted(inode.dev_id()) {
                return Err(inode);
            }
            inodes.get_or_insert_if_fresh(inode, since_gen)
        }
        fn inode_remove(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
            self.inodes.lock().unwrap().remove(dev, ino)
        }
        fn inode_remove_if_fresh(
            &self,
            dev: DevId,
            ino: Ino,
            since_gen: u64,
        ) -> Option<(Option<Arc<dyn VfsInode>>, u64)> {
            self.inodes.lock().unwrap().remove_if_fresh(dev, ino, since_gen)
        }
        fn inode_begin_removal(&self, dev: DevId, ino: Ino, since_gen: u64) -> (Option<Arc<dyn VfsInode>>, u64) {
            self.inodes.lock().unwrap().begin_removal_if_fresh(dev, ino, since_gen)
        }
        fn inode_end_removal(&self, dev: DevId, ino: Ino) -> u64 {
            self.inodes.lock().unwrap().end_removal(dev, ino)
        }
        fn inode_evict_detached(&self, dev: DevId, ino: Ino) -> bool {
            self.inodes.lock().unwrap().evict_detached(dev, ino)
        }
        fn inode_remove_if_same(
            &self,
            dev: DevId,
            ino: Ino,
            expected: &Arc<dyn VfsInode>,
        ) -> Option<Arc<dyn VfsInode>> {
            self.inodes.lock().unwrap().remove_if_same(dev, ino, expected)
        }
        fn inode_record_removal(&self, dev: DevId, ino: Ino) -> u64 {
            self.inodes.lock().unwrap().record_removal(dev, ino)
        }
        fn inode_record_removal_if_absent(&self, dev: DevId, ino: Ino) -> Result<u64, Arc<dyn VfsInode>> {
            self.inodes.lock().unwrap().record_removal_if_absent(dev, ino)
        }
    }

    type LookupHook = Box<dyn FnOnce() + Send>;

    struct HookDirInode {
        inner: Arc<dyn VfsInode>,
        post_lookup: Mutex<Option<LookupHook>>,
    }

    impl HookDirInode {
        fn wrap(inner: Arc<dyn VfsInode>, hook: LookupHook) -> Arc<dyn VfsInode> {
            Arc::new(Self { inner, post_lookup: Mutex::new(Some(hook)) })
        }
    }

    impl VfsInode for HookDirInode {
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
            let res = self.inner.lookup(name);
            if name != "."
                && name != ".."
                && let Some(hook) = self.post_lookup.lock().unwrap().take()
            {
                hook();
            }
            res
        }
        fn create(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.inner.create(name, mode)
        }
        fn mkdir(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.inner.mkdir(name, mode)
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
            self.inner.symlink(name, target)
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

    let root_fs = MemFs::new(DevId::ROOT, "ext4", false);
    let env = SyncTestEnv::new(root_fs);

    // Subcase 1: Concurrent `rename_at2` immediately before `lookup_component` cache publication.
    let ren_dir = mkdir_at(env.as_ref(), None, b"/ren_dir", 0o755, 0).unwrap();
    let (target_file, _) =
        open_inode(env.as_ref(), None, b"/ren_dir/target", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    target_file.write_at(0, b"orig").unwrap();
    // Evict `"target"` from DentryCache so `resolve_path` hits the cache-miss `lookup_component` path.
    env.dentry_invalidate(ren_dir.dev_id(), ren_dir.ino(), "target");

    let env_for_ren = Arc::clone(&env);
    let hook_ren_dir = HookDirInode::wrap(
        Arc::clone(&ren_dir),
        Box::new(move || {
            rename_at2(env_for_ren.as_ref(), None, b"/ren_dir/target", None, b"/ren_dir/moved", RenameFlags(0))
                .unwrap();
        }),
    );
    let pre_ren = resolve_path(env.as_ref(), Some(&hook_ren_dir), b"target", ResolveFlags::default()).unwrap();
    assert_eq!(pre_ren.inode.ino(), target_file.ino());
    assert_eq!(env.dentry_get(ren_dir.dev_id(), ren_dir.ino(), "target"), None);
    assert_eq!(
        resolve_path(env.as_ref(), None, b"/ren_dir/target", ResolveFlags::default()).unwrap_err(),
        VfsError::NoEnt
    );
    let moved = resolve_path(env.as_ref(), None, b"/ren_dir/moved", ResolveFlags::default()).unwrap();
    assert_eq!(moved.inode.ino(), target_file.ino());

    // Subcase 2: Concurrent `unlink_at` immediately before `lookup_component` cache publication.
    let un_dir = mkdir_at(env.as_ref(), None, b"/un_dir", 0o755, 0).unwrap();
    let (ufile, _) =
        open_inode(env.as_ref(), None, b"/un_dir/ufile", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let ufile_dev = ufile.dev_id();
    let ufile_ino = ufile.ino();
    drop(ufile);
    env.dentry_invalidate(un_dir.dev_id(), un_dir.ino(), "ufile");

    let env_for_un = Arc::clone(&env);
    let hook_un_dir = HookDirInode::wrap(
        Arc::clone(&un_dir),
        Box::new(move || {
            unlink_at(env_for_un.as_ref(), None, b"/un_dir/ufile", false).unwrap();
        }),
    );
    let pre_un = resolve_path(env.as_ref(), Some(&hook_un_dir), b"ufile", ResolveFlags::default()).unwrap();
    assert_eq!(pre_un.inode.ino(), ufile_ino);
    assert_eq!(env.dentry_get(un_dir.dev_id(), un_dir.ino(), "ufile"), None);
    assert!(env.inode_get(ufile_dev, ufile_ino).is_none());
    assert_eq!(
        resolve_path(env.as_ref(), None, b"/un_dir/ufile", ResolveFlags::default()).unwrap_err(),
        VfsError::NoEnt
    );

    // Subcase 3: Concurrent `mount` over a parent directory during cache-miss lookup under it.
    let mp_dir = mkdir_at(env.as_ref(), None, b"/mp", 0o755, 0).unwrap();
    open_inode(env.as_ref(), None, b"/mp/under_mp", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
        .unwrap();
    env.dentry_invalidate(mp_dir.dev_id(), mp_dir.ino(), "under_mp");

    let child_fs = MemFs::new(DevId::TMPFS, "tmpfs", false);
    let inside = child_fs.root.create("inside_mount", 0o644).unwrap();
    inside.write_at(0, b"mounted").unwrap();

    let env_for_mnt = Arc::clone(&env);
    let mp_raw = Arc::clone(&mp_dir);
    let child_fs_dyn: Arc<dyn FileSystem> = child_fs;
    let hook_mp_dir = HookDirInode::wrap(
        Arc::clone(&mp_dir),
        Box::new(move || {
            env_for_mnt.mount(mp_raw, "/mp", child_fs_dyn).unwrap();
        }),
    );
    let pre_mnt = resolve_path(env.as_ref(), Some(&hook_mp_dir), b"under_mp", ResolveFlags::default()).unwrap();
    assert_eq!(pre_mnt.inode.dev_id(), DevId::ROOT);
    assert_eq!(env.dentry_get(mp_dir.dev_id(), mp_dir.ino(), "under_mp"), None);
    assert_eq!(
        resolve_path(env.as_ref(), None, b"/mp/under_mp", ResolveFlags::default()).unwrap_err(),
        VfsError::NoEnt
    );
    let mounted_res = resolve_path(env.as_ref(), None, b"/mp/inside_mount", ResolveFlags::default()).unwrap();
    assert_eq!(mounted_res.inode.dev_id(), DevId::TMPFS);
}

#[test]
fn open_creat_retries_on_concurrent_exist_race_across_winner_types() {
    use alloc::boxed::Box;

    type ActionHook = Box<dyn FnMut(&Arc<dyn VfsInode>) + Send>;

    struct RaceCreateDirInode {
        inner: Arc<dyn VfsInode>,
        pre_create: Mutex<Option<ActionHook>>,
        pre_lookup: Mutex<Option<ActionHook>>,
    }

    impl RaceCreateDirInode {
        fn new(inner: Arc<dyn VfsInode>) -> Arc<Self> {
            Arc::new(Self { inner, pre_create: Mutex::new(None), pre_lookup: Mutex::new(None) })
        }

        fn arm_pre_create(&self, hook: ActionHook) {
            *self.pre_create.lock().unwrap() = Some(hook);
        }

        fn arm_pre_lookup(&self, hook: ActionHook) {
            *self.pre_lookup.lock().unwrap() = Some(hook);
        }
    }

    impl VfsInode for RaceCreateDirInode {
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
            if name != "."
                && name != ".."
                && let Some(mut hook) = self.pre_lookup.lock().unwrap().take()
            {
                hook(&self.inner);
            }
            self.inner.lookup(name)
        }
        fn create(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
            if let Some(mut hook) = self.pre_create.lock().unwrap().take() {
                hook(&self.inner);
            }
            self.inner.create(name, mode)
        }
        fn mkdir(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.inner.mkdir(name, mode)
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
            self.inner.symlink(name, target)
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

    let (env, _) = setup_vfs();
    let base = mkdir_at(&env, None, b"/race", 0o755, 0).unwrap();
    let race_dir = RaceCreateDirInode::new(Arc::clone(&base));
    let race_dyn: Arc<dyn VfsInode> = race_dir.clone();

    // (a) Winner creates a regular file -> non-exclusive O_CREAT retries and opens it (preserving data without O_TRUNC).
    race_dir.arm_pre_create(Box::new(|dir| {
        let winner = dir.create("reg_keep", 0o644).unwrap();
        winner.write_at(0, b"winner").unwrap();
    }));
    let (opened_keep, _) =
        open_inode(&env, Some(&race_dyn), b"reg_keep", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let mut buf = [0u8; 16];
    let n = opened_keep.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"winner");

    // (a1) Winner creates a regular file and caller specified O_TRUNC -> truncated to 0.
    race_dir.arm_pre_create(Box::new(|dir| {
        let winner = dir.create("reg_trunc", 0o644).unwrap();
        winner.write_at(0, b"winner").unwrap();
    }));
    let (opened_trunc, _) = open_inode(
        &env,
        Some(&race_dyn),
        b"reg_trunc",
        OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .unwrap();
    assert_eq!(opened_trunc.stat().unwrap().size, 0);

    // (a2) Same creation race with O_CREAT | O_EXCL -> still returns Exist.
    race_dir.arm_pre_create(Box::new(|dir| {
        let _ = dir.create("reg_excl", 0o644).unwrap();
    }));
    assert_eq!(
        open_inode(
            &env,
            Some(&race_dyn),
            b"reg_excl",
            OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_EXCL | OpenFlags::O_RDWR),
            0o644,
            0
        )
        .unwrap_err(),
        VfsError::Exist
    );

    // (b) Winner creates a directory -> returns IsDir even with O_RDONLY.
    race_dir.arm_pre_create(Box::new(|dir| {
        let _ = dir.mkdir("dir_winner", 0o755).unwrap();
    }));
    assert_eq!(
        open_inode(&env, Some(&race_dyn), b"dir_winner", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0)
            .unwrap_err(),
        VfsError::IsDir
    );

    // (c1) Winner creates a symlink and caller specified O_NOFOLLOW -> returns Loop.
    race_dir.arm_pre_create(Box::new(|dir| {
        let _ = dir.symlink("sym_nf", "tgt").unwrap();
    }));
    assert_eq!(
        open_inode(
            &env,
            Some(&race_dyn),
            b"sym_nf",
            OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR | OpenFlags::O_NOFOLLOW),
            0o644,
            0
        )
        .unwrap_err(),
        VfsError::Loop
    );

    // (c2) Winner creates a symlink to a regular file -> follows symlink and applies O_TRUNC.
    race_dir.arm_pre_create(Box::new(|dir| {
        let real = dir.create("real_tgt", 0o644).unwrap();
        real.write_at(0, b"old_data").unwrap();
        let _ = dir.symlink("sym_follow", "real_tgt").unwrap();
    }));
    let (followed, _) = open_inode(
        &env,
        Some(&race_dyn),
        b"sym_follow",
        OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR | OpenFlags::O_TRUNC),
        0o644,
        0,
    )
    .unwrap();
    assert_eq!(followed.stat().unwrap().size, 0);

    // (c3) Creation race under a 40-deep symlink prefix does not double-count `symlink_depth` (no spurious ELOOP).
    mkdir_at(&env, None, b"/chain", 0o755, 0).unwrap();
    let chain_real = mkdir_at(&env, None, b"/chain/real", 0o755, 0).unwrap();
    let deep_race = RaceCreateDirInode::new(Arc::clone(&chain_real));
    let _ = env.inode_remove(chain_real.dev_id(), chain_real.ino());
    let deep_dyn: Arc<dyn VfsInode> = deep_race.clone();
    let _ = env.inode_get_or_insert(deep_dyn);
    symlink_at(&env, b"real", None, b"/chain/d1").unwrap();
    for i in 2..=40 {
        let tgt = format!("d{}", i - 1);
        let name = format!("/chain/d{i}");
        symlink_at(&env, tgt.as_bytes(), None, name.as_bytes()).unwrap();
    }
    deep_race.arm_pre_create(Box::new(|dir| {
        let winner = dir.create("raced_deep", 0o644).unwrap();
        winner.write_at(0, b"deep_ok").unwrap();
    }));
    let (deep_opened, _) =
        open_inode(&env, None, b"/chain/d40/raced_deep", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let n_deep = deep_opened.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf[..n_deep], b"deep_ok");

    // (d) Winner creates `"ephem"` then unlinks it before retry `lookup` -> retries `create` and succeeds.
    let race_dir_clone = Arc::clone(&race_dir);
    race_dir.arm_pre_create(Box::new(move |dir| {
        let _ = dir.create("ephem", 0o644).unwrap();
        race_dir_clone.arm_pre_lookup(Box::new(|d| {
            d.unlink("ephem").unwrap();
        }));
    }));
    let (ephem, _) =
        open_inode(&env, Some(&race_dyn), b"ephem", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    ephem.write_at(0, b"created_after_unlink").unwrap();

    // (e) Bounded retry limit prevents livelock if create-then-unlink repeats beyond MAX_CREAT_RACE_RETRIES (64).
    fn arm_endless_churn(race_dir: &Arc<RaceCreateDirInode>) {
        let dir_for_lookup = Arc::clone(race_dir);
        race_dir.arm_pre_create(Box::new(move |dir| {
            let _ = dir.create("churn", 0o644).unwrap();
            let dir_for_next = Arc::clone(&dir_for_lookup);
            dir_for_lookup.arm_pre_lookup(Box::new(move |d| {
                d.unlink("churn").unwrap();
                arm_endless_churn(&dir_for_next);
            }));
        }));
    }
    arm_endless_churn(&race_dir);
    assert_eq!(
        open_inode(&env, Some(&race_dyn), b"churn", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap_err(),
        VfsError::Exist
    );
}

// ============================================================================
// Round 2 InodeCache Coherency Infrastructure & Regression/Stress Tests
// ============================================================================

std::thread_local! {
    static TEST_LOCK9_DEPTH: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

struct Lock9DepthGuard;

impl Lock9DepthGuard {
    fn enter() -> Self {
        TEST_LOCK9_DEPTH.with(|d| d.set(d.get() + 1));
        Self
    }
}

impl Drop for Lock9DepthGuard {
    fn drop(&mut self) {
        TEST_LOCK9_DEPTH.with(|d| d.set(d.get() - 1));
    }
}

fn assert_test_lock9_free() {
    let depth = TEST_LOCK9_DEPTH.with(|d| d.get());
    assert_eq!(depth, 0, "Lock 9 held across VfsInode/FileSystem call (depth={depth})");
}

struct SyncVfsTables {
    mounts: Mutex<MountTable>,
    dentries: Mutex<DentryCache>,
    inodes: Mutex<InodeCache>,
}

impl SyncVfsTables {
    fn new(root: Arc<dyn FileSystem>) -> Arc<Self> {
        Self::with_removal_history(root, InodeCache::DEFAULT_REMOVAL_HISTORY, DentryCache::DEFAULT_CAPACITY)
    }

    fn with_removal_history(root: Arc<dyn FileSystem>, max_history: usize, dentry_cap: usize) -> Arc<Self> {
        let env = Arc::new(Self {
            mounts: Mutex::new(MountTable::new()),
            dentries: Mutex::new(DentryCache::new(dentry_cap)),
            inodes: Mutex::new(InodeCache::with_removal_history(max_history)),
        });
        let root_inode = root.root_inode();
        let root_ino = root_inode.ino();
        {
            let _g = Lock9DepthGuard::enter();
            env.mounts.lock().unwrap().mount_root_with_ino(root, root_ino).unwrap();
            let _ = env.inodes.lock().unwrap().get_or_insert(root_inode);
        }
        env
    }

    fn mount(&self, mountpoint: Arc<dyn VfsInode>, path: &str, fs: Arc<dyn FileSystem>) -> Result<(), VfsError> {
        let mp_dev = mountpoint.dev_id();
        let mp_ino = mountpoint.ino();
        let root = fs.root_inode();
        let root_ino = root.ino();
        let _g = Lock9DepthGuard::enter();
        let mut mounts = self.mounts.lock().unwrap();
        let mut dentries = self.dentries.lock().unwrap();
        let mut inodes = self.inodes.lock().unwrap();
        mounts.mount_with_ino(mountpoint, path, fs, root_ino)?;
        dentries.invalidate_dir(mp_dev, mp_ino);
        let _ = inodes.get_or_insert(root);
        Ok(())
    }

    fn umount(&self, mountpoint_dev: DevId, mountpoint_ino: Ino) -> Result<Arc<dyn FileSystem>, VfsError> {
        let _g = Lock9DepthGuard::enter();
        let mut mounts = self.mounts.lock().unwrap();
        let mut dentries = self.dentries.lock().unwrap();
        let mut inodes = self.inodes.lock().unwrap();
        let fs = mounts.umount(mountpoint_dev, mountpoint_ino)?;
        let dev = fs.dev_id();
        dentries.invalidate_fs(dev);
        dentries.invalidate_dir(mountpoint_dev, mountpoint_ino);
        let _ = inodes.evict_fs(dev);
        Ok(fs)
    }

    fn snapshot_inodes(&self) -> Vec<Arc<dyn VfsInode>> {
        let _g = Lock9DepthGuard::enter();
        self.inodes.lock().unwrap().snapshot_all()
    }

    fn snapshot_dentries(&self) -> Vec<(DevId, Ino, String, DentryTarget)> {
        let _g = Lock9DepthGuard::enter();
        self.dentries.lock().unwrap().snapshot_all()
    }

    fn is_dev_mounted(&self, dev: DevId) -> bool {
        let _g = Lock9DepthGuard::enter();
        self.mounts.lock().unwrap().is_dev_mounted(dev)
    }
}

// Leaves out `SimpleVfsTables`'s `VfsInode::cacheable` checks: no
// filesystem mounted on these tables has a non-cacheable inode (#264's
// generic `KernelVfsTables` could let these share the check).
impl VfsCacheOps for SyncVfsTables {
    fn root_inode(&self) -> Result<Arc<dyn VfsInode>, VfsError> {
        let root_fs = {
            let _g = Lock9DepthGuard::enter();
            self.mounts.lock().unwrap().root_fs().ok_or(VfsError::NoEnt)?
        };
        Ok(root_fs.root_inode())
    }
    fn is_global_root(&self, dev: DevId, ino: Ino) -> bool {
        let _g = Lock9DepthGuard::enter();
        self.mounts.lock().unwrap().is_global_root(dev, ino)
    }
    fn mount_at(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn FileSystem>> {
        let _g = Lock9DepthGuard::enter();
        self.mounts.lock().unwrap().mount_at(dev, ino)
    }
    fn mount_at_with_id(&self, dev: DevId, ino: Ino) -> Option<(Arc<dyn FileSystem>, u64)> {
        let _g = Lock9DepthGuard::enter();
        self.mounts.lock().unwrap().mount_at_with_id(dev, ino)
    }
    fn parent_mount(&self, mounted_dev: DevId) -> Option<Arc<dyn VfsInode>> {
        let _g = Lock9DepthGuard::enter();
        self.mounts.lock().unwrap().parent_mount(mounted_dev)
    }
    fn parent_mount_if_root(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        let _g = Lock9DepthGuard::enter();
        self.mounts.lock().unwrap().parent_mount_if_root(dev, ino)
    }
    fn is_mountpoint(&self, dev: DevId, ino: Ino) -> bool {
        let _g = Lock9DepthGuard::enter();
        self.mounts.lock().unwrap().is_mountpoint(dev, ino)
    }
    fn fs_read_only(&self, dev: DevId) -> bool {
        let _g = Lock9DepthGuard::enter();
        self.mounts.lock().unwrap().fs_by_dev(dev).is_some_and(|fs| fs.read_only())
    }
    fn fs_by_dev(&self, dev: DevId) -> Option<Arc<dyn FileSystem>> {
        let _g = Lock9DepthGuard::enter();
        self.mounts.lock().unwrap().fs_by_dev(dev)
    }
    fn cache_gens(&self) -> (u64, u64) {
        let _g = Lock9DepthGuard::enter();
        let dentries = self.dentries.lock().unwrap();
        let inodes = self.inodes.lock().unwrap();
        (dentries.invalidation_gen(), inodes.removal_gen())
    }
    fn dentry_lookup_inode(&self, pdev: DevId, pino: Ino, name: &str) -> (Option<Arc<dyn VfsInode>>, u64, u64) {
        let _g = Lock9DepthGuard::enter();
        let dentries = self.dentries.lock().unwrap();
        let inodes = self.inodes.lock().unwrap();
        let (target_opt, dentry_gen) = dentries.get_with_gen(pdev, pino, name);
        let inode_gen = inodes.removal_gen();
        let inode = target_opt.and_then(|t| inodes.get(t.dev, t.ino));
        (inode, dentry_gen, inode_gen)
    }
    fn publish_lookup_inode(
        &self,
        pdev: DevId,
        pino: Ino,
        name: &str,
        inode: Arc<dyn VfsInode>,
        expected_dentry_gen: u64,
        expected_inode_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
        let _g = Lock9DepthGuard::enter();
        let mounts = self.mounts.lock().unwrap();
        let mut dentries = self.dentries.lock().unwrap();
        let mut inodes = self.inodes.lock().unwrap();
        if !mounts.is_dev_mounted(inode.dev_id()) {
            return Err(inode);
        }
        let unified = inodes.get_or_insert_if_fresh(inode, expected_inode_gen)?;
        if !mounts.is_mountpoint(pdev, pino) {
            let _ = dentries.insert_if_gen(
                pdev,
                pino,
                name,
                DentryTarget { dev: unified.dev_id(), ino: unified.ino() },
                expected_dentry_gen,
            );
        }
        Ok(unified)
    }
    fn publish_created_inode(
        &self,
        pdev: DevId,
        pino: Ino,
        name: &str,
        inode: Arc<dyn VfsInode>,
        expected_dentry_gen: u64,
        expected_inode_gen: u64,
    ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
        let _g = Lock9DepthGuard::enter();
        let mounts = self.mounts.lock().unwrap();
        let mut dentries = self.dentries.lock().unwrap();
        let mut inodes = self.inodes.lock().unwrap();
        if !mounts.is_dev_mounted(inode.dev_id()) {
            return Err(inode);
        }
        let was_cached = inodes.get(inode.dev_id(), inode.ino()).is_some();
        let unified = inodes.get_or_insert_created_if_fresh(inode, expected_inode_gen)?;
        if !was_cached && !mounts.is_mountpoint(pdev, pino) {
            let _ = dentries.insert_if_gen(
                pdev,
                pino,
                name,
                DentryTarget { dev: unified.dev_id(), ino: unified.ino() },
                expected_dentry_gen,
            );
        }
        Ok(unified)
    }
    fn dentry_gen(&self) -> u64 {
        let _g = Lock9DepthGuard::enter();
        self.dentries.lock().unwrap().invalidation_gen()
    }
    fn dentry_get(&self, pdev: DevId, pino: Ino, name: &str) -> Option<DentryTarget> {
        let _g = Lock9DepthGuard::enter();
        self.dentries.lock().unwrap().get(pdev, pino, name)
    }
    fn dentry_get_with_gen(&self, pdev: DevId, pino: Ino, name: &str) -> (Option<DentryTarget>, u64) {
        let _g = Lock9DepthGuard::enter();
        self.dentries.lock().unwrap().get_with_gen(pdev, pino, name)
    }
    fn dentry_insert(&self, pdev: DevId, pino: Ino, name: &str, target: DentryTarget) {
        let _g = Lock9DepthGuard::enter();
        self.dentries.lock().unwrap().insert(pdev, pino, name, target);
    }
    fn dentry_insert_if_gen(
        &self,
        pdev: DevId,
        pino: Ino,
        name: &str,
        target: DentryTarget,
        expected_gen: u64,
    ) -> bool {
        let _g = Lock9DepthGuard::enter();
        let mounts = self.mounts.lock().unwrap();
        let mut dentries = self.dentries.lock().unwrap();
        if mounts.is_mountpoint(pdev, pino) {
            return false;
        }
        dentries.insert_if_gen(pdev, pino, name, target, expected_gen)
    }
    fn dentry_invalidate(&self, pdev: DevId, pino: Ino, name: &str) {
        let _g = Lock9DepthGuard::enter();
        self.dentries.lock().unwrap().invalidate(pdev, pino, name);
    }
    fn dentry_invalidate_dir(&self, pdev: DevId, pino: Ino) {
        let _g = Lock9DepthGuard::enter();
        self.dentries.lock().unwrap().invalidate_dir(pdev, pino);
    }
    fn inode_get(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        let _g = Lock9DepthGuard::enter();
        self.inodes.lock().unwrap().get(dev, ino)
    }
    fn inode_get_if_fresh(&self, dev: DevId, ino: Ino, since_gen: u64) -> Option<Arc<dyn VfsInode>> {
        let _g = Lock9DepthGuard::enter();
        self.inodes.lock().unwrap().get_if_fresh(dev, ino, since_gen)
    }
    fn inode_get_or_insert(&self, inode: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
        let _g = Lock9DepthGuard::enter();
        self.inodes.lock().unwrap().get_or_insert(inode)
    }
    fn inode_get_or_insert_if_mounted(
        &self,
        mp_dev: DevId,
        mp_ino: Ino,
        mount_id: u64,
        inode: Arc<dyn VfsInode>,
    ) -> Option<Arc<dyn VfsInode>> {
        let _g = Lock9DepthGuard::enter();
        let mounts = self.mounts.lock().unwrap();
        let mut inodes = self.inodes.lock().unwrap();
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
        let _g = Lock9DepthGuard::enter();
        let mounts = self.mounts.lock().unwrap();
        let mut inodes = self.inodes.lock().unwrap();
        if !mounts.is_dev_mounted(inode.dev_id()) {
            return Err(inode);
        }
        inodes.get_or_insert_if_fresh(inode, since_gen)
    }
    fn inode_remove(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
        let _g = Lock9DepthGuard::enter();
        self.inodes.lock().unwrap().remove(dev, ino)
    }
    fn inode_remove_if_fresh(&self, dev: DevId, ino: Ino, since_gen: u64) -> Option<(Option<Arc<dyn VfsInode>>, u64)> {
        let _g = Lock9DepthGuard::enter();
        self.inodes.lock().unwrap().remove_if_fresh(dev, ino, since_gen)
    }
    fn inode_begin_removal(&self, dev: DevId, ino: Ino, since_gen: u64) -> (Option<Arc<dyn VfsInode>>, u64) {
        let _g = Lock9DepthGuard::enter();
        self.inodes.lock().unwrap().begin_removal_if_fresh(dev, ino, since_gen)
    }
    fn inode_end_removal(&self, dev: DevId, ino: Ino) -> u64 {
        let _g = Lock9DepthGuard::enter();
        self.inodes.lock().unwrap().end_removal(dev, ino)
    }
    fn inode_evict_detached(&self, dev: DevId, ino: Ino) -> bool {
        let _g = Lock9DepthGuard::enter();
        self.inodes.lock().unwrap().evict_detached(dev, ino)
    }
    fn inode_remove_if_same(&self, dev: DevId, ino: Ino, expected: &Arc<dyn VfsInode>) -> Option<Arc<dyn VfsInode>> {
        let _g = Lock9DepthGuard::enter();
        self.inodes.lock().unwrap().remove_if_same(dev, ino, expected)
    }
    fn inode_record_removal(&self, dev: DevId, ino: Ino) -> u64 {
        let _g = Lock9DepthGuard::enter();
        self.inodes.lock().unwrap().record_removal(dev, ino)
    }
    fn inode_record_removal_if_absent(&self, dev: DevId, ino: Ino) -> Result<u64, Arc<dyn VfsInode>> {
        let _g = Lock9DepthGuard::enter();
        self.inodes.lock().unwrap().record_removal_if_absent(dev, ino)
    }
}

struct ReusingSharedMeta {
    body_id: u64,
    orig_ino: Ino,
    inode_type: InodeType,
    open_count: AtomicU32,
    detached: AtomicBool,
    current_ino: AtomicU64,
    locks: Mutex<AdvisoryLocks>,
    retired_body: Mutex<Option<ReusingNodeBody>>,
}

struct ReusingNodeBody {
    meta: Arc<ReusingSharedMeta>,
    mode: u16,
    nlink: u32,
    data: Vec<u8>,
    symlink_target: String,
    children: BTreeMap<String, Ino>,
    parent: Ino,
}

struct ReusingFsState {
    next_ino: u64,
    next_body_id: u64,
    free_inos: Vec<Ino>,
    inodes: BTreeMap<Ino, ReusingNodeBody>,
    orphans: BTreeMap<u64, ReusingNodeBody>,
}

type FsOpHook = alloc::boxed::Box<dyn FnOnce() + Send>;

#[derive(Default)]
struct ReusingHooks {
    pre_lookup: BTreeMap<String, FsOpHook>,
    post_lookup: BTreeMap<String, FsOpHook>,
    pre_create: BTreeMap<String, FsOpHook>,
    post_create: BTreeMap<String, FsOpHook>,
    pre_mkdir: BTreeMap<String, FsOpHook>,
    post_mkdir: BTreeMap<String, FsOpHook>,
    pre_symlink: BTreeMap<String, FsOpHook>,
    post_symlink: BTreeMap<String, FsOpHook>,
    pre_unlink: BTreeMap<String, FsOpHook>,
    post_unlink: BTreeMap<String, FsOpHook>,
    pre_rmdir: BTreeMap<String, FsOpHook>,
    post_rename: BTreeMap<String, FsOpHook>,
    rmdir_override_err: BTreeMap<String, VfsError>,
}

struct ReusingFsInner {
    dev: DevId,
    name: &'static str,
    read_only: bool,
    use_rename_lock: bool,
    rename_lock: Mutex<()>,
    state: Mutex<ReusingFsState>,
    hooks: Mutex<ReusingHooks>,
}

/// Backend constructing a fresh `Arc<dyn VfsInode>` per `lookup`/`create`/
/// `mkdir`/`symlink`/`root_inode` and aggressively reusing freed `Ino`s (LIFO).
struct FreshArcReusingFs {
    inner: Arc<ReusingFsInner>,
}

impl FreshArcReusingFs {
    fn new(dev: DevId, name: &'static str, read_only: bool, use_rename_lock: bool) -> Arc<Self> {
        let mut inodes = BTreeMap::new();
        let root_meta = Arc::new(ReusingSharedMeta {
            body_id: 1,
            orig_ino: 1,
            inode_type: InodeType::Dir,
            open_count: AtomicU32::new(0),
            detached: AtomicBool::new(false),
            current_ino: AtomicU64::new(1),
            locks: Mutex::new(AdvisoryLocks::new()),
            retired_body: Mutex::new(None),
        });
        inodes.insert(
            1,
            ReusingNodeBody {
                meta: root_meta,
                mode: 0o755,
                nlink: 2,
                data: Vec::new(),
                symlink_target: String::new(),
                children: BTreeMap::new(),
                parent: 1,
            },
        );
        Arc::new(Self {
            inner: Arc::new(ReusingFsInner {
                dev,
                name,
                read_only,
                use_rename_lock,
                rename_lock: Mutex::new(()),
                state: Mutex::new(ReusingFsState {
                    next_ino: 2,
                    next_body_id: 2,
                    free_inos: Vec::new(),
                    inodes,
                    orphans: BTreeMap::new(),
                }),
                hooks: Mutex::new(ReusingHooks::default()),
            }),
        })
    }

    fn alloc_ino_and_body_id(state: &mut ReusingFsState) -> (Ino, u64) {
        let ino = state.free_inos.pop().unwrap_or_else(|| {
            let v = state.next_ino;
            state.next_ino += 1;
            v
        });
        let body_id = state.next_body_id;
        state.next_body_id += 1;
        (ino, body_id)
    }

    fn make_handle(inner: &Arc<ReusingFsInner>, meta: Arc<ReusingSharedMeta>) -> Arc<ReusingHandle> {
        Arc::new(ReusingHandle { lock_id: next_lock_id(), inner: Arc::clone(inner), meta })
    }

    fn retire_if_unlinked(state: &mut ReusingFsState, orig_ino: Ino) {
        let Some(body) = state.inodes.get(&orig_ino) else {
            return;
        };
        if body.nlink > 0 {
            return;
        }
        let body = state.inodes.remove(&orig_ino).unwrap();
        let open_cnt = body.meta.open_count.load(Ordering::Acquire);
        if open_cnt > 0 {
            body.meta.detached.store(true, Ordering::Release);
            body.meta.current_ino.store(next_detached_ino(), Ordering::Release);
            let bid = body.meta.body_id;
            state.orphans.insert(bid, body);
        } else {
            let meta = Arc::clone(&body.meta);
            *meta.retired_body.lock().unwrap() = Some(body);
            state.free_inos.push(orig_ino);
        }
    }
}

impl FileSystem for FreshArcReusingFs {
    fn dev_id(&self) -> DevId {
        self.inner.dev
    }
    fn name(&self) -> &'static str {
        self.inner.name
    }
    fn root_inode(&self) -> Arc<dyn VfsInode> {
        let state = self.inner.state.lock().unwrap();
        let root_meta = Arc::clone(&state.inodes.get(&1).expect("root inode exists").meta);
        Self::make_handle(&self.inner, root_meta)
    }
    fn sync(&self) -> Result<(), VfsError> {
        assert_test_lock9_free();
        Ok(())
    }
    fn read_only(&self) -> bool {
        self.inner.read_only
    }
    fn with_rename_lock(&self, f: &mut dyn FnMut() -> Result<(), VfsError>) -> Result<(), VfsError> {
        assert_test_lock9_free();
        if self.inner.use_rename_lock {
            let _g = self.inner.rename_lock.lock().unwrap();
            f()
        } else {
            f()
        }
    }
}

struct ReusingHandle {
    lock_id: u64,
    inner: Arc<ReusingFsInner>,
    meta: Arc<ReusingSharedMeta>,
}

impl ReusingHandle {
    fn lookup_body(state: &mut ReusingFsState, orig_ino: Ino, body_id: u64) -> Option<&mut ReusingNodeBody> {
        if state.orphans.contains_key(&body_id) {
            return state.orphans.get_mut(&body_id);
        }
        state.inodes.get_mut(&orig_ino).filter(|b| b.meta.body_id == body_id)
    }

    fn with_body_mut<R>(&self, f: impl FnOnce(&mut ReusingNodeBody) -> R) -> Result<R, VfsError> {
        let mut state = self.inner.state.lock().unwrap();
        if let Some(body) = Self::lookup_body(&mut state, self.meta.orig_ino, self.meta.body_id) {
            return Ok(f(body));
        }
        drop(state);
        let mut retired = self.meta.retired_body.lock().unwrap();
        let body = retired.as_mut().ok_or(VfsError::NoEnt)?;
        Ok(f(body))
    }
}

impl VfsInode for ReusingHandle {
    fn lock_id(&self) -> u64 {
        self.lock_id
    }
    fn dev_id(&self) -> DevId {
        self.inner.dev
    }
    fn ino(&self) -> Ino {
        self.meta.current_ino.load(Ordering::Acquire)
    }
    fn inode_type(&self) -> InodeType {
        self.meta.inode_type
    }
    fn open_count(&self) -> u32 {
        self.meta.open_count.load(Ordering::Acquire)
    }
    fn inc_open_count(&self) {
        self.meta.open_count.fetch_add(1, Ordering::AcqRel);
    }
    fn dec_open_count(&self) -> u32 {
        let prev = self.meta.open_count.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 && self.meta.detached.load(Ordering::Acquire) {
            let mut state = self.inner.state.lock().unwrap();
            if state.orphans.remove(&self.meta.body_id).is_some() {
                state.free_inos.push(self.meta.orig_ino);
            }
        }
        prev - 1
    }
    fn is_detached(&self) -> bool {
        self.meta.detached.load(Ordering::Acquire)
    }
    fn stat(&self) -> Result<Metadata, VfsError> {
        assert_test_lock9_free();
        self.with_body_mut(|body| {
            let size = match self.meta.inode_type {
                InodeType::File => body.data.len() as u64,
                InodeType::Symlink => body.symlink_target.len() as u64,
                _ => 0,
            };
            Metadata {
                dev: self.inner.dev,
                ino: self.ino(),
                inode_type: self.meta.inode_type,
                mode: body.mode,
                nlink: body.nlink,
                uid: 0,
                gid: 0,
                rdev: DevId(0),
                size,
                blksize: 4096,
                blocks: size.div_ceil(512),
                atime: Timespec::default(),
                mtime: Timespec::default(),
                ctime: Timespec::default(),
            }
        })
    }
    fn lookup(&self, name: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
        assert_test_lock9_free();
        if self.meta.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        let hook = self.inner.hooks.lock().unwrap().pre_lookup.remove(name);
        if let Some(hook) = hook {
            hook();
        }
        let res = {
            let state = self.inner.state.lock().unwrap();
            let Some(dir_body) = state.inodes.get(&self.meta.orig_ino).filter(|b| b.meta.body_id == self.meta.body_id)
            else {
                return Err(VfsError::NoEnt);
            };
            if name == "." {
                Ok(FreshArcReusingFs::make_handle(&self.inner, Arc::clone(&self.meta)) as Arc<dyn VfsInode>)
            } else if name == ".." {
                let p_ino = dir_body.parent;
                state
                    .inodes
                    .get(&p_ino)
                    .map(|p| FreshArcReusingFs::make_handle(&self.inner, Arc::clone(&p.meta)) as Arc<dyn VfsInode>)
                    .ok_or(VfsError::NoEnt)
            } else if let Some(&c_ino) = dir_body.children.get(name) {
                state
                    .inodes
                    .get(&c_ino)
                    .map(|c| FreshArcReusingFs::make_handle(&self.inner, Arc::clone(&c.meta)) as Arc<dyn VfsInode>)
                    .ok_or(VfsError::NoEnt)
            } else {
                Err(VfsError::NoEnt)
            }
        };
        let hook = self.inner.hooks.lock().unwrap().post_lookup.remove(name);
        if let Some(hook) = hook {
            hook();
        }
        res
    }
    fn create(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        assert_test_lock9_free();
        if self.meta.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        let hook = self.inner.hooks.lock().unwrap().pre_create.remove(name);
        if let Some(hook) = hook {
            hook();
        }
        let res = {
            let mut state = self.inner.state.lock().unwrap();
            let Some(dir_body) = state.inodes.get(&self.meta.orig_ino).filter(|b| b.meta.body_id == self.meta.body_id)
            else {
                return Err(VfsError::NoEnt);
            };
            if dir_body.children.contains_key(name) {
                return Err(VfsError::Exist);
            }
            let (ino, body_id) = FreshArcReusingFs::alloc_ino_and_body_id(&mut state);
            let meta = Arc::new(ReusingSharedMeta {
                body_id,
                orig_ino: ino,
                inode_type: InodeType::File,
                open_count: AtomicU32::new(0),
                detached: AtomicBool::new(false),
                current_ino: AtomicU64::new(ino),
                locks: Mutex::new(AdvisoryLocks::new()),
                retired_body: Mutex::new(None),
            });
            state.inodes.insert(
                ino,
                ReusingNodeBody {
                    meta: Arc::clone(&meta),
                    mode: mode & 0o7777,
                    nlink: 1,
                    data: Vec::new(),
                    symlink_target: String::new(),
                    children: BTreeMap::new(),
                    parent: self.meta.orig_ino,
                },
            );
            state.inodes.get_mut(&self.meta.orig_ino).unwrap().children.insert(String::from(name), ino);
            Ok(FreshArcReusingFs::make_handle(&self.inner, meta) as Arc<dyn VfsInode>)
        };
        let hook = self.inner.hooks.lock().unwrap().post_create.remove(name);
        if let Some(hook) = hook {
            hook();
        }
        res
    }
    fn mkdir(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        assert_test_lock9_free();
        if self.meta.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        let hook = self.inner.hooks.lock().unwrap().pre_mkdir.remove(name);
        if let Some(hook) = hook {
            hook();
        }
        let res = {
            let mut state = self.inner.state.lock().unwrap();
            let Some(dir_body) = state.inodes.get(&self.meta.orig_ino).filter(|b| b.meta.body_id == self.meta.body_id)
            else {
                return Err(VfsError::NoEnt);
            };
            if dir_body.children.contains_key(name) {
                return Err(VfsError::Exist);
            }
            let (ino, body_id) = FreshArcReusingFs::alloc_ino_and_body_id(&mut state);
            let meta = Arc::new(ReusingSharedMeta {
                body_id,
                orig_ino: ino,
                inode_type: InodeType::Dir,
                open_count: AtomicU32::new(0),
                detached: AtomicBool::new(false),
                current_ino: AtomicU64::new(ino),
                locks: Mutex::new(AdvisoryLocks::new()),
                retired_body: Mutex::new(None),
            });
            state.inodes.insert(
                ino,
                ReusingNodeBody {
                    meta: Arc::clone(&meta),
                    mode: mode & 0o7777,
                    nlink: 2,
                    data: Vec::new(),
                    symlink_target: String::new(),
                    children: BTreeMap::new(),
                    parent: self.meta.orig_ino,
                },
            );
            let parent_body = state.inodes.get_mut(&self.meta.orig_ino).unwrap();
            parent_body.children.insert(String::from(name), ino);
            parent_body.nlink += 1;
            Ok(FreshArcReusingFs::make_handle(&self.inner, meta) as Arc<dyn VfsInode>)
        };
        let hook = self.inner.hooks.lock().unwrap().post_mkdir.remove(name);
        if let Some(hook) = hook {
            hook();
        }
        res
    }
    fn unlink(&self, name: &str) -> Result<(), VfsError> {
        assert_test_lock9_free();
        if self.meta.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        let hook = self.inner.hooks.lock().unwrap().pre_unlink.remove(name);
        if let Some(hook) = hook {
            hook();
        }
        let res = {
            let mut state = self.inner.state.lock().unwrap();
            let Some(dir_body) = state.inodes.get(&self.meta.orig_ino).filter(|b| b.meta.body_id == self.meta.body_id)
            else {
                return Err(VfsError::NoEnt);
            };
            let Some(&c_ino) = dir_body.children.get(name) else {
                return Err(VfsError::NoEnt);
            };
            if state.inodes.get(&c_ino).is_some_and(|c| c.meta.inode_type == InodeType::Dir) {
                return Err(VfsError::IsDir);
            }
            state.inodes.get_mut(&self.meta.orig_ino).unwrap().children.remove(name);
            if let Some(c_body) = state.inodes.get_mut(&c_ino) {
                c_body.nlink = c_body.nlink.saturating_sub(1);
            }
            FreshArcReusingFs::retire_if_unlinked(&mut state, c_ino);
            Ok(())
        };
        let hook = self.inner.hooks.lock().unwrap().post_unlink.remove(name);
        if let Some(hook) = hook {
            hook();
        }
        res
    }
    fn rmdir(&self, name: &str) -> Result<(), VfsError> {
        assert_test_lock9_free();
        if self.meta.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        let hook = self.inner.hooks.lock().unwrap().pre_rmdir.remove(name);
        if let Some(hook) = hook {
            hook();
        }
        let override_err = self.inner.hooks.lock().unwrap().rmdir_override_err.remove(name);
        if let Some(err) = override_err {
            return Err(err);
        }
        let mut state = self.inner.state.lock().unwrap();
        let Some(dir_body) = state.inodes.get(&self.meta.orig_ino).filter(|b| b.meta.body_id == self.meta.body_id)
        else {
            return Err(VfsError::NoEnt);
        };
        let Some(&c_ino) = dir_body.children.get(name) else {
            return Err(VfsError::NoEnt);
        };
        let Some(c_body) = state.inodes.get(&c_ino) else {
            return Err(VfsError::NoEnt);
        };
        if c_body.meta.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if !c_body.children.is_empty() {
            return Err(VfsError::NotEmpty);
        }
        let p_body = state.inodes.get_mut(&self.meta.orig_ino).unwrap();
        p_body.children.remove(name);
        p_body.nlink = p_body.nlink.saturating_sub(1);
        if let Some(c_body) = state.inodes.get_mut(&c_ino) {
            c_body.nlink = 0;
        }
        FreshArcReusingFs::retire_if_unlinked(&mut state, c_ino);
        Ok(())
    }
    fn link(&self, name: &str, target: &Arc<dyn VfsInode>) -> Result<(), VfsError> {
        assert_test_lock9_free();
        if self.meta.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if target.inode_type() == InodeType::Dir {
            return Err(VfsError::Perm);
        }
        if target.dev_id() != self.inner.dev {
            return Err(VfsError::XDev);
        }
        if target.is_detached() {
            return Err(VfsError::NoEnt);
        }
        let target_ino = target.ino();
        let mut state = self.inner.state.lock().unwrap();
        let Some(dir_body) = state.inodes.get(&self.meta.orig_ino).filter(|b| b.meta.body_id == self.meta.body_id)
        else {
            return Err(VfsError::NoEnt);
        };
        if dir_body.children.contains_key(name) {
            return Err(VfsError::Exist);
        }
        let Some(target_body) = state.inodes.get_mut(&target_ino) else {
            return Err(VfsError::NoEnt);
        };
        if target_body.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        target_body.nlink += 1;
        state.inodes.get_mut(&self.meta.orig_ino).unwrap().children.insert(String::from(name), target_ino);
        Ok(())
    }
    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
        assert_test_lock9_free();
        if self.meta.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        let hook = self.inner.hooks.lock().unwrap().pre_symlink.remove(name);
        if let Some(hook) = hook {
            hook();
        }
        let res = {
            let mut state = self.inner.state.lock().unwrap();
            let Some(dir_body) = state.inodes.get(&self.meta.orig_ino).filter(|b| b.meta.body_id == self.meta.body_id)
            else {
                return Err(VfsError::NoEnt);
            };
            if dir_body.children.contains_key(name) {
                return Err(VfsError::Exist);
            }
            let (ino, body_id) = FreshArcReusingFs::alloc_ino_and_body_id(&mut state);
            let meta = Arc::new(ReusingSharedMeta {
                body_id,
                orig_ino: ino,
                inode_type: InodeType::Symlink,
                open_count: AtomicU32::new(0),
                detached: AtomicBool::new(false),
                current_ino: AtomicU64::new(ino),
                locks: Mutex::new(AdvisoryLocks::new()),
                retired_body: Mutex::new(None),
            });
            state.inodes.insert(
                ino,
                ReusingNodeBody {
                    meta: Arc::clone(&meta),
                    mode: 0o777,
                    nlink: 1,
                    data: Vec::new(),
                    symlink_target: String::from(target),
                    children: BTreeMap::new(),
                    parent: self.meta.orig_ino,
                },
            );
            state.inodes.get_mut(&self.meta.orig_ino).unwrap().children.insert(String::from(name), ino);
            Ok(FreshArcReusingFs::make_handle(&self.inner, meta) as Arc<dyn VfsInode>)
        };
        let hook = self.inner.hooks.lock().unwrap().post_symlink.remove(name);
        if let Some(hook) = hook {
            hook();
        }
        res
    }
    fn readlink(&self) -> Result<Vec<u8>, VfsError> {
        assert_test_lock9_free();
        if self.meta.inode_type != InodeType::Symlink {
            return Err(VfsError::Inval);
        }
        self.with_body_mut(|body| body.symlink_target.as_bytes().to_vec())
    }
    fn rename(
        &self,
        old_name: &str,
        new_dir: &Arc<dyn VfsInode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<(), VfsError> {
        assert_test_lock9_free();
        if self.meta.inode_type != InodeType::Dir || new_dir.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if new_dir.dev_id() != self.inner.dev || new_dir.is_detached() {
            return Err(if new_dir.dev_id() != self.inner.dev { VfsError::XDev } else { VfsError::NoEnt });
        }
        let old_dir_ino = self.meta.orig_ino;
        let new_dir_ino = new_dir.ino();
        let res = {
            let mut state = self.inner.state.lock().unwrap();
            if !state.inodes.get(&old_dir_ino).is_some_and(|b| b.meta.body_id == self.meta.body_id)
                || !state.inodes.contains_key(&new_dir_ino)
            {
                return Err(VfsError::NoEnt);
            }
            let old_child_ino = state.inodes.get(&old_dir_ino).and_then(|d| d.children.get(old_name).copied());
            let Some(old_child_ino) = old_child_ino else {
                return Err(VfsError::NoEnt);
            };
            let new_child_ino = state.inodes.get(&new_dir_ino).and_then(|d| d.children.get(new_name).copied());

            if flags.has_exchange() {
                let Some(dst_ino) = new_child_ino else {
                    return Err(VfsError::NoEnt);
                };
                if old_dir_ino == new_dir_ino && old_name == new_name {
                    return Ok(());
                }
                let old_is_dir = state.inodes.get(&old_child_ino).is_some_and(|b| b.meta.inode_type == InodeType::Dir);
                let dst_is_dir = state.inodes.get(&dst_ino).is_some_and(|b| b.meta.inode_type == InodeType::Dir);
                state.inodes.get_mut(&new_dir_ino).unwrap().children.insert(String::from(new_name), old_child_ino);
                state.inodes.get_mut(&old_dir_ino).unwrap().children.insert(String::from(old_name), dst_ino);
                if old_dir_ino != new_dir_ino {
                    if old_is_dir {
                        state.inodes.get_mut(&old_child_ino).unwrap().parent = new_dir_ino;
                        let od = state.inodes.get_mut(&old_dir_ino).unwrap();
                        od.nlink = od.nlink.saturating_sub(1);
                        state.inodes.get_mut(&new_dir_ino).unwrap().nlink += 1;
                    }
                    if dst_is_dir {
                        state.inodes.get_mut(&dst_ino).unwrap().parent = old_dir_ino;
                        let nd = state.inodes.get_mut(&new_dir_ino).unwrap();
                        nd.nlink = nd.nlink.saturating_sub(1);
                        state.inodes.get_mut(&old_dir_ino).unwrap().nlink += 1;
                    }
                }
                Ok(())
            } else {
                if flags.has_noreplace() && new_child_ino.is_some() {
                    return Err(VfsError::Exist);
                }
                if old_dir_ino == new_dir_ino && old_name == new_name {
                    return Ok(());
                }
                if let Some(dst_ino) = new_child_ino
                    && let Some(dst_body) = state.inodes.get(&dst_ino)
                    && dst_body.meta.inode_type == InodeType::Dir
                    && !dst_body.children.is_empty()
                {
                    return Err(VfsError::NotEmpty);
                }

                let old_is_dir = state.inodes.get(&old_child_ino).is_some_and(|b| b.meta.inode_type == InodeType::Dir);
                state.inodes.get_mut(&old_dir_ino).unwrap().children.remove(old_name);
                if old_dir_ino != new_dir_ino && old_is_dir {
                    state.inodes.get_mut(&old_child_ino).unwrap().parent = new_dir_ino;
                    let od = state.inodes.get_mut(&old_dir_ino).unwrap();
                    od.nlink = od.nlink.saturating_sub(1);
                    state.inodes.get_mut(&new_dir_ino).unwrap().nlink += 1;
                }
                let replaced =
                    state.inodes.get_mut(&new_dir_ino).unwrap().children.insert(String::from(new_name), old_child_ino);
                if let Some(dst_ino) = replaced
                    && dst_ino != old_child_ino
                {
                    let dst_is_dir = state.inodes.get(&dst_ino).is_some_and(|b| b.meta.inode_type == InodeType::Dir);
                    if dst_is_dir {
                        let nd = state.inodes.get_mut(&new_dir_ino).unwrap();
                        nd.nlink = nd.nlink.saturating_sub(1);
                    }
                    if let Some(dst_body) = state.inodes.get_mut(&dst_ino) {
                        dst_body.nlink = if dst_is_dir { 0 } else { dst_body.nlink.saturating_sub(1) };
                    }
                    FreshArcReusingFs::retire_if_unlinked(&mut state, dst_ino);
                }
                Ok(())
            }
        };
        let hook = self.inner.hooks.lock().unwrap().post_rename.remove(new_name);
        if let Some(hook) = hook {
            hook();
        }
        res
    }
    fn readdir(&self, cursor: u64, out: &mut Vec<DirEntry>, max_entries: usize) -> Result<(), VfsError> {
        assert_test_lock9_free();
        if self.meta.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        let state = self.inner.state.lock().unwrap();
        let Some(dir_body) = state.inodes.get(&self.meta.orig_ino).filter(|b| b.meta.body_id == self.meta.body_id)
        else {
            return Ok(());
        };
        let parent_ino =
            state.inodes.get(&dir_body.parent).map_or(self.ino(), |p| p.meta.current_ino.load(Ordering::Acquire));
        let mut list: Vec<(Ino, InodeType, String)> = Vec::with_capacity(2 + dir_body.children.len());
        list.push((self.ino(), InodeType::Dir, String::from(".")));
        list.push((parent_ino, InodeType::Dir, String::from("..")));
        for (name, &c_ino) in &dir_body.children {
            if let Some(c_body) = state.inodes.get(&c_ino) {
                list.push((c_body.meta.current_ino.load(Ordering::Acquire), c_body.meta.inode_type, name.clone()));
            }
        }
        for (idx, (ino, inode_type, name)) in list.into_iter().enumerate().skip(cursor as usize) {
            if out.len() >= max_entries {
                break;
            }
            out.push(DirEntry { ino, inode_type, name, next_cursor: (idx + 1) as u64 });
        }
        Ok(())
    }
    fn truncate(&self, size: u64) -> Result<(), VfsError> {
        assert_test_lock9_free();
        if self.meta.inode_type != InodeType::File {
            return Err(VfsError::IsDir);
        }
        self.with_body_mut(|body| {
            body.data.resize(size as usize, 0);
        })
    }
    fn set_mode(&self, mode: u16) -> Result<(), VfsError> {
        assert_test_lock9_free();
        self.with_body_mut(|body| {
            body.mode = mode & 0o7777;
        })
    }
    fn set_owner(&self, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), VfsError> {
        assert_test_lock9_free();
        Ok(())
    }
    fn set_times(&self, _atime: SetTime, _mtime: SetTime) -> Result<(), VfsError> {
        assert_test_lock9_free();
        Ok(())
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        assert_test_lock9_free();
        if self.meta.inode_type != InodeType::File {
            return Err(VfsError::IsDir);
        }
        self.with_body_mut(|body| {
            let off = offset as usize;
            if off >= body.data.len() {
                return 0;
            }
            let n = buf.len().min(body.data.len() - off);
            buf[..n].copy_from_slice(&body.data[off..off + n]);
            n
        })
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
        assert_test_lock9_free();
        if self.meta.inode_type != InodeType::File {
            return Err(VfsError::IsDir);
        }
        self.with_body_mut(|body| {
            let end = offset as usize + buf.len();
            if end > body.data.len() {
                body.data.resize(end, 0);
            }
            body.data[offset as usize..end].copy_from_slice(buf);
            buf.len()
        })
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
        assert_test_lock9_free();
        Ok(())
    }
    fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
        assert_test_lock9_free();
        let mut locks = self.meta.locks.lock().unwrap();
        f(&mut locks);
        Ok(())
    }
}

#[test]
fn inode_cache_removal_ring_bounded_and_overflow_safe() {
    let fs = FreshArcReusingFs::new(DevId::ROOT, "ext4", false, true);
    let tmpfs = MemFs::new(DevId::TMPFS, "tmpfs", false);
    let mut cache = InodeCache::with_removal_history(4);

    let g0 = cache.removal_gen();
    let f10 = fs.root_inode().create("f10", 0o644).unwrap();
    let f11 = fs.root_inode().create("f11", 0o644).unwrap();
    let ino10 = f10.ino();
    let tmp_f = tmpfs.root_inode().create("tf", 0o644).unwrap();
    let tmp_ino = tmp_f.ino();

    assert!(cache.get_or_insert_if_fresh(Arc::clone(&f10), g0).is_ok());
    assert!(cache.get_or_insert_if_fresh(Arc::clone(&f11), g0).is_ok());
    assert!(cache.get_or_insert_if_fresh(Arc::clone(&tmp_f), g0).is_ok());

    // Unrelated key removal does not cause `f10` or `f11` to be considered stale.
    assert!(cache.remove(DevId::ROOT, 999).is_none());
    assert!(cache.get_if_fresh(DevId::ROOT, ino10, g0).is_some());
    assert!(cache.get_or_insert_if_fresh(Arc::clone(&f11), g0).is_ok());

    // Same-key compaction keeps `removal_order_len() <= 2 * max_removal_history` and `oldest_tracked_gen == 0`.
    for _ in 0..20 {
        let _ = cache.remove(DevId::ROOT, 50);
    }
    assert!(cache.removal_order_len() <= 8, "removal_order_len={}", cache.removal_order_len());
    assert_eq!(cache.oldest_tracked_gen(), 0);

    // `evict_fs` evicts only `TMPFS` and updates `fs_evict_gen` for `TMPFS` without affecting `ROOT`.
    let g_before_evict = cache.removal_gen();
    let evicted = cache.evict_fs(DevId::TMPFS);
    assert_eq!(evicted.len(), 1);
    assert!(cache.get_if_fresh(DevId::TMPFS, tmp_ino, g_before_evict).is_none());
    assert!(cache.get_or_insert_if_fresh(Arc::clone(&tmp_f), g_before_evict).is_err());
    assert!(cache.get_if_fresh(DevId::ROOT, ino10, g_before_evict).is_some());

    // Eager detached-entry eviction in `get`, `get_if_fresh`, `get_or_insert`, and `get_or_insert_if_fresh`.
    let mem = MemFs::new(DevId::makedev(0, 55), "mem", false);
    let mf = mem.root_inode().create("mf", 0o644).unwrap();
    let mf_ino = mf.ino();
    let mf_guard = OpenGuard::new_unchecked(Arc::clone(&mf));
    let g_before_mf = cache.removal_gen();
    assert!(cache.get_or_insert_if_fresh(Arc::clone(&mf), g_before_mf).is_ok());
    mem.root_inode().unlink("mf").unwrap();
    assert!(mf.is_detached());
    assert!(cache.get(mem.dev_id(), mf_ino).is_none());
    assert!(cache.get_if_fresh(mem.dev_id(), mf_ino, g_before_mf).is_none());
    // Re-insert directly via `get_or_insert`? `mf` is detached, so `get_or_insert` skips it too.
    assert!(Arc::ptr_eq(&cache.get_or_insert(Arc::clone(&mf)), &mf));
    assert!(cache.get(mem.dev_id(), mf_ino).is_none());

    // Simulate a cached entry that becomes detached while still in `inodes` (e.g. direct backend detach).
    let mf2 = mem.root_inode().create("mf2", 0o644).unwrap();
    let mf2_ino = mf2.ino();
    let mf2_guard = OpenGuard::new_unchecked(Arc::clone(&mf2));
    let g_mf2 = cache.removal_gen();
    assert!(cache.get_or_insert_if_fresh(Arc::clone(&mf2), g_mf2).is_ok());
    mem.root_inode().unlink("mf2").unwrap();
    // Force a fresh live inode with same `(dev, mf2_ino)` using a mock wrapper or check `get_or_insert_if_fresh` eviction.
    let mf2_fresh = mem.root_inode().create("mf2_other", 0o644).unwrap();
    let _ = mf2_fresh;
    assert!(cache.get_if_fresh(mem.dev_id(), mf2_ino, g_mf2).is_none());
    // Now `get_or_insert` on a fresh non-detached inode when a detached one was cached evicts the stale one.
    let mf3 = mem.root_inode().create("mf3", 0o644).unwrap();
    let mf3_ino = mf3.ino();
    let mf3_guard = OpenGuard::new_unchecked(Arc::clone(&mf3));
    cache.get_or_insert(Arc::clone(&mf3));
    mem.root_inode().unlink("mf3").unwrap();
    // `mf3` is detached in `cache`; `get_or_insert_if_fresh` on a non-detached `f10`-style entry or checking `remove_if_fresh`:
    assert!(cache.get(mem.dev_id(), mf3_ino).is_none());
    drop(mf_guard);
    drop(mf2_guard);
    drop(mf3_guard);

    // Conservative `oldest_tracked_gen` overflow behavior (cap = 4, remove 5 distinct keys).
    let g_pre_overflow = cache.removal_gen();
    for k in 100..105u64 {
        let _ = cache.remove(DevId::ROOT, k);
    }
    assert!(cache.oldest_tracked_gen() > g_pre_overflow);
    let f200 = fs.root_inode().create("f200", 0o644).unwrap();
    assert!(cache.get_or_insert_if_fresh(Arc::clone(&f200), g_pre_overflow).is_err());
    assert!(cache.get_if_fresh(DevId::ROOT, ino10, g_pre_overflow).is_none());
    let g_oldest = cache.oldest_tracked_gen();
    assert!(cache.get_or_insert_if_fresh(Arc::clone(&f200), g_oldest).is_ok());
}

#[test]
fn inode_cache_survives_unrelated_dentry_invalidations_on_fresh_arc_backend() {
    // Parts 1–3 invoke `unlink_at` re-entrantly inside `post_create`/`post_mkdir`/`post_symlink` hooks
    // on the same `fs`, so they run with `use_rename_lock = false` to avoid self-deadlocking Lock 3.
    let fs_nolock = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, false);
    let env_nolock = SyncVfsTables::new(Arc::clone(&fs_nolock) as Arc<dyn FileSystem>);

    for i in 1..=3 {
        let p = format!("/unrel_{i}");
        open_inode(
            env_nolock.as_ref(),
            None,
            p.as_bytes(),
            OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
            0o644,
            0,
        )
        .unwrap();
    }

    // 1. Unrelated unlink right after `create`.
    let env_c = Arc::clone(&env_nolock);
    fs_nolock.inner.hooks.lock().unwrap().post_create.insert(
        String::from("f_create"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_c.as_ref(), None, b"/unrel_1", false).unwrap();
        }),
    );
    let (created, _) = open_inode(
        env_nolock.as_ref(),
        None,
        b"/f_create",
        OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
        0o644,
        0,
    )
    .unwrap();
    let resolved_c = resolve_path(env_nolock.as_ref(), None, b"/f_create", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&created, &resolved_c));

    // 2. Unrelated unlink right after `mkdir`.
    let env_m = Arc::clone(&env_nolock);
    fs_nolock.inner.hooks.lock().unwrap().post_mkdir.insert(
        String::from("d_mkdir"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_m.as_ref(), None, b"/unrel_2", false).unwrap();
        }),
    );
    let mk = mkdir_at(env_nolock.as_ref(), None, b"/d_mkdir", 0o755, 0).unwrap();
    let resolved_m = resolve_path(env_nolock.as_ref(), None, b"/d_mkdir", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&mk, &resolved_m));

    // 3. Unrelated unlink right after `symlink`.
    let env_s = Arc::clone(&env_nolock);
    fs_nolock.inner.hooks.lock().unwrap().post_symlink.insert(
        String::from("s_sym"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_s.as_ref(), None, b"/unrel_3", false).unwrap();
        }),
    );
    let sym = symlink_at(env_nolock.as_ref(), b"/f_create", None, b"/s_sym").unwrap();
    let resolved_s = resolve_path(env_nolock.as_ref(), None, b"/s_sym", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&sym, &resolved_s));

    // Parts 4–5 run `resolve_path` `post_lookup` hooks (not holding Lock 3) with `use_rename_lock = true`.
    let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, true);
    let env = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);
    let (created_l, _) =
        open_inode(env.as_ref(), None, b"/f_create", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    for i in 4..=5 {
        let p = format!("/unrel_{i}");
        open_inode(env.as_ref(), None, p.as_bytes(), OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    }

    // 4. Unrelated unlink right after `lookup` (on dentry cache miss).
    env.dentry_invalidate(DevId::ROOT, 1, "f_create");
    let env_l = Arc::clone(&env);
    fs.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from("f_create"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_l.as_ref(), None, b"/unrel_4", false).unwrap();
        }),
    );
    let looked = resolve_path(env.as_ref(), None, b"/f_create", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&created_l, &looked));

    // 5. Codex P1 preserved: same-key `unlink` or `rename` right after `lookup` still skips DentryCache resurrection.
    env.dentry_invalidate(DevId::ROOT, 1, "unrel_5");
    let env_same = Arc::clone(&env);
    fs.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from("unrel_5"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_same.as_ref(), None, b"/unrel_5", false).unwrap();
        }),
    );
    let pre_un = resolve_path(env.as_ref(), None, b"/unrel_5", ResolveFlags::default()).unwrap().inode;
    assert_eq!(pre_un.stat().unwrap().nlink, 0);
    assert!(env.dentry_get(DevId::ROOT, 1, "unrel_5").is_none());
    assert_eq!(resolve_path(env.as_ref(), None, b"/unrel_5", ResolveFlags::default()).unwrap_err(), VfsError::NoEnt);

    open_inode(env.as_ref(), None, b"/ren_src", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    env.dentry_invalidate(DevId::ROOT, 1, "ren_src");
    let env_ren = Arc::clone(&env);
    fs.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from("ren_src"),
        alloc::boxed::Box::new(move || {
            rename_at2(env_ren.as_ref(), None, b"/ren_src", None, b"/ren_dst", RenameFlags(0)).unwrap();
        }),
    );
    let pre_ren = resolve_path(env.as_ref(), None, b"/ren_src", ResolveFlags::default()).unwrap().inode;
    assert!(env.dentry_get(DevId::ROOT, 1, "ren_src").is_none());
    assert_eq!(resolve_path(env.as_ref(), None, b"/ren_src", ResolveFlags::default()).unwrap_err(), VfsError::NoEnt);
    let dst = resolve_path(env.as_ref(), None, b"/ren_dst", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&pre_ren, &dst));
}

#[test]
fn ino_reuse_during_lookup_create_mkdir_symlink_never_aliases_wrong_file() {
    let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, true);
    let env = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);
    mkdir_at(env.as_ref(), None, b"/a", 0o755, 0).unwrap();

    // Part A1: `lookup_component` interrupted by `unlink_at` + `create` reusing exact freed `ino`.
    let (foo, _) =
        open_inode(env.as_ref(), None, b"/a/foo", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    let foo_ino = foo.ino();
    foo.write_at(0, b"FOO").unwrap();
    drop(foo);
    env.dentry_invalidate(DevId::ROOT, 2, "foo");

    let env_h = Arc::clone(&env);
    fs.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from("foo"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_h.as_ref(), None, b"/a/foo", false).unwrap();
            let (bar, _) = open_inode(
                env_h.as_ref(),
                None,
                b"/a/bar",
                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                0o644,
                0,
            )
            .unwrap();
            bar.write_at(0, b"BAR").unwrap();
        }),
    );
    let got_foo = resolve_path(env.as_ref(), None, b"/a/foo", ResolveFlags::default()).unwrap().inode;
    let bar_live = resolve_path(env.as_ref(), None, b"/a/bar", ResolveFlags::default()).unwrap().inode;
    assert_eq!(bar_live.ino(), foo_ino);
    assert!(!Arc::ptr_eq(&got_foo, &bar_live));
    let mut buf = [0u8; 8];
    let n = got_foo.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"FOO");
    assert!(Arc::ptr_eq(&env.inode_get(DevId::ROOT, foo_ino).unwrap(), &bar_live));

    // Parts A2–A5 & Part B (`new_f`/`new_d`/`new_s`) invoke `unlink_at`/`open_inode(O_CREAT)` re-entrantly
    // inside hooks under Lock 3, so they run on `use_rename_lock = false`.
    let fs_nolock = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, false);
    let env_nolock = SyncVfsTables::new(Arc::clone(&fs_nolock) as Arc<dyn FileSystem>);
    mkdir_at(env_nolock.as_ref(), None, b"/a", 0o755, 0).unwrap();

    // Part A2: `open_inode(O_CREAT)` `lookup` arm interrupted by `unlink_at` + `create` reusing `ino`.
    let (ex, _) =
        open_inode(env_nolock.as_ref(), None, b"/a/ex", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let ex_ino = ex.ino();
    ex.write_at(0, b"EX").unwrap();
    drop(ex);

    let env_h = Arc::clone(&env_nolock);
    fs_nolock.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from("ex"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_h.as_ref(), None, b"/a/ex", false).unwrap();
            let (bar, _) = open_inode(
                env_h.as_ref(),
                None,
                b"/a/bar_ex",
                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                0o644,
                0,
            )
            .unwrap();
            bar.write_at(0, b"BAR_EX").unwrap();
        }),
    );
    let (got_ex, _) =
        open_inode(env_nolock.as_ref(), None, b"/a/ex", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let bar_ex = resolve_path(env_nolock.as_ref(), None, b"/a/bar_ex", ResolveFlags::default()).unwrap().inode;
    assert_eq!(bar_ex.ino(), ex_ino);
    assert!(!Arc::ptr_eq(&got_ex, &bar_ex));
    assert_ne!(got_ex.ino(), ex_ino);
    let n = got_ex.read_at(0, &mut buf).unwrap();
    assert_eq!(n, 0);
    assert!(Arc::ptr_eq(&env_nolock.inode_get(DevId::ROOT, ex_ino).unwrap(), &bar_ex));
    assert!(Arc::ptr_eq(&env_nolock.inode_get(DevId::ROOT, got_ex.ino()).unwrap(), &got_ex));

    // Part A3: post-`create` interrupted by `unlink_at` + `create` reusing `ino`.
    let env_h = Arc::clone(&env_nolock);
    fs_nolock.inner.hooks.lock().unwrap().post_create.insert(
        String::from("c1"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_h.as_ref(), None, b"/a/c1", false).unwrap();
            let (bar, _) = open_inode(
                env_h.as_ref(),
                None,
                b"/a/bar_c1",
                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                0o644,
                0,
            )
            .unwrap();
            bar.write_at(0, b"BAR_C1").unwrap();
        }),
    );
    let (got_c1, _) =
        open_inode(env_nolock.as_ref(), None, b"/a/c1", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let bar_c1 = resolve_path(env_nolock.as_ref(), None, b"/a/bar_c1", ResolveFlags::default()).unwrap().inode;
    assert_eq!(got_c1.ino(), bar_c1.ino());
    assert!(!Arc::ptr_eq(&got_c1, &bar_c1));
    assert_eq!(got_c1.stat().unwrap().nlink, 0);
    assert!(Arc::ptr_eq(&env_nolock.inode_get(DevId::ROOT, bar_c1.ino()).unwrap(), &bar_c1));

    // Part A4: post-`mkdir` interrupted by `rmdir` + `create` reusing `ino`.
    let env_h = Arc::clone(&env_nolock);
    fs_nolock.inner.hooks.lock().unwrap().post_mkdir.insert(
        String::from("d1"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_h.as_ref(), None, b"/a/d1", true).unwrap();
            let (bar, _) = open_inode(
                env_h.as_ref(),
                None,
                b"/a/bar_d1",
                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                0o644,
                0,
            )
            .unwrap();
            bar.write_at(0, b"BAR_D1").unwrap();
        }),
    );
    let got_d1 = mkdir_at(env_nolock.as_ref(), None, b"/a/d1", 0o755, 0).unwrap();
    let bar_d1 = resolve_path(env_nolock.as_ref(), None, b"/a/bar_d1", ResolveFlags::default()).unwrap().inode;
    assert_eq!(got_d1.ino(), bar_d1.ino());
    assert_eq!(got_d1.inode_type(), InodeType::Dir);
    assert_eq!(bar_d1.inode_type(), InodeType::File);
    assert!(Arc::ptr_eq(&env_nolock.inode_get(DevId::ROOT, bar_d1.ino()).unwrap(), &bar_d1));

    // Part A5: post-`symlink` interrupted by `unlink_at` + `create` reusing `ino`.
    let env_h = Arc::clone(&env_nolock);
    fs_nolock.inner.hooks.lock().unwrap().post_symlink.insert(
        String::from("s1"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_h.as_ref(), None, b"/a/s1", false).unwrap();
            let (bar, _) = open_inode(
                env_h.as_ref(),
                None,
                b"/a/bar_s1",
                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                0o644,
                0,
            )
            .unwrap();
            bar.write_at(0, b"BAR_S1").unwrap();
        }),
    );
    let got_s1 = symlink_at(env_nolock.as_ref(), b"/a/bar", None, b"/a/s1").unwrap();
    let bar_s1 = resolve_path(env_nolock.as_ref(), None, b"/a/bar_s1", ResolveFlags::default()).unwrap().inode;
    assert_eq!(got_s1.ino(), bar_s1.ino());
    assert_eq!(got_s1.inode_type(), InodeType::Symlink);
    assert_eq!(bar_s1.inode_type(), InodeType::File);
    assert!(Arc::ptr_eq(&env_nolock.inode_get(DevId::ROOT, bar_s1.ino()).unwrap(), &bar_s1));

    // Part B (`Case Y`): `unlink("old")` frees `ino = X` AFTER `cache_gens()` and BEFORE `create`/`mkdir`/`symlink`/`lookup` reuses `X` -> MUST unify in `InodeCache`.
    let (old_f, _) =
        open_inode(env_nolock.as_ref(), None, b"/a/old_f", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let old_f_ino = old_f.ino();
    drop(old_f);
    let env_y = Arc::clone(&env_nolock);
    fs_nolock.inner.hooks.lock().unwrap().pre_create.insert(
        String::from("new_f"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_y.as_ref(), None, b"/a/old_f", false).unwrap();
        }),
    );
    let (new_f, _) =
        open_inode(env_nolock.as_ref(), None, b"/a/new_f", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    assert_eq!(new_f.ino(), old_f_ino);
    let res_new_f = resolve_path(env_nolock.as_ref(), None, b"/a/new_f", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&new_f, &res_new_f));

    let (old_d, _) =
        open_inode(env_nolock.as_ref(), None, b"/a/old_d", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let old_d_ino = old_d.ino();
    drop(old_d);
    let env_y = Arc::clone(&env_nolock);
    fs_nolock.inner.hooks.lock().unwrap().pre_mkdir.insert(
        String::from("new_d"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_y.as_ref(), None, b"/a/old_d", false).unwrap();
        }),
    );
    let new_d = mkdir_at(env_nolock.as_ref(), None, b"/a/new_d", 0o755, 0).unwrap();
    assert_eq!(new_d.ino(), old_d_ino);
    let res_new_d = resolve_path(env_nolock.as_ref(), None, b"/a/new_d", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&new_d, &res_new_d));

    let (old_s, _) =
        open_inode(env_nolock.as_ref(), None, b"/a/old_s", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let old_s_ino = old_s.ino();
    drop(old_s);
    let env_y = Arc::clone(&env_nolock);
    fs_nolock.inner.hooks.lock().unwrap().pre_symlink.insert(
        String::from("new_s"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_y.as_ref(), None, b"/a/old_s", false).unwrap();
        }),
    );
    let new_s = symlink_at(env_nolock.as_ref(), b"new_f", None, b"/a/new_s").unwrap();
    assert_eq!(new_s.ino(), old_s_ino);
    let res_new_s = resolve_path(env_nolock.as_ref(), None, b"/a/new_s", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&new_s, &res_new_s));

    // `Case Y` on `lookup_component`: `old_l` unlinked and `new_l` created reusing `old_l`'s ino right before `dir.lookup("new_l")`.
    let (old_l, _) =
        open_inode(env.as_ref(), None, b"/a/old_l", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let old_l_ino = old_l.ino();
    drop(old_l);
    let env_y = Arc::clone(&env);
    fs.inner.hooks.lock().unwrap().pre_lookup.insert(
        String::from("new_l"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_y.as_ref(), None, b"/a/old_l", false).unwrap();
            let _ = open_inode(
                env_y.as_ref(),
                None,
                b"/a/new_l",
                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                0o644,
                0,
            )
            .unwrap();
        }),
    );
    let res_new_l_1 = resolve_path(env.as_ref(), None, b"/a/new_l", ResolveFlags::default()).unwrap().inode;
    assert_eq!(res_new_l_1.ino(), old_l_ino);
    let res_new_l_2 = resolve_path(env.as_ref(), None, b"/a/new_l", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&res_new_l_1, &res_new_l_2));
}

#[test]
fn concurrent_hardlink_unlink_and_rename_overwrite_evicts_inode_cache() {
    use std::sync::Barrier;

    // 1. Concurrent hard-link `unlink_at` across both `use_rename_lock` modes and `open_count` 0 vs >0.
    for use_rename_lock in [true, false] {
        for hold_open in [false, true] {
            let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, use_rename_lock);
            let env = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);
            mkdir_at(env.as_ref(), None, b"/hl", 0o755, 0).unwrap();

            for iter in 0..16 {
                let (orig, _) = open_inode(
                    env.as_ref(),
                    None,
                    b"/hl/a",
                    OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                    0o644,
                    0,
                )
                .unwrap();
                let orig_ino = orig.ino();
                orig.write_at(0, b"stale_hl").unwrap();
                link_at(env.as_ref(), None, b"/hl/a", None, b"/hl/b", false, false).unwrap();
                let open_guard = if hold_open { Some(OpenGuard::new_unchecked(Arc::clone(&orig))) } else { None };
                drop(orig);

                let barrier = Barrier::new(2);
                std::thread::scope(|s| {
                    s.spawn(|| {
                        barrier.wait();
                        unlink_at(env.as_ref(), None, b"/hl/a", false).unwrap();
                    });
                    s.spawn(|| {
                        barrier.wait();
                        unlink_at(env.as_ref(), None, b"/hl/b", false).unwrap();
                    });
                });

                assert!(
                    env.inode_get(DevId::ROOT, orig_ino).is_none(),
                    "iter={iter} rename_lock={use_rename_lock} hold_open={hold_open}: InodeCache still holds zombie!"
                );
                if let Some(g) = open_guard {
                    assert!(g.inode().is_detached());
                    drop(g);
                }

                // Reusing `orig_ino` yields a live inode, never a zombie.
                let (fresh, _) = open_inode(
                    env.as_ref(),
                    None,
                    b"/hl/fresh",
                    OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                    0o644,
                    0,
                )
                .unwrap();
                assert_eq!(fresh.ino(), orig_ino);
                assert!(!fresh.is_detached());
                assert_eq!(fresh.stat().unwrap().nlink, 1);
                fresh.write_at(0, b"fresh_hl").unwrap();
                let resolved = resolve_path(env.as_ref(), None, b"/hl/fresh", ResolveFlags::default()).unwrap().inode;
                assert!(Arc::ptr_eq(&fresh, &resolved));
                unlink_at(env.as_ref(), None, b"/hl/fresh", false).unwrap();
            }
        }
    }

    // 2. Concurrent `unlink_at("/hl2/a")` vs overwriting `rename_at2("/src", "/hl2/b")`.
    for use_rename_lock in [true, false] {
        let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, use_rename_lock);
        let env = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);
        mkdir_at(env.as_ref(), None, b"/hl2", 0o755, 0).unwrap();

        for iter in 0..16 {
            let (orig, _) =
                open_inode(env.as_ref(), None, b"/hl2/a", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
                    .unwrap();
            let orig_ino = orig.ino();
            link_at(env.as_ref(), None, b"/hl2/a", None, b"/hl2/b", false, false).unwrap();
            open_inode(env.as_ref(), None, b"/src", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
                .unwrap();
            drop(orig);

            let barrier = Barrier::new(2);
            std::thread::scope(|s| {
                s.spawn(|| {
                    barrier.wait();
                    unlink_at(env.as_ref(), None, b"/hl2/a", false).unwrap();
                });
                s.spawn(|| {
                    barrier.wait();
                    rename_at2(env.as_ref(), None, b"/src", None, b"/hl2/b", RenameFlags(0)).unwrap();
                });
            });

            assert!(
                env.inode_get(DevId::ROOT, orig_ino).is_none(),
                "iter={iter} rename_lock={use_rename_lock}: InodeCache retained overwritten hardlink!"
            );
            unlink_at(env.as_ref(), None, b"/hl2/b", false).unwrap();
        }
    }

    // 3. `rmdir` `NotEmpty` rollback after concurrent `ino` reuse must not evict new file or restore dead `child`.
    // `pre_rmdir` invokes `open_inode(O_CREAT)` on the same `fs`, so it runs with `use_rename_lock = false`.
    let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, false);
    let env = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);
    let victim = mkdir_at(env.as_ref(), None, b"/victim", 0o755, 0).unwrap();
    let victim_ino = victim.ino();
    let env_r = Arc::clone(&env);
    let fs_r = Arc::clone(&fs);
    let holder: Arc<Mutex<Option<Arc<dyn VfsInode>>>> = Arc::new(Mutex::new(None));
    let holder_c = Arc::clone(&holder);
    fs.inner.hooks.lock().unwrap().pre_rmdir.insert(
        String::from("victim"),
        alloc::boxed::Box::new(move || {
            // Concurrent unlink/rmdir of `victim` + immediate reuse of `victim_ino` by `new_reuser`.
            fs_r.root_inode().rmdir("victim").unwrap();
            let _ = env_r.inode_remove(DevId::ROOT, victim_ino);
            let (reuser, _) = open_inode(
                env_r.as_ref(),
                None,
                b"/new_reuser",
                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                0o644,
                0,
            )
            .unwrap();
            reuser.write_at(0, b"REUSER").unwrap();
            *holder_c.lock().unwrap() = Some(reuser);
            fs_r.inner.hooks.lock().unwrap().rmdir_override_err.insert(String::from("victim"), VfsError::NotEmpty);
        }),
    );

    assert_eq!(unlink_at(env.as_ref(), None, b"/victim", true).unwrap_err(), VfsError::NotEmpty);
    let reuser = holder.lock().unwrap().take().unwrap();
    assert_eq!(reuser.ino(), victim_ino);
    let cached = env.inode_get(DevId::ROOT, victim_ino).expect("new_reuser must remain in InodeCache");
    assert!(Arc::ptr_eq(&cached, &reuser));
    assert!(!Arc::ptr_eq(&cached, &victim));
}

#[test]
fn cross_mounts_down_vs_umount_does_not_leak_unmounted_root_inode() {
    struct HookMountFs {
        inner: Arc<MemFs>,
        on_root: Mutex<Option<FsOpHook>>,
    }

    impl FileSystem for HookMountFs {
        fn dev_id(&self) -> DevId {
            self.inner.dev_id()
        }
        fn name(&self) -> &'static str {
            self.inner.name()
        }
        fn root_inode(&self) -> Arc<dyn VfsInode> {
            assert_test_lock9_free();
            let hook = self.on_root.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
            self.inner.root_inode()
        }
        fn sync(&self) -> Result<(), VfsError> {
            assert_test_lock9_free();
            self.inner.sync()
        }
        fn read_only(&self) -> bool {
            self.inner.read_only()
        }
        fn with_rename_lock(&self, f: &mut dyn FnMut() -> Result<(), VfsError>) -> Result<(), VfsError> {
            assert_test_lock9_free();
            self.inner.with_rename_lock(f)
        }
    }

    let root_fs = MemFs::new(DevId::ROOT, "ext4", false);
    let env = SyncVfsTables::new(Arc::clone(&root_fs) as Arc<dyn FileSystem>);
    let mp = mkdir_at(env.as_ref(), None, b"/mp", 0o755, 0).unwrap();
    let mp_dev = mp.dev_id();
    let mp_ino = mp.ino();

    let hook_fs = Arc::new(HookMountFs { inner: MemFs::new(DevId::TMPFS, "tmpfs", false), on_root: Mutex::new(None) });
    env.mount(Arc::clone(&mp), "/mp", Arc::clone(&hook_fs) as Arc<dyn FileSystem>).unwrap();

    // Evict `/mp` dentry and arm `HookMountFs::root_inode` to `umount(/mp)` mid-`cross_mounts_down`.
    env.dentry_invalidate(DevId::ROOT, 1, "mp");
    let env_u = Arc::clone(&env);
    *hook_fs.on_root.lock().unwrap() = Some(alloc::boxed::Box::new(move || {
        env_u.umount(mp_dev, mp_ino).unwrap();
    }));

    let resolved = resolve_path(env.as_ref(), None, b"/mp", ResolveFlags::default()).unwrap().inode;
    assert_eq!(resolved.dev_id(), DevId::ROOT);
    assert_eq!(resolved.ino(), mp_ino);
    assert!(
        env.snapshot_inodes().into_iter().all(|i| i.dev_id() != DevId::TMPFS),
        "unmounted DevId::TMPFS root inode leaked into InodeCache!"
    );
}

fn run_unhooked_4_thread_concurrent_vfs_stress(use_rename_lock: bool, hist: usize, dentry_cap: usize) {
    let root_fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, use_rename_lock);
    let tmp_fs = FreshArcReusingFs::new(DevId::TMPFS, "fresh-tmpfs", false, use_rename_lock);
    let env = SyncVfsTables::with_removal_history(Arc::clone(&root_fs) as Arc<dyn FileSystem>, hist, dentry_cap);

    for d in [b"/d0" as &[u8], b"/d1", b"/d0/sub", b"/mnt"] {
        mkdir_at(env.as_ref(), None, d, 0o755, 0).unwrap();
    }
    let mnt_mp = resolve_path(env.as_ref(), None, b"/mnt", ResolveFlags::default()).unwrap().inode;

    let paths: [&[u8]; 16] = [
        b"/d0/a",
        b"/d0/b",
        b"/d0/c",
        b"/d0/sub/a",
        b"/d0/sub/b",
        b"/d0/sub/c",
        b"/d1/a",
        b"/d1/b",
        b"/d1/c",
        b"/d0/dir_x",
        b"/d1/dir_y",
        b"/d0/s0",
        b"/d1/s1",
        b"/mnt/f0",
        b"/mnt/f1",
        b"/mnt/sub",
    ];
    let file_paths = &paths[..9];
    let dir_paths: [&[u8]; 3] = [b"/d0/dir_x", b"/d1/dir_y", b"/mnt/sub"];
    let sym_paths: [&[u8]; 2] = [b"/d0/s0", b"/d1/s1"];

    let mnt_lock = Mutex::new(());

    std::thread::scope(|s| {
        for tid in 0..4u64 {
            let env_t = Arc::clone(&env);
            let tmp_t = Arc::clone(&tmp_fs);
            let mp_t = Arc::clone(&mnt_mp);
            let mnt_l = &mnt_lock;
            s.spawn(move || {
                let mut rng = 0x9e37_79b9_7f4a_7c15u64 ^ ((tid + 1).wrapping_mul(0xbf58_476d_1ce4_e5b9));
                let mut next_u64 = || -> u64 {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    rng
                };

                let mut held_guards: Vec<OpenGuard> = Vec::new();

                for step in 0..1500usize {
                    let op = next_u64() % 10;
                    match op {
                        0..=2 => {
                            // open_inode with various O_CREAT / O_TRUNC / O_EXCL / O_NOFOLLOW combinations
                            let p = paths[(next_u64() as usize) % paths.len()];
                            let raw_flags = match next_u64() % 4 {
                                0 => OpenFlags::O_CREAT | OpenFlags::O_RDWR,
                                1 => OpenFlags::O_CREAT | OpenFlags::O_RDWR | OpenFlags::O_TRUNC,
                                2 => OpenFlags::O_CREAT | OpenFlags::O_EXCL | OpenFlags::O_RDWR,
                                _ => OpenFlags::O_RDONLY | OpenFlags::O_NOFOLLOW,
                            };
                            if let Ok((inode, _)) = open_inode(env_t.as_ref(), None, p, OpenFlags(raw_flags), 0o644, 0)
                            {
                                if (step % 7 == 0) && held_guards.len() < 4 {
                                    held_guards.push(OpenGuard::new_unchecked(inode));
                                } else {
                                    let _ = inode.write_at(0, b"data");
                                }
                            }
                        }
                        3 => {
                            let p = dir_paths[(next_u64() as usize) % dir_paths.len()];
                            let _ = mkdir_at(env_t.as_ref(), None, p, 0o755, 0);
                        }
                        4 => {
                            let p = paths[(next_u64() as usize) % paths.len()];
                            let _ = unlink_at(env_t.as_ref(), None, p, false);
                        }
                        5 => {
                            let p = dir_paths[(next_u64() as usize) % dir_paths.len()];
                            let _ = unlink_at(env_t.as_ref(), None, p, true);
                        }
                        6 => {
                            let src = file_paths[(next_u64() as usize) % file_paths.len()];
                            let dst = file_paths[(next_u64() as usize) % file_paths.len()];
                            let _ = link_at(env_t.as_ref(), None, src, None, dst, false, false);
                        }
                        7 => {
                            let linkp = sym_paths[(next_u64() as usize) % sym_paths.len()];
                            let tgt = file_paths[(next_u64() as usize) % file_paths.len()];
                            let _ = symlink_at(env_t.as_ref(), tgt, None, linkp);
                        }
                        8 => {
                            let src = paths[(next_u64() as usize) % paths.len()];
                            let dst = paths[(next_u64() as usize) % paths.len()];
                            let rflags = match next_u64() % 3 {
                                0 => RenameFlags(0),
                                1 => RenameFlags(RenameFlags::NOREPLACE),
                                _ => RenameFlags(RenameFlags::EXCHANGE),
                            };
                            let _ = rename_at2(env_t.as_ref(), None, src, None, dst, rflags);
                        }
                        _ => {
                            if (next_u64() % 8) == 0 {
                                // Exercise concurrent mount/umount on `/mnt` alongside resolution.
                                if let Ok(_g) = mnt_l.try_lock() {
                                    if env_t.is_dev_mounted(DevId::TMPFS) {
                                        let _ = env_t.umount(mp_t.dev_id(), mp_t.ino());
                                    } else {
                                        let _ = env_t.mount(
                                            Arc::clone(&mp_t),
                                            "/mnt",
                                            Arc::clone(&tmp_t) as Arc<dyn FileSystem>,
                                        );
                                    }
                                }
                            } else {
                                let p = paths[(next_u64() as usize) % paths.len()];
                                let _ = resolve_path(
                                    env_t.as_ref(),
                                    None,
                                    p,
                                    ResolveFlags {
                                        follow_final: (next_u64() % 2) == 0,
                                        empty_path: false,
                                        require_dir: false,
                                    },
                                );
                            }
                        }
                    }
                    if (step % 19) == 0 && !held_guards.is_empty() {
                        held_guards.pop();
                    }
                }
                held_guards.clear();
            });
        }
    });

    // At quiescence: check InodeCache invariants, 100% Arc::ptr_eq live tree unification, and 100% DentryCache match.
    assert_test_lock9_free();
    for cached in env.snapshot_inodes() {
        assert!(
            !cached.is_detached(),
            "rename_lock={use_rename_lock} hist={hist}: detached inode in InodeCache: {cached:?}"
        );
        assert_eq!(
            cached.ino() & DETACHED_INO_BIT,
            0,
            "rename_lock={use_rename_lock} hist={hist}: DETACHED_INO_BIT in InodeCache: {cached:?}"
        );
        assert!(
            env.is_dev_mounted(cached.dev_id()),
            "rename_lock={use_rename_lock} hist={hist}: unmounted FS inode in InodeCache: {cached:?}"
        );
        let meta = cached.stat().expect("cached inode must stat cleanly");
        assert!(
            meta.nlink > 0,
            "rename_lock={use_rename_lock} hist={hist}: nlink == 0 zombie in InodeCache: {cached:?}"
        );
    }

    // Walk entire live tree from `/` and verify 100% Arc::ptr_eq unification and collect live `(pdev, pino, name) -> (dev, ino)`.
    let mut live_edges: BTreeMap<(DevId, Ino, String), DentryTarget> = BTreeMap::new();
    let mut queue: Vec<String> = vec![String::from("/")];
    while let Some(dir_path) = queue.pop() {
        let r1 = resolve_path(env.as_ref(), None, dir_path.as_bytes(), ResolveFlags::default()).unwrap().inode;
        let r2 = resolve_path(env.as_ref(), None, dir_path.as_bytes(), ResolveFlags::default()).unwrap().inode;
        assert!(Arc::ptr_eq(&r1, &r2), "split Arc for live dir {dir_path}");
        assert!(Arc::ptr_eq(&env.inode_get(r1.dev_id(), r1.ino()).unwrap(), &r1));

        let mut cursor = 0u64;
        let mut entries = Vec::new();
        loop {
            entries.clear();
            r1.readdir(cursor, &mut entries, 64).unwrap();
            if entries.is_empty() {
                break;
            }
            let mut next_cursor = cursor;
            for e in &entries {
                next_cursor = e.next_cursor;
                if e.name == "." || e.name == ".." {
                    continue;
                }
                let child_path =
                    if dir_path == "/" { format!("/{}", e.name) } else { format!("{}/{}", dir_path, e.name) };
                let c1 =
                    resolve_path(env.as_ref(), None, child_path.as_bytes(), ResolveFlags::default()).unwrap().inode;
                let c2 =
                    resolve_path(env.as_ref(), None, child_path.as_bytes(), ResolveFlags::default()).unwrap().inode;
                assert!(Arc::ptr_eq(&c1, &c2), "split Arc for live path {child_path}");
                assert!(Arc::ptr_eq(&env.inode_get(c1.dev_id(), c1.ino()).unwrap(), &c1));

                // Direct physical child on `r1` (before mount crossing, for DentryCache match).
                let raw_child = r1.lookup(&e.name).unwrap();
                live_edges.insert(
                    (r1.dev_id(), r1.ino(), e.name.clone()),
                    DentryTarget { dev: raw_child.dev_id(), ino: raw_child.ino() },
                );
                if c1.inode_type() == InodeType::Dir {
                    queue.push(child_path);
                }
            }
            if next_cursor == cursor {
                break;
            }
            cursor = next_cursor;
        }
    }

    for (pdev, pino, name, target) in env.snapshot_dentries() {
        assert_eq!(target.ino & DETACHED_INO_BIT, 0, "DETACHED_INO_BIT in DentryCache: {name}");
        let live_target = live_edges.get(&(pdev, pino, name.clone())).copied();
        assert_eq!(live_target, Some(target), "DentryCache mismatch for ({pdev:?}, {pino}, {name})");
    }

    // If `TMPFS` is mounted on `mnt_mp`, also walk `mnt_mp`'s shadowed `ROOT` subtree without crossing down.
    if env.is_dev_mounted(DevId::TMPFS) {
        let mut shadowed_queue: Vec<Arc<dyn VfsInode>> = vec![Arc::clone(&mnt_mp)];
        while let Some(dir_inode) = shadowed_queue.pop() {
            let mut cursor = 0u64;
            let mut entries = Vec::new();
            loop {
                entries.clear();
                dir_inode.readdir(cursor, &mut entries, 64).unwrap();
                if entries.is_empty() {
                    break;
                }
                let mut next_cursor = cursor;
                for e in &entries {
                    next_cursor = e.next_cursor;
                    if e.name == "." || e.name == ".." {
                        continue;
                    }
                    let c1 = resolve_path(env.as_ref(), Some(&dir_inode), e.name.as_bytes(), ResolveFlags::default())
                        .unwrap()
                        .inode;
                    let c2 = resolve_path(env.as_ref(), Some(&dir_inode), e.name.as_bytes(), ResolveFlags::default())
                        .unwrap()
                        .inode;
                    assert!(Arc::ptr_eq(&c1, &c2), "split Arc for shadowed path {}", e.name);
                    assert!(Arc::ptr_eq(&env.inode_get(c1.dev_id(), c1.ino()).unwrap(), &c1));

                    let raw_child = dir_inode.lookup(&e.name).unwrap();
                    live_edges.insert(
                        (dir_inode.dev_id(), dir_inode.ino(), e.name.clone()),
                        DentryTarget { dev: raw_child.dev_id(), ino: raw_child.ino() },
                    );
                    if c1.inode_type() == InodeType::Dir {
                        shadowed_queue.push(c1);
                    }
                }
                if next_cursor == cursor {
                    break;
                }
                cursor = next_cursor;
            }
        }
    }

    // Finally unmount `TMPFS` if currently mounted and confirm 0 `TMPFS` inodes remain.
    if env.is_dev_mounted(DevId::TMPFS) {
        env.umount(mnt_mp.dev_id(), mnt_mp.ino()).unwrap();
    }
    assert!(env.snapshot_inodes().into_iter().all(|i| i.dev_id() != DevId::TMPFS));
}

#[test]
fn unhooked_4_thread_concurrent_vfs_stress_with_ino_reuse() {
    for (use_rename_lock, hist, dentry_cap) in
        [(true, InodeCache::DEFAULT_REMOVAL_HISTORY, DentryCache::DEFAULT_CAPACITY), (true, 4, 16), (true, 0, 0)]
    {
        run_unhooked_4_thread_concurrent_vfs_stress(use_rename_lock, hist, dentry_cap);
    }
}

fn assert_no_zombies_in_cache(env: &SyncVfsTables) {
    for cached in env.snapshot_inodes() {
        assert!(!cached.is_detached(), "detached inode in InodeCache: {cached:?}");
        assert_eq!(cached.ino() & DETACHED_INO_BIT, 0, "DETACHED_INO_BIT in InodeCache: {cached:?}");
        let meta = cached.stat().expect("cached inode must stat cleanly");
        assert!(meta.nlink > 0, "nlink == 0 zombie inode in InodeCache: {cached:?}");
    }
}

#[test]
fn cross_fs_and_single_fs_overflow_never_skips_removal_or_leaves_zombie() {
    // Part 1: Cross-FS overflow (`PROBE 3I`, `3I-rmdir`, `3I-rename`) at `DEFAULT_REMOVAL_HISTORY = 4096`.
    let root_fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, true);
    let tmp_fs = FreshArcReusingFs::new(DevId::TMPFS, "fresh-tmpfs", false, true);
    let env = SyncVfsTables::new(Arc::clone(&root_fs) as Arc<dyn FileSystem>);

    let mnt_mp = mkdir_at(env.as_ref(), None, b"/mnt", 0o755, 0).unwrap();
    env.mount(mnt_mp, "/mnt", Arc::clone(&tmp_fs) as Arc<dyn FileSystem>).unwrap();

    let (victim, _) =
        open_inode(env.as_ref(), None, b"/victim", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    victim.write_at(0, b"VICTIM_4096").unwrap();
    let victim_ino = victim.ino();
    drop(victim);
    let vdir = mkdir_at(env.as_ref(), None, b"/vdir", 0o755, 0).unwrap();
    let vdir_ino = vdir.ino();
    drop(vdir);
    let (r_src, _) =
        open_inode(env.as_ref(), None, b"/r_src", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    drop(r_src);
    let (r_dst, _) =
        open_inode(env.as_ref(), None, b"/r_dst", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    r_dst.write_at(0, b"R_DST_OLD").unwrap();
    let r_dst_ino = r_dst.ino();
    drop(r_dst);

    assert!(env.inode_get(DevId::ROOT, victim_ino).is_some());
    assert!(env.inode_get(DevId::ROOT, vdir_ino).is_some());
    assert!(env.inode_get(DevId::ROOT, r_dst_ino).is_some());

    let create_tmp_batch = |prefix: &str| {
        for i in 0..4100usize {
            let p = format!("/mnt/{prefix}_{i}");
            open_inode(env.as_ref(), None, p.as_bytes(), OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
                .unwrap();
        }
    };

    // 1a. `PROBE 3I` (`unlink_at` file)
    create_tmp_batch("t");
    let env_u = Arc::clone(&env);
    root_fs.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from("victim"),
        alloc::boxed::Box::new(move || {
            for i in 0..4100usize {
                let p = format!("/mnt/t_{i}");
                unlink_at(env_u.as_ref(), None, p.as_bytes(), false).unwrap();
            }
        }),
    );
    unlink_at(env.as_ref(), None, b"/victim", false).unwrap();
    assert!(env.inode_get(DevId::ROOT, victim_ino).is_none());

    // 1b. `PROBE 3I-rmdir` (`unlink_at` dir)
    create_tmp_batch("d");
    let env_d = Arc::clone(&env);
    root_fs.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from("vdir"),
        alloc::boxed::Box::new(move || {
            for i in 0..4100usize {
                let p = format!("/mnt/d_{i}");
                unlink_at(env_d.as_ref(), None, p.as_bytes(), false).unwrap();
            }
        }),
    );
    unlink_at(env.as_ref(), None, b"/vdir", true).unwrap();
    assert!(env.inode_get(DevId::ROOT, vdir_ino).is_none());

    // 1c. `PROBE 3I-rename` (overwriting `rename_at2`)
    create_tmp_batch("r");
    let env_r = Arc::clone(&env);
    root_fs.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from("r_dst"),
        alloc::boxed::Box::new(move || {
            for i in 0..4100usize {
                let p = format!("/mnt/r_{i}");
                unlink_at(env_r.as_ref(), None, p.as_bytes(), false).unwrap();
            }
        }),
    );
    rename_at2(env.as_ref(), None, b"/r_src", None, b"/r_dst", RenameFlags(0)).unwrap();
    assert!(env.inode_get(DevId::ROOT, r_dst_ino).is_none());
    assert_no_zombies_in_cache(env.as_ref());

    // LIFO freed ino order: `r_dst_ino`, then `vdir_ino`, then `victim_ino`.
    // Create `/r_src2` (reuses `r_dst_ino`), `/fresh_dir_ino` (reuses `vdir_ino`), and `/fresh` (reuses `victim_ino`).
    let (r_src2, _) =
        open_inode(env.as_ref(), None, b"/r_src2", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    assert_eq!(r_src2.ino(), r_dst_ino);
    assert_eq!(r_src2.stat().unwrap().nlink, 1);

    let (fresh_from_vdir, _) =
        open_inode(env.as_ref(), None, b"/fresh_vdir", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    assert_eq!(fresh_from_vdir.ino(), vdir_ino);
    assert_eq!(fresh_from_vdir.inode_type(), InodeType::File);
    rename_at2(env.as_ref(), None, b"/fresh_vdir", None, b"/r_dst2", RenameFlags(0)).unwrap();

    let (fresh, _) =
        open_inode(env.as_ref(), None, b"/fresh", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    assert_eq!(fresh.ino(), victim_ino);
    assert_eq!(fresh.stat().unwrap().nlink, 1);
    let mut buf = [0u8; 32];
    assert_eq!(fresh.read_at(0, &mut buf).unwrap(), 0);
    let fresh_res = resolve_path(env.as_ref(), None, b"/fresh", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&fresh, &fresh_res));

    // Part 2a: Single-FS overflow on `DevId::ROOT` (`with_removal_history(4)`), pre-cached victim.
    let fs2 = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, true);
    let env2 = SyncVfsTables::with_removal_history(Arc::clone(&fs2) as Arc<dyn FileSystem>, 4, 64);
    let (s_pre, _) =
        open_inode(env2.as_ref(), None, b"/s_pre", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let s_pre_ino = s_pre.ino();
    drop(s_pre);
    let mut other_inos = Vec::new();
    for i in 0..5usize {
        let p = format!("/s_other_{i}");
        let (f, _) =
            open_inode(env2.as_ref(), None, p.as_bytes(), OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
                .unwrap();
        other_inos.push(f.ino());
    }
    let env2_h = Arc::clone(&env2);
    let fs2_h = Arc::clone(&fs2);
    fs2.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from("s_pre"),
        alloc::boxed::Box::new(move || {
            for (i, ino) in other_inos.into_iter().enumerate() {
                let name = format!("s_other_{i}");
                fs2_h.root_inode().unlink(&name).unwrap();
                let _ = env2_h.inode_remove(DevId::ROOT, ino);
            }
        }),
    );
    unlink_at(env2.as_ref(), None, b"/s_pre", false).unwrap();
    assert!(env2.inode_get(DevId::ROOT, s_pre_ino).is_none());
    assert_no_zombies_in_cache(env2.as_ref());

    // Part 2b: Single-FS overflow (`hist = 0`) with concurrent post-`since_gen` insertion before `begin_removal`.
    let fs3 = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, true);
    let env3 = SyncVfsTables::with_removal_history(Arc::clone(&fs3) as Arc<dyn FileSystem>, 0, 0);
    let (s_post, _) =
        open_inode(env3.as_ref(), None, b"/s_post", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let s_post_ino = s_post.ino();
    drop(s_post);
    let (s_unrel, _) =
        open_inode(env3.as_ref(), None, b"/s_unrel", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let s_unrel_ino = s_unrel.ino();
    drop(s_unrel);
    // Evict `s_post` from `InodeCache` so it is uncached at `since_gen`.
    let _ = env3.inode_remove(DevId::ROOT, s_post_ino);
    let env3_h = Arc::clone(&env3);
    let fs3_h = Arc::clone(&fs3);
    fs3.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from("s_post"),
        alloc::boxed::Box::new(move || {
            // Re-insert `s_post` into `InodeCache` AFTER `since_gen` and bump `dev_overflow_gen[ROOT]` via unrelated unlink.
            let _ = resolve_path(env3_h.as_ref(), None, b"/s_post", ResolveFlags::default()).unwrap();
            fs3_h.root_inode().unlink("s_unrel").unwrap();
            let _ = env3_h.inode_remove(DevId::ROOT, s_unrel_ino);
        }),
    );
    unlink_at(env3.as_ref(), None, b"/s_post", false).unwrap();
    assert!(env3.inode_get(DevId::ROOT, s_post_ino).is_none());
    assert_no_zombies_in_cache(env3.as_ref());
}

#[test]
fn publish_created_inode_retry_respects_pending_removals_and_o_excl_race() {
    use std::sync::Barrier;

    // 1. Reviewer 10 Finding 1 across `open_inode(O_CREAT)`, `mkdir_at`, and `symlink_at`,
    //    both while `pending_removals` remains active for all retries AND when `unlink`+`guard.finish()` completes mid-retry.
    //    Hooks synchronize with Thread B's `unlink_at` under `create`/`mkdir`/`symlink`, so `use_rename_lock = false`.
    for finish_mid_retry in [false, true] {
        // 1a. `open_inode(O_CREAT)` vs `unlink_at`
        {
            let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, false);
            let env = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);
            let b_created = Arc::new(Barrier::new(2));
            let b_in_guard = Arc::new(Barrier::new(2));
            let b_in_lookup = Arc::new(Barrier::new(2));
            let b_release = Arc::new(Barrier::new(2));

            let b1 = Arc::clone(&b_created);
            let b2 = Arc::clone(&b_in_guard);
            fs.inner.hooks.lock().unwrap().post_create.insert(
                String::from("f_creat"),
                alloc::boxed::Box::new(move || {
                    b1.wait();
                    b2.wait();
                }),
            );
            let b2_u = Arc::clone(&b_in_guard);
            let b_lu = Arc::clone(&b_in_lookup);
            let b3_u = Arc::clone(&b_release);
            let fs_u = Arc::clone(&fs);
            fs.inner.hooks.lock().unwrap().pre_unlink.insert(
                String::from("f_creat"),
                alloc::boxed::Box::new(move || {
                    if finish_mid_retry {
                        let b_lu_c = Arc::clone(&b_lu);
                        let b3_lookup = Arc::clone(&b3_u);
                        fs_u.inner.hooks.lock().unwrap().pre_lookup.insert(
                            String::from("f_creat"),
                            alloc::boxed::Box::new(move || {
                                b_lu_c.wait();
                                b3_lookup.wait();
                            }),
                        );
                        b2_u.wait();
                        b_lu.wait();
                    } else {
                        b2_u.wait();
                        b3_u.wait();
                    }
                }),
            );

            let created_ino = std::thread::scope(|s| {
                let env_b = Arc::clone(&env);
                let b1_b = Arc::clone(&b_created);
                let b3_b = Arc::clone(&b_release);
                let t_b = s.spawn(move || {
                    b1_b.wait();
                    unlink_at(env_b.as_ref(), None, b"/f_creat", false).unwrap();
                    if finish_mid_retry {
                        b3_b.wait();
                    }
                });

                let (created, _) = open_inode(
                    env.as_ref(),
                    None,
                    b"/f_creat",
                    OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                    0o644,
                    0,
                )
                .unwrap();
                let ino = created.ino();
                if !finish_mid_retry {
                    b_release.wait();
                }
                t_b.join().unwrap();
                drop(created);
                ino
            });

            assert!(env.inode_get(DevId::ROOT, created_ino).is_none());
            assert_no_zombies_in_cache(env.as_ref());
            let (reused, _) = open_inode(
                env.as_ref(),
                None,
                b"/f_reused",
                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                0o644,
                0,
            )
            .unwrap();
            assert_eq!(reused.ino(), created_ino);
            assert_eq!(reused.stat().unwrap().nlink, 1);
            assert!(Arc::ptr_eq(
                &reused,
                &resolve_path(env.as_ref(), None, b"/f_reused", ResolveFlags::default()).unwrap().inode
            ));
        }

        // 1b. `mkdir_at` vs `unlink_at(..., true)` (`pre_rmdir`)
        {
            let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, false);
            let env = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);
            let b_created = Arc::new(Barrier::new(2));
            let b_in_guard = Arc::new(Barrier::new(2));
            let b_in_lookup = Arc::new(Barrier::new(2));
            let b_release = Arc::new(Barrier::new(2));

            let b1 = Arc::clone(&b_created);
            let b2 = Arc::clone(&b_in_guard);
            fs.inner.hooks.lock().unwrap().post_mkdir.insert(
                String::from("d_mk"),
                alloc::boxed::Box::new(move || {
                    b1.wait();
                    b2.wait();
                }),
            );
            let b2_u = Arc::clone(&b_in_guard);
            let b_lu = Arc::clone(&b_in_lookup);
            let b3_u = Arc::clone(&b_release);
            let fs_u = Arc::clone(&fs);
            fs.inner.hooks.lock().unwrap().pre_rmdir.insert(
                String::from("d_mk"),
                alloc::boxed::Box::new(move || {
                    if finish_mid_retry {
                        let b_lu_c = Arc::clone(&b_lu);
                        let b3_lookup = Arc::clone(&b3_u);
                        fs_u.inner.hooks.lock().unwrap().pre_lookup.insert(
                            String::from("d_mk"),
                            alloc::boxed::Box::new(move || {
                                b_lu_c.wait();
                                b3_lookup.wait();
                            }),
                        );
                        b2_u.wait();
                        b_lu.wait();
                    } else {
                        b2_u.wait();
                        b3_u.wait();
                    }
                }),
            );

            let created_ino = std::thread::scope(|s| {
                let env_b = Arc::clone(&env);
                let b1_b = Arc::clone(&b_created);
                let b3_b = Arc::clone(&b_release);
                let t_b = s.spawn(move || {
                    b1_b.wait();
                    unlink_at(env_b.as_ref(), None, b"/d_mk", true).unwrap();
                    if finish_mid_retry {
                        b3_b.wait();
                    }
                });

                let dir = mkdir_at(env.as_ref(), None, b"/d_mk", 0o755, 0).unwrap();
                let ino = dir.ino();
                if !finish_mid_retry {
                    b_release.wait();
                }
                t_b.join().unwrap();
                drop(dir);
                ino
            });

            assert!(env.inode_get(DevId::ROOT, created_ino).is_none());
            assert_no_zombies_in_cache(env.as_ref());
            let (reused, _) = open_inode(
                env.as_ref(),
                None,
                b"/d_reused",
                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                0o644,
                0,
            )
            .unwrap();
            assert_eq!(reused.ino(), created_ino);
            assert_eq!(reused.inode_type(), InodeType::File);
            assert_eq!(reused.stat().unwrap().nlink, 1);
        }

        // 1c. `symlink_at` vs `unlink_at(..., false)`
        {
            let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, false);
            let env = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);
            let b_created = Arc::new(Barrier::new(2));
            let b_in_guard = Arc::new(Barrier::new(2));
            let b_in_lookup = Arc::new(Barrier::new(2));
            let b_release = Arc::new(Barrier::new(2));

            let b1 = Arc::clone(&b_created);
            let b2 = Arc::clone(&b_in_guard);
            fs.inner.hooks.lock().unwrap().post_symlink.insert(
                String::from("s_sym"),
                alloc::boxed::Box::new(move || {
                    b1.wait();
                    b2.wait();
                }),
            );
            let b2_u = Arc::clone(&b_in_guard);
            let b_lu = Arc::clone(&b_in_lookup);
            let b3_u = Arc::clone(&b_release);
            let fs_u = Arc::clone(&fs);
            fs.inner.hooks.lock().unwrap().pre_unlink.insert(
                String::from("s_sym"),
                alloc::boxed::Box::new(move || {
                    if finish_mid_retry {
                        let b_lu_c = Arc::clone(&b_lu);
                        let b3_lookup = Arc::clone(&b3_u);
                        fs_u.inner.hooks.lock().unwrap().pre_lookup.insert(
                            String::from("s_sym"),
                            alloc::boxed::Box::new(move || {
                                b_lu_c.wait();
                                b3_lookup.wait();
                            }),
                        );
                        b2_u.wait();
                        b_lu.wait();
                    } else {
                        b2_u.wait();
                        b3_u.wait();
                    }
                }),
            );

            let created_ino = std::thread::scope(|s| {
                let env_b = Arc::clone(&env);
                let b1_b = Arc::clone(&b_created);
                let b3_b = Arc::clone(&b_release);
                let t_b = s.spawn(move || {
                    b1_b.wait();
                    unlink_at(env_b.as_ref(), None, b"/s_sym", false).unwrap();
                    if finish_mid_retry {
                        b3_b.wait();
                    }
                });

                let sym = symlink_at(env.as_ref(), b"/target", None, b"/s_sym").unwrap();
                let ino = sym.ino();
                if !finish_mid_retry {
                    b_release.wait();
                }
                t_b.join().unwrap();
                drop(sym);
                ino
            });

            assert!(env.inode_get(DevId::ROOT, created_ino).is_none());
            assert_no_zombies_in_cache(env.as_ref());
            let (reused, _) = open_inode(
                env.as_ref(),
                None,
                b"/s_reused",
                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                0o644,
                0,
            )
            .unwrap();
            assert_eq!(reused.ino(), created_ino);
            assert_eq!(reused.inode_type(), InodeType::File);
            assert_eq!(reused.stat().unwrap().nlink, 1);
        }
    }

    // 2. `PROBE 3F` & Codex P1: `O_CREAT | O_EXCL` post-create unlink + recreate same name reusing `ino`.
    //    `post_create` calls `unlink_at` + `open_inode` on the same `fs`, so `use_rename_lock = false`.
    let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, false);
    let env = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);
    mkdir_at(env.as_ref(), None, b"/a", 0o755, 0).unwrap();

    let env_h = Arc::clone(&env);
    let other_holder: Arc<Mutex<Option<Arc<dyn VfsInode>>>> = Arc::new(Mutex::new(None));
    let other_holder_c = Arc::clone(&other_holder);
    fs.inner.hooks.lock().unwrap().post_create.insert(
        String::from("excl_f"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_h.as_ref(), None, b"/a/excl_f", false).unwrap();
            let (other, _) = open_inode(
                env_h.as_ref(),
                None,
                b"/a/excl_f",
                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_EXCL | OpenFlags::O_RDWR),
                0o644,
                0,
            )
            .unwrap();
            other.write_at(0, b"OTHER_SECRET").unwrap();
            *other_holder_c.lock().unwrap() = Some(other);
        }),
    );

    let (mine, _) = open_inode(
        env.as_ref(),
        None,
        b"/a/excl_f",
        OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_EXCL | OpenFlags::O_RDWR),
        0o600,
        0,
    )
    .unwrap();
    let other = other_holder.lock().unwrap().take().unwrap();
    assert_eq!(mine.ino(), other.ino());
    assert!(!Arc::ptr_eq(&mine, &other));
    assert_eq!(mine.stat().unwrap().nlink, 0);
    let mut buf = [0u8; 32];
    assert_eq!(mine.read_at(0, &mut buf).unwrap(), 0);
    let resolved = resolve_path(env.as_ref(), None, b"/a/excl_f", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&resolved, &other));
    assert_no_zombies_in_cache(env.as_ref());
}

#[test]
fn noop_rename_lock_hardlink_unlink_and_rename_races_evict_zombies() {
    // Part 1: `PROBE 1D-1` (`use_rename_lock = false`, `open_count == 0`, `/hl/a` & `/hl/b` cached).
    let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, false);
    let env = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);
    mkdir_at(env.as_ref(), None, b"/hl", 0o755, 0).unwrap();

    let (orig, _) =
        open_inode(env.as_ref(), None, b"/hl/a", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    let orig_ino = orig.ino();
    orig.write_at(0, b"STALE_HL_DATA").unwrap();
    drop(orig);
    link_at(env.as_ref(), None, b"/hl/a", None, b"/hl/b", false, false).unwrap();
    assert!(env.inode_get(DevId::ROOT, orig_ino).is_some());

    // Simulate concurrent `unlink_at("/hl/a")` leaving `nlink == 1`, then `unlink_at("/hl/b")` hitting
    // `nlink 1 -> 0` and reusing `orig_ino` right inside `post_unlink("b")` before `evict_zombie_inode`.
    unlink_at(env.as_ref(), None, b"/hl/a", false).unwrap();
    let env_h = Arc::clone(&env);
    let new_holder: Arc<Mutex<Option<Arc<dyn VfsInode>>>> = Arc::new(Mutex::new(None));
    let new_holder_c = Arc::clone(&new_holder);
    fs.inner.hooks.lock().unwrap().post_unlink.insert(
        String::from("b"),
        alloc::boxed::Box::new(move || {
            let (new_f, _) = open_inode(
                env_h.as_ref(),
                None,
                b"/hl/new",
                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                0o644,
                0,
            )
            .unwrap();
            new_f.write_at(0, b"FRESH_DATALINK_DATA").unwrap();
            *new_holder_c.lock().unwrap() = Some(new_f);
        }),
    );
    unlink_at(env.as_ref(), None, b"/hl/b", false).unwrap();
    let new_f = new_holder.lock().unwrap().take().unwrap();
    assert_eq!(new_f.ino(), orig_ino);
    assert_eq!(new_f.stat().unwrap().nlink, 1);
    let mut buf = [0u8; 32];
    let n = new_f.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"FRESH_DATALINK_DATA");
    let resolved_new = resolve_path(env.as_ref(), None, b"/hl/new", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&new_f, &resolved_new));
    assert_no_zombies_in_cache(env.as_ref());

    // Repeat `PROBE 1D-1` for `unlink_at("/hl2/a")` vs overwriting `rename_at2("/src", "/hl2/b")`.
    mkdir_at(env.as_ref(), None, b"/hl2", 0o755, 0).unwrap();
    let (orig2, _) =
        open_inode(env.as_ref(), None, b"/hl2/a", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    let orig2_ino = orig2.ino();
    orig2.write_at(0, b"STALE_REN_HL").unwrap();
    drop(orig2);
    link_at(env.as_ref(), None, b"/hl2/a", None, b"/hl2/b", false, false).unwrap();
    let (src, _) =
        open_inode(env.as_ref(), None, b"/src", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    src.write_at(0, b"SRC_PAYLOAD").unwrap();
    drop(src);

    // Pause `rename_at2` after `last_link_info` observed `nlink == 2` (`post_lookup("b")` unlinks `/hl2/a` -> `nlink == 1`),
    // then `rename` drops `nlink 1 -> 0` and `post_rename("b")` reuses `orig2_ino` before `evict_zombie_inode`.
    let env_u = Arc::clone(&env);
    fs.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from("b"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_u.as_ref(), None, b"/hl2/a", false).unwrap();
        }),
    );
    let env_r = Arc::clone(&env);
    let ren_new_holder: Arc<Mutex<Option<Arc<dyn VfsInode>>>> = Arc::new(Mutex::new(None));
    let ren_new_holder_c = Arc::clone(&ren_new_holder);
    fs.inner.hooks.lock().unwrap().post_rename.insert(
        String::from("b"),
        alloc::boxed::Box::new(move || {
            let (new_f2, _) = open_inode(
                env_r.as_ref(),
                None,
                b"/hl2/new2",
                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                0o644,
                0,
            )
            .unwrap();
            new_f2.write_at(0, b"FRESH_REN_DATALINK").unwrap();
            *ren_new_holder_c.lock().unwrap() = Some(new_f2);
        }),
    );
    rename_at2(env.as_ref(), None, b"/src", None, b"/hl2/b", RenameFlags(0)).unwrap();
    let new_f2 = ren_new_holder.lock().unwrap().take().unwrap();
    assert_eq!(new_f2.ino(), orig2_ino);
    assert_eq!(new_f2.stat().unwrap().nlink, 1);
    let n2 = new_f2.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf[..n2], b"FRESH_REN_DATALINK");
    let resolved_new2 = resolve_path(env.as_ref(), None, b"/hl2/new2", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&new_f2, &resolved_new2));
    assert_no_zombies_in_cache(env.as_ref());

    // Part 2: `PROBE 1D-2` (`use_rename_lock = false`, `(ROOT, orig3_ino)` evicted from `InodeCache`).
    mkdir_at(env.as_ref(), None, b"/hl3", 0o755, 0).unwrap();
    let (orig3, _) =
        open_inode(env.as_ref(), None, b"/hl3/a", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0).unwrap();
    let orig3_ino = orig3.ino();
    orig3.write_at(0, b"PRE_UNLINK_1D2").unwrap();
    drop(orig3);
    link_at(env.as_ref(), None, b"/hl3/a", None, b"/hl3/b", false, false).unwrap();

    // Evict `(ROOT, orig3_ino)` and invalidate dentry `"a"` so `resolve_path("/hl3/a")` goes to `dir.lookup("a")`.
    let _ = env.inode_remove(DevId::ROOT, orig3_ino);
    let hl3_dir = resolve_path(env.as_ref(), None, b"/hl3", ResolveFlags::default()).unwrap().inode;
    env.dentry_invalidate(DevId::ROOT, hl3_dir.ino(), "a");

    let env_1d2 = Arc::clone(&env);
    fs.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from("a"),
        alloc::boxed::Box::new(move || {
            unlink_at(env_1d2.as_ref(), None, b"/hl3/a", false).unwrap();
            unlink_at(env_1d2.as_ref(), None, b"/hl3/b", false).unwrap();
        }),
    );
    let pre_un = resolve_path(env.as_ref(), None, b"/hl3/a", ResolveFlags::default()).unwrap().inode;
    assert_eq!(pre_un.ino(), orig3_ino);
    assert_eq!(pre_un.stat().unwrap().nlink, 0);
    assert!(env.inode_get(DevId::ROOT, orig3_ino).is_none());
    assert_no_zombies_in_cache(env.as_ref());
}

#[test]
fn overflow_and_magic_link_race_retries_preserve_unification() {
    // 1. `resolve_global_root` retry under `InodeCache::with_removal_history(2)` overflow.
    struct HookRootFs {
        inner: Arc<FreshArcReusingFs>,
        on_root: Mutex<Option<FsOpHook>>,
    }
    impl FileSystem for HookRootFs {
        fn dev_id(&self) -> DevId {
            self.inner.dev_id()
        }
        fn name(&self) -> &'static str {
            self.inner.name()
        }
        fn root_inode(&self) -> Arc<dyn VfsInode> {
            let hook = self.on_root.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
            self.inner.root_inode()
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

    let raw_root_fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, true);
    let hook_root_fs = Arc::new(HookRootFs { inner: Arc::clone(&raw_root_fs), on_root: Mutex::new(None) });
    let env = SyncVfsTables::with_removal_history(Arc::clone(&hook_root_fs) as Arc<dyn FileSystem>, 2, 64);
    for i in 0..3usize {
        let p = format!("/r_unrel_{i}");
        open_inode(env.as_ref(), None, p.as_bytes(), OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    }
    let _ = env.inode_remove(DevId::ROOT, 1);
    let env_r = Arc::clone(&env);
    *hook_root_fs.on_root.lock().unwrap() = Some(alloc::boxed::Box::new(move || {
        for i in 0..3usize {
            let p = format!("/r_unrel_{i}");
            unlink_at(env_r.as_ref(), None, p.as_bytes(), false).unwrap();
        }
    }));
    let r1 = resolve_path(env.as_ref(), None, b"/", ResolveFlags::default()).unwrap().inode;
    let r2 = resolve_path(env.as_ref(), None, b"/", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&r1, &r2));
    assert!(Arc::ptr_eq(&env.inode_get(DevId::ROOT, 1).unwrap(), &r1));

    // 2. `lookup_dotdot` retry under `hist = 2` overflow.
    mkdir_at(env.as_ref(), None, b"/sub", 0o755, 0).unwrap();
    let sub = resolve_path(env.as_ref(), None, b"/sub", ResolveFlags::default()).unwrap().inode;
    for i in 0..3usize {
        let p = format!("/dd_unrel_{i}");
        open_inode(env.as_ref(), None, p.as_bytes(), OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    }
    let env_dd = Arc::clone(&env);
    raw_root_fs.inner.hooks.lock().unwrap().pre_lookup.insert(
        String::from(".."),
        alloc::boxed::Box::new(move || {
            for i in 0..3usize {
                let p = format!("/dd_unrel_{i}");
                unlink_at(env_dd.as_ref(), None, p.as_bytes(), false).unwrap();
            }
        }),
    );
    let dd = resolve_path(env.as_ref(), Some(&sub), b"..", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&dd, &r1));

    // 3. `lookup_and_unify_inode` retry under `hist = 2` overflow (`rename_at2` unifies with live cached `Arc`).
    let (ren_src, _) =
        open_inode(env.as_ref(), None, b"/sub/ren_src", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let ren_src_ino = ren_src.ino();
    drop(ren_src);
    let _ = env.inode_remove(DevId::ROOT, ren_src_ino);
    env.dentry_invalidate(DevId::ROOT, sub.ino(), "ren_src");
    let mut lu_inos = Vec::new();
    for i in 0..3usize {
        let p = format!("/lu_unrel_{i}");
        let (f, _) =
            open_inode(env.as_ref(), None, p.as_bytes(), OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
                .unwrap();
        lu_inos.push(f.ino());
    }
    let live_holder: Arc<Mutex<Option<Arc<dyn VfsInode>>>> = Arc::new(Mutex::new(None));
    let live_holder_c = Arc::clone(&live_holder);
    let env_lu = Arc::clone(&env);
    let raw_lu = Arc::clone(&raw_root_fs);
    raw_root_fs.inner.hooks.lock().unwrap().pre_lookup.insert(
        String::from("ren_src"),
        alloc::boxed::Box::new(move || {
            let live = resolve_path(env_lu.as_ref(), None, b"/sub/ren_src", ResolveFlags::default()).unwrap().inode;
            *live_holder_c.lock().unwrap() = Some(live);
            for (i, ino) in lu_inos.into_iter().enumerate() {
                let name = format!("lu_unrel_{i}");
                raw_lu.root_inode().unlink(&name).unwrap();
                let _ = env_lu.inode_remove(DevId::ROOT, ino);
            }
        }),
    );
    rename_at2(env.as_ref(), None, b"/sub/ren_src", None, b"/sub/ren_dst", RenameFlags(0)).unwrap();
    let live_arc = live_holder.lock().unwrap().take().unwrap();
    let dst_arc = resolve_path(env.as_ref(), None, b"/sub/ren_dst", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&dst_arc, &live_arc));

    // 4. `resolve_and_unify_magic_target` under overflow + target unlink+reuse *during* `resolve_magic_link()` + `PROBE 3B`.
    type MagicResolveCb = alloc::boxed::Box<dyn FnMut() -> Option<Arc<dyn VfsInode>> + Send>;
    struct HookMagicInode {
        inner: Arc<dyn VfsInode>,
        on_resolve: Mutex<Option<MagicResolveCb>>,
    }
    impl VfsInode for HookMagicInode {
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
        fn resolve_magic_link(&self) -> Option<Arc<dyn VfsInode>> {
            self.on_resolve.lock().unwrap().as_mut().and_then(|cb| cb())
        }
        fn stat(&self) -> Result<Metadata, VfsError> {
            self.inner.stat()
        }
        fn lookup(&self, name: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.inner.lookup(name)
        }
        fn create(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.inner.create(name, mode)
        }
        fn mkdir(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.inner.mkdir(name, mode)
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
            self.inner.symlink(name, target)
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
    struct MagicDirFs {
        dev: DevId,
        root: Arc<dyn VfsInode>,
    }
    struct MagicRootDir {
        inner: Arc<dyn VfsInode>,
        children: Mutex<BTreeMap<String, Arc<dyn VfsInode>>>,
    }
    impl VfsInode for MagicRootDir {
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
            self.children.lock().unwrap().get(name).cloned().ok_or(VfsError::NoEnt)
        }
        fn create(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.inner.create(name, mode)
        }
        fn mkdir(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.inner.mkdir(name, mode)
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
            self.inner.symlink(name, target)
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
    impl FileSystem for MagicDirFs {
        fn dev_id(&self) -> DevId {
            self.dev
        }
        fn name(&self) -> &'static str {
            "procfs"
        }
        fn root_inode(&self) -> Arc<dyn VfsInode> {
            Arc::clone(&self.root)
        }
        fn sync(&self) -> Result<(), VfsError> {
            Ok(())
        }
        fn read_only(&self) -> bool {
            true
        }
    }

    let proc_backing = MemFs::new(DevId::PROCFS, "procfs", true);
    let proc_mp = mkdir_at(env.as_ref(), None, b"/proc", 0o755, 0).unwrap();
    let proc_root = Arc::new(MagicRootDir { inner: proc_backing.root_inode(), children: Mutex::new(BTreeMap::new()) });
    let proc_fs = Arc::new(MagicDirFs { dev: DevId::PROCFS, root: Arc::clone(&proc_root) as Arc<dyn VfsInode> });
    env.mount(proc_mp, "/proc", proc_fs).unwrap();

    // 4a. Magic link overflow on first `resolve_magic_link` call retries and unifies.
    let (m_target, _) =
        open_inode(env.as_ref(), None, b"/m_target", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let m_target_ino = m_target.ino();
    drop(m_target);
    let _ = env.inode_remove(DevId::ROOT, m_target_ino);
    for i in 0..3usize {
        let p = format!("/mg_unrel_{i}");
        open_inode(env.as_ref(), None, p.as_bytes(), OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    }
    let env_m = Arc::clone(&env);
    let raw_r = Arc::clone(&raw_root_fs);
    let mut overflow_calls = 0usize;
    let m_sym1 = proc_backing.root_inode().symlink("m_over", "/target").unwrap();
    let m_link_overflow = Arc::new(HookMagicInode {
        inner: m_sym1,
        on_resolve: Mutex::new(Some(alloc::boxed::Box::new(move || {
            overflow_calls += 1;
            if overflow_calls == 1 {
                for i in 0..3usize {
                    let p = format!("/mg_unrel_{i}");
                    unlink_at(env_m.as_ref(), None, p.as_bytes(), false).unwrap();
                }
            }
            raw_r.root_inode().lookup("m_target").ok()
        }))),
    });
    proc_root.children.lock().unwrap().insert(String::from("m_over"), m_link_overflow);
    let (via_open, _) =
        open_inode(env.as_ref(), None, b"/proc/m_over", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDONLY), 0o644, 0)
            .unwrap();
    let via_resolve = resolve_path(
        env.as_ref(),
        None,
        b"/proc/m_over",
        ResolveFlags { follow_final: true, ..ResolveFlags::default() },
    )
    .unwrap()
    .inode;
    assert!(Arc::ptr_eq(&via_open, &via_resolve));
    assert!(Arc::ptr_eq(&env.inode_get(DevId::ROOT, m_target_ino).unwrap(), &via_open));

    // 4b. Target unlinked + `ino` reused *during* `resolve_magic_link()` (proves `cache_gens()` captured BEFORE `resolve_magic_link()`).
    let (old_mt, _) =
        open_inode(env.as_ref(), None, b"/old_mt", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let old_mt_ino = old_mt.ino();
    old_mt.write_at(0, b"OLD_MT").unwrap();
    drop(old_mt);
    let _ = env.inode_remove(DevId::ROOT, old_mt_ino);

    let env_m2 = Arc::clone(&env);
    let raw_r2 = Arc::clone(&raw_root_fs);
    let new_mt_holder: Arc<Mutex<Option<Arc<dyn VfsInode>>>> = Arc::new(Mutex::new(None));
    let new_mt_holder_c = Arc::clone(&new_mt_holder);
    let mut reuse_calls = 0usize;
    let m_sym2 = proc_backing.root_inode().symlink("m_reuse", "/target").unwrap();
    let m_link_reuse = Arc::new(HookMagicInode {
        inner: m_sym2,
        on_resolve: Mutex::new(Some(alloc::boxed::Box::new(move || {
            reuse_calls += 1;
            if reuse_calls == 1 {
                let stale_old = raw_r2.root_inode().lookup("old_mt").unwrap();
                unlink_at(env_m2.as_ref(), None, b"/old_mt", false).unwrap();
                let (new_mt, _) = open_inode(
                    env_m2.as_ref(),
                    None,
                    b"/new_mt",
                    OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                    0o644,
                    0,
                )
                .unwrap();
                new_mt.write_at(0, b"NEW_MT").unwrap();
                *new_mt_holder_c.lock().unwrap() = Some(new_mt);
                Some(stale_old)
            } else {
                None
            }
        }))),
    });
    proc_root.children.lock().unwrap().insert(String::from("m_reuse"), m_link_reuse);
    let got_reuse = resolve_path(
        env.as_ref(),
        None,
        b"/proc/m_reuse",
        ResolveFlags { follow_final: true, ..ResolveFlags::default() },
    )
    .unwrap()
    .inode;
    let new_mt = new_mt_holder.lock().unwrap().take().unwrap();
    assert_eq!(got_reuse.ino(), old_mt_ino);
    assert_eq!(new_mt.ino(), old_mt_ino);
    assert!(!Arc::ptr_eq(&got_reuse, &new_mt));
    let mut buf = [0u8; 16];
    let n = got_reuse.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"OLD_MT");
    assert!(Arc::ptr_eq(&env.inode_get(DevId::ROOT, old_mt_ino).unwrap(), &new_mt));

    // 4c. `PROBE 3B`: detached magic link target exits in 1 step without entering `InodeCache`.
    let (det_mt, _) =
        open_inode(env.as_ref(), None, b"/det_mt", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let det_guard = OpenGuard::new_unchecked(Arc::clone(&det_mt));
    unlink_at(env.as_ref(), None, b"/det_mt", false).unwrap();
    assert!(det_mt.is_detached());
    let det_calls = Arc::new(core::sync::atomic::AtomicUsize::new(0));
    let det_calls_c = Arc::clone(&det_calls);
    let det_clone = Arc::clone(&det_mt);
    let m_sym3 = proc_backing.root_inode().symlink("m_det", "/target").unwrap();
    let m_link_det = Arc::new(HookMagicInode {
        inner: m_sym3,
        on_resolve: Mutex::new(Some(alloc::boxed::Box::new(move || {
            det_calls_c.fetch_add(1, Ordering::AcqRel);
            Some(Arc::clone(&det_clone))
        }))),
    });
    proc_root.children.lock().unwrap().insert(String::from("m_det"), m_link_det);
    let got_det = resolve_path(
        env.as_ref(),
        None,
        b"/proc/m_det",
        ResolveFlags { follow_final: true, ..ResolveFlags::default() },
    )
    .unwrap()
    .inode;
    assert_eq!(det_calls.load(Ordering::Acquire), 1);
    assert!(got_det.is_detached());
    assert!(Arc::ptr_eq(&got_det, &det_mt));
    drop(det_guard);
    assert_no_zombies_in_cache(env.as_ref());
}

#[test]
fn rename_at2_vs_creat_mkdir_symlink_serialized_by_rename_lock_zero_zombies() {
    use std::sync::Condvar;
    use std::time::Duration;

    fn wait_signal(sig: &(Mutex<bool>, Condvar)) {
        let mut ready = sig.0.lock().unwrap();
        while !*ready {
            ready = sig.1.wait(ready).unwrap();
        }
    }

    fn send_signal(sig: &(Mutex<bool>, Condvar)) {
        let mut ready = sig.0.lock().unwrap();
        *ready = true;
        sig.1.notify_all();
    }

    enum OpKind {
        OpenCreat,
        Mkdir,
        Symlink,
    }

    // 1. Forward direction: `rename_at2("/src", "/dst")` holds Lock 3 across `post_lookup("dst")`,
    //    serializing concurrent `open_inode(O_CREAT)`, `mkdir_at`, and `symlink_at` on `/dst`.
    for hold_open in [false, true] {
        for kind in [OpKind::OpenCreat, OpKind::Mkdir, OpKind::Symlink] {
            let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, true);
            let env = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);

            let (src, _) =
                open_inode(env.as_ref(), None, b"/src", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
                    .unwrap();
            let src_ino = src.ino();
            let _guard = if hold_open { Some(OpenGuard::new_unchecked(src)) } else { None };

            let sig = Arc::new((Mutex::new(false), Condvar::new()));
            let sig_hook = Arc::clone(&sig);
            fs.inner.hooks.lock().unwrap().post_lookup.insert(
                String::from("dst"),
                alloc::boxed::Box::new(move || {
                    send_signal(&sig_hook);
                    std::thread::sleep(Duration::from_millis(5));
                }),
            );

            std::thread::scope(|s| {
                let env_b = Arc::clone(&env);
                let sig_b = Arc::clone(&sig);
                let t_b = s.spawn(move || {
                    wait_signal(&sig_b);
                    match kind {
                        OpKind::OpenCreat => {
                            let (got, _) = open_inode(
                                env_b.as_ref(),
                                None,
                                b"/dst",
                                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                                0o644,
                                0,
                            )
                            .unwrap();
                            assert_eq!(got.ino(), src_ino);
                        }
                        OpKind::Mkdir => {
                            assert_eq!(mkdir_at(env_b.as_ref(), None, b"/dst", 0o755, 0).unwrap_err(), VfsError::Exist);
                        }
                        OpKind::Symlink => {
                            assert_eq!(symlink_at(env_b.as_ref(), b"/t", None, b"/dst").unwrap_err(), VfsError::Exist);
                        }
                    }
                });

                rename_at2(env.as_ref(), None, b"/src", None, b"/dst", RenameFlags(0)).unwrap();
                t_b.join().unwrap();
            });

            assert_no_zombies_in_cache(env.as_ref());
            let resolved = resolve_path(env.as_ref(), None, b"/dst", ResolveFlags::default()).unwrap().inode;
            assert_eq!(resolved.ino(), src_ino);
        }
    }

    // 2. Reverse direction: `open_inode(O_CREAT)`, `mkdir_at`, and `symlink_at` hold Lock 3 across
    //    `post_create`/`post_mkdir`/`post_symlink`, serializing concurrent `rename_at2("/src", "/dst")`
    //    under both `RenameFlags(0)` and `RenameFlags::NOREPLACE`.
    for hold_open in [false, true] {
        for rflags in [RenameFlags(0), RenameFlags(RenameFlags::NOREPLACE)] {
            for kind in [OpKind::OpenCreat, OpKind::Mkdir, OpKind::Symlink] {
                let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, true);
                let env = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);

                let (src, _) = open_inode(
                    env.as_ref(),
                    None,
                    b"/src",
                    OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                    0o644,
                    0,
                )
                .unwrap();
                let src_ino = src.ino();
                drop(src);

                let sig = Arc::new((Mutex::new(false), Condvar::new()));
                let sig_hook = Arc::clone(&sig);
                let cb: FsOpHook = alloc::boxed::Box::new(move || {
                    send_signal(&sig_hook);
                    std::thread::sleep(Duration::from_millis(5));
                });
                {
                    let mut hooks = fs.inner.hooks.lock().unwrap();
                    match kind {
                        OpKind::OpenCreat => {
                            hooks.post_create.insert(String::from("dst"), cb);
                        }
                        OpKind::Mkdir => {
                            hooks.post_mkdir.insert(String::from("dst"), cb);
                        }
                        OpKind::Symlink => {
                            hooks.post_symlink.insert(String::from("dst"), cb);
                        }
                    }
                }

                std::thread::scope(|s| {
                    let env_b = Arc::clone(&env);
                    let sig_b = Arc::clone(&sig);
                    let t_b = s.spawn(move || {
                        wait_signal(&sig_b);
                        rename_at2(env_b.as_ref(), None, b"/src", None, b"/dst", rflags)
                    });

                    let created = match kind {
                        OpKind::OpenCreat => {
                            open_inode(
                                env.as_ref(),
                                None,
                                b"/dst",
                                OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
                                0o644,
                                0,
                            )
                            .unwrap()
                            .0
                        }
                        OpKind::Mkdir => mkdir_at(env.as_ref(), None, b"/dst", 0o755, 0).unwrap(),
                        OpKind::Symlink => symlink_at(env.as_ref(), b"/t", None, b"/dst").unwrap(),
                    };
                    let created_ino = created.ino();
                    let _guard = if hold_open { Some(OpenGuard::new_unchecked(created)) } else { None };
                    let ren_res = t_b.join().unwrap();

                    if rflags.has_noreplace() {
                        assert_eq!(ren_res.unwrap_err(), VfsError::Exist);
                        let dst = resolve_path(env.as_ref(), None, b"/dst", ResolveFlags::default()).unwrap().inode;
                        assert_eq!(dst.ino(), created_ino);
                    } else if matches!(kind, OpKind::Mkdir) {
                        // Non-dir `/src` cannot overwrite directory `/dst`.
                        assert_eq!(ren_res.unwrap_err(), VfsError::IsDir);
                    } else {
                        ren_res.unwrap();
                        let dst = resolve_path(env.as_ref(), None, b"/dst", ResolveFlags::default()).unwrap().inode;
                        assert_eq!(dst.ino(), src_ino);
                    }
                });

                assert_no_zombies_in_cache(env.as_ref());
            }
        }
    }

    // 3. Cross-FS symlink hop in `open_inode(O_CREAT)` drops `ROOT`'s Lock 3 before crossing to `TMPFS`.
    let root_fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, true);
    let tmp_fs = FreshArcReusingFs::new(DevId::TMPFS, "fresh-tmpfs", false, true);
    let env = SyncVfsTables::new(Arc::clone(&root_fs) as Arc<dyn FileSystem>);
    let mp = mkdir_at(env.as_ref(), None, b"/mnt", 0o755, 0).unwrap();
    env.mount(mp, "/mnt", Arc::clone(&tmp_fs) as Arc<dyn FileSystem>).unwrap();
    symlink_at(env.as_ref(), b"/mnt/file", None, b"/link_to_tmp").unwrap();

    let (f, _) =
        open_inode(env.as_ref(), None, b"/link_to_tmp", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    assert_eq!(f.dev_id(), DevId::TMPFS);
    let f_res = resolve_path(env.as_ref(), None, b"/mnt/file", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&f, &f_res));
    assert_no_zombies_in_cache(env.as_ref());
}

#[test]
fn publish_created_inode_retry_cache_gens_window_and_evict_zombie_atomicity() {
    use std::sync::Barrier;

    type CacheHook = alloc::boxed::Box<dyn FnOnce() + Send>;

    struct HookCacheEnv {
        inner: Arc<SyncVfsTables>,
        on_cache_gens: Mutex<Option<CacheHook>>,
        on_before_record_if_absent: Mutex<Option<CacheHook>>,
        on_after_record_if_absent: Mutex<Option<CacheHook>>,
        on_after_publish_lookup: Mutex<Option<CacheHook>>,
    }

    impl HookCacheEnv {
        fn new(inner: Arc<SyncVfsTables>) -> Arc<Self> {
            Arc::new(Self {
                inner,
                on_cache_gens: Mutex::new(None),
                on_before_record_if_absent: Mutex::new(None),
                on_after_record_if_absent: Mutex::new(None),
                on_after_publish_lookup: Mutex::new(None),
            })
        }
    }

    impl VfsCacheOps for HookCacheEnv {
        fn root_inode(&self) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.inner.root_inode()
        }
        fn is_global_root(&self, dev: DevId, ino: Ino) -> bool {
            self.inner.is_global_root(dev, ino)
        }
        fn mount_at(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn FileSystem>> {
            self.inner.mount_at(dev, ino)
        }
        fn mount_at_with_id(&self, dev: DevId, ino: Ino) -> Option<(Arc<dyn FileSystem>, u64)> {
            self.inner.mount_at_with_id(dev, ino)
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
        fn cache_gens(&self) -> (u64, u64) {
            let hook = self.on_cache_gens.lock().unwrap().take();
            if let Some(h) = hook {
                h();
            }
            self.inner.cache_gens()
        }
        fn dentry_lookup_inode(&self, pdev: DevId, pino: Ino, name: &str) -> (Option<Arc<dyn VfsInode>>, u64, u64) {
            self.inner.dentry_lookup_inode(pdev, pino, name)
        }
        fn publish_lookup_inode(
            &self,
            pdev: DevId,
            pino: Ino,
            name: &str,
            inode: Arc<dyn VfsInode>,
            expected_dentry_gen: u64,
            expected_inode_gen: u64,
        ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
            let res = self.inner.publish_lookup_inode(pdev, pino, name, inode, expected_dentry_gen, expected_inode_gen);
            let hook = self.on_after_publish_lookup.lock().unwrap().take();
            if let Some(h) = hook {
                h();
            }
            res
        }
        fn publish_created_inode(
            &self,
            pdev: DevId,
            pino: Ino,
            name: &str,
            inode: Arc<dyn VfsInode>,
            expected_dentry_gen: u64,
            expected_inode_gen: u64,
        ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
            self.inner.publish_created_inode(pdev, pino, name, inode, expected_dentry_gen, expected_inode_gen)
        }
        fn dentry_gen(&self) -> u64 {
            self.inner.dentry_gen()
        }
        fn dentry_get(&self, pdev: DevId, pino: Ino, name: &str) -> Option<DentryTarget> {
            self.inner.dentry_get(pdev, pino, name)
        }
        fn dentry_get_with_gen(&self, pdev: DevId, pino: Ino, name: &str) -> (Option<DentryTarget>, u64) {
            self.inner.dentry_get_with_gen(pdev, pino, name)
        }
        fn dentry_insert(&self, pdev: DevId, pino: Ino, name: &str, target: DentryTarget) {
            self.inner.dentry_insert(pdev, pino, name, target)
        }
        fn dentry_insert_if_gen(
            &self,
            pdev: DevId,
            pino: Ino,
            name: &str,
            target: DentryTarget,
            expected_gen: u64,
        ) -> bool {
            self.inner.dentry_insert_if_gen(pdev, pino, name, target, expected_gen)
        }
        fn dentry_invalidate(&self, pdev: DevId, pino: Ino, name: &str) {
            self.inner.dentry_invalidate(pdev, pino, name)
        }
        fn dentry_invalidate_dir(&self, pdev: DevId, pino: Ino) {
            self.inner.dentry_invalidate_dir(pdev, pino)
        }
        fn inode_get(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
            self.inner.inode_get(dev, ino)
        }
        fn inode_get_if_fresh(&self, dev: DevId, ino: Ino, since_gen: u64) -> Option<Arc<dyn VfsInode>> {
            self.inner.inode_get_if_fresh(dev, ino, since_gen)
        }
        fn inode_get_or_insert(&self, inode: Arc<dyn VfsInode>) -> Arc<dyn VfsInode> {
            self.inner.inode_get_or_insert(inode)
        }
        fn inode_get_or_insert_if_mounted(
            &self,
            mp_dev: DevId,
            mp_ino: Ino,
            mount_id: u64,
            inode: Arc<dyn VfsInode>,
        ) -> Option<Arc<dyn VfsInode>> {
            self.inner.inode_get_or_insert_if_mounted(mp_dev, mp_ino, mount_id, inode)
        }
        fn inode_get_or_insert_if_fresh(
            &self,
            inode: Arc<dyn VfsInode>,
            since_gen: u64,
        ) -> Result<Arc<dyn VfsInode>, Arc<dyn VfsInode>> {
            self.inner.inode_get_or_insert_if_fresh(inode, since_gen)
        }
        fn inode_remove(&self, dev: DevId, ino: Ino) -> Option<Arc<dyn VfsInode>> {
            self.inner.inode_remove(dev, ino)
        }
        fn inode_remove_if_fresh(
            &self,
            dev: DevId,
            ino: Ino,
            since_gen: u64,
        ) -> Option<(Option<Arc<dyn VfsInode>>, u64)> {
            self.inner.inode_remove_if_fresh(dev, ino, since_gen)
        }
        fn inode_begin_removal(&self, dev: DevId, ino: Ino, since_gen: u64) -> (Option<Arc<dyn VfsInode>>, u64) {
            self.inner.inode_begin_removal(dev, ino, since_gen)
        }
        fn inode_end_removal(&self, dev: DevId, ino: Ino) -> u64 {
            self.inner.inode_end_removal(dev, ino)
        }
        fn inode_evict_detached(&self, dev: DevId, ino: Ino) -> bool {
            self.inner.inode_evict_detached(dev, ino)
        }
        fn inode_remove_if_same(
            &self,
            dev: DevId,
            ino: Ino,
            expected: &Arc<dyn VfsInode>,
        ) -> Option<Arc<dyn VfsInode>> {
            self.inner.inode_remove_if_same(dev, ino, expected)
        }
        fn inode_record_removal(&self, dev: DevId, ino: Ino) -> u64 {
            self.inner.inode_record_removal(dev, ino)
        }
        fn inode_record_removal_if_absent(&self, dev: DevId, ino: Ino) -> Result<u64, Arc<dyn VfsInode>> {
            let pre = self.on_before_record_if_absent.lock().unwrap().take();
            if let Some(h) = pre {
                h();
            }
            let res = self.inner.inode_record_removal_if_absent(dev, ino);
            let post = self.on_after_record_if_absent.lock().unwrap().take();
            if let Some(h) = post {
                h();
            }
            res
        }
    }

    // 1. `reviewer_11` Finding 2 (`ops.rs:118-135`): `O_CREAT | O_EXCL` preempted right before
    //    retry-loop `env.cache_gens()` must NOT adopt replacement `other`.
    {
        let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, false);
        let inner = SyncVfsTables::with_removal_history(Arc::clone(&fs) as Arc<dyn FileSystem>, 0, 64);
        let env = HookCacheEnv::new(Arc::clone(&inner));
        mkdir_at(inner.as_ref(), None, b"/a", 0o755, 0).unwrap();
        open_inode(inner.as_ref(), None, b"/a/unrel", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();

        let other_holder: Arc<Mutex<Option<Arc<dyn VfsInode>>>> = Arc::new(Mutex::new(None));
        let other_holder_c = Arc::clone(&other_holder);
        let env_c = Arc::clone(&env);
        let inner_c = Arc::clone(&inner);
        fs.inner.hooks.lock().unwrap().post_create.insert(
            String::from("excl_w"),
            alloc::boxed::Box::new(move || {
                unlink_at(inner_c.as_ref(), None, b"/a/unrel", false).unwrap();
                let inner_g = Arc::clone(&inner_c);
                let holder_g = Arc::clone(&other_holder_c);
                *env_c.on_cache_gens.lock().unwrap() = Some(alloc::boxed::Box::new(move || {
                    unlink_at(inner_g.as_ref(), None, b"/a/excl_w", false).unwrap();
                    let (other, _) = open_inode(
                        inner_g.as_ref(),
                        None,
                        b"/a/excl_w",
                        OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_EXCL | OpenFlags::O_RDWR),
                        0o644,
                        0,
                    )
                    .unwrap();
                    other.write_at(0, b"OTHER_SECRET").unwrap();
                    *holder_g.lock().unwrap() = Some(other);
                }));
            }),
        );

        let (mine, _) = open_inode(
            env.as_ref(),
            None,
            b"/a/excl_w",
            OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_EXCL | OpenFlags::O_RDWR),
            0o600,
            0,
        )
        .unwrap();
        let other = other_holder.lock().unwrap().take().unwrap();
        assert_eq!(mine.ino(), other.ino());
        assert!(!Arc::ptr_eq(&mine, &other));
        assert_eq!(mine.stat().unwrap().nlink, 0);
        let mut buf = [0u8; 32];
        assert_eq!(mine.read_at(0, &mut buf).unwrap(), 0);
        let resolved = resolve_path(inner.as_ref(), None, b"/a/excl_w", ResolveFlags::default()).unwrap().inode;
        assert!(Arc::ptr_eq(&resolved, &other));
        let n = resolved.read_at(0, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"OTHER_SECRET");
        assert_no_zombies_in_cache(&inner);
    }

    // 2. `PROBE 1D-4` (`evict_zombie_inode` vs `lookup_component` atomicity, `use_rename_lock = false`).
    for publish_before_record in [true, false] {
        let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, false);
        let inner = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);
        let env = HookCacheEnv::new(Arc::clone(&inner));
        let hl_dir = mkdir_at(inner.as_ref(), None, b"/hl", 0o755, 0).unwrap();

        let (orig, _) =
            open_inode(inner.as_ref(), None, b"/hl/a", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
                .unwrap();
        let orig_ino = orig.ino();
        drop(orig);
        link_at(inner.as_ref(), None, b"/hl/a", None, b"/hl/b", false, false).unwrap();

        // Evict `(ROOT, orig_ino)` and dentries `"a"`/`"b"` so both `resolve_path("/hl/a")` and `unlink_at("/hl/b")` hit disk lookup.
        let _ = inner.inode_remove(DevId::ROOT, orig_ino);
        inner.dentry_invalidate(DevId::ROOT, hl_dir.ino(), "a");
        inner.dentry_invalidate(DevId::ROOT, hl_dir.ino(), "b");

        let b_looked_up = Arc::new(Barrier::new(2));
        let b_proceed_publish = Arc::new(Barrier::new(2));
        let b_published = Arc::new(Barrier::new(2));

        let b_lu_b = Arc::clone(&b_looked_up);
        let b_pp_b = Arc::clone(&b_proceed_publish);
        fs.inner.hooks.lock().unwrap().post_lookup.insert(
            String::from("a"),
            alloc::boxed::Box::new(move || {
                b_lu_b.wait();
                b_pp_b.wait();
            }),
        );

        let b_lu_a = Arc::clone(&b_looked_up);
        let inner_a = Arc::clone(&inner);
        fs.inner.hooks.lock().unwrap().pre_unlink.insert(
            String::from("b"),
            alloc::boxed::Box::new(move || {
                b_lu_a.wait();
                unlink_at(inner_a.as_ref(), None, b"/hl/a", false).unwrap();
            }),
        );

        if publish_before_record {
            let b_pp_a = Arc::clone(&b_proceed_publish);
            let b_pub_a = Arc::clone(&b_published);
            *env.on_before_record_if_absent.lock().unwrap() = Some(alloc::boxed::Box::new(move || {
                b_pp_a.wait();
                b_pub_a.wait();
            }));
            let b_pub_b = Arc::clone(&b_published);
            *env.on_after_publish_lookup.lock().unwrap() = Some(alloc::boxed::Box::new(move || {
                b_pub_b.wait();
            }));
        } else {
            let b_pp_a = Arc::clone(&b_proceed_publish);
            *env.on_after_record_if_absent.lock().unwrap() = Some(alloc::boxed::Box::new(move || {
                b_pp_a.wait();
            }));
        }

        std::thread::scope(|s| {
            let env_b = Arc::clone(&env);
            let t_b =
                s.spawn(move || resolve_path(env_b.as_ref(), None, b"/hl/a", ResolveFlags::default()).unwrap().inode);
            unlink_at(env.as_ref(), None, b"/hl/b", false).unwrap();
            let pre_un = t_b.join().unwrap();
            assert_eq!(pre_un.ino(), orig_ino);
            assert_eq!(pre_un.stat().unwrap().nlink, 0);
        });

        assert!(
            inner.inode_get(DevId::ROOT, orig_ino).is_none(),
            "publish_before_record={publish_before_record}: InodeCache retained nlink==0 zombie!"
        );
        assert_no_zombies_in_cache(&inner);

        let (reused, _) = open_inode(
            inner.as_ref(),
            None,
            b"/hl/reused",
            OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
            0o644,
            0,
        )
        .unwrap();
        assert_eq!(reused.ino(), orig_ino);
        assert_eq!(reused.stat().unwrap().nlink, 1);
        let res_reused = resolve_path(inner.as_ref(), None, b"/hl/reused", ResolveFlags::default()).unwrap().inode;
        assert!(Arc::ptr_eq(&reused, &res_reused));
    }
}

#[test]
fn shadowed_mountpoint_never_pollutes_dentry_cache() {
    // Verify both `SyncVfsTables` and `SimpleVfsTables` skip `DentryCache` insertion
    // when `parent` is an active mountpoint, while still unifying via `InodeCache`.
    let root_fs = MemFs::new(DevId::ROOT, "ext4", false);
    let tmp_fs = MemFs::new(DevId::TMPFS, "tmpfs", false);
    let sync_env = SyncVfsTables::new(Arc::clone(&root_fs) as Arc<dyn FileSystem>);
    let mnt_mp = mkdir_at(sync_env.as_ref(), None, b"/mnt", 0o755, 0).unwrap();
    sync_env.mount(Arc::clone(&mnt_mp), "/mnt", Arc::clone(&tmp_fs) as Arc<dyn FileSystem>).unwrap();

    let (under_c, _) = open_inode(
        sync_env.as_ref(),
        Some(&mnt_mp),
        b"under",
        OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR),
        0o644,
        0,
    )
    .unwrap();
    let under_r = resolve_path(sync_env.as_ref(), Some(&mnt_mp), b"under", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&under_c, &under_r));
    assert!(Arc::ptr_eq(&sync_env.inode_get(under_c.dev_id(), under_c.ino()).unwrap(), &under_c));
    assert!(sync_env.dentry_get(mnt_mp.dev_id(), mnt_mp.ino(), "under").is_none());
    assert!(!sync_env.dentry_insert_if_gen(
        mnt_mp.dev_id(),
        mnt_mp.ino(),
        "under",
        DentryTarget { dev: under_c.dev_id(), ino: under_c.ino() },
        sync_env.dentry_gen(),
    ));
    assert!(sync_env.dentry_get(mnt_mp.dev_id(), mnt_mp.ino(), "under").is_none());

    let simple_env = SimpleVfsTables::new();
    let root_fs2 = MemFs::new(DevId::ROOT, "ext4", false);
    let tmp_fs2 = MemFs::new(DevId::TMPFS, "tmpfs", false);
    simple_env.mount_root(root_fs2).unwrap();
    let mp2 = mkdir_at(&simple_env, None, b"/mnt", 0o755, 0).unwrap();
    simple_env.mount(Arc::clone(&mp2), "/mnt", tmp_fs2).unwrap();

    let (u2_c, _) =
        open_inode(&simple_env, Some(&mp2), b"under", OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR), 0o644, 0)
            .unwrap();
    let u2_r = resolve_path(&simple_env, Some(&mp2), b"under", ResolveFlags::default()).unwrap().inode;
    assert!(Arc::ptr_eq(&u2_c, &u2_r));
    assert!(simple_env.dentry_get(mp2.dev_id(), mp2.ino(), "under").is_none());
    assert!(!simple_env.dentry_insert_if_gen(
        mp2.dev_id(),
        mp2.ino(),
        "under",
        DentryTarget { dev: u2_c.dev_id(), ino: u2_c.ino() },
        simple_env.dentry_gen(),
    ));
}

#[test]
fn compute_cwd_path_retries_on_concurrent_rename_and_rejects_unlinked_cwd() {
    // 1. Direct `cwd` rename between `lookup_dotdot` and `find_child_name_in_dir` (`readdir`).
    let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, true);
    let env = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);

    mkdir_at(env.as_ref(), None, b"/p1", 0o755, 0).unwrap();
    mkdir_at(env.as_ref(), None, b"/p2", 0o755, 0).unwrap();
    let cwd = mkdir_at(env.as_ref(), None, b"/p1/leaf", 0o755, 0).unwrap();
    let guard = OpenGuard::new_unchecked(Arc::clone(&cwd));

    let env_h = Arc::clone(&env);
    fs.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from(".."),
        alloc::boxed::Box::new(move || {
            rename_at2(env_h.as_ref(), None, b"/p1/leaf", None, b"/p2/leaf", RenameFlags(0)).unwrap();
        }),
    );
    assert_eq!(compute_cwd_path(env.as_ref(), &cwd).unwrap(), "/p2/leaf");
    assert!(!fs.inner.hooks.lock().unwrap().post_lookup.contains_key(".."));

    // 2. Multi-hop ancestor rename after `components` already recorded `"deep"`,
    //    verifying `components` is cleared on retry without duplicating components.
    mkdir_at(env.as_ref(), None, b"/a1", 0o755, 0).unwrap();
    mkdir_at(env.as_ref(), None, b"/a2", 0o755, 0).unwrap();
    mkdir_at(env.as_ref(), None, b"/a1/sub", 0o755, 0).unwrap();
    let deep = mkdir_at(env.as_ref(), None, b"/a1/sub/deep", 0o755, 0).unwrap();
    let deep_guard = OpenGuard::new_unchecked(Arc::clone(&deep));

    let fs_h2 = Arc::clone(&fs);
    let env_h2 = Arc::clone(&env);
    fs.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from(".."),
        alloc::boxed::Box::new(move || {
            // Re-arm for the second `".."` lookup (`sub -> a1`), after `"deep"` is pushed.
            fs_h2.inner.hooks.lock().unwrap().post_lookup.insert(
                String::from(".."),
                alloc::boxed::Box::new(move || {
                    rename_at2(env_h2.as_ref(), None, b"/a1/sub", None, b"/a2/sub", RenameFlags(0)).unwrap();
                }),
            );
        }),
    );
    assert_eq!(compute_cwd_path(env.as_ref(), &deep).unwrap(), "/a2/sub/deep");
    assert!(!fs.inner.hooks.lock().unwrap().post_lookup.contains_key(".."));
    drop(deep_guard);

    // 3a. `cwd` unlinked mid-walk while open (`OpenGuard` held -> detached) returns `Err(NoEnt)`.
    let env_u1 = Arc::clone(&env);
    fs.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from(".."),
        alloc::boxed::Box::new(move || {
            unlink_at(env_u1.as_ref(), None, b"/p2/leaf", true).unwrap();
        }),
    );
    assert_eq!(compute_cwd_path(env.as_ref(), &cwd).unwrap_err(), VfsError::NoEnt);
    assert!(cwd.is_detached());
    assert_eq!(compute_cwd_path(env.as_ref(), &cwd).unwrap_err(), VfsError::NoEnt);
    drop(guard);

    // 3b. `cwd` unlinked mid-walk without `OpenGuard` (`nlink == 0`) also returns `Err(NoEnt)`.
    let unguarded = mkdir_at(env.as_ref(), None, b"/p1/unguarded", 0o755, 0).unwrap();
    let env_u2 = Arc::clone(&env);
    fs.inner.hooks.lock().unwrap().post_lookup.insert(
        String::from(".."),
        alloc::boxed::Box::new(move || {
            unlink_at(env_u2.as_ref(), None, b"/p1/unguarded", true).unwrap();
        }),
    );
    assert_eq!(compute_cwd_path(env.as_ref(), &unguarded).unwrap_err(), VfsError::NoEnt);
    assert_no_zombies_in_cache(env.as_ref());
}

#[test]
fn concurrent_compute_cwd_path_vs_rename_at2_stress() {
    use core::sync::atomic::AtomicUsize;
    use std::sync::Barrier;

    let fs = FreshArcReusingFs::new(DevId::ROOT, "fresh-ext4", false, true);
    let env = SyncVfsTables::new(Arc::clone(&fs) as Arc<dyn FileSystem>);

    mkdir_at(env.as_ref(), None, b"/p1", 0o755, 0).unwrap();
    mkdir_at(env.as_ref(), None, b"/p2", 0o755, 0).unwrap();
    let cwd = mkdir_at(env.as_ref(), None, b"/p1/leaf", 0o755, 0).unwrap();
    let _guard = OpenGuard::new_unchecked(Arc::clone(&cwd));

    const MIN_TOGGLES: usize = 1000;
    const READER_ITERS: usize = 1000;
    let start_barrier = Arc::new(Barrier::new(3));
    let readers_done = Arc::new(AtomicUsize::new(0));

    std::thread::scope(|s| {
        for _ in 0..2 {
            let env_r = Arc::clone(&env);
            let cwd_r = Arc::clone(&cwd);
            let barrier_r = Arc::clone(&start_barrier);
            let done_r = Arc::clone(&readers_done);
            s.spawn(move || {
                barrier_r.wait();
                for _ in 0..READER_ITERS {
                    let path = compute_cwd_path(env_r.as_ref(), &cwd_r).unwrap();
                    assert!(path == "/p1/leaf" || path == "/p2/leaf", "unexpected cwd path: {path}");
                }
                done_r.fetch_add(1, Ordering::Release);
            });
        }

        start_barrier.wait();
        let mut toggles = 0usize;
        while readers_done.load(Ordering::Acquire) < 2 || toggles < MIN_TOGGLES {
            rename_at2(env.as_ref(), None, b"/p1/leaf", None, b"/p2/leaf", RenameFlags(0)).unwrap();
            rename_at2(env.as_ref(), None, b"/p2/leaf", None, b"/p1/leaf", RenameFlags(0)).unwrap();
            toggles += 1;
        }
        assert!(toggles >= MIN_TOGGLES);
    });

    assert_eq!(compute_cwd_path(env.as_ref(), &cwd).unwrap(), "/p1/leaf");
    assert_no_zombies_in_cache(env.as_ref());
}

/// A `procfs`-like filesystem for `VfsInode::cacheable` (#263): `/<pid>`
/// exists while `pid` is in `live`, holding `fd/0` (a magic link to `info`,
/// there until its pid is in `closed_fds`) and `info`; all of those are
/// non-cacheable, `version` is cacheable. `/<pid>/ver` is a magic link to
/// `version`. `/<pid>/cached` breaks the rule that a non-cacheable
/// directory's children are non-cacheable.
mod uncached {
    use super::*;
    use alloc::collections::BTreeSet;

    /// What the filesystem's inodes share: which pids and fds exist, and
    /// how often the VFS asked.
    #[derive(Default)]
    pub(super) struct Shared {
        pub(super) live: Mutex<BTreeSet<u32>>,
        /// Pids whose `fd/0` is closed (it is open by default).
        pub(super) closed_fds: Mutex<BTreeSet<u32>>,
        /// Every `lookup` the filesystem answered.
        pub(super) lookups: AtomicU64,
        /// Every `resolve_magic_link` it answered.
        pub(super) magic_resolves: AtomicU64,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Node {
        Root,
        Version,
        Pid(u32),
        Fd(u32),
        Info(u32),
        FdLink(u32),
        VerLink(u32),
    }

    impl Node {
        fn ino(self) -> Ino {
            let per = |p: u32, tag: u64| (1u64 << 48) | (u64::from(p) << 16) | tag;
            match self {
                Self::Root => 1,
                Self::Version => 2,
                Self::Pid(p) => per(p, 0),
                Self::Fd(p) => per(p, 1),
                Self::Info(p) => per(p, 2),
                Self::FdLink(p) => per(p, 3),
                Self::VerLink(p) => per(p, 4),
            }
        }

        fn pid(self) -> Option<u32> {
            match self {
                Self::Root | Self::Version => None,
                Self::Pid(p) | Self::Fd(p) | Self::Info(p) | Self::FdLink(p) | Self::VerLink(p) => Some(p),
            }
        }

        fn inode_type(self) -> InodeType {
            match self {
                Self::Root | Self::Pid(_) | Self::Fd(_) => InodeType::Dir,
                Self::Version | Self::Info(_) => InodeType::File,
                Self::FdLink(_) | Self::VerLink(_) => InodeType::Symlink,
            }
        }
    }

    pub(super) struct DynFs {
        dev: DevId,
        pub(super) shared: Arc<Shared>,
        root: Arc<DynInode>,
    }

    impl DynFs {
        pub(super) fn new(dev: DevId) -> Arc<Self> {
            let shared = Arc::new(Shared::default());
            let root = DynInode::new(dev, Node::Root, Arc::clone(&shared));
            Arc::new(Self { dev, shared, root })
        }

        /// How often the VFS has asked: `(lookups, magic link resolutions)`.
        pub(super) fn asked(&self) -> (u64, u64) {
            (self.shared.lookups.load(Ordering::Relaxed), self.shared.magic_resolves.load(Ordering::Relaxed))
        }
    }

    impl FileSystem for DynFs {
        fn dev_id(&self) -> DevId {
            self.dev
        }
        fn name(&self) -> &'static str {
            "dynfs"
        }
        fn root_inode(&self) -> Arc<dyn VfsInode> {
            self.root.clone()
        }
        fn sync(&self) -> Result<(), VfsError> {
            Ok(())
        }
        fn read_only(&self) -> bool {
            true
        }
    }

    struct DynInode {
        lock_id: u64,
        dev: DevId,
        node: Node,
        shared: Arc<Shared>,
        open_count: AtomicU32,
        locks: Mutex<AdvisoryLocks>,
    }

    impl DynInode {
        fn new(dev: DevId, node: Node, shared: Arc<Shared>) -> Arc<Self> {
            Arc::new(Self {
                lock_id: next_lock_id(),
                dev,
                node,
                shared,
                open_count: AtomicU32::new(0),
                locks: Mutex::new(AdvisoryLocks::new()),
            })
        }

        fn spawn(&self, node: Node) -> Arc<dyn VfsInode> {
            Self::new(self.dev, node, Arc::clone(&self.shared))
        }

        fn alive(&self, pid: u32) -> bool {
            self.shared.live.lock().unwrap().contains(&pid)
        }

        fn fd_open(&self, pid: u32) -> bool {
            self.alive(pid) && !self.shared.closed_fds.lock().unwrap().contains(&pid)
        }

        fn is_live(&self) -> bool {
            match self.node {
                Node::FdLink(p) => self.fd_open(p),
                node => node.pid().is_none_or(|p| self.alive(p)),
            }
        }
    }

    impl VfsInode for DynInode {
        fn lock_id(&self) -> u64 {
            self.lock_id
        }
        fn dev_id(&self) -> DevId {
            self.dev
        }
        fn ino(&self) -> Ino {
            self.node.ino()
        }
        fn inode_type(&self) -> InodeType {
            self.node.inode_type()
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
        fn cacheable(&self) -> bool {
            self.node.pid().is_none()
        }
        fn resolve_magic_link(&self) -> Option<Arc<dyn VfsInode>> {
            self.shared.magic_resolves.fetch_add(1, Ordering::Relaxed);
            match self.node {
                Node::FdLink(p) if self.fd_open(p) => Some(self.spawn(Node::Info(p))),
                Node::VerLink(p) if self.alive(p) => Some(self.spawn(Node::Version)),
                _ => None,
            }
        }
        fn stat(&self) -> Result<Metadata, VfsError> {
            if !self.is_live() {
                return Err(VfsError::NoEnt);
            }
            let itype = self.inode_type();
            Ok(Metadata {
                dev: self.dev,
                ino: self.ino(),
                inode_type: itype,
                mode: 0o555,
                nlink: if itype == InodeType::Dir { 2 } else { 1 },
                uid: 0,
                gid: 0,
                rdev: DevId::default(),
                size: 0,
                blksize: 4096,
                blocks: 0,
                atime: Timespec::default(),
                mtime: Timespec::default(),
                ctime: Timespec::default(),
            })
        }
        fn lookup(&self, name: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
            self.shared.lookups.fetch_add(1, Ordering::Relaxed);
            if !self.is_live() {
                return Err(VfsError::NoEnt);
            }
            match (self.node, name) {
                (Node::Root, "version") => Ok(self.spawn(Node::Version)),
                (Node::Root, other) => {
                    let pid: u32 = other.parse().map_err(|_| VfsError::NoEnt)?;
                    if !self.alive(pid) {
                        return Err(VfsError::NoEnt);
                    }
                    Ok(self.spawn(Node::Pid(pid)))
                }
                (Node::Pid(_), "..") => Ok(self.spawn(Node::Root)),
                (Node::Pid(p), "fd") => Ok(self.spawn(Node::Fd(p))),
                (Node::Pid(p), "info") => Ok(self.spawn(Node::Info(p))),
                (Node::Pid(_), "cached") => Ok(self.spawn(Node::Version)),
                (Node::Pid(p), "ver") => Ok(self.spawn(Node::VerLink(p))),
                (Node::Fd(p), "..") => Ok(self.spawn(Node::Pid(p))),
                (Node::Fd(p), "0") if self.fd_open(p) => Ok(self.spawn(Node::FdLink(p))),
                (Node::Pid(_) | Node::Fd(_), _) => Err(VfsError::NoEnt),
                _ => Err(VfsError::NotDir),
            }
        }
        fn create(&self, _name: &str, _mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
            Err(VfsError::RoFs)
        }
        fn mkdir(&self, _name: &str, _mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
            Err(VfsError::RoFs)
        }
        fn unlink(&self, _name: &str) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn rmdir(&self, _name: &str) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn link(&self, _name: &str, _target: &Arc<dyn VfsInode>) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn symlink(&self, _name: &str, _target: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
            Err(VfsError::RoFs)
        }
        fn readlink(&self) -> Result<Vec<u8>, VfsError> {
            match self.node {
                Node::FdLink(_) if self.is_live() => Ok(b"../info".to_vec()),
                Node::VerLink(_) if self.is_live() => Ok(b"../version".to_vec()),
                Node::FdLink(_) | Node::VerLink(_) => Err(VfsError::NoEnt),
                _ => Err(VfsError::Inval),
            }
        }
        fn rename(
            &self,
            _old_name: &str,
            _new_dir: &Arc<dyn VfsInode>,
            _new_name: &str,
            _flags: RenameFlags,
        ) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn readdir(&self, _cursor: u64, _out: &mut Vec<DirEntry>, _max_entries: usize) -> Result<(), VfsError> {
            if self.inode_type() == InodeType::Dir { Ok(()) } else { Err(VfsError::NotDir) }
        }
        fn truncate(&self, _size: u64) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn set_mode(&self, _mode: u16) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn set_owner(&self, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn set_times(&self, _atime: SetTime, _mtime: SetTime) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, VfsError> {
            Ok(0)
        }
        fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, VfsError> {
            Err(VfsError::RoFs)
        }
        fn read_page(&self, _page_index: u64, _buf: &mut [u8; 4096]) -> Result<usize, VfsError> {
            Err(VfsError::Inval)
        }
        fn write_page(&self, _page_index: u64, _buf: &[u8]) -> Result<(), VfsError> {
            Err(VfsError::RoFs)
        }
        fn sync(&self, _data_only: bool) -> Result<(), VfsError> {
            Ok(())
        }
        fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
            f(&mut self.locks.lock().unwrap());
            Ok(())
        }
    }
}

const DYNFS_DEV: DevId = DevId::makedev(0, 77);

/// A root `MemFs` with a [`uncached::DynFs`] mounted at `/proc`.
fn setup_dynfs() -> (SimpleVfsTables, Arc<uncached::DynFs>, Arc<dyn VfsInode>) {
    let (env, _root_fs) = setup_vfs();
    let mp = mkdir_at(&env, None, b"/proc", 0o555, 0).unwrap();
    let dynfs = uncached::DynFs::new(DYNFS_DEV);
    env.mount(mp, "/proc", dynfs.clone()).unwrap();
    let proc_root = resolve_path(&env, None, b"/proc", ResolveFlags::default()).unwrap().inode;
    assert_eq!(proc_root.dev_id(), DYNFS_DEV);
    (env, dynfs, proc_root)
}

/// How many per-pid entries `InodeCache` and `DentryCache` hold.
fn dynfs_cached_per_pid(env: &SimpleVfsTables) -> (usize, usize) {
    let per_pid = |ino: Ino| ino & (1u64 << 48) != 0;
    let inodes =
        env.inodes.borrow().snapshot_all().iter().filter(|i| i.dev_id() == DYNFS_DEV && per_pid(i.ino())).count();
    let dentries = env
        .dentries
        .borrow()
        .snapshot_all()
        .iter()
        .filter(|(pdev, pino, _, target)| {
            (*pdev == DYNFS_DEV && per_pid(*pino)) || (target.dev == DYNFS_DEV && per_pid(target.ino))
        })
        .count();
    (inodes, dentries)
}

#[test]
fn non_cacheable_child_that_disappears_is_enoent_on_the_next_resolve() {
    let (env, dynfs, proc_root) = setup_dynfs();
    let nofollow = ResolveFlags { follow_final: false, ..ResolveFlags::default() };
    let follow = ResolveFlags { follow_final: true, ..ResolveFlags::default() };
    dynfs.shared.live.lock().unwrap().insert(7);

    // Each path takes one of the caches' doors: a plain lookup, `..` out of
    // a non-cacheable directory, and a magic link's target.
    let paths: [(&[u8], ResolveFlags); 5] = [
        (b"/proc/7", nofollow),
        (b"/proc/7/fd", nofollow),
        (b"/proc/7/fd/..", nofollow),
        (b"/proc/7/fd/0", nofollow),
        (b"/proc/7/fd/0", follow),
    ];
    for (path, flags) in paths {
        resolve_path(&env, None, path, flags).unwrap();
    }
    // A second resolve while it lives asks the filesystem again, exactly
    // once per non-cacheable step: a step the tables refused as stale
    // (`Err`) would be retried.
    let asks: [(u64, u64); 5] = [(1, 0), (2, 0), (3, 0), (3, 0), (3, 1)];
    for ((path, flags), want) in paths.into_iter().zip(asks) {
        let (lookups, magic) = dynfs.asked();
        resolve_path(&env, None, path, flags).unwrap();
        let (lookups_after, magic_after) = dynfs.asked();
        assert_eq!((lookups_after - lookups, magic_after - magic), want, "{}", String::from_utf8_lossy(path));
    }
    assert_eq!(resolve_path(&env, None, b"/proc/7", nofollow).unwrap().inode.ino(), (1u64 << 48) | (7 << 16));
    // The other insert paths hand a non-cacheable inode back uncached too.
    let pid_dir = proc_root.lookup("7").unwrap();
    let (dentry_gen, inode_gen) = env.cache_gens();
    let created =
        env.publish_created_inode(DYNFS_DEV, proc_root.ino(), "7", Arc::clone(&pid_dir), dentry_gen, inode_gen);
    assert!(Arc::ptr_eq(&created.unwrap(), &pid_dir));
    assert!(Arc::ptr_eq(&env.inode_get_or_insert(Arc::clone(&pid_dir)), &pid_dir));
    let mp = env.parent_mount(DYNFS_DEV).unwrap();
    let (_, mount_id) = env.mount_at_with_id(mp.dev_id(), mp.ino()).unwrap();
    let mounted = env.inode_get_or_insert_if_mounted(mp.dev_id(), mp.ino(), mount_id, Arc::clone(&pid_dir));
    assert!(Arc::ptr_eq(&mounted.unwrap(), &pid_dir));
    assert_eq!(env.dentry_get(DYNFS_DEV, proc_root.ino(), "7"), None);
    assert_eq!(dynfs_cached_per_pid(&env), (0, 0));

    // A cacheable sibling is still cached.
    let version = resolve_path(&env, None, b"/proc/version", nofollow).unwrap().inode;
    assert_eq!(env.dentry_get(DYNFS_DEV, proc_root.ino(), "version").map(|t| t.ino), Some(version.ino()));
    assert!(Arc::ptr_eq(&env.inode_get(DYNFS_DEV, version.ino()).unwrap(), &version));
    // A non-cacheable magic link still lands on its cacheable target's
    // cached inode, asking for the link once.
    let (lookups, magic) = dynfs.asked();
    let via_link = resolve_path(&env, None, b"/proc/7/ver", follow).unwrap().inode;
    let (lookups_after, magic_after) = dynfs.asked();
    assert!(Arc::ptr_eq(&via_link, &version));
    assert_eq!((lookups_after - lookups, magic_after - magic), (2, 1));

    dynfs.shared.live.lock().unwrap().remove(&7);
    for (path, flags) in paths {
        assert_eq!(
            resolve_path(&env, None, path, flags).unwrap_err(),
            VfsError::NoEnt,
            "{} after its pid went",
            String::from_utf8_lossy(path)
        );
    }
    assert_eq!(
        open_inode(&env, None, b"/proc/7/info", OpenFlags(OpenFlags::O_RDONLY), 0, 0).unwrap_err(),
        VfsError::NoEnt
    );
}

#[test]
fn non_cacheable_lookups_leave_the_caches_flat() {
    let (env, dynfs, _proc_root) = setup_dynfs();
    let nofollow = ResolveFlags { follow_final: false, ..ResolveFlags::default() };
    let follow = ResolveFlags { follow_final: true, ..ResolveFlags::default() };
    resolve_path(&env, None, b"/proc/version", nofollow).unwrap();
    let inodes_before = env.inodes.borrow().len();
    let dentries_before = env.dentries.borrow().len();

    for pid in 1..=2000u32 {
        dynfs.shared.live.lock().unwrap().insert(pid);
        for (path, flags) in [
            (format!("/proc/{pid}"), nofollow),
            (format!("/proc/{pid}/info"), nofollow),
            (format!("/proc/{pid}/fd/.."), nofollow),
            (format!("/proc/{pid}/fd/0"), nofollow),
            (format!("/proc/{pid}/fd/0"), follow),
        ] {
            resolve_path(&env, None, path.as_bytes(), flags).unwrap();
        }
        // `open(O_CREAT)` on an existing name publishes it too.
        let path = format!("/proc/{pid}/info");
        let (opened, _) =
            open_inode(&env, None, path.as_bytes(), OpenFlags(OpenFlags::O_RDONLY | OpenFlags::O_CREAT), 0o444, 0)
                .unwrap();
        assert!(!opened.cacheable());
        dynfs.shared.live.lock().unwrap().remove(&pid);
    }

    assert_eq!(dynfs_cached_per_pid(&env), (0, 0));
    assert_eq!(env.inodes.borrow().len(), inodes_before);
    assert_eq!(env.dentries.borrow().len(), dentries_before);
}

#[test]
fn non_cacheable_fd_link_is_enoent_once_its_fd_closes_while_its_process_lives() {
    let (env, dynfs, _proc_root) = setup_dynfs();
    let nofollow = ResolveFlags { follow_final: false, ..ResolveFlags::default() };
    let follow = ResolveFlags { follow_final: true, ..ResolveFlags::default() };
    dynfs.shared.live.lock().unwrap().insert(8);
    resolve_path(&env, None, b"/proc/8/fd/0", nofollow).unwrap();
    resolve_path(&env, None, b"/proc/8/fd/0", follow).unwrap();

    dynfs.shared.closed_fds.lock().unwrap().insert(8);
    for flags in [nofollow, follow] {
        assert_eq!(resolve_path(&env, None, b"/proc/8/fd/0", flags).unwrap_err(), VfsError::NoEnt);
    }
    // Only the fd went: its process and the rest of its directory are there.
    for path in [&b"/proc/8"[..], b"/proc/8/fd", b"/proc/8/info"] {
        resolve_path(&env, None, path, nofollow).unwrap();
    }
    assert_eq!(dynfs_cached_per_pid(&env), (0, 0));
}

/// A non-cacheable directory's cacheable child would leave a dentry keyed
/// by a parent that goes away: debug builds refuse it.
#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "non-cacheable directory")]
fn a_cacheable_child_of_a_non_cacheable_directory_is_refused() {
    let (env, dynfs, _proc_root) = setup_dynfs();
    dynfs.shared.live.lock().unwrap().insert(9);
    let _ = resolve_path(&env, None, b"/proc/9/cached", ResolveFlags::default());
}

// #262: open racing unlink. `GateFs` frees an unlinked inode's data the way
// ext4 does (Gate 6: the last unlink reads `open_count()` under the inode's
// lock, the one `stat` takes, and frees it if 0, detaches it otherwise), so an
// open file description on a freed inode shows up as `ENOENT` on read. Hooks
// put the unlink at each point of the open. Its rename lock is a real one, so
// a hook can show it runs outside Lock 3.

type GateHook = alloc::boxed::Box<dyn FnOnce() + Send>;

struct GateState {
    nlink: u32,
    /// `None` once freed by an unlink that saw no open reference.
    data: Option<Vec<u8>>,
    children: BTreeMap<String, Arc<GateInode>>,
}

#[derive(Default)]
struct GateHooks {
    /// Runs once, right after the next `inc_open_count` raised the count.
    after_inc: Option<GateHook>,
    /// Runs once, at the start of the next `stat` (before its lock).
    before_stat: Option<GateHook>,
    /// Runs once, right after the next `stat` (its lock released).
    after_stat: Option<GateHook>,
    /// Runs once, right after the next lookup of this name (its lock released).
    after_lookup: Option<(String, GateHook)>,
    /// Runs once, at the next `exempt_from_rofs`: in `open_inode_guarded`,
    /// after the lookup and after Lock 3, before the count goes up.
    at_rofs_check: Option<GateHook>,
    /// The next `stat` fails with this.
    stat_error: Option<VfsError>,
    /// Every `truncate` fails with this.
    truncate_error: Option<VfsError>,
}

struct GateInode {
    lock_id: u64,
    ino: Ino,
    inode_type: InodeType,
    next_ino: Arc<AtomicU64>,
    open_count: AtomicU32,
    detached: AtomicBool,
    state: Mutex<GateState>,
    hooks: Mutex<GateHooks>,
    locks: Mutex<AdvisoryLocks>,
}

impl GateInode {
    fn new(next_ino: &Arc<AtomicU64>, inode_type: InodeType, nlink: u32, data: Option<Vec<u8>>) -> Arc<Self> {
        Arc::new(Self {
            lock_id: next_lock_id(),
            ino: next_ino.fetch_add(1, Ordering::Relaxed),
            inode_type,
            next_ino: Arc::clone(next_ino),
            open_count: AtomicU32::new(0),
            detached: AtomicBool::new(false),
            state: Mutex::new(GateState { nlink, data, children: BTreeMap::new() }),
            hooks: Mutex::new(GateHooks::default()),
            locks: Mutex::new(AdvisoryLocks::new()),
        })
    }

    fn arm(&self, f: impl FnOnce(&mut GateHooks)) {
        f(&mut self.hooks.lock().unwrap());
    }

    fn run(&self, take: impl FnOnce(&mut GateHooks) -> Option<GateHook>) {
        let hook = take(&mut self.hooks.lock().unwrap());
        if let Some(hook) = hook {
            hook();
        }
    }

    fn all_hooks_ran(&self) -> bool {
        let h = self.hooks.lock().unwrap();
        h.after_inc.is_none()
            && h.before_stat.is_none()
            && h.after_stat.is_none()
            && h.after_lookup.is_none()
            && h.at_rofs_check.is_none()
    }

    fn child(&self, name: &str) -> Option<Arc<GateInode>> {
        self.state.lock().unwrap().children.get(name).cloned()
    }
}

impl VfsInode for GateInode {
    fn lock_id(&self) -> u64 {
        self.lock_id
    }
    fn dev_id(&self) -> DevId {
        DevId::ROOT
    }
    fn ino(&self) -> Ino {
        self.ino
    }
    fn inode_type(&self) -> InodeType {
        self.inode_type
    }
    fn open_count(&self) -> u32 {
        self.open_count.load(Ordering::Acquire)
    }
    fn inc_open_count(&self) {
        self.open_count.fetch_add(1, Ordering::AcqRel);
        self.run(|h| h.after_inc.take());
    }
    fn dec_open_count(&self) -> u32 {
        self.open_count.fetch_sub(1, Ordering::AcqRel) - 1
    }
    fn is_detached(&self) -> bool {
        self.detached.load(Ordering::Acquire)
    }
    fn exempt_from_rofs(&self) -> bool {
        self.run(|h| h.at_rofs_check.take());
        false
    }
    fn stat(&self) -> Result<Metadata, VfsError> {
        self.run(|h| h.before_stat.take());
        if let Some(e) = self.hooks.lock().unwrap().stat_error.take() {
            return Err(e);
        }
        let meta = {
            let g = self.state.lock().unwrap();
            Metadata {
                dev: DevId::ROOT,
                ino: self.ino,
                inode_type: self.inode_type,
                mode: 0o644,
                nlink: g.nlink,
                uid: 0,
                gid: 0,
                rdev: DevId(0),
                size: g.data.as_ref().map_or(0, |d| d.len() as u64),
                blksize: 4096,
                blocks: 0,
                atime: Timespec::default(),
                mtime: Timespec::default(),
                ctime: Timespec::default(),
            }
        };
        self.run(|h| h.after_stat.take());
        Ok(meta)
    }
    fn lookup(&self, name: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
        let found = self.child(name);
        self.run(|h| match &h.after_lookup {
            Some((n, _)) if n == name => h.after_lookup.take().map(|(_, hook)| hook),
            _ => None,
        });
        found.map(|c| c as Arc<dyn VfsInode>).ok_or(VfsError::NoEnt)
    }
    fn create(&self, name: &str, _mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        let mut g = self.state.lock().unwrap();
        if g.children.contains_key(name) {
            return Err(VfsError::Exist);
        }
        let f = GateInode::new(&self.next_ino, InodeType::File, 1, Some(Vec::new()));
        g.children.insert(String::from(name), Arc::clone(&f));
        Ok(f)
    }
    fn mkdir(&self, _name: &str, _mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        Err(VfsError::Perm)
    }
    fn unlink(&self, name: &str) -> Result<(), VfsError> {
        let child = self.state.lock().unwrap().children.remove(name).ok_or(VfsError::NoEnt)?;
        // Gate 6: the count read and the free or detach in one hold of the
        // child's lock.
        let mut cg = child.state.lock().unwrap();
        if cg.nlink > 1 {
            cg.nlink -= 1;
            return Ok(());
        }
        cg.nlink = 0;
        if child.open_count() > 0 {
            child.detached.store(true, Ordering::Release);
        } else {
            cg.data = None;
        }
        Ok(())
    }
    fn rmdir(&self, _name: &str) -> Result<(), VfsError> {
        Err(VfsError::Perm)
    }
    fn link(&self, _name: &str, _target: &Arc<dyn VfsInode>) -> Result<(), VfsError> {
        Err(VfsError::Perm)
    }
    fn symlink(&self, _name: &str, _target: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
        Err(VfsError::Perm)
    }
    fn readlink(&self) -> Result<Vec<u8>, VfsError> {
        Err(VfsError::Inval)
    }
    fn rename(&self, _old: &str, _new_dir: &Arc<dyn VfsInode>, _new: &str, _f: RenameFlags) -> Result<(), VfsError> {
        Err(VfsError::Perm)
    }
    fn readdir(&self, _cursor: u64, _out: &mut Vec<DirEntry>, _max: usize) -> Result<(), VfsError> {
        Ok(())
    }
    fn truncate(&self, size: u64) -> Result<(), VfsError> {
        if let Some(e) = self.hooks.lock().unwrap().truncate_error {
            return Err(e);
        }
        let mut g = self.state.lock().unwrap();
        g.data.as_mut().ok_or(VfsError::NoEnt)?.resize(size as usize, 0);
        Ok(())
    }
    fn set_mode(&self, _mode: u16) -> Result<(), VfsError> {
        Ok(())
    }
    fn set_owner(&self, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), VfsError> {
        Ok(())
    }
    fn set_times(&self, _atime: SetTime, _mtime: SetTime) -> Result<(), VfsError> {
        Ok(())
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        let g = self.state.lock().unwrap();
        let data = g.data.as_ref().ok_or(VfsError::NoEnt)?;
        let off = (offset as usize).min(data.len());
        let n = buf.len().min(data.len() - off);
        buf[..n].copy_from_slice(&data[off..off + n]);
        Ok(n)
    }
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, VfsError> {
        Err(VfsError::Perm)
    }
    fn read_page(&self, _page_index: u64, _buf: &mut [u8; 4096]) -> Result<usize, VfsError> {
        Err(VfsError::Perm)
    }
    fn write_page(&self, _page_index: u64, _buf: &[u8]) -> Result<(), VfsError> {
        Err(VfsError::Perm)
    }
    fn sync(&self, _data_only: bool) -> Result<(), VfsError> {
        Ok(())
    }
    fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
        f(&mut self.locks.lock().unwrap());
        Ok(())
    }
}

struct GateFs {
    root: Arc<GateInode>,
    rename_lock: Arc<Mutex<()>>,
}

impl FileSystem for GateFs {
    fn dev_id(&self) -> DevId {
        DevId::ROOT
    }
    fn name(&self) -> &'static str {
        "gatefs"
    }
    fn root_inode(&self) -> Arc<dyn VfsInode> {
        self.root.clone()
    }
    fn sync(&self) -> Result<(), VfsError> {
        Ok(())
    }
    fn read_only(&self) -> bool {
        false
    }
    fn with_rename_lock(&self, f: &mut dyn FnMut() -> Result<(), VfsError>) -> Result<(), VfsError> {
        let _held = self.rename_lock.lock().unwrap();
        f()
    }
}

const GATE_DATA: &[u8] = b"still here";

struct Gate {
    env: SimpleVfsTables,
    root: Arc<GateInode>,
    files: Vec<Arc<GateInode>>,
    rename_lock: Arc<Mutex<()>>,
}

impl Gate {
    /// A hook unlinking `name` from the root, the way `unlink_at` reaches
    /// the filesystem: under Lock 3 (so it must run outside it).
    fn unlink_hook(&self, name: &'static str) -> GateHook {
        let root = Arc::clone(&self.root);
        let lock = Arc::clone(&self.rename_lock);
        alloc::boxed::Box::new(move || {
            let _held = lock.try_lock().expect("the hook runs outside Lock 3");
            root.unlink(name).unwrap();
        })
    }
}

/// Tables over a `GateFs` whose root holds the files `names`, each with
/// `GATE_DATA` and one link.
fn gate_vfs(names: &[&str]) -> Gate {
    let next_ino = Arc::new(AtomicU64::new(1));
    let root = GateInode::new(&next_ino, InodeType::Dir, 2, None);
    let files: Vec<Arc<GateInode>> = names
        .iter()
        .map(|name| {
            let f = GateInode::new(&next_ino, InodeType::File, 1, Some(GATE_DATA.to_vec()));
            root.state.lock().unwrap().children.insert(String::from(*name), Arc::clone(&f));
            f
        })
        .collect();
    let rename_lock = Arc::new(Mutex::new(()));
    let env = SimpleVfsTables::new();
    env.mount_root(Arc::new(GateFs { root: Arc::clone(&root), rename_lock: Arc::clone(&rename_lock) })).unwrap();
    Gate { env, root, files, rename_lock }
}

fn read_gate(of: &OpenFile<StdLock<u64>>) -> Result<Vec<u8>, VfsError> {
    let mut buf = [0u8; 32];
    let n = of.inode.read_at(0, &mut buf)?;
    Ok(buf[..n].to_vec())
}

fn gate_file(opened: Opened) -> OpenFile<StdLock<u64>> {
    OpenFile::<StdLock<u64>>::from_opened(opened).unwrap()
}

const RDONLY: OpenFlags = OpenFlags(OpenFlags::O_RDONLY);
const OPATH: OpenFlags = OpenFlags(OpenFlags::O_PATH);

#[test]
fn open_file_new_refuses_an_inode_freed_since_open_inode() {
    // The window of #262: `open_inode` returned, the unlink frees the file
    // (no open reference yet), then the open file description is built.
    let g = gate_vfs(&["a"]);
    let (inode, path) = open_inode(&g.env, None, b"/a", RDONLY, 0, 0).unwrap();
    assert_eq!(g.files[0].open_count(), 0, "open_inode returns holding no reference");
    unlink_at(&g.env, None, b"/a", false).unwrap();
    match OpenFile::<StdLock<u64>>::new(inode, Arc::from(path.as_str()), RDONLY) {
        Ok(of) => panic!("an open file description on a freed inode, reading {:?}", read_gate(&of)),
        Err(e) => assert_eq!(e, VfsError::NoEnt),
    }
    assert_eq!(g.files[0].open_count(), 0, "the refused open gave its reference back");
}

#[test]
fn guarded_open_keeps_the_data_through_a_racing_unlink() {
    let g = gate_vfs(&["after", "between", "checked", "before"]);
    let f = &g.files;

    // 1. The unlink lands after the guarded open returned, before the open
    //    file description is built: the reference the open took carries over.
    let opened = open_inode_guarded(&g.env, None, b"/after", RDONLY, 0, 0).unwrap();
    assert_eq!(f[0].open_count(), 1);
    assert_eq!(opened.flags(), RDONLY);
    unlink_at(&g.env, None, b"/after", false).unwrap();
    let of = gate_file(opened);
    assert_eq!(f[0].open_count(), 1, "from_opened takes the reference over, no second count");
    assert!(f[0].is_detached());
    assert_eq!(read_gate(&of).unwrap(), GATE_DATA);
    drop(of);
    assert_eq!(f[0].open_count(), 0);

    // 2. The unlink lands right after the open raised the count, before its
    //    liveness check: Gate 6 sees the reference and detaches.
    let hook = g.unlink_hook("between");
    f[1].arm(|h| h.after_inc = Some(hook));
    let of = gate_file(open_inode_guarded(&g.env, None, b"/between", RDONLY, 0, 0).unwrap());
    assert!(f[1].all_hooks_ran());
    assert!(f[1].is_detached());
    assert_eq!(f[1].open_count(), 1);
    assert_eq!(read_gate(&of).unwrap(), GATE_DATA);
    drop(of);
    assert_eq!(f[1].open_count(), 0);

    // 3. The unlink lands right after the liveness check read the link
    //    count: the count was already up, so it still detaches.
    let hook = g.unlink_hook("checked");
    f[2].arm(|h| h.after_stat = Some(hook));
    let of = gate_file(open_inode_guarded(&g.env, None, b"/checked", RDONLY, 0, 0).unwrap());
    assert!(f[2].all_hooks_ran());
    assert!(f[2].is_detached());
    assert_eq!(read_gate(&of).unwrap(), GATE_DATA);
    drop(of);
    assert_eq!(f[2].open_count(), 0);

    // 4. The unlink lands after the lookup, before the count went up: the
    //    file is freed and the open fails cleanly, its reference given back.
    let hook = g.unlink_hook("before");
    g.root.arm(|h| h.after_lookup = Some((String::from("before"), hook)));
    assert_eq!(open_inode_guarded(&g.env, None, b"/before", RDONLY, 0, 0).err(), Some(VfsError::NoEnt));
    assert!(g.root.all_hooks_ran());
    assert!(!f[3].is_detached());
    assert_eq!(f[3].open_count(), 0);
    let mut buf = [0u8; 4];
    assert_eq!(f[3].read_at(0, &mut buf), Err(VfsError::NoEnt), "the model freed it");
}

#[test]
fn try_new_reads_the_link_count_before_the_detached_flag() {
    // The unlink lands inside the liveness check, after the count went up and
    // before `stat` reads: it detaches, and the check must see that. Reading
    // `is_detached` before `stat` would see "not detached" and then
    // `nlink == 0`, refusing a live file.
    let g = gate_vfs(&["mid"]);
    let hook = g.unlink_hook("mid");
    g.files[0].arm(|h| h.before_stat = Some(hook));
    let of = gate_file(open_inode_guarded(&g.env, None, b"/mid", RDONLY, 0, 0).unwrap());
    assert!(g.files[0].all_hooks_ran());
    assert!(g.files[0].is_detached());
    assert_eq!(read_gate(&of).unwrap(), GATE_DATA);
    drop(of);
    assert_eq!(g.files[0].open_count(), 0);
}

#[test]
fn o_path_open_racing_unlink_detaches_or_fails_cleanly() {
    let g = gate_vfs(&["between", "before", "plain"]);
    let f = &g.files;

    // After the count went up: detached, the descriptor holds it.
    let hook = g.unlink_hook("between");
    f[0].arm(|h| h.after_inc = Some(hook));
    let of = gate_file(open_inode_guarded(&g.env, None, b"/between", OPATH, 0, 0).unwrap());
    assert!(f[0].all_hooks_ran());
    assert!(of.is_path);
    assert!(f[0].is_detached());
    assert_eq!(f[0].open_count(), 1);
    drop(of);
    assert_eq!(f[0].open_count(), 0);

    // Before it went up: freed, the open fails, nothing counted.
    let hook = g.unlink_hook("before");
    g.root.arm(|h| h.after_lookup = Some((String::from("before"), hook)));
    assert_eq!(open_inode_guarded(&g.env, None, b"/before", OPATH, 0, 0).err(), Some(VfsError::NoEnt));
    assert!(g.root.all_hooks_ran());
    assert_eq!(f[1].open_count(), 0);

    // `open_inode` with `O_PATH` gives its reference back.
    let (inode, _) = open_inode(&g.env, None, b"/plain", OPATH, 0, 0).unwrap();
    assert!(Arc::ptr_eq(&(Arc::clone(&f[2]) as Arc<dyn VfsInode>), &inode));
    assert_eq!(f[2].open_count(), 0);
}

#[test]
fn o_creat_open_of_a_file_unlinked_after_lock_3_opens_a_new_one() {
    // `O_CREAT` (no `O_EXCL`) finds `/c` under Lock 3; the unlink lands
    // once Lock 3 is released, before the count goes up, and frees it.
    // POSIX: the old file or a new one, never `ENOENT`. The open goes
    // round again and creates a new `/c`.
    let g = gate_vfs(&["c"]);
    let old = Arc::clone(&g.files[0]);
    let hook = g.unlink_hook("c");
    old.arm(|h| h.at_rofs_check = Some(hook));
    let flags = OpenFlags(OpenFlags::O_CREAT | OpenFlags::O_RDWR);
    let of = gate_file(open_inode_guarded(&g.env, None, b"/c", flags, 0o644, 0).unwrap());
    assert!(old.all_hooks_ran());
    assert_eq!(old.open_count(), 0);
    assert!(!old.is_detached());
    let new = g.root.child("c").expect("a new /c");
    assert!(!Arc::ptr_eq(&new, &old));
    assert_eq!(of.inode.lock_id(), new.lock_id());
    assert_eq!(new.open_count(), 1);
    assert_eq!(read_gate(&of).unwrap(), b"");
    drop(of);
    assert_eq!(new.open_count(), 0);

    // Without `O_CREAT` the same race is a clean `ENOENT`.
    let g = gate_vfs(&["c"]);
    let hook = g.unlink_hook("c");
    g.files[0].arm(|h| h.at_rofs_check = Some(hook));
    assert_eq!(open_inode_guarded(&g.env, None, b"/c", RDONLY, 0, 0).err(), Some(VfsError::NoEnt));
    assert_eq!(g.files[0].open_count(), 0);
}

#[test]
fn failed_opens_give_their_reference_back() {
    let g = gate_vfs(&["stat", "stat_new", "trunc"]);
    let f = &g.files;

    // `stat` fails inside `try_new`: its error, unchanged, and no count.
    f[0].arm(|h| h.stat_error = Some(VfsError::Io));
    assert_eq!(open_inode_guarded(&g.env, None, b"/stat", RDONLY, 0, 0).err(), Some(VfsError::Io));
    assert_eq!(f[0].open_count(), 0);
    f[1].arm(|h| h.stat_error = Some(VfsError::Io));
    let res = OpenFile::<StdLock<u64>>::new(Arc::clone(&f[1]) as Arc<dyn VfsInode>, Arc::from("/stat_new"), RDONLY);
    assert_eq!(res.err(), Some(VfsError::Io));
    assert_eq!(f[1].open_count(), 0);

    // `O_TRUNC`'s truncate fails after the count went up.
    f[2].arm(|h| h.truncate_error = Some(VfsError::Io));
    let trunc = OpenFlags(OpenFlags::O_WRONLY | OpenFlags::O_TRUNC);
    assert_eq!(open_inode_guarded(&g.env, None, b"/trunc", trunc, 0, 0).err(), Some(VfsError::Io));
    assert_eq!(f[2].open_count(), 0);
}
