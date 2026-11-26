//! Unified 4 KiB frame-backed file `PageCache` (shared by `ext4` and `tmpfs`).
//!
//! Each cached page wraps an [`OwnedFrame`](crate::mm::frame::OwnedFrame)
//! accessed through the HHDM with raw pointers, so no `&mut [u8]` to the
//! frame ever exists. Raw pointers avoid reference aliasing, not data races:
//! kernel readers and writers of a published page are serialized by the
//! owning inode's `PageCache` lock (Lock 5), including a copy-on-write
//! split's copy ([`MappedPage::copy_to_frame`] takes it). A `MAP_SHARED`
//! user mapping (#255, through [`mmap_page`]) writes the frame from ring 3
//! without that lock: those are hardware stores outside Rust's memory model,
//! and a kernel copy racing one reads some mix of old and new bytes, as on
//! Linux. Kernel accesses never race each other. `CachePage::drop` and `PageCache::drop` take
//! no sleeping lock (`OwnedFrame::drop` takes only Lock 10, the `frame`
//! `SpinLock`); the one exception is a quota over-release, a bug elsewhere,
//! whose `error!` takes the log's leaf lock.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use lumen_mm::PhysFrame;
use lumen_vfs::page::{self, PageSpan};
use lumen_vfs::{DevId, VfsError, VfsInode};

use crate::arch;
use crate::mm;
use crate::mm::frame::{self, OwnedFrame};
use crate::mm::user_vm::AddressSpace;
use crate::sync::{SleepMutex, SpinLock};

pub use lumen_vfs::page::PAGE_SIZE;

/// Soft global reclaim target for live `ext4` page-cache frames (32 MiB = 8192 pages).
pub const EXT4_SOFT_LIMIT_PAGES: usize = 8192;
/// Hard limit on live `ext4` page-cache frames (64 MiB): past it, after a
/// reclaim attempt, `ext4` writes that need a new page fail with `-ENOSPC`
/// (or return short). Reclaim can only stay above it when writeback keeps
/// failing (a full or failing disk) or pages are pinned, so this bounds the
/// memory a writer can fill with pages that never reach the disk.
pub const EXT4_HARD_LIMIT_PAGES: usize = 2 * EXT4_SOFT_LIMIT_PAGES;
/// Default `tmpfs` memory quota (32 MiB = 8192 pages' worth), which its
/// pages share with, since #267, its inodes, names and symlink targets.
/// `vfs::init` gives it to `/tmp` and to a diskless `/`; `mount` shows it,
/// in bytes, as their `limit`, which `test_fs.py`'s `TMPFS_LIMIT` copies.
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
/// ([`EXT4_HARD_LIMIT_PAGES`]), so no new page reclaim can't drop (a dirty
/// one, or a mapping's) may be added: [`PageCache::admit_unreclaimable`].
fn ext4_at_hard_limit() -> bool {
    EXT4_CACHED_PAGES.load(Ordering::Relaxed) >= EXT4_HARD_LIMIT.load(Ordering::Relaxed)
}

/// A new page was refused at the hard limit since the cache last fell back
/// under the soft limit: one warning per crossing.
static HARD_LIMIT_WARNED: AtomicBool = AtomicBool::new(false);

/// Log (once per crossing of the hard limit) that an `ext4` page was
/// refused, to a write or a mapping ([`PageCache::admit_unreclaimable`]).
/// The cache is full of pages reclaim can't drop: dirty pages whose
/// writeback keeps failing (its own warning names the inode) or that
/// belong to a volume a failed write made read-only (#349: never written
/// back, so never freed until reboot; that volume's own ERROR line says
/// so), or pages pinned by mappings, which only unmapping frees. Writes and new mapped
/// pages of any linked file (detached files use their own quota) now fail
/// with `ENOSPC`.
fn note_hard_limit_refusal() {
    if !HARD_LIMIT_WARNED.swap(true, Ordering::AcqRel) {
        log::warn!(
            target: "vfs",
            "ext4 page cache at its hard limit ({} pages): refusing new pages (ENOSPC) until writeback or unmapping frees some",
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

/// Lock-free atomic memory quota, counted in bytes: `tmpfs`'s (its pages,
/// and since #267 its inodes, directory entries and symlink targets) and
/// Gate 6 detach's (pages only). The `_pages` methods count whole pages.
/// The count never passes the limit. A quota dropped with bytes still
/// reserved logs an error: its owner leaked them.
#[derive(Debug)]
pub struct QuotaCounter {
    /// What the quota is for, named in its log lines.
    name: &'static str,
    /// The mount it bounds (a `tmpfs`'s), also named in its log lines.
    dev: Option<DevId>,
    limit_bytes: usize,
    used_bytes: AtomicUsize,
}

impl QuotaCounter {
    /// A quota of `limit_pages` pages.
    pub const fn new_pages(name: &'static str, limit_pages: usize) -> Self {
        Self { name, dev: None, limit_bytes: limit_pages.saturating_mul(PAGE_SIZE), used_bytes: AtomicUsize::new(0) }
    }

    /// The quota of `limit_bytes` bytes bounding the mount `dev`.
    pub const fn for_mount(name: &'static str, dev: DevId, limit_bytes: usize) -> Self {
        Self { name, dev: Some(dev), limit_bytes, used_bytes: AtomicUsize::new(0) }
    }

    /// Reserve `pages` pages, or `false` if that would pass the limit. A
    /// `true` must be released later, or the pages leak from the quota.
    #[must_use]
    pub fn try_reserve_pages(&self, pages: usize) -> bool {
        pages.checked_mul(PAGE_SIZE).is_some_and(|bytes| self.try_reserve_bytes(bytes))
    }

    /// Reserve `bytes`, or `false` if that would pass the limit. A `true`
    /// must be released later, or the bytes leak from the quota.
    #[must_use]
    pub fn try_reserve_bytes(&self, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        let mut cur = self.used_bytes.load(Ordering::Acquire);
        loop {
            let Some(next) = cur.checked_add(bytes) else {
                return false;
            };
            if next > self.limit_bytes {
                return false;
            }
            match self.used_bytes.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Return `pages` pages to the quota, as [`Self::release_bytes`].
    pub fn release_pages(&self, pages: usize) {
        self.release_bytes(pages.saturating_mul(PAGE_SIZE));
    }

    /// Return `bytes` to the quota. Releasing more than is reserved is a
    /// bug elsewhere: the count saturates at zero (logged) rather than
    /// wrapping, which would make every later reservation fail.
    pub fn release_bytes(&self, bytes: usize) {
        if bytes == 0 {
            return;
        }
        let mut cur = self.used_bytes.load(Ordering::Acquire);
        loop {
            let next = cur.saturating_sub(bytes);
            match self.used_bytes.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => break,
                Err(actual) => cur = actual,
            }
        }
        if bytes > cur {
            log::error!(target: "vfs", "quota {self}: released {bytes} bytes with only {cur} reserved");
        }
    }

    /// The whole pages' worth reserved: exact for a quota only ever
    /// reserved in pages (the detached one).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    pub fn used_pages(&self) -> usize {
        self.used_bytes() / PAGE_SIZE
    }

    pub fn used_bytes(&self) -> usize {
        self.used_bytes.load(Ordering::Acquire)
    }

    /// The most that may be reserved at once.
    pub const fn limit_bytes(&self) -> usize {
        self.limit_bytes
    }
}

/// `name`, the mount if one, and the limit: `tmpfs 0:2 (33554432 bytes)`.
impl fmt::Display for QuotaCounter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name)?;
        if let Some(dev) = self.dev {
            write!(f, " {dev}")?;
        }
        write!(f, " ({} bytes)", self.limit_bytes)
    }
}

impl Drop for QuotaCounter {
    /// A quota goes when the last of its owners (a `tmpfs` mount, its
    /// inodes and directories, their pages and charges) has gone, each
    /// having given back what it held: bytes still reserved were leaked by
    /// an owner that dropped without giving them back. An owner leaked
    /// itself keeps the quota alive, so that logs nothing. Takes no
    /// sleeping lock (the leak's `error!` takes the log's leaf lock).
    fn drop(&mut self) {
        let left = *self.used_bytes.get_mut();
        if left != 0 {
            log::error!(target: "vfs", "quota {self}: dropped with {left} bytes still reserved");
        }
    }
}

/// Global Gate 6 quota for open-unlinked detached `ext4` inodes.
pub static DETACHED_QUOTA: QuotaCounter = QuotaCounter::new_pages("detached", DETACHED_QUOTA_PAGES);

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
    /// A fresh 4 KiB [`CachePage`], zeroed and in no cache yet: `NoMem`
    /// when no frame is free.
    pub(super) fn new_zeroed() -> Result<Self, VfsError> {
        let frame = frame::alloc_owned().ok_or(VfsError::NoMem)?;
        let ptr = mm::phys_to_virt(frame.frame().start()).as_mut_ptr::<u8>();
        // SAFETY: `frame` is a freshly allocated, uniquely owned 4 KiB RAM frame
        // mapped RW through the HHDM.
        unsafe { core::ptr::write_bytes(ptr, 0, PAGE_SIZE) };
        Ok(Self {
            frame,
            dirty: AtomicBool::new(false),
            writable_maps: AtomicU32::new(0),
            last_access: AtomicU64::new(ACCESS_SEQ.fetch_add(1, Ordering::Relaxed)),
        })
    }

    /// Underlying physical frame, for a zero-copy `PTE_SHARED` user mapping
    /// of the page (#255).
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
    pub(super) fn touch(&self) {
        self.last_access.store(ACCESS_SEQ.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
    }

    #[inline]
    pub(super) fn last_access(&self) -> u64 {
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
    fn zero_range(&self, start: usize, end: usize) {
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
    ///
    /// The count is loaded first (Acquire), then `dirty`. Reclaim (and
    /// `fsync`'s `collect_dirty`) calls this holding Locks 4 and 5, neither
    /// of which an unmap's [`dec_writable_maps`] takes, and can be preempted
    /// between the loads. Read the other way
    /// round, it could see `dirty` still clear, then the count already 0,
    /// and evict a page whose mapped stores were never written. A count of
    /// 0 read with Acquire synchronises with that decrement, so the `dirty`
    /// it set first is then visible.
    ///
    /// [`dec_writable_maps`]: Self::dec_writable_maps
    #[inline]
    pub fn is_dirty(&self) -> bool {
        let mapped_writable = self.writable_maps.load(Ordering::Acquire) > 0;
        #[cfg(feature = "ktest")]
        ktest_hooks::between_dirty_loads(self);
        mapped_writable || self.dirty.load(Ordering::Acquire)
    }

    /// Mark the page dirty: a kernel write, or (through [`WritableMap`]) a
    /// writable `MAP_SHARED` mapping made or found dirty.
    #[inline]
    pub(super) fn set_dirty(&self) {
        self.dirty.store(true, Ordering::Release);
    }

    /// Atomically swap `dirty`, returning the previous value: writeback's
    /// clear, `vfs` only.
    #[inline]
    pub(super) fn swap_dirty(&self, val: bool) -> bool {
        self.dirty.swap(val, Ordering::AcqRel)
    }

    /// How many `PRESENT | WRITABLE | PTE_SHARED` mappings the page has.
    #[inline]
    pub fn writable_maps(&self) -> u32 {
        self.writable_maps.load(Ordering::Acquire)
    }

    /// One more `PRESENT | WRITABLE | PTE_SHARED` mapping of this page.
    /// Outside the ktests only [`MappedPage::map_writable`] calls it, with
    /// [`set_dirty`](Self::set_dirty), and the [`WritableMap`] it returns
    /// makes the matching [`dec_writable_maps`](Self::dec_writable_maps).
    #[inline]
    pub(super) fn inc_writable_maps(&self) {
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
    /// writeback cleared `dirty`, would be lost again (ARCHITECTURE section
    /// 5's unmap order). Outside the ktests only [`WritableMap`]'s `Drop`
    /// calls it, and the VM core drops one only after that flush.
    #[inline]
    pub(super) fn dec_writable_maps(&self) {
        self.dirty.store(true, Ordering::Release);
        let prev = self.writable_maps.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0, "writable_maps underflow");
    }

    /// Copy the page's first `valid` bytes into `dst` and zero the rest of
    /// `dst`: a `MAP_PRIVATE` copy-on-write split, where `valid` is what
    /// lies below the file's size (`PAGE_SIZE` for a page wholly inside it).
    ///
    /// Lock 5 contract, as for [`read_bytes`](Self::read_bytes): the caller
    /// holds the owning inode's `PageCache` lock, so no kernel `write_at`
    /// writes the page meanwhile. Outside `vfs`,
    /// [`MappedPage::copy_to_frame`] takes that lock and calls this. A
    /// `MAP_SHARED` store from ring 3 racing the copy is a hardware write
    /// outside Rust's memory model and leaves some mix of old and new bytes
    /// in `dst`, as on Linux.
    pub(super) fn copy_to_frame(&self, dst: &mut OwnedFrame, valid: usize) {
        let valid = valid.min(PAGE_SIZE);
        let dst_ptr = mm::phys_to_virt(dst.frame().start()).as_mut_ptr::<u8>();
        // SAFETY: both frames are allocated 4 KiB RAM frames mapped RW through
        // the HHDM and alive for the borrows; they are distinct (`dst` is the
        // caller's, `self.frame` this page's), so the ranges don't overlap,
        // and `valid <= PAGE_SIZE`. `&mut OwnedFrame` means nothing else uses
        // `dst`'s frame. Kernel writers of this page are excluded by the
        // owning inode's Lock 5, which the caller holds.
        unsafe {
            core::ptr::copy_nonoverlapping(self.hhdm_ptr(), dst_ptr, valid);
            core::ptr::write_bytes(dst_ptr.add(valid), 0, PAGE_SIZE - valid);
        }
        self.touch();
    }
}

/// A file's cached page handed out for a mapping by [`mmap_page`].
pub(crate) struct MappedPage {
    page: Arc<CachePage>,
    /// The page's index in the file (file offset `index * 4096`).
    index: u64,
    file_size: u64,
    /// The inode whose cache holds `page`: an `ext4` or `tmpfs` inode, as
    /// [`mmap_page`] checked, for [`Self::copy_to_frame`]'s lock.
    owner: Arc<dyn VfsInode>,
}

impl MappedPage {
    /// Only [`mmap_page`] makes one, once `owner` has downcast to an `ext4`
    /// or `tmpfs` inode.
    fn new(page: Arc<CachePage>, index: u64, file_size: u64, owner: Arc<dyn VfsInode>) -> Self {
        Self { page, index, file_size, owner }
    }

    /// How many of the page's bytes lie below [`file_size`](Self::file_size):
    /// `PAGE_SIZE` for a page wholly inside the file, fewer for the page
    /// holding its end ([`mmap_page`] hands out no page starting at or past
    /// it).
    pub(crate) fn valid_len(&self) -> usize {
        page::valid_len(self.index, self.file_size)
    }

    /// Copy the page's bytes below the file's size ([`valid_len`]) into
    /// `dst` and zero the rest of `dst`: a `MAP_PRIVATE` copy-on-write
    /// split. The length comes from the page's own index and the size read
    /// with it, so a caller can't pass a wrong one; a fault that needs the
    /// size as it is now (the file may have grown since the page was
    /// mapped) looks the page up again with [`mmap_page`] first.
    ///
    /// [`valid_len`]: Self::valid_len
    ///
    /// The copy holds the owning inode's Lock 5, as the kernel's writers of
    /// the page (`write_at`, truncate's zeroing) do, so no kernel write
    /// races it. A `MAP_SHARED` store from ring 3 is a hardware write
    /// outside Rust's memory model; one racing the copy leaves some mix of
    /// old and new bytes in `dst`, as on Linux. Lock order: the caller may
    /// hold Lock 1 (`AddressSpace::inner`, as the fault path does) or Lock 2,
    /// nothing numbered 3 or higher, and no `SpinLock`. This is `mm`'s one
    /// way to copy a page's bytes; it maps a page zero-copy through
    /// [`CachePage::phys_frame`].
    pub(crate) fn copy_to_frame(&self, dst: &mut OwnedFrame) {
        let lock = page_cache_lock(&*self.owner).expect("a MappedPage's owner is an ext4 or tmpfs inode");
        let _cache = lock.lock();
        self.page.copy_to_frame(dst, self.valid_len());
    }

    /// Whether the page's volume has gone read-only since the mapping was
    /// made (`errors=remount-ro`, #349): a store fault on it is then a bus
    /// error, as Linux's `page_mkwrite` refuses on a shut-down `ext4`.
    pub(crate) fn volume_read_only(&self) -> bool {
        mapped_read_only(&*self.owner)
    }

    /// Count one `PRESENT | WRITABLE | PTE_SHARED` mapping of the page: mark
    /// it dirty and raise its writable-mapping count together, before the
    /// caller makes the PTE writable. The returned [`WritableMap`] lowers
    /// the count again when dropped, which the VM core does only after the
    /// PTE is cleared or made read-only and the TLB flushed (ARCHITECTURE
    /// section 5's unmap order). This is the one way to count a mapping.
    #[must_use = "dropping the guard uncounts the mapping at once"]
    pub(crate) fn map_writable(&self) -> WritableMap {
        self.page.set_dirty();
        self.page.inc_writable_maps();
        WritableMap { page: Arc::clone(&self.page) }
    }

    /// The inode's cached page itself (not a copy). Holding the `Arc` (a
    /// clone of it) pins the page: reclaim evicts only pages whose strong
    /// count is 1.
    pub(crate) fn page(&self) -> &Arc<CachePage> {
        &self.page
    }

    /// The file's size when the page was looked up (under the inode's Lock
    /// 4). The page starts below it; the page's bytes past it are zero
    /// unless a mapping stored there. A snapshot: a truncate may change the
    /// size once Lock 4 is released, even while the fault holds
    /// `AddressSpace::inner` (Lock 1), since the truncate takes Lock 4 alone.
    /// A page mapped from a stale snapshot is unmapped all the same: the
    /// shrinking syscall's `invalidate_file_from` takes Lock 1 after the
    /// truncate, and so runs once that fault's mapping is in place
    /// (ARCHITECTURE section 5's rules: the caller's own space only until
    /// M8). A private copy-on-write copy of the end
    /// page made from a stale snapshot can keep bytes past the new end
    /// (bytes a shared mapping stored past it). (The fault path needs only
    /// [`valid_len`](Self::valid_len), through `copy_to_frame`.)
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // the ktests' check of the snapshot
    pub(crate) fn file_size(&self) -> u64 {
        self.file_size
    }
}

/// One `PRESENT | WRITABLE | PTE_SHARED` user mapping of a cached page,
/// counted in its `writable_maps` while the guard lives (#255).
///
/// Only [`MappedPage::map_writable`] makes one, marking the page dirty and
/// raising the count together; `Drop` lowers the count (which marks the
/// page dirty first, #266). `Drop` can't know whether the PTE is gone and
/// the TLB flushed, so the VM core keeps each guard beside its PTE and
/// drops it only after clearing or downgrading the PTE and flushing
/// (ARCHITECTURE section 5's unmap order). A `fork` that copies a
/// writable shared PTE would count it again with a second guard of its
/// own. `Drop` takes no lock (in the ktests, it takes the release-order
/// recorder's only while a test records).
pub(crate) struct WritableMap {
    page: Arc<CachePage>,
}

impl WritableMap {
    /// The PTE's hardware `DIRTY` bit was found set: mark the page dirty.
    /// (The guard marks it when made and when dropped as well, so this only
    /// says it sooner, for a walk that downgrades or clears the PTE, like
    /// `mprotect`'s.)
    pub(crate) fn note_pte_dirty(&self) {
        self.page.set_dirty();
    }
}

impl Drop for WritableMap {
    fn drop(&mut self) {
        self.page.dec_writable_maps();
        crate::mm::user_vm::release_order::record(
            self.page.phys_frame().as_u64(),
            crate::mm::user_vm::release_order::Step::Released,
        );
    }
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
///   fails (logged, naming the page), and `NoSpc` for a missing page of a
///   linked `ext4` file while the `ext4` cache is at its hard limit (a
///   mapping pins its pages, so reclaim can't make room), asked before the
///   fill and again after it, by the same check as a write's new page
///   ([`PageCache::map_page`]). A detached `ext4` file's pages count
///   against the detached quota instead of the hard limit.
/// - **EOF:** `Range` when the page starts at or past the file's size
///   (`index * 4096 >= file_size`, or the product overflows): there is no
///   page to map, and the fault path raises `SIGBUS`. The page holding the
///   end of the file is returned; its bytes past `file_size` are zero
///   unless a mapping stored there.
/// - **Truncate:** a shrink's [`PageCache::resize`] drops the cache's
///   reference to the pages past the new size and stops counting them, even
///   one a mapping still holds. The page stays alive, pinned by that mapping,
///   until ARCHITECTURE section 5's truncate invalidation, which the
///   shrinking syscall (`truncate`, `ftruncate`, `openat(O_TRUNC)`) runs
///   once the truncate has returned, unmaps it (with section 5's limit: the
///   caller's own space only until M8).
/// - **Other inodes:** `NoDev` for anything but a regular file of `ext4`
///   or `tmpfs` (directories, symlinks, `devfs`, `procfs`). `NoEnt` for an
///   `ext4` inode unlinked and not open, as `read_at` says.
/// - **Locking:** takes the inode's Lock 4 then Lock 5 (and Locks 6–8 to
///   read the disk), after an `ext4` soft-limit reclaim run with
///   none of them held, as `read_at` does. The caller may hold Lock 1
///   (`AddressSpace::inner`) or Lock 2, nothing numbered 3 or higher, and
///   no `SpinLock`.
///
/// It doesn't mark the page dirty or count a writable mapping: the caller
/// does both, through [`MappedPage::map_writable`], when it maps the page
/// writable.
pub(crate) fn mmap_page(inode: &Arc<dyn VfsInode>, index: u64) -> Result<MappedPage, VfsError> {
    let any = inode.as_any().ok_or(VfsError::NoDev)?;
    let (page, file_size) = if let Some(ext4) = any.downcast_ref::<super::ext4::Ext4Inode>() {
        ext4.mmap_page(index)?
    } else if let Some(tmpfs) = any.downcast_ref::<super::tmpfs::TmpfsInode>() {
        tmpfs.mmap_page(index)?
    } else {
        return Err(VfsError::NoDev);
    };
    Ok(MappedPage::new(page, index, file_size, Arc::clone(inode)))
}

/// Whether [`mmap_page`] can hand out pages of `inode`: a regular file of
/// `ext4` or `tmpfs`. (`mmap` refuses anything else with `ENODEV` up front,
/// instead of a `SIGBUS` at the first fault.)
pub(crate) fn mappable(inode: &dyn VfsInode) -> bool {
    inode.inode_type() == lumen_vfs::InodeType::File && page_cache_lock(inode).is_some()
}

/// Whether `inode`'s volume refuses writing through a shared mapping: a new
/// writable one (`EROFS`), a store fault or promotion on an older one (a
/// bus error) and `mprotect(+W)` (`EACCES`). An `ext4` one read-only,
/// mounted so or errored since its descriptor was opened `O_RDWR`
/// (`errors=remount-ro`, #349), unless the file is detached (open but
/// unlinked: all in memory, as its `write` is). Asked of the inode's own
/// filesystem, so a volume outside the mount table (a ktest's) answers too;
/// `tmpfs` never refuses.
pub(crate) fn mapped_read_only(inode: &dyn VfsInode) -> bool {
    inode
        .as_any()
        .and_then(|any| any.downcast_ref::<super::ext4::Ext4Inode>())
        .is_some_and(super::ext4::Ext4Inode::volume_read_only)
}

/// The `PageCache` lock (Lock 5) of an `ext4` or `tmpfs` inode.
fn page_cache_lock(inode: &dyn VfsInode) -> Option<&SleepMutex<PageCache>> {
    let any = inode.as_any()?;
    if let Some(ext4) = any.downcast_ref::<super::ext4::Ext4Inode>() {
        return Some(ext4.page_cache_lock());
    }
    any.downcast_ref::<super::tmpfs::TmpfsInode>().map(super::tmpfs::TmpfsInode::page_cache_lock)
}

/// The address spaces currently mapping an `ext4` or `tmpfs` inode (level 10c
/// leaf spinlock; never acquired under [`page_cache_lock`]).
pub(crate) fn page_cache_mappers(inode: &dyn VfsInode) -> Option<&SpinLock<Vec<Weak<AddressSpace>>>> {
    let any = inode.as_any()?;
    if let Some(ext4) = any.downcast_ref::<super::ext4::Ext4Inode>() {
        return Some(ext4.mappers());
    }
    any.downcast_ref::<super::tmpfs::TmpfsInode>().map(super::tmpfs::TmpfsInode::mappers)
}

/// `Range` unless page `index` starts below `file_size` (an overflowing
/// offset is past any file): [`mmap_page`]'s EOF rule, for
/// [`PageCache::map_page`].
fn mmap_page_in_file(index: u64, file_size: u64) -> Result<(), VfsError> {
    match index.checked_mul(PAGE_SIZE as u64) {
        Some(off) if off < file_size => Ok(()),
        _ => Err(VfsError::Range),
    }
}

/// Test seams for the ktests (none in the real kernel).
#[cfg(feature = "ktest")]
pub(super) mod ktest_hooks {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use super::CachePage;

    /// The page (its address) whose next `is_dirty` drops one writable
    /// mapping between its two loads, as an unmap preempting reclaim there
    /// would; 0 for none.
    static DEC_BETWEEN_DIRTY_LOADS: AtomicUsize = AtomicUsize::new(0);

    /// Make `page`'s next `is_dirty` call `dec_writable_maps` between its
    /// loads. Only that page is affected, whatever else runs meanwhile.
    pub fn dec_between_dirty_loads(page: &CachePage) {
        DEC_BETWEEN_DIRTY_LOADS.store(core::ptr::from_ref(page).addr(), Ordering::Release);
    }

    pub(super) fn between_dirty_loads(page: &CachePage) {
        let me = core::ptr::from_ref(page).addr();
        if DEC_BETWEEN_DIRTY_LOADS.compare_exchange(me, 0, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            page.dec_writable_maps();
        }
    }
}

/// Scratch files on fresh `tmpfs` and `ext4` volumes for the file-backed VM's
/// ktests (`mm::user_vm`) and the positional I/O and `fsync` ktests
/// (`linux::syscall::io`), which can't reach `vfs`'s in-memory block device
/// themselves. Nothing here mounts itself in the global tables (a ktest
/// that wants one there mounts `Ext4Volume::fs` itself, and unmounts it).
#[cfg(feature = "ktest")]
pub(crate) mod ktest_files {
    use alloc::sync::Arc;
    use core::sync::atomic::Ordering;

    use lumen_vfs::{DevId, FileSystem, VfsInode};

    use super::super::build_minimal_ext4_image_blocks;
    use super::super::ext4::ktest_util::{self, Mnt};
    use super::super::ext4::{Ext4FileSystem, Ext4Inode, LatchableCache};

    /// A fresh writable `ext4` volume (1 MiB) on an in-memory disk, behind
    /// `ext4::ktest_util`'s fault-injecting block cache.
    pub(crate) struct Ext4Volume {
        mnt: Mnt,
        pub(crate) root: Arc<dyn VfsInode>,
    }

    impl Ext4Volume {
        pub(crate) fn new(minor: u32) -> Result<Self, &'static str> {
            let mnt = ktest_util::mount(build_minimal_ext4_image_blocks(256), minor)?;
            Ok(Self { root: Arc::clone(&mnt.root), mnt })
        }

        /// The mounted filesystem (to mount it in the global tables).
        pub(crate) fn fs(&self) -> Arc<dyn FileSystem> {
            Arc::clone(&self.mnt.fs) as Arc<dyn FileSystem>
        }

        /// The root of a read-only mount of the same disk (through the same
        /// block cache): what it reads is on the disk, not in the first
        /// mount's page cache. (`ext4::ktest_util::remount_ro`, by contrast,
        /// syncs the block cache and mounts a copy of the device; see also
        /// [`Self::mount_device_copy_ro`], which mounts a copy without
        /// syncing.)
        pub(crate) fn remount_ro(&self, minor: u32) -> Result<Arc<dyn VfsInode>, &'static str> {
            let backend: Arc<dyn LatchableCache> = Arc::clone(&self.mnt.cache) as Arc<dyn LatchableCache>;
            let fs = Ext4FileSystem::mount(backend, DevId::makedev(0, minor), true).map_err(|_| "ro mount")?;
            Ok(fs.root_inode())
        }

        /// The volume's free inodes, as its superblock counts them.
        pub(crate) fn free_inodes(&self) -> u32 {
            ktest_util::free_inodes(&self.mnt.fs)
        }

        /// Make every disk read fail (`true`) or work again.
        pub(crate) fn fail_reads(&self, fail: bool) {
            self.mnt.cache.fail_reads.store(fail, Ordering::Release);
        }

        /// Make every write into the block cache, and its syncs, fail
        /// (`true`) or work again (`fsync`'s `-EIO`, #254).
        pub(crate) fn fail_writes(&self, fail: bool) {
            self.mnt.cache.fail_writes.store(fail, Ordering::Release);
        }

        /// Fail every write into the block cache from the `from`th on,
        /// counting from now, its syncs still working, until
        /// [`Self::clear_faults`].
        pub(crate) fn arm_write_from(&self, from: usize) {
            self.mnt.cache.arm_write_from(from);
        }

        /// Make every injected fault stop.
        pub(crate) fn clear_faults(&self) {
            self.mnt.cache.clear();
        }

        /// Make the block cache's syncs alone fail (`true`) or work again:
        /// writes still land in the cache, which keeps them dirty.
        pub(crate) fn fail_syncs(&self, fail: bool) {
            self.mnt.cache.fail_syncs.store(fail, Ordering::Release);
        }

        /// The root of a read-only mount of a copy of the disk's bytes as
        /// they are now, not through the block cache: what it reads reached
        /// the device.
        pub(crate) fn mount_device_copy_ro(&self, minor: u32) -> Result<Arc<dyn VfsInode>, &'static str> {
            Ok(ktest_util::mount_ro_image(self.mnt.dev_bytes(), minor)?.root_inode())
        }
    }

    /// A fresh writable `ext4` volume (1 MiB) over `ext4::ktest_util`'s
    /// `FaultCache`, which counts the filesystem's writes and fails a chosen
    /// one (`arm_write`) or all (`fail_writes`).
    pub(crate) fn fault_ext4(minor: u32) -> Result<Mnt, &'static str> {
        ktest_util::mount(build_minimal_ext4_image_blocks(256), minor)
    }

    /// `len` bytes of a test pattern for a file, seeded by `seed`: never
    /// zero, so a zero read back is a hole or a tail.
    pub(crate) fn pattern(len: usize, seed: u8) -> alloc::vec::Vec<u8> {
        (0..len).map(|i| ((i % 251) as u8).wrapping_add(seed) | 1).collect()
    }

    /// `len` bytes of `f` at `off` (short at EOF, or where a page past the
    /// first failed to read: `PageCache::read_into`), through the VFS's
    /// read path.
    pub(crate) fn read(f: &Arc<dyn VfsInode>, off: u64, len: usize) -> Result<alloc::vec::Vec<u8>, &'static str> {
        let mut buf = alloc::vec![0u8; len];
        let n = f.read_at(off, &mut buf).map_err(|_| "read_at")?;
        buf.truncate(n);
        Ok(buf)
    }

    /// A file named `name` in `dir` holding `data`.
    pub(crate) fn file(dir: &Arc<dyn VfsInode>, name: &str, data: &[u8]) -> Result<Arc<dyn VfsInode>, &'static str> {
        let f = dir.create(name.as_bytes(), 0o644).map_err(|_| "create")?;
        if f.write_at(0, data).map_err(|_| "write_at")? != data.len() {
            return Err("short write");
        }
        Ok(f)
    }

    /// A fresh `tmpfs` holding `quota_bytes` of data, and its few files'
    /// inodes and names (`tmpfs::KTEST_METADATA_ROOM`).
    pub(crate) struct TmpfsVolume {
        _fs: Arc<super::super::tmpfs::Tmpfs>,
        pub(crate) root: Arc<dyn VfsInode>,
    }

    impl TmpfsVolume {
        pub(crate) fn new(minor: u32, quota_bytes: usize) -> Self {
            let fs = super::super::tmpfs::Tmpfs::new(
                DevId::makedev(0, minor),
                quota_bytes + super::super::tmpfs::KTEST_METADATA_ROOM,
            );
            Self { root: fs.root_inode(), _fs: fs }
        }
    }

    /// Reclaim an `ext4` file's pages: write every dirty one back, then
    /// evict every page nothing else holds. The number still cached.
    pub(crate) fn reclaim_ext4(file: &Arc<dyn VfsInode>) -> Result<usize, &'static str> {
        let ext4 = file.as_any().and_then(|a| a.downcast_ref::<Ext4Inode>()).ok_or("not an ext4 inode")?;
        ext4.reclaim_dirty_pages(usize::MAX);
        Ok(ext4.page_cache_lock().lock().len())
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
    /// any quota or count changes. A page of a linked `ext4` file that is
    /// dirty when inserted is refused at the hard limit (`NoSpc`,
    /// [`Self::admit_unreclaimable`]), whichever path made it; a clean one
    /// is not, since reclaim can drop it once nothing else holds it. (A
    /// page a mapping will pin is clean here: [`Self::map_page`] asks
    /// [`Self::admit_unreclaimable`] itself.)
    fn insert_new(&mut self, page_index: u64, page: Arc<CachePage>) -> Result<(), VfsError> {
        if self.pages.contains_key(&page_index) {
            return Err(VfsError::Exist);
        }
        if page.is_dirty() {
            self.admit_unreclaimable()?;
        }
        match &self.owner {
            CacheOwner::Ext4 => {
                EXT4_CACHED_PAGES.fetch_add(1, Ordering::Relaxed);
                TOTAL_CACHED_PAGES.fetch_add(1, Ordering::Relaxed);
            }
            CacheOwner::Tmpfs(quota) => {
                if !quota.try_reserve_pages(1) {
                    return Err(VfsError::NoSpc);
                }
                TOTAL_CACHED_PAGES.fetch_add(1, Ordering::Relaxed);
            }
            CacheOwner::Detached => {
                if !DETACHED_QUOTA.try_reserve_pages(1) {
                    return Err(VfsError::NoSpc);
                }
                DETACHED_CACHED_PAGES.fetch_add(1, Ordering::Relaxed);
                TOTAL_CACHED_PAGES.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.pages.insert(page_index, page);
        Ok(())
    }

    /// Whether this cache's pages are written back: a linked `ext4` file's
    /// (`CacheOwner::Ext4`). A write (or a truncate's zeroing) marks its
    /// pages dirty only then: a `tmpfs` file's pages and a detached file's
    /// are the file's only copy, never written back, and count against
    /// their quotas instead of the hard limit. (A writable shared mapping
    /// still marks any owner's page dirty, and `commit_detached` keeps a
    /// page's dirty bit; nothing writes those back.) The owner turns
    /// `Detached` under Locks 4 and 5, which every writer holds.
    fn writes_back(&self) -> bool {
        matches!(self.owner, CacheOwner::Ext4)
    }

    /// Whether a new page reclaim can't drop (a dirty one, or one a mapping
    /// pins) may join this cache: the one check of the `ext4` hard cap.
    /// `NoSpc` (the hard-limit warning, once per crossing) for a linked
    /// `ext4` file's cache ([`Self::writes_back`]) while the `ext4` cache is
    /// at its hard limit. Reclaim, run before the caller took its locks,
    /// couldn't get below it (the dirty data can't be written back, or
    /// mappings pin the pages), the caller's own write added more pages
    /// than the gap, or other threads added pages while a fill slept (a
    /// disk read), which is why [`Self::write_from`] and [`Self::map_page`]
    /// ask again after their fill. Quotas (`tmpfs`, detached) are reserved
    /// by `insert_new` itself. The caller holds this lock (Lock 5).
    fn admit_unreclaimable(&self) -> Result<(), VfsError> {
        if self.writes_back() && ext4_at_hard_limit() {
            note_hard_limit_refusal();
            return Err(VfsError::NoSpc);
        }
        Ok(())
    }

    /// Page `index` of a file of `file_size` bytes for a mapping: each
    /// filesystem's side of [`mmap_page`], under that contract. `Range` when
    /// the page starts at or past `file_size`. The cached page, or else a
    /// new page `make` makes (a zero page for a hole, or the file's bytes
    /// read from disk), inserted ([`Self::insert_new`]: the `tmpfs` or
    /// detached quota's `NoSpc`). A mapping pins its page, so reclaim can't
    /// drop it: a linked `ext4` file's new page is held to the hard cap as
    /// a write's new page is ([`Self::admit_unreclaimable`]), before `make`
    /// runs (no frame, no disk read) and again after it, since `make` may
    /// sleep (a disk read) while other threads add pages. Either way the
    /// page is touched (reclaim's age). The caller holds the inode's Lock 4,
    /// under which it read `file_size`, and this lock (Lock 5).
    pub(super) fn map_page(
        &mut self,
        index: u64,
        file_size: u64,
        make: impl FnOnce() -> Result<CachePage, VfsError>,
    ) -> Result<Arc<CachePage>, VfsError> {
        mmap_page_in_file(index, file_size)?;
        let page = match self.pages.get(&index) {
            Some(page) => Arc::clone(page),
            None => {
                self.admit_unreclaimable()?;
                let page = Arc::new(make()?);
                self.admit_unreclaimable()?;
                self.insert_new(index, Arc::clone(&page))?;
                page
            }
        };
        page.touch();
        Ok(page)
    }

    /// Copy the file's bytes at `offset` into `buf`, as far as `file_size`,
    /// page by page; returns how many were copied (0 at or past the end).
    /// A page not cached is asked of `miss`, by index: `Some(page)`, a new
    /// page holding the file's bytes there, is read and then inserted (its
    /// insert's error returned); `None` reads as zeros without a page (a
    /// hole). An error from `miss` (or the insert) is returned only when it
    /// is the first page's, else a short count ([`page::short_count`],
    /// #321). (The pages loaded before the error stay cached.) The caller
    /// holds the inode's Lock 4, under which it read `file_size`, and this
    /// lock (Lock 5).
    pub fn read_into(
        &mut self,
        offset: u64,
        buf: &mut [u8],
        file_size: u64,
        mut miss: impl FnMut(u64) -> Result<Option<CachePage>, VfsError>,
    ) -> Result<usize, VfsError> {
        if offset >= file_size {
            return Ok(0);
        }
        let to_read = buf.len().min(usize::try_from(file_size - offset).unwrap_or(usize::MAX));
        // Never `None`: the range ends at or below `file_size`.
        let spans = page::spans(offset, to_read).ok_or(VfsError::Inval)?;
        page::short_count(spans, |span| self.read_span(span, &mut buf[span.buf()], &mut miss))
    }

    /// One page's part of [`Self::read_into`].
    fn read_span(
        &mut self,
        span: &PageSpan,
        dst: &mut [u8],
        miss: &mut impl FnMut(u64) -> Result<Option<CachePage>, VfsError>,
    ) -> Result<(), VfsError> {
        if let Some(cached) = self.pages.get(&span.index()) {
            cached.read_bytes(span.in_page(), dst);
        } else if let Some(loaded) = miss(span.index())? {
            loaded.read_bytes(span.in_page(), dst);
            self.insert_new(span.index(), Arc::new(loaded))?;
        } else {
            dst.fill(0);
        }
        Ok(())
    }

    /// Write `buf` at file offset `offset` of a file of `old_size` bytes,
    /// page by page: into the cached page, or else into a new page `fill`
    /// makes for the span (a zero page, or one holding the file's bytes a
    /// partial write keeps), which is then inserted ([`Self::insert_new`]).
    /// A write starting past the end first zeroes the old last page's bytes
    /// past `old_size`, as a growth to `offset` would ([`Self::resize`]),
    /// and they stay zeroed even if the write then fails (they lie past the
    /// end, so the file reads the same). That page is marked dirty, if
    /// written back, only once the write has written something (only then
    /// do its zeros lie below the new size); a write refused outright (an
    /// `Err`) leaves the size as it was and a clean tail page clean, so that
    /// no writeback of it can fail, and turn up as a later `fsync`'s `EIO`,
    /// for a write the caller saw fail (#321). A linked `ext4` file's pages
    /// ([`Self::writes_back`]) are marked dirty, and a new one is refused
    /// at the hard limit ([`Self::admit_unreclaimable`]) before `fill`
    /// runs. That is an early answer: `fill` may sleep (a disk read) or be
    /// preempted, and another thread may add pages meanwhile, so the insert
    /// asks again and refuses the page `fill` made. Returns how many bytes
    /// were written: an error (the admission's, `fill`'s or the insert's,
    /// either check's refusal alike) only when it is the first page's, else
    /// a short count ([`page::short_count`]). `FBig` when the range ends
    /// past `u64::MAX`, before anything changes. The caller holds the
    /// inode's Lock 4 (`old_size` is the size it read under it) and this
    /// lock (Lock 5), and sets the size and times.
    pub fn write_from(
        &mut self,
        offset: u64,
        buf: &[u8],
        old_size: u64,
        mut fill: impl FnMut(&PageSpan) -> Result<CachePage, VfsError>,
    ) -> Result<usize, VfsError> {
        let spans = page::spans(offset, buf.len()).ok_or(VfsError::FBig)?;
        // At or before the end (`offset <= old_size`) nothing is zeroed: the
        // write overwrites the bytes it extends the file over.
        let tail = self.zero_tail(old_size, offset.max(old_size));
        let done = page::short_count(spans, |span| self.write_span(span, &buf[span.buf()], &mut fill))?;
        if let Some(tail) = tail
            && done > 0
        {
            self.mark_dirty(&tail);
        }
        Ok(done)
    }

    /// One page's part of [`Self::write_from`].
    fn write_span(
        &mut self,
        span: &PageSpan,
        src: &[u8],
        fill: &mut impl FnMut(&PageSpan) -> Result<CachePage, VfsError>,
    ) -> Result<(), VfsError> {
        if let Some(cached) = self.pages.get(&span.index()) {
            cached.write_bytes(span.in_page(), src);
            self.mark_dirty(cached);
            return Ok(());
        }
        self.admit_unreclaimable()?;
        let fresh = fill(span)?;
        fresh.write_bytes(span.in_page(), src);
        self.mark_dirty(&fresh);
        self.insert_new(span.index(), Arc::new(fresh))
    }

    /// Mark `page` dirty if this cache's pages are written back
    /// ([`Self::writes_back`]): a write's or a truncate's change to it.
    fn mark_dirty(&self, page: &CachePage) {
        if self.writes_back() {
            page.set_dirty();
        }
    }

    /// The cache's side of a change of the file's size from `old_size` to
    /// `new_size` (a truncate): on a shrink, drop every page from
    /// [`page::pages_in`]`(new_size)` on; then zero, in the cached page
    /// holding it, what [`page::resize_tail`] says (on a shrink, the cut
    /// bytes of the new last page; on a growth, the old last page's bytes
    /// past the old end, where a `MAP_SHARED` mapping may have stored),
    /// marking that page dirty if it is written back
    /// ([`Self::writes_back`]). A page not cached is left to the filesystem
    /// (`ext4` zeroes its disk tail itself). The caller holds the inode's
    /// Lock 4 and this lock (Lock 5), once the filesystem holds the new
    /// size and before it sets it in memory.
    pub fn resize(&mut self, old_size: u64, new_size: u64) {
        if new_size < old_size {
            self.drop_from(page::pages_in(new_size));
        }
        if let Some(cached) = self.zero_tail(old_size, new_size) {
            self.mark_dirty(&cached);
        }
    }

    /// Zero, in the cached page holding it, what [`page::resize_tail`] says
    /// a change of the size from `old_size` to `new_size` zeroes, leaving
    /// the page's dirty bit as it is; the page zeroed, if any. Its callers
    /// mark it dirty if it is written back: [`Self::resize`] at once, a
    /// write past the end ([`Self::write_from`]) once it has written
    /// something.
    #[must_use = "a grown tail page's zeros reach the disk only once it is marked dirty"]
    fn zero_tail(&self, old_size: u64, new_size: u64) -> Option<Arc<CachePage>> {
        let tail = page::resize_tail(old_size, new_size)?;
        let cached = self.pages.get(&tail.index())?;
        cached.zero_range(tail.from(), PAGE_SIZE);
        Some(Arc::clone(cached))
    }

    /// Drop all cached pages with `page_index >= start_page`, updating
    /// accounting: [`Self::resize`]'s shrink, which also zeroes the new
    /// last page's tail.
    fn drop_from(&mut self, start_page: u64) {
        let removed = self.pages.split_off(&start_page);
        self.release_accounting(removed.len());
    }

    /// Drop every page from `start_page` on without a size change, dirty or
    /// mapped ones too (unlike reclaim, which writes dirty pages back and
    /// skips mapped ones): a dirty page's data is lost. Ktests only: a
    /// truncate goes through [`Self::resize`].
    #[cfg(feature = "ktest")]
    pub(super) fn remove_from(&mut self, start_page: u64) {
        self.drop_from(start_page);
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
                quota.release_pages(count);
            }
            CacheOwner::Detached => {
                DETACHED_CACHED_PAGES.fetch_sub(count, Ordering::Relaxed);
                DETACHED_QUOTA.release_pages(count);
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
        if DETACHED_QUOTA.try_reserve_pages(pages) {
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
            DETACHED_QUOTA.release_pages(self.reserved);
            self.reserved = 0;
        }
    }
}

/// Soft global reclaim for `ext4` `PageCache` frames when above the soft
/// limit ([`EXT4_SOFT_LIMIT_PAGES`]).
///
/// The caller holds no lock at level 3 or above (§9: reclaim copies each
/// `ext4` filesystem's live inodes under its Lock 6, released, then takes
/// 4 -> 5 -> 6 -> 7 -> 8 per inode), with interrupts enabled.
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

    let inodes = super::snapshot_live_ext4_inodes();
    // Pass 1: evict clean unpinned ext4 pages (Lock 4(B) -> Lock 5(B)).
    for inode in &inodes {
        let cur = EXT4_CACHED_PAGES.load(Ordering::Relaxed);
        if cur <= target_pages {
            return;
        }
        let excess = cur - target_pages;
        inode.reclaim_clean_pages(excess);
    }

    // Pass 2: if still over target, flush dirty ext4 pages and evict if unpinned.
    for inode in &inodes {
        let cur = EXT4_CACHED_PAGES.load(Ordering::Relaxed);
        if cur <= target_pages {
            return;
        }
        let excess = cur - target_pages;
        inode.reclaim_dirty_pages(excess);
    }
}

crate::ktest!(
    fn page_cache_refuses_a_second_page_at_one_index() {
        use crate::{kassert, kassert_eq};
        let mut pc = PageCache::new_ext4();
        let before = ext4_cached_pages();
        let first = Arc::new(CachePage::new_zeroed().map_err(|_| "frame")?);
        first.write_bytes(0, b"dirty");
        first.set_dirty();
        pc.insert_new(7, Arc::clone(&first)).map_err(|_| "insert")?;
        let second = Arc::new(CachePage::new_zeroed().map_err(|_| "frame")?);
        kassert_eq!(pc.insert_new(7, second).err(), Some(VfsError::Exist));
        kassert_eq!(ext4_cached_pages(), before + 1); // counted once
        kassert!(Arc::ptr_eq(pc.get(7).ok_or("page")?, &first)); // the dirty page kept
        drop(pc);
        kassert_eq!(ext4_cached_pages(), before);
        Ok(())
    }
);

/// The ktests' crossing of the `ext4` hard limit: `f` runs with the
/// hard-limit warning re-armed, so the crossing logs it once however many
/// refusals `f` sees (tests/scenarios/test_ktest.py counts one per
/// crossing). The real limits and the warning's state from before are
/// restored after it, also when `f` (or a fill it hands on) put the cache
/// at the limit ([`reach_hard_limit`]).
#[cfg(feature = "ktest")]
fn hard_limit_crossing<T>(f: impl FnOnce() -> T) -> T {
    let warned = HARD_LIMIT_WARNED.swap(false, Ordering::AcqRel);
    let out = f();
    set_ext4_limits_for_test(None);
    HARD_LIMIT_WARNED.store(warned, Ordering::Release);
    out
}

/// Put the `ext4` cache at its hard limit, whatever it holds.
#[cfg(feature = "ktest")]
fn reach_hard_limit() {
    set_ext4_limits_for_test(Some((usize::MAX, 0)));
}

/// A linked `ext4` file's cache holding a clean page 0.
#[cfg(feature = "ktest")]
fn ext4_cache_with_page0() -> Result<PageCache, &'static str> {
    let mut pc = PageCache::new_ext4();
    pc.insert_new(0, Arc::new(CachePage::new_zeroed().map_err(|_| "frame")?)).map_err(|_| "insert")?;
    Ok(pc)
}

crate::ktest!(
    fn page_cache_insert_refuses_a_dirty_ext4_page_at_the_hard_limit() {
        // The hard limit is `insert_new`'s, so no path adding a dirty page
        // of a linked ext4 file can pass it: refused (`NoSpc`) at the limit,
        // let in below it. A clean page (a read's, which reclaim can drop)
        // and a tmpfs page (its own quota) are let in at the limit.
        use crate::{kassert, kassert_eq};
        let mut pc = PageCache::new_ext4();
        let mut tmp = PageCache::new_tmpfs(Arc::new(QuotaCounter::new_pages("ktest", 4)));
        let dirty = Arc::new(CachePage::new_zeroed().map_err(|_| "frame")?);
        dirty.set_dirty();
        let (clean, tmp_dirty) = (
            Arc::new(CachePage::new_zeroed().map_err(|_| "frame")?),
            Arc::new(CachePage::new_zeroed().map_err(|_| "frame")?),
        );
        tmp_dirty.set_dirty();
        let at_limit = hard_limit_crossing(|| {
            reach_hard_limit();
            (pc.insert_new(1, Arc::clone(&dirty)), pc.insert_new(2, clean), tmp.insert_new(1, tmp_dirty))
        });
        kassert_eq!(at_limit, (Err(VfsError::NoSpc), Ok(()), Ok(())));
        kassert!(pc.get(1).is_none());
        kassert_eq!(pc.len(), 1);
        kassert_eq!(pc.insert_new(1, dirty), Ok(())); // below the limit
        kassert_eq!(pc.len(), 2);
        Ok(())
    }
);

crate::ktest!(
    fn page_cache_write_from_refuses_at_the_hard_limit_before_filling() {
        // A write into a linked ext4 file's cache at the hard limit: a new
        // page is refused before `fill` runs (no frame, no disk read), and a
        // write that has already written a cached page returns short
        // instead. A detached file's cache (a write dirties none of its
        // pages; its own quota) isn't held to it.
        use crate::{kassert, kassert_eq};
        let mut pc = ext4_cache_with_page0()?;
        let mut detached = PageCache::new_ext4();
        detached.commit_detached(Vec::new()); // no pages yet: nothing to reserve
        let mut fills = 0usize;
        let mut fill = |_: &PageSpan| {
            fills += 1;
            CachePage::new_zeroed()
        };
        let data = [0x5a; 16];
        let size = 3 * PAGE_SIZE as u64;
        let (new_page, spanning, in_detached) = hard_limit_crossing(|| {
            reach_hard_limit();
            (
                pc.write_from(PAGE_SIZE as u64, &data[..10], size, &mut fill),
                pc.write_from(PAGE_SIZE as u64 - 6, &data, size, &mut fill),
                detached.write_from(2 * PAGE_SIZE as u64, &data[..10], size, &mut fill),
            )
        });
        kassert_eq!(new_page, Err(VfsError::NoSpc));
        kassert_eq!(spanning, Ok(6)); // the cached page's part
        kassert_eq!(in_detached, Ok(10));
        kassert_eq!(fills, 1); // the detached file's page alone
        kassert!(pc.get(0).ok_or("page 0")?.is_dirty());
        kassert!(pc.get(1).is_none());
        kassert!(!detached.get(2).ok_or("detached page")?.is_dirty());
        let mut back = [0u8; 8];
        kassert_eq!(pc.read_into(PAGE_SIZE as u64 - 8, &mut back, size, |_| Ok(None)), Ok(8));
        kassert_eq!(back, [0, 0, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a, 0x5a]);
        Ok(())
    }
);

crate::ktest!(
    fn page_cache_write_from_refuses_a_page_the_cache_filled_up_for() {
        // `fill` may sleep (a disk read) or be preempted while another
        // thread adds pages, so the insert asks the hard cap again: a page
        // made while the cache reached it is refused all the same (it is
        // dirty when inserted), `NoSpc` on the first page and a short count
        // after a cached one. Here `fill` lowers the limit itself.
        use crate::{kassert, kassert_eq};
        let mut pc = ext4_cache_with_page0()?;
        let mut fills = 0usize;
        let mut fill = |_: &PageSpan| {
            fills += 1;
            reach_hard_limit(); // the cache filled meanwhile
            CachePage::new_zeroed()
        };
        let data = [0x5a; 16];
        let size = 3 * PAGE_SIZE as u64;
        let (first, spanning) = hard_limit_crossing(|| {
            let first = pc.write_from(PAGE_SIZE as u64, &data[..10], size, &mut fill);
            set_ext4_limits_for_test(None);
            (first, pc.write_from(PAGE_SIZE as u64 - 6, &data, size, &mut fill))
        });
        kassert_eq!(first, Err(VfsError::NoSpc));
        kassert_eq!(spanning, Ok(6)); // the cached page's part
        kassert_eq!(fills, 2); // past the early check both times
        kassert!(pc.get(1).is_none());
        kassert_eq!(pc.len(), 1);
        Ok(())
    }
);

crate::ktest!(
    fn page_cache_map_page_asks_the_hard_cap_before_and_after_make() {
        // A mapping pins its page, so a linked ext4 file's new mapped page
        // is held to the hard cap: at the limit `make` never runs (no frame,
        // no disk read), and a page `make` returns once the cache filled up
        // meanwhile (here `make` lowers the limit itself) is refused, not
        // inserted. A cached page is handed out at the limit, and a tmpfs
        // file's new page isn't held to it (its own quota).
        use crate::{kassert, kassert_eq};
        let mut pc = ext4_cache_with_page0()?;
        let mut tmp = PageCache::new_tmpfs(Arc::new(QuotaCounter::new_pages("ktest", 4)));
        let size = 3 * PAGE_SIZE as u64;
        let mut makes = 0usize;
        let (before, cached, in_tmpfs, after) = hard_limit_crossing(|| {
            reach_hard_limit();
            let before = pc.map_page(1, size, || {
                makes += 1;
                CachePage::new_zeroed()
            });
            let cached = pc.map_page(0, size, || Err(VfsError::Io)).is_ok();
            let in_tmpfs = tmp.map_page(1, size, CachePage::new_zeroed).is_ok();
            set_ext4_limits_for_test(None);
            let after = pc.map_page(1, size, || {
                makes += 1;
                reach_hard_limit(); // the cache filled meanwhile
                CachePage::new_zeroed()
            });
            (before.err(), cached, in_tmpfs, after.err())
        });
        kassert_eq!((before, after), (Some(VfsError::NoSpc), Some(VfsError::NoSpc)));
        kassert_eq!(makes, 1); // the second call's alone
        kassert!(cached && in_tmpfs);
        kassert!(pc.get(1).is_none());
        kassert_eq!(pc.len(), 1);
        // Below the limit: made and inserted, then handed out again.
        let page = pc.map_page(1, size, CachePage::new_zeroed).map_err(|_| "map below the limit")?;
        kassert!(Arc::ptr_eq(&page, pc.get(1).ok_or("page 1")?));
        kassert_eq!(pc.map_page(3, size, CachePage::new_zeroed).err(), Some(VfsError::Range));
        Ok(())
    }
);

crate::ktest!(
    fn page_cache_a_refused_write_past_the_end_leaves_its_tail_page_clean() {
        // A write past the end zeroes the old last page's bytes past the end
        // first, and marks that page dirty only once it has written
        // something (its zeros then lie below the new size): a write refused
        // outright leaves a clean tail page clean, so a failing disk can't
        // fail its writeback, and a later `fsync` can't return EIO for it,
        // after a write the caller saw fail (#321). A short write and a
        // whole one mark it dirty.
        use crate::{kassert, kassert_eq};
        let p = PAGE_SIZE as u64;
        let size = 100; // page 0 is the tail
        let mut pc = ext4_cache_with_page0()?;
        let tail = Arc::clone(pc.get(0).ok_or("page 0")?);
        tail.write_bytes(200, b"junk"); // past the end, as a mapping stores
        let refused = pc.write_from(2 * p, b"ab", size, |_| Err(VfsError::Io));
        kassert_eq!(refused, Err(VfsError::Io));
        kassert!(!tail.is_dirty());
        let mut stale = [1u8; 4];
        tail.read_bytes(200, &mut stale);
        kassert_eq!(stale, [0; 4]); // zeroed all the same
        kassert_eq!(pc.len(), 1);
        // Page 1's byte written, page 2's fill failing: a short count.
        let short = pc.write_from(2 * p - 1, b"ab", size, |span| {
            if span.index() == 1 { CachePage::new_zeroed() } else { Err(VfsError::Io) }
        });
        kassert_eq!(short, Ok(1));
        kassert!(tail.is_dirty());
        let mut whole = ext4_cache_with_page0()?;
        kassert_eq!(whole.write_from(2 * p, b"ab", size, |_| CachePage::new_zeroed()), Ok(2));
        kassert!(whole.get(0).ok_or("page 0")?.is_dirty());
        // A tmpfs file's pages are never written back: its tail page isn't
        // marked dirty either.
        let mut tmp = PageCache::new_tmpfs(Arc::new(QuotaCounter::new_pages("ktest", 4)));
        tmp.insert_new(0, Arc::new(CachePage::new_zeroed().map_err(|_| "frame")?)).map_err(|_| "insert")?;
        kassert_eq!(tmp.write_from(2 * p, b"ab", size, |_| CachePage::new_zeroed()), Ok(2));
        kassert!(!tmp.get(0).ok_or("tmpfs page 0")?.is_dirty());
        Ok(())
    }
);

crate::ktest!(
    fn page_cache_ext4_fdatasync_after_a_refused_write_past_the_end_succeeds() {
        // End to end (#321): an ext4 file whose last page is cached and
        // clean; a write past the end refused at the hard limit (one
        // hard-limit warning, pinned by test_ktest.py); then every block
        // write failing, the block cache's own sync still working. The
        // refused write left the tail page clean (and still cached), so
        // `fdatasync` has nothing to write back and succeeds.
        use crate::{kassert, kassert_eq};
        use ktest_files::{file, pattern, read};
        let vol = ktest_files::Ext4Volume::new(179)?;
        let data = pattern(100, 21);
        let f = file(&vol.root, "tail.bin", &data)?;
        f.sync(false).map_err(|_| "sync")?;
        let cache = page_cache_lock(&*f).ok_or("no page cache")?;
        let cached = cache.lock().len();
        kassert_eq!(cached, 1); // the tail page
        let refused = hard_limit_crossing(|| {
            reach_hard_limit();
            f.write_at(2 * PAGE_SIZE as u64, b"ab")
        });
        kassert_eq!(refused, Err(VfsError::NoSpc));
        let still_cached = cache.lock().get(0).is_some();
        kassert!(still_cached); // the page the refused write zeroed past the end
        let dirty = cache.lock().dirty_count();
        vol.arm_write_from(1);
        let synced = f.sync(true);
        vol.clear_faults();
        kassert_eq!(synced, Ok(()));
        kassert_eq!(dirty, 0);
        kassert!(read(&f, 0, 200)? == data);
        Ok(())
    }
);

crate::ktest!(
    fn page_cache_a_read_stops_at_its_first_failing_page() {
        // A read over three pages whose middle one fails to load returns the
        // first page's bytes alone: never the third page's (cached, so it
        // would read) past the gap, and `miss` is asked of the middle one
        // alone (`page::short_count`, #321).
        use crate::{kassert, kassert_eq};
        let mut pc = PageCache::new_ext4();
        for (index, byte) in [(0u64, 0xa0u8), (2, 0xa2)] {
            let page = CachePage::new_zeroed().map_err(|_| "frame")?;
            page.write_bytes(0, &alloc::vec![byte; PAGE_SIZE]);
            pc.insert_new(index, Arc::new(page)).map_err(|_| "insert")?;
        }
        let mut asked = Vec::new();
        let mut buf = alloc::vec![0u8; 3 * PAGE_SIZE];
        let got = pc.read_into(0, &mut buf, 3 * PAGE_SIZE as u64, |index| {
            asked.push(index);
            Err(VfsError::Io)
        });
        kassert_eq!(got, Ok(PAGE_SIZE));
        kassert_eq!(asked, [1]);
        kassert!(buf[..PAGE_SIZE].iter().all(|&b| b == 0xa0));
        kassert!(buf[PAGE_SIZE..].iter().all(|&b| b == 0)); // nothing copied past the gap
        Ok(())
    }
);

crate::ktest!(
    fn page_cache_a_read_failing_past_its_first_page_returns_the_bytes_before_it() {
        // A read whose second page's disk read fails returns the first
        // page's bytes, a short count as Linux's `filemap_read` gives, not
        // the error; the next read, at the failing page, gets it (#321).
        // One warning in all (both failures in one streak), pinned by
        // test_ktest.py.
        use crate::{kassert, kassert_eq};
        use ktest_files::{file, pattern, read, reclaim_ext4};
        let vol = ktest_files::Ext4Volume::new(198)?;
        let data = pattern(2 * PAGE_SIZE, 13);
        let f = file(&vol.root, "short.bin", &data)?;
        kassert_eq!(reclaim_ext4(&f)?, 0); // on disk alone
        kassert!(read(&f, 0, PAGE_SIZE)? == data[..PAGE_SIZE]); // page 0 cached, page 1 not
        let mut whole = alloc::vec![0u8; 2 * PAGE_SIZE];
        let mut second = alloc::vec![0u8; PAGE_SIZE];
        vol.fail_reads(true);
        let (got_whole, got_second) = (f.read_at(0, &mut whole), f.read_at(PAGE_SIZE as u64, &mut second));
        vol.fail_reads(false);
        kassert_eq!(got_whole, Ok(PAGE_SIZE));
        kassert!(whole[..PAGE_SIZE] == data[..PAGE_SIZE]);
        kassert_eq!(got_second, Err(VfsError::Io));
        kassert!(read(&f, 0, 2 * PAGE_SIZE)? == data); // the disk reads again
        Ok(())
    }
);

/// `write_at` over a file's cached pages on `root`'s filesystem (through
/// [`PageCache::write_from`]): an overwrite inside the file, at an offset
/// inside a page, keeps the rest of that page (it is no shrink, and zeroes
/// no tail); and a write starting past the end zeroes the old last page's
/// bytes past the end, where a mapping stored, so the gap it opens reads
/// as zeros. With `ext4`, the first file's pages are written back, dropped
/// and read in again first: the pages a read loads stay cached.
#[cfg(feature = "ktest")]
fn check_writes_over_cached_pages(root: &Arc<dyn VfsInode>, ext4: bool) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    use ktest_files::{file, pattern, read, reclaim_ext4};
    fn cache(f: &Arc<dyn VfsInode>) -> Result<&SleepMutex<PageCache>, &'static str> {
        page_cache_lock(&**f).ok_or("no page cache")
    }
    let mut want = pattern(10_000, 3);
    let f = file(root, "over.bin", &want)?;
    if ext4 {
        kassert_eq!(reclaim_ext4(&f)?, 0);
        kassert!(read(&f, 0, want.len())? == want);
    }
    kassert_eq!(cache(&f)?.lock().len(), 3);
    // Page 0, the overwrite's, held mapped so reclaim can't drop it first.
    let page0 = mmap_page(&f, 0).map_err(|_| "map page 0")?;
    kassert_eq!(f.write_at(100, b"xy").map_err(|_| "overwrite")?, 2);
    want[100..102].copy_from_slice(b"xy");
    kassert!(read(&f, 0, want.len())? == want);
    drop(page0);

    // A file of a page and 100 bytes; a store through a mapping at 200 of
    // its page 1 (past the end); then a write at a page and 1000.
    let p = PAGE_SIZE as u64;
    let g = file(root, "tail.bin", &pattern(PAGE_SIZE + 100, 5))?;
    // The mapping is held throughout, so reclaim can't drop the page (and
    // the store with it) before the write: the write found it cached.
    let mapped = mmap_page(&g, 1).map_err(|_| "map")?;
    mapped.page().write_bytes(200, b"junk");
    kassert_eq!(g.write_at(p + 1000, b"z").map_err(|_| "write past the end")?, 1);
    let back = read(&g, p + 100, 901)?;
    let still = cache(&g)?.lock().get(1).map(|c| Arc::ptr_eq(c, mapped.page()));
    kassert_eq!(still, Some(true));
    kassert!(back[..900].iter().all(|&b| b == 0));
    kassert_eq!(back[900], b'z');
    Ok(())
}

crate::ktest!(
    fn page_cache_tmpfs_writes_keep_their_page_and_zero_a_stale_tail() {
        let vol = ktest_files::TmpfsVolume::new(184, 16 * PAGE_SIZE);
        check_writes_over_cached_pages(&vol.root, false)
    }
);

crate::ktest!(
    fn page_cache_ext4_writes_keep_their_page_and_zero_a_stale_tail() {
        let vol = ktest_files::Ext4Volume::new(185)?;
        check_writes_over_cached_pages(&vol.root, true)
    }
);

/// Shrink `f` (at least three pages long) to 100 bytes, its pages 0 to 2
/// held mapped across the truncate so reclaim can't drop them first, then
/// grow it again by a write at three pages: the shrink
/// drops the pages past the new end, and the cut bytes read back as zeros
/// ([`PageCache::resize`]; with its sizes swapped, a shrink would keep the
/// pages and leave them to the next truncate, and a write would read the
/// cut bytes back).
#[cfg(feature = "ktest")]
fn check_shrink_then_write_past(f: &Arc<dyn VfsInode>) -> Result<(), &'static str> {
    use crate::{kassert, kassert_eq};
    let p = PAGE_SIZE as u64;
    let cached = || page_cache_lock(&**f).map(|l| l.lock().len()).ok_or("no page cache");
    let held = [0, 1, 2].map(|i| mmap_page(f, i));
    let held: Vec<MappedPage> = held.into_iter().collect::<Result<_, _>>().map_err(|_| "map pages 0 to 2")?;
    f.truncate(100).map_err(|_| "shrink")?;
    kassert_eq!(cached()?, 1); // page 0 alone
    drop(held);
    kassert_eq!(f.write_at(3 * p, b"z").map_err(|_| "write past the end")?, 1);
    let back = ktest_files::read(f, 100, 3 * PAGE_SIZE - 99)?;
    kassert_eq!(back.len(), 3 * PAGE_SIZE - 99);
    kassert!(back[..back.len() - 1].iter().all(|&b| b == 0));
    kassert_eq!(back.last(), Some(&b'z'));
    Ok(())
}

crate::ktest!(
    fn page_cache_tmpfs_shrink_then_write_past_reads_zeros() {
        let vol = ktest_files::TmpfsVolume::new(187, 16 * PAGE_SIZE);
        let f = ktest_files::file(&vol.root, "shrink.bin", &ktest_files::pattern(3 * PAGE_SIZE, 7))?;
        check_shrink_then_write_past(&f)
    }
);

crate::ktest!(
    fn page_cache_ext4_shrink_then_write_past_reads_zeros() {
        let vol = ktest_files::Ext4Volume::new(188)?;
        let f = ktest_files::file(&vol.root, "shrink.bin", &ktest_files::pattern(3 * PAGE_SIZE, 7))?;
        check_shrink_then_write_past(&f)
    }
);

crate::ktest!(
    fn page_cache_detached_ext4_file_never_reads_the_disk() {
        // An open-unlinked ext4 file holds its data in its pages (read in
        // when it was detached; its blocks are freed): with every disk read
        // failing from then on, a hole reads as zeros, a write into a hole
        // starts from a zero page, and a shrink then a growth by a write
        // reads zeros where the cut bytes were.
        use crate::{kassert, kassert_eq};
        use ktest_files::{file, pattern, read, reclaim_ext4};
        let p = PAGE_SIZE as u64;
        let vol = ktest_files::Ext4Volume::new(189)?;
        let f = file(&vol.root, "detached.bin", &pattern(PAGE_SIZE, 9))?;
        kassert_eq!(f.write_at(3 * p, &pattern(PAGE_SIZE, 11)).map_err(|_| "write page 3")?, PAGE_SIZE);
        kassert_eq!(reclaim_ext4(&f)?, 0); // on disk alone: the detach reads pages 0 and 3 in
        let open = lumen_vfs::OpenGuard::try_new(Arc::clone(&f)).map_err(|_| "open")?;
        vol.root.unlink(b"detached.bin").map_err(|_| "unlink")?;
        kassert!(f.is_detached());
        vol.fail_reads(true);
        let checks = (|| -> Result<(), &'static str> {
            kassert!(read(&f, p, PAGE_SIZE)?.iter().all(|&b| b == 0)); // page 1, a hole
            kassert_eq!(f.write_at(2 * p + 10, b"hi").map_err(|_| "write into a hole")?, 2);
            let page2 = read(&f, 2 * p, PAGE_SIZE)?;
            kassert_eq!(&page2[10..12], b"hi");
            kassert!(page2[..10].iter().chain(&page2[12..]).all(|&b| b == 0));
            check_shrink_then_write_past(&f)
        })();
        vol.fail_reads(false);
        drop(open);
        checks
    }
);

crate::ktest!(
    fn page_cache_ext4_writes_back_only_the_bytes_below_eof() {
        // A store through a shared mapping past the end of a file's last
        // page stays out of the file: writeback writes the page's bytes
        // below the size alone (`page::valid_len`), so once the page is
        // dropped, a growth reads zeros there from the disk, not the store.
        use crate::{kassert, kassert_eq};
        use ktest_files::{file, pattern, read, reclaim_ext4};
        let p = PAGE_SIZE as u64;
        let vol = ktest_files::Ext4Volume::new(190)?;
        let g = file(&vol.root, "eof.bin", &pattern(PAGE_SIZE + 100, 5))?;
        let mapped = mmap_page(&g, 1).map_err(|_| "map")?;
        mapped.page().write_bytes(200, b"junk"); // past the end
        drop(mapped);
        kassert_eq!(g.write_at(p + 50, b"q").map_err(|_| "dirty the page")?, 1);
        kassert_eq!(reclaim_ext4(&g)?, 0); // written back and dropped
        g.truncate(2 * p).map_err(|_| "grow")?;
        kassert!(read(&g, p + 100, PAGE_SIZE - 100)?.iter().all(|&b| b == 0));
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
        let zero = devfs.root_inode().lookup(b"zero").map_err(|_| "lookup zero")?;
        kassert_eq!(mmap_page(&zero, 0).err(), Some(VfsError::NoDev));
        let procfs = super::procfs::Procfs::new(DevId::makedev(0, 127));
        let version = procfs.root_inode().lookup(b"version").map_err(|_| "lookup version")?;
        kassert_eq!(version.inode_type(), InodeType::File);
        kassert_eq!(mmap_page(&version, 0).err(), Some(VfsError::NoDev));
        Ok(())
    }
);

crate::ktest!(
    fn is_dirty_sees_an_unmap_landing_between_its_loads() {
        // Reclaim's `is_dirty` can be preempted between its two loads, and
        // an unmap's `dec_writable_maps` (which takes no lock) can run
        // there. The page was mapped writable and a writeback cleared
        // `dirty`: whichever way the unmap falls, the page must read dirty,
        // or reclaim evicts its mapped stores unwritten.
        use crate::{kassert, kassert_eq};
        let page = CachePage::new_zeroed().map_err(|_| "frame")?;
        page.set_dirty();
        page.inc_writable_maps();
        kassert!(page.swap_dirty(false)); // a writeback while mapped
        kassert!(page.is_dirty()); // through the count
        ktest_hooks::dec_between_dirty_loads(&page);
        kassert!(page.is_dirty()); // the unmap landed between the loads
        kassert_eq!(page.writable_maps(), 0); // (it did run)
        kassert!(page.is_dirty()); // and after it, through `dirty`
        Ok(())
    }
);

crate::ktest!(
    fn mapped_page_copies_to_a_frame_under_the_cache_lock() {
        // `MappedPage::copy_to_frame` (a copy-on-write split): the page's
        // bytes below the file's size, zeros after them (even where a
        // mapping stored past EOF), whatever the frame held before; and the
        // copy holds the owning inode's Lock 5, as the kernel's writers of
        // the page do: while another thread holds that lock, a copy started
        // meanwhile waits.
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::AtomicU8;
        use lumen_vfs::{DevId, FileSystem};
        let fs = super::tmpfs::Tmpfs::new(DevId::makedev(0, 128), 4 * PAGE_SIZE);
        let f = fs.root_inode().create(b"cow.bin", 0o644).map_err(|_| "create")?;
        let bytes: Vec<u8> = (0..PAGE_SIZE + 1000).map(|i| (i % 251) as u8 | 1).collect();
        kassert_eq!(f.write_at(0, &bytes).map_err(|_| "write")?, bytes.len());
        let mapped = mmap_page(&f, 0).map_err(|_| "map")?;
        let tail = mmap_page(&f, 1).map_err(|_| "map tail")?;
        kassert_eq!((mapped.valid_len(), tail.valid_len()), (PAGE_SIZE, 1000));
        // A store through a mapping past EOF: in the page, never in a copy.
        tail.page().write_bytes(1000, &[0x77; 64]);
        let mut dst = frame::alloc_owned().ok_or("dst frame")?;
        let dst_ptr = mm::phys_to_virt(dst.frame().start()).as_mut_ptr::<u8>();
        let read_dst = |out: &mut [u8; PAGE_SIZE]| {
            // SAFETY: `dst` is an allocated frame this test owns, mapped
            // through the HHDM; no reference to it is live while copying.
            unsafe { core::ptr::copy_nonoverlapping(dst_ptr, out.as_mut_ptr(), PAGE_SIZE) };
        };
        // SAFETY: as above; filling it with junk first.
        unsafe { core::ptr::write_bytes(dst_ptr, 0xa5, PAGE_SIZE) };
        tail.copy_to_frame(&mut dst);
        let mut got = [0u8; PAGE_SIZE];
        read_dst(&mut got);
        kassert!(got[..1000] == bytes[PAGE_SIZE..]);
        kassert!(got[1000..].iter().all(|&b| b == 0));
        mapped.copy_to_frame(&mut dst);
        read_dst(&mut got);
        kassert!(got[..] == bytes[..PAGE_SIZE]);

        // Hold the cache's lock, start a copy in a thread on this CPU, and
        // let it run: it must still be waiting when the lock is let go.
        let tmpfs = f.as_any().and_then(|a| a.downcast_ref::<super::tmpfs::TmpfsInode>()).ok_or("not tmpfs")?;
        let state = Arc::new(AtomicU8::new(0)); // 1: copying, 2: copied
        let held = tmpfs.page_cache_lock().lock();
        let (st, mut dst2) = (Arc::clone(&state), frame::alloc_owned().ok_or("dst frame 2")?);
        let copier = crate::sched::spawn("ktest-cow-copy", move || {
            st.store(1, Ordering::Release);
            mapped.copy_to_frame(&mut dst2);
            st.store(2, Ordering::Release);
        })
        .map_err(|_| "spawn")?;
        for _ in 0..100_000 {
            if state.load(Ordering::Acquire) != 0 {
                break;
            }
            crate::sched::yield_now();
        }
        let slept = crate::sched::sleep(20_000_000); // time to finish, were it not waiting
        let while_held = state.load(Ordering::Acquire);
        drop(held);
        copier.join();
        kassert!(slept);
        kassert_eq!(while_held, 1); // started, and waiting for the lock
        kassert_eq!(state.load(Ordering::Acquire), 2);
        Ok(())
    }
);

crate::ktest!(
    fn page_quota_release_saturates_and_detached_quota_reports_nospc() {
        // Releasing more than is reserved (a bug elsewhere) saturates at
        // zero, logged, rather than wrapping to "full"; a quota dropped with
        // bytes still reserved logs that leak. Both lines are pinned in
        // test_ktest.py's `VFS_EXACT_LINES`.
        use crate::kassert_eq;
        let q = QuotaCounter::new_pages("ktest", 4);
        kassert_eq!(q.try_reserve_pages(2), true);
        q.release_pages(5);
        kassert_eq!(q.used_bytes(), 0);
        kassert_eq!(q.try_reserve_pages(4), true); // not wrapped to "full"
        kassert_eq!(q.try_reserve_bytes(1), false); // full to the byte
        q.release_bytes(PAGE_SIZE - 1);
        kassert_eq!(q.used_bytes(), 3 * PAGE_SIZE + 1);
        drop(q); // with those still reserved: the leak's line
        let mut g = DetachedQuotaGuard::new();
        kassert_eq!(g.try_reserve(DETACHED_QUOTA_PAGES + 1).err(), Some(VfsError::NoSpc));
        Ok(())
    }
);

crate::ktest!(
    fn page_cache_soft_limit_triggers_reclaim() {
        // Past the soft limit, the next ext4 read or write reclaims (writing
        // dirty pages back) before it runs. Reclaim reaches the file through
        // its filesystem in `EXT4_FILESYSTEMS` (each one's live inodes); the
        // mount in `TABLES` is only for opening it by path. The limits in
        // force outside this test: 32 MiB soft, twice that hard.
        crate::kassert_eq!(EXT4_SOFT_LIMIT_PAGES * PAGE_SIZE, 32 << 20);
        crate::kassert_eq!(EXT4_HARD_LIMIT_PAGES, 2 * EXT4_SOFT_LIMIT_PAGES);
        use crate::vfs::{EXT4_FILESYSTEMS, TABLES, ext4};
        use crate::{kassert, kassert_eq};
        use lumen_bcache::BlockCache;
        use lumen_vfs::{FileSystem, OpenFlags, mkdir_at, open_inode, unlink_at};

        let img = crate::vfs::build_minimal_ext4_image_blocks(256);
        let dev = crate::vfs::MemBlkDev::new(img, false);
        let bcache: Arc<dyn ext4::LatchableCache> =
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
            let on_disk = ro.root_inode().lookup(b"f").map_err(|_| "ro lookup")?;
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
