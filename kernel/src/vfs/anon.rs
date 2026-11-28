//! Anonymous inodes (`anon_inodefs`, dev `0:9`; M9 B0, #471, ARCHITECTURE
//! sections 8 and 10).
//!
//! Used by kernel objects that have no path in any mounted filesystem:
//! `epoll` (`anon_inode:[eventpoll]`), `eventfd` (`anon_inode:[eventfd]`) and
//! `timerfd` (`anon_inode:[timerfd]`). Each instance allocates a fresh inode
//! number on device `0:9`, bypasses the VFS caches, and attaches its object
//! to `KernelFile::anon`.

#![cfg_attr(not(feature = "ktest"), allow(dead_code))] // constructed by B5/B6

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use lumen_vfs::{
    AdvisoryLocks, DevId, DirEntry, Ino, InodeType, Metadata, SetTime, Timespec, VfsError, VfsInode, mode_after_chown,
    next_lock_id,
};

use crate::sync::SleepMutex;
use crate::time;

/// Device identifier for the internal `anon_inodefs` (`0:9`, never mounted).
pub const ANON_DEV: DevId = DevId::makedev(0, 9);

static NEXT_ANON_INO: AtomicU64 = AtomicU64::new(1);

/// The kind of anonymous inode (`anon_inodefs`, dev `0:9`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // constructed by B5/B6
pub enum AnonKind {
    /// `epoll` instance (`anon_inode:[eventpoll]`).
    EventPoll,
    /// `eventfd` instance (`anon_inode:[eventfd]`).
    EventFd,
    /// `timerfd` instance (`anon_inode:[timerfd]`).
    TimerFd,
}

impl AnonKind {
    /// The link target shown in `/proc/<pid>/fd/<n>`.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by B5/B6
    pub const fn path(self) -> &'static str {
        match self {
            Self::EventPoll => "anon_inode:[eventpoll]",
            Self::EventFd => "anon_inode:[eventfd]",
            Self::TimerFd => "anon_inode:[timerfd]",
        }
    }
}

struct AnonMeta {
    mode: u16,
    uid: u32,
    gid: u32,
    atime: Timespec,
    mtime: Timespec,
    ctime: Timespec,
    locks: AdvisoryLocks,
}

/// Unmounted anonymous inode on `anon_inodefs` (dev `0:9`, never cached).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // constructed by B5/B6
pub struct AnonInode {
    lock_id: u64,
    ino: Ino,
    kind: AnonKind,
    open_count: AtomicU32,
    meta: SleepMutex<AnonMeta>,
}

impl AnonInode {
    /// Allocate a fresh anonymous inode of `kind` with a unique inode number on
    /// [`ANON_DEV`].
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // constructed by B5/B6
    pub fn new(kind: AnonKind) -> Self {
        let now = time::realtime_timespec();
        Self {
            lock_id: next_lock_id(),
            ino: NEXT_ANON_INO.fetch_add(1, Ordering::Relaxed),
            kind,
            open_count: AtomicU32::new(0),
            meta: SleepMutex::new(AnonMeta {
                mode: 0o600,
                uid: 0,
                gid: 0,
                atime: now,
                mtime: now,
                ctime: now,
                locks: AdvisoryLocks::new(),
            }),
        }
    }

    /// The anonymous inode's kind (`eventpoll`, `eventfd`, or `timerfd`).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by B5/B6
    pub const fn kind(&self) -> AnonKind {
        self.kind
    }
}

/// Allocate an anonymous inode of `kind` and its `/proc/<pid>/fd` display path.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by B5/B6
pub fn new_inode(kind: AnonKind) -> (Arc<dyn VfsInode>, Arc<[u8]>) {
    (Arc::new(AnonInode::new(kind)), Arc::from(kind.path().as_bytes()))
}

impl VfsInode for AnonInode {
    fn lock_id(&self) -> u64 {
        self.lock_id
    }

    fn dev_id(&self) -> DevId {
        ANON_DEV
    }

    fn ino(&self) -> Ino {
        self.ino
    }

    fn inode_type(&self) -> InodeType {
        InodeType::File
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
        false
    }

    fn stat(&self) -> Result<Metadata, VfsError> {
        let meta = self.meta.lock();
        Ok(Metadata {
            dev: ANON_DEV,
            ino: self.ino,
            inode_type: InodeType::File,
            mode: meta.mode,
            nlink: 1,
            uid: meta.uid,
            gid: meta.gid,
            rdev: DevId::default(),
            size: 0,
            blksize: 4096,
            blocks: 0,
            atime: meta.atime,
            mtime: meta.mtime,
            ctime: meta.ctime,
        })
    }

    fn lookup(&self, _name: &[u8]) -> Result<Arc<dyn VfsInode>, VfsError> {
        Err(VfsError::NotDir)
    }

    lumen_vfs::fixed_namespace_ops!();

    fn readlink(&self) -> Result<Vec<u8>, VfsError> {
        Err(VfsError::Inval)
    }

    fn readdir(&self, _cursor: u64, _out: &mut Vec<DirEntry>, _max_entries: usize) -> Result<(), VfsError> {
        Err(VfsError::NotDir)
    }

    fn truncate(&self, _size: u64) -> Result<(), VfsError> {
        Err(VfsError::Inval)
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
        meta.mode = mode_after_chown(InodeType::File, meta.mode);
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

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, VfsError> {
        Err(VfsError::Inval)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, VfsError> {
        Err(VfsError::Inval)
    }

    fn read_page(&self, _page_index: u64, _buf: &mut [u8; 4096]) -> Result<usize, VfsError> {
        Err(VfsError::Inval)
    }

    fn write_page(&self, _page_index: u64, _buf: &[u8]) -> Result<(), VfsError> {
        Err(VfsError::Inval)
    }

    fn sync(&self, _data_only: bool) -> Result<(), VfsError> {
        Err(VfsError::Inval)
    }

    fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        f(&mut meta.locks);
        Ok(())
    }

    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}

crate::ktest!(
    fn anon_inodes_report_dev_0_9_and_proc_fd_names() {
        use crate::linux::process::{self, KernelFile};
        use crate::{kassert, kassert_eq};
        use lumen_vfs::OpenFlags;

        let cases = [
            (AnonKind::EventPoll, "anon_inode:[eventpoll]"),
            (AnonKind::EventFd, "anon_inode:[eventfd]"),
            (AnonKind::TimerFd, "anon_inode:[timerfd]"),
        ];

        process::in_test_process(move |pid| {
            let mut prev_ino = 0u64;
            let mut prev_id = 0u64;
            for (kind, expected_name) in cases {
                let (inode, path) = new_inode(kind);
                kassert_eq!(inode.dev_id(), ANON_DEV);
                kassert_eq!((inode.dev_id().major(), inode.dev_id().minor()), (0, 9));
                kassert!(!inode.cacheable());
                kassert!(inode.ino() > prev_ino);
                prev_ino = inode.ino();
                let st = inode.stat().map_err(|_| "stat failed")?;
                kassert_eq!(st.dev, ANON_DEV);
                kassert_eq!(st.ino, inode.ino());
                kassert_eq!(st.mode, 0o600);
                kassert_eq!(st.nlink, 1);
                kassert_eq!(
                    inode.as_any().and_then(|a| a.downcast_ref::<AnonInode>()).map(AnonInode::kind),
                    Some(kind)
                );
                kassert_eq!(&*path, expected_name.as_bytes());

                let file = KernelFile::new_unchecked_flags(inode, path, OpenFlags::from_bits(0o2))
                    .map_err(|_| "KernelFile::new_unchecked_flags")?;
                kassert!(file.id() > prev_id);
                prev_id = file.id();
                kassert!(file.anon().is_none());
                kassert!(file.pollable().is_none());

                let fd = process::install_current_fd(file, false).map_err(|_| "install_current_fd")?;
                let dup_fd = process::dup_current_fd_from(fd, 0, false).map_err(|_| "dup_current_fd_from")?;
                let f1 = process::current_io_file(fd).ok_or("fd missing")?;
                let f2 = process::current_io_file(dup_fd).ok_or("dup_fd missing")?;
                kassert_eq!(f1.id(), f2.id());
                drop((f1, f2));

                let target = process::process_fd_target(pid, fd).ok_or("proc fd target missing")?;
                kassert_eq!(target.1.as_slice(), expected_name.as_bytes());

                process::close_current_fd(dup_fd).map_err(|_| "close dup_fd")?;
                process::close_current_fd(fd).map_err(|_| "close fd")?;
            }
            Ok(())
        })?
    }
);
