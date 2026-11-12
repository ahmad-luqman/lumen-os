//! Turning this CPU's interrupts off and back on, nested.
//!
//! [`push_off`] (xv6's name) disables interrupts for a critical section,
//! such as a held `SpinLock`, until the [`IrqOff`] it returns is dropped.
//! Sections nest by depth, not by saved state: the first records whether
//! interrupts were on, and only the last to close restores that, so guards
//! dropped in any order never turn interrupts on while another section is
//! still open. `IrqOff` can't be sent to another thread: the depth belongs to
//! the CPU that opened the section. Once [`disable_for_good`] has run (the
//! crash path), no section turns them back on.
//!
//! The depth and saved state are per CPU (indexed by
//! [`percpu::index`](super::percpu::index)); [`disable_for_good`] holds on
//! every CPU (a crash stops them all).

use core::arch::asm;
use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::percpu::{self, MAX_CPUS};

/// Open critical sections, per CPU.
static DEPTH: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
/// Whether interrupts were on when a CPU's outermost section opened.
static WERE_ON: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];
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

// `cli` and `sti` are not `nomem`: they must also be compiler barriers, so no
// memory access a critical section protects moves out of it (a lock's
// acquire before the `cli`, or its release after the `sti`).

fn cli() {
    // SAFETY: masking interrupts on this CPU is always allowed in ring 0.
    unsafe { asm!("cli", options(nostack)) };
}

fn sti() {
    // SAFETY: the IDT is loaded and every vector has a handler; callers only
    // turn interrupts on outside every critical section.
    unsafe { asm!("sti", options(nostack)) };
}

/// An open critical section: interrupts are off until it is dropped.
#[must_use = "interrupts come back on when this is dropped"]
pub struct IrqOff {
    /// Not `Send` or `Sync`: the section belongs to this CPU.
    _cpu: PhantomData<*const ()>,
}

/// Open a critical section: interrupts off until the returned guard drops.
pub fn push_off() -> IrqOff {
    let were_on = enabled();
    cli();
    // Interrupts are off: only an NMI or an exception (both fatal) can run
    // between these, and only this CPU touches its own entries.
    let cpu = percpu::index();
    if DEPTH[cpu].fetch_add(1, Ordering::Relaxed) == 0 {
        WERE_ON[cpu].store(were_on, Ordering::Relaxed);
    }
    IrqOff { _cpu: PhantomData }
}

impl Drop for IrqOff {
    /// Close the section; the last one turns interrupts back on if they were
    /// on before the first (and [`disable_for_good`] hasn't run).
    fn drop(&mut self) {
        assert!(!enabled(), "interrupts were turned on inside a critical section");
        let cpu = percpu::index();
        let depth = DEPTH[cpu].load(Ordering::Relaxed);
        assert!(depth > 0, "closing a critical section that isn't open");
        DEPTH[cpu].store(depth - 1, Ordering::Relaxed);
        if depth == 1 && WERE_ON[cpu].load(Ordering::Relaxed) && !FOR_GOOD.load(Ordering::Relaxed) {
            sti();
        }
    }
}

/// How many critical sections are open on this CPU.
pub fn depth() -> usize {
    DEPTH[percpu::index()].load(Ordering::Relaxed)
}

/// Turn interrupts on, outside any critical section (boot, once everything
/// that handles them is set up).
pub fn enable() {
    assert_eq!(depth(), 0, "enabling interrupts inside a critical section");
    assert!(!FOR_GOOD.load(Ordering::Relaxed), "enabling interrupts after disable_for_good");
    sti();
}

/// Unless `ready()`, wait for an interrupt with interrupts on. `ready` runs
/// with interrupts off, then `sti; hlt`: `sti` holds interrupts off until
/// after the next instruction, so the CPU is already in `hlt` when a pending
/// one arrives and wakes it; one that makes `ready` true can't slip in
/// between the check and the halt and leave this CPU asleep. Outside every critical
/// section; interrupts are on afterwards (and stay off for good, with this
/// CPU halted, after [`disable_for_good`]).
pub fn wait_unless(ready: impl FnOnce() -> bool) {
    assert_eq!(depth(), 0, "waiting inside a critical section");
    cli();
    if FOR_GOOD.load(Ordering::Relaxed) {
        super::cpu::halt_forever();
    }
    if ready() {
        sti();
        return;
    }
    // SAFETY: the IDT is loaded and every vector has a handler; no
    // critical section is open (checked above).
    unsafe { asm!("sti", "hlt", options(nomem, nostack)) };
}

/// Turn interrupts off on this CPU; from now on no CPU turns them back on
/// (closing sections, [`enable`] and [`wait_unless`] all refuse). For the
/// crash path, which never returns to what it interrupted (it stops the
/// other CPUs itself).
pub fn disable_for_good() {
    cli();
    FOR_GOOD.store(true, Ordering::Relaxed);
}
