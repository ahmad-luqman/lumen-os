//! The kernel's physical frame allocator (hard rule 2: never hand out memory
//! you don't own).
//!
//! The logic is in the host-tested `lumen-mm` crate; this module feeds it:
//!
//! 1. **Usable memory** is exactly the `usable` entries of Limine's memory map.
//!    Bootloader-reclaimable memory stays reserved for now (Limine's page
//!    tables, the boot stack and the kernel file live there).
//! 2. **Reservations** are made explicitly, by name, from what the kernel is
//!    actually using, found by translating live virtual addresses through the
//!    page tables rather than trusting the map's labels: frame 0, the kernel
//!    image, the kernel ELF file (the crash reporter's symbols), the boot
//!    stack, the framebuffer, the ACPI RSDP and the allocator's own bitmaps.
//!    On top of that every non-usable map entry is reserved (in case entries
//!    overlap) and so is every frame of the active page tables.
//!
//! The named list is printed at `debug` level (`lumen.log=debug`) and kept for
//! the in-kernel test, which allocates every frame and checks it against the
//! memory map, this list and the live mappings.

use limine::memory_map::{Entry, EntryType};
use log::{debug, info, warn};
use lumen_mm::{FRAME_SIZE, FrameAllocator, FreeError, PhysRange};

use crate::arch::{self, paging};
use crate::boot;
use crate::sync::SpinLock;

const MAX_RESERVATIONS: usize = 16;
/// Size of an ACPI 2.0+ RSDP structure.
const RSDP_SIZE: u64 = 36;

/// A named physical range the allocator must never hand out.
#[derive(Clone, Copy, Debug)]
pub struct Reservation {
    pub name: &'static str,
    pub range: PhysRange,
}

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
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // read by the ktest; X-ray (M5) will show it
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

/// The physical range behind `[virt, virt + len)`, which must be mapped and
/// physically contiguous (true for the HHDM and for the kernel image Limine
/// loads). Panics otherwise: guessing would risk handing out live memory.
fn phys_range_of(hhdm: u64, what: &str, virt: u64, len: u64) -> PhysRange {
    let translate =
        |v: u64| paging::translate(hhdm, v).unwrap_or_else(|| panic!("{what}: virtual address {v:#x} is not mapped"));
    let start = translate(virt);
    let first_page = virt & !(FRAME_SIZE - 1);
    let mut page = first_page;
    while page < virt + len {
        let expected = (start & !(FRAME_SIZE - 1)) + (page - first_page);
        if translate(page) != expected {
            panic!("{what}: not physically contiguous at {page:#x}");
        }
        page += FRAME_SIZE;
    }
    PhysRange::from_len(start, len)
}

fn usable(entries: &[&Entry]) -> impl Iterator<Item = PhysRange> + Clone {
    entries.iter().filter(|e| e.entry_type == EntryType::USABLE).map(|e| PhysRange::from_len(e.base, e.length))
}

/// Memory-map entry types that describe RAM (as opposed to MMIO holes).
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

/// Everything the kernel uses that must never be handed out, by name.
fn named_reservations(hhdm: u64) -> Reservations {
    let mut r = Reservations::new();
    r.push("null frame", PhysRange::new(0, FRAME_SIZE));

    let (kstart, klen) = kernel_image_virt();
    let image = phys_range_of(hhdm, "kernel image", kstart, klen);
    if let Some(addr) = boot::EXECUTABLE_ADDRESS.get_response()
        && addr.physical_base() != image.start
    {
        warn!(target: "mem", "Limine says kernel at {:#x}, page tables say {:#x}", addr.physical_base(), image.start);
    }
    r.push("kernel image", image);

    if let Some(resp) = boot::EXECUTABLE_FILE.get_response() {
        let file = resp.file();
        r.push("kernel file", phys_range_of(hhdm, "kernel file", file.addr() as u64, file.size()));
    }

    let (bottom, top) = arch::stacks::boot_stack();
    r.push("boot stack", phys_range_of(hhdm, "boot stack", bottom, top - bottom));

    if let Some(fb) = boot::FRAMEBUFFER.get_response().and_then(|resp| resp.framebuffers().next()) {
        r.push("framebuffer", phys_range_of(hhdm, "framebuffer", fb.addr() as u64, fb.pitch() * fb.height()));
    }

    // The ACPI tables themselves are in `acpi-reclaimable`/`acpi-nvs` map
    // entries, which are reserved below with every other non-usable entry.
    // Parsing them (#2) must reserve any table it finds outside those.
    if let Some(resp) = boot::RSDP.get_response() {
        // Physical since base revision 3; older revisions gave an HHDM pointer.
        let addr = resp.address() as u64;
        let phys = if addr >= hhdm { addr - hhdm } else { addr };
        r.push("acpi rsdp", PhysRange::from_len(phys, RSDP_SIZE));
    }
    r
}

/// Build the frame allocator. Called once, early in boot, before anything
/// allocates.
pub fn init(hhdm: u64) {
    let entries = boot::MEMMAP.get_response().expect("Limine gave no memory map").entries();
    let mut reserved = named_reservations(hhdm);

    // The bitmaps cover the span from the lowest to the highest usable frame.
    let span = usable(entries)
        .reduce(|a, b| PhysRange::new(a.start.min(b.start), a.end.max(b.end)))
        .expect("memory map has no usable memory");
    let bytes = lumen_mm::storage_bytes(span);
    let start = lumen_mm::find_hole(usable(entries), reserved.as_slice().iter().map(|r| r.range), bytes)
        .expect("no usable memory large enough for the frame bitmap");
    let bitmap = PhysRange::from_len(start, bytes);
    // Limine allocates its page tables from bootloader-reclaimable memory, so
    // none should be in a usable hole. Check before zeroing the bitmap over one.
    paging::for_each_table_frame(hhdm, |table| {
        assert!(!bitmap.contains(table), "frame bitmap would overwrite page table {table:#x}");
    });

    // SAFETY: `bitmap` is inside a usable map entry, which the HHDM maps
    // writable; it overlaps no named reservation (find_hole) and no page
    // table (checked above), and nothing else uses usable memory before the
    // allocator exists. It is reserved below, so it is never handed out, and
    // never freed, so `'static` holds.
    let storage = unsafe { core::slice::from_raw_parts_mut((hhdm + start) as *mut u64, lumen_mm::storage_words(span)) };
    let mut frames = FrameAllocator::new(span, storage).expect("bitmap storage sized by storage_words");
    for range in usable(entries) {
        frames.add_usable(range).expect("usable regions are added before any reservation");
    }
    reserved.push("frame bitmap", bitmap);
    for r in reserved.as_slice() {
        frames.reserve(r.range);
    }
    for e in entries.iter().filter(|e| e.entry_type != EntryType::USABLE) {
        frames.reserve(PhysRange::from_len(e.base, e.length));
    }
    let tables = paging::for_each_table_frame(hhdm, |table| {
        frames.reserve(PhysRange::from_len(table, FRAME_SIZE));
    });

    for r in reserved.as_slice() {
        debug!(target: "mem", "reservation {} {} ({})", r.range, r.name, Size(r.range.len()));
    }
    debug!(target: "mem", "reservation: {tables} page-table frames, and every non-usable map entry");
    // "reserved" = RAM frames that can't be allocated. MMIO and firmware
    // `reserved` entries aren't RAM (one is 12 GiB on q35), so they don't count.
    let ram: u64 = entries.iter().filter(|e| is_ram(e.entry_type)).map(|e| e.length / FRAME_SIZE).sum();
    let free = frames.free_frames();
    info!(target: "mem", "frames: {free} usable, {} reserved", ram.saturating_sub(free as u64));

    *STATE.lock() = Some(State { frames, reserved });
}

/// Allocate one 4 KiB frame; returns its physical address.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: page tables (#4), heap (#5)
pub fn alloc() -> Option<u64> {
    STATE.lock().as_mut()?.frames.alloc()
}

/// Allocate `count` contiguous frames aligned to `align_frames` frames (a
/// power of two); returns the first physical address.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: page tables (#4), heap (#5)
pub fn alloc_contiguous(count: usize, align_frames: u64) -> Option<u64> {
    STATE.lock().as_mut()?.frames.alloc_contiguous(count, align_frames)
}

/// Return a frame from [`alloc`] or [`alloc_contiguous`].
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: page tables (#4), heap (#5)
pub fn free(addr: u64) -> Result<(), FreeError> {
    STATE.lock().as_mut().expect("frame allocator not initialized").frames.free(addr)
}

#[cfg(feature = "ktest")]
fn with_state<R>(f: impl FnOnce(&State) -> R) -> R {
    f(STATE.lock().as_ref().expect("frame allocator not initialized"))
}

/// FNV-1a: detects writes to memory the test must not touch.
#[cfg(feature = "ktest")]
fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

/// Kernel `.text` and `.rodata` (which ends with `.ktests`): never written.
#[cfg(feature = "ktest")]
fn kernel_readonly() -> &'static [u8] {
    unsafe extern "C" {
        static __ktests_end: u8;
    }
    let (start, _) = kernel_image_virt();
    let len = &raw const __ktests_end as u64 - start;
    // SAFETY: the kernel's text and rodata segments are mapped and immutable
    // for the life of the kernel.
    unsafe { core::slice::from_raw_parts(start as *const u8, len as usize) }
}

#[cfg(feature = "ktest")]
fn kernel_file() -> &'static [u8] {
    boot::EXECUTABLE_FILE.get_response().map_or(&[], |resp| {
        // SAFETY: Limine maps the file for the life of the kernel, and it is
        // reserved (bootloader-reclaimable memory is never reclaimed yet).
        unsafe { core::slice::from_raw_parts(resp.file().addr(), resp.file().size() as usize) }
    })
}

/// Is any page of `[virt, virt + len)` backed by a frame the allocator owns?
/// Uses the page tables, not the reservation list, so it catches a missing
/// reservation.
#[cfg(feature = "ktest")]
fn live_range_is_owned(hhdm: u64, virt: u64, len: u64) -> Option<u64> {
    let mut page = virt & !(FRAME_SIZE - 1);
    while page < virt + len {
        if let Some(phys) = paging::translate(hhdm, page)
            && with_state(|s| s.frames.is_owned(phys))
        {
            return Some(phys);
        }
        page += FRAME_SIZE;
    }
    None
}

#[cfg(feature = "ktest")]
const FILL: u8 = 0x5a;

/// Why an allocated frame is wrong, if it is.
#[cfg(feature = "ktest")]
fn bad_frame(frame: u64, entries: &[&Entry], reserved: &[Reservation]) -> Option<&'static str> {
    let range = PhysRange::from_len(frame, FRAME_SIZE);
    if !frame.is_multiple_of(FRAME_SIZE) {
        return Some("frame address not aligned");
    }
    if !usable(entries).any(|u| u.start <= range.start && range.end <= u.end) {
        return Some("frame outside every usable map entry");
    }
    let non_usable = entries.iter().filter(|e| e.entry_type != EntryType::USABLE);
    if non_usable.map(|e| PhysRange::from_len(e.base, e.length)).any(|r| r.overlaps(&range)) {
        return Some("frame overlaps a non-usable map entry");
    }
    if let Some(r) = reserved.iter().find(|r| r.range.overlaps(&range)) {
        crate::ktest::diag(format_args!("frame {frame:#x} is inside reservation '{}' {}", r.name, r.range));
        return Some("frame overlaps a named reservation");
    }
    None
}

crate::ktest!(
    fn never_returns_reserved() {
        use crate::{kassert, kassert_eq};
        let hhdm = boot::HHDM.get_response().ok_or("no HHDM")?.offset();
        let entries = boot::MEMMAP.get_response().ok_or("no memory map")?.entries();
        let reserved = with_state(|s| s.reserved);
        let reserved = reserved.as_slice();

        // 1. Oracle independent of the reservation list: nothing behind a live
        //    mapping, and no page table, is allocatable.
        let (kstart, klen) = kernel_image_virt();
        let (stack_lo, stack_hi) = arch::stacks::boot_stack();
        let rsp = arch::read_rsp();
        let file = kernel_file();
        let mut live = [
            ("kernel image", kstart, klen),
            ("boot stack", stack_lo, stack_hi - stack_lo),
            ("current stack page", rsp, 1),
            ("kernel file", file.as_ptr() as u64, file.len() as u64),
            ("framebuffer", 0, 0),
        ];
        if let Some(fb) = boot::FRAMEBUFFER.get_response().and_then(|r| r.framebuffers().next()) {
            live[4] = ("framebuffer", fb.addr() as u64, fb.pitch() * fb.height());
        }
        for (name, virt, len) in live {
            if let Some(phys) = live_range_is_owned(hhdm, virt, len) {
                crate::ktest::diag(format_args!("{name}: frame {phys:#x} is allocatable"));
                return Err("a frame behind a live mapping is allocatable");
            }
        }
        let mut table_owned = None;
        paging::for_each_table_frame(hhdm, |t| {
            if with_state(|s| s.frames.is_owned(t)) {
                table_owned = Some(t);
            }
        });
        kassert_eq!(table_owned, None);
        let bitmap = reserved.iter().find(|r| r.name == "frame bitmap").ok_or("no bitmap reservation")?.range;
        kassert!(!with_state(|s| s.frames.is_owned(bitmap.start) || s.frames.is_owned(bitmap.end - 1)));

        // 2. Allocate every frame. Each one is checked against the memory map
        //    and the named list, then filled completely and linked into a
        //    chain (word 0: previous frame, word 1: its own address) so they
        //    can all be freed again without a heap.
        let before = with_state(|s| s.frames.free_frames());
        let readonly_sum = checksum(kernel_readonly());
        let file_sum = checksum(file);
        let mut head = 0u64; // 0 ends the chain: frame 0 is never handed out
        let mut count = 0usize;
        let mut failure = None;
        while let Some(frame) = alloc() {
            if let Some(why) = bad_frame(frame, entries, reserved) {
                crate::ktest::diag(format_args!("frame {frame:#x}: {why}"));
                failure = Some(why);
                let _ = free(frame);
                break;
            }
            let p = crate::mm::phys_to_virt(frame);
            // SAFETY: the allocator just handed us this frame and it passed
            // the checks above, so nothing else uses it; the HHDM maps it.
            unsafe {
                core::ptr::write_bytes(p, FILL, FRAME_SIZE as usize);
                p.cast::<u64>().write(head);
                p.cast::<u64>().add(1).write(frame);
            }
            head = frame;
            count += 1;
        }
        let exhausted = failure.is_none() && alloc_contiguous(1, 1).is_none();

        // 3. Free the chain, checking every frame still holds what we wrote
        //    (a frame handed out twice would have been overwritten).
        let mut freed = 0usize;
        let mut corrupt = None;
        while head != 0 {
            let p = crate::mm::phys_to_virt(head);
            // SAFETY: `head` is a frame we allocated and filled above.
            let (next, tag, last) = unsafe {
                (p.cast::<u64>().read(), p.cast::<u64>().add(1).read(), p.add(FRAME_SIZE as usize - 1).read())
            };
            if tag != head || last != FILL {
                corrupt = Some(head);
                break;
            }
            if free(head).is_err() {
                return Err("freeing an allocated frame failed");
            }
            freed += 1;
            head = next;
        }

        if let Some(why) = failure {
            return Err(why);
        }
        kassert_eq!(corrupt, None);
        kassert!(exhausted);
        kassert_eq!(count, before);
        kassert_eq!(freed, count);
        kassert_eq!(with_state(|s| s.frames.free_frames()), before);
        kassert_eq!(checksum(kernel_readonly()), readonly_sum);
        kassert_eq!(checksum(file), file_sum);
        crate::ktest::diag(format_args!("allocated and freed all {count} frames"));
        Ok(())
    }
);

crate::ktest!(
    fn alloc_free_reuse() {
        use crate::{kassert, kassert_eq};
        let before = with_state(|s| s.frames.free_frames());
        let a = alloc().ok_or("out of frames")?;
        kassert_eq!(free(a), Ok(()));
        kassert_eq!(alloc(), Some(a));
        kassert_eq!(free(a), Ok(()));
        kassert_eq!(free(a), Err(FreeError::DoubleFree));

        let hhdm = boot::HHDM.get_response().ok_or("no HHDM")?.offset();
        let kernel = paging::translate(hhdm, kernel_image_virt().0).ok_or("kernel not mapped")?;
        kassert!(matches!(free(kernel), Err(FreeError::NotOwned | FreeError::OutOfRange)));
        kassert!(free(0).is_err());

        let run = alloc_contiguous(4, 4).ok_or("no 4-frame run")?;
        kassert!(run.is_multiple_of(4 * FRAME_SIZE));
        for i in 0..4 {
            kassert_eq!(free(run + i * FRAME_SIZE), Ok(()));
        }
        kassert_eq!(with_state(|s| s.frames.free_frames()), before);
        Ok(())
    }
);
