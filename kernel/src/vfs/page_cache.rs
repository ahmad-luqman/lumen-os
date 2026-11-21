//! Unified 4 KiB frame-backed file `PageCache` (shared by `ext4` and `tmpfs`).
//!
//! Each cached page wraps an [`OwnedFrame`](crate::mm::frame::OwnedFrame)
//! accessed RW via the HHDM through raw pointers (avoiding Rust aliasing UB
//! under future `MAP_SHARED` concurrent user writes). `CachePage::drop` and
//! `PageCache::drop` are strictly lock-free (`OwnedFrame::drop` takes only Lock
//! 10 `frame` `SpinLock`).

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use lumen_mm::PhysFrame;
use lumen_vfs::VfsError;

use crate::arch;
use crate::mm;
use crate::mm::frame::{self, OwnedFrame};

pub const PAGE_SIZE: usize = 4096;

/// Soft global reclaim target for live `ext4` page-cache frames (32 MiB = 8192 pages).
pub const EXT4_SOFT_LIMIT_PAGES: usize = 8192;
/// Default `tmpfs` memory quota (32 MiB = 8192 pages).
pub const TMPFS_DEFAULT_QUOTA_PAGES: usize = 8192;
/// Non-evictable quota for open-unlinked detached `ext4` inodes (32 MiB = 8192 pages).
pub const DETACHED_QUOTA_PAGES: usize = 8192;

static EXT4_CACHED_PAGES: AtomicUsize = AtomicUsize::new(0);
static TOTAL_CACHED_PAGES: AtomicUsize = AtomicUsize::new(0);
static DETACHED_CACHED_PAGES: AtomicUsize = AtomicUsize::new(0);
static ACCESS_SEQ: AtomicU64 = AtomicU64::new(1);

/// Current number of live `ext4` `PageCache` pages.
pub fn ext4_cached_pages() -> usize {
    EXT4_CACHED_PAGES.load(Ordering::Relaxed)
}

/// Total number of `PageCache` pages across `ext4`, `tmpfs`, and detached inodes.
pub fn total_cached_pages() -> usize {
    TOTAL_CACHED_PAGES.load(Ordering::Relaxed)
}

/// Current number of detached open-unlinked `ext4` pages.
#[allow(dead_code)]
pub fn detached_cached_pages() -> usize {
    DETACHED_CACHED_PAGES.load(Ordering::Relaxed)
}

/// Lock-free atomic page-frame quota counter (used by `tmpfs` and Gate 6 detach).
#[derive(Debug)]
pub struct QuotaCounter {
    limit_pages: usize,
    used_pages: AtomicUsize,
}

impl QuotaCounter {
    pub const fn new(limit_pages: usize) -> Self {
        Self { limit_pages, used_pages: AtomicUsize::new(0) }
    }

    pub fn try_reserve(&self, pages: usize) -> bool {
        if pages == 0 {
            return true;
        }
        let mut cur = self.used_pages.load(Ordering::Acquire);
        loop {
            let Some(next) = cur.checked_add(pages) else {
                return false;
            };
            if next > self.limit_pages {
                return false;
            }
            match self.used_pages.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    pub fn release(&self, pages: usize) {
        if pages > 0 {
            self.used_pages.fetch_sub(pages, Ordering::AcqRel);
        }
    }

    pub fn used_pages(&self) -> usize {
        self.used_pages.load(Ordering::Acquire)
    }

    #[allow(dead_code)]
    pub const fn limit_pages(&self) -> usize {
        self.limit_pages
    }
}

/// Global Gate 6 quota for open-unlinked detached `ext4` inodes.
pub static DETACHED_QUOTA: QuotaCounter = QuotaCounter::new(DETACHED_QUOTA_PAGES);

/// Ownership class controlling accounting and reclaim eligibility.
#[derive(Clone, Debug)]
pub enum CacheOwner {
    Ext4,
    Tmpfs(Arc<QuotaCounter>),
    Detached,
}

/// One 4 KiB physical page cached in a file's [`PageCache`].
///
/// `Drop` frees its [`OwnedFrame`] via Lock 10 only (never acquires a sleeping lock).
pub struct CachePage {
    frame: OwnedFrame,
    dirty: AtomicBool,
    writable_maps: AtomicU32,
    last_access: AtomicU64,
}

impl CachePage {
    /// Allocate and zero a fresh 4 KiB [`CachePage`].
    pub fn new_zeroed() -> Option<Self> {
        let frame = frame::alloc_owned()?;
        let ptr = mm::phys_to_virt(frame.frame().start()).as_mut_ptr::<u8>();
        // SAFETY: `frame` is a freshly allocated, uniquely owned 4 KiB RAM frame
        // mapped RW through the HHDM.
        unsafe { core::ptr::write_bytes(ptr, 0, PAGE_SIZE) };
        Some(Self {
            frame,
            dirty: AtomicBool::new(false),
            writable_maps: AtomicU32::new(0),
            last_access: AtomicU64::new(ACCESS_SEQ.fetch_add(1, Ordering::Relaxed)),
        })
    }

    /// Underlying physical frame (for zero-copy `PTE_SHARED` user mappings in M8).
    #[allow(dead_code)]
    #[inline]
    pub fn phys_frame(&self) -> PhysFrame {
        self.frame.frame()
    }

    #[inline]
    fn hhdm_ptr(&self) -> *mut u8 {
        mm::phys_to_virt(self.frame.frame().start()).as_mut_ptr::<u8>()
    }

    /// Update LRU access stamp.
    #[inline]
    pub fn touch(&self) {
        self.last_access.store(ACCESS_SEQ.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    }

    #[inline]
    pub fn last_access(&self) -> u64 {
        self.last_access.load(Ordering::Relaxed)
    }

    /// Read bytes from `[off .. off + dst.len())` via raw HHDM pointer.
    pub fn read_bytes(&self, off: usize, dst: &mut [u8]) {
        assert!(off.saturating_add(dst.len()) <= PAGE_SIZE);
        self.touch();
        if dst.is_empty() {
            return;
        }
        // SAFETY: `self.frame` stays alive for `&self`, HHDM maps the entire
        // 4 KiB frame RW, and `off + dst.len() <= PAGE_SIZE`.
        unsafe {
            core::ptr::copy_nonoverlapping(self.hhdm_ptr().add(off), dst.as_mut_ptr(), dst.len());
        }
    }

    /// Write bytes into `[off .. off + src.len())` via raw HHDM pointer.
    pub fn write_bytes(&self, off: usize, src: &[u8]) {
        assert!(off.saturating_add(src.len()) <= PAGE_SIZE);
        self.touch();
        if src.is_empty() {
            return;
        }
        // SAFETY: `self.frame` stays alive for `&self`, HHDM maps the entire
        // 4 KiB frame RW, and `off + src.len() <= PAGE_SIZE`.
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), self.hhdm_ptr().add(off), src.len());
        }
    }

    /// Zero `[start .. end)` inside the 4 KiB frame.
    pub fn zero_range(&self, start: usize, end: usize) {
        assert!(start <= end && end <= PAGE_SIZE);
        if start == end {
            return;
        }
        // SAFETY: `self.frame` stays alive for `&self`, HHDM maps the entire
        // 4 KiB frame RW, and `end <= PAGE_SIZE`.
        unsafe {
            core::ptr::write_bytes(self.hhdm_ptr().add(start), 0, end - start);
        }
    }

    /// True if explicitly dirtied or currently mapped writable via `MAP_SHARED`.
    #[inline]
    pub fn is_dirty(&self) -> bool {
        self.dirty.load(Ordering::Acquire) || self.writable_maps.load(Ordering::Acquire) > 0
    }

    #[inline]
    pub fn set_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    /// Atomically swap `dirty`, returning the previous value.
    #[inline]
    pub fn swap_dirty(&self, val: bool) -> bool {
        self.dirty.swap(val, Ordering::AcqRel)
    }

    #[allow(dead_code)]
    #[inline]
    pub fn writable_maps(&self) -> u32 {
        self.writable_maps.load(Ordering::Acquire)
    }

    #[allow(dead_code)]
    #[inline]
    pub fn inc_writable_maps(&self) {
        self.writable_maps.fetch_add(1, Ordering::AcqRel);
    }

    #[allow(dead_code)]
    #[inline]
    pub fn dec_writable_maps(&self) {
        let prev = self.writable_maps.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "writable_maps underflow");
    }
}

/// Per-inode Lock-5 page cache holding `Arc<CachePage>` keyed by `page_index`.
pub struct PageCache {
    owner: CacheOwner,
    pages: BTreeMap<u64, Arc<CachePage>>,
}

impl PageCache {
    pub fn new_ext4() -> Self {
        Self { owner: CacheOwner::Ext4, pages: BTreeMap::new() }
    }

    pub fn new_tmpfs(quota: Arc<QuotaCounter>) -> Self {
        Self { owner: CacheOwner::Tmpfs(quota), pages: BTreeMap::new() }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    #[inline]
    pub fn get(&self, page_index: u64) -> Option<&Arc<CachePage>> {
        self.pages.get(&page_index)
    }

    /// Insert a newly allocated page at `page_index` (must not already exist),
    /// reserving `tmpfs`/`detached` quota if applicable.
    pub fn insert_new(&mut self, page_index: u64, page: Arc<CachePage>) -> Result<(), VfsError> {
        debug_assert!(!self.pages.contains_key(&page_index));
        match &self.owner {
            CacheOwner::Ext4 => {
                EXT4_CACHED_PAGES.fetch_add(1, Ordering::Relaxed);
                TOTAL_CACHED_PAGES.fetch_add(1, Ordering::Relaxed);
            }
            CacheOwner::Tmpfs(quota) => {
                if !quota.try_reserve(1) {
                    return Err(VfsError::NoSpc);
                }
                TOTAL_CACHED_PAGES.fetch_add(1, Ordering::Relaxed);
            }
            CacheOwner::Detached => {
                if !DETACHED_QUOTA.try_reserve(1) {
                    return Err(VfsError::NoSpc);
                }
                DETACHED_CACHED_PAGES.fetch_add(1, Ordering::Relaxed);
                TOTAL_CACHED_PAGES.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.pages.insert(page_index, page);
        Ok(())
    }

    /// Drop all cached pages with `page_index >= start_page`, updating accounting.
    pub fn remove_from(&mut self, start_page: u64) -> usize {
        let removed = self.pages.split_off(&start_page);
        let count = removed.len();
        if count > 0 {
            self.release_accounting(count);
        }
        count
    }

    /// Collect `(page_index, Arc<CachePage>)` in ascending order for all dirty pages.
    pub fn collect_dirty(&self) -> Vec<(u64, Arc<CachePage>)> {
        self.pages.iter().filter(|(_, p)| p.is_dirty()).map(|(&idx, p)| (idx, Arc::clone(p))).collect()
    }

    /// Collect clean `ext4` eviction candidates (`Arc::strong_count == 1 && !is_dirty()`)
    /// sorted oldest `last_access` first.
    pub fn evict_clean_oldest(&mut self, max_evict: usize) -> usize {
        if !matches!(self.owner, CacheOwner::Ext4) || max_evict == 0 {
            return 0;
        }
        let mut candidates: Vec<(u64, u64)> = self
            .pages
            .iter()
            .filter(|(_, p)| !p.is_dirty() && Arc::strong_count(p) == 1)
            .map(|(&idx, p)| (p.last_access(), idx))
            .collect();
        candidates.sort_unstable();
        let mut evicted = 0usize;
        for (_, idx) in candidates.into_iter().take(max_evict) {
            if self.pages.remove(&idx).is_some() {
                evicted += 1;
            }
        }
        if evicted > 0 {
            self.release_accounting(evicted);
        }
        evicted
    }

    /// Evict specific clean pages if still `!is_dirty() && Arc::strong_count == 1`.
    pub fn evict_if_clean_unpinned(&mut self, indices: &[u64]) -> usize {
        if !matches!(self.owner, CacheOwner::Ext4) || indices.is_empty() {
            return 0;
        }
        let mut evicted = 0usize;
        for &idx in indices {
            let can_evict = self.pages.get(&idx).is_some_and(|p| !p.is_dirty() && Arc::strong_count(p) == 1);
            if can_evict && self.pages.remove(&idx).is_some() {
                evicted += 1;
            }
        }
        if evicted > 0 {
            self.release_accounting(evicted);
        }
        evicted
    }

    /// Commit transition from `CacheOwner::Ext4` to `CacheOwner::Detached`.
    ///
    /// Caller must have ALREADY reserved `self.pages.len() + extra_pages.len()`
    /// in [`DETACHED_QUOTA`].
    pub fn commit_detached(&mut self, extra_pages: Vec<(u64, Arc<CachePage>)>) {
        debug_assert!(matches!(self.owner, CacheOwner::Ext4));
        let existing = self.pages.len();
        let added = extra_pages.len();
        if existing > 0 {
            EXT4_CACHED_PAGES.fetch_sub(existing, Ordering::Relaxed);
        }
        DETACHED_CACHED_PAGES.fetch_add(existing + added, Ordering::Relaxed);
        if added > 0 {
            TOTAL_CACHED_PAGES.fetch_add(added, Ordering::Relaxed);
        }
        for (idx, page) in extra_pages {
            self.pages.insert(idx, page);
        }
        self.owner = CacheOwner::Detached;
    }

    fn release_accounting(&self, count: usize) {
        if count == 0 {
            return;
        }
        TOTAL_CACHED_PAGES.fetch_sub(count, Ordering::Relaxed);
        match &self.owner {
            CacheOwner::Ext4 => {
                EXT4_CACHED_PAGES.fetch_sub(count, Ordering::Relaxed);
            }
            CacheOwner::Tmpfs(quota) => {
                quota.release(count);
            }
            CacheOwner::Detached => {
                DETACHED_CACHED_PAGES.fetch_sub(count, Ordering::Relaxed);
                DETACHED_QUOTA.release(count);
            }
        }
    }
}

impl Drop for PageCache {
    fn drop(&mut self) {
        let count = self.pages.len();
        if count > 0 {
            self.release_accounting(count);
        }
        self.pages.clear();
    }
}

/// RAII reservation guard against [`DETACHED_QUOTA`] for Gate 6 detach faulting.
pub struct DetachedQuotaGuard {
    reserved: usize,
}

impl DetachedQuotaGuard {
    pub fn new() -> Self {
        Self { reserved: 0 }
    }

    pub fn try_reserve(&mut self, pages: usize) -> Result<(), VfsError> {
        if DETACHED_QUOTA.try_reserve(pages) {
            self.reserved += pages;
            Ok(())
        } else {
            Err(VfsError::NoMem)
        }
    }

    pub fn commit(mut self) {
        self.reserved = 0;
    }
}

impl Drop for DetachedQuotaGuard {
    fn drop(&mut self) {
        if self.reserved > 0 {
            DETACHED_QUOTA.release(self.reserved);
            self.reserved = 0;
        }
    }
}

/// Soft global reclaim for `ext4` `PageCache` frames when above 32 MiB.
///
/// MUST ONLY be called with interrupts enabled, `irq::depth() == 0`, and NO
/// Lock 3, Lock 4, Lock 5, or Lock 10 held.
pub fn maybe_reclaim_ext4_pages() {
    if EXT4_CACHED_PAGES.load(Ordering::Relaxed) <= EXT4_SOFT_LIMIT_PAGES {
        return;
    }
    reclaim_ext4_to_target(EXT4_SOFT_LIMIT_PAGES);
}

/// Reclaim unpinned `ext4` `PageCache` frames down to `target_pages` outside Lock 4/5.
pub fn reclaim_ext4_to_target(target_pages: usize) {
    if EXT4_CACHED_PAGES.load(Ordering::Relaxed) <= target_pages {
        return;
    }
    debug_assert!(
        (arch::irq::enabled() || arch::irq::disabled_for_good_but_not_here()) && arch::irq::depth() == 0,
        "reclaim_ext4_to_target called with interrupts off"
    );

    let inodes = super::snapshot_cached_inodes();
    // Pass 1: evict clean unpinned ext4 pages (Lock 4(B) -> Lock 5(B)).
    for inode in &inodes {
        let cur = EXT4_CACHED_PAGES.load(Ordering::Relaxed);
        if cur <= target_pages {
            return;
        }
        if inode.is_detached() {
            continue;
        }
        let excess = cur - target_pages;
        super::reclaim_ext4_inode_clean(inode, excess);
    }

    // Pass 2: if still over target, flush dirty ext4 pages and evict if unpinned.
    for inode in &inodes {
        let cur = EXT4_CACHED_PAGES.load(Ordering::Relaxed);
        if cur <= target_pages {
            return;
        }
        if inode.is_detached() {
            continue;
        }
        let excess = cur - target_pages;
        super::reclaim_ext4_inode_dirty(inode, excess);
    }
}
