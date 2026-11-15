//! Sound synthesis for the shell's coming `beep` and `play` (#99), kept
//! free of kernel dependencies so it can be tested on the host. Everything
//! is integer arithmetic: the kernel is built without SSE, so floating
//! point would be slow soft-float calls.
//!
//! - [`osc`](mod@osc): phase-accumulator oscillators (square, triangle, sine
//!   from a table, noise from an LFSR).
//! - [`env`](mod@env): ADSR envelopes.
//! - [`melody`](mod@melody): the notation `play` takes
//!   (`"t=140 C4/8 E4/8 G4/4. r/8"`).
//! - [`play`](mod@play): voices, mixing with saturation, and the
//!   [`Source`]s a sound driver pulls PCM from ([`beep`]'s [`Tone`], a
//!   melody's [`Sequencer`]).
//!
//! Output is signed 16-bit PCM, written into caller-provided buffers
//! ([`Source::fill`]), so a driver can feed fixed-size periods without
//! allocating.

#![no_std]

#[cfg(test)]
extern crate std;

pub mod env;
pub mod melody;
pub mod osc;
pub mod play;

#[cfg(test)]
mod test_util;

use core::fmt;
use core::num::{NonZeroU8, NonZeroU32};
use core::time::Duration;

pub use env::{Adsr, Envelope};
pub use melody::{Length, Melody, ParseError, ParseErrorKind, Pitch, Token};
pub use osc::{Oscillator, Waveform};
pub use play::{Filled, Sequencer, Source, Tone, Voice, beep, mix};

/// A frequency, in millihertz (notes aren't whole hertz: C4 is 261.626 Hz).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Freq(u32);

impl Freq {
    /// `hz` hertz (saturating at about 4.29 MHz).
    pub const fn from_hz(hz: u32) -> Freq {
        Freq(hz.saturating_mul(1000))
    }

    /// `mhz` millihertz.
    pub const fn from_millihz(mhz: u32) -> Freq {
        Freq(mhz)
    }

    /// The frequency in millihertz.
    pub const fn millihz(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Freq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{:03} Hz", self.0 / 1000, self.0 % 1000)
    }
}

/// A sample rate in hertz (never 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rate(NonZeroU32);

impl Rate {
    /// 48 kHz, the rate used when nothing else is negotiated.
    pub const DEFAULT: Rate = Rate(NonZeroU32::new(48_000).unwrap());

    /// `hz` samples a second, or `None` for 0.
    pub const fn new(hz: u32) -> Option<Rate> {
        match NonZeroU32::new(hz) {
            Some(hz) => Some(Rate(hz)),
            None => None,
        }
    }

    /// Samples a second.
    pub const fn hz(self) -> u32 {
        self.0.get()
    }

    /// The number of samples in `ms` milliseconds (rounded down, saturating).
    pub fn samples_in_ms(self, ms: u32) -> u32 {
        u32::try_from(u64::from(ms) * u64::from(self.hz()) / 1000).unwrap_or(u32::MAX)
    }

    /// The number of samples in `length` (rounded down, saturating).
    pub fn samples_in(self, length: Duration) -> u32 {
        // Whole seconds and the rest apart, so nothing overflows a u64.
        let whole = length.as_secs().saturating_mul(u64::from(self.hz()));
        let part = u64::from(length.subsec_nanos()) * u64::from(self.hz()) / 1_000_000_000;
        u32::try_from(whole.saturating_add(part)).unwrap_or(u32::MAX)
    }

    /// Whether `freq` is below half the rate (the highest pitch these
    /// samples can carry); at or above it, oscillators can't play it.
    pub fn carries(self, freq: Freq) -> bool {
        u64::from(freq.millihz()) * 2 < u64::from(self.hz()) * 1000
    }

    /// `Ok` if the rate [`carries`](Self::carries) `freq`.
    pub fn check(self, freq: Freq) -> Result<(), TooHigh> {
        if self.carries(freq) { Ok(()) } else { Err(TooHigh { freq, rate: self }) }
    }
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} Hz", self.hz())
    }
}

/// A frequency at or above half the sample rate: the samples can't carry
/// it (a sine would play near-silence, a square an alias).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TooHigh {
    pub freq: Freq,
    pub rate: Rate,
}

impl fmt::Display for TooHigh {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} is too high for a {} sample rate (it must be below half)", self.freq, self.rate)
    }
}

impl core::error::Error for TooHigh {}

/// A PCM stream's shape: the rate and how many channels each frame has.
/// Sources are mono; [`Source::fill`] copies each sample to every channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Format {
    pub rate: Rate,
    pub channels: NonZeroU8,
}

impl Format {
    /// The number of `i16` samples in one frame.
    pub fn frame_len(self) -> usize {
        usize::from(self.channels.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_convert_lengths_and_saturate() {
        let r = Rate::DEFAULT;
        assert_eq!(r.hz(), 48_000);
        assert_eq!(Rate::new(0), None);
        assert_eq!(r.samples_in_ms(250), 12_000);
        assert_eq!(r.samples_in_ms(u32::MAX), u32::MAX);
        assert_eq!(r.samples_in(Duration::from_micros(1500)), 72);
        assert_eq!(r.samples_in(Duration::from_secs(2)), 96_000);
        assert_eq!(r.samples_in(Duration::MAX), u32::MAX);
    }

    #[test]
    fn rates_carry_only_frequencies_below_half() {
        let r = Rate::new(8000).unwrap();
        assert!(r.carries(Freq::from_millihz(3_999_999)));
        assert!(!r.carries(Freq::from_hz(4000)));
        assert_eq!(r.check(Freq::from_hz(5000)), Err(TooHigh { freq: Freq::from_hz(5000), rate: r }));
        assert!(!Rate::new(1).unwrap().carries(Freq::from_millihz(500)));
    }

    #[test]
    fn freq_saturates_and_prints() {
        assert_eq!(Freq::from_hz(u32::MAX).millihz(), u32::MAX);
        assert_eq!(Freq::from_hz(4_294_967).millihz(), 4_294_967_000);
        assert_eq!(std::format!("{}", Freq::from_millihz(261_626)), "261.626 Hz");
    }

    #[test]
    fn formats_count_samples_per_frame() {
        let f = Format { rate: Rate::DEFAULT, channels: NonZeroU8::new(2).unwrap() };
        assert_eq!(f.frame_len(), 2);
    }
}
