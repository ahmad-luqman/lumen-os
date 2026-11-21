//! Writable `ext4` VFS adapter over `ext4plus = "=0.1.0-rc.2"` and `lumen_bcache::BlockCache`.
//!
//! Enforces `docs/ARCHITECTURE.md` §9 lock ordering (`Lock 3 rename_lock ->
//! Lock 4 VfsInode (dirs asc lock_id -> non-dirs asc lock_id) -> Lock 5
//! PageCache -> Lock 6 Ext4 -> Lock 7 BlockCache -> Lock 8 BlockDevice`),
//! Gate 6 open-unlinked detach to `PageCache`, `>= 60B` block symlinks +
//! `<= 60B` inline symlink `readlink`/`unlink` workarounds, truncate tail
//! zeroing, and explicit `rmdir` / `rename` (`0`, `NOREPLACE`, `EXCHANGE`).

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::error::Error;
use core::fmt;
use core::num::NonZeroU32;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use core::time::Duration;

use ext4plus::dir::Dir;
use ext4plus::error::Ext4Error;
use ext4plus::file::{read_at as ext4_read_at, truncate as ext4_truncate, write_at as ext4_write_at};
use ext4plus::inode::{Inode, InodeCreationOptions, InodeFlags, InodeMode};
use ext4plus::path::PathBuf;
use ext4plus::{DirEntryName, Ext4, Ext4Read, Ext4Write, FileType, IncompatibleFeatures};
use lumen_bcache::{BlockCache, BlockDevice, BlockError};
use lumen_vfs::{
    AdvisoryLocks, DevId, DirEntry, FileSystem, Ino, InodeType, Metadata, RenameFlags, SetTime, Timespec, VfsError,
    VfsInode, next_detached_ino, next_lock_id,
};

use crate::sync::{SleepMutex, SleepMutexGuard};
use crate::time;

use super::page_cache::{self, CachePage, DetachedQuotaGuard, PAGE_SIZE, PageCache};

/// Abstraction over `SleepMutex<BlockCache<B>>` allowing `Ext4FileSystem` to
/// mount over both `virtio-blk` devices and in-memory test `BlockDevice`s.
pub trait BlockCacheBackend: Send + Sync {
    fn read_at(&self, byte_off: u64, dst: &mut [u8]) -> Result<(), BlockError>;
    fn write_at(&self, byte_off: u64, src: &[u8]) -> Result<(), BlockError>;
    fn sync(&self) -> Result<(), BlockError>;
}

impl<B: BlockDevice + Send + Sync + 'static> BlockCacheBackend for SleepMutex<BlockCache<B>> {
    fn read_at(&self, byte_off: u64, dst: &mut [u8]) -> Result<(), BlockError> {
        self.lock().read_at(byte_off, dst)
    }

    fn write_at(&self, byte_off: u64, src: &[u8]) -> Result<(), BlockError> {
        self.lock().write_at(byte_off, src)
    }

    fn sync(&self) -> Result<(), BlockError> {
        self.lock().sync()
    }
}

type SharedBlockCache = Arc<dyn BlockCacheBackend>;

const KNOWN_RO_COMPAT_BITS: u32 = 0x1_bfff;
const DISALLOWED_RW_RO_COMPAT_BITS: u32 = 0x0004 | 0x0010 | 0x0100 | 0x0200 | 0x2000;

#[derive(Debug)]
struct BcacheIoError(BlockError);

impl fmt::Display for BcacheIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "block cache error: {}", self.0)
    }
}

impl Error for BcacheIoError {}

#[derive(Clone)]
struct BcacheIo(SharedBlockCache);

impl Ext4Read for BcacheIo {
    fn read(&self, start_byte: u64, dst: &mut [u8]) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
        self.0.read_at(start_byte, dst).map_err(|e| Box::new(BcacheIoError(e)) as _)
    }
}

impl Ext4Write for BcacheIo {
    fn write(&self, start_byte: u64, src: &[u8]) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
        self.0.write_at(start_byte, src).map_err(|e| Box::new(BcacheIoError(e)) as _)
    }
}

fn map_ext4_err(err: Ext4Error) -> VfsError {
    match err {
        Ext4Error::NotFound => VfsError::NoEnt,
        Ext4Error::AlreadyExists => VfsError::Exist,
        Ext4Error::IsADirectory => VfsError::IsDir,
        Ext4Error::NotADirectory => VfsError::NotDir,
        Ext4Error::Readonly => VfsError::RoFs,
        Ext4Error::NoSpace => VfsError::NoSpc,
        Ext4Error::PathTooLong => VfsError::NameTooLong,
        Ext4Error::NotASymlink | Ext4Error::MalformedPath | Ext4Error::NotAbsolute | Ext4Error::DotEntry => {
            VfsError::Inval
        }
        Ext4Error::TooManySymlinks => VfsError::Loop,
        Ext4Error::FileTooLarge => VfsError::FBig,
        Ext4Error::Io(ref inner) => match inner.downcast_ref::<BcacheIoError>() {
            Some(BcacheIoError(BlockError::ReadOnly)) => VfsError::RoFs,
            Some(BcacheIoError(BlockError::Unaligned | BlockError::OutOfRange)) => VfsError::Inval,
            _ => VfsError::Io,
        },
        _ => VfsError::Io,
    }
}

#[derive(Clone, Copy)]
struct Ext4DiskLayout {
    block_size: u64,
    inode_size: u16,
    inodes_per_group: u32,
    desc_size: u16,
    is_64bit: bool,
}

impl Ext4DiskLayout {
    fn from_superblock_bytes(sb: &[u8; 1024]) -> Result<Self, VfsError> {
        let log_bs = u32::from_le_bytes(sb[0x18..0x1c].try_into().unwrap());
        let block_size = 1024u64.checked_shl(log_bs).ok_or(VfsError::Inval)?;
        let inodes_per_group = u32::from_le_bytes(sb[0x28..0x2c].try_into().unwrap());
        if inodes_per_group == 0 {
            return Err(VfsError::Inval);
        }
        let inode_size = u16::from_le_bytes(sb[0x58..0x5a].try_into().unwrap());
        let incompat = u32::from_le_bytes(sb[0x60..0x64].try_into().unwrap());
        let is_64bit = (incompat & 0x80) != 0;
        let desc_size = if is_64bit { u16::from_le_bytes(sb[0xfe..0x100].try_into().unwrap()) } else { 32 };
        if desc_size < 32 || (is_64bit && desc_size != 64) {
            return Err(VfsError::Inval);
        }
        Ok(Self { block_size, inode_size, inodes_per_group, desc_size, is_64bit })
    }

    fn inode_disk_offset(&self, bcache: &SharedBlockCache, ino: u32) -> Result<u64, VfsError> {
        let ino_m1 = ino.checked_sub(1).ok_or(VfsError::Inval)?;
        let bg_idx = ino_m1 / self.inodes_per_group;
        let idx_in_group = ino_m1 % self.inodes_per_group;

        let bgd_start_block: u64 = if self.block_size == 1024 { 2 } else { 1 };
        let desc_sz = u64::from(self.desc_size);
        let bgd_per_block = self.block_size / desc_sz;
        let bgd_block = bgd_start_block + u64::from(bg_idx) / bgd_per_block;
        let bgd_off = (u64::from(bg_idx) % bgd_per_block) * desc_sz;
        let bgd_byte = bgd_block * self.block_size + bgd_off;

        let mut bgd = [0u8; 64];
        let read_len = usize::from(self.desc_size).min(bgd.len());
        bcache.read_at(bgd_byte, &mut bgd[..read_len]).map_err(|_| VfsError::Io)?;

        let lo = u32::from_le_bytes(bgd[0x8..0xc].try_into().unwrap());
        let hi =
            if self.is_64bit && read_len >= 0x2c { u32::from_le_bytes(bgd[0x28..0x2c].try_into().unwrap()) } else { 0 };
        let inode_table_block = (u64::from(hi) << 32) | u64::from(lo);
        let byte_off = inode_table_block
            .checked_mul(self.block_size)
            .and_then(|b| b.checked_add(u64::from(idx_in_group) * u64::from(self.inode_size)))
            .ok_or(VfsError::Inval)?;
        Ok(byte_off)
    }
}

fn duration_to_timespec(d: Duration) -> Timespec {
    Timespec { sec: d.as_secs() as i64, nsec: i64::from(d.subsec_nanos()) }
}

fn timespec_to_duration(ts: Timespec) -> Duration {
    if ts.sec <= 0 {
        Duration::from_secs(0)
    } else {
        Duration::new(ts.sec as u64, ts.nsec.clamp(0, 999_999_999) as u32)
    }
}

fn realtime_duration() -> Duration {
    let (sec, nsec) = time::realtime_sec_nsec();
    Duration::new(sec, (nsec as u32).min(999_999_999))
}

fn map_file_type(ft: FileType) -> InodeType {
    match ft {
        FileType::Regular => InodeType::File,
        FileType::Directory => InodeType::Dir,
        FileType::Symlink => InodeType::Symlink,
        FileType::CharacterDevice => InodeType::CharDevice,
        FileType::BlockDevice => InodeType::BlockDevice,
        FileType::Fifo => InodeType::Fifo,
        FileType::Socket => InodeType::Socket,
    }
}

fn is_inline_symlink(inode: &Inode) -> bool {
    inode.file_type().is_symlink()
        && !inode.flags().contains(InodeFlags::EXTENTS)
        && inode.blocks() == 0
        && inode.size_in_bytes() <= 60
}

fn entry_name(name: &str) -> Result<DirEntryName<'_>, VfsError> {
    if name.len() > lumen_vfs::NAME_MAX {
        return Err(VfsError::NameTooLong);
    }
    DirEntryName::try_from(name).map_err(|_| VfsError::Inval)
}

struct Ext4FsInner {
    ext4: Ext4,
    bcache: SharedBlockCache,
    layout: Ext4DiskLayout,
    live_inodes: BTreeMap<u32, Weak<Ext4Inode>>,
    nlink_floors: BTreeMap<u32, u32>,
}

impl Ext4FsInner {
    fn nlink_floor(&self, disk_ino: u32) -> u32 {
        self.nlink_floors.get(&disk_ino).copied().unwrap_or(0)
    }

    fn record_nlink_floor(&mut self, disk_ino: u32, min_nlink: u32) {
        if min_nlink == 0 {
            return;
        }
        self.nlink_floors.entry(disk_ino).and_modify(|cur| *cur = (*cur).max(min_nlink)).or_insert(min_nlink);
    }

    fn set_nlink_floor(&mut self, disk_ino: u32, floor: u32) {
        if floor == 0 {
            self.nlink_floors.remove(&disk_ino);
        } else {
            self.nlink_floors.insert(disk_ino, floor);
        }
    }

    fn clear_nlink_floor(&mut self, disk_ino: u32) {
        self.nlink_floors.remove(&disk_ino);
    }

    fn get_or_create_inode(&mut self, disk: Inode, shared: &Arc<Ext4Shared>) -> Arc<Ext4Inode> {
        let disk_ino = disk.index.get();
        if let Some(weak) = self.live_inodes.get(&disk_ino)
            && let Some(existing) = weak.upgrade()
        {
            return existing;
        }
        // Opportunistically prune dead weak entries when table grows.
        if self.live_inodes.len() > 256 {
            self.live_inodes.retain(|_, w| w.strong_count() > 0);
        }
        let floor = self.nlink_floor(disk_ino);
        let arc = Ext4Inode::from_disk_inode(Arc::clone(shared), &disk, self.layout.block_size as u32, floor);
        self.live_inodes.insert(disk_ino, Arc::downgrade(&arc));
        arc
    }

    fn snapshot_live(&mut self) -> Vec<Arc<Ext4Inode>> {
        let mut out = Vec::with_capacity(self.live_inodes.len());
        self.live_inodes.retain(|_, w| {
            if let Some(arc) = w.upgrade() {
                out.push(arc);
                true
            } else {
                false
            }
        });
        out
    }
}

struct Ext4Shared {
    dev: DevId,
    read_only: bool,
    state: SleepMutex<Ext4FsInner>,
}

/// Mounted writable (or read-only) `ext4` filesystem over a [`BlockCache`].
pub struct Ext4FileSystem {
    shared: Arc<Ext4Shared>,
    rename_lock: SleepMutex<()>,
    root: Arc<Ext4Inode>,
}

impl Ext4FileSystem {
    /// Probe and mount an `ext4` filesystem on `bcache` as `dev`.
    pub fn mount(bcache: SharedBlockCache, dev: DevId, hw_read_only: bool) -> Result<Arc<Self>, VfsError> {
        let mut raw_sb = [0u8; 1024];
        bcache.read_at(1024, &mut raw_sb).map_err(|_| VfsError::Io)?;
        let magic = u16::from_le_bytes(raw_sb[0x38..0x3a].try_into().unwrap());
        if magic != 0xef53 {
            return Err(VfsError::Inval);
        }
        let layout = Ext4DiskLayout::from_superblock_bytes(&raw_sb)?;

        let io = BcacheIo(Arc::clone(&bcache));
        // Probe read-only first to inspect superblock feature flags safely.
        let probe_fs = Ext4::load_with_writer(Box::new(io.clone()), None).map_err(map_ext4_err)?;
        let feats = probe_fs.superblock().features();
        let ro_compat_bits = feats.read_only_compatible().bits();
        let sb_read_only = feats.incompatible().contains(IncompatibleFeatures::RECOVERY)
            || (ro_compat_bits & !KNOWN_RO_COMPAT_BITS) != 0
            || (ro_compat_bits & DISALLOWED_RW_RO_COMPAT_BITS) != 0;
        let read_only = hw_read_only || sb_read_only;

        let ext4 = if read_only {
            probe_fs
        } else {
            drop(probe_fs);
            Ext4::load_with_writer(Box::new(io.clone()), Some(Box::new(io))).map_err(map_ext4_err)?
        };

        let root_disk = ext4.read_root_inode().map_err(map_ext4_err)?;
        let shared = Arc::new(Ext4Shared {
            dev,
            read_only,
            state: SleepMutex::new(Ext4FsInner {
                ext4,
                bcache,
                layout,
                live_inodes: BTreeMap::new(),
                nlink_floors: BTreeMap::new(),
            }),
        });

        let root = {
            let mut inner = shared.state.lock();
            inner.get_or_create_inode(root_disk, &shared)
        };

        Ok(Arc::new(Self { shared, rename_lock: SleepMutex::new(()), root }))
    }

    pub fn lookup_live_by_lock_id(&self, lock_id: u64) -> Option<Arc<Ext4Inode>> {
        let inner = self.shared.state.lock();
        inner.live_inodes.values().filter_map(Weak::upgrade).find(|n| n.lock_id == lock_id)
    }
}

impl FileSystem for Ext4FileSystem {
    fn dev_id(&self) -> DevId {
        self.shared.dev
    }

    fn name(&self) -> &'static str {
        "ext4"
    }

    fn root_inode(&self) -> Arc<dyn VfsInode> {
        Arc::clone(&self.root) as Arc<dyn VfsInode>
    }

    fn sync(&self) -> Result<(), VfsError> {
        if self.shared.read_only {
            return Ok(());
        }
        let live = {
            let mut inner = self.shared.state.lock();
            inner.snapshot_live()
        };
        for inode in live {
            if !inode.is_detached() {
                inode.flush_to_ext4(false)?;
            }
        }
        let bcache = {
            let inner = self.shared.state.lock();
            Arc::clone(&inner.bcache)
        };
        bcache.sync().map_err(|_| VfsError::Io)?;
        Ok(())
    }

    fn read_only(&self) -> bool {
        self.shared.read_only
    }

    fn with_rename_lock(&self, f: &mut dyn FnMut() -> Result<(), VfsError>) -> Result<(), VfsError> {
        let _guard = self.rename_lock.lock();
        f()
    }
}

struct Ext4InodeMeta {
    mode: u16,
    nlink: u32,
    uid: u32,
    gid: u32,
    size: u64,
    blksize: u32,
    blocks_512: u64,
    atime: Timespec,
    mtime: Timespec,
    ctime: Timespec,
    meta_dirty: bool,
    detached_symlink_target: Vec<u8>,
    locks: AdvisoryLocks,
}

/// VFS inode backed by `ext4` + unified `PageCache`.
///
/// `Drop` NEVER acquires any sleeping lock.
pub struct Ext4Inode {
    lock_id: u64,
    shared: Arc<Ext4Shared>,
    disk_ino: u32,
    ino: AtomicU64,
    inode_type: InodeType,
    open_count: AtomicU32,
    detached: AtomicBool,
    meta: SleepMutex<Ext4InodeMeta>,
    pages: SleepMutex<PageCache>,
}

impl Ext4Inode {
    fn from_disk_inode(shared: Arc<Ext4Shared>, disk: &Inode, blksize: u32, nlink_floor: u32) -> Arc<Self> {
        let disk_ino = disk.index.get();
        let meta = Ext4InodeMeta {
            mode: disk.mode().bits() & 0o7777,
            nlink: u32::from(disk.links_count()).max(nlink_floor),
            uid: disk.uid(),
            gid: disk.gid(),
            size: disk.size_in_bytes(),
            blksize,
            blocks_512: disk.blocks(),
            atime: duration_to_timespec(disk.atime()),
            mtime: duration_to_timespec(disk.mtime()),
            ctime: duration_to_timespec(disk.ctime()),
            meta_dirty: false,
            detached_symlink_target: Vec::new(),
            locks: AdvisoryLocks::new(),
        };
        Arc::new(Self {
            lock_id: next_lock_id(),
            shared,
            disk_ino,
            ino: AtomicU64::new(u64::from(disk_ino)),
            inode_type: map_file_type(disk.file_type()),
            open_count: AtomicU32::new(0),
            detached: AtomicBool::new(false),
            meta: SleepMutex::new(meta),
            pages: SleepMutex::new(PageCache::new_ext4()),
        })
    }

    fn disk_inode_index(&self) -> NonZeroU32 {
        NonZeroU32::new(self.disk_ino).expect("ext4 inode index is non-zero")
    }

    /// Reclaim up to `max_evict` clean unpinned `PageCache` pages (Lock 4 -> Lock 5).
    pub fn reclaim_clean_pages(&self, max_evict: usize) -> usize {
        if self.is_detached() || self.inode_type != InodeType::File {
            return 0;
        }
        let meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return 0;
        }
        let mut pages = self.pages.lock();
        pages.evict_clean_oldest(max_evict)
    }

    /// Flush dirty pages to `ext4` (Lock 4 -> Lock 5 -> Lock 6 -> 7 -> 8) and
    /// evict up to `max_evict` unpinned clean pages.
    pub fn reclaim_dirty_pages(&self, max_evict: usize) -> usize {
        if self.is_detached() || self.inode_type != InodeType::File || self.shared.read_only {
            return 0;
        }
        let mut meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return 0;
        }
        let file_size = meta.size;
        let mut pages = self.pages.lock();
        let dirty = pages.collect_dirty();
        let mut flushed_indices = Vec::new();
        for (page_idx, page) in dirty {
            let page_off = page_idx * PAGE_SIZE as u64;
            if page_off >= file_size {
                continue;
            }
            let valid = (file_size - page_off).min(PAGE_SIZE as u64) as usize;
            let _was_dirty = page.swap_dirty(false);
            let mut bounce = [0u8; PAGE_SIZE];
            page.read_bytes(0, &mut bounce[..valid]);
            let write_res = {
                let inner = self.shared.state.lock();
                let mut disk = match Inode::read(&inner.ext4, self.disk_inode_index()) {
                    Ok(d) => d,
                    Err(_) => {
                        page.set_dirty();
                        return 0;
                    }
                };
                ext4_write_at(&inner.ext4, &mut disk, &bounce[..valid], page_off).map(|_| disk.blocks())
            };
            match write_res {
                Ok(blocks_512) => {
                    meta.blocks_512 = blocks_512;
                }
                Err(_) => {
                    page.set_dirty();
                    return 0;
                }
            }
            flushed_indices.push(page_idx);
        }
        let mut evicted = pages.evict_if_clean_unpinned(&flushed_indices);
        if evicted < max_evict {
            evicted += pages.evict_clean_oldest(max_evict - evicted);
        }
        evicted
    }

    /// Flush all dirty `PageCache` pages and authoritative metadata to `ext4`
    /// under `Lock 4 -> Lock 5 -> Lock 6 -> Lock 7 -> Lock 8`.
    fn flush_to_ext4(&self, data_only: bool) -> Result<(), VfsError> {
        if self.is_detached() || self.shared.read_only {
            return Ok(());
        }
        let mut meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Ok(());
        }
        let file_size = meta.size;
        if self.inode_type == InodeType::File {
            let pages = self.pages.lock();
            let dirty = pages.collect_dirty();
            for (page_idx, page) in dirty {
                let page_off = page_idx * PAGE_SIZE as u64;
                if page_off >= file_size {
                    continue;
                }
                let valid = (file_size - page_off).min(PAGE_SIZE as u64) as usize;
                let _was_dirty = page.swap_dirty(false);
                let mut bounce = [0u8; PAGE_SIZE];
                page.read_bytes(0, &mut bounce[..valid]);

                let res = {
                    let inner = self.shared.state.lock();
                    Inode::read(&inner.ext4, self.disk_inode_index())
                        .and_then(|mut disk| {
                            ext4_write_at(&inner.ext4, &mut disk, &bounce[..valid], page_off).map(|_| disk.blocks())
                        })
                        .map_err(map_ext4_err)
                };
                match res {
                    Ok(blocks_512) => {
                        meta.blocks_512 = blocks_512;
                    }
                    Err(e) => {
                        page.set_dirty();
                        return Err(e);
                    }
                }
            }
        }

        if meta.meta_dirty || !data_only {
            let inner = self.shared.state.lock();
            let mut disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
            if self.inode_type == InodeType::File && disk.size_in_bytes() != meta.size {
                ext4_truncate(&inner.ext4, &mut disk, meta.size).map_err(map_ext4_err)?;
            }
            if meta.meta_dirty {
                let full_mode = (self.inode_type.to_mode_bits() as u16) | (meta.mode & 0o7777);
                let _ = disk.set_mode(InodeMode::from_bits_retain(full_mode));
                disk.set_uid(meta.uid);
                disk.set_gid(meta.gid);
                disk.set_atime(timespec_to_duration(meta.atime));
                disk.set_mtime(timespec_to_duration(meta.mtime));
                disk.set_ctime(timespec_to_duration(meta.ctime));
                disk.write(&inner.ext4).map_err(map_ext4_err)?;
                meta.blocks_512 = disk.blocks();
                meta.meta_dirty = false;
            }
        }
        Ok(())
    }

    fn read_inline_symlink_unlocked(inner: &Ext4FsInner, disk: &Inode) -> Result<Vec<u8>, VfsError> {
        let len = disk.size_in_bytes() as usize;
        if len == 0 {
            return Ok(Vec::new());
        }
        if len > 60 {
            return Err(VfsError::Inval);
        }
        let off = inner.layout.inode_disk_offset(&inner.bcache, disk.index.get())?;
        let mut buf = vec![0u8; len];
        inner.bcache.read_at(off + 0x28, &mut buf).map_err(|_| VfsError::Io)?;
        Ok(buf)
    }

    fn zero_inline_symlink_before_unlink(
        inner: &Ext4FsInner,
        disk_ino: u32,
        disk: Inode,
        child_nlink: u32,
    ) -> Result<(Inode, Option<SavedInlineSymlink>), VfsError> {
        let idx = NonZeroU32::new(disk_ino).ok_or(VfsError::Inval)?;
        if child_nlink <= 1 && disk.links_count() <= 1 && is_inline_symlink(&disk) {
            let off = inner.layout.inode_disk_offset(&inner.bcache, disk_ino)?;
            let mut inline_60 = [0u8; 60];
            let mut size_lo_4 = [0u8; 4];
            let mut size_hi_4 = [0u8; 4];
            inner.bcache.read_at(off + 0x28, &mut inline_60).map_err(|_| VfsError::Io)?;
            inner.bcache.read_at(off + 0x04, &mut size_lo_4).map_err(|_| VfsError::Io)?;
            inner.bcache.read_at(off + 0x6c, &mut size_hi_4).map_err(|_| VfsError::Io)?;
            let saved = SavedInlineSymlink { off, inline_60, size_lo_4, size_hi_4 };

            let zeros60 = [0u8; 60];
            let zero4 = [0u8; 4];
            if inner.bcache.write_at(off + 0x28, &zeros60).is_err()
                || inner.bcache.write_at(off + 0x04, &zero4).is_err()
                || inner.bcache.write_at(off + 0x6c, &zero4).is_err()
            {
                saved.restore(inner);
                return Err(VfsError::Io);
            }
            return match Inode::read(&inner.ext4, idx) {
                Ok(zeroed_disk) => Ok((zeroed_disk, Some(saved))),
                Err(e) => {
                    saved.restore(inner);
                    Err(map_ext4_err(e))
                }
            };
        }
        Ok((disk, None))
    }

    /// Phase 1 of Gate 6 detach-while-open: fault non-hole pages (or symlink
    /// target bytes) and reserve `DETACHED_QUOTA` WITHOUT mutating `detached`,
    /// `nlink`, `meta_dirty`, `live_inodes`, or `PageCache` ownership yet.
    ///
    /// If the subsequent disk unlink/rmdir/rename fails, dropping the returned
    /// `PreparedDetach` automatically releases the `DETACHED_QUOTA` reservation.
    ///
    /// Caller holds Lock 3 and ordered Lock 4 on `target` (`_meta_guard`).
    fn prepare_detach<'a>(
        target: &'a Arc<Ext4Inode>,
        meta_guard: &Ext4InodeMeta,
    ) -> Result<Option<PreparedDetach<'a>>, VfsError> {
        if target.is_detached() {
            return Ok(None);
        }
        let payload = match target.inode_type {
            InodeType::File => {
                let pages = target.pages.lock();
                let expected_existing_pages = pages.len();
                let mut quota_guard = DetachedQuotaGuard::new();
                quota_guard.try_reserve(expected_existing_pages)?;

                let file_size = meta_guard.size;
                let mut faulted: Vec<(u64, Arc<CachePage>)> = Vec::new();
                if file_size > 0 {
                    let disk_has_data = {
                        let inner = target.shared.state.lock();
                        let disk = Inode::read(&inner.ext4, target.disk_inode_index()).map_err(map_ext4_err)?;
                        disk.blocks() > 0 && disk.size_in_bytes() > 0
                    };
                    if disk_has_data {
                        let num_pages = file_size.div_ceil(PAGE_SIZE as u64);
                        let mut tmp = vec![0u8; PAGE_SIZE];
                        for p in 0..num_pages {
                            if pages.get(p).is_some() {
                                continue;
                            }
                            let page_off = p * PAGE_SIZE as u64;
                            let chunk = (file_size - page_off).min(PAGE_SIZE as u64) as usize;
                            tmp[..chunk].fill(0);
                            {
                                let inner = target.shared.state.lock();
                                let disk = Inode::read(&inner.ext4, target.disk_inode_index()).map_err(map_ext4_err)?;
                                let mut read_off = 0usize;
                                while read_off < chunk {
                                    let n = ext4_read_at(
                                        &inner.ext4,
                                        &disk,
                                        &mut tmp[read_off..chunk],
                                        page_off + read_off as u64,
                                    )
                                    .map_err(map_ext4_err)?;
                                    if n == 0 {
                                        break;
                                    }
                                    read_off += n;
                                }
                            }
                            if tmp[..chunk].iter().any(|&b| b != 0) {
                                quota_guard.try_reserve(1)?;
                                let cp = Arc::new(CachePage::new_zeroed().ok_or(VfsError::NoMem)?);
                                cp.write_bytes(0, &tmp[..chunk]);
                                faulted.push((p, cp));
                            }
                        }
                    }
                }
                PreparedDetachPayload::File { expected_existing_pages, faulted, quota_guard }
            }
            InodeType::Symlink => {
                let inner = target.shared.state.lock();
                let disk = Inode::read(&inner.ext4, target.disk_inode_index()).map_err(map_ext4_err)?;
                let target_bytes = if is_inline_symlink(&disk) {
                    Self::read_inline_symlink_unlocked(&inner, &disk)?
                } else {
                    inner.ext4.read_inode_file(&disk).map_err(map_ext4_err)?
                };
                PreparedDetachPayload::Symlink { target_bytes }
            }
            _ => PreparedDetachPayload::Other,
        };
        Ok(Some(PreparedDetach { target, payload }))
    }

    /// Phase 3 (infallible) of Gate 6 detach-while-open: commit `PageCache`
    /// ownership, remove from `live_inodes`, assign synthetic high-bit `ino`,
    /// and mark `detached = true` (`nlink = 0`, `meta_dirty = false`) AFTER
    /// the disk unlink/rmdir/rename succeeds.
    ///
    /// Caller holds Lock 3 and ordered Lock 4 on `prepared.target` (`meta_guard`).
    fn commit_detach(prepared: PreparedDetach<'_>, meta_guard: &mut Ext4InodeMeta) {
        match prepared.payload {
            PreparedDetachPayload::File { expected_existing_pages, faulted, quota_guard } => {
                let mut pages = prepared.target.pages.lock();
                debug_assert_eq!(pages.len(), expected_existing_pages);
                pages.commit_detached(faulted);
                quota_guard.commit();
            }
            PreparedDetachPayload::Symlink { target_bytes } => {
                meta_guard.detached_symlink_target = target_bytes;
            }
            PreparedDetachPayload::Other => {}
        }

        {
            let mut inner = prepared.target.shared.state.lock();
            inner.live_inodes.remove(&prepared.target.disk_ino);
        }
        prepared.target.detached.store(true, Ordering::Release);
        prepared.target.ino.store(next_detached_ino(), Ordering::Release);
        meta_guard.nlink = 0;
        meta_guard.meta_dirty = false;
    }

    fn unlink_nondir_with_recovery(
        inner: &mut Ext4FsInner,
        parent_ino: NonZeroU32,
        dname: DirEntryName<'_>,
        child_arc: &Arc<Ext4Inode>,
        child_nlink: u32,
    ) -> Result<UnlinkDiskOutcome, VfsError> {
        let parent_disk = Inode::read(&inner.ext4, parent_ino).map_err(map_ext4_err)?;
        let mut parent_dir = Dir::open_inode(&inner.ext4, parent_disk).map_err(map_ext4_err)?;
        let mut child_disk = Inode::read(&inner.ext4, child_arc.disk_inode_index()).map_err(map_ext4_err)?;
        let floor = inner.nlink_floor(child_arc.disk_ino);
        let effective_nlink = u32::from(child_disk.links_count()).max(child_nlink).max(floor);
        if u32::from(child_disk.links_count()) < effective_nlink {
            child_disk.set_links_count(u16::try_from(effective_nlink).unwrap_or(u16::MAX));
            child_disk.write(&inner.ext4).map_err(map_ext4_err)?;
        }
        let (child_disk, saved_inline) =
            Self::zero_inline_symlink_before_unlink(inner, child_arc.disk_ino, child_disk, effective_nlink)?;
        let orig_child_links = child_disk.links_count();
        match parent_dir.unlink(dname, child_disk) {
            Ok(rem) => {
                if let Some(ref rem_disk) = rem {
                    let expected_after = effective_nlink.saturating_sub(1);
                    if u32::from(rem_disk.links_count()) >= expected_after {
                        inner.clear_nlink_floor(child_arc.disk_ino);
                    } else {
                        inner.set_nlink_floor(child_arc.disk_ino, expected_after);
                    }
                } else {
                    inner.live_inodes.remove(&child_arc.disk_ino);
                    inner.clear_nlink_floor(child_arc.disk_ino);
                }
                Ok(UnlinkDiskOutcome::Completed { remaining: rem, post_err: None })
            }
            Err(e) => {
                let err = map_ext4_err(e);
                let probe = Inode::read(&inner.ext4, parent_ino)
                    .and_then(|p| Dir::open_inode(&inner.ext4, p))
                    .and_then(|d| d.get_entry(dname));
                match probe {
                    Ok(entry) if entry.index.get() == child_arc.disk_ino => {
                        if let Some(saved) = saved_inline {
                            saved.restore(inner);
                        }
                        let target_links = orig_child_links.max(u16::try_from(effective_nlink).unwrap_or(u16::MAX));
                        let target_u32 = u32::from(target_links);
                        match Inode::read(&inner.ext4, child_arc.disk_inode_index()) {
                            Ok(mut cur) => {
                                if cur.links_count() != target_links {
                                    cur.set_links_count(target_links);
                                    if cur.write(&inner.ext4).is_ok() {
                                        inner.clear_nlink_floor(child_arc.disk_ino);
                                    } else {
                                        inner.record_nlink_floor(child_arc.disk_ino, target_u32);
                                    }
                                } else {
                                    inner.clear_nlink_floor(child_arc.disk_ino);
                                }
                            }
                            Err(_) => {
                                inner.record_nlink_floor(child_arc.disk_ino, target_u32);
                            }
                        }
                        // Entry is still present on disk: never downgrade meta.nlink
                        // below orig_child_links even if restoring disk links fails.
                        Ok(UnlinkDiskOutcome::Aborted { err, actual_nlink: None })
                    }
                    Err(Ext4Error::NotFound) => {
                        // `remove_dir_entry` already committed on disk before
                        // `delete_file` / `free_inode` failed. Do NOT write
                        // `saved_inline` or `orig_child_links` onto the freed inode.
                        let remaining = if effective_nlink > 1 || orig_child_links > 1 {
                            match Inode::read(&inner.ext4, child_arc.disk_inode_index()) {
                                Ok(cur) if cur.links_count() > 0 => Some(cur),
                                _ => None,
                            }
                        } else {
                            None
                        };
                        if let Some(ref rem_disk) = remaining {
                            let expected_after = effective_nlink.saturating_sub(1);
                            if u32::from(rem_disk.links_count()) >= expected_after {
                                inner.clear_nlink_floor(child_arc.disk_ino);
                            } else {
                                inner.set_nlink_floor(child_arc.disk_ino, expected_after);
                            }
                        } else {
                            inner.live_inodes.remove(&child_arc.disk_ino);
                            inner.clear_nlink_floor(child_arc.disk_ino);
                        }
                        Ok(UnlinkDiskOutcome::Completed { remaining, post_err: Some(err) })
                    }
                    Ok(_) | Err(_) => {
                        inner.record_nlink_floor(child_arc.disk_ino, u32::from(orig_child_links).max(effective_nlink));
                        Ok(UnlinkDiskOutcome::Aborted { err, actual_nlink: None })
                    }
                }
            }
        }
    }

    fn unlink_dir_with_recovery(
        inner: &mut Ext4FsInner,
        parent_ino: NonZeroU32,
        dname: DirEntryName<'_>,
        child_arc: &Arc<Ext4Inode>,
        child_nlink: u32,
    ) -> Result<RmdirDiskOutcome, VfsError> {
        let mut child_disk = Inode::read(&inner.ext4, child_arc.disk_inode_index()).map_err(map_ext4_err)?;
        let orig_child_links = child_disk.links_count();
        let floor = inner.nlink_floor(child_arc.disk_ino);
        let effective_links = u32::from(orig_child_links).max(child_nlink).max(floor);
        let effective_links_u16 = u16::try_from(effective_links).unwrap_or(u16::MAX);
        let pre_unlink_links = if effective_links > 2 { effective_links_u16 } else { 1 };
        if child_disk.links_count() != pre_unlink_links {
            child_disk.set_links_count(pre_unlink_links);
            child_disk.write(&inner.ext4).map_err(map_ext4_err)?;
        }

        let mut remaining = None;
        let unlink_res = (|| -> Result<(), Ext4Error> {
            let parent_disk = Inode::read(&inner.ext4, parent_ino)?;
            let mut parent_dir = Dir::open_inode(&inner.ext4, parent_disk)?;
            remaining = parent_dir.unlink(dname, child_disk.clone())?;
            Ok(())
        })();

        if let Err(e) = unlink_res {
            let err = map_ext4_err(e);
            let probe = Inode::read(&inner.ext4, parent_ino)
                .and_then(|p| Dir::open_inode(&inner.ext4, p))
                .and_then(|d| d.get_entry(dname));
            return match probe {
                Ok(entry) if entry.index.get() == child_arc.disk_ino => {
                    let rollback_links = effective_links_u16.max(orig_child_links);
                    let rollback_u32 = u32::from(rollback_links);
                    match Inode::read(&inner.ext4, child_arc.disk_inode_index()) {
                        Ok(mut cur) => {
                            if cur.links_count() != rollback_links {
                                cur.set_links_count(rollback_links);
                                if cur.write(&inner.ext4).is_ok() {
                                    inner.clear_nlink_floor(child_arc.disk_ino);
                                } else {
                                    inner.record_nlink_floor(child_arc.disk_ino, rollback_u32);
                                }
                            } else {
                                inner.clear_nlink_floor(child_arc.disk_ino);
                            }
                        }
                        Err(_) => {
                            inner.record_nlink_floor(child_arc.disk_ino, rollback_u32);
                        }
                    }
                    Ok(RmdirDiskOutcome::StillPresent { err })
                }
                Err(Ext4Error::NotFound) => {
                    // `remove_dir_entry` already committed on disk before
                    // `delete_file` / `free_inode` failed. Do NOT write
                    // `orig_child_links` onto the freed inode unless a surviving link remains.
                    let remaining = if effective_links > 2 {
                        match Inode::read(&inner.ext4, child_arc.disk_inode_index()) {
                            Ok(cur) if cur.links_count() > 0 => Some(cur),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    if let Some(ref rem_disk) = remaining {
                        let expected_after = effective_links.saturating_sub(1);
                        if u32::from(rem_disk.links_count()) >= expected_after {
                            inner.clear_nlink_floor(child_arc.disk_ino);
                        } else {
                            inner.set_nlink_floor(child_arc.disk_ino, expected_after);
                        }
                    } else {
                        inner.live_inodes.remove(&child_arc.disk_ino);
                        inner.clear_nlink_floor(child_arc.disk_ino);
                    }
                    Ok(RmdirDiskOutcome::Removed { remaining, parent_updated: false, post_err: Some(err) })
                }
                Ok(_) | Err(_) => {
                    inner.record_nlink_floor(child_arc.disk_ino, effective_links);
                    Ok(RmdirDiskOutcome::StillPresent { err })
                }
            };
        }

        if let Some(ref rem_disk) = remaining {
            let expected_after = effective_links.saturating_sub(1);
            if u32::from(rem_disk.links_count()) >= expected_after {
                inner.clear_nlink_floor(child_arc.disk_ino);
            } else if effective_links > 2 {
                inner.set_nlink_floor(child_arc.disk_ino, expected_after);
            }
        } else {
            inner.live_inodes.remove(&child_arc.disk_ino);
            inner.clear_nlink_floor(child_arc.disk_ino);
        }
        let post_res = (|| -> Result<(), VfsError> {
            let mut parent_after = Inode::read(&inner.ext4, parent_ino).map_err(map_ext4_err)?;
            let p_floor = inner.nlink_floor(parent_ino.get());
            let p_cur = u32::from(parent_after.links_count()).max(p_floor);
            let p_new = p_cur.saturating_sub(1);
            parent_after.set_links_count(u16::try_from(p_new).unwrap_or(u16::MAX));
            parent_after.write(&inner.ext4).map_err(map_ext4_err)?;
            if p_new >= p_floor.saturating_sub(1) {
                inner.clear_nlink_floor(parent_ino.get());
            } else if p_floor > 0 {
                inner.set_nlink_floor(parent_ino.get(), p_floor.saturating_sub(1));
            }
            Ok(())
        })();
        Ok(RmdirDiskOutcome::Removed { remaining, parent_updated: post_res.is_ok(), post_err: post_res.err() })
    }
}

enum UnlinkDiskOutcome {
    Completed { remaining: Option<Inode>, post_err: Option<VfsError> },
    Aborted { err: VfsError, actual_nlink: Option<u32> },
}

enum RmdirDiskOutcome {
    Removed { remaining: Option<Inode>, parent_updated: bool, post_err: Option<VfsError> },
    StillPresent { err: VfsError },
}

struct SavedInlineSymlink {
    off: u64,
    inline_60: [u8; 60],
    size_lo_4: [u8; 4],
    size_hi_4: [u8; 4],
}

impl SavedInlineSymlink {
    fn restore(&self, inner: &Ext4FsInner) {
        let _ = inner.bcache.write_at(self.off + 0x28, &self.inline_60);
        let _ = inner.bcache.write_at(self.off + 0x04, &self.size_lo_4);
        let _ = inner.bcache.write_at(self.off + 0x6c, &self.size_hi_4);
    }
}

enum PreparedDetachPayload {
    File { expected_existing_pages: usize, faulted: Vec<(u64, Arc<CachePage>)>, quota_guard: DetachedQuotaGuard },
    Symlink { target_bytes: Vec<u8> },
    Other,
}

struct PreparedDetach<'a> {
    target: &'a Arc<Ext4Inode>,
    payload: PreparedDetachPayload,
}

struct LockedInode<'a> {
    inode: &'a Arc<Ext4Inode>,
    guard: SleepMutexGuard<'a, Ext4InodeMeta>,
}

/// Lock distinct `Ext4Inode`s in strict Lock-4 order:
/// `dirs ascending lock_id -> non-dirs ascending lock_id`.
fn lock_inodes_ordered<'a>(inodes: &[&'a Arc<Ext4Inode>]) -> Vec<LockedInode<'a>> {
    let mut unique: Vec<&'a Arc<Ext4Inode>> = Vec::with_capacity(inodes.len());
    for &ino in inodes {
        if !unique.iter().any(|x| x.lock_id == ino.lock_id) {
            unique.push(ino);
        }
    }
    unique.sort_by_key(|x| (x.inode_type != InodeType::Dir, x.lock_id));
    unique.into_iter().map(|inode| LockedInode { inode, guard: inode.meta.lock() }).collect()
}

fn find_guard_mut<'a, 'b>(guards: &'b mut [LockedInode<'a>], target: &Arc<Ext4Inode>) -> &'b mut Ext4InodeMeta {
    &mut guards.iter_mut().find(|g| g.inode.lock_id == target.lock_id).expect("locked inode present").guard
}

fn is_ext4_dir_empty(ext4: &Ext4, dir_inode: &Inode) -> Result<bool, VfsError> {
    let dir = Dir::open_inode(ext4, dir_inode.clone()).map_err(map_ext4_err)?;
    let rd = dir.read_dir().map_err(map_ext4_err)?;
    for item in rd {
        let entry = item.map_err(map_ext4_err)?;
        let name = entry.file_name();
        if name != "." && name != ".." {
            return Ok(false);
        }
    }
    Ok(true)
}

fn update_dir_dotdot(ext4: &Ext4, dir_ino: u32, new_parent_ino: u32) -> Result<(), VfsError> {
    let mut dir_inode = Inode::read(ext4, NonZeroU32::new(dir_ino).ok_or(VfsError::Inval)?).map_err(map_ext4_err)?;
    let mut hdr = [0u8; 24];
    let mut off = 0usize;
    while off < hdr.len() {
        let n = ext4_read_at(ext4, &dir_inode, &mut hdr[off..], off as u64).map_err(map_ext4_err)?;
        if n == 0 {
            return Err(VfsError::Io);
        }
        off += n;
    }
    // Verify standard block-0 `..` entry at offset 12..22 (`name_len == 2`, `FT_DIR == 2`, `".."`).
    if hdr[18] != 2 || hdr[19] != 2 || &hdr[20..22] != b".." {
        return Err(VfsError::Io);
    }
    let bytes = new_parent_ino.to_le_bytes();
    let written = ext4_write_at(ext4, &mut dir_inode, &bytes, 12).map_err(map_ext4_err)?;
    if written != 4 {
        return Err(VfsError::Io);
    }
    Ok(())
}

fn read_disk_nlink(ext4: &Ext4, ino: NonZeroU32) -> Option<u32> {
    Inode::read(ext4, ino).ok().map(|d| u32::from(d.links_count()))
}

fn dir_has_entry(
    ext4: &Ext4,
    parent_ino: NonZeroU32,
    dname: DirEntryName<'_>,
    expected_ino: u32,
) -> Result<bool, Ext4Error> {
    let parent = Inode::read(ext4, parent_ino)?;
    let dir = Dir::open_inode(ext4, parent)?;
    match dir.get_entry(dname) {
        Ok(entry) => Ok(entry.index.get() == expected_ino),
        Err(Ext4Error::NotFound) => Ok(false),
        Err(e) => Err(e),
    }
}

fn dir_entry_absent(ext4: &Ext4, parent_ino: NonZeroU32, dname: DirEntryName<'_>) -> bool {
    let Ok(parent) = Inode::read(ext4, parent_ino) else {
        return false;
    };
    let Ok(dir) = Dir::open_inode(ext4, parent) else {
        return false;
    };
    matches!(dir.get_entry(dname), Err(Ext4Error::NotFound))
}

fn rollback_link_entry(
    ext4: &Ext4,
    parent_ino: NonZeroU32,
    dname: DirEntryName<'_>,
    child_ino: NonZeroU32,
    min_links_before_unlink: u16,
) {
    if matches!(dir_has_entry(ext4, parent_ino, dname, child_ino.get()), Ok(true))
        && let Ok(mut child_disk) = Inode::read(ext4, child_ino)
    {
        let needed = min_links_before_unlink.max(2);
        if child_disk.links_count() < needed {
            child_disk.set_links_count(needed);
            if child_disk.write(ext4).is_err() {
                return;
            }
        }
        if child_disk.links_count() >= 2
            && let Ok(parent_disk) = Inode::read(ext4, parent_ino)
            && let Ok(mut parent_dir) = Dir::open_inode(ext4, parent_disk)
        {
            let _ = parent_dir.unlink(dname, child_disk);
        }
    }
}

fn rollback_restore_entry(ext4: &Ext4, parent_ino: NonZeroU32, dname: DirEntryName<'_>, child_ino: NonZeroU32) {
    if dir_entry_absent(ext4, parent_ino, dname)
        && let Ok(mut child_disk) = Inode::read(ext4, child_ino)
        && let Ok(parent_disk) = Inode::read(ext4, parent_ino)
        && let Ok(mut parent_dir) = Dir::open_inode(ext4, parent_disk)
    {
        let _ = parent_dir.link(dname, &mut child_disk);
    }
}

fn restore_disk_links_count(ext4: &Ext4, ino: NonZeroU32, target_links: u16) -> Result<(), VfsError> {
    let mut disk = Inode::read(ext4, ino).map_err(map_ext4_err)?;
    if disk.links_count() != target_links {
        disk.set_links_count(target_links);
        disk.write(ext4).map_err(map_ext4_err)?;
    }
    Ok(())
}

fn ensure_min_disk_links_count(ext4: &Ext4, ino: NonZeroU32, min_links: u16) -> Result<(), VfsError> {
    let mut disk = Inode::read(ext4, ino).map_err(map_ext4_err)?;
    if disk.links_count() < min_links {
        disk.set_links_count(min_links);
        disk.write(ext4).map_err(map_ext4_err)?;
    }
    Ok(())
}

impl VfsInode for Ext4Inode {
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
        let blocks = if meta.meta_dirty && self.inode_type == InodeType::File {
            meta.blocks_512.max(meta.size.div_ceil(512))
        } else {
            meta.blocks_512
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
            blksize: meta.blksize,
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
        let meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let dname = entry_name(name)?;
        let mut inner = self.shared.state.lock();
        let dir_inode = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
        let dir = Dir::open_inode(&inner.ext4, dir_inode).map_err(map_ext4_err)?;
        let child_disk = dir.get_entry(dname).map_err(map_ext4_err)?;
        let child = inner.get_or_create_inode(child_disk, &self.shared);
        Ok(child as Arc<dyn VfsInode>)
    }

    fn create(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.shared.read_only {
            return Err(VfsError::RoFs);
        }
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if name == "." || name == ".." {
            return Err(VfsError::Exist);
        }
        let mut meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let dname = entry_name(name)?;
        let now_dur = realtime_duration();
        let now_ts = duration_to_timespec(now_dur);

        let mut inner = self.shared.state.lock();
        let parent_disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
        let mut parent_dir = Dir::open_inode(&inner.ext4, parent_disk).map_err(map_ext4_err)?;
        if parent_dir.get_entry(dname).is_ok() {
            return Err(VfsError::Exist);
        }

        let mut child_disk = inner
            .ext4
            .create_inode(InodeCreationOptions {
                file_type: FileType::Regular,
                mode: InodeMode::S_IFREG | InodeMode::from_bits_retain(mode & 0o7777),
                uid: 0,
                gid: 0,
                time: now_dur,
                flags: InodeFlags::empty(),
            })
            .map_err(map_ext4_err)?;
        inner.clear_nlink_floor(child_disk.index.get());
        parent_dir.link(dname, &mut child_disk).map_err(map_ext4_err)?;
        let child = inner.get_or_create_inode(child_disk, &self.shared);
        meta.mtime = now_ts;
        meta.ctime = now_ts;
        meta.meta_dirty = true;
        Ok(child as Arc<dyn VfsInode>)
    }

    fn mkdir(&self, name: &str, mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.shared.read_only {
            return Err(VfsError::RoFs);
        }
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if name == "." || name == ".." {
            return Err(VfsError::Exist);
        }
        let mut meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let dname = entry_name(name)?;
        let now_dur = realtime_duration();
        let now_ts = duration_to_timespec(now_dur);

        let mut inner = self.shared.state.lock();
        let mut parent_disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
        let parent_floor = inner.nlink_floor(self.disk_ino);
        if u32::from(parent_disk.links_count()) < parent_floor {
            parent_disk.set_links_count(u16::try_from(parent_floor).unwrap_or(u16::MAX));
            parent_disk.write(&inner.ext4).map_err(map_ext4_err)?;
        }
        let parent_check = Dir::open_inode(&inner.ext4, parent_disk).map_err(map_ext4_err)?;
        if parent_check.get_entry(dname).is_ok() {
            return Err(VfsError::Exist);
        }
        drop(parent_check);

        let sub_raw = inner
            .ext4
            .create_inode(InodeCreationOptions {
                file_type: FileType::Directory,
                mode: InodeMode::S_IFDIR | InodeMode::from_bits_retain(mode & 0o7777),
                uid: 0,
                gid: 0,
                time: now_dur,
                flags: InodeFlags::empty(),
            })
            .map_err(map_ext4_err)?;
        inner.clear_nlink_floor(sub_raw.index.get());
        let sub_dir = Dir::init(inner.ext4.clone(), sub_raw, self.disk_inode_index()).map_err(map_ext4_err)?;
        let sub_ino = sub_dir.inode().index;
        drop(sub_dir);

        let mut sub_inode = Inode::read(&inner.ext4, sub_ino).map_err(map_ext4_err)?;
        let parent_disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
        let mut parent_dir = Dir::open_inode(&inner.ext4, parent_disk).map_err(map_ext4_err)?;
        parent_dir.link(dname, &mut sub_inode).map_err(map_ext4_err)?;
        if parent_floor > 0 {
            inner.clear_nlink_floor(self.disk_ino);
        }
        let sub_fresh = Inode::read(&inner.ext4, sub_ino).map_err(map_ext4_err)?;
        let child = inner.get_or_create_inode(sub_fresh, &self.shared);
        meta.nlink = meta.nlink.saturating_add(1);
        meta.mtime = now_ts;
        meta.ctime = now_ts;
        meta.meta_dirty = true;
        Ok(child as Arc<dyn VfsInode>)
    }

    fn unlink(&self, name: &str) -> Result<(), VfsError> {
        if self.shared.read_only {
            return Err(VfsError::RoFs);
        }
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if name == "." || name == ".." {
            return Err(VfsError::IsDir);
        }
        let dname = entry_name(name)?;

        let (self_arc, child_arc) = {
            let self_meta = self.meta.lock();
            if self.is_detached() || self_meta.nlink == 0 {
                return Err(VfsError::NoEnt);
            }
            let mut inner = self.shared.state.lock();
            let self_disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
            let parent_dir = Dir::open_inode(&inner.ext4, self_disk.clone()).map_err(map_ext4_err)?;
            let child_disk = parent_dir.get_entry(dname).map_err(map_ext4_err)?;
            if child_disk.file_type().is_dir() {
                return Err(VfsError::IsDir);
            }
            let self_arc = inner.get_or_create_inode(self_disk, &self.shared);
            let child_arc = inner.get_or_create_inode(child_disk, &self.shared);
            (self_arc, child_arc)
        };

        let mut guards = lock_inodes_ordered(&[&self_arc, &child_arc]);
        if self_arc.is_detached() || find_guard_mut(&mut guards, &self_arc).nlink == 0 {
            return Err(VfsError::NoEnt);
        }

        let now_ts = duration_to_timespec(realtime_duration());
        let child_nlink = find_guard_mut(&mut guards, &child_arc).nlink;
        if child_arc.is_detached() || child_nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let prepared_detach = if child_nlink <= 1 && child_arc.open_count() > 0 {
            let child_meta = find_guard_mut(&mut guards, &child_arc);
            Self::prepare_detach(&child_arc, child_meta)?
        } else {
            None
        };

        let outcome = {
            let mut inner = self.shared.state.lock();
            Self::unlink_nondir_with_recovery(&mut inner, self.disk_inode_index(), dname, &child_arc, child_nlink)?
        };

        match outcome {
            UnlinkDiskOutcome::Aborted { err, actual_nlink } => {
                drop(prepared_detach);
                if let Some(nlink) = actual_nlink
                    && !child_arc.is_detached()
                {
                    find_guard_mut(&mut guards, &child_arc).nlink = nlink;
                }
                Err(err)
            }
            UnlinkDiskOutcome::Completed { remaining, post_err } => {
                let child_meta = find_guard_mut(&mut guards, &child_arc);
                if let Some(rem) = remaining {
                    drop(prepared_detach);
                    if !child_arc.is_detached() {
                        child_meta.nlink = u32::from(rem.links_count());
                        child_meta.ctime = now_ts;
                        if child_meta.nlink > 0 {
                            child_meta.meta_dirty = true;
                        }
                    }
                } else if let Some(prepared) = prepared_detach {
                    Self::commit_detach(prepared, child_meta);
                } else if !child_arc.is_detached() {
                    child_meta.nlink = 0;
                    child_meta.ctime = now_ts;
                    child_meta.meta_dirty = false;
                }
                let self_meta = find_guard_mut(&mut guards, &self_arc);
                self_meta.mtime = now_ts;
                self_meta.ctime = now_ts;
                self_meta.meta_dirty = true;
                match post_err {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            }
        }
    }

    fn rmdir(&self, name: &str) -> Result<(), VfsError> {
        if self.shared.read_only {
            return Err(VfsError::RoFs);
        }
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if name == "." {
            return Err(VfsError::Inval);
        }
        if name == ".." {
            return Err(VfsError::NotEmpty);
        }
        let dname = entry_name(name)?;

        let (self_arc, child_arc) = {
            let self_meta = self.meta.lock();
            if self.is_detached() || self_meta.nlink == 0 {
                return Err(VfsError::NoEnt);
            }
            let mut inner = self.shared.state.lock();
            let self_disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
            let parent_dir = Dir::open_inode(&inner.ext4, self_disk.clone()).map_err(map_ext4_err)?;
            let child_disk = parent_dir.get_entry(dname).map_err(map_ext4_err)?;
            if !child_disk.file_type().is_dir() {
                return Err(VfsError::NotDir);
            }
            if !is_ext4_dir_empty(&inner.ext4, &child_disk)? {
                return Err(VfsError::NotEmpty);
            }
            let self_arc = inner.get_or_create_inode(self_disk, &self.shared);
            let child_arc = inner.get_or_create_inode(child_disk, &self.shared);
            (self_arc, child_arc)
        };

        let mut guards = lock_inodes_ordered(&[&self_arc, &child_arc]);
        if self_arc.is_detached()
            || find_guard_mut(&mut guards, &self_arc).nlink == 0
            || child_arc.is_detached()
            || find_guard_mut(&mut guards, &child_arc).nlink == 0
        {
            return Err(VfsError::NoEnt);
        }

        // Re-check emptiness with both Lock 4 mutexes held.
        {
            let inner = self.shared.state.lock();
            let child_disk = Inode::read(&inner.ext4, child_arc.disk_inode_index()).map_err(map_ext4_err)?;
            if !is_ext4_dir_empty(&inner.ext4, &child_disk)? {
                return Err(VfsError::NotEmpty);
            }
        }

        let child_nlink = find_guard_mut(&mut guards, &child_arc).nlink;
        let prepared_detach = if child_nlink <= 2 && child_arc.open_count() > 0 {
            let child_meta = find_guard_mut(&mut guards, &child_arc);
            Self::prepare_detach(&child_arc, child_meta)?
        } else {
            None
        };

        let outcome = {
            let mut inner = self.shared.state.lock();
            Self::unlink_dir_with_recovery(&mut inner, self.disk_inode_index(), dname, &child_arc, child_nlink)?
        };

        let now_ts = duration_to_timespec(realtime_duration());
        match outcome {
            RmdirDiskOutcome::StillPresent { err } => {
                drop(prepared_detach);
                Err(err)
            }
            RmdirDiskOutcome::Removed { remaining, parent_updated, post_err } => {
                let child_meta = find_guard_mut(&mut guards, &child_arc);
                if let Some(rem) = remaining {
                    drop(prepared_detach);
                    if !child_arc.is_detached() {
                        child_meta.nlink = u32::from(rem.links_count());
                        child_meta.ctime = now_ts;
                        if child_meta.nlink > 0 {
                            child_meta.meta_dirty = true;
                        }
                    }
                } else if let Some(prepared) = prepared_detach {
                    Self::commit_detach(prepared, child_meta);
                } else {
                    child_meta.nlink = 0;
                    child_meta.meta_dirty = false;
                }
                if parent_updated {
                    let self_meta = find_guard_mut(&mut guards, &self_arc);
                    self_meta.nlink = self_meta.nlink.saturating_sub(1);
                    self_meta.mtime = now_ts;
                    self_meta.ctime = now_ts;
                    self_meta.meta_dirty = true;
                }
                match post_err {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            }
        }
    }

    fn link(&self, name: &str, target: &Arc<dyn VfsInode>) -> Result<(), VfsError> {
        if self.shared.read_only {
            return Err(VfsError::RoFs);
        }
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
        if name == "." || name == ".." {
            return Err(VfsError::Exist);
        }
        let dname = entry_name(name)?;
        let target_ino = u32::try_from(target.ino()).map_err(|_| VfsError::NoEnt)?;

        let (self_arc, target_arc) = {
            let self_meta = self.meta.lock();
            if self.is_detached() || self_meta.nlink == 0 {
                return Err(VfsError::NoEnt);
            }
            let mut inner = self.shared.state.lock();
            let self_disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
            let target_disk =
                Inode::read(&inner.ext4, NonZeroU32::new(target_ino).ok_or(VfsError::NoEnt)?).map_err(map_ext4_err)?;
            let self_arc = inner.get_or_create_inode(self_disk, &self.shared);
            let target_arc = inner.get_or_create_inode(target_disk, &self.shared);
            (self_arc, target_arc)
        };

        let mut guards = lock_inodes_ordered(&[&self_arc, &target_arc]);
        if self_arc.is_detached()
            || target_arc.is_detached()
            || find_guard_mut(&mut guards, &self_arc).nlink == 0
            || find_guard_mut(&mut guards, &target_arc).nlink == 0
        {
            return Err(VfsError::NoEnt);
        }

        {
            let mut inner = self.shared.state.lock();
            let self_disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
            let mut parent_dir = Dir::open_inode(&inner.ext4, self_disk).map_err(map_ext4_err)?;
            if parent_dir.get_entry(dname).is_ok() {
                return Err(VfsError::Exist);
            }
            let mut target_disk = Inode::read(&inner.ext4, target_arc.disk_inode_index()).map_err(map_ext4_err)?;
            let floor = inner.nlink_floor(target_arc.disk_ino);
            if u32::from(target_disk.links_count()) < floor {
                target_disk.set_links_count(u16::try_from(floor).unwrap_or(u16::MAX));
            }
            parent_dir.link(dname, &mut target_disk).map_err(map_ext4_err)?;
            inner.clear_nlink_floor(target_arc.disk_ino);
        }

        let now_ts = duration_to_timespec(realtime_duration());
        let t_meta = find_guard_mut(&mut guards, &target_arc);
        t_meta.nlink = t_meta.nlink.saturating_add(1);
        t_meta.ctime = now_ts;
        t_meta.meta_dirty = true;
        let p_meta = find_guard_mut(&mut guards, &self_arc);
        p_meta.mtime = now_ts;
        p_meta.ctime = now_ts;
        p_meta.meta_dirty = true;
        Ok(())
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.shared.read_only {
            return Err(VfsError::RoFs);
        }
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
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
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let dname = entry_name(name)?;
        let now_dur = realtime_duration();
        let now_ts = duration_to_timespec(now_dur);

        let mut inner = self.shared.state.lock();
        let parent_disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
        let mut parent_dir = Dir::open_inode(&inner.ext4, parent_disk).map_err(map_ext4_err)?;
        if parent_dir.get_entry(dname).is_ok() {
            return Err(VfsError::Exist);
        }

        let sym_disk = if target.len() <= 59 {
            let pbuf = PathBuf::try_from(target.as_bytes()).map_err(|_| VfsError::Inval)?;
            let mut sym = inner.ext4.symlink(&mut parent_dir, dname, pbuf, 0, 0, now_dur).map_err(map_ext4_err)?;
            inner.clear_nlink_floor(sym.index.get());
            sym.set_mode(InodeMode::S_IFLNK | InodeMode::from_bits_retain(0o777)).map_err(map_ext4_err)?;
            sym.write(&inner.ext4).map_err(map_ext4_err)?;
            sym
        } else {
            let mut sym = inner
                .ext4
                .create_inode(InodeCreationOptions {
                    file_type: FileType::Symlink,
                    mode: InodeMode::S_IFLNK | InodeMode::from_bits_retain(0o777),
                    uid: 0,
                    gid: 0,
                    time: now_dur,
                    flags: InodeFlags::empty(),
                })
                .map_err(map_ext4_err)?;
            inner.clear_nlink_floor(sym.index.get());
            if let Err(e) = ext4_write_at(&inner.ext4, &mut sym, target.as_bytes(), 0) {
                let _ = ext4_truncate(&inner.ext4, &mut sym, 0);
                return Err(map_ext4_err(e));
            }
            if let Err(e) = parent_dir.link(dname, &mut sym) {
                let _ = ext4_truncate(&inner.ext4, &mut sym, 0);
                return Err(map_ext4_err(e));
            }
            sym
        };

        let child = inner.get_or_create_inode(sym_disk, &self.shared);
        meta.mtime = now_ts;
        meta.ctime = now_ts;
        meta.meta_dirty = true;
        Ok(child as Arc<dyn VfsInode>)
    }

    fn readlink(&self) -> Result<Vec<u8>, VfsError> {
        if self.inode_type != InodeType::Symlink {
            return Err(VfsError::Inval);
        }
        let meta = self.meta.lock();
        if self.is_detached() {
            return Ok(meta.detached_symlink_target.clone());
        }
        if meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let inner = self.shared.state.lock();
        let disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
        if is_inline_symlink(&disk) {
            Self::read_inline_symlink_unlocked(&inner, &disk)
        } else {
            inner.ext4.read_inode_file(&disk).map_err(map_ext4_err)
        }
    }

    fn rename(
        &self,
        old_name: &str,
        new_dir: &Arc<dyn VfsInode>,
        new_name: &str,
        flags: RenameFlags,
    ) -> Result<(), VfsError> {
        let flags = flags.validate()?;
        if self.shared.read_only {
            return Err(VfsError::RoFs);
        }
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if new_dir.dev_id() != self.shared.dev {
            return Err(VfsError::XDev);
        }
        if new_dir.inode_type() != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if new_dir.is_detached() {
            return Err(VfsError::NoEnt);
        }
        if old_name == "." || old_name == ".." || new_name == "." || new_name == ".." {
            return Err(VfsError::Inval);
        }
        let old_dname = entry_name(old_name)?;
        let new_dname = entry_name(new_name)?;
        let new_dir_disk_ino = u32::try_from(new_dir.ino()).map_err(|_| VfsError::NoEnt)?;

        // Phase 1: probe source, destination, and live Arc<Ext4Inode>s under Lock 6.
        let (old_dir_arc, new_dir_arc, old_child_arc, new_child_opt) = {
            let mut inner = self.shared.state.lock();
            if self.is_detached() || new_dir.is_detached() {
                return Err(VfsError::NoEnt);
            }
            let old_parent_disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
            let new_parent_disk = Inode::read(&inner.ext4, NonZeroU32::new(new_dir_disk_ino).ok_or(VfsError::NoEnt)?)
                .map_err(map_ext4_err)?;

            let old_dir_obj = Dir::open_inode(&inner.ext4, old_parent_disk.clone()).map_err(map_ext4_err)?;
            let old_child_disk = old_dir_obj.get_entry(old_dname).map_err(map_ext4_err)?;

            let new_dir_obj = Dir::open_inode(&inner.ext4, new_parent_disk.clone()).map_err(map_ext4_err)?;
            let new_child_disk_opt = match new_dir_obj.get_entry(new_dname) {
                Ok(d) => Some(d),
                Err(Ext4Error::NotFound) => None,
                Err(e) => return Err(map_ext4_err(e)),
            };

            if flags.has_noreplace() && new_child_disk_opt.is_some() {
                return Err(VfsError::Exist);
            }
            if flags.has_exchange() && new_child_disk_opt.is_none() {
                return Err(VfsError::NoEnt);
            }

            if !flags.has_exchange()
                && let Some(ref nc) = new_child_disk_opt
            {
                if nc.index == old_child_disk.index {
                    return Ok(());
                }
                if old_child_disk.file_type().is_dir() && !nc.file_type().is_dir() {
                    return Err(VfsError::NotDir);
                }
                if !old_child_disk.file_type().is_dir() && nc.file_type().is_dir() {
                    return Err(VfsError::IsDir);
                }
                if nc.file_type().is_dir() && !is_ext4_dir_empty(&inner.ext4, nc)? {
                    return Err(VfsError::NotEmpty);
                }
            }

            let old_dir_arc = inner.get_or_create_inode(old_parent_disk, &self.shared);
            let new_dir_arc = inner.get_or_create_inode(new_parent_disk, &self.shared);
            let old_child_arc = inner.get_or_create_inode(old_child_disk, &self.shared);
            let new_child_opt = new_child_disk_opt.map(|d| inner.get_or_create_inode(d, &self.shared));
            (old_dir_arc, new_dir_arc, old_child_arc, new_child_opt)
        };

        if self.disk_ino == new_dir_disk_ino && old_name == new_name {
            return Ok(());
        }

        // Phase 2: Acquire all Lock 4 mutexes in strict canonical order.
        let mut to_lock: Vec<&Arc<Ext4Inode>> = Vec::with_capacity(4);
        to_lock.push(&old_dir_arc);
        to_lock.push(&new_dir_arc);
        to_lock.push(&old_child_arc);
        if let Some(ref nc) = new_child_opt {
            to_lock.push(nc);
        }
        let mut guards = lock_inodes_ordered(&to_lock);
        if old_dir_arc.is_detached()
            || new_dir_arc.is_detached()
            || find_guard_mut(&mut guards, &old_dir_arc).nlink == 0
            || find_guard_mut(&mut guards, &new_dir_arc).nlink == 0
        {
            return Err(VfsError::NoEnt);
        }

        let now_ts = duration_to_timespec(realtime_duration());
        let same_parent = old_dir_arc.disk_ino == new_dir_arc.disk_ino;
        let old_is_dir = old_child_arc.inode_type == InodeType::Dir;

        if flags.has_exchange() {
            let new_child_arc = new_child_opt.as_ref().expect("checked Some for EXCHANGE");
            if old_child_arc.disk_ino == new_child_arc.disk_ino {
                return Ok(());
            }
            let new_is_dir = new_child_arc.inode_type == InodeType::Dir;
            struct ExchangeSync {
                old_child_nlink: Option<u32>,
                new_child_nlink: Option<u32>,
                old_dir_nlink: Option<u32>,
                new_dir_nlink: Option<u32>,
                touch_timestamps: bool,
                res: Result<(), VfsError>,
            }
            let outcome = {
                let mut inner = self.shared.state.lock();
                let old_child_ino = old_child_arc.disk_inode_index();
                let new_child_ino = new_child_arc.disk_inode_index();
                let old_dir_ino = old_dir_arc.disk_inode_index();
                let new_dir_ino = new_dir_arc.disk_inode_index();

                let mut old_disk = Inode::read(&inner.ext4, old_child_ino).map_err(map_ext4_err)?;
                let mut new_disk = Inode::read(&inner.ext4, new_child_ino).map_err(map_ext4_err)?;
                let old_p_init = Inode::read(&inner.ext4, old_dir_ino).map_err(map_ext4_err)?;
                let new_p_init = Inode::read(&inner.ext4, new_dir_ino).map_err(map_ext4_err)?;

                let old_orig_links = old_disk
                    .links_count()
                    .max(u16::try_from(inner.nlink_floor(old_child_ino.get())).unwrap_or(u16::MAX));
                let new_orig_links = new_disk
                    .links_count()
                    .max(u16::try_from(inner.nlink_floor(new_child_ino.get())).unwrap_or(u16::MAX));
                let old_p_orig_links = old_p_init
                    .links_count()
                    .max(u16::try_from(inner.nlink_floor(old_dir_ino.get())).unwrap_or(u16::MAX));
                let new_p_orig_links = new_p_init
                    .links_count()
                    .max(u16::try_from(inner.nlink_floor(new_dir_ino.get())).unwrap_or(u16::MAX));

                let rollback_exchange = |inner: &mut Ext4FsInner,
                                         undo_old_at_new: bool,
                                         undo_new_at_old: bool,
                                         restore_old_entry: bool,
                                         restore_new_entry: bool|
                 -> (Option<u32>, Option<u32>, Option<u32>, Option<u32>) {
                    if undo_new_at_old {
                        rollback_link_entry(&inner.ext4, old_dir_ino, old_dname, new_child_ino, 2);
                    }
                    if undo_old_at_new {
                        rollback_link_entry(&inner.ext4, new_dir_ino, new_dname, old_child_ino, 2);
                    }
                    if restore_old_entry {
                        rollback_restore_entry(&inner.ext4, old_dir_ino, old_dname, old_child_ino);
                    }
                    if restore_new_entry {
                        rollback_restore_entry(&inner.ext4, new_dir_ino, new_dname, new_child_ino);
                    }

                    let old_in_old_res = dir_has_entry(&inner.ext4, old_dir_ino, old_dname, old_child_ino.get());
                    let old_in_new_res = dir_has_entry(&inner.ext4, new_dir_ino, new_dname, old_child_ino.get());
                    let new_in_new_res = dir_has_entry(&inner.ext4, new_dir_ino, new_dname, new_child_ino.get());
                    let new_in_old_res = dir_has_entry(&inner.ext4, old_dir_ino, old_dname, new_child_ino.get());

                    let mut min_old_child = None;
                    let mut min_new_child = None;
                    let mut min_old_dir = None;
                    let mut min_new_dir = None;

                    if let (Ok(oo), Ok(on)) = (&old_in_old_res, &old_in_new_res) {
                        let slots = u16::from(*oo) + u16::from(*on);
                        let target = old_orig_links.saturating_sub(1).saturating_add(slots);
                        if slots >= 2 {
                            let dual_min = target.max(if old_is_dir { 3 } else { 2 });
                            if restore_disk_links_count(&inner.ext4, old_child_ino, dual_min).is_ok() {
                                inner.clear_nlink_floor(old_child_ino.get());
                            } else {
                                inner.record_nlink_floor(old_child_ino.get(), u32::from(dual_min));
                            }
                            min_old_child = Some(u32::from(dual_min));
                        } else {
                            if restore_disk_links_count(&inner.ext4, old_child_ino, target).is_ok() {
                                inner.clear_nlink_floor(old_child_ino.get());
                            } else {
                                inner.record_nlink_floor(old_child_ino.get(), u32::from(target));
                            }
                            if target > 0 {
                                min_old_child = Some(u32::from(target));
                            }
                        }
                    } else {
                        if ensure_min_disk_links_count(&inner.ext4, old_child_ino, old_orig_links).is_ok() {
                            inner.clear_nlink_floor(old_child_ino.get());
                        } else {
                            inner.record_nlink_floor(old_child_ino.get(), u32::from(old_orig_links));
                        }
                        min_old_child = Some(u32::from(old_orig_links));
                    }

                    if let (Ok(nn), Ok(no)) = (&new_in_new_res, &new_in_old_res) {
                        let slots = u16::from(*nn) + u16::from(*no);
                        let target = new_orig_links.saturating_sub(1).saturating_add(slots);
                        if slots >= 2 {
                            let dual_min = target.max(if new_is_dir { 3 } else { 2 });
                            if restore_disk_links_count(&inner.ext4, new_child_ino, dual_min).is_ok() {
                                inner.clear_nlink_floor(new_child_ino.get());
                            } else {
                                inner.record_nlink_floor(new_child_ino.get(), u32::from(dual_min));
                            }
                            min_new_child = Some(u32::from(dual_min));
                        } else {
                            if restore_disk_links_count(&inner.ext4, new_child_ino, target).is_ok() {
                                inner.clear_nlink_floor(new_child_ino.get());
                            } else {
                                inner.record_nlink_floor(new_child_ino.get(), u32::from(target));
                            }
                            if target > 0 {
                                min_new_child = Some(u32::from(target));
                            }
                        }
                    } else {
                        if ensure_min_disk_links_count(&inner.ext4, new_child_ino, new_orig_links).is_ok() {
                            inner.clear_nlink_floor(new_child_ino.get());
                        } else {
                            inner.record_nlink_floor(new_child_ino.get(), u32::from(new_orig_links));
                        }
                        min_new_child = Some(u32::from(new_orig_links));
                    }

                    if let (Ok(oo), Ok(on), Ok(nn), Ok(no)) =
                        (old_in_old_res, old_in_new_res, new_in_new_res, new_in_old_res)
                    {
                        let old_slot_has_dir = (oo && old_is_dir) || (no && new_is_dir);
                        let new_slot_has_dir = (nn && new_is_dir) || (on && old_is_dir);
                        if same_parent {
                            let expected_p = old_p_orig_links
                                .saturating_sub(u16::from(old_is_dir) + u16::from(new_is_dir))
                                .saturating_add(u16::from(old_slot_has_dir) + u16::from(new_slot_has_dir));
                            let restored = restore_disk_links_count(&inner.ext4, old_dir_ino, expected_p).is_ok();
                            if restored {
                                inner.clear_nlink_floor(old_dir_ino.get());
                            } else {
                                inner.record_nlink_floor(old_dir_ino.get(), u32::from(expected_p));
                            }
                            if expected_p > old_p_orig_links || !restored {
                                min_old_dir = Some(u32::from(expected_p));
                                min_new_dir = min_old_dir;
                            }
                        } else {
                            let expected_old_p = old_p_orig_links
                                .saturating_sub(u16::from(old_is_dir))
                                .saturating_add(u16::from(old_slot_has_dir));
                            let expected_new_p = new_p_orig_links
                                .saturating_sub(u16::from(new_is_dir))
                                .saturating_add(u16::from(new_slot_has_dir));
                            let restored_old =
                                restore_disk_links_count(&inner.ext4, old_dir_ino, expected_old_p).is_ok();
                            if restored_old {
                                inner.clear_nlink_floor(old_dir_ino.get());
                            } else {
                                inner.record_nlink_floor(old_dir_ino.get(), u32::from(expected_old_p));
                            }
                            let restored_new =
                                restore_disk_links_count(&inner.ext4, new_dir_ino, expected_new_p).is_ok();
                            if restored_new {
                                inner.clear_nlink_floor(new_dir_ino.get());
                            } else {
                                inner.record_nlink_floor(new_dir_ino.get(), u32::from(expected_new_p));
                            }
                            if expected_old_p > old_p_orig_links || !restored_old {
                                min_old_dir = Some(u32::from(expected_old_p));
                            }
                            if expected_new_p > new_p_orig_links || !restored_new {
                                min_new_dir = Some(u32::from(expected_new_p));
                            }
                        }
                    } else {
                        if ensure_min_disk_links_count(&inner.ext4, old_dir_ino, old_p_orig_links).is_ok() {
                            inner.clear_nlink_floor(old_dir_ino.get());
                        } else {
                            inner.record_nlink_floor(old_dir_ino.get(), u32::from(old_p_orig_links));
                            min_old_dir = Some(u32::from(old_p_orig_links));
                        }
                        if !same_parent {
                            if ensure_min_disk_links_count(&inner.ext4, new_dir_ino, new_p_orig_links).is_ok() {
                                inner.clear_nlink_floor(new_dir_ino.get());
                            } else {
                                inner.record_nlink_floor(new_dir_ino.get(), u32::from(new_p_orig_links));
                                min_new_dir = Some(u32::from(new_p_orig_links));
                            }
                        }
                    }

                    (min_old_child, min_new_child, min_old_dir, min_new_dir)
                };

                let sync_with_mins = |inner: &Ext4FsInner,
                                      mins: (Option<u32>, Option<u32>, Option<u32>, Option<u32>),
                                      res: Result<(), VfsError>| {
                    let clamp = |ino: NonZeroU32, disk: Option<u32>, min: Option<u32>| -> Option<u32> {
                        let floor = inner.nlink_floor(ino.get());
                        let eff_min = match min {
                            Some(m) => Some(m.max(floor)),
                            None if floor > 0 => Some(floor),
                            None => None,
                        };
                        match eff_min {
                            Some(m) => Some(disk.unwrap_or(m).max(m)),
                            None => disk,
                        }
                    };
                    ExchangeSync {
                        old_child_nlink: clamp(old_child_ino, read_disk_nlink(&inner.ext4, old_child_ino), mins.0),
                        new_child_nlink: clamp(new_child_ino, read_disk_nlink(&inner.ext4, new_child_ino), mins.1),
                        old_dir_nlink: clamp(old_dir_ino, read_disk_nlink(&inner.ext4, old_dir_ino), mins.2),
                        new_dir_nlink: clamp(new_dir_ino, read_disk_nlink(&inner.ext4, new_dir_ino), mins.3),
                        touch_timestamps: false,
                        res,
                    }
                };

                // 1. Bump both link counts so unlinking never deletes either inode.
                old_disk.set_links_count(old_orig_links.saturating_add(1));
                old_disk.write(&inner.ext4).map_err(map_ext4_err)?;
                new_disk.set_links_count(new_orig_links.saturating_add(1));
                if let Err(e) = new_disk.write(&inner.ext4) {
                    let mins = rollback_exchange(&mut inner, false, false, false, false);
                    sync_with_mins(&inner, mins, Err(map_ext4_err(e)))
                } else if let Err(e) = Inode::read(&inner.ext4, old_dir_ino)
                    .and_then(|old_p| Dir::open_inode(&inner.ext4, old_p))
                    .and_then(|mut d| d.unlink(old_dname, old_disk))
                {
                    // 2a failed.
                    let mins = rollback_exchange(&mut inner, false, false, true, false);
                    sync_with_mins(&inner, mins, Err(map_ext4_err(e)))
                } else if let Err(e) = Inode::read(&inner.ext4, new_dir_ino)
                    .and_then(|new_p| Dir::open_inode(&inner.ext4, new_p))
                    .and_then(|mut d| d.unlink(new_dname, new_disk))
                {
                    // 2b failed.
                    let mins = rollback_exchange(&mut inner, false, false, true, true);
                    sync_with_mins(&inner, mins, Err(map_ext4_err(e)))
                } else if let Err(e) = (|| -> Result<(), Ext4Error> {
                    // 3a. Link `old_child` into `new_dir` at `new_dname`.
                    let mut old_disk = Inode::read(&inner.ext4, old_child_ino)?;
                    let new_p = Inode::read(&inner.ext4, new_dir_ino)?;
                    Dir::open_inode(&inner.ext4, new_p)?.link(new_dname, &mut old_disk)
                })() {
                    let mins = rollback_exchange(&mut inner, true, false, true, true);
                    sync_with_mins(&inner, mins, Err(map_ext4_err(e)))
                } else if let Err(e) = (|| -> Result<(), Ext4Error> {
                    // 3b. Link `new_child` into `old_dir` at `old_dname`.
                    let mut new_disk = Inode::read(&inner.ext4, new_child_ino)?;
                    let old_p = Inode::read(&inner.ext4, old_dir_ino)?;
                    Dir::open_inode(&inner.ext4, old_p)?.link(old_dname, &mut new_disk)
                })() {
                    let mins = rollback_exchange(&mut inner, true, true, true, true);
                    sync_with_mins(&inner, mins, Err(map_ext4_err(e)))
                } else {
                    // Both entries are now swapped on disk. Run steps 4 and 5
                    // best-effort without `?` short-circuiting so independent fixups
                    // complete even when an earlier post-commit write fails.
                    let mut first_err: Option<VfsError> = None;
                    let mut record_err = |res: Result<(), VfsError>| {
                        if let Err(e) = res
                            && first_err.is_none()
                        {
                            first_err = Some(e);
                        }
                    };

                    // 4. Restore exact link counts on both swapped children.
                    let old_write_res = (|| -> Result<(), VfsError> {
                        let mut old_disk = Inode::read(&inner.ext4, old_child_ino).map_err(map_ext4_err)?;
                        old_disk.set_links_count(old_orig_links);
                        old_disk.write(&inner.ext4).map_err(map_ext4_err)
                    })();
                    if old_write_res.is_ok() {
                        inner.clear_nlink_floor(old_child_ino.get());
                    }
                    record_err(old_write_res);

                    let new_write_res = (|| -> Result<(), VfsError> {
                        let mut new_disk = Inode::read(&inner.ext4, new_child_ino).map_err(map_ext4_err)?;
                        new_disk.set_links_count(new_orig_links);
                        new_disk.write(&inner.ext4).map_err(map_ext4_err)
                    })();
                    if new_write_res.is_ok() {
                        inner.clear_nlink_floor(new_child_ino.get());
                    }
                    record_err(new_write_res);

                    // 5. Adjust parent link counts and `..` when moving directories.
                    if same_parent {
                        let dir_delta = u16::from(old_is_dir) + u16::from(new_is_dir);
                        if dir_delta > 0 {
                            let p_res = (|| -> Result<(), VfsError> {
                                let mut p = Inode::read(&inner.ext4, old_dir_ino).map_err(map_ext4_err)?;
                                let p_floor = inner.nlink_floor(old_dir_ino.get());
                                let p_cur = u32::from(p.links_count()).max(p_floor);
                                let p_new = p_cur.saturating_sub(u32::from(dir_delta));
                                p.set_links_count(u16::try_from(p_new).unwrap_or(u16::MAX));
                                p.write(&inner.ext4).map_err(map_ext4_err)
                            })();
                            if p_res.is_ok() {
                                inner.clear_nlink_floor(old_dir_ino.get());
                            }
                            record_err(p_res);
                        }
                    } else {
                        if old_is_dir {
                            let op_res = (|| -> Result<(), VfsError> {
                                let mut old_p = Inode::read(&inner.ext4, old_dir_ino).map_err(map_ext4_err)?;
                                let p_floor = inner.nlink_floor(old_dir_ino.get());
                                let p_cur = u32::from(old_p.links_count()).max(p_floor);
                                let p_new = p_cur.saturating_sub(1);
                                old_p.set_links_count(u16::try_from(p_new).unwrap_or(u16::MAX));
                                old_p.write(&inner.ext4).map_err(map_ext4_err)
                            })();
                            if op_res.is_ok() {
                                inner.clear_nlink_floor(old_dir_ino.get());
                            }
                            record_err(op_res);
                            record_err(update_dir_dotdot(&inner.ext4, old_child_arc.disk_ino, new_dir_arc.disk_ino));
                        }
                        if new_is_dir {
                            let np_res = (|| -> Result<(), VfsError> {
                                let mut new_p = Inode::read(&inner.ext4, new_dir_ino).map_err(map_ext4_err)?;
                                let p_floor = inner.nlink_floor(new_dir_ino.get());
                                let p_cur = u32::from(new_p.links_count()).max(p_floor);
                                let p_new = p_cur.saturating_sub(1);
                                new_p.set_links_count(u16::try_from(p_new).unwrap_or(u16::MAX));
                                new_p.write(&inner.ext4).map_err(map_ext4_err)
                            })();
                            if np_res.is_ok() {
                                inner.clear_nlink_floor(new_dir_ino.get());
                            }
                            record_err(np_res);
                            record_err(update_dir_dotdot(&inner.ext4, new_child_arc.disk_ino, old_dir_arc.disk_ino));
                        }
                    }
                    let post_res = match first_err {
                        Some(e) => Err(e),
                        None => Ok(()),
                    };
                    ExchangeSync {
                        old_child_nlink: read_disk_nlink(&inner.ext4, old_child_ino),
                        new_child_nlink: read_disk_nlink(&inner.ext4, new_child_ino),
                        old_dir_nlink: read_disk_nlink(&inner.ext4, old_dir_ino),
                        new_dir_nlink: read_disk_nlink(&inner.ext4, new_dir_ino),
                        touch_timestamps: true,
                        res: post_res,
                    }
                }
            };

            if let Some(n) = outcome.old_child_nlink {
                find_guard_mut(&mut guards, &old_child_arc).nlink = n;
            }
            if let Some(n) = outcome.new_child_nlink {
                find_guard_mut(&mut guards, new_child_arc).nlink = n;
            }
            if let Some(n) = outcome.old_dir_nlink {
                find_guard_mut(&mut guards, &old_dir_arc).nlink = n;
            }
            if let Some(n) = outcome.new_dir_nlink {
                find_guard_mut(&mut guards, &new_dir_arc).nlink = n;
            }
            if outcome.touch_timestamps {
                find_guard_mut(&mut guards, &old_dir_arc).mtime = now_ts;
                find_guard_mut(&mut guards, &old_dir_arc).ctime = now_ts;
                find_guard_mut(&mut guards, &old_dir_arc).meta_dirty = true;
                find_guard_mut(&mut guards, &new_dir_arc).mtime = now_ts;
                find_guard_mut(&mut guards, &new_dir_arc).ctime = now_ts;
                find_guard_mut(&mut guards, &new_dir_arc).meta_dirty = true;
                find_guard_mut(&mut guards, &old_child_arc).ctime = now_ts;
                find_guard_mut(&mut guards, &old_child_arc).meta_dirty = true;
                find_guard_mut(&mut guards, new_child_arc).ctime = now_ts;
                find_guard_mut(&mut guards, new_child_arc).meta_dirty = true;
            }
            return outcome.res;
        }

        // Standard overwrite / move rename.
        if let Some(ref new_child_arc) = new_child_opt {
            let new_is_dir = new_child_arc.inode_type == InodeType::Dir;
            let new_nlink = find_guard_mut(&mut guards, new_child_arc).nlink;
            let prepared_detach = if ((new_is_dir && new_nlink <= 2) || (!new_is_dir && new_nlink <= 1))
                && new_child_arc.open_count() > 0
            {
                let nc_meta = find_guard_mut(&mut guards, new_child_arc);
                Self::prepare_detach(new_child_arc, nc_meta)?
            } else {
                None
            };

            if new_is_dir {
                let outcome = {
                    let mut inner = self.shared.state.lock();
                    Self::unlink_dir_with_recovery(
                        &mut inner,
                        new_dir_arc.disk_inode_index(),
                        new_dname,
                        new_child_arc,
                        new_nlink,
                    )?
                };
                match outcome {
                    RmdirDiskOutcome::StillPresent { err } => {
                        drop(prepared_detach);
                        return Err(err);
                    }
                    RmdirDiskOutcome::Removed { remaining, parent_updated, post_err } => {
                        let nc_meta = find_guard_mut(&mut guards, new_child_arc);
                        if let Some(rem) = remaining {
                            drop(prepared_detach);
                            if !new_child_arc.is_detached() {
                                nc_meta.nlink = u32::from(rem.links_count());
                                nc_meta.ctime = now_ts;
                                if nc_meta.nlink > 0 {
                                    nc_meta.meta_dirty = true;
                                }
                            }
                        } else if let Some(prepared) = prepared_detach {
                            Self::commit_detach(prepared, nc_meta);
                        } else {
                            nc_meta.nlink = 0;
                            nc_meta.meta_dirty = false;
                        }
                        if parent_updated {
                            let np_meta = find_guard_mut(&mut guards, &new_dir_arc);
                            np_meta.nlink = np_meta.nlink.saturating_sub(1);
                            np_meta.mtime = now_ts;
                            np_meta.ctime = now_ts;
                            np_meta.meta_dirty = true;
                        }
                        if let Some(e) = post_err {
                            return Err(e);
                        }
                    }
                }
            } else {
                let outcome = {
                    let mut inner = self.shared.state.lock();
                    Self::unlink_nondir_with_recovery(
                        &mut inner,
                        new_dir_arc.disk_inode_index(),
                        new_dname,
                        new_child_arc,
                        new_nlink,
                    )?
                };
                match outcome {
                    UnlinkDiskOutcome::Aborted { err, actual_nlink } => {
                        drop(prepared_detach);
                        if let Some(nlink) = actual_nlink
                            && !new_child_arc.is_detached()
                        {
                            find_guard_mut(&mut guards, new_child_arc).nlink = nlink;
                        }
                        return Err(err);
                    }
                    UnlinkDiskOutcome::Completed { remaining, post_err } => {
                        let nc_meta = find_guard_mut(&mut guards, new_child_arc);
                        if let Some(rem) = remaining {
                            drop(prepared_detach);
                            if !new_child_arc.is_detached() {
                                nc_meta.nlink = u32::from(rem.links_count());
                                nc_meta.ctime = now_ts;
                                if nc_meta.nlink > 0 {
                                    nc_meta.meta_dirty = true;
                                }
                            }
                        } else if let Some(prepared) = prepared_detach {
                            Self::commit_detach(prepared, nc_meta);
                        } else if !new_child_arc.is_detached() {
                            nc_meta.nlink = 0;
                            nc_meta.ctime = now_ts;
                            nc_meta.meta_dirty = false;
                        }
                        let np_meta = find_guard_mut(&mut guards, &new_dir_arc);
                        np_meta.mtime = now_ts;
                        np_meta.ctime = now_ts;
                        np_meta.meta_dirty = true;
                        if let Some(e) = post_err {
                            return Err(e);
                        }
                    }
                }
            }
        }

        // Link `old_child` at `new_name` in `new_dir` FIRST, then unlink `old_name` from `old_dir`.
        struct Step2Outcome {
            moved_on_disk: bool,
            op_after_written: bool,
            sync_old_child: Option<u32>,
            sync_old_dir: Option<u32>,
            sync_new_dir: Option<u32>,
            res: Result<(), VfsError>,
        }
        let step2 = {
            let mut inner = self.shared.state.lock();
            let old_child_ino = old_child_arc.disk_inode_index();
            let new_dir_ino = new_dir_arc.disk_inode_index();
            let old_dir_ino = old_dir_arc.disk_inode_index();

            let mut old_disk = Inode::read(&inner.ext4, old_child_ino).map_err(map_ext4_err)?;
            let old_floor = inner.nlink_floor(old_child_ino.get());
            let orig_old_links = old_disk.links_count().max(u16::try_from(old_floor).unwrap_or(u16::MAX));
            if old_disk.links_count() < orig_old_links {
                old_disk.set_links_count(orig_old_links);
                old_disk.write(&inner.ext4).map_err(map_ext4_err)?;
            }
            let np_disk = Inode::read(&inner.ext4, new_dir_ino).map_err(map_ext4_err)?;
            let np_floor = inner.nlink_floor(new_dir_ino.get());
            let orig_np_links = np_disk.links_count().max(u16::try_from(np_floor).unwrap_or(u16::MAX));

            let rollback_step2_nlinks = |inner: &mut Ext4FsInner| -> (Option<u32>, Option<u32>, Option<u32>) {
                rollback_link_entry(
                    &inner.ext4,
                    new_dir_ino,
                    new_dname,
                    old_child_ino,
                    orig_old_links.saturating_add(1),
                );
                let new_still_has = dir_has_entry(&inner.ext4, new_dir_ino, new_dname, old_child_ino.get());
                match new_still_has {
                    Ok(true) => {
                        let min_old = orig_old_links.saturating_add(1).max(if old_is_dir { 3 } else { 2 });
                        if ensure_min_disk_links_count(&inner.ext4, old_child_ino, min_old).is_ok() {
                            inner.clear_nlink_floor(old_child_ino.get());
                        } else {
                            inner.record_nlink_floor(old_child_ino.get(), u32::from(min_old));
                        }
                        if old_is_dir {
                            let min_np = orig_np_links.saturating_add(1);
                            if ensure_min_disk_links_count(&inner.ext4, new_dir_ino, min_np).is_ok() {
                                inner.clear_nlink_floor(new_dir_ino.get());
                            } else {
                                inner.record_nlink_floor(new_dir_ino.get(), u32::from(min_np));
                            }
                        }
                    }
                    Ok(false) => {
                        if restore_disk_links_count(&inner.ext4, old_child_ino, orig_old_links).is_ok() {
                            inner.clear_nlink_floor(old_child_ino.get());
                        } else {
                            inner.record_nlink_floor(old_child_ino.get(), u32::from(orig_old_links));
                        }
                        if old_is_dir {
                            if restore_disk_links_count(&inner.ext4, new_dir_ino, orig_np_links).is_ok() {
                                inner.clear_nlink_floor(new_dir_ino.get());
                            } else {
                                inner.record_nlink_floor(new_dir_ino.get(), u32::from(orig_np_links));
                            }
                        }
                    }
                    Err(_) => {
                        if ensure_min_disk_links_count(&inner.ext4, old_child_ino, orig_old_links).is_ok() {
                            inner.clear_nlink_floor(old_child_ino.get());
                        } else {
                            inner.record_nlink_floor(old_child_ino.get(), u32::from(orig_old_links));
                        }
                        if old_is_dir && np_floor > 0 {
                            if ensure_min_disk_links_count(&inner.ext4, new_dir_ino, orig_np_links).is_ok() {
                                inner.clear_nlink_floor(new_dir_ino.get());
                            } else {
                                inner.record_nlink_floor(new_dir_ino.get(), u32::from(orig_np_links));
                            }
                        }
                    }
                }
                let clamp_floor = |ino: NonZeroU32, disk: Option<u32>| -> Option<u32> {
                    let fl = inner.nlink_floor(ino.get());
                    if fl > 0 { Some(disk.unwrap_or(fl).max(fl)) } else { disk }
                };
                let both_present =
                    matches!(dir_has_entry(&inner.ext4, old_dir_ino, old_dname, old_child_ino.get()), Ok(true))
                        && matches!(new_still_has, Ok(true));
                let mut sync_old_child = clamp_floor(old_child_ino, read_disk_nlink(&inner.ext4, old_child_ino));
                let mut sync_new_dir = clamp_floor(new_dir_ino, read_disk_nlink(&inner.ext4, new_dir_ino));
                let sync_old_dir = clamp_floor(old_dir_ino, read_disk_nlink(&inner.ext4, old_dir_ino));
                if both_present {
                    let min_links = u32::from(orig_old_links.saturating_add(1)).max(if old_is_dir { 3 } else { 2 });
                    sync_old_child = Some(sync_old_child.unwrap_or(min_links).max(min_links));
                    if old_is_dir {
                        let min_np = u32::from(orig_np_links.saturating_add(1));
                        sync_new_dir = Some(sync_new_dir.unwrap_or(min_np).max(min_np));
                    }
                }
                (sync_old_child, sync_old_dir, sync_new_dir)
            };

            let link_res = Dir::open_inode(&inner.ext4, np_disk).and_then(|mut d| d.link(new_dname, &mut old_disk));
            if let Err(e) = link_res {
                let (sync_old_child, sync_old_dir, sync_new_dir) = rollback_step2_nlinks(&mut inner);
                Step2Outcome {
                    moved_on_disk: false,
                    op_after_written: false,
                    sync_old_child,
                    sync_old_dir,
                    sync_new_dir,
                    res: Err(map_ext4_err(e)),
                }
            } else {
                let unlink_res = Inode::read(&inner.ext4, old_dir_ino)
                    .and_then(|op_disk| Dir::open_inode(&inner.ext4, op_disk))
                    .and_then(|mut d| d.unlink(old_dname, old_disk));
                match unlink_res {
                    Err(e) => {
                        let err = map_ext4_err(e);
                        let old_probe = Inode::read(&inner.ext4, old_dir_ino)
                            .and_then(|op| Dir::open_inode(&inner.ext4, op))
                            .and_then(|d| d.get_entry(old_dname));
                        match old_probe {
                            Ok(entry) if entry.index.get() == old_child_arc.disk_ino => {
                                let (sync_old_child, sync_old_dir, sync_new_dir) = rollback_step2_nlinks(&mut inner);
                                Step2Outcome {
                                    moved_on_disk: false,
                                    op_after_written: false,
                                    sync_old_child,
                                    sync_old_dir,
                                    sync_new_dir,
                                    res: Err(err),
                                }
                            }
                            Err(Ext4Error::NotFound) => {
                                // `old_dname` is already gone on disk: the move committed.
                                // Do NOT undo `link(new_dname)`; sync `nlink`s from disk.
                                let child_disk_n = read_disk_nlink(&inner.ext4, old_child_ino);
                                if child_disk_n.is_some_and(|n| n >= u32::from(orig_old_links)) {
                                    inner.clear_nlink_floor(old_child_ino.get());
                                } else {
                                    inner.record_nlink_floor(old_child_ino.get(), u32::from(orig_old_links));
                                }
                                Step2Outcome {
                                    moved_on_disk: true,
                                    op_after_written: false,
                                    sync_old_child: child_disk_n.map(|n| n.max(u32::from(orig_old_links))),
                                    sync_old_dir: read_disk_nlink(&inner.ext4, old_dir_ino),
                                    sync_new_dir: read_disk_nlink(&inner.ext4, new_dir_ino),
                                    res: Err(err),
                                }
                            }
                            Ok(_) | Err(_) => {
                                inner.record_nlink_floor(old_child_ino.get(), u32::from(orig_old_links));
                                Step2Outcome {
                                    moved_on_disk: false,
                                    op_after_written: false,
                                    sync_old_child: read_disk_nlink(&inner.ext4, old_child_ino)
                                        .map(|n| n.max(u32::from(orig_old_links))),
                                    sync_old_dir: read_disk_nlink(&inner.ext4, old_dir_ino),
                                    sync_new_dir: read_disk_nlink(&inner.ext4, new_dir_ino),
                                    res: Err(err),
                                }
                            }
                        }
                    }
                    Ok(_) => {
                        inner.clear_nlink_floor(old_child_ino.get());
                        let mut op_after_written = false;
                        let post_res = if old_is_dir {
                            if !same_parent
                                && np_floor > 0
                                && read_disk_nlink(&inner.ext4, new_dir_ino).is_some_and(|n| n >= np_floor)
                            {
                                inner.clear_nlink_floor(new_dir_ino.get());
                            }
                            let mut first_err: Option<VfsError> = None;
                            let op_res = (|| -> Result<(), VfsError> {
                                let mut op_after = Inode::read(&inner.ext4, old_dir_ino).map_err(map_ext4_err)?;
                                let op_floor = inner.nlink_floor(old_dir_ino.get());
                                let op_cur = u32::from(op_after.links_count()).max(op_floor);
                                let op_new = op_cur.saturating_sub(1);
                                op_after.set_links_count(u16::try_from(op_new).unwrap_or(u16::MAX));
                                op_after.write(&inner.ext4).map_err(map_ext4_err)?;
                                if op_new >= op_floor.saturating_sub(1) {
                                    inner.clear_nlink_floor(old_dir_ino.get());
                                } else if op_floor > 0 {
                                    inner.set_nlink_floor(old_dir_ino.get(), op_floor.saturating_sub(1));
                                }
                                Ok(())
                            })();
                            match op_res {
                                Ok(()) => op_after_written = true,
                                Err(e) => first_err = Some(e),
                            }
                            if !same_parent
                                && let Err(e) =
                                    update_dir_dotdot(&inner.ext4, old_child_arc.disk_ino, new_dir_arc.disk_ino)
                                && first_err.is_none()
                            {
                                first_err = Some(e);
                            }
                            match first_err {
                                Some(e) => Err(e),
                                None => Ok(()),
                            }
                        } else {
                            Ok(())
                        };
                        Step2Outcome {
                            moved_on_disk: true,
                            op_after_written,
                            sync_old_child: None,
                            sync_old_dir: None,
                            sync_new_dir: None,
                            res: post_res,
                        }
                    }
                }
            }
        };

        if let Some(n) = step2.sync_old_child {
            find_guard_mut(&mut guards, &old_child_arc).nlink = n;
        }
        if let Some(n) = step2.sync_old_dir {
            find_guard_mut(&mut guards, &old_dir_arc).nlink = n;
        }
        if let Some(n) = step2.sync_new_dir {
            find_guard_mut(&mut guards, &new_dir_arc).nlink = n;
        }

        if step2.moved_on_disk {
            if old_is_dir && step2.sync_old_dir.is_none() && step2.sync_new_dir.is_none() {
                if !same_parent {
                    if step2.op_after_written {
                        find_guard_mut(&mut guards, &old_dir_arc).nlink =
                            find_guard_mut(&mut guards, &old_dir_arc).nlink.saturating_sub(1);
                    }
                    find_guard_mut(&mut guards, &new_dir_arc).nlink =
                        find_guard_mut(&mut guards, &new_dir_arc).nlink.saturating_add(1);
                } else if !step2.op_after_written {
                    find_guard_mut(&mut guards, &old_dir_arc).nlink =
                        find_guard_mut(&mut guards, &old_dir_arc).nlink.saturating_add(1);
                }
            }
            find_guard_mut(&mut guards, &old_dir_arc).mtime = now_ts;
            find_guard_mut(&mut guards, &old_dir_arc).ctime = now_ts;
            find_guard_mut(&mut guards, &old_dir_arc).meta_dirty = true;
            find_guard_mut(&mut guards, &new_dir_arc).mtime = now_ts;
            find_guard_mut(&mut guards, &new_dir_arc).ctime = now_ts;
            find_guard_mut(&mut guards, &new_dir_arc).meta_dirty = true;
            find_guard_mut(&mut guards, &old_child_arc).ctime = now_ts;
            find_guard_mut(&mut guards, &old_child_arc).meta_dirty = true;
        }
        step2.res
    }

    fn readdir(&self, cursor: u64, out: &mut Vec<DirEntry>, max_entries: usize) -> Result<(), VfsError> {
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        let meta = self.meta.lock();
        if self.is_detached() {
            return Ok(());
        }
        if meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        if max_entries == 0 {
            return Ok(());
        }
        let inner = self.shared.state.lock();
        let disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
        let dir = Dir::open_inode(&inner.ext4, disk).map_err(map_ext4_err)?;
        let rd = dir.read_dir().map_err(map_ext4_err)?;
        for (idx, item) in rd.enumerate() {
            let entry = item.map_err(map_ext4_err)?;
            let cur_idx = idx as u64;
            if cur_idx < cursor {
                continue;
            }
            let Ok(name_str) = entry.file_name().as_str() else {
                continue;
            };
            let inode_type = match entry.file_type() {
                Ok(ft) => map_file_type(ft),
                Err(_) => InodeType::File,
            };
            out.push(DirEntry {
                ino: u64::from(entry.inode.get()),
                inode_type,
                name: String::from(name_str),
                next_cursor: cur_idx + 1,
            });
            if out.len() >= max_entries {
                break;
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
        if self.shared.read_only && !self.is_detached() {
            return Err(VfsError::RoFs);
        }
        let mut meta = self.meta.lock();
        let detached = self.is_detached();
        if !detached && meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let mut pages = self.pages.lock();
        let old_size = meta.size;

        if new_size < old_size {
            let tail_rem = (new_size % PAGE_SIZE as u64) as usize;
            if !detached {
                let inner = self.shared.state.lock();
                let mut disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
                // `ext4plus::truncate` does not zero `[new_size % 4096 .. 4096)`
                // on disk; zero non-zero disk tail bytes (confined to `check_len`
                // so sub-page files are never temporarily extended to 4096)
                // before truncating, and restore them if `ext4_truncate` fails.
                let mut saved_tail: Option<Vec<u8>> = None;
                if tail_rem != 0 && disk.size_in_bytes() > new_size {
                    let tail_end = (new_size / PAGE_SIZE as u64 + 1) * PAGE_SIZE as u64;
                    let check_len = (disk.size_in_bytes().min(tail_end) - new_size) as usize;
                    if check_len > 0 {
                        let mut probe = vec![0u8; check_len];
                        let mut got = 0usize;
                        while got < check_len {
                            let n = ext4_read_at(&inner.ext4, &disk, &mut probe[got..], new_size + got as u64)
                                .map_err(map_ext4_err)?;
                            if n == 0 {
                                break;
                            }
                            got += n;
                        }
                        if probe[..got].iter().any(|&b| b != 0) {
                            let zeros = vec![0u8; got];
                            if let Err(e) = ext4_write_at(&inner.ext4, &mut disk, &zeros, new_size) {
                                let _ = ext4_write_at(&inner.ext4, &mut disk, &probe[..got], new_size);
                                return Err(map_ext4_err(e));
                            }
                            probe.truncate(got);
                            saved_tail = Some(probe);
                        }
                    }
                }
                if let Err(e) = ext4_truncate(&inner.ext4, &mut disk, new_size) {
                    if let Some(orig_tail) = saved_tail {
                        let mut restore_disk = Inode::read(&inner.ext4, self.disk_inode_index()).unwrap_or(disk);
                        let _ = ext4_write_at(&inner.ext4, &mut restore_disk, &orig_tail, new_size);
                    }
                    return Err(map_ext4_err(e));
                }
                meta.blocks_512 = disk.blocks();
            }
            pages.remove_from(new_size.div_ceil(PAGE_SIZE as u64));
            if tail_rem != 0 {
                let tail_page = new_size / PAGE_SIZE as u64;
                if let Some(page) = pages.get(tail_page) {
                    page.zero_range(tail_rem, PAGE_SIZE);
                    if !detached {
                        page.set_dirty();
                    }
                }
            }
        } else if new_size > old_size {
            let tail_rem = (old_size % PAGE_SIZE as u64) as usize;
            if !detached {
                let inner = self.shared.state.lock();
                let mut disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
                ext4_truncate(&inner.ext4, &mut disk, new_size).map_err(map_ext4_err)?;
                meta.blocks_512 = disk.blocks();
            }
            if tail_rem != 0 {
                let tail_page = old_size / PAGE_SIZE as u64;
                if let Some(page) = pages.get(tail_page) {
                    page.zero_range(tail_rem, PAGE_SIZE);
                    if !detached {
                        page.set_dirty();
                    }
                }
            }
        }

        let now_ts = duration_to_timespec(realtime_duration());
        meta.size = new_size;
        meta.mtime = now_ts;
        meta.ctime = now_ts;
        if !detached {
            meta.meta_dirty = true;
        }
        Ok(())
    }

    fn set_mode(&self, mode: u16) -> Result<(), VfsError> {
        if self.shared.read_only && !self.is_detached() {
            return Err(VfsError::RoFs);
        }
        let mut meta = self.meta.lock();
        if !self.is_detached() && meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let now_dur = realtime_duration();
        let now_ts = duration_to_timespec(now_dur);
        let perm = mode & 0o7777;
        if !self.is_detached() {
            let inner = self.shared.state.lock();
            let mut disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
            let full_mode = (self.inode_type.to_mode_bits() as u16) | perm;
            disk.set_mode(InodeMode::from_bits_retain(full_mode)).map_err(map_ext4_err)?;
            disk.set_ctime(now_dur);
            disk.write(&inner.ext4).map_err(map_ext4_err)?;
        }
        meta.mode = perm;
        meta.ctime = now_ts;
        Ok(())
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), VfsError> {
        if self.shared.read_only && !self.is_detached() {
            return Err(VfsError::RoFs);
        }
        let mut meta = self.meta.lock();
        if !self.is_detached() && meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let now_dur = realtime_duration();
        let now_ts = duration_to_timespec(now_dur);
        let new_uid = uid.unwrap_or(meta.uid);
        let new_gid = gid.unwrap_or(meta.gid);
        if !self.is_detached() {
            let inner = self.shared.state.lock();
            let mut disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
            disk.set_uid(new_uid);
            disk.set_gid(new_gid);
            disk.set_ctime(now_dur);
            disk.write(&inner.ext4).map_err(map_ext4_err)?;
        }
        meta.uid = new_uid;
        meta.gid = new_gid;
        meta.ctime = now_ts;
        Ok(())
    }

    fn set_times(&self, atime: SetTime, mtime: SetTime) -> Result<(), VfsError> {
        if self.shared.read_only && !self.is_detached() {
            return Err(VfsError::RoFs);
        }
        let mut meta = self.meta.lock();
        if !self.is_detached() && meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let now_ts = duration_to_timespec(realtime_duration());
        let new_atime = match atime {
            SetTime::Omit => meta.atime,
            SetTime::Now => now_ts,
            SetTime::Set(ts) => ts,
        };
        let new_mtime = match mtime {
            SetTime::Omit => meta.mtime,
            SetTime::Now => now_ts,
            SetTime::Set(ts) => ts,
        };
        if !self.is_detached() {
            let inner = self.shared.state.lock();
            let mut disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
            disk.set_atime(timespec_to_duration(new_atime));
            disk.set_mtime(timespec_to_duration(new_mtime));
            disk.set_ctime(timespec_to_duration(now_ts));
            disk.write(&inner.ext4).map_err(map_ext4_err)?;
        }
        meta.atime = new_atime;
        meta.mtime = new_mtime;
        meta.ctime = now_ts;
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
        if !self.is_detached() {
            page_cache::maybe_reclaim_ext4_pages();
        }

        let meta = self.meta.lock();
        let detached = self.is_detached();
        if !detached && meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        if offset >= meta.size {
            return Ok(0);
        }
        let to_read = buf.len().min((meta.size - offset) as usize);
        let mut pages = self.pages.lock();

        let mut done = 0usize;
        while done < to_read {
            let cur_off = offset + done as u64;
            let page_idx = cur_off / PAGE_SIZE as u64;
            let in_page = (cur_off % PAGE_SIZE as u64) as usize;
            let chunk = (PAGE_SIZE - in_page).min(to_read - done);

            if let Some(page) = pages.get(page_idx) {
                page.read_bytes(in_page, &mut buf[done..done + chunk]);
            } else if detached {
                buf[done..done + chunk].fill(0);
            } else {
                let page_base = page_idx * PAGE_SIZE as u64;
                let valid = (meta.size - page_base).min(PAGE_SIZE as u64) as usize;
                let mut tmp = vec![0u8; valid];
                {
                    let inner = self.shared.state.lock();
                    let disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
                    let mut got = 0usize;
                    while got < valid {
                        let n = ext4_read_at(&inner.ext4, &disk, &mut tmp[got..valid], page_base + got as u64)
                            .map_err(map_ext4_err)?;
                        if n == 0 {
                            break;
                        }
                        got += n;
                    }
                }
                let cp = Arc::new(CachePage::new_zeroed().ok_or(VfsError::NoMem)?);
                cp.write_bytes(0, &tmp);
                cp.read_bytes(in_page, &mut buf[done..done + chunk]);
                pages.insert_new(page_idx, cp)?;
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
        if self.shared.read_only && !self.is_detached() {
            return Err(VfsError::RoFs);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if !self.is_detached() {
            page_cache::maybe_reclaim_ext4_pages();
        }

        let mut meta = self.meta.lock();
        let detached = self.is_detached();
        if !detached && meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let mut pages = self.pages.lock();
        let old_size = meta.size;

        let _ = offset.checked_add(buf.len() as u64).ok_or(VfsError::FBig)?;
        if offset > old_size && !old_size.is_multiple_of(PAGE_SIZE as u64) {
            let tail_page = old_size / PAGE_SIZE as u64;
            let tail_off = (old_size % PAGE_SIZE as u64) as usize;
            if let Some(page) = pages.get(tail_page) {
                page.zero_range(tail_off, PAGE_SIZE);
                if !detached {
                    page.set_dirty();
                }
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
                if !detached {
                    page.set_dirty();
                }
            } else {
                let Some(raw) = CachePage::new_zeroed() else {
                    if done > 0 {
                        break;
                    }
                    return Err(VfsError::NoMem);
                };
                let cp = Arc::new(raw);
                let page_base = page_idx * PAGE_SIZE as u64;
                if !detached && chunk < PAGE_SIZE && page_base < old_size {
                    let valid = (old_size - page_base).min(PAGE_SIZE as u64) as usize;
                    let mut tmp = vec![0u8; valid];
                    let read_res: Result<(), VfsError> = (|| {
                        let inner = self.shared.state.lock();
                        let disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
                        let mut got = 0usize;
                        while got < valid {
                            let n = ext4_read_at(&inner.ext4, &disk, &mut tmp[got..valid], page_base + got as u64)
                                .map_err(map_ext4_err)?;
                            if n == 0 {
                                break;
                            }
                            got += n;
                        }
                        Ok(())
                    })();
                    if let Err(err) = read_res {
                        if done > 0 {
                            break;
                        }
                        return Err(err);
                    }
                    cp.write_bytes(0, &tmp);
                }
                cp.write_bytes(in_page, &buf[done..done + chunk]);
                if !detached {
                    cp.set_dirty();
                }
                if let Err(err) = pages.insert_new(page_idx, cp) {
                    if done > 0 {
                        break;
                    }
                    return Err(err);
                }
            }
            done += chunk;
        }

        let written_end = offset + done as u64;
        let now_ts = duration_to_timespec(realtime_duration());
        if written_end > meta.size {
            meta.size = written_end;
        }
        meta.mtime = now_ts;
        meta.ctime = now_ts;
        if !detached {
            meta.meta_dirty = true;
        }
        Ok(done)
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

    fn sync(&self, data_only: bool) -> Result<(), VfsError> {
        if self.is_detached() || self.shared.read_only {
            return Ok(());
        }
        self.flush_to_ext4(data_only)?;
        let bcache = {
            let inner = self.shared.state.lock();
            Arc::clone(&inner.bcache)
        };
        bcache.sync().map_err(|_| VfsError::Io)?;
        Ok(())
    }

    fn with_locks_mut(&self, f: &mut dyn FnMut(&mut AdvisoryLocks)) -> Result<(), VfsError> {
        let mut meta = self.meta.lock();
        f(&mut meta.locks);
        Ok(())
    }
}
