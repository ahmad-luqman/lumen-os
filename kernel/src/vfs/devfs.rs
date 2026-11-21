//! Read-only `/dev` namespace (`devfs`, `dev = 0:3`) with 6 `S_ISCHR` nodes
//! exempt from `-EROFS` for `O_WRONLY`/`O_RDWR`/`O_TRUNC`/`W_OK`.

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use lumen_vfs::{
    AdvisoryLocks, DevId, DirEntry, FileSystem, Ino, InodeType, Metadata, RenameFlags, SetTime, Timespec, VfsError,
    VfsInode, next_lock_id,
};

use crate::arch;
use crate::klog;
use crate::linux::process;
use crate::rng;
use crate::sync::SleepMutex;
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
                ctime: now,
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
            ctime: now,
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

pub struct DevfsInode {
    lock_id: u64,
    dev: DevId,
    ino: Ino,
    kind: DevKind,
    ctime: Timespec,
    self_weak: Weak<DevfsInode>,
    open_count: AtomicU32,
    nodes: Vec<(&'static str, Arc<DevfsInode>)>,
    locks: SleepMutex<AdvisoryLocks>,
}

fn fill_splitmix64(buf: &mut [u8]) {
    let mut state = time::now_ns() ^ arch::rdtsc().rotate_left(17) ^ 0x9e37_79b9_7f4a_7c15;
    let mut off = 0usize;
    while off < buf.len() {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        let chunk = (buf.len() - off).min(8);
        buf[off..off + chunk].copy_from_slice(&z.to_le_bytes()[..chunk]);
        off += chunk;
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

    fn exempt_from_rofs(&self) -> bool {
        self.kind != DevKind::Root
    }

    fn stat(&self) -> Result<Metadata, VfsError> {
        Ok(Metadata {
            dev: self.dev,
            ino: self.ino,
            inode_type: self.inode_type(),
            mode: self.kind.mode(),
            nlink: if self.kind == DevKind::Root { 2 } else { 1 },
            uid: 0,
            gid: 0,
            rdev: self.kind.rdev(),
            size: 0,
            blksize: 4096,
            blocks: 0,
            atime: self.ctime,
            mtime: self.ctime,
            ctime: self.ctime,
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

    fn set_mode(&self, _mode: u16) -> Result<(), VfsError> {
        Err(VfsError::RoFs)
    }

    fn set_owner(&self, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), VfsError> {
        Err(VfsError::RoFs)
    }

    fn set_times(&self, _atime: SetTime, _mtime: SetTime) -> Result<(), VfsError> {
        Err(VfsError::RoFs)
    }

    fn read_at(&self, _offset: u64, buf: &mut [u8]) -> Result<usize, VfsError> {
        match self.kind {
            DevKind::Root => Err(VfsError::IsDir),
            DevKind::Null => Ok(0),
            DevKind::Zero => {
                buf.fill(0);
                Ok(buf.len())
            }
            DevKind::Random => {
                if buf.is_empty() {
                    return Ok(0);
                }
                match rng::fill(buf) {
                    Ok(()) => Ok(buf.len()),
                    Err(_) => Err(VfsError::Again),
                }
            }
            DevKind::Urandom => {
                if buf.is_empty() {
                    return Ok(0);
                }
                if rng::fill(buf).is_err() {
                    fill_splitmix64(buf);
                }
                Ok(buf.len())
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
