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
//!   mapped (`acpi` maps the tables it reads with [`map_mmio`]);
//! - the **kernel image** per segment from `linker.ld`: text R-X, rodata
//!   (and `.got`) R--, data and bss RW-;
//!
//! checks ([`preflight`]) that everything the kernel is using is mapped to
//! the same frames, with the rights and memory type it needs, and switches
//! CR3. Nothing else is mapped: the lower half is empty, so null and small
//! pointers fault.
//!
//! After that, [`map`] and [`unmap`] manage 4 KiB pages in the kernel heap
//! and stack windows for whoever holds the window's token (handed out once
//! ever: [`claim_heap_window`] to [`super::heap`], [`claim_stack_window`] to
//! [`super::stack`]); `map` takes the frame as an owned one. [`map_mmio`] maps
//! device registers uncached in the MMIO window (ARCHITECTURE.md section 5).
//! The format, walks and mapper are host-tested in `lumen_mm::paging`; this
//! module gives them access to table frames through the HHDM ([`Hhdm`]).
//!
//! The crash reporter calls [`write_walk`] for a page fault; it takes no lock.

use core::fmt::{self, Write};

use limine::memory_map::{Entry, EntryType};
use log::{debug, info};
pub use lumen_mm::paging::WalkEnd;
use lumen_mm::paging::{
    self as pt, Attrs, Cache, ENTRIES, MapError, Mapping, PageSize, TableMemory, TableMemoryMut, UnmapError, Walk,
};
use lumen_mm::{FRAME_SIZE, PhysAddr, PhysFrame, VirtAddr, align_down, align_up};

use lumen_mm::window::{Claims, Window};

use super::frame::OwnedFrame;
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
/// Base of the kernel heap window ([`super::heap`]). [`map`] and [`unmap`]
/// work here and in the stacks window ([`STACKS_BASE`]).
pub const HEAP_BASE: u64 = 0xffff_c000_0000_0000;
/// The heap grows up from [`HEAP_BASE`] and stops below this. The top GiB of
/// the window is left for the paging ktests' scratch page.
pub const HEAP_LIMIT: u64 = HEAP_BASE + WINDOW - (1 << 30);
/// Base of the kernel stack window ([`super::stack`]).
pub const STACKS_BASE: u64 = 0xffff_d000_0000_0000;
/// Kernel stacks are handed out up from [`STACKS_BASE`] and stop below this.
/// The top GiB of the window is left for the paging ktests' stack page.
pub const STACKS_LIMIT: u64 = STACKS_BASE + WINDOW - (1 << 30);
// The indices of the window claims in `CLAIMS`.
const HEAP_CLAIM: usize = 0;
#[cfg(feature = "ktest")]
const HEAP_TEST_CLAIM: usize = 1;
const STACKS_CLAIM: usize = 2;
#[cfg(feature = "ktest")]
const STACKS_TEST_CLAIM: usize = 3;
// The windows `CLAIMS` covers don't overlap anything else the kernel maps:
// the heap below the stacks, the stacks below MMIO, MMIO below the image
// (and `build` checks the HHDM ends below the heap).
const _: () =
    assert!(HEAP_BASE + WINDOW <= STACKS_BASE && STACKS_BASE + WINDOW <= MMIO_BASE && MMIO_END <= KERNEL_BASE);

/// Where [`map`] and [`unmap`] work, as window claims: the heap, the top GiB
/// of the heap window (a ktest page), the stacks, the top GiB of the stack
/// window (a ktest page).
// SAFETY: these ranges tile the heap and stack windows, nothing else in the
// kernel maps there (`map` is the only way in; `build` checks the HHDM ends
// below them), and this is the only table covering them.
static CLAIMS: Claims<4> = unsafe {
    Claims::new([
        (HEAP_BASE, HEAP_LIMIT),
        (HEAP_LIMIT, HEAP_BASE + WINDOW),
        (STACKS_BASE, STACKS_LIMIT),
        (STACKS_LIMIT, STACKS_BASE + WINDOW),
    ])
};

/// The heap's window, `[HEAP_BASE, HEAP_LIMIT)`. Panics if claimed before.
#[must_use = "dropping the heap window gives it up for good"]
pub(super) fn claim_heap_window() -> Window {
    CLAIMS.claim(HEAP_CLAIM).expect("heap window claimed twice (heap::init called again?)")
}

/// The kernel stacks' window, `[STACKS_BASE, STACKS_LIMIT)`. Panics if
/// claimed before.
#[must_use = "dropping the stack window gives it up for good"]
pub(super) fn claim_stack_window() -> Window {
    CLAIMS.claim(STACKS_CLAIM).expect("stack window claimed twice (stack::init called again?)")
}

/// Device mappings: [`map_mmio`] hands out addresses from here.
const MMIO_BASE: u64 = 0xffff_e000_0000_0000;
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

/// The rights the HHDM maps an entry type with.
fn hhdm_attrs(kind: EntryType) -> Attrs {
    match kind {
        EntryType::FRAMEBUFFER => Attrs::FRAMEBUFFER,
        // The kernel image (and any modules): nothing writes them through
        // the HHDM, and a writable alias of the kernel's code would undo W^X.
        // (The kernel *file* Limine loads is bootloader-reclaimable memory.)
        EntryType::EXECUTABLE_AND_MODULES => Attrs::RODATA,
        _ => Attrs::DATA,
    }
}

/// The HHDM's physical spans `(start, end, rights)`: each entry rounded out
/// to pages, minus what an earlier entry already covers. Limine sorts the
/// map; usable and reclaimable entries are page aligned, others may share a
/// page with a neighbour, which is fine only if both want the same rights.
fn hhdm_spans() -> impl Iterator<Item = (u64, u64, Attrs)> {
    let (mut mapped_to, mut prev) = (0, None::<&Entry>);
    hhdm_entries().filter_map(move |e| {
        let (start, end) = rounded(e);
        let attrs = hhdm_attrs(e.entry_type);
        if let Some(p) = prev {
            assert!(p.base <= e.base, "memory map not sorted at {:#x}", e.base);
            assert!(
                start >= mapped_to || hhdm_attrs(p.entry_type) == attrs,
                "memory-map entries at {:#x} and {:#x} share a page but need different rights",
                p.base,
                e.base
            );
        }
        prev = Some(e);
        let start = start.max(mapped_to);
        (start < end).then(|| {
            mapped_to = end;
            (start, end, attrs)
        })
    })
}

/// Is the frame `phys` entirely inside a memory-map entry the HHDM maps?
/// (Arithmetic saturates: the map may be corrupt when the crash reporter
/// asks.)
fn frame_in_hhdm(phys: PhysFrame) -> bool {
    let p = phys.as_u64();
    hhdm_entries().any(|e| e.base <= p && p.saturating_add(FRAME_SIZE) <= e.base.saturating_add(e.length))
}

/// The first page of `[first, last)` (page aligned) that the HHDM maps at
/// all, if any.
fn first_hhdm_page(first: u64, last: u64) -> Option<u64> {
    hhdm_entries()
        .map(rounded)
        .filter(|&(start, end)| start < last && first < end)
        .map(|(start, _)| start.max(first))
        .min()
}

/// Does the HHDM map every page of `[phys, phys + len)`? (False for an
/// empty or overflowing range.)
pub fn hhdm_covers(phys: PhysAddr, len: u64) -> bool {
    let Some(end) = phys.as_u64().checked_add(len).filter(|_| len > 0) else {
        return false;
    };
    let mut page = align_down(phys.as_u64(), FRAME_SIZE);
    while page < end {
        let Some(next) = page.checked_add(FRAME_SIZE) else {
            return false;
        };
        if !hhdm_entries().map(rounded).any(|(start, end)| start <= page && next <= end) {
            return false;
        }
        page = next;
    }
    true
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

/// Map `[virt, virt + len)` to `[phys, phys + len)`, all or nothing,
/// invalidating any page a failure unmaps again.
fn map_range(root: PhysFrame, virt: u64, phys: u64, len: u64, attrs: Attrs) -> Result<(), MapError> {
    let va = VirtAddr::new(virt).expect("mapped ranges are canonical");
    let pa = PhysFrame::from_u64(phys).expect("mapped ranges are aligned physical addresses");
    pt::map_range(&mut Hhdm, root, va, pa, len, attrs, hw::invalidate)
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
    assert!(start <= rodata && rodata <= data && data <= start + len, "kernel segment symbols out of order");
    [
        ("text", start, rodata, Attrs::CODE),
        ("rodata", rodata, data, Attrs::RODATA),
        ("data", data, start + len, Attrs::DATA),
    ]
}

/// Build the kernel's hierarchy (not yet loaded): the HHDM and the image.
fn build() -> PhysFrame {
    let hhdm = super::hhdm_offset();
    let root = frame::alloc_zeroed().expect("no frame for the root page table");
    for (start, end, attrs) in hhdm_spans() {
        // The heap and stack windows are `map`'s alone (`CLAIMS`, and
        // `unmap`'s safety rests on it): the HHDM must end below them.
        assert!(hhdm + end <= HEAP_BASE, "HHDM {start:#x}..{end:#x} reaches the heap window");
        map_range(root, hhdm + start, start, end - start, attrs)
            .unwrap_or_else(|err| panic!("HHDM {start:#x}..{end:#x}: {err}"));
    }
    let image = boot::EXECUTABLE_ADDRESS.get_response().expect("Limine gave no executable address response");
    assert_eq!(image.virtual_base(), super::kernel_image().0, "kernel not loaded at its link address");
    for (name, start, end, attrs) in kernel_segments() {
        assert!(start % FRAME_SIZE == 0 && end % FRAME_SIZE == 0, "kernel {name} segment is not page aligned");
        let phys = image.physical_base() + (start - image.virtual_base());
        map_range(root, start, phys, end - start, attrs).unwrap_or_else(|err| panic!("kernel {name}: {err}"));
    }
    root
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

    let root = build();
    preflight(hw::active_root(), root);
    // SAFETY: `preflight` checked that every page of every live range, each
    // memory-map entry, every page Limine mapped in the kernel's 2 GiB and
    // every 2 MiB step of each HHDM span translate to the same frame under
    // `root`, writable where the kernel writes, executable exactly where
    // Limine's mapping is (the text), with the memory type each HHDM span
    // needs. EFER.NXE is on, so the NX bits in `root` are valid.
    unsafe { hw::load_root(root) };

    let tables = pt::for_each_table(&Hhdm, root, |_| {}).expect("the new page tables are readable");
    info!(target: "mem", "paging: kernel page tables active ({tables} tables, root {root})");
    *SPACE.lock() = Some(Space { root, next_mmio: MMIO_BASE });
}

/// What a page the kernel is using needs from the new tables.
#[derive(Clone, Copy)]
struct Need {
    /// The kernel writes it.
    write: bool,
    /// It must be executable (`Some(true)`) or not (`Some(false)`).
    exec: Option<bool>,
    cache: Option<Cache>,
}

const SAME_FRAME: Need = Need { write: false, exec: None, cache: None };

/// Whether [`preflight_mismatch`] checks ranges on allocated stacks (the
/// stack window, which a fresh [`build`] doesn't map). The real switch checks
/// everything (the kernel is still on the boot stack then); only the ktest
/// that plants faults in a fresh copy, running on the allocated main stack,
/// skips them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RunningStack {
    Check,
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by a ktest
    Skip,
}

/// Why the new tables don't give the kernel what it needs at `virt`.
#[derive(Debug)]
struct Mismatch {
    what: &'static str,
    virt: VirtAddr,
    before: WalkEnd,
    after: WalkEnd,
}

/// The first place `new` doesn't map something the kernel is using like
/// `old` does: the same frame, with the rights and memory type it needs.
fn preflight_mismatch(old: PhysFrame, new: PhysFrame, running: RunningStack) -> Option<Mismatch> {
    let check = |what: &'static str, virt: u64, need: Need| {
        let v = VirtAddr::new(virt).expect("live addresses are canonical");
        let (before, after) = (pt::walk(&Hhdm, old, v), pt::walk(&Hhdm, new, v));
        let at = |w: &Walk| w.mapping().and_then(|m| m.phys_of(v));
        let ok = at(&before).is_some()
            && at(&after) == at(&before)
            && after.mapping().is_some_and(|m| {
                (!need.write || m.writable)
                    && need.exec.is_none_or(|x| m.executable == x)
                    && need.cache.is_none_or(|c| m.cache() == Some(c))
            });
        (!ok).then_some(Mismatch { what, virt: v, before: before.end(), after: after.end() })
    };
    for r in super::live_ranges() {
        if r.allocated && running == RunningStack::Skip {
            continue;
        }
        let Some((start, len)) = r.range else {
            assert!(r.optional, "live range '{}' is missing (Limine response absent or empty)", r.name);
            debug!(target: "mem", "paging: no {}; nothing to check", r.name);
            continue;
        };
        let need = Need { write: r.written, ..SAME_FRAME };
        if let Some(m) = pages(start, len).find_map(|p| check(r.name, p, need)) {
            return Some(m);
        }
    }
    // Each memory-map entry is its own pointer (read on every table access).
    for e in boot::MEMMAP.get_response().expect("checked by init").entries() {
        if let Some(m) = check("memory map entry", core::ptr::from_ref(*e) as u64, SAME_FRAME) {
            return Some(m);
        }
    }
    let [_, _, (_, kdata, kend, _)] = kernel_segments();
    if let Some(m) =
        pages(kdata, kend - kdata).find_map(|p| check("kernel data", p, Need { write: true, ..SAME_FRAME }))
    {
        return Some(m);
    }
    // Everything Limine maps in the kernel's 2 GiB (from the ELF's PT_LOAD
    // segments), not just [__kernel_start, __kernel_end): this is what
    // caught an orphan `.got` the linker had placed after __kernel_end.
    // Execute rights must match too: Limine maps the text R-X and the rest
    // NX from the same PHDRs the linker script lays out.
    let mut found = None;
    let walked = pt::for_each_mapping(&Hhdm, old, |m| {
        if found.is_none() && m.virt.as_u64() >= KERNEL_BASE {
            let need = Need { exec: Some(m.executable), ..SAME_FRAME };
            found = pages(m.virt.as_u64(), m.size.bytes()).find_map(|p| check("kernel image (boot mapping)", p, need));
        }
    });
    assert_eq!(walked, Ok(()), "boot page tables unreadable");
    if found.is_some() {
        return found;
    }
    let hhdm = super::hhdm_offset();
    for (start, end, attrs) in hhdm_spans() {
        let need = Need { write: attrs.writable(), exec: Some(false), cache: Some(attrs.cache()) };
        let steps = (start..end).step_by(PageSize::Size2M.bytes() as usize).chain([end - FRAME_SIZE]);
        if let Some(m) = steps.map(|p| hhdm + p).find_map(|p| check("HHDM", p, need)) {
            return Some(m);
        }
    }
    None
}

/// Panic unless `new` maps everything the kernel is using like `old` does:
/// better an assertion with a message than a fault with no stack right after
/// the CR3 write.
fn preflight(old: PhysFrame, new: PhysFrame) {
    if let Some(m) = preflight_mismatch(old, new, RunningStack::Check) {
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
    /// The page is outside the caller's [`Window`].
    NotOwner(VirtAddr),
    /// Only the heap and stack windows are managed page by page; everything
    /// else (the image, the HHDM, MMIO) is fixed. (A window outside them
    /// means its claims table was built wrongly.)
    OutsideWindow(VirtAddr),
    /// The frame isn't one the allocator handed out, so it may be in use.
    NotAllocated(PhysFrame),
    /// RAM is mapped write-back (the HHDM already maps it that way).
    NotWriteBack,
    /// Allocator frames are writable through the HHDM, so an executable
    /// mapping of one would break W^X; the heap and stacks never need one.
    Executable,
    Map(MapError),
    Unmap(UnmapError),
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmError::NotOwner(v) => write!(f, "{v} is outside the caller's window"),
            VmError::OutsideWindow(v) => write!(f, "{v} is outside the heap and stack windows"),
            VmError::NotAllocated(p) => write!(f, "frame {p} was not allocated"),
            VmError::NotWriteBack => f.write_str("RAM must be mapped write-back"),
            VmError::Executable => f.write_str("window pages can't be executable"),
            VmError::Map(e) => write!(f, "{e}"),
            VmError::Unmap(e) => write!(f, "{e}"),
        }
    }
}

fn in_window(virt: VirtAddr) -> bool {
    let v = virt.as_u64();
    (HEAP_BASE..HEAP_BASE + WINDOW).contains(&v) || (STACKS_BASE..STACKS_BASE + WINDOW).contains(&v)
}

/// Map the 4 KiB page at `virt` to `frame`, for the holder of the window
/// that owns the page. Safe: the window proves the page is the caller's, the
/// owned frame proves the frame is, and the rights are limited to
/// write-back, non-executable data. Consumes the frame (held by the mapping
/// until [`unmap`]); on failure gives it back and maps nothing (page tables
/// allocated on the way stay). Refuses a page outside `window`
/// ([`VmError::NotOwner`]) or outside the heap and stack windows, a frame
/// nobody allocated, executable or non-write-back rights, and a page already
/// mapped.
pub fn map(window: &mut Window, virt: VirtAddr, frame: OwnedFrame, attrs: Attrs) -> Result<(), (VmError, OwnedFrame)> {
    if let Err(e) = check_page(window, virt) {
        return Err((e, frame));
    }
    if attrs.executable() {
        return Err((VmError::Executable, frame));
    }
    if attrs.cache() != Cache::WriteBack {
        return Err((VmError::NotWriteBack, frame));
    }
    let raw = frame.frame();
    let mapped = with_space(|s| {
        // Under SPACE (the lock order is SPACE, then the allocator).
        if !frame::is_allocated(raw) {
            return Err(VmError::NotAllocated(raw));
        }
        pt::map(&mut Hhdm, s.root, virt, raw, PageSize::Size4K, attrs).map_err(VmError::Map)
    });
    // The frame stays out of the closure, so a refusal can give it back.
    match mapped {
        Ok(()) => {
            let _held_by_the_mapping = frame.into_raw();
            Ok(())
        }
        Err(e) => Err((e, frame)),
    }
}

/// Unmap the page at `virt` (owned by `window`), flush it from the TLB, and
/// give back what it mapped and its frame (dropping the frame frees it).
///
/// # Safety
/// Nothing refers to memory through the page any more (a reference into it
/// would afterwards fault, or, once the page is mapped again, read someone
/// else's memory).
///
/// # Panics
/// If the page wasn't mapped by `map` (not a 4 KiB leaf, or its frame isn't
/// allocated): a broken invariant. The mapping is left in place for the
/// crash report.
pub unsafe fn unmap(window: &mut Window, virt: VirtAddr) -> Result<(Mapping, OwnedFrame), VmError> {
    check_page(window, virt)?;
    let m = with_space(|s| {
        // What `map` makes; checked before the entry goes.
        if let Some(m) = pt::walk(&Hhdm, s.root, virt).mapping() {
            assert!(
                m.size == PageSize::Size4K && frame::is_allocated(m.phys),
                "window page {virt} maps {m:?}, not by `map`"
            );
        }
        let m = pt::unmap(&mut Hhdm, s.root, virt).map_err(VmError::Unmap)?;
        hw::invalidate(virt);
        Ok(m)
    })?;
    // SAFETY: window pages are mapped only by `map` (4 KiB leaves), which
    // took an owned frame for each (vouched for by `alloc_owned` or an
    // `unsafe` `from_raw`) and left it held by the mapping until now.
    Ok((m, unsafe { OwnedFrame::from_raw(m.phys) }))
}

/// Is the page at `virt` the caller's: in `window` and in the heap or stack
/// windows?
fn check_page(window: &Window, virt: VirtAddr) -> Result<(), VmError> {
    if !window.owns_page(virt.as_u64()) {
        Err(VmError::NotOwner(virt))
    } else if !in_window(virt) {
        Err(VmError::OutsideWindow(virt))
    } else {
        Ok(())
    }
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
    /// The MMIO window has no room for it.
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
pub fn map_mmio(phys: PhysAddr, len: u64) -> Result<VirtAddr, MmioError> {
    let end = phys.checked_add(len).filter(|_| len > 0).ok_or(MmioError::BadRange)?;
    let (first, last) = (align_down(phys.as_u64(), FRAME_SIZE), align_up(end.as_u64(), FRAME_SIZE));
    let span = last - first;
    if span > WINDOW {
        return Err(MmioError::WindowFull);
    }
    if let Some(page) = first_hhdm_page(first, last) {
        return Err(MmioError::Ram(PhysAddr::new(page).expect("inside a valid range")));
    }
    with_space(|s| {
        let base = s.next_mmio;
        let top = base.checked_add(span).filter(|&t| t <= MMIO_END).ok_or(MmioError::WindowFull)?;
        // Claim the addresses first: a failure leaves nothing mapped, and
        // the next call starts past them either way.
        s.next_mmio = top;
        map_range(s.root, base, first, span, Attrs::MMIO).map_err(MmioError::Map)?;
        Ok(VirtAddr::new(base + (phys.as_u64() - first)).expect("the MMIO window is canonical"))
    })
}

/// Walk `virt` through the active page tables. Takes no lock.
pub fn walk(virt: VirtAddr) -> Walk {
    pt::walk(&Hhdm, hw::active_root(), virt)
}

/// The physical address `virt` maps to in the active page tables.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: ktests; kmon/X-ray later
pub fn translate(virt: VirtAddr) -> Option<PhysAddr> {
    pt::translate(&Hhdm, hw::active_root(), virt)
}

/// Call `f` with every table frame of the active hierarchy; returns how
/// many, or the first table that couldn't be read (whose subtree was then
/// not visited).
pub fn for_each_active_table(f: impl FnMut(PhysFrame)) -> Result<usize, PhysFrame> {
    pt::for_each_table(&Hhdm, hw::active_root(), f)
}

/// Call `f` with every page the active tables map. Fails with the first
/// table that could not be read.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: ktests; X-ray (M5)
pub fn for_each_active_mapping(f: impl FnMut(Mapping)) -> Result<(), PhysFrame> {
    pt::for_each_mapping(&Hhdm, hw::active_root(), f)
}

/// A 4 KiB kernel page, read-write, no-execute, write-back, backed by an
/// allocated frame: how the heap and the kernel stacks are mapped (ktests).
#[cfg(feature = "ktest")]
pub fn is_private_data_page(m: &Mapping) -> bool {
    m.size == PageSize::Size4K
        && m.writable
        && !m.executable
        && !m.user
        && m.cache() == Some(Cache::WriteBack)
        && frame::is_allocated(m.phys)
}

/// How many table frames the active hierarchy uses (ktests' frame
/// accounting: tables are never freed).
#[cfg(feature = "ktest")]
pub fn active_table_count() -> Result<usize, &'static str> {
    for_each_active_table(|_| {}).map_err(|_| "a page table is unreadable")
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
/// crash reporter notes that the walk faulted and ends the report).
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

/// `map` must refuse (for the heap's and stacks' ktests too).
#[cfg(feature = "ktest")]
pub use checks::map_refused;

#[cfg(feature = "ktest")]
mod checks {
    pub use super::*;

    /// A virtual page nothing maps: in the heap window, but above where the
    /// heap may grow (the tests unmap what they map).
    pub const SCRATCH: u64 = HEAP_LIMIT;

    pub fn v(a: u64) -> VirtAddr {
        VirtAddr::new(a).expect("canonical")
    }

    pub fn mapping(virt: u64) -> Result<Mapping, &'static str> {
        walk(v(virt)).mapping().ok_or("address not mapped")
    }

    pub fn hhdm_virt(phys: u64) -> u64 {
        crate::mm::phys_to_virt(PhysAddr::new(phys).expect("physical")).as_u64()
    }

    pub fn code_addr() -> u64 {
        init as *const () as u64
    }

    pub fn rsdp() -> Result<PhysAddr, &'static str> {
        let addr = boot::RSDP.get_response().ok_or("no RSDP response")?.address() as u64;
        PhysAddr::new(addr).ok_or("RSDP is not physical")
    }

    /// Fill a frame through the HHDM.
    ///
    /// # Safety
    /// The caller owns `frame` (it isn't mapped elsewhere or used).
    pub unsafe fn fill(frame: PhysFrame, byte: u8) {
        // SAFETY: upheld by the caller; the HHDM maps the frame.
        unsafe { core::ptr::write_bytes(hhdm_virt(frame.as_u64()) as *mut u8, byte, FRAME_SIZE as usize) }
    }

    /// Read one byte.
    ///
    /// # Safety
    /// `virt` is mapped (by the caller, or the HHDM) to memory the caller
    /// may read.
    pub unsafe fn read(virt: u64) -> u8 {
        // SAFETY: upheld by the caller.
        unsafe { core::ptr::read_volatile(virt as *const u8) }
    }

    /// A half-open address range `(start, end)`.
    pub type Span = (u64, u64);

    /// The text segment's virtual and physical ranges.
    pub fn text() -> Result<(Span, Span), &'static str> {
        let [(_, start, end, _), ..] = kernel_segments();
        let image = boot::EXECUTABLE_ADDRESS.get_response().ok_or("no executable address response")?;
        let phys = image.physical_base() + (start - image.virtual_base());
        Ok(((start, end), (phys, phys + (end - start))))
    }

    /// Flip `bits` in the leaf entry mapping `virt` under `root`.
    ///
    /// # Safety
    /// `root` is a hierarchy the caller owns and that isn't loaded (CR3).
    pub unsafe fn flip_leaf(root: PhysFrame, virt: u64, bits: u64) -> Result<(), &'static str> {
        let w = pt::walk(&Hhdm, root, v(virt));
        w.mapping().ok_or("not mapped")?;
        let leaf = *w.steps().last().ok_or("no steps")?;
        Hhdm.write(leaf.table, leaf.index, leaf.entry.raw() ^ bits);
        Ok(())
    }

    /// Return every table of a hierarchy (not the active one) to the
    /// allocator.
    ///
    /// # Safety
    /// The hierarchy is the caller's (built for it, not loaded in CR3) and
    /// nothing refers to its tables any more.
    pub unsafe fn free_tables(root: PhysFrame) -> Result<(), &'static str> {
        let mut failed = false;
        // SAFETY: the caller owns every table and nothing uses them.
        pt::for_each_table(&Hhdm, root, |t| failed |= unsafe { frame::free(t) }.is_err())
            .map_err(|_| "unreadable table")?;
        if failed { Err("freeing a table failed") } else { Ok(()) }
    }

    pub use crate::ktest::Buf;

    /// The two ktest windows (the top GiB of the heap and of the stack
    /// window), claimed on first use and lent out: claims are once ever.
    static TEST_WINDOWS: SpinLock<Option<(Window, Window)>> = SpinLock::new(None);

    /// Run `f` with the heap and stack test windows. `f` must not split them
    /// (that would take SCRATCH away from later tests) or call
    /// `with_test_windows` again (it would spin forever).
    pub fn with_test_windows<R>(f: impl FnOnce(&mut Window, &mut Window) -> R) -> R {
        let mut windows = TEST_WINDOWS.lock();
        let (heap, stack) = windows.get_or_insert_with(|| {
            let claim = |i| CLAIMS.claim(i).unwrap_or_else(|| panic!("ktest window {i} claimed twice"));
            (claim(HEAP_TEST_CLAIM), claim(STACKS_TEST_CLAIM))
        });
        f(heap, stack)
    }

    /// An allocated frame as an owned one (or "out of frames").
    pub fn owned() -> Result<OwnedFrame, &'static str> {
        frame::alloc_owned().ok_or("out of frames")
    }

    /// `map` must refuse: its error and the frame it gives back. If it maps
    /// instead, the test fails naming `case` (printed first), and the page is
    /// unmapped again, its frame given up raw so a frame the test doesn't own
    /// is never freed. (For a frame the allocator doesn't own, `unmap`'s own
    /// check panics instead, after the case was printed.)
    pub fn map_refused(
        window: &mut Window,
        virt: VirtAddr,
        frame: OwnedFrame,
        attrs: Attrs,
        case: &'static str,
    ) -> Result<(VmError, OwnedFrame), &'static str> {
        match map(window, virt, frame, attrs) {
            Err((e, frame)) => Ok((e, frame)),
            Ok(()) => {
                crate::ktest::diag(format_args!("map at {virt} was not refused: {case}"));
                // SAFETY: the test just mapped this page and holds nothing in it.
                match unsafe { unmap(window, virt) } {
                    Ok((_, frame)) => {
                        let _ = frame.into_raw();
                    }
                    Err(e) => crate::ktest::diag(format_args!("and unmapping it again failed: {e}")),
                }
                Err("a map that should be refused succeeded")
            }
        }
    }
}

crate::ktest!(
    fn map_unmap_roundtrip() {
        use self::checks::*;
        use crate::kassert_eq;
        with_test_windows(|heap, stack| {
            let (a, b) = (owned()?, owned()?);
            let (a_raw, b_raw) = (a.frame(), b.frame());
            // SAFETY: both frames are this test's and unused.
            unsafe {
                fill(a_raw, 0xa1);
                fill(b_raw, 0xb2);
            }
            // Collect everything, clean up, then assert: a failure must not
            // leave SCRATCH mapped for the tests after this one.
            let before = translate(v(SCRATCH));
            let mapped = map(heap, v(SCRATCH), a, Attrs::DATA).map_err(|(e, _)| e);
            if let Err(e) = mapped {
                // Reading SCRATCH below would fault and hide the reason.
                crate::ktest::diag(format_args!("mapping SCRATCH failed: {e}"));
                return Err("mapping SCRATCH failed");
            }
            let at = translate(v(SCRATCH + 0x123));
            // SAFETY: SCRATCH maps `a` (or the read faults and the test fails
            // with a crash report); same for the reads below.
            let first = unsafe { read(SCRATCH + 7) };
            // A second mapping is refused and gives `b` back; another window
            // can neither map over the live page nor unmap it.
            let (twice, b) = map_refused(heap, v(SCRATCH), b, Attrs::DATA, "a second mapping of SCRATCH")?;
            let (foreign_map, b) = map_refused(stack, v(SCRATCH), b, Attrs::DATA, "SCRATCH from the stack window")?;
            // SAFETY: refused before touching the mapping (asserted).
            let foreign_unmap = unsafe { unmap(stack, v(SCRATCH)) }.map(|_| ());
            let still_a = translate(v(SCRATCH));
            // SAFETY: SCRATCH maps `a`, which the test owns.
            unsafe { core::ptr::write_volatile((SCRATCH + 8) as *mut u8, 0x77) };
            // SAFETY: `a` is the test's; the HHDM maps it.
            let wrote_through = unsafe { read(hhdm_virt(a_raw.as_u64()) + 8) };
            // SAFETY: nothing refers into SCRATCH (same for the calls below).
            let unmapped = unsafe { unmap(heap, v(SCRATCH)) };
            let after = translate(v(SCRATCH));
            // Remap to `b`: a stale TLB entry (no invlpg on unmap) would
            // still read `a`.
            let remapped = map(heap, v(SCRATCH), b, Attrs::DATA).map_err(|(e, _)| e);
            // SAFETY: SCRATCH maps `b`.
            let second = unsafe { read(SCRATCH + 7) };
            // SAFETY: as above.
            let cleanup = unsafe { unmap(heap, v(SCRATCH)) };
            kassert_eq!(before, None);
            kassert_eq!(mapped, Ok(()));
            kassert_eq!(at, a_raw.start().checked_add(0x123));
            kassert_eq!(first, 0xa1);
            kassert_eq!(twice, VmError::Map(MapError::AlreadyMapped));
            kassert_eq!(foreign_map, VmError::NotOwner(v(SCRATCH)));
            kassert_eq!(foreign_unmap, Err(VmError::NotOwner(v(SCRATCH))));
            kassert_eq!(still_a, Some(a_raw.start()));
            kassert_eq!(wrote_through, 0x77);
            // `unmap` gives back the very frame it mapped (dropping it frees it).
            kassert_eq!(
                unmapped.map(|(m, f)| (m.phys, f.frame(), m.size, m.writable)),
                Ok((a_raw, a_raw, PageSize::Size4K, true))
            );
            kassert_eq!(after, None);
            kassert_eq!(remapped, Ok(()));
            kassert_eq!(second, 0xb2);
            kassert_eq!(cleanup.map(|(m, f)| (m.phys, f.frame())), Ok((b_raw, b_raw)));
            // SAFETY: as above.
            kassert_eq!(unsafe { unmap(heap, v(SCRATCH)) }.map(|_| ()), Err(VmError::Unmap(UnmapError::NotMapped)));
            Ok(())
        })
    }
);

crate::ktest!(
    fn map_and_unmap_refuse_what_they_dont_own() {
        // The checks map/unmap make: pages the window owns (NotOwner), in
        // the heap and stack windows (OutsideWindow), allocated frames,
        // write-back, never executable. Each refusal gives the frame back.
        use self::checks::*;
        use crate::kassert_eq;
        let bitmap = frame::bitmap().ok_or("no bitmap")?.start();
        let not_ours = PhysFrame::from_u64(bitmap).ok_or("bitmap not aligned")?;
        let text_page = v(code_addr()).align_down(FRAME_SIZE);
        let (free_before, tables_before) = (frame::free_frames(), active_table_count()?);
        with_test_windows(|heap, stack| {
            let alias = v(hhdm_virt(not_ours.as_u64()));
            let past_heap = v(HEAP_BASE + WINDOW);
            let heap_last = v(HEAP_LIMIT - FRAME_SIZE);
            let stacks_last = v(STACKS_LIMIT - FRAME_SIZE);
            let (scratch, stack_test) = (v(SCRATCH), v(STACKS_LIMIT));
            // Each refusal gives the frame back for the next one. (No heap
            // allocation in here: growing the heap would upset the frame
            // count checked at the end.)
            let mut f = owned()?;
            // Through the heap test window: outside both the token and the
            // windows, then outside only the token.
            for (virt, case) in [
                (text_page, "the kernel text"),
                (alias, "an HHDM alias"),
                (past_heap, "past the heap window"),
                (heap_last, "the real heap's last page"),
                (stack_test, "the stack test window's first page"),
            ] {
                let (got, back) = map_refused(heap, virt, f, Attrs::DATA, case)?;
                kassert_eq!(got, VmError::NotOwner(virt));
                f = back;
            }
            // Through the stack test window: outside only the token.
            for (virt, case) in [(stacks_last, "the real stacks' last page"), (scratch, "the heap test window")] {
                let (got, back) = map_refused(stack, virt, f, Attrs::DATA, case)?;
                kassert_eq!(got, VmError::NotOwner(virt));
                f = back;
            }
            for (attrs, want, case) in [
                (Attrs::MMIO, VmError::NotWriteBack, "uncached rights"),
                (Attrs::CODE, VmError::Executable, "executable rights"),
            ] {
                let (got, back) = map_refused(heap, scratch, f, attrs, case)?;
                kassert_eq!(got, want);
                f = back;
            }
            drop(f); // frees it
            // A frame nobody handed out, and one that was freed: only
            // `unsafe` code can claim to own them; `map` still refuses. Each
            // is given back raw before anything is asserted, so an early
            // return never frees a frame the test doesn't own.
            // SAFETY: deliberately false ownership, refused before any use
            // (asserted), and given back raw so nothing frees it.
            let bogus = unsafe { frame::OwnedFrame::from_raw(not_ours) };
            let (got, bogus) = map_refused(heap, scratch, bogus, Attrs::DATA, "a frame nobody handed out")?;
            let _ = bogus.into_raw();
            kassert_eq!(got, VmError::NotAllocated(not_ours));
            let freed = owned()?.frame(); // allocated, then freed on drop
            // SAFETY: as above (`freed` is the allocator's again).
            let stale = unsafe { frame::OwnedFrame::from_raw(freed) };
            let (got, stale) = map_refused(heap, scratch, stale, Attrs::DATA, "a freed frame")?;
            let _ = stale.into_raw();
            kassert_eq!(got, VmError::NotAllocated(freed));
            // `unmap` refuses the same way (outside the token).
            for (on_stack, page) in [(false, text_page), (false, heap_last), (true, scratch)] {
                let window = if on_stack { &mut *stack } else { &mut *heap };
                // SAFETY: refused before touching any mapping (asserted).
                kassert_eq!(unsafe { unmap(window, page) }.map(|_| ()), Err(VmError::NotOwner(page)));
            }
            // The stack test window works like the heap one.
            let stack_page = v(STACKS_LIMIT);
            let f = owned()?;
            let raw = f.frame();
            let stack_map = map(stack, stack_page, f, Attrs::DATA).map_err(|(e, _)| e);
            // SAFETY: nothing refers into the page.
            let stack_unmap = unsafe { unmap(stack, stack_page) };
            kassert_eq!(stack_map, Ok(()));
            kassert_eq!(stack_unmap.map(|(m, f)| (m.phys, f.frame())), Ok((raw, raw)));
            kassert_eq!(translate(v(SCRATCH)), None);
            Ok(())
        })?;
        // Every owned frame went back to the allocator (page tables made on
        // the way stay).
        kassert_eq!(frame::free_frames() + (active_table_count()? - tables_before), free_before);
        Ok(())
    }
);

crate::ktest!(
    fn a_window_outside_the_windows_is_refused_too() {
        // `map` and `unmap` also check the heap and stack windows themselves,
        // in case a claims table was built wrongly (only `unsafe` code can).
        use self::checks::*;
        use crate::kassert_eq;
        let text_page = align_down(code_addr(), FRAME_SIZE);
        let mmio_top = MMIO_END - FRAME_SIZE; // never handed out, unmapped
        // SAFETY: deliberately wrong (the kernel text, and the MMIO window);
        // `map` and `unmap` refuse before touching anything (asserted), so
        // nothing is ever mapped or unmapped there. This table is a second
        // one only in the test kernel.
        let wrong = unsafe { Claims::new([(text_page, text_page + FRAME_SIZE), (mmio_top, MMIO_END)]) };
        let (mut text, mut mmio) = (wrong.claim(0).ok_or("claimed")?, wrong.claim(1).ok_or("claimed")?);
        let mapped = map(&mut text, v(text_page), owned()?, Attrs::DATA).map_err(|(e, _)| e);
        // SAFETY: refused before touching any mapping (asserted).
        let unmapped = unsafe { unmap(&mut mmio, v(mmio_top)) }.map(|_| ());
        kassert_eq!(mapped, Err(VmError::OutsideWindow(v(text_page))));
        kassert_eq!(unmapped, Err(VmError::OutsideWindow(v(mmio_top))));
        Ok(())
    }
);

crate::ktest!(
    fn windows_are_claimed_once_ever() {
        // The heap and stack own their windows (claimed in `mm::init`), the
        // ktests theirs; no one can claim any of them again.
        use self::checks::*;
        use crate::kassert;
        with_test_windows(|_, _| ());
        kassert!(
            [HEAP_CLAIM, HEAP_TEST_CLAIM, STACKS_CLAIM, STACKS_TEST_CLAIM]
                .into_iter()
                .all(|i| CLAIMS.claim(i).is_none())
        );
        Ok(())
    }
);

crate::ktest!(
    fn map_fresh_rolls_back_at_an_already_mapped_page() {
        // `map_fresh` (safe) over three pages whose middle one is already
        // mapped: it fails, unmaps what it mapped, and leaves the page that
        // was there before alone.
        use self::checks::*;
        use crate::kassert_eq;
        let (free_before, tables_before) = (frame::free_frames(), active_table_count()?);
        with_test_windows(|heap, _| {
            let f = owned()?;
            let raw = f.frame();
            let middle = v(SCRATCH + FRAME_SIZE);
            if let Err((e, _)) = map(heap, middle, f, Attrs::DATA) {
                crate::ktest::diag(format_args!("mapping the middle page failed: {e}"));
                return Err("mapping the middle page failed");
            }
            let result = super::window::map_fresh(heap, SCRATCH..SCRATCH + 3 * FRAME_SIZE);
            let (first, still) = (translate(v(SCRATCH)), translate(middle));
            // SAFETY: nothing refers into these pages (a broken rollback may
            // have left the first one mapped: clean it up for later tests).
            let cleanup = unsafe { unmap(heap, middle) }.map(|(m, _)| m.phys);
            if first.is_some() {
                // SAFETY: as above.
                let _ = unsafe { unmap(heap, v(SCRATCH)) };
            }
            kassert_eq!(result, Err(super::window::WindowError::Map(VmError::Map(MapError::AlreadyMapped))));
            kassert_eq!(first, None);
            kassert_eq!(still, Some(raw.start()));
            kassert_eq!(cleanup, Ok(raw));
            Ok(())
        })?;
        kassert_eq!(frame::free_frames() + (active_table_count()? - tables_before), free_before);
        Ok(())
    }
);

crate::ktest!(
    fn dropping_an_owned_frame_frees_it() {
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        let before = frame::free_frames();
        let f = owned()?;
        let raw = f.frame();
        kassert_eq!(frame::free_frames(), before - 1);
        kassert!(frame::is_allocated(raw));
        drop(f);
        kassert_eq!(frame::free_frames(), before);
        kassert!(!frame::is_allocated(raw));
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
        let code = mapping(code_addr())?;
        kassert!(!code.writable && code.executable);
        let rodata = mapping(VirtAddr::from_ptr(&RODATA_PROBE).as_u64())?;
        kassert!(!rodata.writable && !rodata.executable);
        let data = mapping(VirtAddr::from_ptr(&DATA_PROBE).as_u64())?;
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
        let (mut pages, mut exec, mut saw_code, mut first_bad) = (0usize, 0u64, false, None);
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
                saw_code |= (lo..hi).contains(&code_addr());
            }
        });
        kassert_eq!(swept, Ok(()));
        kassert_eq!(first_bad, None);
        kassert!(pages > 512); // the sweep really ran
        kassert!(saw_code);
        kassert_eq!(exec, text_hi - text_lo); // all of the text, nothing else
        crate::ktest::diag(format_args!("{pages} pages mapped; only the {exec} bytes of text are executable"));
        Ok(())
    }
);

crate::ktest!(
    fn hhdm_maps_exactly_the_hhdm_types() {
        // Every mapping is the HHDM (inside an HHDM span, at phys == virt -
        // hhdm), the kernel image, the MMIO window, the heap's mapped span or
        // a registered kernel stack (4 KiB pages, read-write, no-execute,
        // write-back, allocated frames); every stack's guard page is
        // unmapped. Each HHDM span is mapped throughout (every 2 MiB step and
        // its last page) with its own rights and memory type. ACPI and
        // reserved memory are not mapped (spot check: each entry's first and
        // last whole page).
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        let hhdm = crate::mm::hhdm_offset();
        let (kstart, klen) = crate::mm::kernel_image();
        // Keep a block live so the heap has pages mapped, whichever tests
        // ran before this one.
        let keep = core::hint::black_box(alloc::vec![0u8; 1]);
        let heap = crate::mm::heap::mapped();
        kassert!(!heap.is_empty());
        let (mut big, mut stray) = (0usize, None);
        let swept = for_each_active_mapping(|m| {
            let (lo, size) = (m.virt.as_u64(), m.size.bytes());
            let in_image = kstart <= lo && lo + size <= kstart + klen;
            let in_mmio = (MMIO_BASE..MMIO_END).contains(&lo);
            let in_hhdm = lo.checked_sub(hhdm).is_some_and(|p| {
                p == m.phys.as_u64() && !m.executable && hhdm_spans().any(|(s, e, _)| s <= p && p + size <= e)
            });
            let private_page = is_private_data_page(&m);
            let in_heap = heap.contains(&lo) && private_page;
            let in_stack = crate::arch::stacks::REGISTRY.contains(lo, size) && private_page;
            big += usize::from(in_hhdm && m.size == PageSize::Size2M);
            if !in_hhdm && !in_image && !in_mmio && !in_heap && !in_stack {
                stray.get_or_insert(m);
            }
        });
        drop(keep);
        kassert_eq!(swept, Ok(()));
        kassert_eq!(stray, None);
        let stacks = || crate::arch::stacks::REGISTRY.iter();
        kassert!(stacks().count() >= 4); // the main stack and three IST stacks
        kassert!(stacks().all(|r| walk(v(r.range.guard())).mapping().is_none()));
        kassert!(big > 0); // 2 MiB pages are used where they fit
        let mut wrong = None;
        for (start, end, attrs) in hhdm_spans() {
            let steps = (start..end).step_by(PageSize::Size2M.bytes() as usize).chain([end - FRAME_SIZE]);
            for p in steps {
                let m = walk(v(hhdm + p)).mapping();
                let ok = m.is_some_and(|m| {
                    m.phys_of(v(hhdm + p)).map(PhysAddr::as_u64) == Some(p)
                        && m.writable == attrs.writable()
                        && m.cache() == Some(attrs.cache())
                });
                if !ok {
                    wrong.get_or_insert((p, m));
                }
            }
        }
        kassert_eq!(wrong, None);
        let entries = boot::MEMMAP.get_response().ok_or("no memory map")?.entries();
        let excluded = [EntryType::ACPI_RECLAIMABLE, EntryType::ACPI_NVS, EntryType::RESERVED];
        // Written out, not HHDM_TYPES: adding a type there must fail this.
        let limine_hhdm = [
            EntryType::USABLE,
            EntryType::BOOTLOADER_RECLAIMABLE,
            EntryType::EXECUTABLE_AND_MODULES,
            EntryType::FRAMEBUFFER,
        ];
        let shared = |p: u64| {
            entries.iter().any(|e| {
                let (s, end) = rounded(e);
                limine_hhdm.contains(&e.entry_type) && s < p + FRAME_SIZE && p < end
            })
        };
        let mut checked = 0;
        for e in entries.iter().filter(|e| excluded.contains(&e.entry_type)) {
            let (first, last) = (align_up(e.base, FRAME_SIZE), align_down(e.base + e.length, FRAME_SIZE));
            if first < last {
                for p in [first, last - FRAME_SIZE] {
                    if !shared(p) {
                        kassert_eq!(translate(v(hhdm + p)), None);
                        checked += 1;
                    }
                }
            }
        }
        kassert!(checked > 0); // OVMF's map has ACPI and reserved entries
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
        let rsdp = rsdp()?;
        let mapped = |phys: PhysAddr, len: u64| {
            map_mmio(phys, len).map_err(|e| {
                crate::ktest::diag(format_args!("map_mmio({phys}, {len}): {e}"));
                "map_mmio failed"
            })
        };
        let at = mapped(rsdp, 20)?;
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
        let again = mapped(rsdp.checked_add(8).ok_or("phys")?, 12)?;
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
        // SAFETY: allocated by this test and not used since.
        kassert_eq!(unsafe { frame::free(frame) }, Ok(()));
        kassert_eq!(ram, Err(MmioError::Ram(frame.start())));
        // So is the framebuffer (mapped write-combining by the HHDM).
        let fb = translate(v(frame::framebuffer_virt().ok_or("no framebuffer")?.0)).ok_or("framebuffer unmapped")?;
        let fb_page = PhysAddr::new(fb.as_u64() & !0xfff).ok_or("phys")?;
        kassert_eq!(map_mmio(fb.checked_add(0x10).ok_or("phys")?, 4), Err(MmioError::Ram(fb_page)));
        kassert_eq!(map_mmio(rsdp, 0), Err(MmioError::BadRange));
        kassert_eq!(map_mmio(rsdp, WINDOW + 1), Err(MmioError::WindowFull));
        Ok(())
    }
);

crate::ktest!(
    fn walk_reports_each_level() {
        // The crash reporter prints this walk; check it on known addresses.
        use self::checks::*;
        use crate::kassert_eq;
        use lumen_mm::paging::Level;
        let text = walk(v(code_addr()));
        let levels: [Level; 4] = [Level::L4, Level::L3, Level::L2, Level::L1];
        for (s, l) in text.steps().iter().zip(levels) {
            kassert_eq!(s.level, l);
        }
        kassert_eq!((text.steps()[0].index, text.steps()[1].index), (511, 510));
        kassert_eq!(walk(v(0xdead_b000)).end(), WalkEnd::NotPresent(Level::L4));
        // An HHDM page: physical address hhdm-offset away.
        let bitmap = frame::bitmap().ok_or("no bitmap")?.start();
        let direct = walk(v(hhdm_virt(bitmap)));
        kassert_eq!(direct.mapping().and_then(|m| m.phys_of(v(hhdm_virt(bitmap)))), PhysAddr::new(bitmap));
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
        use crate::{kassert, kassert_eq};
        use lumen_mm::paging::Level;
        let acpi = PhysFrame::containing(rsdp()?);
        kassert!(!frame_in_hhdm(acpi));
        kassert_eq!(pt::walk(&Hhdm, acpi, v(0x1000)).end(), WalkEnd::Unreadable(Level::L4, acpi));
        let mut out = Buf::new();
        let held = SPACE.lock(); // a deadlock here hangs the test (harness timeout)
        let wrote = write_walk_from(&mut out, acpi, v(0x1000)).and_then(|()| write_walk(&mut out, 0xdead_b000));
        drop(held);
        kassert_eq!(wrote, Ok(()));
        let text = out.as_str();
        kassert!(text.contains("is not in mapped RAM; stopped"));
        kassert!(text.contains("-> not present at L4"));
        Ok(())
    }
);

crate::ktest!(
    fn preflight_detects_each_kind_of_mismatch() {
        // The check guarding the CR3 switch must be able to fail, for each
        // thing it checks: build a fresh copy of the kernel's tables (which
        // must pass), plant one fault at a time and see it named.
        use self::checks::*;
        use crate::kassert_eq;
        use lumen_mm::paging::bits::{NO_EXECUTE, PCD, PWT, WRITABLE};
        let active = hw::active_root();
        let copy = build();
        // This runs on the allocated main stack, which a fresh copy doesn't
        // map: checked strictly, that is the first thing named; the planted
        // faults are looked for with it skipped.
        let strict = preflight_mismatch(active, copy, RunningStack::Check).map(|m| m.what);
        let what = |old| preflight_mismatch(old, copy, RunningStack::Skip).map(|m| m.what);
        let clean = what(active);
        let [_, _, (_, kdata, _, _)] = kernel_segments();
        let ram = hhdm_spans().find(|&(_, _, a)| a == Attrs::DATA).ok_or("no usable memory")?.0;
        let ram = crate::mm::hhdm_offset() + ram;
        let mut planted = [None; 3];
        for (i, (virt, bits)) in
            [(kdata, WRITABLE), (code_addr(), NO_EXECUTE), (ram, PCD | PWT)].into_iter().enumerate()
        {
            // SAFETY: `copy` was built for this test and is never loaded.
            unsafe { flip_leaf(copy, virt, bits)? };
            planted[i] = what(active);
            // SAFETY: as above.
            unsafe { flip_leaf(copy, virt, bits)? };
        }
        let restored = what(active);
        // SAFETY: `copy` was built for this test, never loaded, and nothing
        // refers to its tables any more.
        let freed = unsafe { free_tables(copy) };
        let empty = frame::alloc_zeroed().ok_or("out of frames")?;
        let nothing = preflight_mismatch(active, empty, RunningStack::Check);
        // SAFETY: allocated by this test and not used since.
        kassert_eq!(unsafe { frame::free(empty) }, Ok(()));
        kassert_eq!(freed, Ok(()));
        kassert_eq!(strict, Some("current stack page"));
        kassert_eq!(clean, None);
        kassert_eq!(planted, [Some("kernel data"), Some("kernel image (boot mapping)"), Some("HHDM")]);
        kassert_eq!(restored, None);
        kassert_eq!(
            nothing.map(|m| (m.what, m.after)),
            Some((frame::KERNEL_IMAGE, WalkEnd::NotPresent(lumen_mm::paging::Level::L4)))
        );
        Ok(())
    }
);
