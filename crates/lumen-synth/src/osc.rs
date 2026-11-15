//! Oscillators: a 32-bit phase accumulator advanced by a fixed step each
//! sample (one full cycle is 2^32), shaped into a waveform. Every waveform
//! is at full scale (±[`FULL_SCALE`]); volume and envelopes come later
//! ([`crate::play::Voice`]).
//!
//! The square and noise are naive (not band-limited): at high pitches
//! their harmonics above half the rate fold back as audible aliases.

use crate::{Freq, Rate};

/// The largest sample an oscillator produces (and the negative of the
/// smallest), so negating a sample never overflows.
pub const FULL_SCALE: i16 = i16::MAX;

/// The shape of an oscillator's output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Waveform {
    /// High for the first half of the cycle, low for the second.
    #[default]
    Square,
    /// Linear ramps, in phase with [`Waveform::Sine`].
    Triangle,
    /// A sine from a quarter-wave table with linear interpolation.
    Sine,
    /// Binary noise: a 16-bit LFSR (period 65535) sampled and held, one new
    /// bit per cycle of the frequency, so the frequency sets how coarse it
    /// sounds.
    Noise,
}

impl Waveform {
    /// Every waveform, in the order of their names in [`Waveform::name`].
    pub const ALL: [Waveform; 4] = [Waveform::Square, Waveform::Triangle, Waveform::Sine, Waveform::Noise];

    /// The waveform's name in the melody notation (`w=sine`).
    pub const fn name(self) -> &'static str {
        match self {
            Waveform::Square => "square",
            Waveform::Triangle => "triangle",
            Waveform::Sine => "sine",
            Waveform::Noise => "noise",
        }
    }

    /// The waveform named `name` (as [`Waveform::name`] spells it).
    pub fn from_name(name: &str) -> Option<Waveform> {
        Waveform::ALL.into_iter().find(|w| w.name() == name)
    }
}

impl core::fmt::Display for Waveform {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

/// The phase step for `freq` at `rate`: the fraction of a cycle per sample,
/// scaled to 2^32 and rounded down. Frequencies at or above half the rate
/// are clamped just below it; [`Rate::check`] refuses them before they get
/// here (at the clamp a sine or triangle samples near its zeros: silence).
pub fn phase_step(freq: Freq, rate: Rate) -> u32 {
    // (2^32 - 1) << 32 still fits in a u64, so this can't overflow.
    let step = (u64::from(freq.millihz()) << 32) / (u64::from(rate.hz()) * 1000);
    step.min((1 << 31) - 1) as u32
}

/// The noise LFSR's taps (16, 14, 13, 11): a maximal-length sequence.
const LFSR_TAPS: u16 = 0xB400;

/// One oscillator.
#[derive(Clone, Debug)]
pub struct Oscillator {
    wave: Waveform,
    rate: Rate,
    phase: u32,
    step: u32,
    /// The noise generator's state (never 0).
    lfsr: u16,
}

impl Oscillator {
    /// An oscillator playing `freq` at `rate`, starting at phase 0 (where
    /// the sine and triangle cross zero rising). At 0 Hz it is silent.
    pub fn new(wave: Waveform, freq: Freq, rate: Rate) -> Oscillator {
        Oscillator { wave, rate, phase: 0, step: phase_step(freq, rate), lfsr: 0xACE1 }
    }

    /// Change the frequency, keeping the phase (no click).
    pub fn set_freq(&mut self, freq: Freq) {
        self.step = phase_step(freq, self.rate);
    }

    /// Change the waveform, keeping the phase.
    pub fn set_wave(&mut self, wave: Waveform) {
        self.wave = wave;
    }

    /// The sample rate.
    pub fn rate(&self) -> Rate {
        self.rate
    }

    /// The next sample, at full scale (0 at 0 Hz: a square or noise held
    /// at one level would be DC, not silence).
    pub fn sample(&mut self) -> i16 {
        if self.step == 0 {
            return 0;
        }
        let phase = self.phase;
        let (next, wrapped) = phase.overflowing_add(self.step);
        self.phase = next;
        match self.wave {
            Waveform::Square => {
                if phase < 1 << 31 {
                    FULL_SCALE
                } else {
                    -FULL_SCALE
                }
            }
            Waveform::Triangle => triangle(phase),
            Waveform::Sine => sine(phase),
            Waveform::Noise => {
                if wrapped {
                    self.lfsr = lfsr_step(self.lfsr);
                }
                if self.lfsr & 1 == 1 { FULL_SCALE } else { -FULL_SCALE }
            }
        }
    }
}

/// One step of the Galois LFSR.
fn lfsr_step(state: u16) -> u16 {
    let shifted = state >> 1;
    if state & 1 == 1 { shifted ^ LFSR_TAPS } else { shifted }
}

/// A triangle in phase with the sine: 0 at phase 0, the peak at a quarter,
/// 0 at half, the trough at three quarters.
fn triangle(phase: u32) -> i16 {
    // The top 16 bits, 0..65536 per cycle; each ramp's `* 2` spans
    // -32768..=32768, clamped to ±FULL_SCALE.
    let t = (phase >> 16) as i32;
    let v = if t < 16384 {
        t * 2
    } else if t < 49152 {
        32768 - (t - 16384) * 2
    } else {
        (t - 65536) * 2
    };
    v.clamp(-i32::from(FULL_SCALE), i32::from(FULL_SCALE)) as i16
}

/// The sine's first quarter, 0..=[`FULL_SCALE`], sampled at 257 points
/// (both ends included); built at compile time from a Taylor series in
/// fixed point (no floating point anywhere).
static QUARTER: [i16; 257] = quarter_table();

const fn quarter_table() -> [i16; 257] {
    const Q30: i64 = 1 << 30;
    // π/2 in Q30 (1.5707963267948966 * 2^30, rounded).
    const HALF_PI: i64 = 1_686_629_713;
    let mut table = [0i16; 257];
    let mut i = 0;
    while i < 257 {
        let x = HALF_PI * i as i64 / 256;
        // sin x = x - x^3/3! + x^5/5! - ...; up to x^17 is plenty for Q30.
        // Every product stays below 2^63: |term|, x <= 1.6 * 2^30.
        let mut term = x;
        let mut sum = x;
        let mut k = 1;
        while k <= 8 {
            term = -(term * x / Q30) * x / Q30 / ((2 * k) * (2 * k + 1));
            sum += term;
            k += 1;
        }
        table[i] = ((sum * FULL_SCALE as i64 + Q30 / 2) / Q30) as i16;
        i += 1;
    }
    table
}

/// The sine at `phase` (2^32 is a full cycle), at full scale.
fn sine(phase: u32) -> i16 {
    const QUARTER_CYCLE: u32 = 1 << 30;
    let quadrant = phase >> 30;
    let pos = phase & (QUARTER_CYCLE - 1);
    // The second and fourth quadrants read the table backwards.
    let pos = if quadrant & 1 == 1 { QUARTER_CYCLE - pos } else { pos };
    // `pos` is 0..=2^30: the table index is pos >> 22, 0..=256 (256 only
    // when the fraction is 0), the interpolation fraction the low 22 bits.
    let index = (pos >> 22) as usize;
    let frac = (pos & 0x3f_ffff) as i32;
    let a = i32::from(QUARTER[index]);
    let b = QUARTER.get(index + 1).map_or(a, |&b| i32::from(b));
    // The table only rises, and by at most 32767 * sin(π/512) ≈ 201 per
    // entry, so `(b - a) * frac` < 2^30 fits an i32.
    let v = (a + (((b - a) * frac) >> 22)) as i16;
    if quadrant >= 2 { -v } else { v }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{measured_hz, within_1_percent};
    use std::vec::Vec;

    const RATE: Rate = Rate::DEFAULT;

    fn render(wave: Waveform, hz: u32, samples: usize) -> Vec<i16> {
        let mut osc = Oscillator::new(wave, Freq::from_hz(hz), RATE);
        (0..samples).map(|_| osc.sample()).collect()
    }

    #[test]
    fn tones_are_at_their_frequency_within_one_percent() {
        for wave in [Waveform::Square, Waveform::Triangle, Waveform::Sine] {
            for hz in [55, 110, 440, 1000, 2500, 8000] {
                let got = measured_hz(&render(wave, hz, 48_000), RATE.hz());
                assert!(within_1_percent(got, f64::from(hz)), "{wave} {hz} Hz: {got}");
            }
        }
    }

    #[test]
    fn the_sine_table_matches_floating_point() {
        for k in 0..=4096u64 {
            let phase = (k << 20) as u32;
            let want = (std::f64::consts::TAU * phase as f64 / 4294967296.0).sin() * f64::from(FULL_SCALE);
            let got = f64::from(sine(phase));
            assert!((got - want).abs() <= 2.0, "phase {phase:#x}: {got} vs {want}");
        }
        assert_eq!(QUARTER[0], 0);
        assert_eq!(QUARTER[256], FULL_SCALE);
        assert!(QUARTER.windows(2).all(|w| w[0] <= w[1] && w[1] - w[0] <= 202), "rising by at most 202");
    }

    #[test]
    fn waveforms_hit_their_peaks_and_never_reach_i16_min() {
        for wave in Waveform::ALL {
            let s = render(wave, 440, 4800);
            let (lo, hi) = (*s.iter().min().unwrap(), *s.iter().max().unwrap());
            assert!(lo >= -FULL_SCALE, "{wave}: {lo}");
            assert!(lo < -FULL_SCALE + 400 && hi > FULL_SCALE - 400, "{wave} misses its peaks: {lo}..{hi}");
        }
    }

    #[test]
    fn zero_hz_is_silence_on_every_waveform() {
        for wave in Waveform::ALL {
            assert!(render(wave, 0, 1000).iter().all(|&v| v == 0), "{wave}");
        }
    }

    #[test]
    fn square_is_high_half_the_time() {
        let highs = render(Waveform::Square, 440, 48_000).iter().filter(|&&v| v > 0).count();
        assert!((23_500..=24_500).contains(&highs), "{highs}");
    }

    #[test]
    fn sine_and_triangle_start_at_zero_rising() {
        for wave in [Waveform::Sine, Waveform::Triangle] {
            let s = render(wave, 440, 10);
            assert_eq!(s[0], 0, "{wave}");
            assert!(s.windows(2).all(|w| w[1] > w[0]), "{wave}: {s:?}");
        }
    }

    #[test]
    fn triangle_is_piecewise_linear() {
        // A quarter cycle rises by the same amount each sample (to ±2: the
        // phase's low 16 bits are dropped, then doubled).
        let s = render(Waveform::Triangle, 480, 25);
        let diffs: Vec<i32> = s.windows(2).map(|w| i32::from(w[1]) - i32::from(w[0])).collect();
        let first = diffs[0];
        assert!(diffs.iter().all(|&d| (d - first).abs() <= 2), "{diffs:?}");
    }

    #[test]
    fn the_noise_lfsr_has_period_65535() {
        let start = 0xACE1;
        let mut state = lfsr_step(start);
        let mut period = 1u32;
        while state != start {
            assert_ne!(state, 0, "the LFSR must never reach 0");
            state = lfsr_step(state);
            period += 1;
        }
        assert_eq!(period, 65_535);
    }

    #[test]
    fn noise_takes_a_new_bit_once_per_cycle() {
        let s = render(Waveform::Noise, 1000, 48_000);
        let changes = s.windows(2).filter(|w| w[0] != w[1]).count();
        // At most one new bit per cycle (1000 a second), about half differ.
        assert!(changes <= 1000 && changes > 300, "{changes} changes");
        let highs = s.iter().filter(|&&v| v > 0).count();
        assert!((18_000..30_000).contains(&highs), "{highs} high samples of 48000");
    }

    #[test]
    fn phase_step_is_exact_and_clamps_below_half_the_rate() {
        assert_eq!(phase_step(Freq::from_hz(12_000), RATE), 1 << 30);
        assert_eq!(phase_step(Freq::from_hz(0), RATE), 0);
        assert_eq!(phase_step(Freq::from_hz(24_000), RATE), (1 << 31) - 1);
        assert_eq!(phase_step(Freq::from_millihz(u32::MAX), Rate::new(1).unwrap()), (1 << 31) - 1);
    }

    #[test]
    fn changing_the_frequency_keeps_the_phase() {
        let mut osc = Oscillator::new(Waveform::Sine, Freq::from_hz(440), RATE);
        let last = (0..20).map(|_| osc.sample()).last().unwrap();
        osc.set_freq(Freq::from_hz(441));
        let next = osc.sample();
        assert!((i32::from(next) - i32::from(last)).abs() < 2000, "jump {last} -> {next}");
        // A square 3/8 of a cycle in has 1/8 of the new cycle (10 samples
        // at 600 Hz) left high; a reset phase would stay high for 40.
        let mut sq = Oscillator::new(Waveform::Square, Freq::from_hz(1200), RATE);
        (0..15).for_each(|_| {
            sq.sample();
        });
        sq.set_freq(Freq::from_hz(600));
        let high = (0..80).take_while(|_| sq.sample() == FULL_SCALE).count();
        assert!((9..=11).contains(&high), "{high} samples high after the change");
    }

    #[test]
    fn waveform_names_round_trip() {
        for w in Waveform::ALL {
            assert_eq!(Waveform::from_name(w.name()), Some(w));
        }
        assert_eq!(Waveform::from_name("saw"), None);
    }
}
