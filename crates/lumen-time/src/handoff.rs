//! Where the boot timeline's origin falls after the bootloader's handoff.
//!
//! Limine reports when it started and when it handed off to the kernel, in
//! whole microseconds of the TSC by its own calibration, and the frequency
//! it calibrated (its bootloader-performance and TSC-frequency responses;
//! the kernel reads them in `boot.rs`). [`LimineTimes`] holds them;
//! [`BeforeKmain`] adds the kernel's origin (`kmain`'s first TSC reading)
//! and the kernel's own TSC calibration, checks them
//! ([`BeforeKmain::check`], on every boot) and shows them as the
//! `boot: before kmain:` log line. None of the checks is a time bound:
//! Limine's microseconds are turned back into ticks with Limine's own
//! frequency and compared with ticks, and the one comparison of
//! frequencies is one-sided, so a starved emulator stretching the boot
//! can't fail any of them (#55).
//!
//! The check bounds the origin from below: at least Limine's post-handoff
//! stall after its handoff reading. Together with the timeline's own order
//! (the origin at or before arch-init) that places the origin between
//! Limine's handoff and arch-init. It doesn't prove the origin is `kmain`'s
//! first instruction: an origin read later, before arch-init, passes.
//!
//! It trusts Limine's microseconds: apart from `0 < started < handoff` and
//! the handoff being at least [`LIMINE_STALL_US`] after the TSC's 0, nothing
//! checks that the handoff time is right (the frequency check bounds the
//! stall's length in our ticks, not where the handoff tick is). Times far
//! too small would let a too-early origin pass.

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

    /// Limine's TSC frequency. Limine takes it from CPUID leaf 0x15 when
    /// the CPU reports a crystal ratio and frequency there; otherwise
    /// (always under QEMU, which zeroes leaf 0x15) it times three 10 ms
    /// firmware `Stall`s and keeps the smallest count, so a stretched
    /// `Stall` can only make it read high.
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

    /// [`LIMINE_STALL_US`] in Limine's ticks, rounded up as Limine's
    /// `stall` does (`(tsc_freq * us + 999999) / 1000000`, i.e.
    /// `ceil(hz / 100)`).
    pub fn stall_ticks(&self) -> u64 {
        self.clock.ticks(LIMINE_STALL_US * 1000)
    }
}

/// How long Limine (v12.9.4, the pinned version) waits after its handoff
/// reading before it jumps to the kernel. After `exec_usec = rdtsc_usec()`,
/// `common_spinup` (after `lgdt`, which serializes) calls `flush_irqs`.
/// That stalls only for `IRQ_PIC_APIC_FLUSH`, which `limine.c` sets on
/// x86-64 just before, unconditionally: `stall(10000)` reads the TSC and
/// spins until it has advanced `(tsc_freq * 10000 + 999999) / 1000000`
/// ticks, with the same `tsc_freq` it reports. So the kernel's first
/// instruction comes at least that many ticks after the handoff reading.
/// This depends on those Limine internals: check them again when Limine is
/// bumped.
pub const LIMINE_STALL_US: u64 = 10_000;

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

/// What [`BeforeKmain::check`] found out of order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disorder {
    /// Limine says it started at the TSC's 0.
    StartedAtZero,
    /// Limine says it started at or after it handed off.
    StartedAtOrAfterHandoff,
    /// Limine says it handed off less than its own post-handoff wait after
    /// the TSC's 0: not a believable time (the firmware alone takes longer).
    HandoffTooEarly { handed_off_us: u64 },
    /// Limine's handoff in ticks doesn't fit in 64 bits.
    HandoffOverflows,
    /// The handoff tick, turned back into microseconds, isn't Limine's
    /// own microsecond (or one up to `ceil(10^6 / hz)` before it). An
    /// internal consistency assertion: it fails only if the conversion is
    /// broken (or `Clock::ns` saturates, some 584 years in).
    UnitsDisagree { back_us: u64 },
    /// Limine's TSC frequency is under half of the kernel's own
    /// calibration: its 10 ms wait would be under 5 ms of ours.
    FrequenciesDisagree { limine_hz: u64, ours_hz: u64 },
    /// The origin is before the handoff tick.
    OriginBeforeHandoff,
    /// The origin is after the handoff tick, but by less than Limine's
    /// wait after it ([`LimineTimes::stall_ticks`]).
    OriginWithinLimineStall { after: u64, stall: u64 },
}

impl fmt::Display for Disorder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Disorder::StartedAtZero => f.write_str("Limine started at the TSC's 0"),
            Disorder::StartedAtOrAfterHandoff => f.write_str("Limine started at or after its handoff"),
            Disorder::HandoffTooEarly { handed_off_us } => write!(
                f,
                "Limine handed off at {handed_off_us} us, sooner after the TSC's 0 than its own {LIMINE_STALL_US} us wait"
            ),
            Disorder::HandoffOverflows => f.write_str("Limine's handoff time in ticks doesn't fit in 64 bits"),
            Disorder::UnitsDisagree { back_us } => {
                write!(f, "the handoff tick reads back as {back_us} us, not Limine's microsecond")
            }
            Disorder::FrequenciesDisagree { limine_hz, ours_hz } => {
                write!(f, "Limine's TSC frequency {limine_hz} Hz is under half of ours, {ours_hz} Hz")
            }
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

    /// The first failure of, in order:
    /// - Limine started after the TSC's 0 and before its handoff, and
    ///   handed off at least [`LIMINE_STALL_US`] after the TSC's 0 (beyond
    ///   these, Limine's microseconds are trusted);
    /// - the handoff tick fits in 64 bits, and reads back as Limine's
    ///   microsecond (an internal consistency assertion);
    /// - Limine's frequency is at least half of ours, so its 10 ms wait is
    ///   at least 5 ms of ours. One-sided: a high Limine frequency (what a
    ///   stretched calibration `Stall` gives) only lengthens the wait in
    ///   our ticks, so it can't make an early origin pass;
    /// - the origin is at least Limine's wait after the handoff tick.
    ///
    /// None is a time bound: it runs on every boot, and a starved boot
    /// can't fail it.
    pub fn check(&self) -> Result<(), Disorder> {
        let l = &self.limine;
        if l.started_us == 0 {
            return Err(Disorder::StartedAtZero);
        }
        if l.started_us >= l.handed_off_us {
            return Err(Disorder::StartedAtOrAfterHandoff);
        }
        if l.handed_off_us < LIMINE_STALL_US {
            return Err(Disorder::HandoffTooEarly { handed_off_us: l.handed_off_us });
        }
        self.check_with(l.handoff_tick().ok_or(Disorder::HandoffOverflows)?)
    }

    /// The rest of [`check`](Self::check), given the handoff tick.
    fn check_with(&self, handoff: u64) -> Result<(), Disorder> {
        let l = &self.limine;
        units_agree(l, handoff)?;
        let (limine_hz, ours_hz) = (l.hz(), self.clock.hz());
        if u128::from(limine_hz) * 2 < u128::from(ours_hz) {
            return Err(Disorder::FrequenciesDisagree { limine_hz, ours_hz });
        }
        let after = self.entry.checked_sub(handoff).ok_or(Disorder::OriginBeforeHandoff)?;
        let stall = l.stall_ticks();
        if after < stall {
            return Err(Disorder::OriginWithinLimineStall { after, stall });
        }
        Ok(())
    }
}

/// The handoff tick turned back into microseconds, through `Clock::ns`
/// rather than the conversion being checked: rounded down twice, so
/// Limine's own microsecond, or one up to `ceil(10^6 / hz)` before it (the
/// one before, at 1 MHz and up). Anything else means the tick isn't in
/// Limine's units. Time-free: it holds however fast the boot went.
fn units_agree(l: &LimineTimes, handoff: u64) -> Result<(), Disorder> {
    let back_us = l.clock.ns(handoff) / 1000;
    let slack = 1_000_000u64.div_ceil(l.hz());
    if !(l.handed_off_us.saturating_sub(slack)..=l.handed_off_us).contains(&back_us) {
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
        let Some(handoff) = l.handoff_tick() else {
            return f.write_str("at an unknown time after the handoff (its time in ticks doesn't fit in 64 bits)");
        };
        match self.entry.checked_sub(handoff) {
            Some(after) => write!(f, "{:+} after the handoff", Millis(self.clock.ns(after))),
            None => write!(f, "-{} after the handoff", Millis(self.clock.ns(handoff - self.entry))),
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
        assert_eq!(Unknown::NoTscFrequency.to_string(), "no TSC frequency from Limine");
        assert_eq!(Unknown::NoClock.to_string(), "the TSC isn't calibrated");
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
    #[allow(clippy::manual_div_ceil)] // Limine's formula, written as Limine writes it
    fn the_stall_is_limines_10_ms_rounded_up() {
        let stall = |hz| limine(hz, 1, 2).stall_ticks();
        assert_eq!(stall(GHZ3), 30_000_000);
        assert_eq!(stall(2_899_956_252), 28_999_563, "rounded up");
        assert_eq!(stall(100), 1);
        assert_eq!(stall(u64::MAX), u64::MAX / 100 + 1);
        // Limine's own formula, `(tsc_freq * 10000 + 999999) / 1000000`.
        for hz in [1_000_000u64, 1_193_182, 999_999_937, 2_899_956_252, 5_123_456_789] {
            assert_eq!(u128::from(stall(hz)), (u128::from(hz) * 10_000 + 999_999) / 1_000_000, "{hz} Hz");
        }
    }

    #[test]
    fn an_origin_after_limines_wait_is_in_order() {
        // Limine at 2.9 GHz-ish (an odd calibration), ours slightly off.
        let hz = 2_899_956_252;
        let l = limine(hz, 1_932_697, 2_514_149);
        let handoff = l.handoff_tick().unwrap();
        let stall = l.stall_ticks();
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
        assert_eq!(before(limine(GHZ3, 1_999_999, 2_000_000), entry, GHZ3).check(), Ok(()), "a microsecond before");
        assert_eq!(before(limine(GHZ3, 1, 2_000_000), entry, GHZ3).check(), Ok(()), "a microsecond after the TSC's 0");
        assert_eq!(Disorder::StartedAtOrAfterHandoff.to_string(), "Limine started at or after its handoff");
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
    fn limines_frequency_must_be_at_least_half_of_ours() {
        let l = limine(GHZ3, 1_000_000, 2_000_000);
        let entry = l.handoff_tick().unwrap() + 40_000_000;
        assert_eq!(before(l, entry, GHZ3 * 2).check(), Ok(()), "exactly half");
        assert_eq!(
            before(l, entry, GHZ3 * 2 + 1).check(),
            Err(Disorder::FrequenciesDisagree { limine_hz: GHZ3, ours_hz: GHZ3 * 2 + 1 })
        );
        // One-sided: a Limine frequency far above ours (a stretched
        // calibration `Stall` reads high) passes.
        assert_eq!(before(l, entry, GHZ3 / 2 - 1).check(), Ok(()));
        assert_eq!(before(l, entry, GHZ3 / 3).check(), Ok(()), "3x ours");
        assert_eq!(
            Disorder::FrequenciesDisagree { limine_hz: 1_000_000_000, ours_hz: 2_100_000_000 }.to_string(),
            "Limine's TSC frequency 1000000000 Hz is under half of ours, 2100000000 Hz"
        );
    }

    #[test]
    fn frequencies_other_than_round_ones_convert_back_to_limines_microsecond() {
        // At 1 MHz a tick is a microsecond; at odd and low frequencies the
        // round trip still lands on Limine's microsecond or the one(s)
        // before it, and the check accepts it.
        for hz in [1_000_000, 1_193_182, 14_318_180, 999_999_937, 2_899_956_252, 5_123_456_789] {
            for us in [1_000_001, 2_514_149, 13_565_029, 99_999_999] {
                let l = limine(hz, 1, us);
                let entry = l.handoff_tick().unwrap() + l.stall_ticks();
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

    #[test]
    fn a_handoff_sooner_than_limines_own_wait_is_not_believed() {
        let entry = 10 * GHZ3;
        let early = before(limine(GHZ3, 1, LIMINE_STALL_US - 1), entry, GHZ3);
        assert_eq!(early.check(), Err(Disorder::HandoffTooEarly { handed_off_us: 9_999 }));
        assert_eq!(before(limine(GHZ3, 1, LIMINE_STALL_US), entry, GHZ3).check(), Ok(()), "exactly the wait");
        assert_eq!(
            Disorder::HandoffTooEarly { handed_off_us: 9_999 }.to_string(),
            "Limine handed off at 9999 us, sooner after the TSC's 0 than its own 10000 us wait"
        );
    }

    /// `check` runs the units check on the tick it computes: given a tick in
    /// microseconds (the mix-up #55's review found), it says so.
    #[test]
    fn check_refuses_a_handoff_tick_in_microseconds() {
        let l = limine(GHZ3, 1_000_000, 2_000_000);
        let b = before(l, 10 * GHZ3, GHZ3);
        assert_eq!(b.check_with(l.handoff_tick().unwrap()), Ok(()));
        assert_eq!(b.check_with(2_000_000), Err(Disorder::UnitsDisagree { back_us: 666 }));
    }
}
