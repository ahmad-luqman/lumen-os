use alloc::boxed::Box;
use alloc::format;
use alloc::sync::Arc;
use alloc::vec;

use crate::{BLOCK_SIZE, BlockCache, BlockDevice, BlockError, BlockInfo, MemBlockDevice, SECTOR_SIZE};

struct MockInfoDevice(BlockInfo);

impl BlockDevice for MockInfoDevice {
    fn info(&self) -> BlockInfo {
        self.0
    }

    fn read_sectors(&self, _lba: u64, _buf: &mut [u8]) -> Result<(), BlockError> {
        Ok(())
    }

    fn write_sectors(&self, _lba: u64, _buf: &[u8]) -> Result<(), BlockError> {
        Ok(())
    }

    fn flush(&self) -> Result<(), BlockError> {
        Ok(())
    }
}

#[test]
fn new_validates_geometry_and_capacity() {
    for bad_sector in [0, 256, 513, 1000, 8192] {
        let dev = MockInfoDevice(BlockInfo {
            sector_size: bad_sector,
            sector_count: 16,
            read_only: false,
            flush_supported: true,
        });
        assert_eq!(BlockCache::new(dev, 4).err(), Some(BlockError::Unaligned));
    }

    let zero_sectors = MockInfoDevice(BlockInfo {
        sector_size: SECTOR_SIZE as u32,
        sector_count: 0,
        read_only: false,
        flush_supported: true,
    });
    assert_eq!(BlockCache::new(zero_sectors, 4).err(), Some(BlockError::OutOfRange));

    let overflow_sectors = MockInfoDevice(BlockInfo {
        sector_size: 512,
        sector_count: u64::MAX / 256,
        read_only: false,
        flush_supported: true,
    });
    assert_eq!(BlockCache::new(overflow_sectors, 4).err(), Some(BlockError::OutOfRange));

    let zero_cap = MemBlockDevice::new(512, 16);
    assert_eq!(BlockCache::new(zero_cap, 0).err(), Some(BlockError::CacheFull));

    for valid_sector in [512, 1024, 2048, 4096] {
        let dev = MemBlockDevice::new(valid_sector, 16);
        let cache = BlockCache::new(dev, 2).expect("valid geometry");
        assert_eq!(cache.info().sector_size, valid_sector);
    }
}

#[test]
fn blanket_impls_arc_box_ref_and_error_display() {
    let dev = Arc::new(MemBlockDevice::new(512, 16));
    let c1 = BlockCache::new(dev.clone(), 2).unwrap();
    c1.write_at(0, &[0xAB; 16]).unwrap();
    c1.sync().unwrap();

    let boxed: Box<dyn BlockDevice> = Box::new(MemBlockDevice::new(512, 16));
    let c2 = BlockCache::new(boxed, 2).unwrap();
    assert_eq!(c2.info().sector_count, 16);

    let borrowed: &MemBlockDevice = &dev;
    let c3 = BlockCache::new(borrowed, 2).unwrap();
    let mut out = [0u8; 16];
    c3.read_at(0, &mut out).unwrap();
    assert_eq!(out, [0xAB; 16]);

    for err in [
        BlockError::OutOfRange,
        BlockError::Unaligned,
        BlockError::ReadOnly,
        BlockError::CacheFull,
        BlockError::Io,
        BlockError::Timeout,
        BlockError::DeviceFailed,
    ] {
        assert!(!format!("{err}").is_empty());
    }
}

#[test]
fn lru_eviction_order_and_stats() {
    let dev = MemBlockDevice::new(512, 64); // 8 blocks of 4 KiB
    for b in 0..8u8 {
        dev.seed(usize::from(b) * BLOCK_SIZE, &[b + 1; 8]);
    }

    let cache = BlockCache::new(dev, 3).unwrap();
    assert_eq!(cache.stats().capacity, 3);
    assert_eq!(cache.stats().cached, 0);

    // Load blocks 0, 1, 2 (LRU tail -> 0, 1, 2 <- MRU head).
    for id in 0..3 {
        let p = cache.pin(id).unwrap();
        assert_eq!(p[0], id as u8 + 1);
    }
    assert_eq!(cache.stats().cached, 3);
    assert_eq!(cache.stats().misses, 3);
    assert_eq!(cache.stats().hits, 0);

    // Touch block 0 -> LRU becomes 1 (oldest), then 2, then 0 (newest).
    assert_eq!(cache.pin(0).unwrap()[0], 1);
    assert_eq!(cache.stats().hits, 1);

    // Miss on block 3 -> must evict block 1 (NOT block 0).
    assert_eq!(cache.pin(3).unwrap()[0], 4);
    assert_eq!(cache.stats().evictions, 1);
    assert_eq!(cache.stats().misses, 4);

    // Block 0 and 2 should still hit, block 1 should miss (evicting 2? wait: 0 hit -> 2 hit -> order 3, 0, 2 -> evicts 3).
    cache.device().clear_logs();
    assert_eq!(cache.pin(0).unwrap()[0], 1);
    assert_eq!(cache.pin(2).unwrap()[0], 3);
    assert!(cache.device().read_log().is_empty());

    // Now block 3 is LRU -> requesting 1 evicts 3.
    assert_eq!(cache.pin(1).unwrap()[0], 2);
    assert_eq!(cache.device().read_log(), vec![(8, BLOCK_SIZE)]);
    assert_eq!(cache.stats().evictions, 2);
}

#[test]
fn raii_pins_prevent_eviction_and_return_cache_full() {
    let dev = MemBlockDevice::new(512, 32); // 4 blocks
    let cache = BlockCache::new(dev, 2).unwrap();

    {
        let p0 = cache.pin(0).unwrap();
        let p0_again = cache.pin(0).unwrap();
        assert_eq!(p0[0], p0_again[0]);
        let p1 = cache.pin(1).unwrap();
        assert_eq!(cache.stats().pinned, 2);

        // All slots pinned -> pinning block 2 must return CacheFull.
        assert_eq!(cache.pin(2).err(), Some(BlockError::CacheFull));
        assert_eq!(cache.pin_mut(2).err(), Some(BlockError::CacheFull));

        // Dropping one of the two pins on block 0 keeps block 0 pinned.
        drop(p0_again);
        assert_eq!(cache.pin(2).err(), Some(BlockError::CacheFull));

        // Dropping p0 makes block 0 evictable while p1 stays pinned.
        drop(p0);
        assert_eq!(cache.stats().pinned, 1);
        let p2 = cache.pin(2).unwrap();
        assert_eq!(p1[0], 0);
        assert_eq!(p2[0], 0);
        assert_eq!(cache.stats().evictions, 1);
    }
    assert_eq!(cache.stats().pinned, 0);

    // Test PinnedBlockMut marks dirty only on mutable deref / mark_dirty and unpins on drop.
    {
        let mut m = cache.pin_mut(3).unwrap();
        m[0] = 0x77;
    }
    assert_eq!(cache.stats().dirty, 1);
    assert_eq!(cache.stats().pinned, 0);
    cache.sync().unwrap();
    assert_eq!(cache.stats().dirty, 0);
    assert_eq!(cache.device().durable_snapshot()[3 * BLOCK_SIZE], 0x77);
}

#[test]
fn read_only_guard_rejects_before_mutating_cache_or_device() {
    let dev = MemBlockDevice::with_options(512, 16, true, true);
    dev.seed(0, &[0x42; 16]);
    let cache = BlockCache::new(dev, 2).unwrap();

    assert_eq!(cache.write_at(0, &[0x99; 4]), Err(BlockError::ReadOnly));
    assert_eq!(cache.pin_mut(0).err(), Some(BlockError::ReadOnly));
    assert_eq!(cache.write_at(0, &[]), Err(BlockError::ReadOnly));
    assert_eq!(cache.stats().cached, 0);
    assert_eq!(cache.stats().dirty, 0);
    assert!(cache.device().read_log().is_empty());
    assert!(cache.device().write_log().is_empty());

    let mut out = [0u8; 16];
    cache.read_at(0, &mut out).unwrap();
    assert_eq!(out, [0x42; 16]);
    assert_eq!(cache.pin(0).unwrap()[0], 0x42);
}

#[test]
fn partial_tail_block_clamping_bounds_and_zero_len() {
    // 11 sectors * 512 = 5632 bytes -> block 0: 8 sectors (4096 B), block 1: 3 sectors (1536 B).
    let dev = MemBlockDevice::new(512, 11);
    dev.seed(4096, &[0xCC; 1536]);
    let cache = BlockCache::new(dev, 2).unwrap();

    // len == 0 is a no-op with zero device calls.
    cache.read_at(0, &mut []).unwrap();
    cache.write_at(5632, &[]).unwrap();
    assert!(cache.device().read_log().is_empty());
    assert!(cache.device().write_log().is_empty());

    // Out-of-range and overflow checks happen before mutating block 0.
    assert_eq!(cache.read_at(5632, &mut [0u8; 1]), Err(BlockError::OutOfRange));
    assert_eq!(cache.read_at(u64::MAX, &mut [0u8; 1]), Err(BlockError::OutOfRange));
    assert_eq!(cache.write_at(4090, &[0xEE; 1543]), Err(BlockError::OutOfRange));
    assert_eq!(cache.write_at(u64::MAX - 2, &[0xEE; 4]), Err(BlockError::OutOfRange));
    assert_eq!(cache.stats().cached, 0);
    assert_eq!(cache.stats().dirty, 0);

    // Partial tail read clamps to 3 sectors = 1536 bytes.
    let mut tail = [0u8; 1536];
    cache.read_at(4096, &mut tail).unwrap();
    assert_eq!(tail, [0xCC; 1536]);
    assert_eq!(cache.device().read_log(), vec![(8, 1536)]);

    // Partial tail write + sync clamps writeback to 3 sectors = 1536 bytes.
    cache.device().clear_logs();
    cache.write_at(5120, &[0x55; 512]).unwrap();
    cache.sync().unwrap();
    assert_eq!(cache.device().write_log(), vec![(8, 1536)]);
    assert_eq!(&cache.device().durable_snapshot()[4096..5120], &[0xCC; 1024]);
    assert_eq!(&cache.device().durable_snapshot()[5120..5632], &[0x55; 512]);
}

#[test]
fn ext4_unaligned_and_cross_block_rw() {
    // Exercise 1 KiB, 2 KiB, and unaligned cross-block operations (e.g., ext4 superblock at 1024).
    let dev = MemBlockDevice::new(512, 32); // 4 blocks = 16 KiB
    let cache = BlockCache::new(dev, 2).unwrap();

    let sb = vec![0xA1u8; 1024];
    cache.write_at(1024, &sb).unwrap();

    let cross = vec![0xB2u8; 2048];
    // Spans block 0 (3072..4096) and block 1 (0..1024).
    cache.write_at(3072, &cross).unwrap();

    // Full block 2 write (skip_read path) and block 3 partial write (forces LRU eviction of blocks 0/1).
    let full = vec![0xC3u8; 4096];
    cache.write_at(8192, &full).unwrap();
    cache.write_at(12288 + 512, &[0xD4u8; 1024]).unwrap();

    cache.sync().unwrap();

    let mut got_sb = vec![0u8; 1024];
    cache.read_at(1024, &mut got_sb).unwrap();
    assert_eq!(got_sb, sb);

    let mut got_cross = vec![0u8; 2048];
    cache.read_at(3072, &mut got_cross).unwrap();
    assert_eq!(got_cross, cross);

    let mut got_full = vec![0u8; 4096];
    cache.read_at(8192, &mut got_full).unwrap();
    assert_eq!(got_full, full);

    let mut got_b3 = vec![0u8; 1024];
    cache.read_at(12288 + 512, &mut got_b3).unwrap();
    assert_eq!(got_b3, vec![0xD4u8; 1024]);
}

#[test]
fn writeback_ascending_order_and_crash_consistency_with_needs_device_flush() {
    let dev = MemBlockDevice::new(512, 40); // 5 blocks
    let cache = BlockCache::new(dev, 2).unwrap();

    // Dirty blocks 4, 1, 3 -> evicts 4 and 1 to volatile, leaves 3 dirty in cache.
    cache.write_at(4 * BLOCK_SIZE as u64, &[0x44; 64]).unwrap();
    cache.write_at(BLOCK_SIZE as u64, &[0x11; 64]).unwrap();
    assert!(cache.stats().dirty == 2 && !cache.needs_device_flush());

    cache.write_at(3 * BLOCK_SIZE as u64, &[0x33; 64]).unwrap();
    // Block 4 was LRU dirty -> written back to volatile, needs_device_flush = true!
    assert!(cache.needs_device_flush());
    assert_eq!(cache.device().flush_calls(), 0);

    // Dirty block 0 -> evicts block 1 to volatile; read clean blocks 2 and 4 -> evicts blocks 3 and 0!
    cache.write_at(0, &[0x00; 64]).unwrap();
    let _ = cache.pin(2).unwrap();
    let _ = cache.pin(4).unwrap();
    // Now 0 dirty blocks remain in cache, yet needs_device_flush() MUST still be true!
    assert_eq!(cache.stats().dirty, 0);
    assert!(cache.needs_device_flush());

    // Power loss before sync() discards volatile writes (durable is still zeros).
    cache.device().simulate_power_loss();
    assert_eq!(cache.device().durable_snapshot()[BLOCK_SIZE], 0);
    assert_eq!(cache.device().volatile_snapshot()[BLOCK_SIZE], 0);

    // Write blocks 3, 0, 2 (cap=3 on a fresh cache) and test ascending writeback order in sync() & flush_blocks().
    let dev2 = MemBlockDevice::new(512, 40);
    let cache2 = BlockCache::new(dev2, 4).unwrap();
    cache2.write_at(3 * BLOCK_SIZE as u64, &[0x33; 64]).unwrap();
    cache2.write_at(0, &[0xAA; 64]).unwrap();
    cache2.write_at(2 * BLOCK_SIZE as u64, &[0x22; 64]).unwrap();
    cache2.write_at(BLOCK_SIZE as u64, &[0x11; 64]).unwrap();

    // flush_blocks([3, 1, 3]) must write 1 then 3 (ascending, deduped), and call flush().
    cache2.flush_blocks(&[3, 1, 3]).unwrap();
    assert_eq!(cache2.device().write_log(), vec![(8, BLOCK_SIZE), (24, BLOCK_SIZE)]);
    assert_eq!(cache2.device().flush_calls(), 1);
    assert!(!cache2.needs_device_flush());
    assert_eq!(cache2.stats().dirty, 2); // blocks 0 and 2 still dirty

    cache2.device().clear_logs();
    cache2.sync().unwrap();
    assert_eq!(cache2.device().write_log(), vec![(0, BLOCK_SIZE), (16, BLOCK_SIZE)]);
    assert_eq!(cache2.device().flush_calls(), 1);
    assert!(!cache2.needs_device_flush());

    // Now test sync() when 0 dirty blocks remain in cache after dirty LRU eviction!
    let dev3 = MemBlockDevice::new(512, 24); // 3 blocks
    let cache3 = BlockCache::new(dev3, 1).unwrap();
    cache3.write_at(0, &[0x7A; 32]).unwrap();
    let mut tmp = [0u8; 32];
    cache3.read_at(BLOCK_SIZE as u64, &mut tmp).unwrap(); // evicts dirty block 0 -> volatile
    assert_eq!(cache3.stats().dirty, 0);
    assert!(cache3.needs_device_flush());
    assert_eq!(cache3.device().flush_calls(), 0);

    // sync() with 0 dirty blocks MUST call device.flush() and clear needs_device_flush!
    cache3.sync().unwrap();
    assert_eq!(cache3.device().flush_calls(), 1);
    assert!(!cache3.needs_device_flush());
    cache3.device().simulate_power_loss();
    assert_eq!(&cache3.device().volatile_snapshot()[..32], &[0x7A; 32]);

    // Also verify flush_supported == false still routes flush() through BlockDevice::flush().
    let dev_no_flush = MemBlockDevice::with_options(512, 16, false, false);
    let cache_nf = BlockCache::new(dev_no_flush, 2).unwrap();
    cache_nf.write_at(0, &[0x5E; 16]).unwrap();
    cache_nf.sync().unwrap();
    assert_eq!(cache_nf.device().flush_calls(), 1);
    assert!(!cache_nf.needs_device_flush());
}

#[test]
fn injected_io_errors_preserve_dirty_state_and_recover_cleanly() {
    // 1. Writeback failure on dirty eviction keeps victim dirty and cached.
    let dev = MemBlockDevice::new(512, 24);
    let cache = BlockCache::new(dev, 1).unwrap();
    cache.write_at(0, &[0x12; 16]).unwrap();
    cache.device().inject_write_error(BlockError::Io);
    assert_eq!(cache.pin(1).err(), Some(BlockError::Io));
    assert_eq!(cache.stats().dirty, 1);
    assert_eq!(cache.stats().cached, 1);
    assert!(!cache.needs_device_flush());
    let mut verify = [0u8; 16];
    cache.read_at(0, &mut verify).unwrap();
    assert_eq!(verify, [0x12; 16]);

    // 2. Clean eviction + read_sectors failure leaves no poisoned entry cached.
    cache.sync().unwrap();
    assert_eq!(cache.stats().dirty, 0);
    cache.device().inject_read_error(BlockError::Io);
    assert_eq!(cache.pin(1).err(), Some(BlockError::Io));
    assert_eq!(cache.stats().cached, 0);
    assert_eq!(cache.stats().evictions, 1);

    // 3. Mid-sync write failure (fail 2nd of 3 dirty writes): block 0 clean, 1 & 2 dirty, needs_device_flush true.
    let dev2 = MemBlockDevice::new(512, 32);
    let cache2 = BlockCache::new(dev2, 4).unwrap();
    cache2.write_at(0, &[0xA0; 8]).unwrap();
    cache2.write_at(BLOCK_SIZE as u64, &[0xB0; 8]).unwrap();
    cache2.write_at(2 * BLOCK_SIZE as u64, &[0xC0; 8]).unwrap();
    cache2.device().inject_write_error_after(1, BlockError::Io);
    assert_eq!(cache2.sync(), Err(BlockError::Io));
    assert_eq!(cache2.stats().dirty, 2);
    assert!(cache2.needs_device_flush());
    assert_eq!(cache2.device().flush_calls(), 0);

    // Retry sync: writes only blocks 1 and 2, then flushes.
    cache2.device().clear_logs();
    cache2.sync().unwrap();
    assert_eq!(cache2.device().write_log(), vec![(8, BLOCK_SIZE), (16, BLOCK_SIZE)]);
    assert_eq!(cache2.device().flush_calls(), 1);
    assert_eq!(cache2.stats().dirty, 0);
    assert!(!cache2.needs_device_flush());

    // 4. flush() failure during sync(): dirty blocks are clean, needs_device_flush stays true; retry flushes with 0 writes.
    cache2.write_at(0, &[0xDD; 8]).unwrap();
    cache2.device().inject_flush_error(BlockError::Io);
    assert_eq!(cache2.sync(), Err(BlockError::Io));
    assert_eq!(cache2.stats().dirty, 0);
    assert!(cache2.needs_device_flush());

    cache2.device().clear_logs();
    cache2.sync().unwrap();
    assert!(cache2.device().write_log().is_empty());
    assert_eq!(cache2.device().flush_calls(), 1);
    assert!(!cache2.needs_device_flush());
}

/// #331: a device write fails only when the cache writes a block back, and
/// every such failure reaches a caller, never reported as success: the
/// `write_at` or `read_at` whose miss needed the room (for another block's
/// failed write), `flush_blocks` and `sync`. The failed block stays dirty and
/// cached, the missed block's bytes are cached nowhere (a write spanning
/// blocks would keep its earlier blocks' bytes, dirty), and the next
/// writeback writes the block. (`sync` and `flush_blocks` stop at the first
/// block that fails, leaving the later ones dirty and the device unflushed:
/// `injected_io_errors_preserve_dirty_state_and_recover_cleanly`.)
#[test]
fn every_failed_device_write_reaches_a_caller_and_keeps_its_block_dirty() {
    let dev = MemBlockDevice::new(512, 32); // 4 blocks
    let cache = BlockCache::new(dev, 1).unwrap();
    cache.write_at(0, &[0x12; 16]).unwrap();
    assert!(cache.device().write_log().is_empty()); // written back later

    // A write needing block 0's slot: block 0's writeback fails, so this
    // write fails too, and its bytes are cached nowhere.
    cache.device().inject_write_error(BlockError::Io);
    assert_eq!(cache.write_at(BLOCK_SIZE as u64, &[0x34; 16]), Err(BlockError::Io));
    // A read needing the slot gets the failed write's error.
    cache.device().inject_write_error(BlockError::Timeout);
    let mut buf = [0u8; 16];
    assert_eq!(cache.read_at(2 * BLOCK_SIZE as u64, &mut buf), Err(BlockError::Timeout));
    // So do `flush_blocks` and `sync`, which write back block 0 itself.
    cache.device().inject_write_error(BlockError::DeviceFailed);
    assert_eq!(cache.flush_blocks(&[0]), Err(BlockError::DeviceFailed));
    cache.device().inject_write_error(BlockError::Io);
    assert_eq!(cache.sync(), Err(BlockError::Io));
    let stats = cache.stats();
    assert_eq!((stats.cached, stats.dirty, stats.writebacks), (1, 1, 0));
    // Each failed writeback counted, the eviction's (a read's too) and the
    // syncs': how a caller tells a read's error from another block's (#349).
    assert_eq!((cache.writeback_failures(), stats.writeback_failures), (4, 4));
    assert_eq!(cache.device().flush_calls(), 0);
    assert_eq!(cache.device().volatile_snapshot()[..16], [0; 16]);
    assert_eq!(cache.device().volatile_snapshot()[BLOCK_SIZE..BLOCK_SIZE + 16], [0; 16]);

    // The device works again: the next sync writes block 0 and flushes.
    cache.sync().unwrap();
    assert_eq!(cache.stats().dirty, 0);
    assert_eq!(cache.device().durable_snapshot()[..16], [0x12; 16]);
    cache.read_at(0, &mut buf).unwrap();
    assert_eq!(buf, [0x12; 16]);
}

/// The eviction victim is always the least recently used unpinned block:
/// while its writeback fails, every miss that needs an eviction fails, reads
/// included, even with a clean block that could make room. Pinned for #349,
/// whose read-only volume must keep reads working once it stops writing back,
/// and which is expected to change this.
#[test]
fn a_failing_dirty_lru_victim_fails_every_miss_even_with_a_clean_slot() {
    let cache = BlockCache::new(MemBlockDevice::new(512, 32), 2).unwrap();
    let mut buf = [0u8; 16];
    cache.write_at(0, &[0x56; 16]).unwrap(); // dirty, least recently used
    cache.read_at(BLOCK_SIZE as u64, &mut buf).unwrap(); // clean
    for block in [2u64, 3] {
        cache.device().inject_write_error(BlockError::Io);
        assert_eq!(cache.read_at(block * BLOCK_SIZE as u64, &mut buf), Err(BlockError::Io));
    }
    assert_eq!(cache.stats().cached, 2);
    assert_eq!(cache.writeback_failures(), 2);
    // Frozen (a filesystem's errors=remount-ro, #349), it skips the dirty
    // block and evicts the clean one: the misses are served.
    cache.device().clear_logs();
    cache.freeze();
    cache.device().inject_write_error(BlockError::Io);
    for block in [2u64, 3] {
        cache.read_at(block * BLOCK_SIZE as u64, &mut buf).unwrap();
    }
    assert_eq!(cache.writeback_failures(), 2);
    assert!(cache.device().write_log().is_empty());
}

#[test]
fn pinned_mut_deref_marks_dirty_immediately_and_blocks_sync_until_dropped() {
    let dev = MemBlockDevice::new(512, 32); // 4 blocks
    let cache = BlockCache::new(dev, 4).unwrap();

    // Dirty block 1 first so we also cover mixed unpinned-dirty + pinned-dirty sync.
    cache.write_at(BLOCK_SIZE as u64, &[0x11; 8]).unwrap();
    assert_eq!(cache.stats().dirty, 1);

    let mut p = cache.pin_mut(0).unwrap();
    // Before DerefMut / mark_dirty, block 0 is still clean.
    assert_eq!(cache.stats().dirty, 1);

    // Mutably dereferencing marks block 0 dirty immediately.
    p[0] = 0xAB;
    assert_eq!(cache.stats().dirty, 2);

    // While `p` is still alive, both `sync()` and `flush_blocks(&[0])` must fail with CacheFull
    // and must NOT persist partial data or flush.
    assert_eq!(cache.sync(), Err(BlockError::CacheFull));
    assert_eq!(cache.flush_blocks(&[0]), Err(BlockError::CacheFull));
    assert_eq!(cache.flush_blocks(&[0, 1]), Err(BlockError::CacheFull));
    assert!(cache.device().write_log().is_empty());
    assert_eq!(cache.device().flush_calls(), 0);
    assert_eq!(cache.device().durable_snapshot()[0], 0);
    assert_eq!(cache.stats().dirty, 2);

    // Flushing an unrelated block (block 1) still succeeds.
    cache.flush_blocks(&[1]).unwrap();
    assert_eq!(cache.device().durable_snapshot()[BLOCK_SIZE], 0x11);
    assert_eq!(cache.stats().dirty, 1);

    drop(p);
    assert_eq!(cache.stats().pinned, 0);
    assert_eq!(cache.stats().dirty, 1);

    cache.sync().unwrap();
    assert_eq!(cache.stats().dirty, 0);
    assert_eq!(cache.device().durable_snapshot()[0], 0xAB);
}

#[test]
fn pinned_mut_mark_dirty_immediately_blocks_flush_and_sync_until_dropped() {
    let dev = MemBlockDevice::new(512, 16); // 2 blocks
    let cache = BlockCache::new(dev, 2).unwrap();

    let mut p = cache.pin_mut(0).unwrap();
    assert_eq!(cache.stats().dirty, 0);

    // Call `mark_dirty()` directly without `DerefMut`.
    p.mark_dirty();
    assert_eq!(cache.stats().dirty, 1);
    assert_eq!(cache.sync(), Err(BlockError::CacheFull));
    assert_eq!(cache.flush_blocks(&[0]), Err(BlockError::CacheFull));
    assert!(cache.device().write_log().is_empty());
    assert_eq!(cache.device().flush_calls(), 0);

    // Calling `mark_dirty()` repeatedly is idempotent.
    p.mark_dirty();
    assert_eq!(cache.stats().dirty, 1);

    drop(p);
    assert_eq!(cache.stats().pinned, 0);
    assert_eq!(cache.stats().dirty, 1);

    cache.flush_blocks(&[0]).unwrap();
    assert_eq!(cache.stats().dirty, 0);
    assert_eq!(cache.device().write_log(), vec![(0, BLOCK_SIZE)]);
    assert_eq!(cache.device().flush_calls(), 1);
}

#[test]
fn unmodified_pin_mut_stays_clean_and_allows_sync() {
    let dev = MemBlockDevice::new(512, 16); // 2 blocks
    let cache = BlockCache::new(dev, 2).unwrap();

    {
        let p = cache.pin_mut(0).unwrap();
        // Reading through `Deref` (no `DerefMut` or `mark_dirty`) keeps the slot clean.
        assert_eq!(p[0], 0);
        assert_eq!(cache.stats().dirty, 0);
        assert_eq!(cache.stats().pinned, 1);
        cache.sync().unwrap();
        cache.flush_blocks(&[0]).unwrap();
        assert!(cache.device().write_log().is_empty());
        assert_eq!(cache.device().flush_calls(), 0);
    }
    assert_eq!(cache.stats().dirty, 0);
    assert_eq!(cache.stats().pinned, 0);

    // An untouched `pin_mut` on an ALREADY-dirty block must still block sync/flush with CacheFull.
    cache.write_at(0, &[0x55; 4]).unwrap();
    assert_eq!(cache.stats().dirty, 1);
    {
        let p = cache.pin_mut(0).unwrap();
        assert_eq!(p[0], 0x55);
        assert_eq!(cache.sync(), Err(BlockError::CacheFull));
        assert_eq!(cache.flush_blocks(&[0]), Err(BlockError::CacheFull));
    }
    cache.sync().unwrap();
    assert_eq!(cache.stats().dirty, 0);
    assert_eq!(cache.device().durable_snapshot()[0], 0x55);

    // By contrast, a shared read `pin` on a dirty block still allows `sync()` to flush.
    cache.write_at(0, &[0x66; 4]).unwrap();
    {
        let r = cache.pin(0).unwrap();
        assert_eq!(r[0], 0x66);
        cache.sync().unwrap();
        assert_eq!(cache.device().durable_snapshot()[0], 0x66);
    }
}

#[test]
fn a_frozen_cache_writes_nothing_and_keeps_its_dirty_blocks() {
    let dev = MemBlockDevice::new(512, 64); // 8 blocks of 4 KiB
    for b in 0..8u8 {
        dev.seed(usize::from(b) * BLOCK_SIZE, &[b + 1; 8]);
    }
    let cache = BlockCache::new(dev, 3).unwrap();
    // Block 0 dirty, blocks 1 and 2 clean (LRU tail: 0, then 1, then 2).
    cache.write_at(0, &[0xaa; 4]).unwrap();
    cache.pin(1).unwrap();
    cache.pin(2).unwrap();
    // One write before the freeze lands on a sync, as ever.
    cache.write_at(BLOCK_SIZE as u64, &[0xbb; 4]).unwrap();
    cache.flush_blocks(&[1]).unwrap();
    cache.device().clear_logs();
    let before = cache.device().volatile_snapshot();

    cache.freeze();
    assert!(cache.is_frozen());
    assert_eq!(cache.write_at(2 * BLOCK_SIZE as u64, &[0xcc; 4]), Err(BlockError::ReadOnly));
    assert_eq!(cache.pin_mut(2).err(), Some(BlockError::ReadOnly));
    assert_eq!(cache.sync(), Err(BlockError::ReadOnly));
    assert_eq!(cache.flush_blocks(&[0]), Err(BlockError::ReadOnly));
    // Misses evict the clean blocks, never the dirty one, which still reads
    // back as written.
    for id in 3..8u64 {
        assert_eq!(cache.pin(id).unwrap()[0], id as u8 + 1);
    }
    let mut got = [0u8; 4];
    cache.read_at(0, &mut got).unwrap();
    assert_eq!(got, [0xaa; 4]);
    assert_eq!(cache.stats().dirty, 1);
    // With every unpinned slot dirty a miss can't be served.
    {
        let _p6 = cache.pin(6).unwrap();
        let _p7 = cache.pin(7).unwrap();
        assert_eq!(cache.pin(5).err(), Some(BlockError::CacheFull));
    }
    assert!(cache.device().write_log().is_empty());
    assert_eq!(cache.device().flush_calls(), 0);
    assert_eq!(cache.device().volatile_snapshot(), before);

    // Thawed (the tests' way to read back what it held), it writes again.
    cache.thaw();
    cache.sync().unwrap();
    assert_eq!(cache.device().durable_snapshot()[..4], [0xaa; 4]);
}

#[test]
fn the_first_device_failure_stops_writeback_when_asked() {
    // A failed eviction (on a read's miss), with stop_writeback_on_failure:
    // counted, and from then on nothing reaches the device, though writes
    // into the cache are still taken and misses served from clean slots.
    let mut buf = [0u8; 16];
    let cache = BlockCache::new(MemBlockDevice::new(512, 64), 3).unwrap();
    cache.stop_writeback_on_failure();
    cache.write_at(0, &[0x11; 16]).unwrap(); // dirty, least recently used
    cache.write_at(BLOCK_SIZE as u64, &[0x22; 16]).unwrap(); // dirty
    cache.read_at(2 * BLOCK_SIZE as u64, &mut buf).unwrap(); // clean
    cache.device().inject_write_error(BlockError::Io);
    assert_eq!(cache.read_at(3 * BLOCK_SIZE as u64, &mut buf), Err(BlockError::Io));
    assert!(cache.writeback_stopped() && !cache.is_frozen());
    assert_eq!(cache.writeback_failures(), 1);
    cache.device().clear_logs();
    let before = cache.device().volatile_snapshot();
    // Block 0 would have been the victim again: now only the clean block 2 goes.
    cache.read_at(3 * BLOCK_SIZE as u64, &mut buf).unwrap();
    cache.write_at(2 + BLOCK_SIZE as u64, &[0x33; 4]).unwrap(); // still taken
    assert_eq!(cache.sync(), Err(BlockError::ReadOnly));
    assert_eq!(cache.flush_blocks(&[0, 1]), Err(BlockError::ReadOnly));
    // Block 3 dirtied too: every slot is dirty, so a miss can't be served.
    cache.write_at(3 * BLOCK_SIZE as u64, &[0x44; 4]).unwrap();
    assert_eq!(cache.read_at(5 * BLOCK_SIZE as u64, &mut buf), Err(BlockError::CacheFull));
    assert!(cache.device().write_log().is_empty());
    assert_eq!(cache.device().flush_calls(), 0);
    assert_eq!(cache.device().volatile_snapshot(), before);
    cache.read_at(BLOCK_SIZE as u64, &mut buf).unwrap();
    assert_eq!(buf[..4], [0x22, 0x22, 0x33, 0x33]);

    // A failed flush is counted, and stops writeback too.
    let cache = BlockCache::new(MemBlockDevice::new(512, 64), 3).unwrap();
    cache.stop_writeback_on_failure();
    cache.write_at(0, &[0x55; 16]).unwrap();
    cache.device().inject_flush_error(BlockError::Io);
    assert_eq!(cache.sync(), Err(BlockError::Io));
    assert_eq!(cache.writeback_failures(), 1);
    assert!(cache.writeback_stopped());
    cache.write_at(0, &[0x66; 16]).unwrap();
    assert_eq!(cache.sync(), Err(BlockError::ReadOnly));

    // Without asking, a failed flush is counted and the next sync retries.
    let cache = BlockCache::new(MemBlockDevice::new(512, 64), 3).unwrap();
    cache.write_at(0, &[0x77; 16]).unwrap();
    cache.device().inject_flush_error(BlockError::Io);
    assert_eq!(cache.sync(), Err(BlockError::Io));
    assert_eq!(cache.writeback_failures(), 1);
    assert!(!cache.writeback_stopped());
    cache.sync().unwrap();
    assert_eq!(cache.device().durable_snapshot()[..16], [0x77; 16]);
}
