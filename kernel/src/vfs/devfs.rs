//! Read-only `/dev` namespace (`devfs`, `dev = 0:3`) with 6 `S_ISCHR` nodes.
//!
//! Namespace changes (create, unlink, mkdir, rename) return `-EROFS`, and
//! so does a metadata change on `/dev` itself (a divergence: devtmpfs
//! takes those on `/dev` too). The character nodes are special files, so
//! `lumen-vfs`'s open and access checks let `O_WRONLY`/`O_RDWR`/`O_TRUNC`/
//! `W_OK` through the read-only mount (`InodeType::is_special`). The
//! syscall layer's `writable_mount` lets `chmod`/`chown`/`utimensat`
//! through for this mount's (`DevId::DEVFS`) nodes, and their setters keep
//! the new mode, owner and times in memory until reboot, as Linux's
//! devtmpfs does. The setters take a change on any instance's node: only
//! the mount check tells `/dev`'s instance from another.
//! `random` and `urandom` read from `crate::rng` and return `-EAGAIN` while
//! its pool has never been seeded (no RDRAND and no virtio-rng answer).

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use lumen_vfs::{
    AdvisoryLocks, DevId, DirEntry, FileSystem, Ino, InodeType, Metadata, RenameFlags, SetTime, Timespec, VfsError,
    VfsInode, mode_after_chown, next_lock_id,
};

use crate::klog;
use crate::linux::process;
use crate::rng;
use crate::sync::{SleepMutex, SleepMutexGuard};
use crate::time;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DevKind {
    Root,
    Null,
    Zero,
    Random,
    Urandom,
    Tty,
    Console,
}

impl DevKind {
    const fn rdev(self) -> DevId {
        match self {
            Self::Root => DevId::makedev(0, 0),
            Self::Null => DevId::makedev(1, 3),
            Self::Zero => DevId::makedev(1, 5),
            Self::Random => DevId::makedev(1, 8),
            Self::Urandom => DevId::makedev(1, 9),
            Self::Tty => DevId::makedev(5, 0),
            Self::Console => DevId::makedev(5, 1),
        }
    }

    const fn mode(self) -> u16 {
        match self {
            Self::Root => 0o755,
            Self::Console => 0o620,
            Self::Null | Self::Zero | Self::Random | Self::Urandom | Self::Tty => 0o666,
        }
    }
}

/// Pseudo-filesystem mounted at `/dev` (`0:3`).
pub struct Devfs {
    dev: DevId,
    root: Arc<DevfsInode>,
}

impl Devfs {
    pub fn new(dev: DevId) -> Arc<Self> {
        let now = time::realtime_timespec();
        let mk_dev = |ino: u64, kind: DevKind| -> Arc<DevfsInode> {
            Arc::new_cyclic(|w| DevfsInode {
                lock_id: next_lock_id(),
                dev,
                ino,
                kind,
                meta: SleepMutex::new(DevfsMeta::new(kind, now)),
                self_weak: Weak::clone(w),
                open_count: AtomicU32::new(0),
                nodes: Vec::new(),
                locks: SleepMutex::new(AdvisoryLocks::new()),
            })
        };
        let entries = alloc::vec![
            ("console", mk_dev(7, DevKind::Console)),
            ("null", mk_dev(2, DevKind::Null)),
            ("random", mk_dev(4, DevKind::Random)),
            ("tty", mk_dev(6, DevKind::Tty)),
            ("urandom", mk_dev(5, DevKind::Urandom)),
            ("zero", mk_dev(3, DevKind::Zero)),
        ];
        let root = Arc::new_cyclic(|w| DevfsInode {
            lock_id: next_lock_id(),
            dev,
            ino: 1,
            kind: DevKind::Root,
            meta: SleepMutex::new(DevfsMeta::new(DevKind::Root, now)),
            self_weak: Weak::clone(w),
            open_count: AtomicU32::new(0),
            nodes: entries,
            locks: SleepMutex::new(AdvisoryLocks::new()),
        });
        Arc::new(Self { dev, root })
    }
}

impl FileSystem for Devfs {
    fn dev_id(&self) -> DevId {
        self.dev
    }

    fn name(&self) -> &'static str {
        "devfs"
    }

    fn root_inode(&self) -> Arc<dyn VfsInode> {
        Arc::clone(&self.root) as Arc<dyn VfsInode>
    }

    fn sync(&self) -> Result<(), VfsError> {
        Ok(())
    }

    fn read_only(&self) -> bool {
        true
    }
}

/// A node's mode, owner and times: fixed at boot, changed only by a
/// character node's setters (Lock 4, the inode's metadata lock, as
/// tmpfs's), kept in memory only.
struct DevfsMeta {
    mode: u16,
    uid: u32,
    gid: u32,
    atime: Timespec,
    mtime: Timespec,
    ctime: Timespec,
}

impl DevfsMeta {
    const fn new(kind: DevKind, now: Timespec) -> Self {
        Self { mode: kind.mode(), uid: 0, gid: 0, atime: now, mtime: now, ctime: now }
    }
}

pub struct DevfsInode {
    lock_id: u64,
    dev: DevId,
    ino: Ino,
    kind: DevKind,
    meta: SleepMutex<DevfsMeta>,
    self_weak: Weak<DevfsInode>,
    open_count: AtomicU32,
    nodes: Vec<(&'static str, Arc<DevfsInode>)>,
    locks: SleepMutex<AdvisoryLocks>,
}

impl DevfsInode {
    /// The metadata, held for a change: a character node's, as Linux's
    /// devtmpfs takes `chmod`, `chown` and `utimensat` on its nodes. `/dev`
    /// itself is `-EROFS`, as the syscall layer's mount check already says
    /// for it (a directory isn't exempt).
    fn changeable_meta(&self) -> Result<SleepMutexGuard<'_, DevfsMeta>, VfsError> {
        if self.kind == DevKind::Root {
            return Err(VfsError::RoFs);
        }
        Ok(self.meta.lock())
    }
}

impl VfsInode for DevfsInode {
    fn lock_id(&self) -> u64 {
        self.lock_id
    }

    fn dev_id(&self) -> DevId {
        self.dev
    }

    fn ino(&self) -> Ino {
        self.ino
    }

    fn inode_type(&self) -> InodeType {
        match self.kind {
            DevKind::Root => InodeType::Dir,
            _ => InodeType::CharDevice,
        }
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

    fn is_tty(&self) -> bool {
        matches!(self.kind, DevKind::Tty | DevKind::Console)
    }

    fn stat(&self) -> Result<Metadata, VfsError> {
        let meta = self.meta.lock();
        Ok(Metadata {
            dev: self.dev,
            ino: self.ino,
            inode_type: self.inode_type(),
            mode: meta.mode,
            nlink: if self.kind == DevKind::Root { 2 } else { 1 },
            uid: meta.uid,
            gid: meta.gid,
            rdev: self.kind.rdev(),
            size: 0,
            blksize: 4096,
            blocks: 0,
            atime: meta.atime,
            mtime: meta.mtime,
            ctime: meta.ctime,
        })
    }

    fn lookup(&self, name: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.kind != DevKind::Root {
            return Err(VfsError::NotDir);
        }
        if name == "." || name == ".." {
            return self.self_weak.upgrade().map(|a| a as Arc<dyn VfsInode>).ok_or(VfsError::NoEnt);
        }
        self.nodes
            .iter()
            .find(|&&(n, _)| n == name)
            .map(|(_, inode)| Arc::clone(inode) as Arc<dyn VfsInode>)
            .ok_or(VfsError::NoEnt)
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
        Err(VfsError::Inval)
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

    fn readdir(&self, cursor: u64, out: &mut Vec<DirEntry>, max_entries: usize) -> Result<(), VfsError> {
        if self.kind != DevKind::Root {
            return Err(VfsError::NotDir);
        }
        if max_entries == 0 {
            return Ok(());
        }
        for (idx, (ino, itype, name)) in [(1, InodeType::Dir, "."), (1, InodeType::Dir, "..")]
            .into_iter()
            .chain(self.nodes.iter().map(|&(name, ref node)| (node.ino, InodeType::CharDevice, name)))
            .enumerate()
        {
            if out.len() >= max_entries {
                break;
            }
            let cur_idx = idx as u64;
            if cur_idx >= cursor {
                out.push(DirEntry { ino, inode_type: itype, name: String::from(name), next_cursor: cur_idx + 1 });
            }
        }
        Ok(())
    }

    fn truncate(&self, _size: u64) -> Result<(), VfsError> {
        if self.kind == DevKind::Root { Err(VfsError::IsDir) } else { Err(VfsError::Inval) }
    }

    fn set_mode(&self, mode: u16) -> Result<(), VfsError> {
        let mut meta = self.changeable_meta()?;
        meta.mode = mode & 0o7777;
        meta.ctime = time::realtime_timespec();
        Ok(())
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), VfsError> {
        let mut meta = self.changeable_meta()?;
        if let Some(u) = uid {
            meta.uid = u;
        }
        if let Some(g) = gid {
            meta.gid = g;
        }
        // In the same hold (`VfsInode::set_owner`'s contract).
        meta.mode = mode_after_chown(self.inode_type(), meta.mode);
        meta.ctime = time::realtime_timespec();
        Ok(())
    }

    fn set_times(&self, atime: SetTime, mtime: SetTime) -> Result<(), VfsError> {
        let mut meta = self.changeable_meta()?;
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

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        match self.kind {
            DevKind::Root => Err(VfsError::IsDir),
            DevKind::Null => Ok(0),
            DevKind::Zero => {
                buf.fill(0);
                Ok(buf.len())
            }
            DevKind::Random | DevKind::Urandom => {
                if buf.is_empty() {
                    return Ok(0);
                }
                match rng::fill(buf) {
                    Ok(()) => Ok(buf.len()),
                    Err(_) => Err(VfsError::Again),
                }
            }
            DevKind::Tty => match process::try_current_io() {
                Some(io) => Ok(process::read_console(io, buf)),
                None => Err(VfsError::NoDev),
            },
            DevKind::Console => match process::try_current_io() {
                Some(io) => Ok(process::read_console(io, buf)),
                None => Ok(0),
            },
        }
    }

    fn write_at(&self, _offset: u64, buf: &[u8]) -> Result<usize, VfsError> {
        match self.kind {
            DevKind::Root => Err(VfsError::IsDir),
            DevKind::Null | DevKind::Zero | DevKind::Random | DevKind::Urandom => Ok(buf.len()),
            DevKind::Tty => match process::try_current_io() {
                Some(io) => {
                    process::write_console(io, buf);
                    Ok(buf.len())
                }
                None => Err(VfsError::NoDev),
            },
            DevKind::Console => match process::try_current_io() {
                Some(io) => {
                    process::write_console(io, buf);
                    Ok(buf.len())
                }
                None => {
                    klog::with_line_lock(|| {
                        let _ = klog::burst_held(buf);
                    });
                    Ok(buf.len())
                }
            },
        }
    }

    fn read_page(&self, _page_index: u64, _buf: &mut [u8; 4096]) -> Result<usize, VfsError> {
        Err(VfsError::Inval)
    }

    fn write_page(&self, _page_index: u64, _buf: &[u8]) -> Result<(), VfsError> {
        Err(VfsError::Inval)
    }

    fn sync(&self, _data_only: bool) -> Result<(), VfsError> {
        Ok(())
    }

    fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
        let mut locks = self.locks.lock();
        f(&mut locks);
        Ok(())
    }
}

crate::ktest!(
    fn devfs_nodes_start_with_their_boot_metadata() {
        // A fresh instance (not `/dev`'s, whose nodes a `chmod` may have
        // changed): each node's Linux mode, number and type, owned by root.
        use crate::{kassert, kassert_eq};
        let fs = Devfs::new(DevId::makedev(0, 134));
        let root = fs.root_inode();
        let st = root.stat().map_err(|_| "stat /")?;
        kassert_eq!((st.inode_type, st.mode, st.uid, st.gid), (InodeType::Dir, 0o755, 0, 0));
        for (name, maj, min, mode) in [
            ("null", 1, 3, 0o666),
            ("zero", 1, 5, 0o666),
            ("random", 1, 8, 0o666),
            ("urandom", 1, 9, 0o666),
            ("tty", 5, 0, 0o666),
            ("console", 5, 1, 0o620),
        ] {
            let node = root.lookup(name).map_err(|_| "lookup")?.stat().map_err(|_| "stat")?;
            kassert_eq!(node.inode_type, InodeType::CharDevice);
            kassert_eq!((node.rdev, node.mode, node.uid, node.gid), (DevId::makedev(maj, min), mode, 0, 0));
        }
        // The root refuses a metadata change itself, whatever the caller
        // (`vfstest chmod` calls the setter without the syscalls' mount
        // check), and keeps its metadata.
        kassert_eq!(root.set_mode(0o700), Err(VfsError::RoFs));
        kassert_eq!(root.set_owner(Some(1), Some(1)), Err(VfsError::RoFs));
        kassert_eq!(root.set_times(SetTime::Now, SetTime::Now), Err(VfsError::RoFs));
        let after = root.stat().map_err(|_| "stat / after")?;
        kassert_eq!((after.mode, after.uid, after.gid), (0o755, 0, 0));
        kassert!(after.mtime == st.mtime && after.ctime == st.ctime);
        Ok(())
    }
);
