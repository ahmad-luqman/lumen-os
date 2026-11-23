//! ext4's on-disk encodings that the kernel's adapter encodes and decodes
//! itself, because `ext4plus` doesn't: a device node's number in `i_block`,
//! timestamps before 1970 or past 2038 (`ext4plus` reads the low 32 bits
//! of a time as unsigned), and the first block `ext4plus`'s `delete_file`
//! frees of an inode, with where that block's bit is in the block bitmap
//! (`ext4plus` keeps its block maps and bitmaps to itself; #330).
//!
//! The first two follow Linux (`fs/ext4/inode.c`'s `ext4_iget`,
//! `fs/ext4/ext4.h`'s `ext4_encode_extra_time`/`ext4_decode_extra_time`,
//! `include/linux/kdev_t.h`); the extent and block-map layouts are ext4's
//! (`fs/ext4/ext4_extents.h`, `fs/ext4/indirect.c`).

use alloc::vec;
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
/// its group's bitmap block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    block_size: u64,
    first_data_block: u32,
    blocks_per_group: u32,
    blocks_count: u64,
}

/// Where a block's bit is: its group, the byte in that group's block bitmap
/// (below the block size), and the bit's mask in that byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BitmapBit {
    pub group: u32,
    pub byte: u32,
    pub mask: u8,
}

/// The superblock's `INCOMPAT_64BIT` feature: `s_blocks_count_hi` counts.
const INCOMPAT_64BIT: u32 = 0x80;

impl Geometry {
    /// The geometry in the superblock's 1024 bytes: `s_blocks_count_lo`
    /// (0x04, with `s_blocks_count_hi` at 0x150 on a 64-bit volume),
    /// `s_first_data_block` (0x14), `s_log_block_size` (0x18) and
    /// `s_blocks_per_group` (0x20). `None` unless the block size is 1 to
    /// 64 KiB, a group has 1 to 8 × block-size blocks (one bitmap block's
    /// bits, as ext4 allows) and the first data block is on the volume.
    pub fn from_superblock(sb: &[u8; 1024]) -> Option<Self> {
        let log = le32(sb, 0x18);
        if log > 6 {
            return None;
        }
        let block_size = 1024u64 << log;
        let first_data_block = le32(sb, 0x14);
        let blocks_per_group = le32(sb, 0x20);
        let hi = if le32(sb, 0x60) & INCOMPAT_64BIT != 0 { le32(sb, 0x150) } else { 0 };
        let blocks_count = (u64::from(hi) << 32) | u64::from(le32(sb, 0x04));
        let fits = blocks_per_group != 0 && u64::from(blocks_per_group) <= 8 * block_size;
        (fits && u64::from(first_data_block) < blocks_count).then_some(Self {
            block_size,
            first_data_block,
            blocks_per_group,
            blocks_count,
        })
    }

    pub fn block_size(&self) -> u64 {
        self.block_size
    }

    /// Block `block`'s first byte on the device; `None` for block 0 (the
    /// superblock's, never a map's or bitmap's) or past the volume, which
    /// `ext4plus`'s block reads refuse too.
    pub fn block_byte(&self, block: u64) -> Option<u64> {
        if block == 0 || block >= self.blocks_count {
            return None;
        }
        block.checked_mul(self.block_size)
    }

    /// Where block `block`'s bit is in its group's block bitmap, as
    /// `ext4plus`'s `block_block_group_location` and `BitmapHandle` place it;
    /// `None` for a block outside the groups.
    pub fn bitmap_bit(&self, block: u64) -> Option<BitmapBit> {
        let rel = block.checked_sub(u64::from(self.first_data_block))?;
        if block >= self.blocks_count {
            return None;
        }
        let group = u32::try_from(rel / u64::from(self.blocks_per_group)).ok()?;
        let bit = (rel % u64::from(self.blocks_per_group)) as u32;
        Some(BitmapBit { group, byte: bit / 8, mask: 1 << (bit % 8) })
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

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
    /// something else, so a parse reading it shows.
    fn superblock(log: u32, count: u64, first: u32, per_group: u32, is_64bit: bool) -> [u8; 1024] {
        let mut sb = [0u8; 1024];
        let mut put = |at: usize, v: u32| sb[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(0x04, count as u32);
        put(0x14, first);
        put(0x18, log);
        put(0x20, per_group);
        put(0x24, per_group / 2 + 1);
        put(0x60, if is_64bit { INCOMPAT_64BIT } else { 0 });
        put(0x150, (count >> 32) as u32);
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
        // The high half of the count only on a 64-bit volume.
        let wide = (1u64 << 32) + 100;
        let g = Geometry::from_superblock(&superblock(2, wide, 0, 32768, true)).unwrap();
        assert_eq!(g.block_byte(wide - 1), Some((wide - 1) * 4096));
        assert_eq!(g.block_byte(wide), None);
        let narrow = Geometry::from_superblock(&superblock(2, wide, 0, 32768, false)).unwrap();
        assert_eq!(narrow.block_byte(100), None);
        assert_eq!(narrow.block_byte(99), Some(99 * 4096));
        // Block 0 is never a map's or a bitmap's.
        assert_eq!(narrow.block_byte(0), None);
        // A group with more blocks than its bitmap block has bits, or none.
        assert_eq!(Geometry::from_superblock(&superblock(0, 65536, 1, 8193, false)), None);
        assert!(Geometry::from_superblock(&superblock(0, 65536, 1, 8192, false)).is_some());
        assert_eq!(Geometry::from_superblock(&superblock(0, 65536, 1, 0, false)), None);
        // The first data block on the volume; a block size up to 64 KiB.
        assert_eq!(Geometry::from_superblock(&superblock(0, 1, 1, 8192, false)), None);
        assert!(Geometry::from_superblock(&superblock(6, 16, 0, 8192, false)).is_some());
        assert_eq!(Geometry::from_superblock(&superblock(7, 16, 0, 8192, false)), None);
    }
}
