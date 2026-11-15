//! Sound synthesis for the kernel's `beep` and `play`, kept free of kernel
//! dependencies so it can be tested on the host. Everything is integer
//! arithmetic: the kernel is built without SSE, so floating point would be
//! slow soft-float calls.
//!
//! - [`osc`]: phase-accumulator oscillators (square, triangle, sine from a
//!   table, noise from an LFSR).
//! - [`env`]: ADSR envelopes.
//! - [`melody`]: the notation `play` takes (`"t=140 C4/8 E4/8 G4/4. r/8"`).
//! - [`play`]: voices, mixing with saturation, and the [`Source`]s a sound
//!   driver pulls PCM from ([`beep`]'s [`Tone`], a melody's [`Sequencer`]).
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

pub use env::{Adsr, Envelope};
pub use melody::{Melody, ParseError, ParseErrorKind, Token};
pub use osc::{Oscillator, Waveform};
pub use play::{Sequencer, Source, Tone, Voice, beep, mix, saturate};

/// The sample rate used when nothing else is negotiated, in Hz.
pub const DEFAULT_RATE: u32 = 48_000;

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

    /// The frequency rounded to whole hertz.
    pub const fn hz(self) -> u32 {
        self.0 / 1000 + (self.0 % 1000 >= 500) as u32
    }
}

/// The number of samples in `ms` milliseconds at `rate` Hz (rounded down,
/// saturating).
pub const fn samples_in_ms(ms: u32, rate: u32) -> u32 {
    let n = ms as u64 * rate as u64 / 1000;
    if n > u32::MAX as u64 { u32::MAX } else { n as u32 }
}
