//! `no_std` block device abstraction, 4 KiB LRU block cache, and in-memory
//! test device for Lumen OS.

#![no_std]

extern crate alloc;

#[cfg(test)]
extern crate std;

mod cache;
mod mem;
#[cfg(test)]
mod tests;

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::fmt;

pub use cache::{BlockCache, BlockCacheStats, PinnedBlock, PinnedBlockMut};
pub use mem::MemBlockDevice;

/// Standard logical sector size in bytes (virtio-blk baseline).
pub const SECTOR_SIZE: usize = 512;

/// Block cache unit size in bytes (4 KiB = 8 × 512-byte sectors).
pub const BLOCK_SIZE: usize = 4096;

/// Default block cache capacity in 4 KiB blocks (1024 blocks = 4 MiB).
pub const DEFAULT_CAPACITY: usize = 1024;

/// Static geometry and capability descriptor of a [`BlockDevice`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockInfo {
    /// Logical sector size in bytes (`512` for virtio-blk baseline).
    pub sector_size: u32,
    /// Total device capacity in logical sectors (`> 0`).
    pub sector_count: u64,
    /// Whether writes are rejected (`VIRTIO_BLK_F_RO`).
    pub read_only: bool,
    /// Whether the device advertises explicit cache flush (`VIRTIO_BLK_F_FLUSH`).
    pub flush_supported: bool,
}

/// Errors returned by [`BlockDevice`] and [`BlockCache`] operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockError {
    /// `lba + sectors > sector_count`, byte range past end of device, or overflow.
    OutOfRange,
    /// Buffer length is zero or not a multiple of `sector_size`, or invalid geometry.
    Unaligned,
    /// Attempted write on a read-only device or cache.
    ReadOnly,
    /// No evictable victim slot: every slot pinned, or (writeback stopped)
    /// every unpinned slot dirty.
    CacheFull,
    /// Device returned an I/O error, unsupported status, or malformed completion.
    Io,
    /// Bounded wait for device completion expired.
    Timeout,
    /// Device was retired after a prior fatal error.
    DeviceFailed,
}

impl fmt::Display for BlockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BlockError::OutOfRange => f.write_str("block address out of range"),
            BlockError::Unaligned => f.write_str("unaligned or empty block transfer"),
            BlockError::ReadOnly => f.write_str("block device is read-only"),
            BlockError::CacheFull => f.write_str("block cache full (no evictable slot)"),
            BlockError::Io => f.write_str("block device I/O error"),
            BlockError::Timeout => f.write_str("block device request timed out"),
            BlockError::DeviceFailed => f.write_str("block device failed and was retired"),
        }
    }
}

impl core::error::Error for BlockError {}

/// Synchronous, blocking sector-level block device interface.
///
/// Callers always run in preemptible contexts with interrupts enabled (never
/// holding an IRQ-off spinlock).
pub trait BlockDevice: Send + Sync {
    /// Static device geometry and capability flags.
    fn info(&self) -> BlockInfo;
    /// Read `buf.len() / sector_size` sectors starting at `lba`.
    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError>;
    /// Write `buf.len() / sector_size` sectors starting at `lba`.
    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError>;
    /// Flush volatile device write caches to durable storage.
    fn flush(&self) -> Result<(), BlockError>;
}

impl<T: BlockDevice + ?Sized> BlockDevice for Arc<T> {
    fn info(&self) -> BlockInfo {
        (**self).info()
    }

    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        (**self).read_sectors(lba, buf)
    }

    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        (**self).write_sectors(lba, buf)
    }

    fn flush(&self) -> Result<(), BlockError> {
        (**self).flush()
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for Box<T> {
    fn info(&self) -> BlockInfo {
        (**self).info()
    }

    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        (**self).read_sectors(lba, buf)
    }

    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        (**self).write_sectors(lba, buf)
    }

    fn flush(&self) -> Result<(), BlockError> {
        (**self).flush()
    }
}

impl<T: BlockDevice + ?Sized> BlockDevice for &T {
    fn info(&self) -> BlockInfo {
        (**self).info()
    }

    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        (**self).read_sectors(lba, buf)
    }

    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError> {
        (**self).write_sectors(lba, buf)
    }

    fn flush(&self) -> Result<(), BlockError> {
        (**self).flush()
    }
}
