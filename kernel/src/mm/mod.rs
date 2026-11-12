//! Memory management: the physical frame allocator ([`frame`]), the
//! kernel's page tables ([`paging`]) and the kernel heap ([`heap`]). Physical
//! memory is reached through the higher-half direct map (HHDM), which Limine
//! sets up and [`paging`] rebuilds at the same address.

pub mod frame;
pub mod heap;
pub mod paging;
pub mod stack;
mod window;

use core::sync::atomic::{AtomicU64, Ordering};

use lumen_mm::{FRAME_SIZE, PhysAddr, VirtAddr, align_down};

use crate::{arch, boot};

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Set up memory management from the bootloader's responses: the frame
/// allocator (built while Limine's page tables are active), the kernel's
/// own page tables, then the heap (from here on `alloc` works).
pub fn init() {
    let hhdm = boot::HHDM.get_response().expect("Limine gave no HHDM response").offset();
    HHDM_OFFSET.store(hhdm, Ordering::Relaxed);
    frame::init(hhdm);
    paging::init();
    heap::init();
}

/// The HHDM offset, or 0 before [`init`] stores it (its first step).
fn hhdm_offset() -> u64 {
    HHDM_OFFSET.load(Ordering::Relaxed)
}

/// The virtual address at which physical address `phys` is mapped. Valid
/// once [`init`] has stored the HHDM offset (its first step), and only for
/// memory the HHDM maps ([`paging::HHDM_TYPES`]); ACPI tables and device
/// memory need [`paging::map_mmio`].
pub fn phys_to_virt(phys: PhysAddr) -> VirtAddr {
    let offset = hhdm_offset();
    assert_ne!(offset, 0, "phys_to_virt before mm::init");
    VirtAddr::new(offset + phys.as_u64()).expect("HHDM addresses are canonical")
}

/// The 4 KiB-aligned addresses of the pages `[start, start + len)` touches.
fn pages(start: u64, len: u64) -> impl Iterator<Item = u64> {
    (align_down(start, FRAME_SIZE)..start + len).step_by(FRAME_SIZE as usize)
}

unsafe extern "C" {
    static __kernel_start: u8;
    static __kernel_end: u8;
}

/// The kernel image's virtual range as `(start, length)`, from `linker.ld`
/// (`make audit` checks every allocated section lies inside it).
fn kernel_image() -> (u64, u64) {
    let start = &raw const __kernel_start as u64;
    (start, &raw const __kernel_end as u64 - start)
}

/// A virtual range the kernel is using.
#[derive(Clone, Copy, Debug)]
struct LiveRange {
    name: &'static str,
    /// `(start, length)`, or `None` where it is unknown or empty (missing
    /// Limine response, empty cmdline, allocator not built yet).
    range: Option<(u64, u64)>,
    /// May legitimately be `None` (the response or value can be absent).
    optional: bool,
    /// The kernel writes it, so it must stay writable.
    written: bool,
    /// Backed by frames the allocator handed out (an allocated stack), which
    /// must never be free; otherwise by frames the allocator must never own.
    allocated: bool,
}

const fn live(name: &'static str, range: Option<(u64, u64)>, optional: bool, written: bool) -> LiveRange {
    LiveRange { name, range, optional, written, allocated: false }
}

/// The virtual ranges the kernel is known to depend on (the memory map's
/// entries, each its own pointer, and the rest of Limine's responses are
/// covered separately by the pre-switch check). The page tables
/// must map all of them (checked before switching to the kernel's own
/// tables) and the frames behind them must never be handed out (checked by
/// the frame allocator's tests).
fn live_ranges() -> [LiveRange; 8] {
    let (stack_lo, stack_hi) = arch::stacks::boot_stack();
    let file = boot::EXECUTABLE_FILE.get_response().map(|r| (r.file().addr() as u64, r.file().size()));
    let bitmap = frame::bitmap().map(|b| (phys_to_virt(PhysAddr::new(b.start()).expect("physical")).as_u64(), b.len()));
    let memmap = boot::MEMMAP.get_response().map(|r| {
        let entries = r.entries();
        (entries.as_ptr() as u64, core::mem::size_of_val(entries) as u64)
    });
    let cmdline = crate::cmdline::get();
    let cmdline = (!cmdline.is_empty()).then_some((cmdline.as_ptr() as u64, cmdline.len() as u64));
    [
        live(frame::KERNEL_IMAGE, Some(kernel_image()), false, false),
        live(frame::BOOT_STACK, (stack_hi > stack_lo).then_some((stack_lo, stack_hi - stack_lo)), false, true),
        LiveRange {
            // On the boot stack until `kmain` switches to an allocated one.
            allocated: arch::stacks::REGISTRY.contains(arch::read_rsp(), 1),
            ..live("current stack page", Some((arch::read_rsp(), 1)), false, true)
        },
        live(frame::KERNEL_FILE, file, true, false),
        live(frame::FRAMEBUFFER, frame::framebuffer_virt(), true, true),
        live(frame::BITMAP, bitmap, false, true),
        live("memory map (entry pointers)", memmap, false, false),
        live("cmdline", cmdline, true, false),
    ]
}
