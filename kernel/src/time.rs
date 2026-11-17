//! Timekeeping: the TSC's frequency, calibrated at boot against the HPET (or
//! the PIT if there is none, or it doesn't work), [`now_ns`], [`sleep`] and
//! [`spin`] from it, and the local APIC timer ticking at [`TICK_HZ`],
//! calibrated against the TSC (on the boot CPU; each other CPU starts its
//! own with the same count, [`start_timer_here`]). It also holds the boot's
//! origin ([`mark_boot`]), which the boot timeline and the log's timestamps
//! ([`since_boot_ns`]) count from.
//!
//! Calibration polls with interrupts off and takes the median of several
//! 10 ms windows. Each TSC calibration window starts and ends with a reading of
//! the reference clock's counter between two TSC reads (for the PIT too: its
//! counter runs on past the end), so a vCPU paused while waiting for the end
//! doesn't stretch it; a window whose start or end reading itself was paused
//! (a wide gap between its TSC reads) is taken again, spaced out, an HPET one
//! slow to read longer. The local APIC timer's windows have the same shape
//! against the TSC: the timer started, and its count read, each between two
//! TSC reads, and a paused one taken again, spaced out (#129). Short of steady
//! windows, the least unsteady fill in, and each boot calibration is judged
//! by how far off its median can be against its clock's tolerance ([`report`],
//! #139): at most INFO within it, a warning past it, a panic with no bound
//! left. Every poll is bounded, so a clock that doesn't count is an error, not
//! a hang. [`now_ns`] and [`ticks`] are lock-free (interrupt
//! handlers and the crash path may call them).

use core::fmt;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};

use log::{info, warn};
use lumen_mm::PhysAddr;
use lumen_time::{Clock, Hz, PIT_HZ, Pause, PitBadReadback, SetAside, Window};

use crate::arch::{self, apic, idt, pit};
use crate::mm::paging;
use crate::sync::SpinLock;

/// The local APIC timer's rate.
pub const TICK_HZ: u64 = 1000;
/// Calibration windows, and how long each is.
const WINDOWS: usize = 5;
const WINDOW_NS: u64 = 10_000_000;
/// How many windows a calibration (the TSC's, or the local APIC timer's) may
/// take to get [`WINDOWS`] steady ones.
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
/// The local APIC timer's initial count for [`TICK_HZ`], published once the
/// boot CPU's timer runs (0 before): the other CPUs start theirs with it
/// (every local APIC counts the same bus clock).
static TIMER_COUNT: AtomicU32 = AtomicU32::new(0);
/// What the TSC was calibrated against: 0 not yet, else a [`Reference`].
static REFERENCE: AtomicU8 = AtomicU8::new(0);
/// The TSC when `kmain` began: the boot timeline's and the log's origin (0
/// until [`mark_boot`]).
static BOOT_TSC: AtomicU64 = AtomicU64::new(0);
/// The HPET, mapped once, if the TSC could be calibrated against it.
static HPET: SpinLock<Option<HpetCounter>> = SpinLock::new(None);
/// The boot calibration (None until [`init`]).
static BOOT_CALIBRATION: SpinLock<Option<Calibration>> = SpinLock::new(None);
/// The local APIC timer's boot calibration (None until [`init`]).
static BOOT_TIMER_CALIBRATION: SpinLock<Option<Calibration>> = SpinLock::new(None);

/// A calibration: the median of its windows' frequencies (the TSC's against a
/// reference, or the local APIC timer's against the TSC), the windows (the
/// steady ones first, in the order they were taken; if attempts ran out, the
/// least unsteady fill the rest), how many unsteady windows were taken (each
/// followed by another, unless attempts ran out; filling in, some are kept)
/// and the last of those, and which rate the windows give.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Calibration {
    pub hz: u64,
    pub windows: [Window; WINDOWS],
    pub retaken: usize,
    pub last_retaken: Option<Window>,
    pub measured: Measured,
}

/// Which rate a calibration's windows give, which decides how far off each
/// can be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Measured {
    /// The TSC's, against a reference ([`Window::tsc_error_ppm`]).
    Tsc,
    /// The local APIC timer's, against the TSC ([`Window::gap_ppm`]).
    AgainstTsc,
}

impl Calibration {
    /// How many of its windows are steady (the first ones).
    pub fn steady(&self) -> usize {
        self.windows.iter().filter(|w| w.is_steady()).count()
    }

    /// The most its median can be off, in parts per million of the true
    /// rate ([`lumen_time::median_bound_ppm_by`], with each window's bound
    /// for the rate it gives).
    pub fn median_bound_ppm(&self) -> u64 {
        match self.measured {
            Measured::Tsc => lumen_time::median_bound_ppm_by(&self.windows, Window::tsc_error_ppm),
            Measured::AgainstTsc => lumen_time::median_bound_ppm(&self.windows),
        }
    }
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
    /// Its windows' stalls left the median with no bound under 100%: the
    /// rate it gives says nothing ([`Verdict::Unusable`]).
    NoInformation,
    /// The PIT's counter read back values that don't fit how it was
    /// programmed.
    BadReadback(PitBadReadback),
}

impl fmt::Display for CalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalError::NoCount => f.write_str("it didn't count through a 10 ms window"),
            CalError::Implausible(hz) => write!(f, "it gave an implausible TSC frequency, {}", Hz(*hz)),
            CalError::NoInformation => {
                f.write_str("its windows were stalled as long as themselves, so the rate it gives says nothing")
            }
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

/// The local APIC timer's boot calibration, with its windows (None before
/// [`init`]).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: a ktest
pub fn boot_timer_calibration() -> Option<Calibration> {
    *BOOT_TIMER_CALIBRATION.lock()
}

/// The HPET, if [`init`] calibrated the TSC against it.
pub fn hpet() -> Option<HpetCounter> {
    *HPET.lock()
}

/// A TSC calibration against a reference counting at `reference_hz`: the
/// median frequency of [`WINDOWS`] windows from `take`. An unsteady one is
/// set aside and another taken after [`tsc_retake_pause`], up to [`ATTEMPTS`]
/// in all; if attempts run out first, the least unsteady fill the rest
/// ([`lumen_time::steadiest_windows`]) and [`tsc_report`] judges how far off
/// the median can be (#139: a loaded host's stalls can outlast every retake,
/// and the PIT, which an unsteady HPET used to fall back to, stalls with
/// it). An implausible frequency, or a reference that didn't count, fails.
fn median_tsc_hz(reference_hz: u64, take: impl FnMut() -> Result<Window, CalError>) -> Result<Calibration, CalError> {
    let ruler = core::cell::Cell::new(u64::MAX);
    median_tsc_hz_with(reference_hz, take, &ruler, || tsc_retake_pause(ruler.get()))
}

/// [`median_tsc_hz`] with its own wait before a retake (ktests count them):
/// `ruler` is kept at the fewest TSC ticks any window taken so far had.
fn median_tsc_hz_with(
    reference_hz: u64,
    mut take: impl FnMut() -> Result<Window, CalError>,
    ruler: &core::cell::Cell<u64>,
    pause: impl FnMut(),
) -> Result<Calibration, CalError> {
    let take = || {
        let w = take()?;
        ruler.set(ruler.get().min(w.tsc));
        Ok(w)
    };
    let got = lumen_time::steadiest_windows::<CalError, WINDOWS>(ATTEMPTS, take, pause)?;
    median_calibration(*got.windows(), got.aside(), Measured::Tsc, |w| match w.tsc_hz(reference_hz) {
        Some(hz) if lumen_time::plausible_tsc_hz(hz) => Ok(hz),
        Some(hz) => Err(CalError::Implausible(hz)),
        None => Err(CalError::NoCount),
    })
}

/// How many windows' worth of TSC ticks the TSC's calibration waits before a
/// window taken again: the TSC isn't calibrated yet, but every window spans
/// at least [`WINDOW_NS`] of the reference, so the fewest TSC ticks any took
/// (a stall only adds to a window) is at least that long: two of them, about
/// 20 ms, as the local APIC timer's retakes wait ([`TIMER_RETRY_SPACING_NS`],
/// where #129's loaded KVM boots showed back-to-back retakes running out
/// inside one burst of stalls and spaced ones not).
const TSC_RETRY_SPACING_WINDOWS: u64 = 2;

/// The wait before a TSC calibration window taken again:
/// [`TSC_RETRY_SPACING_WINDOWS`] × `ruler` TSC ticks, busy (interrupts are
/// off at boot); none before a window has been taken.
fn tsc_retake_pause(ruler: u64) {
    if ruler == u64::MAX {
        return;
    }
    let (start, ticks) = (arch::rdtsc(), ruler.saturating_mul(TSC_RETRY_SPACING_WINDOWS));
    while arch::rdtsc().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

/// A calibration from `windows` (the steady ones first) and the unsteady ones
/// taken: the median of the frequencies `rate` gives them, or the first error
/// it gives.
fn median_calibration(
    windows: [Window; WINDOWS],
    aside: SetAside,
    measured: Measured,
    rate: impl Fn(&Window) -> Result<u64, CalError>,
) -> Result<Calibration, CalError> {
    let mut hz = [0u64; WINDOWS];
    for (h, w) in hz.iter_mut().zip(&windows) {
        *h = rate(w)?;
    }
    let hz = lumen_time::median(&mut hz).expect("WINDOWS samples");
    Ok(Calibration { hz, windows, retaken: aside.count, last_retaken: aside.last, measured })
}

/// A [`Pause`] hook that stalls each window's end by `stall.end` parts per
/// million of how long it ran, and its start (from the second window) by
/// `stall.start` ppm of how long the previous one ran: busy, by raw TSC
/// ticks, so before the TSC is calibrated too. The boot's calibrations take
/// it from `lumen.stall_tsc_windows=<end ppm>[,<start ppm>]` and
/// `lumen.stall_timer_windows=...` (a deliberate fault for scenarios, like the
/// shell's `crash`), so a real boot reaches the fill-in, its warning and its
/// panic. (Under the timer's bound, [`Window::gap_ppm`], an end stall alone
/// never fills a window, which counts it: a start stall can.) With both 0 it
/// does nothing (no TSC read inside a bracket).
fn stall_window_ends(stall: Stall) -> impl FnMut(Pause) {
    let (mut started, mut last_run) = (0u64, 0u64);
    let spin = |ticks: u64| {
        let from = arch::rdtsc();
        while arch::rdtsc().wrapping_sub(from) < ticks {
            core::hint::spin_loop();
        }
    };
    let share =
        |ticks: u64, ppm: u64| u64::try_from(u128::from(ticks) * u128::from(ppm) / 1_000_000).unwrap_or(u64::MAX);
    move |at| match at {
        Pause::StartAfterRead if stall != Stall::NONE => {
            spin(share(last_run, stall.start));
            started = arch::rdtsc();
        }
        Pause::EndAfterRead if stall != Stall::NONE => {
            let now = arch::rdtsc();
            last_run = now.wrapping_sub(started);
            spin(share(last_run, stall.end));
        }
        _ => {}
    }
}

/// How [`stall_window_ends`] stalls each window, in parts per million.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stall {
    end: u64,
    start: u64,
}

impl Stall {
    const NONE: Stall = Stall { end: 0, start: 0 };
}

/// The `lumen.stall_*_windows=<end ppm>[,<start ppm>]` value for `key` (see
/// [`stall_window_ends`]), none without one; one that isn't numbers is warned
/// about and ignored.
fn stall_from_cmdline(key: &str) -> Stall {
    let Some(v) = crate::cmdline::value(key) else { return Stall::NONE };
    let mut parts = v.splitn(2, ',').map(str::parse::<u64>);
    match (parts.next(), parts.next()) {
        (Some(Ok(end)), None) => Stall { end, start: 0 },
        (Some(Ok(end)), Some(Ok(start))) => Stall { end, start },
        _ => {
            warn!(target: "time", "{key}={v}: not <end ppm>[,<start ppm>]; ignored");
            Stall::NONE
        }
    }
}

/// How long an HPET window may get ([`lumen_time::stretched_for_gaps`]): 16
/// windows' worth, 160 ms (the 32-bit counter wraps after 43 s).
const MOST_HPET_STRETCH: u64 = 16;

/// The TSC's frequency against the HPET, `pause` called at each window's
/// [`Pause`] points (see [`hpet_window`]). A window whose ends were slow to
/// read is taken again longer, up to [`MOST_HPET_STRETCH`] times (#139: on
/// KVM a normal window's gaps already take 0.23% of it, and a slowed host's
/// HPET reads took 20 times as long, every window unsteady however late it
/// was taken; the PIT's 16-bit counter can't be stretched past 11 ms). Call
/// with interrupts off.
fn calibrate_with_hpet(hpet: &HpetCounter, mut pause: impl FnMut(Pause)) -> Result<Calibration, CalError> {
    let base = hpet.clock().ticks(WINDOW_NS);
    let mut target = base;
    median_tsc_hz(hpet.clock().hz(), || {
        let w = hpet_window(hpet, target, &mut pause)?;
        target = lumen_time::stretched_for_gaps(target, &w, base.saturating_mul(MOST_HPET_STRETCH));
        Ok(w)
    })
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

/// The TSC's frequency against the PIT's channel 2, `pause` as for
/// [`calibrate_with_hpet`]. Call with interrupts off.
fn calibrate_with_pit(mut pause: impl FnMut(Pause)) -> Result<Calibration, CalError> {
    let count = lumen_time::pit_count(WINDOW_NS).expect("a 10 ms window fits the PIT");
    median_tsc_hz(PIT_HZ, || pit_window(count, &mut pause))
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
        Reference::Hpet => Some(calibrate_with_hpet(&hpet()?, |_| {})),
        Reference::Pit => Some(calibrate_with_pit(|_| {})),
    }
}

/// How long the local APIC timer's calibration waits before taking a window
/// again after an unsteady one: 20 ms. On a loaded KVM runner the vCPU
/// stalled next to the timer's register accesses in bursts lasting hundreds
/// of ms (#129: in one boot 17 of 20 windows taken back to back were
/// unsteady); spaced, 20 attempts span about 600 ms.
const TIMER_RETRY_SPACING_NS: u64 = 20_000_000;

/// The local APIC timer's input frequency against the (calibrated) TSC: the
/// median of [`WINDOWS`] windows from [`timer_window`], `pause` called at
/// each one's [`Pause`] points. A window with a paused end is taken again,
/// after [`timer_retake_pause`], up to [`ATTEMPTS`] in all; if attempts run
/// out first, the least unsteady windows make up the rest
/// ([`lumen_time::steadiest_windows`]), and [`init`] judges how far off the
/// median can be ([`report`]). Call with interrupts off.
fn calibrate_lapic_timer(tsc: Clock, mut pause: impl FnMut(Pause)) -> Result<Calibration, CalError> {
    let window = tsc.ticks(WINDOW_NS);
    calibrate_timer_with(tsc, || timer_window(window, &mut pause))
}

/// [`calibrate_lapic_timer`] with its windows from `take` (ktests make them
/// up), against `tsc`.
fn calibrate_timer_with(tsc: Clock, take: impl FnMut() -> Result<Window, CalError>) -> Result<Calibration, CalError> {
    let got = lumen_time::steadiest_windows::<CalError, WINDOWS>(ATTEMPTS, take, || timer_retake_pause(tsc))?;
    median_calibration(*got.windows(), got.aside(), Measured::AgainstTsc, |w| {
        w.reference_hz(tsc.hz()).ok_or(CalError::NoCount)
    })
}

/// The wait before a timer calibration window taken again:
/// [`TIMER_RETRY_SPACING_NS`] by `tsc`, the clock being calibrated against,
/// busy (interrupts are off at boot).
fn timer_retake_pause(tsc: Clock) {
    let (start, ticks) = (arch::rdtsc(), tsc.ticks(TIMER_RETRY_SPACING_NS));
    while arch::rdtsc().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

/// What a boot calibration is of, for [`report`]: the TSC against a
/// reference, or the local APIC timer against the TSC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Calibrated {
    Tsc(Reference),
    Timer,
}

/// How far off the TSC's boot calibration may be before boot warns, in ppm:
/// 1%, half of what its tightest consumers allow. `now_ns` against the HPET,
/// and the boot calibration against the PIT measured again, are each checked
/// within 2% (`now_ns_follows_the_hpet`, `tsc_calibrated_within_tolerance`):
/// half for this calibration, half for the other clock's measurement.
const TSC_TOLERANCE_PPM: u64 = 10_000;

/// How far off the local APIC timer's boot calibration may be before boot
/// warns, in ppm: 2%. Its tightest consumer, `the_timer_is_programmed_for_1_khz`,
/// checks the rate the timer counts at (against the HPET) within 5% of its
/// calibration, which inherits the TSC's error (up to [`TSC_TOLERANCE_PPM`],
/// 1%): this leaves 2% for the test's own one-shot windows. (#139's loaded
/// KVM boot had 2 of 20 windows steady and a bound of 0.7%, harmless for a
/// tick that paces preemption, yet warned under the old "fewer than 3 of 5
/// steady" rule, failing scenarios that expect no warning.)
const TIMER_TOLERANCE_PPM: u64 = 20_000;

/// A bound of this many ppm or more says nothing about the rate: the middle
/// windows' stalls were as long as themselves.
const NO_INFORMATION_PPM: u64 = 1_000_000;

impl Calibrated {
    fn tolerance_ppm(self) -> u64 {
        match self {
            Calibrated::Tsc(_) => TSC_TOLERANCE_PPM,
            Calibrated::Timer => TIMER_TOLERANCE_PPM,
        }
    }

    /// Its name in a log line ("tsc calibration ...").
    fn name(self) -> &'static str {
        match self {
            Calibrated::Tsc(_) => "tsc",
            Calibrated::Timer => "local APIC timer",
        }
    }

    /// What a window's ends read, for "a stall next to ...".
    fn reads(self) -> &'static str {
        match self {
            Calibrated::Tsc(Reference::Hpet) => "the HPET's reads",
            Calibrated::Tsc(Reference::Pit) => "the PIT's reads",
            Calibrated::Timer => "the timer's register accesses",
        }
    }

    /// What it is calibrated against.
    fn against(self) -> &'static str {
        match self {
            Calibrated::Tsc(Reference::Hpet) => "the HPET",
            Calibrated::Tsc(Reference::Pit) => "the PIT",
            Calibrated::Timer => "the TSC",
        }
    }
}

/// What [`init`] says about a boot calibration ([`report`]), and at what
/// level.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    /// Every window steady at the first try: nothing to say.
    Clean,
    /// Windows taken again, its median within its clock's tolerance (INFO,
    /// with the bound): steady ones, or enough least-unsteady ones filling
    /// in (#139).
    Retook,
    /// Its median may be off by more than its clock's tolerance (WARN, with
    /// the bound): its consumers may be off too. A warning, not a panic: a
    /// rate off by a few percent still runs the system.
    Imprecise,
    /// The bound is [`NO_INFORMATION_PPM`] or more: the rate says nothing (a
    /// tick from it could come arbitrarily fast or slow, a `sleep` last any
    /// time). The HPET then gives way to the PIT; otherwise `init` panics, as
    /// when a clock doesn't count at all.
    Unusable,
}

/// A boot calibration judged: what of, the verdict, the calibration and its
/// median's bound in ppm.
#[derive(Clone, Copy, Debug)]
struct Report {
    of: Calibrated,
    verdict: Verdict,
    cal: Calibration,
    bound_ppm: u64,
}

/// How `init` reports `cal`, a calibration `of` a clock: by its median's
/// bound ([`Calibration::median_bound_ppm`]) against the clock's tolerance,
/// whatever the steady windows' count (with most steady, the bound is under
/// 0.51%, within either tolerance).
fn report(of: Calibrated, cal: &Calibration) -> Report {
    let bound_ppm = cal.median_bound_ppm();
    let verdict = if bound_ppm >= NO_INFORMATION_PPM {
        Verdict::Unusable
    } else if bound_ppm > of.tolerance_ppm() {
        Verdict::Imprecise
    } else if cal.retaken == 0 {
        Verdict::Clean
    } else {
        Verdict::Retook
    };
    Report { of, verdict, cal: *cal, bound_ppm }
}

impl Report {
    /// The level `init` logs it at: none for [`Verdict::Clean`] (nothing to
    /// say) or [`Verdict::Unusable`] (a panic, or the PIT, instead).
    fn level(&self) -> Option<log::Level> {
        match self.verdict {
            Verdict::Clean | Verdict::Unusable => None,
            Verdict::Retook => Some(log::Level::Info),
            Verdict::Imprecise => Some(log::Level::Warn),
        }
    }
}

/// `ppm` as a percentage, rounded up to a tenth: `0.7`.
struct Percent(u64);

impl fmt::Display for Percent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tenths = self.0.div_ceil(1_000);
        write!(f, "{}.{}", tenths / 10, tenths % 10)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Report { of, cal, bound_ppm, .. } = *self;
        let (name, steady, bound) = (of.name(), cal.steady(), Percent(bound_ppm));
        match self.verdict {
            Verdict::Clean => write!(f, "{name} calibration: every window steady"),
            Verdict::Retook => write!(
                f,
                "{name} calibration took {} windows again, {steady} of {WINDOWS} kept steady, median within \
                 {bound}% (read too late to trust; the last, tsc/reference/polls/start_gap/end_gap: {})",
                cal.retaken,
                cal.last_retaken.unwrap_or_default()
            ),
            Verdict::Imprecise => write!(
                f,
                "{name} calibration: only {steady} of {ATTEMPTS} windows read promptly ({WINDOWS} wanted; a stall \
                 next to {}), so its rate may be off by up to {bound}%, over the {}% allowed \
                 (tsc/reference/polls/start_gap/end_gap: {cal})",
                of.reads(),
                Percent(of.tolerance_ppm())
            ),
            Verdict::Unusable => write!(
                f,
                "can't calibrate the {name} against {}: only {steady} of {ATTEMPTS} windows read promptly \
                 ({WINDOWS} wanted), and the middle ones' stalls were as long as themselves, so the rate says nothing \
                 (tsc/reference/polls/start_gap/end_gap: {cal})",
                of.against()
            ),
        }
    }
}

/// One window of the local APIC timer against the TSC, at least `window` TSC
/// ticks long: the timer started (one-shot, masked, from `u32::MAX`) between
/// two TSC reads, and its count read between two more, as [`hpet_window`]
/// reads the HPET. So `reference` is the timer's ticks, and `start_gap` and
/// `end_gap` are how far off each end can be (a vCPU paused between the
/// timer's register access and the TSC read next to it widens them). `pause`
/// is called at the [`Pause`] points (a no-op but in tests). Call with
/// interrupts off. A count of 0 (it ran out, or reads as 0) is
/// [`CalError::NoCount`].
fn timer_window(window: u64, mut pause: impl FnMut(Pause)) -> Result<Window, CalError> {
    apic::timer_prepare_one_shot();
    let pre = arch::rdtsc();
    pause(Pause::StartBeforeRead);
    apic::timer_start(u32::MAX);
    pause(Pause::StartAfterRead);
    let t0 = arch::rdtsc();
    let before = loop {
        let now = arch::rdtsc();
        if now.wrapping_sub(t0) >= window {
            break now;
        }
        core::hint::spin_loop();
    };
    pause(Pause::EndBeforeRead);
    let current = apic::timer_current();
    pause(Pause::EndAfterRead);
    let t = arch::rdtsc();
    if current == 0 {
        return Err(CalError::NoCount);
    }
    Ok(Window {
        tsc: t - t0,
        reference: u64::from(u32::MAX - current),
        polls: 1,
        start_gap: t0 - pre,
        end_gap: t - before,
    })
}

/// The TSC's frequency against the HPET ACPI lists (kept for [`hpet`] if
/// that works), or against the PIT if there is none or it doesn't work
/// (warned about), with `stall` stalled into each window
/// ([`stall_window_ends`]; none but in scenarios). An HPET whose windows were
/// stalled is still used, its windows filled in (the PIT stalls with it,
/// #139): only one whose rate then says nothing ([`Verdict::Unusable`]) gives
/// way to the PIT. Panics if neither counts; [`init`] judges the result.
fn calibrate_tsc(stall: Stall) -> (Calibration, Reference) {
    let mut hpet_error = None;
    if let Some(addr) = crate::acpi::info().and_then(|i| i.hpet).map(|h| h.address) {
        let calibrated = HpetCounter::new(addr).and_then(|hpet| {
            calibrate_with_hpet(&hpet, stall_window_ends(stall)).map(|cal| (hpet, cal)).map_err(HpetError::Window)
        });
        match calibrated {
            Ok((hpet, cal)) => {
                let judged = report(Calibrated::Tsc(Reference::Hpet), &cal);
                if judged.verdict != Verdict::Unusable {
                    *HPET.lock() = Some(hpet);
                    return (cal, Reference::Hpet);
                }
                warn!(target: "time", "{judged}; calibrating against the PIT");
                hpet_error = Some(HpetError::Window(CalError::NoInformation));
            }
            Err(e) => {
                warn!(target: "time", "the HPET at {addr:#x} is unusable ({e}); calibrating against the PIT");
                hpet_error = Some(e);
            }
        }
    }
    match (calibrate_with_pit(stall_window_ends(stall)), hpet_error) {
        (Ok(cal), _) => (cal, Reference::Pit),
        (Err(e), Some(h)) => panic!("can't calibrate the TSC: the HPET is unusable ({h}), and against the PIT {e}"),
        (Err(e), None) => panic!("can't calibrate the TSC: no HPET, and against the PIT {e}"),
    }
}

/// Log `judged` at its level ([`Report::level`]), or panic with it if the
/// rate says nothing.
fn log_or_panic(judged: &Report) {
    match judged.level() {
        Some(level) => log::log!(target: "time", level, "{judged}"),
        None if judged.verdict == Verdict::Unusable => panic!("{judged}"),
        None => {}
    }
}

/// Calibrate the TSC and start the local APIC timer at [`TICK_HZ`] (its
/// interrupts arrive once interrupts are enabled). Needs `acpi::init` and
/// `interrupts::init`; call with interrupts off. An HPET ACPI lists but that
/// doesn't count is warned about (the PIT is used instead). Each calibration
/// is judged by how far off its median can be ([`report`]): within its
/// clock's tolerance, at most an INFO line; past it, a warning; with no
/// information, a panic. Panics too if no clock can calibrate the TSC, or
/// the local APIC timer doesn't count or can't tick at 1 kHz.
pub fn init() {
    assert!(!arch::irq::enabled(), "time::init with interrupts on");
    let (cal, reference) = calibrate_tsc(stall_from_cmdline("lumen.stall_tsc_windows"));
    let tsc_report = report(Calibrated::Tsc(reference), &cal);
    if tsc_report.verdict == Verdict::Unusable {
        panic!("{tsc_report}");
    }
    let hz = cal.hz;
    *BOOT_CALIBRATION.lock() = Some(cal);
    TSC_HZ.store(hz, Ordering::Relaxed);
    REFERENCE.store(reference as u8, Ordering::Relaxed);
    info!(target: "time", "tsc {} (calibrated against {reference})", Hz(hz));
    log_or_panic(&tsc_report);

    let tsc = Clock::new(hz).expect("calibrated");
    let timer_cal = calibrate_lapic_timer(tsc, stall_window_ends(stall_from_cmdline("lumen.stall_timer_windows")))
        .unwrap_or_else(|e| panic!("can't calibrate the local APIC timer against the TSC: {e}"));
    *BOOT_TIMER_CALIBRATION.lock() = Some(timer_cal);
    let timer_hz = timer_cal.hz;
    log_or_panic(&report(Calibrated::Timer, &timer_cal));
    let count = lumen_time::periodic_count(timer_hz, TICK_HZ)
        .unwrap_or_else(|| panic!("the local APIC timer, at {}, can't tick at {}", Hz(timer_hz), Hz(TICK_HZ)));
    LAPIC_TIMER_HZ.store(timer_hz, Ordering::Relaxed);
    apic::timer_periodic(count);
    // An AP that misses this checked it before the store, so it was online
    // already (`smp` marks it with an `xchg`, a full fence on x86, as is this
    // SeqCst store) when `sched::init` later looks, and is woken to check
    // again; one that comes online after that look checks after the store.
    TIMER_COUNT.store(count, Ordering::SeqCst);
    info!(
        target: "time",
        "local APIC timer {} (bus {} / {}), ticking at {}",
        Hz(timer_hz),
        Hz(timer_hz * apic::TIMER_DIVISOR),
        apic::TIMER_DIVISOR,
        Hz(TICK_HZ)
    );
}

/// Start this CPU's local APIC timer at [`TICK_HZ`] with the boot CPU's
/// calibrated count, if [`init`] has published it; whether it did. For the
/// other CPUs (`sched`, from each one's idle loop); interrupts off.
pub fn start_timer_here() -> bool {
    match TIMER_COUNT.load(Ordering::SeqCst) {
        0 => false,
        count => {
            apic::timer_periodic(count);
            true
        }
    }
}

/// Whether [`start_timer_here`] would start the timer.
pub fn timer_published() -> bool {
    TIMER_COUNT.load(Ordering::SeqCst) != 0
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

/// Wait at least `ns` nanoseconds. A thread blocks (`sched::sleep`: the CPU
/// runs other threads or halts meanwhile) where it can; this busy-waits
/// ([`spin`]) where it can't: before the scheduler runs on this CPU, on its
/// idle thread, with interrupts off or inside a critical section (a held
/// lock). Panics before [`init`].
pub fn sleep(ns: u64) {
    if !crate::sched::sleep(ns) {
        spin(ns);
    }
}

/// Busy-wait at least `ns` nanoseconds without yielding or blocking (the
/// timer still preempts it if another thread is ready on this CPU): for
/// [`sleep`] where a thread can't block, and for tests that must not switch
/// threads (the calibration ktests' injected pauses among them; the boot's
/// timer calibration waits with [`timer_retake_pause`] instead, on the clock
/// it was given). Panics before [`init`].
pub fn spin(ns: u64) {
    let tsc = tsc().expect("time::spin before time::init");
    let start = arch::rdtsc();
    let ticks = tsc.ticks(ns);
    while arch::rdtsc().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

/// The boot CPU's local APIC timer interrupts so far. Lock-free.
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

/// A window pause for the ktests: spins `ns` at `at`, as a host descheduling
/// the vCPU there would.
#[cfg(feature = "ktest")]
fn sleep_at(at: Pause, ns: u64) -> impl FnMut(Pause) {
    move |p| {
        if p == at {
            spin(ns)
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

/// The spin `now_ns_follows_the_hpet` checks `now_ns` across: 50 ms.
#[cfg(feature = "ktest")]
const NOW_NS_SPIN_NS: u64 = 50_000_000;
/// That check's budget, 2% of its spin: 1 ms. Used only to bound
/// [`HPET_BRACKET_NS`] at compile time; at run time the check takes 2% of
/// what the HPET measured.
#[cfg(feature = "ktest")]
const NOW_NS_BUDGET_NS: u64 = NOW_NS_SPIN_NS / 50;
/// The widest bracket (in ns, between its two `now_ns` reads)
/// [`now_ns_against_the_hpet`] trusts around an HPET reading: 100 µs, a
/// tenth of the 1 ms budget (both ends that wide put the interval off by at
/// most 100 µs). Under KVM a bracket is about one exit to QEMU: 8 to 64 µs
/// in PR #121's KVM runs, loaded or not. A wider one means something took
/// longer than this between the two reads.
#[cfg(feature = "ktest")]
const HPET_BRACKET_NS: u64 = 100_000;
/// How many HPET readings [`now_ns_against_the_hpet`] takes at each end
/// before giving up on a narrow one: 9, so at most 8 retries, spaced
/// [`RETRY_SPACING_NS`] apart.
#[cfg(feature = "ktest")]
const HPET_BRACKET_TRIES: usize = 9;
/// How long the clock ktests wait, with interrupts on, before taking a
/// reading (or a placement try) again: 20 ms. On a loaded KVM runner HPET
/// reads (each an exit to QEMU) slowed to hundreds of µs in bursts lasting
/// 60 ms or more (PR #121, run 43786595767's load-5: 436, 320, 189 and
/// 169 µs back to back); retries taken back to back, about 1 ms apart, all
/// landed inside one. Spaced, a reading's retries span 160 ms and sample
/// around such a burst.
#[cfg(feature = "ktest")]
const RETRY_SPACING_NS: u64 = 20_000_000;
// Pinned: 9 tries 20 ms apart span at least 150 ms, more than load-5's burst;
// `lumen_harness.clock_diags`'s `MOST_HPET_RETRIES = 4` assumes this spacing
// (one burst of up to about 80 ms costs 4 retries on one end); and a limit
// over a tenth of the budget would let two wide ends use up a fifth of it.
#[cfg(feature = "ktest")]
const _: () = assert!(
    HPET_BRACKET_TRIES == 9
        && (HPET_BRACKET_TRIES as u64 - 1) * RETRY_SPACING_NS >= 150_000_000
        && HPET_BRACKET_NS * 10 <= NOW_NS_BUDGET_NS
);

/// Before try `tried` (from 0) of a reading or a placement: nothing for the
/// first, else [`RETRY_SPACING_NS`] of [`sleep`] (the thread blocks with
/// interrupts on). Call outside any bracket and with interrupts on.
#[cfg(feature = "ktest")]
fn space_retry(tried: usize) {
    if tried > 0 {
        sleep(RETRY_SPACING_NS);
    }
}
/// A pause the ktests inject inside a bracket to make it wide: twice the
/// widest trusted.
#[cfg(feature = "ktest")]
const STALL_NS: u64 = 2 * HPET_BRACKET_NS;
/// What the injected-pause tests' diags start with.
#[cfg(feature = "ktest")]
const INJECTED: &str = "injected pause: ";
/// Why [`now_ns_against_the_hpet`] fails when every reading at one end was
/// wide (a diag lists their widths). Failing, rather than passing on a wide
/// reading or skipping, keeps whatever stalled every reading visible: a
/// reading that wide can be off by more than the budget allows.
#[cfg(feature = "ktest")]
const EVERY_BRACKET_WIDE: &str = "every HPET reading at one end was wider than HPET_BRACKET_NS";
/// Why a `now_ns` ktest fails when `now_ns` went backwards inside a bracket:
/// a clock bug, not a stall, so not retaken.
#[cfg(feature = "ktest")]
const BACKWARDS_IN_BRACKET: &str = "now_ns went backwards inside a bracket";
/// Why a `now_ns` ktest fails when a later reading's midpoint is before an
/// earlier one's.
#[cfg(feature = "ktest")]
const BACKWARDS: &str = "now_ns went backwards";

/// The pause points inside a start reading and an end reading: before the
/// HPET is read, and after. [`hpet_between_now_ns`] takes only these two.
#[cfg(feature = "ktest")]
const START_READING: [Pause; 2] = [Pause::StartBeforeRead, Pause::StartAfterRead];
#[cfg(feature = "ktest")]
const END_READING: [Pause; 2] = [Pause::EndBeforeRead, Pause::EndAfterRead];

/// An HPET counter value read between two `now_ns` reads (`at` and `width`
/// in ns).
#[cfg(feature = "ktest")]
type HpetReading = lumen_time::Bracketed<u64>;
/// One end's readings in [`now_ns_against_the_hpet`].
#[cfg(feature = "ktest")]
type HpetEnd = lumen_time::NarrowReading<u64, HPET_BRACKET_TRIES>;

/// The HPET's counter read between two [`now_ns`] readings, with interrupts
/// off (so a wide bracket isn't a guest interrupt), placed at their midpoint.
/// `pause` runs at the reading's two points (`points`: [`START_READING`] or
/// [`END_READING`]; a no-op but in tests). `now_ns` going backwards across
/// it is an error.
#[cfg(feature = "ktest")]
fn hpet_between_now_ns(
    hpet: &HpetCounter,
    [before_read, after_read]: [Pause; 2],
    mut pause: impl FnMut(Pause),
) -> Result<HpetReading, lumen_time::Backwards> {
    let _off = arch::irq::push_off();
    let before_ns = now_ns();
    pause(before_read);
    let value = hpet.now();
    pause(after_read);
    let after_ns = now_ns();
    lumen_time::Bracketed::checked_between(before_ns, value, after_ns)
}

/// The first of up to [`HPET_BRACKET_TRIES`] readings from
/// [`hpet_between_now_ns`] (`points` and `pause` as there) at most
/// [`HPET_BRACKET_NS`] wide, each after the one before by
/// [`RETRY_SPACING_NS`], with every reading's width. `now_ns` going
/// backwards in one fails at once, named in a diag that starts with `tag`.
#[cfg(feature = "ktest")]
fn narrow_hpet_reading(
    hpet: &HpetCounter,
    points: [Pause; 2],
    tag: &str,
    pause: impl FnMut(Pause),
) -> Result<HpetEnd, &'static str> {
    narrow_hpet_reading_timed(hpet, points, tag, pause, &mut 0)
}

/// [`narrow_hpet_reading`], adding to `hpet_ticks` the HPET ticks that
/// passed over each reading (read just before and after it, not counting
/// the [`RETRY_SPACING_NS`] between readings): every reading's bracket fits
/// inside its own, unless `now_ns` itself is off.
#[cfg(feature = "ktest")]
fn narrow_hpet_reading_timed(
    hpet: &HpetCounter,
    points: [Pause; 2],
    tag: &str,
    mut pause: impl FnMut(Pause),
    hpet_ticks: &mut u64,
) -> Result<HpetEnd, &'static str> {
    let mut tried = 0;
    let take = || {
        space_retry(tried);
        tried += 1;
        let first = hpet.now();
        let reading = hpet_between_now_ns(hpet, points, &mut pause);
        *hpet_ticks = hpet_ticks.saturating_add(hpet.since(first, hpet.now()));
        reading
    };
    lumen_time::first_narrow(HPET_BRACKET_NS, take).map_err(|b| {
        crate::ktest::diag(format_args!("{tag}{BACKWARDS_IN_BRACKET}: from {} ns to {} ns", b.before, b.after));
        BACKWARDS_IN_BRACKET
    })
}

/// `later.at - earlier.at`, in ns; [`BACKWARDS`] (with a diag starting with
/// `tag`) if `later` was placed before `earlier`.
#[cfg(feature = "ktest")]
fn now_ns_between(earlier: &HpetReading, later: &HpetReading, tag: &str) -> Result<u64, &'static str> {
    later.at.checked_sub(earlier.at).ok_or_else(|| {
        crate::ktest::diag(format_args!(
            "{tag}{BACKWARDS}: a reading at {} ns, then one at {} ns",
            earlier.at, later.at
        ));
        BACKWARDS
    })
}

/// `now_ns_follows_the_hpet` across a spin of `ns` (50 ms in the real
/// check, whose 2% is the 1 ms budget the bracket limit is a tenth of; the
/// injected-pause tests spin less): each end a narrow HPET reading placed at
/// the midpoint of the `now_ns` readings around it, so a stall between the
/// two clocks' reads shows as a wide bracket (taken again, see
/// [`HPET_BRACKET_TRIES`] and [`RETRY_SPACING_NS`]) instead of skewing the
/// interval. `pause` runs
/// inside every bracket (see [`hpet_between_now_ns`]; a no-op but in tests),
/// and `tag` starts every diag ([`INJECTED`] in the injected-pause tests).
/// The ends' readings on a pass.
#[cfg(feature = "ktest")]
fn now_ns_against_the_hpet(
    hpet: &HpetCounter,
    ns: u64,
    tag: &str,
    mut pause: impl FnMut(Pause),
) -> Result<[HpetEnd; 2], &'static str> {
    use crate::kassert;
    // Each end's readings, and the HPET ticks that passed over them (each
    // reading's own, added up, without the sleeps between them): their
    // brackets fit inside that unless `now_ns` itself is off.
    let mut read_end = |points| {
        let mut hpet_ticks = 0;
        let end = narrow_hpet_reading_timed(hpet, points, tag, &mut pause, &mut hpet_ticks)?;
        Ok::<_, &'static str>((end, hpet_ticks))
    };
    let widths = |start: &HpetEnd, end: Option<&HpetEnd>| {
        crate::ktest::diag(format_args!(
            "{tag}bracket widths: start {:?} ns, end {:?} ns ({} taken again, {} ms apart; at most {HPET_BRACKET_NS} \
             ns trusted)",
            start.widths(),
            end.map_or(&[][..], |e| e.widths()),
            start.retries() + end.map_or(0, |e| e.retries()),
            RETRY_SPACING_NS / 1_000_000,
        ))
    };
    let every_one_wide = |which: &str, readings: &HpetEnd, span_ticks: u64| {
        crate::ktest::diag(format_args!(
            "{tag}all {HPET_BRACKET_TRIES} readings at the {which} were wider than {HPET_BRACKET_NS} ns: something \
             between the two now_ns reads took longer than that each time (a vCPU stall, a slow HPET read, or now_ns \
             itself off). Their widths add up to {} ns by now_ns; the HPET counted {span_ticks} ticks ({} ns) over \
             the same readings, each timed on its own without the {} ms between them (the widths fit inside \
             that unless now_ns itself is off)",
            readings.widths().iter().fold(0u64, |sum, &w| sum.saturating_add(w)),
            hpet.clock().ns(span_ticks),
            RETRY_SPACING_NS / 1_000_000,
        ));
        Err(EVERY_BRACKET_WIDE)
    };
    let (start, start_span_ticks) = read_end(START_READING)?;
    let Some(r0) = start.reading() else {
        widths(&start, None);
        return every_one_wide("start", &start, start_span_ticks);
    };
    spin(ns);
    let (end, end_span_ticks) = read_end(END_READING)?;
    widths(&start, Some(&end));
    let Some(r1) = end.reading() else {
        return every_one_wide("end", &end, end_span_ticks);
    };
    let moved_ns = now_ns_between(&r0, &r1, tag)?;
    let by_hpet_ns = hpet.clock().ns(hpet.since(r0.value, r1.value));
    crate::ktest::diag(format_args!(
        "{tag}now_ns moved {moved_ns} ns, the HPET {by_hpet_ns} ns: {} ns apart, of a {} ns budget",
        moved_ns.abs_diff(by_hpet_ns),
        by_hpet_ns / 50,
    ));
    kassert!(r0.at > 0 && moved_ns.abs_diff(by_hpet_ns) * 50 <= by_hpet_ns);
    Ok([start, end])
}

/// The local APIC timer's current count read between two HPET reads, placed
/// at their midpoint (`at` and `width` in HPET ticks). `pause` runs just
/// before the timer is read and just after (a no-op but in tests). Call with
/// interrupts off.
#[cfg(feature = "ktest")]
fn timer_between_hpet(hpet: &HpetCounter, mut pause: impl FnMut()) -> lumen_time::Bracketed<u32> {
    let before_ticks = hpet.now();
    pause();
    let value = apic::timer_current();
    pause();
    let after_ticks = hpet.now();
    lumen_time::Bracketed::between(before_ticks, value, after_ticks, hpet.mask)
}

crate::ktest!(
    fn tsc_calibrated_within_tolerance() {
        // Cross-check against the PIT, a different clock from the HPET the
        // boot calibrated against (the ktest machine has one): within 2%
        // (twice TSC_TOLERANCE_PPM), or what the two calibrations' bounds
        // allow if more (stalled windows filling in).
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
        let (boot_ppm, pit_ppm) = (boot.median_bound_ppm(), pit.median_bound_ppm());
        crate::ktest::diag(format_args!("bounds: boot {boot_ppm} ppm, pit {pit_ppm} ppm"));
        kassert!(within_bounds(pit.hz, hz, pit_ppm, boot_ppm, 2 * TSC_TOLERANCE_PPM));
        Ok(())
    }
);

/// Whether two measurements of one rate, `a` off by at most `a_ppm` of the
/// true rate and `b` by at most `b_ppm`, agree: within those bounds of each
/// other, `|a - b| ≤ (a_ppm + b_ppm) × true` (the true rate is at most
/// `b / (1 - b_ppm)`), but never held tighter than `floor_ppm`. The floor is
/// for what the bounds leave out: the two were measured at different times
/// against different clocks, which QEMU drives from the host's (slewed)
/// monotonic clock while KVM's TSC isn't slewed, so tens to hundreds of ppm
/// can come from the host, not the code. Stalled windows can need more than
/// the floor, which their bounds then give.
#[cfg(feature = "ktest")]
fn within_bounds(a: u64, b: u64, a_ppm: u64, b_ppm: u64, floor_ppm: u64) -> bool {
    const M: u128 = 1_000_000;
    let allowed = u128::from(a_ppm.saturating_add(b_ppm).max(floor_ppm));
    let b_ppm = u128::from(b_ppm);
    b_ppm < M && u128::from(a.abs_diff(b)) * (M - b_ppm) <= u128::from(b) * allowed
}

/// A PIT window paused 2 ms at `at` (Started, Ended or CountRead, which
/// shouldn't matter): the pause happened (for Started and Ended, the readings
/// show the 2 ms of ticks; CountRead's is after both), and a window taken as
/// calibration takes them (steady if one of [`ATTEMPTS`] is, retakes spaced,
/// else the least unsteady: a loaded host can stall every reading for a
/// while, #139, which this isn't about) still gives the TSC's frequency,
/// within 1%, or what that window's gaps and the boot calibration's allow
/// if more ([`Window::tsc_error_ppm`], [`Calibration::median_bound_ppm`],
/// [`within_bounds`]). A window that allows 10% or more can't tell: getting
/// these wrong was 20% off.
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
    // the end reading (CountRead is after both, so it shows in neither).
    // Counter arithmetic, wrapping: a host stall on top of the pause can run
    // the count out and on before the start is read.
    let paused = match at {
        Pause::Started => Some(count.wrapping_sub(w.start.count)),
        Pause::Ended => Some(0u16.wrapping_sub(w.end.count)),
        _ => None,
    };
    crate::ktest::diag(format_args!("paused at {at:?}: {paused:?} ticks; readings {w:?}"));
    if let Some(paused) = paused {
        kassert!(paused >= 2000);
    }
    let tsc = tsc().ok_or("TSC not calibrated")?;
    let got = off_or_diag("no PIT window", || {
        lumen_time::steadiest_windows::<CalError, 1>(
            ATTEMPTS,
            || pit_window(count, &mut pause),
            || timer_retake_pause(tsc),
        )
    })?;
    let [w] = *got.windows();
    let measured = w.tsc_hz(PIT_HZ).ok_or("no frequency")?;
    let boot_ppm = boot_calibration().ok_or("no boot calibration")?.median_bound_ppm();
    let allowed_ppm = w.tsc_error_ppm().saturating_add(boot_ppm);
    crate::ktest::diag(format_args!(
        "{measured} Hz against {hz} Hz; window {w} (steady: {}), bounds {allowed_ppm} ppm ({boot_ppm} the boot's)",
        w.is_steady()
    ));
    kassert!(w.tsc_error_ppm() < 100_000); // else it can't tell
    kassert!(within_bounds(measured, hz, w.tsc_error_ppm(), boot_ppm, 10_000));
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
    fn reading_the_latched_pit_count_is_outside_the_window() {
        // Paused 1 ms while each reading's latched count is read, after its
        // bracket closed (as a slow VM exit under KVM would): the windows are
        // still steady and give the TSC's frequency. With the reads inside the
        // bracket, every window would be unsteady.
        a_pit_window_paused_at_still_gives_the_tsc(Pause::CountRead)
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
        // TSC's frequency, within 1% (or its bound and the boot's, if more).
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
        // Within 1%, or its bound and the boot calibration's if more.
        let boot_ppm = boot_calibration().ok_or("no boot calibration")?.median_bound_ppm();
        kassert!(within_bounds(cal.hz, hz, cal.median_bound_ppm(), boot_ppm, 10_000));
        Ok(())
    }
);

crate::ktest!(
    fn a_tsc_calibration_short_of_steady_windows_fills_in() {
        // A reference whose every window has a paused end (1,000,000 TSC
        // ticks of 10,000,000 or so, 11,932 PIT ticks: about 11% each): all
        // ATTEMPTS windows taken, each retake after a wait, the least gapped
        // five kept (each window is a tick longer than the last, so the same
        // gap is a smaller share of it: the last five, least gapped first),
        // the median and its bound theirs (the middle one's), past the TSC's
        // tolerance: a warning, not an error (#139).
        use crate::{kassert, kassert_eq};
        let w = |taken: u64| Window {
            tsc: 10_000_000 + taken,
            reference: 11_932,
            polls: 1,
            start_gap: 0,
            end_gap: 1_000_000,
        };
        let (mut taken, mut waits) = (0, 0);
        let ruler = core::cell::Cell::new(u64::MAX);
        let cal = median_tsc_hz_with(
            PIT_HZ,
            || {
                taken += 1;
                Ok(w(taken))
            },
            &ruler,
            || waits += 1,
        )
        .map_err(|_| "calibrating failed")?;
        crate::ktest::diag(format_args!("{cal}, median within {} ppm", cal.median_bound_ppm()));
        kassert_eq!((taken, waits, cal.retaken, cal.steady()), (ATTEMPTS as u64, ATTEMPTS - 1, ATTEMPTS, 0));
        kassert_eq!(cal.windows, [20, 19, 18, 17, 16].map(w));
        kassert_eq!(cal.last_retaken, Some(w(ATTEMPTS as u64)));
        kassert_eq!(ruler.get(), w(1).tsc); // the fewest ticks a window took
        kassert_eq!(Some(cal.hz), w(18).tsc_hz(PIT_HZ));
        kassert_eq!(cal.median_bound_ppm(), w(18).tsc_error_ppm());
        kassert!(cal.median_bound_ppm() > 110_000 && cal.median_bound_ppm() < 112_000);
        kassert_eq!(report(Calibrated::Tsc(Reference::Pit), &cal).verdict, Verdict::Imprecise);
        Ok(())
    }
);

crate::ktest!(
    fn a_tsc_calibration_waits_between_retakes() {
        // Made-up windows of 2,000,000 TSC ticks (the ruler: no window took
        // fewer), the first three unsteady: each is taken again after a
        // wait of two windows' ticks, so at least 12,000,000 ticks pass; a
        // no-op wait would take microseconds. The rate is the steady ones'.
        use crate::{kassert, kassert_eq};
        let steady = Window { tsc: 2_000_000, reference: 2_437, polls: 1, start_gap: 0, end_gap: 1_000 };
        let stalled = Window { end_gap: 200_000, ..steady };
        let mut taken = 0;
        let start = arch::rdtsc();
        let cal = median_tsc_hz(PIT_HZ, || {
            taken += 1;
            Ok(if taken <= 3 { stalled } else { steady })
        })
        .map_err(|_| "calibrating failed")?;
        let waited = arch::rdtsc().wrapping_sub(start);
        crate::ktest::diag(format_args!("{cal}; {waited} TSC ticks in all"));
        kassert_eq!((cal.steady(), cal.retaken, taken), (WINDOWS, 3, 8));
        kassert_eq!(Some(cal.hz), steady.tsc_hz(PIT_HZ));
        kassert!(waited >= 3 * TSC_RETRY_SPACING_WINDOWS * steady.tsc);
        Ok(())
    }
);

crate::ktest!(
    fn an_hpet_slow_to_read_stretches_its_windows_until_steady() {
        // Every window's end reading held up 200 µs, as a slowed host's HPET
        // reads were (#139: 250 µs each): 2% of a 10 ms window, unsteady
        // however late it is taken again. Taken again longer, until the same
        // gaps are under 0.5% of it, all five kept windows are steady (none
        // filling in) and longer than 10 ms, and the rate is within 1% (or
        // its bound and the boot's, if more).
        use crate::{kassert, kassert_eq};
        let hpet = hpet().ok_or("no HPET")?;
        let hz = tsc().ok_or("TSC not calibrated")?.hz();
        let base = hpet.clock().ticks(WINDOW_NS);
        let cal = off_or_diag("calibrating against the HPET failed", || {
            calibrate_with_hpet(&hpet, sleep_at(Pause::EndAfterRead, 200_000))
        })?;
        crate::ktest::diag(format_args!("{cal}, median within {} ppm", cal.median_bound_ppm()));
        kassert_eq!(cal.steady(), WINDOWS);
        kassert!(cal.retaken >= 1);
        // Longer than 10 ms, and at most 16 times that: a window ends at the
        // first poll past its target, so up to a poll (450 µs on loaded KVM)
        // past it, under one more window.
        kassert!(cal.windows.iter().all(|w| w.reference > base && w.reference < base * (MOST_HPET_STRETCH + 1)));
        let boot_ppm = boot_calibration().ok_or("no boot calibration")?.median_bound_ppm();
        kassert!(within_bounds(cal.hz, hz, cal.median_bound_ppm(), boot_ppm, 10_000));
        Ok(())
    }
);

// Pinned: a_tsc_calibration_waits_between_retakes counts the waits in it, and
// it matches the timer's 20 ms (TIMER_RETRY_SPACING_NS) for 10 ms windows.
const _: () = assert!(TSC_RETRY_SPACING_WINDOWS == 2);

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

/// Runs `f` with interrupts off and this CPU's local APIC timer free for a
/// ktest to run once, masked (which stops its 1 kHz tick: [`timer_window`],
/// or the 1 kHz check's one-shot windows), then restarts the tick before
/// interrupts come back, and checks the timer's registers (LVT, error LVT,
/// initial count, divider) read as they did before: `f`'s result if they do.
/// Interrupts stay off throughout, so the thread neither waits for a tick
/// that is stopped nor moves to another CPU and restarts that one's.
#[cfg(feature = "ktest")]
fn with_tick_stopped<T>(f: impl FnOnce() -> T) -> Result<T, &'static str> {
    use crate::kassert_eq;
    /// Restarts the tick when dropped.
    struct TickStopped;
    impl Drop for TickStopped {
        fn drop(&mut self) {
            apic::timer_periodic(TIMER_COUNT.load(Ordering::SeqCst));
        }
    }
    let registers = || (apic::timer_registers(), apic::timer_divide_config());
    let (before, out, after) = {
        let _off = arch::irq::push_off();
        let before = registers();
        let out = {
            let _tick = TickStopped;
            f()
        };
        (before, out, registers())
    };
    kassert_eq!(after, before); // the tick restarted as it was
    Ok(out)
}

crate::ktest!(
    fn a_paused_timer_reading_is_not_steady() {
        // Paused 1 ms inside a local APIC timer window's start (before or
        // after the timer is started) or end (before or after its count is
        // read): that end is 10% uncertain, so the window would be taken
        // again. And the pause lands where the register access says. A
        // window's rate is off by at most its start gap high and its end gap
        // low. Paused while the timer counts (after the start, or before the
        // read), the pause can't take the rate low: after the start the timer
        // counts it but the TSC window doesn't (about 10% high); before the
        // read both have it. Paused while it doesn't (before the start, or
        // after the read, #129's stall: about 9% low), it can't take the rate
        // high. Each check allows the other gap (a loaded host may stall
        // there too) and 1% for the calibration. All four windows are taken
        // with the tick stopped (`with_tick_stopped`).
        use crate::kassert;
        let tsc = tsc().ok_or("TSC not calibrated")?;
        let timer_hz = lapic_timer().ok_or("timer not calibrated")?.hz();
        let (window, ms) = (tsc.ticks(WINDOW_NS), tsc.ticks(1_000_000));
        const AT: [Pause; 4] =
            [Pause::StartBeforeRead, Pause::StartAfterRead, Pause::EndBeforeRead, Pause::EndAfterRead];
        let windows = with_tick_stopped(|| AT.map(|at| timer_window(window, sleep_at(at, 1_000_000))))?;
        // Parts per million of `w`'s TSC span.
        let ppm = |ticks: u64, w: &Window| (u128::from(ticks) * 1_000_000 / u128::from(w.tsc.max(1))) as i64;
        for (at, w) in AT.into_iter().zip(windows) {
            let w = w.map_err(|e| {
                crate::ktest::diag(format_args!("{e}"));
                "no timer window"
            })?;
            let rate = w.reference_hz(tsc.hz()).unwrap_or(0);
            let off_ppm = (rate as i64 - timer_hz as i64) * 1_000_000 / timer_hz as i64;
            crate::ktest::diag(format_args!("paused at {at:?}: {w}, rate {rate} Hz ({off_ppm} ppm off)"));
            kassert!(!w.is_steady());
            let start = matches!(at, Pause::StartBeforeRead | Pause::StartAfterRead);
            kassert!(if start { w.start_gap } else { w.end_gap } >= ms); // the pause is in it
            let (high, low, paused) = (ppm(w.start_gap, &w) + 10_000, ppm(w.end_gap, &w) + 10_000, ppm(ms, &w));
            if matches!(at, Pause::StartAfterRead | Pause::EndBeforeRead) {
                kassert!(off_ppm >= paused - low && off_ppm <= high); // the timer counted the pause
            } else {
                kassert!(off_ppm >= -low && off_ppm <= high - paused); // it didn't
            }
        }
        Ok(())
    }
);

// Pinned: a_timer_calibration_waits_between_retakes measures the waits
// against this, and loaded KVM's bursts of stalls are what it was chosen for.
const _: () = assert!(TIMER_RETRY_SPACING_NS == 20_000_000);

/// Made-up timer windows in `tsc`'s ticks, 10 ms each, at `hz`, with
/// `end_gap_ns` of end gap (50 µs or less is steady).
#[cfg(feature = "ktest")]
fn made_up_timer_window(tsc: Clock, hz: u64, end_gap_ns: u64) -> Window {
    Window { tsc: tsc.ticks(WINDOW_NS), reference: hz / 100, polls: 1, start_gap: 0, end_gap: tsc.ticks(end_gap_ns) }
}

crate::ktest!(
    fn a_timer_calibration_waits_between_retakes() {
        // Made-up windows in this TSC's ticks: the first three 10% low with
        // a 1 ms end gap (unsteady; three can move a median of five), then
        // steady ones. All three are taken again, each after a wait of
        // 20 ms by the TSC (60 ms in all; a no-op wait would take
        // microseconds), and the rate is the steady windows'.
        use crate::{kassert, kassert_eq};
        let tsc = tsc().ok_or("TSC not calibrated")?;
        let steady = made_up_timer_window(tsc, 62_500_000, 10_000);
        let stalled = made_up_timer_window(tsc, 56_250_000, 1_000_000);
        let mut taken = 0;
        let start = arch::rdtsc();
        let cal = calibrate_timer_with(tsc, || {
            taken += 1;
            Ok(if taken <= 3 { stalled } else { steady })
        })
        .map_err(|_| "calibrating failed")?;
        let waited = arch::rdtsc().wrapping_sub(start);
        crate::ktest::diag(format_args!("{cal}; {} ns in all", tsc.ns(waited)));
        kassert_eq!((cal.steady(), cal.retaken, taken), (WINDOWS, 3, 8));
        kassert_eq!(Some(cal.hz), steady.reference_hz(tsc.hz()));
        kassert!(waited >= tsc.ticks(60_000_000));
        Ok(())
    }
);

crate::ktest!(
    fn a_timer_calibration_short_of_steady_windows_fills_in() {
        // Made-up windows in this TSC's ticks, only windows 4, 11 and 17 of
        // ATTEMPTS steady; the others 2% low with a 200 µs end gap (two of
        // them) or 20% low with over 2 ms: no error (a timer is still
        // started), the three steady ones first and the two 2% ones after,
        // the rate a steady one's, and an INFO report (most are steady, so
        // the bound is a steady window's, within the timer's tolerance).
        // 16 waits of 20 ms, about 320 ms.
        use crate::kassert_eq;
        let tsc = tsc().ok_or("TSC not calibrated")?;
        let steady = made_up_timer_window(tsc, 62_500_000, 10_000);
        let near = made_up_timer_window(tsc, 61_250_000, 200_000);
        let far = made_up_timer_window(tsc, 50_000_000, 2_100_000);
        let mut taken = 0usize;
        let cal = calibrate_timer_with(tsc, || {
            taken += 1;
            Ok(match taken {
                4 | 11 | 17 => steady,
                7 | 9 => near,
                _ => far,
            })
        })
        .map_err(|_| "calibrating failed")?;
        crate::ktest::diag(format_args!("{cal}"));
        kassert_eq!((taken, cal.steady(), cal.retaken), (ATTEMPTS, 3, ATTEMPTS - 3));
        kassert_eq!(cal.windows, [steady, steady, steady, near, near]);
        kassert_eq!(Some(cal.hz), steady.reference_hz(tsc.hz()));
        kassert_eq!(report(Calibrated::Timer, &cal).verdict, Verdict::Retook);
        Ok(())
    }
);

crate::ktest!(
    fn a_calibration_report_fits_each_outcome() {
        // What init logs, and at what level, judged by the median's bound
        // against each clock's tolerance (the timer 2%, the TSC 1%), not by
        // how many windows are steady. Made-up windows of 10,000,000 TSC
        // ticks; 50,000 ticks of gaps or less is steady.
        // - Every window steady at once: nothing.
        // - Retaken, bound within the tolerance: INFO with the bound, even
        //   with none steady (#139's 0.7% timer, which used to warn).
        // - Past the tolerance: WARN with the bound, rounded up to a tenth
        //   (20,001 ppm shows as 2.1%; the timer's third smallest share).
        // - The middle windows' gaps as long as themselves: a panic (no
        //   level), saying so.
        // The same windows judged as the TSC's (its error, end gap over the
        // rest plus a reference tick, against 1%) can warn where the timer's
        // don't.
        use crate::{kassert, kassert_eq};
        let w = |end_gap: u64| Window { tsc: 10_000_000, reference: 625_000, polls: 1, start_gap: 0, end_gap };
        let cal = |gaps: [u64; WINDOWS], retaken, measured| Calibration {
            hz: 62_500_000,
            windows: gaps.map(w),
            retaken,
            last_retaken: Some(w(60_000)),
            measured,
        };
        let timer = |gaps, retaken| report(Calibrated::Timer, &cal(gaps, retaken, Measured::AgainstTsc));
        let tsc = |gaps, retaken| report(Calibrated::Tsc(Reference::Hpet), &cal(gaps, retaken, Measured::Tsc));
        let text = |r: &Report| alloc::format!("{r}");

        let clean = timer([1_000, 2_000, 3_000, 4_000, 5_000], 0);
        kassert_eq!((clean.verdict, clean.level()), (Verdict::Clean, None));
        let retook = timer([1_000, 2_000, 3_000, 200_000, 300_000], 17);
        kassert_eq!((retook.verdict, retook.level()), (Verdict::Retook, Some(log::Level::Info)));
        kassert!(
            text(&retook).starts_with(
                "local APIC timer calibration took 17 windows again, 3 of 5 kept steady, median within 0.1%"
            )
        );
        // None steady, but every gap 0.7%: still within the timer's 2%.
        let filled = timer([70_000, 70_000, 70_000, 70_000, 70_000], 20);
        kassert_eq!((filled.cal.steady(), filled.bound_ppm), (0, 7_000));
        kassert_eq!((filled.verdict, filled.level()), (Verdict::Retook, Some(log::Level::Info)));
        kassert!(text(&filled).contains("0 of 5 kept steady, median within 0.7%"));
        let imprecise = timer([1_000, 2_000, 200_010, 300_000, 2_000_000], 18);
        kassert_eq!((imprecise.cal.steady(), imprecise.bound_ppm), (2, 20_001));
        kassert_eq!((imprecise.verdict, imprecise.level()), (Verdict::Imprecise, Some(log::Level::Warn)));
        let warning = text(&imprecise);
        crate::ktest::diag(format_args!("{warning}"));
        kassert!(warning.starts_with(
            "local APIC timer calibration: only 2 of 20 windows read promptly (5 wanted; a stall next to the timer's \
             register accesses), so its rate may be off by up to 2.1%, over the 2.0% allowed ("
        ));
        let unusable = timer([1_000, 2_000, 10_000_000, 10_000_000, 20_000_000], 18);
        kassert_eq!((unusable.verdict, unusable.level()), (Verdict::Unusable, None));
        kassert!(text(&unusable).starts_with("can't calibrate the local APIC timer against the TSC: only 2 of 20"));

        // As the TSC's: 0.7% gaps are within its 1% too, 1.2% are not.
        let tsc_filled = tsc([70_000, 70_000, 70_000, 70_000, 70_000], 20);
        kassert_eq!(tsc_filled.bound_ppm, w(70_000).tsc_error_ppm());
        kassert_eq!(tsc_filled.verdict, Verdict::Retook);
        kassert!(text(&tsc_filled).starts_with("tsc calibration took 20 windows again, 0 of 5 kept steady"));
        let tsc_off = tsc([120_000, 120_000, 120_000, 120_000, 120_000], 20);
        kassert!(tsc_off.bound_ppm > 12_000);
        kassert_eq!((tsc_off.verdict, tsc_off.level()), (Verdict::Imprecise, Some(log::Level::Warn)));
        kassert!(text(&tsc_off).starts_with(
            "tsc calibration: only 0 of 20 windows read promptly (5 wanted; a stall next to the HPET's reads), so \
             its rate may be off by up to 1.3%, over the 1.0% allowed ("
        ));
        let tsc_unusable = tsc([5_000_000, 5_000_000, 5_000_000, 5_000_000, 5_000_000], 20);
        kassert_eq!((tsc_unusable.verdict, tsc_unusable.level()), (Verdict::Unusable, None));
        kassert!(text(&tsc_unusable).starts_with("can't calibrate the tsc against the HPET: only 0 of 20"));
        Ok(())
    }
);

crate::ktest!(
    fn the_stalled_boots_seen_on_kvm_report_quietly() {
        // Windows from the two stalled KVM boots #139 recorded, replayed
        // through the judgement (both warned before, failing scenarios that
        // expect no warning):
        // - CI run 43966495801's timer windows, 0 of 5 steady after 20: the
        //   bound is the third smallest gap share, 0.7%, inside the timer's
        //   2%: INFO.
        // - An idle measurement run's (44062785900) timer windows, every end
        //   about 25 µs slow (a slowed host, not a burst): 1 steady, 0.5%:
        //   INFO.
        // - The HPET windows of the first boot: 4 of 20 read promptly, the
        //   others like the one its warning showed (40 polls in 10 ms, 3.9%
        //   of gaps). With the least gapped filling in, the median lies
        //   between steady windows': INFO, and the HPET kept (it used to give
        //   way to the PIT).
        use crate::kassert_eq;
        let parse = |w: &str| {
            let mut f = w.split('/').map(|x| x.parse::<u64>().unwrap_or(0));
            let mut n = || f.next().unwrap_or(0);
            Window { tsc: n(), reference: n(), polls: n(), start_gap: n(), end_gap: n() }
        };
        let timer = |windows: [&str; WINDOWS], retaken| Calibration {
            hz: 62_500_000,
            windows: windows.map(parse),
            retaken,
            last_retaken: None,
            measured: Measured::AgainstTsc,
        };
        let ci = timer(
            [
                "24545668/625907/1/30698/90919",
                "24497354/626076/1/32806/42630",
                "24591679/628578/1/31997/136930",
                "24604051/628884/1/65734/149327",
                "24829059/629536/1/32781/374262",
            ],
            18,
        );
        let ci = report(Calibrated::Timer, &ci);
        kassert_eq!((ci.verdict, ci.bound_ppm, ci.level()), (Verdict::Retook, 6_870, Some(log::Level::Info)));
        let idle = timer(
            [
                "24516562/626599/1/60221/62328",
                "24517738/626623/1/59314/63504",
                "24515508/626637/1/62304/61274",
                "24518522/626698/1/61471/64264",
                "24518841/626699/1/61397/64631",
            ],
            19,
        );
        let idle = report(Calibrated::Timer, &idle);
        kassert_eq!((idle.verdict, idle.bound_ppm), (Verdict::Retook, 5_041));
        let slow = parse("24547212/1000016/40/436492/529641");
        let prompt = parse("24547212/1000016/800/30000/30000");
        let mut taken = 0;
        let ruler = core::cell::Cell::new(u64::MAX);
        let hpet = median_tsc_hz_with(
            100_000_000,
            || {
                taken += 1;
                Ok(if matches!(taken, 3 | 8 | 13 | 19) { prompt } else { slow })
            },
            &ruler,
            || {},
        )
        .map_err(|_| "calibrating failed")?;
        let hpet = report(Calibrated::Tsc(Reference::Hpet), &hpet);
        kassert_eq!((hpet.cal.steady(), hpet.verdict, hpet.level()), (4, Verdict::Retook, Some(log::Level::Info)));
        kassert_eq!(hpet.bound_ppm, prompt.tsc_error_ppm());
        Ok(())
    }
);

crate::ktest!(
    fn the_boot_timer_calibration_is_within_what_the_tick_needs() {
        // Expected, not guaranteed: the boot's median is within the timer's
        // tolerance (TIMER_TOLERANCE_PPM), as its steady windows' or enough
        // least-unsteady ones filling in allow, and the timer runs at it. A
        // boot whose bound was past it warned, and fails this on purpose: its
        // rate may be off by more than the tick allows.
        use crate::{kassert, kassert_eq};
        let cal = boot_timer_calibration().ok_or("no timer calibration")?;
        let tsc_hz = tsc().ok_or("TSC not calibrated")?.hz();
        // Its windows (tsc/timer/polls/start_gap/end_gap) and each one's
        // rate, so a calibration that was off shows which windows moved it.
        let mut rates = cal.windows.map(|w| w.reference_hz(tsc_hz).unwrap_or(0));
        crate::ktest::diag(format_args!(
            "timer boot calibration {cal}, {} steady, median within {} ppm; window rates {rates:?} Hz",
            cal.steady(),
            cal.median_bound_ppm()
        ));
        // The steady ones first.
        kassert_eq!(cal.windows.iter().take_while(|w| w.is_steady()).count(), cal.steady());
        kassert_eq!(cal.measured, Measured::AgainstTsc);
        kassert!(cal.median_bound_ppm() <= TIMER_TOLERANCE_PPM);
        kassert_eq!(lumen_time::median(&mut rates), Some(cal.hz));
        kassert_eq!(lapic_timer().map(|c| c.hz()), Some(cal.hz));
        Ok(())
    }
);

crate::ktest!(
    fn the_boot_tsc_calibration_is_within_what_now_ns_needs() {
        // Expected, not guaranteed: the boot's TSC median is within the
        // TSC's tolerance (TSC_TOLERANCE_PPM, 1%), its steady windows first,
        // the frequency their median, and now_ns runs at it. A boot past it
        // warned, and fails this on purpose.
        use crate::{kassert, kassert_eq};
        let cal = boot_calibration().ok_or("no boot calibration")?;
        let reference = reference().ok_or("no reference")?;
        let reference_hz = match reference {
            Reference::Hpet => hpet().ok_or("no HPET")?.clock().hz(),
            Reference::Pit => PIT_HZ,
        };
        let mut rates = cal.windows.map(|w| w.tsc_hz(reference_hz).unwrap_or(0));
        crate::ktest::diag(format_args!(
            "tsc boot calibration ({reference}) {cal}, {} steady, median within {} ppm; window rates {rates:?} Hz",
            cal.steady(),
            cal.median_bound_ppm()
        ));
        kassert_eq!(cal.windows.iter().take_while(|w| w.is_steady()).count(), cal.steady());
        kassert_eq!(cal.measured, Measured::Tsc);
        kassert!(cal.median_bound_ppm() <= TSC_TOLERANCE_PPM);
        kassert_eq!(lumen_time::median(&mut rates), Some(cal.hz));
        kassert_eq!(tsc().map(|c| c.hz()), Some(cal.hz));
        Ok(())
    }
);

crate::ktest!(
    fn now_ns_follows_the_hpet() {
        // `now_ns` is the TSC converted; across a 50 ms spin it moves as
        // far as the HPET says, within 2% (a 1 ms budget). Each HPET reading
        // is bracketed by `now_ns` readings (see `now_ns_against_the_hpet`).
        // Read one after the other, the two clocks came out 1.63 ms apart
        // once under KVM (#117); a stall of about that much between two of
        // the reads is the likely cause (the calibration agreed to 0.002%).
        let hpet = hpet().ok_or("no HPET")?;
        now_ns_against_the_hpet(&hpet, NOW_NS_SPIN_NS, "", |_| {}).map(|_| ())
    }
);

/// How far below twice its pause a placement test's paused bracket may
/// measure: the clocks' rounding and calibration (a pause spins the TSC for
/// at least that long). Narrower means a pause didn't run, or the clocks
/// disagree.
#[cfg(feature = "ktest")]
const PAUSE_SLACK_NS: u64 = 10_000;
/// The longest stretch `a_timer_reading_is_placed_at_its_brackets_midpoint`
/// trusts the periodic local APIC timer's count over: under its 1 ms reload
/// period, with room, since the count can't tell how many reloads passed.
/// (`the_timer_is_programmed_for_1_khz` measures one-shot, without reloads.)
#[cfg(feature = "ktest")]
const TIMER_WINDOW_MAX_NS: u64 = 950_000;
/// How long each of `the_timer_is_programmed_for_1_khz`'s one-shot windows
/// is, by the HPET.
#[cfg(feature = "ktest")]
const TIMER_ONE_SHOT_WINDOW_NS: u64 = 20_000_000;

crate::ktest!(
    fn an_hpet_reading_is_placed_at_its_brackets_midpoint() {
        // A reading paused 5 ms before and 5 ms after its HPET read (a 10 ms
        // bracket) is placed at its midpoint, where the HPET was read: from
        // a narrow reference reading, the placed `now_ns` and the HPET agree
        // to under 2.5 ms (half the pause). Reading the HPET outside the
        // bracket, or placing the reading at either end of it, puts it 5 ms
        // off. A try is judged only if `lumen_time::placement_is_judged`
        // says so (its uncertainty and the reference's width each at most a
        // quarter of the pause: a loaded host stalls a 5 ms spin), so a
        // right placement is at most 1.25 ms off and a wrong one at least
        // 3.75 ms. Other tries are taken again, RETRY_SPACING_NS apart (a
        // burst of stalls on a loaded host lasts tens of ms).
        use crate::kassert;
        const PAUSE_NS: u64 = 5_000_000;
        const TRIES: usize = 8;
        let hpet = hpet().ok_or("no HPET")?;
        let reference = narrow_hpet_reading(&hpet, START_READING, INJECTED, |_| {})?;
        let Some(reference) = reference.reading() else {
            crate::ktest::diag(format_args!(
                "{INJECTED}no narrow reference reading: widths {:?} ns",
                reference.widths()
            ));
            return Err("no narrow reference reading");
        };
        let mut widths_ns = [0u64; TRIES];
        for tried in 0..TRIES {
            space_retry(tried);
            let paused = hpet_between_now_ns(&hpet, END_READING, |_| spin(PAUSE_NS)).map_err(|b| {
                crate::ktest::diag(format_args!("{INJECTED}{BACKWARDS_IN_BRACKET}: {b:?}"));
                BACKWARDS_IN_BRACKET
            })?;
            widths_ns[tried] = paused.width;
            kassert!(paused.width.saturating_add(PAUSE_SLACK_NS) >= 2 * PAUSE_NS);
            if !lumen_time::placement_is_judged(reference.width, paused.width, PAUSE_NS) {
                continue;
            }
            let by_now_ns = now_ns_between(&reference, &paused, INJECTED)?;
            let by_hpet_ns = hpet.clock().ns(hpet.since(reference.value, paused.value));
            let off_ns = by_now_ns.abs_diff(by_hpet_ns);
            crate::ktest::diag(format_args!(
                "{INJECTED}HPET placement {off_ns} ns off (at most {} ns allowed) on try {} of {TRIES}, {} ms apart; \
                 paused brackets {:?} ns, the reference's {} ns",
                PAUSE_NS / 2,
                tried + 1,
                RETRY_SPACING_NS / 1_000_000,
                &widths_ns[..=tried],
                reference.width,
            ));
            kassert!(off_ns * 2 <= PAUSE_NS);
            return Ok(());
        }
        crate::ktest::diag(format_args!(
            "{INJECTED}every paused reading had a natural stall too: brackets {widths_ns:?} ns, the reference's {} ns",
            reference.width
        ));
        Err("every paused HPET reading was stalled as well")
    }
);

crate::ktest!(
    fn a_stalled_hpet_bracket_is_read_again() {
        // The first two readings are paused just over HPET_BRACKET_NS (1 µs
        // over) after the HPET read, as a host descheduling the vCPU (or a
        // slow HPET read) would: both are too wide, so another is taken and
        // the first narrow one kept, and the wide widths are still reported.
        // (At least two retries: a loaded host can stall an unpaused reading
        // too.) A limit looser than HPET_BRACKET_NS would keep the first.
        use crate::kassert;
        const OVER_NS: u64 = HPET_BRACKET_NS + 1_000;
        let hpet = hpet().ok_or("no HPET")?;
        let mut paused = 0;
        let r = narrow_hpet_reading(&hpet, START_READING, INJECTED, |p| {
            if p == Pause::StartAfterRead && paused < 2 {
                paused += 1;
                spin(OVER_NS);
            }
        })?;
        let widths_ns = r.widths();
        crate::ktest::diag(format_args!("{INJECTED}bracket widths {widths_ns:?} ns"));
        kassert!(r.retries() >= 2 && widths_ns[..2].iter().all(|&w| w >= OVER_NS));
        kassert!(r.reading().is_some_and(|b| Some(&b.width) == widths_ns.last() && b.width <= HPET_BRACKET_NS));
        Ok(())
    }
);

crate::ktest!(
    fn every_stalled_hpet_bracket_is_a_failure() {
        // Every reading at one end paused STALL_NS inside its bracket: at
        // the start (no spin follows), or at the end only (after a 1 ms
        // spin). No reading at that end is trusted, so the check fails with
        // its own reason (and a diag listing every width), not a pass or a
        // skip.
        use crate::kassert_eq;
        let hpet = hpet().ok_or("no HPET")?;
        for at in [Pause::StartAfterRead, Pause::EndAfterRead] {
            let mut paused = 0;
            let result = now_ns_against_the_hpet(&hpet, 1_000_000, INJECTED, |p| {
                if p == at {
                    paused += 1;
                    spin(STALL_NS);
                }
            });
            kassert_eq!(result.err(), Some(EVERY_BRACKET_WIDE));
            kassert_eq!(paused, HPET_BRACKET_TRIES); // every reading at that end
        }
        Ok(())
    }
);

crate::ktest!(
    fn a_burst_of_stalls_is_sampled_around() {
        // Every reading that starts within 50 ms of the first is paused
        // STALL_NS inside its bracket, as a burst of slow HPET reads on a
        // loaded host would (load-5 in PR #121 saw one last 60 ms or more).
        // Spaced RETRY_SPACING_NS apart, the first three readings fall in it
        // and a later one after it is kept; taken back to back, every reading
        // would fall in it.
        use crate::kassert;
        const BURST_NS: u64 = 50_000_000;
        let hpet = hpet().ok_or("no HPET")?;
        let (mut first_ns, mut paused) = (None, 0);
        let r = narrow_hpet_reading(&hpet, START_READING, INJECTED, |p| {
            if p == Pause::StartAfterRead {
                let now = now_ns();
                // The first two always: a late wake from the first sleep
                // can't end the burst before the second reading.
                if now - *first_ns.get_or_insert(now) < BURST_NS || paused < 2 {
                    paused += 1;
                    spin(STALL_NS);
                }
            }
        })?;
        let widths_ns = r.widths();
        crate::ktest::diag(format_args!(
            "{INJECTED}a {BURST_NS} ns burst: bracket widths {widths_ns:?} ns, {} ms apart",
            RETRY_SPACING_NS / 1_000_000
        ));
        kassert!(r.reading().is_some_and(|b| b.width <= HPET_BRACKET_NS));
        kassert!(r.retries() >= 2 && widths_ns[..2].iter().all(|&w| w >= STALL_NS));
        Ok(())
    }
);

crate::ktest!(
    fn one_stalled_reading_at_each_end_still_passes() {
        // The full 50 ms check with the first reading at each end paused
        // 5 ms: at the start after its HPET read, at the end before it.
        // Kept, the start's HPET value would be 2.5 ms before its midpoint
        // and the end's 2.5 ms after, so the clocks would come out 5 ms
        // apart, over the 1 ms budget. Each is read again, and the check
        // passes on the narrow readings.
        use crate::kassert;
        const PAUSE_NS: u64 = 5_000_000;
        let hpet = hpet().ok_or("no HPET")?;
        let mut paused = [false; 2];
        let [start, end] = now_ns_against_the_hpet(&hpet, NOW_NS_SPIN_NS, INJECTED, |p| {
            let end = match p {
                Pause::StartAfterRead => 0,
                Pause::EndBeforeRead => 1,
                _ => return,
            };
            if !paused[end] {
                paused[end] = true;
                spin(PAUSE_NS);
            }
        })?;
        for e in [start, end] {
            kassert!(e.widths().len() >= 2 && e.widths()[0] >= PAUSE_NS);
        }
        Ok(())
    }
);

crate::ktest!(
    fn sleep_10ms_elapses() {
        // Measured with the HPET and the tick count, not the TSC that
        // `sleep` itself waits on. The HPET readings aren't bracketed (#117):
        // they come before and after the sleep, so a stall next to either
        // only lengthens the interval, and only its 1 s upper bound could
        // catch that; the tick count is only checked to have moved.
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
        // its calibrated input; and its input counting at that rate by the
        // HPET. This checks the rate without counting interrupts (an
        // emulator delivers them late).
        use crate::{kassert, kassert_eq};
        kassert_eq!(TICK_HZ, 1000);
        let (lvt, error_lvt, initial) = apic::timer_registers();
        kassert_eq!((lvt & 0xff, lvt & (1 << 16), lvt & (1 << 17)), (u32::from(idt::TIMER), 0, 1 << 17));
        kassert_eq!(error_lvt & 0x1_00ff, u32::from(idt::LAPIC_ERROR)); // unmasked
        let timer = lapic_timer().ok_or("timer not calibrated")?;
        kassert_eq!(Some(initial), lumen_time::periodic_count(timer.hz(), 1000));
        // Divided by 16, as calibrated: the rate measured below is the
        // timer's input after the divider the periodic setup left.
        kassert_eq!(apic::timer_divide_config(), apic::DIVIDE_BY_16);
        let hpet = hpet().ok_or("no HPET")?;
        let tsc = tsc().ok_or("TSC not calibrated")?;
        // The rate, by the HPET, measured in one-shot mode (#129): the timer
        // masked and started from u32::MAX, which at 62.5 MHz runs 68 s before
        // it would reach 0, so a count over any window is exact. Periodic, it
        // reloads every 1 ms and a count says nothing about how many reloads
        // passed; the old 750 µs windows each had to stay under the period,
        // and a loaded KVM runner's HPET reads (0.5 ms and more in bursts)
        // stretched most past it, or ate the 5% budget (37.5 µs) with two
        // brackets. Overcommitted KVM pauses the vCPU for longer than a
        // period often enough that no polling can count across reloads
        // either. Switching to one-shot writes only the LVT, so this is the
        // input the periodic count divides: the divider is read again at
        // the end of the measurement, before the tick is restarted (which
        // writes it). 7 windows of 20 ms by the HPET, each end a timer
        // reading between two HPET reads, placed at their midpoint (#117):
        // two 35 µs brackets put a window off by at most 35 µs (half of
        // each), 0.175% of it, and one a burst of wide brackets throws off is
        // a sample the median absorbs. How many windows' brackets added up
        // to over 5% of them is reported, with the widths and every sample.
        // It runs with the tick stopped (`with_tick_stopped`, which checks
        // the timer's registers read as before afterwards). A window over
        // which the timer counted nothing (or up), or over which the HPET
        // didn't reach 20 ms within 1 s by the TSC, fails the test there.
        const ONE_SHOT_WINDOWS: usize = 7;
        let window_ticks = hpet.clock().ticks(TIMER_ONE_SHOT_WINDOW_NS);
        let hpet_bound = tsc.ticks(lumen_time::NS_PER_SEC);
        let mut samples_hz = [0u64; ONE_SHOT_WINDOWS];
        let mut widths_ticks = [0u64; 2 * ONE_SHOT_WINDOWS];
        let mut wide = 0;
        let during = with_tick_stopped(|| {
            apic::timer_mask_one_shot();
            apic::timer_start(u32::MAX);
            for i in 0..ONE_SHOT_WINDOWS {
                let start = timer_between_hpet(&hpet, || {});
                let waiting = arch::rdtsc();
                while hpet.since(start.at, hpet.now()) < window_ticks {
                    if arch::rdtsc().wrapping_sub(waiting) > hpet_bound {
                        break;
                    }
                    core::hint::spin_loop();
                }
                let end = timer_between_hpet(&hpet, || {});
                (widths_ticks[2 * i], widths_ticks[2 * i + 1]) = (start.width, end.width);
                let elapsed_ticks = hpet.since(start.at, end.at);
                let so_far = |what: fmt::Arguments<'_>| {
                    crate::ktest::diag(format_args!(
                        "window {i}: {what}; bracket widths so far {:?} HPET ticks, samples so far {:?} Hz",
                        &widths_ticks[..2 * i + 2],
                        &samples_hz[..i]
                    ))
                };
                if elapsed_ticks < window_ticks {
                    so_far(format_args!(
                        "the HPET counted {elapsed_ticks} of {window_ticks} ticks within 1 s by the TSC"
                    ));
                    return Err("the HPET stalled in a one-shot window");
                }
                if start.width.saturating_add(end.width).saturating_mul(20) > elapsed_ticks {
                    wide += 1;
                }
                let counted = start.value.saturating_sub(end.value);
                if counted == 0 {
                    so_far(format_args!(
                        "the timer counted nothing over {elapsed_ticks} HPET ticks (it read {}, then {})",
                        start.value, end.value
                    ));
                    return Err("the timer didn't count over a window");
                }
                samples_hz[i] =
                    lumen_time::frequency(u64::from(counted), elapsed_ticks, hpet.clock().hz()).unwrap_or(0);
            }
            Ok(apic::timer_divide_config())
        })??;
        // The divider as the measurement left it: not rewritten.
        kassert_eq!(during, apic::DIVIDE_BY_16);
        let median_ticks = lumen_time::median(&mut widths_ticks).unwrap_or(0); // sorts them
        let ns = |ticks: u64| hpet.clock().ns(ticks);
        crate::ktest::diag(format_args!(
            "bracket widths min/median/max {}/{}/{} ns over {} brackets; {wide} of {ONE_SHOT_WINDOWS} one-shot windows \
             with brackets over 5% of them",
            ns(widths_ticks[0]),
            ns(median_ticks),
            ns(widths_ticks[2 * ONE_SHOT_WINDOWS - 1]),
            2 * ONE_SHOT_WINDOWS,
        ));
        crate::ktest::diag(format_args!("samples {samples_hz:?} Hz"));
        let rate = lumen_time::median(&mut samples_hz).ok_or("no samples")?;
        crate::ktest::diag(format_args!("timer counts at {} (calibrated {})", Hz(rate), Hz(timer.hz())));
        kassert!(rate.abs_diff(timer.hz()) * 20 <= timer.hz());
        Ok(())
    }
);

crate::ktest!(
    fn a_timer_reading_is_placed_at_its_brackets_midpoint() {
        // A timer reading paused 400 µs before and 400 µs after its count is
        // read (an 800 µs bracket) is placed at its midpoint, where the timer
        // was read: from a reference reading, the HPET midpoints and the
        // timer's count (at its calibrated rate) agree to under 200 µs (half
        // the pause). Reading the timer outside its bracket, or placing it at
        // either end, puts it 400 µs off. As for the HPET's placement test, a
        // try is judged only if `lumen_time::placement_is_judged` says so, so
        // a right placement is at most 100 µs off and a wrong one at least
        // 300 µs (the reference at most 100 µs wide, so even misplaced the
        // same way it can't make up the difference). The two readings, with
        // that uncertainty, must also be under TIMER_WINDOW_MAX_NS apart: the
        // timer reloads every 1 ms, and its count says nothing about how many
        // reloads passed. Other tries are taken again, RETRY_SPACING_NS apart,
        // each with its reason in the diag.
        use crate::kassert;
        const PAUSE_NS: u64 = 400_000;
        const TRIES: usize = 8;
        let (_, _, initial) = apic::timer_registers();
        let timer = lapic_timer().ok_or("timer not calibrated")?;
        let hpet = hpet().ok_or("no HPET")?;
        let clock = hpet.clock();
        // Each try's (why it was taken again, reference width, paused width,
        // HPET ns between the midpoints).
        let mut tried = [("", 0u64, 0u64, 0u64); TRIES];
        for n in 0..TRIES {
            space_retry(n);
            let _off = arch::irq::push_off();
            let reference = timer_between_hpet(&hpet, || {});
            let paused = timer_between_hpet(&hpet, || spin(PAUSE_NS));
            let by_hpet_ns = clock.ns(hpet.since(reference.at, paused.at));
            let (reference_ns, paused_ns) = (clock.ns(reference.width), clock.ns(paused.width));
            kassert!(paused_ns.saturating_add(PAUSE_SLACK_NS) >= 2 * PAUSE_NS);
            let uncertainty_ns = lumen_time::placement_uncertainty(reference_ns, paused_ns, PAUSE_NS);
            let why = if !lumen_time::placement_is_judged(reference_ns, paused_ns, PAUSE_NS) {
                "stalled"
            } else if by_hpet_ns.saturating_add(uncertainty_ns) >= TIMER_WINDOW_MAX_NS {
                "near a reload"
            } else {
                ""
            };
            tried[n] = (why, reference_ns, paused_ns, by_hpet_ns);
            if !why.is_empty() {
                continue;
            }
            let counted = lumen_time::down_counter_delta(reference.value, paused.value, initial);
            let by_timer_ns = timer.ns(u64::from(counted));
            let off_ns = by_hpet_ns.abs_diff(by_timer_ns);
            crate::ktest::diag(format_args!(
                "{INJECTED}timer placement {off_ns} ns off (at most {} ns allowed) on try {} of {TRIES}, {} ms apart; \
                 {by_hpet_ns} ns by the HPET, {by_timer_ns} ns by the timer; tries (taken again because, \
                 reference, paused, by the HPET, in ns) {:?}",
                PAUSE_NS / 2,
                n + 1,
                RETRY_SPACING_NS / 1_000_000,
                &tried[..=n],
            ));
            kassert!(off_ns * 2 <= PAUSE_NS);
            return Ok(());
        }
        crate::ktest::diag(format_args!(
            "{INJECTED}no try could be judged: (taken again because, reference, paused, by the HPET, in ns) {tried:?}"
        ));
        Err("no timer placement try could be judged")
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
        // 1 kHz (100 in 100 ms; the bound, 150, leaves room). The timer's
        // input rate is checked by `the_timer_is_programmed_for_1_khz` (against the
        // HPET): QEMU's TCG delivers ticks late (late periodic timers skip
        // periods): 29 to 81 in #45's stress runs (busy-waiting then; the
        // sleep now halts in the idle thread), and a lock held across a
        // period merges them.
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
        // Spinning, not sleeping: switching to the idle thread and back is
        // traced.
        let ring = crate::trace::ring();
        let (t0, w0) = (ticks(), ring.written());
        spin(20_000_000);
        kassert!(ticks() > t0);
        kassert_eq!(ring.written(), w0);
        Ok(())
    }
);
