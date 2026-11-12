//! Memory management: the physical frame allocator ([`frame`]) and the
//! kernel's page tables ([`paging`]); the heap (#5) will join them. Physical
//! memory is reached through the higher-half direct map (HHDM), which Limine
//! sets up and [`paging`] rebuilds at the same address.

pub mod frame;
pub mod paging;

use core::sync::atomic::{AtomicU64, Ordering};

use lumen_mm::{PhysAddr, VirtAddr};

use crate::{arch, boot};

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Set up memory management from the bootloader's responses: the frame
/// allocator (built while Limine's page tables are active), then the
/// kernel's own page tables.
pub fn init() {
    let hhdm = boot::HHDM.get_response().expect("Limine gave no HHDM response").offset();
    HHDM_OFFSET.store(hhdm, Ordering::Relaxed);
    frame::init(hhdm);
    paging::init();
}

/// The HHDM offset, or 0 before [`init`].
fn hhdm_offset() -> u64 {
    HHDM_OFFSET.load(Ordering::Relaxed)
}

/// The virtual address at which physical address `phys` is mapped. Only
/// valid after [`init`], and only for RAM the HHDM maps (see [`paging`]).
pub fn phys_to_virt(phys: PhysAddr) -> VirtAddr {
    let offset = hhdm_offset();
    assert_ne!(offset, 0, "phys_to_virt before mm::init");
    VirtAddr::new(offset + phys.as_u64()).expect("HHDM addresses are canonical")
}

/// A virtual range the kernel is using: its name, and `(start, length)`, or
/// `None` where the Limine response it comes from is missing.
pub type LiveRange = (&'static str, Option<(u64, u64)>);

/// Every virtual range the running kernel depends on. The page tables must
/// map all of them (checked before switching to the kernel's own tables) and
/// the frames behind them must never be handed out (checked by the frame
/// allocator's tests).
pub fn live_ranges() -> [LiveRange; 8] {
    unsafe extern "C" {
        static __kernel_start: u8;
        static __kernel_end: u8;
    }
    let kstart = &raw const __kernel_start as u64;
    let kend = &raw const __kernel_end as u64;
    let (stack_lo, stack_hi) = arch::stacks::boot_stack();
    let file = boot::EXECUTABLE_FILE.get_response().map(|r| (r.file().addr() as u64, r.file().size()));
    let bitmap = frame::bitmap().map(|b| (hhdm_offset() + b.start(), b.len()));
    let memmap = boot::MEMMAP.get_response().map(|r| {
        let entries = r.entries();
        (entries.as_ptr() as u64, core::mem::size_of_val(entries) as u64)
    });
    let cmdline = crate::cmdline::get();
    let cmdline = (!cmdline.is_empty()).then_some((cmdline.as_ptr() as u64, cmdline.len() as u64));
    [
        (frame::KERNEL_IMAGE, Some((kstart, kend - kstart))),
        (frame::BOOT_STACK, (stack_hi > stack_lo).then_some((stack_lo, stack_hi - stack_lo))),
        ("current stack page", Some((arch::read_rsp(), 1))),
        (frame::KERNEL_FILE, file),
        (frame::FRAMEBUFFER, frame::framebuffer_virt()),
        (frame::BITMAP, bitmap),
        ("memory map entries", memmap),
        ("cmdline", cmdline),
    ]
}
