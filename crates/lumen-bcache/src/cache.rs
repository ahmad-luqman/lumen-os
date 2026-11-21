use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cell::{Ref, RefCell, RefMut};
use core::ops::{Deref, DerefMut};

use crate::{BLOCK_SIZE, BlockDevice, BlockError, BlockInfo, DEFAULT_CAPACITY};

/// Runtime statistics snapshot from [`BlockCache::stats`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockCacheStats {
    /// Maximum number of 4 KiB blocks the cache can hold.
    pub capacity: usize,
    /// Number of valid cached blocks currently resident.
    pub cached: usize,
    /// Number of resident blocks currently pinned (`pin_count > 0`).
    pub pinned: usize,
    /// Number of resident blocks with uncommitted dirty data.
    pub dirty: usize,
    /// Cache hits across `pin`, `pin_mut`, `read_at`, and `write_at`.
    pub hits: u64,
    /// Cache misses across `pin`, `pin_mut`, `read_at`, and `write_at`.
    pub misses: u64,
    /// Number of clean/flushed resident blocks evicted to make room.
    pub evictions: u64,
    /// Number of dirty block writebacks issued to the underlying device.
    pub writebacks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SlotState {
    Empty,
    Occupied { block_id: u64, dirty: bool },
}

struct Slot {
    state: SlotState,
    pin_count: usize,
    prev: Option<usize>,
    next: Option<usize>,
    data: Box<RefCell<[u8; BLOCK_SIZE]>>,
}

impl Slot {
    fn new_empty() -> Self {
        Self {
            state: SlotState::Empty,
            pin_count: 0,
            prev: None,
            next: None,
            data: Box::new(RefCell::new([0u8; BLOCK_SIZE])),
        }
    }
}

struct CacheInner {
    capacity: usize,
    slots: Vec<Slot>,
    free_slots: Vec<usize>,
    index: BTreeMap<u64, usize>,
    mru_head: Option<usize>,
    lru_tail: Option<usize>,
    needs_device_flush: bool,
    hits: u64,
    misses: u64,
    evictions: u64,
    writebacks: u64,
}

impl CacheInner {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            slots: Vec::with_capacity(capacity),
            free_slots: Vec::new(),
            index: BTreeMap::new(),
            mru_head: None,
            lru_tail: None,
            needs_device_flush: false,
            hits: 0,
            misses: 0,
            evictions: 0,
            writebacks: 0,
        }
    }

    fn unlink(&mut self, idx: usize) {
        let prev = self.slots[idx].prev;
        let next = self.slots[idx].next;
        match prev {
            Some(p) => self.slots[p].next = next,
            None => self.mru_head = next,
        }
        match next {
            Some(n) => self.slots[n].prev = prev,
            None => self.lru_tail = prev,
        }
        self.slots[idx].prev = None;
        self.slots[idx].next = None;
    }

    fn link_mru(&mut self, idx: usize) {
        debug_assert!(self.slots[idx].prev.is_none() && self.slots[idx].next.is_none());
        let old_head = self.mru_head;
        self.slots[idx].prev = None;
        self.slots[idx].next = old_head;
        match old_head {
            Some(h) => self.slots[h].prev = Some(idx),
            None => self.lru_tail = Some(idx),
        }
        self.mru_head = Some(idx);
    }

    fn touch_mru(&mut self, idx: usize) {
        if self.mru_head == Some(idx) {
            return;
        }
        self.unlink(idx);
        self.link_mru(idx);
    }

    fn find_lru_unpinned(&self) -> Option<usize> {
        let mut cur = self.lru_tail;
        while let Some(idx) = cur {
            if self.slots[idx].pin_count == 0 {
                return Some(idx);
            }
            cur = self.slots[idx].prev;
        }
        None
    }
}

/// Fixed-capacity 4 KiB LRU block cache over any [`BlockDevice`].
///
/// Uses interior mutability (`RefCell`) rather than an IRQ-off spinlock so
/// callers (serialized externally by a sleeping mutex in the kernel) never
/// hold a spinlock across blocking [`BlockDevice`] I/O.
pub struct BlockCache<B: BlockDevice> {
    device: B,
    info: BlockInfo,
    sectors_per_block: u64,
    total_bytes: u64,
    total_blocks: u64,
    inner: RefCell<CacheInner>,
}

impl<B: BlockDevice> BlockCache<B> {
    /// Create a block cache with `capacity` 4 KiB block slots.
    pub fn new(device: B, capacity: usize) -> Result<Self, BlockError> {
        if capacity == 0 {
            return Err(BlockError::CacheFull);
        }
        let info = device.info();
        let sector_size = info.sector_size as usize;
        if !(512..=BLOCK_SIZE).contains(&sector_size) || !sector_size.is_power_of_two() {
            return Err(BlockError::Unaligned);
        }
        if info.sector_count == 0 {
            return Err(BlockError::OutOfRange);
        }
        let total_bytes = info.sector_count.checked_mul(u64::from(info.sector_size)).ok_or(BlockError::OutOfRange)?;
        let sectors_per_block = (BLOCK_SIZE / sector_size) as u64;
        let total_blocks = info.sector_count.div_ceil(sectors_per_block);

        Ok(Self {
            device,
            info,
            sectors_per_block,
            total_bytes,
            total_blocks,
            inner: RefCell::new(CacheInner::new(capacity)),
        })
    }

    /// Create a block cache with [`DEFAULT_CAPACITY`] (1024 blocks = 4 MiB).
    pub fn with_default_capacity(device: B) -> Result<Self, BlockError> {
        Self::new(device, DEFAULT_CAPACITY)
    }

    /// Underlying device geometry and capability flags.
    pub fn info(&self) -> BlockInfo {
        self.info
    }

    /// Total device capacity in bytes.
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Total number of 4 KiB blocks (including any partial tail block).
    pub fn total_blocks(&self) -> u64 {
        self.total_blocks
    }

    /// Borrow the underlying device.
    pub fn device(&self) -> &B {
        &self.device
    }

    /// Consume the cache and return the underlying device.
    pub fn into_device(self) -> B {
        self.device
    }

    /// Whether dirty writes have hit the device since the last successful [`BlockDevice::flush`].
    pub fn needs_device_flush(&self) -> bool {
        self.inner.borrow().needs_device_flush
    }

    /// Current cache statistics snapshot.
    pub fn stats(&self) -> BlockCacheStats {
        let inner = self.inner.borrow();
        let mut pinned = 0usize;
        let mut dirty = 0usize;
        for &slot_idx in inner.index.values() {
            let slot = &inner.slots[slot_idx];
            if slot.pin_count > 0 {
                pinned += 1;
            }
            if matches!(slot.state, SlotState::Occupied { dirty: true, .. }) {
                dirty += 1;
            }
        }
        BlockCacheStats {
            capacity: inner.capacity,
            cached: inner.index.len(),
            pinned,
            dirty,
            hits: inner.hits,
            misses: inner.misses,
            evictions: inner.evictions,
            writebacks: inner.writebacks,
        }
    }

    /// Compute `(starting_lba, valid_bytes)` for `block_id`, clamping partial tail blocks.
    fn block_extent(&self, block_id: u64) -> Result<(u64, usize), BlockError> {
        if block_id >= self.total_blocks {
            return Err(BlockError::OutOfRange);
        }
        let lba = block_id.checked_mul(self.sectors_per_block).ok_or(BlockError::OutOfRange)?;
        let remaining_sectors =
            self.info.sector_count.checked_sub(lba).filter(|&s| s > 0).ok_or(BlockError::OutOfRange)?;
        let valid_sectors = remaining_sectors.min(self.sectors_per_block) as usize;
        Ok((lba, valid_sectors * self.info.sector_size as usize))
    }

    /// Ensure `block_id` is resident in a cache slot and return its slot index.
    ///
    /// When `skip_read` is `true` (used by full-block `write_at` overwrites),
    /// a cache miss skips the pre-read from `BlockDevice` and only zeroes any
    /// unused tail bytes past `valid_bytes`.
    fn ensure_slot(&self, block_id: u64, skip_read: bool) -> Result<usize, BlockError> {
        let (lba, valid_bytes) = self.block_extent(block_id)?;

        // Step 1: fast path hit or acquire/evict a slot.
        let slot_idx = loop {
            let mut inner = self.inner.try_borrow_mut().map_err(|_| BlockError::CacheFull)?;
            if let Some(&slot_idx) = inner.index.get(&block_id) {
                inner.touch_mru(slot_idx);
                inner.hits += 1;
                return Ok(slot_idx);
            }

            if let Some(slot_idx) = inner.free_slots.pop() {
                inner.slots[slot_idx].pin_count += 1;
                break slot_idx;
            }

            if inner.slots.len() < inner.capacity {
                let slot_idx = inner.slots.len();
                inner.slots.push(Slot::new_empty());
                inner.slots[slot_idx].pin_count += 1;
                break slot_idx;
            }

            let victim_idx = inner.find_lru_unpinned().ok_or(BlockError::CacheFull)?;
            let SlotState::Occupied { block_id: old_block_id, dirty } = inner.slots[victim_idx].state else {
                unreachable!("only occupied slots are linked into the LRU list");
            };

            if dirty {
                let (old_lba, old_valid_bytes) = self.block_extent(old_block_id)?;
                // Pin victim across the blocking writeback so its Box<RefCell> stays immovable.
                inner.slots[victim_idx].pin_count += 1;
                let data_ptr: *const RefCell<[u8; BLOCK_SIZE]> = &*inner.slots[victim_idx].data;
                drop(inner);

                let write_res = {
                    // SAFETY: `victim_idx` has `pin_count > 0` and its slot `Box` is never moved/dropped
                    // while `BlockCache` is borrowed; `BlockCache` is `!Sync`.
                    let guard = unsafe { &*data_ptr }.try_borrow().map_err(|_| BlockError::CacheFull);
                    match guard {
                        Ok(buf) => self.device.write_sectors(old_lba, &buf[..old_valid_bytes]),
                        Err(e) => Err(e),
                    }
                };

                let mut inner = self.inner.borrow_mut();
                inner.slots[victim_idx].pin_count -= 1;
                match write_res {
                    Ok(()) => {
                        inner.slots[victim_idx].state = SlotState::Occupied { block_id: old_block_id, dirty: false };
                        inner.writebacks += 1;
                        inner.needs_device_flush = true;
                        // Continue loop: re-check victim cleanly and evict it (or hit if somehow present).
                    }
                    Err(e) => return Err(e),
                }
            } else {
                inner.unlink(victim_idx);
                inner.index.remove(&old_block_id);
                inner.slots[victim_idx].state = SlotState::Empty;
                inner.evictions += 1;
                inner.slots[victim_idx].pin_count += 1;
                break victim_idx;
            }
        };

        // Step 2: load `block_id` into `slot_idx` (currently `Empty` and pinned).
        let data_ptr: *const RefCell<[u8; BLOCK_SIZE]> = {
            let inner = self.inner.borrow();
            &*inner.slots[slot_idx].data
        };

        let load_res = {
            // SAFETY: `slot_idx` is pinned (`pin_count > 0`), `Empty`, and exclusively owned during load.
            let mut buf = unsafe { &*data_ptr }.borrow_mut();
            let res = if skip_read { Ok(()) } else { self.device.read_sectors(lba, &mut buf[..valid_bytes]) };
            if res.is_ok() && valid_bytes < BLOCK_SIZE {
                buf[valid_bytes..].fill(0);
            }
            res
        };

        let mut inner = self.inner.borrow_mut();
        inner.slots[slot_idx].pin_count -= 1;
        match load_res {
            Ok(()) => {
                inner.slots[slot_idx].state = SlotState::Occupied { block_id, dirty: false };
                inner.index.insert(block_id, slot_idx);
                inner.link_mru(slot_idx);
                inner.misses += 1;
                Ok(slot_idx)
            }
            Err(e) => {
                inner.slots[slot_idx].state = SlotState::Empty;
                inner.free_slots.push(slot_idx);
                Err(e)
            }
        }
    }

    /// Pin 4 KiB block `block_id` for shared read access.
    pub fn pin(&self, block_id: u64) -> Result<PinnedBlock<'_, B>, BlockError> {
        let slot_idx = self.ensure_slot(block_id, false)?;
        let mut inner = self.inner.borrow_mut();
        let data_ptr: *const RefCell<[u8; BLOCK_SIZE]> = &*inner.slots[slot_idx].data;
        // SAFETY: slot `Box` lives as long as `&self`, and `pin_count` prevents eviction/replacement.
        let guard = unsafe { &*data_ptr }.try_borrow().map_err(|_| BlockError::CacheFull)?;
        inner.slots[slot_idx].pin_count += 1;
        Ok(PinnedBlock { guard, cache: self, slot_idx })
    }

    /// Pin 4 KiB block `block_id` for exclusive mutable access.
    pub fn pin_mut(&self, block_id: u64) -> Result<PinnedBlockMut<'_, B>, BlockError> {
        if self.info.read_only {
            return Err(BlockError::ReadOnly);
        }
        let slot_idx = self.ensure_slot(block_id, false)?;
        let mut inner = self.inner.borrow_mut();
        let data_ptr: *const RefCell<[u8; BLOCK_SIZE]> = &*inner.slots[slot_idx].data;
        // SAFETY: slot `Box` lives as long as `&self`, and `pin_count` prevents eviction/replacement.
        let guard = unsafe { &*data_ptr }.try_borrow_mut().map_err(|_| BlockError::CacheFull)?;
        inner.slots[slot_idx].pin_count += 1;
        Ok(PinnedBlockMut { guard, cache: self, slot_idx })
    }

    fn mark_slot_dirty(&self, slot_idx: usize) {
        let mut inner = self.inner.borrow_mut();
        if let SlotState::Occupied { dirty, .. } = &mut inner.slots[slot_idx].state {
            *dirty = true;
        }
    }

    /// Read `dst.len()` bytes starting at byte offset `byte_off`.
    pub fn read_at(&self, byte_off: u64, dst: &mut [u8]) -> Result<(), BlockError> {
        let end = byte_off.checked_add(dst.len() as u64).ok_or(BlockError::OutOfRange)?;
        if end > self.total_bytes {
            return Err(BlockError::OutOfRange);
        }
        if dst.is_empty() {
            return Ok(());
        }

        let mut cur_off = byte_off;
        let mut written = 0usize;
        while written < dst.len() {
            let block_id = cur_off / BLOCK_SIZE as u64;
            let in_block = (cur_off % BLOCK_SIZE as u64) as usize;
            let chunk = (dst.len() - written).min(BLOCK_SIZE - in_block);

            let slot_idx = self.ensure_slot(block_id, false)?;
            let inner = self.inner.borrow();
            let buf = inner.slots[slot_idx].data.try_borrow().map_err(|_| BlockError::CacheFull)?;
            dst[written..written + chunk].copy_from_slice(&buf[in_block..in_block + chunk]);

            cur_off += chunk as u64;
            written += chunk;
        }
        Ok(())
    }

    /// Write `src` starting at byte offset `byte_off`.
    pub fn write_at(&self, byte_off: u64, src: &[u8]) -> Result<(), BlockError> {
        if self.info.read_only {
            return Err(BlockError::ReadOnly);
        }
        let end = byte_off.checked_add(src.len() as u64).ok_or(BlockError::OutOfRange)?;
        if end > self.total_bytes {
            return Err(BlockError::OutOfRange);
        }
        if src.is_empty() {
            return Ok(());
        }

        let mut cur_off = byte_off;
        let mut read = 0usize;
        while read < src.len() {
            let block_id = cur_off / BLOCK_SIZE as u64;
            let in_block = (cur_off % BLOCK_SIZE as u64) as usize;
            let (_, valid_bytes) = self.block_extent(block_id)?;
            let chunk = (src.len() - read).min(BLOCK_SIZE - in_block);
            let skip_read = in_block == 0 && chunk == valid_bytes;

            let slot_idx = self.ensure_slot(block_id, skip_read)?;
            let mut inner = self.inner.borrow_mut();
            {
                let mut buf = inner.slots[slot_idx].data.try_borrow_mut().map_err(|_| BlockError::CacheFull)?;
                buf[in_block..in_block + chunk].copy_from_slice(&src[read..read + chunk]);
            }
            inner.slots[slot_idx].state = SlotState::Occupied { block_id, dirty: true };

            cur_off += chunk as u64;
            read += chunk;
        }
        Ok(())
    }

    /// Write back dirty blocks whose IDs are in `block_ids` in ascending `block_id` order,
    /// then invoke [`BlockDevice::flush`] if `needs_device_flush` is set.
    pub fn flush_blocks(&self, block_ids: &[u64]) -> Result<(), BlockError> {
        let mut targets: Vec<u64> = Vec::with_capacity(block_ids.len());
        for &id in block_ids {
            if id >= self.total_blocks {
                return Err(BlockError::OutOfRange);
            }
            targets.push(id);
        }
        targets.sort_unstable();
        targets.dedup();

        let dirty_list: Vec<(u64, usize)> = {
            let inner = self.inner.borrow();
            let mut out = Vec::new();
            for id in targets {
                if let Some(&slot_idx) = inner.index.get(&id)
                    && matches!(inner.slots[slot_idx].state, SlotState::Occupied { dirty: true, .. })
                {
                    out.push((id, slot_idx));
                }
            }
            out
        };

        self.writeback_and_flush(&dirty_list)
    }

    /// Write back all dirty blocks in ascending `block_id` order, then invoke
    /// [`BlockDevice::flush`] if `needs_device_flush` is set.
    pub fn sync(&self) -> Result<(), BlockError> {
        let dirty_list: Vec<(u64, usize)> = {
            let inner = self.inner.borrow();
            inner
                .index
                .iter()
                .filter_map(|(&block_id, &slot_idx)| match inner.slots[slot_idx].state {
                    SlotState::Occupied { dirty: true, .. } => Some((block_id, slot_idx)),
                    _ => None,
                })
                .collect()
        };

        self.writeback_and_flush(&dirty_list)
    }

    fn writeback_and_flush(&self, dirty_list: &[(u64, usize)]) -> Result<(), BlockError> {
        {
            let inner = self.inner.borrow();
            for &(block_id, slot_idx) in dirty_list {
                if matches!(inner.slots[slot_idx].state, SlotState::Occupied { block_id: b, dirty: true } if b == block_id)
                    && inner.slots[slot_idx].data.try_borrow().is_err()
                {
                    return Err(BlockError::CacheFull);
                }
            }
        }

        for &(block_id, slot_idx) in dirty_list {
            let (lba, valid_bytes) = self.block_extent(block_id)?;
            let data_ptr: *const RefCell<[u8; BLOCK_SIZE]> = {
                let mut inner = self.inner.borrow_mut();
                if !matches!(inner.slots[slot_idx].state, SlotState::Occupied { block_id: b, dirty: true } if b == block_id)
                {
                    continue;
                }
                // Pin across `write_sectors` so the slot's Box<RefCell> stays immovable.
                inner.slots[slot_idx].pin_count += 1;
                &*inner.slots[slot_idx].data
            };

            let write_res = {
                // SAFETY: `slot_idx` is pinned and `BlockCache` is `!Sync`.
                match unsafe { &*data_ptr }.try_borrow() {
                    Ok(buf) => self.device.write_sectors(lba, &buf[..valid_bytes]),
                    Err(_) => Err(BlockError::CacheFull),
                }
            };

            let mut inner = self.inner.borrow_mut();
            inner.slots[slot_idx].pin_count -= 1;
            write_res?;
            inner.slots[slot_idx].state = SlotState::Occupied { block_id, dirty: false };
            inner.writebacks += 1;
            inner.needs_device_flush = true;
        }

        let need_flush = self.inner.borrow().needs_device_flush;
        if need_flush {
            self.device.flush()?;
            self.inner.borrow_mut().needs_device_flush = false;
        }
        Ok(())
    }
}

/// RAII shared pin guard for a cached 4 KiB block.
///
/// While alive, the pinned block will not be evicted by LRU replacement.
pub struct PinnedBlock<'a, B: BlockDevice> {
    // Field order matters: `guard` drops before `PinnedBlock::drop` decrements `pin_count`.
    guard: Ref<'a, [u8; BLOCK_SIZE]>,
    cache: &'a BlockCache<B>,
    slot_idx: usize,
}

impl<B: BlockDevice> Deref for PinnedBlock<'_, B> {
    type Target = [u8; BLOCK_SIZE];

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<B: BlockDevice> Drop for PinnedBlock<'_, B> {
    fn drop(&mut self) {
        let mut inner = self.cache.inner.borrow_mut();
        debug_assert!(inner.slots[self.slot_idx].pin_count > 0);
        inner.slots[self.slot_idx].pin_count -= 1;
    }
}

/// RAII exclusive mutable pin guard for a cached 4 KiB block.
///
/// Marks the block dirty whenever mutably dereferenced (or via [`Self::mark_dirty`]).
/// While alive, the pinned block will not be evicted by LRU replacement.
pub struct PinnedBlockMut<'a, B: BlockDevice> {
    // Field order matters: `guard` drops before `PinnedBlockMut::drop` decrements `pin_count`.
    guard: RefMut<'a, [u8; BLOCK_SIZE]>,
    cache: &'a BlockCache<B>,
    slot_idx: usize,
}

impl<B: BlockDevice> PinnedBlockMut<'_, B> {
    /// Explicitly mark the pinned block dirty.
    pub fn mark_dirty(&mut self) {
        self.cache.mark_slot_dirty(self.slot_idx);
    }
}

impl<B: BlockDevice> Deref for PinnedBlockMut<'_, B> {
    type Target = [u8; BLOCK_SIZE];

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl<B: BlockDevice> DerefMut for PinnedBlockMut<'_, B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.cache.mark_slot_dirty(self.slot_idx);
        &mut self.guard
    }
}

impl<B: BlockDevice> Drop for PinnedBlockMut<'_, B> {
    fn drop(&mut self) {
        let mut inner = self.cache.inner.borrow_mut();
        debug_assert!(inner.slots[self.slot_idx].pin_count > 0);
        inner.slots[self.slot_idx].pin_count -= 1;
    }
}
