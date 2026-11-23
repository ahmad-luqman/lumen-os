//! Writable `ext4` VFS adapter over `ext4plus = "=0.1.0-rc.2"` and `lumen_bcache::BlockCache`.
//!
//! Enforces `docs/ARCHITECTURE.md` §9 lock ordering (`Lock 3 rename_lock ->
//! Lock 4 VfsInode (dirs asc lock_id -> non-dirs asc lock_id) -> Lock 5
//! PageCache -> Lock 6 Ext4 -> Lock 7 BlockCache -> Lock 8 BlockDevice`),
//! Gate 6 open-unlinked detach to `PageCache`, `>= 60B` block symlinks +
//! `<= 60B` inline symlink `readlink`/`unlink` workarounds, truncate tail
//! zeroing, explicit `rmdir` / `rename` (`0`, `NOREPLACE`, `EXCHANGE`), and
//! device numbers and signed times read and written past `ext4plus`
//! (`lumen_vfs::ext4`, [`InodeTimes`], [`new_inode`]).

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

use ext4plus::dir::Dir;
use ext4plus::error::Ext4Error;
use ext4plus::file::{read_at as ext4_read_at, truncate as ext4_truncate, write_at as ext4_write_at};
use ext4plus::inode::{Inode, InodeFlags, InodeMode};
use ext4plus::{DirEntryName, Ext4, Ext4Read, Ext4Write, FileType, IncompatibleFeatures};
use lumen_bcache::{BlockCache, BlockDevice, BlockError};
use lumen_vfs::ext4::rdev_from_i_block;
use lumen_vfs::nlink::NlinkBounds;
use lumen_vfs::{
    AdvisoryLocks, DevId, DirEntry, FileSystem, Ino, InodeType, Metadata, RenameFlags, SetTime, Timespec, VfsError,
    VfsInode, mode_after_chown, next_detached_ino, next_lock_id,
};

use log::{error, warn};

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

/// `ro_compat` bits the adapter recognises (up to `ORPHAN_PRESENT`, 0x10000).
/// Any other bit forces a read-only mount, and so do the recognised ones in
/// [`DISALLOWED_RW_RO_COMPAT_BITS`].
const KNOWN_RO_COMPAT_BITS: u32 = 0x1_bfff;
/// Recognised `ro_compat` bits that still force a read-only mount
/// (ARCHITECTURE §9's `mke2fs -O ^metadata_csum,...` mask is the writable
/// profile): `BTREE_DIR` (0x4), `GDT_CSUM` (0x10), `QUOTA` (0x100),
/// `BIGALLOC` (0x200), `METADATA_CSUM` (0x400: the adapter's raw inode-table
/// and `..` writes (`zero_inline_symlink_before_unlink`,
/// `write_inline_symlink_target`, `SavedInlineSymlink::restore`,
/// `update_dir_dotdot`) never update checksums), `READONLY` (0x1000),
/// `PROJECT` (0x2000) and `ORPHAN_PRESENT` (0x10000: the orphan file has
/// entries, which `ext4plus` never processes).
const DISALLOWED_RW_RO_COMPAT_BITS: u32 = 0x0004 | 0x0010 | 0x0100 | 0x0200 | 0x0400 | 0x1000 | 0x2000 | 0x1_0000;
// A disallowed bit dropped from the known set would be reported as unknown.
const _: () = assert!(DISALLOWED_RW_RO_COMPAT_BITS & !KNOWN_RO_COMPAT_BITS == 0);

/// Why a filesystem's superblock forces a read-only mount, if it does.
fn forced_read_only_reason(needs_recovery: bool, ro_compat_bits: u32) -> Option<&'static str> {
    if needs_recovery {
        Some("the journal needs recovery")
    } else if ro_compat_bits & !KNOWN_RO_COMPAT_BITS != 0 {
        Some("unknown ro_compat feature")
    } else if ro_compat_bits & DISALLOWED_RW_RO_COMPAT_BITS != 0 {
        Some("ro_compat feature not supported for writing (metadata_csum, quota, bigalloc, orphan_present, ...)")
    } else {
        None
    }
}

/// Lowest free-block count at which `create`/`mkdir`/`symlink` still allocate
/// an inode. Linking the new entry can need a block for the directory (plus
/// an extent-index or indirect block), `mkdir` one for the new directory and
/// a long symlink up to 4 for its target; with fewer free blocks the link
/// could fail with `ENOSPC` after the inode is allocated, and without a free
/// directory slot that inode couldn't be released (`discard_orphan`).
const CREATE_RESERVE_BLOCKS: u64 = 8;

/// The most links an inode may have (Linux's `EXT4_LINK_MAX`): `mkdir` in a
/// directory already at it is refused.
const EXT4_LINK_MAX: u32 = 65_000;

/// `ext4plus::file::write_at` writes at most one filesystem block per call on
/// block-mapped files and may stop short (e.g. out of space): loop until all
/// of `buf` is written, a zero-length write counting as out of space.
fn ext4_write_all(ext4: &Ext4, disk: &mut Inode, buf: &[u8], offset: u64) -> Result<(), Ext4Error> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = ext4_write_at(ext4, disk, &buf[done..], offset + done as u64)?;
        if n == 0 {
            return Err(Ext4Error::NoSpace);
        }
        done += n;
    }
    Ok(())
}

/// Fill `buf` from `offset` (stopping early only at the end of the file).
fn ext4_read_full(ext4: &Ext4, disk: &Inode, buf: &mut [u8], offset: u64) -> Result<usize, Ext4Error> {
    let mut got = 0usize;
    while got < buf.len() {
        let n = ext4_read_at(ext4, disk, &mut buf[got..], offset + got as u64)?;
        if n == 0 {
            break;
        }
        got += n;
    }
    Ok(got)
}

/// `Exist` if `dir` has an entry `name`, nothing if it hasn't, and any other
/// lookup error (an unreadable directory block) as itself: never "absent".
fn ensure_absent(dir: &Dir, name: DirEntryName<'_>) -> Result<(), VfsError> {
    match dir.get_entry(name) {
        Ok(_) => Err(VfsError::Exist),
        Err(Ext4Error::NotFound) => Ok(()),
        Err(e) => Err(map_ext4_err(e)),
    }
}

/// The largest file `ext4plus` can map with `block_size` blocks: with
/// extents, 2^32 - 1 logical blocks; block-mapped, the smallest of 12 direct
/// blocks plus the single, double and triple indirect trees, that same
/// logical-block limit, and 2^32 - 1 512-byte sectors (`i_blocks` without
/// `HUGE_FILE`). Linux also caps extent files at 2^32 - 1 sectors without
/// `huge_file`; this adapter follows `ext4plus`, which doesn't.
fn max_file_size(block_size: u64, extents: bool) -> u64 {
    let extent_limit = u64::from(u32::MAX).saturating_mul(block_size);
    if extents {
        return extent_limit;
    }
    let per = block_size / 4;
    let blocks = 12u64
        .saturating_add(per)
        .saturating_add(per.saturating_mul(per))
        .saturating_add(per.saturating_mul(per).saturating_mul(per));
    blocks.saturating_mul(block_size).min(extent_limit).min(u64::from(u32::MAX) * 512)
}

/// `NoSpc` unless at least [`CREATE_RESERVE_BLOCKS`] blocks are free.
fn ensure_create_reserve(ext4: &Ext4) -> Result<(), VfsError> {
    if ext4.superblock().free_blocks_count() < CREATE_RESERVE_BLOCKS {
        return Err(VfsError::NoSpc);
    }
    Ok(())
}

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

/// A block-cache error as the filesystem reports it: `EROFS` for a
/// read-only device, else `EIO` (an address past the device comes from the
/// filesystem's own metadata, not from the caller's arguments).
fn map_block_error(err: &BlockError) -> VfsError {
    match err {
        BlockError::ReadOnly => VfsError::RoFs,
        _ => VfsError::Io,
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
            Some(BcacheIoError(e)) => map_block_error(e),
            None => VfsError::Io,
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
        bcache.read_at(bgd_byte, &mut bgd[..read_len]).map_err(|e| map_block_error(&e))?;

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

/// Where `i_block` (60 bytes: block numbers, an extent tree, a fast
/// symlink's target or a device's number) starts in an on-disk inode.
const I_BLOCK_OFFSET: u64 = 0x28;

use self::times::{InodeTimes, new_inode};

/// An inode's three times, kept as the inode holds them on disk. The fields
/// are private, so every time goes in through [`InodeTimes::from_disk`] or a
/// setter, which clamp it to what this inode can hold, and out to the disk
/// through [`InodeTimes::store`] or [`InodeTimes::store_ctime`]; a new inode
/// gets its times through [`new_inode`]. `stat` then shows what the disk
/// holds or will hold, before and after a flush or a remount (#294).
mod times {
    use core::time::Duration;

    use ext4plus::error::Ext4Error;
    use ext4plus::inode::{Inode, InodeCreationOptions, InodeFlags, InodeMode};
    use ext4plus::{Ext4, FileType};
    use lumen_vfs::Timespec;
    use lumen_vfs::ext4::{time_from_disk, time_to_disk, truncate_time};

    /// Which of an inode's times have their extra field (nanoseconds and the
    /// epoch bits that carry a time past 2038).
    ///
    /// `ext4plus` reads and writes each one only when the inode's
    /// `i_extra_isize` reaches it (`i_ctime_extra` from 8, `i_mtime_extra`
    /// from 12, `i_atime_extra` from 16), as Linux's `EXT4_FITS_IN_INODE`
    /// does. Two differences from Linux, both because `ext4plus` never grows
    /// `i_extra_isize`: an `i_extra_isize` of 0 on a volume with large
    /// inodes means no extra fields here, where Linux's `__ext4_iget` takes
    /// it as 32 (and writes 32 back); and a time a field is missing for is
    /// clamped here for good, where Linux grows the inode
    /// (`ext4_try_to_expand_extra_isize`) or, failing that, stores the time
    /// clamped while keeping it untruncated in memory.
    #[derive(Clone, Copy)]
    struct ExtraTimes {
        atime: bool,
        mtime: bool,
        ctime: bool,
    }

    impl ExtraTimes {
        /// `ext4plus` doesn't say how big an inode's `i_extra_isize` is, so
        /// ask it: a copy of `disk` keeps a time's nanoseconds exactly when
        /// that time's extra field is there. `ext4plus` sets `i_extra_isize`
        /// only when it creates an inode; Linux may grow it on a later mount,
        /// but not while this mount owns the volume, so once per load is
        /// enough.
        fn of(disk: &Inode) -> Self {
            let mut probe = disk.clone();
            let one = Duration::new(0, 1);
            probe.set_atime(one);
            probe.set_mtime(one);
            probe.set_ctime(one);
            Self { atime: probe.atime() == one, mtime: probe.mtime() == one, ctime: probe.ctime() == one }
        }
    }

    /// A new inode from `ext4plus`'s `create_inode`, with atime, mtime,
    /// ctime and crtime `now` as it holds them. `create_inode` sets those
    /// times before `i_extra_isize`, so it writes only the low 32 bits of the
    /// first three (no epoch: a time past 2038 would read as 1901 to 1970; no
    /// nanoseconds) and nothing of crtime (`ext4plus` places `i_crtime`, at
    /// 0x90, by `128 + i_extra_isize`, still 0 then, so it stayed 0, read as
    /// 1970 by Linux's `statx`; #303). They are set again here; the caller's next write of the inode
    /// stores them, before the inode is linked. Lumen itself reports no
    /// btime.
    pub(super) fn new_inode(
        ext4: &Ext4,
        file_type: FileType,
        mode: InodeMode,
        now: Timespec,
    ) -> Result<Inode, Ext4Error> {
        let mut disk = ext4.create_inode(InodeCreationOptions {
            file_type,
            mode,
            uid: 0,
            gid: 0,
            time: Duration::ZERO,
            flags: InodeFlags::empty(),
        })?;
        let mut times = InodeTimes::from_disk(&disk);
        times.set_atime(now);
        times.modified(now);
        times.store(&mut disk);
        // `set_crtime` writes `i_crtime` only when the inode reaches it, and
        // `i_crtime_extra` (0x94) only when it reaches that too: ask a copy
        // which, as `ExtraTimes::of` does for the others.
        let mut probe = disk.clone();
        let one = Duration::new(0, 1);
        probe.set_crtime(one);
        disk.set_crtime(time_to_disk(now, probe.crtime() == Some(one)));
        Ok(disk)
    }

    #[derive(Clone, Copy)]
    pub(super) struct InodeTimes {
        atime: Timespec,
        mtime: Timespec,
        ctime: Timespec,
        extra: ExtraTimes,
    }

    impl InodeTimes {
        /// `disk`'s times. The clamp is defensive: it changes nothing
        /// `ext4plus` decodes (its getters' range is its setters'), so a
        /// loaded inode needs no write-back; it keeps the invariant local.
        /// A corrupt extra word whose nanoseconds are 1e9 or more is carried
        /// into the seconds by `ext4plus`'s `Duration::new`, which can cross
        /// the base word's sign bit (base `0x7fff_ffff`, extra `0xffff_fffc`
        /// reads as 1901 here and 2038 in Linux): garbage in, garbage out.
        pub(super) fn from_disk(disk: &Inode) -> Self {
            let extra = ExtraTimes::of(disk);
            Self {
                atime: truncate_time(time_from_disk(disk.atime()), extra.atime),
                mtime: truncate_time(time_from_disk(disk.mtime()), extra.mtime),
                ctime: truncate_time(time_from_disk(disk.ctime()), extra.ctime),
                extra,
            }
        }

        pub(super) fn atime(&self) -> Timespec {
            self.atime
        }

        pub(super) fn mtime(&self) -> Timespec {
            self.mtime
        }

        pub(super) fn ctime(&self) -> Timespec {
            self.ctime
        }

        /// Each setter keeps the time as this inode holds it: Linux's
        /// `timestamp_truncate` (seconds clamped to the range, nanoseconds
        /// gone if that changed the second, whole seconds without the extra
        /// field), applied per field from `i_extra_isize`; see
        /// [`ExtraTimes`] for how that differs from Linux.
        pub(super) fn set_atime(&mut self, ts: Timespec) {
            self.atime = truncate_time(ts, self.extra.atime);
        }

        pub(super) fn set_mtime(&mut self, ts: Timespec) {
            self.mtime = truncate_time(ts, self.extra.mtime);
        }

        pub(super) fn set_ctime(&mut self, ts: Timespec) {
            self.ctime = truncate_time(ts, self.extra.ctime);
        }

        /// The contents changed at `now`: mtime and ctime.
        pub(super) fn modified(&mut self, now: Timespec) {
            self.set_mtime(now);
            self.set_ctime(now);
        }

        /// Write all three to `disk` (not yet to the device). Each is already
        /// what the inode holds, so it reads back unchanged.
        pub(super) fn store(&self, disk: &mut Inode) {
            disk.set_atime(time_to_disk(self.atime, self.extra.atime));
            disk.set_mtime(time_to_disk(self.mtime, self.extra.mtime));
            self.store_ctime(disk);
        }

        /// Write ctime alone to `disk`.
        pub(super) fn store_ctime(&self, disk: &mut Inode) {
            disk.set_ctime(time_to_disk(self.ctime, self.extra.ctime));
        }
    }
}

/// A live directory record's inode number and name bytes.
type LiveRecord<'a> = (u32, &'a [u8]);

/// What is wrong with a directory record, for the warning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BadRecord {
    /// Fewer than 8 bytes left in the block for its header.
    ShortHeader,
    /// `rec_len` below 8, which would never move on.
    RecLenTooSmall,
    /// `rec_len` running past the end of its block.
    PastBlock,
    /// `name_len` running past `rec_len`.
    NamePastRecord,
}

/// The directory record at `off` in `block`: its `rec_len`, and, if it is
/// live (inode not 0), its inode number and name bytes. A block's records
/// chain by `rec_len` to its end, as `ext4plus`'s `ReadDir` walks them;
/// records with inode 0 are free space, the checksum tail, or an htree's
/// index nodes (the root's index sits inside `..`'s `rec_len`, so it is
/// never read as records).
fn dir_record(block: &[u8], off: usize) -> Result<(usize, Option<LiveRecord<'_>>), BadRecord> {
    let header = block.get(off..off + 8).ok_or(BadRecord::ShortHeader)?;
    let ino = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let rec_len = usize::from(u16::from_le_bytes([header[4], header[5]]));
    if rec_len < 8 {
        return Err(BadRecord::RecLenTooSmall);
    }
    if off + rec_len > block.len() {
        return Err(BadRecord::PastBlock);
    }
    if ino == 0 {
        return Ok((rec_len, None));
    }
    let name_len = usize::from(header[6]);
    if 8 + name_len > rec_len {
        return Err(BadRecord::NamePastRecord);
    }
    Ok((rec_len, Some((ino, &block[off + 8..off + 8 + name_len]))))
}

/// A live directory record found by [`LiveDirents`].
struct LiveDirent {
    /// Its byte position in the directory's data: the `readdir` cursor.
    pos: u64,
    ino: u32,
    name: Vec<u8>,
}

/// Why [`LiveDirents`] stopped.
enum WalkError {
    /// A block couldn't be read: the read's own error.
    Read(VfsError),
    /// A malformed record at this position.
    Record(u64, BadRecord),
}

/// A directory's live records in order, with their byte positions, read a
/// block at a time (one block-sized buffer, the blocks read only as far as
/// the walk goes), beside `ReadDir`, which checks each block's checksum but
/// doesn't say where an entry sits.
struct LiveDirents<'a> {
    ext4: &'a Ext4,
    inode: Inode,
    size: u64,
    block: Vec<u8>,
    /// The loaded block's position, `None` before the first.
    block_at: Option<u64>,
    /// The next record's offset in the loaded block.
    off: usize,
}

impl<'a> LiveDirents<'a> {
    fn new(ext4: &'a Ext4, inode: Inode, block_size: usize) -> Self {
        let size = inode.size_in_bytes();
        Self { ext4, inode, size, block: alloc::vec![0; block_size], block_at: None, off: block_size }
    }

    /// The next live record, `None` at the end of the directory's data.
    fn next(&mut self) -> Result<Option<LiveDirent>, WalkError> {
        let bs = self.block.len();
        loop {
            if self.off >= bs {
                let at = self.block_at.map_or(0, |b| b + bs as u64);
                if at >= self.size {
                    return Ok(None);
                }
                self.block.fill(0);
                ext4_read_at(self.ext4, &self.inode, &mut self.block, at)
                    .map_err(|e| WalkError::Read(map_ext4_err(e)))?;
                self.block_at = Some(at);
                self.off = 0;
            }
            let block_at = self.block_at.unwrap_or(0);
            let pos = block_at + self.off as u64;
            let (rec_len, live) = dir_record(&self.block, self.off).map_err(|why| WalkError::Record(pos, why))?;
            self.off += rec_len;
            if let Some((ino, name)) = live {
                return Ok(Some(LiveDirent { pos, ino, name: name.to_vec() }));
            }
        }
    }
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

/// Whether `i_block` holds something other than block numbers that
/// `ext4plus`'s `delete_file` would still free as blocks: an inline symlink's
/// target text, or a character or block device's device number (Linux keeps
/// `rdev` in `i_block` and allocates no blocks; old-style 1:3 is `0x0103`,
/// i.e. block 259).
fn i_block_holds_no_block_numbers(inode: &Inode) -> bool {
    is_inline_symlink(inode)
        || (matches!(inode.file_type(), FileType::CharacterDevice | FileType::BlockDevice)
            && !inode.flags().contains(InodeFlags::EXTENTS)
            && inode.blocks() == 0)
}

fn entry_name(name: &[u8]) -> Result<DirEntryName<'_>, VfsError> {
    if name.len() > lumen_vfs::NAME_MAX {
        return Err(VfsError::NameTooLong);
    }
    DirEntryName::try_from(name).map_err(|_| VfsError::Inval)
}

/// Most uncached pages `prepare_detach` reads looking for an open file's
/// on-disk data before refusing the unlink with `ENOSPC` (128 MiB, four times
/// the detached quota).
const DETACHED_SCAN_LIMIT_PAGES: usize = 4 * page_cache::DETACHED_QUOTA_PAGES;

/// One mounted filesystem's state behind Lock 6.
///
/// `nlink` ([`NlinkBounds`]) holds in-memory corrections to on-disk
/// `links_count`s. A namespace operation that fails part-way (a write error
/// after `ext4plus` already changed one count) rolls the counts back, and if
/// a rollback write fails too, the disk keeps a count that disagrees with the
/// entries that really exist. The bounds record what the count must be (at
/// least a floor, at most a ceiling, or exactly both): `get_or_create_inode`
/// applies them when an inode is (re)loaded, so `nlink` stays right after the
/// inode is evicted from the cache, and the next namespace operation on that
/// inode writes the corrected count before it changes it. A bound is cleared
/// as soon as a write of the right count lands. They live only in memory: if
/// the volume is unmounted or the machine reboots first, the disk keeps the
/// wrong count until `e2fsck` fixes it.
///
/// Setting a bound (`set_nlink_exact`, `record_nlink_floor`) first reads the
/// disk count: if the disk already agrees there is nothing to keep, and if it
/// doesn't (or can't be read) the bound is kept and logged at `error`, once
/// per change of the bound, because that is the one place a failed undo of a
/// `links_count` leaves the disk wrong.
struct Ext4FsInner {
    dev: DevId,
    ext4: Ext4,
    bcache: SharedBlockCache,
    layout: Ext4DiskLayout,
    live_inodes: BTreeMap<u32, Weak<Ext4Inode>>,
    nlink: NlinkBounds,
}

impl Ext4FsInner {
    fn apply_nlink_bounds(&self, disk_ino: u32, raw_nlink: u32) -> u32 {
        self.nlink.apply(disk_ino, raw_nlink)
    }

    /// The on-disk `links_count` of `disk_ino`, if it can be read.
    fn disk_links_count(&self, disk_ino: u32) -> Option<u32> {
        read_disk_nlink(&self.ext4, NonZeroU32::new(disk_ino)?)
    }

    fn log_kept_bound(&self, disk_ino: u32, disk: Option<u32>, kept: &str) {
        let disk = disk.map_or_else(|| String::from("unreadable"), |n| alloc::format!("{n}"));
        error!(
            target: "vfs",
            "ext4 {:?} inode {disk_ino}: links_count on disk is {disk}, keeping {kept} in memory until a write lands; \
             run e2fsck if the volume is unmounted first",
            self.dev
        );
    }

    /// Require at least `min_nlink` links for `disk_ino` (a count that a
    /// failed write may have left too low on disk). Nothing is kept when the
    /// disk count is already at least that.
    fn record_nlink_floor(&mut self, disk_ino: u32, min_nlink: u32) {
        if min_nlink == 0 {
            return;
        }
        let disk = self.disk_links_count(disk_ino);
        if self.nlink.raise_floor(disk_ino, disk, min_nlink) {
            let floor = self.nlink.floor(disk_ino);
            self.log_kept_bound(disk_ino, disk, &alloc::format!("at least {floor}"));
        }
    }

    /// Pin `disk_ino`'s link count to `exact` (the entries that really
    /// exist) when a write of that count failed. Nothing is kept when the disk
    /// already holds `exact`.
    fn set_nlink_exact(&mut self, disk_ino: u32, exact: u32) {
        let disk = self.disk_links_count(disk_ino);
        if self.nlink.reconcile(disk_ino, disk, exact) {
            self.log_kept_bound(disk_ino, disk, &alloc::format!("{exact}"));
        }
    }

    fn clear_nlink_bounds(&mut self, disk_ino: u32) {
        self.nlink.clear(disk_ino);
    }

    /// `disk`'s count with its bounds applied, as ext4 stores it
    /// ([`links_u16`]).
    fn bounded_links(&self, disk: &Inode) -> u16 {
        links_u16(self.apply_nlink_bounds(disk.index.get(), u32::from(disk.links_count())))
    }

    /// Put `links`, `disk`'s count with its bounds applied, on disk before
    /// an operation changes it, if the disk holds another; its bounds are
    /// cleared once that lands.
    fn store_bounded_links(&mut self, disk: &mut Inode, links: u16) -> Result<(), VfsError> {
        if disk.links_count() != links {
            disk.set_links_count(links);
            disk.write(&self.ext4).map_err(map_ext4_err)?;
            self.clear_nlink_bounds(disk.index.get());
        }
        Ok(())
    }

    /// A `Dir::link` of `dname` into `dir` failed, after raising `ino`'s
    /// count from `before` maybe: if the entry isn't there, that count goes
    /// back ([`Self::write_links_or_pin`]); if it is, the link landed and the
    /// count stands; if the directory can't be read back, at least `before`
    /// is kept ([`Self::record_nlink_floor`]).
    fn undo_failed_link(&mut self, dir: NonZeroU32, dname: DirEntryName<'_>, ino: NonZeroU32, before: u32) {
        match dir_entry_absent(&self.ext4, dir, dname) {
            Ok(true) => {
                let _ = self.write_links_or_pin(ino, links_u16(before));
            }
            Ok(false) => {}
            Err(_) => self.record_nlink_floor(ino.get(), before),
        }
    }

    /// Take the `..` link of a directory that left `parent_ino` (removed,
    /// replaced by a rename, or moved to another parent) off its count on
    /// disk (its bounds applied first): cleared once written, pinned in
    /// memory when the write fails. When the parent can't be read,
    /// `if_unread` (a count from memory) is pinned instead, if given.
    /// Returns the count for memory and the error.
    fn remove_dotdot_link(
        &mut self,
        parent_ino: NonZeroU32,
        if_unread: Option<u32>,
    ) -> (Option<u32>, Option<VfsError>) {
        let mut parent = match Inode::read(&self.ext4, parent_ino) {
            Ok(p) => p,
            Err(e) => {
                if let Some(p) = if_unread {
                    self.set_nlink_exact(parent_ino.get(), p);
                }
                return (if_unread, Some(map_ext4_err(e)));
            }
        };
        let p_new = self.apply_nlink_bounds(parent_ino.get(), u32::from(parent.links_count())).saturating_sub(1);
        parent.set_links_count(links_u16(p_new));
        if let Err(e) = parent.write(&self.ext4) {
            self.set_nlink_exact(parent_ino.get(), p_new);
            return (Some(p_new), Some(map_ext4_err(e)));
        }
        self.clear_nlink_bounds(parent_ino.get());
        (Some(p_new), None)
    }

    /// `disk_ino`'s inode was freed on disk: forget it and its bounds.
    fn forget_freed(&mut self, disk_ino: u32) {
        self.live_inodes.remove(&disk_ino);
        self.clear_nlink_bounds(disk_ino);
    }

    /// An entry of `disk_ino`, which had `effective` links, is gone. With a
    /// link left (`remaining`: the inode as read back), its count on disk is
    /// checked against the one expected ([`Self::reconcile_disk_nlink`]) and
    /// comes back with its bounds applied; without, it was freed and is
    /// forgotten ([`Self::forget_freed`]).
    fn after_entry_gone(&mut self, disk_ino: u32, effective: u32, remaining: Option<&Inode>) -> Option<u32> {
        let Some(rem) = remaining else {
            self.forget_freed(disk_ino);
            return None;
        };
        let disk = u32::from(rem.links_count());
        self.reconcile_disk_nlink(disk_ino, disk, effective.saturating_sub(1));
        Some(self.apply_nlink_bounds(disk_ino, disk))
    }

    /// `disk_ino`'s count on disk was just read as `disk_nlink` and should be
    /// `expected`: nothing is kept when they agree, else the count is pinned
    /// ([`Self::set_nlink_exact`], which reads the disk again).
    fn reconcile_disk_nlink(&mut self, disk_ino: u32, disk_nlink: u32, expected: u32) {
        if disk_nlink == expected {
            self.clear_nlink_bounds(disk_ino);
        } else {
            self.set_nlink_exact(disk_ino, expected);
        }
    }

    /// Put `links` (the entries that really exist) back as `ino`'s count on
    /// disk ([`restore_disk_links_count`]): once that lands its bounds are
    /// cleared, and if it fails the count is pinned in memory
    /// ([`Self::set_nlink_exact`]). The write's error is handed back for the
    /// callers that report it.
    fn write_links_or_pin(&mut self, ino: NonZeroU32, links: u16) -> Result<(), VfsError> {
        let res = restore_disk_links_count(&self.ext4, ino, links);
        self.links_written_or_pinned(ino, links, res)
    }

    /// [`Self::write_links_or_pin`] with one more try before pinning (an
    /// undo's last chance to leave the disk right).
    fn write_links_or_pin_retrying(&mut self, ino: NonZeroU32, links: u16) -> Result<(), VfsError> {
        let res = restore_disk_links_count(&self.ext4, ino, links)
            .or_else(|_| restore_disk_links_count(&self.ext4, ino, links));
        self.links_written_or_pinned(ino, links, res)
    }

    fn links_written_or_pinned(
        &mut self,
        ino: NonZeroU32,
        links: u16,
        res: Result<(), VfsError>,
    ) -> Result<(), VfsError> {
        match res {
            Ok(()) => self.clear_nlink_bounds(ino.get()),
            Err(_) => self.set_nlink_exact(ino.get(), u32::from(links)),
        }
        res
    }

    /// Raise `ino`'s count on disk to at least `min` (when which entries
    /// exist can't be read back, so only a floor is known): once that lands
    /// its floor is cleared, and if it fails the floor is kept in memory
    /// ([`Self::record_nlink_floor`]). Returns whether the write landed.
    fn raise_links_or_floor(&mut self, ino: NonZeroU32, min: u16) -> bool {
        let landed = ensure_min_disk_links_count(&self.ext4, ino, min).is_ok();
        if landed {
            self.nlink.clear_floor(ino.get());
        } else {
            self.record_nlink_floor(ino.get(), u32::from(min));
        }
        landed
    }

    fn live_inode(&self, disk_ino: u32) -> Option<Arc<Ext4Inode>> {
        self.live_inodes.get(&disk_ino).and_then(Weak::upgrade)
    }

    /// The live `Ext4Inode` for an inode found on disk, made if there is
    /// none. A character or block device's number is read from its `i_block`
    /// then, while the inode is surely linked: unlinking the last link zeroes
    /// it (`zero_inline_symlink_before_unlink`), and the inode can be reused.
    fn get_or_create_inode(&mut self, disk: Inode, shared: &Arc<Ext4Shared>) -> Result<Arc<Ext4Inode>, VfsError> {
        if let Some(existing) = self.live_inode(disk.index.get()) {
            return Ok(existing);
        }
        let rdev = self.read_rdev(&disk)?;
        Ok(self.insert_inode(disk, shared, rdev))
    }

    /// [`Self::get_or_create_inode`] for an inode this adapter just made and
    /// linked (a file, directory or symlink, never a device), past the point
    /// where the operation can fail.
    fn created_inode(&mut self, disk: Inode, shared: &Arc<Ext4Shared>) -> Arc<Ext4Inode> {
        debug_assert!(!matches!(disk.file_type(), FileType::CharacterDevice | FileType::BlockDevice));
        self.live_inode(disk.index.get()).unwrap_or_else(|| self.insert_inode(disk, shared, DevId::default()))
    }

    fn insert_inode(&mut self, disk: Inode, shared: &Arc<Ext4Shared>, rdev: DevId) -> Arc<Ext4Inode> {
        let disk_ino = disk.index.get();
        // Opportunistically prune dead weak entries when table grows.
        if self.live_inodes.len() > 256 {
            self.live_inodes.retain(|_, w| w.strong_count() > 0);
        }
        let effective_nlink = self.apply_nlink_bounds(disk_ino, u32::from(disk.links_count()));
        let arc =
            Ext4Inode::from_disk_inode(Arc::clone(shared), &disk, self.layout.block_size as u32, effective_nlink, rdev);
        self.live_inodes.insert(disk_ino, Arc::downgrade(&arc));
        arc
    }

    /// A character or block device's number (`st_rdev`), 0:0 for any other
    /// type. Linux keeps it in `i_block[0..2]`, which `ext4plus` doesn't
    /// expose, so the two words are read from the inode table directly.
    fn read_rdev(&self, disk: &Inode) -> Result<DevId, VfsError> {
        if !matches!(disk.file_type(), FileType::CharacterDevice | FileType::BlockDevice) {
            return Ok(DevId::default());
        }
        let off = self.layout.inode_disk_offset(&self.bcache, disk.index.get())?;
        let mut i_block = [0u8; 8];
        self.bcache.read_at(off + I_BLOCK_OFFSET, &mut i_block).map_err(|e| map_block_error(&e))?;
        let word = |i: usize| u32::from_le_bytes(i_block[i * 4..i * 4 + 4].try_into().unwrap());
        Ok(rdev_from_i_block(word(0), word(1)))
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
    /// The largest file size the filesystem can map ([`max_file_size`]).
    max_file_size: u64,
    /// The errno of the last per-inode sync's (`fsync`/`fdatasync`) failed
    /// block-cache sync, 0 after one that worked: its warning is logged
    /// once per failing streak, as a page's writeback is.
    bcache_sync_errno: AtomicU32,
    state: SleepMutex<Ext4FsInner>,
    /// The ktests' clock: when set, [`Ext4Shared::now`] reads it.
    #[cfg(feature = "ktest")]
    clock: SleepMutex<Option<Timespec>>,
}

impl Ext4Shared {
    /// The wall clock, for times this filesystem sets.
    fn now(&self) -> Timespec {
        #[cfg(feature = "ktest")]
        if let Some(now) = *self.clock.lock() {
            return now;
        }
        let (sec, nsec) = time::realtime_sec_nsec();
        Timespec { sec: sec as i64, nsec: nsec.min(999_999_999) as i64 }
    }

    /// Write the block cache back to the device and flush it (outside Lock 6:
    /// the block cache has its own lock, 7). A failure comes back as the
    /// errno to report and its cause, for the caller to log once.
    fn sync_bcache(&self) -> Result<(), (VfsError, BlockError)> {
        let bcache = {
            let inner = self.state.lock();
            Arc::clone(&inner.bcache)
        };
        bcache.sync().map_err(|e| (map_block_error(&e), e))
    }
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
        bcache.read_at(1024, &mut raw_sb).map_err(|e| map_block_error(&e))?;
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
        let forced =
            forced_read_only_reason(feats.incompatible().contains(IncompatibleFeatures::RECOVERY), ro_compat_bits);
        if let Some(why) = forced
            && !hw_read_only
        {
            warn!(target: "vfs", "ext4 on {dev:?}: mounting read-only: {why} (ro_compat {ro_compat_bits:#x})");
        }
        let read_only = hw_read_only || forced.is_some();

        let ext4 = if read_only {
            probe_fs
        } else {
            drop(probe_fs);
            Ext4::load_with_writer(Box::new(io.clone()), Some(Box::new(io))).map_err(map_ext4_err)?
        };

        let root_disk = ext4.read_root_inode().map_err(map_ext4_err)?;
        let extents = ext4.superblock().features().incompatible().contains(IncompatibleFeatures::EXTENTS);
        let shared = Arc::new(Ext4Shared {
            dev,
            read_only,
            max_file_size: max_file_size(layout.block_size, extents),
            bcache_sync_errno: AtomicU32::new(0),
            state: SleepMutex::new(Ext4FsInner {
                dev,
                ext4,
                bcache,
                layout,
                live_inodes: BTreeMap::new(),
                nlink: NlinkBounds::new(),
            }),
            #[cfg(feature = "ktest")]
            clock: SleepMutex::new(None),
        });

        let root = {
            let mut inner = shared.state.lock();
            inner.get_or_create_inode(root_disk, &shared)?
        };

        Ok(Arc::new(Self { shared, rename_lock: SleepMutex::new(()), root }))
    }

    /// Every live inode (dead entries pruned), for reclaim and `sync`; Lock 6
    /// is held only while copying.
    pub fn snapshot_live(&self) -> Vec<Arc<Ext4Inode>> {
        self.shared.state.lock().snapshot_live()
    }

    /// The live inode whose `lock_id` is `lock_id`, for the ktests: a scan
    /// of every live inode.
    #[cfg(feature = "ktest")]
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

    /// Flush every live inode, then the block cache, even past failures:
    /// one inode that can't be written mustn't keep the others (and the
    /// metadata in the block cache) off the disk. Each failure is logged;
    /// the first is returned.
    fn sync(&self) -> Result<(), VfsError> {
        if self.shared.read_only {
            return Ok(());
        }
        let live = self.snapshot_live();
        let mut first_err: Option<VfsError> = None;
        for inode in live {
            // A detached inode returns before it locks or reads anything.
            if let Err(e) = inode.flush_to_ext4(false) {
                warn!(target: "vfs", "sync: ext4 {:?} inode {}: {e:?}", self.shared.dev, inode.disk_ino);
                first_err.get_or_insert(e);
            }
        }
        if let Err((e, cause)) = self.shared.sync_bcache() {
            warn!(target: "vfs", "sync: ext4 {:?} block cache: {cause}", self.shared.dev);
            first_err.get_or_insert(e);
        }
        first_err.map_or(Ok(()), Err)
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
    /// The times, as the inode holds them.
    times: InodeTimes,
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
    /// A character or block device's number, read once when the inode was
    /// first loaded (`Ext4FsInner::read_rdev`); 0:0 for other types.
    rdev: DevId,
    open_count: AtomicU32,
    detached: AtomicBool,
    /// The errno of the writeback failure last logged, 0 once a writeback
    /// fully succeeds: one warning per failing streak and kind of error, not
    /// one per page or reclaim pass.
    writeback_errno: AtomicU32,
    /// The errno of the page-fill read failure last logged, 0 once a fill
    /// succeeds: one warning per failing streak and kind of error.
    read_errno: AtomicU32,
    /// A listing found the directory's records and `ReadDir` disagreeing,
    /// or a malformed record, and said so (once).
    warned_bad_listing: AtomicBool,
    meta: SleepMutex<Ext4InodeMeta>,
    pages: SleepMutex<PageCache>,
}

impl Ext4Inode {
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
        if self.shared.read_only && !self.is_detached() {
            return Err(VfsError::RoFs);
        }
        if let Some(at) = offset
            && buf.is_empty()
        {
            return Ok((at, 0));
        }
        if !self.is_detached() {
            page_cache::maybe_reclaim_ext4_pages();
        }

        let mut meta = self.meta.lock();
        let detached = self.is_detached();
        if !detached && meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let offset = offset.unwrap_or(meta.size);
        if buf.is_empty() {
            return Ok((offset, 0));
        }
        let mut pages = self.pages.lock();
        let old_size = meta.size;

        // Past the largest file the filesystem can map, write what fits (a
        // short write) and `EFBIG` once nothing does, as POSIX asks.
        let max = self.shared.max_file_size;
        if offset >= max {
            return Err(VfsError::FBig);
        }
        let buf = &buf[..buf.len().min(usize::try_from(max - offset).unwrap_or(usize::MAX))];
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
                // Past the hard limit no more pages are added: reclaim, run
                // once before taking the locks, couldn't get below it (the
                // dirty data can't be written back), or this one write added
                // more pages than the gap.
                if !detached && page_cache::ext4_at_hard_limit() {
                    page_cache::note_hard_limit_refusal();
                    if done > 0 {
                        break;
                    }
                    return Err(VfsError::NoSpc);
                }
                let Some(raw) = CachePage::new_zeroed() else {
                    if done > 0 {
                        break;
                    }
                    return Err(VfsError::NoMem);
                };
                let cp = Arc::new(raw);
                let page_base = page_idx * PAGE_SIZE as u64;
                if !detached && chunk < PAGE_SIZE && page_base < old_size {
                    match self.read_disk_page(page_idx, old_size) {
                        Ok(tmp) => cp.write_bytes(0, &tmp),
                        Err(err) => {
                            if done > 0 {
                                break;
                            }
                            return Err(err);
                        }
                    }
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
        let now_ts = self.shared.now();
        if written_end > meta.size {
            meta.size = written_end;
        }
        meta.times.modified(now_ts);
        if !detached {
            meta.meta_dirty = true;
        }
        Ok((offset, done))
    }

    fn from_disk_inode(
        shared: Arc<Ext4Shared>,
        disk: &Inode,
        blksize: u32,
        effective_nlink: u32,
        rdev: DevId,
    ) -> Arc<Self> {
        let disk_ino = disk.index.get();
        let meta = Ext4InodeMeta {
            mode: disk.mode().bits() & 0o7777,
            nlink: effective_nlink,
            uid: disk.uid(),
            gid: disk.gid(),
            size: disk.size_in_bytes(),
            blksize,
            blocks_512: disk.blocks(),
            times: InodeTimes::from_disk(disk),
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
            rdev,
            open_count: AtomicU32::new(0),
            detached: AtomicBool::new(false),
            writeback_errno: AtomicU32::new(0),
            read_errno: AtomicU32::new(0),
            warned_bad_listing: AtomicBool::new(false),
            meta: SleepMutex::new(meta),
            pages: SleepMutex::new(PageCache::new_ext4()),
        })
    }

    /// `EIO` for a listing of this directory that can't be trusted, said
    /// once per directory: its records and `ReadDir` disagree, or one is
    /// malformed.
    fn bad_listing(&self, what: core::fmt::Arguments<'_>) -> VfsError {
        if !self.warned_bad_listing.swap(true, Ordering::AcqRel) {
            warn!(target: "vfs", "ext4 {:?} directory {}: can't list it: {what}", self.shared.dev, self.disk_ino);
        }
        VfsError::Io
    }

    /// [`LiveDirents`]'s error as `readdir`'s: a failed read as it is, a
    /// malformed record a [`bad_listing`](Self::bad_listing).
    fn walk_failed(&self, e: WalkError) -> VfsError {
        match e {
            WalkError::Read(e) => e,
            WalkError::Record(pos, why) => self.bad_listing(format_args!("a malformed record at {pos}: {why:?}")),
        }
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

    /// The file's bytes of page `page_idx` below `file_size`, read from disk
    /// (Locks 6–8): at most a page, fewer for the file's last page.
    /// The caller holds Lock 4 (`file_size` is its `meta.size`) and Lock 5,
    /// and the page starts below `file_size`.
    fn read_disk_page(&self, page_idx: u64, file_size: u64) -> Result<Vec<u8>, VfsError> {
        let page_base = page_idx * PAGE_SIZE as u64;
        let valid = (file_size - page_base).min(PAGE_SIZE as u64) as usize;
        let mut tmp = vec![0u8; valid];
        let inner = self.shared.state.lock();
        let disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
        ext4_read_full(&inner.ext4, &disk, &mut tmp, page_base).map_err(map_ext4_err)?;
        Ok(tmp)
    }

    /// A new page, not yet in the cache, holding page `page_idx` as on disk
    /// and zero past `file_size`: the fill of a missing page for `read_at`
    /// and `mmap_page`. (`write_at`'s partial-page fill calls
    /// [`Self::read_disk_page`] itself and hands a failure to the writer,
    /// unlogged.) Same locks as `read_disk_page`. A failed read is logged
    /// naming the device, inode and page ([`Self::note_read_error`]): a
    /// mapping's fault turns it into `SIGBUS`, which says nothing of the
    /// disk.
    fn load_page(&self, page_idx: u64, file_size: u64) -> Result<Arc<CachePage>, VfsError> {
        let tmp = match self.read_disk_page(page_idx, file_size) {
            Ok(tmp) => {
                self.read_errno.store(0, Ordering::Relaxed);
                tmp
            }
            Err(e) => {
                self.note_read_error(e, page_idx);
                return Err(e);
            }
        };
        let cp = Arc::new(CachePage::new_zeroed().ok_or(VfsError::NoMem)?);
        cp.write_bytes(0, &tmp);
        Ok(cp)
    }

    /// This file's page-cache lock (Lock 5), for
    /// `page_cache::MappedPage::copy_to_frame`.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: file-backed MAP_PRIVATE CoW (#255)
    pub(super) fn page_cache_lock(&self) -> &SleepMutex<PageCache> {
        &self.pages
    }

    /// This file's cached page `index`, filled when missing, and its size
    /// read with it: `ext4`'s side of [`page_cache::mmap_page`] (its
    /// contract). A hole in a detached file
    /// (whose pages were all read in when it was detached) gets a zero page,
    /// counted against the detached quota (not the hard limit). For a linked
    /// file at the cache's hard limit a missing page is refused (`NoSpc`,
    /// logged once per crossing), as
    /// `write_at` refuses one: a mapping pins its pages, so reclaim can't
    /// keep the cache under the limit as it does for `read_at`'s pages.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: file-backed mmap (#255)
    pub(super) fn mmap_page(&self, index: u64) -> Result<(Arc<CachePage>, u64), VfsError> {
        if self.inode_type != InodeType::File {
            return Err(VfsError::NoDev);
        }
        if !self.is_detached() {
            page_cache::maybe_reclaim_ext4_pages();
        }
        let meta = self.meta.lock();
        let detached = self.is_detached();
        if !detached && meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let file_size = meta.size;
        page_cache::mmap_page_in_file(index, file_size)?;
        let mut pages = self.pages.lock();
        let page = match pages.get(index) {
            Some(page) => Arc::clone(page),
            None => {
                let page = if detached {
                    Arc::new(CachePage::new_zeroed().ok_or(VfsError::NoMem)?)
                } else if page_cache::ext4_at_hard_limit() {
                    page_cache::note_hard_limit_refusal();
                    return Err(VfsError::NoSpc);
                } else {
                    self.load_page(index, file_size)?
                };
                pages.insert_new(index, Arc::clone(&page))?;
                page
            }
        };
        page.touch();
        Ok((page, file_size))
    }

    /// A failed truncate couldn't put back the tail bytes it zeroed on disk:
    /// the file reads zeros there once its cached page is evicted.
    fn log_tail_restore_failure(&self) {
        error!(
            target: "vfs",
            "ext4 {:?} inode {}: truncate failed and its tail bytes couldn't be restored on disk",
            self.shared.dev,
            self.disk_ino
        );
    }

    /// Log a writeback failure of `what` once per failing streak, and again
    /// whenever the kind of error changes.
    fn note_writeback_error(&self, err: VfsError, what: fmt::Arguments<'_>) {
        if self.writeback_errno.swap(err.to_errno(), Ordering::AcqRel) != err.to_errno() {
            warn!(target: "vfs", "ext4 {:?} inode {}: writeback of {what} failed: {err:?}", self.shared.dev, self.disk_ino);
        }
    }

    /// Log a page fill's failed disk read once per failing streak (until a
    /// fill succeeds), and again whenever the kind of error changes.
    fn note_read_error(&self, err: VfsError, page_idx: u64) {
        let errno = err.to_errno();
        if self.read_errno.swap(errno, Ordering::AcqRel) != errno {
            warn!(
                target: "vfs",
                "ext4 {:?} inode {}: reading page {page_idx} (offset {}) failed: {err:?}",
                self.shared.dev,
                self.disk_ino,
                page_idx * PAGE_SIZE as u64
            );
        }
    }

    /// Write one dirty page's bytes below `file_size` back to disk (Lock 6
    /// and below), clearing its dirty bit first and setting it again if the
    /// write fails, so a failed writeback is retried and the page is never
    /// evicted as clean. The caller holds the inode's Lock 4 and Lock 5 (the
    /// page's bytes are read under Lock 5). `Ok(None)` for a page wholly past
    /// `file_size` (nothing to write), else the new `i_blocks`.
    fn write_back_page(&self, page_idx: u64, page: &CachePage, file_size: u64) -> Result<Option<u64>, VfsError> {
        let page_off = page_idx * PAGE_SIZE as u64;
        if page_off >= file_size {
            return Ok(None);
        }
        let valid = (file_size - page_off).min(PAGE_SIZE as u64) as usize;
        page.swap_dirty(false);
        let mut bounce = [0u8; PAGE_SIZE];
        page.read_bytes(0, &mut bounce[..valid]);
        let res = {
            let inner = self.shared.state.lock();
            Inode::read(&inner.ext4, self.disk_inode_index())
                .and_then(|mut disk| {
                    ext4_write_all(&inner.ext4, &mut disk, &bounce[..valid], page_off)?;
                    Ok(disk.blocks())
                })
                .map_err(map_ext4_err)
        };
        if let Err(e) = res {
            page.set_dirty();
            self.note_writeback_error(e, format_args!("page {page_idx} (offset {page_off})"));
        }
        res.map(Some)
    }

    /// Write back every dirty page (Lock 4 -> Lock 5 -> Lock 6 -> 7 -> 8),
    /// then evict every page it wrote that is still clean and unpinned, then
    /// up to `max_evict` minus those more clean pages, oldest first. A page
    /// whose write fails stays dirty (and is never evicted); the others are
    /// still written. The number of pages evicted.
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
        let mut failed = false;
        for (page_idx, page) in dirty {
            match self.write_back_page(page_idx, &page, file_size) {
                Ok(Some(blocks_512)) => {
                    meta.blocks_512 = blocks_512;
                    flushed_indices.push(page_idx);
                }
                Ok(None) => {}
                Err(_) => failed = true,
            }
        }
        if !failed {
            self.writeback_errno.store(0, Ordering::Release);
        }
        let mut evicted = pages.evict_if_clean_unpinned(&flushed_indices);
        if evicted < max_evict {
            evicted += pages.evict_clean_oldest(max_evict - evicted);
        }
        evicted
    }

    /// Flush all dirty `PageCache` pages and authoritative metadata to `ext4`
    /// under `Lock 4 -> Lock 5 -> Lock 6 -> Lock 7 -> Lock 8`. A page whose
    /// write fails stays dirty, and `meta_dirty` stays set if the metadata
    /// write fails, so a later sync retries both; every other page is still
    /// written, and the first error is returned.
    fn flush_to_ext4(&self, data_only: bool) -> Result<(), VfsError> {
        if self.is_detached() || self.shared.read_only {
            return Ok(());
        }
        let mut meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Ok(());
        }
        let file_size = meta.size;
        let mut first_err: Option<VfsError> = None;
        if self.inode_type == InodeType::File {
            let pages = self.pages.lock();
            for (page_idx, page) in pages.collect_dirty() {
                match self.write_back_page(page_idx, &page, file_size) {
                    Ok(Some(blocks_512)) => meta.blocks_512 = blocks_512,
                    Ok(None) => {}
                    Err(e) => {
                        first_err.get_or_insert(e);
                    }
                }
            }
        }

        if meta.meta_dirty || !data_only {
            let res = (|| -> Result<(), VfsError> {
                let inner = self.shared.state.lock();
                let mut disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
                if self.inode_type == InodeType::File && disk.size_in_bytes() != meta.size {
                    ext4_truncate(&inner.ext4, &mut disk, meta.size).map_err(map_ext4_err)?;
                }
                if meta.meta_dirty {
                    let full_mode = (self.inode_type.to_mode_bits() as u16) | (meta.mode & 0o7777);
                    disk.set_mode(InodeMode::from_bits_retain(full_mode)).map_err(map_ext4_err)?;
                    disk.set_uid(meta.uid);
                    disk.set_gid(meta.gid);
                    meta.times.store(&mut disk);
                    disk.write(&inner.ext4).map_err(map_ext4_err)?;
                    meta.blocks_512 = disk.blocks();
                    meta.meta_dirty = false;
                }
                Ok(())
            })();
            if let Err(e) = res {
                self.note_writeback_error(e, format_args!("its metadata"));
                first_err.get_or_insert(e);
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => {
                self.writeback_errno.store(0, Ordering::Release);
                Ok(())
            }
        }
    }

    /// Make the new symlink inode `sym` a fast symlink to `target` (at most 60
    /// bytes fit in `i_block`; `symlink` passes at most 59, the most
    /// `ext4plus` reads back inline): no `EXTENTS` flag, `i_size = target.len()`, the target in
    /// `i_block`. `ext4plus` keeps its inline-data setter private, so the
    /// bytes are written to the inode table directly (as
    /// `zero_inline_symlink_before_unlink` does), then the inode is re-read.
    fn write_inline_symlink_target(inner: &Ext4FsInner, mut sym: Inode, target: &[u8]) -> Result<Inode, VfsError> {
        debug_assert!(target.len() <= 60);
        sym.set_flags(sym.flags().difference(InodeFlags::EXTENTS));
        sym.set_size_in_bytes(target.len() as u64);
        sym.write(&inner.ext4).map_err(map_ext4_err)?;
        let off = inner.layout.inode_disk_offset(&inner.bcache, sym.index.get())?;
        let mut body = [0u8; 60];
        body[..target.len()].copy_from_slice(target);
        inner.bcache.write_at(off + I_BLOCK_OFFSET, &body).map_err(|e| map_block_error(&e))?;
        Inode::read(&inner.ext4, sym.index).map_err(map_ext4_err)
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
        inner.bcache.read_at(off + I_BLOCK_OFFSET, &mut buf).map_err(|e| map_block_error(&e))?;
        Ok(buf)
    }

    /// Zero an inline (fast) symlink's or a device node's size and `i_block`
    /// on disk before the unlink that frees it, and hand back the bytes to
    /// restore if the unlink fails. `ext4plus`'s `delete_file` reads `i_block`
    /// as block numbers (it has no fast-symlink or device case), so freeing an
    /// inode whose `i_block` holds target text or a device number would free
    /// someone else's blocks or panic on garbage ones. Only when this is the
    /// last link: other links keep the target or device number.
    fn zero_inline_symlink_before_unlink(
        inner: &Ext4FsInner,
        disk_ino: u32,
        disk: Inode,
        child_nlink: u32,
    ) -> Result<(Inode, Option<SavedInlineSymlink>), VfsError> {
        let idx = NonZeroU32::new(disk_ino).ok_or(VfsError::Inval)?;
        if child_nlink <= 1 && disk.links_count() <= 1 && i_block_holds_no_block_numbers(&disk) {
            let off = inner.layout.inode_disk_offset(&inner.bcache, disk_ino)?;
            // Read the contiguous inode span `[0x04..0x70)` covering `size_lo` (0x04..0x08),
            // `inline_60` (0x28..0x64), and `size_hi` (0x6c..0x70) so we can zero all three
            // fields in a single `bcache.write_at` call without ever splitting `inline_60`
            // from `size_lo` on a write-cutoff fault.
            let mut span = [0u8; 0x6c];
            inner.bcache.read_at(off + 0x04, &mut span).map_err(|e| map_block_error(&e))?;
            let saved = SavedInlineSymlink { off, span };

            let mut zeroed_span = span;
            zeroed_span[0x00..0x04].fill(0);
            zeroed_span[0x24..0x60].fill(0);
            zeroed_span[0x68..0x6c].fill(0);
            inner.bcache.write_at(off + 0x04, &zeroed_span).map_err(|e| map_block_error(&e))?;

            return match Inode::read(&inner.ext4, idx) {
                Ok(zeroed_disk) => Ok((zeroed_disk, Some(saved))),
                Err(e) => {
                    saved.restore(inner, disk_ino);
                    Err(map_ext4_err(e))
                }
            };
        }
        Ok((disk, None))
    }

    /// Phase 1 of Gate 6 detach-while-open: copy the file's on-disk data that
    /// isn't cached yet into new pages (or read the symlink target) and
    /// reserve `DETACHED_QUOTA` WITHOUT mutating `detached`, `nlink`,
    /// `meta_dirty`, `live_inodes`, or `PageCache` ownership yet. Pages that
    /// read back all zero are left out: once detached, a missing page reads as
    /// zeros anyway.
    ///
    /// The scan is bounded, since it runs under the namespace locks. It stops
    /// as soon as the non-zero blocks copied account for every block the
    /// inode has allocated (so a large sparse file with its data near the
    /// start is cheap), and gives up with `NoSpc` after
    /// [`DETACHED_SCAN_LIMIT_PAGES`] uncached pages. The trade-off: unlinking
    /// an open file fails whenever its allocated blocks aren't all accounted
    /// for by non-zero data within that scan (data lying further out in a
    /// sparse file, a long run of zero-filled blocks, or extent and
    /// indirect blocks counted in `i_blocks`), as one with more data than
    /// the detached quota already does.
    ///
    /// If the subsequent disk unlink/rmdir/rename fails, dropping the returned
    /// `PreparedDetach` automatically releases the `DETACHED_QUOTA` reservation.
    ///
    /// Caller holds Lock 3 and ordered Lock 4 on `target` (`meta_guard`).
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
                    let (disk, allocated_blocks, block_size) = {
                        let inner = target.shared.state.lock();
                        let disk = Inode::read(&inner.ext4, target.disk_inode_index()).map_err(map_ext4_err)?;
                        let allocated = disk.fs_blocks(&inner.ext4).map_err(map_ext4_err)?;
                        (disk, allocated, inner.layout.block_size as usize)
                    };
                    if allocated_blocks > 0 && disk.size_in_bytes() > 0 {
                        // Data on disk ends at its own size even if the cached
                        // size is larger.
                        let num_pages = file_size.min(disk.size_in_bytes()).div_ceil(PAGE_SIZE as u64);
                        let mut tmp = vec![0u8; PAGE_SIZE];
                        // Non-zero filesystem blocks copied so far: each was
                        // allocated, so once they reach `allocated_blocks` no
                        // data is left on disk to copy. (Metadata blocks and
                        // zero-filled data blocks only delay this; they never
                        // end the scan early.)
                        let mut data_blocks_seen = 0u64;
                        let mut last_counted_block: Option<u64> = None;
                        let mut scanned = 0usize;
                        let step = block_size.min(PAGE_SIZE);
                        for p in 0..num_pages {
                            if data_blocks_seen >= allocated_blocks {
                                break;
                            }
                            // A cached page is authoritative and isn't copied,
                            // but the disk blocks under it still count toward
                            // the allocated ones. Those reads are bounded by
                            // the cached page count, which the reservation
                            // above caps at the detached quota.
                            let cached = pages.get(p).is_some();
                            if !cached {
                                if scanned >= DETACHED_SCAN_LIMIT_PAGES {
                                    return Err(VfsError::NoSpc);
                                }
                                scanned += 1;
                            }
                            let page_off = p * PAGE_SIZE as u64;
                            let chunk = (file_size - page_off).min(PAGE_SIZE as u64) as usize;
                            tmp[..chunk].fill(0);
                            {
                                let inner = target.shared.state.lock();
                                ext4_read_full(&inner.ext4, &disk, &mut tmp[..chunk], page_off)
                                    .map_err(map_ext4_err)?;
                            }
                            // Count each filesystem block with a non-zero byte
                            // once (a block may span several pages).
                            let mut any_nonzero = false;
                            for (i, sub) in tmp[..chunk].chunks(step).enumerate() {
                                if sub.iter().any(|&x| x != 0) {
                                    any_nonzero = true;
                                    let blk = (page_off + (i * step) as u64) / block_size as u64;
                                    if last_counted_block != Some(blk) {
                                        last_counted_block = Some(blk);
                                        data_blocks_seen += 1;
                                    }
                                }
                            }
                            if any_nonzero && !cached {
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
        let effective_nlink =
            inner.apply_nlink_bounds(child_arc.disk_ino, u32::from(child_disk.links_count()).max(child_nlink));
        let effective_u16 = links_u16(effective_nlink);
        inner.store_bounded_links(&mut child_disk, effective_u16)?;
        let (child_disk, saved_inline) =
            Self::zero_inline_symlink_before_unlink(inner, child_arc.disk_ino, child_disk, effective_nlink)?;
        match parent_dir.unlink(dname, child_disk) {
            Ok(rem) => {
                let remaining_nlink = inner.after_entry_gone(child_arc.disk_ino, effective_nlink, rem.as_ref());
                Ok(UnlinkDiskOutcome::Completed { remaining_nlink, post_err: None })
            }
            Err(e) => {
                let err = map_ext4_err(e);
                match entry_at(&inner.ext4, parent_ino, dname) {
                    Ok(entry) if entry.index.get() == child_arc.disk_ino => {
                        if let Some(saved) = saved_inline {
                            saved.restore(inner, child_arc.disk_ino);
                        }
                        let _ = inner.write_links_or_pin(child_arc.disk_inode_index(), effective_u16);
                        // Entry is still present on disk: keep meta.nlink aligned with bounds.
                        Ok(UnlinkDiskOutcome::Aborted { err, actual_nlink: u32::from(effective_u16) })
                    }
                    Err(Ext4Error::NotFound) => {
                        // The entry was removed on disk before `delete_file` /
                        // `free_inode` failed, so the inode may already be
                        // freed: don't restore `saved_inline` or write
                        // `effective_nlink` onto it unless a link survives.
                        let remaining =
                            surviving_inode(&inner.ext4, child_arc.disk_inode_index(), false, effective_nlink);
                        let remaining_nlink =
                            inner.after_entry_gone(child_arc.disk_ino, effective_nlink, remaining.as_ref());
                        Ok(UnlinkDiskOutcome::Completed { remaining_nlink, post_err: Some(err) })
                    }
                    Ok(_) => {
                        // The name now points at another inode: our child's
                        // entry is gone, so its zeroed inline target is not
                        // restored (the inode may be freed).
                        inner.set_nlink_exact(child_arc.disk_ino, effective_nlink);
                        Ok(UnlinkDiskOutcome::Aborted { err, actual_nlink: effective_nlink })
                    }
                    Err(_) => {
                        // The directory can't be read back, so decide from
                        // the child inode itself. `Dir::unlink` writes the
                        // child's lowered count, removes the entry, and only
                        // then (`delete_file`) zeroes the inode and frees it
                        // in the bitmap. A zeroed mode means the entry is gone
                        // and the inode may be free: restoring onto it would
                        // revive a number another create can reuse.
                        let child_now = Inode::read(&inner.ext4, child_arc.disk_inode_index());
                        let zeroed = match &child_now {
                            Ok(cur) => cur.mode().bits() == 0,
                            // A zeroed inode may not parse; one that can't be
                            // read at all leaves it unknown (treated as live,
                            // below).
                            Err(Ext4Error::Corrupt(_)) => true,
                            Err(_) => false,
                        };
                        if zeroed {
                            inner.forget_freed(child_arc.disk_ino);
                            return Ok(UnlinkDiskOutcome::Completed { remaining_nlink: None, post_err: Some(err) });
                        }
                        // Otherwise the inode is still allocated (the bitmap
                        // is freed only after the zeroing): assume the entry
                        // survived and put the inline target back, or a
                        // surviving symlink would read as empty. If the entry
                        // was in fact removed, the restored inode is an orphan
                        // `e2fsck` reattaches, with its target intact.
                        if child_now.is_err() {
                            error!(
                                target: "vfs",
                                "ext4 {:?} inode {}: unlink failed and neither its directory nor the inode reads \
                                 back; assuming the entry survived; run e2fsck",
                                inner.dev,
                                child_arc.disk_ino
                            );
                        }
                        if let Some(saved) = saved_inline {
                            saved.restore(inner, child_arc.disk_ino);
                        }
                        inner.set_nlink_exact(child_arc.disk_ino, effective_nlink);
                        Ok(UnlinkDiskOutcome::Aborted { err, actual_nlink: effective_nlink })
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
        let effective_links =
            inner.apply_nlink_bounds(child_arc.disk_ino, u32::from(child_disk.links_count()).max(child_nlink));
        let effective_links_u16 = links_u16(effective_links);
        let keeps_a_link = !is_last_link(true, effective_links);
        let pre_unlink_links = if keeps_a_link { effective_links_u16 } else { 1 };
        if child_disk.links_count() != pre_unlink_links {
            child_disk.set_links_count(pre_unlink_links);
            child_disk.write(&inner.ext4).map_err(map_ext4_err)?;
            if keeps_a_link {
                inner.clear_nlink_bounds(child_arc.disk_ino);
            }
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
            return match entry_at(&inner.ext4, parent_ino, dname) {
                Ok(entry) if entry.index.get() == child_arc.disk_ino => {
                    let _ = inner.write_links_or_pin(child_arc.disk_inode_index(), effective_links_u16);
                    Ok(RmdirDiskOutcome::StillPresent { err, actual_nlink: effective_links })
                }
                Err(Ext4Error::NotFound) => {
                    // The entry was removed on disk before `delete_file` /
                    // `free_inode` failed, so the inode may already be freed:
                    // don't write `effective_links` onto it unless a link
                    // survives.
                    let remaining = surviving_inode(&inner.ext4, child_arc.disk_inode_index(), true, effective_links);
                    let remaining_nlink =
                        inner.after_entry_gone(child_arc.disk_ino, effective_links, remaining.as_ref());
                    let (parent_nlink, _) = inner.remove_dotdot_link(parent_ino, None);
                    Ok(RmdirDiskOutcome::Removed { remaining_nlink, parent_nlink, post_err: Some(err) })
                }
                Ok(_) | Err(_) => {
                    inner.set_nlink_exact(child_arc.disk_ino, effective_links);
                    Ok(RmdirDiskOutcome::StillPresent { err, actual_nlink: effective_links })
                }
            };
        }

        let remaining_nlink = inner.after_entry_gone(child_arc.disk_ino, effective_links, remaining.as_ref());
        let (parent_nlink, post_err) = inner.remove_dotdot_link(parent_ino, None);
        Ok(RmdirDiskOutcome::Removed { remaining_nlink, parent_nlink, post_err })
    }

    /// Take a rename target's entry `dname` out of `parent_ino` without
    /// lowering the target's link count, so its inode and data stay
    /// allocated until the source has landed at that name (see `rename`).
    /// `Dir::unlink` writes the count it is handed minus one, so it is handed
    /// one more than the target keeps. Returns the count the target keeps on
    /// disk. An error leaves the entry in place; the target's count is the
    /// same either way.
    fn take_out_target_entry(
        inner: &mut Ext4FsInner,
        parent_ino: NonZeroU32,
        dname: DirEntryName<'_>,
        target: &Arc<Ext4Inode>,
        target_nlink: u32,
    ) -> Result<u16, VfsError> {
        let mut disk = Inode::read(&inner.ext4, target.disk_inode_index()).map_err(map_ext4_err)?;
        let effective = inner.apply_nlink_bounds(target.disk_ino, u32::from(disk.links_count()).max(target_nlink));
        // One more than `u16::MAX` can't be handed over (as with mkdir's
        // link limit, `VfsError` has no EMLINK).
        let pinned = u16::try_from(effective).ok().and_then(|n| n.checked_add(1)).ok_or(VfsError::NoSpc)?;
        let effective_u16 = pinned - 1;
        inner.store_bounded_links(&mut disk, effective_u16)?;
        disk.set_links_count(pinned);
        let res = Inode::read(&inner.ext4, parent_ino)
            .and_then(|p| Dir::open_inode(&inner.ext4, p))
            .and_then(|mut d| d.unlink(dname, disk));
        let Err(e) = res else {
            return Ok(effective_u16);
        };
        let err = map_ext4_err(e);
        match dir_has_entry(&inner.ext4, parent_ino, dname, target.disk_ino) {
            Ok(true) => Err(err),
            // The entry went before a later write failed: carry on with the
            // target held as if the removal had succeeded.
            Ok(false) => Ok(effective_u16),
            Err(_) => {
                error!(
                    target: "vfs",
                    "ext4 {:?} directory {parent_ino}: taking rename target inode {} out failed and the directory \
                     doesn't read back; assuming its entry is still there; run e2fsck",
                    inner.dev,
                    target.disk_ino
                );
                Err(err)
            }
        }
    }
}

impl Ext4Inode {
    /// Apply `fate` to `child`'s in-memory state (`meta`, under its Lock 4
    /// guard) and end its prepared detach: committed when its last entry
    /// went ([`ChildFate::Freed`], [`Self::commit_detach`]), else dropped,
    /// which releases the quota it reserved. An inode detached already is
    /// left alone.
    fn settle_child(
        child: &Arc<Ext4Inode>,
        meta: &mut Ext4InodeMeta,
        prepared: Option<PreparedDetach<'_>>,
        fate: ChildFate,
    ) {
        if let ChildFate::Freed { .. } = fate
            && let Some(prepared) = prepared
        {
            Self::commit_detach(prepared, meta);
            return;
        }
        drop(prepared);
        if child.is_detached() {
            return;
        }
        match fate {
            ChildFate::Kept(links) => meta.nlink = links,
            ChildFate::Remaining { links, now } => {
                meta.nlink = links;
                meta.times.set_ctime(now);
                if links > 0 {
                    meta.meta_dirty = true;
                }
            }
            ChildFate::Freed { ctime } => {
                meta.nlink = 0;
                if let Some(now) = ctime {
                    meta.times.set_ctime(now);
                }
                meta.meta_dirty = false;
            }
        }
    }

    /// Take a rename's existing target out of `new_dir` and hold it
    /// ([`HeldTarget`]), preparing its detach first when that entry is its
    /// last link and it is open. An error leaves the target where it was.
    fn hold_target<'a>(
        &self,
        guards: &mut [LockedInode<'_>],
        new_dir: &Arc<Ext4Inode>,
        new_dname: DirEntryName<'_>,
        target: &'a Arc<Ext4Inode>,
    ) -> Result<HeldTarget<'a>, VfsError> {
        let is_dir = target.inode_type == InodeType::Dir;
        let nlink = find_guard_mut(guards, target).nlink;
        let prepared = if is_last_link(is_dir, nlink) && target.open_count() > 0 {
            Self::prepare_detach(target, find_guard_mut(guards, target))?
        } else {
            None
        };
        let links = {
            let mut inner = self.shared.state.lock();
            Self::take_out_target_entry(&mut inner, new_dir.disk_inode_index(), new_dname, target, nlink)?
        };
        Ok(HeldTarget { arc: target, links, is_dir, prepared })
    }

    /// The move failed: put the held target back at `dname` with its counts
    /// unchanged and drop its prepared detach (releasing the quota). If that
    /// fails the target keeps its inode and data but no entry, which
    /// `e2fsck` reattaches in `lost+found`: logged.
    fn put_back_target(
        &self,
        guards: &mut [LockedInode<'_>],
        new_dir: &Arc<Ext4Inode>,
        dname: DirEntryName<'_>,
        target: HeldTarget<'_>,
    ) {
        {
            let inner = self.shared.state.lock();
            if let Err(e) =
                relink_keeping_counts(&inner.ext4, new_dir.disk_inode_index(), dname, target.arc.disk_inode_index())
            {
                error!(
                    target: "vfs",
                    "ext4 {:?} directory {}: putting rename target inode {} back after a failed move failed: {e:?}; \
                     its data is intact; run e2fsck",
                    inner.dev,
                    new_dir.disk_ino,
                    target.arc.disk_ino
                );
            }
        }
        let meta = find_guard_mut(guards, target.arc);
        Self::settle_child(target.arc, meta, target.prepared, ChildFate::Kept(u32::from(target.links)));
    }

    /// The move landed: the held target loses the link its old entry was.
    /// If it was its last, the inode is freed (through a temporary entry in
    /// `new_dir`, since `ext4plus` frees only through `Dir::unlink`) and an
    /// open target is detached; a directory target also takes its `..` link
    /// off the parent. A failed free leaves an unreferenced inode for
    /// `e2fsck` (logged here); the rename itself has happened. Runs after
    /// [`MoveOutcome::apply_counts`]: the parent's count to keep if it can't
    /// be read back comes from `new_dir`'s guard, which that set.
    fn retire_target(
        &self,
        guards: &mut [LockedInode<'_>],
        new_dir: &Arc<Ext4Inode>,
        target: HeldTarget<'_>,
        now: Timespec,
    ) {
        let last = is_last_link(target.is_dir, u32::from(target.links));
        let ino = target.arc.disk_inode_index();
        let new_dir_ino = new_dir.disk_inode_index();
        let new_dir_mem_nlink = find_guard_mut(guards, new_dir).nlink;
        let (remaining, parent_nlink) = {
            let mut inner = self.shared.state.lock();
            let remaining = if last {
                // The parent's count before the temporary link, which both
                // attempts put back.
                let base = Inode::read(&inner.ext4, new_dir_ino).map(|d| d.links_count()).map_err(map_ext4_err);
                let temp = base.and_then(|base| Ok((base, unused_temp_name(&inner.ext4, new_dir_ino, ino)?)));
                let res = temp.and_then(|(base, temp)| {
                    let name = DirEntryName::try_from(temp.as_str()).map_err(|_| VfsError::Inval)?;
                    let first = try_discard_orphan(&mut inner, new_dir_ino, name, ino, Some(base));
                    // While the temporary entry is still there the inode is
                    // still allocated and linked (the free comes after the
                    // entry goes), so one retry is safe.
                    if first.is_err() && matches!(dir_has_entry(&inner.ext4, new_dir_ino, name, ino.get()), Ok(true)) {
                        try_discard_orphan(&mut inner, new_dir_ino, name, ino, Some(base))
                    } else {
                        first
                    }
                });
                if let Err(e) = res {
                    error!(
                        target: "vfs",
                        "ext4 {:?} directory {new_dir_ino}: freeing inode {ino}, replaced by a rename, failed: {e:?}; \
                         run e2fsck",
                        inner.dev
                    );
                }
                inner.forget_freed(target.arc.disk_ino);
                None
            } else {
                let rem = target.links - 1;
                let _ = inner.write_links_or_pin(ino, rem);
                Some(u32::from(rem))
            };
            let parent_nlink = if target.is_dir {
                inner.remove_dotdot_link(new_dir_ino, Some(new_dir_mem_nlink.saturating_sub(1))).0
            } else {
                None
            };
            (remaining, parent_nlink)
        };
        let meta = find_guard_mut(guards, target.arc);
        Self::settle_child(target.arc, meta, target.prepared, ChildFate::gone(remaining, now, Some(now)));
        if let Some(p) = parent_nlink {
            find_guard_mut(guards, new_dir).nlink = p;
        }
    }
}

/// A rename's replaced target between `Ext4Inode::take_out_target_entry` and
/// the end of the move: its entry is gone but its inode is still allocated
/// with `links` links on disk.
#[must_use = "a held target must be put back or retired"]
struct HeldTarget<'a> {
    arc: &'a Arc<Ext4Inode>,
    links: u16,
    is_dir: bool,
    prepared: Option<PreparedDetach<'a>>,
}

/// Put `child_ino` back at `dname` in `parent_ino` (if it isn't there)
/// without changing its link count or the parent's: `Dir::link` writes each
/// count it is handed plus one, so it is handed one less.
fn relink_keeping_counts(
    ext4: &Ext4,
    parent_ino: NonZeroU32,
    dname: DirEntryName<'_>,
    child_ino: NonZeroU32,
) -> Result<(), VfsError> {
    if dir_has_entry(ext4, parent_ino, dname, child_ino.get()).map_err(map_ext4_err)? {
        return Ok(());
    }
    let mut child = Inode::read(ext4, child_ino).map_err(map_ext4_err)?;
    child.set_links_count(child.links_count().saturating_sub(1));
    let mut dir = Dir::open_inode(ext4, Inode::read(ext4, parent_ino).map_err(map_ext4_err)?).map_err(map_ext4_err)?;
    ensure_absent(&dir, dname)?;
    if child.file_type().is_dir() {
        let parent_links = dir.inode().links_count();
        dir.inode_mut().set_links_count(parent_links.saturating_sub(1));
    }
    dir.link(dname, &mut child).map_err(map_ext4_err)
}

/// A `RENAME_EXCHANGE` on disk, past `rename`'s checks: the two entries, the
/// inodes they name and their parents, with the four link counts from before
/// (bounds applied). [`Exchange::run`] makes the swap.
#[derive(Clone, Copy)]
struct Exchange<'n> {
    old_dir_ino: NonZeroU32,
    new_dir_ino: NonZeroU32,
    old_child_ino: NonZeroU32,
    new_child_ino: NonZeroU32,
    old_dname: DirEntryName<'n>,
    new_dname: DirEntryName<'n>,
    same_parent: bool,
    old_is_dir: bool,
    new_is_dir: bool,
    old_orig_links: u16,
    new_orig_links: u16,
    old_p_orig_links: u16,
    new_p_orig_links: u16,
}

/// The exchange step that failed, which says what [`Exchange::roll_back`]
/// undoes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExchangeStep {
    /// Raising the new child's count (the old child's was raised).
    RaiseNewCount,
    /// Taking `old_child`'s entry out of `old_dir`.
    UnlinkOld,
    /// Taking `new_child`'s entry out of `new_dir`.
    UnlinkNew,
    /// Linking `old_child` at `new_dname` in `new_dir`.
    LinkOldAtNew,
    /// Linking `new_child` at `old_dname` in `old_dir`.
    LinkNewAtOld,
}

/// The least count each inode keeps in memory after an exchange's undo
/// (`None`: what the disk and the bounds say).
struct ExchangeMins {
    old_child: Option<u32>,
    new_child: Option<u32>,
    old_dir: Option<u32>,
    new_dir: Option<u32>,
}

/// What an exchange leaves for `rename` to apply under the Lock 4 guards
/// ([`ExchangeSync::apply`]): each inode's count in memory (`None`:
/// unchanged), and whether the swap landed (and the timestamps move), with
/// the result.
struct ExchangeSync {
    old_child_nlink: Option<u32>,
    new_child_nlink: Option<u32>,
    old_dir_nlink: Option<u32>,
    new_dir_nlink: Option<u32>,
    swap: Landed,
}

/// Whether a rename's change (the exchange's swap, the move of the source)
/// landed on disk, with the rename's result: one that didn't always carries
/// its error.
enum Landed {
    /// It landed; `Err`: a fix-up after it failed, the change kept.
    Yes(Result<(), VfsError>),
    /// It didn't: undone, or not known to have landed (nothing undone).
    No(VfsError),
}

impl Landed {
    fn into_result(self) -> Result<(), VfsError> {
        match self {
            Self::Yes(res) => res,
            Self::No(err) => Err(err),
        }
    }
}

/// One child's count after an exchange's undo: with its entries read back
/// (`here`: at its own name, `there`: at the other's), the count they call
/// for (its count from before, `orig`, less the entry it lost, plus each
/// found; at least `two_names`, the count with both, when both are there)
/// is written ([`Ext4FsInner::write_links_or_pin_retrying`]); unread, it
/// is raised to `orig` ([`Ext4FsInner::raise_links_or_floor`]). Returns the
/// least count to keep in memory (`None`: what the disk says).
fn child_count_after_undo(
    inner: &mut Ext4FsInner,
    ino: NonZeroU32,
    orig: u16,
    two_names: u16,
    here: &Result<bool, Ext4Error>,
    there: &Result<bool, Ext4Error>,
) -> Option<u32> {
    let (Ok(here), Ok(there)) = (here, there) else {
        inner.raise_links_or_floor(ino, orig);
        return Some(u32::from(orig));
    };
    let slots = u16::from(*here) + u16::from(*there);
    let target = orig.saturating_sub(1).saturating_add(slots);
    let resolved = if slots >= 2 { target.max(two_names) } else { target };
    let _ = inner.write_links_or_pin_retrying(ino, resolved);
    (resolved > 0 || slots >= 2).then_some(u32::from(resolved))
}

impl<'n> Exchange<'n> {
    /// Swap `old_dname` in `old_dir_arc` and `new_dname` in `new_dir_arc` on
    /// disk: raise both children's counts so that neither unlink frees one,
    /// take both entries out, link each child at the other's name, then put
    /// the exact counts back and fix the parents' counts and `..` for moved
    /// directories ([`Self::finish`]). A step that fails before the swap is
    /// undone ([`Self::roll_back`]); one after it is reported, the swap kept.
    /// `Err`: the swap never started (a write putting a parent's bounded
    /// count on disk may have landed first).
    fn run(
        inner: &mut Ext4FsInner,
        old_dir_arc: &Arc<Ext4Inode>,
        new_dir_arc: &Arc<Ext4Inode>,
        old_child_arc: &Arc<Ext4Inode>,
        new_child_arc: &Arc<Ext4Inode>,
        old_dname: DirEntryName<'n>,
        new_dname: DirEntryName<'n>,
    ) -> Result<ExchangeSync, VfsError> {
        let same_parent = old_dir_arc.disk_ino == new_dir_arc.disk_ino;
        let old_is_dir = old_child_arc.inode_type == InodeType::Dir;
        let new_is_dir = new_child_arc.inode_type == InodeType::Dir;
        let old_child_ino = old_child_arc.disk_inode_index();
        let new_child_ino = new_child_arc.disk_inode_index();
        let old_dir_ino = old_dir_arc.disk_inode_index();
        let new_dir_ino = new_dir_arc.disk_inode_index();

        let mut old_disk = Inode::read(&inner.ext4, old_child_ino).map_err(map_ext4_err)?;
        let mut new_disk = Inode::read(&inner.ext4, new_child_ino).map_err(map_ext4_err)?;
        let mut old_p_init = Inode::read(&inner.ext4, old_dir_ino).map_err(map_ext4_err)?;
        let mut new_p_init = Inode::read(&inner.ext4, new_dir_ino).map_err(map_ext4_err)?;

        let old_orig_links = inner.bounded_links(&old_disk);
        let new_orig_links = inner.bounded_links(&new_disk);
        let old_p_orig_links = inner.bounded_links(&old_p_init);
        let new_p_orig_links = inner.bounded_links(&new_p_init);

        if old_is_dir || new_is_dir {
            inner.store_bounded_links(&mut old_p_init, old_p_orig_links)?;
            if !same_parent {
                inner.store_bounded_links(&mut new_p_init, new_p_orig_links)?;
            }
        }

        let x = Exchange {
            old_dir_ino,
            new_dir_ino,
            old_child_ino,
            new_child_ino,
            old_dname,
            new_dname,
            same_parent,
            old_is_dir,
            new_is_dir,
            old_orig_links,
            new_orig_links,
            old_p_orig_links,
            new_p_orig_links,
        };
        // 1. Bump both link counts so unlinking never deletes either inode.
        old_disk.set_links_count(old_orig_links.saturating_add(1));
        old_disk.write(&inner.ext4).map_err(map_ext4_err)?;
        new_disk.set_links_count(new_orig_links.saturating_add(1));
        let (failed, e) = if let Err(e) = new_disk.write(&inner.ext4) {
            (ExchangeStep::RaiseNewCount, e)
        } else if let Err(e) = Inode::read(&inner.ext4, old_dir_ino)
            .and_then(|old_p| Dir::open_inode(&inner.ext4, old_p))
            .and_then(|mut d| d.unlink(old_dname, old_disk))
        {
            // 2a (unlinking `old_child` from `old_dir`) failed.
            (ExchangeStep::UnlinkOld, e)
        } else if let Err(e) = Inode::read(&inner.ext4, new_dir_ino)
            .and_then(|new_p| Dir::open_inode(&inner.ext4, new_p))
            .and_then(|mut d| d.unlink(new_dname, new_disk))
        {
            // 2b (unlinking `new_child` from `new_dir`) failed.
            (ExchangeStep::UnlinkNew, e)
        } else if let Err(e) = (|| -> Result<(), Ext4Error> {
            // 3a. Link `old_child` into `new_dir` at `new_dname`.
            let mut old_disk = Inode::read(&inner.ext4, old_child_ino)?;
            let new_p = Inode::read(&inner.ext4, new_dir_ino)?;
            Dir::open_inode(&inner.ext4, new_p)?.link(new_dname, &mut old_disk)
        })() {
            (ExchangeStep::LinkOldAtNew, e)
        } else if let Err(e) = (|| -> Result<(), Ext4Error> {
            // 3b. Link `new_child` into `old_dir` at `old_dname`.
            let mut new_disk = Inode::read(&inner.ext4, new_child_ino)?;
            let old_p = Inode::read(&inner.ext4, old_dir_ino)?;
            Dir::open_inode(&inner.ext4, old_p)?.link(old_dname, &mut new_disk)
        })() {
            (ExchangeStep::LinkNewAtOld, e)
        } else {
            return Ok(x.finish(inner));
        };
        let mins = x.roll_back(inner, failed);
        Ok(x.after_undo(inner, mins, map_ext4_err(e)))
    }

    /// Undo an exchange whose step `failed`: take out the links it may have
    /// made, put back the entries it may have taken out, retry once writes
    /// work again if an entry went missing or two names point at one inode,
    /// then write back the counts the entries found call for (pinned in
    /// memory when that fails, a floor when the entries can't be read).
    fn roll_back(&self, inner: &mut Ext4FsInner, failed: ExchangeStep) -> ExchangeMins {
        let Exchange {
            old_dir_ino,
            new_dir_ino,
            old_child_ino,
            new_child_ino,
            old_dname,
            new_dname,
            same_parent,
            old_is_dir,
            new_is_dir,
            old_orig_links,
            new_orig_links,
            old_p_orig_links,
            new_p_orig_links,
        } = *self;
        let undo_old_at_new = matches!(failed, ExchangeStep::LinkOldAtNew | ExchangeStep::LinkNewAtOld);
        let undo_new_at_old = failed == ExchangeStep::LinkNewAtOld;
        let restore_old_entry = failed != ExchangeStep::RaiseNewCount;
        let restore_new_entry =
            matches!(failed, ExchangeStep::UnlinkNew | ExchangeStep::LinkOldAtNew | ExchangeStep::LinkNewAtOld);
        let old_min_unlink = two_name_links(old_is_dir);
        let new_min_unlink = two_name_links(new_is_dir);
        if undo_new_at_old {
            rollback_link_entry(&inner.ext4, old_dir_ino, old_dname, new_child_ino, new_min_unlink);
        }
        if undo_old_at_new {
            rollback_link_entry(&inner.ext4, new_dir_ino, new_dname, old_child_ino, old_min_unlink);
        }
        if restore_old_entry {
            rollback_restore_entry(&inner.ext4, old_dir_ino, old_dname, old_child_ino);
        }
        if restore_new_entry {
            rollback_restore_entry(&inner.ext4, new_dir_ino, new_dname, new_child_ino);
        }

        // Which of the four entries exist: each child at its own name and at
        // the other's.
        let probe = |ext4: &Ext4| {
            (
                dir_has_entry(ext4, old_dir_ino, old_dname, old_child_ino.get()),
                dir_has_entry(ext4, new_dir_ino, new_dname, old_child_ino.get()),
                dir_has_entry(ext4, new_dir_ino, new_dname, new_child_ino.get()),
                dir_has_entry(ext4, old_dir_ino, old_dname, new_child_ino.get()),
            )
        };
        let (mut old_in_old_res, mut old_in_new_res, mut new_in_new_res, mut new_in_old_res) = probe(&inner.ext4);

        // If a rollback write above itself failed, an entry may have
        // vanished or both names may point at one inode. Once writes
        // succeed again, restore any missing entry to `(old, new)`.
        if let (Ok(oo), Ok(on), Ok(nn), Ok(no)) = (&old_in_old_res, &old_in_new_res, &new_in_new_res, &new_in_old_res)
            && ((!*oo && !*on) || (!*nn && !*no))
        {
            if !*oo {
                if *no {
                    if !*nn && !*on && matches!(dir_entry_absent(&inner.ext4, new_dir_ino, new_dname), Ok(true)) {
                        rollback_restore_entry(&inner.ext4, new_dir_ino, new_dname, new_child_ino);
                    }
                    if matches!(dir_has_entry(&inner.ext4, new_dir_ino, new_dname, new_child_ino.get()), Ok(true)) {
                        rollback_link_entry(&inner.ext4, old_dir_ino, old_dname, new_child_ino, new_min_unlink);
                    }
                }
                rollback_restore_entry(&inner.ext4, old_dir_ino, old_dname, old_child_ino);
            }

            let oo_now = matches!(dir_has_entry(&inner.ext4, old_dir_ino, old_dname, old_child_ino.get()), Ok(true));
            let on_now = matches!(dir_has_entry(&inner.ext4, new_dir_ino, new_dname, old_child_ino.get()), Ok(true));
            let nn_now = matches!(dir_has_entry(&inner.ext4, new_dir_ino, new_dname, new_child_ino.get()), Ok(true));
            if !nn_now {
                if on_now {
                    if !oo_now && matches!(dir_entry_absent(&inner.ext4, old_dir_ino, old_dname), Ok(true)) {
                        rollback_restore_entry(&inner.ext4, old_dir_ino, old_dname, old_child_ino);
                    }
                    if matches!(dir_has_entry(&inner.ext4, old_dir_ino, old_dname, old_child_ino.get()), Ok(true)) {
                        rollback_link_entry(&inner.ext4, new_dir_ino, new_dname, old_child_ino, old_min_unlink);
                    }
                }
                rollback_restore_entry(&inner.ext4, new_dir_ino, new_dname, new_child_ino);
            }

            (old_in_old_res, old_in_new_res, new_in_new_res, new_in_old_res) = probe(&inner.ext4);
        }
        if !matches!(
            (&old_in_old_res, &old_in_new_res, &new_in_new_res, &new_in_old_res),
            (Ok(true), Ok(false), Ok(true), Ok(false))
        ) {
            log_rename_undo_failure(inner.dev, "exchange", old_dir_ino, new_dir_ino);
        }

        let min_old_child = child_count_after_undo(
            inner,
            old_child_ino,
            old_orig_links,
            old_min_unlink,
            &old_in_old_res,
            &old_in_new_res,
        );
        let min_new_child = child_count_after_undo(
            inner,
            new_child_ino,
            new_orig_links,
            new_min_unlink,
            &new_in_new_res,
            &new_in_old_res,
        );
        let mut min_old_dir = None;
        let mut min_new_dir = None;

        if let (Ok(oo), Ok(on), Ok(nn), Ok(no)) = (old_in_old_res, old_in_new_res, new_in_new_res, new_in_old_res) {
            let old_slot_has_dir = (oo && old_is_dir) || (no && new_is_dir);
            let new_slot_has_dir = (nn && new_is_dir) || (on && old_is_dir);
            if same_parent {
                let expected_p = old_p_orig_links
                    .saturating_sub(u16::from(old_is_dir) + u16::from(new_is_dir))
                    .saturating_add(u16::from(old_slot_has_dir) + u16::from(new_slot_has_dir));
                let restored = inner.write_links_or_pin_retrying(old_dir_ino, expected_p).is_ok();
                if expected_p != old_p_orig_links || !restored {
                    min_old_dir = Some(u32::from(expected_p));
                    min_new_dir = min_old_dir;
                }
            } else {
                let expected_old_p =
                    old_p_orig_links.saturating_sub(u16::from(old_is_dir)).saturating_add(u16::from(old_slot_has_dir));
                let expected_new_p =
                    new_p_orig_links.saturating_sub(u16::from(new_is_dir)).saturating_add(u16::from(new_slot_has_dir));
                let restored_old = inner.write_links_or_pin_retrying(old_dir_ino, expected_old_p).is_ok();
                let restored_new = inner.write_links_or_pin_retrying(new_dir_ino, expected_new_p).is_ok();
                if expected_old_p != old_p_orig_links || !restored_old {
                    min_old_dir = Some(u32::from(expected_old_p));
                }
                if expected_new_p != new_p_orig_links || !restored_new {
                    min_new_dir = Some(u32::from(expected_new_p));
                }
            }
        } else {
            if !inner.raise_links_or_floor(old_dir_ino, old_p_orig_links) {
                min_old_dir = Some(u32::from(old_p_orig_links));
            }
            if !same_parent && !inner.raise_links_or_floor(new_dir_ino, new_p_orig_links) {
                min_new_dir = Some(u32::from(new_p_orig_links));
            }
        }

        ExchangeMins { old_child: min_old_child, new_child: min_new_child, old_dir: min_old_dir, new_dir: min_new_dir }
    }

    /// The counts in memory after an undo: each inode's on disk with its
    /// bounds applied, raised to `mins` when it has no ceiling; `err`, the
    /// step's that failed, is the result.
    fn after_undo(&self, inner: &Ext4FsInner, mins: ExchangeMins, err: VfsError) -> ExchangeSync {
        let clamp = |ino: NonZeroU32, fallback: Option<u32>| -> Option<u32> {
            match read_disk_nlink(&inner.ext4, ino) {
                Some(d) => {
                    let bounded = inner.apply_nlink_bounds(ino.get(), d);
                    match fallback {
                        Some(m) if inner.nlink.ceiling(ino.get()).is_none() => Some(bounded.max(m)),
                        _ => Some(bounded),
                    }
                }
                None => fallback.map(|m| inner.apply_nlink_bounds(ino.get(), m)),
            }
        };
        ExchangeSync {
            old_child_nlink: clamp(self.old_child_ino, mins.old_child),
            new_child_nlink: clamp(self.new_child_ino, mins.new_child),
            old_dir_nlink: clamp(self.old_dir_ino, mins.old_dir),
            new_dir_nlink: clamp(self.new_dir_ino, mins.new_dir),
            swap: Landed::No(err),
        }
    }

    /// Steps 4 and 5, once both entries are swapped on disk.
    fn finish(&self, inner: &mut Ext4FsInner) -> ExchangeSync {
        let Exchange {
            old_dir_ino,
            new_dir_ino,
            old_child_ino,
            new_child_ino,
            same_parent,
            old_is_dir,
            new_is_dir,
            old_orig_links,
            new_orig_links,
            old_p_orig_links,
            new_p_orig_links,
            ..
        } = *self;
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

        // 4. Restore exact link counts on both swapped children. (Every
        // count written in steps 4 and 5 differs from the disk's, which
        // `ext4plus` just raised, so each write happens.)
        record_err(inner.write_links_or_pin(old_child_ino, old_orig_links));
        record_err(inner.write_links_or_pin(new_child_ino, new_orig_links));

        // 5. Adjust parent link counts and `..` when moving directories:
        // each parent loses the `..` of a directory that left it and gains
        // that of one that came in (one parent: no change).
        let (expected_old_dir_nlink, expected_new_dir_nlink) = if same_parent {
            (old_p_orig_links, old_p_orig_links)
        } else {
            (
                old_p_orig_links.saturating_sub(u16::from(old_is_dir)).saturating_add(u16::from(new_is_dir)),
                new_p_orig_links.saturating_sub(u16::from(new_is_dir)).saturating_add(u16::from(old_is_dir)),
            )
        };
        if same_parent {
            if old_is_dir || new_is_dir {
                record_err(inner.write_links_or_pin(old_dir_ino, expected_old_dir_nlink));
            }
        } else {
            if old_is_dir {
                record_err(inner.write_links_or_pin(old_dir_ino, expected_old_dir_nlink));
                record_err(update_dir_dotdot(&inner.ext4, old_child_ino.get(), new_dir_ino.get()));
            }
            if new_is_dir {
                record_err(inner.write_links_or_pin(new_dir_ino, expected_new_dir_nlink));
                record_err(update_dir_dotdot(&inner.ext4, new_child_ino.get(), old_dir_ino.get()));
            }
        }
        let post_res = match first_err {
            Some(e) => Err(e),
            None => Ok(()),
        };
        // Each count for memory: the disk's with its bounds applied, or,
        // unread, the one just meant to be written.
        let in_memory = |ino: NonZeroU32, written: u16| {
            read_disk_nlink(&inner.ext4, ino)
                .map(|d| inner.apply_nlink_bounds(ino.get(), d))
                .or(Some(u32::from(written)))
        };
        ExchangeSync {
            old_child_nlink: in_memory(old_child_ino, old_orig_links),
            new_child_nlink: in_memory(new_child_ino, new_orig_links),
            old_dir_nlink: in_memory(old_dir_ino, expected_old_dir_nlink),
            new_dir_nlink: in_memory(new_dir_ino, expected_new_dir_nlink),
            swap: Landed::Yes(post_res),
        }
    }
}

/// What a rename's move of its source left on disk, for `rename` to apply
/// under the Lock 4 guards: whether the source moved, with the result, and
/// each inode's count in memory (`None`: unchanged).
struct MoveOutcome {
    moved: Landed,
    sync_old_child: Option<u32>,
    sync_old_dir: Option<u32>,
    sync_new_dir: Option<u32>,
}

/// A rename's move of `old_child` from `old_dname` in `old_dir` to
/// `new_dname` in `new_dir` on disk, past `rename`'s checks and with any
/// target already out of the way ([`Ext4Inode::hold_target`]), with the
/// counts from before (bounds applied). [`MoveEntry::run`] makes it.
#[derive(Clone, Copy)]
struct MoveEntry<'n> {
    old_dir_ino: NonZeroU32,
    new_dir_ino: NonZeroU32,
    old_child_ino: NonZeroU32,
    old_dname: DirEntryName<'n>,
    new_dname: DirEntryName<'n>,
    same_parent: bool,
    old_is_dir: bool,
    /// The old parent's count in memory before the move.
    old_dir_nlink_before: u32,
    orig_old_links: u16,
    orig_np_links: u16,
    /// Whether the new parent's count had a floor kept in memory.
    np_had_floor: bool,
}

/// `disk`, a count read from disk, with `ino`'s bounds applied; with none
/// read, what the bounds alone say ([`NlinkBounds::assumed`]).
fn clamp_to_bounds(inner: &Ext4FsInner, ino: NonZeroU32, disk: Option<u32>) -> Option<u32> {
    match disk {
        Some(d) => Some(inner.apply_nlink_bounds(ino.get(), d)),
        None => inner.nlink.assumed(ino.get()),
    }
}

/// `ino`'s count read from disk now, through [`clamp_to_bounds`].
fn read_bounded(inner: &Ext4FsInner, ino: NonZeroU32) -> Option<u32> {
    clamp_to_bounds(inner, ino, read_disk_nlink(&inner.ext4, ino))
}

impl<'n> MoveEntry<'n> {
    /// Link `old_child` at `new_dname` in `new_dir` FIRST, then unlink
    /// `old_dname` from `old_dir`, then, for a directory, fix the old
    /// parent's count (and between parents the child's `..`). A failure with
    /// the old entry still there is undone ([`Self::roll_back`]); one after
    /// the unlink landed is reported, the move kept; if the old entry can't
    /// be judged, nothing is undone and only a floor is kept. `Err`: the move
    /// never started (a write putting the child's or the new parent's
    /// bounded count on disk may have landed first).
    fn run(
        inner: &mut Ext4FsInner,
        old_dir_arc: &Arc<Ext4Inode>,
        new_dir_arc: &Arc<Ext4Inode>,
        old_child_arc: &Arc<Ext4Inode>,
        old_dname: DirEntryName<'n>,
        new_dname: DirEntryName<'n>,
        old_dir_nlink_before: u32,
    ) -> Result<MoveOutcome, VfsError> {
        let same_parent = old_dir_arc.disk_ino == new_dir_arc.disk_ino;
        let old_is_dir = old_child_arc.inode_type == InodeType::Dir;
        let old_child_ino = old_child_arc.disk_inode_index();
        let new_dir_ino = new_dir_arc.disk_inode_index();
        let old_dir_ino = old_dir_arc.disk_inode_index();

        let mut old_disk = Inode::read(&inner.ext4, old_child_ino).map_err(map_ext4_err)?;
        let orig_old_links = inner.bounded_links(&old_disk);
        inner.store_bounded_links(&mut old_disk, orig_old_links)?;
        let mut np_disk = Inode::read(&inner.ext4, new_dir_ino).map_err(map_ext4_err)?;
        let np_had_floor = inner.nlink.floor(new_dir_ino.get()) > 0;
        let orig_np_links = inner.bounded_links(&np_disk);
        if old_is_dir {
            inner.store_bounded_links(&mut np_disk, orig_np_links)?;
        }
        let mv = MoveEntry {
            old_dir_ino,
            new_dir_ino,
            old_child_ino,
            old_dname,
            new_dname,
            same_parent,
            old_is_dir,
            old_dir_nlink_before,
            orig_old_links,
            orig_np_links,
            np_had_floor,
        };
        Ok(mv.link_then_unlink(inner, old_disk, np_disk))
    }

    fn link_then_unlink(&self, inner: &mut Ext4FsInner, mut old_disk: Inode, np_disk: Inode) -> MoveOutcome {
        let MoveEntry { old_dir_ino, new_dir_ino, old_child_ino, old_dname, new_dname, orig_old_links, .. } = *self;
        let link_res = Dir::open_inode(&inner.ext4, np_disk).and_then(|mut d| d.link(new_dname, &mut old_disk));
        if let Err(e) = link_res {
            return self.roll_back(inner, map_ext4_err(e));
        }
        let unlink_res = Inode::read(&inner.ext4, old_dir_ino)
            .and_then(|op_disk| Dir::open_inode(&inner.ext4, op_disk))
            .and_then(|mut d| d.unlink(old_dname, old_disk));
        let Err(e) = unlink_res else {
            return self.finish(inner);
        };
        let err = map_ext4_err(e);
        match entry_at(&inner.ext4, old_dir_ino, old_dname) {
            Ok(entry) if entry.index.get() == old_child_ino.get() => self.roll_back(inner, err),
            Err(Ext4Error::NotFound) => {
                // `old_dname` is already gone on disk: the move committed.
                // Do NOT undo `link(new_dname)`; sync `nlink`s from disk.
                let child_disk_n = read_disk_nlink(&inner.ext4, old_child_ino);
                match child_disk_n {
                    Some(n) => inner.reconcile_disk_nlink(old_child_ino.get(), n, u32::from(orig_old_links)),
                    None => inner.record_nlink_floor(old_child_ino.get(), u32::from(orig_old_links)),
                }
                MoveOutcome {
                    moved: Landed::Yes(Err(err)),
                    sync_old_child: clamp_to_bounds(inner, old_child_ino, child_disk_n),
                    sync_old_dir: read_bounded(inner, old_dir_ino),
                    sync_new_dir: read_bounded(inner, new_dir_ino),
                }
            }
            Ok(_) | Err(_) => {
                inner.record_nlink_floor(old_child_ino.get(), u32::from(orig_old_links));
                MoveOutcome {
                    moved: Landed::No(err),
                    sync_old_child: read_bounded(inner, old_child_ino),
                    sync_old_dir: read_bounded(inner, old_dir_ino),
                    sync_new_dir: read_bounded(inner, new_dir_ino),
                }
            }
        }
    }

    /// The unlink landed, so the move is done on disk; for a directory, fix
    /// the old parent's count (between parents one less, as after an
    /// `rmdir`; within one, the count from before, which the link raised)
    /// and, between parents, the child's `..`. A failure there is
    /// reported, the move kept.
    fn finish(&self, inner: &mut Ext4FsInner) -> MoveOutcome {
        let MoveEntry {
            old_dir_ino,
            new_dir_ino,
            old_child_ino,
            same_parent,
            old_is_dir,
            old_dir_nlink_before,
            orig_old_links,
            orig_np_links,
            ..
        } = *self;
        inner.clear_nlink_bounds(old_child_ino.get());
        let (sync_old_dir, sync_new_dir, res) = if !old_is_dir {
            (None, None, Ok(()))
        } else if same_parent {
            // `Dir::link` wrote the parent's count plus one and the unlink
            // left it, so this write always happens.
            let res = inner.write_links_or_pin(old_dir_ino, orig_np_links);
            let op_expected = Some(u32::from(orig_np_links));
            (op_expected, op_expected, res)
        } else {
            inner.clear_nlink_bounds(new_dir_ino.get());
            let (op_nlink, op_err) =
                inner.remove_dotdot_link(old_dir_ino, Some(old_dir_nlink_before.saturating_sub(1)));
            let dotdot = update_dir_dotdot(&inner.ext4, old_child_ino.get(), new_dir_ino.get());
            let res = match op_err {
                Some(e) => Err(e),
                None => dotdot,
            };
            (op_nlink, Some(u32::from(orig_np_links).saturating_add(1)), res)
        };
        MoveOutcome {
            moved: Landed::Yes(res),
            sync_old_child: Some(u32::from(orig_old_links)),
            sync_old_dir,
            sync_new_dir,
        }
    }

    /// Undo the link at `new_dname` and put the counts back: as they were if
    /// the new entry is gone, one up for the source (and the new parent, for
    /// a directory) if it stayed, a floor for the source if it can't be read
    /// (and for a directory's new parent, only if it had one); the counts
    /// for memory come back with `err`, the move undone.
    fn roll_back(&self, inner: &mut Ext4FsInner, err: VfsError) -> MoveOutcome {
        let MoveEntry {
            old_dir_ino,
            new_dir_ino,
            old_child_ino,
            old_dname,
            new_dname,
            old_is_dir,
            orig_old_links,
            orig_np_links,
            np_had_floor,
            ..
        } = *self;
        rollback_link_entry(&inner.ext4, new_dir_ino, new_dname, old_child_ino, orig_old_links.saturating_add(1));
        let new_still_has = dir_has_entry(&inner.ext4, new_dir_ino, new_dname, old_child_ino.get());
        if !matches!(new_still_has, Ok(false)) {
            log_rename_undo_failure(inner.dev, "rename", old_dir_ino, new_dir_ino);
        }
        // The counts with the source at both names.
        let src_both = orig_old_links.saturating_add(1).max(two_name_links(old_is_dir));
        let np_both = orig_np_links.saturating_add(1);
        match new_still_has {
            Ok(true) => {
                let _ = inner.write_links_or_pin(old_child_ino, src_both);
                if old_is_dir {
                    let _ = inner.write_links_or_pin(new_dir_ino, np_both);
                }
            }
            Ok(false) => {
                let _ = inner.write_links_or_pin(old_child_ino, orig_old_links);
                if old_is_dir {
                    let _ = inner.write_links_or_pin(new_dir_ino, orig_np_links);
                }
            }
            Err(_) => {
                inner.raise_links_or_floor(old_child_ino, orig_old_links);
                if old_is_dir && np_had_floor {
                    inner.raise_links_or_floor(new_dir_ino, orig_np_links);
                }
            }
        }
        let both_present = matches!(dir_has_entry(&inner.ext4, old_dir_ino, old_dname, old_child_ino.get()), Ok(true))
            && matches!(new_still_has, Ok(true));
        let mut sync_old_child = read_bounded(inner, old_child_ino);
        let mut sync_new_dir = read_bounded(inner, new_dir_ino);
        let sync_old_dir = read_bounded(inner, old_dir_ino);
        if both_present {
            let min_links = u32::from(src_both);
            sync_old_child = Some(sync_old_child.unwrap_or(min_links).max(min_links));
            if old_is_dir {
                let min_np = u32::from(np_both);
                sync_new_dir = Some(sync_new_dir.unwrap_or(min_np).max(min_np));
            }
        }
        MoveOutcome { moved: Landed::No(err), sync_old_child, sync_old_dir, sync_new_dir }
    }
}

impl MoveOutcome {
    /// Apply the counts to the source's and the parents' in-memory state
    /// (their Lock 4 guards).
    fn apply_counts(
        &self,
        guards: &mut [LockedInode<'_>],
        old_child: &Arc<Ext4Inode>,
        old_dir: &Arc<Ext4Inode>,
        new_dir: &Arc<Ext4Inode>,
    ) {
        set_counts(
            guards,
            [(old_child, self.sync_old_child), (old_dir, self.sync_old_dir), (new_dir, self.sync_new_dir)],
        );
    }

    /// Once a held target is settled ([`Ext4Inode::retire_target`] or
    /// [`Ext4Inode::put_back_target`], after [`Self::apply_counts`]): if the
    /// source moved, its and the parents' timestamps, and (a directory,
    /// neither parent's count read back, so the old parent's decrement never
    /// written) one more link on the parent that gained it. The result.
    fn stamp(
        self,
        guards: &mut [LockedInode<'_>],
        old_child: &Arc<Ext4Inode>,
        old_dir: &Arc<Ext4Inode>,
        new_dir: &Arc<Ext4Inode>,
        now: Timespec,
    ) -> Result<(), VfsError> {
        if let Landed::Yes(_) = self.moved {
            if old_child.inode_type == InodeType::Dir && self.sync_old_dir.is_none() && self.sync_new_dir.is_none() {
                let bumped = if old_dir.disk_ino == new_dir.disk_ino { old_dir } else { new_dir };
                let g = find_guard_mut(guards, bumped);
                g.nlink = g.nlink.saturating_add(1);
            }
            stamp_rename(guards, &[old_dir, new_dir], &[old_child], now);
        }
        self.moved.into_result()
    }
}

impl ExchangeSync {
    /// Apply the counts to the four inodes' in-memory state (their Lock 4
    /// guards), and the swap's timestamps if it happened; the result.
    fn apply(
        self,
        guards: &mut [LockedInode<'_>],
        old_child: &Arc<Ext4Inode>,
        new_child: &Arc<Ext4Inode>,
        old_dir: &Arc<Ext4Inode>,
        new_dir: &Arc<Ext4Inode>,
        now: Timespec,
    ) -> Result<(), VfsError> {
        set_counts(
            guards,
            [
                (old_child, self.old_child_nlink),
                (new_child, self.new_child_nlink),
                (old_dir, self.old_dir_nlink),
                (new_dir, self.new_dir_nlink),
            ],
        );
        if let Landed::Yes(_) = self.swap {
            stamp_rename(guards, &[old_dir, new_dir], &[old_child, new_child], now);
        }
        self.swap.into_result()
    }
}

/// Set each inode's count in memory (its Lock 4 guard) to the one given
/// (`None`: unchanged).
fn set_counts<const N: usize>(guards: &mut [LockedInode<'_>], counts: [(&Arc<Ext4Inode>, Option<u32>); N]) {
    for (inode, nlink) in counts {
        if let Some(n) = nlink {
            find_guard_mut(guards, inode).nlink = n;
        }
    }
}

/// A rename's timestamps, in memory: each of `dirs` modified at `now`, each
/// of `children` changed at `now`.
fn stamp_rename(guards: &mut [LockedInode<'_>], dirs: &[&Arc<Ext4Inode>], children: &[&Arc<Ext4Inode>], now: Timespec) {
    for dir in dirs {
        let g = find_guard_mut(guards, dir);
        g.times.modified(now);
        g.meta_dirty = true;
    }
    for child in children {
        let g = find_guard_mut(guards, child);
        g.times.set_ctime(now);
        g.meta_dirty = true;
    }
}

/// What became of a namespace operation's child: the name `unlink` or
/// `rmdir` took out, or a rename's replaced target. [`Ext4Inode::settle_child`]
/// applies it.
enum ChildFate {
    /// Its entry is meant to be still there, with this many links: the
    /// operation failed before taking it out, or put it back (a failed
    /// put-back is logged where it happens).
    Kept(u32),
    /// Its entry is gone and it keeps `links` links. `links` can be 0 when
    /// the disk, read back, still counts a link the entries don't call for
    /// (0 is then pinned in memory): the inode isn't freed (an orphan for
    /// `e2fsck`), and its prepared detach is dropped, not committed.
    Remaining { links: u32, now: Timespec },
    /// That entry was its last link: its inode is freed on disk, or left
    /// for `e2fsck` when the free failed (logged where it happens). `ctime`
    /// is the change time to stamp on it (`rmdir` stamps none).
    Freed { ctime: Option<Timespec> },
}

/// The inode `dname` names in `parent_ino`, read through the parent's inode
/// and its directory blocks.
fn entry_at(ext4: &Ext4, parent_ino: NonZeroU32, dname: DirEntryName<'_>) -> Result<Inode, Ext4Error> {
    Dir::open_inode(ext4, Inode::read(ext4, parent_ino)?)?.get_entry(dname)
}

/// A link count for ext4's 16-bit `links_count`: one past it (beyond
/// ext4's link limit, which `Dir::link` refuses to pass) is capped.
fn links_u16(n: u32) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// `ino` read back after an entry of its, with `effective` links before,
/// went: `None` if that was its last link or the inode has none left on
/// disk (so it may be freed).
fn surviving_inode(ext4: &Ext4, ino: NonZeroU32, is_dir: bool, effective: u32) -> Option<Inode> {
    if is_last_link(is_dir, effective) {
        return None;
    }
    Inode::read(ext4, ino).ok().filter(|cur| cur.links_count() > 0)
}

/// Whether an entry is its inode's last link, given its count (a
/// directory's own `.` is one of them).
fn is_last_link(is_dir: bool, links: u32) -> bool {
    if is_dir { links <= 2 } else { links <= 1 }
}

/// The least count of an inode with two names (a directory's own `.` is
/// one more).
fn two_name_links(is_dir: bool) -> u16 {
    if is_dir { 3 } else { 2 }
}

impl ChildFate {
    /// The entry went at `now`, leaving `remaining` links (`None`: it was
    /// the last, and the inode gets `freed_ctime`).
    fn gone(remaining: Option<u32>, now: Timespec, freed_ctime: Option<Timespec>) -> Self {
        match remaining {
            Some(links) => Self::Remaining { links, now },
            None => Self::Freed { ctime: freed_ctime },
        }
    }
}

enum UnlinkDiskOutcome {
    Completed { remaining_nlink: Option<u32>, post_err: Option<VfsError> },
    Aborted { err: VfsError, actual_nlink: u32 },
}

enum RmdirDiskOutcome {
    Removed { remaining_nlink: Option<u32>, parent_nlink: Option<u32>, post_err: Option<VfsError> },
    StillPresent { err: VfsError, actual_nlink: u32 },
}

struct SavedInlineSymlink {
    off: u64,
    span: [u8; 0x6c],
}

impl SavedInlineSymlink {
    /// Put the saved size and inline target back (one retry). If both writes
    /// fail the symlink's target stays zeroed on disk: logged, since the
    /// caller only reports the original unlink error.
    fn restore(&self, inner: &Ext4FsInner, disk_ino: u32) {
        if inner.bcache.write_at(self.off + 0x04, &self.span).is_ok()
            || inner.bcache.write_at(self.off + 0x04, &self.span).is_ok()
        {
            return;
        }
        error!(target: "vfs", "ext4 inode {disk_ino}: restoring the inline symlink target failed; run e2fsck");
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
    if ext4_read_full(ext4, &dir_inode, &mut hdr, 0).map_err(map_ext4_err)? != hdr.len() {
        return Err(VfsError::Io);
    }
    // Verify standard block-0 `..` entry at offset 12..22 (`name_len == 2`, `FT_DIR == 2`, `".."`).
    if hdr[18] != 2 || hdr[19] != 2 || &hdr[20..22] != b".." {
        return Err(VfsError::Io);
    }
    let bytes = new_parent_ino.to_le_bytes();
    ext4_write_all(ext4, &mut dir_inode, &bytes, 12).map_err(map_ext4_err)
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
    match entry_at(ext4, parent_ino, dname) {
        Ok(entry) => Ok(entry.index.get() == expected_ino),
        Err(Ext4Error::NotFound) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether `parent_ino` has no entry `dname`; a read error is reported, not
/// taken as either answer.
fn dir_entry_absent(ext4: &Ext4, parent_ino: NonZeroU32, dname: DirEntryName<'_>) -> Result<bool, Ext4Error> {
    match entry_at(ext4, parent_ino, dname) {
        Ok(_) => Ok(false),
        Err(Ext4Error::NotFound) => Ok(true),
        Err(e) => Err(e),
    }
}

/// Release an inode no entry points at any more: one `create`/`mkdir`/
/// `symlink` allocated before a later step failed, or a rename's replaced
/// target once the move has landed (`why` says which, for the log).
/// `ext4plus` frees inodes only through `Dir::unlink`, so the inode is linked
/// at `dname` (if that step didn't already happen) and unlinked again, which
/// frees it and its blocks; a directory's link also raises the parent's
/// count, which is put back. An inline symlink's target is zeroed first
/// (`zero_inline_symlink_before_unlink`). Best effort, logged on failure (the
/// orphan then stays until `e2fsck`); the caller reports its own error either
/// way. Needs a free slot in the directory, which `CREATE_RESERVE_BLOCKS`
/// keeps available when the failure was running out of space.
fn discard_orphan(
    inner: &mut Ext4FsInner,
    parent_ino: NonZeroU32,
    dname: DirEntryName<'_>,
    orphan_ino: NonZeroU32,
    why: &str,
) {
    if let Err(e) = try_discard_orphan(inner, parent_ino, dname, orphan_ino, None) {
        error!(
            target: "vfs",
            "ext4: freeing inode {orphan_ino} {why} in directory {parent_ino} failed: {e:?}; run e2fsck"
        );
    }
}

/// How many `.lumen-rename-<ino>[-n]` names [`unused_temp_name`] tries.
const TEMP_NAME_TRIES: u32 = 64;

/// A name in `dir_ino` to link `ino` at while freeing it: the first of
/// `.lumen-rename-<ino>`, `.lumen-rename-<ino>-1`, ... that is free, or that
/// already points at `ino` (left by an earlier free of it that crashed).
/// Users can create these names too (or a rename can land on one), so a
/// taken one is skipped rather than assumed free.
fn unused_temp_name(ext4: &Ext4, dir_ino: NonZeroU32, ino: NonZeroU32) -> Result<alloc::string::String, VfsError> {
    let dir = Dir::open_inode(ext4, Inode::read(ext4, dir_ino).map_err(map_ext4_err)?).map_err(map_ext4_err)?;
    for n in 0..TEMP_NAME_TRIES {
        let name =
            if n == 0 { alloc::format!(".lumen-rename-{ino}") } else { alloc::format!(".lumen-rename-{ino}-{n}") };
        let dname = DirEntryName::try_from(name.as_str()).map_err(|_| VfsError::Inval)?;
        match dir.get_entry(dname) {
            Err(Ext4Error::NotFound) => return Ok(name),
            Ok(entry) if entry.index == ino => return Ok(name),
            Ok(_) => continue,
            Err(e) => return Err(map_ext4_err(e)),
        }
    }
    Err(VfsError::Exist)
}

/// `discard_orphan` without the log: the error, for a caller that retries.
/// `parent_after` is the count a directory orphan's parent ends at; `None`
/// works it out from the count it reads (right for a first attempt only: a
/// retry finds the parent already put back).
fn try_discard_orphan(
    inner: &mut Ext4FsInner,
    parent_ino: NonZeroU32,
    dname: DirEntryName<'_>,
    orphan_ino: NonZeroU32,
    parent_after: Option<u16>,
) -> Result<(), VfsError> {
    // A directory's link raises the parent's count and the unlink won't
    // lower it, so the parent's count is put back on every exit once the
    // link may have run (including a link that failed after raising it).
    let mut parent_restore: Option<u16> = None;
    let res = (|| -> Result<(), VfsError> {
        let ext4 = &inner.ext4;
        let open_parent = || -> Result<Dir, VfsError> {
            Dir::open_inode(ext4, Inode::read(ext4, parent_ino).map_err(map_ext4_err)?).map_err(map_ext4_err)
        };
        let parent_links = Inode::read(ext4, parent_ino).map_err(map_ext4_err)?.links_count();
        let mut orphan = Inode::read(ext4, orphan_ino).map_err(map_ext4_err)?;
        let is_dir = orphan.file_type().is_dir();
        if orphan.file_type().is_symlink() && !orphan.flags().contains(InodeFlags::EXTENTS) && orphan.blocks() == 0 {
            orphan.set_links_count(0);
            orphan.write(ext4).map_err(map_ext4_err)?;
            orphan = Ext4Inode::zero_inline_symlink_before_unlink(inner, orphan_ino.get(), orphan, 0)?.0;
        }
        // If the failed create's own link already landed (and raised the
        // parent's count), the parent ends one below the count read above.
        let linked = dir_has_entry(ext4, parent_ino, dname, orphan_ino.get()).map_err(map_ext4_err)?;
        if is_dir {
            parent_restore =
                Some(parent_after.unwrap_or(if linked { parent_links.saturating_sub(1) } else { parent_links }));
        }
        if !linked {
            let mut dir = open_parent()?;
            ensure_absent(&dir, dname)?;
            orphan.set_links_count(0);
            dir.link(dname, &mut orphan).map_err(map_ext4_err)?;
        }
        let mut orphan = Inode::read(ext4, orphan_ino).map_err(map_ext4_err)?;
        orphan.set_links_count(1);
        open_parent()?.unlink(dname, orphan).map_err(map_ext4_err)?;
        Ok(())
    })();
    if let Some(links) = parent_restore
        && restore_disk_links_count(&inner.ext4, parent_ino, links).is_err()
    {
        // Keep the right count in memory (logged there).
        inner.set_nlink_exact(parent_ino.get(), u32::from(links));
    }
    res
}

/// Undo a link of `child_ino` at `dname` in `parent_ino`, if it is there:
/// raise the child's count so the unlink can't free it, then unlink. Best
/// effort and silent: the callers re-probe the entries afterwards (often
/// retrying once writes succeed again) and log the end state if it is wrong.
fn rollback_link_entry(
    ext4: &Ext4,
    parent_ino: NonZeroU32,
    dname: DirEntryName<'_>,
    child_ino: NonZeroU32,
    min_links_before_unlink: u16,
) {
    if !matches!(dir_has_entry(ext4, parent_ino, dname, child_ino.get()), Ok(true)) {
        return;
    }
    let _attempt = (|| -> Result<(), Ext4Error> {
        let mut child_disk = Inode::read(ext4, child_ino)?;
        let needed = min_links_before_unlink.max(2);
        if child_disk.links_count() < needed {
            child_disk.set_links_count(needed);
            child_disk.write(ext4)?;
        }
        let parent_disk = Inode::read(ext4, parent_ino)?;
        Dir::open_inode(ext4, parent_disk)?.unlink(dname, child_disk)?;
        Ok(())
    })();
}

/// Put back a removed entry `dname` -> `child_ino` in `parent_ino`, if the
/// name is free. Best effort and silent, like [`rollback_link_entry`].
fn rollback_restore_entry(ext4: &Ext4, parent_ino: NonZeroU32, dname: DirEntryName<'_>, child_ino: NonZeroU32) {
    if !matches!(dir_entry_absent(ext4, parent_ino, dname), Ok(true)) {
        return;
    }
    let _attempt = (|| -> Result<(), Ext4Error> {
        let mut child_disk = Inode::read(ext4, child_ino)?;
        let parent_disk = Inode::read(ext4, parent_ino)?;
        Dir::open_inode(ext4, parent_disk)?.link(dname, &mut child_disk)
    })();
}

/// Log that undoing a failed rename couldn't put the entries back as they
/// were (or couldn't read them to check).
fn log_rename_undo_failure(dev: DevId, what: &str, old_dir: NonZeroU32, new_dir: NonZeroU32) {
    error!(
        target: "vfs",
        "ext4 {dev:?}: undoing a failed {what} between directories {old_dir} and {new_dir} left its entries \
         inconsistent or unreadable; run e2fsck"
    );
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
        // Blocks are allocated only at writeback, so until then the disk's
        // count lags what was written. Dirty pages stand in for the data
        // still to be allocated (each 8 sectors): never holes, which a
        // size-based guess would count for a sparse file, and never more
        // than was written. Lock 4 (meta) then Lock 5 (pages), in order.
        let blocks = if self.inode_type == InodeType::File {
            let dirty = self.pages.lock().dirty_count() as u64;
            meta.blocks_512.max(dirty * (PAGE_SIZE as u64 / 512))
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
            rdev: self.rdev,
            size: meta.size,
            blksize: meta.blksize,
            blocks,
            atime: meta.times.atime(),
            mtime: meta.times.mtime(),
            ctime: meta.times.ctime(),
        })
    }

    fn lookup(&self, name: &[u8]) -> Result<Arc<dyn VfsInode>, VfsError> {
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
        let child = inner.get_or_create_inode(child_disk, &self.shared)?;
        Ok(child as Arc<dyn VfsInode>)
    }

    fn create(&self, name: &[u8], mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.shared.read_only {
            return Err(VfsError::RoFs);
        }
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if name == b"." || name == b".." {
            return Err(VfsError::Exist);
        }
        let mut meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let dname = entry_name(name)?;
        let now_ts = self.shared.now();

        let mut inner = self.shared.state.lock();
        let parent_disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
        let mut parent_dir = Dir::open_inode(&inner.ext4, parent_disk).map_err(map_ext4_err)?;
        ensure_absent(&parent_dir, dname)?;
        ensure_create_reserve(&inner.ext4)?;

        let mode = InodeMode::S_IFREG | InodeMode::from_bits_retain(mode & 0o7777);
        let mut child_disk = new_inode(&inner.ext4, FileType::Regular, mode, now_ts).map_err(map_ext4_err)?;
        inner.clear_nlink_bounds(child_disk.index.get());
        if let Err(e) = parent_dir.link(dname, &mut child_disk) {
            discard_orphan(&mut inner, self.disk_inode_index(), dname, child_disk.index, "after a failed create");
            return Err(map_ext4_err(e));
        }
        let child = inner.created_inode(child_disk, &self.shared);
        meta.times.modified(now_ts);
        meta.meta_dirty = true;
        Ok(child as Arc<dyn VfsInode>)
    }

    fn mkdir(&self, name: &[u8], mode: u16) -> Result<Arc<dyn VfsInode>, VfsError> {
        if self.shared.read_only {
            return Err(VfsError::RoFs);
        }
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if name == b"." || name == b".." {
            return Err(VfsError::Exist);
        }
        let mut meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let dname = entry_name(name)?;
        let now_ts = self.shared.now();

        let mut inner = self.shared.state.lock();
        let mut parent_disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
        let parent_eff = inner.apply_nlink_bounds(self.disk_ino, u32::from(parent_disk.links_count()));
        inner.store_bounded_links(&mut parent_disk, links_u16(parent_eff))?;
        let parent_check = Dir::open_inode(&inner.ext4, parent_disk).map_err(map_ext4_err)?;
        ensure_absent(&parent_check, dname)?;
        drop(parent_check);
        ensure_create_reserve(&inner.ext4)?;
        // The new directory's `..` raises the parent's count; past ext4's
        // link limit `Dir::link` would fail after the inode and its block are
        // allocated (and so would `discard_orphan`'s own link). Refused before
        // allocating anything (Linux says EMLINK, which `VfsError` lacks).
        if parent_eff >= EXT4_LINK_MAX {
            return Err(VfsError::NoSpc);
        }

        let mode = InodeMode::S_IFDIR | InodeMode::from_bits_retain(mode & 0o7777);
        let sub_raw = new_inode(&inner.ext4, FileType::Directory, mode, now_ts).map_err(map_ext4_err)?;
        inner.clear_nlink_bounds(sub_raw.index.get());
        let sub_ino = sub_raw.index;
        let linked = Dir::init(inner.ext4.clone(), sub_raw, self.disk_inode_index()).and_then(|sub_dir| {
            drop(sub_dir);
            let mut sub_inode = Inode::read(&inner.ext4, sub_ino)?;
            let parent_disk = Inode::read(&inner.ext4, self.disk_inode_index())?;
            Dir::open_inode(&inner.ext4, parent_disk)?.link(dname, &mut sub_inode)?;
            Ok(sub_inode)
        });
        let sub_inode = match linked {
            Ok(sub_inode) => sub_inode,
            Err(e) => {
                inner.undo_failed_link(self.disk_inode_index(), dname, self.disk_inode_index(), parent_eff);
                discard_orphan(&mut inner, self.disk_inode_index(), dname, sub_ino, "after a failed create");
                return Err(map_ext4_err(e));
            }
        };
        inner.clear_nlink_bounds(self.disk_ino);
        // The new directory is linked: from here on mkdir succeeds. The inode
        // `link` wrote is the current one.
        let child = inner.created_inode(sub_inode, &self.shared);
        meta.nlink = meta.nlink.saturating_add(1);
        meta.times.modified(now_ts);
        meta.meta_dirty = true;
        Ok(child as Arc<dyn VfsInode>)
    }

    fn unlink(&self, name: &[u8]) -> Result<(), VfsError> {
        if self.shared.read_only {
            return Err(VfsError::RoFs);
        }
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if name == b"." || name == b".." {
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
            let self_arc = inner.get_or_create_inode(self_disk, &self.shared)?;
            let child_arc = inner.get_or_create_inode(child_disk, &self.shared)?;
            (self_arc, child_arc)
        };

        let mut guards = lock_inodes_ordered(&[&self_arc, &child_arc]);
        if self_arc.is_detached() || find_guard_mut(&mut guards, &self_arc).nlink == 0 {
            return Err(VfsError::NoEnt);
        }

        let now_ts = self.shared.now();
        let child_nlink = find_guard_mut(&mut guards, &child_arc).nlink;
        if child_arc.is_detached() || child_nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let prepared_detach = if is_last_link(false, child_nlink) && child_arc.open_count() > 0 {
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
                let child_meta = find_guard_mut(&mut guards, &child_arc);
                Self::settle_child(&child_arc, child_meta, prepared_detach, ChildFate::Kept(actual_nlink));
                Err(err)
            }
            UnlinkDiskOutcome::Completed { remaining_nlink, post_err } => {
                let child_meta = find_guard_mut(&mut guards, &child_arc);
                let fate = ChildFate::gone(remaining_nlink, now_ts, Some(now_ts));
                Self::settle_child(&child_arc, child_meta, prepared_detach, fate);
                let self_meta = find_guard_mut(&mut guards, &self_arc);
                self_meta.times.modified(now_ts);
                self_meta.meta_dirty = true;
                match post_err {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            }
        }
    }

    fn rmdir(&self, name: &[u8]) -> Result<(), VfsError> {
        if self.shared.read_only {
            return Err(VfsError::RoFs);
        }
        if self.inode_type != InodeType::Dir {
            return Err(VfsError::NotDir);
        }
        if name == b"." {
            return Err(VfsError::Inval);
        }
        if name == b".." {
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
            let self_arc = inner.get_or_create_inode(self_disk, &self.shared)?;
            let child_arc = inner.get_or_create_inode(child_disk, &self.shared)?;
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
        let prepared_detach = if is_last_link(true, child_nlink) && child_arc.open_count() > 0 {
            let child_meta = find_guard_mut(&mut guards, &child_arc);
            Self::prepare_detach(&child_arc, child_meta)?
        } else {
            None
        };

        let outcome = {
            let mut inner = self.shared.state.lock();
            Self::unlink_dir_with_recovery(&mut inner, self.disk_inode_index(), dname, &child_arc, child_nlink)?
        };

        let now_ts = self.shared.now();
        match outcome {
            RmdirDiskOutcome::StillPresent { err, actual_nlink } => {
                let child_meta = find_guard_mut(&mut guards, &child_arc);
                Self::settle_child(&child_arc, child_meta, prepared_detach, ChildFate::Kept(actual_nlink));
                Err(err)
            }
            RmdirDiskOutcome::Removed { remaining_nlink, parent_nlink, post_err } => {
                // The child was checked live under its guard, which every
                // detach holds, so a freed one is never detached already.
                let child_meta = find_guard_mut(&mut guards, &child_arc);
                // A removed directory gets no change time (as before).
                let fate = ChildFate::gone(remaining_nlink, now_ts, None);
                Self::settle_child(&child_arc, child_meta, prepared_detach, fate);
                if let Some(p_nlink) = parent_nlink {
                    let self_meta = find_guard_mut(&mut guards, &self_arc);
                    self_meta.nlink = p_nlink;
                    if post_err.is_none() {
                        self_meta.times.modified(now_ts);
                        self_meta.meta_dirty = true;
                    }
                }
                match post_err {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            }
        }
    }

    fn link(&self, name: &[u8], target: &Arc<dyn VfsInode>) -> Result<(), VfsError> {
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
        if name == b"." || name == b".." {
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
            let self_arc = inner.get_or_create_inode(self_disk, &self.shared)?;
            let target_arc = inner.get_or_create_inode(target_disk, &self.shared)?;
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
            ensure_absent(&parent_dir, dname)?;
            let mut target_disk = Inode::read(&inner.ext4, target_arc.disk_inode_index()).map_err(map_ext4_err)?;
            let target_eff = inner.apply_nlink_bounds(target_arc.disk_ino, u32::from(target_disk.links_count()));
            let target_eff_u16 = links_u16(target_eff);
            if target_disk.links_count() != target_eff_u16 {
                target_disk.set_links_count(target_eff_u16);
            }
            if let Err(e) = parent_dir.link(dname, &mut target_disk) {
                inner.undo_failed_link(self.disk_inode_index(), dname, target_arc.disk_inode_index(), target_eff);
                return Err(map_ext4_err(e));
            }
            inner.clear_nlink_bounds(target_arc.disk_ino);
        }

        let now_ts = self.shared.now();
        let t_meta = find_guard_mut(&mut guards, &target_arc);
        t_meta.nlink = t_meta.nlink.saturating_add(1);
        t_meta.times.set_ctime(now_ts);
        t_meta.meta_dirty = true;
        let p_meta = find_guard_mut(&mut guards, &self_arc);
        p_meta.times.modified(now_ts);
        p_meta.meta_dirty = true;
        Ok(())
    }

    fn symlink(&self, name: &[u8], target: &[u8]) -> Result<Arc<dyn VfsInode>, VfsError> {
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
        if name == b"." || name == b".." {
            return Err(VfsError::Exist);
        }
        let mut meta = self.meta.lock();
        if self.is_detached() || meta.nlink == 0 {
            return Err(VfsError::NoEnt);
        }
        let dname = entry_name(name)?;
        let now_ts = self.shared.now();

        let mut inner = self.shared.state.lock();
        let parent_disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
        let mut parent_dir = Dir::open_inode(&inner.ext4, parent_disk).map_err(map_ext4_err)?;
        ensure_absent(&parent_dir, dname)?;
        ensure_create_reserve(&inner.ext4)?;

        let mode = InodeMode::S_IFLNK | InodeMode::from_bits_retain(0o777);
        let mut sym = new_inode(&inner.ext4, FileType::Symlink, mode, now_ts).map_err(map_ext4_err)?;
        let sym_ino = sym.index;
        inner.clear_nlink_bounds(sym_ino.get());
        // A target of up to 59 bytes is stored inline in `i_block` (a fast
        // symlink, no `EXTENTS` flag, no blocks); a longer one in a data
        // block. `ext4plus::Ext4::symlink` isn't used: when its final link
        // fails it doesn't hand back the inode it allocated, which then
        // couldn't be freed.
        let built = if target.len() <= 59 {
            Self::write_inline_symlink_target(&inner, sym, target)
        } else {
            ext4_write_all(&inner.ext4, &mut sym, target, 0).map_err(map_ext4_err).map(|()| sym)
        };
        let sym_disk =
            match built.and_then(|mut sym| parent_dir.link(dname, &mut sym).map(|()| sym).map_err(map_ext4_err)) {
                Ok(sym) => sym,
                Err(e) => {
                    discard_orphan(&mut inner, self.disk_inode_index(), dname, sym_ino, "after a failed create");
                    return Err(e);
                }
            };

        let child = inner.created_inode(sym_disk, &self.shared);
        meta.times.modified(now_ts);
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
        old_name: &[u8],
        new_dir: &Arc<dyn VfsInode>,
        new_name: &[u8],
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
        if old_name == b"." || old_name == b".." || new_name == b"." || new_name == b".." {
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

            let old_dir_arc = inner.get_or_create_inode(old_parent_disk, &self.shared)?;
            let new_dir_arc = inner.get_or_create_inode(new_parent_disk, &self.shared)?;
            let old_child_arc = inner.get_or_create_inode(old_child_disk, &self.shared)?;
            let new_child_opt = new_child_disk_opt.map(|d| inner.get_or_create_inode(d, &self.shared)).transpose()?;
            (old_dir_arc, new_dir_arc, old_child_arc, new_child_opt)
        };

        // The disk inodes were found by number: if either directory was
        // removed and its number reused (a `rmdir` and a `mkdir` between the
        // caller's lookup and here), the live inode for that number is a
        // different one, and the entry must not move into the stranger.
        if old_dir_arc.lock_id != self.lock_id || new_dir_arc.lock_id != new_dir.lock_id() {
            return Err(VfsError::NoEnt);
        }

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

        let now_ts = self.shared.now();

        if flags.has_exchange() {
            let new_child_arc = new_child_opt.as_ref().expect("checked Some for EXCHANGE");
            if old_child_arc.disk_ino == new_child_arc.disk_ino {
                return Ok(());
            }
            let outcome = {
                let mut inner = self.shared.state.lock();
                Exchange::run(
                    &mut inner,
                    &old_dir_arc,
                    &new_dir_arc,
                    &old_child_arc,
                    new_child_arc,
                    old_dname,
                    new_dname,
                )?
            };
            return outcome.apply(&mut guards, &old_child_arc, new_child_arc, &old_dir_arc, &new_dir_arc, now_ts);
        }

        // Standard overwrite / move rename. An existing target is held, not
        // removed: its entry comes out but its inode keeps its count, so its
        // data stays on disk until the source has landed at `new_name`. If
        // the move fails, the entry is put back (POSIX: a failed rename
        // leaves the target as it was); once it lands, the target loses that
        // link and, if it was the last, its inode.
        let held = match new_child_opt {
            Some(ref target) => Some(self.hold_target(&mut guards, &new_dir_arc, new_dname, target)?),
            None => None,
        };

        let old_dir_nlink_before = find_guard_mut(&mut guards, &old_dir_arc).nlink;
        // An early error (before anything moved) still reaches the held
        // target's put-back below.
        let step2 = {
            let mut inner = self.shared.state.lock();
            MoveEntry::run(
                &mut inner,
                &old_dir_arc,
                &new_dir_arc,
                &old_child_arc,
                old_dname,
                new_dname,
                old_dir_nlink_before,
            )
        };
        let step2 = match step2 {
            Ok(step2) => step2,
            Err(e) => {
                if let Some(target) = held {
                    self.put_back_target(&mut guards, &new_dir_arc, new_dname, target);
                }
                return Err(e);
            }
        };

        step2.apply_counts(&mut guards, &old_child_arc, &old_dir_arc, &new_dir_arc);
        if let Some(target) = held {
            match step2.moved {
                Landed::Yes(_) => self.retire_target(&mut guards, &new_dir_arc, target, now_ts),
                Landed::No(_) => self.put_back_target(&mut guards, &new_dir_arc, new_dname, target),
            }
        }
        step2.stamp(&mut guards, &old_child_arc, &old_dir_arc, &new_dir_arc, now_ts)
    }

    /// The cursor is an entry's byte position in the directory's data, as
    /// Linux uses for a linear directory; the next cursor is the position
    /// plus 1. In a linear directory `ext4plus` never moves a record:
    /// removing one merges it into the record before (or zeroes its inode),
    /// and adding one takes the slack of the first record with room (which
    /// keeps its own position) or a new block at the end. So a listing
    /// resumed after removals or additions lists every entry that stayed
    /// exactly once. Two exceptions (ARCHITECTURE section 9):
    /// - a rename over an existing name, or `RENAME_EXCHANGE`, is an unlink
    ///   and a link: the name that stayed (the target) gets a new record,
    ///   which first-fit may place behind the cursor (skipped) or ahead of
    ///   it (listed again);
    /// - a hashed (htree) directory: an insert repacks a leaf block, split
    ///   or not, moving its records. `ext4plus` never makes one, so only
    ///   directories made outside Lumen (`mke2fs -d`, the Alpine image)
    ///   can be.
    ///
    /// Each call walks the directory from its start (`ReadDir` has no
    /// seek), reading the raw records a block at a time beside it for their
    /// positions and checking that both agree (inode number and name) and
    /// end together: a disagreement or a malformed record is `EIO`, logged
    /// once per directory.
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
        let mut raw = LiveDirents::new(&inner.ext4, disk.clone(), inner.layout.block_size as usize);
        let dir = Dir::open_inode(&inner.ext4, disk).map_err(map_ext4_err)?;
        let rd = dir.read_dir().map_err(map_ext4_err)?;
        let mut finished = true;
        for (idx, item) in rd.enumerate() {
            let entry = item.map_err(map_ext4_err)?;
            let live = match raw.next() {
                Ok(Some(live)) => live,
                Ok(None) => return Err(self.bad_listing(format_args!("entry {idx}: the raw records ran out first"))),
                Err(e) => return Err(self.walk_failed(e)),
            };
            if live.ino != entry.inode.get() || live.name.as_slice() != entry.file_name().as_ref() {
                return Err(self.bad_listing(format_args!(
                    "entry {idx} at {}: the raw record (inode {}) isn't ReadDir's (inode {})",
                    live.pos, live.ino, entry.inode
                )));
            }
            let pos = live.pos;
            if pos < cursor {
                continue;
            }
            // The name's bytes, UTF-8 or not (#268).
            let name = entry.file_name().as_ref().to_vec();
            let inode_type = match entry.file_type() {
                Ok(ft) => map_file_type(ft),
                Err(_) => InodeType::File,
            };
            out.push(DirEntry { ino: u64::from(entry.inode.get()), inode_type, name, next_cursor: pos + 1 });
            if out.len() >= max_entries {
                finished = false;
                break;
            }
        }
        // `ReadDir` is done: so must the raw records be.
        if finished {
            match raw.next() {
                Ok(None) => {}
                Ok(Some(live)) => {
                    return Err(self.bad_listing(format_args!(
                        "a raw record at {} (inode {}) past ReadDir's last entry",
                        live.pos, live.ino
                    )));
                }
                Err(e) => return Err(self.walk_failed(e)),
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
        if new_size > self.shared.max_file_size {
            return Err(VfsError::FBig);
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
                        let got = ext4_read_full(&inner.ext4, &disk, &mut probe, new_size).map_err(map_ext4_err)?;
                        if probe[..got].iter().any(|&b| b != 0) {
                            let zeros = vec![0u8; got];
                            if let Err(e) = ext4_write_all(&inner.ext4, &mut disk, &zeros, new_size) {
                                if ext4_write_all(&inner.ext4, &mut disk, &probe[..got], new_size).is_err() {
                                    self.log_tail_restore_failure();
                                }
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
                        if ext4_write_all(&inner.ext4, &mut restore_disk, &orig_tail, new_size).is_err() {
                            self.log_tail_restore_failure();
                        }
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

        let now_ts = self.shared.now();
        meta.size = new_size;
        meta.times.modified(now_ts);
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
        let mut times = meta.times;
        times.set_ctime(self.shared.now());
        let perm = mode & 0o7777;
        if !self.is_detached() {
            let inner = self.shared.state.lock();
            let mut disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
            let full_mode = (self.inode_type.to_mode_bits() as u16) | perm;
            disk.set_mode(InodeMode::from_bits_retain(full_mode)).map_err(map_ext4_err)?;
            times.store_ctime(&mut disk);
            disk.write(&inner.ext4).map_err(map_ext4_err)?;
        }
        meta.mode = perm;
        meta.times = times;
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
        let mut times = meta.times;
        times.set_ctime(self.shared.now());
        let new_uid = uid.unwrap_or(meta.uid);
        let new_gid = gid.unwrap_or(meta.gid);
        // The set-id bits a chown drops, in the same hold and disk write as
        // the owner (`VfsInode::set_owner`'s contract).
        let new_mode = mode_after_chown(self.inode_type, meta.mode);
        if !self.is_detached() {
            let inner = self.shared.state.lock();
            let mut disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
            disk.set_uid(new_uid);
            disk.set_gid(new_gid);
            if new_mode != meta.mode {
                let full_mode = (self.inode_type.to_mode_bits() as u16) | new_mode;
                disk.set_mode(InodeMode::from_bits_retain(full_mode)).map_err(map_ext4_err)?;
            }
            times.store_ctime(&mut disk);
            disk.write(&inner.ext4).map_err(map_ext4_err)?;
        }
        meta.uid = new_uid;
        meta.gid = new_gid;
        meta.mode = new_mode;
        meta.times = times;
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
        let now_ts = self.shared.now();
        // Set in memory only once the disk holds them.
        let mut times = meta.times;
        match atime {
            SetTime::Omit => {}
            SetTime::Now => times.set_atime(now_ts),
            SetTime::Set(ts) => times.set_atime(ts),
        }
        match mtime {
            SetTime::Omit => {}
            SetTime::Now => times.set_mtime(now_ts),
            SetTime::Set(ts) => times.set_mtime(ts),
        }
        times.set_ctime(now_ts);
        if !self.is_detached() {
            let inner = self.shared.state.lock();
            let mut disk = Inode::read(&inner.ext4, self.disk_inode_index()).map_err(map_ext4_err)?;
            times.store(&mut disk);
            disk.write(&inner.ext4).map_err(map_ext4_err)?;
        }
        meta.times = times;
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
                let cp = self.load_page(page_idx, meta.size)?;
                cp.read_bytes(in_page, &mut buf[done..done + chunk]);
                pages.insert_new(page_idx, cp)?;
            }
            done += chunk;
        }
        Ok(to_read)
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

    fn sync(&self, data_only: bool) -> Result<(), VfsError> {
        if self.is_detached() || self.shared.read_only {
            return Ok(());
        }
        self.flush_to_ext4(data_only)?;
        // A global sync goes through the filesystem's own `sync` (which logs
        // a block-cache failure once, not once per inode), never through
        // here: a per-inode sync (`fsync`/`fdatasync`) logs its own, once
        // per failing streak.
        match self.shared.sync_bcache() {
            Ok(()) => {
                self.shared.bcache_sync_errno.store(0, Ordering::Release);
                Ok(())
            }
            Err((e, cause)) => {
                if self.shared.bcache_sync_errno.swap(e.to_errno(), Ordering::AcqRel) != e.to_errno() {
                    warn!(target: "vfs", "fsync: ext4 {:?} inode {}: block cache: {cause}", self.shared.dev, self.disk_ino);
                }
                Err(e)
            }
        }
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

/// Test seams for the ktests below: a block cache that counts and injects
/// faults, and mount, remount and image helpers.
#[cfg(feature = "ktest")]
pub(crate) mod ktest_util {
    use alloc::sync::Arc;
    use alloc::vec;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    use lumen_bcache::{BlockCache, BlockError};
    use lumen_vfs::{DevId, FileSystem, VfsInode};

    use super::{BlockCacheBackend, Ext4FileSystem};
    use crate::sync::SleepMutex;
    use crate::vfs::{KernelVfsTables, MemBlkDev};

    /// A block cache over an in-memory device that counts reads and writes
    /// and fails them on request: one write by its number, two, or every
    /// write from one on (each counted from the `arm_write*` call that set
    /// it, which disarms the others), every read or write, syncs, or reads
    /// of a range.
    pub struct FaultCache {
        inner: SleepMutex<BlockCache<MemBlkDev>>,
        pub(in crate::vfs) dev: MemBlkDev,
        pub reads: AtomicUsize,
        pub writes: AtomicUsize,
        /// Fail the write with this number (counting from `arm_write`), once;
        /// 0 for none.
        fail_write_at: AtomicUsize,
        /// A second write to fail, once (`arm_write_pair`); 0 for none.
        fail_write_at2: AtomicUsize,
        /// Fail every write from this number on (`arm_write_from`); 0 for
        /// none.
        fail_write_from: AtomicUsize,
        /// After that write fails, fail every read until cleared.
        pub reads_fail_after_write_fault: AtomicBool,
        /// After that write fails, fail every read of the range set by
        /// `fail_reads_in` until cleared.
        pub range_reads_fail_after_write_fault: AtomicBool,
        pub fail_reads: AtomicBool,
        pub fail_writes: AtomicBool,
        /// Fail only `sync` (written blocks stay in the cache, dirty).
        pub fail_syncs: AtomicBool,
        /// Fail the next `fail_range_reads` reads overlapping `[lo, hi)`.
        range_lo: AtomicU64,
        range_hi: AtomicU64,
        fail_range_reads: AtomicUsize,
    }

    impl FaultCache {
        /// Fail write number `nth` from now (0: none).
        pub fn arm_write(&self, nth: usize) {
            self.arm_writes(nth, 0, 0);
        }

        /// Fail writes number `first` and `second` from now.
        pub fn arm_write_pair(&self, first: usize, second: usize) {
            self.arm_writes(first, second, 0);
        }

        /// Fail every write from number `from` on, counting from now.
        pub fn arm_write_from(&self, from: usize) {
            self.arm_writes(0, 0, from);
        }

        fn arm_writes(&self, at: usize, at2: usize, from: usize) {
            self.writes.store(0, Ordering::Release);
            self.fail_write_at.store(at, Ordering::Release);
            self.fail_write_at2.store(at2, Ordering::Release);
            self.fail_write_from.store(from, Ordering::Release);
        }

        pub fn fail_reads_in(&self, lo: u64, hi: u64, count: usize) {
            self.range_lo.store(lo, Ordering::Release);
            self.range_hi.store(hi, Ordering::Release);
            self.fail_range_reads.store(count, Ordering::Release);
        }

        pub fn clear(&self) {
            self.fail_write_at.store(0, Ordering::Release);
            self.fail_write_at2.store(0, Ordering::Release);
            self.fail_write_from.store(0, Ordering::Release);
            self.reads_fail_after_write_fault.store(false, Ordering::Release);
            self.range_reads_fail_after_write_fault.store(false, Ordering::Release);
            self.fail_reads.store(false, Ordering::Release);
            self.fail_writes.store(false, Ordering::Release);
            self.fail_syncs.store(false, Ordering::Release);
            self.fail_range_reads.store(0, Ordering::Release);
        }
    }

    impl BlockCacheBackend for FaultCache {
        fn read_at(&self, byte_off: u64, dst: &mut [u8]) -> Result<(), BlockError> {
            self.reads.fetch_add(1, Ordering::AcqRel);
            if self.fail_reads.load(Ordering::Acquire) {
                return Err(BlockError::Io);
            }
            let end = byte_off + dst.len() as u64;
            if byte_off < self.range_hi.load(Ordering::Acquire)
                && end > self.range_lo.load(Ordering::Acquire)
                && self.fail_range_reads.try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1)).is_ok()
            {
                return Err(BlockError::Io);
            }
            self.inner.lock().read_at(byte_off, dst)
        }

        fn write_at(&self, byte_off: u64, src: &[u8]) -> Result<(), BlockError> {
            if self.fail_writes.load(Ordering::Acquire) {
                return Err(BlockError::Io);
            }
            let n = self.writes.fetch_add(1, Ordering::AcqRel) + 1;
            let from = self.fail_write_from.load(Ordering::Acquire);
            if self.fail_write_at.load(Ordering::Acquire) == n
                || self.fail_write_at2.load(Ordering::Acquire) == n
                || (from != 0 && n >= from)
            {
                if self.reads_fail_after_write_fault.load(Ordering::Acquire) {
                    self.fail_reads.store(true, Ordering::Release);
                }
                if self.range_reads_fail_after_write_fault.load(Ordering::Acquire) {
                    self.fail_range_reads.store(usize::MAX, Ordering::Release);
                }
                return Err(BlockError::Io);
            }
            self.inner.lock().write_at(byte_off, src)
        }

        fn sync(&self) -> Result<(), BlockError> {
            if self.fail_writes.load(Ordering::Acquire) || self.fail_syncs.load(Ordering::Acquire) {
                return Err(BlockError::Io);
            }
            self.inner.lock().sync()
        }
    }

    /// A writable mount of an image over a [`FaultCache`], with VFS tables
    /// of its own that have it at `/`.
    pub struct Mnt {
        pub cache: Arc<FaultCache>,
        pub fs: Arc<Ext4FileSystem>,
        pub root: Arc<dyn VfsInode>,
        pub tables: KernelVfsTables,
    }

    pub fn mount(img: Vec<u8>, minor: u32) -> Result<Mnt, &'static str> {
        let dev = MemBlkDev::new(img, false);
        let cache = Arc::new(FaultCache {
            inner: SleepMutex::new(BlockCache::new(dev.clone(), 64).map_err(|_| "bcache")?),
            dev,
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            fail_write_at: AtomicUsize::new(0),
            fail_write_at2: AtomicUsize::new(0),
            fail_write_from: AtomicUsize::new(0),
            reads_fail_after_write_fault: AtomicBool::new(false),
            range_reads_fail_after_write_fault: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            fail_writes: AtomicBool::new(false),
            fail_syncs: AtomicBool::new(false),
            range_lo: AtomicU64::new(0),
            range_hi: AtomicU64::new(0),
            fail_range_reads: AtomicUsize::new(0),
        });
        let backend: Arc<dyn BlockCacheBackend> = Arc::clone(&cache) as Arc<dyn BlockCacheBackend>;
        let fs = Ext4FileSystem::mount(backend, DevId::makedev(0, minor), false).map_err(|_| "ext4 mount")?;
        let root = fs.root_inode();
        let tables = KernelVfsTables::new();
        tables.mount_root(Arc::clone(&fs) as Arc<dyn FileSystem>).map_err(|_| "mount_root")?;
        Ok(Mnt { cache, fs, root, tables })
    }

    /// The device's bytes after writing the block cache back.
    pub fn image(m: &Mnt) -> Result<Vec<u8>, &'static str> {
        m.cache.sync().map_err(|_| "cache sync")?;
        Ok(m.dev_bytes())
    }

    impl FaultCache {
        /// The device's bytes as they are, under the block cache.
        pub fn dev_bytes(&self) -> Vec<u8> {
            self.dev.0.data.lock().clone()
        }
    }

    impl Mnt {
        /// The device's bytes as they are, under the block cache.
        pub fn dev_bytes(&self) -> Vec<u8> {
            self.cache.dev_bytes()
        }
    }

    /// A fresh read-only mount of `img`.
    pub fn mount_ro_image(img: Vec<u8>, minor: u32) -> Result<Arc<Ext4FileSystem>, &'static str> {
        let dev = MemBlkDev::new(img, true);
        let bcache: Arc<dyn BlockCacheBackend> =
            Arc::new(SleepMutex::new(BlockCache::new(dev, 32).map_err(|_| "ro bcache")?));
        Ext4FileSystem::mount(bcache, DevId::makedev(0, minor), true).map_err(|_| "ro mount")
    }

    /// A fresh read-only mount of what `m` has written so far.
    pub fn remount_ro(m: &Mnt, minor: u32) -> Result<Arc<Ext4FileSystem>, &'static str> {
        mount_ro_image(image(m)?, minor)
    }

    pub fn free_inodes(fs: &Ext4FileSystem) -> u32 {
        fs.shared.state.lock().ext4.superblock().free_inodes_count()
    }

    pub fn free_blocks(fs: &Ext4FileSystem) -> u64 {
        fs.shared.state.lock().ext4.superblock().free_blocks_count()
    }

    /// Inode `ino`'s `(i_mode, i_links_count)` as stored, read raw (a zeroed
    /// inode doesn't parse).
    pub fn raw_mode_and_links(fs: &Ext4FileSystem, ino: u32) -> Result<(u16, u16), &'static str> {
        let inner = fs.shared.state.lock();
        let off = inner.layout.inode_disk_offset(&inner.bcache, ino).map_err(|_| "inode offset")?;
        let mut raw = [0u8; 0x1c];
        inner.bcache.read_at(off, &mut raw).map_err(|_| "inode read")?;
        Ok((u16::from_le_bytes([raw[0], raw[1]]), u16::from_le_bytes([raw[0x1a], raw[0x1b]])))
    }

    /// Whether `inode`'s inode is still allocated on `fs`: a mode and a
    /// link, as stored ([`raw_mode_and_links`]).
    pub fn still_allocated(fs: &Ext4FileSystem, inode: &Arc<dyn VfsInode>) -> Result<bool, &'static str> {
        let (mode, links) = raw_mode_and_links(fs, u32::try_from(inode.ino()).map_err(|_| "ino")?)?;
        Ok(mode != 0 && links > 0)
    }

    /// Inode `ino`'s creation time as `ext4plus` reads it, `None` if the
    /// inode is too small for `i_crtime`.
    pub fn crtime(fs: &Ext4FileSystem, ino: u64) -> Result<Option<lumen_vfs::Timespec>, &'static str> {
        let ino = u32::try_from(ino).ok().and_then(core::num::NonZeroU32::new).ok_or("inode number")?;
        let inner = fs.shared.state.lock();
        let disk = super::Inode::read(&inner.ext4, ino).map_err(|_| "inode read")?;
        Ok(disk.crtime().map(lumen_vfs::ext4::time_from_disk))
    }

    /// The ext4 inode behind `inode`.
    pub fn ext4_inode(inode: &Arc<dyn VfsInode>) -> Result<&super::Ext4Inode, &'static str> {
        inode.as_any().and_then(|a| a.downcast_ref::<super::Ext4Inode>()).ok_or("not an ext4 inode")
    }

    pub fn read_all(inode: &Arc<dyn VfsInode>) -> Result<Vec<u8>, &'static str> {
        crate::vfs::read_all_inode(inode).map_err(|_| "read")
    }

    pub fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) | 1).collect()
    }

    use crate::vfs::{write_u16le as w16, write_u32le as w32};

    /// [`build_1k_blockmap_image`]'s block size and inode size; its inode
    /// table starts at block 5.
    pub const BS: usize = 1024;
    pub const ISZ: usize = 256;
    /// Where an on-disk inode's `i_block` and `i_extra_isize` are.
    pub const I_BLOCK: usize = super::I_BLOCK_OFFSET as usize;
    pub const I_EXTRA_ISIZE: usize = 0x80;

    /// Where inode `ino` starts in a [`build_1k_blockmap_image`].
    pub fn inode_off(ino: u64) -> usize {
        5 * BS + (ino as usize - 1) * ISZ
    }

    /// Set the clock `fs` stamps times with (`None`: the wall clock).
    pub fn set_clock(fs: &Ext4FileSystem, now: Option<lumen_vfs::Timespec>) {
        *fs.shared.clock.lock() = now;
    }

    /// An `e2fsck`-clean ext4 image with 1 KiB blocks and no `extents`
    /// feature, so files are block-mapped and `ext4plus` writes at most one
    /// 1 KiB block per `write_at` call. 32 inodes; the root directory in
    /// block 13.
    pub fn build_1k_blockmap_image(total: usize) -> Vec<u8> {
        const BS: usize = 1024;
        const IPG: u32 = 32;
        const ISZ: usize = 256;
        const RES: u32 = 10;
        let mut img = vec![0u8; total * BS];
        let used: u32 = 13; // blocks 1..=13: superblock, BGDT, bitmaps, inode table, root dir
        let free = total as u32 - 1 - used;
        let sb = &mut img[1024..2048];
        w32(sb, 0x00, IPG);
        w32(sb, 0x04, total as u32);
        w32(sb, 0x0c, free);
        w32(sb, 0x10, IPG - RES);
        w32(sb, 0x14, 1); // first data block
        w32(sb, 0x18, 0); // 1 KiB blocks
        w32(sb, 0x1c, 0);
        w32(sb, 0x20, 8192);
        w32(sb, 0x24, 8192);
        w32(sb, 0x28, IPG);
        w16(sb, 0x36, u16::MAX);
        w16(sb, 0x38, 0xef53);
        w16(sb, 0x3a, 1);
        w16(sb, 0x3c, 1);
        w32(sb, 0x4c, 1);
        w32(sb, 0x54, RES + 1);
        w16(sb, 0x58, ISZ as u16);
        w32(sb, 0x60, 0x2); // filetype only: no extents
        w32(sb, 0x64, 0x3 | 0x40); // sparse_super, large_file, extra_isize
        sb[0x68..0x78].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
        w16(sb, 0x15c, 32);
        w16(sb, 0x15e, 32);
        let bgd = &mut img[2 * BS..2 * BS + 32];
        w32(bgd, 0x00, 3);
        w32(bgd, 0x04, 4);
        w32(bgd, 0x08, 5);
        w16(bgd, 0x0c, free as u16);
        w16(bgd, 0x0e, (IPG - RES) as u16);
        w16(bgd, 0x10, 1);
        let bmap = &mut img[3 * BS..4 * BS];
        bmap[0] = 0xff;
        bmap[1] = 0x1f;
        for bit in (total - 1)..BS * 8 {
            bmap[bit / 8] |= 1 << (bit % 8);
        }
        let imap = &mut img[4 * BS..5 * BS];
        imap[0] = 0xff;
        imap[1] = 0x03;
        imap[(IPG as usize) / 8..].fill(0xff);
        let off = 5 * BS + ISZ; // inode 2, the root
        let r = &mut img[off..off + ISZ];
        w16(r, 0x00, 0o040755);
        w32(r, 0x04, BS as u32);
        w16(r, 0x1a, 2);
        w32(r, 0x1c, (BS / 512) as u32);
        w32(r, 0x28, 13);
        w16(r, 0x80, 32);
        let d = &mut img[13 * BS..14 * BS];
        w32(d, 0, 2);
        w16(d, 4, 12);
        d[6] = 1;
        d[7] = 2;
        d[8] = b'.';
        w32(d, 12, 2);
        w16(d, 16, (BS - 12) as u16);
        d[18] = 2;
        d[19] = 2;
        d[20..22].copy_from_slice(b"..");
        img
    }

    /// `build_minimal_ext4_image_blocks` (4 KiB blocks, extents): its root
    /// directory's block, for read faults.
    pub const ROOT_DIR_BLOCK_4K: u64 = 6;
}

crate::ktest!(
    fn ext4_writeback_completes_short_block_writes() {
        // 1 KiB block-mapped files: `ext4plus` writes one block per call, so a
        // page needs four. Every byte of a two-page file and of a long
        // symlink target must reach the disk.
        use self::ktest_util::*;
        use crate::kassert_eq;
        let m = mount(build_1k_blockmap_image(512), 200)?;
        let f = m.root.create(b"pages.bin", 0o644).map_err(|_| "create")?;
        let data = pattern(2 * PAGE_SIZE + 300, 7);
        kassert_eq!(f.write_at(0, &data).map_err(|_| "write")?, data.len());
        f.sync(false).map_err(|_| "sync")?;
        let target: alloc::string::String = core::iter::repeat_n('t', 2000).collect();
        m.root.symlink(b"long", target.as_bytes()).map_err(|_| "symlink")?;
        let ro = remount_ro(&m, 201)?;
        let got = read_all(&ro.root_inode().lookup(b"pages.bin").map_err(|_| "ro lookup")?)?;
        kassert_eq!(got, data);
        let link = ro.root_inode().lookup(b"long").map_err(|_| "ro lookup long")?;
        kassert_eq!(link.readlink().map_err(|_| "readlink")?, target.into_bytes());
        Ok(())
    }
);

crate::ktest!(
    fn ext4_rename_over_frees_the_target_when_its_temporary_name_is_taken() {
        // The replaced target is freed through a temporary `.lumen-rename-<ino>`
        // entry. A user file already holding that name, or the rename landing
        // on it, must not stop the free (the inode would leak until e2fsck).
        use self::ktest_util::*;
        use crate::kassert_eq;
        let m = mount(build_1k_blockmap_image(512), 205)?;
        let mut kept = alloc::vec::Vec::new();

        // A bystander file takes the target's first temporary name.
        let target = m.root.create(b"t", 0o644).map_err(|_| "create t")?;
        target.write_at(0, b"old").map_err(|_| "write t")?;
        let name = alloc::format!(".lumen-rename-{}", target.ino());
        let bystander = m.root.create(name.as_bytes(), 0o644).map_err(|_| "create bystander")?;
        bystander.write_at(0, b"mine").map_err(|_| "write bystander")?;
        kept.push((name, b"mine".to_vec()));
        let s = m.root.create(b"s", 0o644).map_err(|_| "create s")?;
        s.write_at(0, b"new").map_err(|_| "write s")?;
        // Outside the VFS's inode cache a dropped inode's dirty pages go
        // with it, so each file is written back before its handle drops.
        for f in [&target, &bystander, &s] {
            f.sync(false).map_err(|_| "sync file")?;
        }
        drop((target, bystander, s));
        let free = free_inodes(&m.fs);
        m.root.rename(b"s", &m.root, b"t", RenameFlags(0)).map_err(|_| "rename s over t")?;
        kassert_eq!(free_inodes(&m.fs), free + 1);
        kept.push(("t".into(), b"new".to_vec()));

        // The rename's own destination is the target's temporary name. In
        // a directory of its own: the freed target's number is reused, and
        // its temporary name would be the bystander's above.
        let d = m.root.mkdir(b"d", 0o755).map_err(|_| "mkdir d")?;
        let a = d.create(b"a", 0o644).map_err(|_| "create a")?;
        let own = alloc::format!(".lumen-rename-{}", a.ino());
        drop(a);
        d.rename(b"a", &d, own.as_bytes(), RenameFlags(0)).map_err(|_| "rename a")?;
        let b = d.create(b"b", 0o644).map_err(|_| "create b")?;
        b.write_at(0, b"bee").map_err(|_| "write b")?;
        b.sync(false).map_err(|_| "sync b")?;
        drop(b);
        let free = free_inodes(&m.fs);
        d.rename(b"b", &d, own.as_bytes(), RenameFlags(0)).map_err(|_| "rename b over own")?;
        kassert_eq!(free_inodes(&m.fs), free + 1);
        drop(d);

        m.fs.sync().map_err(|_| "sync")?;
        let ro = remount_ro(&m, 206)?;
        kassert_eq!(free_inodes(&ro), free_inodes(&m.fs));
        for (name, data) in kept {
            let got = read_all(&ro.root_inode().lookup(name.as_bytes()).map_err(|_| "ro lookup")?)?;
            kassert_eq!(got, data);
        }
        let ro_d = ro.root_inode().lookup(b"d").map_err(|_| "ro lookup d")?;
        kassert_eq!(read_all(&ro_d.lookup(own.as_bytes()).map_err(|_| "ro lookup own")?)?, b"bee".to_vec());
        Ok(())
    }
);

crate::ktest!(
    fn ext4_st_blocks_counts_written_data_not_holes() {
        // `st_blocks` is allocated storage. Extending a file leaves a hole,
        // which counts nothing before writeback or after; written but not
        // yet written-back pages count as the blocks they will take.
        use self::ktest_util::*;
        use crate::{kassert, kassert_eq};
        let m = mount(crate::vfs::build_minimal_ext4_image(), 207)?;
        let f = m.root.create(b"sparse", 0o644).map_err(|_| "create")?;
        let blocks = |f: &Arc<dyn VfsInode>| f.stat().map(|s| s.blocks).map_err(|_| "stat");
        f.truncate(1 << 30).map_err(|_| "truncate")?;
        kassert_eq!(blocks(&f)?, 0);
        f.write_at(0, b"x").map_err(|_| "write")?;
        kassert_eq!(blocks(&f)?, (PAGE_SIZE / 512) as u64);
        f.sync(false).map_err(|_| "sync")?;
        let after = blocks(&f)?;
        kassert!(after > 0 && after <= 64);
        let ro = remount_ro(&m, 208)?;
        let ro_blocks = ro.root_inode().lookup(b"sparse").map_err(|_| "ro")?.stat().map_err(|_| "ro stat")?.blocks;
        kassert_eq!(after, ro_blocks);
        Ok(())
    }
);

crate::ktest!(
    fn ext4_unlinking_a_device_node_frees_no_blocks() {
        // A device node keeps its device number in `i_block` (Linux and
        // mke2fs -d make them; the adapter can't). Here its "device number"
        // is the block holding another file's data: unlinking the node must
        // free its inode and nothing else.
        use self::ktest_util::*;
        use crate::{kassert, kassert_eq};
        let m = mount(build_1k_blockmap_image(512), 202)?;
        let keep = m.root.create(b"keep.bin", 0o644).map_err(|_| "create keep")?;
        let data = pattern(BS, 3);
        kassert_eq!(keep.write_at(0, &data).map_err(|_| "write keep")?, data.len());
        keep.sync(false).map_err(|_| "sync keep")?;
        let dev = m.root.create(b"null", 0o644).map_err(|_| "create node")?;
        let (keep_ino, dev_ino) = (keep.ino(), dev.ino());
        drop((keep, dev));
        m.fs.sync().map_err(|_| "fs sync")?;
        let mut img = image(&m)?;
        drop(m);
        let keep_block = u32::from_le_bytes(img[inode_off(keep_ino) + I_BLOCK..][..4].try_into().unwrap());
        kassert!(keep_block != 0);
        let d = &mut img[inode_off(dev_ino)..][..ISZ];
        d[0..2].copy_from_slice(&0o020666u16.to_le_bytes());
        d[I_BLOCK..I_BLOCK + 4].copy_from_slice(&keep_block.to_le_bytes());

        let m = mount(img, 203)?;
        let (inodes, blocks) = (free_inodes(&m.fs), free_blocks(&m.fs));
        m.root.unlink(b"null").map_err(|_| "unlink node")?;
        kassert_eq!(free_inodes(&m.fs), inodes + 1);
        kassert_eq!(free_blocks(&m.fs), blocks);
        let ro = remount_ro(&m, 204)?;
        kassert_eq!(free_blocks(&ro), blocks);
        kassert_eq!(read_all(&ro.root_inode().lookup(b"keep.bin").map_err(|_| "ro lookup")?)?, data);
        Ok(())
    }
);

crate::ktest!(
    fn ext4_device_nodes_report_their_device_numbers() {
        // `mke2fs -d` and Linux keep a device's number in `i_block`: the old
        // 16-bit encoding in `i_block[0]`, or (when that is 0) the new 32-bit
        // one in `i_block[1]` (#292). The adapter can't make device nodes, so
        // regular files are turned into them on the image. A file with data
        // and a directory have block numbers there, and no device number.
        use self::ktest_util::*;
        use crate::{kassert, kassert_eq};
        let m = mount(build_1k_blockmap_image(128), 205)?;
        let mut inos = Vec::new();
        for name in ["null", "wide", "sda1", "plain"] {
            inos.push(m.root.create(name.as_bytes(), 0o644).map_err(|_| "create")?.ino());
        }
        let plain = m.root.lookup(b"plain").map_err(|_| "lookup plain")?;
        plain.write_at(0, b"x").map_err(|_| "write plain")?;
        plain.sync(false).map_err(|_| "sync plain")?;
        drop(plain);
        m.fs.sync().map_err(|_| "fs sync")?;
        let mut img = image(&m)?;
        drop(m);
        kassert!(img[inode_off(inos[3]) + I_BLOCK..][..4] != [0; 4]);
        // `new_encode_dev(259, 300)`: a minor past 255 and a major past 255.
        let wide: u32 = (300 & 0xff) | (259 << 8) | ((300 & !0xff) << 12);
        for (ino, mode, i_block0, i_block1) in
            [(inos[0], 0o020666u16, 0x0103u32, 0u32), (inos[1], 0o020600, 0, wide), (inos[2], 0o060660, 0x0801, 0)]
        {
            let d = &mut img[inode_off(ino)..][..ISZ];
            d[0..2].copy_from_slice(&mode.to_le_bytes());
            d[I_BLOCK..I_BLOCK + 4].copy_from_slice(&i_block0.to_le_bytes());
            d[I_BLOCK + 4..I_BLOCK + 8].copy_from_slice(&i_block1.to_le_bytes());
        }

        let expect = [
            ("null", InodeType::CharDevice, DevId::makedev(1, 3)),
            ("wide", InodeType::CharDevice, DevId::makedev(259, 300)),
            ("sda1", InodeType::BlockDevice, DevId::makedev(8, 1)),
            ("plain", InodeType::File, DevId::default()),
        ];
        let m = mount(img.clone(), 206)?;
        kassert_eq!(m.root.stat().map_err(|_| "root stat")?.rdev, DevId::default());
        for (name, ty, rdev) in expect {
            let st = m.root.lookup(name.as_bytes()).map_err(|_| "lookup")?.stat().map_err(|_| "stat")?;
            kassert_eq!(st.inode_type, ty);
            kassert_eq!(st.rdev, rdev);
        }
        let ro = mount_ro_image(img, 207)?;
        kassert_eq!(ro.root_inode().stat().map_err(|_| "ro root stat")?.rdev, DevId::default());
        for (name, _, rdev) in expect {
            let st = ro.root_inode().lookup(name.as_bytes()).map_err(|_| "ro lookup")?.stat().map_err(|_| "ro stat")?;
            kassert_eq!(st.rdev, rdev);
        }
        // Unlinking the last link zeroes `i_block` on disk and frees the
        // inode; a node still held keeps the number it was loaded with.
        let held = m.root.lookup(b"null").map_err(|_| "lookup null")?;
        m.root.unlink(b"null").map_err(|_| "unlink null")?;
        let st = held.stat().map_err(|_| "stat held")?;
        kassert_eq!(st.nlink, 0);
        kassert_eq!(st.rdev, DevId::makedev(1, 3));
        kassert!(m.root.lookup(b"null").is_err());
        Ok(())
    }
);

crate::ktest!(
    fn ext4_times_before_1970_and_past_2038_survive_a_remount() {
        // `set_times` (`utimensat`) and the metadata flush both store a time
        // as Linux does (a signed 32-bit second, two epoch bits, nanoseconds),
        // and memory keeps what the disk holds (#294).
        use self::ktest_util::*;
        use crate::kassert_eq;
        use lumen_vfs::ext4::{TIME_MAX_EXTRA, TIME_MIN};
        let ts = |sec: i64, nsec: i64| Timespec { sec, nsec };
        let times = |i: &Arc<dyn VfsInode>| i.stat().map(|st| (st.atime, st.mtime)).map_err(|_| "stat");
        let m = mount(build_1k_blockmap_image(128), 208)?;
        // A file per case: (atime, mtime) asked for, and what an inode holds
        // of it if not that.
        let cases = [
            ("a", (ts(-946_771_200, 123_456_789), ts(-1, 999_999_999)), None),
            ("b", (ts(0, 1), ts(-1, 0)), None),
            ("c", (ts(0x2_0000_0005, 42), ts(0x0_8000_0000, 7)), None),
            ("d", (ts(-(1 << 40), 5), ts(1 << 40, 5)), Some((ts(TIME_MIN, 0), ts(TIME_MAX_EXTRA, 0)))),
            ("e", (ts(TIME_MIN, 9), ts(TIME_MAX_EXTRA, 9)), None),
        ];
        for (name, (atime, mtime), held) in cases {
            let f = m.root.create(name.as_bytes(), 0o644).map_err(|_| "create")?;
            f.set_times(SetTime::Set(atime), SetTime::Set(mtime)).map_err(|_| "set_times")?;
            kassert_eq!(times(&f)?, held.unwrap_or((atime, mtime)));
        }
        let ro = remount_ro(&m, 209)?;
        for (name, (atime, mtime), held) in cases {
            let back = ro.root_inode().lookup(name.as_bytes()).map_err(|_| "ro lookup")?;
            kassert_eq!(times(&back)?, held.unwrap_or((atime, mtime)));
        }

        // The metadata flush rewrites every time from memory: after a write,
        // the write's mtime and ctime and the atime set before it; after a
        // link (ctime alone), the atime and mtime set before it.
        let f = m.root.lookup(b"a").map_err(|_| "lookup")?;
        let atime = ts(-2_000_000_000, 500);
        f.set_times(SetTime::Set(atime), SetTime::Omit).map_err(|_| "set atime")?;
        f.write_at(0, b"x").map_err(|_| "write")?;
        f.sync(false).map_err(|_| "sync")?;
        let g = m.root.lookup(b"c").map_err(|_| "lookup c")?;
        let (g_atime, g_mtime) = (ts(0x2_8000_0001, 3), ts(-1_234_567_890, 999_999_999));
        g.set_times(SetTime::Set(g_atime), SetTime::Set(g_mtime)).map_err(|_| "set c")?;
        m.root.link(b"c2", &g).map_err(|_| "link")?;
        m.fs.sync().map_err(|_| "fs sync")?;
        let ro = remount_ro(&m, 210)?;
        for (name, file) in [("a", &f), ("c", &g)] {
            let before = file.stat().map_err(|_| "stat")?;
            let after =
                ro.root_inode().lookup(name.as_bytes()).map_err(|_| "ro lookup")?.stat().map_err(|_| "ro stat")?;
            kassert_eq!((after.atime, after.mtime, after.ctime), (before.atime, before.mtime, before.ctime));
        }
        kassert_eq!(f.stat().map_err(|_| "stat a")?.atime, atime);
        kassert_eq!(times(&g)?, (g_atime, g_mtime));
        Ok(())
    }
);

crate::ktest!(
    fn ext4_times_follow_the_extra_fields_an_inode_has() {
        // `i_extra_isize` decides which times have their extra field, as
        // `ext4plus` reads them (`i_ctime_extra` from 8, `i_mtime_extra` from
        // 12, `i_atime_extra` from 16; 0 is none, where Linux would take it as
        // 32). A time without one holds whole signed 32-bit seconds (1901 to
        // 2038): memory keeps that too, not nanoseconds or an epoch the disk
        // can't hold (#294).
        use self::ktest_util::*;
        use crate::kassert_eq;
        use lumen_vfs::ext4::{TIME_MIN, truncate_time};
        let ts = |sec: i64, nsec: i64| Timespec { sec, nsec };
        // Each `i_extra_isize`, twice: a file per case.
        let cases = [(ts(-5, 7), ts(0x2_0000_0005, 42)), (ts(TIME_MIN - 1, 0), ts(1_700_000_000, 999))];
        let sizes = [0u16, 8, 12, 16];
        let name = |extra_isize: u16, k: usize| alloc::format!("x{extra_isize}-{k}");
        let m = mount(build_1k_blockmap_image(128), 211)?;
        let mut inos = Vec::new();
        for extra_isize in sizes {
            for k in 0..cases.len() {
                inos.push((
                    extra_isize,
                    m.root.create(name(extra_isize, k).as_bytes(), 0o644).map_err(|_| "create")?.ino(),
                ));
            }
        }
        m.fs.sync().map_err(|_| "fs sync")?;
        let mut img = image(&m)?;
        drop(m);
        for (extra_isize, ino) in inos {
            img[inode_off(ino) + I_EXTRA_ISIZE..][..2].copy_from_slice(&extra_isize.to_le_bytes());
        }

        let m = mount(img, 212)?;
        // A clock with nanoseconds, for the times the adapter stamps.
        set_clock(&m.fs, Some(ts(1_800_000_000, 123_456_789)));
        let mut set = Vec::new();
        for extra_isize in sizes {
            let (ex_c, ex_m, ex_a) = (extra_isize >= 8, extra_isize >= 12, extra_isize >= 16);
            for (k, (atime, mtime)) in cases.into_iter().enumerate() {
                let f = m.root.lookup(name(extra_isize, k).as_bytes()).map_err(|_| "lookup")?;
                f.set_times(SetTime::Set(atime), SetTime::Set(mtime)).map_err(|_| "set_times")?;
                let st = f.stat().map_err(|_| "stat")?;
                kassert_eq!((st.atime, st.mtime), (truncate_time(atime, ex_a), truncate_time(mtime, ex_m)));
                kassert_eq!(st.ctime.nsec != 0, ex_c);
                set.push((name(extra_isize, k), f));
            }
        }
        let ro = remount_ro(&m, 213)?;
        for (name, inode) in &set {
            let st = inode.stat().map_err(|_| "stat")?;
            let back =
                ro.root_inode().lookup(name.as_bytes()).map_err(|_| "ro lookup")?.stat().map_err(|_| "ro stat")?;
            kassert_eq!((back.atime, back.mtime, back.ctime), (st.atime, st.mtime, st.ctime));
        }

        // A write sets mtime and ctime in memory only: as the inode will hold
        // them, so the flush changes nothing `stat` shows.
        for extra_isize in sizes {
            let f = m.root.lookup(name(extra_isize, 0).as_bytes()).map_err(|_| "lookup")?;
            f.write_at(0, b"x").map_err(|_| "write")?;
            let written = f.stat().map_err(|_| "stat written")?;
            kassert_eq!(written.mtime.nsec != 0, extra_isize >= 12);
            kassert_eq!(written.ctime.nsec != 0, extra_isize >= 8);
            f.sync(false).map_err(|_| "sync")?;
            let synced = f.stat().map_err(|_| "stat synced")?;
            kassert_eq!((synced.atime, synced.mtime, synced.ctime), (written.atime, written.mtime, written.ctime));
        }
        Ok(())
    }
);

crate::ktest!(
    fn ext4_each_operation_that_sets_a_time_puts_it_on_disk() {
        // Every other operation that sets a time, each on its own inode at its
        // own clock reading, on inodes without the extra time fields
        // (`i_extra_isize` 0, as `ext4plus` reads it). Memory holds the time in
        // whole seconds, and a remount shows what memory held. chmod and chown
        // write the inode at once; truncate, link and the directory
        // operations go through the metadata flush (#294).
        use self::ktest_util::*;
        use crate::kassert_eq;
        let files = ["chmod", "chown", "truncate", "link"];
        let dirs = ["create", "rename", "unlink", "mkdir", "rmdir", "symlink"];
        let m = mount(build_1k_blockmap_image(128), 216)?;
        let mut inos = Vec::new();
        for name in files {
            inos.push(m.root.create(name.as_bytes(), 0o644).map_err(|_| "create")?.ino());
        }
        for name in dirs {
            inos.push(m.root.mkdir(name.as_bytes(), 0o755).map_err(|_| "mkdir")?.ino());
        }
        // What the rename, unlink and rmdir steps take away.
        m.root.lookup(b"rename").and_then(|d| d.create(b"c", 0o644)).map_err(|_| "create rename/c")?;
        m.root.lookup(b"unlink").and_then(|d| d.create(b"c", 0o644)).map_err(|_| "create unlink/c")?;
        m.root.lookup(b"rmdir").and_then(|d| d.mkdir(b"sub", 0o755)).map_err(|_| "mkdir rmdir/sub")?;
        m.fs.sync().map_err(|_| "fs sync")?;
        let mut img = image(&m)?;
        drop(m);
        for ino in inos {
            img[inode_off(ino) + I_EXTRA_ISIZE..][..2].copy_from_slice(&0u16.to_le_bytes());
        }

        // Each step on the inode it is named after, and whether it changes
        // that inode's mtime as well as its ctime.
        type Step<'a> = &'a dyn Fn(&Arc<dyn VfsInode>) -> Result<(), VfsError>;
        let m = mount(img, 217)?;
        let steps: [(&str, bool, Step<'_>); 10] = [
            ("chmod", false, &|i| i.set_mode(0o600)),
            ("chown", false, &|i| i.set_owner(Some(1), Some(1))),
            ("truncate", true, &|i| i.truncate(3)),
            ("link", false, &|i| m.root.link(b"linked", i)),
            ("create", true, &|d| d.create(b"c", 0o644).map(drop)),
            ("rename", true, &|d| d.rename(b"c", d, b"c2", RenameFlags(0))),
            ("unlink", true, &|d| d.unlink(b"c")),
            ("mkdir", true, &|d| d.mkdir(b"sub", 0o755).map(drop)),
            ("rmdir", true, &|d| d.rmdir(b"sub")),
            ("symlink", true, &|d| d.symlink(b"s", b"c").map(drop)),
        ];
        let mut done = Vec::new();
        let mut wrong: Vec<&str> = Vec::new();
        for (k, (name, sets_mtime, step)) in steps.into_iter().enumerate() {
            let now = Timespec { sec: 1_800_000_000 + 10 * k as i64, nsec: 123_456_789 };
            set_clock(&m.fs, Some(now));
            let inode = m.root.lookup(name.as_bytes()).map_err(|_| "lookup")?;
            step(&inode).map_err(|_| "step")?;
            let st = inode.stat().map_err(|_| "stat")?;
            let held = Timespec { sec: now.sec, nsec: 0 };
            if st.ctime != held || (sets_mtime && st.mtime != held) {
                wrong.push(name);
            }
            done.push((name, inode));
        }
        kassert_eq!(wrong, Vec::<&str>::new());

        m.fs.sync().map_err(|_| "fs sync")?;
        let ro = remount_ro(&m, 218)?;
        let mut lost: Vec<&str> = Vec::new();
        for (name, inode) in &done {
            let st = inode.stat().map_err(|_| "stat")?;
            let back =
                ro.root_inode().lookup(name.as_bytes()).map_err(|_| "ro lookup")?.stat().map_err(|_| "ro stat")?;
            if (back.atime, back.mtime, back.ctime) != (st.atime, st.mtime, st.ctime) {
                lost.push(name);
            }
        }
        kassert_eq!(lost, Vec::<&str>::new());
        Ok(())
    }
);

crate::ktest!(
    fn ext4_namespace_operations_set_their_childrens_ctime() {
        // The test above checks the parents; this one the children: a moved
        // or renamed file or directory (renamed in its parent, or moved to
        // another), both sides of an exchange between parents, an inode
        // that keeps a link after an unlink, and a rename's target that keeps
        // one. Each operation runs at its own clock reading, and each child
        // holds that reading as its ctime, in memory and after a remount.
        // The parents' counts on disk then match memory (the directory
        // renamed in its parent puts the parent's count back).
        use self::ktest_util::*;
        use crate::kassert_eq;
        let at = |k: i64| Timespec { sec: 1_800_000_000 + 10 * k, nsec: 0 };
        let m = mount(build_1k_blockmap_image(128), 280)?;
        set_clock(&m.fs, Some(at(0)));
        let d = m.root.mkdir(b"d", 0o755).map_err(|_| "mkdir d")?;
        let u = m.root.create(b"u", 0o644).map_err(|_| "create u")?;
        m.root.link(b"u2", &u).map_err(|_| "link u2")?;
        let mv = m.root.create(b"mv", 0o644).map_err(|_| "create mv")?;
        let rn = m.root.create(b"rn", 0o644).map_err(|_| "create rn")?;
        let md = m.root.mkdir(b"md", 0o755).map_err(|_| "mkdir md")?;
        let rd = m.root.mkdir(b"rd", 0o755).map_err(|_| "mkdir rd")?;
        let x = m.root.create(b"x", 0o644).map_err(|_| "create x")?;
        let y = d.mkdir(b"y", 0o755).map_err(|_| "mkdir d/y")?;
        let t = m.root.create(b"t", 0o644).map_err(|_| "create t")?;
        m.root.link(b"t2", &t).map_err(|_| "link t2")?;
        let o = m.root.create(b"o", 0o644).map_err(|_| "create o")?;

        // Each step, and the children it stamps with the paths they end at.
        type Step<'a> = &'a dyn Fn() -> Result<(), VfsError>;
        type Children<'a> = &'a [(&'a Arc<dyn VfsInode>, &'a [&'a [u8]])];
        let steps: [(&str, Step<'_>, Children<'_>); 7] = [
            ("unlink", &|| m.root.unlink(b"u2"), &[(&u, &[b"u"])]),
            ("move", &|| m.root.rename(b"mv", &d, b"mv", RenameFlags(0)), &[(&mv, &[b"d", b"mv"])]),
            ("rename", &|| m.root.rename(b"rn", &m.root, b"rn2", RenameFlags(0)), &[(&rn, &[b"rn2"])]),
            ("dir move", &|| m.root.rename(b"md", &d, b"md", RenameFlags(0)), &[(&md, &[b"d", b"md"])]),
            (
                "exchange",
                &|| m.root.rename(b"x", &d, b"y", RenameFlags(RenameFlags::EXCHANGE)),
                &[(&x, &[b"d", b"y"]), (&y, &[b"x"])],
            ),
            ("over", &|| m.root.rename(b"o", &m.root, b"t", RenameFlags(0)), &[(&o, &[b"t"]), (&t, &[b"t2"])]),
            // Last: no later step resyncs the root's count from the disk.
            ("dir rename", &|| m.root.rename(b"rd", &m.root, b"rd2", RenameFlags(0)), &[(&rd, &[b"rd2"])]),
        ];
        let mut wrong: Vec<&str> = Vec::new();
        for (k, (name, step, children)) in steps.iter().enumerate() {
            let now = at(k as i64 + 1);
            set_clock(&m.fs, Some(now));
            step().map_err(|_| "step")?;
            for (child, _) in children.iter() {
                if child.stat().map_err(|_| "stat")?.ctime != now {
                    wrong.push(name);
                }
            }
        }
        kassert_eq!(wrong, Vec::<&str>::new());

        m.fs.sync().map_err(|_| "fs sync")?;
        let ro = remount_ro(&m, 281)?;
        let mut lost: Vec<&str> = Vec::new();
        for (k, (name, _, children)) in steps.iter().enumerate() {
            for (child, path) in children.iter() {
                let mut node = ro.root_inode();
                for part in path.iter() {
                    node = node.lookup(part).map_err(|_| "ro lookup")?;
                }
                kassert_eq!(node.ino(), child.ino());
                if node.stat().map_err(|_| "ro stat")?.ctime != at(k as i64 + 1) {
                    lost.push(name);
                }
            }
        }
        kassert_eq!(lost, Vec::<&str>::new());
        for (dir, path) in [(&m.root, &[][..]), (&d, &[&b"d"[..]][..])] {
            let mut node = ro.root_inode();
            for part in path.iter() {
                node = node.lookup(part).map_err(|_| "ro lookup")?;
            }
            kassert_eq!(node.stat().map_err(|_| "ro stat")?.nlink, dir.stat().map_err(|_| "stat")?.nlink);
        }
        Ok(())
    }
);

crate::ktest!(
    fn ext4_files_made_past_2038_keep_their_times() {
        // `ext4plus`'s `create_inode` sets a new inode's atime, mtime and
        // ctime before its `i_extra_isize`, so only their low 32 bits land: a
        // file made in 2040 would read as 1903. The adapter sets them again,
        // epoch and nanoseconds included, before the inode is linked (#294
        // review).
        use self::ktest_util::*;
        use crate::kassert_eq;
        let now = Timespec { sec: 2_208_988_800, nsec: 987_654_321 }; // 2040-01-01
        let m = mount(build_1k_blockmap_image(128), 214)?;
        set_clock(&m.fs, Some(now));
        let long: alloc::string::String = core::iter::repeat_n('t', 100).collect();
        let made = [
            ("f", m.root.create(b"f", 0o644).map_err(|_| "create")?),
            ("d", m.root.mkdir(b"d", 0o755).map_err(|_| "mkdir")?),
            ("s", m.root.symlink(b"s", b"f").map_err(|_| "symlink")?),
            ("l", m.root.symlink(b"l", long.as_bytes()).map_err(|_| "long symlink")?),
        ];
        for (_, inode) in &made {
            let st = inode.stat().map_err(|_| "stat")?;
            kassert_eq!((st.atime, st.mtime, st.ctime), (now, now, now));
        }
        let root = m.root.stat().map_err(|_| "root stat")?;
        kassert_eq!((root.mtime, root.ctime), (now, now));
        m.fs.sync().map_err(|_| "fs sync")?;
        let ro = remount_ro(&m, 215)?;
        for (name, _) in &made {
            let st = ro.root_inode().lookup(name.as_bytes()).map_err(|_| "ro lookup")?.stat().map_err(|_| "ro stat")?;
            kassert_eq!((st.atime, st.mtime, st.ctime), (now, now, now));
        }
        let root = ro.root_inode().stat().map_err(|_| "ro root stat")?;
        kassert_eq!((root.mtime, root.ctime), (now, now));
        Ok(())
    }
);

crate::ktest!(
    fn ext4_new_inodes_get_a_creation_time_that_survives_a_remount() {
        // `ext4plus`'s `create_inode` sets `i_crtime` before `i_extra_isize`,
        // so it wrote nothing and every inode Lumen made read as born in 1970
        // on Linux (#303). `new_inode` sets it, epoch and nanoseconds
        // included, and later writes of the inode keep it.
        use self::ktest_util::*;
        use crate::kassert_eq;
        use lumen_vfs::ext4::time_from_disk;
        let now = Timespec { sec: 2_208_988_800, nsec: 987_654_321 }; // 2040-01-01
        let m = mount(build_1k_blockmap_image(128), 252)?;
        set_clock(&m.fs, Some(now));
        let long: alloc::string::String = core::iter::repeat_n('t', 100).collect();
        let made = [
            m.root.create(b"f", 0o644).map_err(|_| "create")?.ino(),
            m.root.mkdir(b"d", 0o755).map_err(|_| "mkdir")?.ino(),
            m.root.symlink(b"s", b"f").map_err(|_| "symlink")?.ino(),
            m.root.symlink(b"l", long.as_bytes()).map_err(|_| "long symlink")?.ino(),
        ];
        // `i_crtime` and `i_crtime_extra` as stored: the base word signed,
        // the extra word's epoch bits above it and its nanoseconds.
        let raw_crtime = |img: &[u8], ino: u64| {
            let at = |off: usize| u32::from_le_bytes(img[inode_off(ino) + off..][..4].try_into().unwrap());
            let (base, extra) = (at(0x90), at(0x94));
            time_from_disk(core::time::Duration::new((u64::from(extra & 3) << 32) | u64::from(base), extra >> 2))
        };
        m.fs.sync().map_err(|_| "fs sync")?;
        let img = image(&m)?;
        for &ino in &made {
            kassert_eq!(raw_crtime(&img, ino), now);
        }

        // Later writes of the file and the directory, and a flush, leave every
        // inode's crtime as it was.
        set_clock(&m.fs, Some(Timespec { sec: now.sec + 100, nsec: 5 }));
        let f = m.root.lookup(b"f").map_err(|_| "lookup f")?;
        f.write_at(0, b"data").map_err(|_| "write")?;
        f.set_mode(0o600).map_err(|_| "chmod")?;
        m.root.lookup(b"d").and_then(|d| d.create(b"inner", 0o644)).map_err(|_| "create d/inner")?;
        m.root.link(b"f2", &f).map_err(|_| "link")?;
        m.fs.sync().map_err(|_| "fs sync again")?;
        let img = image(&m)?;
        let ro = mount_ro_image(img.clone(), 253)?;
        for &ino in &made {
            kassert_eq!(raw_crtime(&img, ino), now);
            kassert_eq!(crtime(&ro, ino)?, Some(now));
        }
        Ok(())
    }
);

crate::ktest!(
    fn ext4_unwritable_ro_compat_features_force_read_only() {
        use crate::{kassert, kassert_eq};
        // `metadata_csum`: the adapter's raw inode and `..` writes don't
        // update checksums. `readonly`: the volume asks not to be written.
        kassert!(forced_read_only_reason(false, 0x0400).is_some());
        kassert!(forced_read_only_reason(false, 0x1000).is_some());
        // `orphan_present`: the orphan file has entries ext4plus won't process.
        kassert!(forced_read_only_reason(false, 0x1_0000).is_some());
        // The writable profile's bits (sparse_super, large_file, huge_file,
        // dir_nlink, extra_isize) don't.
        kassert!(forced_read_only_reason(false, 0x6b).is_none());
        let mut img = crate::vfs::build_minimal_ext4_image_blocks(64);
        let ro_compat = u32::from_le_bytes(img[1024 + 0x64..1024 + 0x68].try_into().unwrap());
        img[1024 + 0x64..1024 + 0x68].copy_from_slice(&(ro_compat | 0x1000).to_le_bytes());
        let m = self::ktest_util::mount(img, 202)?;
        kassert!(m.fs.read_only());
        kassert_eq!(m.root.create(b"x", 0o644).err(), Some(VfsError::RoFs));
        Ok(())
    }
);

crate::ktest!(
    fn ext4_rename_refuses_a_removed_directory_whose_number_was_reused() {
        use self::ktest_util::*;
        use crate::{kassert, kassert_eq};
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 203)?;
        m.root.create(b"f", 0o644).map_err(|_| "create f")?;
        let d = m.root.mkdir(b"d", 0o755).map_err(|_| "mkdir d")?;
        let d_ino = d.ino();
        m.root.rmdir(b"d").map_err(|_| "rmdir d")?;
        let e = m.root.mkdir(b"e", 0o755).map_err(|_| "mkdir e")?;
        kassert_eq!(e.ino(), d_ino); // the number was reused
        // A rename resolved against the old `/d` must not land in `/e`.
        kassert_eq!(m.root.rename(b"f", &d, b"g", RenameFlags(0)).err(), Some(VfsError::NoEnt));
        kassert!(e.lookup(b"g").is_err());
        kassert!(m.root.lookup(b"f").is_ok());
        Ok(())
    }
);

crate::ktest!(
    fn ext4_create_reports_an_unreadable_directory_instead_of_a_new_name() {
        // A read error looking the name up is not "absent": create must stop
        // with EIO, before allocating anything.
        use self::ktest_util::*;
        use crate::kassert_eq;
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 204)?;
        m.root.create(b"dup", 0o644).map_err(|_| "create dup")?;
        let before = free_inodes(&m.fs);
        let blk = ROOT_DIR_BLOCK_4K * 4096;
        m.cache.fail_reads_in(blk, blk + 4096, 1);
        kassert_eq!(m.root.create(b"dup", 0o644).err(), Some(VfsError::Io));
        m.cache.fail_reads_in(blk, blk + 4096, 1);
        kassert_eq!(m.root.mkdir(b"dup2", 0o755).err(), Some(VfsError::Io));
        m.cache.fail_reads_in(blk, blk + 4096, 1);
        kassert_eq!(m.root.symlink(b"dup3", b"t").err(), Some(VfsError::Io));
        // `link` adds no inode, but must not take the read error for "absent"
        // either (ext4plus would then answer EEXIST, not EIO).
        let target = m.root.lookup(b"dup").map_err(|_| "lookup dup")?;
        m.root.link(b"dup_link", &target).map_err(|_| "link")?;
        m.cache.fail_reads_in(blk, blk + 4096, 1);
        kassert_eq!(m.root.link(b"dup_link", &target).err(), Some(VfsError::Io));
        m.cache.clear();
        kassert_eq!(target.stat().map_err(|_| "stat dup")?.nlink, 2);
        kassert_eq!(free_inodes(&m.fs), before);
        Ok(())
    }
);

crate::ktest!(
    fn ext4_mkdir_at_the_link_limit_allocates_nothing() {
        // A parent at ext4's link limit can't take another subdirectory's
        // `..`: mkdir is refused before allocating the inode and its block
        // (at 65535, `Dir::link` itself would fail after both were taken).
        use self::ktest_util::*;
        use crate::kassert_eq;
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 238)?;
        for links in [65_000u16, u16::MAX] {
            {
                let inner = m.fs.shared.state.lock();
                let root_ino = NonZeroU32::new(2).ok_or("root")?;
                let mut root = Inode::read(&inner.ext4, root_ino).map_err(|_| "read root")?;
                root.set_links_count(links);
                root.write(&inner.ext4).map_err(|_| "write root")?;
            }
            let (inodes, blocks) = (free_inodes(&m.fs), free_blocks(&m.fs));
            kassert_eq!(m.root.mkdir(b"sub", 0o755).err(), Some(VfsError::NoSpc));
            kassert_eq!((free_inodes(&m.fs), free_blocks(&m.fs)), (inodes, blocks));
            kassert_eq!(m.root.lookup(b"sub").err(), Some(VfsError::NoEnt));
        }
        Ok(())
    }
);

crate::ktest!(
    fn ext4_failed_create_frees_its_inode() {
        // Fail each write after the inode is allocated, one at a time: the
        // create fails, no entry appears, and the inode is free again.
        use self::ktest_util::*;
        use crate::{kassert, kassert_eq};
        let img = crate::vfs::build_minimal_ext4_image_blocks(256);
        let long: alloc::string::String = core::iter::repeat_n('L', 300).collect();
        let kinds: [(&str, FileType, InodeMode); 4] = [
            ("file", FileType::Regular, InodeMode::S_IFREG),
            ("dir", FileType::Directory, InodeMode::S_IFDIR),
            ("short-symlink", FileType::Symlink, InodeMode::S_IFLNK),
            ("long-symlink", FileType::Symlink, InodeMode::S_IFLNK),
        ];
        let mut minor = 205;
        for (kind, ft, type_bits) in kinds {
            // Writes `create_inode` alone makes: failing one of those is
            // inside `ext4plus`'s allocator, beyond the adapter.
            let alloc_writes = {
                let m = mount(img.clone(), minor)?;
                m.cache.arm_write(0);
                let inner = m.fs.shared.state.lock();
                inner
                    .ext4
                    .create_inode(ext4plus::inode::InodeCreationOptions {
                        file_type: ft,
                        mode: type_bits | InodeMode::from_bits_retain(0o644),
                        uid: 0,
                        gid: 0,
                        time: core::time::Duration::from_secs(0),
                        flags: InodeFlags::empty(),
                    })
                    .map_err(|_| "create_inode")?;
                m.cache.writes.load(Ordering::Acquire)
            };
            let mut k = alloc_writes + 1;
            let mut failed = 0;
            loop {
                let m = mount(img.clone(), minor)?;
                let before = free_inodes(&m.fs);
                let before_blocks = free_blocks(&m.fs);
                let root_nlink = m.root.stat().map_err(|_| "stat root")?.nlink;
                m.cache.arm_write(k);
                let res = match kind {
                    "file" => m.root.create(b"new", 0o644).map(drop),
                    "dir" => m.root.mkdir(b"new", 0o755).map(drop),
                    "short-symlink" => m.root.symlink(b"new", b"short/target").map(drop),
                    _ => m.root.symlink(b"new", long.as_bytes()).map(drop),
                };
                let reached = m.cache.writes.load(Ordering::Acquire) >= k;
                m.cache.clear();
                if !reached {
                    kassert!(res.is_ok());
                    break; // every write of this create has been failed once
                }
                if res.is_err() {
                    failed += 1;
                    crate::ktest::diag(format_args!("{kind}: write {k} failed"));
                    kassert!(m.root.lookup(b"new").is_err());
                    kassert_eq!(free_inodes(&m.fs), before);
                    let ro = remount_ro(&m, minor + 1)?;
                    kassert_eq!(free_inodes(&ro), before);
                    // A block leaks only when the write fails inside
                    // `ext4plus`, between allocating the new directory's (or
                    // long symlink's) block, three writes after the inode's,
                    // and recording it in the inode: `ext4plus` frees blocks
                    // only through ones an inode holds, out of the adapter's
                    // reach. Every other failure gives the block back too.
                    let inside_block_alloc =
                        matches!(kind, "dir" | "long-symlink") && (alloc_writes + 4..=alloc_writes + 6).contains(&k);
                    kassert_eq!(free_blocks(&ro), before_blocks - u64::from(inside_block_alloc));
                    kassert_eq!(ro.root_inode().stat().map_err(|_| "ro stat root")?.nlink, root_nlink);
                } else {
                    kassert!(m.root.lookup(b"new").is_ok());
                }
                k += 1;
            }
            crate::ktest::diag(format_args!("{kind}: {failed} failed creates checked"));
            kassert!(failed >= 1); // a write after the allocation was reached
            minor += 2;
        }
        Ok(())
    }
);

crate::ktest!(
    fn ext4_create_in_a_full_directory_on_a_full_disk_leaks_nothing() {
        // The directory's block has no room for the new name and the disk no
        // block to grow it: the create must fail with ENOSPC without leaving
        // an allocated inode behind.
        use self::ktest_util::*;
        use crate::{kassert, kassert_eq};
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 215)?;
        let big = m.root.create(b"big", 0o644).map_err(|_| "create big")?;
        let long = |i: u8| -> alloc::string::String {
            let mut s: alloc::string::String = core::iter::repeat_n('n', 254).collect();
            s.push((b'a' + i) as char);
            s
        };
        // 15 entries of 255-byte names fill the root's 4 KiB block (as hard
        // links: the image has only 22 free inodes).
        for i in 0..15 {
            m.root.link(long(i).as_bytes(), &big).map_err(|_| "link")?;
        }
        let chunk = pattern(PAGE_SIZE, 3);
        for p in 0..256u64 {
            big.write_at(p * PAGE_SIZE as u64, &chunk).map_err(|_| "fill write")?;
        }
        kassert!(big.sync(false).is_err()); // out of space part-way
        kassert!(free_blocks(&m.fs) < CREATE_RESERVE_BLOCKS);
        let before = free_inodes(&m.fs);
        kassert_eq!(m.root.create(long(20).as_bytes(), 0o644).err(), Some(VfsError::NoSpc));
        kassert_eq!(m.root.mkdir(long(21).as_bytes(), 0o755).err(), Some(VfsError::NoSpc));
        kassert_eq!(free_inodes(&m.fs), before);
        Ok(())
    }
);

crate::ktest!(
    fn ext4_sync_flushes_the_rest_past_a_failing_inode() {
        use self::ktest_util::*;
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::Ordering as O;
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 216)?;
        let a = m.root.create(b"a", 0o644).map_err(|_| "create a")?;
        let b = m.root.create(b"b", 0o644).map_err(|_| "create b")?;
        kassert!(a.ino() < b.ino()); // `a` is flushed first
        m.fs.sync().map_err(|_| "first sync")?; // nothing else (the root) left to write
        a.write_at(0, &pattern(PAGE_SIZE, 1)).map_err(|_| "write a")?;
        b.write_at(0, &pattern(PAGE_SIZE, 2)).map_err(|_| "write b")?;
        m.cache.dev.0.flushed.store(false, O::Release);
        m.cache.arm_write(1); // `a`'s first data write
        kassert!(m.fs.sync().is_err());
        m.cache.clear();
        let a4 = m.fs.lookup_live_by_lock_id(a.lock_id()).ok_or("live a")?;
        let b4 = m.fs.lookup_live_by_lock_id(b.lock_id()).ok_or("live b")?;
        kassert_eq!(a4.reclaim_clean_pages(16), 0); // still dirty
        kassert_eq!(b4.reclaim_clean_pages(16), 1); // written anyway
        kassert!(m.cache.dev.0.flushed.load(O::Acquire)); // and the device flushed
        Ok(())
    }
);

crate::ktest!(
    fn ext4_write_stops_at_the_largest_file_size() {
        use self::ktest_util::*;
        use crate::kassert_eq;
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 217)?;
        let f = m.root.create(b"edge", 0o644).map_err(|_| "create")?;
        let max = m.fs.shared.max_file_size;
        kassert_eq!(max, ((1u64 << 32) - 1) * 4096);
        kassert_eq!(f.write_at(max, b"x").err(), Some(VfsError::FBig));
        kassert_eq!(f.write_at(max - 1, b"xy").map_err(|_| "write at max-1")?, 1);
        kassert_eq!(f.truncate(max + 1).err(), Some(VfsError::FBig));
        f.truncate(0).map_err(|_| "truncate 0")?;
        kassert_eq!(max_file_size(1024, false), (12 + 256 + 256 * 256 + 256 * 256 * 256) * 1024);
        Ok(())
    }
);

crate::ktest!(
    fn ext4_writes_past_the_hard_page_limit_fail() {
        use self::ktest_util::*;
        use crate::kassert_eq;
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 218)?;
        let f = m.root.create(b"capped", 0o644).map_err(|_| "create")?;
        // This filesystem isn't in `EXT4_FILESYSTEMS`, so reclaim can't
        // free its pages: three more pages reach the limit.
        let cur = page_cache::ext4_cached_pages();
        page_cache::set_ext4_limits_for_test(Some((usize::MAX, cur + 3)));
        let res = f.write_at(0, &pattern(5 * PAGE_SIZE, 4));
        let res2 = f.write_at(5 * PAGE_SIZE as u64, b"more");
        page_cache::set_ext4_limits_for_test(None);
        kassert_eq!(res.map_err(|_| "capped write")?, 3 * PAGE_SIZE);
        kassert_eq!(res2.err(), Some(VfsError::NoSpc));
        Ok(())
    }
);

crate::ktest!(
    fn ext4_open_unlink_of_a_sparse_file_scans_a_bounded_range() {
        use self::ktest_util::*;
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::Ordering as O;
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 219)?;
        // 64 MiB apparent, one byte of data at the start.
        let f = m.root.create(b"sparse", 0o644).map_err(|_| "create")?;
        f.write_at(0, b"S").map_err(|_| "write")?;
        f.truncate(64 << 20).map_err(|_| "extend")?;
        f.sync(false).map_err(|_| "sync")?;
        let f4 = m.fs.lookup_live_by_lock_id(f.lock_id()).ok_or("live")?;
        f4.reclaim_clean_pages(16);
        f.inc_open_count();
        m.cache.reads.store(0, O::Release);
        m.root.unlink(b"sparse").map_err(|_| "unlink")?;
        let reads = m.cache.reads.load(O::Acquire);
        crate::ktest::diag(format_args!("reads during the unlink: {reads}"));
        kassert!(reads < 200); // not one per page of the 16384
        kassert!(f.is_detached());
        let mut b = [0u8; 2];
        kassert_eq!(f.read_at(0, &mut b).map_err(|_| "read")?, 2);
        kassert_eq!(&b, b"S\0");
        // Larger than the scan limit, data only at the start: the stop once
        // the data blocks are covered ends the scan long before the limit.
        let big = m.root.create(b"sparse_big", 0o644).map_err(|_| "create big")?;
        big.write_at(0, b"B").map_err(|_| "write big")?;
        big.truncate((DETACHED_SCAN_LIMIT_PAGES as u64 + 1024) * PAGE_SIZE as u64).map_err(|_| "extend big")?;
        big.sync(false).map_err(|_| "sync big")?;
        m.fs.lookup_live_by_lock_id(big.lock_id()).ok_or("live big")?.reclaim_clean_pages(16);
        big.inc_open_count();
        m.root.unlink(b"sparse_big").map_err(|_| "unlink big")?;
        kassert!(big.is_detached());
        kassert_eq!(big.read_at(0, &mut b).map_err(|_| "read big")?, 2);
        kassert_eq!(&b, b"B\0");
        big.dec_open_count();
        f.dec_open_count();

        // Data past the scan limit: refused rather than scanned for good.
        let g = m.root.create(b"far", 0o644).map_err(|_| "create far")?;
        let far = (DETACHED_SCAN_LIMIT_PAGES as u64 + 16) * PAGE_SIZE as u64;
        g.write_at(far, b"F").map_err(|_| "write far")?;
        g.sync(false).map_err(|_| "sync far")?;
        let g4 = m.fs.lookup_live_by_lock_id(g.lock_id()).ok_or("live far")?;
        g4.reclaim_clean_pages(16);
        g.inc_open_count();
        kassert_eq!(m.root.unlink(b"far").err(), Some(VfsError::NoSpc));
        kassert!(!g.is_detached());
        kassert!(m.root.lookup(b"far").is_ok());
        g.dec_open_count();
        Ok(())
    }
);

crate::ktest!(
    fn ext4_dir_record_reads_each_kind_of_record_and_refuses_malformed_ones() {
        // Over hand-made bytes: a live record, free space (inode 0), the
        // checksum tail, and each malformed record.
        use crate::kassert_eq;
        let rec = |ino: u32, rec_len: u16, name: &[u8]| {
            let mut r = alloc::vec![0u8; usize::from(rec_len).max(8)];
            r[..4].copy_from_slice(&ino.to_le_bytes());
            r[4..6].copy_from_slice(&rec_len.to_le_bytes());
            r[6] = name.len() as u8;
            r[7] = 1;
            let end = (8 + name.len()).min(r.len());
            r[8..end].copy_from_slice(&name[..end - 8]);
            r
        };
        // A block: "ab" (inode 12, 12 bytes), free space (16), then the
        // checksum tail (inode 0, 12 bytes, file type 0xde).
        let mut block = rec(12, 12, b"ab");
        block.extend(rec(0, 16, b""));
        let mut tail = rec(0, 12, b"");
        tail[7] = 0xde;
        block.extend(tail);
        kassert_eq!(dir_record(&block, 0), Ok((12, Some((12, &b"ab"[..])))));
        kassert_eq!(dir_record(&block, 12), Ok((16, None)));
        kassert_eq!(dir_record(&block, 28), Ok((12, None)));
        // Malformed: too short a header, rec_len < 8, past the block, a name
        // past its record.
        kassert_eq!(dir_record(&block, 36), Err(BadRecord::ShortHeader));
        kassert_eq!(dir_record(&block[..4], 0), Err(BadRecord::ShortHeader));
        kassert_eq!(dir_record(&rec(12, 4, b""), 0), Err(BadRecord::RecLenTooSmall));
        kassert_eq!(dir_record(&rec(12, 0, b""), 0), Err(BadRecord::RecLenTooSmall));
        kassert_eq!(dir_record(&rec(12, 16, b"ab")[..12], 0), Err(BadRecord::PastBlock));
        kassert_eq!(dir_record(&rec(12, 12, b"abcdef"), 0), Err(BadRecord::NamePastRecord));
        Ok(())
    }
);

crate::ktest!(
    fn ext4_names_that_are_not_utf8_are_listed_looked_up_renamed_and_removed() {
        // #268: names are bytes. On `ktest_ext4_non_utf8`'s image, the name
        // made not UTF-8 on disk is listed and looked up like its siblings;
        // on a writable volume with one, it is renamed (to another name that
        // isn't UTF-8, which reaches the disk), unlinked, and its directory
        // then removes as empty.
        use self::ktest_util::*;
        use crate::{kassert, kassert_eq};
        let ro = crate::vfs::ktest_ext4_non_utf8(220)?;
        let root = ro.root_inode();
        let mut out = Vec::new();
        root.readdir(0, &mut out, 16).map_err(|_| "readdir")?;
        let listed = out.iter().find(|e| e.name == b"\xff\xfe").ok_or("the name that isn't UTF-8 is listed")?;
        let a0 = root.lookup(b"a0").map_err(|_| "lookup a0")?;
        kassert_eq!(listed.ino, a0.ino());
        kassert_eq!(root.lookup(b"\xff\xfe").map_err(|_| "lookup ff fe")?.ino(), a0.ino());
        kassert_eq!(root.lookup(b"a3").err(), Some(VfsError::NoEnt));

        // A writable volume whose `d/zqxj` is renamed on disk to `ff fe x j`.
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 244)?;
        let d = m.root.mkdir(b"d", 0o755).map_err(|_| "mkdir d")?;
        let f = d.create(b"zqxj", 0o644).map_err(|_| "create")?;
        f.write_at(0, b"odd").map_err(|_| "write")?;
        f.sync(false).map_err(|_| "sync")?;
        drop((f, d));
        let mut img = image(&m)?;
        drop(m);
        let at = img.windows(4).position(|w| w == b"zqxj").ok_or("the entry on disk")?;
        kassert!(img.windows(4).filter(|w| *w == b"zqxj").count() == 1);
        img[at] = 0xff;
        img[at + 1] = 0xfe;
        let m = mount(img, 270)?;
        let d = m.root.lookup(b"d").map_err(|_| "lookup d")?;
        let mut out = Vec::new();
        d.readdir(0, &mut out, 16).map_err(|_| "readdir d")?;
        kassert!(out.iter().map(|e| e.name.as_slice()).eq([&b"."[..], b"..", b"\xff\xfexj"]));
        kassert_eq!(read_all(&d.lookup(b"\xff\xfexj").map_err(|_| "lookup ff fe x j")?)?, b"odd".to_vec());
        // Non-empty: rmdir refuses.
        kassert_eq!(m.root.rmdir(b"d").err(), Some(VfsError::NotEmpty));
        d.rename(b"\xff\xfexj", &d, b"\x80\xc3", RenameFlags(0)).map_err(|_| "rename")?;
        kassert_eq!(d.lookup(b"\xff\xfexj").err(), Some(VfsError::NoEnt));
        let on_disk = remount_ro(&m, 271)?;
        let ro_d = on_disk.root_inode().lookup(b"d").map_err(|_| "ro lookup d")?;
        kassert_eq!(read_all(&ro_d.lookup(b"\x80\xc3").map_err(|_| "ro lookup the new name")?)?, b"odd".to_vec());
        d.unlink(b"\x80\xc3").map_err(|_| "unlink")?;
        drop(d);
        m.root.rmdir(b"d").map_err(|_| "rmdir once empty")?;
        kassert_eq!(m.root.lookup(b"d").err(), Some(VfsError::NoEnt));
        Ok(())
    }
);

crate::ktest!(
    fn ext4_writeback_continues_past_a_failing_page() {
        // The first page's write fails: the other pages are still written
        // (and the sync reports the error), so one bad block doesn't keep
        // the rest of the file off the disk.
        use self::ktest_util::*;
        use crate::kassert_eq;
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 239)?;
        let f = m.root.create(b"pages", 0o644).map_err(|_| "create")?;
        let data = pattern(3 * PAGE_SIZE, 5);
        f.write_at(0, &data).map_err(|_| "write")?;
        m.cache.arm_write(1); // the first page's block allocation, as it happens
        crate::kassert!(f.sync(false).is_err());
        m.cache.clear();
        let ro = remount_ro(&m, 240)?;
        let on_disk = read_all(&ro.root_inode().lookup(b"pages").map_err(|_| "ro lookup")?)?;
        kassert_eq!(on_disk.len(), data.len());
        kassert_eq!(&on_disk[PAGE_SIZE..], &data[PAGE_SIZE..]); // pages 1 and 2 reached the disk
        f.sync(false).map_err(|_| "retry sync")?; // the failed page stayed dirty
        let ro = remount_ro(&m, 241)?;
        kassert_eq!(read_all(&ro.root_inode().lookup(b"pages").map_err(|_| "ro lookup 2")?)?, data);
        Ok(())
    }
);

crate::ktest!(
    fn ext4_corrupt_block_address_is_an_io_error() {
        use crate::kassert_eq;
        for e in [BlockError::OutOfRange, BlockError::Unaligned] {
            kassert_eq!(map_ext4_err(Ext4Error::Io(Box::new(BcacheIoError(e)))), VfsError::Io);
        }
        kassert_eq!(map_ext4_err(Ext4Error::Io(Box::new(BcacheIoError(BlockError::ReadOnly)))), VfsError::RoFs);
        Ok(())
    }
);

crate::ktest!(
    fn ext4_reclaim_dirty_preserves_dirty_on_write_error() {
        use self::ktest_util::*;
        use crate::kassert_eq;
        use core::sync::atomic::Ordering as O;
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 222)?;
        let f = m.root.create(b"r", 0o644).map_err(|_| "create")?;
        let data = pattern(2 * PAGE_SIZE, 9);
        f.write_at(0, &data).map_err(|_| "write")?;
        let f4 = m.fs.lookup_live_by_lock_id(f.lock_id()).ok_or("live")?;
        m.cache.fail_writes.store(true, O::Release);
        kassert_eq!(f4.reclaim_dirty_pages(16), 0);
        kassert_eq!(f4.reclaim_clean_pages(16), 0); // nothing was dropped as clean
        m.cache.clear();
        f.sync(false).map_err(|_| "sync")?;
        let ro = remount_ro(&m, 223)?;
        kassert_eq!(read_all(&ro.root_inode().lookup(b"r").map_err(|_| "ro lookup")?)?, data);
        Ok(())
    }
);

crate::ktest!(
    fn ext4_flush_persists_mtime() {
        use self::ktest_util::*;
        use crate::{kassert, kassert_eq};
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 224)?;
        let f = m.root.create(b"t", 0o644).map_err(|_| "create")?;
        f.sync(false).map_err(|_| "sync 1")?;
        let created = f.stat().map_err(|_| "stat 1")?.mtime;
        crate::time::sleep(2_000_000);
        f.write_at(0, b"later").map_err(|_| "write")?;
        let written = f.stat().map_err(|_| "stat 2")?.mtime;
        kassert!(written != created);
        f.sync(false).map_err(|_| "sync 2")?;
        let ro = remount_ro(&m, 225)?;
        let on_disk = ro.root_inode().lookup(b"t").map_err(|_| "ro lookup")?.stat().map_err(|_| "ro stat")?.mtime;
        kassert_eq!(on_disk, written);
        Ok(())
    }
);

crate::ktest!(
    fn ext4_cross_directory_rename_moves_the_parent_link() {
        use self::ktest_util::*;
        use crate::kassert_eq;
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 226)?;
        let a = m.root.mkdir(b"a", 0o755).map_err(|_| "mkdir a")?;
        let b = m.root.mkdir(b"b", 0o755).map_err(|_| "mkdir b")?;
        a.mkdir(b"sub", 0o755).map_err(|_| "mkdir sub")?;
        kassert_eq!(a.stat().map_err(|_| "stat a")?.nlink, 3);
        a.rename(b"sub", &b, b"sub", RenameFlags(0)).map_err(|_| "rename")?;
        kassert_eq!(a.stat().map_err(|_| "stat a")?.nlink, 2);
        kassert_eq!(b.stat().map_err(|_| "stat b")?.nlink, 3);
        m.fs.sync().map_err(|_| "sync")?;
        let ro = remount_ro(&m, 227)?;
        let ro_root = ro.root_inode();
        kassert_eq!(ro_root.lookup(b"a").map_err(|_| "ro a")?.stat().map_err(|_| "ro stat a")?.nlink, 2);
        let ro_b = ro_root.lookup(b"b").map_err(|_| "ro b")?;
        kassert_eq!(ro_b.stat().map_err(|_| "ro stat b")?.nlink, 3);
        let ro_sub = ro_b.lookup(b"sub").map_err(|_| "ro sub")?;
        kassert_eq!(ro_sub.lookup(b"..").map_err(|_| "ro ..")?.ino(), ro_b.ino());
        Ok(())
    }
);

crate::ktest!(
    fn ext4_failed_rename_over_keeps_the_target() {
        // Fail each write of a rename over an existing target once, for each
        // kind of target, closed and held open. If the source didn't land at
        // the target's name, the rename reported an error and the target is
        // still there, with its data and link counts, and nothing was
        // allocated or freed (as a read-only remount sees it). If it landed,
        // the source is at the name and its old name is gone.
        use self::ktest_util::*;
        use self::{SWEPT_DST as DST, SWEPT_SRC as SRC};
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::Ordering as O;
        const LINK: &[u8] = b"inline/target";
        // The adapter keeps no inode alive by itself (the VFS's inode cache
        // does that): hold each one until the fixture is synced.
        fn file(
            dir: &Arc<dyn VfsInode>,
            name: &str,
            data: &[u8],
            keep: &mut Vec<Arc<dyn VfsInode>>,
        ) -> Result<(), &'static str> {
            let f = dir.create(name.as_bytes(), 0o644).map_err(|_| "create")?;
            kassert_eq!(f.write_at(0, data).map_err(|_| "write")?, data.len());
            keep.push(f);
            Ok(())
        }
        type Parents = (Arc<dyn VfsInode>, Arc<dyn VfsInode>);
        // Where the rename goes: (source parent, target parent), `a` and `b`
        // for the cross-directory kinds, else the root for both.
        fn parents(root: &Arc<dyn VfsInode>, kind: &str) -> Result<Parents, &'static str> {
            if kind.starts_with("cross") {
                Ok((root.lookup(b"a").map_err(|_| "a")?, root.lookup(b"b").map_err(|_| "b")?))
            } else {
                Ok((Arc::clone(root), Arc::clone(root)))
            }
        }
        fn is_target(inode: &Arc<dyn VfsInode>, kind: &str) -> Result<bool, &'static str> {
            Ok(match kind {
                "symlink" => inode.readlink().map_err(|_| "readlink")? == LINK,
                "dir" | "crossdir" => inode.inode_type() == InodeType::Dir && inode.lookup(b"inner").is_err(),
                _ => read_all(inode)? == DST,
            })
        }
        fn is_source(inode: &Arc<dyn VfsInode>, kind: &str) -> Result<bool, &'static str> {
            Ok(match kind {
                "dir" | "crossdir" => read_all(&inode.lookup(b"inner").map_err(|_| "inner")?)? == SRC,
                _ => read_all(inode)? == SRC,
            })
        }
        let quota_before = page_cache::DETACHED_QUOTA.used_pages();
        let mut minor = 240;
        for kind in ["file", "cross", "linked", "symlink", "dir", "crossdir"] {
            let is_dir = kind.ends_with("dir");
            // Whether the target loses its last link, and the blocks it holds.
            let (last_link, target_blocks) = match kind {
                "linked" => (false, 0),
                "symlink" => (true, 0),
                _ => (true, 1),
            };
            let img = {
                let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), minor)?;
                if kind.starts_with("cross") {
                    m.root.mkdir(b"a", 0o755).map_err(|_| "mkdir a")?;
                    m.root.mkdir(b"b", 0o755).map_err(|_| "mkdir b")?;
                }
                let (sp, dp) = parents(&m.root, kind)?;
                let mut keep = Vec::new();
                if is_dir {
                    let src = sp.mkdir(b"src", 0o755).map_err(|_| "mkdir src")?;
                    file(&src, "inner", SRC, &mut keep)?;
                    dp.mkdir(b"dst", 0o755).map_err(|_| "mkdir dst")?;
                } else {
                    file(&sp, "src", SRC, &mut keep)?;
                    if kind == "symlink" {
                        dp.symlink(b"dst", LINK).map_err(|_| "symlink")?;
                    } else {
                        file(&dp, "dst", DST, &mut keep)?;
                    }
                    if kind == "linked" {
                        let dst = dp.lookup(b"dst").map_err(|_| "dst")?;
                        dp.link(b"dst2", &dst).map_err(|_| "link dst2")?;
                    }
                }
                m.fs.sync().map_err(|_| "sync")?;
                drop(keep);
                image(&m)?
            };
            for open in [false, true] {
                let mut k = 1;
                let mut kept = 0;
                loop {
                    let m = mount(img.clone(), minor)?;
                    let (sp, dp) = parents(&m.root, kind)?;
                    let src = sp.lookup(b"src").map_err(|_| "src")?;
                    let dst = dp.lookup(b"dst").map_err(|_| "dst")?;
                    let (src_ino, dst_ino) = (src.ino(), dst.ino());
                    if open {
                        dst.inc_open_count();
                    }
                    let before = (free_inodes(&m.fs), free_blocks(&m.fs));
                    m.cache.arm_write(k);
                    let res = sp.rename(b"src", &dp, b"dst", RenameFlags(0));
                    let reached = m.cache.writes.load(O::Acquire) >= k;
                    m.cache.clear();
                    let ro = remount_ro(&m, minor + 1)?;
                    let (ro_sp, ro_dp) = parents(&ro.root_inode(), kind)?;
                    let at_dst = ro_dp.lookup(b"dst").map_err(|_| "ro dst")?;
                    if at_dst.ino() == dst_ino {
                        // The source didn't land: an error, and nothing changed.
                        kassert!(res.is_err());
                        kept += 1;
                        if !is_target(&at_dst, kind)? {
                            crate::ktest::diag(format_args!(
                                "{kind} open={open} k={k} res={res:?}: target reads {:?}",
                                read_all(&at_dst).map(|v| alloc::string::String::from_utf8_lossy(&v).into_owned())
                            ));
                        }
                        kassert!(is_target(&at_dst, kind)?);
                        let ro_src = ro_sp.lookup(b"src").map_err(|_| "ro src")?;
                        kassert_eq!(ro_src.ino(), src_ino);
                        kassert!(is_source(&ro_src, kind)?);
                        kassert_eq!((free_inodes(&ro), free_blocks(&ro)), before);
                        kassert!(!dst.is_detached());
                        kassert_eq!(dst.stat().map_err(|_| "stat dst")?.nlink, at_dst.stat().map_err(|_| "ro")?.nlink);
                        kassert_eq!(src.stat().map_err(|_| "stat src")?.nlink, ro_src.stat().map_err(|_| "ro")?.nlink);
                        kassert_eq!(sp.stat().map_err(|_| "stat sp")?.nlink, ro_sp.stat().map_err(|_| "ro")?.nlink);
                        kassert_eq!(dp.stat().map_err(|_| "stat dp")?.nlink, ro_dp.stat().map_err(|_| "ro")?.nlink);
                        kassert_eq!(dp.lookup(b"dst").map_err(|_| "live dst")?.ino(), dst_ino);
                    } else {
                        // The source landed (`Ok`, or an error after it did).
                        kassert_eq!(at_dst.ino(), src_ino);
                        kassert!(is_source(&at_dst, kind)?);
                        kassert!(ro_sp.lookup(b"src").is_err());
                        if kind == "linked" {
                            let other = ro_dp.lookup(b"dst2").map_err(|_| "ro dst2")?;
                            kassert_eq!(other.ino(), dst_ino);
                            kassert!(is_target(&other, kind)?);
                            kassert_eq!(dst.stat().map_err(|_| "stat dst")?.nlink, 1);
                            kassert_eq!(dp.lookup(b"dst2").map_err(|_| "dst2")?.stat().map_err(|_| "stat")?.nlink, 1);
                            // A fault on the count's own write leaves the disk
                            // one high until the next namespace operation
                            // (the true count is kept in memory, and logged).
                            if !reached {
                                kassert_eq!(other.stat().map_err(|_| "ro")?.nlink, 1);
                            }
                        } else if open {
                            kassert!(dst.is_detached());
                            kassert!(is_target(&dst, kind)?);
                        } else {
                            kassert_eq!(dst.stat().map_err(|_| "stat dst")?.nlink, 0);
                        }
                        // Freed, or (a fault while freeing it) left for e2fsck.
                        kassert!(free_inodes(&ro) >= before.0);
                        if res.is_ok() && !reached {
                            // No fault at all: exactly the target's inode and
                            // blocks came back, and memory agrees with disk.
                            let freed = u32::from(last_link);
                            kassert_eq!(
                                (free_inodes(&ro), free_blocks(&ro)),
                                (before.0 + freed, before.1 + target_blocks)
                            );
                            kassert_eq!(
                                src.stat().map_err(|_| "stat src")?.nlink,
                                at_dst.stat().map_err(|_| "ro")?.nlink
                            );
                            kassert_eq!(sp.stat().map_err(|_| "stat sp")?.nlink, ro_sp.stat().map_err(|_| "ro")?.nlink);
                            kassert_eq!(dp.stat().map_err(|_| "stat dp")?.nlink, ro_dp.stat().map_err(|_| "ro")?.nlink);
                        }
                    }
                    drop((src, dst, sp, dp, at_dst, ro_sp, ro_dp, ro, m));
                    kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
                    if !reached {
                        kassert!(res.is_ok());
                        break; // every write of this rename has been failed once
                    }
                    k += 1;
                }
                crate::ktest::diag(format_args!("{kind} open={open}: {} writes, target kept {kept}", k - 1));
                kassert!(kept >= 1);
            }
            minor += 2;
        }
        Ok(())
    }
);

/// The renames the read-fault and put-back sweeps fail, each from `a/s`
/// into `b`: the kind's name, whether the source is a directory, the name
/// it goes to in `b` (`t`: there is a target there, of the source's type),
/// and whether it is an exchange.
#[cfg(feature = "ktest")]
const SWEPT_RENAMES: [(&str, bool, &[u8], bool); 6] = [
    ("move", false, b"s", false),
    ("movedir", true, b"s", false),
    ("exchange", false, b"t", true),
    ("exchangedir", true, b"t", true),
    ("over", false, b"t", false),
    ("overdir", true, b"t", false),
];

/// An image with directories `a` and `b`, `a/s` and (when `target`) `b/t`:
/// files holding `SRC` and `DST`, or directories, `a/s` holding a file
/// `inner` (a target directory must be empty).
#[cfg(feature = "ktest")]
fn swept_rename_image(is_dir: bool, target: bool, minor: u32) -> Result<Vec<u8>, &'static str> {
    use self::ktest_util::*;
    let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), minor)?;
    let a = m.root.mkdir(b"a", 0o755).map_err(|_| "mkdir a")?;
    let b = m.root.mkdir(b"b", 0o755).map_err(|_| "mkdir b")?;
    let mut keep = Vec::new();
    let file = |dir: &Arc<dyn VfsInode>, name: &[u8], data: &[u8]| -> Result<Arc<dyn VfsInode>, &'static str> {
        let f = dir.create(name, 0o644).map_err(|_| "create")?;
        crate::kassert_eq!(f.write_at(0, data).map_err(|_| "write")?, data.len());
        Ok(f)
    };
    if is_dir {
        let s = a.mkdir(b"s", 0o755).map_err(|_| "mkdir s")?;
        keep.push(file(&s, b"inner", SWEPT_SRC)?);
        keep.push(s);
        if target {
            keep.push(b.mkdir(b"t", 0o755).map_err(|_| "mkdir t")?);
        }
    } else {
        keep.push(file(&a, b"s", SWEPT_SRC)?);
        if target {
            keep.push(file(&b, b"t", SWEPT_DST)?);
        }
    }
    m.fs.sync().map_err(|_| "sync")?;
    drop((keep, a, b));
    image(&m)
}

#[cfg(feature = "ktest")]
const SWEPT_SRC: &[u8] = b"SOURCE";
#[cfg(feature = "ktest")]
const SWEPT_DST: &[u8] = b"TARGET-DATA";

/// The swept renames' directories `a` and `b` under `root`.
#[cfg(feature = "ktest")]
fn swept_dirs(root: &Arc<dyn VfsInode>) -> Result<(Arc<dyn VfsInode>, Arc<dyn VfsInode>), &'static str> {
    Ok((root.lookup(b"a").map_err(|_| "a")?, root.lookup(b"b").map_err(|_| "b")?))
}

/// Each inode's change and modification times, in memory.
#[cfg(feature = "ktest")]
fn change_times(inodes: &[&Arc<dyn VfsInode>]) -> Result<Vec<(Timespec, Timespec)>, &'static str> {
    inodes.iter().map(|i| i.stat().map(|st| (st.ctime, st.mtime)).map_err(|_| "stat times")).collect()
}

/// Whether `inode` is the swept rename's source (`SRC`, or a directory
/// holding it as `inner`) or target (`DST`, or an empty directory).
#[cfg(feature = "ktest")]
fn swept_holds(inode: &Arc<dyn VfsInode>, source: bool) -> Result<bool, &'static str> {
    use self::ktest_util::read_all;
    Ok(match (inode.inode_type() == InodeType::Dir, source) {
        (true, true) => read_all(&inode.lookup(b"inner").map_err(|_| "inner")?)? == SWEPT_SRC,
        (true, false) => inode.lookup(b"inner").is_err(),
        (false, true) => read_all(inode)? == SWEPT_SRC,
        (false, false) => read_all(inode)? == SWEPT_DST,
    })
}

crate::ktest!(
    fn ext4_failed_rename_that_cant_read_back_keeps_every_entry() {
        // Fail each write of a rename once, with reads failing after it:
        // every read, or only the blocks past the inode table (directories
        // and data), so the undo can't read back which entries exist (the
        // read-failure arms of the exchange's and the move's undo, and of
        // a replaced target's retirement). For each kind of
        // `SWEPT_RENAMES`:
        // - a rename that didn't complete reported an error, moved no
        //   time, and left a target it would have replaced with its count
        //   (put back, or nameless if that failed), not freed;
        // - each name left holds its inode's data, and an inode left with
        //   no name (an undo that couldn't read its entries back, logged)
        //   is still allocated, for e2fsck, unless it is a target the
        //   source replaced;
        // - each count in memory is at least the entries on disk (a count
        //   too low frees an inode still named), and once the rename
        //   completed, each parent's is exactly what its entries call for;
        // - every name then comes off, and while another name still has
        //   an inode, a new file never gets its number;
        // - for each kind and mode, at least one fault left the undo
        //   unable to read back (a nameless inode, or #322's state), so
        //   reads really failed.
        // Known exception (#322): a move that leaves its source at both
        // names can keep too low a count, whichever undo left it there (the
        // unlink failing with its old directory unreadable, or the undo of
        // the link not knowing whether the new entry went, `roll_back`'s
        // `Err` arm); in that state, whatever the fault, the source's and
        // its new parent's counts and the source's removal aren't checked.
        use self::ktest_util::*;
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::Ordering as O;
        let quota_before = page_cache::DETACHED_QUOTA.used_pages();
        let past_itable = ROOT_DIR_BLOCK_4K * 4096;
        for (kind, is_dir, dst_name, exchange) in SWEPT_RENAMES {
            let target = dst_name == b"t";
            let img = swept_rename_image(is_dir, target, 254)?;
            let flags = RenameFlags(if exchange { RenameFlags::EXCHANGE } else { 0 });
            for all_reads in [true, false] {
                let (mut k, mut errs, mut nameless, mut known) = (1, 0, 0, 0);
                loop {
                    let m = mount(img.clone(), 254)?;
                    let (a, b) = swept_dirs(&m.root)?;
                    let s = a.lookup(b"s").map_err(|_| "s")?;
                    let t = if target { Some(b.lookup(b"t").map_err(|_| "t")?) } else { None };
                    let t_links = match &t {
                        Some(t) => t.stat().map_err(|_| "stat t")?.nlink,
                        None => 0,
                    };
                    let stamped: Vec<&Arc<dyn VfsInode>> = [&s, &a, &b].into_iter().chain(t.as_ref()).collect();
                    let times_before = change_times(&stamped)?;
                    set_clock(&m.fs, Some(Timespec { sec: 1_900_000_000, nsec: 0 }));
                    m.cache.arm_write(k);
                    if all_reads {
                        m.cache.reads_fail_after_write_fault.store(true, O::Release);
                    } else {
                        m.cache.fail_reads_in(past_itable, u64::MAX, 0);
                        m.cache.range_reads_fail_after_write_fault.store(true, O::Release);
                    }
                    let res = a.rename(b"s", &b, dst_name, flags);
                    let reached = m.cache.writes.load(O::Acquire) >= k;
                    m.cache.clear();
                    if !reached {
                        res.map_err(|_| "rename without a fault")?;
                        break;
                    }
                    errs += usize::from(res.is_err());

                    // Which names hold which inode, on disk.
                    let ro = remount_ro(&m, 255)?;
                    let (ro_a, ro_b) = swept_dirs(&ro.root_inode())?;
                    let at_src = ro_a.lookup(b"s").ok();
                    let at_dst = ro_b.lookup(dst_name).ok();
                    let names_of = |ino: u64| {
                        u32::from(at_src.as_ref().is_some_and(|i| i.ino() == ino))
                            + u32::from(at_dst.as_ref().is_some_and(|i| i.ino() == ino))
                    };
                    let s_names = names_of(s.ino());
                    let t_at_src = t.as_ref().is_some_and(|t| at_src.as_ref().is_some_and(|i| i.ino() == t.ino()));
                    let completed = at_dst.as_ref().is_some_and(|i| i.ino() == s.ino())
                        && if exchange { t_at_src } else { at_src.is_none() };
                    if !completed {
                        kassert!(res.is_err());
                        kassert_eq!(change_times(&stamped)?, times_before);
                        if let Some(t) = t.as_ref().filter(|_| !exchange) {
                            kassert_eq!(t.stat().map_err(|_| "stat t")?.nlink, t_links);
                            kassert!(!t.is_detached());
                        }
                    }
                    drop(stamped);
                    let both = !exchange && s_names == 2; // #322
                    known += usize::from(both);
                    for (inode, source) in core::iter::once((&s, true)).chain(t.as_ref().map(|t| (t, false))) {
                        for at in [&at_src, &at_dst].into_iter().flatten().filter(|i| i.ino() == inode.ino()) {
                            kassert!(swept_holds(at, source)?);
                        }
                        if names_of(inode.ino()) == 0 && (source || exchange || !completed) {
                            nameless += 1;
                            kassert!(still_allocated(&ro, inode)?);
                        }
                    }
                    // Counts in memory: never below the entries on disk.
                    let dir_extra = u32::from(is_dir);
                    let subdirs = |at: &Option<Arc<dyn VfsInode>>| {
                        u32::from(at.as_ref().is_some_and(|i| i.inode_type() == InodeType::Dir))
                    };
                    if !both {
                        kassert!(s.stat().map_err(|_| "stat s")?.nlink >= s_names + dir_extra);
                        kassert!(b.stat().map_err(|_| "stat b")?.nlink >= 2 + subdirs(&at_dst));
                    }
                    if let Some(t) = &t {
                        let t_names = names_of(t.ino());
                        if t_names > 0 {
                            kassert!(t.stat().map_err(|_| "stat t")?.nlink >= t_names + dir_extra);
                        }
                    }
                    kassert!(a.stat().map_err(|_| "stat a")?.nlink >= 2 + subdirs(&at_src));
                    if completed {
                        kassert_eq!(a.stat().map_err(|_| "stat a")?.nlink, 2 + subdirs(&at_src));
                        kassert_eq!(b.stat().map_err(|_| "stat b")?.nlink, 2 + subdirs(&at_dst));
                    }
                    drop((ro_a, ro_b, at_src, at_dst, ro));

                    // Every name comes off; while another still names an
                    // inode, a new file doesn't get its number.
                    let cleared = if both { t.as_ref() } else { Some(&s) };
                    for inode in cleared.into_iter().chain(t.as_ref().filter(|_| !both)) {
                        let names: Vec<(&Arc<dyn VfsInode>, &[u8])> = [(&a, &b"s"[..]), (&b, dst_name)]
                            .into_iter()
                            .filter(|(dir, name)| dir.lookup(name).is_ok_and(|i| i.ino() == inode.ino()))
                            .collect();
                        for (i, (dir, name)) in names.iter().enumerate() {
                            if is_dir {
                                let _ = dir.lookup(name).map_err(|_| "lookup")?.unlink(b"inner");
                                dir.rmdir(name).map_err(|_| "rmdir a name")?;
                            } else {
                                dir.unlink(name).map_err(|_| "unlink a name")?;
                            }
                            if i + 1 < names.len() {
                                let canary = m.root.create(b"canary", 0o644).map_err(|_| "canary")?;
                                kassert!(canary.ino() != inode.ino());
                                m.root.unlink(b"canary").map_err(|_| "unlink canary")?;
                            }
                        }
                    }
                    drop((s, t, a, b, m));
                    kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
                    k += 1;
                }
                crate::ktest::diag(format_args!(
                    "{kind}, {}: {} writes, {errs} failed, {nameless} inodes left nameless, {known} #322",
                    if all_reads { "every read failing" } else { "reads past the inode table failing" },
                    k - 1
                ));
                kassert!(errs >= 1);
                kassert!(nameless + known >= 1);
            }
        }
        Ok(())
    }
);

crate::ktest!(
    fn ext4_failed_rename_over_whose_put_back_fails_leaves_the_target_allocated() {
        // A rename over a target (a file, a directory) between directories,
        // with two writes failing (the second within six of the first) or
        // every write from one on: when the move fails and so does putting
        // the target back, the target has no name but keeps its inode, its
        // data and its count (for e2fsck's lost+found; logged), nothing is
        // freed, the source stays where it was, and the rename reported an
        // error.
        // Otherwise the target is back at its name or the source took it.
        use self::ktest_util::*;
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::Ordering as O;
        let quota_before = page_cache::DETACHED_QUOTA.used_pages();
        for (kind, is_dir) in [("over", false), ("overdir", true)] {
            let img = swept_rename_image(is_dir, true, 256)?;
            let total = {
                let m = mount(img.clone(), 256)?;
                let (a, b) = swept_dirs(&m.root)?;
                m.cache.arm_write(0);
                a.rename(b"s", &b, b"t", RenameFlags(0)).map_err(|_| "measure")?;
                m.cache.writes.load(O::Acquire)
            };
            let faults = (1..=total)
                .flat_map(|c1| (c1 + 1..=c1 + 6).map(move |c2| (c1, Some(c2))))
                .chain((1..=total).map(|k| (k, None)));
            let (mut orphaned, mut kept, mut landed) = (0, 0, 0);
            for (c1, c2) in faults {
                let m = mount(img.clone(), 256)?;
                let (a, b) = swept_dirs(&m.root)?;
                let s = a.lookup(b"s").map_err(|_| "s")?;
                let t = b.lookup(b"t").map_err(|_| "t")?;
                let t_links = t.stat().map_err(|_| "stat t")?.nlink;
                let before = (free_inodes(&m.fs), free_blocks(&m.fs));
                match c2 {
                    Some(c2) => m.cache.arm_write_pair(c1, c2),
                    None => m.cache.arm_write_from(c1),
                }
                let res = a.rename(b"s", &b, b"t", RenameFlags(0));
                m.cache.clear();
                let ro = remount_ro(&m, 257)?;
                let (ro_a, ro_b) = swept_dirs(&ro.root_inode())?;
                let src_kept = ro_a.lookup(b"s").is_ok_and(|i| i.ino() == s.ino());
                match ro_b.lookup(b"t") {
                    Ok(at) if at.ino() == s.ino() && !src_kept => {
                        landed += 1;
                        kassert!(swept_holds(&at, true)?);
                        kassert!(ro_a.lookup(b"s").is_err());
                    }
                    Ok(at) if at.ino() == t.ino() => {
                        kept += 1;
                        kassert!(res.is_err());
                        kassert!(swept_holds(&at, false)?);
                        kassert_eq!(ro_a.lookup(b"s").map_err(|_| "ro s")?.ino(), s.ino());
                    }
                    // The move failed and so did the put-back: the source
                    // where it was (and at the target's name too when the
                    // move's undo failed as well).
                    at => {
                        orphaned += 1;
                        kassert!(res.is_err());
                        kassert!(at.as_ref().map_or(true, |at| at.ino() == s.ino() && src_kept));
                        let at = ro_a.lookup(b"s").map_err(|_| "ro s")?;
                        kassert_eq!(at.ino(), s.ino());
                        kassert!(swept_holds(&at, true)?);
                        kassert!(still_allocated(&ro, &t)?);
                        kassert_eq!((free_inodes(&ro), free_blocks(&ro)), before);
                        kassert!(!t.is_detached());
                        kassert_eq!(t.stat().map_err(|_| "stat t")?.nlink, t_links);
                        kassert!(swept_holds(&t, false)?);
                    }
                }
                drop((s, t, a, b, ro_a, ro_b, ro, m));
                kassert_eq!(page_cache::DETACHED_QUOTA.used_pages(), quota_before);
            }
            crate::ktest::diag(format_args!(
                "{kind}: {total} writes; target orphaned {orphaned}, kept {kept}, replaced {landed}"
            ));
            kassert!(orphaned >= 1 && kept >= 1 && landed >= 1);
        }
        Ok(())
    }
);

crate::ktest!(
    fn ext4_unlink_restores_an_inline_symlink_when_the_directory_cant_be_read_back() {
        // Fail each write of the unlink once, with every read failing after
        // it: wherever the unlink stops, a symlink whose entry survives must
        // still have its target.
        use self::ktest_util::*;
        use crate::kassert_eq;
        use core::sync::atomic::Ordering as O;
        let mut k = 1;
        let mut survived = 0;
        loop {
            let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 228)?;
            m.root.symlink(b"s", b"inline/target").map_err(|_| "symlink")?;
            m.cache.arm_write(k);
            m.cache.reads_fail_after_write_fault.store(true, O::Release);
            let res = m.root.unlink(b"s");
            let reached = m.cache.writes.load(O::Acquire) >= k;
            m.cache.clear();
            if !reached {
                res.map_err(|_| "unlink without a fault")?;
                break;
            }
            if res.is_err() {
                let ro = remount_ro(&m, 229)?;
                if let Ok(s) = ro.root_inode().lookup(b"s") {
                    survived += 1;
                    kassert_eq!(s.readlink().map_err(|_| "readlink")?, b"inline/target".to_vec());
                }
            }
            k += 1;
        }
        crate::ktest::diag(format_args!("entries that survived a failed unlink: {survived}"));
        crate::kassert!(survived >= 1); // the case this test is about was reached
        Ok(())
    }
);

crate::ktest!(
    fn ext4_unlink_never_restores_onto_a_freed_inode() {
        // Fail each write of the unlink once, with only the directory's
        // reads failing after it, so the adapter decides from the inode
        // itself: an entry that survived keeps its target, and an inode the
        // unlink already zeroed (and maybe freed) gets nothing written back.
        use self::ktest_util::*;
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::Ordering as O;
        let dir_block = ktest_util::ROOT_DIR_BLOCK_4K * 4096;
        let (mut k, mut survived, mut gone) = (1, 0, 0);
        loop {
            let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 236)?;
            m.root.symlink(b"s", b"inline/target").map_err(|_| "symlink")?;
            let ino = u32::try_from(m.root.lookup(b"s").map_err(|_| "lookup")?.ino()).map_err(|_| "ino")?;
            m.cache.fail_reads_in(dir_block, dir_block + 4096, 0);
            m.cache.arm_write(k);
            m.cache.range_reads_fail_after_write_fault.store(true, O::Release);
            let res = m.root.unlink(b"s");
            let reached = m.cache.writes.load(O::Acquire) >= k;
            m.cache.clear();
            if !reached {
                res.map_err(|_| "unlink without a fault")?;
                break;
            }
            if res.is_err() {
                let ro = remount_ro(&m, 237)?;
                if let Ok(s) = ro.root_inode().lookup(b"s") {
                    survived += 1;
                    kassert_eq!(s.readlink().map_err(|_| "readlink")?, b"inline/target".to_vec());
                } else {
                    gone += 1;
                    let (mode, links) = raw_mode_and_links(&ro, ino)?;
                    kassert!(!(mode == 0 && links > 0)); // nothing restored onto a zeroed inode
                }
            }
            k += 1;
        }
        crate::ktest::diag(format_args!("failed unlinks: {survived} kept the entry, {gone} removed it"));
        kassert!(survived >= 1 && gone >= 1);
        Ok(())
    }
);

crate::ktest!(
    fn ext4_mmap_page_hands_out_the_cached_page_filled_from_disk() {
        // `vfs::mmap_page` on ext4: the page holding the file's bytes, read
        // from disk when not cached; the same `Arc` again, and the one
        // `read_at`/`write_at` use; pinned while held; refused at the hard
        // limit; `Range` past EOF and `NoDev` for a directory. (The pages'
        // bytes are read here without Lock 5: nothing else uses this mount.)
        use self::ktest_util::*;
        use crate::vfs::mmap_page;
        use crate::{kassert, kassert_eq};
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 245)?;
        let data = pattern(PAGE_SIZE + 1000, 3);
        let f = m.root.create(b"map.bin", 0o644).map_err(|_| "create")?;
        kassert_eq!(f.write_at(0, &data).map_err(|_| "write")?, data.len());
        f.sync(false).map_err(|_| "sync")?;
        // A fresh mount of the disk: nothing cached, so the pages are filled
        // from the disk.
        let m2 = mount(image(&m)?, 246)?;
        let g = m2.root.lookup(b"map.bin").map_err(|_| "lookup")?;
        let e = ext4_inode(&g)?;
        kassert_eq!(e.pages.lock().len(), 0);
        let first = mmap_page(&g, 0).map_err(|_| "map page 0")?;
        kassert_eq!(first.file_size(), data.len() as u64);
        kassert!(crate::mm::frame::is_allocated(first.page().phys_frame()));
        let mut got = [0u8; PAGE_SIZE];
        first.page().read_bytes(0, &mut got);
        kassert!(got[..] == data[..PAGE_SIZE]);
        let last = mmap_page(&g, 1).map_err(|_| "map page 1")?;
        last.page().read_bytes(0, &mut got);
        kassert!(got[..1000] == data[PAGE_SIZE..]);
        kassert!(got[1000..].iter().all(|&b| b == 0)); // past EOF: zero
        kassert_eq!(e.pages.lock().len(), 2); // both cached
        // The same page again, and the one the file's writes go to.
        let again = mmap_page(&g, 0).map_err(|_| "map page 0 again")?;
        kassert!(Arc::ptr_eq(again.page(), first.page()));
        drop(again);
        kassert_eq!(g.write_at(10, b"coherent").map_err(|_| "write_at")?, 8);
        first.page().read_bytes(10, &mut got[..8]);
        kassert!(&got[..8] == b"coherent");
        // Held, a page is pinned: reclaim evicts the other (clean, unheld)
        // one but not it.
        g.sync(false).map_err(|_| "sync again")?;
        drop(last);
        e.reclaim_clean_pages(usize::MAX);
        kassert_eq!(e.pages.lock().len(), 1);
        kassert!(Arc::ptr_eq(mmap_page(&g, 0).map_err(|_| "map pinned")?.page(), first.page()));
        // At the cache's hard limit no page is added (mapped pages are
        // pinned, so reclaim couldn't get back under it); a cached one is
        // still handed out. (One warning, the limit crossed.)
        page_cache::set_ext4_limits_for_test(Some((usize::MAX, page_cache::ext4_cached_pages())));
        let missing = mmap_page(&g, 1).err();
        let cached = mmap_page(&g, 0).map(|p| Arc::ptr_eq(p.page(), first.page()));
        page_cache::set_ext4_limits_for_test(None);
        kassert_eq!(missing, Some(VfsError::NoSpc));
        kassert!(matches!(cached, Ok(true)));
        kassert_eq!(e.pages.lock().len(), 1);
        // Past EOF there is no page: `1 << 52` pages is 2^64 bytes, which a
        // plain multiplication wraps to offset 0. Nor in a directory or a
        // symlink.
        kassert_eq!(mmap_page(&g, 2).err(), Some(VfsError::Range));
        kassert_eq!(mmap_page(&g, 1 << 52).err(), Some(VfsError::Range));
        kassert_eq!(mmap_page(&g, u64::MAX).err(), Some(VfsError::Range));
        kassert_eq!(mmap_page(&m2.root, 0).err(), Some(VfsError::NoDev));
        let link = m2.root.symlink(b"link", b"map.bin").map_err(|_| "symlink")?;
        kassert_eq!(mmap_page(&link, 0).err(), Some(VfsError::NoDev));
        Ok(())
    }
);

crate::ktest!(
    fn ext4_mmap_page_refuses_an_unlinked_file_nobody_has_open() {
        // An ext4 file unlinked while not open has no blocks left to read
        // (they were freed): `NoEnt`, as `read_at` says, not a page read
        // from freed blocks.
        use self::ktest_util::*;
        use crate::vfs::mmap_page;
        use crate::{kassert, kassert_eq};
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 249)?;
        let f = m.root.create(b"gone.bin", 0o644).map_err(|_| "create")?;
        kassert_eq!(f.write_at(0, &pattern(PAGE_SIZE, 5)).map_err(|_| "write")?, PAGE_SIZE);
        f.sync(false).map_err(|_| "sync")?;
        ext4_inode(&f)?.pages.lock().remove_from(0); // nothing cached to hand out
        kassert_eq!(f.open_count(), 0);
        m.root.unlink(b"gone.bin").map_err(|_| "unlink")?;
        kassert!(!f.is_detached());
        kassert_eq!(mmap_page(&f, 0).err(), Some(VfsError::NoEnt));
        Ok(())
    }
);

crate::ktest!(
    fn ext4_mmap_page_reports_a_failed_disk_read() {
        // A page fill whose disk read fails: `Io`, nothing cached, and one
        // warning naming the device, inode and page for the failing streak
        // (a mapping's fault turns the error into SIGBUS, which names
        // nothing); the next fill reads the page and ends the streak, so the
        // next failure warns again. Page 1, so a wrong offset shows. (Two
        // warnings in all, pinned by test_ktest.py.)
        use self::ktest_util::*;
        use crate::vfs::mmap_page;
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::Ordering as O;
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 250)?;
        let data = pattern(2 * PAGE_SIZE, 9);
        let f = m.root.create(b"bad.bin", 0o644).map_err(|_| "create")?;
        kassert_eq!(f.write_at(0, &data).map_err(|_| "write")?, data.len());
        f.sync(false).map_err(|_| "sync")?;
        let e = ext4_inode(&f)?;
        e.pages.lock().remove_from(0); // filled from the disk next
        m.cache.fail_reads.store(true, O::Release);
        let first = mmap_page(&f, 1).err(); // warns
        let second = mmap_page(&f, 1).err(); // same streak: not logged again
        m.cache.clear();
        kassert_eq!(first, Some(VfsError::Io));
        kassert_eq!(second, Some(VfsError::Io));
        kassert_eq!(e.pages.lock().len(), 0);
        let mapped = mmap_page(&f, 1).map_err(|_| "map after the fault")?;
        let mut got = [0u8; PAGE_SIZE];
        mapped.page().read_bytes(0, &mut got);
        kassert!(got[..] == data[PAGE_SIZE..]);
        drop(mapped);
        e.pages.lock().remove_from(0);
        m.cache.fail_reads.store(true, O::Release);
        let third = mmap_page(&f, 1).err(); // a new streak: warns again
        m.cache.clear();
        kassert_eq!(third, Some(VfsError::Io));
        Ok(())
    }
);

crate::ktest!(
    fn ext4_mmap_page_maps_holes_in_linked_and_detached_files() {
        // A hole in a linked, sparse ext4 file is read from the disk, as
        // zeros. A hole in a detached file (unlinked while open: its blocks
        // are freed, its data held in its pages) gets a zero page from the
        // detached quota, never a read of the freed blocks; the ext4 hard
        // limit doesn't apply to it, and the used-up quota refuses it.
        use self::ktest_util::*;
        use crate::vfs::mmap_page;
        use crate::{kassert, kassert_eq};
        use page_cache::{DETACHED_QUOTA, DETACHED_QUOTA_PAGES, DetachedQuotaGuard, detached_cached_pages};
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 251)?;
        let data = pattern(PAGE_SIZE, 13);
        let f = m.root.create(b"sparse.bin", 0o644).map_err(|_| "create")?;
        kassert_eq!(f.write_at(0, &data).map_err(|_| "write")?, PAGE_SIZE);
        f.truncate(4 * PAGE_SIZE as u64).map_err(|_| "truncate")?; // pages 1-3: holes
        f.sync(false).map_err(|_| "sync")?;
        let e = ext4_inode(&f)?;
        let mut got = [0xa5u8; PAGE_SIZE];
        // Linked: page 3 is filled from the disk, as zeros.
        kassert_eq!(e.pages.lock().len(), 1);
        let hole = mmap_page(&f, 3).map_err(|_| "map a linked hole")?;
        hole.page().read_bytes(0, &mut got);
        kassert!(got.iter().all(|&b| b == 0));
        drop(hole);
        e.pages.lock().remove_from(1); // the holes uncached again
        // Open (the checked open reference an open file holds), then unlink:
        // detached, page 0 kept in memory.
        let open = lumen_vfs::OpenGuard::try_new(Arc::clone(&f)).map_err(|_| "open")?;
        m.root.unlink(b"sparse.bin").map_err(|_| "unlink")?;
        kassert!(f.is_detached());
        kassert_eq!(e.pages.lock().len(), 1);
        let before = detached_cached_pages();
        // With the ext4 cache "at its hard limit" (any count is), a detached
        // hole is still mapped: a zero page, counted as detached.
        page_cache::set_ext4_limits_for_test(Some((usize::MAX, 0)));
        let mapped = mmap_page(&f, 1);
        page_cache::set_ext4_limits_for_test(None);
        let mapped = mapped.map_err(|_| "map a detached hole")?;
        got.fill(0xa5);
        mapped.page().read_bytes(0, &mut got);
        kassert!(got.iter().all(|&b| b == 0));
        kassert_eq!(detached_cached_pages(), before + 1);
        kassert_eq!(e.pages.lock().len(), 2);
        // The detached quota used up: the next hole is refused.
        let mut rest = DetachedQuotaGuard::new();
        let reserved = rest.try_reserve(DETACHED_QUOTA_PAGES - DETACHED_QUOTA.used_pages());
        let refused = mmap_page(&f, 2).err();
        drop(rest);
        reserved.map_err(|_| "reserve the rest of the detached quota")?;
        kassert_eq!(refused, Some(VfsError::NoSpc));
        kassert_eq!(detached_cached_pages(), before + 1);
        drop(mapped);
        drop(open);
        Ok(())
    }
);

crate::ktest!(
    fn ext4_shared_mapping_stores_reach_the_disk_after_the_last_unmap() {
        // #266: a `MAP_SHARED` store sets nothing in the page; only the
        // writable-mapping count keeps it dirty. Once the last writable
        // mapping goes the page must still be dirty, so reclaim writes it
        // back instead of dropping it as clean.
        use self::ktest_util::*;
        use crate::vfs::mmap_page;
        use crate::{kassert, kassert_eq};
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 247)?;
        let old = pattern(PAGE_SIZE, 11);
        let f = m.root.create(b"shared.bin", 0o644).map_err(|_| "create")?;
        kassert_eq!(f.write_at(0, &old).map_err(|_| "write")?, PAGE_SIZE);
        f.sync(false).map_err(|_| "sync")?; // on disk, and the page clean
        let mapped = mmap_page(&f, 0).map_err(|_| "map")?;
        let page = mapped.page();
        kassert!(!page.is_dirty());
        // A write fault maps it writable (`set_dirty` and the count). A
        // writeback while it is mapped clears `dirty` (checked: the test
        // means nothing if it didn't); then a store through the mapping,
        // which only the CPU sees (and which takes no lock).
        page.set_dirty();
        page.inc_writable_maps();
        f.sync(false).map_err(|_| "sync while mapped")?;
        kassert!(!page.swap_dirty(false)); // the raw bit, not `is_dirty`
        kassert_eq!(page.writable_maps(), 1);
        let new = pattern(PAGE_SIZE, 97);
        page.write_bytes(0, &new);
        // `munmap`: the last writable mapping goes.
        page.dec_writable_maps();
        kassert_eq!(page.writable_maps(), 0);
        let dirty_after_unmap = page.is_dirty();
        drop(mapped);
        // Reclaim: dirty pages written back, then clean unheld ones evicted,
        // so what is read next comes from the disk. (Checked before the
        // dirty bit, so losing the store shows as lost data.)
        let e = ext4_inode(&f)?;
        e.reclaim_dirty_pages(usize::MAX);
        kassert_eq!(e.pages.lock().len(), 0);
        let ro = remount_ro(&m, 248)?;
        kassert!(read_all(&ro.root_inode().lookup(b"shared.bin").map_err(|_| "ro lookup")?)? == new);
        kassert!(read_all(&f)? == new); // read back from the disk
        kassert!(dirty_after_unmap);
        Ok(())
    }
);

crate::ktest!(
    fn ext4_open_racing_unlink_keeps_the_data_or_fails_cleanly() {
        // #262: an unlink landing between the path lookup of an open and the
        // open file description it builds. Gate 6 frees an inode nobody has
        // open, so the reference must already be up by then.
        use self::ktest_util::*;
        use crate::vfs::KernelVfsLock;
        use crate::{kassert, kassert_eq};
        use lumen_vfs::{OpenFile, OpenFlags, open_inode, open_inode_guarded, unlink_at};
        // Each failure's `VfsError` goes to the TAP diagnostics.
        fn why(what: &'static str) -> impl FnOnce(VfsError) -> &'static str {
            move |e| {
                crate::ktest::diag(format_args!("{what}: {e:?}"));
                what
            }
        }
        let m = mount(crate::vfs::build_minimal_ext4_image_blocks(256), 242)?;
        let tables = &m.tables;
        let data = pattern(2 * PAGE_SIZE + 100, 9);
        for name in ["kept", "freed"] {
            let f = m.root.create(name.as_bytes(), 0o644).map_err(why("create"))?;
            kassert_eq!(f.write_at(0, &data).map_err(why("write"))?, data.len());
            f.sync(false).map_err(why("sync"))?;
        }
        let rd = OpenFlags::from_bits_truncate(OpenFlags::O_RDONLY);

        // The guarded open's reference carries into the open file
        // description: the unlink sees it and detaches, and the file reads
        // its data.
        let opened = open_inode_guarded(tables, None, b"/kept", rd, 0, 0).map_err(why("open kept"))?;
        unlink_at(tables, None, b"/kept", false).map_err(why("unlink kept"))?;
        let of = OpenFile::<KernelVfsLock<u64>>::from_opened(opened).map_err(why("OpenFile::from_opened"))?;
        kassert!(of.inode.is_detached());
        kassert_eq!(of.inode.open_count(), 1);
        kassert_eq!(read_all(&of.inode)?, data);
        let kept = Arc::clone(&of.inode);
        drop(of);
        kassert_eq!(kept.open_count(), 0);

        // `open_inode` holds no reference once it returns: the unlink frees
        // the inode, and an open file description for it is refused.
        let (inode, path) = open_inode(tables, None, b"/freed", rd, 0, 0).map_err(why("open freed"))?;
        kassert_eq!(inode.open_count(), 0);
        unlink_at(tables, None, b"/freed", false).map_err(why("unlink freed"))?;
        kassert!(!inode.is_detached());
        let res =
            OpenFile::<KernelVfsLock<u64>>::new_unchecked_flags(Arc::clone(&inode), Arc::from(path.as_slice()), rd);
        kassert_eq!(res.err(), Some(VfsError::NoEnt));
        kassert_eq!(inode.open_count(), 0);
        Ok(())
    }
);
