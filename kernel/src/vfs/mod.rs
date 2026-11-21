//! Virtual filesystem integration (M7+).
//!
//! Backed on disk by the pinned `ext4plus` crate (`=0.1.0-rc.2`, elected license:
//! MIT, `default-features = false` with `sync` + `multi-threaded`; see
//! `docs/ARCHITECTURE.md` sections 2, 9, and 15).
//!
//! This module currently pins and links `ext4plus` into both `lumen-kernel` and
//! `lumen-kernel-ktest` and exercises its core writable ext4 operations via
//! `ext4_crate_smoke`. Full `PageCache`, `BlockCache` adapter, `tmpfs`/`devfs`/
//! `procfs`, and boot mounting land in #253.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::error::Error;
use core::fmt;
use core::time::Duration;

use ext4plus::dir::Dir;
use ext4plus::error::Ext4Error;
use ext4plus::file::{truncate, write_at};
use ext4plus::inode::{Inode, InodeCreationOptions, InodeFlags, InodeMode};
use ext4plus::path::PathBuf;
use ext4plus::{DirEntryName, Ext4, Ext4Read, Ext4Write, FileType, IncompatibleFeatures};

use crate::sync::SleepMutex;

const BLOCK_SIZE: usize = 4096;
const IMAGE_BLOCKS: usize = 64;
const INODE_SIZE: usize = 256;
const INODES_PER_GROUP: u32 = 32;
// Blocks 0..=6: superblock (0), BGD (1), block bitmap (2), inode bitmap (3),
// inode table (4..=5), root directory data block (6).
const USED_BLOCKS: u32 = 7;
const FREE_BLOCKS: u32 = IMAGE_BLOCKS as u32 - USED_BLOCKS;
// Inodes 1..=10 are marked reserved/used (`s_first_ino = 11`).
const RESERVED_INODES: u32 = 10;
const FREE_INODES: u32 = INODES_PER_GROUP - RESERVED_INODES;

#[derive(Debug)]
struct MemIoErr;

impl fmt::Display for MemIoErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("in-memory ext4 I/O out of bounds")
    }
}

impl Error for MemIoErr {}

#[derive(Clone)]
struct MemExt4Device(Arc<SleepMutex<Vec<u8>>>);

impl MemExt4Device {
    fn new(data: Vec<u8>) -> Self {
        Self(Arc::new(SleepMutex::new(data)))
    }
}

impl Ext4Read for MemExt4Device {
    fn read(&self, start_byte: u64, dst: &mut [u8]) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
        let guard = self.0.lock();
        let start = usize::try_from(start_byte).map_err(|_| Box::new(MemIoErr))?;
        let end = start.checked_add(dst.len()).ok_or_else(|| Box::new(MemIoErr))?;
        let slice = guard.get(start..end).ok_or_else(|| Box::new(MemIoErr))?;
        dst.copy_from_slice(slice);
        Ok(())
    }
}

impl Ext4Write for MemExt4Device {
    fn write(&self, start_byte: u64, src: &[u8]) -> Result<(), Box<dyn Error + Send + Sync + 'static>> {
        let mut guard = self.0.lock();
        let start = usize::try_from(start_byte).map_err(|_| Box::new(MemIoErr))?;
        let end = start.checked_add(src.len()).ok_or_else(|| Box::new(MemIoErr))?;
        let slice = guard.get_mut(start..end).ok_or_else(|| Box::new(MemIoErr))?;
        slice.copy_from_slice(src);
        Ok(())
    }
}

fn write_u16le(buf: &mut [u8], off: usize, val: u16) {
    buf[off..off + 2].copy_from_slice(&val.to_le_bytes());
}

fn write_u32le(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

/// Build a minimal 64-block (256 KiB, 4 KiB blocks, 256-byte inodes) ext4 image
/// matching `-t ext4 -b 4096 -I 256 -O ^orphan_file,^metadata_csum_seed,^metadata_csum,^has_journal`.
fn build_minimal_ext4_image() -> Vec<u8> {
    let mut img = vec![0u8; IMAGE_BLOCKS * BLOCK_SIZE];

    // Block 0, offset 1024..2048: Superblock.
    let sb = &mut img[1024..2048];
    write_u32le(sb, 0x00, INODES_PER_GROUP); // s_inodes_count
    write_u32le(sb, 0x04, IMAGE_BLOCKS as u32); // s_blocks_count_lo
    write_u32le(sb, 0x0c, FREE_BLOCKS); // s_free_blocks_count_lo
    write_u32le(sb, 0x10, FREE_INODES); // s_free_inodes_count
    write_u32le(sb, 0x14, 0); // s_first_data_block (0 for 4 KiB blocks)
    write_u32le(sb, 0x18, 2); // s_log_block_size (1024 << 2 == 4096)
    write_u32le(sb, 0x1c, 2); // s_log_cluster_size
    write_u32le(sb, 0x20, IMAGE_BLOCKS as u32); // s_blocks_per_group
    write_u32le(sb, 0x24, IMAGE_BLOCKS as u32); // s_clusters_per_group
    write_u32le(sb, 0x28, INODES_PER_GROUP); // s_inodes_per_group
    write_u16le(sb, 0x36, u16::MAX); // s_max_mnt_count
    write_u16le(sb, 0x38, 0xef53); // s_magic
    write_u16le(sb, 0x3a, 1); // s_state (clean)
    write_u16le(sb, 0x3c, 1); // s_errors (continue)
    write_u32le(sb, 0x4c, 1); // s_rev_level (EXT4_DYNAMIC_REV)
    write_u32le(sb, 0x54, RESERVED_INODES + 1); // s_first_ino (11)
    write_u16le(sb, 0x58, INODE_SIZE as u16); // s_inode_size
    // Compat: EXT_ATTR (0x8) | DIR_INDEX (0x20), ^has_journal (omit RESIZE_INODE without inode 7).
    write_u32le(sb, 0x5c, 0x28);
    // Incompat: FILETYPE (0x2) | EXTENTS (0x40) | 64BIT (0x80) | FLEX_BG (0x200), ^metadata_csum_seed.
    write_u32le(sb, 0x60, 0x2c2);
    // RO-compat: SPARSE_SUPER (0x1) | LARGE_FILE (0x2) | HUGE_FILE (0x8) | DIR_NLINK (0x20) | EXTRA_ISIZE (0x40),
    // ^orphan_file, ^metadata_csum.
    write_u32le(sb, 0x64, 0x6b);
    sb[0x68..0x78].copy_from_slice(&[
        0x4c, 0x75, 0x6d, 0x65, 0x6e, 0x4f, 0x53, 0x2d, 0x4d, 0x37, 0x2d, 0x45, 0x78, 0x74, 0x34, 0x01,
    ]);
    sb[0x78..0x83].copy_from_slice(b"lumen-smoke");
    write_u16le(sb, 0xfe, 64); // s_desc_size (64 for IS_64BIT)
    write_u16le(sb, 0x15c, 32); // s_min_extra_isize
    write_u16le(sb, 0x15e, 32); // s_want_extra_isize

    // Block 1: Block Group Descriptor 0 (64 bytes).
    let bgd = &mut img[BLOCK_SIZE..BLOCK_SIZE + 64];
    write_u32le(bgd, 0x00, 2); // bg_block_bitmap_lo
    write_u32le(bgd, 0x04, 3); // bg_inode_bitmap_lo
    write_u32le(bgd, 0x08, 4); // bg_inode_table_lo
    write_u16le(bgd, 0x0c, FREE_BLOCKS as u16); // bg_free_blocks_count_lo
    write_u16le(bgd, 0x0e, FREE_INODES as u16); // bg_free_inodes_count_lo
    write_u16le(bgd, 0x10, 1); // bg_used_dirs_count_lo (root)
    write_u16le(bgd, 0x12, 0x0004); // bg_flags (EXT4_BG_INODE_ZEROED)
    write_u16le(bgd, 0x1c, 0); // bg_unused_inodes_count_lo

    // Block 2: Block bitmap (blocks 0..7 used; padding bits >= IMAGE_BLOCKS marked used).
    let bmap = &mut img[2 * BLOCK_SIZE..3 * BLOCK_SIZE];
    bmap[0] = 0x7f;
    bmap[IMAGE_BLOCKS / 8..].fill(0xff);

    // Block 3: Inode bitmap (inodes 1..=10 reserved/used; padding bits >= INODES_PER_GROUP marked used).
    let imap = &mut img[3 * BLOCK_SIZE..4 * BLOCK_SIZE];
    imap[0] = 0xff;
    imap[1] = 0x03;
    imap[(INODES_PER_GROUP as usize) / 8..].fill(0xff);

    // Blocks 4..6: Inode table; inode 2 (root) is at index 1.
    let root_off = 4 * BLOCK_SIZE + INODE_SIZE;
    let root_inode = &mut img[root_off..root_off + INODE_SIZE];
    write_u16le(root_inode, 0x00, 0o040755); // i_mode
    write_u32le(root_inode, 0x04, BLOCK_SIZE as u32); // i_size_lo
    write_u16le(root_inode, 0x1a, 2); // i_links_count (. and ..)
    write_u32le(root_inode, 0x1c, (BLOCK_SIZE / 512) as u32); // i_blocks_lo (512B sectors)
    write_u32le(root_inode, 0x20, 0x0008_0000); // i_flags (EXT4_EXTENTS_FL)
    // Extent tree header (1 entry, max 4, depth 0) + extent (logical 0 -> physical 6, len 1).
    write_u16le(root_inode, 0x28, 0xf30a); // eh_magic
    write_u16le(root_inode, 0x2a, 1); // eh_entries
    write_u16le(root_inode, 0x2c, 4); // eh_max
    write_u16le(root_inode, 0x2e, 0); // eh_depth
    write_u32le(root_inode, 0x34, 0); // ee_block
    write_u16le(root_inode, 0x38, 1); // ee_len
    write_u16le(root_inode, 0x3a, 0); // ee_start_hi
    write_u32le(root_inode, 0x3c, 6); // ee_start_lo
    write_u16le(root_inode, 0x80, 32); // i_extra_isize

    // Block 6: Root directory data block (`.` and `..`).
    let root_dir = &mut img[6 * BLOCK_SIZE..7 * BLOCK_SIZE];
    write_u32le(root_dir, 0, 2); // `.` inode
    write_u16le(root_dir, 4, 12); // `.` rec_len
    root_dir[6] = 1; // name_len
    root_dir[7] = 2; // FT_DIR
    root_dir[8] = b'.';

    write_u32le(root_dir, 12, 2); // `..` inode
    write_u16le(root_dir, 16, (BLOCK_SIZE - 12) as u16); // `..` rec_len
    root_dir[18] = 2; // name_len
    root_dir[19] = 2; // FT_DIR
    root_dir[20..22].copy_from_slice(b"..");

    img
}

struct SmokeStats {
    blocks_count: u64,
    free_blocks_after: u64,
    free_inodes_after: u32,
}

/// Exercise `ext4plus` mount, read, create, write, truncate, fast symlink,
/// hard link, mkdir/rmdir, and unlink against an in-memory 64-block ext4 image.
///
/// Shared between [`log_ext4_crate_summary`] (so `lumen-kernel` links `ext4plus`
/// read/write/dir/extent codegen for `make audit`) and [`ext4_crate_smoke`].
fn exercise_ext4_smoke() -> Result<SmokeStats, &'static str> {
    let image = build_minimal_ext4_image();
    {
        let ro_dev = MemExt4Device::new(image.clone());
        let ro_fs = Ext4::load_with_writer(Box::new(ro_dev), None).map_err(|_| "ext4 load ro failed")?;
        let ro_err = ro_fs.create_inode(InodeCreationOptions {
            file_type: FileType::Regular,
            mode: InodeMode::S_IFREG | InodeMode::S_IRUSR | InodeMode::S_IWUSR,
            uid: 0,
            gid: 0,
            time: Duration::ZERO,
            flags: InodeFlags::empty(),
        });
        if !matches!(ro_err, Err(Ext4Error::Readonly)) {
            return Err("read-only mount did not return Ext4Error::Readonly");
        }
    }

    let dev = MemExt4Device::new(image);
    let fs = Ext4::load_with_writer(Box::new(dev.clone()), Some(Box::new(dev))).map_err(|_| "ext4 load rw failed")?;
    if fs.superblock().features().incompatible().contains(IncompatibleFeatures::RECOVERY) {
        return Err("unexpected RECOVERY bit on clean image");
    }

    let root = fs.read_root_inode().map_err(|_| "read_root_inode failed")?;
    if root.index.get() != 2 || !root.file_type().is_dir() || root.links_count() != 2 {
        return Err("unexpected root inode metadata");
    }
    let root_ino = root.index;
    let mut root_dir = Dir::open_inode(&fs, root).map_err(|_| "Dir::open_inode failed")?;

    // 1. Create regular file, link, write_at, truncate, read back.
    let mut file_inode = fs
        .create_inode(InodeCreationOptions {
            file_type: FileType::Regular,
            mode: InodeMode::S_IFREG
                | InodeMode::S_IRUSR
                | InodeMode::S_IWUSR
                | InodeMode::S_IRGRP
                | InodeMode::S_IROTH,
            uid: 0,
            gid: 0,
            time: Duration::from_secs(1),
            flags: InodeFlags::empty(),
        })
        .map_err(|_| "create_inode regular failed")?;
    let file_ino = file_inode.index;
    let hello_name = DirEntryName::try_from(b"hello.txt".as_slice()).map_err(|_| "DirEntryName failed")?;
    root_dir.link(hello_name, &mut file_inode).map_err(|_| "Dir::link file failed")?;
    if file_inode.links_count() != 1 {
        return Err("file links_count != 1 after link");
    }

    let payload = b"hello ext4plus!!";
    let written = write_at(&fs, &mut file_inode, payload, 0).map_err(|_| "write_at failed")?;
    if written != payload.len() {
        return Err("short write_at");
    }
    let data = fs.read("/hello.txt").map_err(|_| "Ext4::read failed")?;
    if data != payload {
        return Err("read payload mismatch");
    }

    truncate(&fs, &mut file_inode, 5).map_err(|_| "truncate failed")?;
    let truncated = fs.read("/hello.txt").map_err(|_| "Ext4::read after truncate failed")?;
    if truncated != b"hello" {
        return Err("truncated payload mismatch");
    }

    // 2. Fast symlink (<= 59 bytes) + read_link + follow read.
    // Note: ext4plus 0.1.0-rc.2's `Ext4::symlink` clears `InodeFlags::EXTENTS`
    // and stores inline target bytes in `i_block`, so `Dir::unlink` ->
    // `delete_file` -> `BlockMap::free_all` would misinterpret target bytes as
    // block numbers if called directly on an inline symlink (#253 adapter
    // clears `i_block` on disk before `Dir::unlink`). Keep `/sym` allocated
    // here (0 data blocks, 1 inode) and exercise `Dir::unlink` on regular
    // files and directories below.
    let sym_name = DirEntryName::try_from(b"sym".as_slice()).map_err(|_| "sym DirEntryName failed")?;
    let _sym_inode = fs
        .symlink(&mut root_dir, sym_name, PathBuf::new("/hello.txt"), 0, 0, Duration::from_secs(1))
        .map_err(|_| "Ext4::symlink failed")?;
    let target = fs.read_link("/sym").map_err(|_| "Ext4::read_link failed")?;
    if target.as_path().to_str().ok() != Some("/hello.txt") {
        return Err("read_link target mismatch");
    }
    let via_sym = fs.read("/sym").map_err(|_| "read via symlink failed")?;
    if via_sym != b"hello" {
        return Err("read via symlink payload mismatch");
    }

    // 3. Hard link (links_count == 2) + unlink first name (links_count == 1).
    let mut file_inode = Inode::read(&fs, file_ino).map_err(|_| "Inode::read before hardlink failed")?;
    let hard_name = DirEntryName::try_from(b"hello_hard.txt".as_slice()).map_err(|_| "hard DirEntryName failed")?;
    root_dir.link(hard_name, &mut file_inode).map_err(|_| "Dir::link hardlink failed")?;
    if file_inode.links_count() != 2 {
        return Err("hardlink links_count != 2");
    }
    let remaining = root_dir
        .unlink(hello_name, file_inode)
        .map_err(|_| "unlink first hardlink failed")?
        .ok_or("file unexpectedly deleted at links_count == 2")?;
    if remaining.links_count() != 1 {
        return Err("remaining hardlink links_count != 1");
    }
    if fs.read("/hello_hard.txt").map_err(|_| "read remaining hardlink failed")? != b"hello" {
        return Err("remaining hardlink content mismatch");
    }

    // 4. mkdir (Dir::init + Dir::link) + rmdir.
    let sub_raw = fs
        .create_inode(InodeCreationOptions {
            file_type: FileType::Directory,
            mode: InodeMode::S_IFDIR
                | InodeMode::S_IRUSR
                | InodeMode::S_IWUSR
                | InodeMode::S_IXUSR
                | InodeMode::S_IRGRP
                | InodeMode::S_IXGRP
                | InodeMode::S_IROTH
                | InodeMode::S_IXOTH,
            uid: 0,
            gid: 0,
            time: Duration::from_secs(1),
            flags: InodeFlags::empty(),
        })
        .map_err(|_| "create_inode dir failed")?;
    let sub_dir = Dir::init(fs.clone(), sub_raw, root_ino).map_err(|_| "Dir::init failed")?;
    let sub_ino = sub_dir.inode().index;
    drop(sub_dir);
    let mut sub_inode = Inode::read(&fs, sub_ino).map_err(|_| "Inode::read subdir failed")?;
    let sub_name = DirEntryName::try_from(b"sub".as_slice()).map_err(|_| "sub DirEntryName failed")?;
    root_dir.link(sub_name, &mut sub_inode).map_err(|_| "Dir::link subdir failed")?;
    if sub_inode.links_count() != 2 || root_dir.inode().links_count() != 3 {
        return Err("mkdir link counts mismatch");
    }

    // Explicit rmdir: drop `.` self-link (2 -> 1), unlink from parent (1 -> 0 frees),
    // and decrement parent's `..` link count (3 -> 2).
    sub_inode.set_links_count(1);
    sub_inode.write(&fs).map_err(|_| "subdir set_links_count failed")?;
    if root_dir.unlink(sub_name, sub_inode).map_err(|_| "rmdir unlink failed")?.is_some() {
        return Err("subdir not freed during rmdir");
    }
    let parent_links = root_dir.inode().links_count();
    root_dir.inode_mut().set_links_count(parent_links.saturating_sub(1));
    root_dir.inode_mut().write(&fs).map_err(|_| "parent write after rmdir failed")?;
    if root_dir.inode().links_count() != 2 || fs.exists("/sub").map_err(|_| "exists(/sub) failed")? {
        return Err("rmdir post-state mismatch");
    }

    // 5. Final unlink of remaining hard link (links_count 1 -> 0 frees file & blocks).
    let file_inode = Inode::read(&fs, file_ino).map_err(|_| "Inode::read before final unlink failed")?;
    if file_inode.links_count() != 1 {
        return Err("expected links_count == 1 before final unlink");
    }
    if root_dir.unlink(hard_name, file_inode).map_err(|_| "final unlink failed")?.is_some() {
        return Err("file not freed on final unlink");
    }
    if fs.exists("/hello_hard.txt").map_err(|_| "exists(/hello_hard.txt) failed")? {
        return Err("unlinked file still exists");
    }

    let sb = fs.superblock();
    let stats = SmokeStats {
        blocks_count: sb.blocks_count(),
        free_blocks_after: sb.free_blocks_count(),
        free_inodes_after: sb.free_inodes_count(),
    };
    if stats.blocks_count != IMAGE_BLOCKS as u64
        || stats.free_blocks_after != u64::from(FREE_BLOCKS)
        || stats.free_inodes_after != FREE_INODES - 1
    {
        return Err("free block/inode accounting leaked after unlink/rmdir");
    }
    Ok(stats)
}

/// Probe and log a debug summary of the pinned `ext4plus` crate at boot.
///
/// Called from `kmain_on_kernel_stack` so `ext4plus` mount, read, write,
/// directory, symlink, and extent code paths are linked into both
/// `lumen-kernel` and `lumen-kernel-ktest` and verified by `make audit`.
/// Emits only `log::debug!` to leave default boot logs untouched.
pub fn log_ext4_crate_summary() {
    match exercise_ext4_smoke() {
        Ok(stats) => {
            log::debug!(
                target: "vfs",
                "ext4plus =0.1.0-rc.2 (MIT): smoke ok (blocks={}, free_blocks={}, free_inodes={})",
                stats.blocks_count,
                stats.free_blocks_after,
                stats.free_inodes_after,
            );
        }
        Err(err) => {
            log::debug!(target: "vfs", "ext4plus =0.1.0-rc.2: smoke failed: {err}");
        }
    }
}

crate::ktest!(
    fn ext4_crate_smoke() {
        let stats = exercise_ext4_smoke()?;
        crate::kassert_eq!(stats.blocks_count, IMAGE_BLOCKS as u64);
        crate::kassert_eq!(stats.free_blocks_after, u64::from(FREE_BLOCKS));
        crate::kassert_eq!(stats.free_inodes_after, FREE_INODES - 1);
        Ok(())
    }
);
