//! Local APIC (xAPIC, memory-mapped) and I/O APIC registers.
//!
//! The kernel maps the registers (`map_mmio`, uncached) and hands the
//! virtual addresses to [`set_lapic`] and [`IoApic::new`]. The local APIC's
//! address is kept in a static so the interrupt path can send an EOI without
//! a lock. Every CPU's local APIC sits at the same address (each CPU reaches
//! its own there): the boot CPU maps it once ([`set_lapic`]), and each other
//! CPU then turns its own on ([`enable_local`]).

use core::sync::atomic::{AtomicU64, Ordering};

use super::cpu::rdmsr;
use super::idt::{LAPIC_ERROR, SPURIOUS, TIMER};

const IA32_APIC_BASE: u32 = 0x1b;
const BASE_ENABLE: u64 = 1 << 11;
const BASE_X2APIC: u64 = 1 << 10;
const BASE_ADDR_MASK: u64 = 0x000f_ffff_ffff_f000;

// Local APIC register offsets.
const ID: usize = 0x20;
const TPR: usize = 0x80;
const EOI: usize = 0xb0;
const ISR: usize = 0x100;
const SVR: usize = 0xf0;
const ESR: usize = 0x280;
const LVT_TIMER: usize = 0x320;
const LVT_LINT0: usize = 0x350;
const LVT_LINT1: usize = 0x360;
const LVT_ERROR: usize = 0x370;
const TIMER_INITIAL: usize = 0x380;
const TIMER_CURRENT: usize = 0x390;
const TIMER_DIVIDE: usize = 0x3e0;

const SVR_ENABLE: u32 = 1 << 8;
const LVT_MASKED: u32 = 1 << 16;
/// LVT delivery mode NMI (bits 8-10 = 0b100).
const LVT_NMI: u32 = 0b100 << 8;
const LVT_PERIODIC: u32 = 1 << 17;
/// Divide configuration 0b0011: the timer counts at the bus clock / 16.
const DIVIDE_BY_16: u32 = 0b0011;
/// What [`DIVIDE_BY_16`] divides by.
pub const TIMER_DIVISOR: u64 = 16;

/// The local APIC's mapped registers (0 until [`set_lapic`]).
static LAPIC: AtomicU64 = AtomicU64::new(0);

/// What `IA32_APIC_BASE` says about this CPU's local APIC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApicBase {
    pub phys: u64,
    /// The APIC is on (globally enabled).
    pub enabled: bool,
    /// It is in x2APIC mode: its registers are MSRs, not memory.
    pub x2apic: bool,
}

/// Read `IA32_APIC_BASE`.
pub fn apic_base() -> ApicBase {
    // SAFETY: every x86-64 CPU has IA32_APIC_BASE.
    let v = unsafe { rdmsr(IA32_APIC_BASE) };
    ApicBase { phys: v & BASE_ADDR_MASK, enabled: v & BASE_ENABLE != 0, x2apic: v & BASE_X2APIC != 0 }
}

fn reg(offset: usize) -> *mut u32 {
    let base = LAPIC.load(Ordering::Relaxed);
    assert_ne!(base, 0, "local APIC used before set_lapic");
    (base as usize + offset) as *mut u32
}

fn read(offset: usize) -> u32 {
    // SAFETY: `set_lapic` was given the mapped registers; local APIC
    // registers are 32-bit and read with 32-bit accesses.
    unsafe { reg(offset).read_volatile() }
}

fn write(offset: usize, value: u32) {
    // SAFETY: as for `read`; writes to these registers only affect the local
    // APIC's own state.
    unsafe { reg(offset).write_volatile(value) }
}

/// Use the local APIC registers at `virt` from now on, and enable the boot
/// CPU's ([`enable_local`]).
///
/// # Safety
/// `virt` maps the local APIC registers (4 KiB, uncached) for the kernel's
/// whole life, and the APIC is in xAPIC mode.
pub unsafe fn set_lapic(virt: u64) {
    assert!(LAPIC.swap(virt, Ordering::Relaxed) == 0, "set_lapic called twice");
    enable_local();
}

/// Enable this CPU's local APIC: spurious interrupts on [`SPURIOUS`], errors
/// on [`LAPIC_ERROR`], LINT0 (the 8259s' line) and the timer masked, LINT1
/// delivering NMIs (the platform's NMI line, as wired on PCs), every
/// priority accepted. Panics before [`set_lapic`].
pub fn enable_local() {
    // Software-enable first: while the APIC is software-disabled, every LVT
    // entry is forced masked and writes clearing the mask are ignored.
    write(SVR, SVR_ENABLE | u32::from(SPURIOUS));
    write(LVT_TIMER, LVT_MASKED);
    write(LVT_LINT0, LVT_MASKED);
    write(LVT_LINT1, LVT_NMI);
    write(LVT_ERROR, u32::from(LAPIC_ERROR));
    // The ESR latches errors on a write; clear what firmware left.
    write(ESR, 0);
    write(ESR, 0);
    write(TPR, 0);
    write(EOI, 0);
}

/// Has [`set_lapic`] run?
pub fn lapic_ready() -> bool {
    LAPIC.load(Ordering::Relaxed) != 0
}

/// This CPU's local APIC id (xAPIC: bits 24-31).
pub fn lapic_id() -> u32 {
    read(ID) >> 24
}

/// End the interrupt being handled. Lock-free; does nothing before
/// [`set_lapic`] (no APIC interrupt can have arrived then).
pub fn eoi() {
    if lapic_ready() {
        write(EOI, 0);
    }
}

/// Is `vector` in service at the local APIC (delivered by it and not yet
/// EOI'd)? False before [`set_lapic`].
pub fn in_service(vector: u8) -> bool {
    lapic_ready() && read(ISR + 0x10 * usize::from(vector / 32)) & (1 << (vector % 32)) != 0
}

/// Latch and read the error status (for the error interrupt).
pub fn error_status() -> u32 {
    write(ESR, 0);
    read(ESR)
}

/// Run the timer once from `count`, masked (for calibration).
pub fn timer_one_shot(count: u32) {
    write(TIMER_DIVIDE, DIVIDE_BY_16);
    write(LVT_TIMER, LVT_MASKED);
    write(TIMER_INITIAL, count);
}

/// The timer's current count.
pub fn timer_current() -> u32 {
    read(TIMER_CURRENT)
}

/// Interrupt on [`TIMER`] every `count` timer ticks (the bus clock /
/// [`TIMER_DIVISOR`]).
pub fn timer_periodic(count: u32) {
    write(TIMER_DIVIDE, DIVIDE_BY_16);
    write(LVT_TIMER, LVT_PERIODIC | u32::from(TIMER));
    write(TIMER_INITIAL, count);
}

/// The timer's LVT entry, error LVT entry and initial count (for ktests).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub fn timer_registers() -> (u32, u32, u32) {
    (read(LVT_TIMER), read(LVT_ERROR), read(TIMER_INITIAL))
}

/// An I/O APIC's mapped registers: an index/data pair, so every access
/// takes `&mut self` (the two steps must not interleave).
#[derive(Debug)]
pub struct IoApic {
    base: u64,
    entries: u32,
}

/// The mask bit of an I/O APIC redirection entry.
pub const REDIRECTION_MASKED: u64 = 1 << 16;

const IOREGSEL: usize = 0x00;
const IOWIN: usize = 0x10;
const IOAPICVER: u32 = 0x01;
const IOREDTBL: u32 = 0x10;

impl IoApic {
    /// # Safety
    /// `virt` maps an I/O APIC's registers (at least 32 bytes, uncached)
    /// for the kernel's whole life, and nothing else drives that I/O APIC.
    pub unsafe fn new(virt: u64) -> IoApic {
        let mut io = IoApic { base: virt, entries: 0 };
        // IOREGSEL is 8 bits: entries past 119 can't be addressed.
        io.entries = (((io.read(IOAPICVER) >> 16) & 0xff) + 1).min(120);
        io
    }

    fn read(&mut self, index: u32) -> u32 {
        // SAFETY: `new`'s contract; select, then read the window (32-bit).
        unsafe {
            ((self.base as usize + IOREGSEL) as *mut u32).write_volatile(index);
            ((self.base as usize + IOWIN) as *const u32).read_volatile()
        }
    }

    fn write(&mut self, index: u32, value: u32) {
        // SAFETY: as for `read`.
        unsafe {
            ((self.base as usize + IOREGSEL) as *mut u32).write_volatile(index);
            ((self.base as usize + IOWIN) as *mut u32).write_volatile(value);
        }
    }

    /// How many redirection entries (inputs) it has.
    pub fn entries(&self) -> u32 {
        self.entries
    }

    /// Redirection entry `i` (64 bits). Panics if there is no entry `i`.
    pub fn entry(&mut self, i: u32) -> u64 {
        assert!(i < self.entries, "I/O APIC has no input {i}");
        u64::from(self.read(IOREDTBL + 2 * i)) | u64::from(self.read(IOREDTBL + 2 * i + 1)) << 32
    }

    /// Set redirection entry `i`. Panics if there is no entry `i`.
    pub fn set_entry(&mut self, i: u32, value: u64) {
        assert!(i < self.entries, "I/O APIC has no input {i}");
        // Low word last: it holds the mask bit.
        self.write(IOREDTBL + 2 * i + 1, (value >> 32) as u32);
        self.write(IOREDTBL + 2 * i, value as u32);
    }

    /// Mask every input (entries keep their other fields).
    pub fn mask_all(&mut self) {
        for i in 0..self.entries {
            let e = self.entry(i);
            self.set_entry(i, e | REDIRECTION_MASKED);
        }
    }
}

/// An I/O APIC redirection entry: deliver `vector` to the CPU with APIC id
/// `dest`, fixed delivery, physical destination, with the ISA override's
/// polarity and trigger `flags` (MADT INTI flags: bits 0-1 polarity, 2-3
/// trigger; 0 = the bus's default, edge/active-high for ISA), `masked` or not.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: device drivers (M3)
pub fn redirection(vector: u8, dest: u8, flags: u16, masked: bool) -> u64 {
    let active_low = flags & 0b11 == 0b11;
    let level = (flags >> 2) & 0b11 == 0b11;
    u64::from(vector)
        | u64::from(active_low) << 13
        | u64::from(level) << 15
        | if masked { REDIRECTION_MASKED } else { 0 }
        | u64::from(dest) << 56
}
