//! Voices, mixing, and the sources a sound driver pulls PCM from.

use crate::env::{Adsr, Envelope};
use crate::melody::{self, Melody, Token, Tokens};
use crate::osc::{Oscillator, Waveform};
use crate::{Freq, samples_in_ms};

/// Clamp a mixed sample to 16 bits (never wrap: a wrapped peak is a loud
/// click).
pub fn saturate(sample: i64) -> i16 {
    sample.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
}

/// Add samples together, saturating to 16 bits.
pub fn mix(samples: impl IntoIterator<Item = i32>) -> i16 {
    saturate(samples.into_iter().map(i64::from).sum())
}

/// An oscillator shaped by an envelope, at a volume.
#[derive(Clone, Debug)]
pub struct Voice {
    osc: Oscillator,
    env: Envelope,
    volume: u8,
    rate: u32,
}

impl Voice {
    /// The volume a new voice plays at, in percent: half scale, so two
    /// voices mix without clipping.
    pub const DEFAULT_VOLUME: u8 = 50;

    /// A silent voice at `rate` samples a second.
    pub fn new(wave: Waveform, adsr: Adsr, rate: u32) -> Voice {
        Voice {
            osc: Oscillator::new(wave, Freq::default(), rate),
            env: Envelope::new(adsr, rate),
            volume: Voice::DEFAULT_VOLUME,
            rate,
        }
    }

    /// Set the volume in percent (0..=100; more counts as 100).
    pub fn set_volume(&mut self, percent: u8) {
        self.volume = percent.min(100);
    }

    /// Change the waveform (takes effect at once, keeping the phase).
    pub fn set_wave(&mut self, wave: Waveform) {
        self.osc.set_wave(wave);
    }

    /// Start playing `freq` (the phase carries on, so no click).
    pub fn note_on(&mut self, freq: Freq) {
        self.osc.set_freq(freq, self.rate);
        self.env.note_on();
    }

    /// Release the note.
    pub fn note_off(&mut self) {
        self.env.note_off();
    }

    /// Whether the voice is silent until the next note.
    pub fn is_idle(&self) -> bool {
        self.env.is_idle()
    }

    /// The number of samples a release from full level takes.
    pub fn release_samples(&self) -> u32 {
        self.env.release_samples()
    }

    /// The next sample (within ±32767; [`mix`] it with others).
    pub fn sample(&mut self) -> i32 {
        let raw = i64::from(self.osc.sample());
        let gain = i64::from(self.env.gain());
        (raw * gain * i64::from(self.volume) / (100 << 16)) as i32
    }
}

/// Something that produces mono samples until it ends.
pub trait Source {
    /// The next sample, or `None` once the sound is over.
    fn next_sample(&mut self) -> Option<i16>;

    /// Fill `out` with interleaved frames of `channels` samples (each frame
    /// the same sample on every channel) and return how many frames hold
    /// sound. Frames after the end are silence (zeros), so a driver can
    /// always hand the whole buffer to the device; a return of less than
    /// `out.len() / channels` means the sound ended in this buffer.
    /// Samples beyond the last whole frame are left alone.
    ///
    /// # Panics
    ///
    /// If `channels` is 0.
    fn fill(&mut self, out: &mut [i16], channels: usize) -> usize {
        assert!(channels > 0, "fill: channels must be at least 1");
        let mut frames = 0;
        let mut ended = false;
        for frame in out.chunks_exact_mut(channels) {
            let sample = if ended { None } else { self.next_sample() };
            match sample {
                Some(s) => {
                    frame.fill(s);
                    frames += 1;
                }
                None => {
                    ended = true;
                    frame.fill(0);
                }
            }
        }
        frames
    }
}

/// A single tone of fixed length ([`beep`]).
#[derive(Clone, Debug)]
pub struct Tone {
    voice: Voice,
    left: u32,
    release_at: u32,
}

/// A square-wave tone of `freq_hz` for `ms` milliseconds at `rate` samples
/// a second, with click-free edges ([`Adsr::CLICK_FREE`]): it lasts exactly
/// `ms * rate / 1000` samples and fades out by its end.
pub fn beep(freq_hz: u32, ms: u32, rate: u32) -> Tone {
    Tone::new(Waveform::Square, Freq::from_hz(freq_hz), ms, rate)
}

impl Tone {
    /// A tone of `freq` in `wave` for `ms` milliseconds at `rate`.
    pub fn new(wave: Waveform, freq: Freq, ms: u32, rate: u32) -> Tone {
        let mut voice = Voice::new(wave, Adsr::CLICK_FREE, rate);
        let left = samples_in_ms(ms, rate);
        voice.note_on(freq);
        // Release so the fade ends with the tone (or halfway, if it's short).
        let release_at = voice.release_samples().min(left / 2);
        Tone { voice, left, release_at }
    }

    /// Set the volume in percent (0..=100).
    pub fn set_volume(&mut self, percent: u8) {
        self.voice.set_volume(percent);
    }
}

impl Source for Tone {
    fn next_sample(&mut self) -> Option<i16> {
        if self.left == 0 {
            return None;
        }
        if self.left == self.release_at {
            self.voice.note_off();
        }
        self.left -= 1;
        Some(saturate(i64::from(self.voice.sample())))
    }
}

/// A melody being played ([`Melody`]).
#[derive(Clone, Debug)]
pub struct Sequencer<'a> {
    tokens: Tokens<'a>,
    voice: Voice,
    rate: u32,
    tempo: u16,
    /// Samples left in the current note or rest.
    left: u32,
    /// When `left` reaches this, the note is released (`u32::MAX`: never).
    release_at: u32,
}

impl<'a> Sequencer<'a> {
    /// Play `melody` at `rate` samples a second, in the default waveform
    /// and [`Adsr::PLUCK`] until the melody's `w=` says otherwise. It lasts
    /// exactly [`Melody::duration_samples`].
    pub fn new(melody: &Melody<'a>, rate: u32) -> Sequencer<'a> {
        Sequencer {
            tokens: melody.tokens(),
            voice: Voice::new(Waveform::default(), Adsr::PLUCK, rate),
            rate,
            tempo: melody::DEFAULT_TEMPO,
            left: 0,
            release_at: u32::MAX,
        }
    }

    /// Set the volume in percent (0..=100).
    pub fn set_volume(&mut self, percent: u8) {
        self.voice.set_volume(percent);
    }
}

impl Source for Sequencer<'_> {
    fn next_sample(&mut self) -> Option<i16> {
        while self.left == 0 {
            match self.tokens.next()? {
                Token::Note(pitch, len) => {
                    self.left = len.samples(self.tempo, self.rate);
                    // Release before the next note so repeated notes are
                    // heard as separate (short notes: halfway).
                    self.release_at = self.voice.release_samples().min(self.left / 2);
                    self.voice.note_on(pitch.freq());
                }
                Token::Rest(len) => {
                    self.left = len.samples(self.tempo, self.rate);
                    self.release_at = u32::MAX;
                    self.voice.note_off();
                }
                Token::Tempo(t) => self.tempo = t,
                Token::Wave(w) => self.voice.set_wave(w),
            }
        }
        if self.left == self.release_at {
            self.voice.note_off();
        }
        self.left -= 1;
        Some(saturate(i64::from(self.voice.sample())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    const RATE: u32 = 48_000;

    fn collect(source: &mut impl Source) -> Vec<i16> {
        core::iter::from_fn(|| source.next_sample()).collect()
    }

    fn rising_zero_crossings(samples: &[i16]) -> usize {
        samples.windows(2).filter(|w| w[0] < 0 && w[1] >= 0).count()
    }

    #[test]
    fn mixing_saturates_instead_of_wrapping() {
        assert_eq!(mix([20_000, 20_000]), i16::MAX);
        assert_eq!(mix([-20_000, -20_000]), i16::MIN);
        assert_eq!(mix([i32::MAX, i32::MAX, i32::MAX]), i16::MAX);
        assert_eq!(mix([i32::MIN, i32::MIN]), i16::MIN);
        assert_eq!(mix([1000, -400, 25]), 625);
        assert_eq!(mix([]), 0);
        assert_eq!(saturate(i64::MIN), i16::MIN);
    }

    #[test]
    fn beep_lasts_exactly_its_length_and_fades_at_both_ends() {
        let s = collect(&mut beep(440, 250, RATE));
        assert_eq!(s.len(), 12_000);
        assert!(s[0].unsigned_abs() < 2000, "starts quietly: {}", s[0]);
        assert!(s.last().unwrap().unsigned_abs() < 200, "ends quietly: {}", s.last().unwrap());
        let peak = s.iter().map(|v| v.unsigned_abs()).max().unwrap();
        let half = i16::MAX as u16 / 2;
        assert!(peak.abs_diff(half) < 100, "default volume is half scale: {peak}");
    }

    #[test]
    fn beep_has_its_frequency_within_one_percent() {
        for hz in [220, 440, 1000, 3000] {
            let s = collect(&mut beep(hz, 1000, RATE));
            let n = rising_zero_crossings(&s);
            assert!(n.abs_diff(hz as usize) <= hz as usize / 100 + 1, "{hz} Hz: {n}");
        }
    }

    #[test]
    fn zero_length_and_zero_volume_beeps_are_silent() {
        assert!(collect(&mut beep(440, 0, RATE)).is_empty());
        let mut quiet = beep(440, 10, RATE);
        quiet.set_volume(0);
        assert!(collect(&mut quiet).iter().all(|&v| v == 0));
    }

    #[test]
    fn fill_interleaves_and_pads_with_silence() {
        let mut tone = beep(1000, 1, RATE); // 48 samples
        let mut buf = vec![7i16; 2 * 64 + 1];
        let frames = tone.fill(&mut buf, 2);
        assert_eq!(frames, 48);
        assert!(buf[..96].chunks(2).all(|f| f[0] == f[1]), "both channels the same");
        assert!(buf[96..128].iter().all(|&v| v == 0), "silence after the end");
        assert_eq!(buf[128], 7, "the partial frame is left alone");
        assert_eq!(tone.fill(&mut buf, 2), 0);
        // In periods: the same samples as pulling them one at a time.
        let whole = collect(&mut beep(440, 20, RATE));
        let mut tone = beep(440, 20, RATE);
        let mut pieces = Vec::new();
        let mut period = [0i16; 100];
        loop {
            let n = tone.fill(&mut period, 1);
            pieces.extend_from_slice(&period[..n]);
            if n < period.len() {
                break;
            }
        }
        assert_eq!(pieces, whole);
    }

    #[test]
    #[should_panic(expected = "channels")]
    fn fill_refuses_zero_channels() {
        beep(440, 10, RATE).fill(&mut [0; 4], 0);
    }

    #[test]
    fn a_melody_lasts_its_duration_and_plays_its_notes_in_order() {
        let m = Melody::parse("t=120 w=sine A4/8 r/8 E5/8 A5/8").unwrap();
        let s = collect(&mut Sequencer::new(&m, RATE));
        assert_eq!(s.len() as u32, m.duration_samples(RATE));
        assert_eq!(s.len(), 4 * 12_000);
        // Each note's first 200 ms (before its release falls silent): 88,
        // 0, 132 and 176 rising zero crossings (within 2%).
        let crossings: Vec<usize> = s.chunks(12_000).map(|note| rising_zero_crossings(&note[..9600])).collect();
        for (got, want) in crossings.iter().zip([88, 0, 132, 176]) {
            assert!(got.abs_diff(want) <= want / 50 + 1, "{crossings:?}");
        }
        assert!(s[12_000 + 2400..24_000].iter().all(|&v| v == 0), "the rest is silent after the release");
    }

    #[test]
    fn repeated_notes_are_separated_by_their_release() {
        let m = Melody::parse("C4/4 C4/4").unwrap();
        let s = collect(&mut Sequencer::new(&m, RATE));
        let boundary = 24_000;
        let quiet = s[boundary - 50..boundary].iter().map(|v| v.unsigned_abs()).max().unwrap();
        let loud = s[boundary + 2000..boundary + 3000].iter().map(|v| v.unsigned_abs()).max().unwrap();
        assert!(quiet < loud / 8, "a gap before the repeat: {quiet} vs {loud}");
    }

    #[test]
    fn volume_scales_the_output() {
        let m = Melody::parse("w=square A4/4").unwrap();
        let peak = |vol| {
            let mut seq = Sequencer::new(&m, RATE);
            seq.set_volume(vol);
            collect(&mut seq).iter().map(|v| v.unsigned_abs()).max().unwrap()
        };
        assert_eq!(peak(0), 0);
        let (full, half) = (peak(100), peak(50));
        assert!(full > 32_000 && half.abs_diff(full / 2) < 100, "{full} {half}");
        assert_eq!(peak(200), full, "volume over 100 counts as 100");
    }
}
