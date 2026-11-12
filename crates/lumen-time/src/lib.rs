//! Timekeeping arithmetic for the Lumen kernel: working out a counter's
//! frequency from a reference clock, converting ticks to and from
//! nanoseconds, and the counts the PIT and the local APIC timer are
//! programmed with. All of it is integer math (the kernel has no floating
//! point) in `u128` where products can overflow, so it holds for any uptime.

#![no_std]

use core::fmt;

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

/// The median of `samples` (sorted in place). A calibration window can be
/// stretched by an SMI or, in a VM, by the host descheduling the vCPU; the
/// median of a few windows ignores such outliers.
pub fn median(samples: &mut [u64]) -> Option<u64> {
    samples.sort_unstable();
    match samples.len() {
        0 => None,
        n if n % 2 == 1 => Some(samples[n / 2]),
        n => Some(samples[n / 2 - 1] / 2 + samples[n / 2] / 2 + (samples[n / 2 - 1] % 2 + samples[n / 2] % 2) / 2),
    }
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

/// A frequency for humans, with three significant figures at most:
/// `2.90 GHz` is shown as "2.9 GHz", `1_193_182` as "1.19 MHz".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hz(pub u64);

impl fmt::Display for Hz {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(u64, &str); 4] = [(1_000_000_000, "GHz"), (1_000_000, "MHz"), (1_000, "kHz"), (1, "Hz")];
        let (scale, unit) = UNITS.into_iter().find(|&(s, _)| self.0 >= s).unwrap_or((1, "Hz"));
        if scale == 1 {
            return write!(f, "{} Hz", self.0);
        }
        // Hundredths of the unit, rounded; then drop trailing zeros.
        let hundredths = (u128::from(self.0) * 100 + u128::from(scale / 2)) / u128::from(scale);
        let (whole, frac) = (hundredths / 100, hundredths % 100);
        match frac {
            0 => write!(f, "{whole} {unit}"),
            f10 if f10 % 10 == 0 => write!(f, "{whole}.{} {unit}", f10 / 10),
            _ => write!(f, "{whole}.{frac:02} {unit}"),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::string::ToString;

    use super::*;

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
