//! The kernel's own page tables (#4).
//!
//! At boot the kernel runs on Limine's tables. [`init`] builds a fresh
//! hierarchy from frames of the frame allocator, maps
//!
//! - the **HHDM** at Limine's offset, covering exactly the memory-map entry
//!   types Limine's HHDM covers at base revision 3 ([`HHDM_TYPES`]): usable,
//!   bootloader-reclaimable (the boot stack, Limine's responses), kernel and
//!   modules, and framebuffer (write-combining). Read-write, no-execute; 2 MiB
//!   pages where a whole one fits, 4 KiB at the edges. ACPI memory is not
//!   mapped (#2 maps what it needs);
//! - the **kernel image** with W^X per segment from `linker.ld`: text R-X,
//!   rodata R--, data and bss RW-;
//!
//! checks that every range the kernel is using ([`super::live_ranges`])
//! translates to the same frames as under Limine's tables, and switches CR3.
//! Nothing else is mapped: the lower half is empty, so null and small
//! pointers fault.
//!
//! After that, [`map`] and [`unmap`] change kernel mappings (4 KiB pages) and
//! [`map_mmio`] maps device registers uncached in the MMIO window. The
//! format, walks and mapper are host-tested in `lumen_mm::paging`; this
//! module gives them access to table frames through the HHDM ([`Hhdm`]).
//!
//! The crash reporter calls [`write_walk`] for a page fault; it takes no lock.

use core::fmt::{self, Write};

use limine::memory_map::EntryType;
use log::info;
use lumen_mm::paging::{
    self as pt, Attrs, ENTRIES, MapError, Mapping, PageSize, TableMemory, TableMemoryMut, UnmapError, Walk, WalkEnd,
};
use lumen_mm::{FRAME_SIZE, PhysAddr, PhysFrame, VirtAddr, align_down, align_up};

use super::frame;
use crate::arch::paging as hw;
use crate::boot;
use crate::sync::SpinLock;

/// The memory-map entry types the HHDM maps (Limine's choice at base
/// revision 3, kept so every HHDM pointer Limine handed out stays valid).
pub const HHDM_TYPES: [EntryType; 4] =
    [EntryType::USABLE, EntryType::BOOTLOADER_RECLAIMABLE, EntryType::EXECUTABLE_AND_MODULES, EntryType::FRAMEBUFFER];

/// Device mappings (ARCHITECTURE.md section 5): 1 TiB from here.
pub const MMIO_BASE: u64 = 0xffff_e000_0000_0000;
const MMIO_END: u64 = MMIO_BASE + (1 << 40);

struct Space {
    root: PhysFrame,
    /// Next free address in the MMIO window (it only grows).
    next_mmio: u64,
}

static SPACE: SpinLock<Option<Space>> = SpinLock::new(None);

/// The memory-map entries the HHDM maps.
fn hhdm_entries() -> impl Iterator<Item = &'static limine::memory_map::Entry> {
    boot::MEMMAP
        .get_response()
        .into_iter()
        .flat_map(|r| r.entries().iter().copied())
        .filter(|e| HHDM_TYPES.contains(&e.entry_type))
}

/// Is `[phys, phys + len)` inside one memory-map entry the HHDM maps?
fn in_hhdm(phys: u64, len: u64) -> bool {
    hhdm_entries().any(|e| e.base <= phys && phys.saturating_add(len) <= e.base + e.length)
}

/// Page-table frames, read and written through the HHDM. A frame outside
/// the RAM the HHDM maps is refused, so a corrupt entry ends a walk instead
/// of faulting (the crash reporter walks the tables of a kernel in an
/// unknown state).
struct Hhdm;

impl Hhdm {
    fn entry(table: PhysFrame, index: usize) -> Option<*mut u64> {
        if index >= ENTRIES || super::hhdm_offset() == 0 || !in_hhdm(table.as_u64(), FRAME_SIZE) {
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
        // SAFETY: the mapper writes only tables of a hierarchy it walked from
        // a root this module owns, or tables it just allocated; the caller
        // holds the SPACE lock (or runs alone during init), so no one else
        // writes them.
        unsafe { entry.write_volatile(value) }
    }

    fn alloc_table(&mut self) -> Option<PhysFrame> {
        frame::alloc_zeroed()
    }
}

/// Map `[virt, virt + len)` to `[phys, phys + len)` with 2 MiB pages where
/// a whole one fits (both addresses aligned) and 4 KiB pages elsewhere.
fn map_range(root: PhysFrame, virt: u64, phys: u64, len: u64, attrs: Attrs) -> Result<(), MapError> {
    let mut off = 0;
    while off < len {
        let (v, p) = (virt + off, phys + off);
        let big = PageSize::Size2M.bytes();
        let size = if v % big == 0 && p % big == 0 && len - off >= big { PageSize::Size2M } else { PageSize::Size4K };
        let va = VirtAddr::new(v).expect("mapped ranges are canonical");
        let pa = PhysAddr::new(p).expect("mapped ranges are physical addresses");
        pt::map(&mut Hhdm, root, va, pa, size, attrs)?;
        off += size.bytes();
    }
    Ok(())
}

unsafe extern "C" {
    static __kernel_start: u8;
    static __rodata_start: u8;
    static __data_start: u8;
    static __kernel_end: u8;
}

/// The kernel image's segments as `(name, start, end, rights)`, page
/// aligned by `linker.ld`.
fn kernel_segments() -> [(&'static str, u64, u64, Attrs); 3] {
    let (start, rodata, data, end) = (
        &raw const __kernel_start as u64,
        &raw const __rodata_start as u64,
        &raw const __data_start as u64,
        &raw const __kernel_end as u64,
    );
    [("text", start, rodata, Attrs::CODE), ("rodata", rodata, data, Attrs::RODATA), ("data", data, end, Attrs::DATA)]
}

/// Build the kernel's page tables and switch to them. Called once from
/// `mm::init`, after the frame allocator exists.
pub fn init() {
    assert!(SPACE.lock().is_none(), "page tables initialized twice");
    assert!(hw::four_level(), "5-level paging is on; the kernel supports 4 levels");
    hw::check_pat();
    // Before any entry with NX set is live: without EFER.NXE that bit is
    // reserved and every access through it faults.
    hw::enforce_nx_and_wp();

    let hhdm = super::hhdm_offset();
    let root = frame::alloc_zeroed().expect("no frame for the root page table");
    for e in hhdm_entries() {
        let attrs = if e.entry_type == EntryType::FRAMEBUFFER { Attrs::FRAMEBUFFER } else { Attrs::DATA };
        let (start, end) = (align_down(e.base, FRAME_SIZE), align_up(e.base + e.length, FRAME_SIZE));
        map_range(root, hhdm + start, start, end - start, attrs)
            .unwrap_or_else(|err| panic!("HHDM {start:#x}..{end:#x}: {err}"));
    }
    let image = boot::EXECUTABLE_ADDRESS.get_response().expect("Limine gave no executable address response");
    assert_eq!(image.virtual_base(), &raw const __kernel_start as u64, "kernel not loaded at its link address");
    for (name, start, end, attrs) in kernel_segments() {
        assert!(start % FRAME_SIZE == 0 && end % FRAME_SIZE == 0, "kernel {name} segment is not page aligned");
        let phys = image.physical_base() + (start - image.virtual_base());
        map_range(root, start, phys, end - start, attrs).unwrap_or_else(|err| panic!("kernel {name}: {err}"));
    }

    preflight(hw::active_root(), root);
    // SAFETY: `preflight` checked that every range the kernel uses, and the
    // HHDM, translate to the same frames under `root` as now.
    unsafe { hw::load_root(root) };

    let tables = pt::for_each_table(&Hhdm, root, |_, _| {});
    info!(target: "mem", "paging: kernel page tables active ({tables} tables, root {root})");
    *SPACE.lock() = Some(Space { root, next_mmio: MMIO_BASE });
}

/// Panic unless `new` maps everything the kernel is using to the same frames
/// as `old`: better an assertion with a message than a fault with no stack
/// right after the CR3 write.
fn preflight(old: PhysFrame, new: PhysFrame) {
    let check = |what: &str, virt: u64| {
        let v = VirtAddr::new(virt).expect("live addresses are canonical");
        let before = pt::translate(&Hhdm, old, v);
        assert!(before.is_some(), "{what}: {v} is not mapped by the boot page tables");
        let after = pt::translate(&Hhdm, new, v);
        assert_eq!(after, before, "{what}: the kernel's page tables map {v} differently");
    };
    for (name, range) in super::live_ranges() {
        if let Some((start, len)) = range {
            for page in (align_down(start, FRAME_SIZE)..start + len).step_by(FRAME_SIZE as usize) {
                check(name, page);
            }
        }
    }
    let hhdm = super::hhdm_offset();
    for e in hhdm_entries() {
        check("HHDM", hhdm + e.base);
        check("HHDM", hhdm + align_up(e.base + e.length, FRAME_SIZE) - FRAME_SIZE);
    }
    // Everything Limine maps in the kernel's 2 GiB (from the ELF's PT_LOAD
    // segments), not just [__kernel_start, __kernel_end): this is what
    // caught an orphan `.got` the linker had placed after __kernel_end.
    let walked = pt::for_each_mapping(&Hhdm, old, |m| {
        if m.virt.as_u64() >= KERNEL_HALF_TOP {
            for page in (m.virt.as_u64()..m.virt.as_u64() + m.size.bytes()).step_by(FRAME_SIZE as usize) {
                check("kernel image (boot mapping)", page);
            }
        }
    });
    assert_eq!(walked, Ok(()), "boot page tables unreadable");
}

/// The kernel is linked in the top 2 GiB (the kernel code model).
const KERNEL_HALF_TOP: u64 = 0xffff_ffff_8000_0000;

fn with_space<R>(f: impl FnOnce(&mut Space) -> R) -> R {
    f(SPACE.lock().as_mut().expect("page tables used before mm::init"))
}

/// The root of the kernel's page tables.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests; processes (M2)
pub fn kernel_root() -> PhysFrame {
    with_space(|s| s.root)
}

/// Map the 4 KiB page at `virt` to `frame`. Fails, changing nothing, if
/// something is mapped there.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: heap (#5), stacks (#6)
pub fn map(virt: VirtAddr, frame: PhysFrame, attrs: Attrs) -> Result<(), MapError> {
    with_space(|s| pt::map(&mut Hhdm, s.root, virt, frame.start(), PageSize::Size4K, attrs))
}

/// Unmap the page starting at `virt`, flush it from the TLB, and return what
/// it mapped. Freeing the frame is the caller's job.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: heap (#5), stacks (#6)
pub fn unmap(virt: VirtAddr) -> Result<Mapping, UnmapError> {
    with_space(|s| {
        let m = pt::unmap(&mut Hhdm, s.root, virt)?;
        hw::invalidate(virt);
        Ok(m)
    })
}

/// Why [`map_mmio`] failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MmioError {
    /// The range is RAM the HHDM maps write-back; an uncached alias of it
    /// would give the same memory two memory types.
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
            MmioError::Ram(p) => write!(f, "{p} is RAM"),
            MmioError::BadRange => f.write_str("empty or invalid range"),
            MmioError::WindowFull => f.write_str("MMIO window full"),
            MmioError::Map(e) => write!(f, "{e}"),
        }
    }
}

/// Map the device registers at `[phys, phys + len)` uncached and read-write
/// into the MMIO window; returns the virtual address of `phys`. Mappings are
/// permanent (drivers keep their registers mapped).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ACPI (#2), APIC timer (#7)
pub fn map_mmio(phys: PhysAddr, len: u64) -> Result<VirtAddr, MmioError> {
    let end = phys.checked_add(len).filter(|_| len > 0).ok_or(MmioError::BadRange)?;
    let (first, last) = (align_down(phys.as_u64(), FRAME_SIZE), align_up(end.as_u64(), FRAME_SIZE));
    if let Some(page) = (first..last).step_by(FRAME_SIZE as usize).find(|&p| in_hhdm(p, 1)) {
        return Err(MmioError::Ram(PhysAddr::new(page).expect("checked above")));
    }
    with_space(|s| {
        let base = s.next_mmio;
        if base + (last - first) > MMIO_END {
            return Err(MmioError::WindowFull);
        }
        map_range(s.root, base, first, last - first, Attrs::MMIO).map_err(MmioError::Map)?;
        s.next_mmio = base + (last - first);
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

/// Call `f` with every table frame of the active hierarchy; returns how many.
pub fn for_each_active_table(mut f: impl FnMut(PhysFrame)) -> usize {
    pt::for_each_table(&Hhdm, hw::active_root(), |_, t| f(t))
}

/// Call `f` with every page the active tables map. Fails with the first
/// table that could not be read.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: ktests; X-ray (M5)
pub fn for_each_active_mapping(f: impl FnMut(Mapping)) -> Result<(), PhysFrame> {
    pt::for_each_mapping(&Hhdm, hw::active_root(), f)
}

/// What a leaf allows, for humans: "read-only, executable".
pub struct Rights(pub Mapping);

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
/// then how it ended. Takes no lock and reads only RAM the HHDM maps, so it
/// is safe on the crash path.
pub fn write_walk(w: &mut dyn Write, virt: u64) -> fmt::Result {
    if super::hhdm_offset() == 0 {
        return writeln!(w, "  page walk: unavailable before mm::init");
    }
    let Some(v) = VirtAddr::new(virt) else {
        return writeln!(w, "  page walk: {virt:#018x} is not canonical");
    };
    let root = hw::active_root();
    writeln!(w, "  page walk (cr3 {:#x}):", root.as_u64())?;
    let walk = pt::walk(&Hhdm, root, v);
    for s in walk.steps() {
        writeln!(w, "    L{}[{:03}] {:#018x}", s.level.number(), s.index, s.entry.raw())?;
    }
    match walk.end {
        WalkEnd::Mapped(m) => writeln!(w, "    -> {} page at phys {:#x}: {}", m.size, m.phys.as_u64(), Rights(m)),
        WalkEnd::NotPresent(level) => writeln!(w, "    -> not present at L{}", level.number()),
        WalkEnd::Unreadable(level, t) => {
            writeln!(w, "    -> L{} table {:#x} is not in mapped RAM; stopped", level.number(), t.as_u64())
        }
    }
}

#[cfg(feature = "ktest")]
mod checks {
    use super::*;

    /// A virtual page nothing maps: the top of the kernel heap window (#5
    /// will own the window; the tests unmap what they map).
    pub const SCRATCH: u64 = 0xffff_c0ff_ffff_e000;

    pub fn v(a: u64) -> VirtAddr {
        VirtAddr::new(a).expect("canonical")
    }

    pub fn mapping(virt: u64) -> Result<Mapping, &'static str> {
        walk(v(virt)).mapping().ok_or("address not mapped")
    }

    /// Fill a frame (through the HHDM) with `byte`.
    pub fn fill(frame: PhysFrame, byte: u8) {
        // SAFETY: the test owns the frame (just allocated); the HHDM maps it.
        unsafe { core::ptr::write_bytes(crate::mm::phys_to_virt(frame.start()).as_mut_ptr::<u8>(), byte, 4096) }
    }

    pub fn read(virt: u64) -> u8 {
        // SAFETY: the caller mapped `virt` to a frame it owns.
        unsafe { core::ptr::read_volatile(virt as *const u8) }
    }
}

crate::ktest!(
    fn map_unmap_roundtrip() {
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        let (a, b) = (frame::alloc().ok_or("out of frames")?, frame::alloc().ok_or("out of frames")?);
        fill(a, 0xa1);
        fill(b, 0xb2);
        kassert_eq!(translate(v(SCRATCH)), None);
        kassert_eq!(map(v(SCRATCH), a, Attrs::DATA), Ok(()));
        kassert_eq!(translate(v(SCRATCH + 0x123)), Some(PhysAddr::new(a.as_u64() + 0x123).ok_or("phys")?));
        let first = read(SCRATCH + 7);
        let twice = map(v(SCRATCH), b, Attrs::DATA);
        // SAFETY: SCRATCH maps `a`, which the test owns.
        unsafe { core::ptr::write_volatile((SCRATCH + 8) as *mut u8, 0x77) };
        let wrote_through = read(crate::mm::phys_to_virt(a.start()).as_u64() + 8);
        let unmapped = unmap(v(SCRATCH));
        let after = translate(v(SCRATCH));
        // Remap to `b`: a stale TLB entry (no invlpg on unmap) would still
        // read `a`.
        kassert_eq!(map(v(SCRATCH), b, Attrs::DATA), Ok(()));
        let second = read(SCRATCH + 7);
        let cleanup = unmap(v(SCRATCH)).map(|m| m.phys);
        kassert_eq!(frame::free(a), Ok(()));
        kassert_eq!(frame::free(b), Ok(()));
        kassert_eq!(first, 0xa1);
        kassert_eq!(twice, Err(MapError::AlreadyMapped));
        kassert_eq!(wrote_through, 0x77);
        kassert!(unmapped.is_ok_and(|m| m.phys == a.start() && m.size == PageSize::Size4K && m.writable));
        kassert_eq!(after, None);
        kassert_eq!(second, 0xb2);
        kassert_eq!(cleanup, Ok(b.start()));
        kassert_eq!(unmap(v(SCRATCH)), Err(UnmapError::NotMapped));
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
        let text = mapping(init as *const () as u64)?;
        kassert!(!text.writable && text.executable);
        let rodata = mapping(core::ptr::from_ref(&RODATA_PROBE) as u64)?;
        kassert!(!rodata.writable && !rodata.executable);
        let data = mapping(core::ptr::from_ref(&DATA_PROBE) as u64)?;
        kassert!(data.writable && !data.executable);
        // The segments' edges, from the linker script.
        for (_, start, end, attrs) in kernel_segments() {
            for addr in [start, end - 1] {
                let m = mapping(addr)?;
                kassert_eq!((m.writable, m.executable), (attrs.writable, attrs.executable));
            }
        }
        Ok(())
    }
);

crate::ktest!(
    fn no_page_is_writable_and_executable() {
        use crate::kassert_eq;
        let (mut pages, mut wx, mut lower, mut first_bad) = (0usize, 0usize, 0usize, None);
        let swept = for_each_active_mapping(|m| {
            pages += 1;
            if m.writable && m.executable {
                wx += 1;
                first_bad.get_or_insert(m.virt);
            }
            if m.virt.as_u64() < 1 << 47 {
                lower += 1;
                first_bad.get_or_insert(m.virt);
            }
        });
        if let Some(v) = first_bad {
            crate::ktest::diag(format_args!("first bad page: {v}"));
        }
        kassert_eq!(swept, Ok(()));
        kassert_eq!(wx, 0);
        kassert_eq!(lower, 0); // the lower half is empty (null guard)
        crate::ktest::diag(format_args!("{pages} pages mapped, none both writable and executable"));
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
        kassert!(n > 1);
        let efer = hw::efer();
        kassert!(efer & hw::EFER_NXE != 0);
        kassert!(crate::arch::read_cr0() & hw::CR0_WP != 0);
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
            kassert_eq!(m.pat_slot, lumen_mm::paging::Cache::WriteCombining.pat_slot());
            kassert_eq!((m.writable, m.executable), (true, false));
        }
        let ram = mapping(
            crate::mm::phys_to_virt(PhysAddr::new(frame::bitmap().ok_or("no bitmap")?.start()).ok_or("phys")?).as_u64(),
        )?;
        kassert_eq!(ram.pat_slot, lumen_mm::paging::Cache::WriteBack.pat_slot());
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
        // SAFETY: map_mmio just mapped these 8 bytes (RSDP signature).
        let sig = unsafe { core::ptr::read_volatile(at.as_ptr::<[u8; 8]>()) };
        kassert_eq!(&sig, b"RSD PTR ");
        let m = mapping(at.as_u64())?;
        kassert_eq!(m.pat_slot, lumen_mm::paging::Cache::Uncached.pat_slot());
        kassert!(m.writable && !m.executable);
        kassert!((MMIO_BASE..MMIO_END).contains(&at.as_u64()));
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
        kassert_eq!(text.steps().len(), 4);
        kassert_eq!(text.steps()[0].level, Level::L4);
        kassert_eq!(text.steps()[0].index, 511);
        kassert_eq!(walk(v(0xdead_b000)).end, WalkEnd::NotPresent(Level::L4));
        kassert_eq!(walk(v(SCRATCH)).mapping(), None);
        Ok(())
    }
);
