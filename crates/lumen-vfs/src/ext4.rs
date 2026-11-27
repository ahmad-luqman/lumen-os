//! ext4's on-disk encodings that the kernel's adapter encodes and decodes
//! itself, because `ext4plus` doesn't: a device node's number in `i_block`,
//! timestamps before 1970 or past 2038 (`ext4plus` reads the low 32 bits
//! of a time as unsigned), and the first block `ext4plus`'s `delete_file`
//! frees of an inode, with where that block's bit is in the block bitmap
//! (`ext4plus` keeps its block maps and bitmaps to itself; #330).
//! Writable mounts also check allocation counts and ownership through
//! [`check_allocation`] before enabling writes (#355).
//!
//! The first two follow Linux (`fs/ext4/inode.c`'s `ext4_iget`,
//! `fs/ext4/ext4.h`'s `ext4_encode_extra_time`/`ext4_decode_extra_time`,
//! `include/linux/kdev_t.h`); the extent and block-map layouts are ext4's
//! (`fs/ext4/ext4_extents.h`, `fs/ext4/indirect.c`).

use alloc::vec;
use alloc::vec::Vec;
use core::time::Duration;

use crate::types::{DevId, Timespec};

/// Linux's `old_decode_dev`: a 16-bit device number, 8 bits of major and 8 of
/// minor.
pub const fn old_decode_dev(val: u16) -> DevId {
    DevId::makedev(((val >> 8) & 0xff) as u32, (val & 0xff) as u32)
}

/// Linux's `new_decode_dev`: a 32-bit device number, 12 bits of major and 20
/// of minor (the minor's low 8 bits first).
pub const fn new_decode_dev(dev: u32) -> DevId {
    DevId::makedev((dev & 0xfff00) >> 8, (dev & 0xff) | ((dev >> 12) & 0xfff00))
}

/// A character or block device's number from its inode's first two `i_block`
/// words, as `ext4_iget` reads it: the old encoding in `i_block[0]` when that
/// is non-zero, else the new one in `i_block[1]`.
pub const fn rdev_from_i_block(i_block0: u32, i_block1: u32) -> DevId {
    if i_block0 != 0 { old_decode_dev(i_block0 as u16) } else { new_decode_dev(i_block1) }
}

/// The earliest time an ext4 inode holds (`EXT4_TIMESTAMP_MIN`, 1901-12-13).
pub const TIME_MIN: i64 = i32::MIN as i64;
/// The latest time an inode with the extra time fields holds
/// (`EXT4_EXTRA_TIMESTAMP_MAX`, 2446-05-10): two epoch bits over a signed
/// 32-bit second.
pub const TIME_MAX_EXTRA: i64 = (1 << 34) - 1 + i32::MIN as i64;
/// The latest time an inode without them holds
/// (`EXT4_NON_EXTRA_TIMESTAMP_MAX`, 2038-01-19).
pub const TIME_MAX_NO_EXTRA: i64 = i32::MAX as i64;

/// `ts` clamped and rounded as Linux's `timestamp_truncate` does on a
/// filesystem whose inodes have (`extra`) or lack the extra time fields:
/// seconds clamped to the range, nanoseconds dropped if that changed the
/// second, and whole seconds without the extra fields. Linux decides this per
/// filesystem; the kernel's adapter applies it per inode and per field, from
/// `i_extra_isize`, because `ext4plus` can't grow an inode's extra fields as
/// Linux does.
pub fn truncate_time(ts: Timespec, extra: bool) -> Timespec {
    let max = if extra { TIME_MAX_EXTRA } else { TIME_MAX_NO_EXTRA };
    let sec = ts.sec.clamp(TIME_MIN, max);
    let nsec = if !extra || sec != ts.sec { 0 } else { ts.nsec.clamp(0, 999_999_999) };
    Timespec { sec, nsec }
}

/// The `Duration` to hand `ext4plus`'s time setters so that they write Linux's
/// bytes for `ts` (after [`truncate_time`]). They write `as_secs()`'s low 32
/// bits as the base word and its next two bits as the extra word's epoch, so
/// this is the signed second's low 32 bits with Linux's epoch above them
/// (`(sec - (s32)sec) >> 32`), never negative.
pub fn time_to_disk(ts: Timespec, extra: bool) -> Duration {
    let ts = truncate_time(ts, extra);
    let low = ts.sec as u32;
    let epoch = ((ts.sec - i64::from(low as i32)) >> 32) as u64 & 3;
    Duration::new((epoch << 32) | u64::from(low), ts.nsec as u32)
}

/// The time an `ext4plus` getter's `Duration` stands for: the base word
/// signed, plus the epoch bits times 2^32 (`ext4_decode_extra_time`). An
/// inode without the extra fields reads with no epoch, so its times run
/// from 1901 to 2038.
pub fn time_from_disk(d: Duration) -> Timespec {
    let secs = d.as_secs();
    let low = i64::from(secs as u32 as i32);
    let epoch = ((secs >> 32) & 3) as i64;
    Timespec { sec: low + (epoch << 32), nsec: i64::from(d.subsec_nanos()) }
}

/// Why [`first_block_freed`] couldn't name a block: a read failed, the map
/// doesn't parse (a bad extent header, an index block of the wrong depth),
/// or finding its first block takes more reads than [`MAP_READS_MAX`] (a
/// map with many empty blocks first, as a block map truncated by
/// `ext4plus`, which keeps its emptied indirect blocks, can be).
#[derive(Debug, PartialEq, Eq)]
pub enum BlockMapError<E> {
    Read(E),
    Corrupt,
    TooManyReads,
}

/// The most map blocks [`first_block_freed`] reads: an extent tree's
/// leftmost path (at most 5 deep) or a block map's three indirect levels,
/// with room for a few empty nodes on the way.
pub const MAP_READS_MAX: usize = 16;

const EXTENT_MAGIC: u16 = 0xf30a;
/// An extent tree's depth limit (Linux's `EXT4_MAX_EXTENT_DEPTH`).
const EXTENT_DEPTH_MAX: u16 = 5;

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// The first block `ext4plus`'s `delete_file` frees of an inode whose
/// `i_block` is `i_block` (`extents`: the inode has the `EXTENTS` flag),
/// reading the map's other blocks (`block_size` bytes each) through `read`;
/// `None` if it frees none.
///
/// `delete_file` frees an inode's data blocks first, in file order (an
/// extent tree's extents depth first, leftmost first, each extent's blocks
/// from its start; a block map's direct blocks, then those under its
/// single, double and triple indirect blocks), its map blocks after them,
/// and only then writes, zeroes and frees the inode. So once any of an
/// inode's blocks is free, this one is. The order is `ext4plus`
/// 0.1.0-rc.2's, read from its source (`ExtentTree::collect_extents`,
/// `BlockMap::collect_data_and_metadata_blocks`, `free_freed_ranges`): no
/// test runs `ext4plus` against it beyond the kernel's ktests, so a bump
/// of `ext4plus` must check it again. With no data block, it is the first map block freed: an
/// extent tree's first index block under its root, a block map's first
/// indirect block. An extent with no start or no length is skipped, as
/// `free_freed_ranges` skips it.
///
/// Not for an inode whose `i_block` holds no block numbers (an inline
/// symlink's target, a device's number): it would read those as blocks.
pub fn first_block_freed<E>(
    i_block: &[u8; 60],
    extents: bool,
    block_size: usize,
    mut read: impl FnMut(u64, &mut [u8]) -> Result<(), E>,
) -> Result<Option<u64>, BlockMapError<E>> {
    let mut reads = 0;
    let mut read_block = |block: u64, buf: &mut [u8]| {
        reads += 1;
        if reads > MAP_READS_MAX {
            return Err(BlockMapError::TooManyReads);
        }
        read(block, buf).map_err(BlockMapError::Read)
    };
    if extents {
        let depth = extent_header(i_block, None)?;
        if let Some(block) = first_extent_start(i_block, depth, block_size, &mut read_block)? {
            return Ok(Some(block));
        }
        // No data: the first index block under the root, if any.
        Ok((depth > 0 && le16(i_block, 2) > 0).then(|| extent_index_child(i_block, 0)))
    } else {
        let word = |i: usize| u64::from(le32(i_block, i * 4));
        if let Some(block) = (0..12).map(word).find(|&b| b != 0) {
            return Ok(Some(block));
        }
        for (level, slot) in [(1, 12), (2, 13), (3, 14)] {
            let top = word(slot);
            if top != 0
                && let Some(block) = first_mapped(top, level, block_size, &mut read_block)?
            {
                return Ok(Some(block));
            }
        }
        // No data: the first indirect block, if any.
        Ok((12..15).map(word).find(|&b| b != 0))
    }
}

/// An extent node's header checked (its magic, its entries within its room
/// and its room within the node's bytes, its depth at most 5, and `want`
/// when given); its depth.
fn extent_header<E>(node: &[u8], want: Option<u16>) -> Result<u16, BlockMapError<E>> {
    let (entries, max, depth) = (le16(node, 2), le16(node, 4), le16(node, 6));
    let fits = 12 + usize::from(max) * 12 <= node.len();
    let depth_ok = depth <= EXTENT_DEPTH_MAX && want.is_none_or(|w| w == depth);
    if le16(node, 0) != EXTENT_MAGIC || entries > max || !fits || !depth_ok {
        return Err(BlockMapError::Corrupt);
    }
    Ok(depth)
}

/// Index entry `i`'s child block (`ei_leaf_lo`, `ei_leaf_hi`).
fn extent_index_child(node: &[u8], i: usize) -> u64 {
    let at = 12 + i * 12;
    u64::from(le32(node, at + 4)) | (u64::from(le16(node, at + 8)) << 32)
}

/// The first extent's start in `node` (its header checked, at `depth`) and
/// under it, depth first, leftmost first.
fn first_extent_start<E>(
    node: &[u8],
    depth: u16,
    block_size: usize,
    read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), BlockMapError<E>>,
) -> Result<Option<u64>, BlockMapError<E>> {
    let entries = usize::from(le16(node, 2));
    if depth == 0 {
        return Ok((0..entries).find_map(|i| {
            let at = 12 + i * 12;
            // `ee_len`: an unwritten extent's (past 32768, its length plus
            // 32768) is never 0 either.
            let len = le16(node, at + 4);
            let start = (u64::from(le16(node, at + 6)) << 32) | u64::from(le32(node, at + 8));
            (start != 0 && len != 0).then_some(start)
        }));
    }
    let mut child = vec![0u8; block_size];
    for i in 0..entries {
        read(extent_index_child(node, i), &mut child)?;
        extent_header(&child, Some(depth - 1))?;
        if let Some(block) = first_extent_start(&child, depth - 1, block_size, read)? {
            return Ok(Some(block));
        }
    }
    Ok(None)
}

/// The first non-zero block number `level` indirect blocks under `block`
/// (1: `block` lists data blocks), depth first.
fn first_mapped<E>(
    block: u64,
    level: u32,
    block_size: usize,
    read: &mut impl FnMut(u64, &mut [u8]) -> Result<(), BlockMapError<E>>,
) -> Result<Option<u64>, BlockMapError<E>> {
    let mut buf = vec![0u8; block_size];
    read(block, &mut buf)?;
    let mapped = buf.as_chunks::<4>().0.iter().map(|w| u64::from(u32::from_le_bytes(*w))).filter(|&b| b != 0);
    for below in mapped {
        if level == 1 {
            return Ok(Some(below));
        }
        if let Some(found) = first_mapped(below, level - 1, block_size, read)? {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// A volume's block geometry, read from its superblock by
/// [`Geometry::from_superblock`], which refuses one whose groups don't fit
/// their bitmaps: every `Geometry` places each of its blocks' bits inside
/// its group's bitmap block, or (bigalloc) places none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    block_size: u64,
    first_data_block: u32,
    blocks_per_group: u32,
    blocks_count: u64,
    is_64bit: bool,
    /// `RO_COMPAT_BIGALLOC`: the bitmaps count clusters, not blocks
    /// ([`Geometry::bitmap_bit`] places no block's bit).
    bigalloc: bool,
}

/// Where a block's bit is: its group, the byte in that group's block bitmap
/// (below the block size), and the bit's mask in that byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitmapBit {
    pub group: u32,
    pub byte: u32,
    pub mask: u8,
}

/// Why [`Geometry::from_superblock`] refused a superblock: the field, its
/// value and its limit (its `Display` says all three).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeometryError {
    /// `s_log_block_size` past 6 (a block over 64 KiB).
    BlockSize { log: u32 },
    /// `s_blocks_per_group` 0, or more than one bitmap block's bits (`max`).
    BlocksPerGroup { value: u32, max: u64 },
    /// On a bigalloc volume, `s_clusters_per_group` 0 or more than `max`.
    ClustersPerGroup { value: u32, max: u64 },
    /// `s_first_data_block` not below `s_blocks_count`.
    FirstDataBlock { first: u32, count: u64 },
}

impl core::fmt::Display for GeometryError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::BlockSize { log } => write!(f, "s_log_block_size {log} > 6"),
            Self::BlocksPerGroup { value, max } => write!(f, "s_blocks_per_group {value} not in 1..={max}"),
            Self::ClustersPerGroup { value, max } => write!(f, "s_clusters_per_group {value} not in 1..={max}"),
            Self::FirstDataBlock { first, count } => {
                write!(f, "s_first_data_block {first} not below s_blocks_count {count}")
            }
        }
    }
}

/// The superblock's `INCOMPAT_64BIT` feature: `s_blocks_count_hi` counts.
const INCOMPAT_64BIT: u32 = 0x80;
/// The superblock's `RO_COMPAT_BIGALLOC` feature: a bitmap bit per cluster.
const RO_COMPAT_BIGALLOC: u32 = 0x200;

impl Geometry {
    /// The geometry in the superblock's 1024 bytes: `s_blocks_count_lo`
    /// (0x04, with `s_blocks_count_hi` at 0x150 when `s_feature_incompat`,
    /// 0x60, has 64-bit), `s_first_data_block` (0x14), `s_log_block_size`
    /// (0x18), `s_blocks_per_group` (0x20) and, when `s_feature_ro_compat`
    /// (0x64) has bigalloc, `s_clusters_per_group` (0x24). Refused unless
    /// the block size is 1 to 64 KiB, a group has 1 to 8 × block-size bits
    /// in its bitmap (blocks, or on a bigalloc volume clusters, as Linux's
    /// `ext4_fill_super` checks) and the first data block is on the volume.
    pub fn from_superblock(sb: &[u8; 1024]) -> Result<Self, GeometryError> {
        let log = le32(sb, 0x18);
        if log > 6 {
            return Err(GeometryError::BlockSize { log });
        }
        let block_size = 1024u64 << log;
        let max = 8 * block_size;
        let first_data_block = le32(sb, 0x14);
        let blocks_per_group = le32(sb, 0x20);
        let is_64bit = le32(sb, 0x60) & INCOMPAT_64BIT != 0;
        let bigalloc = le32(sb, 0x64) & RO_COMPAT_BIGALLOC != 0;
        let hi = if is_64bit { le32(sb, 0x150) } else { 0 };
        let blocks_count = (u64::from(hi) << 32) | u64::from(le32(sb, 0x04));
        let in_bitmap = |value: u32| value != 0 && u64::from(value) <= max;
        if bigalloc {
            let value = le32(sb, 0x24);
            if !in_bitmap(value) {
                return Err(GeometryError::ClustersPerGroup { value, max });
            }
            if blocks_per_group == 0 {
                return Err(GeometryError::BlocksPerGroup { value: 0, max: u64::from(u32::MAX) });
            }
        } else if !in_bitmap(blocks_per_group) {
            return Err(GeometryError::BlocksPerGroup { value: blocks_per_group, max });
        }
        if u64::from(first_data_block) >= blocks_count {
            return Err(GeometryError::FirstDataBlock { first: first_data_block, count: blocks_count });
        }
        Ok(Self { block_size, first_data_block, blocks_per_group, blocks_count, is_64bit, bigalloc })
    }

    pub fn block_size(&self) -> u64 {
        self.block_size
    }

    /// Whether `s_blocks_count_hi` and the descriptors' high halves count.
    pub fn is_64bit(&self) -> bool {
        self.is_64bit
    }

    /// Block `block`'s first byte on the device; `None` for block 0 (its
    /// first 1024 bytes reserved, never a map's or a bitmap's) or past the
    /// volume, which `ext4plus`'s block reads refuse too.
    pub fn block_byte(&self, block: u64) -> Option<u64> {
        if block == 0 || block >= self.blocks_count {
            return None;
        }
        block.checked_mul(self.block_size)
    }

    /// Where block `block`'s bit is in its group's block bitmap, as
    /// `ext4plus`'s `block_block_group_location` and `BitmapHandle` place it;
    /// `None` for a block outside the groups, and on a bigalloc volume
    /// (whose bits are clusters'; such a volume mounts read-only).
    pub fn bitmap_bit(&self, block: u64) -> Option<BitmapBit> {
        if self.bigalloc {
            return None;
        }
        let rel = block.checked_sub(u64::from(self.first_data_block))?;
        if block >= self.blocks_count {
            return None;
        }
        let group = u32::try_from(rel / u64::from(self.blocks_per_group)).ok()?;
        let bit = (rel % u64::from(self.blocks_per_group)) as u32;
        Some(BitmapBit { group, byte: bit / 8, mask: 1 << (bit % 8) })
    }
}

/// Count clear bits only within `bits`, excluding bitmap padding.
pub fn bitmap_free_count(bitmap: &[u8], bits: usize) -> Option<u64> {
    if bits > bitmap.len().checked_mul(8)? {
        return None;
    }
    let whole = bits / 8;
    let used: u64 = bitmap[..whole].iter().map(|b| u64::from(b.count_ones())).sum();
    let tail = if bits.is_multiple_of(8) { 0 } else { (bitmap[whole] & ((1 << (bits % 8)) - 1)).count_ones() };
    Some(bits as u64 - used - u64::from(tail))
}

/// Visit every data and map block, including unwritten extents and mappings
/// beyond EOF. For a map block, `visit` must fill the supplied buffer after
/// checking ownership; rejecting a duplicate before reading bounds cyclic
/// maps. Extent depth is at most five, indirect depth at most three.
pub fn visit_inode_blocks<E>(
    i_block: &[u8; 60],
    extents: bool,
    block_size: usize,
    mut visit: impl FnMut(u64, Option<&mut [u8]>) -> Result<(), E>,
) -> Result<(), BlockMapError<E>> {
    fn extents_walk<E>(
        node: &[u8],
        want: Option<u16>,
        bs: usize,
        visit: &mut impl FnMut(u64, Option<&mut [u8]>) -> Result<(), E>,
    ) -> Result<(), BlockMapError<E>> {
        let depth = extent_header(node, want)?;
        for i in 0..usize::from(le16(node, 2)) {
            if depth == 0 {
                let at = 12 + i * 12;
                let encoded = le16(node, at + 4);
                let len = if encoded > 32768 { encoded - 32768 } else { encoded };
                let start = u64::from(le32(node, at + 8)) | (u64::from(le16(node, at + 6)) << 32);
                if start == 0 || len == 0 {
                    return Err(BlockMapError::Corrupt);
                }
                for block in start..start + u64::from(len) {
                    visit(block, None).map_err(BlockMapError::Read)?;
                }
            } else {
                let mut child = vec![0; bs];
                visit(extent_index_child(node, i), Some(&mut child)).map_err(BlockMapError::Read)?;
                extents_walk(&child, Some(depth - 1), bs, visit)?;
            }
        }
        Ok(())
    }
    fn indirect_walk<E>(
        block: u64,
        depth: u32,
        bs: usize,
        visit: &mut impl FnMut(u64, Option<&mut [u8]>) -> Result<(), E>,
    ) -> Result<(), BlockMapError<E>> {
        if block == 0 {
            return Ok(());
        }
        if depth == 0 {
            return visit(block, None).map_err(BlockMapError::Read);
        }
        let mut buf = vec![0; bs];
        visit(block, Some(&mut buf)).map_err(BlockMapError::Read)?;
        for word in buf.as_chunks::<4>().0 {
            indirect_walk(u64::from(u32::from_le_bytes(*word)), depth - 1, bs, visit)?;
        }
        Ok(())
    }
    if !(1024..=65536).contains(&block_size) || !block_size.is_power_of_two() {
        return Err(BlockMapError::Corrupt);
    }
    if extents {
        extents_walk(i_block, None, block_size, &mut visit)
    } else {
        for i in 0..15 {
            indirect_walk(u64::from(le32(i_block, i * 4)), i.saturating_sub(11) as u32, block_size, &mut visit)?;
        }
        Ok(())
    }
}

/// Why an allocation check could not establish that writing is safe.
#[derive(Debug, PartialEq, Eq)]
pub enum AllocationError<E> {
    Read(E),
    Invalid(&'static str),
    Count { group: Option<u32>, bitmap: &'static str, recorded: u64, actual: u64 },
    Block { block: u64, problem: &'static str },
}

impl<E: core::fmt::Debug> core::fmt::Display for AllocationError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Read(e) => write!(f, "allocation metadata read failed: {e:?}"),
            Self::Invalid(why) => write!(f, "invalid allocation metadata: {why}"),
            Self::Count { group, bitmap, recorded, actual } => {
                if let Some(group) = group {
                    write!(f, "group {group} ")?;
                } else {
                    write!(f, "superblock ")?;
                }
                write!(f, "{bitmap} free count {recorded} != bitmap {actual}")
            }
            Self::Block { block, problem } => write!(f, "block {block} {problem}"),
        }
    }
}

/// Allocated blocks with no owner are leaks, never a reason to refuse writes.
#[derive(Debug, PartialEq, Eq)]
pub struct AllocationReport {
    pub leaked_blocks: u64,
}

struct AllocationGroup {
    block_bitmap: u64,
    inode_bitmap_block: u64,
    inode_table: u64,
    inodes: Vec<u8>,
    inode_count: usize,
}

fn bit_set(bits: &[u8], bit: usize) -> bool {
    bits[bit / 8] & (1 << (bit % 8)) != 0
}

fn set_bit(bits: &mut [u8], bit: usize) {
    bits[bit / 8] |= 1 << (bit % 8);
}

struct AllocationCheck<'a, R> {
    read: &'a mut R,
    geometry: Geometry,
    allocated: Vec<u8>,
    owned: Vec<u8>,
}

impl<E, R: FnMut(u64, &mut [u8]) -> Result<(), E>> AllocationCheck<'_, R> {
    fn claim(&mut self, block: u64) -> Result<(), AllocationError<E>> {
        let problem = if block < u64::from(self.geometry.first_data_block) || block >= self.geometry.blocks_count {
            Some("is outside the volume")
        } else if !bit_set(&self.allocated, block as usize) {
            Some("is mapped but free")
        } else if bit_set(&self.owned, block as usize) {
            Some("is mapped twice")
        } else {
            None
        };
        if let Some(problem) = problem {
            return Err(AllocationError::Block { block, problem });
        }
        set_bit(&mut self.owned, block as usize);
        Ok(())
    }

    fn read_block(&mut self, block: u64, buf: &mut [u8]) -> Result<(), AllocationError<E>> {
        let off = self
            .geometry
            .block_byte(block)
            .ok_or(AllocationError::Block { block, problem: "is outside the volume" })?;
        (self.read)(off, buf).map_err(AllocationError::Read)
    }

    fn inode(&mut self, raw: &[u8]) -> Result<(), AllocationError<E>> {
        // i_file_acl is private in ext4plus, like i_block; Linux's high
        // half is present only for a 64-bit filesystem.
        let acl =
            u64::from(le32(raw, 0x68)) | if self.geometry.is_64bit { u64::from(le16(raw, 0x76)) << 32 } else { 0 };
        if acl != 0 {
            self.claim(acl)?;
        }
        let mode = le16(raw, 0) & 0xf000;
        let flags = le32(raw, 0x20);
        let blocks = u64::from(le32(raw, 0x1c)) | (u64::from(le16(raw, 0x74)) << 32);
        let acl_sectors = if acl == 0 { 0 } else { self.geometry.block_size / 512 };
        let fast_symlink = mode == 0xa000 && blocks == acl_sectors && flags & 0x80000 == 0;
        // Zero-mode reserved inodes (notably the bad-block inode) may map
        // blocks. Bitmap membership, not links/dtime/size, decides liveness.
        if flags & 0x1000_0000 != 0 || fast_symlink || !matches!(mode, 0 | 0x4000 | 0x8000 | 0xa000) {
            return Ok(());
        }
        let map: &[u8; 60] = raw[0x28..0x64].try_into().unwrap();
        visit_inode_blocks(map, flags & 0x80000 != 0, self.geometry.block_size as usize, |block, buf| {
            self.claim(block)?;
            if let Some(buf) = buf {
                self.read_block(block, buf)?;
            }
            Ok(())
        })
        .map_err(|e| match e {
            BlockMapError::Read(e) => e,
            _ => AllocationError::Invalid("inode block map"),
        })
    }
}

/// Check bitmap/descriptor/superblock counts before walking every allocated
/// inode's data and metadata. No writes occur. Fixed metadata and the resize
/// inode's reserved GDT references are included; duplicate or free mappings
/// fail closed. The memory bound is two bits per volume block plus the inode
/// bitmaps, group records, two block buffers and at most five map blocks.
///
/// Shared external xattr blocks conservatively fail as duplicate ownership:
/// this checker does not certify their reference counts. Read-only callers
/// should skip this check entirely.
pub fn check_allocation<E>(
    sb: &[u8; 1024],
    mut read: impl FnMut(u64, &mut [u8]) -> Result<(), E>,
) -> Result<AllocationReport, AllocationError<E>> {
    use AllocationError::{Count, Invalid, Read};
    let geometry = Geometry::from_superblock(sb).map_err(|_| Invalid("block geometry"))?;
    let bs = geometry.block_size as usize;
    let first = u64::from(geometry.first_data_block);
    if geometry.bigalloc || first != u64::from(bs == 1024) || le32(sb, 0x60) & 0x10 != 0 {
        return Err(Invalid("unsupported allocation geometry"));
    }
    let blocks = geometry.blocks_count;
    if blocks.checked_mul(bs as u64).is_none() {
        return Err(Invalid("volume byte size overflows"));
    }
    let groups = (blocks - first).div_ceil(u64::from(geometry.blocks_per_group));
    let ipg = le32(sb, 0x28) as usize;
    let isz = usize::from(le16(sb, 0x58));
    let inodes = u64::from(le32(sb, 0));
    if ipg == 0
        || ipg > bs * 8
        || isz < 128
        || isz > bs
        || !isz.is_power_of_two()
        || inodes == 0
        || inodes.div_ceil(ipg as u64) != groups
    {
        return Err(Invalid("inode geometry"));
    }
    let dsz = if geometry.is_64bit { usize::from(le16(sb, 0xfe)) } else { 32 };
    if !matches!(dsz, 32 | 64) || (geometry.is_64bit && dsz != 64) {
        return Err(Invalid("descriptor size"));
    }
    let bytes = usize::try_from(blocks.div_ceil(8)).map_err(|_| Invalid("volume too large"))?;
    let mut check = AllocationCheck { read: &mut read, geometry, allocated: Vec::new(), owned: Vec::new() };
    for bits in [&mut check.allocated, &mut check.owned] {
        bits.try_reserve_exact(bytes).map_err(|_| Invalid("no memory for allocation bitmap"))?;
        bits.resize(bytes, 0);
    }
    let mut descriptors = Vec::new();
    let mut bitmap = vec![0; bs];
    let mut free_blocks = 0;
    let mut free_inodes = 0;
    for group in 0..groups {
        let mut desc = [0; 64];
        (check.read)((first + 1) * bs as u64 + group * dsz as u64, &mut desc[..dsz]).map_err(Read)?;
        let address =
            |at| u64::from(le32(&desc, at)) | if dsz == 64 { u64::from(le32(&desc, at + 32)) << 32 } else { 0 };
        let count = |at| u64::from(le16(&desc, at)) | if dsz == 64 { u64::from(le16(&desc, at + 32)) << 16 } else { 0 };
        // Uninitialized bitmaps need checksum-aware synthesis, which the
        // writable profile already excludes (GDT_CSUM/METADATA_CSUM).
        if le16(&desc, 0x12) & 3 != 0 {
            return Err(Invalid("uninitialized bitmap"));
        }
        let start = first + group * u64::from(geometry.blocks_per_group);
        let real = (blocks - start).min(u64::from(geometry.blocks_per_group)) as usize;
        let mut g = AllocationGroup {
            block_bitmap: address(0),
            inode_bitmap_block: address(4),
            inode_table: address(8),
            inodes: vec![0; bs],
            inode_count: (inodes - group * ipg as u64).min(ipg as u64) as usize,
        };
        check.read_block(g.block_bitmap, &mut bitmap)?;
        let actual = bitmap_free_count(&bitmap, real).unwrap();
        if actual != count(0xc) {
            return Err(Count { group: Some(group as u32), bitmap: "block", recorded: count(0xc), actual });
        }
        free_blocks += actual;
        for bit in 0..real {
            if bit_set(&bitmap, bit) {
                set_bit(&mut check.allocated, start as usize + bit);
            }
        }
        check.read_block(g.inode_bitmap_block, &mut g.inodes)?;
        let actual = bitmap_free_count(&g.inodes, g.inode_count).unwrap();
        if actual != count(0xe) {
            return Err(Count { group: Some(group as u32), bitmap: "inode", recorded: count(0xe), actual });
        }
        free_inodes += actual;
        descriptors.push(g);
    }
    let recorded_blocks =
        u64::from(le32(sb, 0xc)) | if geometry.is_64bit { u64::from(le32(sb, 0x158)) << 32 } else { 0 };
    for (bitmap, recorded, actual) in
        [("block", recorded_blocks, free_blocks), ("inode", u64::from(le32(sb, 0x10)), free_inodes)]
    {
        if recorded != actual {
            return Err(Count { group: None, bitmap, recorded, actual });
        }
    }
    let gdt_blocks = (groups * dsz as u64).div_ceil(bs as u64);
    let reserved = u64::from(le16(sb, 0xce));
    let has_super = |g: u64| {
        if g == 0 {
            return true;
        }
        if le32(sb, 0x5c) & 0x200 != 0 {
            // sparse_super2
            return g == u64::from(le32(sb, 0x24c)) || g == u64::from(le32(sb, 0x250));
        }
        if le32(sb, 0x64) & 1 == 0 || g == 1 {
            return true;
        }
        [3, 5, 7].into_iter().any(|base| {
            let mut n = g;
            while n > 1 && n.is_multiple_of(base) {
                n /= base;
            }
            n == 1
        })
    };
    for (i, g) in descriptors.iter().enumerate() {
        if has_super(i as u64) {
            let start = first + i as u64 * u64::from(geometry.blocks_per_group);
            for block in start..start + 1 + gdt_blocks {
                check.claim(block)?;
            }
        }
        check.claim(g.block_bitmap)?;
        check.claim(g.inode_bitmap_block)?;
        for index in 0..(ipg * isz).div_ceil(bs) as u64 {
            // Claim index zero before adding anything to an untrusted
            // 64-bit address. A successful claim bounds it to the volume.
            check.claim(g.inode_table + index)?;
        }
    }
    if le32(sb, 0x60) & 0x100 != 0 {
        // MMP
        check.claim(u64::from(le32(sb, 0x168)) | (u64::from(le32(sb, 0x16c)) << 32))?;
    }
    // Reserved GDT space must not overlap a bitmap, table or live GDT.
    // Clear these temporary claims so the resize inode can own them once.
    for group in 0..groups {
        if has_super(group) {
            let start = first + group * u64::from(geometry.blocks_per_group) + 1 + gdt_blocks;
            if 1 + gdt_blocks + reserved > (blocks - (start - 1 - gdt_blocks)).min(u64::from(geometry.blocks_per_group))
            {
                return Err(Invalid("reserved GDT past group"));
            }
            for block in start..start + reserved {
                check.claim(block)?;
            }
        }
    }
    for group in 0..groups {
        if has_super(group) {
            let start = first + group * u64::from(geometry.blocks_per_group) + 1 + gdt_blocks;
            for block in start..start + reserved {
                check.owned[block as usize / 8] &= !(1 << (block % 8));
            }
        }
    }
    // The resize inode owns reserved GDT blocks legitimately. Walk it
    // first, rejecting duplicates within it, then reserve any remaining
    // GDT blocks before any other inode can claim them.
    let resize = le32(sb, 0x5c) & 0x10 != 0;
    if resize {
        if inodes < 7 || !bit_set(&descriptors[6 / ipg].inodes, 6 % ipg) {
            return Err(Invalid("resize inode is free"));
        }
        let off = descriptors[6 / ipg].inode_table * bs as u64 + (6 % ipg * isz) as u64;
        let mut raw = [0; 128];
        (check.read)(off, &mut raw).map_err(Read)?;
        check.inode(&raw)?;
    }
    for group in 0..groups {
        if has_super(group) {
            let start = first + group * u64::from(geometry.blocks_per_group) + 1 + gdt_blocks;
            for block in start..start + reserved {
                if block >= blocks || !bit_set(&check.owned, block as usize) {
                    check.claim(block)?;
                }
            }
        }
    }
    // The internal journal is an ordinary allocated inode in this pass.
    if le32(sb, 0x5c) & 4 != 0 {
        let ino = u64::from(le32(sb, 0xe0));
        if ino == 0 || ino > inodes || !bit_set(&descriptors[(ino as usize - 1) / ipg].inodes, (ino as usize - 1) % ipg)
        {
            return Err(Invalid("journal inode is free"));
        }
    }
    let mut table = vec![0; bs];
    for (group, g) in descriptors.iter().enumerate() {
        for index in (0..g.inode_count).step_by(bs / isz) {
            check.read_block(g.inode_table + (index * isz / bs) as u64, &mut table)?;
            for slot in 0..(bs / isz).min(g.inode_count - index) {
                let ino = group * ipg + index + slot + 1;
                if bit_set(&g.inodes, index + slot) && !(resize && ino == 7) {
                    check.inode(&table[slot * isz..slot * isz + 128])?;
                }
            }
        }
    }
    let leaked_blocks = (first..blocks)
        .filter(|&b| bit_set(&check.allocated, b as usize) && !bit_set(&check.owned, b as usize))
        .count() as u64;
    Ok(AllocationReport { leaked_blocks })
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    #[test]
    fn allocation_bitmap_count_matches_bit_model() {
        for byte in 0..=255 {
            let bitmap = [byte, !byte];
            for bits in 0..=16 {
                let model = (0..bits).filter(|i| bitmap[i / 8] & (1 << (i % 8)) == 0).count() as u64;
                assert_eq!(bitmap_free_count(&bitmap, bits), Some(model));
            }
            assert_eq!(bitmap_free_count(&bitmap, 17), None);
        }
        assert_eq!(bitmap_free_count(&[], 0), Some(0));
    }

    #[test]
    fn allocation_walk_includes_extent_nodes_and_unwritten_blocks() {
        let tree = root(1, &[index(0, 20)]);
        let leaf = extent_node(BS, 84, 0, &[extent(0, 2, 30), extent(7, 32770, 40)]);
        let mut seen = Vec::new();
        visit_inode_blocks(&tree, true, BS, |b, buf| {
            seen.push(b);
            if let Some(buf) = buf {
                assert_eq!(b, 20);
                buf.copy_from_slice(&leaf);
            }
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(seen, [20, 30, 31, 40, 41]);
        let mut count = 0;
        visit_inode_blocks(&root(0, &[extent(0, 32768, 100)]), true, BS, |_, _| {
            count += 1;
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(count, 32768);
    }

    #[test]
    fn allocation_walk_includes_all_indirect_levels_and_rejects_cycles() {
        let mut words = [0; 15];
        words[11] = 10;
        words[12] = 20;
        words[13] = 30;
        words[14] = 40;
        let maps = [(20, 21), (30, 31), (31, 32), (40, 41), (41, 42), (42, 43)];
        let mut seen = Vec::new();
        visit_inode_blocks(&block_map(words), false, BS, |b, buf| {
            seen.push(b);
            if let Some(buf) = buf {
                let next = maps.iter().find(|(at, _)| *at == b).unwrap().1;
                buf.copy_from_slice(&indirect(&[next]));
            }
            Ok::<_, ()>(())
        })
        .unwrap();
        assert_eq!(seen, [10, 20, 21, 30, 31, 32, 40, 41, 42, 43]);
        let mut reads = 0;
        let mut seen = Vec::new();
        let cyclic = root(1, &[index(0, 20), index(1, 20)]);
        let err = visit_inode_blocks(&cyclic, true, BS, |b, buf| {
            if seen.contains(&b) {
                return Err("duplicate");
            }
            seen.push(b);
            if let Some(buf) = buf {
                reads += 1;
                buf.copy_from_slice(&extent_node(BS, 84, 0, &[]));
            }
            Ok(())
        });
        assert_eq!(err, Err(BlockMapError::Read("duplicate")));
        assert_eq!(reads, 1);
        assert_eq!(
            visit_inode_blocks(&root(1, &[index(0, 20)]), true, BS, |_, _| Err("io")),
            Err(BlockMapError::Read("io"))
        );
        assert_eq!(visit_inode_blocks(&root(6, &[]), true, BS, |_, _| Ok::<_, ()>(())), Err(BlockMapError::Corrupt));
    }

    fn put16(img: &mut [u8], at: usize, value: u16) {
        img[at..at + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn put32(img: &mut [u8], at: usize, value: u32) {
        img[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    // One partial 1 KiB group: superblock, GDT, two bitmaps, one inode
    // table block and root data. Only inode 2 is in use.
    fn allocation_image() -> Vec<u8> {
        let mut img = vec![0; 32 * BS];
        let sb = &mut img[BS..2 * BS];
        put32(sb, 0, 8);
        put32(sb, 4, 32);
        put32(sb, 0xc, 25);
        put32(sb, 0x10, 7);
        put32(sb, 0x14, 1);
        put32(sb, 0x20, 32);
        put32(sb, 0x28, 8);
        put16(sb, 0x58, 128);
        put32(&mut img, 2 * BS, 3);
        put32(&mut img, 2 * BS + 4, 4);
        put32(&mut img, 2 * BS + 8, 5);
        put16(&mut img, 2 * BS + 0xc, 25);
        put16(&mut img, 2 * BS + 0xe, 7);
        img[3 * BS] = 0x3f;
        img[4 * BS] = 2;
        put16(&mut img, 5 * BS + 128, 0x4000);
        put32(&mut img, 5 * BS + 128 + 0x28, 6);
        img
    }

    fn checked(img: &[u8]) -> Result<AllocationReport, AllocationError<()>> {
        check_allocation(img[BS..2 * BS].try_into().unwrap(), |off, buf| {
            let src = img.get(off as usize..off as usize + buf.len()).ok_or(())?;
            buf.copy_from_slice(src);
            Ok(())
        })
    }

    #[test]
    fn allocation_counts_check_both_bitmaps_and_both_superblock_totals_first() {
        let img = allocation_image();
        assert_eq!(checked(&img), Ok(AllocationReport { leaked_blocks: 0 }));
        for (at, wide, bitmap, group, actual) in [
            (2 * BS + 0xc, false, "block", Some(0), 25),
            (2 * BS + 0xe, false, "inode", Some(0), 7),
            (BS + 0xc, true, "block", None, 25),
            (BS + 0x10, true, "inode", None, 7),
        ] {
            for delta in [-1, 1] {
                let mut bad = img.clone();
                let recorded = (actual as i64 + delta) as u64;
                if wide {
                    put32(&mut bad, at, recorded as u32);
                } else {
                    put16(&mut bad, at, recorded as u16);
                }
                let result = check_allocation(bad[BS..2 * BS].try_into().unwrap(), |off, buf| {
                    assert!(off < (5 * BS) as u64, "count failure must precede inode table reads");
                    buf.copy_from_slice(&bad[off as usize..off as usize + buf.len()]);
                    Ok::<_, ()>(())
                });
                assert_eq!(result, Err(AllocationError::Count { group, bitmap, recorded, actual }));
            }
        }
    }

    #[test]
    fn allocation_ownership_rejects_free_duplicate_and_fixed_metadata_aliases() {
        let img = allocation_image();
        for (block, problem) in
            [(7, "is mapped but free"), (6, "is mapped twice"), (3, "is mapped twice"), (32, "is outside the volume")]
        {
            let mut bad = img.clone();
            // A second pointer in the same inode must count, even past EOF.
            put32(&mut bad, 5 * BS + 128 + 0x2c, block);
            assert_eq!(checked(&bad), Err(AllocationError::Block { block: u64::from(block), problem }));
        }
        let mut leaked = img.clone();
        leaked[3 * BS] |= 0x40;
        put16(&mut leaked, 2 * BS + 0xc, 24);
        put32(&mut leaked, BS + 0xc, 24);
        assert_eq!(checked(&leaked), Ok(AllocationReport { leaked_blocks: 1 }));
        // Allocated special inodes' rdev is not a block map; external
        // xattrs still are owned and must have an allocated bit.
        put16(&mut leaked, 5 * BS + 128, 0x2000);
        put32(&mut leaked, 5 * BS + 128 + 0x28, u32::MAX);
        put32(&mut leaked, 5 * BS + 128 + 0x68, 7);
        assert_eq!(checked(&leaked), Ok(AllocationReport { leaked_blocks: 1 }));
        put32(&mut leaked, 5 * BS + 128 + 0x68, 8);
        assert_eq!(checked(&leaked), Err(AllocationError::Block { block: 8, problem: "is mapped but free" }));
    }

    #[test]
    fn allocation_reserved_gdt_belongs_only_to_resize_inode() {
        let mut img = allocation_image();
        // Insert one reserved GDT block before the bitmaps and table.
        img.copy_within(3 * BS..7 * BS, 4 * BS);
        put32(&mut img, 2 * BS, 4);
        put32(&mut img, 2 * BS + 4, 5);
        put32(&mut img, 2 * BS + 8, 6);
        img[4 * BS] = 0x7f;
        img[5 * BS] = 2 | 0x40;
        put32(&mut img, BS + 0xc, 24);
        put16(&mut img, 2 * BS + 0xc, 24);
        put32(&mut img, BS + 0x10, 6);
        put16(&mut img, 2 * BS + 0xe, 6);
        put32(&mut img, BS + 0x5c, 0x10);
        put16(&mut img, BS + 0xce, 1);
        put32(&mut img, 6 * BS + 128 + 0x28, 7);
        put16(&mut img, 6 * BS + 6 * 128, 0x8000);
        put32(&mut img, 6 * BS + 6 * 128 + 0x28, 3);
        assert_eq!(checked(&img), Ok(AllocationReport { leaked_blocks: 0 }));
        let mut bad = img.clone();
        put32(&mut bad, 6 * BS + 128 + 0x2c, 3);
        assert_eq!(checked(&bad), Err(AllocationError::Block { block: 3, problem: "is mapped twice" }));
        let mut bad = img.clone();
        put32(&mut bad, 6 * BS + 6 * 128 + 0x2c, 3);
        assert_eq!(checked(&bad), Err(AllocationError::Block { block: 3, problem: "is mapped twice" }));
        // A bitmap placed in reserved GDT space is never a resize alias.
        put32(&mut img, 2 * BS, 3);
        img.copy_within(4 * BS..5 * BS, 3 * BS);
        assert_eq!(checked(&img), Err(AllocationError::Block { block: 3, problem: "is mapped twice" }));
    }

    #[test]
    fn allocation_metadata_read_errors_and_invalid_maps_fail_closed() {
        let img = allocation_image();
        for fail_at in [2 * BS, 3 * BS, 4 * BS, 5 * BS] {
            let result = check_allocation(img[BS..2 * BS].try_into().unwrap(), |off, buf| {
                if off == fail_at as u64 {
                    return Err(fail_at);
                }
                buf.copy_from_slice(&img[off as usize..off as usize + buf.len()]);
                Ok(())
            });
            assert_eq!(result, Err(AllocationError::Read(fail_at)));
        }
        let mut bad = img.clone();
        put32(&mut bad, 5 * BS + 128 + 0x20, 0x80000);
        assert_eq!(checked(&bad), Err(AllocationError::Invalid("inode block map")));
        // An extent tree node is owned even when its leaf has no data.
        let tree = root(1, &[index(0, 7)]);
        bad[5 * BS + 128 + 0x28..5 * BS + 128 + 0x64].copy_from_slice(&tree);
        assert_eq!(checked(&bad), Err(AllocationError::Block { block: 7, problem: "is mapped but free" }));
        bad[3 * BS] |= 0x40;
        put16(&mut bad, 2 * BS + 0xc, 24);
        put32(&mut bad, BS + 0xc, 24);
        bad[7 * BS..8 * BS].copy_from_slice(&extent_node(BS, 84, 0, &[]));
        assert_eq!(checked(&bad), Ok(AllocationReport { leaked_blocks: 1 }));
        // Two leaf entries sharing a map node fail before rereading it.
        let tree = root(1, &[index(0, 7), index(1, 7)]);
        bad[5 * BS + 128 + 0x28..5 * BS + 128 + 0x64].copy_from_slice(&tree);
        assert_eq!(checked(&bad), Err(AllocationError::Block { block: 7, problem: "is mapped twice" }));
    }

    /// What `ext4plus`'s setters write for a `Duration` (its
    /// `duration_to_timestamp`), base word and extra word.
    fn ext4plus_words(d: Duration) -> (u32, u32) {
        let secs = d.as_secs();
        (secs as u32, ((secs >> 32) as u32 & 3) | (d.subsec_nanos() << 2))
    }

    /// What its getters return for the two words (its `timestamp_to_duration`).
    fn ext4plus_read(base: u32, extra: u32) -> Duration {
        Duration::new((u64::from(extra & 3) << 32) | u64::from(base), extra >> 2)
    }

    /// Linux's `ext4_encode_extra_time` and the base word, from `ext4.h`.
    fn linux_words(ts: Timespec) -> (u32, u32) {
        let extra = (((ts.sec - i64::from(ts.sec as i32)) >> 32) as u32 & 3) | ((ts.nsec as u32) << 2);
        (ts.sec as u32, extra)
    }

    /// Linux's `ext4_decode_extra_time`.
    fn linux_decode(base: u32, extra: u32) -> Timespec {
        let mut sec = i64::from(base as i32);
        if extra & 3 != 0 {
            sec += i64::from(extra & 3) << 32;
        }
        Timespec { sec, nsec: i64::from(extra >> 2) }
    }

    fn ts(sec: i64, nsec: i64) -> Timespec {
        Timespec { sec, nsec }
    }

    #[test]
    fn device_numbers_decode_as_linux_does() {
        // Old encoding: 1:3 (`/dev/null`) is 0x0103.
        assert_eq!(rdev_from_i_block(0x0103, 0), DevId::makedev(1, 3));
        assert_eq!(rdev_from_i_block(0x0103, 0xdead_beef), DevId::makedev(1, 3));
        // Only the low 16 bits of i_block[0] count, but all 32 choose the
        // old encoding.
        assert_eq!(rdev_from_i_block(0xffff_0801, 0), DevId::makedev(8, 1));
        assert_eq!(rdev_from_i_block(0xffff_0000, 0x0801), DevId::makedev(0, 0));
        assert_eq!(rdev_from_i_block(0xffff, 0), DevId::makedev(255, 255));
        // New encoding: major 259, minor 300 (`new_encode_dev`: minor's low
        // byte, major << 8, the rest of minor << 12).
        let new = (300 & 0xff) | (259 << 8) | ((300 & !0xff) << 12);
        assert_eq!(rdev_from_i_block(0, new), DevId::makedev(259, 300));
        assert_eq!(rdev_from_i_block(0, 0), DevId::makedev(0, 0));
        // The widest the new encoding holds: 12-bit major, 20-bit minor.
        assert_eq!(rdev_from_i_block(0, 0xffff_ffff), DevId::makedev(0xfff, 0xfffff));
        assert_eq!(new_decode_dev(0x0010_0000), DevId::makedev(0, 0x100));
    }

    /// The epoch table in Linux's `fs/ext4/ext4.h`: each row's first and last
    /// second, with the extra word's epoch bits and the base word's top bit.
    #[test]
    fn the_epoch_table_round_trips() {
        let rows: [(i64, i64, u32, u32); 8] = [
            (-0x8000_0000, -0x0000_0001, 0, 1),
            (0x0_0000_0000, 0x0_7fff_ffff, 0, 0),
            (0x0_8000_0000, 0x0_ffff_ffff, 1, 1),
            (0x1_0000_0000, 0x1_7fff_ffff, 1, 0),
            (0x1_8000_0000, 0x1_ffff_ffff, 2, 1),
            (0x2_0000_0000, 0x2_7fff_ffff, 2, 0),
            (0x2_8000_0000, 0x2_ffff_ffff, 3, 1),
            (0x3_0000_0000, 0x3_7fff_ffff, 3, 0),
        ];
        for (first, last, epoch, msb) in rows {
            for sec in [first, first + 1, last - 1, last] {
                let t = ts(sec, 123_456_789);
                let d = time_to_disk(t, true);
                let (base, extra) = ext4plus_words(d);
                assert_eq!((base, extra), linux_words(t), "{sec:#x}");
                assert_eq!(extra & 3, epoch, "{sec:#x}");
                assert_eq!(base >> 31, msb, "{sec:#x}");
                assert_eq!(linux_decode(base, extra), t, "{sec:#x}");
                assert_eq!(time_from_disk(ext4plus_read(base, extra)), t, "{sec:#x}");
                assert_eq!(time_from_disk(d), t, "{sec:#x}");
            }
        }
    }

    #[test]
    fn times_near_the_epoch() {
        for t in [ts(0, 0), ts(0, 1), ts(0, 999_999_999), ts(-1, 0), ts(-1, 999_999_999), ts(1, 5)] {
            let d = time_to_disk(t, true);
            assert_eq!(ext4plus_words(d), linux_words(t), "{t:?}");
            assert_eq!(time_from_disk(d), t, "{t:?}");
        }
        // -1 is 0xffffffff with epoch 0, which ext4plus alone reads as 2106.
        assert_eq!(ext4plus_words(time_to_disk(ts(-1, 0), true)), (0xffff_ffff, 0));
        assert_eq!(time_from_disk(Duration::from_secs(0xffff_ffff)), ts(-1, 0));
    }

    /// Linux's `timestamp_truncate` (current master: nanoseconds go only when
    /// the clamp changed the second) for one filesystem's range.
    #[test]
    fn out_of_range_times_clamp_as_timestamp_truncate_does() {
        let at_min = ts(TIME_MIN, 0);
        let at_max = ts(TIME_MAX_EXTRA, 0);
        assert_eq!(truncate_time(ts(TIME_MIN - 1, 5), true), at_min);
        assert_eq!(truncate_time(ts(i64::MIN, 5), true), at_min);
        assert_eq!(truncate_time(ts(TIME_MIN, 5), true), ts(TIME_MIN, 5));
        assert_eq!(truncate_time(ts(TIME_MIN + 1, 5), true), ts(TIME_MIN + 1, 5));
        assert_eq!(truncate_time(ts(TIME_MAX_EXTRA + 1, 5), true), at_max);
        assert_eq!(truncate_time(ts(i64::MAX, 5), true), at_max);
        assert_eq!(truncate_time(ts(TIME_MAX_EXTRA, 5), true), ts(TIME_MAX_EXTRA, 5));
        assert_eq!(truncate_time(ts(TIME_MAX_EXTRA - 1, 5), true), ts(TIME_MAX_EXTRA - 1, 5));
        for t in [ts(i64::MIN, 0), ts(i64::MAX, 999_999_999)] {
            let stored = truncate_time(t, true);
            assert_eq!(time_from_disk(time_to_disk(t, true)), stored);
            assert_eq!(ext4plus_words(time_to_disk(t, true)), linux_words(stored));
        }
    }

    #[test]
    fn without_the_extra_fields_a_time_is_whole_signed_32_bit_seconds() {
        assert_eq!(truncate_time(ts(5, 7), false), ts(5, 0));
        assert_eq!(truncate_time(ts(-5, 7), false), ts(-5, 0));
        assert_eq!(truncate_time(ts(TIME_MAX_NO_EXTRA + 1, 7), false), ts(TIME_MAX_NO_EXTRA, 0));
        assert_eq!(truncate_time(ts(TIME_MIN - 1, 7), false), ts(TIME_MIN, 0));
        // ext4plus then writes only the base word (an extra word of 0 if the
        // inode has one), and reads it back as the same time.
        for t in [ts(-5, 7), ts(TIME_MAX_NO_EXTRA + 9, 0), ts(1_700_000_000, 3)] {
            let d = time_to_disk(t, false);
            assert_eq!(d.subsec_nanos(), 0);
            assert!(d.as_secs() <= u64::from(u32::MAX));
            assert_eq!(time_from_disk(Duration::from_secs(d.as_secs())), truncate_time(t, false));
        }
    }

    const BS: usize = 1024;

    /// An extent node: its header (`max` entries of room) and 12-byte
    /// entries, padded to `len` bytes.
    fn extent_node(len: usize, max: u16, depth: u16, entries: &[[u8; 12]]) -> Vec<u8> {
        let mut node = vec![0u8; len];
        node[0..2].copy_from_slice(&EXTENT_MAGIC.to_le_bytes());
        node[2..4].copy_from_slice(&(entries.len() as u16).to_le_bytes());
        node[4..6].copy_from_slice(&max.to_le_bytes());
        node[6..8].copy_from_slice(&depth.to_le_bytes());
        for (i, e) in entries.iter().enumerate() {
            node[12 + i * 12..24 + i * 12].copy_from_slice(e);
        }
        node
    }

    /// A leaf entry: `len` blocks (`ee_len` as stored) from `start`.
    fn extent(logical: u32, len: u16, start: u64) -> [u8; 12] {
        let mut e = [0u8; 12];
        e[0..4].copy_from_slice(&logical.to_le_bytes());
        e[4..6].copy_from_slice(&len.to_le_bytes());
        e[6..8].copy_from_slice(&((start >> 32) as u16).to_le_bytes());
        e[8..12].copy_from_slice(&(start as u32).to_le_bytes());
        e
    }

    /// An index entry pointing at `child`.
    fn index(logical: u32, child: u64) -> [u8; 12] {
        let mut e = [0u8; 12];
        e[0..4].copy_from_slice(&logical.to_le_bytes());
        e[4..8].copy_from_slice(&(child as u32).to_le_bytes());
        e[8..10].copy_from_slice(&((child >> 32) as u16).to_le_bytes());
        e
    }

    fn root(depth: u16, entries: &[[u8; 12]]) -> [u8; 60] {
        extent_node(60, 4, depth, entries).try_into().unwrap()
    }

    /// `first_block_freed` over `blocks` (block number, bytes); the blocks
    /// it read, in order.
    fn first_with(
        i_block: &[u8; 60],
        extents: bool,
        blocks: &[(u64, Vec<u8>)],
    ) -> (Result<Option<u64>, BlockMapError<u64>>, Vec<u64>) {
        let mut read = Vec::new();
        let res = first_block_freed(i_block, extents, BS, |b, buf: &mut [u8]| {
            read.push(b);
            let (_, bytes) = blocks.iter().find(|(n, _)| *n == b).ok_or(b)?;
            buf.copy_from_slice(bytes);
            Ok(())
        });
        (res, read)
    }

    #[test]
    fn an_extent_roots_first_extent_with_blocks_is_freed_first() {
        let only = root(0, &[extent(0, 2, 1234), extent(2, 1, 99)]);
        assert_eq!(first_with(&only, true, &[]), (Ok(Some(1234)), vec![]));
        // No start or no length: skipped, as `free_freed_ranges` does.
        let skips = root(0, &[extent(0, 0, 50), extent(1, 3, 0), extent(4, 1, 77)]);
        assert_eq!(first_with(&skips, true, &[]).0, Ok(Some(77)));
        // An unwritten extent (ee_len past 32768) holds ee_len - 32768
        // blocks; 32768 itself is a written one of 32768.
        assert_eq!(first_with(&root(0, &[extent(0, 32768, 5)]), true, &[]).0, Ok(Some(5)));
        assert_eq!(first_with(&root(0, &[extent(0, 32769, 6)]), true, &[]).0, Ok(Some(6)));
        // A start past 32 bits (ee_start_hi).
        let high = (7u64 << 32) | 0x8000_0001;
        assert_eq!(first_with(&root(0, &[extent(0, 1, high)]), true, &[]).0, Ok(Some(high)));
        // A full root (4 entries of 4: `ext4plus` builds one for 4 extents).
        let full = root(0, &[extent(0, 1, 40), extent(2, 1, 41), extent(4, 1, 42), extent(6, 1, 43)]);
        assert_eq!(first_with(&full, true, &[]).0, Ok(Some(40)));
        // Nothing mapped, or only extents that free nothing: none (not a
        // leaf entry read as an index).
        assert_eq!(first_with(&root(0, &[]), true, &[]), (Ok(None), vec![]));
        assert_eq!(first_with(&root(0, &[extent(0, 0, 50), extent(1, 3, 0)]), true, &[]).0, Ok(None));
    }

    #[test]
    fn a_deeper_extent_tree_is_walked_leftmost_first_to_its_first_extent() {
        let leaf = |extents: &[[u8; 12]]| extent_node(BS, 84, 0, extents);
        let blocks = [
            (500, leaf(&[])),
            (501, leaf(&[extent(0, 4, 3000), extent(4, 4, 2000)])),
            (502, leaf(&[extent(8, 1, 1000)])),
        ];
        let tree = root(1, &[index(0, 500), index(0, 501), index(8, 502)]);
        // The empty leaf passed, the next leaf's first extent: in file
        // order, not the lowest block number.
        assert_eq!(first_with(&tree, true, &blocks), (Ok(Some(3000)), vec![500, 501]));
        // Two levels (ei_leaf_hi used too).
        let mid = (1u64 << 32) | 600;
        let two = [(mid, extent_node(BS, 84, 1, &[index(0, 502)])), (502, blocks[2].1.clone())];
        assert_eq!(first_with(&root(2, &[index(0, mid)]), true, &two), (Ok(Some(1000)), vec![mid, 502]));
        // No data under the root: its first index block, freed first of the
        // map blocks.
        let empty = [(500, leaf(&[])), (501, leaf(&[]))];
        assert_eq!(first_with(&root(1, &[index(0, 500), index(9, 501)]), true, &empty).0, Ok(Some(500)));
        // A root with no index entries: none (not block 0).
        assert_eq!(first_with(&root(1, &[]), true, &[]), (Ok(None), vec![]));
        // A full leaf (84 entries of 84 in a 1 KiB block).
        let many: Vec<[u8; 12]> = (0..84).map(|i| extent(i * 2, 1, 7000 + u64::from(i))).collect();
        let full = [(500, leaf(&many))];
        assert_eq!(first_with(&root(1, &[index(0, 500)]), true, &full).0, Ok(Some(7000)));
        // Five levels below the root, the deepest allowed.
        let chain: Vec<(u64, Vec<u8>)> = (0..5u16)
            .map(|d| {
                let depth = 4 - d;
                let node = if depth == 0 {
                    leaf(&[extent(0, 1, 9999)])
                } else {
                    extent_node(BS, 84, depth, &[index(0, 801 + u64::from(d))])
                };
                (800 + u64::from(d), node)
            })
            .collect();
        let deep = first_with(&root(5, &[index(0, 800)]), true, &chain);
        assert_eq!(deep, (Ok(Some(9999)), vec![800, 801, 802, 803, 804]));
    }

    #[test]
    fn a_bad_extent_tree_or_a_failed_read_names_no_block() {
        let mut bad_magic = root(0, &[extent(0, 1, 9)]);
        bad_magic[0] = 0;
        assert_eq!(first_with(&bad_magic, true, &[]).0, Err(BlockMapError::Corrupt));
        // More entries than room, room past the node, too deep.
        let mut over = root(0, &[extent(0, 1, 9)]);
        over[2..4].copy_from_slice(&5u16.to_le_bytes());
        assert_eq!(first_with(&over, true, &[]).0, Err(BlockMapError::Corrupt));
        let roomy: [u8; 60] = extent_node(60, 5, 0, &[]).try_into().unwrap();
        assert_eq!(first_with(&roomy, true, &[]).0, Err(BlockMapError::Corrupt));
        assert_eq!(first_with(&root(6, &[]), true, &[]).0, Err(BlockMapError::Corrupt));
        // A child at the wrong depth.
        let wrong = [(500, extent_node(BS, 84, 1, &[]))];
        assert_eq!(first_with(&root(1, &[index(0, 500)]), true, &wrong).0, Err(BlockMapError::Corrupt));
        // A read that fails.
        assert_eq!(first_with(&root(1, &[index(0, 500)]), true, &[]).0, Err(BlockMapError::Read(500)));
    }

    /// `words` little-endian, padded with zeros to `len` bytes.
    fn words_le(words: &[u32], len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        for (i, w) in words.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        out
    }

    /// A block map's `i_block`: 12 direct blocks, then the single, double and
    /// triple indirect ones.
    fn block_map(words: [u32; 15]) -> [u8; 60] {
        words_le(&words, 60).try_into().unwrap()
    }

    fn indirect(entries: &[u32]) -> Vec<u8> {
        words_le(entries, BS)
    }

    #[test]
    fn a_block_maps_first_data_block_is_freed_first() {
        let mut words = [0u32; 15];
        words[2] = 77;
        words[5] = 33;
        words[12] = 400;
        assert_eq!(first_with(&block_map(words), false, &[]), (Ok(Some(77)), vec![]));
        let mut last_direct = [0u32; 15];
        last_direct[11] = 78;
        assert_eq!(first_with(&block_map(last_direct), false, &[]).0, Ok(Some(78)));
        // No direct block: the single indirect's first, then the double's,
        // each in file order (an indirect block's first data entry, a
        // double's first child that leads to data).
        let mut sparse = [0u32; 15];
        sparse[12] = 400;
        sparse[13] = 401;
        let blocks = [
            (400, indirect(&[0, 0])),
            (401, indirect(&[0, 402, 404])),
            (402, indirect(&[0, 0, 55, 66])),
            (403, indirect(&[])),
            (404, indirect(&[77])),
        ];
        assert_eq!(first_with(&block_map(sparse), false, &blocks), (Ok(Some(55)), vec![400, 401, 402]));
        let mut single = [0u32; 15];
        single[12] = 402;
        assert_eq!(first_with(&block_map(single), false, &blocks).0, Ok(Some(55)));
        // Triple.
        let mut triple = [0u32; 15];
        triple[14] = 405;
        let deep = [(405, indirect(&[406])), (406, indirect(&[0, 402])), (402, indirect(&[0, 0, 55]))];
        assert_eq!(first_with(&block_map(triple), false, &deep).0, Ok(Some(55)));
        // No data block: the first indirect block (single before double);
        // nothing at all: none.
        let mut bare = [0u32; 15];
        bare[13] = 403;
        assert_eq!(first_with(&block_map(bare), false, &blocks).0, Ok(Some(403)));
        bare[12] = 400;
        assert_eq!(first_with(&block_map(bare), false, &blocks).0, Ok(Some(400)));
        assert_eq!(first_with(&block_map([0; 15]), false, &[]), (Ok(None), vec![]));
    }

    #[test]
    fn a_map_with_too_many_empty_blocks_to_read_names_no_block() {
        // A double indirect block listing more empty single ones than the
        // reads allowed.
        let mut words = [0u32; 15];
        words[13] = 900;
        let children: Vec<u32> = (901..901 + MAP_READS_MAX as u32).collect();
        let mut blocks = vec![(900, indirect(&children))];
        blocks.extend(children.iter().map(|&c| (u64::from(c), indirect(&[]))));
        let (res, read) = first_with(&block_map(words), false, &blocks);
        assert_eq!(res, Err(BlockMapError::TooManyReads));
        assert_eq!(read.len(), MAP_READS_MAX);
        // One fewer empty child fits, and the next has data.
        let fits: Vec<u32> = (901..900 + MAP_READS_MAX as u32).collect();
        let mut blocks = vec![(900, indirect(&fits))];
        blocks.extend(fits.iter().map(|&c| (u64::from(c), indirect(&[]))));
        blocks.last_mut().unwrap().1 = indirect(&[0, 4242]);
        assert_eq!(first_with(&block_map(words), false, &blocks).0, Ok(Some(4242)));
    }

    /// A superblock's bytes with the geometry fields set: `log` block size,
    /// `count` blocks (its high half only on a 64-bit volume), groups of
    /// `per_group` from `first`. Clusters per group (0x24) is set to
    /// something else, so a parse reading it shows. (The feature bits are
    /// written as numbers, not the code's constants.)
    fn superblock(log: u32, count: u64, first: u32, per_group: u32, is_64bit: bool) -> [u8; 1024] {
        let mut sb = [0u8; 1024];
        let mut put = |at: usize, v: u32| sb[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(0x04, count as u32);
        put(0x14, first);
        put(0x18, log);
        put(0x20, per_group);
        put(0x24, per_group / 2 + 1);
        // Incompat flex_bg (0x200, mke2fs's default; bigalloc's value in
        // ro_compat), so a parse reading bigalloc from incompat shows.
        put(0x60, 0x200 | if is_64bit { 0x80 } else { 0 });
        put(0x150, (count >> 32) as u32);
        sb
    }

    /// [`superblock`] of a bigalloc volume: `per_group` blocks, `clusters`
    /// clusters to a group.
    fn bigalloc(log: u32, count: u64, per_group: u32, clusters: u32) -> [u8; 1024] {
        let mut sb = superblock(log, count, 0, per_group, false);
        sb[0x24..0x28].copy_from_slice(&clusters.to_le_bytes());
        sb[0x64..0x68].copy_from_slice(&0x200u32.to_le_bytes());
        sb
    }

    fn bit(group: u32, byte: u32, mask: u8) -> Option<BitmapBit> {
        Some(BitmapBit { group, byte, mask })
    }

    #[test]
    fn a_blocks_bitmap_bit_is_its_place_in_its_group() {
        // 1 KiB blocks: the groups start at block 1, 8192 to a group.
        let g = Geometry::from_superblock(&superblock(0, 65536, 1, 8192, false)).unwrap();
        assert_eq!(g.block_size(), 1024);
        assert!(!g.is_64bit());
        assert_eq!(g.bitmap_bit(1), bit(0, 0, 1));
        assert_eq!(g.bitmap_bit(10), bit(0, 1, 2));
        assert_eq!(g.bitmap_bit(8192), bit(0, 1023, 0x80));
        assert_eq!(g.bitmap_bit(8193), bit(1, 0, 1));
        assert_eq!(g.bitmap_bit(0), None);
        // 4 KiB blocks: from block 0, 32768 to a group; past the volume.
        let g = Geometry::from_superblock(&superblock(2, 65536, 0, 32768, false)).unwrap();
        assert_eq!(g.bitmap_bit(32768 + 17), bit(1, 2, 2));
        assert_eq!(g.bitmap_bit(65535), bit(1, 4095, 0x80));
        assert_eq!(g.bitmap_bit(65536), None);
        // Smaller groups than a bitmap holds.
        let g = Geometry::from_superblock(&superblock(0, 4096, 1, 1000, false)).unwrap();
        assert_eq!(g.bitmap_bit(1001), bit(1, 0, 1));
        assert_eq!(g.bitmap_bit(1000), bit(0, 124, 0x80));
    }

    #[test]
    fn a_superblocks_geometry_is_read_and_checked() {
        use GeometryError::*;
        // The high half of the count only on a 64-bit volume.
        let wide = (1u64 << 32) + 100;
        let g = Geometry::from_superblock(&superblock(2, wide, 0, 32768, true)).unwrap();
        assert!(g.is_64bit());
        assert_eq!(g.block_byte(wide - 1), Some((wide - 1) * 4096));
        assert_eq!(g.block_byte(wide), None);
        let narrow = Geometry::from_superblock(&superblock(2, wide, 0, 32768, false)).unwrap();
        assert_eq!(narrow.block_byte(100), None);
        assert_eq!(narrow.block_byte(99), Some(99 * 4096));
        // Block 0 is never a map's or a bitmap's.
        assert_eq!(narrow.block_byte(0), None);
        // A group with more blocks than its bitmap block has bits, or none.
        let over = Geometry::from_superblock(&superblock(0, 65536, 1, 8193, false));
        assert_eq!(over, Err(BlocksPerGroup { value: 8193, max: 8192 }));
        assert!(Geometry::from_superblock(&superblock(0, 65536, 1, 8192, false)).is_ok());
        let none = Geometry::from_superblock(&superblock(0, 65536, 1, 0, false));
        assert_eq!(none, Err(BlocksPerGroup { value: 0, max: 8192 }));
        // The first data block on the volume; a block size up to 64 KiB.
        let first = Geometry::from_superblock(&superblock(0, 1, 1, 8192, false));
        assert_eq!(first, Err(FirstDataBlock { first: 1, count: 1 }));
        assert!(Geometry::from_superblock(&superblock(6, 16, 0, 8192, false)).is_ok());
        assert_eq!(Geometry::from_superblock(&superblock(7, 16, 0, 8192, false)), Err(BlockSize { log: 7 }));
    }

    #[test]
    fn a_bigalloc_volume_bounds_its_clusters_and_places_no_block_bit() {
        use GeometryError::*;
        // `mke2fs -O bigalloc -b 4096 -C 65536`: 524288 blocks, 32768
        // clusters to a group. Its bitmaps count clusters, so no block's
        // bit is placed.
        let g = Geometry::from_superblock(&bigalloc(2, 1 << 20, 524_288, 32768)).unwrap();
        assert_eq!(g.bitmap_bit(5), None);
        assert_eq!(g.block_byte(5), Some(5 * 4096));
        // Clusters bounded by the bitmap block instead; blocks still not 0.
        let over = Geometry::from_superblock(&bigalloc(2, 1 << 20, 524_288, 32769));
        assert_eq!(over, Err(ClustersPerGroup { value: 32769, max: 32768 }));
        assert_eq!(
            Geometry::from_superblock(&bigalloc(2, 1 << 20, 524_288, 0)),
            Err(ClustersPerGroup { value: 0, max: 32768 })
        );
        assert!(matches!(
            Geometry::from_superblock(&bigalloc(2, 1 << 20, 0, 32768)),
            Err(BlocksPerGroup { value: 0, .. })
        ));
        // The first data block on the volume there too.
        let first = Geometry::from_superblock(&bigalloc(2, 0, 524_288, 32768));
        assert_eq!(first, Err(FirstDataBlock { first: 0, count: 0 }));
        // Each refusal names its field and value.
        let says = |e: GeometryError| alloc::format!("{e}");
        assert_eq!(says(BlocksPerGroup { value: 8193, max: 8192 }), "s_blocks_per_group 8193 not in 1..=8192");
        assert_eq!(says(ClustersPerGroup { value: 0, max: 32768 }), "s_clusters_per_group 0 not in 1..=32768");
        assert_eq!(says(FirstDataBlock { first: 1, count: 1 }), "s_first_data_block 1 not below s_blocks_count 1");
        assert_eq!(says(BlockSize { log: 7 }), "s_log_block_size 7 > 6");
    }
}
