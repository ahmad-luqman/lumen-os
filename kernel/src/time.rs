//! Timekeeping: the TSC's frequency, calibrated at boot against the HPET (or
//! the PIT if there is none, or it doesn't work), [`now_ns`] and [`sleep`]
//! from it, and the local APIC timer ticking at [`TICK_HZ`], calibrated
//! against the TSC. It also holds the boot's origin ([`mark_boot`]), which
//! the boot timeline and the log's timestamps ([`since_boot_ns`]) count from.
//!
//! Calibration polls with interrupts off and takes the median of several
//! 10 ms windows, so one window stretched by an SMI or a descheduled vCPU
//! doesn't skew it. Every poll is bounded, so a clock that doesn't count is
//! an error, not a hang. [`now_ns`] and [`ticks`] are lock-free (interrupt
//! handlers and the crash path may call them).

use core::fmt;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use log::{info, warn};
use lumen_mm::PhysAddr;
use lumen_time::{Clock, Hz, PIT_HZ};

use crate::arch::{self, apic, idt, pit};
use crate::mm::paging;
use crate::sync::SpinLock;

/// The local APIC timer's rate.
pub const TICK_HZ: u64 = 1000;
/// Calibration windows, and how long each is.
const WINDOWS: usize = 5;
const WINDOW_NS: u64 = 10_000_000;
/// The longest a 10 ms window may take, in TSC ticks (TSC unknown yet: over
/// 1 s at up to 4 GHz), before its reference counts as stalled.
const WINDOW_BOUND_TSC: u64 = 1 << 32;
/// How long the HPET may take to tick once, in TSC ticks (over 10 ms at up
/// to 6 GHz; it ticks every 100 ns at most).
const HPET_TICK_BOUND_TSC: u64 = 1 << 26;

/// The TSC's frequency (0 until [`init`]).
static TSC_HZ: AtomicU64 = AtomicU64::new(0);
/// The local APIC timer's input frequency, after its divider (0 until
/// [`init`]).
static LAPIC_TIMER_HZ: AtomicU64 = AtomicU64::new(0);
/// What the TSC was calibrated against: 0 not yet, else a [`Reference`].
static REFERENCE: AtomicU8 = AtomicU8::new(0);
/// The TSC when `kmain` began: the boot timeline's and the log's origin (0
/// until [`mark_boot`]).
static BOOT_TSC: AtomicU64 = AtomicU64::new(0);
/// The HPET, mapped once, if the TSC could be calibrated against it.
static HPET: SpinLock<Option<HpetCounter>> = SpinLock::new(None);

/// What the TSC was calibrated against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Reference {
    Hpet = 1,
    Pit = 2,
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Reference::Hpet => "hpet",
            Reference::Pit => "pit",
        })
    }
}

/// What the TSC was calibrated against at boot, once it was.
pub fn reference() -> Option<Reference> {
    const HPET: u8 = Reference::Hpet as u8;
    const PIT: u8 = Reference::Pit as u8;
    match REFERENCE.load(Ordering::Relaxed) {
        HPET => Some(Reference::Hpet),
        PIT => Some(Reference::Pit),
        _ => None,
    }
}

/// Why calibrating against a clock failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CalError {
    /// It didn't count through a window (or isn't there).
    NoCount,
    /// It gave this implausible frequency.
    Implausible(u64),
}

impl fmt::Display for CalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalError::NoCount => f.write_str("it didn't count through a 10 ms window"),
            CalError::Implausible(hz) => write!(f, "it gave an implausible TSC frequency, {}", Hz(*hz)),
        }
    }
}

/// Why the HPET can't be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HpetError {
    NotPhysical(u64),
    Map(paging::MmioError),
    /// Its period (femtoseconds per tick) is outside the spec's range.
    Period(u32),
    /// Its counter didn't move.
    Stalled,
    /// Calibrating the TSC against it failed.
    Window(CalError),
}

impl fmt::Display for HpetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HpetError::NotPhysical(a) => write!(f, "{a:#x} is not a physical address"),
            HpetError::Map(e) => write!(f, "mapping it: {e}"),
            HpetError::Period(p) => write!(f, "its period {p} fs is out of range"),
            HpetError::Stalled => f.write_str("its counter doesn't count"),
            HpetError::Window(e) => write!(f, "calibrating against it: {e}"),
        }
    }
}

/// The HPET's main counter.
#[derive(Clone, Copy, Debug)]
pub struct HpetCounter {
    regs: u64,
    clock: Clock,
    /// The counter's width: `u64::MAX`, or `0xffff_ffff` for a 32-bit one.
    mask: u64,
}

const HPET_CAPABILITIES: usize = 0x00;
const HPET_CONFIG: usize = 0x10;
const HPET_COUNTER: usize = 0xf0;
const HPET_ENABLE: u64 = 1 << 0;
const HPET_COUNT_64: u64 = 1 << 13;

impl HpetCounter {
    /// Map the HPET at `phys`, start its counter if it isn't running, and
    /// check that it counts.
    fn new(phys: u64) -> Result<HpetCounter, HpetError> {
        let addr = PhysAddr::new(phys).ok_or(HpetError::NotPhysical(phys))?;
        let regs = paging::map_mmio(addr, 1024).map_err(HpetError::Map)?.as_u64();
        // SAFETY: `regs` maps the HPET's registers (1 KiB, uncached) for good.
        let caps = unsafe { read(regs, HPET_CAPABILITIES) };
        let period = (caps >> 32) as u32;
        let clock = lumen_time::hpet_hz(period).and_then(Clock::new).ok_or(HpetError::Period(period))?;
        let mask = if caps & HPET_COUNT_64 == 0 { 0xffff_ffff } else { u64::MAX };
        let hpet = HpetCounter { regs, clock, mask };
        // SAFETY: as above.
        let config = unsafe { read(regs, HPET_CONFIG) };
        if config & HPET_ENABLE == 0 {
            // SAFETY: as above; only the general configuration's enable bit
            // is set.
            unsafe { write(regs, HPET_CONFIG, config | HPET_ENABLE) };
        }
        let (start, t0) = (hpet.now(), arch::rdtsc());
        while hpet.now() == start {
            if arch::rdtsc().wrapping_sub(t0) > HPET_TICK_BOUND_TSC {
                return Err(HpetError::Stalled);
            }
            core::hint::spin_loop();
        }
        Ok(hpet)
    }

    pub fn now(&self) -> u64 {
        // SAFETY: `regs` maps the HPET for good (`new`).
        unsafe { read(self.regs, HPET_COUNTER) & self.mask }
    }

    /// Ticks from `earlier` to `later` (two [`now`](Self::now) readings),
    /// correct across one wrap of a 32-bit counter (about 43 s at 100 MHz):
    /// not for long-lived timestamps.
    pub fn since(&self, earlier: u64, later: u64) -> u64 {
        lumen_time::counter_delta(earlier, later, self.mask)
    }

    pub fn clock(&self) -> Clock {
        self.clock
    }
}

/// Read a 64-bit HPET register.
///
/// # Safety
/// `regs` maps the HPET's registers (1 KiB, uncached) for good.
unsafe fn read(regs: u64, offset: usize) -> u64 {
    // SAFETY: upheld by the caller; 64-bit registers, 64-bit accesses.
    unsafe { ((regs as usize + offset) as *const u64).read_volatile() }
}

/// Write a 64-bit HPET register.
///
/// # Safety
/// As for [`read`], and the write must leave the HPET in a state the kernel
/// expects.
unsafe fn write(regs: u64, offset: usize, value: u64) {
    // SAFETY: upheld by the caller.
    unsafe { ((regs as usize + offset) as *mut u64).write_volatile(value) }
}

/// The HPET, if [`init`] calibrated the TSC against it.
pub fn hpet() -> Option<HpetCounter> {
    *HPET.lock()
}

/// The median of [`WINDOWS`] samples taken by `window`; the first failure
/// if one fails.
fn median_of_windows(mut window: impl FnMut() -> Result<u64, CalError>) -> Result<u64, CalError> {
    let mut samples = [0u64; WINDOWS];
    for s in &mut samples {
        *s = window()?;
    }
    Ok(lumen_time::median(&mut samples).expect("WINDOWS samples"))
}

/// [`median_of_windows`] for TSC frequencies: `window` gives one, or `None`
/// if its reference didn't count; an implausible one fails.
fn median_tsc_hz(mut window: impl FnMut() -> Option<u64>) -> Result<u64, CalError> {
    median_of_windows(|| match window() {
        Some(hz) if lumen_time::plausible_tsc_hz(hz) => Ok(hz),
        Some(hz) => Err(CalError::Implausible(hz)),
        None => Err(CalError::NoCount),
    })
}

/// The TSC's frequency against the HPET. Call with interrupts off.
fn calibrate_with_hpet(hpet: &HpetCounter) -> Result<u64, CalError> {
    let target = hpet.clock().ticks(WINDOW_NS);
    median_tsc_hz(|| {
        let (h0, t0) = (hpet.now(), arch::rdtsc());
        loop {
            let (h, t) = (hpet.now(), arch::rdtsc());
            let elapsed = hpet.since(h0, h);
            if elapsed >= target {
                break lumen_time::frequency(t - t0, elapsed, hpet.clock().hz());
            }
            if t.wrapping_sub(t0) > WINDOW_BOUND_TSC {
                break None;
            }
            core::hint::spin_loop();
        }
    })
}

/// The TSC's frequency against the PIT's channel 2. Call with interrupts
/// off.
fn calibrate_with_pit() -> Result<u64, CalError> {
    let count = lumen_time::pit_count(WINDOW_NS).expect("a 10 ms window fits the PIT");
    median_tsc_hz(|| {
        let mut t0 = 0;
        if !pit::wait(count, || t0 = arch::rdtsc()) {
            return None;
        }
        lumen_time::frequency(arch::rdtsc() - t0, u64::from(count), PIT_HZ)
    })
}

/// Measure the TSC's frequency again now, against `reference` (with
/// interrupts off meanwhile: about 50 ms): `None` if there is no such clock
/// (no HPET the TSC was calibrated against), else what calibrating against
/// it gave. For cross-checking the boot calibration.
pub fn measure(reference: Reference) -> Option<Result<u64, CalError>> {
    let _off = arch::irq::push_off();
    match reference {
        Reference::Hpet => Some(calibrate_with_hpet(&hpet()?)),
        Reference::Pit => Some(calibrate_with_pit()),
    }
}

/// The local APIC timer's input frequency against the (calibrated) TSC.
fn calibrate_lapic_timer(tsc: Clock) -> Result<u64, CalError> {
    let window = tsc.ticks(WINDOW_NS);
    median_of_windows(|| {
        apic::timer_one_shot(u32::MAX);
        let t0 = arch::rdtsc();
        while arch::rdtsc() - t0 < window {
            core::hint::spin_loop();
        }
        let (current, t1) = (apic::timer_current(), arch::rdtsc());
        // 0: it ran out (or reads as 0), so the count says nothing.
        let counted = u64::from(u32::MAX - current);
        lumen_time::frequency(counted, t1 - t0, tsc.hz()).filter(|_| current != 0).ok_or(CalError::NoCount)
    })
}

/// The TSC's frequency against the HPET ACPI lists (kept for [`hpet`] if
/// that works), or against the PIT if there is none or it doesn't work
/// (warned about). Panics if neither works.
fn calibrate_tsc() -> (u64, Reference) {
    let mut hpet_error = None;
    if let Some(addr) = crate::acpi::info().and_then(|i| i.hpet).map(|h| h.address) {
        let calibrated = HpetCounter::new(addr)
            .and_then(|hpet| calibrate_with_hpet(&hpet).map(|hz| (hpet, hz)).map_err(HpetError::Window));
        match calibrated {
            Ok((hpet, hz)) => {
                *HPET.lock() = Some(hpet);
                return (hz, Reference::Hpet);
            }
            Err(e) => {
                warn!(target: "time", "the HPET at {addr:#x} is unusable ({e}); calibrating against the PIT");
                hpet_error = Some(e);
            }
        }
    }
    match (calibrate_with_pit(), hpet_error) {
        (Ok(hz), _) => (hz, Reference::Pit),
        (Err(e), Some(h)) => panic!("can't calibrate the TSC: the HPET is unusable ({h}), and against the PIT {e}"),
        (Err(e), None) => panic!("can't calibrate the TSC: no HPET, and against the PIT {e}"),
    }
}

/// Calibrate the TSC and start the local APIC timer at [`TICK_HZ`] (its
/// interrupts arrive once interrupts are enabled). Needs `acpi::init` and
/// `interrupts::init`; call with interrupts off. An HPET ACPI lists but that
/// doesn't work is warned about (the PIT is used instead). Panics if no
/// clock can calibrate the TSC, or the local APIC timer doesn't count or
/// can't tick at 1 kHz.
pub fn init() {
    assert!(!arch::irq::enabled(), "time::init with interrupts on");
    let (hz, reference) = calibrate_tsc();
    TSC_HZ.store(hz, Ordering::Relaxed);
    REFERENCE.store(reference as u8, Ordering::Relaxed);
    info!(target: "time", "tsc {} (calibrated against {reference})", Hz(hz));

    let tsc = Clock::new(hz).expect("calibrated");
    let timer_hz = calibrate_lapic_timer(tsc)
        .unwrap_or_else(|e| panic!("can't calibrate the local APIC timer against the TSC: {e}"));
    let count = lumen_time::periodic_count(timer_hz, TICK_HZ)
        .unwrap_or_else(|| panic!("the local APIC timer, at {}, can't tick at {}", Hz(timer_hz), Hz(TICK_HZ)));
    LAPIC_TIMER_HZ.store(timer_hz, Ordering::Relaxed);
    apic::timer_periodic(count);
    info!(
        target: "time",
        "local APIC timer {} (bus {} / {}), ticking at {}",
        Hz(timer_hz),
        Hz(timer_hz * apic::TIMER_DIVISOR),
        apic::TIMER_DIVISOR,
        Hz(TICK_HZ)
    );
}

/// The TSC as a [`Clock`], once calibrated.
pub fn tsc() -> Option<Clock> {
    Clock::new(TSC_HZ.load(Ordering::Relaxed))
}

/// The local APIC timer's input (after its divider) as a [`Clock`], once
/// calibrated.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: ktests
pub fn lapic_timer() -> Option<Clock> {
    Clock::new(LAPIC_TIMER_HZ.load(Ordering::Relaxed))
}

/// Record `tsc`, read as `kmain` began, as the boot's origin. Panics if
/// called twice (keeping the first origin).
pub fn mark_boot(tsc: u64) {
    // 0 means "not marked": a TSC of exactly 0 is stored as 1.
    let first = BOOT_TSC.compare_exchange(0, tsc.max(1), Ordering::Relaxed, Ordering::Relaxed);
    assert!(first.is_ok(), "mark_boot called twice");
}

/// The TSC when `kmain` began, once [`mark_boot`] recorded it.
pub fn boot_tsc() -> Option<u64> {
    Some(BOOT_TSC.load(Ordering::Relaxed)).filter(|&t| t != 0)
}

/// Nanoseconds since `kmain` began, once the TSC is calibrated; `None`
/// before, or if the TSC reads earlier than the origin (it was reset, or
/// another CPU's isn't in step): a zero would read as a measurement.
/// Lock-free (the logger stamps lines with it).
pub fn since_boot_ns() -> Option<u64> {
    let ticks = arch::rdtsc().checked_sub(boot_tsc()?)?;
    Some(tsc()?.ns(ticks))
}

/// Nanoseconds since the TSC was last reset (normally at CPU reset; firmware
/// or a hypervisor may set it), or 0 before [`init`]. Lock-free.
pub fn now_ns() -> u64 {
    tsc().map_or(0, |c| c.ns(arch::rdtsc()))
}

/// Wait at least `ns` nanoseconds (busy: the scheduler comes in M2).
/// Panics before [`init`].
pub fn sleep(ns: u64) {
    let tsc = tsc().expect("time::sleep before time::init");
    let start = arch::rdtsc();
    let ticks = tsc.ticks(ns);
    while arch::rdtsc().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

/// Local APIC timer interrupts so far. Lock-free.
pub fn ticks() -> u64 {
    idt::ticks()
}

crate::ktest!(
    fn tsc_calibrated_within_tolerance() {
        // Cross-check against the PIT, a different clock from the HPET the
        // boot calibrated against (the ktest machine has one). Within 2%.
        use crate::kassert;
        let hz = tsc().ok_or("TSC not calibrated")?.hz();
        if reference() != Some(Reference::Hpet) {
            return Err("the TSC wasn't calibrated against the HPET");
        }
        let other = measure(Reference::Pit).ok_or("no PIT")?.map_err(|_| "calibrating against the PIT failed")?;
        crate::ktest::diag(format_args!("tsc {} (boot), {} against the pit", Hz(hz), Hz(other)));
        kassert!(hz.abs_diff(other) * 50 <= hz);
        Ok(())
    }
);

crate::ktest!(
    fn now_ns_follows_the_hpet() {
        // `now_ns` is the TSC converted; across a 50 ms sleep it moves as
        // far as the HPET says, within 2% (a 1 ms budget: a host stall on one
        // reading doesn't fail it).
        use crate::kassert;
        let hpet = hpet().ok_or("no HPET")?;
        let (n0, h0) = (now_ns(), hpet.now());
        sleep(50_000_000);
        let (n1, h1) = (now_ns(), hpet.now());
        let by_hpet = hpet.clock().ns(hpet.since(h0, h1));
        crate::ktest::diag(format_args!("now_ns moved {} ns, the HPET {by_hpet} ns", n1 - n0));
        kassert!(n0 > 0 && (n1 - n0).abs_diff(by_hpet) * 50 <= by_hpet);
        Ok(())
    }
);

crate::ktest!(
    fn sleep_10ms_elapses() {
        // Measured with the HPET and the tick count, not the TSC that
        // `sleep` itself waits on.
        use crate::kassert;
        let hpet = hpet().ok_or("no HPET")?;
        let (h0, t0) = (hpet.now(), ticks());
        sleep(10_000_000);
        let (elapsed, ticked) = (hpet.since(h0, hpet.now()), ticks() - t0);
        let ns = hpet.clock().ns(elapsed);
        crate::ktest::diag(format_args!("slept {ns} ns by the HPET, {ticked} ticks"));
        // At least 10 ms, less 1%: the TSC was calibrated against this HPET,
        // so only window-to-window noise separates them (the 2% cross-checks
        // are against the PIT). Well under a second. The timer ticked
        // (interrupts are on); its rate is checked elsewhere.
        kassert!((9_900_000..lumen_time::NS_PER_SEC).contains(&ns));
        kassert!(ticked >= 1);
        Ok(())
    }
);

crate::ktest!(
    fn the_timer_is_programmed_for_1_khz() {
        // Periodic and unmasked on the timer vector, reloading every 1 ms of
        // its calibrated input; and counting at that rate by the HPET. This
        // checks the rate without counting interrupts (an emulator delivers
        // them late).
        use crate::{kassert, kassert_eq};
        kassert_eq!(TICK_HZ, 1000);
        let (lvt, error_lvt, initial) = apic::timer_registers();
        kassert_eq!((lvt & 0xff, lvt & (1 << 16), lvt & (1 << 17)), (u32::from(idt::TIMER), 0, 1 << 17));
        kassert_eq!(error_lvt & 0x1_00ff, u32::from(idt::LAPIC_ERROR)); // unmasked
        let timer = lapic_timer().ok_or("timer not calibrated")?;
        kassert_eq!(Some(initial), lumen_time::periodic_count(timer.hz(), 1000));
        let hpet = hpet().ok_or("no HPET")?;
        // 750 µs windows (most cross a reload, so that path counts), under
        // the 1 ms period; a window a stall stretched past 950 µs could hide
        // a whole reload, so it is taken again.
        let (window, too_long) = (hpet.clock().ticks(750_000), hpet.clock().ticks(950_000));
        let mut samples = [0u64; 7];
        let mut taken = 0;
        for _ in 0..20 {
            let _off = arch::irq::push_off();
            let (h0, c0) = (hpet.now(), apic::timer_current());
            while hpet.since(h0, hpet.now()) < window {
                core::hint::spin_loop();
            }
            let (h1, c1) = (hpet.now(), apic::timer_current());
            let elapsed = hpet.since(h0, h1);
            if elapsed >= too_long {
                continue;
            }
            let counted = lumen_time::down_counter_delta(c0, c1, initial);
            samples[taken] = lumen_time::frequency(u64::from(counted), elapsed, hpet.clock().hz()).unwrap_or(0);
            taken += 1;
            if taken == samples.len() {
                break;
            }
        }
        kassert_eq!(taken, samples.len());
        let rate = lumen_time::median(&mut samples).ok_or("no samples")?;
        crate::ktest::diag(format_args!("timer counts at {} (calibrated {})", Hz(rate), Hz(timer.hz())));
        kassert!(rate.abs_diff(timer.hz()) * 20 <= timer.hz());
        Ok(())
    }
);

crate::ktest!(
    fn a_bad_hpet_address_is_an_error() {
        // The fallback to the PIT starts from these errors (an HPET that
        // stalls or calibrates implausibly can't be staged in QEMU).
        use crate::kassert_eq;
        kassert_eq!(HpetCounter::new(u64::MAX).err(), Some(HpetError::NotPhysical(u64::MAX)));
        Ok(())
    }
);

crate::ktest!(
    fn the_timer_ticks_on_its_vector() {
        // Interrupts arrive at roughly 1 kHz: 100 in 100 ms. Loosely
        // bounded (the rate is checked by `the_timer_is_programmed_for_1_khz`):
        // under QEMU's TCG a busy-waiting guest got about 2/3 of its ticks
        // when measured, and a lock held across a period merges ticks.
        use crate::kassert;
        kassert!(arch::irq::enabled());
        let t0 = ticks();
        sleep(100_000_000);
        let ticked = ticks() - t0;
        crate::ktest::diag(format_args!("{ticked} ticks in 100 ms"));
        kassert!((40..=150).contains(&ticked));
        Ok(())
    }
);

crate::ktest!(
    fn a_held_lock_holds_off_the_timer() {
        // While a lock is held (interrupts off) no tick arrives; ticks resume
        // once it is dropped.
        use crate::{kassert, kassert_eq};
        let lock = crate::sync::SpinLock::new(());
        let guard = lock.lock();
        let t0 = ticks();
        sleep(5_000_000);
        let during = ticks() - t0;
        drop(guard);
        sleep(2_000_000);
        kassert_eq!(during, 0);
        kassert!(ticks() > t0);
        Ok(())
    }
);

crate::ktest!(
    fn timer_ticks_are_not_traced() {
        // At 1 kHz they would flood the trace ring.
        use crate::{kassert, kassert_eq};
        let ring = crate::trace::ring();
        let (t0, w0) = (ticks(), ring.written());
        sleep(20_000_000);
        kassert!(ticks() > t0);
        kassert_eq!(ring.written(), w0);
        Ok(())
    }
);
