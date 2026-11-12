//! Turning this CPU's interrupts off and back on, nested.
//!
//! [`push_off`]/[`pop_off`] (xv6's names) disable interrupts for a critical
//! section, such as a held `SpinLock`. They nest by depth, not by saved
//! state: the first `push_off` records whether interrupts were on, and only
//! the last `pop_off` restores that, so guards dropped in any order never
//! turn interrupts on while another critical section is still open. Once
//! [`disable_for_good`] has run (the crash path), nothing turns them back on.
//!
//! One CPU for now: the depth and saved state are global. With SMP (#9) they
//! become per-CPU.

use core::arch::asm;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Open critical sections on this CPU.
static DEPTH: AtomicUsize = AtomicUsize::new(0);
/// Whether interrupts were on when the outermost section opened.
static WERE_ON: AtomicBool = AtomicBool::new(false);
/// Set by [`disable_for_good`]: interrupts stay off from then on.
static FOR_GOOD: AtomicBool = AtomicBool::new(false);

/// RFLAGS.IF.
const IF: u64 = 1 << 9;

/// Are interrupts enabled on this CPU?
pub fn enabled() -> bool {
    let rflags: u64;
    // SAFETY: reads RFLAGS through the stack; no other effect.
    unsafe { asm!("pushfq", "pop {}", out(reg) rflags, options(nomem, preserves_flags)) };
    rflags & IF != 0
}

fn cli() {
    // SAFETY: masking interrupts on this CPU is always allowed in ring 0.
    unsafe { asm!("cli", options(nomem, nostack)) };
}

/// Open a critical section: interrupts off until the matching [`pop_off`].
pub fn push_off() {
    let were_on = enabled();
    cli();
    // Interrupts are off: nothing on this CPU can run between these.
    if DEPTH.fetch_add(1, Ordering::Relaxed) == 0 {
        WERE_ON.store(were_on, Ordering::Relaxed);
    }
}

/// Close a critical section; the last one turns interrupts back on if they
/// were on before the first (and [`disable_for_good`] hasn't run).
pub fn pop_off() {
    assert!(!enabled(), "interrupts were turned on inside a critical section");
    let depth = DEPTH.fetch_sub(1, Ordering::Relaxed);
    assert!(depth > 0, "pop_off without push_off");
    if depth == 1 && WERE_ON.load(Ordering::Relaxed) && !FOR_GOOD.load(Ordering::Relaxed) {
        // SAFETY: the outermost critical section is closed and interrupts
        // were on before it opened.
        unsafe { asm!("sti", options(nomem, nostack)) };
    }
}

/// How many critical sections are open on this CPU.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by ktests
pub fn depth() -> usize {
    DEPTH.load(Ordering::Relaxed)
}

/// Turn interrupts on, outside any critical section (boot, once everything
/// that handles them is set up).
#[allow(dead_code)] // TEMP: kmain enables interrupts later in this PR
pub fn enable() {
    assert_eq!(depth(), 0, "enabling interrupts inside a critical section");
    assert!(!FOR_GOOD.load(Ordering::Relaxed), "enabling interrupts after disable_for_good");
    // SAFETY: no critical section is open; the caller has installed the
    // handlers for every source it unmasked.
    unsafe { asm!("sti", options(nomem, nostack)) };
}

/// Turn interrupts off and keep them off: no [`pop_off`] or [`enable`] turns
/// them on again. For the crash path, which must run alone and never return
/// to what it interrupted.
pub fn disable_for_good() {
    cli();
    FOR_GOOD.store(true, Ordering::Relaxed);
}
