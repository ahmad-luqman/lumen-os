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
const ICR_LOW: usize = 0x300;
const ICR_HIGH: usize = 0x310;
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
/// ICR: the IPI is still being sent.
const ICR_PENDING: u32 = 1 << 12;
/// ICR: level assert (required for every IPI but INIT de-assert).
const ICR_ASSERT: u32 = 1 << 14;
/// How many times [`send`] polls for the IPI to go out before giving up.
const ICR_SPINS: u32 = 1_000_000;
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

/// Send an interrupt on `vector` to the CPU whose local APIC id is `dest`;
/// whether it went out.
#[must_use]
pub fn send_ipi(dest: u32, vector: u8) -> bool {
    send(dest, ICR_ASSERT | u32::from(vector))
}

/// Send an NMI to the CPU whose local APIC id is `dest` (the crash reporter
/// stops the other CPUs with it); whether it went out. Lock-free.
#[must_use]
pub fn send_nmi(dest: u32) -> bool {
    send(dest, ICR_ASSERT | LVT_NMI)
}

/// Write the ICR (destination first: writing the low word sends) once the
/// last IPI has gone, then wait until this one has: false if the ICR stays
/// busy either time (bounded: the crash path must not hang on a wedged
/// APIC; it says so). Interrupts are off meanwhile, so a handler on this CPU
/// can't send one between the writes.
fn send(dest: u32, low: u32) -> bool {
    // Online CPUs' ids passed `smp::ap_main`'s check against this 8-bit
    // register field, so the crash path can't trip this.
    assert!(dest <= 0xff, "xAPIC destination {dest} doesn't fit in 8 bits");
    let _irq = super::irq::push_off();
    if !icr_idle() {
        return false;
    }
    write(ICR_HIGH, dest << 24);
    write(ICR_LOW, low);
    icr_idle()
}

/// Wait, bounded, until no IPI is being sent; whether none is.
fn icr_idle() -> bool {
    for _ in 0..ICR_SPINS {
        if read(ICR_LOW) & ICR_PENDING == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
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

/// The most inputs an I/O APIC can have that the kernel can address:
/// IOREGSEL is 8 bits, and input `i`'s entry is registers `0x10 + 2i` and
/// `0x11 + 2i`.
pub const MAX_INPUTS: u32 = 120;

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
        io.entries = (((io.read(IOAPICVER) >> 16) & 0xff) + 1).min(MAX_INPUTS);
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

    /// Redirection entry `i`. Panics if there is no entry `i`.
    pub fn entry(&mut self, i: u32) -> RedirectionEntry {
        assert!(i < self.entries, "I/O APIC has no input {i}");
        let low = self.read(IOREDTBL + 2 * i);
        let high = self.read(IOREDTBL + 2 * i + 1);
        RedirectionEntry(u64::from(low) | u64::from(high) << 32)
    }

    /// Set redirection entry `i` (its read-only bits are ignored). Panics if
    /// there is no entry `i`.
    pub fn set_entry(&mut self, i: u32, entry: RedirectionEntry) {
        assert!(i < self.entries, "I/O APIC has no input {i}");
        let value = entry.0 & !RedirectionEntry::READ_ONLY;
        // High word first, low word last: the low word holds the mask bit, so
        // a masked input is never unmasked with a half-written destination
        // (retarget a live input by masking it first).
        self.write(IOREDTBL + 2 * i + 1, (value >> 32) as u32);
        self.write(IOREDTBL + 2 * i, value as u32);
    }

    /// Read entry `i`, change it with `f`, write it back (read-modify-write;
    /// the caller holds `&mut self`, so nothing interleaves). Returns what
    /// was written (the read-only bits clear). Panics if there is no entry
    /// `i`.
    pub fn update_entry(&mut self, i: u32, f: impl FnOnce(&mut RedirectionEntry)) -> RedirectionEntry {
        let mut e = self.entry(i);
        f(&mut e);
        self.set_entry(i, e);
        RedirectionEntry(e.0 & !RedirectionEntry::READ_ONLY)
    }

    /// Mask every input (entries keep their other fields).
    pub fn mask_all(&mut self) {
        for i in 0..self.entries {
            self.update_entry(i, |e| e.set_masked(true));
        }
    }
}

/// How an I/O APIC input's interrupt is delivered (bits 8-10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    Fixed = 0b000,
    LowestPriority = 0b001,
    Smi = 0b010,
    Nmi = 0b100,
    Init = 0b101,
    ExtInt = 0b111,
}

/// An input's polarity (bit 13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    ActiveHigh,
    ActiveLow,
}

/// An input's trigger mode (bit 15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Edge,
    Level,
}

/// An I/O APIC redirection entry (Intel 82093AA datasheet, 3.2.4): what
/// one input delivers, to whom and how. Built field by field; the I/O APIC
/// itself sets the two read-only bits (remote IRR, delivery status), which
/// [`IoApic::set_entry`] never writes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RedirectionEntry(u64);

impl RedirectionEntry {
    const VECTOR: u64 = 0xff;
    const DELIVERY_SHIFT: u32 = 8;
    const DELIVERY: u64 = 0b111 << Self::DELIVERY_SHIFT;
    const LOGICAL: u64 = 1 << 11;
    const DELIVERY_STATUS: u64 = 1 << 12;
    const ACTIVE_LOW: u64 = 1 << 13;
    const REMOTE_IRR: u64 = 1 << 14;
    const LEVEL: u64 = 1 << 15;
    const MASKED: u64 = 1 << 16;
    const DEST_SHIFT: u32 = 56;
    const DEST: u64 = 0xff << Self::DEST_SHIFT;
    const READ_ONLY: u64 = Self::DELIVERY_STATUS | Self::REMOTE_IRR;

    /// Masked, with every other field 0: vector 0, fixed delivery, physical
    /// destination 0, active high, edge triggered.
    pub const MASKED_DEFAULT: RedirectionEntry = RedirectionEntry(Self::MASKED);

    /// An ISA interrupt (MADT INTI `flags`: bits 0-1 polarity, 2-3 trigger;
    /// 0b00 is the bus's default, active high and edge for ISA, 0b11 is
    /// active low or level) delivered as `vector` to the CPU with APIC id
    /// `dest`: fixed delivery, physical destination, `masked` or not.
    pub fn isa(vector: u8, dest: u8, flags: u16, masked: bool) -> RedirectionEntry {
        let mut e = Self::MASKED_DEFAULT;
        e.set_vector(vector);
        e.set_destination(dest);
        e.set_polarity(if flags & 0b11 == 0b11 { Polarity::ActiveLow } else { Polarity::ActiveHigh });
        e.set_trigger(if (flags >> 2) & 0b11 == 0b11 { Trigger::Level } else { Trigger::Edge });
        e.set_masked(masked);
        e
    }

    /// The raw 64 bits.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
    pub fn bits(self) -> u64 {
        self.0
    }

    fn set(&mut self, bit: u64, on: bool) {
        if on {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }

    pub fn vector(self) -> u8 {
        (self.0 & Self::VECTOR) as u8
    }

    pub fn set_vector(&mut self, vector: u8) {
        self.0 = (self.0 & !Self::VECTOR) | u64::from(vector);
    }

    /// The delivery mode, or `None` for a reserved encoding.
    pub fn delivery(self) -> Option<Delivery> {
        let raw = (self.0 & Self::DELIVERY) >> Self::DELIVERY_SHIFT;
        [Delivery::Fixed, Delivery::LowestPriority, Delivery::Smi, Delivery::Nmi, Delivery::Init, Delivery::ExtInt]
            .into_iter()
            .find(|&d| d as u64 == raw)
    }

    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
    pub fn set_delivery(&mut self, d: Delivery) {
        self.0 = (self.0 & !Self::DELIVERY) | (d as u64) << Self::DELIVERY_SHIFT;
    }

    /// Whether the destination is a logical one (else a physical APIC id).
    pub fn logical(self) -> bool {
        self.0 & Self::LOGICAL != 0
    }

    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
    pub fn set_logical(&mut self, logical: bool) {
        self.set(Self::LOGICAL, logical);
    }

    pub fn polarity(self) -> Polarity {
        if self.0 & Self::ACTIVE_LOW != 0 { Polarity::ActiveLow } else { Polarity::ActiveHigh }
    }

    pub fn set_polarity(&mut self, p: Polarity) {
        self.set(Self::ACTIVE_LOW, p == Polarity::ActiveLow);
    }

    pub fn trigger(self) -> Trigger {
        if self.0 & Self::LEVEL != 0 { Trigger::Level } else { Trigger::Edge }
    }

    pub fn set_trigger(&mut self, t: Trigger) {
        self.set(Self::LEVEL, t == Trigger::Level);
    }

    pub fn masked(self) -> bool {
        self.0 & Self::MASKED != 0
    }

    pub fn set_masked(&mut self, masked: bool) {
        self.set(Self::MASKED, masked);
    }

    /// The destination: a physical APIC id (or a logical set).
    pub fn destination(self) -> u8 {
        (self.0 >> Self::DEST_SHIFT) as u8
    }

    pub fn set_destination(&mut self, dest: u8) {
        self.0 = (self.0 & !Self::DEST) | u64::from(dest) << Self::DEST_SHIFT;
    }

    /// Read-only: a level-triggered interrupt was accepted and awaits its
    /// EOI.
    pub fn remote_irr(self) -> bool {
        self.0 & Self::REMOTE_IRR != 0
    }

    /// Read-only: an interrupt is waiting to be delivered.
    pub fn delivery_pending(self) -> bool {
        self.0 & Self::DELIVERY_STATUS != 0
    }
}

impl core::fmt::Debug for RedirectionEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RedirectionEntry")
            .field("vector", &self.vector())
            .field("delivery", &self.delivery())
            .field("logical", &self.logical())
            .field("polarity", &self.polarity())
            .field("trigger", &self.trigger())
            .field("masked", &self.masked())
            .field("destination", &self.destination())
            .field("remote_irr", &self.remote_irr())
            .field("delivery_pending", &self.delivery_pending())
            .finish()
    }
}
