//! Memory management: the physical frame allocator ([`frame`]); paging (#4)
//! and the heap (#5) will join it. Physical memory is reached through
//! Limine's higher-half direct map (HHDM).

pub mod frame;

use core::sync::atomic::{AtomicU64, Ordering};

use crate::boot;

static HHDM_OFFSET: AtomicU64 = AtomicU64::new(0);

/// Set up memory management from the bootloader's responses.
pub fn init() {
    let hhdm = boot::HHDM.get_response().expect("Limine gave no HHDM response").offset();
    HHDM_OFFSET.store(hhdm, Ordering::Relaxed);
    frame::init(hhdm);
}

/// The virtual address at which physical address `phys` is mapped. Only
/// valid after [`init`].
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first used by ktests; #4 uses it too
pub fn phys_to_virt(phys: u64) -> *mut u8 {
    let offset = HHDM_OFFSET.load(Ordering::Relaxed);
    assert_ne!(offset, 0, "phys_to_virt before mm::init");
    (offset + phys) as *mut u8
}
