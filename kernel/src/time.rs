//! Timekeeping: the TSC's frequency, calibrated at boot against the HPET (or
//! the PIT if there is none, or it doesn't work), [`now_ns`] and [`sleep`]
//! from it, and the local APIC timer ticking at [`TICK_HZ`], calibrated
//! against the TSC. It also holds the boot's origin ([`mark_boot`]), which
//! the boot timeline and the log's timestamps ([`since_boot_ns`]) count from.
//!
//! Calibration polls with interrupts off and takes the median of several
//! 10 ms windows. Each window starts and ends with a reading of the reference
//! clock's counter between two TSC reads (for the PIT too: its counter runs on
//! past the end), so a vCPU paused while waiting for the end doesn't stretch
//! it; a window whose start or end reading itself was paused (a wide gap
//! between its TSC reads) is taken again. Every poll is bounded, so a clock
//! that doesn't count is an error, not a hang. [`now_ns`] and [`ticks`] are lock-free (interrupt
//! handlers and the crash path may call them).

use core::fmt;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use log::{info, warn};
use lumen_mm::PhysAddr;
use lumen_time::{Clock, Hz, PIT_HZ, SteadyError, Window};

#[cfg(feature = "ktest")]
use crate::arch::pit::Pause;
use crate::arch::{self, apic, idt, pit};
use crate::mm::paging;
use crate::sync::SpinLock;

/// The local APIC timer's rate.
pub const TICK_HZ: u64 = 1000;
/// Calibration windows, and how long each is.
pub const WINDOWS: usize = 5;
/// How many windows a TSC calibration may take to get [`WINDOWS`] steady ones.
const ATTEMPTS: usize = 4 * WINDOWS;
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
/// The boot calibration's windows (None until [`init`]).
static BOOT_CALIBRATION: SpinLock<Option<Calibration>> = SpinLock::new(None);

/// A TSC calibration: the median of its windows' frequencies, the (steady)
/// windows in the order they were taken, and how many unsteady ones were
/// taken again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Calibration {
    pub hz: u64,
    pub windows: [Window; WINDOWS],
    pub retaken: usize,
}

impl fmt::Display for Calibration {
    /// `<hz> Hz [tsc/reference/polls/start_gap/end_gap ...] retook <n>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} Hz [", self.hz)?;
        for (i, w) in self.windows.iter().enumerate() {
            let sep = if i == 0 { "" } else { " " };
            write!(f, "{sep}{}/{}/{}/{}/{}", w.tsc, w.reference, w.polls, w.start_gap, w.end_gap)?;
        }
        write!(f, "] retook {}", self.retaken)
    }
}

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
    /// Only `steady` of `attempts` windows were read promptly at both ends.
    Unsteady { steady: usize, attempts: usize },
}

impl fmt::Display for CalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalError::NoCount => f.write_str("it didn't count through a 10 ms window"),
            CalError::Implausible(hz) => write!(f, "it gave an implausible TSC frequency, {}", Hz(*hz)),
            CalError::Unsteady { steady, attempts } => {
                write!(f, "only {steady} of {attempts} windows were read promptly at both ends")
            }
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

/// The boot calibration, with its windows (None before [`init`]).
pub fn boot_calibration() -> Option<Calibration> {
    *BOOT_CALIBRATION.lock()
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

/// The TSC's frequency against a reference counting at `reference_hz`: the
/// median of [`WINDOWS`] steady windows from `take` (an unsteady one is taken
/// again, up to [`ATTEMPTS`] in all); an implausible one fails.
fn median_tsc_hz(reference_hz: u64, take: impl FnMut() -> Result<Window, CalError>) -> Result<Calibration, CalError> {
    let mut windows = [Window::default(); WINDOWS];
    let retaken = lumen_time::steady_windows(&mut windows, ATTEMPTS, take).map_err(|e| match e {
        SteadyError::Take(e) => e,
        SteadyError::Unsteady { steady, attempts } => CalError::Unsteady { steady, attempts },
    })?;
    let mut hz = [0u64; WINDOWS];
    for (h, w) in hz.iter_mut().zip(&windows) {
        *h = match lumen_time::frequency(w.tsc, w.reference, reference_hz) {
            Some(hz) if lumen_time::plausible_tsc_hz(hz) => hz,
            Some(hz) => return Err(CalError::Implausible(hz)),
            None => return Err(CalError::NoCount),
        };
    }
    Ok(Calibration { hz: lumen_time::median(&mut hz).expect("WINDOWS samples"), windows, retaken })
}

/// The TSC's frequency against the HPET. Call with interrupts off.
fn calibrate_with_hpet(hpet: &HpetCounter) -> Result<Calibration, CalError> {
    let target = hpet.clock().ticks(WINDOW_NS);
    median_tsc_hz(hpet.clock().hz(), || hpet_window(hpet, target, || {}))
}

/// One window of at least `target` HPET ticks. `pause` runs between the final
/// counter read and the TSC read that closes it (a no-op but in tests; a pause
/// there widens the end's gap).
fn hpet_window(hpet: &HpetCounter, target: u64, pause: impl FnOnce()) -> Result<Window, CalError> {
    let pre = arch::rdtsc();
    let (h0, t0) = (hpet.now(), arch::rdtsc());
    let mut polls = 0;
    loop {
        let before = arch::rdtsc();
        let h = hpet.now();
        polls += 1;
        let elapsed = hpet.since(h0, h);
        if elapsed >= target {
            pause();
            let t = arch::rdtsc();
            return Ok(Window { tsc: t - t0, reference: elapsed, polls, start_gap: t0 - pre, end_gap: t - before });
        }
        if arch::rdtsc().wrapping_sub(t0) > WINDOW_BOUND_TSC {
            return Err(CalError::NoCount);
        }
        core::hint::spin_loop();
    }
}

/// The TSC's frequency against the PIT's channel 2. Call with interrupts
/// off.
fn calibrate_with_pit() -> Result<Calibration, CalError> {
    let count = lumen_time::pit_count(WINDOW_NS).expect("a 10 ms window fits the PIT");
    median_tsc_hz(PIT_HZ, || pit_window(count, |_| {}))
}

/// One window of `count` PIT ticks (`pause`: see [`pit::wait`]).
fn pit_window(count: u16, pause: impl FnMut(pit::Pause)) -> Result<Window, CalError> {
    let w = pit::wait(count, pause).ok_or(CalError::NoCount)?;
    let (start, end) = (w.start, w.end);
    let tsc = end.tsc_after - start.tsc_after;
    // An end reading that may have wrapped past 0 again can't be placed:
    // as uncertain as the whole window, so it is taken again.
    let running = w.last_running_tsc - start.tsc_after;
    let late = end.tsc_after - w.last_running_tsc;
    let end_gap =
        if lumen_time::pit_end_unambiguous(start.count, running, late) { end.tsc_after - end.tsc_before } else { tsc };
    Ok(Window {
        tsc,
        reference: lumen_time::pit_elapsed(start.count, end.count),
        polls: w.polls,
        start_gap: start.tsc_after - start.tsc_before,
        end_gap,
    })
}

/// Measure the TSC's frequency again now, against `reference` (with
/// interrupts off meanwhile: about 50 ms): `None` if there is no such clock
/// (no HPET the TSC was calibrated against), else what calibrating against
/// it gave. For cross-checking the boot calibration.
pub fn measure(reference: Reference) -> Option<Result<Calibration, CalError>> {
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
fn calibrate_tsc() -> (Calibration, Reference) {
    let mut hpet_error = None;
    if let Some(addr) = crate::acpi::info().and_then(|i| i.hpet).map(|h| h.address) {
        let calibrated = HpetCounter::new(addr)
            .and_then(|hpet| calibrate_with_hpet(&hpet).map(|cal| (hpet, cal)).map_err(HpetError::Window));
        match calibrated {
            Ok((hpet, cal)) => {
                *HPET.lock() = Some(hpet);
                return (cal, Reference::Hpet);
            }
            Err(e) => {
                warn!(target: "time", "the HPET at {addr:#x} is unusable ({e}); calibrating against the PIT");
                hpet_error = Some(e);
            }
        }
    }
    match (calibrate_with_pit(), hpet_error) {
        (Ok(cal), _) => (cal, Reference::Pit),
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
    let (cal, reference) = calibrate_tsc();
    let hz = cal.hz;
    *BOOT_CALIBRATION.lock() = Some(cal);
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
        let boot = boot_calibration().ok_or("no boot calibration")?;
        let pit = measure(Reference::Pit).ok_or("no PIT")?.map_err(|_| "calibrating against the PIT failed")?;
        // Diagnostics only (windows: tsc/reference/polls/start_gap/end_gap): the HPET measured
        // again too, to tell a PIT-only error from a boot calibration that was off.
        crate::ktest::diag(format_args!("tsc boot (hpet) {boot}"));
        crate::ktest::diag(format_args!("tsc now against the pit {pit}"));
        match measure(Reference::Hpet) {
            Some(Ok(hpet)) => crate::ktest::diag(format_args!("tsc now against the hpet {hpet}")),
            _ => crate::ktest::diag(format_args!("tsc now against the hpet: failed")),
        }
        kassert!(hz.abs_diff(pit.hz) * 50 <= hz);
        Ok(())
    }
);

crate::ktest!(
    fn a_late_look_at_the_pit_doesnt_stretch_its_window() {
        // Paused 2 ms after the count ran out, before its end is read, as a
        // host descheduling the vCPU would: the counter ran on past 0, so the
        // window still gives the TSC's frequency (a yes/no "has it run out"
        // read that late made it 20% high).
        use crate::kassert;
        let hz = tsc().ok_or("TSC not calibrated")?.hz();
        let count = lumen_time::pit_count(WINDOW_NS).ok_or("no PIT count")?;
        // A steady one, as calibration takes them: a loaded host can pause a
        // reading too, which isn't what this checks.
        let mut steady = [Window::default()];
        {
            let _off = arch::irq::push_off();
            let take = || {
                pit_window(count, |at| {
                    if at == Pause::Ended {
                        sleep(2_000_000)
                    }
                })
            };
            lumen_time::steady_windows(&mut steady, ATTEMPTS, take).map_err(|_| "no steady PIT window")?;
        }
        let [w] = steady;
        let measured = lumen_time::frequency(w.tsc, w.reference, PIT_HZ).ok_or("no frequency")?;
        crate::ktest::diag(format_args!("{measured} Hz against {hz} Hz; window {w:?}"));
        kassert!(w.reference >= u64::from(count) + 2000); // ran on about 2386 ticks
        kassert!(hz.abs_diff(measured) * 200 <= hz); // within 0.5%
        Ok(())
    }
);

crate::ktest!(
    fn a_pit_end_read_after_it_could_wrap_is_not_steady() {
        // Paused 400 ms after the count ran out: its counter (16 bits, 55 ms
        // around) may have wrapped, so the window can't be placed and would be
        // taken again. (Far more than 4 × the 10 ms it ran, even if a loaded
        // host stretched that.)
        use crate::kassert;
        let count = lumen_time::pit_count(WINDOW_NS).ok_or("no PIT count")?;
        let w = {
            let _off = arch::irq::push_off();
            pit_window(count, |at| {
                if at == Pause::Ended {
                    sleep(400_000_000)
                }
            })
            .map_err(|_| "no PIT window")?
        };
        kassert!(!w.is_steady());
        kassert!(w.end_gap == w.tsc);
        Ok(())
    }
);

crate::ktest!(
    fn a_paused_pit_reading_is_taken_again() {
        // The first two windows' end readings are paused 1 ms between latching
        // the counter and reading the TSC, as a host descheduling the vCPU
        // there would: their ends are 10% uncertain, so both are taken again
        // (a loaded host may add more), and the calibration still gives the
        // TSC's frequency.
        use crate::kassert;
        let hz = tsc().ok_or("TSC not calibrated")?.hz();
        let count = lumen_time::pit_count(WINDOW_NS).ok_or("no PIT count")?;
        let mut taken = 0;
        let cal = {
            let _off = arch::irq::push_off();
            median_tsc_hz(PIT_HZ, || {
                taken += 1;
                let paused = taken <= 2;
                pit_window(count, |at| {
                    if paused && at == Pause::EndReading {
                        sleep(1_000_000);
                    }
                })
            })
            .map_err(|_| "calibrating against the PIT failed")?
        };
        crate::ktest::diag(format_args!("{cal}"));
        kassert!(cal.retaken >= 2);
        kassert!(cal.windows.iter().all(Window::is_steady));
        kassert!(hz.abs_diff(cal.hz) * 200 <= hz);
        Ok(())
    }
);

crate::ktest!(
    fn a_paused_hpet_reading_is_not_steady() {
        // Paused 1 ms between the last HPET read and the TSC read closing the
        // window: its end is 10% uncertain, so it would be taken again.
        use crate::kassert;
        let hpet = hpet().ok_or("no HPET")?;
        let target = hpet.clock().ticks(WINDOW_NS);
        let w = {
            let _off = arch::irq::push_off();
            hpet_window(&hpet, target, || sleep(1_000_000)).map_err(|_| "no HPET window")?
        };
        crate::ktest::diag(format_args!("paused {w:?}"));
        kassert!(!w.is_steady());
        kassert!(w.end_gap >= tsc().ok_or("TSC not calibrated")?.ticks(1_000_000)); // the pause is in it
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
        // Interrupts arrive on the timer's vector, and not faster than 1 kHz
        // allows: 100 in 100 ms, at most. The rate itself is checked by
        // `the_timer_is_programmed_for_1_khz` (against the HPET): QEMU's TCG
        // delivers only some of a busy-waiting guest's ticks, 46 to 81 here
        // under a loaded host (#45), and a lock held across a period merges them.
        use crate::kassert;
        kassert!(arch::irq::enabled());
        let t0 = ticks();
        sleep(100_000_000);
        let ticked = ticks() - t0;
        crate::ktest::diag(format_args!("{ticked} ticks in 100 ms"));
        kassert!((1..=150).contains(&ticked));
        Ok(())
    }
);

crate::ktest!(
    fn a_held_lock_holds_off_the_timer() {
        // While a lock is held (interrupts off) no tick arrives; ticks resume
        // once it is dropped. The first can come late under QEMU's TCG on a
        // loaded host (a fixed 2 ms wait missed it once, #45), so this waits up
        // to 100 ms for it.
        use crate::{kassert, kassert_eq};
        let lock = crate::sync::SpinLock::new(());
        let guard = lock.lock();
        let t0 = ticks();
        sleep(5_000_000);
        let during = ticks() - t0;
        drop(guard);
        let deadline = now_ns() + 100_000_000;
        while ticks() == t0 && now_ns() < deadline {
            core::hint::spin_loop();
        }
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
