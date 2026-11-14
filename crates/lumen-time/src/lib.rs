//! Timekeeping arithmetic for the Lumen kernel: working out a counter's
//! frequency from a reference clock, converting ticks to and from
//! nanoseconds, the counts the PIT and the local APIC timer are programmed
//! with, how times read in logs (`Stamp`, `Millis`), the rules a
//! calibration window must meet to be trusted (`Window`, `steady_windows`),
//! and how a PIT channel's two readings become a window (`pit_window`). All of it is
//! integer math (the kernel has no floating point) in `u128` where products
//! can overflow, so it holds for centuries of uptime (and then saturates).

#![no_std]

use core::fmt;

pub mod rtc;

pub const NS_PER_SEC: u64 = 1_000_000_000;
/// The PIT's input clock.
pub const PIT_HZ: u64 = 1_193_182;
const FS_PER_SEC: u64 = 1_000_000_000_000_000;
/// The longest HPET tick the spec allows: 100 ns.
const HPET_MAX_PERIOD_FS: u32 = 100_000_000;

/// The frequency of a counter that counted `ticks` while a reference clock
/// of `ref_hz` counted `ref_ticks`. `None` if the reference didn't move, or
/// the result is 0 or doesn't fit.
pub fn frequency(ticks: u64, ref_ticks: u64, ref_hz: u64) -> Option<u64> {
    if ref_ticks == 0 {
        return None;
    }
    let hz = u128::from(ticks) * u128::from(ref_hz) / u128::from(ref_ticks);
    u64::try_from(hz).ok().filter(|&hz| hz > 0)
}

/// The HPET's frequency from the period in its capabilities register
/// (femtoseconds per tick). `None` for 0 or a period over the spec's 100 ns.
pub fn hpet_hz(period_fs: u32) -> Option<u64> {
    (1..=HPET_MAX_PERIOD_FS).contains(&period_fs).then(|| FS_PER_SEC / u64::from(period_fs))
}

/// The PIT count closest to `ns`; `None` if it is 0 or over 16 bits (about
/// 54.9 ms).
pub fn pit_count(ns: u64) -> Option<u16> {
    let count = (u128::from(ns) * u128::from(PIT_HZ) + u128::from(NS_PER_SEC / 2)) / u128::from(NS_PER_SEC);
    u16::try_from(count).ok().filter(|&c| c > 0)
}

/// The initial count of a periodic timer whose input (after any divider)
/// runs at `timer_hz`, to fire `rate` times a second. `None` if that is 0
/// or over 32 bits.
pub fn periodic_count(timer_hz: u64, rate: u64) -> Option<u32> {
    if rate == 0 {
        return None;
    }
    u32::try_from((timer_hz + rate / 2) / rate).ok().filter(|&c| c > 0)
}

/// Ticks from `earlier` to `later`, two readings of a counter `mask` wide
/// (`u64::MAX`, or `0xffff_ffff` for a 32-bit counter): correct across one
/// wrap.
pub fn counter_delta(earlier: u64, later: u64, mask: u64) -> u64 {
    later.wrapping_sub(earlier) & mask
}

/// Ticks a down-counter that reloads to `reload` at 0 counted from reading
/// `earlier` to reading `later`: correct across one reload (two readings
/// less than a period apart).
pub fn down_counter_delta(earlier: u32, later: u32, reload: u32) -> u32 {
    if later <= earlier { earlier - later } else { earlier + (reload - later) }
}

/// Is `hz` a believable TSC frequency? A miscounted reference gives wild
/// values. Real TSCs run between tens of MHz and a few GHz; the bounds (10 MHz
/// to 100 GHz) are generous, so only a broken reference fails.
pub fn plausible_tsc_hz(hz: u64) -> bool {
    (10_000_000..=100_000_000_000).contains(&hz)
}

/// One window of the TSC measured against a reference clock: TSC ticks, the
/// reference's ticks over the same stretch, how often the reference was
/// polled, and how uncertain its start and end are, in TSC ticks. Each end is
/// a reading of the reference taken between two TSC reads; the gap between
/// them is how far off that end can be (a vCPU paused mid-reading widens it).
/// An end that can't be placed at all (a PIT counter that may have wrapped)
/// gets a gap of the whole window, so the window isn't steady.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Window {
    pub tsc: u64,
    pub reference: u64,
    pub polls: u64,
    pub start_gap: u64,
    pub end_gap: u64,
}

/// A window is steady if the gaps at its start and end add up to at most
/// 1/`STEADY` of it: 0.5%.
pub const STEADY: u64 = 200;

impl Window {
    /// Were both its ends read promptly enough to trust it (see [`STEADY`])?
    /// An empty window isn't.
    pub fn is_steady(&self) -> bool {
        self.tsc > 0 && self.start_gap.saturating_add(self.end_gap).saturating_mul(STEADY) <= self.tsc
    }

    /// The TSC's frequency it gives against a reference counting at
    /// `reference_hz` (see [`frequency`]).
    pub fn tsc_hz(&self, reference_hz: u64) -> Option<u64> {
        frequency(self.tsc, self.reference, reference_hz)
    }
}

impl fmt::Display for Window {
    /// `tsc/reference/polls/start_gap/end_gap`, in decimal.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}/{}/{}", self.tsc, self.reference, self.polls, self.start_gap, self.end_gap)
    }
}

/// Where a window measurement may be paused, for tests: a calibration passes
/// a no-op, a test pauses there as a host descheduling the vCPU would. Each
/// end is a reading of the reference between two TSC reads, paused before or
/// after the reference is read (either widens that end's gap); `Started` and
/// `Ended` are only the PIT's (the HPET's first and last readings are its
/// start and end).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pause {
    /// After the PIT started counting, before its start reading. Harmless:
    /// the start is read, not assumed.
    Started,
    /// In the start reading, after the TSC read before it, before the
    /// reference is read.
    StartBeforeRead,
    /// In the start reading, after the reference is read (the PIT's latched),
    /// before the TSC read after it.
    StartAfterRead,
    /// After the PIT ran out, before its end reading. Harmless up to a limit:
    /// the reading says how far past the end it was, while
    /// `pit_end_unambiguous` can show it didn't wrap.
    Ended,
    /// In the end reading, after the TSC read before it, before the
    /// reference is read (the HPET reaches this on every poll).
    EndBeforeRead,
    /// In the end reading, after the reference is read (the PIT's latched),
    /// before the TSC read after it.
    EndAfterRead,
    /// In either PIT reading, after its second TSC read, while the count the
    /// latch froze is read. Harmless: outside the bracket, however slow the
    /// reads are (under KVM each is a VM exit).
    CountRead,
}

/// Why [`steady_windows`] gave up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SteadyError<E> {
    /// Taking a window failed (returned at once, not taken again).
    Take(E),
    /// Only `steady` of `attempts` windows were steady; `last` is the last
    /// one that wasn't.
    Unsteady { steady: usize, attempts: usize, last: Window },
}

/// The windows [`steady_windows`] set aside: how many, and the last.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetAside {
    pub count: usize,
    pub last: Option<Window>,
}

/// Fill `out` with steady windows from `take`, taking at most `attempts`
/// (at least `out.len()`) windows in all; an unsteady one is set aside and
/// another taken.
pub fn steady_windows<E>(
    out: &mut [Window],
    attempts: usize,
    mut take: impl FnMut() -> Result<Window, E>,
) -> Result<SetAside, SteadyError<E>> {
    debug_assert!(attempts >= out.len(), "{attempts} attempts can't fill {} windows", out.len());
    let (mut steady, mut aside) = (0, SetAside::default());
    while steady < out.len() {
        if steady + aside.count == attempts {
            let last = aside.last.unwrap_or_default();
            return Err(SteadyError::Unsteady { steady, attempts, last });
        }
        let w = take().map_err(SteadyError::Take)?;
        if w.is_steady() {
            out[steady] = w;
            steady += 1;
        } else {
            aside = SetAside { count: aside.count + 1, last: Some(w) };
        }
    }
    Ok(aside)
}

/// PIT ticks between two readings of a channel counting down, `start` then
/// `end`. In mode 0 it counts on past 0 (wrapping to 0xffff), so this holds
/// as long as fewer than 65536 ticks passed ([`pit_end_unambiguous`]).
fn pit_elapsed(start: u16, end: u16) -> u64 {
    u64::from(start.wrapping_sub(end))
}

/// Can a PIT window programmed with `programmed` be placed: its start read
/// while it still counted, and its end shown to be fewer than 65536 ticks
/// after the start, so [`pit_elapsed`] is right? (False also means it can't
/// be told.) `running_tsc` is the TSC from the start reading to just before
/// the last poll that still saw the count running, so the start was read
/// before it ran out and fewer than its count (at most `programmed`, which
/// [`pit_window`] then checks) ticks passed in it; `late_tsc` is from then to
/// the end reading. If `late_tsc` is at most 4 × `running_tsc`, fewer than
/// 5 × `programmed` ticks passed in all, which is under 65536 for up to 13107
/// (10 ms is 11932). No poll that saw it running (`running_tsc` 0: the start
/// may have been read after it ran out) places nothing.
fn pit_end_unambiguous(programmed: u16, running_tsc: u64, late_tsc: u64) -> bool {
    running_tsc > 0 && u64::from(programmed) * 5 <= 65536 && late_tsc <= running_tsc.saturating_mul(4)
}

/// A reading of a PIT channel's counter, latched between two TSC reads (the
/// latched count may be read after the second).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PitReading {
    pub count: u16,
    pub tsc_before: u64,
    pub tsc_after: u64,
}

impl PitReading {
    /// How far off the reading can be, in TSC ticks.
    pub fn gap(&self) -> u64 {
        self.tsc_after - self.tsc_before
    }
}

/// How a PIT window went: the channel's counter read just after it started
/// and again once it had run out (in mode 0 it counts on past 0, so the
/// second reading says how far past), the TSC just before the last poll that
/// still saw it running (the start reading's if none did), and how many
/// times its output was polled. The TSC reads come in order:
/// `start.tsc_before <= start.tsc_after <= last_running_tsc <=
/// end.tsc_before <= end.tsc_after`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PitWait {
    pub start: PitReading,
    pub end: PitReading,
    pub last_running_tsc: u64,
    pub polls: u64,
}

/// A PIT window whose readings can't fit how the channel was programmed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PitBadReadback {
    pub programmed: u16,
    pub start: u16,
    pub end: u16,
}

impl fmt::Display for PitBadReadback {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { programmed, start, end } = self;
        write!(
            f,
            "programmed with {programmed}, its counter read back {start} after starting and {end} once it \
             had run out (it should start at most {programmed} and run at least {start} ticks)"
        )
    }
}

/// The window a PIT channel programmed with `count` gave: from its start
/// reading to its end reading, so `reference` is the ticks between them (more
/// than the start reading's count if the end was read late). A window that
/// can't be placed (never seen running after its start reading, which may
/// then have come after it ran out, or its end read more than 4 × as long
/// after it was last seen running as it was seen running:
/// [`pit_end_unambiguous`]) gets the whole window as its end gap, so it isn't
/// steady and another is taken. One that can be placed must fit how the
/// channel was programmed (a start at most `count`, and at least that many
/// ticks run), or its read-back is wrong.
pub fn pit_window(count: u16, w: &PitWait) -> Result<Window, PitBadReadback> {
    let (start, end) = (w.start, w.end);
    let tsc = end.tsc_after - start.tsc_after;
    let reference = pit_elapsed(start.count, end.count);
    let running = w.last_running_tsc - start.tsc_after;
    let late = end.tsc_after - w.last_running_tsc;
    let end_gap = if pit_end_unambiguous(count, running, late) {
        if !pit_counts_make_sense(count, start.count, reference) {
            return Err(PitBadReadback { programmed: count, start: start.count, end: end.count });
        }
        end.gap()
    } else {
        tsc
    };
    Ok(Window { tsc, reference, polls: w.polls, start_gap: start.gap(), end_gap })
}

/// Do a PIT window's readings fit a channel programmed with `programmed` that
/// counted `elapsed` ticks from a start reading of `start` until after it ran
/// out? It counts down from what was programmed (`start` at most that) and
/// had reached 0 (`elapsed` at least `start`). Only meaningful for an end
/// that [`pit_end_unambiguous`] places; a read-back garbled past either bound
/// fails it.
fn pit_counts_make_sense(programmed: u16, start: u16, elapsed: u64) -> bool {
    start <= programmed && elapsed >= u64::from(start)
}

/// The median of `samples` (sorted in place).
pub fn median(samples: &mut [u64]) -> Option<u64> {
    samples.sort_unstable();
    match samples.len() {
        0 => None,
        n if n % 2 == 1 => Some(samples[n / 2]),
        n => Some(samples[n / 2 - 1].midpoint(samples[n / 2])),
    }
}

/// How busy a CPU was, in whole percent (rounded), from timer ticks
/// sampled over a span: `ticks` in all, `idle` of them landing on its idle
/// thread. 0 for no ticks; idle ticks past `ticks` count as all idle.
pub fn busy_percent(ticks: u64, idle: u64) -> u64 {
    percent(ticks - idle.min(ticks), ticks)
}

/// `part` of `whole` in whole percent, rounded (0 for an empty whole; a
/// part past the whole counts as all of it).
pub fn percent(part: u64, whole: u64) -> u64 {
    if whole == 0 {
        return 0;
    }
    let part = u128::from(part.min(whole));
    ((part * 100 + u128::from(whole) / 2) / u128::from(whole)) as u64
}

/// `count` events in `ns` nanoseconds as a rate per second (rounded down;
/// 0 for an empty span, saturating).
pub fn per_second(count: u64, ns: u64) -> u64 {
    if ns == 0 {
        return 0;
    }
    u64::try_from(u128::from(count) * u128::from(NS_PER_SEC) / u128::from(ns)).unwrap_or(u64::MAX)
}

/// A counter of fixed, known frequency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clock {
    hz: u64,
}

impl Clock {
    /// `None` for 0 Hz.
    pub const fn new(hz: u64) -> Option<Clock> {
        if hz == 0 { None } else { Some(Clock { hz }) }
    }

    pub const fn hz(&self) -> u64 {
        self.hz
    }

    /// The nanoseconds `ticks` take, rounded down (saturating, which takes
    /// centuries at any real frequency).
    pub fn ns(&self, ticks: u64) -> u64 {
        let ns = u128::from(ticks) * u128::from(NS_PER_SEC) / u128::from(self.hz);
        u64::try_from(ns).unwrap_or(u64::MAX)
    }

    /// The ticks in `ns`, rounded up: waiting that many ticks waits at least
    /// `ns`.
    pub fn ticks(&self, ns: u64) -> u64 {
        let ticks = (u128::from(ns) * u128::from(self.hz)).div_ceil(u128::from(NS_PER_SEC));
        u64::try_from(ticks).unwrap_or(u64::MAX)
    }
}

/// A frequency for humans: rounded to hundredths of the largest unit it
/// reaches, trailing zeros dropped (2_900_000_000 is "2.9 GHz", 1_193_182
/// "1.19 MHz").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hz(pub u64);

impl fmt::Display for Hz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(u64, &str); 3] = [(1_000_000_000, "GHz"), (1_000_000, "MHz"), (1_000, "kHz")];
        let Some((scale, unit)) = UNITS.into_iter().find(|&(s, _)| self.0 >= s) else {
            return write!(f, "{} Hz", self.0);
        };
        // Hundredths of the unit, rounded; then drop trailing zeros.
        let hundredths = (u128::from(self.0) * 100 + u128::from(scale / 2)) / u128::from(scale);
        let (whole, frac) = (hundredths / 100, hundredths % 100);
        match frac {
            0 => write!(f, "{whole} {unit}"),
            _ if frac % 10 == 0 => write!(f, "{whole}.{} {unit}", frac / 10),
            _ => write!(f, "{whole}.{frac:02} {unit}"),
        }
    }
}

/// A log line's timestamp: `[    0.012345]`, nanoseconds since the
/// caller's origin shown as seconds to the microsecond (100000 s and more
/// widen the field), or a placeholder of the same width while the time isn't
/// known (`None`). A zero would read as a measurement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp(pub Option<u64>);

impl fmt::Display for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(ns) => write!(f, "[{:>5}.{:06}]", ns / NS_PER_SEC, ns % NS_PER_SEC / 1000),
            None => f.write_str("[    ?.??????]"),
        }
    }
}

/// A duration given in **nanoseconds**, shown in milliseconds to the
/// microsecond: `0.012 ms`, `3400.000 ms`; with `{:+}`, as an offset:
/// `+0.012 ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Millis(pub u64);

impl fmt::Display for Millis {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if f.sign_plus() { "+" } else { "" };
        write!(f, "{sign}{}.{:03} ms", self.0 / 1_000_000, self.0 % 1_000_000 / 1000)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::string::ToString;

    use super::*;

    fn window(tsc: u64, start_gap: u64, end_gap: u64) -> Window {
        Window { tsc, reference: 1, polls: 1, start_gap, end_gap }
    }

    #[test]
    fn a_window_is_steady_within_half_a_percent() {
        assert!(window(10_000_000, 0, 0).is_steady());
        assert!(window(10_000_000, 20_000, 30_000).is_steady()); // exactly 0.5%
        assert!(!window(10_000_000, 20_000, 30_001).is_steady());
        assert!(!window(10_000_000, 50_001, 0).is_steady(), "a late start counts too");
        assert!(!window(10_000_000, 0, u64::MAX).is_steady(), "saturates, doesn't wrap");
        // Values where wrapping would give a small product or sum.
        assert!(!window(10_000_000, 0, u64::MAX / 200 + 1).is_steady(), "the product saturates");
        assert!(!window(10_000_000, u64::MAX, 1).is_steady(), "the sum saturates");
        assert!(!window(0, 0, 0).is_steady(), "an empty window isn't steady");
    }

    #[test]
    fn a_window_shows_as_its_five_fields_and_gives_its_frequency() {
        let w = Window { tsc: 29_000_000, reference: 1_000_000, polls: 3, start_gap: 4, end_gap: 5 };
        assert_eq!(w.to_string(), "29000000/1000000/3/4/5");
        assert_eq!(w.tsc_hz(100_000_000), Some(2_900_000_000));
    }

    #[test]
    fn steady_windows_takes_unsteady_ones_again() {
        // Two unsteady windows among steady ones: they are taken again, and
        // only steady ones are kept, in order.
        let mut taken = [window(100, 0, 0), window(101, 0, 50), window(102, 0, 0), window(103, 9, 0)]
            .into_iter()
            .chain((104..).map(|t| window(t, 0, 0)));
        let mut out = [Window::default(); 3];
        let aside = steady_windows::<()>(&mut out, 10, || Ok(taken.next().unwrap()));
        assert_eq!(aside, Ok(SetAside { count: 2, last: Some(window(103, 9, 0)) }));
        assert_eq!(out.map(|w| w.tsc), [100, 102, 104]);
        // All steady: none set aside.
        assert_eq!(steady_windows::<()>(&mut out, 3, || Ok(window(7, 0, 0))), Ok(SetAside::default()));
        // Done on exactly the last attempt.
        let mut n = 0;
        let late = || {
            n += 1;
            Ok::<_, ()>(window(100, 0, if n <= 2 { 99 } else { 0 }))
        };
        assert_eq!(steady_windows(&mut out, 5, late).map(|a| a.count), Ok(2));
    }

    #[test]
    fn steady_windows_gives_up_after_its_attempts() {
        let mut out = [Window::default(); 3];
        let mut n = 0;
        let flaky = || {
            n += 1;
            Ok::<_, ()>(window(100 + n, 0, if n % 2 == 0 { 0 } else { 99 }))
        };
        // Every other window is steady: 5 attempts give only 2, and the last
        // unsteady one (the fifth, not the first) is kept as evidence.
        assert_eq!(
            steady_windows(&mut out, 5, flaky),
            Err(SteadyError::Unsteady { steady: 2, attempts: 5, last: window(105, 0, 99) })
        );
        // A failure to take one is passed on at once.
        let mut calls = 0;
        let failing = || {
            calls += 1;
            Err::<Window, _>("no count")
        };
        assert_eq!(steady_windows(&mut out, 5, failing), Err(SteadyError::Take("no count")));
        assert_eq!(calls, 1);
    }

    #[test]
    fn pit_elapsed_counts_on_past_zero() {
        assert_eq!(pit_elapsed(11_932, 0), 11_932);
        assert_eq!(pit_elapsed(11_932, 100), 11_832, "read before it ran out");
        assert_eq!(pit_elapsed(11_932, 0xffff), 11_933, "one tick past 0");
        assert_eq!(pit_elapsed(11_932, 0xffff - 999), 12_932, "1000 ticks past 0");
        assert_eq!(pit_elapsed(5, 5), 0);
    }

    /// A PIT wait on a 1 GHz TSC: started at TSC 1000 (read in 1 µs) with the
    /// counter at 11932, running until `running_ns`, and the end read
    /// `late_ns` after that with the counter at `end`.
    fn pit_wait(running_ns: u64, late_ns: u64, end: u16) -> PitWait {
        let start = PitReading { count: 11_932, tsc_before: 0, tsc_after: 1000 };
        let last = 1000 + running_ns;
        PitWait {
            start,
            end: PitReading { count: end, tsc_before: last + late_ns - 500, tsc_after: last + late_ns },
            last_running_tsc: last,
            polls: 42,
        }
    }

    #[test]
    fn a_pit_window_runs_between_its_readings() {
        // Read right at the end: 11932 ticks, both gaps from the readings.
        let w = pit_window(11_932, &pit_wait(9_999_000, 1000, 0)).unwrap();
        assert_eq!(w, Window { tsc: 10_000_000, reference: 11_932, polls: 42, start_gap: 1000, end_gap: 500 });
        // Read 2 ms late: the counter ran on 2386 ticks past 0, and the TSC
        // with it, so the frequency is still right (1 GHz, to 0.01%).
        let w = pit_window(11_932, &pit_wait(9_999_000, 2_001_000, 0u16.wrapping_sub(2386))).unwrap();
        assert_eq!(w.reference, 11_932 + 2386);
        assert!(w.is_steady());
        assert!(w.tsc_hz(PIT_HZ).unwrap().abs_diff(1_000_000_000) < 100_000);
        // The start read 100 ticks into the count: the window is the 11832
        // after it.
        let mut wait = pit_wait(9_915_000, 1000, 0);
        wait.start.count = 11_832;
        assert_eq!(pit_window(11_932, &wait).unwrap().reference, 11_832);
    }

    #[test]
    fn a_pit_end_that_may_have_wrapped_is_not_steady_not_an_error() {
        // Read 50 ms late after running 10 ms: more than 4 × as late.
        let w = pit_window(11_932, &pit_wait(10_000_000, 50_000_000, 7)).unwrap();
        assert_eq!(w.end_gap, w.tsc);
        assert!(!w.is_steady());
        // Exactly 4 × as late (47728 ticks past 0) is placed; a tick later isn't.
        let past = 0u16.wrapping_sub(47_728);
        assert_eq!(pit_window(11_932, &pit_wait(10_000_000, 40_000_000, past)).unwrap().end_gap, 500);
        let w = pit_window(11_932, &pit_wait(10_000_000, 40_000_001, past)).unwrap();
        assert_eq!(w.end_gap, w.tsc);
        // Never seen running (the first poll saw it done): can't be placed.
        let w = pit_window(11_932, &pit_wait(0, 1000, 0)).unwrap();
        assert_eq!(w.end_gap, w.tsc);
    }

    #[test]
    fn a_pit_start_read_after_it_ran_out_is_not_steady_not_an_error() {
        // Paused 12 ms before the start reading: the counter ran out and on
        // (about 63150), and no poll saw it running after. Not a bad read-back.
        let mut w = pit_wait(0, 1000, 60_000);
        w.start.count = 63_150;
        let w = pit_window(11_932, &w).unwrap();
        assert_eq!(w.end_gap, w.tsc);
        assert!(!w.is_steady());
    }

    #[test]
    fn a_pit_read_back_that_cant_be_right_is_an_error() {
        // A start above what was programmed.
        let mut w = pit_wait(9_999_000, 1000, 0);
        w.start.count = 11_933;
        assert_eq!(pit_window(11_932, &w), Err(PitBadReadback { programmed: 11_932, start: 11_933, end: 0 }));
        // All ones (a missing or garbled read), though seen running after it.
        w.start.count = 0xffff;
        assert_eq!(pit_window(11_932, &w), Err(PitBadReadback { programmed: 11_932, start: 0xffff, end: 0 }));
        // An end that says it hadn't run out (it had: its output was high).
        let w = pit_wait(9_999_000, 1000, 5);
        assert_eq!(pit_window(11_932, &w), Err(PitBadReadback { programmed: 11_932, start: 11_932, end: 5 }));
    }

    #[test]
    fn pit_counts_must_fit_how_it_was_programmed() {
        assert!(pit_counts_make_sense(11_932, 11_932, 11_932));
        assert!(pit_counts_make_sense(11_932, 11_900, 14_000), "start read after it began, end read late");
        assert!(!pit_counts_make_sense(11_932, 11_933, 12_000), "more than was programmed");
        assert!(!pit_counts_make_sense(11_932, 11_900, 11_899), "hadn't reached 0");
    }

    #[test]
    fn a_pit_end_read_late_may_have_wrapped() {
        // Late by up to 4 × the running stretch: fewer than 5 × 11932 ticks.
        assert!(pit_end_unambiguous(11_932, 1_000_000, 4_000_000));
        assert!(!pit_end_unambiguous(11_932, 1_000_000, 4_000_001));
        assert!(!pit_end_unambiguous(11_932, 0, 0), "it never saw the count running");
        assert!(pit_end_unambiguous(13_107, 1, 4));
        assert!(!pit_end_unambiguous(13_108, 1, 4), "5 × start could reach 65536");
        assert!(pit_end_unambiguous(11_932, u64::MAX, u64::MAX), "saturates");
    }

    #[test]
    fn frequency_scales_by_the_reference() {
        // 29 M TSC ticks while a 100 MHz HPET counted 1 M (10 ms): 2.9 GHz.
        assert_eq!(frequency(29_000_000, 1_000_000, 100_000_000), Some(2_900_000_000));
        // While the PIT counted 11932 (10 ms).
        assert_eq!(frequency(29_000_000, 11_932, PIT_HZ), Some(2_899_956_252));
        assert_eq!(frequency(1, 0, 100), None, "the reference didn't move");
        assert_eq!(frequency(0, 10, 100), None, "the counter didn't move");
        assert_eq!(frequency(u64::MAX, 1, u64::MAX), None, "doesn't fit");
        assert_eq!(frequency(u64::MAX, u64::MAX, 7), Some(7), "no overflow in the product");
    }

    #[test]
    fn hpet_periods() {
        assert_eq!(hpet_hz(10_000_000), Some(100_000_000), "QEMU: 10 ns");
        assert_eq!(hpet_hz(69_841_279), Some(14_318_179), "the classic 14.318 MHz");
        assert_eq!(hpet_hz(HPET_MAX_PERIOD_FS), Some(10_000_000));
        assert_eq!(hpet_hz(HPET_MAX_PERIOD_FS + 1), None);
        assert_eq!(hpet_hz(0), None);
    }

    #[test]
    fn pit_counts() {
        assert_eq!(pit_count(10_000_000), Some(11_932));
        assert_eq!(pit_count(54_924_500), Some(65_535), "the longest");
        assert_eq!(pit_count(54_925_000), None, "rounds to 65536");
        assert_eq!(pit_count(55_000_000), None);
        assert_eq!(pit_count(100), None, "rounds to 0");
        assert_eq!(pit_count(500), Some(1), "rounds to nearest");
    }

    #[test]
    fn periodic_counts() {
        // A 1 GHz bus divided by 16, at 1 kHz.
        assert_eq!(periodic_count(62_500_000, 1000), Some(62_500));
        assert_eq!(periodic_count(1999, 1000), Some(2), "rounds to nearest");
        assert_eq!(periodic_count(499, 1000), None, "rounds to 0");
        assert_eq!(periodic_count(u64::from(u32::MAX) * 1000 + 1000, 1000), None, "over 32 bits");
        assert_eq!(periodic_count(1000, 0), None);
    }

    #[test]
    fn counter_deltas_wrap_at_the_counter_width() {
        assert_eq!(counter_delta(10, 25, u64::MAX), 15);
        assert_eq!(counter_delta(0xffff_fff0, 0x10, 0xffff_ffff), 0x20, "a 32-bit counter wrapped");
        assert_eq!(counter_delta(u64::MAX - 1, 3, u64::MAX), 5, "a 64-bit counter wrapped");
        assert_eq!(counter_delta(5, 5, 0xffff_ffff), 0);
    }

    #[test]
    fn down_counter_deltas_cross_one_reload() {
        assert_eq!(down_counter_delta(60_000, 50_000, 62_500), 10_000);
        assert_eq!(down_counter_delta(5_000, 60_000, 62_500), 7_500, "reloaded once");
        assert_eq!(down_counter_delta(0, 62_500, 62_500), 0, "at 0, then reloaded");
        assert_eq!(down_counter_delta(7, 7, 62_500), 0);
    }

    #[test]
    fn tsc_frequencies_must_be_believable() {
        assert!(plausible_tsc_hz(2_900_000_000) && plausible_tsc_hz(10_000_000) && plausible_tsc_hz(100_000_000_000));
        assert!(!plausible_tsc_hz(9_999_999), "a reference that finished far too soon");
        assert!(!plausible_tsc_hz(100_000_000_001));
    }

    #[test]
    fn busy_percent_rounds_and_clamps() {
        assert_eq!(busy_percent(0, 0), 0);
        assert_eq!(busy_percent(1000, 1000), 0);
        assert_eq!(busy_percent(1000, 0), 100);
        assert_eq!(busy_percent(1000, 875), 13); // 12.5 rounds up
        assert_eq!(busy_percent(1000, 876), 12);
        assert_eq!(busy_percent(3, 1), 67);
        assert_eq!(busy_percent(10, 11), 0);
        assert_eq!(busy_percent(u64::MAX, 0), 100);
    }

    #[test]
    fn percent_rounds_and_clamps() {
        assert_eq!(percent(1, 3), 33);
        assert_eq!(percent(2, 3), 67);
        assert_eq!(percent(5, 0), 0);
        assert_eq!(percent(7, 5), 100);
    }

    #[test]
    fn per_second_scales_by_the_span() {
        assert_eq!(per_second(1000, NS_PER_SEC), 1000);
        assert_eq!(per_second(1500, 1_500_000_000), 1000);
        assert_eq!(per_second(999, 2 * NS_PER_SEC), 499);
        assert_eq!(per_second(5, 0), 0);
        assert_eq!(per_second(u64::MAX, 1), u64::MAX);
    }

    #[test]
    fn median_ignores_outliers() {
        assert_eq!(median(&mut [5, 1, 900, 3, 4]), Some(4));
        assert_eq!(median(&mut [4, 2]), Some(3));
        assert_eq!(median(&mut [u64::MAX, u64::MAX]), Some(u64::MAX), "no overflow");
        assert_eq!(median(&mut [3, 4]), Some(3), "rounds down");
        assert_eq!(median(&mut [5, 7]), Some(6));
        assert_eq!(median(&mut []), None);
    }

    #[test]
    fn clocks_convert_both_ways() {
        let tsc = Clock::new(2_900_000_000).unwrap();
        assert_eq!(tsc.ns(2_900_000_000), NS_PER_SEC);
        assert_eq!(tsc.ns(2), 0, "rounds down");
        assert_eq!(tsc.ns(3), 1);
        assert_eq!(tsc.ticks(1), 3, "rounds up: at least 1 ns");
        assert_eq!(tsc.ticks(10_000_000), 29_000_000);
        // Ten years of uptime: no overflow in either direction.
        let decade = 10 * 365 * 24 * 3600 * NS_PER_SEC;
        assert_eq!(tsc.ns(tsc.ticks(decade)), decade);
        assert_eq!(Clock::new(1).unwrap().ns(u64::MAX), u64::MAX, "saturates");
        assert_eq!(Clock::new(u64::MAX).unwrap().ticks(u64::MAX), u64::MAX, "saturates");
        assert_eq!(Clock::new(0), None);
    }

    #[test]
    fn stamps_have_one_width() {
        assert_eq!(Stamp(Some(12_345_678)).to_string(), "[    0.012345]");
        assert_eq!(Stamp(Some(0)).to_string(), "[    0.000000]");
        assert_eq!(Stamp(Some(3 * NS_PER_SEC + 999)).to_string(), "[    3.000000]", "rounds down to the µs");
        assert_eq!(Stamp(None).to_string(), "[    ?.??????]");
        assert_eq!(Stamp(None).to_string().len(), Stamp(Some(1)).to_string().len());
        assert_eq!(Stamp(Some(123_456 * NS_PER_SEC)).to_string(), "[123456.000000]", "wider than the field: grows");
    }

    #[test]
    fn durations_read_in_milliseconds() {
        use std::format;
        assert_eq!(format!("{:+}", Millis(12_000)), "+0.012 ms");
        assert_eq!(format!("{:+}", Millis(3_400_000_000)), "+3400.000 ms");
        assert_eq!(format!("{:+}", Millis(999)), "+0.000 ms", "rounds down to the µs");
        assert_eq!(Millis(1_234_567).to_string(), "1.234 ms");
        assert_eq!(Millis(3_400_000_000).to_string(), "3400.000 ms");
    }

    #[test]
    fn frequencies_read_well() {
        assert_eq!(Hz(2_900_000_000).to_string(), "2.9 GHz");
        assert_eq!(Hz(2_899_956_252).to_string(), "2.9 GHz");
        assert_eq!(Hz(3_456_789_012).to_string(), "3.46 GHz");
        assert_eq!(Hz(1_000_000_000).to_string(), "1 GHz");
        assert_eq!(Hz(PIT_HZ).to_string(), "1.19 MHz");
        assert_eq!(Hz(100_000_000).to_string(), "100 MHz");
        assert_eq!(Hz(14_318_180).to_string(), "14.32 MHz");
        assert_eq!(Hz(999_999_999).to_string(), "1000 MHz", "rounding stays in the unit");
        assert_eq!(Hz(1_500).to_string(), "1.5 kHz");
        assert_eq!(Hz(999).to_string(), "999 Hz");
        assert_eq!(Hz(0).to_string(), "0 Hz");
    }
}
