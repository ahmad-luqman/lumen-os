//! Timekeeping: the TSC's frequency, calibrated at boot against the HPET (or
//! the PIT if there is none, it doesn't count, its rate says nothing, or the
//! PIT's is better), [`now_ns`], [`sleep`] and
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
use lumen_time::{Clock, Hz, PIT_HZ, Pause, PitBadReadback, Stall, Steadiest, Verdict, Window};

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
/// The longest a window may take, in TSC ticks (TSC unknown yet: over 1 s at
/// up to 4 GHz, past the longest stretched HPET window, 160 ms), before its
/// reference counts as stalled.
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

/// A calibration: the median of its windows' frequencies, what of (the TSC
/// against a reference, or the local APIC timer against the TSC), the
/// windows (the steady ones first, in the order they were taken; if attempts
/// ran out, the unsteady ones with the smallest bounds fill the rest), and
/// how many unsteady windows were taken (each followed by another, unless
/// attempts ran out; filling in, some are kept) and the last of those.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Calibration {
    pub hz: u64,
    pub windows: [Window; WINDOWS],
    pub retaken: usize,
    pub last_retaken: Option<Window>,
    pub of: Calibrated,
}

/// What a calibration is of, which decides how far off its windows' rates
/// can be and what tolerance judges it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Calibrated {
    /// The TSC, against a reference: each window off by at most
    /// [`Window::tsc_error_ppm`].
    Tsc(Reference),
    /// The local APIC timer, against the TSC: each window off by at most
    /// [`Window::gap_ppm`], and the TSC's own calibration, off by up to
    /// `tsc_ppm`, on top ([`lumen_time::compose_ppm`]).
    Timer { tsc_ppm: u64 },
}

impl Calibrated {
    /// The rate `w` gives against the clock known to count at `known_hz` (the
    /// reference for the TSC, the TSC for the timer): for the TSC, an
    /// implausible frequency or none is an error.
    fn rate(self, w: &Window, known_hz: u64) -> Result<u64, CalError> {
        match self {
            Calibrated::Tsc(_) => match w.tsc_hz(known_hz) {
                Some(hz) if lumen_time::plausible_tsc_hz(hz) => Ok(hz),
                Some(hz) => Err(CalError::Implausible(hz)),
                None => Err(CalError::NoCount),
            },
            Calibrated::Timer { .. } => w.reference_hz(known_hz).ok_or(CalError::NoCount),
        }
    }

    /// The most one window's rate can be off, in ppm of the true rate.
    fn window_bound(self, w: &Window) -> u64 {
        match self {
            Calibrated::Tsc(_) => w.tsc_error_ppm(),
            Calibrated::Timer { .. } => w.gap_ppm(),
        }
    }

    /// How far off its boot calibration may be before boot warns
    /// ([`lumen_time::TSC_TOLERANCE_PPM`], [`lumen_time::TIMER_TOLERANCE_PPM`]).
    fn tolerance_ppm(self) -> u64 {
        match self {
            Calibrated::Tsc(_) => lumen_time::TSC_TOLERANCE_PPM,
            Calibrated::Timer { .. } => lumen_time::TIMER_TOLERANCE_PPM,
        }
    }

    /// Its name in a log line ("tsc calibration ...").
    fn name(self) -> &'static str {
        match self {
            Calibrated::Tsc(_) => "tsc",
            Calibrated::Timer { .. } => "local APIC timer",
        }
    }

    /// What a window's ends read, for "a stall next to ...".
    fn reads(self) -> &'static str {
        match self {
            Calibrated::Tsc(Reference::Hpet) => "the HPET's reads",
            Calibrated::Tsc(Reference::Pit) => "the PIT's reads",
            Calibrated::Timer { .. } => "the timer's register accesses",
        }
    }

    /// What it is calibrated against.
    fn against(self) -> &'static str {
        match self {
            Calibrated::Tsc(Reference::Hpet) => "the HPET",
            Calibrated::Tsc(Reference::Pit) => "the PIT",
            Calibrated::Timer { .. } => "the TSC",
        }
    }

    /// Why a median with no bound left says nothing. A TSC window's bound
    /// reaches 100% when its end gap is as long as the rest of it (half of
    /// it); a timer window's when its gaps are as long as itself.
    fn no_information(self) -> &'static str {
        match self {
            Calibrated::Tsc(_) => "the middle ones' end stalls were as long as the rest of their windows",
            Calibrated::Timer { .. } => "the middle ones' stalls were as long as themselves",
        }
    }
}

impl Calibration {
    /// How many of its windows are steady (the first ones).
    pub fn steady(&self) -> usize {
        self.windows.iter().filter(|w| w.is_steady()).count()
    }

    /// The most its windows' median can be off, in ppm, on their own: the
    /// `(N / 2 + 1)`-th smallest of their bounds
    /// ([`lumen_time::median_bound_ppm`]).
    pub fn windows_bound_ppm(&self) -> u64 {
        lumen_time::median_bound_ppm(&self.windows, |w| self.of.window_bound(w))
    }

    /// The most its median can be off, in ppm of the true rate: its windows'
    /// bound ([`windows_bound_ppm`](Self::windows_bound_ppm)), with, for the
    /// timer, the TSC's added.
    pub fn median_bound_ppm(&self) -> u64 {
        match self.of {
            Calibrated::Tsc(_) => self.windows_bound_ppm(),
            Calibrated::Timer { tsc_ppm } => lumen_time::compose_ppm(self.windows_bound_ppm(), tsc_ppm),
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
    /// rate it gives says nothing ([`Verdict::Unusable`]). Only the TSC's
    /// calibrations give it.
    NoInformation,
    /// The PIT's counter read back values that don't fit how it was
    /// programmed.
    BadReadback(PitBadReadback),
}

impl fmt::Display for CalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalError::NoCount => f.write_str("it didn't count through a window"),
            CalError::Implausible(hz) => write!(f, "it gave an implausible TSC frequency, {}", Hz(*hz)),
            CalError::NoInformation => f.write_str(
                "its windows' end stalls were as long as the rest of them, so the rate it gives says nothing",
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

/// The local APIC timer's boot calibration, with its windows (None before
/// [`init`]).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: a ktest
pub fn boot_timer_calibration() -> Option<Calibration> {
    *BOOT_TIMER_CALIBRATION.lock()
}

/// The HPET, if it counted at [`init`] (whichever clock calibrated the TSC:
/// [`reference`] says which).
pub fn hpet() -> Option<HpetCounter> {
    *HPET.lock()
}

/// A calibration `of` a clock from up to [`ATTEMPTS`] windows from `take`,
/// against the clock known to count at `known_hz`: the median rate of
/// [`WINDOWS`] of them ([`median_calibration`]). An unsteady one is set aside
/// and another taken after `pause` (given the ruler,
/// [`lumen_time::steadiest_windows`]); if attempts run out first, the ones
/// with the smallest bounds for that rate fill the rest, and [`report`]
/// judges how far off the median can be (#139: a slowed host's reads
/// outlast every retake). Ranking and building use the same `of`.
fn calibrate(
    of: Calibrated,
    known_hz: u64,
    take: impl FnMut() -> Result<Window, CalError>,
    pause: impl FnMut(u64),
) -> Result<Calibration, CalError> {
    let got = lumen_time::steadiest_windows::<CalError, WINDOWS>(ATTEMPTS, take, |w| of.window_bound(w), pause)?;
    median_calibration(&got, of, known_hz)
}

/// The TSC against `reference`, counting at `reference_hz` ([`calibrate`]),
/// retakes spaced by [`tsc_retake_pause`].
fn median_tsc_hz(
    reference: Reference,
    reference_hz: u64,
    take: impl FnMut() -> Result<Window, CalError>,
) -> Result<Calibration, CalError> {
    calibrate(Calibrated::Tsc(reference), reference_hz, take, tsc_retake_pause)
}

/// How many windows' worth of TSC ticks the TSC's calibration waits before a
/// window taken again: the TSC isn't calibrated yet, but every window spans
/// at least [`WINDOW_NS`] of the reference from its start reading, so the
/// fewest TSC ticks any spanned (a stall only adds) is about that long: two
/// of them, about 20 ms, as the local APIC timer's retakes wait
/// ([`TIMER_RETRY_SPACING_NS`], where #129's loaded KVM boots showed
/// back-to-back retakes running out inside one burst of stalls and spaced
/// ones not).
const TSC_RETRY_SPACING_WINDOWS: u64 = 2;

/// The wait before a TSC calibration window taken again:
/// [`TSC_RETRY_SPACING_WINDOWS`] × `ruler` TSC ticks.
fn tsc_retake_pause(ruler: u64) {
    spin_ticks(ruler.saturating_mul(TSC_RETRY_SPACING_WINDOWS));
}

/// Busy-wait `ticks` raw TSC ticks (before the TSC is calibrated too;
/// interrupts are off at boot). `u64::MAX` (no ruler yet) waits not at all.
fn spin_ticks(ticks: u64) {
    if ticks == u64::MAX {
        return;
    }
    let start = arch::rdtsc();
    while arch::rdtsc().wrapping_sub(start) < ticks {
        core::hint::spin_loop();
    }
}

/// A calibration of `of` from `got`: the median of the rates its windows
/// give against the clock known to count at `known_hz` ([`Calibrated::rate`]),
/// or the first error from a window with a bound. A window with none (a
/// filled-in one that can't be placed) counts as 0: with fewer than half of
/// them the median lies between bounded windows' rates whatever it is, and
/// with more the calibration has no bound itself ([`report`]).
fn median_calibration(got: &Steadiest<WINDOWS>, of: Calibrated, known_hz: u64) -> Result<Calibration, CalError> {
    let mut hz = [0u64; WINDOWS];
    for (h, w) in hz.iter_mut().zip(got.windows()) {
        *h = match of.rate(w, known_hz) {
            Ok(rate) => rate,
            Err(_) if of.window_bound(w) >= lumen_time::NO_INFORMATION_PPM => 0,
            Err(e) => return Err(e),
        };
    }
    let hz = lumen_time::median(&mut hz).expect("WINDOWS samples");
    let aside = got.aside();
    Ok(Calibration { hz, windows: *got.windows(), retaken: aside.count, last_retaken: aside.last, of })
}

/// A [`Pause`] hook that stalls each window's end by `stall.end` parts per
/// million of how long it ran, and its start (from the second window) by
/// `stall.start` ppm of how long the previous one ran, busy, by raw TSC
/// ticks ([`spin_ticks`]). (A stretched window stalls longer; that doesn't
/// feed the stretch: [`lumen_time::stretched_for_gaps`] counts each gap
/// for at most two polls.) The boot's calibrations take it from
/// `lumen.stall_tsc_windows=<end ppm>[,<start ppm>]` and
/// `lumen.stall_timer_windows=...` ([`stall_from_cmdline`]: a deliberate fault
/// for scenarios, like the shell's `crash`), so a real boot reaches the
/// fill-in, its warning and its panic. (Under the timer's bound,
/// [`Window::gap_ppm`], an end stall alone never fills a window, which counts
/// it: a start stall can.) With both 0 it does nothing (no TSC read inside a
/// bracket).
fn stall_window_ends(stall: Stall) -> impl FnMut(Pause) {
    let (mut started, mut last_run) = (0u64, 0u64);
    move |at| match at {
        Pause::StartAfterRead if stall != Stall::default() => {
            spin_ticks(lumen_time::stall_ticks(last_run, stall.start));
            started = arch::rdtsc();
        }
        Pause::EndAfterRead if stall != Stall::default() => {
            last_run = arch::rdtsc().wrapping_sub(started);
            spin_ticks(lumen_time::stall_ticks(last_run, stall.end));
        }
        _ => {}
    }
}

/// The `lumen.stall_*_windows=<end ppm>[,<start ppm>]` value for `key`
/// ([`lumen_time::parse_stall`], up to [`lumen_time::MOST_STALL_PPM`]), none
/// without one. One that doesn't parse is warned about and ignored; one in
/// effect is said once at INFO, not as a warning: test_time's stall
/// scenarios allow a `time` warning only from a calibration's verdict
/// (`lumen_harness.calibration.no_other_warnings`).
fn stall_from_cmdline(key: &str) -> Stall {
    let Some(v) = crate::cmdline::value(key) else { return Stall::default() };
    match lumen_time::parse_stall(v) {
        Some(stall) => {
            if stall != Stall::default() {
                info!(target: "time", "stalling calibration windows by {},{} ppm ({key})", stall.end, stall.start);
            }
            stall
        }
        None => {
            warn!(
                target: "time",
                "{key}={v}: not <end ppm>[,<start ppm>], each at most {}; ignored",
                lumen_time::MOST_STALL_PPM
            );
            Stall::default()
        }
    }
}

/// How long an HPET window may get ([`lumen_time::stretched_for_gaps`]): 16
/// windows' worth, 160 ms (the 32-bit counter wraps after 43 s).
const MOST_HPET_STRETCH: u64 = 16;

// Pinned: interrupts are off through a calibration, so its longest (20
// windows at the cap and 19 waits, about 3.6 s, as `measure` says) is part
// of what this allows. KVM's loaded runs reached the cap before the burst
// cap (e116d8a's, #139) and stayed steady there; since, they stretched to
// 36 ms at most.
const _: () = assert!(MOST_HPET_STRETCH == 16);

/// The TSC's frequency against the HPET, `pause` called at each window's
/// [`Pause`] points (see [`hpet_window`]). A window whose ends were slow to
/// read (each read slow, not one burst: [`lumen_time::stretched_for_gaps`])
/// is taken again longer, up to [`MOST_HPET_STRETCH`] times (#139: on KVM a
/// normal window's gaps already take 0.23% of it, and a slowed host's HPET
/// reads took 20 times as long, 16 of 20 windows unsteady however late they
/// were taken; the PIT can't stretch past `pit_end_unambiguous`'s 13,107
/// ticks, about 11 ms). Call with interrupts off.
fn calibrate_with_hpet(hpet: &HpetCounter, mut pause: impl FnMut(Pause)) -> Result<Calibration, CalError> {
    let base = hpet.clock().ticks(WINDOW_NS);
    let mut target = base;
    median_tsc_hz(Reference::Hpet, hpet.clock().hz(), || {
        let w = hpet_window(hpet, target, &mut pause)?;
        target = lumen_time::stretched_for_gaps(target, &w, base.saturating_mul(MOST_HPET_STRETCH));
        Ok(w)
    })
}

/// One window of at least `target` HPET ticks, each end a counter read between
/// two TSC reads. `pause` is called at the `Pause` points in those readings
/// (a no-op but in tests and stalled scenarios, [`stall_window_ends`]);
/// [`Pause::EndBeforeRead`] on every poll, since any read may be the last.
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
    median_tsc_hz(Reference::Pit, PIT_HZ, || pit_window(count, &mut pause))
}

/// One window of channel 2 started on `count` (see [`lumen_time::pit_window`]);
/// `pause`: see [`pit::wait`].
fn pit_window(count: u16, pause: impl FnMut(Pause)) -> Result<Window, CalError> {
    let w = pit::wait(count, pause).ok_or(CalError::NoCount)?;
    lumen_time::pit_window(count, &w).map_err(CalError::BadReadback)
}

/// Measure the TSC's frequency again now, against `reference`, with
/// interrupts off meanwhile: at least 50 ms; at most, with every window
/// retaken, 20 windows and 19 waits of about 20 ms each, about 0.6 s against
/// the PIT and 3.6 s against the HPET (its windows stretched to 160 ms).
/// `None` if there is no such clock (no HPET that counts), else what
/// calibrating against it gave ([`with_a_bound`]). For cross-checking the
/// boot calibration (the shell's `clocks`, ktests).
pub fn measure(reference: Reference) -> Option<Result<Calibration, CalError>> {
    let _off = arch::irq::push_off();
    let cal = match reference {
        Reference::Hpet => calibrate_with_hpet(&hpet()?, |_| {}),
        Reference::Pit => calibrate_with_pit(|_| {}),
    };
    Some(cal.and_then(with_a_bound))
}

/// `cal`, if its median has a bound under 100%; else
/// [`CalError::NoInformation`] (its rate says nothing).
fn with_a_bound(cal: Calibration) -> Result<Calibration, CalError> {
    if cal.median_bound_ppm() >= lumen_time::NO_INFORMATION_PPM { Err(CalError::NoInformation) } else { Ok(cal) }
}

/// How long the local APIC timer's calibration waits before taking a window
/// again after an unsteady one: 20 ms. On a loaded KVM runner the vCPU
/// stalled next to the timer's register accesses in bursts lasting hundreds
/// of ms (#129: in one boot 17 of 20 windows taken back to back were
/// unsteady); spaced, 20 attempts span about 600 ms.
const TIMER_RETRY_SPACING_NS: u64 = 20_000_000;

/// The local APIC timer's input frequency against the (calibrated) TSC, off
/// by up to `tsc_ppm`: [`calibrate`] with windows from [`timer_window`],
/// `pause` called at each one's [`Pause`] points, retakes after
/// [`timer_retake_pause`]; [`init`] judges how far off the median can be, the
/// TSC's error included ([`report`]). Call with interrupts off.
fn calibrate_lapic_timer(tsc: Clock, tsc_ppm: u64, mut pause: impl FnMut(Pause)) -> Result<Calibration, CalError> {
    let window = tsc.ticks(WINDOW_NS);
    calibrate_timer_with(tsc, tsc_ppm, || timer_window(window, &mut pause))
}

/// [`calibrate_lapic_timer`] with its windows from `take` (ktests make them
/// up), against `tsc`, off by up to `tsc_ppm`.
fn calibrate_timer_with(
    tsc: Clock,
    tsc_ppm: u64,
    take: impl FnMut() -> Result<Window, CalError>,
) -> Result<Calibration, CalError> {
    calibrate(Calibrated::Timer { tsc_ppm }, tsc.hz(), take, |_| timer_retake_pause(tsc))
}

/// The wait before a timer calibration window taken again:
/// [`TIMER_RETRY_SPACING_NS`] by `tsc`, the clock being calibrated against.
fn timer_retake_pause(tsc: Clock) {
    spin_ticks(tsc.ticks(TIMER_RETRY_SPACING_NS));
}

/// A boot calibration judged ([`report`]): the verdict, the calibration and
/// its median's bound in ppm.
#[derive(Clone, Copy, Debug)]
struct Report {
    verdict: Verdict,
    cal: Calibration,
    bound_ppm: u64,
}

/// How `init` reports `cal`: by its median's bound
/// ([`Calibration::median_bound_ppm`], the TSC's included for the timer)
/// against its clock's tolerance ([`lumen_time::verdict`]), whatever the
/// steady windows' count. Within it, at most an INFO line with the bound
/// (filling in is fine then); past it, a warning (a rate off by a few percent
/// still runs the system); with no bound under 100% the rate says nothing (a
/// tick from it could come arbitrarily fast or slow, a `sleep` last any
/// time): the HPET then gives way to the PIT, otherwise boot panics, as
/// when a clock doesn't count at all.
fn report(cal: &Calibration) -> Report {
    let bound_ppm = cal.median_bound_ppm();
    Report { verdict: lumen_time::verdict(bound_ppm, cal.of.tolerance_ppm(), cal.retaken), cal: *cal, bound_ppm }
}

/// What `init` does with a [`Report`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    /// Nothing to say (every window steady at once).
    Silent,
    /// Log it at this level.
    Log(log::Level),
    /// Panic with it: the rate says nothing.
    Panic,
}

impl Report {
    fn action(&self) -> Action {
        match self.verdict {
            Verdict::Clean => Action::Silent,
            Verdict::Retook => Action::Log(log::Level::Info),
            Verdict::Imprecise => Action::Log(log::Level::Warn),
            Verdict::Unusable => Action::Panic,
        }
    }
}

/// A bound in ppm as a percentage, rounded up to a tenth (`0.7%`), or `no
/// bound` from [`lumen_time::NO_INFORMATION_PPM`] on.
struct Percent(u64);

impl fmt::Display for Percent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 >= lumen_time::NO_INFORMATION_PPM {
            return f.write_str("no bound");
        }
        let tenths = self.0.div_ceil(1_000);
        write!(f, "{}.{}%", tenths / 10, tenths % 10)
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Report { cal, bound_ppm, .. } = *self;
        let of = cal.of;
        let (name, steady, bound) = (of.name(), cal.steady(), Percent(bound_ppm));
        // The windows taken: the steady ones and the unsteady ones (at most
        // ATTEMPTS).
        let taken = steady + cal.retaken;
        let tolerance = Percent(of.tolerance_ppm());
        match (self.verdict, of) {
            (Verdict::Clean, _) => write!(f, "{name} calibration: every window steady"),
            (Verdict::Retook, _) => write!(
                f,
                "{name} calibration took {} windows again, {steady} of {WINDOWS} kept steady, median within \
                 {bound} (read too late to trust; the last, tsc/reference/polls/start_gap/end_gap: {})",
                cal.retaken,
                cal.last_retaken.unwrap_or_default()
            ),
            // The timer's own windows within its tolerance: the TSC it was
            // measured against is what takes it past.
            (Verdict::Imprecise, Calibrated::Timer { tsc_ppm }) if cal.windows_bound_ppm() <= of.tolerance_ppm() => {
                write!(
                    f,
                    "{name} calibration: {steady} of {taken} windows read promptly, but the TSC it is measured \
                     against may be off by up to {}, so its rate may be off by up to {bound}, over the {tolerance} \
                     allowed (tsc/reference/polls/start_gap/end_gap: {cal})",
                    Percent(tsc_ppm)
                )
            }
            (Verdict::Imprecise, _) => {
                write!(
                    f,
                    "{name} calibration: only {steady} of {taken} windows read promptly ({WINDOWS} wanted; a stall \
                     next to {}), so its rate may be off by up to {bound}",
                    of.reads()
                )?;
                if let Calibrated::Timer { tsc_ppm: tsc_ppm @ 1.. } = of {
                    write!(f, " ({} of it the TSC's calibration)", Percent(tsc_ppm))?;
                }
                write!(f, ", over the {tolerance} allowed (tsc/reference/polls/start_gap/end_gap: {cal})")
            }
            (Verdict::Unusable, _) => write!(
                f,
                "can't calibrate the {name} against {}: only {steady} of {taken} windows read promptly \
                 ({WINDOWS} wanted), and {}, so the rate says nothing (tsc/reference/polls/start_gap/end_gap: {cal})",
                of.against(),
                of.no_information()
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
/// is called at the [`Pause`] points (a no-op but in tests and stalled
/// scenarios). Call with interrupts off. A count of 0 (it ran out, or reads
/// as 0) is
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

/// The TSC calibrated, judged, and against what: against the HPET ACPI
/// lists, or against the PIT if there is none, it doesn't count, or its rate
/// says nothing (warned about), with `stall` stalled into each window
/// ([`stall_window_ends`]; none but in scenarios). An HPET whose windows were
/// stalled is still used, its windows filled in or stretched (#139); one past
/// the TSC's tolerance is weighed against the PIT ([`the_steadier`]). An HPET
/// that counted is kept for [`hpet`] whichever calibrated the TSC. Panics if
/// neither counts, or the PIT's rate says nothing.
fn calibrate_tsc(stall: Stall) -> (Report, Reference) {
    let mut hpet_error = None;
    if let Some(addr) = crate::acpi::info().and_then(|i| i.hpet).map(|h| h.address) {
        let calibrated = HpetCounter::new(addr).and_then(|hpet| {
            calibrate_with_hpet(&hpet, stall_window_ends(stall)).map(|cal| (hpet, cal)).map_err(HpetError::Window)
        });
        match calibrated {
            Ok((hpet, cal)) => {
                // It counts: a second clock for `clocks` and the ktests,
                // whatever calibrates the TSC.
                *HPET.lock() = Some(hpet);
                let judged = report(&cal);
                match judged.verdict {
                    Verdict::Clean | Verdict::Retook => return (judged, Reference::Hpet),
                    Verdict::Imprecise => {
                        let kept = the_steadier(judged, || calibrate_with_pit(stall_window_ends(stall)));
                        let Calibrated::Tsc(reference) = kept.cal.of else { unreachable!("a TSC calibration") };
                        return (kept, reference);
                    }
                    Verdict::Unusable => {
                        warn!(target: "time", "{judged}; calibrating against the PIT");
                        hpet_error = Some(HpetError::Window(CalError::NoInformation));
                    }
                }
            }
            Err(e) => {
                warn!(target: "time", "the HPET at {addr:#x} is unusable ({e}); calibrating against the PIT");
                hpet_error = Some(e);
            }
        }
    }
    match (calibrate_with_pit(stall_window_ends(stall)), hpet_error) {
        (Ok(cal), _) => {
            let judged = report(&cal);
            if judged.action() == Action::Panic {
                panic!("{judged}");
            }
            (judged, Reference::Pit)
        }
        (Err(e), Some(h)) => panic!("can't calibrate the TSC: the HPET is unusable ({h}), and against the PIT {e}"),
        (Err(e), None) => panic!("can't calibrate the TSC: no HPET, and against the PIT {e}"),
    }
}

/// An HPET calibration past the TSC's tolerance (`hpet`, judged), weighed
/// against one taken against the PIT now (`pit`, [`with_a_bound`]): whichever's
/// median can be off less. A slowed host stalls the PIT too, so the HPET isn't
/// given up on its own word, nor kept on it. Says which it kept and why, at
/// INFO (the verdict on that one is logged after); the HPET if the PIT has no
/// bound, and if it doesn't count or reads back wrong (a broken PIT, a
/// WARN).
fn the_steadier(hpet: Report, pit: impl FnOnce() -> Result<Calibration, CalError>) -> Report {
    let past = Percent(lumen_time::TSC_TOLERANCE_PPM);
    let against_the_hpet = Percent(hpet.bound_ppm);
    match pit().and_then(with_a_bound) {
        Ok(cal) => {
            let pit = report(&cal);
            let kept = if pit.bound_ppm < hpet.bound_ppm { pit } else { hpet };
            info!(
                target: "time",
                "tsc calibration: against the HPET within {against_the_hpet} (past the {past} allowed), against the \
                 PIT within {}: kept {}",
                Percent(pit.bound_ppm),
                kept.cal.of.against()
            );
            kept
        }
        Err(CalError::NoInformation) => {
            info!(
                target: "time",
                "tsc calibration: against the HPET within {against_the_hpet} (past the {past} allowed), against the \
                 PIT with no bound: kept the HPET"
            );
            hpet
        }
        Err(e) => {
            warn!(
                target: "time",
                "tsc calibration: against the HPET within {against_the_hpet} (past the {past} allowed), against the \
                 PIT it failed ({e}): kept the HPET"
            );
            hpet
        }
    }
}

/// Do what `judged` says ([`Report::action`]): nothing, log it, or panic.
fn act_on(judged: &Report) {
    match judged.action() {
        Action::Silent => {}
        Action::Log(level) => log::log!(target: "time", level, "{judged}"),
        Action::Panic => panic!("{judged}"),
    }
}

/// Calibrate the TSC and start the local APIC timer at [`TICK_HZ`] (its
/// interrupts arrive once interrupts are enabled). Needs `acpi::init` and
/// `interrupts::init`; call with interrupts off. An HPET ACPI lists but that
/// doesn't count, or whose rate says nothing, is warned about (the PIT is
/// used instead). Each calibration is judged by how far off its median can
/// be ([`report`]): within its clock's tolerance, at most an INFO line; past
/// it, a warning; with no information, a panic. Panics too if no clock can
/// calibrate the TSC, or the local APIC timer doesn't count or can't tick at
/// 1 kHz.
pub fn init() {
    assert!(!arch::irq::enabled(), "time::init with interrupts on");
    let (tsc_report, reference) = calibrate_tsc(stall_from_cmdline("lumen.stall_tsc_windows"));
    let cal = tsc_report.cal;
    let hz = cal.hz;
    *BOOT_CALIBRATION.lock() = Some(cal);
    TSC_HZ.store(hz, Ordering::Relaxed);
    REFERENCE.store(reference as u8, Ordering::Relaxed);
    info!(target: "time", "tsc {} (calibrated against {reference})", Hz(hz));
    act_on(&tsc_report);

    let tsc = Clock::new(hz).expect("calibrated");
    let stall = stall_window_ends(stall_from_cmdline("lumen.stall_timer_windows"));
    let timer_cal = calibrate_lapic_timer(tsc, tsc_report.bound_ppm, stall)
        .unwrap_or_else(|e| panic!("can't calibrate the local APIC timer against the TSC: {e}"));
    *BOOT_TIMER_CALIBRATION.lock() = Some(timer_cal);
    let timer_hz = timer_cal.hz;
    act_on(&report(&timer_cal));
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
    spin_ticks(tsc().expect("time::spin before time::init").ticks(ns));
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
        // boot calibrated against (the ktest machine has one): within 2%, or
        // what the two calibrations' bounds allow if more (stalled windows
        // filling in). The PIT is measured again until its median is within
        // the TSC's tolerance, up to PIT_TRIES times RETRY_SPACING_NS apart:
        // a wider one could pass on its bound alone, which proves nothing.
        use crate::kassert;
        const PIT_TRIES: usize = 4;
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
        let pit = first_judgeable(
            PIT_TRIES,
            || measure(Reference::Pit).unwrap_or(Err(CalError::NoCount)),
            || spin(RETRY_SPACING_NS),
        )?;
        match measure(Reference::Hpet) {
            Some(Ok(hpet)) => crate::ktest::diag(format_args!("tsc now against the hpet {hpet}")),
            Some(Err(e)) => crate::ktest::diag(format_args!("tsc now against the hpet: failed ({e})")),
            None => crate::ktest::diag(format_args!("tsc now against the hpet: no HPET")),
        }
        let (boot_ppm, pit_ppm) = (boot.median_bound_ppm(), pit.median_bound_ppm());
        crate::ktest::diag(format_args!("bounds: boot {boot_ppm} ppm, pit {pit_ppm} ppm"));
        kassert!(lumen_time::within_bounds(pit.hz, hz, pit_ppm, boot_ppm, PIT_CROSS_CHECK_FLOOR_PPM));
        Ok(())
    }
);

/// The first of up to `tries` calibrations from `measure` (`space` called
/// between them) whose median is within the TSC's tolerance, each shown as
/// a diag; an error saying whether every try failed or every one was too
/// wide (a cross-check against a wider one could pass on its bound alone,
/// which proves nothing).
#[cfg(feature = "ktest")]
fn first_judgeable(
    tries: usize,
    mut measure: impl FnMut() -> Result<Calibration, CalError>,
    mut space: impl FnMut(),
) -> Result<Calibration, &'static str> {
    let mut wide = false;
    for tried in 0..tries {
        if tried > 0 {
            space();
        }
        match measure() {
            Ok(cal) => {
                let ppm = cal.median_bound_ppm();
                crate::ktest::diag(format_args!("tsc now against the pit {cal}, median within {ppm} ppm"));
                if ppm <= lumen_time::TSC_TOLERANCE_PPM {
                    return Ok(cal);
                }
                wide = true;
            }
            Err(e) => crate::ktest::diag(format_args!("against the pit: {e}")),
        }
    }
    Err(if wide {
        "the PIT re-measurement couldn't be judged: never within the TSC's tolerance"
    } else {
        "the PIT re-measurement couldn't be judged: every try failed"
    })
}

crate::ktest!(
    fn a_cross_check_measures_again_until_it_can_judge() {
        // A PIT measurement past the TSC's tolerance (or one that failed) is
        // taken again, spaced, until one is within it; never within it, none
        // (tsc_calibrated_within_tolerance then fails saying it couldn't
        // judge, instead of passing on a wide bound).
        use crate::kassert_eq;
        let w = |end_gap| Window { tsc: 10_000_000, reference: 11_932, polls: 900, start_gap: 0, end_gap };
        let cal = |end_gap| Calibration {
            hz: 2_500_000_000,
            windows: [w(end_gap); WINDOWS],
            retaken: 0,
            last_retaken: None,
            of: Calibrated::Tsc(Reference::Pit),
        };
        let (wide, fine) = (cal(200_000), cal(1_000));
        let mut queue = [Err(CalError::NoCount), Ok(wide), Ok(fine)].into_iter();
        let (mut taken, mut spaced) = (0, 0);
        let got = first_judgeable(
            4,
            || {
                taken += 1;
                queue.next().unwrap_or(Ok(wide))
            },
            || spaced += 1,
        );
        kassert_eq!((got, taken, spaced), (Ok(fine), 3, 2));
        let mut tries = 0;
        let never = first_judgeable(
            4,
            || {
                tries += 1;
                Ok(wide)
            },
            || {},
        );
        kassert_eq!(never, Err("the PIT re-measurement couldn't be judged: never within the TSC's tolerance"));
        kassert_eq!(tries, 4);
        let failing = first_judgeable(2, || Err(CalError::NoCount), || {});
        kassert_eq!(failing, Err("the PIT re-measurement couldn't be judged: every try failed"));
        Ok(())
    }
);

/// The least the boot calibration and the PIT measured again may differ by
/// in `tsc_calibrated_within_tolerance`, in ppm: 2%, twice the TSC's
/// tolerance, half for each clock's calibration.
#[cfg(feature = "ktest")]
const PIT_CROSS_CHECK_FLOOR_PPM: u64 = 2 * lumen_time::TSC_TOLERANCE_PPM;

/// The least a ktest comparing a measurement now with the boot calibration
/// allows them to differ by, in ppm (1%): their bounds leave out the host's
/// clock, which drives QEMU's references slewed while KVM's TSC isn't, and
/// the two are measured seconds apart ([`lumen_time::within_bounds`]).
#[cfg(feature = "ktest")]
const CROSS_CHECK_FLOOR_PPM: u64 = 10_000;

/// A PIT window paused 2 ms at `at` (Started, Ended or CountRead, which
/// shouldn't matter): the pause happened (for Started and Ended, the readings
/// show the 2 ms of ticks; CountRead's is after both), and a window taken as
/// calibration takes them (steady if one of [`ATTEMPTS`] is, retakes spaced,
/// else the least unsteady: a loaded host can stall every reading for a
/// while, #139, which this isn't about) still gives the TSC's frequency,
/// within [`CROSS_CHECK_FLOOR_PPM`], or what that window's gaps and the boot
/// calibration's allow if more ([`Window::tsc_error_ppm`],
/// [`Calibration::median_bound_ppm`], [`lumen_time::within_bounds`]). A window
/// that allows 10% or more can't tell: getting these wrong was 20% off.
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
            Window::tsc_error_ppm,
            |_| timer_retake_pause(tsc),
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
    kassert!(lumen_time::within_bounds(measured, hz, w.tsc_error_ppm(), boot_ppm, CROSS_CHECK_FLOOR_PPM));
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
        // TSC's frequency, within 1% (or its bound and the boot's, if more:
        // CROSS_CHECK_FLOOR_PPM).
        use crate::kassert;
        let hz = tsc().ok_or("TSC not calibrated")?.hz();
        let count = lumen_time::pit_count(WINDOW_NS).ok_or("no PIT count")?;
        let mut taken = 0;
        let cal = off_or_diag("calibrating against the PIT failed", || {
            median_tsc_hz(Reference::Pit, PIT_HZ, || {
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
        // Within CROSS_CHECK_FLOOR_PPM, or its bound and the boot's if more.
        let boot_ppm = boot_calibration().ok_or("no boot calibration")?.median_bound_ppm();
        kassert!(lumen_time::within_bounds(cal.hz, hz, cal.median_bound_ppm(), boot_ppm, CROSS_CHECK_FLOOR_PPM));
        Ok(())
    }
);

crate::ktest!(
    fn a_tsc_calibration_short_of_steady_windows_fills_in() {
        // A reference whose every window has a paused end (about 1,000,000
        // TSC ticks of 10,000,000 or so, 11,932 PIT ticks: about 11% each),
        // each a tick longer and its end gap 1,000 ticks shorter than the
        // last: all ATTEMPTS windows taken, each retake after a wait, the
        // five with the smallest bounds kept (the last five, least first),
        // the median and its bound theirs (the middle one's), past the TSC's
        // tolerance: a warning, not an error (#139).
        use crate::{kassert, kassert_eq};
        let w = |taken: u64| Window {
            tsc: 10_000_000 + taken,
            reference: 11_932,
            polls: 1,
            start_gap: 0,
            end_gap: 1_000_000 - 1_000 * taken,
        };
        let (mut taken, mut rulers) = (0, alloc::vec::Vec::new());
        let cal = calibrate(
            Calibrated::Tsc(Reference::Pit),
            PIT_HZ,
            || {
                taken += 1;
                Ok(w(taken))
            },
            |ruler| rulers.push(ruler),
        )
        .map_err(|_| "calibrating failed")?;
        crate::ktest::diag(format_args!("{cal}, median within {} ppm", cal.median_bound_ppm()));
        kassert_eq!((taken, rulers.len(), cal.retaken, cal.steady()), (ATTEMPTS as u64, ATTEMPTS - 1, ATTEMPTS, 0));
        kassert_eq!(cal.windows, [20, 19, 18, 17, 16].map(w));
        kassert_eq!(cal.last_retaken, Some(w(ATTEMPTS as u64)));
        // The ruler: the fewest ticks a window took, the first's, every time.
        kassert!(rulers.iter().all(|&r| r == w(1).tsc));
        kassert_eq!(Some(cal.hz), w(18).tsc_hz(PIT_HZ));
        kassert_eq!(cal.median_bound_ppm(), w(18).tsc_error_ppm());
        kassert!(cal.median_bound_ppm() > 108_000 && cal.median_bound_ppm() < 110_000);
        kassert_eq!(report(&cal).verdict, Verdict::Imprecise);
        Ok(())
    }
);

crate::ktest!(
    fn tsc_spares_are_ranked_by_the_tsc_bound() {
        // The TSC's fill-in ranks unsteady windows by how far off the TSC's
        // rate from each can be (an end gap counts for more than a start
        // gap), not by the gap share: of three A's (0 + 0.7%) and five B's
        // (0.4% + 0.4%), the five B's are kept, a median within 4,018 ppm
        // (the A's, kept by share, would give 7,051). And a measurement whose
        // median has no bound is refused (`measure`, `with_a_bound`).
        use crate::{kassert, kassert_eq};
        let a = Window { tsc: 10_000_000, reference: 1_000_000, polls: 800, start_gap: 0, end_gap: 70_000 };
        let b = Window { start_gap: 40_000, end_gap: 40_000, ..a };
        let mut it = [a, b, a, b, a, b, b, b].into_iter().chain(core::iter::repeat(b));
        let cal = calibrate(Calibrated::Tsc(Reference::Hpet), 100_000_000, || Ok(it.next().unwrap_or(b)), |_| {})
            .map_err(|_| "calibrating failed")?;
        kassert_eq!(cal.windows, [b; WINDOWS]);
        kassert_eq!(cal.median_bound_ppm(), 4_018);
        let none = Calibration { windows: [Window { end_gap: 5_000_000, ..a }; WINDOWS], ..cal };
        kassert_eq!(with_a_bound(none), Err(CalError::NoInformation));
        kassert_eq!(with_a_bound(cal), Ok(cal));
        // At exactly 100% (a timer's windows within 999,998 ppm, a TSC within
        // 1): no bound; just under, one.
        let gap = |ppm: u64| Window { tsc: 1_000_000, reference: 62_500, polls: 1, start_gap: 0, end_gap: ppm };
        let timer =
            |own, tsc_ppm| Calibration { windows: [gap(own); WINDOWS], of: Calibrated::Timer { tsc_ppm }, ..cal };
        kassert_eq!(timer(999_998, 1).median_bound_ppm(), 1_000_000);
        kassert_eq!(with_a_bound(timer(999_998, 1)), Err(CalError::NoInformation));
        kassert!(with_a_bound(timer(999_998, 0)).is_ok());
        // How a bound reads: tenths rounded up, or none from 100% on.
        kassert_eq!(
            alloc::format!("{} {} {} {}", Percent(7_000), Percent(7_001), Percent(999_000), Percent(1_000_000)),
            "0.7% 0.8% 99.9% no bound"
        );
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
        let cal = median_tsc_hz(Reference::Pit, PIT_HZ, || {
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
        // Every poll's read held up 200 µs, as a slowed host's HPET reads
        // were (#139: 250 µs each, 40 polls in 10 ms): the end gap is 2% of a
        // 10 ms window, unsteady however late it is taken again. Taken again
        // longer (the gap is one poll's worth, a slow read, not a burst),
        // until the same gaps are under 0.5% of it, all five kept windows are
        // steady (none filling in) and longer than 10 ms, and the rate is
        // within 1% (or its bound and the boot's, if more).
        use crate::{kassert, kassert_eq};
        let hpet = hpet().ok_or("no HPET")?;
        let hz = tsc().ok_or("TSC not calibrated")?.hz();
        let base = hpet.clock().ticks(WINDOW_NS);
        let cal = off_or_diag("calibrating against the HPET failed", || {
            calibrate_with_hpet(&hpet, sleep_at(Pause::EndBeforeRead, 200_000))
        })?;
        crate::ktest::diag(format_args!("{cal}, median within {} ppm", cal.median_bound_ppm()));
        kassert_eq!(cal.steady(), WINDOWS);
        // Taken again at least once (the first, 10 ms window was too slow to
        // read), and a stretched window keeps its length: the next one
        // starts from it, not from 10 ms. So any window taken again after
        // the first was a stretched one, set aside for a stall of its own
        // (a busy host's: as many as it likes, #184), never a 10 ms one
        // again, as alternating lengths would leave it; the last set aside
        // shows which.
        kassert!(cal.retaken >= 1);
        if cal.retaken > 1 {
            let last = cal.last_retaken.ok_or("no window set aside")?;
            kassert!(last.reference > 2 * base);
        }
        // Longer than 10 ms, and at most 16 times that: a window ends at the
        // first poll past its target, so up to a poll (450 µs on loaded KVM)
        // past it, under one more window.
        kassert!(cal.windows.iter().all(|w| w.reference > base && w.reference < base * (MOST_HPET_STRETCH + 1)));
        let boot_ppm = boot_calibration().ok_or("no boot calibration")?.median_bound_ppm();
        kassert!(lumen_time::within_bounds(cal.hz, hz, cal.median_bound_ppm(), boot_ppm, CROSS_CHECK_FLOOR_PPM));
        Ok(())
    }
);

crate::ktest!(
    fn an_unplaceable_window_filling_in_doesnt_fail_the_calibration() {
        // Four steady PIT windows in 20, the rest unplaceable (no reference
        // ticks: no rate, no bound): one fills in, its rate counted as 0, and
        // the median (between steady windows') stands, within a steady
        // window's bound, where it used to fail the calibration.
        use crate::kassert_eq;
        let steady = Window { tsc: 24_500_000, reference: 11_932, polls: 900, start_gap: 1_000, end_gap: 1_000 };
        let unplaced = Window { reference: 0, end_gap: 24_500_000, ..steady };
        let mut taken = 0;
        let cal = calibrate(
            Calibrated::Tsc(Reference::Pit),
            PIT_HZ,
            || {
                taken += 1;
                Ok(if taken <= 4 { steady } else { unplaced })
            },
            |_| {},
        )
        .map_err(|e| {
            crate::ktest::diag(format_args!("{e}"));
            "calibrating failed"
        })?;
        kassert_eq!((cal.steady(), cal.windows[4]), (4, unplaced));
        kassert_eq!(Some(cal.hz), steady.tsc_hz(PIT_HZ));
        kassert_eq!(cal.median_bound_ppm(), steady.tsc_error_ppm());
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
        // there too), and is against the calibrated rate, itself off by up
        // to the boot timer calibration's bound B (the TSC's included, at
        // least 1%): a window off by w of the truth reads (1 + w) / (1 ± B)
        // - 1 against it, composed, not added (#145 round 2: added, a 1.2%
        // calibration left 54 ppm). All four windows are taken with the tick
        // stopped (`with_tick_stopped`).
        use crate::kassert;
        let tsc = tsc().ok_or("TSC not calibrated")?;
        let timer_hz = lapic_timer().ok_or("timer not calibrated")?.hz();
        let calibration_ppm = boot_timer_calibration().ok_or("no timer calibration")?.median_bound_ppm().max(10_000);
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
            let (high, low, paused) = (ppm(w.start_gap, &w), ppm(w.end_gap, &w), ppm(ms, &w));
            // The window's own error, of the true rate: the timer counted the
            // pause, or it didn't.
            let (lo_w, hi_w) = if matches!(at, Pause::StartAfterRead | Pause::EndBeforeRead) {
                (paused - low, high)
            } else {
                (-low, high - paused)
            };
            // Against a calibration off by up to B either way, rounded out.
            const M: i128 = 1_000_000;
            let b = i128::from(calibration_ppm);
            let lowest = ((M + i128::from(lo_w)) * M / (M + b) - M - 1) as i64;
            let highest = ((M + i128::from(hi_w)) * M / (M - b) - M + 1) as i64;
            crate::ktest::diag(format_args!("  allowed {lowest}..={highest} ppm"));
            kassert!(off_ppm >= lowest && off_ppm <= highest);
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
        let cal = calibrate_timer_with(tsc, 0, || {
            taken += 1;
            Ok(if taken <= 3 { stalled } else { steady })
        })
        .map_err(|_| "calibrating failed")?;
        let waited = arch::rdtsc().wrapping_sub(start);
        crate::ktest::diag(format_args!("{cal}; {} ns in all", tsc.ns(waited)));
        kassert_eq!((cal.steady(), cal.retaken, taken), (WINDOWS, 3, 8));
        kassert_eq!(Some(cal.hz), steady.reference_hz(tsc.hz()));
        kassert!(waited >= 60_000_000 * tsc.hz() / 1_000_000_000);
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
        let cal = calibrate_timer_with(tsc, 0, || {
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
        kassert_eq!(report(&cal).verdict, Verdict::Retook);
        Ok(())
    }
);

crate::ktest!(
    fn a_calibration_report_fits_each_outcome() {
        // What init does, and at what level, judged by the median's bound
        // against each clock's tolerance (the timer 2%, the TSC 1%), not by
        // how many windows are steady. Made-up windows of 10,000,000 TSC
        // ticks; 50,000 ticks of gaps or less is steady.
        // - Every window steady at once: nothing.
        // - Retaken, bound within the tolerance: INFO with the bound, even
        //   with none steady (#139's 0.7% timer, which used to warn).
        // - Past the tolerance: WARN with the bound, rounded up to a tenth
        //   (20,001 ppm shows as 2.1%; the timer's third smallest share).
        // - The timer's own windows within 2%, but against a TSC off by up
        //   to 1%: past it (the TSC's error is the timer's too).
        // - No bound left: a panic, saying why for that clock.
        // The same windows judged as the TSC's (its error, end gap over the
        // rest plus a reference tick, against 1%) can warn where the timer's
        // don't.
        use crate::{kassert, kassert_eq};
        let w = |end_gap: u64| Window { tsc: 10_000_000, reference: 625_000, polls: 1, start_gap: 0, end_gap };
        let cal = |gaps: [u64; WINDOWS], retaken, of| Calibration {
            hz: 62_500_000,
            windows: gaps.map(w),
            retaken,
            last_retaken: Some(w(60_000)),
            of,
        };
        let timer = |gaps, retaken| report(&cal(gaps, retaken, Calibrated::Timer { tsc_ppm: 0 }));
        let tsc = |gaps, retaken| report(&cal(gaps, retaken, Calibrated::Tsc(Reference::Hpet)));
        let text = |r: &Report| alloc::format!("{r}");
        let info = Action::Log(log::Level::Info);
        let warning = Action::Log(log::Level::Warn);

        let clean = timer([1_000, 2_000, 3_000, 4_000, 5_000], 0);
        kassert_eq!((clean.verdict, clean.action()), (Verdict::Clean, Action::Silent));
        let retook = timer([1_000, 2_000, 3_000, 200_000, 300_000], 17);
        kassert_eq!((retook.verdict, retook.action()), (Verdict::Retook, info));
        kassert!(
            text(&retook).starts_with(
                "local APIC timer calibration took 17 windows again, 3 of 5 kept steady, median within 0.1%"
            )
        );
        // None steady, but every gap 0.7%: still within the timer's 2%.
        let filled = timer([70_000, 70_000, 70_000, 70_000, 70_000], 20);
        kassert_eq!((filled.cal.steady(), filled.bound_ppm), (0, 7_000));
        kassert_eq!((filled.verdict, filled.action()), (Verdict::Retook, info));
        kassert!(text(&filled).contains("0 of 5 kept steady, median within 0.7%"));
        let imprecise = timer([1_000, 2_000, 200_010, 300_000, 2_000_000], 18);
        kassert_eq!((imprecise.cal.steady(), imprecise.bound_ppm), (2, 20_001));
        kassert_eq!((imprecise.verdict, imprecise.action()), (Verdict::Imprecise, warning));
        let said = text(&imprecise);
        crate::ktest::diag(format_args!("{said}"));
        kassert!(said.starts_with(
            "local APIC timer calibration: only 2 of 20 windows read promptly (5 wanted; a stall next to the timer's \
             register accesses), so its rate may be off by up to 2.1%, over the 2.0% allowed ("
        ));
        // 1.5% windows, fine alone, against a TSC within 1%: 2.5%, past it,
        // and the text says the TSC is what took it past.
        let against = report(&cal([150_000; WINDOWS], 20, Calibrated::Timer { tsc_ppm: 10_000 }));
        kassert_eq!((against.bound_ppm, against.verdict), (25_150, Verdict::Imprecise));
        kassert!(text(&against).starts_with(
            "local APIC timer calibration: 0 of 20 windows read promptly, but the TSC it is measured against may be \
             off by up to 1.0%, so its rate may be off by up to 2.6%, over the 2.0% allowed"
        ));
        // Every window steady, at once, against a TSC within 1.9%: 2.2%,
        // the TSC's doing, not the timer's register accesses.
        let steady_but = report(&cal([20_000; WINDOWS], 0, Calibrated::Timer { tsc_ppm: 19_000 }));
        kassert_eq!((steady_but.bound_ppm, steady_but.verdict), (21_038, Verdict::Imprecise));
        let said = text(&steady_but);
        kassert!(said.starts_with(
            "local APIC timer calibration: 5 of 5 windows read promptly, but the TSC it is measured against may be \
             off by up to 1.9%, so its rate may be off by up to 2.2%, over the 2.0% allowed"
        ));
        kassert!(!said.contains("register accesses"));
        // Its own windows past it too: both named, the TSC's share said.
        let both = report(&cal([1_000, 2_000, 250_000, 300_000, 400_000], 18, Calibrated::Timer { tsc_ppm: 5_000 }));
        kassert!(text(&both).contains(
            "a stall next to the timer's register accesses), so its rate may be off by up to 3.1% (0.5% of it the \
             TSC's calibration), over the 2.0% allowed"
        ));
        // At the edge: its own windows at exactly 2% are within it, so the
        // TSC is what takes it past, and named; a ppm more, its own reads.
        let at_edge = report(&cal([200_000; WINDOWS], 20, Calibrated::Timer { tsc_ppm: 1_000 }));
        kassert_eq!((at_edge.cal.windows_bound_ppm(), at_edge.verdict), (20_000, Verdict::Imprecise));
        kassert!(text(&at_edge).contains("but the TSC it is measured against may be off by up to 0.1%, so"));
        let past_edge = report(&cal([200_010; WINDOWS], 20, Calibrated::Timer { tsc_ppm: 1_000 }));
        kassert_eq!(past_edge.cal.windows_bound_ppm(), 20_001);
        kassert!(text(&past_edge).contains("a stall next to the timer's register accesses"));
        let unusable = timer([1_000, 2_000, 10_000_000, 10_000_000, 20_000_000], 18);
        kassert_eq!((unusable.verdict, unusable.action()), (Verdict::Unusable, Action::Panic));
        kassert!(text(&unusable).starts_with(
            "can't calibrate the local APIC timer against the TSC: only 2 of 20 windows read promptly (5 wanted), \
             and the middle ones' stalls were as long as themselves, so the rate says nothing"
        ));

        // As the TSC's: 0.7% gaps are within its 1% too, 1.2% are not.
        let tsc_filled = tsc([70_000, 70_000, 70_000, 70_000, 70_000], 20);
        kassert_eq!(tsc_filled.bound_ppm, w(70_000).tsc_error_ppm());
        kassert_eq!(tsc_filled.verdict, Verdict::Retook);
        kassert!(text(&tsc_filled).starts_with("tsc calibration took 20 windows again, 0 of 5 kept steady"));
        let tsc_off = tsc([120_000, 120_000, 120_000, 120_000, 120_000], 20);
        kassert!(tsc_off.bound_ppm > 12_000);
        kassert_eq!((tsc_off.verdict, tsc_off.action()), (Verdict::Imprecise, warning));
        kassert!(text(&tsc_off).starts_with(
            "tsc calibration: only 0 of 20 windows read promptly (5 wanted; a stall next to the HPET's reads), so \
             its rate may be off by up to 1.3%, over the 1.0% allowed ("
        ));
        // A TSC window's bound has no end left at half of it: the end gap as
        // long as the rest.
        let tsc_unusable = tsc([5_000_000, 5_000_000, 5_000_000, 5_000_000, 5_000_000], 20);
        kassert_eq!((tsc_unusable.verdict, tsc_unusable.action()), (Verdict::Unusable, Action::Panic));
        kassert!(text(&tsc_unusable).starts_with(
            "can't calibrate the tsc against the HPET: only 0 of 20 windows read promptly (5 wanted), and the middle \
             ones' end stalls were as long as the rest of their windows, so the rate says nothing"
        ));
        Ok(())
    }
);

crate::ktest!(
    fn the_steadier_tsc_calibration_is_kept() {
        // An HPET calibration past the TSC's 1% (1.3%) weighed against the
        // PIT's: a PIT one within 0.7% is kept instead; a worse one (2%)
        // leaves the HPET; a PIT that fails leaves the HPET too.
        use crate::kassert_eq;
        let cal = |end_gap: u64, of| Calibration {
            hz: 2_500_000_000,
            windows: [Window { tsc: 10_000_000, reference: 1_000_000, polls: 800, start_gap: 0, end_gap }; WINDOWS],
            retaken: 20,
            last_retaken: None,
            of,
        };
        let hpet = report(&cal(130_000, Calibrated::Tsc(Reference::Hpet)));
        kassert_eq!(hpet.verdict, Verdict::Imprecise);
        let better = the_steadier(hpet, || Ok(cal(70_000, Calibrated::Tsc(Reference::Pit))));
        kassert_eq!((better.cal.of, better.verdict), (Calibrated::Tsc(Reference::Pit), Verdict::Retook));
        let worse = the_steadier(hpet, || Ok(cal(200_000, Calibrated::Tsc(Reference::Pit))));
        kassert_eq!(worse.cal.of, Calibrated::Tsc(Reference::Hpet));
        let failed = the_steadier(hpet, || Err(CalError::NoCount));
        kassert_eq!(failed.cal.of, Calibrated::Tsc(Reference::Hpet));
        // Both past the tolerance, the PIT's smaller: the PIT, though it
        // warns too.
        let far = report(&cal(300_000, Calibrated::Tsc(Reference::Hpet)));
        let nearer = the_steadier(far, || Ok(cal(150_000, Calibrated::Tsc(Reference::Pit))));
        kassert_eq!((nearer.cal.of, nearer.verdict), (Calibrated::Tsc(Reference::Pit), Verdict::Imprecise));
        // A tie keeps the HPET; a PIT with no bound too.
        let tie = the_steadier(hpet, || Ok(cal(130_000, Calibrated::Tsc(Reference::Pit))));
        kassert_eq!(tie.cal.of, Calibrated::Tsc(Reference::Hpet));
        let none = the_steadier(hpet, || Ok(cal(6_000_000, Calibrated::Tsc(Reference::Pit))));
        kassert_eq!(none.cal.of, Calibrated::Tsc(Reference::Hpet));
        Ok(())
    }
);

crate::ktest!(
    fn the_stalled_boots_seen_on_kvm_report_quietly() {
        // Windows from the two stalled KVM boots #139 recorded, replayed
        // through the judgement (both warned before, failing scenarios that
        // expect no warning), each against a TSC within 0.12% (a steady
        // calibration's):
        // - CI run 43966495801's timer windows, 2 of 5 steady after 20 (18
        //   taken again): the bound is the third smallest gap share, 0.7%,
        //   and the TSC's 0.12% on top, inside the timer's 2%: INFO.
        // - An idle measurement run's (44062785900) timer windows, every end
        //   about 25 µs slow (a slowed host, not a burst), 1 steady after 20
        //   (19 taken again): 0.5% and the TSC's: INFO.
        // - The HPET windows of the first boot, made up from what its warning
        //   showed: 4 of 20 read promptly (made-up prompt windows), the others
        //   like the one shown (40 polls in 10 ms, 3.9% of gaps), fed to
        //   median_tsc_hz directly (not calibrate_with_hpet, so the stretch
        //   isn't replayed). With the smallest bounds filling in, the median
        //   lies between steady windows': INFO (it used to give way to the
        //   PIT).
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
            of: Calibrated::Timer { tsc_ppm: 1_200 },
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
        let ci = report(&ci);
        kassert_eq!(ci.cal.steady(), 2);
        kassert_eq!((ci.verdict, ci.bound_ppm), (Verdict::Retook, lumen_time::compose_ppm(6_870, 1_200)));
        kassert_eq!(ci.action(), Action::Log(log::Level::Info));
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
        let idle = report(&idle);
        kassert_eq!((idle.verdict, idle.bound_ppm), (Verdict::Retook, lumen_time::compose_ppm(5_041, 1_200)));
        let slow = parse("24547212/1000016/40/436492/529641");
        let prompt = parse("24547212/1000016/800/30000/30000");
        let mut taken = 0;
        let hpet = calibrate(
            Calibrated::Tsc(Reference::Hpet),
            100_000_000,
            || {
                taken += 1;
                Ok(if matches!(taken, 3 | 8 | 13 | 19) { prompt } else { slow })
            },
            |_| {},
        )
        .map_err(|_| "calibrating failed")?;
        let hpet = report(&hpet);
        kassert_eq!(
            (hpet.cal.steady(), hpet.verdict, hpet.action()),
            (4, Verdict::Retook, Action::Log(log::Level::Info))
        );
        kassert_eq!(hpet.bound_ppm, prompt.tsc_error_ppm());
        Ok(())
    }
);

crate::ktest!(
    fn the_boot_timer_calibration_is_within_what_the_tick_needs() {
        // Expected, not guaranteed: the boot's median is within the timer's
        // tolerance (lumen_time::TIMER_TOLERANCE_PPM), the TSC's error
        // included, as its steady windows' or enough least-unsteady ones
        // filling in allow, and the timer runs at it. A boot whose bound was
        // past it warned, and fails this on purpose: its rate may be off by
        // more than the tick allows.
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
        let boot_tsc_ppm = boot_calibration().ok_or("no boot calibration")?.median_bound_ppm();
        kassert_eq!(cal.of, Calibrated::Timer { tsc_ppm: boot_tsc_ppm });
        kassert!(cal.median_bound_ppm() <= lumen_time::TIMER_TOLERANCE_PPM);
        kassert_eq!(lumen_time::median(&mut rates), Some(cal.hz));
        kassert_eq!(lapic_timer().map(|c| c.hz()), Some(cal.hz));
        Ok(())
    }
);

crate::ktest!(
    fn the_boot_tsc_calibration_is_within_what_now_ns_needs() {
        // Expected, not guaranteed: the boot's TSC median is within the
        // TSC's tolerance (lumen_time::TSC_TOLERANCE_PPM, 1%), its steady
        // windows first, the frequency their median, and now_ns runs at it.
        // A boot past it warned, and fails this on purpose.
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
        kassert_eq!(cal.of, Calibrated::Tsc(reference));
        kassert!(cal.median_bound_ppm() <= lumen_time::TSC_TOLERANCE_PPM);
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
