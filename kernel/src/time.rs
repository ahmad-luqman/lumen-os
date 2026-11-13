//! Timekeeping: the TSC's frequency, calibrated at boot against the HPET (or
//! the PIT if there is none, or it doesn't work), [`now_ns`] and [`sleep`]
//! from it, and the local APIC timer ticking at [`TICK_HZ`], calibrated
//! against the TSC. It also holds the boot's origin ([`mark_boot`]), which
//! the boot timeline and the log's timestamps ([`since_boot_ns`]) count from.
//!
//! Calibration polls with interrupts off and takes the median of several
//! 10 ms windows. Each TSC calibration window starts and ends with a reading of
//! the reference clock's counter between two TSC reads (for the PIT too: its counter runs on
//! past the end), so a vCPU paused while waiting for the end doesn't stretch
//! it; a window whose start or end reading itself was paused (a wide gap
//! between its TSC reads) is taken again. Every poll is bounded, so a clock
//! that doesn't count is an error, not a hang. [`now_ns`] and [`ticks`] are lock-free (interrupt
//! handlers and the crash path may call them).

use core::fmt;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};

use log::{info, warn};
use lumen_mm::PhysAddr;
use lumen_time::{Clock, Hz, PIT_HZ, Pause, PitBadReadback, SteadyError, Window};

use crate::arch::{self, apic, idt, pit};
use crate::mm::paging;
use crate::sync::SpinLock;

/// The local APIC timer's rate.
pub const TICK_HZ: u64 = 1000;
/// Calibration windows, and how long each is.
const WINDOWS: usize = 5;
const WINDOW_NS: u64 = 10_000_000;
/// How many windows a TSC calibration may take to get [`WINDOWS`] steady ones.
const ATTEMPTS: usize = 4 * WINDOWS;
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
/// The boot calibration (None until [`init`]).
static BOOT_CALIBRATION: SpinLock<Option<Calibration>> = SpinLock::new(None);

/// A TSC calibration: the median of its windows' frequencies, the (steady)
/// windows in the order they were taken, and how many unsteady ones were set
/// aside for others (and the last of those).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Calibration {
    pub hz: u64,
    pub windows: [Window; WINDOWS],
    pub retaken: usize,
    pub last_retaken: Option<Window>,
}

impl fmt::Display for Calibration {
    /// `<hz> Hz [tsc/reference/polls/start_gap/end_gap ...] retook <n>`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} Hz [", self.hz)?;
        for (i, w) in self.windows.iter().enumerate() {
            let sep = if i == 0 { "" } else { " " };
            write!(f, "{sep}{w}")?;
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
    /// Only `steady` of `attempts` windows were read promptly at both ends;
    /// `last` is the last one that wasn't.
    Unsteady { steady: usize, attempts: usize, last: Window },
    /// The PIT's counter read back values that don't fit how it was
    /// programmed.
    BadReadback(PitBadReadback),
}

impl From<SteadyError<CalError>> for CalError {
    fn from(e: SteadyError<CalError>) -> Self {
        match e {
            SteadyError::Take(e) => e,
            SteadyError::Unsteady { steady, attempts, last } => CalError::Unsteady { steady, attempts, last },
        }
    }
}

impl fmt::Display for CalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalError::NoCount => f.write_str("it didn't count through a 10 ms window"),
            CalError::Implausible(hz) => write!(f, "it gave an implausible TSC frequency, {}", Hz(*hz)),
            CalError::Unsteady { steady, attempts, last } => write!(
                f,
                "only {steady} of {attempts} windows were read promptly at both ends (the last that \
                 wasn't, tsc/reference/polls/start_gap/end_gap: {last})"
            ),
            CalError::BadReadback(e) => write!(f, "{e}"),
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

/// A TSC calibration against a reference counting at `reference_hz`: the
/// median frequency of [`WINDOWS`] steady windows from `take` (an unsteady one
/// is set aside and another taken, up to [`ATTEMPTS`] in all); an implausible
/// one fails.
fn median_tsc_hz(reference_hz: u64, take: impl FnMut() -> Result<Window, CalError>) -> Result<Calibration, CalError> {
    let mut windows = [Window::default(); WINDOWS];
    let aside = lumen_time::steady_windows(&mut windows, ATTEMPTS, take)?;
    let mut hz = [0u64; WINDOWS];
    for (h, w) in hz.iter_mut().zip(&windows) {
        *h = match w.tsc_hz(reference_hz) {
            Some(hz) if lumen_time::plausible_tsc_hz(hz) => hz,
            Some(hz) => return Err(CalError::Implausible(hz)),
            None => return Err(CalError::NoCount),
        };
    }
    let hz = lumen_time::median(&mut hz).expect("WINDOWS samples");
    Ok(Calibration { hz, windows, retaken: aside.count, last_retaken: aside.last })
}

/// The TSC's frequency against the HPET. Call with interrupts off.
fn calibrate_with_hpet(hpet: &HpetCounter) -> Result<Calibration, CalError> {
    let target = hpet.clock().ticks(WINDOW_NS);
    median_tsc_hz(hpet.clock().hz(), || hpet_window(hpet, target, |_| {}))
}

/// One window of at least `target` HPET ticks, each end a counter read between
/// two TSC reads. `pause` is called at the `Pause` points in those readings
/// (a no-op but in tests); [`Pause::EndBeforeRead`] on every poll, since any
/// read may be the last.
fn hpet_window(hpet: &HpetCounter, target: u64, mut pause: impl FnMut(Pause)) -> Result<Window, CalError> {
    let pre = arch::rdtsc();
    pause(Pause::StartBeforeRead);
    let h0 = hpet.now();
    pause(Pause::StartAfterRead);
    let t0 = arch::rdtsc();
    let mut polls = 0;
    loop {
        let before = arch::rdtsc();
        pause(Pause::EndBeforeRead);
        let h = hpet.now();
        polls += 1;
        let elapsed = hpet.since(h0, h);
        if elapsed >= target {
            pause(Pause::EndAfterRead);
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

/// One window of channel 2 started on `count` (see [`lumen_time::pit_window`]);
/// `pause`: see [`pit::wait`].
fn pit_window(count: u16, pause: impl FnMut(Pause)) -> Result<Window, CalError> {
    let w = pit::wait(count, pause).ok_or(CalError::NoCount)?;
    lumen_time::pit_window(count, &w).map_err(CalError::BadReadback)
}

/// Measure the TSC's frequency again now, against `reference` (with
/// interrupts off meanwhile: at least 50 ms, more if windows are taken
/// again): `None` if there is no such clock (no HPET the TSC was calibrated
/// against), else what calibrating against
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
    if let Some(last) = cal.last_retaken {
        info!(
            target: "time",
            "tsc calibration took {} windows again (read too late to trust; the last, \
             tsc/reference/polls/start_gap/end_gap: {last})",
            cal.retaken
        );
    }

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

/// Wait up to `ns` nanoseconds for [`ticks`] to move past `seen`; whether it
/// did. Panics before [`init`] (the timer doesn't tick yet, and the wait
/// couldn't be bounded).
#[must_use]
pub fn wait_for_tick(seen: u64, ns: u64) -> bool {
    let limit = tsc().expect("time::wait_for_tick before time::init").ticks(ns);
    let start = arch::rdtsc();
    while ticks() == seen && arch::rdtsc().wrapping_sub(start) < limit {
        core::hint::spin_loop();
    }
    ticks() != seen
}

/// A window pause for the ktests: sleeps `ns` at `at`, as a host descheduling
/// the vCPU there would.
#[cfg(feature = "ktest")]
fn sleep_at(at: Pause, ns: u64) -> impl FnMut(Pause) {
    move |p| {
        if p == at {
            sleep(ns)
        }
    }
}

/// `take()` with interrupts off, as calibration runs it; an error is shown as
/// a diag and becomes `what`.
#[cfg(feature = "ktest")]
fn off_or_diag<T>(what: &'static str, take: impl FnOnce() -> Result<T, CalError>) -> Result<T, &'static str> {
    let _off = arch::irq::push_off();
    take().map_err(|e| {
        crate::ktest::diag(format_args!("{e}"));
        what
    })
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
        // Diagnostics (windows: tsc/reference/polls/start_gap/end_gap), the
        // boot's first so they show even if the PIT fails; the HPET measured
        // again too, to tell a PIT-only error from a boot calibration that
        // was off.
        let boot = boot_calibration().ok_or("no boot calibration")?;
        crate::ktest::diag(format_args!("tsc boot (hpet) {boot}"));
        let pit = measure(Reference::Pit).ok_or("no PIT")?.map_err(|e| {
            crate::ktest::diag(format_args!("against the pit: {e}"));
            "calibrating against the PIT failed"
        })?;
        crate::ktest::diag(format_args!("tsc now against the pit {pit}"));
        match measure(Reference::Hpet) {
            Some(Ok(hpet)) => crate::ktest::diag(format_args!("tsc now against the hpet {hpet}")),
            Some(Err(e)) => crate::ktest::diag(format_args!("tsc now against the hpet: failed ({e})")),
            None => crate::ktest::diag(format_args!("tsc now against the hpet: no HPET")),
        }
        kassert!(hz.abs_diff(pit.hz) * 50 <= hz);
        Ok(())
    }
);

/// A PIT window paused 2 ms at `at` (Started or Ended, which shouldn't
/// matter): the pause happened (its readings show the 2 ms of ticks), and a
/// steady window taken as calibration takes them still gives the TSC's
/// frequency, within 1% (half of that is what a steady window's gaps allow;
/// a loaded host can pause a reading too, which this isn't about).
#[cfg(feature = "ktest")]
fn a_pit_window_paused_at_still_gives_the_tsc(at: Pause) -> Result<(), &'static str> {
    use crate::kassert;
    let hz = tsc().ok_or("TSC not calibrated")?.hz();
    let count = lumen_time::pit_count(WINDOW_NS).ok_or("no PIT count")?;
    let mut pause = sleep_at(at, 2_000_000);
    let w = {
        let _off = arch::irq::push_off();
        pit::wait(count, &mut pause).ok_or("no PIT")?
    };
    // About 2386 ticks counted off before the start reading, or past 0 before
    // the end reading. Counter arithmetic, wrapping: a host stall on top of
    // the pause can run the count out and on before the start is read.
    let paused = match at {
        Pause::Started => count.wrapping_sub(w.start.count),
        _ => 0u16.wrapping_sub(w.end.count),
    };
    crate::ktest::diag(format_args!("paused at {at:?}: {paused} ticks; readings {w:?}"));
    kassert!(paused >= 2000);
    let mut window = [Window::default()];
    off_or_diag("no steady PIT window", || {
        lumen_time::steady_windows(&mut window, ATTEMPTS, || pit_window(count, &mut pause)).map_err(CalError::from)
    })?;
    let [w] = window;
    let measured = w.tsc_hz(PIT_HZ).ok_or("no frequency")?;
    crate::ktest::diag(format_args!("{measured} Hz against {hz} Hz; window {w}"));
    kassert!(hz.abs_diff(measured) * 100 <= hz);
    Ok(())
}

crate::ktest!(
    fn a_late_look_at_the_pit_doesnt_stretch_its_window() {
        // Paused 2 ms after the count ran out, before its end is read, as a
        // host descheduling the vCPU would: the counter ran on past 0, so the
        // window still gives the TSC's frequency (a yes/no "has it run out"
        // read that late made it 20% high).
        a_pit_window_paused_at_still_gives_the_tsc(Pause::Ended)
    }
);

crate::ktest!(
    fn a_late_start_reading_doesnt_shift_a_pit_window() {
        // Paused 2 ms after the count started, before its start is read: the
        // window runs from that reading, not from when the count was set, so
        // it still gives the TSC's frequency (assuming the full count from the
        // start made it 20% low).
        a_pit_window_paused_at_still_gives_the_tsc(Pause::Started)
    }
);

crate::ktest!(
    fn a_paused_pit_reading_is_not_steady() {
        // Paused 1 ms inside a counter reading, before or after the counter
        // is read, at the start and at the end: that end is 10% uncertain, so
        // the window would be taken again.
        use crate::kassert;
        let count = lumen_time::pit_count(WINDOW_NS).ok_or("no PIT count")?;
        let ms = tsc().ok_or("TSC not calibrated")?.ticks(1_000_000);
        for at in [Pause::StartBeforeRead, Pause::StartAfterRead, Pause::EndBeforeRead, Pause::EndAfterRead] {
            let w = off_or_diag("no PIT window", || pit_window(count, sleep_at(at, 1_000_000)))?;
            crate::ktest::diag(format_args!("paused at {at:?}: {w}"));
            kassert!(!w.is_steady());
            let start = matches!(at, Pause::StartBeforeRead | Pause::StartAfterRead);
            kassert!(if start { w.start_gap } else { w.end_gap } >= ms); // the pause is in it
        }
        Ok(())
    }
);

crate::ktest!(
    fn a_pit_start_read_after_it_ran_out_is_not_steady() {
        // Paused 15 ms after the count started, before or inside its start
        // reading: the 10 ms count can have run out (and on, past 0) by the
        // time it is read, so the window can't be placed and would be taken
        // again. Not a bad read-back: that would be an error, fatal at a boot
        // without an HPET.
        use crate::kassert;
        let count = lumen_time::pit_count(WINDOW_NS).ok_or("no PIT count")?;
        for at in [Pause::Started, Pause::StartBeforeRead, Pause::StartAfterRead] {
            let w = off_or_diag("no PIT window", || pit_window(count, sleep_at(at, 15_000_000)))?;
            crate::ktest::diag(format_args!("paused at {at:?}: {w}"));
            kassert!(!w.is_steady());
        }
        Ok(())
    }
);

crate::ktest!(
    fn a_pit_end_read_after_it_could_wrap_is_not_steady() {
        // Paused 400 ms after the count ran out: its counter (16 bits, 55 ms
        // around) may have come round past its start value, so the window
        // can't be placed and would be taken again. (Far more than 4 × the
        // 10 ms it ran, even if a loaded host stretched that.)
        use crate::kassert;
        let count = lumen_time::pit_count(WINDOW_NS).ok_or("no PIT count")?;
        let w = off_or_diag("no PIT window", || pit_window(count, sleep_at(Pause::Ended, 400_000_000)))?;
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
        let cal = off_or_diag("calibrating against the PIT failed", || {
            median_tsc_hz(PIT_HZ, || {
                taken += 1;
                if taken <= 2 {
                    pit_window(count, sleep_at(Pause::EndAfterRead, 1_000_000))
                } else {
                    pit_window(count, |_| {})
                }
            })
        })?;
        let shown = alloc::format!("{cal}");
        crate::ktest::diag(format_args!("{shown}"));
        kassert!(cal.retaken >= 2);
        kassert!(cal.last_retaken.is_some_and(|w| !w.is_steady()));
        kassert!(cal.windows.iter().all(Window::is_steady));
        // As clocks shows it (Calibration's Display).
        kassert!(shown.ends_with(&alloc::format!("] retook {}", cal.retaken)));
        kassert!(hz.abs_diff(cal.hz) * 100 <= hz); // within 1%
        Ok(())
    }
);

crate::ktest!(
    fn calibration_gives_up_when_no_window_is_steady() {
        // A reference whose every window has a paused end: after ATTEMPTS
        // windows, an error that says so and shows the last one taken.
        use crate::kassert_eq;
        let mut taken = 0;
        let result = median_tsc_hz(PIT_HZ, || {
            taken += 1;
            Ok(Window { tsc: 10_000_000 + taken, reference: 11_932, polls: 1, start_gap: 0, end_gap: 1_000_000 })
        });
        kassert_eq!(taken, 20); // ATTEMPTS, as documented
        let last = Window { tsc: 10_000_020, reference: 11_932, polls: 1, start_gap: 0, end_gap: 1_000_000 };
        kassert_eq!(result, Err(CalError::Unsteady { steady: 0, attempts: ATTEMPTS, last }));
        Ok(())
    }
);

crate::ktest!(
    fn a_paused_hpet_reading_is_not_steady() {
        // Paused 1 ms inside an HPET reading, before or after the counter is
        // read, at the start and at the end (EndBeforeRead pauses every poll,
        // the last too): that end is 10% uncertain, so the window would be
        // taken again.
        use crate::kassert;
        let hpet = hpet().ok_or("no HPET")?;
        let target = hpet.clock().ticks(WINDOW_NS);
        let ms = tsc().ok_or("TSC not calibrated")?.ticks(1_000_000);
        for at in [Pause::StartBeforeRead, Pause::StartAfterRead, Pause::EndBeforeRead, Pause::EndAfterRead] {
            let w = off_or_diag("no HPET window", || hpet_window(&hpet, target, sleep_at(at, 1_000_000)))?;
            crate::ktest::diag(format_args!("paused at {at:?}: {w}"));
            kassert!(!w.is_steady());
            let start = matches!(at, Pause::StartBeforeRead | Pause::StartAfterRead);
            kassert!(if start { w.start_gap } else { w.end_gap } >= ms); // the pause is in it
        }
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
        // Interrupts arrive on the timer's vector, and not much faster than
        // 1 kHz (100 in 100 ms; the bound, 150, leaves room). The rate itself
        // is checked by `the_timer_is_programmed_for_1_khz` (against the
        // HPET): QEMU's TCG delivers only some of a busy-waiting guest's ticks,
        // 29 to 81 in #45's stress runs, and a lock held across a period merges
        // them.
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
        let resumed = wait_for_tick(t0, 100_000_000);
        kassert_eq!(during, 0);
        kassert!(resumed);
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
