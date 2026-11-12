//! Physical frame allocator logic for the Lumen kernel.
//!
//! This crate is pure logic: it never touches hardware or dereferences a
//! physical address, so all of it is tested on the host
//! (`cargo test -p lumen-mm`). The kernel (`kernel/src/mm/frame.rs`) feeds it
//! the bootloader's memory map and the list of reserved ranges.
//!
//! # Design: two bitmaps
//!
//! [`FrameAllocator`] keeps one bit per 4 KiB frame in two bitmaps:
//!
//! - **owned**: the frame is RAM the allocator may hand out (inside a usable
//!   memory-map entry and outside every reservation).
//! - **free**: the frame is owned and not currently allocated.
//!
//! The owned bitmap is what lets [`FrameAllocator::free`] refuse a frame it
//! never handed out (a reserved frame such as the kernel image, an address
//! outside RAM, or a double free) instead of silently adding it to the pool.
//! That is hard rule 2 ("never hand out memory you don't own") applied to
//! `free` as well as `alloc`.
//!
//! **Why a bitmap and not a buddy allocator:** at 2 bits per frame the cost is
//! 64 KiB per GiB of RAM, it is simple enough to check exhaustively, and
//! single-frame allocation (what page tables, stacks and the heap need) is a
//! word scan from a "lowest free frame" hint. Contiguous allocation is a
//! linear scan, which is fine for the rare large requests (DMA buffers). If
//! profiling shows contiguous allocation matters, a buddy allocator can
//! replace this behind the same interface.
//!
//! # Order of construction
//!
//! Build in two phases: [`FrameAllocator::add_usable`] for every usable
//! region, then [`FrameAllocator::reserve`] for every reserved range. The
//! first `reserve` seals the usable set: a later `add_usable` fails with
//! [`InitError::Sealed`], so a reservation can never be undone by accident.
//! Overlapping or unaligned input is fine: usable ranges are rounded *inward*
//! to whole frames, reservations *outward*, and a frame that is both usable
//! and reserved is reserved.

#![no_std]

use core::fmt;

/// Size of a physical frame (and a small page).
pub const FRAME_SIZE: u64 = 4096;

const BITS: usize = u64::BITS as usize;

/// Round `x` up to a multiple of `align` (a power of two), saturating at the
/// highest multiple that fits in a `u64`.
pub const fn align_up(x: u64, align: u64) -> u64 {
    match x.checked_add(align - 1) {
        Some(v) => v & !(align - 1),
        None => !(align - 1),
    }
}

/// Round `x` down to a multiple of `align` (a power of two).
pub const fn align_down(x: u64, align: u64) -> u64 {
    x & !(align - 1)
}

/// A half-open range of physical addresses, `[start, end)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhysRange {
    pub start: u64,
    pub end: u64,
}

impl PhysRange {
    /// `[start, end)`; an `end` below `start` gives an empty range at `start`.
    pub const fn new(start: u64, end: u64) -> Self {
        Self { start, end: if end < start { start } else { end } }
    }

    /// `[start, start + len)`, saturating at the top of the address space.
    pub const fn from_len(start: u64, len: u64) -> Self {
        Self::new(start, start.saturating_add(len))
    }

    pub const fn len(&self) -> u64 {
        self.end - self.start
    }

    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub const fn contains(&self, addr: u64) -> bool {
        addr >= self.start && addr < self.end
    }

    /// Do the two ranges share at least one byte?
    pub const fn overlaps(&self, other: &PhysRange) -> bool {
        self.start < other.end && other.start < self.end
    }

    /// The largest frame-aligned range inside `self` (possibly empty).
    pub const fn shrink_to_frames(&self) -> Self {
        Self::new(align_up(self.start, FRAME_SIZE), align_down(self.end, FRAME_SIZE))
    }

    /// The smallest frame-aligned range covering `self`.
    pub const fn grow_to_frames(&self) -> Self {
        if self.is_empty() {
            return Self::new(self.start, self.start);
        }
        Self::new(align_down(self.start, FRAME_SIZE), align_up(self.end, FRAME_SIZE))
    }
}

impl fmt::Display for PhysRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:#014x}-{:#014x}", self.start, self.end)
    }
}

/// Why the allocator could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// The storage slice is shorter than [`storage_words`] for the span.
    StorageTooSmall,
    /// `add_usable` after the first `reserve`.
    Sealed,
}

/// Why a frame could not be freed. Every variant is a caller bug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreeError {
    /// The address is not frame-aligned.
    NotAligned,
    /// The address is outside the span the allocator manages.
    OutOfRange,
    /// The frame was never the allocator's to give (reserved or not RAM).
    NotOwned,
    /// The frame is already free.
    DoubleFree,
}

impl fmt::Display for FreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FreeError::NotAligned => "address is not frame-aligned",
            FreeError::OutOfRange => "address outside managed memory",
            FreeError::NotOwned => "frame is reserved or not RAM",
            FreeError::DoubleFree => "frame is already free",
        })
    }
}

fn frames_in(span: PhysRange) -> usize {
    (span.grow_to_frames().len() / FRAME_SIZE) as usize
}

/// Number of `u64` words of storage [`FrameAllocator::new`] needs for `span`.
pub fn storage_words(span: PhysRange) -> usize {
    2 * frames_in(span).div_ceil(BITS)
}

/// Bytes of storage [`FrameAllocator::new`] needs for `span`.
pub fn storage_bytes(span: PhysRange) -> u64 {
    storage_words(span) as u64 * 8
}

/// The lowest frame-aligned address `a` such that `[a, a + size)` lies inside
/// one of `usable` (each rounded inward to whole frames) and overlaps none of
/// `avoid`. Used to place the allocator's own bitmaps before it exists.
pub fn find_hole(
    usable: impl IntoIterator<Item = PhysRange>,
    avoid: impl IntoIterator<Item = PhysRange> + Clone,
    size: u64,
) -> Option<u64> {
    let mut best: Option<u64> = None;
    for region in usable {
        let region = region.shrink_to_frames();
        let mut candidate = region.start;
        while let Some(end) = candidate.checked_add(size) {
            if end > region.end {
                break;
            }
            let want = PhysRange::new(candidate, end);
            match avoid.clone().into_iter().filter(|a| a.overlaps(&want)).map(|a| a.end).max() {
                Some(blocker_end) => candidate = align_up(blocker_end, FRAME_SIZE),
                None => {
                    best = Some(best.map_or(candidate, |b| b.min(candidate)));
                    break;
                }
            }
        }
    }
    best
}

/// A bitmap frame allocator over one contiguous span of physical memory.
pub struct FrameAllocator<'a> {
    /// Frame number (address / FRAME_SIZE) of bit 0.
    base_frame: u64,
    frames: usize,
    owned: &'a mut [u64],
    free: &'a mut [u64],
    owned_count: usize,
    free_count: usize,
    /// No free frame has an index below this.
    hint: usize,
    sealed: bool,
}

impl<'a> FrameAllocator<'a> {
    /// An allocator for `span` (rounded out to whole frames) with nothing
    /// usable yet. `storage` holds both bitmaps and must have at least
    /// [`storage_words`] words; it is zeroed here.
    pub fn new(span: PhysRange, storage: &'a mut [u64]) -> Result<Self, InitError> {
        let span = span.grow_to_frames();
        let frames = frames_in(span);
        let words = frames.div_ceil(BITS);
        if storage.len() < 2 * words {
            return Err(InitError::StorageTooSmall);
        }
        let (owned, rest) = storage.split_at_mut(words);
        let free = &mut rest[..words];
        owned.fill(0);
        free.fill(0);
        Ok(Self {
            base_frame: span.start / FRAME_SIZE,
            frames,
            owned,
            free,
            owned_count: 0,
            free_count: 0,
            hint: 0,
            sealed: false,
        })
    }

    /// The physical span this allocator manages.
    pub fn span(&self) -> PhysRange {
        let start = self.base_frame * FRAME_SIZE;
        PhysRange::new(start, start + self.frames as u64 * FRAME_SIZE)
    }

    /// Frames the allocator may hand out (allocated or not).
    pub fn owned_frames(&self) -> usize {
        self.owned_count
    }

    /// Frames currently free.
    pub fn free_frames(&self) -> usize {
        self.free_count
    }

    /// Is the frame containing `addr` owned (usable and not reserved)?
    pub fn is_owned(&self, addr: u64) -> bool {
        self.index_of(addr).is_some_and(|i| bit(self.owned, i))
    }

    /// Is the frame containing `addr` free?
    pub fn is_free(&self, addr: u64) -> bool {
        self.index_of(addr).is_some_and(|i| bit(self.free, i))
    }

    /// Mark the whole frames inside `range` as usable and free. Returns how
    /// many frames became usable (frames already usable are not counted).
    pub fn add_usable(&mut self, range: PhysRange) -> Result<usize, InitError> {
        if self.sealed {
            return Err(InitError::Sealed);
        }
        let (lo, hi) = self.indices(range.shrink_to_frames());
        let mut added = 0;
        for i in lo..hi {
            if !bit(self.owned, i) {
                set(self.owned, i, true);
                set(self.free, i, true);
                added += 1;
            }
        }
        self.owned_count += added;
        self.free_count += added;
        self.hint = self.hint.min(lo);
        Ok(added)
    }

    /// Take every frame that `range` touches away from the allocator for
    /// good. Returns how many owned frames that removed. Seals the usable set.
    pub fn reserve(&mut self, range: PhysRange) -> usize {
        self.sealed = true;
        let (lo, hi) = self.indices(range.grow_to_frames());
        let mut removed = 0;
        for i in lo..hi {
            if bit(self.owned, i) {
                if bit(self.free, i) {
                    self.free_count -= 1;
                }
                set(self.owned, i, false);
                set(self.free, i, false);
                removed += 1;
            }
        }
        self.owned_count -= removed;
        removed
    }

    /// Allocate the lowest free frame; returns its physical address.
    pub fn alloc(&mut self) -> Option<u64> {
        for w in self.hint / BITS..self.free.len() {
            let bits = self.free[w];
            if bits != 0 {
                // Bits below `hint` are clear (invariant), so this is >= hint.
                let i = w * BITS + bits.trailing_zeros() as usize;
                set(self.free, i, false);
                self.free_count -= 1;
                self.hint = i + 1;
                return Some(self.addr_of(i));
            }
        }
        self.hint = self.frames;
        None
    }

    /// Allocate `count` physically contiguous frames whose first address is
    /// aligned to `align_frames` frames (a power of two); returns the first
    /// address. `None` if `count` is 0, `align_frames` is not a power of two,
    /// or no such run is free.
    pub fn alloc_contiguous(&mut self, count: usize, align_frames: u64) -> Option<u64> {
        if count == 0 || !align_frames.is_power_of_two() {
            return None;
        }
        let mut i = self.hint;
        while i.checked_add(count)? <= self.frames {
            let aligned = (align_up(self.base_frame + i as u64, align_frames) - self.base_frame) as usize;
            if aligned != i {
                i = aligned;
                continue;
            }
            match (i..i + count).find(|&j| !bit(self.free, j)) {
                Some(busy) => i = busy + 1,
                None => {
                    for j in i..i + count {
                        set(self.free, j, false);
                    }
                    self.free_count -= count;
                    if i == self.hint {
                        self.hint = i + count;
                    }
                    return Some(self.addr_of(i));
                }
            }
        }
        None
    }

    /// Return a frame obtained from [`alloc`](Self::alloc) or
    /// [`alloc_contiguous`](Self::alloc_contiguous).
    pub fn free(&mut self, addr: u64) -> Result<(), FreeError> {
        if !addr.is_multiple_of(FRAME_SIZE) {
            return Err(FreeError::NotAligned);
        }
        let i = self.index_of(addr).ok_or(FreeError::OutOfRange)?;
        if !bit(self.owned, i) {
            return Err(FreeError::NotOwned);
        }
        if bit(self.free, i) {
            return Err(FreeError::DoubleFree);
        }
        set(self.free, i, true);
        self.free_count += 1;
        self.hint = self.hint.min(i);
        Ok(())
    }

    fn addr_of(&self, i: usize) -> u64 {
        (self.base_frame + i as u64) * FRAME_SIZE
    }

    fn index_of(&self, addr: u64) -> Option<usize> {
        let frame = (addr / FRAME_SIZE).checked_sub(self.base_frame)?;
        (frame < self.frames as u64).then_some(frame as usize)
    }

    /// Bitmap indices `[lo, hi)` of the frames of an aligned `range`, clamped
    /// to the span.
    fn indices(&self, range: PhysRange) -> (usize, usize) {
        let clamp = |addr: u64| ((addr / FRAME_SIZE).saturating_sub(self.base_frame)).min(self.frames as u64) as usize;
        let (lo, hi) = (clamp(range.start), clamp(range.end));
        (lo, hi.max(lo))
    }
}

fn bit(map: &[u64], i: usize) -> bool {
    map[i / BITS] & (1 << (i % BITS)) != 0
}

fn set(map: &mut [u64], i: usize, value: bool) {
    if value {
        map[i / BITS] |= 1 << (i % BITS);
    } else {
        map[i / BITS] &= !(1 << (i % BITS));
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::collections::BTreeSet;
    use std::{vec, vec::Vec};

    const F: u64 = FRAME_SIZE;

    fn r(start: u64, end: u64) -> PhysRange {
        PhysRange::new(start, end)
    }

    fn storage(span: PhysRange) -> Vec<u64> {
        vec![0xdead_beef; storage_words(span)]
    }

    /// Allocate until exhausted; returns the addresses in order.
    fn drain(fa: &mut FrameAllocator<'_>) -> Vec<u64> {
        core::iter::from_fn(|| fa.alloc()).collect()
    }

    #[test]
    fn rounding_inward_and_outward() {
        let x = r(F + 1, 3 * F - 1);
        assert_eq!(x.shrink_to_frames(), r(2 * F, 2 * F));
        assert!(x.shrink_to_frames().is_empty());
        assert_eq!(x.grow_to_frames(), r(F, 3 * F));
        assert_eq!(r(5, 5).grow_to_frames().len(), 0);
        assert_eq!(align_up(u64::MAX - 5, F), !(F - 1));
        assert_eq!(PhysRange::new(10, 2), r(10, 10));
    }

    #[test]
    fn starts_empty_and_rejects_short_storage() {
        let span = r(0, 1000 * F);
        let mut short = vec![0; storage_words(span) - 1];
        assert_eq!(FrameAllocator::new(span, &mut short).err(), Some(InitError::StorageTooSmall));
        let mut s = storage(span);
        let mut fa = FrameAllocator::new(span, &mut s).unwrap();
        assert_eq!((fa.owned_frames(), fa.free_frames()), (0, 0));
        assert_eq!(fa.alloc(), None, "nothing is usable until add_usable");
    }

    #[test]
    fn alloc_free_reuse() {
        let span = r(0, 64 * F);
        let mut s = storage(span);
        let mut fa = FrameAllocator::new(span, &mut s).unwrap();
        fa.add_usable(r(16 * F, 32 * F)).unwrap();
        let a = fa.alloc().unwrap();
        let b = fa.alloc().unwrap();
        assert_eq!((a, b), (16 * F, 17 * F), "lowest frame first");
        assert_eq!(fa.free_frames(), 14);
        fa.free(a).unwrap();
        assert_eq!(fa.alloc(), Some(a), "a freed frame is reused");
        fa.free(b).unwrap();
        fa.free(a).unwrap();
        assert_eq!(fa.free_frames(), 16);
    }

    #[test]
    fn never_double_allocates_and_exhausts_cleanly() {
        // Several regions, a span that isn't a multiple of 64 frames.
        let span = r(0, 300 * F);
        let mut s = storage(span);
        let mut fa = FrameAllocator::new(span, &mut s).unwrap();
        fa.add_usable(r(F, 70 * F)).unwrap();
        fa.add_usable(r(100 * F, 101 * F)).unwrap();
        fa.add_usable(r(200 * F, 300 * F)).unwrap();
        let total = fa.free_frames();
        let got = drain(&mut fa);
        let unique: BTreeSet<_> = got.iter().copied().collect();
        assert_eq!(got.len(), total);
        assert_eq!(unique.len(), total, "a frame was handed out twice");
        assert!(got.iter().all(|&a| a.is_multiple_of(F) && (F..300 * F).contains(&a)));
        assert_eq!(fa.alloc(), None);
        assert_eq!(fa.alloc_contiguous(1, 1), None);
        for a in got {
            fa.free(a).unwrap();
        }
        assert_eq!(fa.free_frames(), total);
        assert_eq!(drain(&mut fa).len(), total, "everything is reusable after freeing");
    }

    #[test]
    fn unaligned_usable_entries_round_inward() {
        let span = r(0, 16 * F);
        let mut s = storage(span);
        let mut fa = FrameAllocator::new(span, &mut s).unwrap();
        // Only frames 2..5 are wholly inside.
        assert_eq!(fa.add_usable(r(F + 7, 5 * F + 100)).unwrap(), 3);
        // Less than one frame: nothing.
        assert_eq!(fa.add_usable(r(8 * F + 1, 9 * F + 5)).unwrap(), 0);
        assert_eq!(drain(&mut fa), vec![2 * F, 3 * F, 4 * F]);
    }

    #[test]
    fn overlapping_usable_entries_are_counted_once() {
        let span = r(0, 32 * F);
        let mut s = storage(span);
        let mut fa = FrameAllocator::new(span, &mut s).unwrap();
        assert_eq!(fa.add_usable(r(0, 10 * F)).unwrap(), 10);
        assert_eq!(fa.add_usable(r(5 * F, 15 * F)).unwrap(), 5);
        assert_eq!(fa.free_frames(), 15);
        assert_eq!(drain(&mut fa).len(), 15);
    }

    #[test]
    fn reservations_round_outward_and_win_over_usable() {
        let span = r(0, 32 * F);
        let mut s = storage(span);
        let mut fa = FrameAllocator::new(span, &mut s).unwrap();
        fa.add_usable(r(0, 32 * F)).unwrap();
        // One byte reserves its whole frame; an unaligned range both frames.
        assert_eq!(fa.reserve(r(0, 1)), 1);
        assert_eq!(fa.reserve(r(4 * F + 10, 5 * F + 10)), 2);
        // Overlapping reservations don't double count.
        assert_eq!(fa.reserve(r(5 * F, 7 * F)), 1);
        assert_eq!(fa.free_frames(), 32 - 4);
        let got = drain(&mut fa);
        for reserved in [0, 4 * F, 5 * F, 6 * F] {
            assert!(!got.contains(&reserved), "reserved frame {reserved:#x} handed out");
            assert!(!fa.is_owned(reserved));
        }
    }

    #[test]
    fn add_usable_after_reserve_is_refused() {
        let span = r(0, 8 * F);
        let mut s = storage(span);
        let mut fa = FrameAllocator::new(span, &mut s).unwrap();
        fa.add_usable(r(0, 8 * F)).unwrap();
        fa.reserve(r(2 * F, 3 * F));
        assert_eq!(fa.add_usable(r(0, 8 * F)), Err(InitError::Sealed));
        assert!(!fa.is_owned(2 * F), "a reservation can't be undone");
    }

    #[test]
    fn ranges_outside_the_span_are_clamped() {
        let span = r(16 * F, 32 * F);
        let mut s = storage(span);
        let mut fa = FrameAllocator::new(span, &mut s).unwrap();
        assert_eq!(fa.add_usable(r(0, 1 << 40)).unwrap(), 16);
        assert_eq!(fa.reserve(r(0, 17 * F)), 1);
        assert_eq!(fa.reserve(r(1 << 40, u64::MAX)), 0);
        assert_eq!(drain(&mut fa).first(), Some(&(17 * F)));
    }

    #[test]
    fn free_rejects_bad_addresses() {
        let span = r(0, 16 * F);
        let mut s = storage(span);
        let mut fa = FrameAllocator::new(span, &mut s).unwrap();
        fa.add_usable(r(0, 8 * F)).unwrap();
        fa.reserve(r(0, F));
        let a = fa.alloc().unwrap();
        assert_eq!(fa.free(a + 1), Err(FreeError::NotAligned));
        assert_eq!(fa.free(0), Err(FreeError::NotOwned), "reserved frame");
        assert_eq!(fa.free(12 * F), Err(FreeError::NotOwned), "inside span, never usable");
        assert_eq!(fa.free(16 * F), Err(FreeError::OutOfRange));
        assert_eq!(fa.free(a), Ok(()));
        assert_eq!(fa.free(a), Err(FreeError::DoubleFree));
        assert_eq!(fa.free_frames(), 7, "failed frees changed nothing");
    }

    #[test]
    fn contiguous_allocation_respects_alignment_and_holes() {
        let span = r(0, 128 * F);
        let mut s = storage(span);
        let mut fa = FrameAllocator::new(span, &mut s).unwrap();
        fa.add_usable(r(F, 128 * F)).unwrap();
        fa.reserve(r(10 * F, 11 * F));
        // 8 frames aligned to 8: frame 0 is not usable and 8..16 contains the
        // reserved frame 10, so the first fit is 16.
        let a = fa.alloc_contiguous(8, 8).unwrap();
        assert_eq!(a, 16 * F);
        assert!((16..24).all(|f| !fa.is_free(f * F)));
        // Unaligned single frames still come from the lowest free frame.
        assert_eq!(fa.alloc(), Some(F));
        assert_eq!(fa.alloc_contiguous(0, 1), None);
        assert_eq!(fa.alloc_contiguous(1, 3), None, "alignment must be a power of two");
        assert_eq!(fa.alloc_contiguous(1000, 1), None, "larger than memory");
        assert_eq!(fa.alloc_contiguous(usize::MAX, 1), None, "no overflow");
        let before = fa.free_frames();
        for f in 16..24 {
            fa.free(f * F).unwrap();
        }
        assert_eq!(fa.free_frames(), before + 8);
        assert_eq!(fa.alloc_contiguous(8, 8), Some(16 * F), "freed run is reusable");
    }

    #[test]
    fn contiguous_alignment_is_absolute_not_relative_to_the_span() {
        let span = r(3 * F, 64 * F);
        let mut s = storage(span);
        let mut fa = FrameAllocator::new(span, &mut s).unwrap();
        fa.add_usable(span).unwrap();
        assert_eq!(fa.alloc_contiguous(4, 4), Some(4 * F));
        assert_eq!(fa.alloc_contiguous(4, 16), Some(16 * F));
    }

    #[test]
    fn find_hole_skips_reservations_and_small_regions() {
        let usable = [r(F, 4 * F), r(16 * F + 3, 64 * F)];
        let avoid = [r(0, F), r(16 * F, 20 * F + 1)];
        // 4 frames don't fit in the first region (3 frames); in the second the
        // reservation pushes the start to the next frame boundary after it.
        assert_eq!(find_hole(usable, avoid, 4 * F), Some(21 * F));
        assert_eq!(find_hole(usable, avoid, 2 * F), Some(F));
        assert_eq!(find_hole(usable, avoid, 1 << 30), None);
        // Map order doesn't matter: the lowest hole wins.
        assert_eq!(find_hole([usable[1], usable[0]], avoid, 2 * F), Some(F));
    }
}
