//! Unified 4 KiB frame-backed file `PageCache` (shared by `ext4` and `tmpfs`).
//!
//! Each cached page wraps an [`OwnedFrame`](crate::mm::frame::OwnedFrame)
//! accessed through the HHDM with raw pointers, so no `&mut [u8]` to the
//! frame ever exists. Raw pointers avoid reference aliasing, not data races:
//! kernel readers and writers of a published page are serialized by the
//! owning inode's `PageCache` lock (Lock 5). A `MAP_SHARED` user mapping
//! (#255, through [`mmap_page`]) writes the frame without that lock, as a
//! copy-on-write split reads it ([`CachePage::copy_to_frame`]); such a store racing a
//! kernel copy is accepted as it is on Linux (the bytes read are some mix of
//! old and new, nothing else). `CachePage::drop` and `PageCache::drop` take
//! no sleeping lock (`OwnedFrame::drop` takes only Lock 10, the `frame`
//! `SpinLock`); the one exception is a quota over-release, a bug elsewhere,
//! whose `error!` takes the log's leaf lock.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use lumen_mm::PhysFrame;
use lumen_vfs::{VfsError, VfsInode};

use crate::arch;
use crate::mm;
use crate::mm::frame::{self, OwnedFrame};

pub const PAGE_SIZE: usize = 4096;

/// Soft global reclaim target for live `ext4` page-cache frames (32 MiB = 8192 pages).
pub const EXT4_SOFT_LIMIT_PAGES: usize = 8192;
/// Hard limit on live `ext4` page-cache frames (64 MiB): past it, after a
/// reclaim attempt, `ext4` writes that need a new page fail with `-ENOSPC`
/// (or return short). Reclaim can only stay above it when writeback keeps
/// failing (a full or failing disk) or pages are pinned, so this bounds the
/// memory a writer can fill with pages that never reach the disk.
pub const EXT4_HARD_LIMIT_PAGES: usize = 2 * EXT4_SOFT_LIMIT_PAGES;
/// Default `tmpfs` memory quota (32 MiB = 8192 pages).
pub const TMPFS_DEFAULT_QUOTA_PAGES: usize = 8192;
/// Non-evictable quota for open-unlinked detached `ext4` inodes (32 MiB = 8192 pages).
pub const DETACHED_QUOTA_PAGES: usize = 8192;

static EXT4_CACHED_PAGES: AtomicUsize = AtomicUsize::new(0);
static TOTAL_CACHED_PAGES: AtomicUsize = AtomicUsize::new(0);
static DETACHED_CACHED_PAGES: AtomicUsize = AtomicUsize::new(0);
static ACCESS_SEQ: AtomicU64 = AtomicU64::new(1);
/// The soft and hard limits in force: the constants above, lowered only by
/// ktests (`set_ext4_limits_for_test`) so a test needn't fill 32 MiB.
static EXT4_SOFT_LIMIT: AtomicUsize = AtomicUsize::new(EXT4_SOFT_LIMIT_PAGES);
static EXT4_HARD_LIMIT: AtomicUsize = AtomicUsize::new(EXT4_HARD_LIMIT_PAGES);

/// The soft reclaim target in force (see [`EXT4_SOFT_LIMIT_PAGES`]).
pub fn ext4_soft_limit() -> usize {
    EXT4_SOFT_LIMIT.load(Ordering::Relaxed)
}

/// Whether the live `ext4` pages have reached the hard limit
/// ([`EXT4_HARD_LIMIT_PAGES`]), so no new page may be added.
pub fn ext4_at_hard_limit() -> bool {
    EXT4_CACHED_PAGES.load(Ordering::Relaxed) >= EXT4_HARD_LIMIT.load(Ordering::Relaxed)
}

/// A write was refused a page at the hard limit since the cache last fell
/// back under the soft limit: one warning per crossing.
static HARD_LIMIT_WARNED: AtomicBool = AtomicBool::new(false);

/// Log (once per crossing of the hard limit) that an `ext4` write was
/// refused a new page: the cache is full of pages reclaim can't drop,
/// usually because their writeback keeps failing (its own warning names the
/// inode), so writes to any file now fail with `ENOSPC`.
pub fn note_hard_limit_refusal() {
    if !HARD_LIMIT_WARNED.swap(true, Ordering::AcqRel) {
        log::warn!(
            target: "vfs",
            "ext4 page cache at its hard limit ({} pages): refusing new pages (ENOSPC) until writeback frees some",
            EXT4_HARD_LIMIT.load(Ordering::Relaxed)
        );
    }
}

/// Lower (or restore, with `None`) the soft and hard limits for a ktest.
#[cfg(feature = "ktest")]
pub fn set_ext4_limits_for_test(limits: Option<(usize, usize)>) {
    let (soft, hard) = limits.unwrap_or((EXT4_SOFT_LIMIT_PAGES, EXT4_HARD_LIMIT_PAGES));
    EXT4_SOFT_LIMIT.store(soft, Ordering::Relaxed);
    EXT4_HARD_LIMIT.store(hard, Ordering::Relaxed);
}

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
    /// What the quota is for, named in its log lines.
    name: &'static str,
    limit_pages: usize,
    used_pages: AtomicUsize,
}

impl QuotaCounter {
    pub const fn new(name: &'static str, limit_pages: usize) -> Self {
        Self { name, limit_pages, used_pages: AtomicUsize::new(0) }
    }

    /// Reserve `pages`, or `false` if that would pass the limit. A `true`
    /// must be released later, or the pages leak from the quota.
    #[must_use]
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

    /// Return `pages` to the quota. Releasing more than is reserved is a
    /// bug elsewhere: the count saturates at zero (logged) rather than
    /// wrapping, which would make every later reservation fail.
    pub fn release(&self, pages: usize) {
        if pages == 0 {
            return;
        }
        let mut cur = self.used_pages.load(Ordering::Acquire);
        loop {
            let next = cur.saturating_sub(pages);
            match self.used_pages.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        if pages > cur {
            log::error!(
                target: "vfs",
                "page quota {} ({} pages): released {pages} pages with only {cur} reserved",
                self.name,
                self.limit_pages
            );
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
pub static DETACHED_QUOTA: QuotaCounter = QuotaCounter::new("detached", DETACHED_QUOTA_PAGES);

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
    ///
    /// Lock 5 contract (as for [`write_bytes`](Self::write_bytes) and
    /// [`zero_range`](Self::zero_range)): the caller holds the owning
    /// inode's `PageCache` lock, or the page isn't published in a
    /// `PageCache` yet. That is what keeps kernel readers and writers of a
    /// page apart, so these stay inside `vfs`; code elsewhere copies a page
    /// out with [`copy_to_frame`](Self::copy_to_frame).
    pub(super) fn read_bytes(&self, off: usize, dst: &mut [u8]) {
        assert!(off.saturating_add(dst.len()) <= PAGE_SIZE);
        self.touch();
        if dst.is_empty() {
            return;
        }
        // SAFETY: `self.frame` stays alive for `&self`, the HHDM maps the
        // whole 4 KiB frame RW, and `off + dst.len() <= PAGE_SIZE`. `dst` is a
        // separate kernel buffer, so the ranges don't overlap. Kernel writers
        // of this page are excluded by the owning inode's Lock 5 (the caller
        // holds it, or the page isn't published in a `PageCache` yet).
        unsafe {
            core::ptr::copy_nonoverlapping(self.hhdm_ptr().add(off), dst.as_mut_ptr(), dst.len());
        }
    }

    /// Write bytes into `[off .. off + src.len())` via raw HHDM pointer.
    /// Lock 5 held by the caller, as for [`read_bytes`](Self::read_bytes).
    pub(super) fn write_bytes(&self, off: usize, src: &[u8]) {
        assert!(off.saturating_add(src.len()) <= PAGE_SIZE);
        self.touch();
        if src.is_empty() {
            return;
        }
        // SAFETY: `self.frame` stays alive for `&self`, the HHDM maps the
        // whole 4 KiB frame RW, and `off + src.len() <= PAGE_SIZE`. `src` is a
        // separate kernel buffer. Other kernel readers and writers of this
        // page are excluded by the owning inode's Lock 5 (the caller holds
        // it, or the page isn't published in a `PageCache` yet).
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), self.hhdm_ptr().add(off), src.len());
        }
    }

    /// Zero `[start .. end)` inside the 4 KiB frame. Lock 5 held by the
    /// caller, as for [`read_bytes`](Self::read_bytes).
    pub(super) fn zero_range(&self, start: usize, end: usize) {
        assert!(start <= end && end <= PAGE_SIZE);
        if start == end {
            return;
        }
        // SAFETY: `self.frame` stays alive for `&self`, the HHDM maps the
        // whole 4 KiB frame RW, and `start <= end <= PAGE_SIZE`. Other kernel
        // readers and writers are excluded by the owning inode's Lock 5,
        // which the caller holds.
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

    /// One `PRESENT | WRITABLE | PTE_SHARED` mapping of this page is gone.
    ///
    /// The page is marked dirty first (#266): stores made through the
    /// mapping never set `dirty` themselves (only the count kept the page
    /// dirty while it was mapped), so the last unmap must leave `dirty` set,
    /// or those stores look clean and reclaim drops them unwritten. Setting
    /// it before the decrement, on every call, leaves no instant where the
    /// count is 0 and `dirty` still false; while other writable mappings
    /// remain, the page is dirty through the count anyway.
    ///
    /// Call it only once no store can reach the page through that mapping:
    /// after its PTE is cleared (or made read-only) and every TLB that may
    /// still hold the writable entry is flushed (in M7, the one CPU's
    /// `invlpg`). A store landing later, through a stale entry, after a
    /// writeback cleared `dirty`, would be lost again.
    #[allow(dead_code)]
    #[inline]
    pub fn dec_writable_maps(&self) {
        self.dirty.store(true, Ordering::Release);
        let prev = self.writable_maps.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "writable_maps underflow");
    }

    /// Copy the page's first `valid` bytes into `dst` and zero the rest of
    /// `dst`: a `MAP_PRIVATE` copy-on-write split, where `valid` is what
    /// lies below the file's size (`PAGE_SIZE` for a page wholly inside it).
    ///
    /// Unlike the byte accessors this needs no Lock 5, and is the one way to
    /// a page's bytes from outside `vfs`: the page-fault path may not take
    /// the `PageCache` lock. A kernel `write_at` or a `MAP_SHARED` store
    /// racing the copy leaves some mix of old and new bytes in `dst`, as on
    /// Linux (see the module's comment); nothing else can go wrong, since
    /// the caller owns `dst`.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: file-backed MAP_PRIVATE CoW (#255)
    pub(crate) fn copy_to_frame(&self, dst: &mut OwnedFrame, valid: usize) {
        let valid = valid.min(PAGE_SIZE);
        let dst_ptr = mm::phys_to_virt(dst.frame().start()).as_mut_ptr::<u8>();
        // SAFETY: both frames are allocated 4 KiB RAM frames mapped RW through
        // the HHDM and alive for the borrows; they are distinct (`dst` is the
        // caller's, `self.frame` this page's), so the ranges don't overlap,
        // and `valid <= PAGE_SIZE`. `&mut OwnedFrame` means nothing else uses
        // `dst`'s frame.
        unsafe {
            core::ptr::copy_nonoverlapping(self.hhdm_ptr(), dst_ptr, valid);
            core::ptr::write_bytes(dst_ptr.add(valid), 0, PAGE_SIZE - valid);
        }
        self.touch();
    }
}

/// A file's cached page handed out for a mapping by [`mmap_page`].
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: file-backed mmap (#255)
pub(crate) struct MappedPage {
    /// The inode's cached page itself (not a copy). Holding this `Arc` pins
    /// it: reclaim evicts only pages whose strong count is 1.
    pub page: Arc<CachePage>,
    /// The file's size when the page was looked up (under the inode's Lock
    /// 4). The page starts below it; the page's bytes past it are zero
    /// unless a mapping stored there. A snapshot: a truncate may change the
    /// size once Lock 4 is released. A shrink takes `AddressSpace::inner`
    /// (Lock 1) before Lock 4 to invalidate mappings (ARCHITECTURE section
    /// 5), so a fault holding Lock 1 across the call and the mapping can
    /// rely on it; one that doesn't must look again.
    pub file_size: u64,
}

/// The cached page at `index` (file offset `index * 4096`) of a regular
/// `ext4` or `tmpfs` file, for mapping into an address space: the same
/// `Arc<CachePage>` that `read_at`/`write_at` use, so a mapping and the
/// file's reads and writes stay coherent. Another call for the same page
/// returns the same `Arc` while the page stays cached (which holding it
/// guarantees, short of a truncate dropping it).
///
/// - **Filling:** a page not cached yet is read from disk (`ext4`, by the
///   same code as `read_at`), or a zero page is allocated (a `tmpfs` hole,
///   or a hole in a detached `ext4` file), and inserted. Unlike `read_at`, a
///   hole costs a frame, so the `tmpfs` (or detached) quota can refuse it
///   with `NoSpc`. `NoMem` when no frame is free, `Io` when the disk read
///   fails, and `NoSpc` for a missing `ext4` page while the `ext4` cache is
///   at its hard limit (a mapping pins its pages, so reclaim can't make
///   room), as `write_at` refuses one.
/// - **EOF:** `Range` when the page starts at or past the file's size
///   (`index * 4096 >= file_size`, or the product overflows): there is no
///   page to map, and the fault path raises `SIGBUS`. The page holding the
///   end of the file is returned; its bytes past `file_size` are zero.
/// - **Other inodes:** `NoDev` for anything but a regular file of `ext4`
///   or `tmpfs` (directories, symlinks, `devfs`, `procfs`). `NoEnt` for an
///   `ext4` inode unlinked and not open, as `read_at` says.
/// - **Locking:** takes the inode's Lock 4 then Lock 5 (and Lock 6 and
///   below to read the disk), after an `ext4` soft-limit reclaim run with
///   none of them held, as `read_at` does. The caller may hold Lock 1
///   (`AddressSpace::inner`) or Lock 2, nothing numbered 3 or higher, and
///   no `SpinLock`.
///
/// It doesn't mark the page dirty or count a writable mapping: the caller
/// does both (`set_dirty`, `inc_writable_maps`) when it maps the page
/// writable.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: file-backed mmap (#255)
pub(crate) fn mmap_page(inode: &Arc<dyn VfsInode>, index: u64) -> Result<MappedPage, VfsError> {
    let any = inode.as_any().ok_or(VfsError::NoDev)?;
    if let Some(ext4) = any.downcast_ref::<super::ext4::Ext4Inode>() {
        return ext4.mmap_page(index);
    }
    if let Some(tmpfs) = any.downcast_ref::<super::tmpfs::TmpfsInode>() {
        return tmpfs.mmap_page(index);
    }
    Err(VfsError::NoDev)
}

/// Where page `index` starts, or `Range` when that is at or past
/// `file_size` (or past any file): [`mmap_page`]'s EOF rule, for each
/// filesystem's side of it.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: file-backed mmap (#255)
pub(super) fn mmap_page_offset(index: u64, file_size: u64) -> Result<u64, VfsError> {
    match index.checked_mul(PAGE_SIZE as u64) {
        Some(off) if off < file_size => Ok(off),
        _ => Err(VfsError::Range),
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

    /// Insert a newly allocated page at `page_index`, reserving `tmpfs`/
    /// `detached` quota if applicable. A page already cached at that index is
    /// left as it is (it may hold dirty data) and `Exist` returned, before
    /// any quota or count changes.
    pub fn insert_new(&mut self, page_index: u64, page: Arc<CachePage>) -> Result<(), VfsError> {
        if self.pages.contains_key(&page_index) {
            return Err(VfsError::Exist);
        }
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

    /// How many cached pages are dirty (written, or mapped writable, and not
    /// yet written back).
    pub fn dirty_count(&self) -> usize {
        self.pages.values().filter(|p| p.is_dirty()).count()
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

    /// Reserve `pages` more; `NoSpc` (as [`PageCache::insert_new`] reports
    /// the same quota) when it is used up.
    pub fn try_reserve(&mut self, pages: usize) -> Result<(), VfsError> {
        if DETACHED_QUOTA.try_reserve(pages) {
            self.reserved += pages;
            Ok(())
        } else {
            Err(VfsError::NoSpc)
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

/// Soft global reclaim for `ext4` `PageCache` frames when above the soft
/// limit ([`EXT4_SOFT_LIMIT_PAGES`]).
///
/// The caller holds no lock at level 3 or above (§9: reclaim takes Lock 9's
/// snapshot, then 4 -> 5 -> 6 -> 7 -> 8), with interrupts enabled.
pub fn maybe_reclaim_ext4_pages() {
    let soft = ext4_soft_limit();
    if EXT4_CACHED_PAGES.load(Ordering::Relaxed) <= soft {
        HARD_LIMIT_WARNED.store(false, Ordering::Release);
        return;
    }
    reclaim_ext4_to_target(soft);
}

/// Reclaim unpinned `ext4` `PageCache` frames down to `target_pages`. Same
/// precondition as [`maybe_reclaim_ext4_pages`].
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

crate::ktest!(
    fn page_cache_refuses_a_second_page_at_one_index() {
        use crate::{kassert, kassert_eq};
        let mut pc = PageCache::new_ext4();
        let before = ext4_cached_pages();
        let first = Arc::new(CachePage::new_zeroed().ok_or("frame")?);
        first.write_bytes(0, b"dirty");
        first.set_dirty();
        pc.insert_new(7, Arc::clone(&first)).map_err(|_| "insert")?;
        let second = Arc::new(CachePage::new_zeroed().ok_or("frame")?);
        kassert_eq!(pc.insert_new(7, second).err(), Some(VfsError::Exist));
        kassert_eq!(ext4_cached_pages(), before + 1); // counted once
        kassert!(Arc::ptr_eq(pc.get(7).ok_or("page")?, &first)); // the dirty page kept
        drop(pc);
        kassert_eq!(ext4_cached_pages(), before);
        Ok(())
    }
);

crate::ktest!(
    fn mmap_page_refuses_inodes_of_other_filesystems() {
        // Only ext4's and tmpfs's regular files have pages to map: a devfs
        // device and a procfs file (a regular file, but not cached) don't.
        use crate::kassert_eq;
        use lumen_vfs::{DevId, FileSystem, InodeType};
        let devfs = super::devfs::Devfs::new(DevId::makedev(0, 126));
        let zero = devfs.root_inode().lookup("zero").map_err(|_| "lookup zero")?;
        kassert_eq!(mmap_page(&zero, 0).err(), Some(VfsError::NoDev));
        let procfs = super::procfs::Procfs::new(DevId::makedev(0, 127));
        let version = procfs.root_inode().lookup("version").map_err(|_| "lookup version")?;
        kassert_eq!(version.inode_type(), InodeType::File);
        kassert_eq!(mmap_page(&version, 0).err(), Some(VfsError::NoDev));
        Ok(())
    }
);

crate::ktest!(
    fn cache_page_copies_to_a_frame_zeroing_past_valid() {
        // `copy_to_frame` (a copy-on-write split): the page's first `valid`
        // bytes, zeros after them, whatever the frame held before.
        use crate::kassert;
        let page = CachePage::new_zeroed().ok_or("frame")?;
        let bytes: Vec<u8> = (0..PAGE_SIZE).map(|i| (i % 251) as u8 | 1).collect();
        page.write_bytes(0, &bytes);
        let mut dst = frame::alloc_owned().ok_or("dst frame")?;
        let dst_ptr = mm::phys_to_virt(dst.frame().start()).as_mut_ptr::<u8>();
        let read_dst = |out: &mut [u8; PAGE_SIZE]| {
            // SAFETY: `dst` is an allocated frame this test owns, mapped
            // through the HHDM; no reference to it is live while copying.
            unsafe { core::ptr::copy_nonoverlapping(dst_ptr, out.as_mut_ptr(), PAGE_SIZE) };
        };
        // SAFETY: as above; filling it with junk first.
        unsafe { core::ptr::write_bytes(dst_ptr, 0xa5, PAGE_SIZE) };
        page.copy_to_frame(&mut dst, 1000);
        let mut got = [0u8; PAGE_SIZE];
        read_dst(&mut got);
        kassert!(got[..1000] == bytes[..1000]);
        kassert!(got[1000..].iter().all(|&b| b == 0));
        page.copy_to_frame(&mut dst, PAGE_SIZE);
        read_dst(&mut got);
        kassert!(got[..] == bytes[..]);
        Ok(())
    }
);

crate::ktest!(
    fn page_quota_release_saturates_and_detached_quota_reports_nospc() {
        use crate::kassert_eq;
        let q = QuotaCounter::new("ktest", 4);
        kassert_eq!(q.try_reserve(2), true);
        q.release(5); // a bug elsewhere: more than reserved
        kassert_eq!(q.used_pages(), 0);
        kassert_eq!(q.try_reserve(4), true); // not wrapped to "full"
        let mut g = DetachedQuotaGuard::new();
        kassert_eq!(g.try_reserve(DETACHED_QUOTA_PAGES + 1).err(), Some(VfsError::NoSpc));
        Ok(())
    }
);

crate::ktest!(
    fn page_cache_soft_limit_triggers_reclaim() {
        // Past the soft limit, the next ext4 read or write reclaims (writing
        // dirty pages back) before it runs, through the global tables. The
        // limits in force outside this test: 32 MiB soft, twice that hard.
        crate::kassert_eq!(EXT4_SOFT_LIMIT_PAGES * PAGE_SIZE, 32 << 20);
        crate::kassert_eq!(EXT4_HARD_LIMIT_PAGES, 2 * EXT4_SOFT_LIMIT_PAGES);
        use crate::vfs::{EXT4_FILESYSTEMS, TABLES, ext4};
        use crate::{kassert, kassert_eq};
        use lumen_bcache::BlockCache;
        use lumen_vfs::{FileSystem, OpenFlags, mkdir_at, open_inode, unlink_at};

        let img = crate::vfs::build_minimal_ext4_image_blocks(256);
        let dev = crate::vfs::MemBlkDev::new(img, false);
        let bcache: Arc<dyn ext4::BlockCacheBackend> =
            Arc::new(crate::sync::SleepMutex::new(BlockCache::new(dev, 64).map_err(|_| "bcache")?));
        let devid = lumen_vfs::DevId::makedev(0, 231);
        let fs = ext4::Ext4FileSystem::mount(Arc::clone(&bcache), devid, false).map_err(|_| "mount")?;
        let mp = mkdir_at(&TABLES, None, b"/tmp/soft_limit_mnt", 0o755, 0).map_err(|_| "mkdir mnt")?;
        let (mp_dev, mp_ino) = (mp.dev_id(), mp.ino());
        EXT4_FILESYSTEMS.lock().push(Arc::clone(&fs));
        TABLES.mount(mp, "/tmp/soft_limit_mnt", Arc::clone(&fs) as Arc<dyn FileSystem>).map_err(|_| "mount")?;

        let result = (|| -> Result<(), &'static str> {
            let (f, _) = open_inode(
                &TABLES,
                None,
                b"/tmp/soft_limit_mnt/f",
                OpenFlags::from_bits_truncate(OpenFlags::O_RDWR | OpenFlags::O_CREAT | OpenFlags::O_TRUNC),
                0o644,
                0,
            )
            .map_err(|_| "create")?;
            let data: Vec<u8> = (0..6 * PAGE_SIZE).map(|i| (i % 251) as u8 | 1).collect();
            kassert_eq!(f.write_at(0, &data).map_err(|_| "write")?, data.len());
            let base = ext4_cached_pages();
            kassert!(base >= 6);
            set_ext4_limits_for_test(Some((base - 4, usize::MAX)));
            let mut b = [0u8; 1];
            let r = f.read_at(0, &mut b);
            let after = ext4_cached_pages();
            set_ext4_limits_for_test(None);
            r.map_err(|_| "read")?;
            kassert!(after <= base - 4 + 1); // reclaimed, plus at most the page read back
            // The file was never synced, and its pages were all dirty: the
            // data is on the disk only if reclaim wrote it back. A read-only
            // mount of the same block cache reads the disk, not the pages.
            let ro = ext4::Ext4FileSystem::mount(Arc::clone(&bcache), lumen_vfs::DevId::makedev(0, 232), true)
                .map_err(|_| "ro mount")?;
            let on_disk = ro.root_inode().lookup("f").map_err(|_| "ro lookup")?;
            let mut disk = alloc::vec![0u8; data.len()];
            let n = on_disk.read_at(0, &mut disk).map_err(|_| "ro read")?;
            kassert!(n >= (data.len() - 4 * PAGE_SIZE)); // at least the reclaimed pages
            kassert_eq!(&disk[..n], &data[..n]);
            let mut back = alloc::vec![0u8; data.len()];
            kassert_eq!(f.read_at(0, &mut back).map_err(|_| "read back")?, data.len());
            kassert_eq!(back, data); // the dirty pages were written, not dropped
            Ok(())
        })();
        let umounted = TABLES.umount(mp_dev, mp_ino).map(drop).map_err(|_| "umount");
        EXT4_FILESYSTEMS.lock().retain(|e| e.dev_id() != devid);
        let unlinked = unlink_at(&TABLES, None, b"/tmp/soft_limit_mnt", true).map(drop).map_err(|_| "rmdir mnt");
        result.and(umounted).and(unlinked)
    }
);
