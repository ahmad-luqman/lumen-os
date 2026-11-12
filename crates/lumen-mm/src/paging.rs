//! x86-64 4-level page tables: the entry format, walks, and a mapper.
//!
//! Nothing here dereferences a physical address. Every table access goes
//! through [`TableMemory`] (reads) and [`TableMemoryMut`] (writes and new
//! tables), which the kernel implements over the higher-half direct map and
//! the host tests implement with a map of fake frames. That is what lets the
//! whole mapper be tested on the host, including a randomized comparison
//! against a model.
//!
//! # Permissions
//!
//! The CPU combines the rights of every level on the way down: a page is
//! writable only if every entry on its path has `WRITABLE`, and executable
//! only if none has `NO_EXECUTE`. The mapper makes every table entry
//! permissive (present and writable, executable) and enforces permissions at
//! the leaf only; walks report the *combined* rights, which is what the CPU
//! enforces.
//!
//! # Caching
//!
//! A leaf's memory type is the PAT slot selected by its PWT, PCD and PAT bits
//! (the PAT bit is bit 7 in a 4 KiB entry but bit 12 in a 2 MiB or 1 GiB one,
//! where bit 7 means "huge"). [`Cache`] names the slots this kernel uses, in
//! the layout Limine sets up (slot 0 write-back, 3 uncached, 5
//! write-combining); the kernel checks the PAT MSR matches before relying on it.
//!
//! # Scope
//!
//! Kernel mappings only: leaves are never user-accessible (M2 adds that).
//! Tables are never freed: unmapping the last page under a table leaves the
//! empty table in place (it is reused by later maps). A 2 MiB or 1 GiB map
//! where a table already exists fails with [`MapError::AlreadyMapped`] even
//! if that table is empty.

use core::fmt;

use crate::{FRAME_SIZE, PhysAddr, PhysFrame, VirtAddr};

/// Entries per table.
pub const ENTRIES: usize = 512;

/// Entry bits.
pub mod bits {
    pub const PRESENT: u64 = 1 << 0;
    pub const WRITABLE: u64 = 1 << 1;
    pub const USER: u64 = 1 << 2;
    /// Page-level write-through (PAT index bit 0).
    pub const PWT: u64 = 1 << 3;
    /// Page-level cache disable (PAT index bit 1).
    pub const PCD: u64 = 1 << 4;
    pub const ACCESSED: u64 = 1 << 5;
    pub const DIRTY: u64 = 1 << 6;
    /// In an L3 or L2 entry: the entry maps a 1 GiB or 2 MiB page.
    pub const HUGE: u64 = 1 << 7;
    /// PAT index bit 2, in a 4 KiB (L1) entry.
    pub const PAT_4K: u64 = 1 << 7;
    pub const GLOBAL: u64 = 1 << 8;
    /// PAT index bit 2, in a 2 MiB or 1 GiB entry.
    pub const PAT_HUGE: u64 = 1 << 12;
    pub const NO_EXECUTE: u64 = 1 << 63;
}
use bits::*;

/// Address bits of an entry pointing to a table or a 4 KiB page.
const ADDR_4K: u64 = 0x000f_ffff_ffff_f000;
const ADDR_2M: u64 = 0x000f_ffff_ffe0_0000;
const ADDR_1G: u64 = 0x000f_ffff_c000_0000;

/// A level of the hierarchy. `L4` is the root (the table in CR3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    L1 = 1,
    L2 = 2,
    L3 = 3,
    L4 = 4,
}

impl Level {
    pub const fn number(self) -> u8 {
        self as u8
    }

    const fn shift(self) -> u32 {
        12 + 9 * (self as u32 - 1)
    }

    /// The index of `virt`'s entry in a table at this level.
    pub const fn index_of(self, virt: VirtAddr) -> usize {
        ((virt.as_u64() >> self.shift()) & (ENTRIES as u64 - 1)) as usize
    }

    /// The level below, or `None` for `L1`.
    pub const fn below(self) -> Option<Level> {
        match self {
            Level::L4 => Some(Level::L3),
            Level::L3 => Some(Level::L2),
            Level::L2 => Some(Level::L1),
            Level::L1 => None,
        }
    }

    /// The page size a leaf at this level maps, if leaves are allowed here.
    pub const fn leaf_size(self) -> Option<PageSize> {
        match self {
            Level::L1 => Some(PageSize::Size4K),
            Level::L2 => Some(PageSize::Size2M),
            Level::L3 => Some(PageSize::Size1G),
            Level::L4 => None,
        }
    }
}

/// The page sizes x86-64 supports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageSize {
    Size4K,
    Size2M,
    Size1G,
}

impl PageSize {
    pub const fn bytes(self) -> u64 {
        match self {
            PageSize::Size4K => FRAME_SIZE,
            PageSize::Size2M => 1 << 21,
            PageSize::Size1G => 1 << 30,
        }
    }

    /// The level whose entries map pages of this size.
    pub const fn level(self) -> Level {
        match self {
            PageSize::Size4K => Level::L1,
            PageSize::Size2M => Level::L2,
            PageSize::Size1G => Level::L3,
        }
    }

    const fn addr_mask(self) -> u64 {
        match self {
            PageSize::Size4K => ADDR_4K,
            PageSize::Size2M => ADDR_2M,
            PageSize::Size1G => ADDR_1G,
        }
    }

    const fn pat_bit(self) -> u64 {
        match self {
            PageSize::Size4K => PAT_4K,
            _ => PAT_HUGE,
        }
    }
}

impl fmt::Display for PageSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PageSize::Size4K => "4 KiB",
            PageSize::Size2M => "2 MiB",
            PageSize::Size1G => "1 GiB",
        })
    }
}

/// A leaf's memory type: which PAT slot it selects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cache {
    /// Normal RAM (slot 0).
    WriteBack,
    /// Framebuffers (slot 5).
    WriteCombining,
    /// Device registers (slot 3).
    Uncached,
}

impl Cache {
    /// The PAT slot (0-7) this memory type uses.
    pub const fn pat_slot(self) -> u8 {
        match self {
            Cache::WriteBack => 0,
            Cache::Uncached => 3,
            Cache::WriteCombining => 5,
        }
    }

    /// The PWT/PCD/PAT bits selecting this slot in a leaf of `size`.
    const fn bits(self, size: PageSize) -> u64 {
        let slot = self.pat_slot() as u64;
        (if slot & 1 != 0 { PWT } else { 0 })
            | (if slot & 2 != 0 { PCD } else { 0 })
            | (if slot & 4 != 0 { size.pat_bit() } else { 0 })
    }
}

/// What a leaf mapping allows. Leaves are always supervisor-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attrs {
    pub writable: bool,
    pub executable: bool,
    /// Kept in the TLB across CR3 switches (for kernel mappings).
    pub global: bool,
    pub cache: Cache,
}

impl Attrs {
    /// Kernel code: read and execute.
    pub const CODE: Attrs = Attrs { writable: false, executable: true, global: false, cache: Cache::WriteBack };
    /// Read-only data.
    pub const RODATA: Attrs = Attrs { writable: false, executable: false, global: false, cache: Cache::WriteBack };
    /// Read-write data.
    pub const DATA: Attrs = Attrs { writable: true, executable: false, global: false, cache: Cache::WriteBack };
    /// Device registers: read-write, uncached.
    pub const MMIO: Attrs = Attrs { writable: true, executable: false, global: false, cache: Cache::Uncached };
    /// A framebuffer: read-write, write-combining.
    pub const FRAMEBUFFER: Attrs =
        Attrs { writable: true, executable: false, global: false, cache: Cache::WriteCombining };

    /// The leaf entry mapping a page of `size` at `phys` with these rights.
    fn leaf(self, phys: PhysAddr, size: PageSize) -> u64 {
        let mut e = PRESENT | phys.as_u64() | self.cache.bits(size);
        if size != PageSize::Size4K {
            e |= HUGE;
        }
        if self.writable {
            e |= WRITABLE;
        }
        if !self.executable {
            e |= NO_EXECUTE;
        }
        if self.global {
            e |= GLOBAL;
        }
        e
    }
}

/// An entry pointing to a lower-level table: permissive, so that the leaf
/// alone decides the rights.
fn table_entry(table: PhysFrame) -> u64 {
    PRESENT | WRITABLE | table.as_u64()
}

/// A raw page-table entry.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Entry(u64);

impl Entry {
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn is_present(self) -> bool {
        self.0 & PRESENT != 0
    }

    /// Does this present entry, at `level`, map a page (rather than point to a
    /// table)? The huge bit means "page" only in L3 and L2 entries; in L1 it
    /// is the PAT bit and every L1 entry is a page.
    pub const fn is_leaf(self, level: Level) -> bool {
        match level {
            Level::L1 => true,
            Level::L2 | Level::L3 => self.0 & HUGE != 0,
            Level::L4 => false,
        }
    }

    /// The table this non-leaf entry points to.
    pub const fn table(self) -> PhysFrame {
        match PhysFrame::from_u64(self.0 & ADDR_4K) {
            Some(f) => f,
            None => unreachable!(),
        }
    }

    /// The start of the page this leaf entry (of `size`) maps.
    pub const fn page(self, size: PageSize) -> PhysAddr {
        match PhysAddr::new(self.0 & size.addr_mask()) {
            Some(a) => a,
            None => unreachable!(),
        }
    }

    pub const fn has(self, bit: u64) -> bool {
        self.0 & bit != 0
    }

    /// The PAT slot this leaf (of `size`) selects.
    pub const fn pat_slot(self, size: PageSize) -> u8 {
        (self.has(PWT) as u8) | (self.has(PCD) as u8) << 1 | (self.has(size.pat_bit()) as u8) << 2
    }
}

impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Entry({:#018x})", self.0)
    }
}

/// Read access to page-table frames.
pub trait TableMemory {
    /// Entry `index` of the table in frame `table`, or `None` if that frame
    /// can't be read (walks then stop instead of faulting).
    fn read(&self, table: PhysFrame, index: usize) -> Option<u64>;
}

/// Write access to page-table frames, and new tables.
pub trait TableMemoryMut: TableMemory {
    /// Set entry `index` of the table in frame `table` (a frame [`read`]
    /// just succeeded on, or one from [`alloc_table`]).
    ///
    /// [`read`]: TableMemory::read
    /// [`alloc_table`]: TableMemoryMut::alloc_table
    fn write(&mut self, table: PhysFrame, index: usize, value: u64);

    /// A zeroed frame for a new table, or `None` when memory is exhausted.
    fn alloc_table(&mut self) -> Option<PhysFrame>;
}

/// One entry read on the way down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Step {
    pub level: Level,
    pub index: usize,
    pub entry: Entry,
}

/// A present page, with the rights the CPU enforces for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mapping {
    /// Start of the virtual page.
    pub virt: VirtAddr,
    /// Start of the physical page.
    pub phys: PhysAddr,
    pub size: PageSize,
    /// Combined over every level.
    pub writable: bool,
    /// Combined over every level.
    pub executable: bool,
    /// Combined over every level.
    pub user: bool,
    pub global: bool,
    /// The PAT slot of the leaf (compare with [`Cache::pat_slot`]).
    pub pat_slot: u8,
}

/// How a walk ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkEnd {
    /// A present page maps the address.
    Mapped(Mapping),
    /// The entry at this level is not present.
    NotPresent(Level),
    /// The table at this level (in this frame) could not be read.
    Unreadable(Level, PhysFrame),
}

/// The entries read while translating one address, and how it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Walk {
    steps: [Step; 4],
    len: usize,
    pub end: WalkEnd,
}

impl Walk {
    pub fn steps(&self) -> &[Step] {
        &self.steps[..self.len]
    }

    /// The page mapping the address, if one does.
    pub fn mapping(&self) -> Option<Mapping> {
        match self.end {
            WalkEnd::Mapped(m) => Some(m),
            _ => None,
        }
    }
}

/// Follow `virt` down from the root table `root`.
pub fn walk(mem: &impl TableMemory, root: PhysFrame, virt: VirtAddr) -> Walk {
    let empty = Step { level: Level::L4, index: 0, entry: Entry(0) };
    let mut w = Walk { steps: [empty; 4], len: 0, end: WalkEnd::NotPresent(Level::L4) };
    let (mut table, mut level) = (root, Level::L4);
    let (mut writable, mut executable, mut user) = (true, true, true);
    loop {
        let index = level.index_of(virt);
        let Some(raw) = mem.read(table, index) else {
            w.end = WalkEnd::Unreadable(level, table);
            return w;
        };
        let entry = Entry(raw);
        w.steps[w.len] = Step { level, index, entry };
        w.len += 1;
        if !entry.is_present() {
            w.end = WalkEnd::NotPresent(level);
            return w;
        }
        writable &= entry.has(WRITABLE);
        executable &= !entry.has(NO_EXECUTE);
        user &= entry.has(USER);
        if let (true, Some(size)) = (entry.is_leaf(level), level.leaf_size()) {
            w.end = WalkEnd::Mapped(Mapping {
                virt: virt.align_down(size.bytes()),
                phys: entry.page(size),
                size,
                writable,
                executable,
                user,
                global: entry.has(GLOBAL),
                pat_slot: entry.pat_slot(size),
            });
            return w;
        }
        table = entry.table();
        // `is_leaf` is true for every L1 entry, so L1 never gets here.
        level = level.below().expect("an L1 entry is always a leaf");
    }
}

/// The physical address `virt` maps to, if it is mapped.
pub fn translate(mem: &impl TableMemory, root: PhysFrame, virt: VirtAddr) -> Option<PhysAddr> {
    let m = walk(mem, root, virt).mapping()?;
    m.phys.checked_add(virt.as_u64() - m.virt.as_u64())
}

/// Why [`map`] failed. Nothing is mapped when it fails (though tables it
/// created on the way down stay, empty).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// The virtual or physical address is not aligned to the page size.
    Misaligned,
    /// Something is already mapped there: a page of this size, a larger page
    /// covering it, or (for a 2 MiB or 1 GiB page) a table of smaller pages.
    AlreadyMapped,
    /// No frame for a new table.
    OutOfMemory,
    /// A table on the way down could not be read.
    Unreadable(PhysFrame),
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MapError::Misaligned => f.write_str("address not aligned to the page size"),
            MapError::AlreadyMapped => f.write_str("already mapped"),
            MapError::OutOfMemory => f.write_str("out of memory for page tables"),
            MapError::Unreadable(t) => write!(f, "page table {t} unreadable"),
        }
    }
}

/// Map the page of `size` at `virt` to `phys` with `attrs`. The caller
/// flushes nothing: the entry was not present, so no TLB holds it.
pub fn map(
    mem: &mut impl TableMemoryMut,
    root: PhysFrame,
    virt: VirtAddr,
    phys: PhysAddr,
    size: PageSize,
    attrs: Attrs,
) -> Result<(), MapError> {
    if !virt.is_aligned(size.bytes()) || !phys.is_aligned(size.bytes()) {
        return Err(MapError::Misaligned);
    }
    let (mut table, mut level) = (root, Level::L4);
    while level != size.level() {
        let index = level.index_of(virt);
        let entry = Entry(mem.read(table, index).ok_or(MapError::Unreadable(table))?);
        table = if !entry.is_present() {
            let new = mem.alloc_table().ok_or(MapError::OutOfMemory)?;
            mem.write(table, index, table_entry(new));
            new
        } else if entry.is_leaf(level) {
            return Err(MapError::AlreadyMapped);
        } else {
            entry.table()
        };
        level = level.below().expect("loop stops at the leaf level, above L1 at worst");
    }
    let index = level.index_of(virt);
    if Entry(mem.read(table, index).ok_or(MapError::Unreadable(table))?).is_present() {
        return Err(MapError::AlreadyMapped);
    }
    mem.write(table, index, attrs.leaf(phys, size));
    Ok(())
}

/// Why [`unmap`] failed. Nothing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnmapError {
    NotMapped,
    /// `virt` is inside a page but not its first byte.
    NotPageStart,
    Unreadable(PhysFrame),
}

impl fmt::Display for UnmapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnmapError::NotMapped => f.write_str("not mapped"),
            UnmapError::NotPageStart => f.write_str("not the start of a page"),
            UnmapError::Unreadable(t) => write!(f, "page table {t} unreadable"),
        }
    }
}

/// Unmap the page starting at `virt` and return what it mapped. The caller
/// must then invalidate the TLB entry for `virt`.
pub fn unmap(mem: &mut impl TableMemoryMut, root: PhysFrame, virt: VirtAddr) -> Result<Mapping, UnmapError> {
    let w = walk(mem, root, virt);
    let m = match w.end {
        WalkEnd::Mapped(m) => m,
        WalkEnd::NotPresent(_) => return Err(UnmapError::NotMapped),
        WalkEnd::Unreadable(_, t) => return Err(UnmapError::Unreadable(t)),
    };
    if m.virt != virt {
        return Err(UnmapError::NotPageStart);
    }
    // The leaf is the last step; the table holding it is the one the step
    // above points to (or the root, which can't hold a leaf).
    let steps = w.steps();
    let leaf = steps[steps.len() - 1];
    let table = steps[steps.len() - 2].entry.table();
    mem.write(table, leaf.index, 0);
    Ok(m)
}

/// The canonical virtual address with these table indices (L4 first) and
/// offset 0.
fn virt_of(indices: &[usize]) -> VirtAddr {
    let raw = indices
        .iter()
        .zip([Level::L4, Level::L3, Level::L2, Level::L1])
        .fold(0u64, |acc, (&i, level)| acc | ((i as u64) << level.shift()));
    // Sign-extend bit 47.
    let raw = ((raw << 16) as i64 >> 16) as u64;
    VirtAddr::new(raw).expect("sign-extended addresses are canonical")
}

/// Call `f` with every present page under `root`, in address order. Returns
/// the first table that could not be read, if any (the pages under it are
/// skipped).
pub fn for_each_mapping(mem: &impl TableMemory, root: PhysFrame, mut f: impl FnMut(Mapping)) -> Result<(), PhysFrame> {
    struct Ctx<'a, M> {
        mem: &'a M,
        root: PhysFrame,
        f: &'a mut dyn FnMut(Mapping),
        unreadable: Option<PhysFrame>,
    }
    fn go<M: TableMemory>(cx: &mut Ctx<'_, M>, level: Level, table: PhysFrame, path: [usize; 4], depth: usize) {
        for index in 0..ENTRIES {
            let Some(raw) = cx.mem.read(table, index) else {
                cx.unreadable.get_or_insert(table);
                return;
            };
            let entry = Entry(raw);
            if !entry.is_present() {
                continue;
            }
            let mut path = path;
            path[depth] = index;
            if entry.is_leaf(level) {
                // A walk gives the combined rights; re-walking from the root
                // costs four reads per page, which is fine for a sweep.
                if let Some(m) = walk(cx.mem, cx.root, virt_of(&path[..=depth])).mapping() {
                    (cx.f)(m);
                }
            } else if let Some(below) = level.below() {
                go(cx, below, entry.table(), path, depth + 1);
            }
        }
    }
    let mut cx = Ctx { mem, root, f: &mut f, unreadable: None };
    go(&mut cx, Level::L4, root, [0; 4], 0);
    match cx.unreadable {
        Some(t) => Err(t),
        None => Ok(()),
    }
}

/// Call `f` with every table frame under `root` (including `root`), with its
/// level. Returns how many it visited.
pub fn for_each_table(mem: &impl TableMemory, root: PhysFrame, mut f: impl FnMut(Level, PhysFrame)) -> usize {
    fn go(mem: &impl TableMemory, level: Level, table: PhysFrame, f: &mut dyn FnMut(Level, PhysFrame)) -> usize {
        f(level, table);
        let mut n = 1;
        let Some(below) = level.below() else { return n };
        for index in 0..ENTRIES {
            let Some(raw) = mem.read(table, index) else { return n };
            let entry = Entry(raw);
            if entry.is_present() && !entry.is_leaf(level) {
                n += go(mem, below, entry.table(), f);
            }
        }
        n
    }
    go(mem, Level::L4, root, &mut f)
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::vec::Vec;

    /// Page-table frames in a map, with an optional allocation limit and
    /// frames that "can't be read".
    struct FakeMem {
        tables: BTreeMap<u64, [u64; ENTRIES]>,
        next: u64,
        allocs_left: usize,
        unreadable: BTreeSet<u64>,
    }

    impl FakeMem {
        /// A memory with an empty root table; returns it and the root.
        fn new() -> (Self, PhysFrame) {
            let mut m = FakeMem {
                tables: BTreeMap::new(),
                next: 0x10_0000,
                allocs_left: usize::MAX,
                unreadable: BTreeSet::new(),
            };
            let root = m.alloc_table().unwrap();
            (m, root)
        }

        fn entry(&self, table: PhysFrame, index: usize) -> u64 {
            self.tables[&table.as_u64()][index]
        }

        fn set(&mut self, table: PhysFrame, index: usize, value: u64) {
            self.tables.get_mut(&table.as_u64()).unwrap()[index] = value;
        }
    }

    impl TableMemory for FakeMem {
        fn read(&self, table: PhysFrame, index: usize) -> Option<u64> {
            if self.unreadable.contains(&table.as_u64()) {
                return None;
            }
            Some(self.tables.get(&table.as_u64()).expect("read of a frame that is not a table")[index])
        }
    }

    impl TableMemoryMut for FakeMem {
        fn write(&mut self, table: PhysFrame, index: usize, value: u64) {
            assert!(!self.unreadable.contains(&table.as_u64()), "write to an unreadable table");
            self.tables.get_mut(&table.as_u64()).expect("write to a frame that is not a table")[index] = value;
        }

        fn alloc_table(&mut self) -> Option<PhysFrame> {
            self.allocs_left = self.allocs_left.checked_sub(1)?;
            let f = PhysFrame::from_u64(self.next).unwrap();
            self.next += FRAME_SIZE;
            self.tables.insert(f.as_u64(), [0; ENTRIES]);
            Some(f)
        }
    }

    fn v(a: u64) -> VirtAddr {
        VirtAddr::new(a).unwrap()
    }

    fn p(a: u64) -> PhysAddr {
        PhysAddr::new(a).unwrap()
    }

    const KERNEL: u64 = 0xffff_ffff_8000_0000;
    const HHDM: u64 = 0xffff_8000_0000_0000;
    const K: u64 = 4096;
    const M2: u64 = 1 << 21;
    const G1: u64 = 1 << 30;

    #[test]
    fn indices_split_the_address() {
        let a = v(KERNEL + 5 * K + 0x123);
        assert_eq!([Level::L4, Level::L3, Level::L2, Level::L1].map(|l| l.index_of(a)), [511, 510, 0, 5]);
        assert_eq!(Level::L4.index_of(v(HHDM)), 256);
        assert_eq!(virt_of(&[511, 510, 0, 5]), v(KERNEL + 5 * K));
        assert_eq!(virt_of(&[256]), v(HHDM));
        assert_eq!(virt_of(&[1, 2]), v((1 << 39) | (2 << 30)));
    }

    #[test]
    fn a_4k_page_maps_and_translates() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(KERNEL + 5 * K), p(0x20_5000), PageSize::Size4K, Attrs::CODE).unwrap();
        assert_eq!(translate(&m, root, v(KERNEL + 5 * K + 0x123)), Some(p(0x20_5123)));
        assert_eq!(translate(&m, root, v(KERNEL + 6 * K)), None);
        let w = walk(&m, root, v(KERNEL + 5 * K + 0x123));
        assert_eq!(
            w.steps().iter().map(|s| (s.level, s.index)).collect::<Vec<_>>(),
            [(Level::L4, 511), (Level::L3, 510), (Level::L2, 0), (Level::L1, 5)]
        );
        let mapping = w.mapping().unwrap();
        assert_eq!((mapping.virt, mapping.phys, mapping.size), (v(KERNEL + 5 * K), p(0x20_5000), PageSize::Size4K));
        assert!(!mapping.writable && mapping.executable && !mapping.user);
        assert_eq!(mapping.pat_slot, 0);
        // The leaf alone carries the rights; tables are permissive.
        let leaf = w.steps()[3].entry;
        assert_eq!(leaf.raw(), PRESENT | 0x20_5000);
        for s in &w.steps()[..3] {
            assert_eq!(s.entry.raw() & !ADDR_4K, PRESENT | WRITABLE, "{s:?}");
        }
    }

    #[test]
    fn data_is_writable_and_not_executable() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(KERNEL), p(0x30_0000), PageSize::Size4K, Attrs::DATA).unwrap();
        map(&mut m, root, v(KERNEL + K), p(0x30_1000), PageSize::Size4K, Attrs::RODATA).unwrap();
        let data = walk(&m, root, v(KERNEL)).mapping().unwrap();
        assert!(data.writable && !data.executable);
        let ro = walk(&m, root, v(KERNEL + K)).mapping().unwrap();
        assert!(!ro.writable && !ro.executable);
        assert_eq!(walk(&m, root, v(KERNEL)).steps()[3].entry.raw(), PRESENT | WRITABLE | NO_EXECUTE | 0x30_0000);
    }

    #[test]
    fn huge_pages_translate_with_their_offset() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(HHDM + 2 * M2), p(4 * M2), PageSize::Size2M, Attrs::DATA).unwrap();
        map(&mut m, root, v(HHDM + 3 * G1), p(G1), PageSize::Size1G, Attrs::DATA).unwrap();
        assert_eq!(translate(&m, root, v(HHDM + 2 * M2 + 0x1_2345)), Some(p(4 * M2 + 0x1_2345)));
        assert_eq!(translate(&m, root, v(HHDM + 3 * G1 + 0x123_4567)), Some(p(G1 + 0x123_4567)));
        let w2 = walk(&m, root, v(HHDM + 2 * M2 + 5));
        assert_eq!(w2.steps().len(), 3);
        assert_eq!(w2.mapping().unwrap().size, PageSize::Size2M);
        let w1 = walk(&m, root, v(HHDM + 3 * G1));
        assert_eq!(w1.steps().len(), 2);
        assert_eq!(w1.mapping().unwrap().size, PageSize::Size1G);
    }

    #[test]
    fn the_pat_bit_moves_with_the_page_size() {
        // Write-combining is slot 5: PWT plus the PAT bit, which is bit 7 in
        // a 4 KiB entry and bit 12 in a huge one (where bit 7 means "huge").
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(HHDM), p(0x8000_0000), PageSize::Size4K, Attrs::FRAMEBUFFER).unwrap();
        map(&mut m, root, v(HHDM + M2), p(0x8020_0000), PageSize::Size2M, Attrs::FRAMEBUFFER).unwrap();
        map(&mut m, root, v(HHDM + 2 * M2), p(0x8040_0000), PageSize::Size2M, Attrs::MMIO).unwrap();
        let small = walk(&m, root, v(HHDM)).steps()[3].entry;
        assert_eq!(small.raw() & (PWT | PCD | PAT_4K | PAT_HUGE), PWT | PAT_4K);
        let huge = walk(&m, root, v(HHDM + M2)).steps()[2].entry;
        assert_eq!(huge.raw() & (PWT | PCD | HUGE | PAT_HUGE), PWT | HUGE | PAT_HUGE);
        for a in [HHDM, HHDM + M2] {
            assert_eq!(walk(&m, root, v(a)).mapping().unwrap().pat_slot, Cache::WriteCombining.pat_slot());
        }
        assert_eq!(walk(&m, root, v(HHDM + 2 * M2)).mapping().unwrap().pat_slot, Cache::Uncached.pat_slot());
        // The PAT bit (12) of a huge entry is not part of its address.
        assert_eq!(translate(&m, root, v(HHDM + M2 + 7)), Some(p(0x8020_0007)));
    }

    #[test]
    fn rights_combine_over_every_level() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(KERNEL), p(0x40_0000), PageSize::Size4K, Attrs { executable: true, ..Attrs::DATA })
            .unwrap();
        let l3 = Entry(m.entry(root, 511)).table();
        let l2 = Entry(m.entry(l3, 510)).table();
        let before = walk(&m, root, v(KERNEL)).mapping().unwrap();
        assert!(before.writable && before.executable);
        let e = m.entry(l2, 0);
        m.set(l2, 0, e & !WRITABLE);
        assert!(!walk(&m, root, v(KERNEL)).mapping().unwrap().writable);
        m.set(l2, 0, e | NO_EXECUTE);
        let after = walk(&m, root, v(KERNEL)).mapping().unwrap();
        assert!(after.writable && !after.executable);
        m.set(root, 511, m.entry(root, 511) | USER);
        assert!(!walk(&m, root, v(KERNEL)).mapping().unwrap().user, "user only if every level says so");
    }

    #[test]
    fn mapping_over_something_is_refused() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(HHDM), p(0), PageSize::Size2M, Attrs::DATA).unwrap();
        map(&mut m, root, v(HHDM + M2), p(M2), PageSize::Size4K, Attrs::DATA).unwrap();
        let tables = m.tables.len();
        // The same page; a small page inside a huge one; a huge page over a
        // table of small pages; a 1 GiB page over a table of 2 MiB ones.
        assert_eq!(map(&mut m, root, v(HHDM + M2), p(0), PageSize::Size4K, Attrs::DATA), Err(MapError::AlreadyMapped));
        assert_eq!(map(&mut m, root, v(HHDM + K), p(0), PageSize::Size4K, Attrs::DATA), Err(MapError::AlreadyMapped));
        assert_eq!(map(&mut m, root, v(HHDM + M2), p(0), PageSize::Size2M, Attrs::DATA), Err(MapError::AlreadyMapped));
        assert_eq!(map(&mut m, root, v(HHDM), p(0), PageSize::Size1G, Attrs::DATA), Err(MapError::AlreadyMapped));
        assert_eq!(m.tables.len(), tables, "a refused map allocates nothing");
        assert_eq!(translate(&m, root, v(HHDM + K)), Some(p(K)), "and changes nothing");
        assert_eq!(translate(&m, root, v(HHDM + M2)), Some(p(M2)));
    }

    #[test]
    fn misaligned_maps_are_refused() {
        let (mut m, root) = FakeMem::new();
        let cases = [
            (HHDM + 8, 0, PageSize::Size4K),
            (HHDM, 8, PageSize::Size4K),
            (HHDM + K, 0, PageSize::Size2M),
            (HHDM, K, PageSize::Size2M),
            (HHDM + M2, 0, PageSize::Size1G),
            (HHDM, M2, PageSize::Size1G),
        ];
        for (virt, phys, size) in cases {
            assert_eq!(map(&mut m, root, v(virt), p(phys), size, Attrs::DATA), Err(MapError::Misaligned));
        }
        assert_eq!(m.tables.len(), 1);
    }

    #[test]
    fn running_out_of_table_frames_maps_nothing() {
        let (mut m, root) = FakeMem::new();
        m.allocs_left = 2; // a 4 KiB page under an empty root needs 3
        assert_eq!(map(&mut m, root, v(KERNEL), p(0), PageSize::Size4K, Attrs::DATA), Err(MapError::OutOfMemory));
        assert_eq!(translate(&m, root, v(KERNEL)), None);
        m.allocs_left = 1; // the two tables from the failed attempt are reused
        map(&mut m, root, v(KERNEL), p(0x5000), PageSize::Size4K, Attrs::DATA).unwrap();
        assert_eq!(translate(&m, root, v(KERNEL)), Some(p(0x5000)));
    }

    #[test]
    fn unmap_returns_the_page_and_leaves_the_rest() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(KERNEL), p(0x5000), PageSize::Size4K, Attrs::DATA).unwrap();
        map(&mut m, root, v(KERNEL + K), p(0x6000), PageSize::Size4K, Attrs::DATA).unwrap();
        map(&mut m, root, v(HHDM), p(0), PageSize::Size2M, Attrs::DATA).unwrap();
        let got = unmap(&mut m, root, v(KERNEL)).unwrap();
        assert_eq!((got.virt, got.phys, got.size, got.writable), (v(KERNEL), p(0x5000), PageSize::Size4K, true));
        assert_eq!(translate(&m, root, v(KERNEL)), None);
        assert_eq!(translate(&m, root, v(KERNEL + K)), Some(p(0x6000)));
        assert_eq!(unmap(&mut m, root, v(KERNEL)), Err(UnmapError::NotMapped));
        assert_eq!(unmap(&mut m, root, v(HHDM + K)), Err(UnmapError::NotPageStart));
        assert_eq!(translate(&m, root, v(HHDM + K)), Some(p(K)), "a refused unmap changes nothing");
        assert_eq!(unmap(&mut m, root, v(HHDM)).map(|m| m.size), Ok(PageSize::Size2M));
        assert_eq!(translate(&m, root, v(HHDM + K)), None);
        assert_eq!(unmap(&mut m, root, v(0x1000)), Err(UnmapError::NotMapped));
    }

    #[test]
    fn unreadable_tables_stop_the_walk() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(KERNEL), p(0x5000), PageSize::Size4K, Attrs::DATA).unwrap();
        let l3 = Entry(m.entry(root, 511)).table();
        let l2 = Entry(m.entry(l3, 510)).table();
        m.unreadable.insert(l2.as_u64());
        let w = walk(&m, root, v(KERNEL));
        assert_eq!(w.end, WalkEnd::Unreadable(Level::L2, l2));
        assert_eq!(w.steps().len(), 2);
        assert_eq!(translate(&m, root, v(KERNEL)), None);
        assert_eq!(
            map(&mut m, root, v(KERNEL + K), p(0), PageSize::Size4K, Attrs::DATA),
            Err(MapError::Unreadable(l2))
        );
        assert_eq!(unmap(&mut m, root, v(KERNEL)), Err(UnmapError::Unreadable(l2)));
        assert_eq!(for_each_mapping(&m, root, |_| panic!("nothing is reachable")), Err(l2));
        m.unreadable.insert(root.as_u64());
        assert_eq!(walk(&m, root, v(KERNEL)).end, WalkEnd::Unreadable(Level::L4, root));
    }

    #[test]
    fn not_present_reports_the_level() {
        let (mut m, root) = FakeMem::new();
        assert_eq!(walk(&m, root, v(KERNEL)).end, WalkEnd::NotPresent(Level::L4));
        map(&mut m, root, v(KERNEL), p(0x5000), PageSize::Size4K, Attrs::DATA).unwrap();
        assert_eq!(walk(&m, root, v(KERNEL + K)).end, WalkEnd::NotPresent(Level::L1));
        assert_eq!(walk(&m, root, v(KERNEL + M2)).end, WalkEnd::NotPresent(Level::L2));
        assert_eq!(walk(&m, root, v(KERNEL - G1)).end, WalkEnd::NotPresent(Level::L3));
        assert_eq!(walk(&m, root, v(KERNEL + M2)).steps().len(), 3, "the non-present entry is a step");
    }

    #[test]
    fn sweeps_visit_every_page_and_table() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(0x1000), p(0x9000), PageSize::Size4K, Attrs::DATA).unwrap();
        map(&mut m, root, v(HHDM + M2), p(M2), PageSize::Size2M, Attrs::DATA).unwrap();
        map(&mut m, root, v(KERNEL), p(0x5000), PageSize::Size4K, Attrs::CODE).unwrap();
        let mut seen = Vec::new();
        for_each_mapping(&m, root, |mp| seen.push((mp.virt, mp.phys, mp.size, mp.executable))).unwrap();
        assert_eq!(
            seen,
            [
                (v(0x1000), p(0x9000), PageSize::Size4K, false),
                (v(HHDM + M2), p(M2), PageSize::Size2M, false),
                (v(KERNEL), p(0x5000), PageSize::Size4K, true),
            ]
        );
        let mut tables = BTreeSet::new();
        let n = for_each_table(&m, root, |_, t| {
            tables.insert(t.as_u64());
        });
        assert_eq!(n, m.tables.len());
        assert_eq!(tables, m.tables.keys().copied().collect());
    }

    /// xorshift64: deterministic randomness without a dependency.
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
    fn random_maps_and_unmaps_agree_with_a_model() {
        // Pages in an 8 MiB window that straddles a 1 GiB boundary in the
        // upper half, so the walks cross tables at every level.
        let base = HHDM + G1 - 4 * M2;
        let sizes = [PageSize::Size4K, PageSize::Size2M];
        for seed in 1..=20u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            let (mut m, root) = FakeMem::new();
            // virt page start -> (phys, size, writable)
            let mut model: BTreeMap<u64, (u64, PageSize, bool)> = BTreeMap::new();
            let covering = |model: &BTreeMap<u64, (u64, PageSize, bool)>, a: u64| {
                model.range(..=a).next_back().filter(|&(&s, &(_, size, _))| a < s + size.bytes()).map(|(&s, &e)| (s, e))
            };
            for _ in 0..400 {
                let size = sizes[rng.below(2) as usize];
                let virt = base + rng.below(8 * M2 / size.bytes()) * size.bytes();
                if rng.below(3) == 0 {
                    let got = unmap(&mut m, root, v(virt));
                    match covering(&model, virt) {
                        Some((start, (phys, s, _))) if start == virt => {
                            assert_eq!(got.map(|g| (g.phys, g.size)), Ok((p(phys), s)));
                            model.remove(&start);
                        }
                        Some(_) => assert_eq!(got, Err(UnmapError::NotPageStart)),
                        None => assert_eq!(got, Err(UnmapError::NotMapped)),
                    }
                } else {
                    let phys = rng.below(1 << 30) & !(size.bytes() - 1);
                    let writable = rng.below(2) == 0;
                    let attrs = Attrs { writable, ..Attrs::RODATA };
                    let got = map(&mut m, root, v(virt), p(phys), size, attrs);
                    let end = virt + size.bytes();
                    let overlaps = model.iter().any(|(&s, &(_, sz, _))| s < end && virt < s + sz.bytes());
                    // An empty table where a 2 MiB page would go also blocks
                    // it (tables are never freed): a 2 MiB map may fail where
                    // the model has nothing; it must not succeed over a page.
                    match got {
                        Ok(()) => {
                            assert!(!overlaps, "mapped over an existing page at {virt:#x}");
                            model.insert(virt, (phys, size, writable));
                        }
                        Err(MapError::AlreadyMapped) => {
                            assert!(overlaps || size == PageSize::Size2M, "refused a free 4 KiB page at {virt:#x}")
                        }
                        Err(e) => panic!("unexpected {e:?}"),
                    }
                }
                // Probe a few addresses, then compare the whole tree.
                for _ in 0..8 {
                    let a = base + rng.below(8 * M2);
                    let want = covering(&model, a).map(|(s, (phys, _, _))| p(phys + (a - s)));
                    assert_eq!(translate(&m, root, v(a)), want, "seed {seed}, address {a:#x}");
                }
            }
            let mut seen = BTreeMap::new();
            for_each_mapping(&m, root, |mp| {
                seen.insert(mp.virt.as_u64(), (mp.phys.as_u64(), mp.size, mp.writable));
            })
            .unwrap();
            assert_eq!(seen, model, "seed {seed}");
        }
    }
}
