//! Known kernel stacks.
//!
//! Every stack the kernel runs on once `kmain` switches stacks (right after
//! `mm::init`) comes from the allocator with an unmapped guard page below it
//! (hard rule 4; [`crate::mm::stack`]) and is listed in [`REGISTRY`]. The one
//! exception is the stack Limine booted us on, used until that switch; it is
//! recorded here ([`set_boot_stack`], not in the registry) so a crash before
//! the switch, or a backtrace reaching back into `kmain`, can still be
//! walked.
//!
//! The crash reporter uses this module lock-free: a frame pointer is only
//! followed if it points into a known stack ([`contains`]), so a corrupted
//! `rbp` can't make the backtrace fault, and a fault in a guard page is
//! named ([`guard_hit`]): as that stack overflowing or as a stray access
//! (`crash::write_guard_hit`).

use core::sync::atomic::{AtomicU64, Ordering};

use lumen_mm::stacks::{Hit, Registered, Registry};

/// The allocated kernel stacks.
pub static REGISTRY: Registry = Registry::new();

static BOOT_STACK_TOP: AtomicU64 = AtomicU64::new(0);
static BOOT_STACK_BOTTOM: AtomicU64 = AtomicU64::new(0);

/// Record the boot stack: `top` is its highest address (the stack pointer at
/// kernel entry plus Limine's 8-byte return address) and `size` the size
/// Limine confirmed.
pub fn set_boot_stack(top: u64, size: u64) {
    BOOT_STACK_TOP.store(top, Ordering::Relaxed);
    BOOT_STACK_BOTTOM.store(top.saturating_sub(size), Ordering::Relaxed);
}

/// The boot stack as `(bottom, top)` virtual addresses.
pub fn boot_stack() -> (u64, u64) {
    (BOOT_STACK_BOTTOM.load(Ordering::Relaxed), BOOT_STACK_TOP.load(Ordering::Relaxed))
}

/// Is `[addr, addr + len)` entirely inside one known stack (the boot stack
/// or a registered one)?
pub fn contains(addr: u64, len: u64) -> bool {
    let Some(end) = addr.checked_add(len) else { return false };
    let (lo, hi) = boot_stack();
    (lo != 0 && addr >= lo && end <= hi) || REGISTRY.contains(addr, len)
}

/// The `(bottom, top)` of the known stack (the boot stack or a registered
/// one) that `addr` is in, if any.
pub fn bounds_of(addr: u64) -> Option<(u64, u64)> {
    let (lo, hi) = boot_stack();
    if lo != 0 && (lo..hi).contains(&addr) {
        return Some((lo, hi));
    }
    match REGISTRY.find(addr) {
        Some(Hit::Stack(r)) => Some((r.range.bottom, r.range.top)),
        _ => None,
    }
}

/// The registered stack whose guard page `addr` is in, if any.
pub fn guard_hit(addr: u64) -> Option<Registered> {
    match REGISTRY.find(addr) {
        Some(Hit::Guard(r)) => Some(r),
        _ => None,
    }
}
