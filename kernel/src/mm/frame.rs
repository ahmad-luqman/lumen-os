//! The kernel's physical frame allocator (hard rule 2: never hand out memory
//! you don't own).
//!
//! The logic is in the host-tested `lumen-mm` crate; this module feeds it:
//!
//! 1. **Usable memory** is exactly the `usable` entries of Limine's memory map.
//!    Bootloader-reclaimable memory stays reserved for now (the boot stack,
//!    Limine's responses and its page tables, unused after the switch to the
//!    kernel's own, live there).
//! 2. **Reservations** are made explicitly, by name: frame 0, the kernel
//!    image, the kernel ELF file (the crash reporter's symbols), the boot stack
//!    and the framebuffer (those four found by translating their live virtual
//!    addresses through the page tables rather than trusting the map's
//!    labels), the ACPI RSDP (Limine's physical address) and the allocator's
//!    own bitmaps. On top of that every non-usable map entry is reserved (in
//!    case entries overlap) and so is every frame of Limine's page tables
//!    (active while the allocator is built; the kernel's own tables are then
//!    allocated from it).
//! 3. **Memory found later** (an ACPI table in usable memory) is reserved by
//!    name with [`reserve`], which refuses frames that are allocated or hold
//!    something else.
//!
//! The named list is printed at `debug` level (`lumen.log=debug`). The
//! in-kernel tests check, independently of that list, that no frame behind
//! a range the kernel is using (`mm::live_ranges`) is allocatable and no
//! page-table frame is free; then allocate every frame and check each one
//! against the memory map and the list.

use limine::memory_map::{Entry, EntryType};
use log::{debug, info, warn};
use lumen_mm::{FRAME_SIZE, FrameAllocator, FreeError, PhysFrame, PhysRange, VirtAddr, align_down};

use super::{kernel_image, pages, paging};
use crate::arch::{self, paging as hw};
use crate::boot;
use crate::sync::SpinLock;

const MAX_RESERVATIONS: usize = 16;
/// Size of an ACPI 2.0+ RSDP structure.
const RSDP_SIZE: u64 = 36;
// Reservation names (logged, and looked up by the ktests).
const NULL_FRAME: &str = "null frame";
pub const KERNEL_IMAGE: &str = "kernel image";
pub const KERNEL_FILE: &str = "kernel file";
pub const BOOT_STACK: &str = "boot stack";
pub const FRAMEBUFFER: &str = "framebuffer";
/// ACPI tables (and the RSDP) in usable memory: reserved at boot for the
/// RSDP, later by `acpi` for each table it reads there.
pub const ACPI_TABLE: &str = "acpi table";
pub const BITMAP: &str = "frame bitmap";
const NOTHING_ALLOCATED: &str = "nothing is allocated during init";

/// A named physical range the allocator must never hand out.
#[derive(Clone, Copy, Debug)]
struct Reservation {
    name: &'static str,
    range: PhysRange,
}

/// A fixed-capacity list (the frame allocator is built before the heap,
/// which depends on it).
#[derive(Clone, Copy)]
struct Reservations {
    items: [Reservation; MAX_RESERVATIONS],
    len: usize,
}

impl Reservations {
    const fn new() -> Self {
        Self { items: [Reservation { name: "", range: PhysRange::new(0, 0) }; MAX_RESERVATIONS], len: 0 }
    }

    fn push(&mut self, name: &'static str, range: PhysRange) {
        assert!(self.len < MAX_RESERVATIONS, "too many named frame reservations");
        assert!(!range.is_empty(), "reservation '{name}' is empty: it would protect nothing");
        self.items[self.len] = Reservation { name, range };
        self.len += 1;
    }

    fn as_slice(&self) -> &[Reservation] {
        &self.items[..self.len]
    }

    fn get(&self, name: &str) -> Option<PhysRange> {
        self.as_slice().iter().find(|r| r.name == name).map(|r| r.range)
    }
}

struct State {
    frames: FrameAllocator<'static>,
    reserved: Reservations,
}

static STATE: SpinLock<Option<State>> = SpinLock::new(None);

/// The framebuffer as `(virtual address, length)`, if Limine gave one.
pub fn framebuffer_virt() -> Option<(u64, u64)> {
    let fb = boot::FRAMEBUFFER.get_response()?.framebuffers().next()?;
    Some((fb.addr() as u64, fb.pitch() * fb.height()))
}

/// The physical address `virt` maps to in the active page tables, or how
/// the walk ended (not present, or a table outside mapped RAM).
fn translate(virt: u64) -> Result<u64, paging::WalkEnd> {
    let v = VirtAddr::new(virt).expect("live addresses are canonical");
    let w = paging::walk(v);
    w.mapping().and_then(|m| m.phys_of(v)).map(|p| p.as_u64()).ok_or(w.end())
}

/// The physical range behind `[virt, virt + len)`, which must be mapped and
/// physically contiguous (true for the HHDM and for the kernel image Limine
/// loads). Panics otherwise: guessing would risk handing out live memory.
fn phys_range_of(what: &str, virt: u64, len: u64) -> PhysRange {
    let translate = |v: u64| translate(v).unwrap_or_else(|end| panic!("{what}: virtual address {v:#x}: {end:?}"));
    let start = translate(virt);
    let (first_page, first_frame) = (align_down(virt, FRAME_SIZE), align_down(start, FRAME_SIZE));
    for page in pages(virt, len) {
        assert_eq!(
            translate(page),
            first_frame + (page - first_page),
            "{what}: not physically contiguous at {page:#x}"
        );
    }
    PhysRange::from_len(start, len)
}

fn entry_range(e: &Entry) -> PhysRange {
    PhysRange::from_len(e.base, e.length)
}

fn usable(entries: &[&Entry]) -> impl Iterator<Item = PhysRange> + Clone {
    entries.iter().filter(|e| e.entry_type == EntryType::USABLE).map(|e| entry_range(e))
}

fn non_usable(entries: &[&Entry]) -> impl Iterator<Item = PhysRange> + Clone {
    entries.iter().filter(|e| e.entry_type != EntryType::USABLE).map(|e| entry_range(e))
}

/// Memory-map entry types that describe RAM (as opposed to MMIO windows).
fn is_ram(kind: EntryType) -> bool {
    [
        EntryType::USABLE,
        EntryType::BOOTLOADER_RECLAIMABLE,
        EntryType::EXECUTABLE_AND_MODULES,
        EntryType::ACPI_RECLAIMABLE,
        EntryType::ACPI_NVS,
    ]
    .contains(&kind)
}

/// A byte count for humans: `36 B`, `1316 KiB`, `12 MiB`.
struct Size(u64);

impl core::fmt::Display for Size {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            n if n < 1024 => write!(f, "{n} B"),
            n if n < 10 * 1024 * 1024 => write!(f, "{} KiB", n.div_ceil(1024)),
            n => write!(f, "{} MiB", n.div_ceil(1024 * 1024)),
        }
    }
}

/// Everything the kernel uses that must never be handed out, by name. The
/// bitmaps are added by `init` once they are placed.
fn named_reservations(hhdm: u64) -> Reservations {
    let mut r = Reservations::new();
    r.push(NULL_FRAME, PhysRange::new(0, FRAME_SIZE));

    let (kstart, klen) = kernel_image();
    let image = phys_range_of(KERNEL_IMAGE, kstart, klen);
    match boot::EXECUTABLE_ADDRESS.get_response() {
        // Two sources disagreeing means one of them is wrong about where the
        // kernel lives: don't guess which.
        Some(addr) => {
            assert_eq!(addr.physical_base(), image.start(), "Limine and the page tables disagree on the kernel")
        }
        None => warn!(target: "mem", "no executable address response; kernel location not cross-checked"),
    }
    r.push(KERNEL_IMAGE, image);

    match boot::EXECUTABLE_FILE.get_response() {
        Some(resp) => {
            let file = resp.file();
            r.push(KERNEL_FILE, phys_range_of(KERNEL_FILE, file.addr() as u64, file.size()));
        }
        // Not used then (symbols aren't loaded); its memory is non-usable.
        None => debug!(target: "mem", "no kernel file response; nothing to reserve"),
    }

    let (bottom, top) = arch::stacks::boot_stack();
    r.push(BOOT_STACK, phys_range_of(BOOT_STACK, bottom, top - bottom));

    match framebuffer_virt() {
        Some((virt, len)) => r.push(FRAMEBUFFER, phys_range_of(FRAMEBUFFER, virt, len)),
        None => debug!(target: "mem", "no framebuffer; nothing to reserve"),
    }

    // The ACPI tables are normally in `acpi-*` map entries, which are
    // reserved with every other non-usable entry. `acpi` reserves any table
    // (or header) it reads in usable memory with `reserve`, when it reads it.
    match boot::RSDP.get_response() {
        Some(resp) => {
            // Physical at base revision 3 only (kmain refuses to boot under
            // any other); other revisions give an HHDM pointer.
            let addr = resp.address() as u64;
            assert!(addr < hhdm, "RSDP address {addr:#x} is not physical; Limine base revision changed?");
            r.push(ACPI_TABLE, PhysRange::from_len(addr, RSDP_SIZE));
        }
        None => warn!(target: "mem", "no ACPI RSDP from Limine"),
    }
    r
}

/// Build the frame allocator. Called once, early in boot, before anything
/// allocates.
pub fn init(hhdm: u64) {
    assert!(STATE.lock().is_none(), "frame allocator initialized twice");
    assert!(hw::four_level(), "5-level paging is on; the page-table walks assume 4 levels");
    let entries = boot::MEMMAP.get_response().expect("Limine gave no memory map").entries();
    let mut reserved = named_reservations(hhdm);

    // The bitmaps cover the span from the lowest to the highest usable frame,
    // and must avoid every reservation and every non-usable entry (in case
    // the map's entries overlap).
    let span = usable(entries)
        .reduce(|a, b| PhysRange::new(a.start().min(b.start()), a.end().max(b.end())))
        .expect("memory map has no usable memory");
    let bytes = lumen_mm::storage_bytes(span);
    let avoid = reserved.as_slice().iter().map(|r| r.range).chain(non_usable(entries));
    let start =
        lumen_mm::find_hole(usable(entries), avoid, bytes).expect("no usable memory large enough for the frame bitmap");
    let bitmap = PhysRange::from_len(start, bytes);
    // Limine allocates its page tables from bootloader-reclaimable memory, so
    // none should be in a usable hole. Check before zeroing the bitmap over one.
    paging::for_each_active_table(|table| {
        assert!(!bitmap.contains(table.as_u64()), "frame bitmap would overwrite page table {table}");
    })
    .unwrap_or_else(|t| panic!("boot page table {t} is outside HHDM-mapped RAM; can't check its subtree"));

    // SAFETY: `bitmap` lies in a usable map entry, which the HHDM maps
    // writable, and is frame-aligned (so `u64`-aligned). Limine guarantees
    // usable memory is unused and the kernel has touched none of it before
    // this point; it overlaps no reservation or non-usable entry (find_hole)
    // and no page table (checked above). `init` runs once (asserted above),
    // so this is the only reference. It is reserved below and never freed,
    // so `'static` holds.
    let storage = unsafe { core::slice::from_raw_parts_mut((hhdm + start) as *mut u64, lumen_mm::storage_words(span)) };
    let mut frames =
        FrameAllocator::new(span, storage, usable(entries)).expect("bitmap storage is sized by storage_words");

    reserved.push(BITMAP, bitmap);
    for r in reserved.as_slice() {
        let taken = frames.reserve(r.range).expect(NOTHING_ALLOCATED);
        // Live memory inside a `usable` entry means the map's labels are
        // wrong: worth knowing, though the reservation already protects it.
        if taken > 0 && ![NULL_FRAME, BITMAP].contains(&r.name) {
            warn!(target: "mem", "{} {} was in usable memory", r.name, r.range);
        }
    }
    for range in non_usable(entries) {
        frames.reserve(range).expect(NOTHING_ALLOCATED);
    }
    let tables = paging::for_each_active_table(|table| {
        frames.reserve(PhysRange::from_len(table.as_u64(), FRAME_SIZE)).expect(NOTHING_ALLOCATED);
    })
    .unwrap_or_else(|t| panic!("boot page table {t} is outside HHDM-mapped RAM; can't reserve its subtree"));

    for r in reserved.as_slice() {
        debug!(target: "mem", "reservation {} {} ({})", r.range, r.name, Size(r.range.len()));
    }
    debug!(target: "mem", "reservation: {tables} page-table references, and every non-usable map entry");
    // "reserved" = frames in RAM-typed entries (usable, reclaimable, kernel,
    // ACPI) that can't be allocated. `reserved`-typed entries are left out:
    // most are MMIO windows (one is 12 GiB on q35), a few firmware RAM.
    let ram: u64 = entries.iter().filter(|e| is_ram(e.entry_type)).map(|e| e.length / FRAME_SIZE).sum();
    let free = frames.free_frames() as u64;
    let held = ram.checked_sub(free).expect("allocator owns more frames than the map has RAM");
    info!(target: "mem", "frames: {free} usable, {held} reserved");

    *STATE.lock() = Some(State { frames, reserved });
}

fn with_frames<R>(f: impl FnOnce(&mut FrameAllocator<'static>) -> R) -> R {
    f(&mut STATE.lock().as_mut().expect("frame allocator used before mm::init").frames)
}

fn frame(addr: u64) -> PhysFrame {
    PhysFrame::from_u64(addr).expect("the allocator hands out aligned physical frames")
}

/// Allocate one 4 KiB frame, or `None` when physical memory is exhausted.
/// Its contents are whatever was there before (see [`alloc_zeroed`]).
/// Return it with [`free`] (`unsafe`), or use [`alloc_owned`], which frees
/// on drop.
#[must_use = "dropping the frame leaks it"]
pub fn alloc() -> Option<PhysFrame> {
    with_frames(|f| f.alloc()).map(frame)
}

/// Allocate one 4 KiB frame filled with zeros (page tables need this).
#[must_use = "dropping the frame leaks it"]
pub fn alloc_zeroed() -> Option<PhysFrame> {
    let f = alloc()?;
    // SAFETY: the allocator just handed us this frame, so nothing else uses
    // it, and it is usable RAM, which the HHDM maps.
    unsafe { core::ptr::write_bytes(super::phys_to_virt(f.start()).as_mut_ptr::<u8>(), 0, FRAME_SIZE as usize) };
    Some(f)
}

/// Allocate `count` contiguous frames aligned to `align_frames` frames (a
/// power of two); returns the first, or `None` if no such run is free.
/// Panics if `count` is 0 or `align_frames` is not a power of two (caller
/// bugs). Return the run with [`free_contiguous`] and the same count.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: DMA buffers
#[must_use = "dropping the frames leaks them"]
pub fn alloc_contiguous(count: usize, align_frames: usize) -> Option<PhysFrame> {
    with_frames(|f| f.alloc_contiguous(count, align_frames)).map(frame)
}

/// Return a frame from [`alloc`] or [`alloc_zeroed`].
///
/// # Safety
/// The caller owns `f` (it allocated it, or was handed it) and nothing uses
/// it any more; in particular no other [`OwnedFrame`] holds it, and no
/// `paging::map` mapping does (an owned frame is what lets `map` be safe).
pub unsafe fn free(f: PhysFrame) -> Result<(), FreeError> {
    with_frames(|a| a.free(f.as_u64()))
}

/// Return `count` frames from [`alloc_contiguous`], all or nothing.
///
/// # Safety
/// As for [`free`], for every frame of the run.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: DMA buffers
pub unsafe fn free_contiguous(first: PhysFrame, count: usize) -> Result<(), FreeError> {
    with_frames(|f| f.free_contiguous(first.as_u64(), count))
}

/// An allocated frame that only its holder may use: move-only, and freed
/// when dropped. `paging::map` consumes one and `paging::unmap` gives it
/// back, which is half of what makes `map` safe (the other half is the
/// window token). Every frame also has an HHDM alias; only `unsafe` code can
/// reach it (including the `unsafe` ktest helpers).
#[derive(Debug)]
#[must_use = "dropping an owned frame frees it"]
pub struct OwnedFrame(PhysFrame);

impl OwnedFrame {
    /// The frame (to look at; holding the number grants nothing).
    pub fn frame(&self) -> PhysFrame {
        self.0
    }

    /// Take ownership of `f`.
    ///
    /// # Safety
    /// `f` is an allocated frame that nothing else owns or uses (no other
    /// `OwnedFrame`, no raw holder that may free it).
    pub unsafe fn from_raw(f: PhysFrame) -> OwnedFrame {
        OwnedFrame(f)
    }

    /// Give up ownership without freeing: the caller now holds `f` raw (for
    /// `paging::map`, whose mapping holds it until `unmap`).
    #[must_use = "discarding the raw frame leaks it"]
    pub fn into_raw(self) -> PhysFrame {
        let f = self.0;
        core::mem::forget(self);
        f
    }

    /// Free the frame now, reporting a failure (a kernel bug) to the caller,
    /// which can say what it was doing; dropping panics instead.
    pub fn free(self) -> Result<(), FreeError> {
        let f = self.into_raw();
        // SAFETY: as in `drop`.
        unsafe { free(f) }
    }
}

impl Drop for OwnedFrame {
    fn drop(&mut self) {
        // SAFETY: a live `OwnedFrame` is never mapped by `map` (`map` turns it
        // raw; `unmap` hands it back only after removing the mapping and
        // flushing the TLB), its HHDM alias is reached only by `unsafe` code,
        // and nothing else holds it, so nothing uses the frame.
        unsafe { free(self.0) }.unwrap_or_else(|e| panic!("mm: freeing owned frame {} failed: {e:?}", self.0));
    }
}

/// Allocate one 4 KiB frame as an [`OwnedFrame`] (freed when dropped).
#[must_use = "dropping the frame frees it again"]
pub fn alloc_owned() -> Option<OwnedFrame> {
    // SAFETY: just allocated, so nothing else owns or uses it.
    alloc().map(|f| unsafe { OwnedFrame::from_raw(f) })
}

/// Is `f` a frame the allocator has handed out (owned and not free)?
pub fn is_allocated(f: PhysFrame) -> bool {
    with_frames(|a| a.is_owned(f.as_u64()) && !a.is_free(f.as_u64()))
}

/// Why [`reserve`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReserveError {
    /// This frame is allocated: something may already be using it.
    InUse(u64),
    /// This frame is already reserved, for something else.
    ReservedFor { frame: u64, name: &'static str },
    /// This frame isn't usable memory the allocator manages.
    NotUsable(u64),
    /// The range needs a new named reservation and the list is full.
    ListFull,
}

impl core::fmt::Display for ReserveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ReserveError::InUse(a) => write!(f, "frame {a:#x} is already allocated"),
            ReserveError::ReservedFor { frame, name } => write!(f, "frame {frame:#x} is reserved for the {name}"),
            ReserveError::NotUsable(a) => write!(f, "frame {a:#x} is not usable memory"),
            ReserveError::ListFull => write!(f, "all {MAX_RESERVATIONS} named reservations are used"),
        }
    }
}

/// Reserve the usable memory `range` touches for `name`, for good: memory
/// found once the allocator is built, such as an ACPI table in usable
/// memory. Every frame must be free (it is taken now) or already reserved
/// for `name` (the same table read again). Anything else is refused,
/// changing nothing: an allocated frame (`InUse`), one reserved for
/// something else (`ReservedFor`), one the allocator doesn't manage
/// (`NotUsable`), or a range that takes frames but touches no `name`
/// reservation when the named list is full (`ListFull`). A range that takes
/// frames joins the named reservations (merged into a `name` reservation it
/// overlaps or touches), so the in-kernel test that allocates every frame
/// checks none of them is handed out. Returns how many frames it took.
pub fn reserve(name: &'static str, range: PhysRange) -> Result<usize, ReserveError> {
    let mut guard = STATE.lock();
    let s = guard.as_mut().expect("frame allocator used before mm::init");
    let frames = range.grow_to_frames();
    let mut takes_any = false;
    for f in (frames.start()..frames.end()).step_by(FRAME_SIZE as usize) {
        if s.frames.is_owned(f) {
            if !s.frames.is_free(f) {
                return Err(ReserveError::InUse(f));
            }
            takes_any = true;
            continue;
        }
        match s.reserved.as_slice().iter().find(|r| r.range.grow_to_frames().contains(f)) {
            Some(r) if r.name == name => {}
            Some(r) => return Err(ReserveError::ReservedFor { frame: f, name: r.name }),
            None => return Err(ReserveError::NotUsable(f)),
        }
    }
    if !takes_any {
        return Ok(0);
    }
    let merge = s.reserved.as_slice().iter().position(|r| {
        let held = r.range.grow_to_frames();
        r.name == name && held.start() <= frames.end() && frames.start() <= held.end()
    });
    if merge.is_none() && s.reserved.len == MAX_RESERVATIONS {
        return Err(ReserveError::ListFull);
    }
    let taken = s.frames.reserve(frames).expect("checked above: no frame of the range is allocated");
    match merge {
        Some(i) => {
            let r = &mut s.reserved.items[i].range;
            *r = PhysRange::new(r.start().min(frames.start()), r.end().max(frames.end()));
        }
        None => s.reserved.push(name, frames),
    }
    debug!(target: "mem", "reservation {frames} {name} ({taken} frames, found after the allocator was built)");
    Ok(taken)
}

/// Where the allocator's own bitmaps are, once it exists.
pub fn bitmap() -> Option<PhysRange> {
    STATE.lock().as_ref().and_then(|s| s.reserved.get(BITMAP))
}

/// How many frames are free (for the heap's tests).
#[cfg(feature = "ktest")]
pub use checks::free_frames;

#[cfg(feature = "ktest")]
mod checks {
    //! Helpers for the in-kernel tests below.

    use super::*;

    pub fn with_state<R>(f: impl FnOnce(&State) -> R) -> R {
        f(STATE.lock().as_ref().expect("frame allocator not initialized"))
    }

    pub fn is_owned(phys: u64) -> bool {
        with_state(|s| s.frames.is_owned(phys))
    }

    pub fn free_frames() -> usize {
        with_state(|s| s.frames.free_frames())
    }

    /// Is any frame free, by scanning the bitmap (not the counters or hint)?
    pub fn any_free() -> bool {
        with_state(|s| {
            let span = s.frames.span();
            pages(span.start(), span.len()).any(|f| s.frames.is_free(f))
        })
    }

    pub fn reservation(name: &str) -> Result<PhysRange, &'static str> {
        with_state(|s| s.reserved.get(name)).ok_or("named reservation missing")
    }

    pub fn hhdm() -> Result<u64, &'static str> {
        Ok(boot::HHDM.get_response().ok_or("no HHDM response")?.offset())
    }

    pub fn entries() -> Result<&'static [&'static Entry], &'static str> {
        Ok(boot::MEMMAP.get_response().ok_or("no memory map")?.entries())
    }

    /// FNV-1a: detects writes to memory the test must not touch.
    pub fn checksum(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
    }

    /// Kernel `.text` and `.rodata` up to the end of `.ktests` (the `.got`
    /// after it isn't covered): never written.
    pub fn kernel_readonly() -> &'static [u8] {
        unsafe extern "C" {
            static __ktests_end: u8;
        }
        let (start, _) = kernel_image();
        let len = &raw const __ktests_end as u64 - start;
        // SAFETY: the kernel's text and rodata segments are mapped and
        // immutable for the life of the kernel.
        unsafe { core::slice::from_raw_parts(start as *const u8, len as usize) }
    }

    pub fn kernel_file() -> Result<&'static [u8], &'static str> {
        let resp = boot::EXECUTABLE_FILE.get_response().ok_or("no kernel file response")?;
        Ok(crate::file_bytes(resp.file()))
    }

    /// The first frame the kernel is using that may be handed out: a frame
    /// behind a live range ([`live_ranges`], found through the page tables,
    /// not the reservation list) that `is_owned` says the allocator owns (or,
    /// for an allocated stack, that `is_free` says is free), or a page-table
    /// frame that `is_free` says is free. (The boot tables' frames are
    /// reserved; the kernel's own tables and stacks are allocated, so owned
    /// but never free.)
    pub fn first_live_owned(
        is_owned: impl Fn(u64) -> bool,
        is_free: impl Fn(u64) -> bool,
    ) -> Result<Option<(&'static str, u64)>, &'static str> {
        for super::super::LiveRange { name, range, allocated, .. } in super::super::live_ranges() {
            // Every live range is known in the test configuration (Limine
            // gives every response, and the cmdline is never empty), so no
            // check is silently skipped.
            let Some((virt, len)) = range else {
                crate::ktest::diag(format_args!("live range '{name}' unknown"));
                return Err("a live range is unknown (missing Limine response?)");
            };
            for page in pages(virt, len) {
                let phys = translate(page).map_err(|end| {
                    crate::ktest::diag(format_args!("{name}: {page:#x}: {end:?}"));
                    "a live page is not mapped"
                })?;
                // Memory the kernel uses must never be free: reserved memory
                // is never even owned; an allocated stack is owned, not free.
                let handed_out = if allocated { is_free(phys) } else { is_owned(phys) };
                if handed_out {
                    return Ok(Some((name, phys)));
                }
            }
        }
        let mut table = None;
        paging::for_each_active_table(|t| {
            if table.is_none() && is_free(t.as_u64()) {
                table = Some(("page table", t.as_u64()));
            }
        })
        .map_err(|_| "a page table is unreadable")?;
        Ok(table)
    }

    pub fn is_free(phys: u64) -> bool {
        with_state(|s| s.frames.is_free(phys))
    }

    /// [`alloc`] as a raw address (the chain below works in addresses).
    pub fn alloc_raw() -> Option<u64> {
        alloc().map(PhysFrame::as_u64)
    }

    /// Free any address, including invalid ones, to test the allocator's
    /// checks.
    ///
    /// # Safety
    /// As for [`free`](super::free), if `addr` is a frame the allocator
    /// handed out.
    pub unsafe fn free_raw(addr: u64) -> Result<(), FreeError> {
        with_frames(|a| a.free(addr))
    }

    pub fn virt(phys: u64) -> *mut u8 {
        crate::mm::phys_to_virt(lumen_mm::PhysAddr::new(phys).expect("a physical address")).as_mut_ptr()
    }

    pub const FILL: u8 = 0x5a;

    /// Why an allocated frame is wrong, if it is.
    pub fn bad_frame(frame: u64, entries: &[&Entry], reserved: &[Reservation]) -> Option<&'static str> {
        let range = PhysRange::from_len(frame, FRAME_SIZE);
        if !frame.is_multiple_of(FRAME_SIZE) {
            return Some("frame address not aligned");
        }
        if !usable(entries).any(|u| u.start() <= range.start() && range.end() <= u.end()) {
            return Some("frame outside every usable map entry");
        }
        if non_usable(entries).any(|r| r.overlaps(&range)) {
            return Some("frame overlaps a non-usable map entry");
        }
        if let Some(r) = reserved.iter().find(|r| r.range.overlaps(&range)) {
            crate::ktest::diag(format_args!("frame {frame:#x} is inside reservation '{}' {}", r.name, r.range));
            return Some("frame overlaps a named reservation");
        }
        None
    }

    /// Every allocated frame, chained through its own memory.
    pub struct Chain {
        pub head: u64,
        pub count: usize,
        pub failure: Option<&'static str>,
        /// After the last `alloc`, the bitmap held no free frame.
        pub none_left: bool,
    }

    /// Allocate frames until none is left. Each is checked, then filled with
    /// [`FILL`] except for three words: the previous frame (0 ends the chain:
    /// frame 0 is never handed out), a sequence number and its own address.
    /// Stops at the first bad frame (after freeing it) with `failure` set.
    /// Always returns the chain built so far, which the caller must free.
    pub fn allocate_all(entries: &[&Entry], reserved: &[Reservation]) -> Chain {
        let mut chain = Chain { head: 0, count: 0, failure: None, none_left: false };
        while let Some(frame) = alloc_raw() {
            if let Some(why) = bad_frame(frame, entries, reserved) {
                crate::ktest::diag(format_args!("frame {frame:#x}: {why}"));
                chain.failure = Some(why);
                // SAFETY: a frame this test just allocated and no longer uses.
                if let Err(e) = unsafe { free_raw(frame) } {
                    crate::ktest::diag(format_args!("free({frame:#x}): {e}"));
                }
                return chain;
            }
            let p = virt(frame);
            let w = p.cast::<u64>();
            // SAFETY: the allocator just handed us this frame and it passed
            // the checks above, so nothing else uses it; the HHDM maps it.
            unsafe {
                core::ptr::write_bytes(p, FILL, FRAME_SIZE as usize);
                w.write(chain.head);
                w.add(1).write(chain.count as u64);
                w.add(2).write(frame);
            }
            chain.head = frame;
            chain.count += 1;
        }
        chain.none_left = !any_free();
        chain
    }

    /// Is `frame` an allocated frame that still carries `allocate_all`'s
    /// self-tag? Only such a frame may be read or freed by the test.
    fn ours(frame: u64) -> bool {
        let allocated = with_state(|s| s.frames.is_owned(frame) && !s.frames.is_free(frame));
        // SAFETY: reading one word of an allocated frame through the HHDM:
        // it is either ours (filled by allocate_all), leaked by an earlier
        // failed test, or in use elsewhere (heap, stacks, page tables), and
        // nothing writes it meanwhile (this test runs alone; other CPUs only
        // idle).
        allocated && unsafe { virt(frame).cast::<u64>().add(2).read() } == frame
    }

    /// Free every frame of the chain, newest first, checking each one's link,
    /// sequence number (counting down from `count - 1`), self-tag and last
    /// byte. A frame handed out twice (its second allocation overwrote the
    /// first) or a stray write to those words is reported. The walk stops at
    /// the first problem, since the links can't be trusted after it; the rest
    /// of the frames are then found by sweeping the bitmap for allocated
    /// frames carrying their self-tag, so a failure doesn't leak memory into
    /// later tests. Returns `(frames freed, first problem)`.
    ///
    /// # Safety
    /// `chain` came from [`allocate_all`] and nothing uses its frames any
    /// more (nor any other allocated frame carrying a chain tag).
    pub unsafe fn free_chain(chain: &Chain) -> (usize, Option<&'static str>) {
        let (mut head, mut freed, mut problem) = (chain.head, 0, None);
        while head != 0 && problem.is_none() {
            if !ours(head) {
                crate::ktest::diag(format_args!("chain link {head:#x} is not an allocated, tagged frame"));
                problem = Some("the chain of allocated frames is broken");
                break;
            }
            let p = virt(head);
            let w = p.cast::<u64>();
            // SAFETY: `ours` checked `head` is an allocated frame we tagged.
            let (next, seq, last) = unsafe { (w.read(), w.add(1).read(), p.add(FRAME_SIZE as usize - 1).read()) };
            let expected_seq = chain.count.saturating_sub(freed + 1) as u64;
            if (seq, last) != (expected_seq, FILL) {
                crate::ktest::diag(format_args!(
                    "frame {head:#x}: seq {seq} (want {expected_seq}), last byte {last:#x}"
                ));
                problem = Some("an allocated frame was overwritten (handed out twice, or another writer)");
            }
            // SAFETY: each chained frame was allocated by this test, which is done with it.
            match unsafe { free_raw(head) } {
                Ok(()) => freed += 1,
                Err(e) => {
                    crate::ktest::diag(format_args!("free({head:#x}): {e}"));
                    problem = problem.or(Some("freeing an allocated frame failed"));
                }
            }
            head = next;
        }
        if problem.is_some() {
            let span = with_state(|s| s.frames.span());
            // SAFETY: sweeping frames this test allocated (owned and not free), done with them.
            let swept = pages(span.start(), span.len()).filter(|&f| ours(f) && unsafe { free_raw(f) }.is_ok());
            let swept = swept.count();
            crate::ktest::diag(format_args!("freed {swept} more frames by sweeping the bitmap"));
            freed += swept;
        }
        (freed, problem)
    }
}

crate::ktest!(
    fn never_returns_reserved() {
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        let entries = entries()?;
        let reserved = with_state(|s| s.reserved);

        // 1. Through the page tables, not the reservation list (except for
        //    where the bitmap is): no frame behind a live range
        //    (`mm::live_ranges`) is owned by the allocator, and no page-table
        //    frame is free.
        if let Some((name, phys)) = first_live_owned(is_owned, is_free)? {
            crate::ktest::diag(format_args!("{name}: frame {phys:#x} is allocatable"));
            return Err("a frame the kernel is using is allocatable");
        }
        let bitmap = reservation(BITMAP)?;
        kassert!(pages(bitmap.start(), bitmap.len()).all(|p| !is_owned(p)));

        // 2. Allocate every frame, checking each against the memory map and
        //    the named list; 3. free them all, checking their tags.
        let before = free_frames();
        let readonly_sum = checksum(kernel_readonly());
        let file_sum = checksum(kernel_file()?);
        let chain = allocate_all(entries, reserved.as_slice());
        // SAFETY: this test allocated the chain and is done with it.
        let (freed, problem) = unsafe { free_chain(&chain) };

        if let Some(why) = chain.failure.or(problem) {
            return Err(why);
        }
        kassert!(chain.none_left);
        kassert_eq!(chain.count, before);
        kassert_eq!(freed, chain.count);
        kassert_eq!(free_frames(), before);
        kassert_eq!(checksum(kernel_readonly()), readonly_sum);
        kassert_eq!(checksum(kernel_file()?), file_sum);
        crate::ktest::diag(format_args!("allocated and freed all {} frames", chain.count));
        Ok(())
    }
);

crate::ktest!(
    fn live_memory_oracle_detects_a_leak() {
        // The check in never_returns_reserved must be able to fail: pretend
        // one frame in use is allocatable, and it must be named.
        use self::checks::*;
        use crate::kassert_eq;
        let (kstart, _) = kernel_image();
        let (_, stack_top) = arch::stacks::boot_stack();
        let image = translate(kstart).map_err(|_| "kernel not mapped")?;
        let stack = translate(align_down(stack_top - 1, FRAME_SIZE)).map_err(|_| "stack not mapped")?;
        let bitmap = reservation(BITMAP)?.start();
        let mut table = 0;
        paging::for_each_active_table(|t| table = t.as_u64()).map_err(|_| "a page table is unreadable")?;
        let never = |_: u64| false;
        kassert_eq!(first_live_owned(|p| p == image, never)?, Some((KERNEL_IMAGE, image)));
        kassert_eq!(first_live_owned(|p| p == stack, never)?, Some((BOOT_STACK, stack)));
        kassert_eq!(first_live_owned(|p| p == bitmap, never)?, Some((BITMAP, bitmap)));
        kassert_eq!(first_live_owned(never, |p| p == table)?, Some(("page table", table)));
        // The running stack is allocated: owned is fine; free would mean it
        // could be handed out while in use.
        let running = translate(align_down(arch::read_rsp(), FRAME_SIZE)).map_err(|_| "stack not mapped")?;
        kassert_eq!(first_live_owned(|p| p == running, never)?, None);
        kassert_eq!(first_live_owned(never, |p| p == running)?, Some(("current stack page", running)));
        kassert_eq!(first_live_owned(never, never)?, None);
        Ok(())
    }
);

crate::ktest!(
    fn bad_frame_detects_reserved_frames() {
        // The per-frame check in never_returns_reserved must be able to fail.
        use self::checks::*;
        use crate::kassert_eq;
        let entries = entries()?;
        let reserved = with_state(|s| s.reserved);
        let reserved = reserved.as_slice();
        // The bitmap is a named reservation in usable memory.
        kassert_eq!(
            bad_frame(reservation(BITMAP)?.start(), entries, reserved),
            Some("frame overlaps a named reservation")
        );
        kassert_eq!(bad_frame(0, entries, reserved).is_some(), true);
        kassert_eq!(bad_frame(reservation(KERNEL_IMAGE)?.start(), entries, reserved).is_some(), true);
        let frame = alloc_raw().ok_or("out of frames")?;
        let (unaligned, good) = (bad_frame(frame + 1, entries, reserved), bad_frame(frame, entries, reserved));
        // SAFETY: allocated just above by this test and not used since.
        kassert_eq!(unsafe { free_raw(frame) }, Ok(()));
        kassert_eq!(unaligned, Some("frame address not aligned"));
        kassert_eq!(good, None);
        Ok(())
    }
);

crate::ktest!(
    fn named_reservations_are_complete() {
        use self::checks::*;
        use crate::kassert_eq;
        for name in [NULL_FRAME, KERNEL_IMAGE, KERNEL_FILE, BOOT_STACK, FRAMEBUFFER, ACPI_TABLE, BITMAP] {
            if reservation(name).is_err() {
                crate::ktest::diag(format_args!("missing reservation '{name}'"));
                return Err("a named reservation is missing");
            }
        }
        let limine = boot::EXECUTABLE_ADDRESS.get_response().ok_or("no executable address response")?;
        kassert_eq!(reservation(KERNEL_IMAGE)?.start(), limine.physical_base());
        Ok(())
    }
);

crate::ktest!(
    fn reserve_takes_free_frames_and_refuses_the_rest() {
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        const NAME: &str = "ktest reservation";
        let named = || with_state(|s| s.reserved.len);
        // Three neighbouring frames: hold the last, free the others.
        let first = alloc_contiguous(3, 1).ok_or("out of frames")?;
        let (a, b, c) = (first.as_u64(), first.as_u64() + FRAME_SIZE, first.as_u64() + 2 * FRAME_SIZE);
        let held = PhysFrame::from_u64(c).ok_or("frame")?;
        // SAFETY: allocated just above by this test and not used since.
        unsafe { kassert!(free_raw(a).is_ok() && free_raw(b).is_ok()) };
        let refused = reserve(NAME, PhysRange::new(b + 8, c + 8));
        let still = (with_state(|s| s.frames.is_free(b)), is_allocated(held));
        // SAFETY: allocated above by this test and not used since.
        kassert_eq!(unsafe { free_raw(c) }, Ok(()));
        kassert_eq!(refused, Err(ReserveError::InUse(c)));
        kassert_eq!(still, (true, true)); // refusing changed nothing
        // Now all three are free. `b` is taken and named...
        let (before, names) = (free_frames(), named());
        kassert_eq!(reserve(NAME, PhysRange::from_len(b + 8, 16)), Ok(1));
        kassert_eq!((free_frames(), named()), (before - 1, names + 1));
        kassert!(!is_owned(b));
        // ...then `a` with `b` (merging downwards), then `b` with `c` (a
        // range part reserved under the name, part free, as when a table's
        // header and then its whole length are read), then all: nothing.
        kassert_eq!(reserve(NAME, PhysRange::new(a, b + 8)), Ok(1));
        kassert_eq!(reserve(NAME, PhysRange::new(b, c + 8)), Ok(1));
        kassert_eq!(reserve(NAME, PhysRange::new(a, c + FRAME_SIZE)), Ok(0));
        kassert_eq!((free_frames(), named()), (before - 3, names + 1));
        kassert_eq!(reservation(NAME)?, PhysRange::new(a, c + FRAME_SIZE));
        kassert!(!is_owned(a) && !is_owned(c));
        // An empty range takes nothing, wherever it is.
        kassert_eq!(reserve(NAME, PhysRange::new(a + 8, a + 8)), Ok(0));
        // Someone else's frames, and memory the allocator never had (the
        // local APIC's registers), are refused.
        let bitmap = reservation(BITMAP)?.start();
        kassert_eq!(
            reserve(NAME, PhysRange::from_len(bitmap, 1)),
            Err(ReserveError::ReservedFor { frame: bitmap, name: BITMAP })
        );
        kassert_eq!(reserve(NAME, PhysRange::from_len(0xfee0_0000, 4)), Err(ReserveError::NotUsable(0xfee0_0000)));
        kassert_eq!((free_frames(), named()), (before - 3, names + 1));
        Ok(())
    }
);

crate::ktest!(
    fn the_rsdp_is_reserved_as_an_acpi_table() {
        // `acpi` reads tables in usable memory under the same name, so an
        // RSDP there (or a table sharing its frame) is not refused.
        use self::checks::*;
        use crate::kassert_eq;
        let rsdp = reservation(ACPI_TABLE)?;
        let names = with_state(|s| s.reserved.len);
        kassert_eq!(reserve(ACPI_TABLE, rsdp), Ok(0));
        kassert_eq!(with_state(|s| s.reserved.len), names);
        Ok(())
    }
);

crate::ktest!(
    fn translate_agrees_with_the_hhdm() {
        // The reservations and the oracle share the page-table walker; check
        // it against the HHDM's definition: `p` at `hhdm + p`. (The HHDM is
        // built by the same lumen_mm mapper, so this checks walk against
        // map's offsets; the CPU's view is checked by never_returns_reserved
        // writing every frame through the HHDM. 0x1000 is usable low memory
        // on OVMF.)
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        kassert!(hw::four_level());
        let hhdm = hhdm()?;
        let bitmap = reservation(BITMAP)?.start();
        let frame = alloc_raw().ok_or("out of frames")?;
        let got = [frame, frame + 0x123, bitmap, 0x1000].map(|p| (translate(hhdm + p).ok(), p));
        // SAFETY: allocated just above by this test and not used since.
        kassert_eq!(unsafe { free_raw(frame) }, Ok(()));
        for (translated, p) in got {
            kassert_eq!(translated, Some(p));
        }
        Ok(())
    }
);

crate::ktest!(
    fn alloc_free_reuse() {
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        let before = free_frames();
        let a = alloc().ok_or("out of frames")?;
        // SAFETY: allocated above by this test and not used since.
        kassert_eq!(unsafe { free(a) }, Ok(()));
        let b = alloc().ok_or("out of frames")?;
        // SAFETY: allocated above by this test and not used since.
        kassert_eq!(unsafe { free(b) }, Ok(()));
        kassert_eq!(b, a);
        // SAFETY: `a` is free again (it is `b`, freed just above, and nothing
        // allocated since), so `free` only reads the bitmap and refuses.
        kassert_eq!(unsafe { free(a) }, Err(FreeError::DoubleFree));

        let kernel = reservation(KERNEL_IMAGE)?.start();
        // SAFETY: refused (asserted): not a frame the allocator handed out.
        kassert!(matches!(unsafe { free_raw(kernel) }, Err(FreeError::NotOwned | FreeError::OutOfRange)));
        // SAFETY: refused (asserted): not a frame the allocator handed out.
        kassert!(unsafe { free_raw(0) }.is_err());
        // SAFETY: refused (asserted): misaligned.
        kassert!(unsafe { free_raw(a.as_u64() + 8) }.is_err());

        let run = alloc_contiguous(4, 4).ok_or("no 4-frame run")?;
        // SAFETY: the run allocated just above, not used.
        kassert_eq!(unsafe { free_contiguous(run, 4) }, Ok(()));
        kassert!(run.as_u64().is_multiple_of(4 * FRAME_SIZE));

        // alloc_zeroed zeroes a frame that held data.
        let dirty = alloc().ok_or("out of frames")?;
        // SAFETY: the test owns `dirty`; the HHDM maps it.
        unsafe { core::ptr::write_bytes(virt(dirty.as_u64()), 0xee, FRAME_SIZE as usize) };
        // SAFETY: allocated above by this test, which is done with it.
        kassert_eq!(unsafe { free(dirty) }, Ok(()));
        let zeroed = alloc_zeroed().ok_or("out of frames")?;
        // SAFETY: as above, for `zeroed`.
        let all_zero =
            unsafe { core::slice::from_raw_parts(virt(zeroed.as_u64()), FRAME_SIZE as usize) }.iter().all(|&b| b == 0);
        // SAFETY: allocated above by this test, which is done with it.
        kassert_eq!(unsafe { free(zeroed) }, Ok(()));
        kassert_eq!(zeroed, dirty); // the lowest free frame, so the same one
        kassert!(all_zero);
        kassert_eq!(free_frames(), before);
        Ok(())
    }
);
