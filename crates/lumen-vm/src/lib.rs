//! Per-process user virtual memory area (VMA) interval tree and `brk` cursor.
//!
//! Pure `no_std` + `alloc` logic tested on the host (`cargo test -p lumen-vm`).
//! Manages non-overlapping, page-aligned virtual address ranges strictly within
//! [`USER_MIN`]..[`USER_MAX`], with:
//!
//! - exact-range lookup (`find`) and overlap queries;
//! - top-down free-gap allocation from [`MMAP_TOP`] with optional hint,
//!   `MAP_FIXED` unmap-and-replace, and `MAP_FIXED_NOREPLACE` (`Err(Exists)`);
//! - partial-range `unmap` (ignoring holes, returning removed sub-ranges for
//!   PTE teardown) and atomic `protect` (refusing unmapped holes, supporting
//!   `PROT_NONE` guard pages, splitting partial VMAs, and coalescing adjacent
//!   identical VMAs);
//! - byte-granular [`Brk`] cursor tracking over page-aligned backing VMAs.

#![no_std]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::fmt;
use core::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign};

/// Virtual page size (4 KiB).
pub const PAGE_SIZE: u64 = 4096;
/// Lowest mappable user address (first 64 KiB stays unmapped as a null guard).
pub const USER_MIN: u64 = 0x0000_0000_0001_0000;
/// Exclusive upper bound of user space (top canonical user page
/// `0x0000_7fff_ffff_f000` stays unmapped as a boundary guard).
pub const USER_MAX: u64 = 0x0000_7fff_ffff_f000;
/// Default load base for PIE executables.
pub const PIE_BASE: u64 = 0x0000_5555_5555_4000;
/// Default load base for the dynamic linker (`ld-musl`).
pub const INTERP_BASE: u64 = 0x0000_7f00_0000_0000;
/// Top of the top-down `mmap` search region.
pub const MMAP_TOP: u64 = 0x0000_7f00_0000_0000;
/// Initial top of the user stack.
pub const USER_STACK_TOP: u64 = 0x0000_7fff_f000_0000;

const _: () = {
    assert!(PAGE_SIZE.is_power_of_two());
    assert!(USER_MIN.is_multiple_of(PAGE_SIZE));
    assert!(USER_MAX.is_multiple_of(PAGE_SIZE));
    assert!(PIE_BASE.is_multiple_of(PAGE_SIZE));
    assert!(INTERP_BASE.is_multiple_of(PAGE_SIZE));
    assert!(MMAP_TOP.is_multiple_of(PAGE_SIZE));
    assert!(USER_STACK_TOP.is_multiple_of(PAGE_SIZE));
    assert!(USER_MIN < PIE_BASE && PIE_BASE < MMAP_TOP);
    assert!(MMAP_TOP < USER_STACK_TOP && USER_STACK_TOP < USER_MAX);
};

/// Is `addr` a multiple of [`PAGE_SIZE`]?
pub const fn is_page_aligned(addr: u64) -> bool {
    addr & (PAGE_SIZE - 1) == 0
}

/// Round `val` down to a multiple of [`PAGE_SIZE`].
pub const fn align_down_page(val: u64) -> u64 {
    val & !(PAGE_SIZE - 1)
}

/// Round `val` up to a multiple of [`PAGE_SIZE`], or `None` on overflow.
pub const fn align_up_page(val: u64) -> Option<u64> {
    match val.checked_add(PAGE_SIZE - 1) {
        Some(v) => Some(v & !(PAGE_SIZE - 1)),
        None => None,
    }
}

/// Memory protection flags (`PROT_NONE`, `PROT_READ`, `PROT_WRITE`, `PROT_EXEC`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Prot(u8);

impl Prot {
    /// `PROT_NONE`: no access permitted (reserves VA space / guard pages).
    pub const NONE: Self = Self(0);
    /// `PROT_READ`.
    pub const READ: Self = Self(1 << 0);
    /// `PROT_WRITE`.
    pub const WRITE: Self = Self(1 << 1);
    /// `PROT_EXEC`.
    pub const EXEC: Self = Self(1 << 2);
    /// `PROT_READ | PROT_WRITE | PROT_EXEC`.
    pub const ALL: Self = Self(Self::READ.0 | Self::WRITE.0 | Self::EXEC.0);

    /// Build from raw protection bits, returning `None` if unknown bits are set.
    pub const fn from_bits(bits: u8) -> Option<Self> {
        if bits & !Self::ALL.0 == 0 { Some(Self(bits)) } else { None }
    }

    /// Raw bitmask (`READ = 1`, `WRITE = 2`, `EXEC = 4`).
    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    pub const fn read(self) -> bool {
        self.0 & Self::READ.0 != 0
    }

    pub const fn write(self) -> bool {
        self.0 & Self::WRITE.0 != 0
    }

    pub const fn exec(self) -> bool {
        self.0 & Self::EXEC.0 != 0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

impl BitOr for Prot {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

impl BitOrAssign for Prot {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for Prot {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        self.intersection(rhs)
    }
}

impl BitAndAssign for Prot {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl fmt::Display for Prot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}{}",
            if self.read() { 'r' } else { '-' },
            if self.write() { 'w' } else { '-' },
            if self.exec() { 'x' } else { '-' },
        )
    }
}

/// Backing store of a [`Vma`] (extensible to file-backed mappings in M7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backing {
    /// Demand-zeroed anonymous memory.
    Anonymous,
}

/// A single non-empty, page-aligned virtual memory area `[start, end)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vma {
    pub start: u64,
    pub end: u64,
    pub prot: Prot,
    pub backing: Backing,
}

impl Vma {
    /// Length in bytes (`end - start`, always a non-zero multiple of [`PAGE_SIZE`]).
    pub const fn len(&self) -> u64 {
        self.end - self.start
    }

    /// Always `false` for any VMA stored in a [`VmaMap`].
    pub const fn is_empty(&self) -> bool {
        self.start >= self.end
    }

    /// Does this VMA cover `addr`?
    pub const fn contains(&self, addr: u64) -> bool {
        self.start <= addr && addr < self.end
    }

    /// Does this VMA overlap `[start, end)`?
    pub const fn overlaps(&self, start: u64, end: u64) -> bool {
        start < end && self.start < end && start < self.end
    }

    /// Can `self` coalesce with an immediately following VMA `next`
    /// (`self.end == next.start`)?
    pub const fn can_merge_with(&self, next: &Self) -> bool {
        self.end == next.start
            && self.prot.0 == next.prot.0
            && matches!((self.backing, next.backing), (Backing::Anonymous, Backing::Anonymous))
    }
}

/// Why a VMA operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmError {
    /// Start address is not page-aligned.
    Misaligned,
    /// Range is empty (where disallowed), overflows, or lies outside
    /// `[USER_MIN, USER_MAX]`.
    InvalidRange,
    /// No free gap large enough, or `protect` encountered an unmapped hole.
    NoMem,
    /// `MAP_FIXED_NOREPLACE` overlapped an existing mapping (`-EEXIST`).
    Exists,
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            VmError::Misaligned => "address is not page-aligned",
            VmError::InvalidRange => "range is empty, overflows, or outside user bounds",
            VmError::NoMem => "no space or unmapped hole in range",
            VmError::Exists => "mapping already exists in range",
        })
    }
}

/// Placement policy for [`VmaMap::map_anon`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapPlace {
    /// Advisory placement: use `Some(addr)` (non-zero, aligned up to a page) if
    /// it fits within `[USER_MIN, USER_MAX]` without overlapping; otherwise
    /// allocate top-down from [`MMAP_TOP`].
    Hint(Option<u64>),
    /// `MAP_FIXED`: map at the exact page-aligned address, unmapping and
    /// replacing any overlapping VMAs.
    Fixed(u64),
    /// `MAP_FIXED_NOREPLACE`: map at the exact page-aligned address, or fail
    /// with [`VmError::Exists`] if any existing VMA overlaps.
    FixedNoReplace(u64),
}

/// Outcome of [`VmaMap::map_anon`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mapped {
    /// Page-aligned start of the new mapping.
    pub start: u64,
    /// Page-aligned exclusive end of the new mapping.
    pub end: u64,
    /// Existing sub-ranges removed by [`MapPlace::Fixed`] (empty otherwise).
    /// Callers must tear down PTEs and release physical frames for these ranges.
    pub unmapped: Vec<Vma>,
}

/// Per-process table of non-overlapping user VMAs in `[USER_MIN, USER_MAX]`.
///
/// Adjacent VMAs with identical `(prot, backing)` are coalesced automatically,
/// keeping the map in canonical form.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VmaMap {
    vmas: BTreeMap<u64, Vma>,
}

impl VmaMap {
    pub const fn new() -> Self {
        Self { vmas: BTreeMap::new() }
    }

    pub fn len(&self) -> usize {
        self.vmas.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vmas.is_empty()
    }

    /// Iterate over all VMAs in ascending address order.
    pub fn iter(&self) -> impl Iterator<Item = &Vma> {
        self.vmas.values()
    }

    /// The VMA containing `addr`, if any.
    pub fn find(&self, addr: u64) -> Option<&Vma> {
        self.vmas.range(..=addr).next_back().map(|(_, v)| v).filter(|v| v.contains(addr))
    }

    /// All VMAs overlapping `[start, end)` in ascending address order.
    pub fn overlapping(&self, start: u64, end: u64) -> impl Iterator<Item = &Vma> {
        // At most one VMA starting strictly before `start` can reach past `start`
        // (because VMAs are non-overlapping). Start the range from that VMA if it
        // exists, otherwise from `start`.
        let lower = if start < end {
            self.vmas.range(..start).next_back().filter(|(_, v)| v.end > start).map_or(start, |(&k, _)| k)
        } else {
            end
        };
        let upper = if start < end { end } else { lower };
        self.vmas.range(lower..upper).map(|(_, v)| v)
    }

    /// Does any VMA overlap `[start, end)`?
    pub fn overlaps(&self, start: u64, end: u64) -> bool {
        self.overlapping(start, end).next().is_some()
    }

    /// Find the highest page-aligned free gap of `len` bytes in `[USER_MIN, top]`
    /// (with `top` clamped to [`USER_MAX`] and aligned down). `len` must be a
    /// non-zero multiple of [`PAGE_SIZE`].
    pub fn find_free_top_down(&self, len: u64, top: u64) -> Option<u64> {
        if len == 0 || !is_page_aligned(len) {
            return None;
        }
        let mut cursor = align_down_page(top.min(USER_MAX));
        if cursor < USER_MIN || cursor - USER_MIN < len {
            return None;
        }
        for (_, vma) in self.vmas.range(..cursor).rev() {
            if vma.end < cursor && cursor - vma.end >= len {
                return Some(cursor - len);
            }
            cursor = cursor.min(vma.start);
            if cursor < USER_MIN || cursor - USER_MIN < len {
                return None;
            }
        }
        if cursor >= USER_MIN && cursor - USER_MIN >= len { Some(cursor - len) } else { None }
    }

    /// Map an anonymous region of `len` bytes (rounded up to [`PAGE_SIZE`])
    /// with protection `prot` according to `place`.
    pub fn map_anon(&mut self, place: MapPlace, len: u64, prot: Prot) -> Result<Mapped, VmError> {
        if len == 0 {
            return Err(VmError::InvalidRange);
        }
        let aligned_len = align_up_page(len).ok_or(VmError::InvalidRange)?;

        match place {
            MapPlace::Fixed(addr) => {
                let end = validate_user_range(addr, aligned_len)?;
                let unmapped = self.unmap_range_unchecked(addr, end);
                self.insert_and_merge(Vma { start: addr, end, prot, backing: Backing::Anonymous });
                Ok(Mapped { start: addr, end, unmapped })
            }
            MapPlace::FixedNoReplace(addr) => {
                let end = validate_user_range(addr, aligned_len)?;
                if self.overlaps(addr, end) {
                    return Err(VmError::Exists);
                }
                self.insert_and_merge(Vma { start: addr, end, prot, backing: Backing::Anonymous });
                Ok(Mapped { start: addr, end, unmapped: Vec::new() })
            }
            MapPlace::Hint(opt_addr) => {
                let start = opt_addr
                    .filter(|&a| a != 0)
                    .and_then(align_up_page)
                    .and_then(|a| {
                        let end = a.checked_add(aligned_len)?;
                        (a >= USER_MIN && end <= USER_MAX && !self.overlaps(a, end)).then_some(a)
                    })
                    .or_else(|| self.find_free_top_down(aligned_len, MMAP_TOP))
                    .ok_or(VmError::NoMem)?;
                let end = start + aligned_len;
                self.insert_and_merge(Vma { start, end, prot, backing: Backing::Anonymous });
                Ok(Mapped { start, end, unmapped: Vec::new() })
            }
        }
    }

    /// Convenience wrapper for `map_anon(MapPlace::Fixed(start), len, prot)`.
    pub fn map_fixed(&mut self, start: u64, len: u64, prot: Prot) -> Result<Vec<Vma>, VmError> {
        self.map_anon(MapPlace::Fixed(start), len, prot).map(|m| m.unmapped)
    }

    /// Convenience wrapper for `map_anon(MapPlace::FixedNoReplace(start), len, prot)`.
    pub fn map_fixed_noreplace(&mut self, start: u64, len: u64, prot: Prot) -> Result<u64, VmError> {
        self.map_anon(MapPlace::FixedNoReplace(start), len, prot).map(|m| m.start)
    }

    /// Unmap `[start, align_up_page(start + len))`.
    ///
    /// Refuses `len == 0`, misaligned `start`, overflow, or bounds outside
    /// `[USER_MIN, USER_MAX]`. Ignores unmapped holes, splits partially
    /// overlapping VMAs cleanly, and returns the exact removed sub-ranges in
    /// ascending order so callers know which PTEs to tear down.
    pub fn unmap(&mut self, start: u64, len: u64) -> Result<Vec<Vma>, VmError> {
        if !is_page_aligned(start) {
            return Err(VmError::Misaligned);
        }
        if len == 0 {
            return Err(VmError::InvalidRange);
        }
        let aligned_len = align_up_page(len).ok_or(VmError::InvalidRange)?;
        let end = validate_user_range(start, aligned_len)?;
        Ok(self.unmap_range_unchecked(start, end))
    }

    /// Change protections on `[start, align_up_page(start + len))` to `prot`.
    ///
    /// Refuses misaligned `start` (`Misaligned`), out-of-bounds or overflowing
    /// ranges (`InvalidRange`), or any unmapped hole in the range (`NoMem`,
    /// atomically leaving the map unchanged). Accepts `len == 0` when `start`
    /// is page-aligned. Splits partial VMAs and coalesces adjacent VMAs with
    /// identical `(prot, backing)`.
    pub fn protect(&mut self, start: u64, len: u64, prot: Prot) -> Result<(), VmError> {
        if !is_page_aligned(start) {
            return Err(VmError::Misaligned);
        }
        if len == 0 {
            return Ok(());
        }
        let aligned_len = align_up_page(len).ok_or(VmError::InvalidRange)?;
        let end = validate_user_range(start, aligned_len)?;

        // Verify complete coverage with zero gaps before mutating anything.
        let mut covered = start;
        let mut to_update = Vec::new();
        for &vma in self.overlapping(start, end) {
            if vma.start > covered {
                return Err(VmError::NoMem);
            }
            covered = covered.max(vma.end);
            to_update.push(vma);
        }
        if covered < end {
            return Err(VmError::NoMem);
        }

        for vma in &to_update {
            self.vmas.remove(&vma.start);
        }
        for vma in to_update {
            if vma.start < start {
                self.insert_and_merge(Vma { start: vma.start, end: start, ..vma });
            }
            let mid_start = vma.start.max(start);
            let mid_end = vma.end.min(end);
            self.insert_and_merge(Vma { start: mid_start, end: mid_end, prot, backing: vma.backing });
            if vma.end > end {
                self.insert_and_merge(Vma { start: end, end: vma.end, ..vma });
            }
        }
        Ok(())
    }

    /// Remove all mapped sub-ranges intersecting `[start, end)` (validated).
    fn unmap_range_unchecked(&mut self, start: u64, end: u64) -> Vec<Vma> {
        let affected: Vec<Vma> = self.overlapping(start, end).copied().collect();
        let mut removed = Vec::with_capacity(affected.len());
        for vma in affected {
            self.vmas.remove(&vma.start);
            if vma.start < start {
                self.vmas.insert(vma.start, Vma { start: vma.start, end: start, ..vma });
            }
            let cut_start = vma.start.max(start);
            let cut_end = vma.end.min(end);
            removed.push(Vma { start: cut_start, end: cut_end, ..vma });
            if vma.end > end {
                self.vmas.insert(end, Vma { start: end, end: vma.end, ..vma });
            }
        }
        removed
    }

    /// Insert `vma` (which must not overlap any existing VMA) and coalesce with
    /// adjacent compatible VMAs on either side.
    fn insert_and_merge(&mut self, mut vma: Vma) {
        if let Some((&prev_key, &prev)) = self.vmas.range(..vma.start).next_back()
            && prev.can_merge_with(&vma)
        {
            self.vmas.remove(&prev_key);
            vma.start = prev.start;
        }
        if let Some((&next_key, &next)) = self.vmas.range(vma.end..).next()
            && vma.can_merge_with(&next)
        {
            self.vmas.remove(&next_key);
            vma.end = next.end;
        }
        self.vmas.insert(vma.start, vma);
    }
}

/// Validate that `[start, start + aligned_len)` is page-aligned, non-empty,
/// does not overflow, and lies within `[USER_MIN, USER_MAX]`.
fn validate_user_range(start: u64, aligned_len: u64) -> Result<u64, VmError> {
    if !is_page_aligned(start) {
        return Err(VmError::Misaligned);
    }
    if aligned_len == 0 || !is_page_aligned(aligned_len) {
        return Err(VmError::InvalidRange);
    }
    let end = start.checked_add(aligned_len).ok_or(VmError::InvalidRange)?;
    if start < USER_MIN || end > USER_MAX {
        return Err(VmError::InvalidRange);
    }
    Ok(end)
}

/// Outcome of [`Brk::set`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrkChange {
    /// New program break on success, or unchanged current break on query /
    /// failure (matches Linux `sys_brk` return value).
    pub current: u64,
    /// `true` if `addr` was accepted as the new break; `false` if refused (or
    /// queried with `addr == 0`).
    pub succeeded: bool,
    /// Backing sub-ranges unmapped when shrinking across page boundaries.
    /// Callers must tear down their PTEs and free their physical frames.
    pub unmapped: Vec<Vma>,
}

/// Byte-granular `brk` heap cursor backed by page-aligned anonymous VMAs in a
/// [`VmaMap`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Brk {
    initial: u64,
    current: u64,
    mapped_end: u64,
    limit: u64,
}

impl Brk {
    /// Start a program break at page-aligned `initial` within
    /// `[USER_MIN, USER_MAX]`, with upper limit [`USER_MAX`].
    pub fn new(initial: u64) -> Result<Self, VmError> {
        Self::with_limit(initial, USER_MAX)
    }

    /// Start a program break at page-aligned `initial` with an explicit
    /// page-aligned upper `limit` (`USER_MIN <= initial <= limit <= USER_MAX`).
    pub fn with_limit(initial: u64, limit: u64) -> Result<Self, VmError> {
        if !is_page_aligned(initial) || !is_page_aligned(limit) {
            return Err(VmError::Misaligned);
        }
        if initial < USER_MIN || limit > USER_MAX || initial > limit {
            return Err(VmError::InvalidRange);
        }
        Ok(Self { initial, current: initial, mapped_end: initial, limit })
    }

    pub const fn initial(&self) -> u64 {
        self.initial
    }

    pub const fn current(&self) -> u64 {
        self.current
    }

    /// Exclusive page-aligned end of the pages currently claimed by `brk`.
    pub const fn mapped_end(&self) -> u64 {
        self.mapped_end
    }

    pub const fn limit(&self) -> u64 {
        self.limit
    }

    /// Move the program break to `addr` (Linux `brk` semantics).
    ///
    /// - Returns the exact requested `addr` on success (`succeeded: true`).
    /// - On `addr < initial` (including `addr == 0` queries), overflow, limit
    ///   exceeded, or collision with an existing VMA, leaves `current` and
    ///   `vmas` unchanged and returns `succeeded: false` with the current break.
    /// - Grows/shrinks backing page-aligned `RW-` anonymous VMAs only when
    ///   `align_up_page(addr)` crosses a page boundary.
    pub fn set(&mut self, vmas: &mut VmaMap, addr: u64) -> BrkChange {
        let unchanged = |cur| BrkChange { current: cur, succeeded: false, unmapped: Vec::new() };
        if addr < self.initial {
            return unchanged(self.current);
        }
        let Some(new_page_end) = align_up_page(addr) else {
            return unchanged(self.current);
        };
        if new_page_end > self.limit {
            return unchanged(self.current);
        }

        if new_page_end > self.mapped_end {
            let grow = new_page_end - self.mapped_end;
            if vmas.map_fixed_noreplace(self.mapped_end, grow, Prot::READ | Prot::WRITE).is_err() {
                return unchanged(self.current);
            }
            self.mapped_end = new_page_end;
            self.current = addr;
            BrkChange { current: addr, succeeded: true, unmapped: Vec::new() }
        } else if new_page_end < self.mapped_end {
            let shrink = self.mapped_end - new_page_end;
            let unmapped = vmas.unmap(new_page_end, shrink).expect("brk shrink range is aligned and in bounds");
            self.mapped_end = new_page_end;
            self.current = addr;
            BrkChange { current: addr, succeeded: true, unmapped }
        } else {
            self.current = addr;
            BrkChange { current: addr, succeeded: true, unmapped: Vec::new() }
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::format;
    use std::vec;

    const P: u64 = PAGE_SIZE;
    const RW: Prot = Prot::READ.union(Prot::WRITE);
    const RX: Prot = Prot::READ.union(Prot::EXEC);
    const RWX: Prot = Prot::ALL;

    fn assert_invariants(map: &VmaMap) {
        let mut prev: Option<Vma> = None;
        for (&key, &vma) in &map.vmas {
            assert_eq!(key, vma.start, "BTreeMap key must equal vma.start");
            assert!(is_page_aligned(vma.start) && is_page_aligned(vma.end), "unaligned {vma:?}");
            assert!(vma.start >= USER_MIN && vma.end <= USER_MAX, "out of user bounds {vma:?}");
            assert!(vma.start < vma.end, "empty or inverted {vma:?}");
            if let Some(p) = prev {
                assert!(p.end <= vma.start, "overlapping VMAs {p:?} and {vma:?}");
                assert!(!p.can_merge_with(&vma), "adjacent compatible VMAs were not merged: {p:?} and {vma:?}");
            }
            prev = Some(vma);
        }
    }

    #[test]
    fn prot_flags_and_display() {
        assert!(Prot::NONE.is_none());
        assert_eq!(format!("{}", Prot::NONE), "---");
        assert_eq!(format!("{}", Prot::READ), "r--");
        assert_eq!(format!("{}", RW), "rw-");
        assert_eq!(format!("{}", RX), "r-x");
        assert_eq!(format!("{}", RWX), "rwx");
        assert_eq!(Prot::from_bits(0), Some(Prot::NONE));
        assert_eq!(Prot::from_bits(7), Some(Prot::ALL));
        assert_eq!(Prot::from_bits(8), None);
        let mut p = Prot::READ;
        p |= Prot::WRITE;
        assert_eq!(p, RW);
        assert!(p.contains(Prot::READ) && p.contains(Prot::WRITE) && !p.contains(Prot::EXEC));
        p &= Prot::READ;
        assert_eq!(p, Prot::READ);
    }

    #[test]
    fn bounds_and_alignment_validation() {
        let mut map = VmaMap::new();
        // Zero length refused for map and unmap.
        assert_eq!(map.map_anon(MapPlace::Hint(None), 0, RW), Err(VmError::InvalidRange));
        assert_eq!(map.map_fixed(USER_MIN, 0, RW), Err(VmError::InvalidRange));
        assert_eq!(map.map_fixed_noreplace(USER_MIN, 0, RW), Err(VmError::InvalidRange));
        assert_eq!(map.unmap(USER_MIN, 0), Err(VmError::InvalidRange));
        // Misaligned start refused for fixed, unmap, protect.
        assert_eq!(map.map_fixed(USER_MIN + 1, P, RW), Err(VmError::Misaligned));
        assert_eq!(map.map_fixed_noreplace(USER_MIN + 8, P, RW), Err(VmError::Misaligned));
        assert_eq!(map.unmap(USER_MIN + 1, P), Err(VmError::Misaligned));
        assert_eq!(map.protect(USER_MIN + 1, P, Prot::READ), Err(VmError::Misaligned));
        assert_eq!(map.protect(USER_MIN + 1, 0, Prot::READ), Err(VmError::Misaligned));
        // Below USER_MIN (null guard) and at/above USER_MAX (boundary guard) refused.
        assert_eq!(map.map_fixed(0, P, RW), Err(VmError::InvalidRange));
        assert_eq!(map.map_fixed(USER_MIN - P, P, RW), Err(VmError::InvalidRange));
        assert_eq!(map.map_fixed(USER_MAX, P, RW), Err(VmError::InvalidRange));
        assert_eq!(map.map_fixed(USER_MAX - P, 2 * P, RW), Err(VmError::InvalidRange));
        assert_eq!(map.map_fixed(USER_MIN, u64::MAX, RW), Err(VmError::InvalidRange));
        assert_eq!(map.unmap(0, P), Err(VmError::InvalidRange));
        assert_eq!(map.unmap(USER_MAX, P), Err(VmError::InvalidRange));
        assert_eq!(map.protect(0, P, Prot::READ), Err(VmError::InvalidRange));
        assert_eq!(map.protect(USER_MAX, P, Prot::READ), Err(VmError::InvalidRange));
        // Exact boundary pages USER_MIN and USER_MAX - P succeed.
        assert_eq!(map.map_fixed_noreplace(USER_MIN, P, RW), Ok(USER_MIN));
        assert_eq!(map.map_fixed_noreplace(USER_MAX - P, P, RW), Ok(USER_MAX - P));
        assert!(map.find(USER_MIN).is_some());
        assert!(map.find(USER_MAX - 1).is_some());
        assert!(map.find(USER_MAX).is_none());
        assert!(map.find(USER_MIN - 1).is_none());
        assert_invariants(&map);
    }

    #[test]
    fn top_down_allocation_from_mmap_top_and_hints() {
        let mut map = VmaMap::new();
        // Top-down from MMAP_TOP.
        let a = map.map_anon(MapPlace::Hint(None), P, RW).unwrap();
        assert_eq!((a.start, a.end), (MMAP_TOP - P, MMAP_TOP));
        assert!(a.unmapped.is_empty());
        // Second allocation lands directly below the first (and coalesces since both are RW).
        let b = map.map_anon(MapPlace::Hint(Some(0)), 2 * P, RW).unwrap();
        assert_eq!((b.start, b.end), (MMAP_TOP - 3 * P, MMAP_TOP - P));
        assert_eq!(map.len(), 1);
        assert_eq!(map.find(MMAP_TOP - 3 * P).unwrap().end, MMAP_TOP);

        // Non-aligned length rounds up to page size.
        let c = map.map_anon(MapPlace::Hint(None), 1, Prot::READ).unwrap();
        assert_eq!((c.start, c.end), (MMAP_TOP - 4 * P, MMAP_TOP - 3 * P));
        assert_eq!(map.len(), 2);

        // Valid free hint is honoured.
        let hint = PIE_BASE;
        let h = map.map_anon(MapPlace::Hint(Some(hint)), 2 * P, RX).unwrap();
        assert_eq!((h.start, h.end), (hint, hint + 2 * P));

        // Overlapping hint falls back to top-down search.
        let fb = map.map_anon(MapPlace::Hint(Some(hint + P)), P, RX).unwrap();
        assert_eq!((fb.start, fb.end), (MMAP_TOP - 5 * P, MMAP_TOP - 4 * P));

        // Out-of-bounds hint (below USER_MIN) falls back to top-down search.
        let low = map.map_anon(MapPlace::Hint(Some(P)), P, Prot::NONE).unwrap();
        assert_eq!((low.start, low.end), (MMAP_TOP - 6 * P, MMAP_TOP - 5 * P));

        // A fixed mapping straddling MMAP_TOP does not fool top-down search.
        let mut m2 = VmaMap::new();
        m2.map_fixed_noreplace(MMAP_TOP - 2 * P, 4 * P, RW).unwrap();
        let below = m2.map_anon(MapPlace::Hint(None), P, Prot::READ).unwrap();
        assert_eq!((below.start, below.end), (MMAP_TOP - 3 * P, MMAP_TOP - 2 * P));

        // Request larger than available space returns NoMem.
        assert_eq!(m2.map_anon(MapPlace::Hint(None), MMAP_TOP, RW), Err(VmError::NoMem));
        assert_invariants(&map);
        assert_invariants(&m2);
    }

    #[test]
    fn map_fixed_replaces_and_returns_unmapped_subranges() {
        let mut map = VmaMap::new();
        let base = 0x20_0000;
        map.map_fixed_noreplace(base, 4 * P, RW).unwrap();
        map.map_fixed_noreplace(base + 6 * P, 4 * P, RX).unwrap();

        // MAP_FIXED spanning tail of first VMA, hole, and head of second VMA.
        let removed = map.map_fixed(base + 2 * P, 6 * P, Prot::READ).unwrap();
        assert_eq!(
            removed,
            vec![
                Vma { start: base + 2 * P, end: base + 4 * P, prot: RW, backing: Backing::Anonymous },
                Vma { start: base + 6 * P, end: base + 8 * P, prot: RX, backing: Backing::Anonymous },
            ]
        );
        let vmas: Vec<_> = map.iter().copied().collect();
        assert_eq!(
            vmas,
            vec![
                Vma { start: base, end: base + 2 * P, prot: RW, backing: Backing::Anonymous },
                Vma { start: base + 2 * P, end: base + 8 * P, prot: Prot::READ, backing: Backing::Anonymous },
                Vma { start: base + 8 * P, end: base + 10 * P, prot: RX, backing: Backing::Anonymous },
            ]
        );
        assert_invariants(&map);
    }

    #[test]
    fn map_fixed_noreplace_rejects_overlaps() {
        let mut map = VmaMap::new();
        let base = 0x40_0000;
        map.map_fixed_noreplace(base + 2 * P, 3 * P, RW).unwrap();
        let snapshot = map.clone();
        // Overlap from left, inside, right, exact, and enclosing.
        for (s, l) in [(base, 3 * P), (base + 2 * P, P), (base + 3 * P, P), (base + 4 * P, 2 * P), (base, 8 * P)] {
            assert_eq!(map.map_fixed_noreplace(s, l, Prot::READ), Err(VmError::Exists));
            assert_eq!(map, snapshot);
        }
        // Touching boundaries succeed.
        assert_eq!(map.map_fixed_noreplace(base, 2 * P, Prot::READ), Ok(base));
        assert_eq!(map.map_fixed_noreplace(base + 5 * P, 2 * P, Prot::READ), Ok(base + 5 * P));
        assert_invariants(&map);
    }

    #[test]
    fn unmap_splits_handles_holes_and_validates() {
        let mut map = VmaMap::new();
        let base = 0x50_0000;
        map.map_fixed_noreplace(base, 10 * P, RW).unwrap();

        // Split middle out of one VMA.
        let mid = map.unmap(base + 3 * P, 2 * P).unwrap();
        assert_eq!(mid, vec![Vma { start: base + 3 * P, end: base + 5 * P, prot: RW, backing: Backing::Anonymous }]);
        assert_eq!(map.len(), 2);
        assert_eq!(map.find(base + 2 * P).unwrap().end, base + 3 * P);
        assert!(map.find(base + 3 * P).is_none());
        assert_eq!(map.find(base + 5 * P).unwrap().start, base + 5 * P);

        // Unmapping an unmapped hole returns empty Vec.
        assert_eq!(map.unmap(base + 3 * P, 2 * P), Ok(Vec::new()));

        // Unmapping across left tail, hole, and right head trims both cleanly.
        let span = map.unmap(base + 2 * P, 4 * P).unwrap();
        assert_eq!(
            span,
            vec![
                Vma { start: base + 2 * P, end: base + 3 * P, prot: RW, backing: Backing::Anonymous },
                Vma { start: base + 5 * P, end: base + 6 * P, prot: RW, backing: Backing::Anonymous },
            ]
        );
        let remaining: Vec<_> = map.iter().map(|v| (v.start, v.end)).collect();
        assert_eq!(remaining, vec![(base, base + 2 * P), (base + 6 * P, base + 10 * P)]);
        assert_invariants(&map);
    }

    #[test]
    fn protect_splits_merges_and_rejects_holes() {
        let mut map = VmaMap::new();
        let base = 0x60_0000;
        // Zero-length protect on page-aligned address succeeds even when unmapped.
        assert_eq!(map.protect(base, 0, Prot::READ), Ok(()));

        map.map_fixed_noreplace(base, 8 * P, RW).unwrap();
        // Carve a PROT_NONE guard page in the middle (musl mallocng pattern).
        map.protect(base + 3 * P, P, Prot::NONE).unwrap();
        assert_eq!(
            map.iter().map(|v| (v.start, v.end, v.prot)).collect::<Vec<_>>(),
            vec![(base, base + 3 * P, RW), (base + 3 * P, base + 4 * P, Prot::NONE), (base + 4 * P, base + 8 * P, RW),]
        );
        // Restoring RW coalesces all three back into one VMA.
        map.protect(base + 3 * P, P, RW).unwrap();
        assert_eq!(map.iter().map(|v| (v.start, v.end, v.prot)).collect::<Vec<_>>(), vec![(base, base + 8 * P, RW)]);

        // Partial protect across adjacent differently-protected VMAs coalesces cleanly.
        map.protect(base, 4 * P, Prot::READ).unwrap();
        map.protect(base + 2 * P, 4 * P, RW).unwrap();
        assert_eq!(
            map.iter().map(|v| (v.start, v.end, v.prot)).collect::<Vec<_>>(),
            vec![(base, base + 2 * P, Prot::READ), (base + 2 * P, base + 8 * P, RW)]
        );

        // Unmapped hole is refused atomically without changing existing VMAs.
        map.unmap(base + 4 * P, P).unwrap();
        let before = map.clone();
        assert_eq!(map.protect(base + 2 * P, 4 * P, RX), Err(VmError::NoMem));
        assert_eq!(map, before);
        // Trailing hole refused atomically too.
        assert_eq!(map.protect(base + 6 * P, 3 * P, RX), Err(VmError::NoMem));
        assert_eq!(map, before);
        // Leading hole refused atomically too.
        assert_eq!(map.protect(base - P, 2 * P, RX), Err(VmError::NoMem));
        assert_eq!(map, before);
        assert_invariants(&map);
    }

    #[test]
    fn brk_byte_granular_grow_shrink_and_collision() {
        let mut map = VmaMap::new();
        let init = 0x80_0000;
        assert_eq!(Brk::new(init + 1), Err(VmError::Misaligned));
        assert_eq!(Brk::new(0), Err(VmError::InvalidRange));
        assert_eq!(Brk::with_limit(init, init - P), Err(VmError::InvalidRange));

        let mut brk = Brk::new(init).unwrap();
        assert_eq!((brk.initial(), brk.current(), brk.mapped_end()), (init, init, init));

        // brk(0) queries current break without mutating.
        let q = brk.set(&mut map, 0);
        assert_eq!((q.current, q.succeeded), (init, false));
        assert!(map.is_empty());

        // Grow by 1 byte -> maps 1 page, returns exact byte address.
        let g1 = brk.set(&mut map, init + 1);
        assert_eq!((g1.current, g1.succeeded), (init + 1, true));
        assert!(g1.unmapped.is_empty());
        assert_eq!(brk.mapped_end(), init + P);
        assert_eq!(map.find(init).unwrap(), &Vma { start: init, end: init + P, prot: RW, backing: Backing::Anonymous });

        // Grow within same page -> no new VMA change.
        let g2 = brk.set(&mut map, init + 1234);
        assert_eq!((g2.current, g2.succeeded), (init + 1234, true));
        assert_eq!(brk.mapped_end(), init + P);

        // Grow across page boundary -> coalesces into single 3-page VMA.
        let g3 = brk.set(&mut map, init + 2 * P + 7);
        assert_eq!((g3.current, g3.succeeded), (init + 2 * P + 7, true));
        assert_eq!(brk.mapped_end(), init + 3 * P);
        assert_eq!(map.len(), 1);
        assert_eq!(map.find(init).unwrap().end, init + 3 * P);

        // Place a blocker VMA 1 page above current mapped_end.
        map.map_fixed_noreplace(init + 4 * P, P, Prot::READ).unwrap();
        // Growing into the blocker fails and leaves brk unchanged.
        let blocked = brk.set(&mut map, init + 4 * P + 1);
        assert_eq!((blocked.current, blocked.succeeded), (init + 2 * P + 7, false));
        assert_eq!(brk.mapped_end(), init + 3 * P);
        // Growing right up to the blocker succeeds.
        let touch = brk.set(&mut map, init + 4 * P);
        assert_eq!((touch.current, touch.succeeded), (init + 4 * P, true));
        assert_eq!(brk.mapped_end(), init + 4 * P);

        // Shrink within the last page unmaps nothing.
        let s1 = brk.set(&mut map, init + 3 * P + 5);
        assert_eq!((s1.current, s1.succeeded), (init + 3 * P + 5, true));
        assert!(s1.unmapped.is_empty());

        // Shrink across two pages returns exact unmapped sub-range.
        let s2 = brk.set(&mut map, init + P + 10);
        assert_eq!((s2.current, s2.succeeded), (init + P + 10, true));
        assert_eq!(
            s2.unmapped,
            vec![Vma { start: init + 2 * P, end: init + 4 * P, prot: RW, backing: Backing::Anonymous }]
        );
        assert_eq!(brk.mapped_end(), init + 2 * P);
        assert!(map.find(init + 4 * P).is_some(), "blocker VMA untouched");

        // Below initial is refused.
        let below = brk.set(&mut map, init - 1);
        assert_eq!((below.current, below.succeeded), (init + P + 10, false));

        // Overflowing align_up_page is refused.
        let ovf = brk.set(&mut map, u64::MAX);
        assert_eq!((ovf.current, ovf.succeeded), (init + P + 10, false));

        // Shrink all the way back to initial unmaps the rest of brk.
        let s3 = brk.set(&mut map, init);
        assert_eq!((s3.current, s3.succeeded), (init, true));
        assert_eq!(s3.unmapped, vec![Vma { start: init, end: init + 2 * P, prot: RW, backing: Backing::Anonymous }]);
        assert_eq!(brk.mapped_end(), init);
        assert_invariants(&map);
    }

    /// Deterministic xorshift64 PRNG for model checking.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn random_operations_match_page_array_model() {
        // Test inside a 64-page window `[BASE, BASE + N * P)` where BASE sits
        // well inside USER_MIN..MMAP_TOP, plus top-down hints constrained by a
        // sentinel VMA so every mapping stays inside the shadow array.
        const N: usize = 64;
        const BASE: u64 = 0x100_0000;
        const PROTS: [Prot; 6] = [Prot::NONE, Prot::READ, RW, RX, RWX, Prot::WRITE];

        // Track every outcome branch so no test path is vacuous:
        // [fixed_ok, noreplace_ok, noreplace_exists, hint_ok, unmap_ok, protect_ok, protect_nomem, brk_grow, brk_shrink, brk_fail]
        let mut outcomes = [0usize; 10];

        for seed in 1..=20u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let mut map = VmaMap::new();
            let mut model: [Option<Prot>; N] = [None; N];
            // Brk lives in the first 16 pages of the window.
            let mut brk = Brk::with_limit(BASE, BASE + 16 * P).unwrap();

            let check_model = |map: &VmaMap, model: &[Option<Prot>; N]| {
                assert_invariants(map);
                for (i, &want) in model.iter().enumerate() {
                    let addr = BASE + i as u64 * P;
                    let got = map.find(addr).map(|v| v.prot);
                    assert_eq!(got, want, "seed {seed} page {i} ({addr:#x})");
                    assert_eq!(map.find(addr + P - 1).map(|v| v.prot), want);
                }
            };

            let collect_removed = |model: &mut [Option<Prot>; N], lo: usize, hi: usize| -> Vec<Vma> {
                let mut out: Vec<Vma> = Vec::new();
                for (i, slot) in model[lo..hi].iter_mut().enumerate() {
                    if let Some(prot) = slot.take() {
                        let s = BASE + (lo + i) as u64 * P;
                        let e = s + P;
                        if let Some(last) = out.last_mut()
                            && last.end == s
                            && last.prot == prot
                        {
                            last.end = e;
                        } else {
                            out.push(Vma { start: s, end: e, prot, backing: Backing::Anonymous });
                        }
                    }
                }
                out
            };

            for _ in 0..800 {
                let op = rng.below(10);
                let lo = rng.below(N as u64) as usize;
                let max_pages = (N - lo).min(12) as u64;
                let pages = 1 + rng.below(max_pages);
                let start = BASE + lo as u64 * P;
                let len = pages * P;
                let prot = PROTS[rng.below(PROTS.len() as u64) as usize];

                match op {
                    // MAP_FIXED
                    0 | 1 => {
                        let unmapped = map.map_fixed(start, len, prot).unwrap();
                        let want_unmapped = collect_removed(&mut model, lo, lo + pages as usize);
                        assert_eq!(unmapped, want_unmapped, "seed {seed} MAP_FIXED unmapped mismatch");
                        for slot in &mut model[lo..lo + pages as usize] {
                            *slot = Some(prot);
                        }
                        outcomes[0] += 1;
                    }
                    // MAP_FIXED_NOREPLACE
                    2 | 3 => {
                        let has_overlap = model[lo..lo + pages as usize].iter().any(Option::is_some);
                        let res = map.map_fixed_noreplace(start, len, prot);
                        if has_overlap {
                            assert_eq!(res, Err(VmError::Exists));
                            outcomes[2] += 1;
                        } else {
                            assert_eq!(res, Ok(start));
                            for slot in &mut model[lo..lo + pages as usize] {
                                *slot = Some(prot);
                            }
                            outcomes[1] += 1;
                        }
                    }
                    // Advisory Hint inside the window (only when free, so it stays in window)
                    4 => {
                        let free = model[lo..lo + pages as usize].iter().all(Option::is_none);
                        if free {
                            let m = map.map_anon(MapPlace::Hint(Some(start)), len, prot).unwrap();
                            assert_eq!((m.start, m.end), (start, start + len));
                            assert!(m.unmapped.is_empty());
                            for slot in &mut model[lo..lo + pages as usize] {
                                *slot = Some(prot);
                            }
                            outcomes[3] += 1;
                        }
                    }
                    // unmap
                    5 | 6 => {
                        let unmapped = map.unmap(start, len).unwrap();
                        let want_unmapped = collect_removed(&mut model, lo, lo + pages as usize);
                        assert_eq!(unmapped, want_unmapped, "seed {seed} unmap mismatch");
                        outcomes[4] += 1;
                    }
                    // protect
                    7 | 8 => {
                        let all_mapped = model[lo..lo + pages as usize].iter().all(Option::is_some);
                        let res = map.protect(start, len, prot);
                        if all_mapped {
                            assert_eq!(res, Ok(()));
                            for slot in &mut model[lo..lo + pages as usize] {
                                *slot = Some(prot);
                            }
                            outcomes[5] += 1;
                        } else {
                            assert_eq!(res, Err(VmError::NoMem));
                            outcomes[6] += 1;
                        }
                    }
                    // brk (byte-granular inside first 16 pages, occasionally beyond limit)
                    _ => {
                        let old_cur = brk.current();
                        let old_mapped_end = brk.mapped_end();
                        let target = BASE + rng.below(18 * P);
                        let change = brk.set(&mut map, target);
                        let new_page_end = align_up_page(target).unwrap();
                        if new_page_end > brk.limit() {
                            assert!(!change.succeeded && change.current == old_cur);
                            outcomes[9] += 1;
                        } else if new_page_end > old_mapped_end {
                            let m_lo = ((old_mapped_end - BASE) / P) as usize;
                            let m_hi = ((new_page_end - BASE) / P) as usize;
                            let blocked = model[m_lo..m_hi].iter().any(Option::is_some);
                            if blocked {
                                assert!(!change.succeeded && change.current == old_cur);
                                outcomes[9] += 1;
                            } else {
                                assert!(change.succeeded && change.current == target && change.unmapped.is_empty());
                                for slot in &mut model[m_lo..m_hi] {
                                    *slot = Some(RW);
                                }
                                outcomes[7] += 1;
                            }
                        } else if new_page_end < old_mapped_end {
                            let m_lo = ((new_page_end - BASE) / P) as usize;
                            let m_hi = ((old_mapped_end - BASE) / P) as usize;
                            let want_unmapped = collect_removed(&mut model, m_lo, m_hi);
                            assert!(change.succeeded && change.current == target);
                            assert_eq!(change.unmapped, want_unmapped);
                            outcomes[8] += 1;
                        } else {
                            assert!(change.succeeded && change.current == target && change.unmapped.is_empty());
                        }
                    }
                }
                check_model(&map, &model);
            }
        }
        assert!(outcomes.iter().all(|&n| n > 0), "outcomes {outcomes:?}");
    }
}
