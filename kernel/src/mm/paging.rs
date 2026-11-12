//! The kernel's own page tables (#4).
//!
//! At boot the kernel runs on Limine's tables. [`init`] builds a fresh
//! hierarchy from frames of the frame allocator, maps
//!
//! - the **HHDM** at Limine's offset over the memory-map entry types the
//!   Limine protocol says its HHDM covers at base revision 3 ([`HHDM_TYPES`]:
//!   usable, bootloader-reclaimable, kernel and modules, framebuffer), so
//!   every HHDM pointer Limine handed out stays valid. No-execute; read-write,
//!   except the kernel-and-modules entries, which are read-only so the
//!   kernel's code has no writable alias; the framebuffer is write-combining.
//!   2 MiB pages where a whole one fits, 4 KiB elsewhere. ACPI memory is not
//!   mapped ([`map_mmio`] maps what ACPI parsing needs);
//! - the **kernel image** per segment from `linker.ld`: text R-X, rodata
//!   (and `.got`) R--, data and bss RW-;
//!
//! checks ([`preflight`]) that everything the kernel is using translates to
//! the same frames as under Limine's tables, and switches CR3. Nothing else
//! is mapped: the lower half is empty, so null and small pointers fault.
//!
//! After that, [`map`] and [`unmap`] manage 4 KiB pages in the kernel heap
//! and stack windows, and [`map_mmio`] maps device registers uncached in the
//! MMIO window (ARCHITECTURE.md section 5). The format, walks and mapper are
//! host-tested in `lumen_mm::paging`; this module gives them access to table
//! frames through the HHDM ([`Hhdm`]).
//!
//! The crash reporter calls [`write_walk`] for a page fault; it takes no lock.

use core::fmt::{self, Write};

use limine::memory_map::{Entry, EntryType};
use log::{debug, info};
use lumen_mm::paging::{
    self as pt, Attrs, Cache, ENTRIES, MapError, Mapping, PageSize, TableMemory, TableMemoryMut, UnmapError, Walk,
    WalkEnd,
};
use lumen_mm::{FRAME_SIZE, PhysAddr, PhysFrame, VirtAddr, align_down, align_up};

use super::{frame, pages};
use crate::arch::paging as hw;
use crate::boot;
use crate::sync::SpinLock;

/// The memory-map entry types the HHDM maps (Limine's at base revision 3,
/// per its protocol, kept so every HHDM pointer Limine handed out stays
/// valid).
pub const HHDM_TYPES: [EntryType; 4] =
    [EntryType::USABLE, EntryType::BOOTLOADER_RECLAIMABLE, EntryType::EXECUTABLE_AND_MODULES, EntryType::FRAMEBUFFER];

/// The kernel is linked in the top 2 GiB (the kernel code model).
const KERNEL_BASE: u64 = 0xffff_ffff_8000_0000;
/// One TiB: the size of the heap, stack and MMIO windows.
const WINDOW: u64 = 1 << 40;
/// Kernel heap (#5) and kernel stacks (#6): where [`map`] and [`unmap`] work.
const HEAP_BASE: u64 = 0xffff_c000_0000_0000;
const STACKS_BASE: u64 = 0xffff_d000_0000_0000;
/// Device mappings: [`map_mmio`] hands out addresses from here.
pub const MMIO_BASE: u64 = 0xffff_e000_0000_0000;
const MMIO_END: u64 = MMIO_BASE + WINDOW;

struct Space {
    root: PhysFrame,
    /// Next free address in the MMIO window (it only grows).
    next_mmio: u64,
}

static SPACE: SpinLock<Option<Space>> = SpinLock::new(None);

/// The memory-map entries the HHDM maps.
fn hhdm_entries() -> impl Iterator<Item = &'static Entry> {
    boot::MEMMAP
        .get_response()
        .into_iter()
        .flat_map(|r| r.entries().iter().copied())
        .filter(|e| HHDM_TYPES.contains(&e.entry_type))
}

/// The pages an entry's HHDM mapping covers: `[start, end)`, rounded out.
fn rounded(e: &Entry) -> (u64, u64) {
    (align_down(e.base, FRAME_SIZE), align_up(e.base.saturating_add(e.length), FRAME_SIZE))
}

/// Is the frame `phys` entirely inside a memory-map entry the HHDM maps?
/// (Arithmetic saturates: the map may be corrupt when the crash reporter
/// asks.)
fn frame_in_hhdm(phys: PhysFrame) -> bool {
    let p = phys.as_u64();
    hhdm_entries().any(|e| e.base <= p && p.saturating_add(FRAME_SIZE) <= e.base.saturating_add(e.length))
}

/// Does the HHDM map any part of the page at `phys`?
fn page_in_hhdm(phys: u64) -> bool {
    hhdm_entries().any(|e| {
        let (start, end) = rounded(e);
        start < phys.saturating_add(FRAME_SIZE) && phys < end
    })
}

/// Page-table frames, read and written through the HHDM. A frame the memory
/// map doesn't put in HHDM-mapped RAM is refused, so a corrupt entry ends a
/// walk instead of faulting (the crash reporter walks the tables of a kernel
/// in an unknown state). This trusts the HHDM mappings themselves.
struct Hhdm;

impl Hhdm {
    fn entry(table: PhysFrame, index: usize) -> Option<*mut u64> {
        assert!(index < ENTRIES, "page-table index {index} out of range");
        if super::hhdm_offset() == 0 || !frame_in_hhdm(table) {
            return None;
        }
        Some(super::phys_to_virt(table.start()).as_mut_ptr::<u64>().wrapping_add(index))
    }
}

impl TableMemory for Hhdm {
    fn read(&self, table: PhysFrame, index: usize) -> Option<u64> {
        let entry = Self::entry(table, index)?;
        // SAFETY: the frame is RAM the HHDM maps (checked by `entry`) and the
        // pointer is u64-aligned. Volatile: the CPU sets accessed/dirty bits
        // behind the compiler's back, and the tables are written while live.
        Some(unsafe { entry.read_volatile() })
    }
}

impl TableMemoryMut for Hhdm {
    fn write(&mut self, table: PhysFrame, index: usize, value: u64) {
        let entry = Self::entry(table, index).expect("the mapper writes only tables it could read");
        // SAFETY: the frame is usable RAM the HHDM maps read-write (page
        // tables come from the allocator) and the pointer is u64-aligned. The
        // mapper writes only tables of a hierarchy it walked from a root this
        // module owns, or tables it just allocated; the caller holds the SPACE
        // lock (or runs alone during init), so no one else writes them.
        unsafe { entry.write_volatile(value) }
    }

    fn alloc_table(&mut self) -> Option<PhysFrame> {
        frame::alloc_zeroed()
    }
}

/// Map `[virt, virt + len)` to `[phys, phys + len)` with 2 MiB pages where
/// a whole one fits (both addresses aligned) and 4 KiB pages elsewhere. On
/// failure, returns how many bytes it had mapped (from `virt`) with the error.
fn map_range(root: PhysFrame, virt: u64, phys: u64, len: u64, attrs: Attrs) -> Result<(), (u64, MapError)> {
    let mut off = 0;
    while off < len {
        let (v, p) = (virt + off, phys + off);
        let big = PageSize::Size2M.bytes();
        let size = if v % big == 0 && p % big == 0 && len - off >= big { PageSize::Size2M } else { PageSize::Size4K };
        let va = VirtAddr::new(v).expect("mapped ranges are canonical");
        let pa = PhysFrame::from_u64(p).expect("mapped ranges are aligned physical addresses");
        pt::map(&mut Hhdm, root, va, pa, size, attrs).map_err(|e| (off, e))?;
        off += size.bytes();
    }
    Ok(())
}

unsafe extern "C" {
    static __rodata_start: u8;
    static __data_start: u8;
}

/// The kernel image's segments as `(name, start, end, rights)`, page
/// aligned by `linker.ld`.
fn kernel_segments() -> [(&'static str, u64, u64, Attrs); 3] {
    let (start, len) = super::kernel_image();
    let (rodata, data) = (&raw const __rodata_start as u64, &raw const __data_start as u64);
    [
        ("text", start, rodata, Attrs::CODE),
        ("rodata", rodata, data, Attrs::RODATA),
        ("data", data, start + len, Attrs::DATA),
    ]
}

/// The rights the HHDM maps an entry type with.
fn hhdm_attrs(kind: EntryType) -> Attrs {
    match kind {
        EntryType::FRAMEBUFFER => Attrs::FRAMEBUFFER,
        // The kernel image and file: nothing writes them through the HHDM,
        // and a writable alias of the kernel's code would undo W^X.
        EntryType::EXECUTABLE_AND_MODULES => Attrs::RODATA,
        _ => Attrs::DATA,
    }
}

/// Build the kernel's page tables and switch to them. Called once from
/// `mm::init`, after the frame allocator exists.
pub fn init() {
    assert!(SPACE.lock().is_none(), "page tables initialized twice");
    assert!(hw::four_level(), "5-level paging is on; the kernel supports 4 levels");
    assert!(boot::MEMMAP.get_response().is_some(), "Limine gave no memory map");
    hw::check_pat();
    // Before any entry with NX set is live: without EFER.NXE that bit is
    // reserved and every access through it faults.
    hw::enforce_nx_and_wp();

    let hhdm = super::hhdm_offset();
    let root = frame::alloc_zeroed().expect("no frame for the root page table");
    // Entries are sorted by address; two that share a page (rounded out)
    // map it once, with the first one's rights.
    let mut mapped_to = 0;
    for e in hhdm_entries() {
        let (start, end) = rounded(e);
        let start = start.max(mapped_to);
        if start < end {
            map_range(root, hhdm + start, start, end - start, hhdm_attrs(e.entry_type))
                .unwrap_or_else(|(_, err)| panic!("HHDM {start:#x}..{end:#x}: {err}"));
            mapped_to = end;
        }
    }
    let image = boot::EXECUTABLE_ADDRESS.get_response().expect("Limine gave no executable address response");
    assert_eq!(image.virtual_base(), super::kernel_image().0, "kernel not loaded at its link address");
    for (name, start, end, attrs) in kernel_segments() {
        assert!(start % FRAME_SIZE == 0 && end % FRAME_SIZE == 0, "kernel {name} segment is not page aligned");
        let phys = image.physical_base() + (start - image.virtual_base());
        map_range(root, start, phys, end - start, attrs).unwrap_or_else(|(_, err)| panic!("kernel {name}: {err}"));
    }

    preflight(hw::active_root(), root);
    // SAFETY: `preflight` checked that every page of every live range, each
    // memory-map entry, every page Limine mapped in the kernel's 2 GiB and
    // every 2 MiB step of each HHDM entry translate to the same frame under
    // `root`, with write access where the kernel writes. EFER.NXE is on, so
    // the NX bits in `root` are valid.
    unsafe { hw::load_root(root) };

    let tables = pt::for_each_table(&Hhdm, root, |_, _| {}).expect("the new page tables are readable");
    info!(target: "mem", "paging: kernel page tables active ({tables} tables, root {root})");
    *SPACE.lock() = Some(Space { root, next_mmio: MMIO_BASE });
}

/// Ranges [`super::live_ranges`] may leave out (the response or value can
/// legitimately be missing); any other missing range is a bug.
const OPTIONAL_RANGES: [&str; 3] = [frame::KERNEL_FILE, frame::FRAMEBUFFER, super::CMDLINE];
/// Live ranges the kernel writes to: they must stay writable.
const WRITTEN_RANGES: [&str; 3] = [frame::BOOT_STACK, super::CURRENT_STACK, frame::BITMAP];

/// Why the new tables don't match what the kernel needs at `virt`.
struct Mismatch {
    what: &'static str,
    virt: VirtAddr,
    before: WalkEnd,
    after: WalkEnd,
}

/// The first place `new` doesn't map something the kernel is using the way
/// `old` does: to the same frame, and writable where the kernel writes.
fn preflight_mismatch(old: PhysFrame, new: PhysFrame) -> Option<Mismatch> {
    let check = |what: &'static str, virt: u64, write: bool| {
        let v = VirtAddr::new(virt).expect("live addresses are canonical");
        let (before, after) = (pt::walk(&Hhdm, old, v), pt::walk(&Hhdm, new, v));
        let at = |w: &Walk| w.mapping().and_then(|m| m.phys.start().checked_add(virt - m.virt.as_u64()));
        let ok = at(&before).is_some()
            && at(&after) == at(&before)
            && (!write || after.mapping().is_some_and(|m| m.writable));
        (!ok).then_some(Mismatch { what, virt: v, before: before.end(), after: after.end() })
    };
    for (name, range) in super::live_ranges() {
        let Some((start, len)) = range else {
            assert!(OPTIONAL_RANGES.contains(&name), "live range '{name}' is unknown");
            debug!(target: "mem", "paging: no {name}; nothing to check");
            continue;
        };
        let write = WRITTEN_RANGES.contains(&name);
        if let Some(m) = pages(start, len).find_map(|p| check(name, p, write)) {
            return Some(m);
        }
    }
    // Each memory-map entry is its own pointer (read on every table access).
    for e in boot::MEMMAP.get_response().expect("checked by init").entries() {
        if let Some(m) = check("memory map entry", core::ptr::from_ref(*e) as u64, false) {
            return Some(m);
        }
    }
    let (_, kdata, kend, _) = kernel_segments()[2];
    if let Some(m) = pages(kdata, kend - kdata).find_map(|p| check("kernel data", p, true)) {
        return Some(m);
    }
    // Everything Limine maps in the kernel's 2 GiB (from the ELF's PT_LOAD
    // segments), not just [__kernel_start, __kernel_end): this is what
    // caught an orphan `.got` the linker had placed after __kernel_end.
    let mut found = None;
    let walked = pt::for_each_mapping(&Hhdm, old, |m| {
        if found.is_none() && m.virt.as_u64() >= KERNEL_BASE {
            found = pages(m.virt.as_u64(), m.size.bytes()).find_map(|p| check("kernel image (boot mapping)", p, false));
        }
    });
    assert_eq!(walked, Ok(()), "boot page tables unreadable");
    if found.is_some() {
        return found;
    }
    let hhdm = super::hhdm_offset();
    for e in hhdm_entries().filter(|e| e.length > 0) {
        let (start, end) = (e.base, e.base + e.length);
        let steps = (align_down(start, FRAME_SIZE)..end).step_by(PageSize::Size2M.bytes() as usize);
        if let Some(m) = steps.chain([end - 1]).find_map(|p| check("HHDM", hhdm + p, false)) {
            return Some(m);
        }
    }
    None
}

/// Panic unless `new` maps everything the kernel is using like `old` does:
/// better an assertion with a message than a fault with no stack right after
/// the CR3 write.
fn preflight(old: PhysFrame, new: PhysFrame) {
    if let Some(m) = preflight_mismatch(old, new) {
        panic!(
            "{}: the kernel's page tables can't replace the boot ones at {} (boot: {:?}; new: {:?})",
            m.what, m.virt, m.before, m.after
        );
    }
}

fn with_space<R>(f: impl FnOnce(&mut Space) -> R) -> R {
    f(SPACE.lock().as_mut().expect("page tables used before mm::init"))
}

/// The root of the kernel's page tables.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests; processes (M2)
pub fn kernel_root() -> PhysFrame {
    with_space(|s| s.root)
}

/// Why [`map`] or [`unmap`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmError {
    /// Only the heap and stack windows are managed page by page; everything
    /// else (the image, the HHDM, MMIO) is fixed.
    OutsideWindow(VirtAddr),
    /// The frame isn't one the allocator handed out, so it may be in use.
    NotAllocated(PhysFrame),
    /// RAM is mapped write-back (the HHDM already maps it that way).
    NotWriteBack,
    Map(MapError),
    Unmap(UnmapError),
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmError::OutsideWindow(v) => write!(f, "{v} is outside the heap and stack windows"),
            VmError::NotAllocated(p) => write!(f, "frame {p} was not allocated"),
            VmError::NotWriteBack => f.write_str("RAM must be mapped write-back"),
            VmError::Map(e) => write!(f, "{e}"),
            VmError::Unmap(e) => write!(f, "{e}"),
        }
    }
}

fn in_window(virt: VirtAddr) -> bool {
    let v = virt.as_u64();
    (HEAP_BASE..HEAP_BASE + WINDOW).contains(&v) || (STACKS_BASE..STACKS_BASE + WINDOW).contains(&v)
}

/// Map the 4 KiB page at `virt` (in the heap or stack window) to `frame`,
/// which the allocator handed out. Fails, changing nothing, if something is
/// mapped there.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: heap (#5), stacks (#6)
pub fn map(virt: VirtAddr, frame: PhysFrame, attrs: Attrs) -> Result<(), VmError> {
    if !in_window(virt) {
        return Err(VmError::OutsideWindow(virt));
    }
    if attrs.cache() != Cache::WriteBack {
        return Err(VmError::NotWriteBack);
    }
    // Before taking SPACE: the lock order is SPACE, then the allocator.
    if !frame::is_allocated(frame) {
        return Err(VmError::NotAllocated(frame));
    }
    with_space(|s| pt::map(&mut Hhdm, s.root, virt, frame, PageSize::Size4K, attrs)).map_err(VmError::Map)
}

/// Unmap the page starting at `virt` (in the heap or stack window), flush
/// it from the TLB, and return what it mapped. Freeing the frame is the
/// caller's job.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: heap (#5), stacks (#6)
pub fn unmap(virt: VirtAddr) -> Result<Mapping, VmError> {
    if !in_window(virt) {
        return Err(VmError::OutsideWindow(virt));
    }
    with_space(|s| {
        let m = pt::unmap(&mut Hhdm, s.root, virt).map_err(VmError::Unmap)?;
        hw::invalidate(virt);
        Ok(m)
    })
}

/// Why [`map_mmio`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmioError {
    /// The range includes memory the HHDM already maps (write-back RAM, or
    /// the write-combining framebuffer); an uncached alias would give it two
    /// memory types.
    Ram(PhysAddr),
    /// Empty, or past the end of the physical address space.
    BadRange,
    /// The MMIO window is used up.
    WindowFull,
    Map(MapError),
}

impl fmt::Display for MmioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MmioError::Ram(p) => write!(f, "{p} is mapped by the HHDM"),
            MmioError::BadRange => f.write_str("empty or invalid range"),
            MmioError::WindowFull => f.write_str("MMIO window full"),
            MmioError::Map(e) => write!(f, "{e}"),
        }
    }
}

/// Map the device registers at `[phys, phys + len)` uncached and read-write
/// into the MMIO window; returns the virtual address of `phys`. Mappings are
/// permanent (drivers keep their registers mapped). A failed call maps
/// nothing (it may use up window addresses, of which there are plenty).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ACPI (#2), APIC timer (#7)
pub fn map_mmio(phys: PhysAddr, len: u64) -> Result<VirtAddr, MmioError> {
    let end = phys.checked_add(len).filter(|_| len > 0).ok_or(MmioError::BadRange)?;
    let (first, last) = (align_down(phys.as_u64(), FRAME_SIZE), align_up(end.as_u64(), FRAME_SIZE));
    let span = last - first;
    if let Some(page) = pages(first, span).find(|&p| page_in_hhdm(p)) {
        return Err(MmioError::Ram(PhysAddr::new(page).expect("inside a valid range")));
    }
    with_space(|s| {
        let base = s.next_mmio;
        let top = base.checked_add(span).filter(|&t| t <= MMIO_END).ok_or(MmioError::WindowFull)?;
        // Claim the addresses first, so a failure can't leave the next call
        // starting on top of a half-built mapping.
        s.next_mmio = top;
        if let Err((done, err)) = map_range(s.root, base, first, span, Attrs::MMIO) {
            for page in pages(base, done) {
                let v = VirtAddr::new(page).expect("the MMIO window is canonical");
                let undone = pt::unmap(&mut Hhdm, s.root, v);
                assert!(undone.is_ok(), "rolling back MMIO page {v}: {undone:?}");
                hw::invalidate(v);
            }
            return Err(MmioError::Map(err));
        }
        Ok(VirtAddr::new(base + (phys.as_u64() - first)).expect("the MMIO window is canonical"))
    })
}

/// Walk `virt` through the active page tables. Takes no lock.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: ktests; kmon/X-ray later
pub fn walk(virt: VirtAddr) -> Walk {
    pt::walk(&Hhdm, hw::active_root(), virt)
}

/// The physical address `virt` maps to in the active page tables.
pub fn translate(virt: VirtAddr) -> Option<PhysAddr> {
    pt::translate(&Hhdm, hw::active_root(), virt)
}

/// Call `f` with every table frame of the active hierarchy; returns how
/// many, or the first table that couldn't be read (whose subtree was then
/// not visited).
pub fn for_each_active_table(mut f: impl FnMut(PhysFrame)) -> Result<usize, PhysFrame> {
    pt::for_each_table(&Hhdm, hw::active_root(), |_, t| f(t))
}

/// Call `f` with every page the active tables map. Fails with the first
/// table that could not be read.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: ktests; X-ray (M5)
pub fn for_each_active_mapping(f: impl FnMut(Mapping)) -> Result<(), PhysFrame> {
    pt::for_each_mapping(&Hhdm, hw::active_root(), f)
}

/// What a leaf allows, for humans: "read-only, executable".
struct Rights(Mapping);

impl fmt::Display for Rights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.0;
        f.write_str(if m.writable { "writable" } else { "read-only" })?;
        f.write_str(if m.executable { ", executable" } else { ", no-execute" })?;
        if m.user {
            f.write_str(", user")?;
        }
        Ok(())
    }
}

/// Write the page-table walk of `virt` for a crash report: every entry read,
/// then how it ended. Takes no lock. A table the memory map doesn't put in
/// HHDM-mapped RAM ends the walk instead of being read; the walk still
/// relies on the HHDM mappings themselves being intact (if they aren't, the
/// crash reporter's nested-crash guard stops the report).
pub fn write_walk(w: &mut dyn Write, virt: u64) -> fmt::Result {
    if super::hhdm_offset() == 0 {
        return writeln!(w, "page walk: unavailable before mm::init");
    }
    let Some(v) = VirtAddr::new(virt) else {
        return writeln!(w, "page walk: {virt:#018x} is not canonical");
    };
    write_walk_from(w, hw::active_root(), v)
}

fn write_walk_from(w: &mut dyn Write, root: PhysFrame, v: VirtAddr) -> fmt::Result {
    writeln!(w, "page walk of {v} (cr3 {root}):")?;
    let walk = pt::walk(&Hhdm, root, v);
    for s in walk.steps() {
        writeln!(w, "  L{}[{:03}] {:#018x}", s.level.number(), s.index, s.entry.raw())?;
    }
    match walk.end() {
        WalkEnd::Mapped(m) => writeln!(w, "  -> {} page at phys {}: {}", m.size, m.phys, Rights(m)),
        WalkEnd::NotPresent(level) => writeln!(w, "  -> not present at L{}", level.number()),
        WalkEnd::Unreadable(level, t) => {
            writeln!(w, "  -> L{} table {t} is not in mapped RAM; stopped", level.number())
        }
    }
}

#[cfg(feature = "ktest")]
mod checks {
    pub use super::*;

    /// A virtual page nothing maps: the top of the kernel heap window (#5
    /// will own the window; the tests unmap what they map).
    pub const SCRATCH: u64 = HEAP_BASE + WINDOW - 2 * FRAME_SIZE;

    pub fn v(a: u64) -> VirtAddr {
        VirtAddr::new(a).expect("canonical")
    }

    pub fn mapping(virt: u64) -> Result<Mapping, &'static str> {
        walk(v(virt)).mapping().ok_or("address not mapped")
    }

    pub fn hhdm_virt(phys: u64) -> u64 {
        crate::mm::phys_to_virt(PhysAddr::new(phys).expect("physical")).as_u64()
    }

    /// Fill a frame (through the HHDM) with `byte`.
    pub fn fill(frame: PhysFrame, byte: u8) {
        // SAFETY: the test owns the frame (just allocated); the HHDM maps it.
        unsafe { core::ptr::write_bytes(hhdm_virt(frame.as_u64()) as *mut u8, byte, FRAME_SIZE as usize) }
    }

    pub fn read(virt: u64) -> u8 {
        // SAFETY: `virt` is mapped (by the caller, or the HHDM) to a frame
        // the test owns.
        unsafe { core::ptr::read_volatile(virt as *const u8) }
    }

    /// A half-open address range `(start, end)`.
    pub type Span = (u64, u64);

    /// The text segment's virtual and physical ranges.
    pub fn text() -> Result<(Span, Span), &'static str> {
        let (_, start, end, _) = kernel_segments()[0];
        let image = boot::EXECUTABLE_ADDRESS.get_response().ok_or("no executable address response")?;
        let phys = image.physical_base() + (start - image.virtual_base());
        Ok(((start, end), (phys, phys + (end - start))))
    }

    /// A fixed buffer for checking formatted output without a heap.
    pub struct Buf {
        pub data: [u8; 512],
        pub len: usize,
    }

    impl Buf {
        pub const fn new() -> Self {
            Self { data: [0; 512], len: 0 }
        }
        pub fn as_str(&self) -> &str {
            core::str::from_utf8(&self.data[..self.len]).unwrap_or("<invalid utf-8>")
        }
    }

    impl Write for Buf {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            let end = self.len + s.len();
            self.data.get_mut(self.len..end).ok_or(fmt::Error)?.copy_from_slice(s.as_bytes());
            self.len = end;
            Ok(())
        }
    }
}

crate::ktest!(
    fn map_unmap_roundtrip() {
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        let (a, b) = (frame::alloc().ok_or("out of frames")?, frame::alloc().ok_or("out of frames")?);
        fill(a, 0xa1);
        fill(b, 0xb2);
        // Collect everything, clean up, then assert: a failure must not
        // leave SCRATCH mapped for the tests after this one.
        let before = translate(v(SCRATCH));
        let mapped = map(v(SCRATCH), a, Attrs::DATA);
        let at = translate(v(SCRATCH + 0x123));
        let first = read(SCRATCH + 7);
        let twice = map(v(SCRATCH), b, Attrs::DATA);
        // SAFETY: SCRATCH maps `a`, which the test owns (or the write faults
        // and the test fails with a crash report).
        unsafe { core::ptr::write_volatile((SCRATCH + 8) as *mut u8, 0x77) };
        let wrote_through = read(hhdm_virt(a.as_u64()) + 8);
        let unmapped = unmap(v(SCRATCH));
        let after = translate(v(SCRATCH));
        // Remap to `b`: a stale TLB entry (no invlpg on unmap) would still
        // read `a`.
        let remapped = map(v(SCRATCH), b, Attrs::DATA);
        let second = read(SCRATCH + 7);
        let cleanup = unmap(v(SCRATCH)).map(|m| m.phys);
        kassert_eq!(frame::free(a), Ok(()));
        kassert_eq!(frame::free(b), Ok(()));
        kassert_eq!(before, None);
        kassert_eq!(mapped, Ok(()));
        kassert_eq!(at, a.start().checked_add(0x123));
        kassert_eq!(first, 0xa1);
        kassert_eq!(twice, Err(VmError::Map(MapError::AlreadyMapped)));
        kassert_eq!(wrote_through, 0x77);
        kassert!(unmapped.is_ok_and(|m| m.phys == a && m.size == PageSize::Size4K && m.writable));
        kassert_eq!(after, None);
        kassert_eq!(remapped, Ok(()));
        kassert_eq!(second, 0xb2);
        kassert_eq!(cleanup, Ok(b));
        kassert_eq!(unmap(v(SCRATCH)), Err(VmError::Unmap(UnmapError::NotMapped)));
        Ok(())
    }
);

crate::ktest!(
    fn map_and_unmap_refuse_what_they_dont_own() {
        // map/unmap are safe functions: they must not let safe code remap
        // the kernel image, the HHDM or a frame someone else is using.
        use self::checks::*;
        use crate::kassert_eq;
        let text = init as *const () as u64;
        let frame = frame::alloc().ok_or("out of frames")?;
        let bitmap = frame::bitmap().ok_or("no bitmap")?.start();
        let not_ours = PhysFrame::from_u64(bitmap).ok_or("bitmap not aligned")?;
        let results = [
            map(v(text & !0xfff), frame, Attrs::DATA),
            map(v(hhdm_virt(frame.as_u64())), frame, Attrs::DATA),
            map(v(SCRATCH), not_ours, Attrs::DATA),
            map(v(SCRATCH), frame, Attrs::MMIO),
        ];
        let unmaps = [unmap(v(text & !0xfff)).map(|_| ()), unmap(v(hhdm_virt(bitmap))).map(|_| ())];
        let still_mapped = translate(v(SCRATCH));
        kassert_eq!(frame::free(frame), Ok(()));
        kassert_eq!(results[0], Err(VmError::OutsideWindow(v(text & !0xfff))));
        kassert_eq!(results[1], Err(VmError::OutsideWindow(v(hhdm_virt(frame.as_u64())))));
        kassert_eq!(results[2], Err(VmError::NotAllocated(not_ours)));
        kassert_eq!(results[3], Err(VmError::NotWriteBack));
        kassert_eq!(unmaps[0], Err(VmError::OutsideWindow(v(text & !0xfff))));
        kassert_eq!(unmaps[1], Err(VmError::OutsideWindow(v(hhdm_virt(bitmap)))));
        kassert_eq!(still_mapped, None);
        Ok(())
    }
);

/// Data the W^X tests look at: in `.data` (mutable, so not `.rodata`).
#[cfg(feature = "ktest")]
static DATA_PROBE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);
#[cfg(feature = "ktest")]
static RODATA_PROBE: [u8; 4] = *b"lumn";

crate::ktest!(
    fn kernel_text_not_writable() {
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        kassert_eq!(hw::active_root(), kernel_root());
        let code = mapping(init as *const () as u64)?;
        kassert!(!code.writable && code.executable);
        let rodata = mapping(core::ptr::from_ref(&RODATA_PROBE) as u64)?;
        kassert!(!rodata.writable && !rodata.executable);
        let data = mapping(core::ptr::from_ref(&DATA_PROBE) as u64)?;
        kassert!(data.writable && !data.executable);
        // The segments' edges, from the linker script.
        for (_, start, end, attrs) in kernel_segments() {
            for addr in [start, end - 1] {
                let m = mapping(addr)?;
                kassert_eq!((m.writable, m.executable), (attrs.writable(), attrs.executable()));
            }
        }
        // The text's frames through the HHDM: readable, not writable.
        let (_, (phys, _)) = text()?;
        let alias = mapping(hhdm_virt(phys))?;
        kassert!(!alias.writable && !alias.executable);
        Ok(())
    }
);

crate::ktest!(
    fn no_page_is_writable_and_executable() {
        // W^X per page and per frame: the only executable pages are the
        // kernel's text, and no writable page anywhere maps a text frame.
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        let ((text_lo, text_hi), (text_plo, text_phi)) = text()?;
        let (mut pages, mut exec, mut saw_init, mut first_bad) = (0usize, 0u64, false, None);
        let init_addr = init as *const () as u64;
        let swept = for_each_active_mapping(|m| {
            pages += 1;
            let (lo, hi) = (m.virt.as_u64(), m.virt.as_u64() + m.size.bytes());
            let (plo, phi) = (m.phys.as_u64(), m.phys.as_u64() + m.size.bytes());
            let bad = (m.writable && m.executable)
                || (m.executable && !(text_lo <= lo && hi <= text_hi))
                || (m.writable && plo < text_phi && text_plo < phi)
                || lo < 1 << 47; // the lower half is empty (null guard)
            if bad {
                first_bad.get_or_insert(m);
            }
            if m.executable {
                exec += m.size.bytes();
                saw_init |= (lo..hi).contains(&init_addr);
            }
        });
        if let Some(m) = first_bad {
            crate::ktest::diag(format_args!("first bad page: {m:?}"));
        }
        kassert_eq!(swept, Ok(()));
        kassert_eq!(first_bad, None);
        kassert!(pages > 512); // the sweep really ran
        kassert!(saw_init);
        kassert_eq!(exec, text_hi - text_lo); // all of the text, nothing else
        crate::ktest::diag(format_args!("{pages} pages mapped; only the {exec} bytes of text are executable"));
        Ok(())
    }
);

crate::ktest!(
    fn hhdm_maps_exactly_the_hhdm_types() {
        // Every mapping is the HHDM (inside a rounded HHDM_TYPES entry, at
        // phys == virt - hhdm), the kernel image or the MMIO window; the HHDM
        // covers each entry completely; ACPI and reserved memory are not in it.
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        let hhdm = crate::mm::hhdm_offset();
        let (kstart, klen) = crate::mm::kernel_image();
        let (mut hhdm_bytes, mut big, mut stray) = (0u64, 0usize, None);
        let swept = for_each_active_mapping(|m| {
            let (lo, size) = (m.virt.as_u64(), m.size.bytes());
            let in_image = kstart <= lo && lo + size <= kstart + klen;
            let in_mmio = (MMIO_BASE..MMIO_END).contains(&lo);
            let in_hhdm = lo.checked_sub(hhdm).is_some_and(|p| {
                p == m.phys.as_u64()
                    && hhdm_entries().any(|e| {
                        let (start, end) = rounded(e);
                        start <= p && p + size <= end && hhdm_attrs(e.entry_type).writable() == m.writable
                    })
            });
            if in_hhdm {
                hhdm_bytes += size;
                big += usize::from(m.size == PageSize::Size2M);
                if m.executable {
                    stray.get_or_insert(m);
                }
            } else if !in_image && !in_mmio {
                stray.get_or_insert(m);
            }
        });
        if let Some(m) = stray {
            crate::ktest::diag(format_args!("unexpected mapping {m:?}"));
        }
        kassert_eq!(swept, Ok(()));
        kassert_eq!(stray, None);
        let mut covered = 0;
        let mut to = 0;
        for e in hhdm_entries() {
            let (start, end) = rounded(e);
            let start = start.max(to);
            if start < end {
                covered += end - start;
                to = end;
            }
        }
        kassert_eq!(hhdm_bytes, covered); // no holes inside an entry
        kassert!(big > 0); // 2 MiB pages are used where they fit
        let entries = boot::MEMMAP.get_response().ok_or("no memory map")?.entries();
        let excluded = [EntryType::ACPI_RECLAIMABLE, EntryType::ACPI_NVS, EntryType::RESERVED];
        for e in entries.iter().filter(|e| excluded.contains(&e.entry_type) && e.length > 0) {
            let (first, last) = (align_up(e.base, FRAME_SIZE), align_down(e.base + e.length, FRAME_SIZE));
            for p in [first, last.saturating_sub(FRAME_SIZE)].into_iter().filter(|&p| p >= first && p < last) {
                if !page_in_hhdm(p) {
                    kassert_eq!(translate(v(hhdm + p)), None);
                }
            }
        }
        Ok(())
    }
);

crate::ktest!(
    fn tables_are_ours_and_allocated() {
        // The exit-criteria scenarios would also pass on Limine's tables
        // (which are W^X too): check the active ones really are ours.
        use crate::{kassert, kassert_eq};
        let root = hw::active_root();
        kassert_eq!(root, kernel_root());
        let mut bad = None;
        let n = for_each_active_table(|t| {
            if !frame::is_allocated(t) {
                bad.get_or_insert(t);
            }
        });
        if let Some(t) = bad {
            crate::ktest::diag(format_args!("table {t} is not an allocated frame"));
        }
        kassert_eq!(bad, None);
        kassert!(n.is_ok_and(|n| n > 1));
        // Limine sets both already, so this only shows they stay set.
        kassert!(hw::efer() & hw::EFER_NXE != 0);
        kassert!(crate::arch::read_cr0() & hw::CR0_WP != 0);
        Ok(())
    }
);

crate::ktest!(
    fn pat_holds_the_memory_types_we_use() {
        // Literal values (Intel SDM: UC = 0, WC = 1, WB = 6), independent of
        // `Cache::pat_slot` and of the boot check.
        use crate::kassert_eq;
        let pat = hw::pat();
        let slot = |n: u32| (pat >> (8 * n)) & 0x7;
        kassert_eq!((slot(0), slot(3), slot(5)), (6, 0, 1));
        Ok(())
    }
);

crate::ktest!(
    fn framebuffer_is_write_combining() {
        use self::checks::*;
        use crate::kassert_eq;
        let (fb, len) = frame::framebuffer_virt().ok_or("no framebuffer")?;
        for addr in [fb, fb + len - 1] {
            let m = mapping(addr)?;
            kassert_eq!(m.cache(), Some(Cache::WriteCombining));
            kassert_eq!((m.writable, m.executable), (true, false));
        }
        let ram = mapping(hhdm_virt(frame::bitmap().ok_or("no bitmap")?.start()))?;
        kassert_eq!(ram.cache(), Some(Cache::WriteBack));
        Ok(())
    }
);

crate::ktest!(
    fn mmio_maps_the_rsdp_uncached() {
        // The ACPI RSDP is outside the HHDM (ACPI memory isn't mapped there),
        // so reading its signature proves the MMIO mapping goes to the right
        // frame.
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        let rsdp = boot::RSDP.get_response().ok_or("no RSDP response")?.address() as u64;
        let rsdp = PhysAddr::new(rsdp).ok_or("RSDP is not physical")?;
        let at = map_mmio(rsdp, 20).map_err(|_| "map_mmio failed")?;
        // SAFETY: map_mmio just mapped these 8 bytes (the RSDP signature).
        let sig = unsafe { core::ptr::read_volatile(at.as_ptr::<[u8; 8]>()) };
        kassert_eq!(&sig, b"RSD PTR ");
        kassert_eq!(at.as_u64() & 0xfff, rsdp.as_u64() & 0xfff);
        let m = mapping(at.as_u64())?;
        kassert_eq!(m.cache(), Some(Cache::Uncached));
        kassert!(m.writable && !m.executable);
        kassert!((MMIO_BASE..MMIO_END).contains(&at.as_u64()));
        // A second mapping of the same registers, from 8 bytes in: new
        // addresses after the first mapping, same bytes.
        let again = map_mmio(rsdp.checked_add(8).ok_or("phys")?, 12).map_err(|_| "second map_mmio failed")?;
        kassert!(again.as_u64() >= (at.as_u64() & !0xfff) + FRAME_SIZE);
        kassert_eq!(again.as_u64() & 0xfff, (rsdp.as_u64() + 8) & 0xfff);
        // SAFETY: both mappings cover RSDP bytes 8..20.
        let (x, y) = unsafe {
            (core::ptr::read_volatile(at.as_ptr::<u8>().add(8)), core::ptr::read_volatile(again.as_ptr::<u8>()))
        };
        kassert_eq!(x, y);
        // RAM is refused: an uncached alias of write-back memory.
        let frame = frame::alloc().ok_or("out of frames")?;
        let ram = map_mmio(frame.start(), 8);
        kassert_eq!(frame::free(frame), Ok(()));
        kassert_eq!(ram, Err(MmioError::Ram(frame.start())));
        kassert_eq!(map_mmio(rsdp, 0), Err(MmioError::BadRange));
        Ok(())
    }
);

crate::ktest!(
    fn walk_reports_each_level() {
        // The crash reporter prints this walk; check it on known addresses.
        use self::checks::*;
        use crate::kassert_eq;
        use lumen_mm::paging::Level;
        let text = walk(v(init as *const () as u64));
        let levels: [Level; 4] = [Level::L4, Level::L3, Level::L2, Level::L1];
        for (s, l) in text.steps().iter().zip(levels) {
            kassert_eq!(s.level, l);
        }
        kassert_eq!((text.steps()[0].index, text.steps()[1].index), (511, 510));
        kassert_eq!(walk(v(0xdead_b000)).end(), WalkEnd::NotPresent(Level::L4));
        // An HHDM page: physical address hhdm-offset away.
        let bitmap = frame::bitmap().ok_or("no bitmap")?.start();
        let direct = walk(v(hhdm_virt(bitmap)));
        kassert_eq!(direct.mapping().map(|m| m.phys.as_u64() + (bitmap & (m.size.bytes() - 1))), Some(bitmap));
        kassert_eq!(direct.steps()[0].index, 256 + ((bitmap >> 39) & 0x1ff) as usize);
        kassert_eq!(walk(v(SCRATCH)).mapping(), None);
        Ok(())
    }
);

crate::ktest!(
    fn crash_walk_stops_at_unreadable_tables_without_locks() {
        // A corrupt entry pointing outside HHDM-mapped RAM must end the walk,
        // not fault; and the crash walk must not take the page-table lock.
        use self::checks::*;
        use crate::kassert_eq;
        use lumen_mm::paging::Level;
        let rsdp = boot::RSDP.get_response().ok_or("no RSDP response")?.address() as u64;
        let acpi = PhysFrame::containing(PhysAddr::new(rsdp).ok_or("phys")?);
        kassert_eq!(frame_in_hhdm(acpi), false);
        kassert_eq!(pt::walk(&Hhdm, acpi, v(0x1000)).end(), WalkEnd::Unreadable(Level::L4, acpi));
        let mut out = Buf::new();
        let held = SPACE.lock(); // a deadlock here hangs the test (harness timeout)
        let wrote = write_walk_from(&mut out, acpi, v(0x1000)).and_then(|()| write_walk(&mut out, 0xdead_b000));
        drop(held);
        kassert_eq!(wrote, Ok(()));
        let text = out.as_str();
        crate::ktest::diag(format_args!("{text}"));
        kassert_eq!(text.contains("is not in mapped RAM; stopped"), true);
        kassert_eq!(text.contains("-> not present at L4"), true);
        Ok(())
    }
);

crate::ktest!(
    fn preflight_detects_a_missing_mapping() {
        // The check guarding the CR3 switch must be able to fail: an empty
        // hierarchy maps nothing the kernel uses.
        use crate::{kassert, kassert_eq};
        let empty = frame::alloc_zeroed().ok_or("out of frames")?;
        let found = preflight_mismatch(hw::active_root(), empty);
        let same = preflight_mismatch(hw::active_root(), hw::active_root());
        kassert_eq!(frame::free(empty), Ok(()));
        kassert!(found.is_some_and(
            |m| m.what == frame::KERNEL_IMAGE && m.after == WalkEnd::NotPresent(lumen_mm::paging::Level::L4)
        ));
        kassert!(same.is_none());
        Ok(())
    }
);
