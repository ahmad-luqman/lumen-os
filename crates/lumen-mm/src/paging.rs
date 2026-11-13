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
//! the leaf only; walks and sweeps report the *combined* rights, which is
//! what the CPU enforces. [`Attrs`] can't express a writable and executable
//! page: W^X holds for every page this mapper creates.
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
//! Kernel mappings only: leaves are never user-accessible (M9 adds that).
//! Tables are never freed: unmapping the last page under a table leaves the
//! empty table in place (it is reused by later maps). A 2 MiB or 1 GiB map
//! where a table already exists fails with [`MapError::AlreadyMapped`] even
//! if that table is empty.
//!
//! Walks don't model reserved bits: a huge bit in an L4 entry (reserved, a
//! #PF on real hardware) is treated as a table pointer.

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

    /// The memory type in PAT slot `slot`, if it is one of these.
    pub const fn from_pat_slot(slot: u8) -> Option<Cache> {
        match slot {
            0 => Some(Cache::WriteBack),
            3 => Some(Cache::Uncached),
            5 => Some(Cache::WriteCombining),
            _ => None,
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

/// What a leaf mapping allows: one of the named combinations, none of which
/// is both writable and executable (the fields are private so W^X can't be
/// broken by building one). Leaves are always supervisor-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attrs {
    writable: bool,
    executable: bool,
    cache: Cache,
}

impl Attrs {
    /// Kernel code: read and execute.
    pub const CODE: Attrs = Attrs { writable: false, executable: true, cache: Cache::WriteBack };
    /// Read-only data.
    pub const RODATA: Attrs = Attrs { writable: false, executable: false, cache: Cache::WriteBack };
    /// Read-write data.
    pub const DATA: Attrs = Attrs { writable: true, executable: false, cache: Cache::WriteBack };
    /// Device registers: read-write, uncached.
    pub const MMIO: Attrs = Attrs { writable: true, executable: false, cache: Cache::Uncached };
    /// A framebuffer: read-write, write-combining.
    pub const FRAMEBUFFER: Attrs = Attrs { writable: true, executable: false, cache: Cache::WriteCombining };

    pub const fn writable(self) -> bool {
        self.writable
    }

    pub const fn executable(self) -> bool {
        self.executable
    }

    pub const fn cache(self) -> Cache {
        self.cache
    }

    /// The leaf entry mapping a page of `size` at `frame` with these rights.
    fn leaf(self, frame: PhysFrame, size: PageSize) -> u64 {
        let mut e = PRESENT | frame.as_u64() | self.cache.bits(size);
        if size != PageSize::Size4K {
            e |= HUGE;
        }
        if self.writable {
            e |= WRITABLE;
        }
        if !self.executable {
            e |= NO_EXECUTE;
        }
        e
    }
}

/// An entry pointing to a lower-level table: permissive, so that the leaf
/// alone decides the rights.
fn table_entry(table: PhysFrame) -> u64 {
    PRESENT | WRITABLE | table.as_u64()
}

/// A raw page-table entry, as read. The decoding methods assume it is
/// present (check [`is_present`](Self::is_present) first).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Entry(u64);

impl Entry {
    pub const fn raw(self) -> u64 {
        self.0
    }

    pub const fn has(self, bit: u64) -> bool {
        self.0 & bit != 0
    }

    pub const fn is_present(self) -> bool {
        self.has(PRESENT)
    }

    /// The size of the page this present entry maps at `level`, or `None` if
    /// it points to a table. The huge bit means "page" only in L3 and L2
    /// entries; in L1 it is the PAT bit, and every L1 entry is a page.
    pub const fn page_size(self, level: Level) -> Option<PageSize> {
        match level {
            Level::L1 => Some(PageSize::Size4K),
            Level::L2 if self.has(HUGE) => Some(PageSize::Size2M),
            Level::L3 if self.has(HUGE) => Some(PageSize::Size1G),
            _ => None,
        }
    }

    /// Does this present entry, at `level`, map a page (rather than point to
    /// a table)?
    pub const fn is_leaf(self, level: Level) -> bool {
        self.page_size(level).is_some()
    }

    /// The table this present, non-leaf entry points to.
    pub const fn table(self) -> PhysFrame {
        PhysFrame::from_u64(self.0 & ADDR_4K).expect("masked to an aligned address below 2^52")
    }

    /// The first frame of the page this present leaf entry (of `size`) maps.
    pub const fn page(self, size: PageSize) -> PhysFrame {
        PhysFrame::from_u64(self.0 & size.addr_mask()).expect("masked to an aligned address below 2^52")
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

/// One entry read on the way down (built only by [`walk`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Step {
    pub level: Level,
    /// The table the entry was read from.
    pub table: PhysFrame,
    pub index: usize,
    pub entry: Entry,
}

/// A present page, with the rights the CPU enforces for it (built only by
/// walks and sweeps).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Mapping {
    /// Start of the virtual page.
    pub virt: VirtAddr,
    /// First frame of the physical page.
    pub phys: PhysFrame,
    pub size: PageSize,
    /// Combined over every level.
    pub writable: bool,
    /// Combined over every level.
    pub executable: bool,
    /// Combined over every level.
    pub user: bool,
    /// The PAT slot of the leaf (see [`Cache::from_pat_slot`]).
    pub pat_slot: u8,
}

impl Mapping {
    /// The memory type, if it is one [`Cache`] names.
    pub const fn cache(&self) -> Option<Cache> {
        Cache::from_pat_slot(self.pat_slot)
    }

    /// The physical address of `virt`, which must be inside this page.
    pub const fn phys_of(&self, virt: VirtAddr) -> Option<PhysAddr> {
        self.phys.start().checked_add(virt.as_u64() - self.virt.as_u64())
    }
}

/// The rights the CPU combines on the way down (every level must allow).
#[derive(Clone, Copy)]
struct PathRights {
    writable: bool,
    executable: bool,
    user: bool,
}

impl PathRights {
    const ALL: Self = Self { writable: true, executable: true, user: true };

    fn and(self, e: Entry) -> Self {
        Self {
            writable: self.writable && e.has(WRITABLE),
            executable: self.executable && !e.has(NO_EXECUTE),
            user: self.user && e.has(USER),
        }
    }

    /// The mapping of the page starting at `virt` that leaf `e` (of `size`)
    /// maps, with these rights combined with `e`'s.
    fn leaf(self, virt: VirtAddr, e: Entry, size: PageSize) -> Mapping {
        let rights = self.and(e);
        Mapping {
            virt,
            phys: e.page(size),
            size,
            writable: rights.writable,
            executable: rights.executable,
            user: rights.user,
            pat_slot: e.pat_slot(size),
        }
    }
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
    end: WalkEnd,
}

impl Walk {
    pub fn steps(&self) -> &[Step] {
        &self.steps[..self.len]
    }

    pub fn end(&self) -> WalkEnd {
        self.end
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
    let empty = Step { level: Level::L4, table: root, index: 0, entry: Entry(0) };
    let mut w = Walk { steps: [empty; 4], len: 0, end: WalkEnd::NotPresent(Level::L4) };
    let (mut table, mut level, mut rights) = (root, Level::L4, PathRights::ALL);
    loop {
        let index = level.index_of(virt);
        let Some(raw) = mem.read(table, index) else {
            w.end = WalkEnd::Unreadable(level, table);
            return w;
        };
        let entry = Entry(raw);
        w.steps[w.len] = Step { level, table, index, entry };
        w.len += 1;
        if !entry.is_present() {
            w.end = WalkEnd::NotPresent(level);
            return w;
        }
        if let Some(size) = entry.page_size(level) {
            w.end = WalkEnd::Mapped(rights.leaf(virt.align_down(size.bytes()), entry, size));
            return w;
        }
        rights = rights.and(entry);
        table = entry.table();
        level = level.below().expect("every L1 entry is a page");
    }
}

/// The physical address `virt` maps to, if it is mapped.
pub fn translate(mem: &impl TableMemory, root: PhysFrame, virt: VirtAddr) -> Option<PhysAddr> {
    walk(mem, root, virt).mapping()?.phys_of(virt)
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

/// Map the page of `size` at `virt` to the physical page starting at
/// `phys`, with `attrs`. The caller flushes nothing: the entry was not
/// present, so no TLB holds it.
pub fn map(
    mem: &mut impl TableMemoryMut,
    root: PhysFrame,
    virt: VirtAddr,
    phys: PhysFrame,
    size: PageSize,
    attrs: Attrs,
) -> Result<(), MapError> {
    if !virt.is_aligned(size.bytes()) || !phys.start().is_aligned(size.bytes()) {
        return Err(MapError::Misaligned);
    }
    let (mut table, mut level) = (root, Level::L4);
    loop {
        let index = level.index_of(virt);
        let entry = Entry(mem.read(table, index).ok_or(MapError::Unreadable(table))?);
        if level == size.level() {
            if entry.is_present() {
                return Err(MapError::AlreadyMapped);
            }
            mem.write(table, index, attrs.leaf(phys, size));
            return Ok(());
        }
        table = if !entry.is_present() {
            let new = mem.alloc_table().ok_or(MapError::OutOfMemory)?;
            mem.write(table, index, table_entry(new));
            new
        } else if entry.is_leaf(level) {
            return Err(MapError::AlreadyMapped);
        } else {
            entry.table()
        };
        level = level.below().expect("the leaf level is L1 at the lowest");
    }
}

/// Map `[virt, virt + len)` to `[phys, phys + len)` (both page aligned)
/// with 2 MiB pages where a whole one fits (both addresses 2 MiB aligned)
/// and 4 KiB pages elsewhere. All or nothing: if a page can't be mapped, the
/// pages already mapped are unmapped again, calling `flushed` with each (the
/// CPU may have cached them), and the error is returned.
pub fn map_range(
    mem: &mut impl TableMemoryMut,
    root: PhysFrame,
    virt: VirtAddr,
    phys: PhysFrame,
    len: u64,
    attrs: Attrs,
    mut flushed: impl FnMut(VirtAddr),
) -> Result<(), MapError> {
    let big = PageSize::Size2M.bytes();
    let mut off = 0;
    while off < len {
        let (v, p) = (virt.checked_add(off), phys.start().checked_add(off));
        let (Some(v), Some(p)) = (v, p) else { return Err(MapError::Misaligned) };
        let size = if v.is_aligned(big) && p.is_aligned(big) && len - off >= big {
            PageSize::Size2M
        } else {
            PageSize::Size4K
        };
        let frame = PhysFrame::from_start(p).ok_or(MapError::Misaligned)?;
        if let Err(e) = map(mem, root, v, frame, size, attrs) {
            // Undo by what each unmap removed: a 2 MiB page goes in one step.
            let mut undo = 0;
            while undo < off {
                let at = virt.checked_add(undo).expect("mapped above");
                let m = unmap(mem, root, at).expect("a page this call just mapped");
                flushed(at);
                undo += m.size.bytes();
            }
            return Err(e);
        }
        off += size.bytes();
    }
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
    let leaf = *w.steps().last().expect("a mapped walk read its leaf");
    mem.write(leaf.table, leaf.index, 0);
    Ok(m)
}

/// The canonical virtual address with these 48 low bits (sign-extended from
/// bit 47).
fn canonical(raw: u64) -> VirtAddr {
    VirtAddr::new(((raw << 16) as i64 >> 16) as u64).expect("sign-extended addresses are canonical")
}

/// Call `f` with every present page under `root`, in address order, with
/// the rights combined over every level. Returns the first table that could
/// not be read, if any (the pages under it are skipped).
pub fn for_each_mapping(mem: &impl TableMemory, root: PhysFrame, mut f: impl FnMut(Mapping)) -> Result<(), PhysFrame> {
    fn go(
        mem: &impl TableMemory,
        level: Level,
        table: PhysFrame,
        base: u64,
        rights: PathRights,
        f: &mut dyn FnMut(Mapping),
        unreadable: &mut Option<PhysFrame>,
    ) {
        for index in 0..ENTRIES {
            let Some(raw) = mem.read(table, index) else {
                unreadable.get_or_insert(table);
                return;
            };
            let entry = Entry(raw);
            if !entry.is_present() {
                continue;
            }
            let base = base | (index as u64) << level.shift();
            match (entry.page_size(level), level.below()) {
                (Some(size), _) => f(rights.leaf(canonical(base), entry, size)),
                (None, Some(below)) => go(mem, below, entry.table(), base, rights.and(entry), f, unreadable),
                (None, None) => unreachable!("every L1 entry is a page"),
            }
        }
    }
    let mut unreadable = None;
    go(mem, Level::L4, root, 0, PathRights::ALL, &mut f, &mut unreadable);
    unreadable.map_or(Ok(()), Err)
}

/// Call `f` with every table frame under `root` (including `root`). Returns
/// how many it visited, or the first table that could not be read (the
/// tables under it are not visited).
pub fn for_each_table(
    mem: &impl TableMemory,
    root: PhysFrame,
    mut f: impl FnMut(PhysFrame),
) -> Result<usize, PhysFrame> {
    fn go(
        mem: &impl TableMemory,
        level: Level,
        table: PhysFrame,
        f: &mut dyn FnMut(PhysFrame),
        unreadable: &mut Option<PhysFrame>,
    ) -> usize {
        f(table);
        let mut n = 1;
        let Some(below) = level.below() else { return n };
        for index in 0..ENTRIES {
            let Some(raw) = mem.read(table, index) else {
                unreadable.get_or_insert(table);
                return n;
            };
            let entry = Entry(raw);
            if entry.is_present() && !entry.is_leaf(level) {
                n += go(mem, below, entry.table(), f, unreadable);
            }
        }
        n
    }
    let mut unreadable = None;
    let n = go(mem, Level::L4, root, &mut f, &mut unreadable);
    unreadable.map_or(Ok(n), Err)
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

        /// The table holding `virt`'s entry at `level` (which must exist).
        fn table_at(&self, root: PhysFrame, virt: VirtAddr, level: Level) -> PhysFrame {
            let (mut t, mut l) = (root, Level::L4);
            while l != level {
                t = Entry(self.entry(t, l.index_of(virt))).table();
                l = l.below().unwrap();
            }
            t
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

    fn f(a: u64) -> PhysFrame {
        PhysFrame::from_u64(a).unwrap()
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
        assert_eq!(canonical((511 << 39) | (510 << 30) | (5 << 12)), v(KERNEL + 5 * K));
        assert_eq!(canonical(256 << 39), v(HHDM));
        assert_eq!(canonical((1 << 39) | (2 << 30)), v((1 << 39) | (2 << 30)));
    }

    #[test]
    fn a_4k_page_maps_and_translates() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(KERNEL + 5 * K), f(0x20_5000), PageSize::Size4K, Attrs::CODE).unwrap();
        assert_eq!(translate(&m, root, v(KERNEL + 5 * K + 0x123)), Some(p(0x20_5123)));
        assert_eq!(translate(&m, root, v(KERNEL + 6 * K)), None);
        let w = walk(&m, root, v(KERNEL + 5 * K + 0x123));
        assert_eq!(
            w.steps().iter().map(|s| (s.level, s.index)).collect::<Vec<_>>(),
            [(Level::L4, 511), (Level::L3, 510), (Level::L2, 0), (Level::L1, 5)]
        );
        assert_eq!(w.steps()[0].table, root);
        let mapping = w.mapping().unwrap();
        assert_eq!((mapping.virt, mapping.phys, mapping.size), (v(KERNEL + 5 * K), f(0x20_5000), PageSize::Size4K));
        assert!(!mapping.writable && mapping.executable && !mapping.user);
        assert_eq!(mapping.cache(), Some(Cache::WriteBack));
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
        map(&mut m, root, v(KERNEL), f(0x30_0000), PageSize::Size4K, Attrs::DATA).unwrap();
        map(&mut m, root, v(KERNEL + K), f(0x30_1000), PageSize::Size4K, Attrs::RODATA).unwrap();
        let data = walk(&m, root, v(KERNEL)).mapping().unwrap();
        assert!(data.writable && !data.executable);
        let ro = walk(&m, root, v(KERNEL + K)).mapping().unwrap();
        assert!(!ro.writable && !ro.executable);
        assert_eq!(walk(&m, root, v(KERNEL)).steps()[3].entry.raw(), PRESENT | WRITABLE | NO_EXECUTE | 0x30_0000);
        // No named combination is both writable and executable.
        for a in [Attrs::CODE, Attrs::RODATA, Attrs::DATA, Attrs::MMIO, Attrs::FRAMEBUFFER] {
            assert!(!(a.writable() && a.executable()), "{a:?}");
        }
    }

    #[test]
    fn huge_pages_translate_with_their_offset() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(HHDM + 2 * M2), f(4 * M2), PageSize::Size2M, Attrs::DATA).unwrap();
        map(&mut m, root, v(HHDM + 3 * G1), f(G1), PageSize::Size1G, Attrs::DATA).unwrap();
        assert_eq!(translate(&m, root, v(HHDM + 2 * M2 + 0x1_2345)), Some(p(4 * M2 + 0x1_2345)));
        assert_eq!(translate(&m, root, v(HHDM + 3 * G1 + 0x123_4567)), Some(p(G1 + 0x123_4567)));
        let w2 = walk(&m, root, v(HHDM + 2 * M2 + 5));
        assert_eq!(w2.steps().len(), 3);
        assert_eq!(w2.mapping().unwrap().size, PageSize::Size2M);
        let w1 = walk(&m, root, v(HHDM + 3 * G1));
        assert_eq!(w1.steps().len(), 2);
        assert_eq!(w1.mapping().unwrap().size, PageSize::Size1G);
        // Huge pages unmap whole.
        assert_eq!(unmap(&mut m, root, v(HHDM + 3 * G1)).map(|m| (m.phys, m.size)), Ok((f(G1), PageSize::Size1G)));
        assert_eq!(translate(&m, root, v(HHDM + 3 * G1 + 0x123_4567)), None);
        assert_eq!(translate(&m, root, v(HHDM + 2 * M2)), Some(p(4 * M2)));
    }

    #[test]
    fn the_pat_bit_moves_with_the_page_size() {
        // Write-combining is slot 5: PWT plus the PAT bit, which is bit 7 in
        // a 4 KiB entry and bit 12 in a huge one (where bit 7 means "huge").
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(HHDM), f(0x8000_0000), PageSize::Size4K, Attrs::FRAMEBUFFER).unwrap();
        map(&mut m, root, v(HHDM + M2), f(0x8020_0000), PageSize::Size2M, Attrs::FRAMEBUFFER).unwrap();
        map(&mut m, root, v(HHDM + 2 * M2), f(0x8040_0000), PageSize::Size2M, Attrs::MMIO).unwrap();
        map(&mut m, root, v(HHDM + G1), f(G1), PageSize::Size1G, Attrs::FRAMEBUFFER).unwrap();
        let small = walk(&m, root, v(HHDM)).steps()[3].entry;
        assert_eq!(small.raw() & (PWT | PCD | PAT_4K | PAT_HUGE), PWT | PAT_4K);
        let huge = walk(&m, root, v(HHDM + M2)).steps()[2].entry;
        assert_eq!(huge.raw() & (PWT | PCD | HUGE | PAT_HUGE), PWT | HUGE | PAT_HUGE);
        let giant = walk(&m, root, v(HHDM + G1)).steps()[1].entry;
        assert_eq!(giant.raw() & (PWT | PCD | HUGE | PAT_HUGE), PWT | HUGE | PAT_HUGE);
        for a in [HHDM, HHDM + M2, HHDM + G1] {
            assert_eq!(walk(&m, root, v(a)).mapping().unwrap().cache(), Some(Cache::WriteCombining));
        }
        assert_eq!(walk(&m, root, v(HHDM + 2 * M2)).mapping().unwrap().cache(), Some(Cache::Uncached));
        // The PAT bit (12) of a huge entry is not part of its address.
        assert_eq!(translate(&m, root, v(HHDM + M2 + 7)), Some(p(0x8020_0007)));
        assert_eq!(translate(&m, root, v(HHDM + G1 + 0x1234)), Some(p(G1 + 0x1234)));
    }

    #[test]
    fn rights_combine_over_every_level() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(KERNEL), f(0x40_0000), PageSize::Size4K, Attrs::CODE).unwrap();
        // Make the leaf writable too, by hand (Attrs can't), so every right
        // starts out granted.
        let l1 = m.table_at(root, v(KERNEL), Level::L1);
        m.set(l1, 0, m.entry(l1, 0) | WRITABLE);
        let l2 = m.table_at(root, v(KERNEL), Level::L2);
        let sweep = |m: &FakeMem| {
            let mut got = Vec::new();
            for_each_mapping(m, root, |mp| got.push(mp)).unwrap();
            assert_eq!(got.len(), 1);
            got[0]
        };
        let before = walk(&m, root, v(KERNEL)).mapping().unwrap();
        assert!(before.writable && before.executable);
        assert_eq!(sweep(&m), before);
        let e = m.entry(l2, 0);
        m.set(l2, 0, e & !WRITABLE);
        assert!(!walk(&m, root, v(KERNEL)).mapping().unwrap().writable);
        assert!(!sweep(&m).writable, "sweeps combine rights too");
        m.set(l2, 0, e | NO_EXECUTE);
        let after = walk(&m, root, v(KERNEL)).mapping().unwrap();
        assert!(after.writable && !after.executable);
        assert_eq!(sweep(&m), after);
        // User only if every level says so.
        assert!(!after.user);
        let mut path = Vec::new();
        for level in [Level::L4, Level::L3, Level::L2, Level::L1] {
            path.push((m.table_at(root, v(KERNEL), level), level.index_of(v(KERNEL))));
        }
        for &(t, i) in &path[1..] {
            m.set(t, i, m.entry(t, i) | USER);
        }
        assert!(!walk(&m, root, v(KERNEL)).mapping().unwrap().user, "the root entry lacks USER");
        let (t, i) = path[0];
        m.set(t, i, m.entry(t, i) | USER);
        assert!(walk(&m, root, v(KERNEL)).mapping().unwrap().user);
        assert!(sweep(&m).user);
    }

    #[test]
    fn mapping_over_something_is_refused() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(HHDM), f(0), PageSize::Size2M, Attrs::DATA).unwrap();
        map(&mut m, root, v(HHDM + M2), f(M2), PageSize::Size4K, Attrs::DATA).unwrap();
        let tables = m.tables.len();
        // The same page; a small page inside a huge one; a huge page over a
        // table of small pages; a 1 GiB page over a table of 2 MiB ones.
        assert_eq!(map(&mut m, root, v(HHDM + M2), f(0), PageSize::Size4K, Attrs::DATA), Err(MapError::AlreadyMapped));
        assert_eq!(map(&mut m, root, v(HHDM + K), f(0), PageSize::Size4K, Attrs::DATA), Err(MapError::AlreadyMapped));
        assert_eq!(map(&mut m, root, v(HHDM + M2), f(0), PageSize::Size2M, Attrs::DATA), Err(MapError::AlreadyMapped));
        assert_eq!(map(&mut m, root, v(HHDM), f(0), PageSize::Size1G, Attrs::DATA), Err(MapError::AlreadyMapped));
        assert_eq!(m.tables.len(), tables, "a refused map allocates nothing");
        assert_eq!(translate(&m, root, v(HHDM + K)), Some(p(K)), "and changes nothing");
        assert_eq!(translate(&m, root, v(HHDM + M2)), Some(p(M2)));
    }

    #[test]
    fn an_empty_table_still_blocks_a_huge_page() {
        // Documented behaviour: tables are never freed, so unmapping the last
        // small page leaves its (now empty) table, which refuses a 2 MiB map.
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(HHDM), f(0x5000), PageSize::Size4K, Attrs::DATA).unwrap();
        unmap(&mut m, root, v(HHDM)).unwrap();
        assert_eq!(walk(&m, root, v(HHDM)).end(), WalkEnd::NotPresent(Level::L1));
        assert_eq!(map(&mut m, root, v(HHDM), f(0), PageSize::Size2M, Attrs::DATA), Err(MapError::AlreadyMapped));
        map(&mut m, root, v(HHDM + K), f(0x6000), PageSize::Size4K, Attrs::DATA).unwrap();
    }

    #[test]
    fn misaligned_maps_are_refused() {
        let (mut m, root) = FakeMem::new();
        let cases = [
            (HHDM + 8, 0, PageSize::Size4K),
            (HHDM + K, 0, PageSize::Size2M),
            (HHDM, K, PageSize::Size2M),
            (HHDM + M2, 0, PageSize::Size1G),
            (HHDM, M2, PageSize::Size1G),
        ];
        for (virt, phys, size) in cases {
            assert_eq!(map(&mut m, root, v(virt), f(phys), size, Attrs::DATA), Err(MapError::Misaligned));
        }
        assert_eq!(m.tables.len(), 1);
    }

    #[test]
    fn running_out_of_table_frames_maps_nothing() {
        let (mut m, root) = FakeMem::new();
        m.allocs_left = 2; // a 4 KiB page under an empty root needs 3
        assert_eq!(map(&mut m, root, v(KERNEL), f(0), PageSize::Size4K, Attrs::DATA), Err(MapError::OutOfMemory));
        assert_eq!(translate(&m, root, v(KERNEL)), None);
        m.allocs_left = 1; // the two tables from the failed attempt are reused
        map(&mut m, root, v(KERNEL), f(0x5000), PageSize::Size4K, Attrs::DATA).unwrap();
        assert_eq!(translate(&m, root, v(KERNEL)), Some(p(0x5000)));
    }

    #[test]
    fn unmap_returns_the_page_and_leaves_the_rest() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(KERNEL), f(0x5000), PageSize::Size4K, Attrs::DATA).unwrap();
        map(&mut m, root, v(KERNEL + K), f(0x6000), PageSize::Size4K, Attrs::DATA).unwrap();
        map(&mut m, root, v(HHDM), f(0), PageSize::Size2M, Attrs::DATA).unwrap();
        let got = unmap(&mut m, root, v(KERNEL)).unwrap();
        assert_eq!((got.virt, got.phys, got.size, got.writable), (v(KERNEL), f(0x5000), PageSize::Size4K, true));
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
    fn unreadable_tables_stop_the_walk_and_are_reported() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(KERNEL), f(0x5000), PageSize::Size4K, Attrs::DATA).unwrap();
        let l2 = m.table_at(root, v(KERNEL), Level::L2);
        m.unreadable.insert(l2.as_u64());
        let w = walk(&m, root, v(KERNEL));
        assert_eq!(w.end(), WalkEnd::Unreadable(Level::L2, l2));
        assert_eq!(w.steps().len(), 2);
        assert_eq!(translate(&m, root, v(KERNEL)), None);
        assert_eq!(
            map(&mut m, root, v(KERNEL + K), f(0), PageSize::Size4K, Attrs::DATA),
            Err(MapError::Unreadable(l2))
        );
        assert_eq!(unmap(&mut m, root, v(KERNEL)), Err(UnmapError::Unreadable(l2)));
        assert_eq!(for_each_mapping(&m, root, |_| panic!("nothing is reachable")), Err(l2));
        // The table sweep visits the unreadable table but says it couldn't
        // look inside, so its children are known to be missing.
        let mut seen = Vec::new();
        assert_eq!(for_each_table(&m, root, |t| seen.push(t)), Err(l2));
        assert!(seen.contains(&l2));
        m.unreadable.insert(root.as_u64());
        assert_eq!(walk(&m, root, v(KERNEL)).end(), WalkEnd::Unreadable(Level::L4, root));
    }

    #[test]
    fn not_present_reports_the_level() {
        let (mut m, root) = FakeMem::new();
        assert_eq!(walk(&m, root, v(KERNEL)).end(), WalkEnd::NotPresent(Level::L4));
        map(&mut m, root, v(KERNEL), f(0x5000), PageSize::Size4K, Attrs::DATA).unwrap();
        assert_eq!(walk(&m, root, v(KERNEL + K)).end(), WalkEnd::NotPresent(Level::L1));
        assert_eq!(walk(&m, root, v(KERNEL + M2)).end(), WalkEnd::NotPresent(Level::L2));
        assert_eq!(walk(&m, root, v(KERNEL - G1)).end(), WalkEnd::NotPresent(Level::L3));
        assert_eq!(walk(&m, root, v(KERNEL + M2)).steps().len(), 3, "the non-present entry is a step");
    }

    #[test]
    fn sweeps_visit_every_page_and_table() {
        let (mut m, root) = FakeMem::new();
        map(&mut m, root, v(0x1000), f(0x9000), PageSize::Size4K, Attrs::DATA).unwrap();
        map(&mut m, root, v(HHDM + M2), f(M2), PageSize::Size2M, Attrs::DATA).unwrap();
        map(&mut m, root, v(KERNEL), f(0x5000), PageSize::Size4K, Attrs::CODE).unwrap();
        let mut seen = Vec::new();
        for_each_mapping(&m, root, |mp| seen.push((mp.virt, mp.phys, mp.size, mp.executable))).unwrap();
        assert_eq!(
            seen,
            [
                (v(0x1000), f(0x9000), PageSize::Size4K, false),
                (v(HHDM + M2), f(M2), PageSize::Size2M, false),
                (v(KERNEL), f(0x5000), PageSize::Size4K, true),
            ]
        );
        let mut tables = BTreeSet::new();
        let n = for_each_table(&m, root, |t| {
            tables.insert(t.as_u64());
        });
        assert_eq!(n, Ok(m.tables.len()));
        assert_eq!(tables, m.tables.keys().copied().collect());
    }

    #[test]
    fn ranges_use_huge_pages_where_they_fit() {
        let (mut m, root) = FakeMem::new();
        // 4 KiB up to the 2 MiB boundary, one 2 MiB page, then a 4 KiB tail.
        let (virt, phys, len) = (HHDM + M2 - 2 * K, M2 - 2 * K, 2 * K + M2 + K);
        map_range(&mut m, root, v(virt), f(phys), len, Attrs::DATA, |_| panic!("nothing to undo")).unwrap();
        let mut seen = Vec::new();
        for_each_mapping(&m, root, |mp| seen.push((mp.virt.as_u64() - HHDM, mp.size))).unwrap();
        assert_eq!(
            seen,
            [
                (M2 - 2 * K, PageSize::Size4K),
                (M2 - K, PageSize::Size4K),
                (M2, PageSize::Size2M),
                (2 * M2, PageSize::Size4K),
            ]
        );
        for off in [0, 2 * K, len - 1] {
            assert_eq!(translate(&m, root, v(virt + off)), Some(p(phys + off)));
        }
        assert_eq!(translate(&m, root, v(virt + len)), None, "nothing past the end");
        // Only 4 KiB pages when the physical side isn't 2 MiB aligned too.
        let (mut m, root) = FakeMem::new();
        map_range(&mut m, root, v(HHDM), f(K), M2, Attrs::DATA, |_| {}).unwrap();
        let mut sizes = BTreeSet::new();
        for_each_mapping(&m, root, |mp| {
            sizes.insert(mp.size.bytes());
        })
        .unwrap();
        assert_eq!(sizes, BTreeSet::from([K]));
    }

    #[test]
    fn a_failed_range_maps_nothing() {
        // A 2 MiB page, then a 4 KiB tail whose L1 table can't be allocated:
        // the rollback must remove the 2 MiB page in one step (stepping by
        // 4 KiB would find the rest "not mapped").
        let (mut m, root) = FakeMem::new();
        let base = HHDM + G1;
        // L3 + L2 for the 2 MiB page; the tail needs one more (L1).
        m.allocs_left = 2;
        let mut flushed = Vec::new();
        let got = map_range(&mut m, root, v(base), f(G1), M2 + K, Attrs::MMIO, |va| flushed.push(va));
        assert_eq!(got, Err(MapError::OutOfMemory));
        assert_eq!(flushed, [v(base)]);
        assert_eq!(for_each_mapping(&m, root, |mp| panic!("left mapped: {mp:?}")), Ok(()));
        // The same with several 4 KiB pages before the failure.
        let (mut m, root) = FakeMem::new();
        m.allocs_left = 3; // L3, L2, L1 for the first table of small pages
        let mut flushed = Vec::new();
        let got = map_range(&mut m, root, v(base + M2 - 2 * K), f(M2 - 2 * K), 3 * K + M2, Attrs::DATA, |va| {
            flushed.push(va.as_u64() - base)
        });
        // The 2 MiB page needs no new table, the tail's L1 table does.
        assert_eq!(got, Err(MapError::OutOfMemory));
        assert_eq!(flushed, [M2 - 2 * K, M2 - K, M2]);
        assert_eq!(for_each_mapping(&m, root, |mp| panic!("left mapped: {mp:?}")), Ok(()));
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
        // Pages in a 16 MiB window centred on a 1 GiB boundary in the upper
        // half, so walks cross L3 entries (two L2 tables) and many L1 tables.
        let base = HHDM + G1 - 4 * M2;
        let sizes = [PageSize::Size4K, PageSize::Size2M];
        // [2M ok, 4K ok, refused over a page, refused by an empty table, unmap ok]
        let mut outcomes = [0usize; 5];
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
                    // Half the time unmap a page that is mapped, so unmaps
                    // that succeed are common.
                    let virt = match model.keys().nth(rng.below(model.len().max(1) as u64) as usize) {
                        Some(&k) if rng.below(2) == 0 => k,
                        _ => virt,
                    };
                    let got = unmap(&mut m, root, v(virt));
                    match covering(&model, virt) {
                        Some((start, (phys, s, _))) if start == virt => {
                            assert_eq!(got.map(|g| (g.phys, g.size)), Ok((f(phys), s)));
                            model.remove(&start);
                            outcomes[4] += 1;
                        }
                        Some(_) => assert_eq!(got, Err(UnmapError::NotPageStart)),
                        None => assert_eq!(got, Err(UnmapError::NotMapped)),
                    }
                } else {
                    let phys = rng.below(1 << 30) & !(size.bytes() - 1);
                    let writable = rng.below(2) == 0;
                    let attrs = if writable { Attrs::DATA } else { Attrs::RODATA };
                    let got = map(&mut m, root, v(virt), f(phys), size, attrs);
                    let end = virt + size.bytes();
                    let overlaps = model.iter().any(|(&s, &(_, sz, _))| s < end && virt < s + sz.bytes());
                    match got {
                        Ok(()) => {
                            assert!(!overlaps, "mapped over an existing page at {virt:#x}");
                            model.insert(virt, (phys, size, writable));
                            outcomes[if size == PageSize::Size2M { 0 } else { 1 }] += 1;
                        }
                        Err(MapError::AlreadyMapped) if overlaps => outcomes[2] += 1,
                        Err(MapError::AlreadyMapped) => {
                            // Only the documented case: a 2 MiB map onto an
                            // (empty) table of small pages.
                            assert_eq!(size, PageSize::Size2M, "refused a free 4 KiB page at {virt:#x}");
                            assert_eq!(walk(&m, root, v(virt)).end(), WalkEnd::NotPresent(Level::L1), "{virt:#x}");
                            outcomes[3] += 1;
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
        // Every kind of outcome happened, so no branch above is vacuous.
        assert!(outcomes.iter().all(|&n| n > 0), "outcomes {outcomes:?}");
    }
}
