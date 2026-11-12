//! The kernel's physical frame allocator (hard rule 2: never hand out memory
//! you don't own).
//!
//! The logic is in the host-tested `lumen-mm` crate; this module feeds it:
//!
//! 1. **Usable memory** is exactly the `usable` entries of Limine's memory map.
//!    Bootloader-reclaimable memory stays reserved for now (Limine's page
//!    tables, the boot stack and the kernel file live there).
//! 2. **Reservations** are made explicitly, by name: frame 0, the kernel
//!    image, the kernel ELF file (the crash reporter's symbols), the boot stack
//!    and the framebuffer (those four found by translating their live virtual
//!    addresses through the page tables rather than trusting the map's
//!    labels), the ACPI RSDP (Limine's physical address) and the allocator's
//!    own bitmaps. On top of that every non-usable map entry is reserved (in
//!    case entries overlap) and so is every frame of the active page tables.
//!
//! The named list is printed at `debug` level (`lumen.log=debug`). The
//! in-kernel tests check, independently of that list, that nothing behind a
//! live mapping is allocatable; then allocate every frame and check each one
//! against the memory map and the list.

use limine::memory_map::{Entry, EntryType};
use log::{debug, info, warn};
use lumen_mm::{FRAME_SIZE, FrameAllocator, FreeError, PhysRange, align_down};

use crate::arch::{self, paging};
use crate::boot;
use crate::sync::SpinLock;

const MAX_RESERVATIONS: usize = 16;
/// Size of an ACPI 2.0+ RSDP structure.
const RSDP_SIZE: u64 = 36;
const NULL_FRAME: &str = "null frame";
const BITMAP: &str = "frame bitmap";

/// A named physical range the allocator must never hand out.
#[derive(Clone, Copy, Debug)]
struct Reservation {
    name: &'static str,
    range: PhysRange,
}

/// A fixed-capacity list (there is no heap yet).
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
        self.items[self.len] = Reservation { name, range };
        self.len += 1;
    }

    fn as_slice(&self) -> &[Reservation] {
        &self.items[..self.len]
    }
}

struct State {
    frames: FrameAllocator<'static>,
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // read by ktests; X-ray (M5) will show it
    reserved: Reservations,
}

static STATE: SpinLock<Option<State>> = SpinLock::new(None);

unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

fn kernel_image_virt() -> (u64, u64) {
    let start = &raw const __kernel_start as u64;
    (start, &raw const __kernel_end as u64 - start)
}

/// The framebuffer as `(virtual address, length)`, if Limine gave one.
fn framebuffer_virt() -> Option<(u64, u64)> {
    let fb = boot::FRAMEBUFFER.get_response()?.framebuffers().next()?;
    Some((fb.addr() as u64, fb.pitch() * fb.height()))
}

/// The virtual page addresses touched by `[virt, virt + len)`.
fn pages(virt: u64, len: u64) -> impl Iterator<Item = u64> {
    (align_down(virt, FRAME_SIZE)..virt + len).step_by(FRAME_SIZE as usize)
}

/// The physical range behind `[virt, virt + len)`, which must be mapped and
/// physically contiguous (true for the HHDM and for the kernel image Limine
/// loads). Panics otherwise: guessing would risk handing out live memory.
fn phys_range_of(hhdm: u64, what: &str, virt: u64, len: u64) -> PhysRange {
    let translate =
        |v: u64| paging::translate(hhdm, v).unwrap_or_else(|| panic!("{what}: virtual address {v:#x} is not mapped"));
    let start = translate(virt);
    let (first_page, first_frame) = (align_down(virt, FRAME_SIZE), align_down(start, FRAME_SIZE));
    for page in pages(virt, len) {
        if translate(page) != first_frame + (page - first_page) {
            panic!("{what}: not physically contiguous at {page:#x}");
        }
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

    let (kstart, klen) = kernel_image_virt();
    let image = phys_range_of(hhdm, "kernel image", kstart, klen);
    if let Some(addr) = boot::EXECUTABLE_ADDRESS.get_response() {
        // Two sources disagreeing means one of them is wrong about where the
        // kernel lives: don't guess which.
        assert_eq!(addr.physical_base(), image.start(), "Limine and the page tables disagree on the kernel's address");
    }
    r.push("kernel image", image);

    match boot::EXECUTABLE_FILE.get_response() {
        Some(resp) => {
            let file = resp.file();
            r.push("kernel file", phys_range_of(hhdm, "kernel file", file.addr() as u64, file.size()));
        }
        // Not used then (symbols aren't loaded); its memory is non-usable.
        None => debug!(target: "mem", "no kernel file response; nothing to reserve"),
    }

    let (bottom, top) = arch::stacks::boot_stack();
    r.push("boot stack", phys_range_of(hhdm, "boot stack", bottom, top - bottom));

    match framebuffer_virt() {
        Some((virt, len)) => r.push("framebuffer", phys_range_of(hhdm, "framebuffer", virt, len)),
        None => debug!(target: "mem", "no framebuffer; nothing to reserve"),
    }

    // The ACPI tables are normally in `acpi-*` map entries, which are
    // reserved with every other non-usable entry. Parsing them (#2) must
    // reserve any table it finds in usable memory.
    match boot::RSDP.get_response() {
        Some(resp) => {
            // Physical at base revision 3 (what we request); other revisions
            // give an HHDM pointer. Accept either.
            let addr = resp.address() as u64;
            let phys = if addr >= hhdm { addr - hhdm } else { addr };
            r.push("acpi rsdp", PhysRange::from_len(phys, RSDP_SIZE));
        }
        None => warn!(target: "mem", "no ACPI RSDP from Limine"),
    }
    r
}

/// Build the frame allocator. Called once, early in boot, before anything
/// allocates.
pub fn init(hhdm: u64) {
    assert!(STATE.lock().is_none(), "frame allocator initialized twice");
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
    paging::for_each_table_frame(hhdm, |table| {
        assert!(!bitmap.contains(table), "frame bitmap would overwrite page table {table:#x}");
    });

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
    const NOTHING_ALLOCATED: &str = "nothing is allocated during init";
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
    let tables = paging::for_each_table_frame(hhdm, |table| {
        frames.reserve(PhysRange::from_len(table, FRAME_SIZE)).expect(NOTHING_ALLOCATED);
    });

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

/// Allocate one 4 KiB frame; returns its physical address, or `None` when
/// physical memory is exhausted. Frames are not zeroed.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: page tables (#4), heap (#5)
pub fn alloc() -> Option<u64> {
    with_frames(|f| f.alloc())
}

/// Allocate `count` contiguous frames aligned to `align_frames` frames (a
/// power of two); returns the first physical address.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: page tables (#4), heap (#5)
pub fn alloc_contiguous(count: usize, align_frames: usize) -> Option<u64> {
    with_frames(|f| f.alloc_contiguous(count, align_frames))
}

/// Return a frame from [`alloc`] or [`alloc_contiguous`].
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: page tables (#4), heap (#5)
pub fn free(addr: u64) -> Result<(), FreeError> {
    with_frames(|f| f.free(addr))
}

/// Return `count` frames from [`alloc_contiguous`], all or nothing.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: page tables (#4), heap (#5)
pub fn free_contiguous(first: u64, count: usize) -> Result<(), FreeError> {
    with_frames(|f| f.free_contiguous(first, count))
}

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

    pub fn bitmap() -> Result<PhysRange, &'static str> {
        let reserved = with_state(|s| s.reserved);
        reserved.as_slice().iter().find(|r| r.name == BITMAP).map(|r| r.range).ok_or("no bitmap reservation")
    }

    pub fn hhdm() -> Result<u64, &'static str> {
        Ok(boot::HHDM.get_response().ok_or("no HHDM response")?.offset())
    }

    /// FNV-1a: detects writes to memory the test must not touch.
    pub fn checksum(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
    }

    /// Kernel `.text` and `.rodata` (which ends with `.ktests`): never written.
    pub fn kernel_readonly() -> &'static [u8] {
        unsafe extern "C" {
            static __ktests_end: u8;
        }
        let (start, _) = kernel_image_virt();
        let len = &raw const __ktests_end as u64 - start;
        // SAFETY: the kernel's text and rodata segments are mapped and
        // immutable for the life of the kernel.
        unsafe { core::slice::from_raw_parts(start as *const u8, len as usize) }
    }

    pub fn kernel_file() -> Result<&'static [u8], &'static str> {
        let resp = boot::EXECUTABLE_FILE.get_response().ok_or("no kernel file response")?;
        Ok(crate::file_bytes(resp.file()))
    }

    /// The virtual ranges the kernel is using right now, as
    /// `(name, virtual address, length)`. Every Limine response they need
    /// must be present, so no check can be silently skipped.
    pub fn live_ranges() -> Result<[(&'static str, u64, u64); 5], &'static str> {
        let (kstart, klen) = kernel_image_virt();
        let (stack_lo, stack_hi) = arch::stacks::boot_stack();
        let file = kernel_file()?;
        let (fb, fb_len) = framebuffer_virt().ok_or("no framebuffer")?;
        Ok([
            ("kernel image", kstart, klen),
            ("boot stack", stack_lo, stack_hi - stack_lo),
            ("current stack page", arch::read_rsp(), 1),
            ("kernel file", file.as_ptr() as u64, file.len() as u64),
            ("framebuffer", fb, fb_len),
        ])
    }

    /// The first frame the kernel is using that `is_owned` says is
    /// allocatable: behind a live range (found through the page tables, not
    /// the reservation list) or holding a page table.
    pub fn first_live_owned(
        hhdm: u64,
        is_owned: impl Fn(u64) -> bool,
    ) -> Result<Option<(&'static str, u64)>, &'static str> {
        for (name, virt, len) in live_ranges()? {
            for page in pages(virt, len) {
                let phys = paging::translate(hhdm, page).ok_or("a live page is not mapped")?;
                if is_owned(phys) {
                    return Ok(Some((name, phys)));
                }
            }
        }
        let mut table = None;
        paging::for_each_table_frame(hhdm, |t| {
            if table.is_none() && is_owned(t) {
                table = Some(("page table", t));
            }
        });
        Ok(table)
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
        /// `alloc` and `alloc_contiguous` both failed at the end.
        pub exhausted: bool,
    }

    /// Allocate frames until none is left. Each is checked, then filled with
    /// [`FILL`] except for three words: the previous frame (0 ends the chain:
    /// frame 0 is never handed out), a sequence number and its own address.
    /// Never returns early: the caller must always free the chain.
    pub fn allocate_all(entries: &[&Entry], reserved: &[Reservation]) -> Chain {
        let mut chain = Chain { head: 0, count: 0, failure: None, exhausted: false };
        while let Some(frame) = alloc() {
            if let Some(why) = bad_frame(frame, entries, reserved) {
                crate::ktest::diag(format_args!("frame {frame:#x}: {why}"));
                chain.failure = Some(why);
                if let Err(e) = free(frame) {
                    crate::ktest::diag(format_args!("free({frame:#x}): {e}"));
                }
                return chain;
            }
            let p = crate::mm::phys_to_virt(frame).cast::<u64>();
            // SAFETY: the allocator just handed us this frame and it passed
            // the checks above, so nothing else uses it; the HHDM maps it.
            unsafe {
                core::ptr::write_bytes(p.cast::<u8>(), FILL, FRAME_SIZE as usize);
                p.write(chain.head);
                p.add(1).write(chain.count as u64);
                p.add(2).write(frame);
            }
            chain.head = frame;
            chain.count += 1;
        }
        chain.exhausted = match alloc_contiguous(1, 1) {
            None => true,
            Some(extra) => {
                let _ = free(extra); // only to not leak it; the test fails on `exhausted`
                false
            }
        };
        chain
    }

    /// Free every frame of the chain, newest first. Checks each still holds
    /// what was written: its sequence number counts down from `count - 1`, so
    /// a frame handed out twice (overwritten by its second allocation) or any
    /// other writer is caught. Keeps going after a problem so that a failure
    /// doesn't leak memory into the next test. Returns `(freed, first problem)`.
    pub fn free_chain(chain: &Chain) -> (usize, Option<&'static str>) {
        let (mut head, mut freed, mut problem) = (chain.head, 0, None);
        while head != 0 && freed < chain.count {
            let p = crate::mm::phys_to_virt(head);
            let w = p.cast::<u64>();
            // SAFETY: `head` is a frame allocated and filled by allocate_all.
            let (next, seq, tag, last) =
                unsafe { (w.read(), w.add(1).read(), w.add(2).read(), p.add(FRAME_SIZE as usize - 1).read()) };
            let expected_seq = (chain.count - 1 - freed) as u64;
            if (seq, tag, last) != (expected_seq, head, FILL) && problem.is_none() {
                crate::ktest::diag(format_args!("frame {head:#x}: seq {seq} (want {expected_seq}), tag {tag:#x}"));
                problem = Some("an allocated frame was overwritten (handed out twice, or another writer)");
            }
            if let Err(e) = free(head) {
                crate::ktest::diag(format_args!("free({head:#x}): {e}"));
                problem = problem.or(Some("freeing an allocated frame failed"));
            }
            freed += 1;
            head = next;
        }
        (freed, problem)
    }
}

crate::ktest!(
    fn never_returns_reserved() {
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        let hhdm = hhdm()?;
        let entries = boot::MEMMAP.get_response().ok_or("no memory map")?.entries();
        let reserved = with_state(|s| s.reserved);

        // 1. Independent of the reservation list: nothing the kernel is using
        //    (found through the page tables) is allocatable. The bitmap check
        //    needs the list only to know where the bitmap is.
        if let Some((name, phys)) = first_live_owned(hhdm, is_owned)? {
            crate::ktest::diag(format_args!("{name}: frame {phys:#x} is allocatable"));
            return Err("a frame the kernel is using is allocatable");
        }
        let bitmap = bitmap()?;
        kassert!(pages(bitmap.start(), bitmap.len()).all(|p| !is_owned(p)));

        // 2. Allocate every frame, checking each against the memory map and
        //    the named list; 3. free them all, checking contents.
        let before = free_frames();
        let readonly_sum = checksum(kernel_readonly());
        let file_sum = checksum(kernel_file()?);
        let chain = allocate_all(entries, reserved.as_slice());
        let (freed, problem) = free_chain(&chain);

        if let Some(why) = chain.failure.or(problem) {
            return Err(why);
        }
        kassert!(chain.exhausted);
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
        let hhdm = hhdm()?;
        let (kstart, _) = kernel_image_virt();
        let (stack_lo, _) = arch::stacks::boot_stack();
        let image = paging::translate(hhdm, kstart).ok_or("kernel not mapped")?;
        let stack = paging::translate(hhdm, align_down(stack_lo, FRAME_SIZE)).ok_or("boot stack not mapped")?;
        let mut table = 0;
        paging::for_each_table_frame(hhdm, |t| table = t);
        kassert_eq!(first_live_owned(hhdm, |p| p == image)?, Some(("kernel image", image)));
        kassert_eq!(first_live_owned(hhdm, |p| p == stack)?, Some(("boot stack", stack)));
        kassert_eq!(first_live_owned(hhdm, |p| p == table)?, Some(("page table", table)));
        kassert_eq!(first_live_owned(hhdm, |_| false)?, None);
        Ok(())
    }
);

crate::ktest!(
    fn named_reservations_are_complete() {
        use self::checks::*;
        use crate::kassert_eq;
        let reserved = with_state(|s| s.reserved);
        let find = |name| reserved.as_slice().iter().find(|r| r.name == name).filter(|r| !r.range.is_empty());
        for name in [NULL_FRAME, "kernel image", "kernel file", "boot stack", "framebuffer", "acpi rsdp", BITMAP] {
            if find(name).is_none() {
                crate::ktest::diag(format_args!("missing or empty reservation '{name}'"));
                return Err("a named reservation is missing");
            }
        }
        let image = find("kernel image").ok_or("no kernel image")?;
        let limine = boot::EXECUTABLE_ADDRESS.get_response().ok_or("no executable address response")?;
        kassert_eq!(image.range.start(), limine.physical_base());
        Ok(())
    }
);

crate::ktest!(
    fn translate_agrees_with_the_hhdm() {
        // The reservations and the oracle share the page-table walker; check
        // it against an independent answer: the HHDM maps `p` at `hhdm + p`.
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        kassert!(arch::read_cr4() & (1 << 12) == 0); // no LA57: the walker assumes 4 levels
        let hhdm = hhdm()?;
        let frame = alloc().ok_or("out of frames")?;
        for p in [frame, frame + 0x123, bitmap()?.start(), 0x1000] {
            kassert_eq!(paging::translate(hhdm, hhdm + p), Some(p));
        }
        kassert_eq!(free(frame), Ok(()));
        Ok(())
    }
);

crate::ktest!(
    fn alloc_free_reuse() {
        use self::checks::*;
        use crate::{kassert, kassert_eq};
        let before = free_frames();
        let a = alloc().ok_or("out of frames")?;
        kassert_eq!(free(a), Ok(()));
        kassert_eq!(alloc(), Some(a));
        kassert_eq!(free(a), Ok(()));
        kassert_eq!(free(a), Err(FreeError::DoubleFree));

        let kernel = paging::translate(hhdm()?, kernel_image_virt().0).ok_or("kernel not mapped")?;
        kassert!(matches!(free(kernel), Err(FreeError::NotOwned | FreeError::OutOfRange)));
        kassert!(free(0).is_err());

        let run = alloc_contiguous(4, 4).ok_or("no 4-frame run")?;
        kassert!(run.is_multiple_of(4 * FRAME_SIZE));
        kassert_eq!(free_contiguous(run, 4), Ok(()));
        kassert_eq!(free_frames(), before);
        Ok(())
    }
);
