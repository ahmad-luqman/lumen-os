//! Known kernel stacks.
//!
//! Hard rule 4 says every stack comes from the allocator with a guard page
//! below it. Until #6 (which needs the frame allocator and the page tables
//! of #4), the boot stack (provided by Limine) and the three IST stacks below
//! are a **documented temporary exception**: they have no guard pages.
//!
//! The crash reporter also uses this registry: a frame pointer is only
//! followed if it points into one of these ranges, so a corrupted `rbp` can't
//! make the backtrace fault.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU64, Ordering};

pub const IST_STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
pub struct StaticStack(UnsafeCell<[u8; IST_STACK_SIZE]>);

// SAFETY: the memory is only used as a CPU stack, never accessed via `&`.
unsafe impl Sync for StaticStack {}

impl StaticStack {
    const fn new() -> Self {
        Self(UnsafeCell::new([0; IST_STACK_SIZE]))
    }
    pub fn bottom(&self) -> u64 {
        self.0.get() as u64
    }
    pub fn top(&self) -> u64 {
        self.bottom() + IST_STACK_SIZE as u64
    }
}

pub static DOUBLE_FAULT: StaticStack = StaticStack::new();
pub static NMI: StaticStack = StaticStack::new();
pub static MACHINE_CHECK: StaticStack = StaticStack::new();

static BOOT_STACK_TOP: AtomicU64 = AtomicU64::new(0);
static BOOT_STACK_BOTTOM: AtomicU64 = AtomicU64::new(0);

/// Record the boot stack: `top` is the stack pointer at kernel entry and
/// `size` the size we asked Limine for.
pub fn set_boot_stack(top: u64, size: u64) {
    BOOT_STACK_TOP.store(top, Ordering::Relaxed);
    BOOT_STACK_BOTTOM.store(top.saturating_sub(size), Ordering::Relaxed);
}

/// The boot stack as `(bottom, top)` virtual addresses.
pub fn boot_stack() -> (u64, u64) {
    (BOOT_STACK_BOTTOM.load(Ordering::Relaxed), BOOT_STACK_TOP.load(Ordering::Relaxed))
}

/// Is `[addr, addr + len)` entirely inside one known stack?
pub fn contains(addr: u64, len: u64) -> bool {
    let Some(end) = addr.checked_add(len) else { return false };
    let boot = (BOOT_STACK_BOTTOM.load(Ordering::Relaxed), BOOT_STACK_TOP.load(Ordering::Relaxed));
    let ranges = [
        boot,
        (DOUBLE_FAULT.bottom(), DOUBLE_FAULT.top()),
        (NMI.bottom(), NMI.top()),
        (MACHINE_CHECK.bottom(), MACHINE_CHECK.top()),
    ];
    ranges.iter().any(|&(lo, hi)| lo != 0 && addr >= lo && end <= hi)
}
