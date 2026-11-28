//! Anonymous inodes (`anon_inodefs`, dev `0:9`; M9 B0, #471, ARCHITECTURE
//! sections 8 and 10).
//!
//! Used by kernel objects that have no path in any mounted filesystem:
//! `epoll` (`anon_inode:[eventpoll]`), `eventfd` (`anon_inode:[eventfd]`) and
//! `timerfd` (`anon_inode:[timerfd]`). Following Linux v6.12's
//! `alloc_anon_inode`, `alloc_file_pseudo` and
//! `eventpoll_fops`/`eventfd_fops`/`timerfd_fops`, every anonymous file shares
//! one inode number ([`ANON_INO`]) on device `0:9`, reports `st_mode == 0o600`
//! (`InodeType::Anon`, `st_mode & S_IFMT == 0`), bypasses the VFS caches,
//! treats `lseek` as `noop_llseek`, refuses `pread64`/`pwrite64` with
//! `-ESPIPE`, refuses `/proc/<pid>/fd/N` reopen with `-ENXIO` unless `O_PATH`
//! is set, and refuses `mmap` with `-ENODEV`. `KernelFile::new_anon` attaches
//! the kernel object (`AnonObj`) to the description.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use lumen_vfs::{
    AdvisoryLocks, DevId, DirEntry, Ino, InodeType, Metadata, SetTime, Timespec, VfsError, VfsInode, mode_after_chown,
    next_lock_id,
};

use crate::sync::SleepMutex;
use crate::time;

/// Device identifier for the internal `anon_inodefs` (`0:9`, never mounted).
pub const ANON_DEV: DevId = DevId::makedev(0, 9);

/// Shared inode number reported by every anonymous file (matching Linux's
/// shared `anon_inode_inode`).
pub const ANON_INO: Ino = 1;

/// The kind of anonymous inode (`anon_inodefs`, dev `0:9`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnonKind {
    /// `epoll` instance (`anon_inode:[eventpoll]`).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // constructed by B5
    EventPoll,
    /// `eventfd` instance (`anon_inode:[eventfd]`).
    EventFd,
    /// `timerfd` instance (`anon_inode:[timerfd]`).
    TimerFd,
}

impl AnonKind {
    /// The link target shown in `/proc/<pid>/fd/<n>`.
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
pub struct AnonInode {
    lock_id: u64,
    kind: AnonKind,
    open_count: AtomicU32,
    meta: SleepMutex<AnonMeta>,
}

impl AnonInode {
    /// Allocate an anonymous inode of `kind` on [`ANON_DEV`] with shared
    /// [`ANON_INO`] and `0o600` mode (no `S_IFMT` bits).
    pub fn new(kind: AnonKind) -> Self {
        let now = time::realtime_timespec();
        Self {
            lock_id: next_lock_id(),
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
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // inspected by ktests / B5
    pub const fn kind(&self) -> AnonKind {
        self.kind
    }
}

/// Allocate an anonymous inode of `kind` and its `/proc/<pid>/fd` display path.
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
        ANON_INO
    }

    fn inode_type(&self) -> InodeType {
        InodeType::Anon
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
            ino: ANON_INO,
            inode_type: InodeType::Anon,
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
        meta.mode = mode_after_chown(InodeType::Anon, meta.mode);
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
        use crate::arch::syscall::SyscallFrame;
        use crate::linux::process::{self, KernelFile};
        use crate::linux::syscall::dispatch;
        use crate::linux::{errno, general};
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        use alloc::format;
        use core::mem::{offset_of, size_of};
        use lumen_vfs::OpenFlags;

        fn call(nr: u32, a: [u64; 6]) -> u64 {
            let mut frame = SyscallFrame {
                rax: u64::from(nr),
                rdi: a[0],
                rsi: a[1],
                rdx: a[2],
                r10: a[3],
                r8: a[4],
                r9: a[5],
                ..SyscallFrame::default()
            };
            let _ = dispatch(&mut frame);
            frame.rax
        }

        let cases = [
            (AnonKind::EventPoll, "anon_inode:[eventpoll]"),
            (AnonKind::EventFd, "anon_inode:[eventfd]"),
            (AnonKind::TimerFd, "anon_inode:[timerfd]"),
        ];

        process::in_test_process(move |pid| {
            let stat_va = process::KTEST_USER_VA;
            let path_va = stat_va + 256;
            let mut prev_id = 0u64;
            let neg = |e: u32| -(e as i64) as u64;

            for (kind, expected_name) in cases {
                let (inode, path) = new_inode(kind);
                kassert_eq!(inode.dev_id(), ANON_DEV);
                kassert_eq!((inode.dev_id().major(), inode.dev_id().minor()), (0, 9));
                kassert!(!inode.cacheable());
                kassert_eq!(inode.ino(), ANON_INO);
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

                // `fstat`: dev 0:9, shared ANON_INO, st_mode == 0o600 with no S_IFMT bits.
                kassert_eq!(call(general::__NR_fstat, [fd as u64, stat_va, 0, 0, 0, 0]), 0);
                let mut raw = [0u8; size_of::<general::stat>()];
                copy_from_user(&mut raw, stat_va).map_err(|_| "copy stat")?;
                let le_u64 = |off: usize| u64::from_le_bytes(raw[off..off + 8].try_into().expect("8 bytes"));
                let st_dev = le_u64(offset_of!(general::stat, st_dev));
                let st_ino = le_u64(offset_of!(general::stat, st_ino));
                let st_mode = le_u64(offset_of!(general::stat, st_mode)) as u32;
                kassert_eq!(st_dev, ANON_DEV.raw());
                kassert_eq!(st_ino, ANON_INO);
                kassert_eq!(st_mode & general::S_IFMT, 0);
                kassert_eq!(st_mode, 0o600);

                // `lseek`: `noop_llseek` leaves offset at 0 and returns 0.
                for (delta, whence) in
                    [(42i64, general::SEEK_SET), (17i64, general::SEEK_CUR), (-5i64, general::SEEK_END)]
                {
                    kassert_eq!(call(general::__NR_lseek, [fd as u64, delta as u64, u64::from(whence), 0, 0, 0]), 0);
                }

                // `pread64`/`pwrite64`: anonymous inodes have no position (-ESPIPE).
                kassert_eq!(call(general::__NR_pread64, [fd as u64, stat_va, 8, 0, 0, 0]), neg(errno::ESPIPE));
                kassert_eq!(call(general::__NR_pwrite64, [fd as u64, stat_va, 8, 0, 0, 0]), neg(errno::ESPIPE));

                // `mmap`: anonymous inodes cannot be mapped (-ENODEV).
                for map_flag in [general::MAP_SHARED, general::MAP_PRIVATE] {
                    kassert_eq!(
                        call(
                            general::__NR_mmap,
                            [0, 4096, u64::from(general::PROT_READ), u64::from(map_flag), fd as u64, 0],
                        ),
                        neg(errno::ENODEV)
                    );
                }

                // Reopening through `/proc/<pid>/fd/<fd>`: -ENXIO for O_RDWR, succeeds for O_PATH.
                let proc_path = format!("/proc/{pid}/fd/{fd}\0");
                copy_to_user(path_va, proc_path.as_bytes()).map_err(|_| "copy proc path")?;
                kassert_eq!(
                    call(
                        general::__NR_openat,
                        [general::AT_FDCWD as u64, path_va, u64::from(general::O_RDWR), 0, 0, 0],
                    ),
                    neg(errno::ENXIO)
                );
                let path_fd = call(
                    general::__NR_openat,
                    [general::AT_FDCWD as u64, path_va, u64::from(general::O_PATH), 0, 0, 0],
                );
                kassert!((path_fd as i64) >= 0);
                kassert_eq!(call(general::__NR_close, [path_fd, 0, 0, 0, 0, 0]), 0);

                let target = process::process_fd_target(pid, fd).ok_or("proc fd target missing")?;
                kassert_eq!(target.1.as_slice(), expected_name.as_bytes());

                process::close_current_fd(dup_fd).map_err(|_| "close dup_fd")?;
                process::close_current_fd(fd).map_err(|_| "close fd")?;
            }
            Ok(())
        })?
    }
);
