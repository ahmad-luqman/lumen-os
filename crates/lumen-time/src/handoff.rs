//! Where the boot timeline's origin falls after the bootloader's handoff.
//!
//! Limine reports when it started and when it handed off to the kernel, in
//! whole microseconds of the TSC by its own calibration, and the frequency
//! it calibrated (its bootloader-performance and TSC-frequency responses;
//! the kernel reads them in `boot.rs`). [`LimineTimes`] holds them;
//! [`BeforeKmain`] adds the kernel's origin (`kmain`'s first TSC reading)
//! and the kernel's own TSC calibration, checks that they are in order
//! ([`BeforeKmain::check`]) and shows them as the `boot: before kmain:` log
//! line. Every comparison is in ticks, Limine's microseconds turned back
//! with Limine's own frequency, so none depends on how fast the boot went
//! (#55): a starved emulator stretches the gaps but can't reorder them.
//!
//! The check bounds the origin from below: at least Limine's post-handoff
//! stall after its handoff reading. Together with the timeline's own order
//! (the origin at or before arch-init) that places the origin between
//! Limine's handoff and arch-init. It doesn't prove the origin is `kmain`'s
//! first instruction: an origin read later, before arch-init, passes.

use core::fmt;

use crate::{Clock, Millis, ticks_at_usec};

/// Why Limine's times, or the origin, aren't known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unknown {
    /// Limine gave no TSC-frequency response.
    NoTscFrequency,
    /// Limine gave no bootloader-performance response.
    NoPerformanceTimes,
    /// Limine's TSC frequency is 0 Hz.
    ZeroHz,
    /// The kernel's own TSC calibration hasn't run (or failed).
    NoClock,
    /// The kernel recorded no origin.
    NoOrigin,
}

impl fmt::Display for Unknown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Unknown::NoTscFrequency => "no TSC frequency from Limine",
            Unknown::NoPerformanceTimes => "no bootloader performance times from Limine",
            Unknown::ZeroHz => "Limine's TSC frequency is 0 Hz",
            Unknown::NoClock => "the TSC isn't calibrated",
            Unknown::NoOrigin => "no origin",
        })
    }
}

/// Limine's times for the boot, in whole microseconds of the TSC (since
/// it was 0) by Limine's own calibration, and that calibration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimineTimes {
    clock: Clock,
    started_us: u64,
    handed_off_us: u64,
}

impl LimineTimes {
    /// From Limine's two responses: the TSC frequency (`frequency`), and
    /// the performance times (`init_usec`, `exec_usec`). Either missing, or
    /// a frequency of 0 Hz, says which.
    pub fn from_responses(frequency: Option<u64>, performance: Option<(u64, u64)>) -> Result<Self, Unknown> {
        let hz = frequency.ok_or(Unknown::NoTscFrequency)?;
        let (started_us, handed_off_us) = performance.ok_or(Unknown::NoPerformanceTimes)?;
        let clock = Clock::new(hz).ok_or(Unknown::ZeroHz)?;
        Ok(LimineTimes { clock, started_us, handed_off_us })
    }

    /// Limine's TSC frequency.
    pub fn hz(&self) -> u64 {
        self.clock.hz()
    }

    /// When Limine started: its `init_usec`, read just after it calibrated
    /// the TSC on entry (so some 30 ms after the firmware handed it control,
    /// when it calibrates against the firmware's `Stall`).
    pub fn started_us(&self) -> u64 {
        self.started_us
    }

    /// When Limine handed off: its `exec_usec`, the last TSC reading it
    /// records before jumping to the kernel (it reads the TSC again after
    /// it, in [`LIMINE_STALL_US`]'s wait).
    pub fn handed_off_us(&self) -> u64 {
        self.handed_off_us
    }

    /// The handoff in ticks, at or before Limine's own reading (rounded
    /// down; [`ticks_at_usec`]). `None` if it doesn't fit in 64 bits.
    pub fn handoff_tick(&self) -> Option<u64> {
        ticks_at_usec(self.handed_off_us, self.clock.hz())
    }
}

/// How long Limine (v12.9.4, the pinned version) waits after its handoff
/// reading before it jumps to the kernel: after `exec_usec = rdtsc_usec()`,
/// `common_spinup` (after `lgdt`, which serializes) calls `flush_irqs`,
/// which on x86-64 always runs `stall(10000)`: it reads the TSC and spins
/// until it has advanced `(tsc_freq * 10000 + 999999) / 1000000` ticks, with
/// the same `tsc_freq` it reports. So the kernel's first instruction comes
/// at least that many ticks after the handoff reading. This depends on that
/// Limine internal: check it again when Limine is bumped.
pub const LIMINE_STALL_US: u64 = 10_000;

/// [`LIMINE_STALL_US`] in ticks of a `hz` TSC, rounded up as Limine's
/// `stall` does.
pub fn limine_stall_ticks(hz: u64) -> u64 {
    u64::try_from((u128::from(hz) * u128::from(LIMINE_STALL_US)).div_ceil(1_000_000)).unwrap_or(u64::MAX)
}

/// Limine's times, the kernel's origin (`entry`, the TSC as `kmain` began)
/// and the kernel's own TSC calibration (`clock`): what the
/// `boot: before kmain:` line shows, and what [`check`](Self::check)
/// checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BeforeKmain {
    limine: LimineTimes,
    entry: u64,
    clock: Clock,
}

/// Where the origin falls relative to the handoff tick: the sign, once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Gap {
    /// At or after it, by this many ticks.
    After(u64),
    /// Before it, by this many ticks.
    Before(u64),
}

/// What [`BeforeKmain::check`] found out of order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disorder {
    /// Limine says it started at the TSC's 0.
    StartedAtZero,
    /// Limine says it started at or after it handed off.
    StartedAtOrAfterHandoff,
    /// Limine's handoff in ticks doesn't fit in 64 bits.
    HandoffOverflows,
    /// The handoff tick, turned back into microseconds, isn't Limine's
    /// own microsecond (or, rounded down, the one before): the tick isn't
    /// in Limine's units.
    UnitsDisagree { back_us: u64 },
    /// Limine's TSC frequency and the kernel's own calibration differ by
    /// more than a factor of 2.
    FrequenciesDisagree,
    /// The origin is before the handoff tick.
    OriginBeforeHandoff,
    /// The origin is after the handoff tick, but by less than Limine's
    /// wait after it ([`limine_stall_ticks`]).
    OriginWithinLimineStall { after: u64, stall: u64 },
}

impl fmt::Display for Disorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Disorder::StartedAtZero => f.write_str("Limine started at the TSC's 0"),
            Disorder::StartedAtOrAfterHandoff => f.write_str("Limine started at or after its handoff"),
            Disorder::HandoffOverflows => f.write_str("Limine's handoff time in ticks doesn't fit in 64 bits"),
            Disorder::UnitsDisagree { back_us } => {
                write!(f, "the handoff tick reads back as {back_us} us, not Limine's microsecond")
            }
            Disorder::FrequenciesDisagree => f.write_str("Limine's TSC frequency and ours differ by more than 2x"),
            Disorder::OriginBeforeHandoff => f.write_str("the origin is before Limine's handoff"),
            Disorder::OriginWithinLimineStall { after, stall } => {
                write!(f, "the origin is {after} ticks after Limine's handoff, within its {stall}-tick wait")
            }
        }
    }
}

impl BeforeKmain {
    /// Everything the line needs, or the first thing missing: the kernel's
    /// clock, Limine's times, the origin.
    pub fn new(
        clock: Option<Clock>,
        limine: Result<LimineTimes, Unknown>,
        entry: Option<u64>,
    ) -> Result<Self, Unknown> {
        let clock = clock.ok_or(Unknown::NoClock)?;
        let limine = limine?;
        let entry = entry.ok_or(Unknown::NoOrigin)?;
        Ok(BeforeKmain { limine, entry, clock })
    }

    pub fn limine(&self) -> &LimineTimes {
        &self.limine
    }

    pub fn entry(&self) -> u64 {
        self.entry
    }

    /// The kernel's own TSC calibration.
    pub fn clock(&self) -> Clock {
        self.clock
    }

    fn gap(&self) -> Option<Gap> {
        let handoff = self.limine.handoff_tick()?;
        Some(match self.entry.checked_sub(handoff) {
            Some(after) => Gap::After(after),
            None => Gap::Before(handoff - self.entry),
        })
    }

    /// Limine started after the TSC's 0 and before its handoff; the handoff
    /// tick is in Limine's units; Limine's frequency agrees with ours within
    /// 2x (not a timing check: a frequency); and the origin is at least
    /// Limine's post-handoff wait after the handoff tick. The first failure.
    pub fn check(&self) -> Result<(), Disorder> {
        let l = &self.limine;
        if l.started_us == 0 {
            return Err(Disorder::StartedAtZero);
        }
        if l.started_us >= l.handed_off_us {
            return Err(Disorder::StartedAtOrAfterHandoff);
        }
        let handoff = l.handoff_tick().ok_or(Disorder::HandoffOverflows)?;
        units_agree(l, handoff)?;
        let (theirs, ours) = (u128::from(l.hz()), u128::from(self.clock.hz()));
        if theirs * 2 < ours || theirs > ours * 2 {
            return Err(Disorder::FrequenciesDisagree);
        }
        match self.gap() {
            Some(Gap::Before(_)) => Err(Disorder::OriginBeforeHandoff),
            Some(Gap::After(after)) => {
                let stall = limine_stall_ticks(l.hz());
                if after < stall { Err(Disorder::OriginWithinLimineStall { after, stall }) } else { Ok(()) }
            }
            None => Err(Disorder::HandoffOverflows),
        }
    }
}

/// The handoff tick turned back into microseconds, through `Clock::ns`
/// rather than the conversion being checked: rounded down twice, so
/// Limine's own microsecond, or the one before it (at 1 MHz and up; below,
/// up to a tick's worth of microseconds before it). Anything else means the
/// tick isn't in Limine's units. Time-free: it holds however fast the boot
/// went.
fn units_agree(l: &LimineTimes, handoff: u64) -> Result<(), Disorder> {
    let back_us = l.clock.ns(handoff) / 1000;
    let slack = 1_000_000u64.div_ceil(l.hz());
    if back_us > l.handed_off_us || l.handed_off_us - back_us > slack {
        return Err(Disorder::UnitsDisagree { back_us });
    }
    Ok(())
}

/// `limine started at 1932.697 ms and handed off at 2514.149 ms (TSC
/// time), kmain began +10.379 ms after the handoff`: Limine's own times,
/// and the origin's distance from the handoff tick, converted with the
/// kernel's clock and signed (`-` before it). With a handoff that doesn't
/// fit in 64 bits, the distance is unknown and says so.
impl fmt::Display for BeforeKmain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let l = &self.limine;
        let ms = |us: u64| Millis(us.saturating_mul(1000));
        write!(
            f,
            "limine started at {} and handed off at {} (TSC time), kmain began ",
            ms(l.started_us),
            ms(l.handed_off_us)
        )?;
        match self.gap() {
            Some(Gap::After(t)) => write!(f, "{:+} after the handoff", Millis(self.clock.ns(t))),
            Some(Gap::Before(t)) => write!(f, "-{} after the handoff", Millis(self.clock.ns(t))),
            None => f.write_str("at an unknown time after the handoff (its time in ticks doesn't fit in 64 bits)"),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::string::ToString;

    use super::*;

    const GHZ3: u64 = 3_000_000_000;

    fn limine(hz: u64, started_us: u64, handed_off_us: u64) -> LimineTimes {
        LimineTimes::from_responses(Some(hz), Some((started_us, handed_off_us))).unwrap()
    }

    fn before(l: LimineTimes, entry: u64, our_hz: u64) -> BeforeKmain {
        BeforeKmain::new(Clock::new(our_hz), Ok(l), Some(entry)).unwrap()
    }

    #[test]
    fn missing_responses_and_0_hz_say_which() {
        let perf = Some((1, 2));
        assert_eq!(LimineTimes::from_responses(None, perf), Err(Unknown::NoTscFrequency));
        assert_eq!(LimineTimes::from_responses(Some(GHZ3), None), Err(Unknown::NoPerformanceTimes));
        assert_eq!(LimineTimes::from_responses(None, None), Err(Unknown::NoTscFrequency));
        assert_eq!(LimineTimes::from_responses(Some(0), perf), Err(Unknown::ZeroHz));
        let l = LimineTimes::from_responses(Some(GHZ3), perf).unwrap();
        assert_eq!((l.hz(), l.started_us(), l.handed_off_us()), (GHZ3, 1, 2));
        assert_eq!(Unknown::ZeroHz.to_string(), "Limine's TSC frequency is 0 Hz");
        assert_eq!(Unknown::NoPerformanceTimes.to_string(), "no bootloader performance times from Limine");
    }

    #[test]
    fn the_line_needs_a_clock_limines_times_and_an_origin() {
        let times = limine(GHZ3, 1, 2);
        let clock = Clock::new(GHZ3);
        assert_eq!(BeforeKmain::new(None, Ok(times), Some(5)), Err(Unknown::NoClock));
        assert_eq!(BeforeKmain::new(clock, Err(Unknown::NoTscFrequency), Some(5)), Err(Unknown::NoTscFrequency));
        assert_eq!(BeforeKmain::new(clock, Ok(times), None), Err(Unknown::NoOrigin));
        assert_eq!(Unknown::NoOrigin.to_string(), "no origin");
        let b = BeforeKmain::new(clock, Ok(times), Some(5)).unwrap();
        assert_eq!((b.entry(), Some(b.clock()), *b.limine()), (5, clock, times));
    }

    #[test]
    fn the_stall_is_limines_10_ms_rounded_up() {
        assert_eq!(limine_stall_ticks(GHZ3), 30_000_000);
        assert_eq!(limine_stall_ticks(2_899_956_252), 28_999_563, "rounded up");
        assert_eq!(limine_stall_ticks(100), 1);
        assert_eq!(limine_stall_ticks(u64::MAX), u64::MAX / 100 + 1);
    }

    #[test]
    fn an_origin_after_limines_wait_is_in_order() {
        // Limine at 2.9 GHz-ish (an odd calibration), ours slightly off.
        let hz = 2_899_956_252;
        let l = limine(hz, 1_932_697, 2_514_149);
        let handoff = l.handoff_tick().unwrap();
        let stall = limine_stall_ticks(hz);
        let b = before(l, handoff + stall, 2_900_000_000);
        assert_eq!(b.check(), Ok(()));
        assert_eq!(
            b.to_string(),
            "limine started at 1932.697 ms and handed off at 2514.149 ms (TSC time), kmain began +9.999 ms after \
             the handoff"
        );
        // As tests/scenarios/test_boot.py's BEFORE_KMAIN matches it.
        let b = before(l, handoff + 30_100_000, 2_900_000_000);
        assert_eq!(b.check(), Ok(()));
        assert!(b.to_string().ends_with("kmain began +10.379 ms after the handoff"), "{b}");
    }

    #[test]
    fn an_origin_inside_limines_wait_or_before_the_handoff_is_not() {
        let l = limine(GHZ3, 1_000_000, 2_000_000);
        let handoff = l.handoff_tick().unwrap();
        assert_eq!(handoff, 6_000_000_000);
        let stall = 30_000_000;
        let at = |entry| before(l, entry, GHZ3);
        assert_eq!(at(handoff + stall - 1).check(), Err(Disorder::OriginWithinLimineStall { after: stall - 1, stall }));
        assert_eq!(at(handoff).check(), Err(Disorder::OriginWithinLimineStall { after: 0, stall }));
        assert_eq!(at(handoff - 1).check(), Err(Disorder::OriginBeforeHandoff));
        assert_eq!(at(1).check(), Err(Disorder::OriginBeforeHandoff), "the TSC's reset");
        assert_eq!(at(3_000_000_000).check(), Err(Disorder::OriginBeforeHandoff), "Limine's start");
        assert_eq!(
            at(handoff).to_string(),
            "limine started at 1000.000 ms and handed off at 2000.000 ms (TSC time), kmain began +0.000 ms after the handoff"
        );
        assert!(at(handoff - 1).to_string().ends_with("kmain began -0.000 ms after the handoff"));
        assert!(at(1).to_string().ends_with("kmain began -1999.999 ms after the handoff"), "{}", at(1));
        assert_eq!(
            Disorder::OriginWithinLimineStall { after: 5, stall }.to_string(),
            "the origin is 5 ticks after Limine's handoff, within its 30000000-tick wait"
        );
        assert_eq!(Disorder::OriginBeforeHandoff.to_string(), "the origin is before Limine's handoff");
    }

    #[test]
    fn limines_start_must_come_after_0_and_before_its_handoff() {
        let entry = 10 * GHZ3;
        assert_eq!(before(limine(GHZ3, 0, 2_000_000), entry, GHZ3).check(), Err(Disorder::StartedAtZero));
        let same = before(limine(GHZ3, 2_000_000, 2_000_000), entry, GHZ3);
        assert_eq!(same.check(), Err(Disorder::StartedAtOrAfterHandoff));
        let later = before(limine(GHZ3, 2_000_001, 2_000_000), entry, GHZ3);
        assert_eq!(later.check(), Err(Disorder::StartedAtOrAfterHandoff));
        assert_eq!(Disorder::StartedAtZero.to_string(), "Limine started at the TSC's 0");
    }

    #[test]
    fn a_handoff_past_64_bits_is_reported_not_wrapped() {
        let b = before(limine(GHZ3, 1, u64::MAX), u64::MAX, GHZ3);
        assert_eq!(b.limine().handoff_tick(), None);
        assert_eq!(b.check(), Err(Disorder::HandoffOverflows));
        assert!(
            b.to_string().ends_with(
                "kmain began at an unknown time after the handoff (its time in ticks doesn't fit in 64 bits)"
            )
        );
        assert_eq!(Disorder::HandoffOverflows.to_string(), "Limine's handoff time in ticks doesn't fit in 64 bits");
    }

    #[test]
    fn frequencies_must_agree_within_2x() {
        let l = limine(GHZ3, 1_000_000, 2_000_000);
        let entry = l.handoff_tick().unwrap() + 40_000_000;
        assert_eq!(before(l, entry, GHZ3 * 2).check(), Ok(()));
        assert_eq!(before(l, entry, GHZ3 / 2).check(), Ok(()));
        assert_eq!(before(l, entry, GHZ3 * 2 + 1).check(), Err(Disorder::FrequenciesDisagree));
        assert_eq!(before(l, entry, GHZ3 / 2 - 1).check(), Err(Disorder::FrequenciesDisagree));
    }

    #[test]
    fn frequencies_other_than_round_ones_convert_back_to_limines_microsecond() {
        // At 1 MHz a tick is a microsecond; at odd and low frequencies the
        // round trip still lands on Limine's microsecond or the one(s)
        // before it, and the check accepts it.
        for hz in [1_000_000, 1_193_182, 14_318_180, 999_999_937, 2_899_956_252, 5_123_456_789] {
            for us in [1_000_001, 2_514_149, 13_565_029, 99_999_999] {
                let l = limine(hz, 1, us);
                let entry = l.handoff_tick().unwrap() + limine_stall_ticks(hz);
                assert_eq!(before(l, entry, hz).check(), Ok(()), "{hz} Hz, {us} us");
            }
        }
    }

    /// Microseconds taken for ticks (the mix-up #55's review found), and
    /// ticks off by more than rounding: the round trip doesn't land on
    /// Limine's microsecond.
    #[test]
    fn a_handoff_tick_in_the_wrong_units_is_caught() {
        let l = limine(GHZ3, 1_000_000, 2_000_000);
        let tick = l.handoff_tick().unwrap();
        assert_eq!(units_agree(&l, tick), Ok(()));
        assert_eq!(units_agree(&l, tick - 2_999), Ok(()), "the microsecond before");
        assert_eq!(units_agree(&l, tick - 3_001), Err(Disorder::UnitsDisagree { back_us: 1_999_998 }));
        assert_eq!(units_agree(&l, tick + 3_000), Err(Disorder::UnitsDisagree { back_us: 2_000_001 }));
        assert_eq!(units_agree(&l, 2_000_000), Err(Disorder::UnitsDisagree { back_us: 666 }), "us as ticks");
        assert_eq!(
            Disorder::UnitsDisagree { back_us: 666 }.to_string(),
            "the handoff tick reads back as 666 us, not Limine's microsecond"
        );
        // Below 1 MHz a tick spans several microseconds: up to that many.
        let slow = limine(250_000, 1, 2_000_003);
        let tick = slow.handoff_tick().unwrap();
        assert_eq!(units_agree(&slow, tick), Ok(()));
        assert_eq!(units_agree(&slow, tick - 1), Err(Disorder::UnitsDisagree { back_us: 1_999_996 }));
    }
}
