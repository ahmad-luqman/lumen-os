//! Voices, mixing, and the sources a sound driver pulls PCM from.

use core::num::NonZeroU8;
use core::time::Duration;

use crate::env::{Adsr, Envelope, FULL_GAIN};
use crate::melody::{self, Melody, ParseError, Token};
use crate::osc::{Oscillator, Waveform};
use crate::{Freq, Rate, TooHigh};

/// Clamp a mixed sample to 16 bits (never wrap: a wrapped peak is a loud
/// click).
fn saturate(sample: i64) -> i16 {
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
}

impl Voice {
    /// The volume a new voice plays at, in percent: half scale, so two
    /// voices mix without clipping.
    pub const DEFAULT_VOLUME: u8 = 50;

    /// A silent voice at `rate`.
    pub fn new(wave: Waveform, adsr: Adsr, rate: Rate) -> Voice {
        Voice {
            osc: Oscillator::new(wave, Freq::default(), rate),
            env: Envelope::new(adsr, rate),
            volume: Voice::DEFAULT_VOLUME,
        }
    }

    /// The sample rate.
    pub fn rate(&self) -> Rate {
        self.osc.rate()
    }

    /// The volume in percent.
    pub fn volume(&self) -> u8 {
        self.volume
    }

    /// Set the volume in percent (0..=100; more is clamped to 100).
    pub fn set_volume(&mut self, percent: u8) {
        self.volume = percent.min(100);
    }

    /// Change the waveform (takes effect at once, keeping the phase).
    pub fn set_wave(&mut self, wave: Waveform) {
        self.osc.set_wave(wave);
    }

    /// Start playing `freq` (the phase carries on, so no click; 0 Hz is
    /// silent).
    pub fn note_on(&mut self, freq: Freq) {
        self.osc.set_freq(freq);
        self.env.note_on();
    }

    /// Release the note at the envelope's release rate.
    pub fn note_off(&mut self) {
        self.env.note_off();
    }

    /// Release the note so it is silent after at most `samples` more
    /// samples ([`Envelope::release_over`]).
    pub fn release_over(&mut self, samples: u32) {
        self.env.release_over(samples);
    }

    /// Whether the voice is silent until the next note.
    pub fn is_idle(&self) -> bool {
        self.env.is_idle()
    }

    /// At most how many samples a release from full level takes.
    pub fn release_samples(&self) -> u32 {
        self.env.release_samples()
    }

    /// The next sample (within ±32767; [`mix`] it with others).
    pub fn sample(&mut self) -> i16 {
        let raw = i64::from(self.osc.sample());
        let gain = i64::from(self.env.gain());
        // Gain and volume are each at most their divisor, so the result
        // stays within the oscillator's ±32767.
        (raw * gain * i64::from(self.volume) / (i64::from(FULL_GAIN) * 100)) as i16
    }
}

/// What [`Source::fill`] wrote.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Filled {
    /// Frames (one sample per channel) that hold sound; the rest of the
    /// buffer's whole frames are silence.
    pub frames: usize,
    /// The sound ended in this buffer (or before it): every later fill
    /// returns 0 frames.
    pub ended: bool,
}

/// Something that produces mono samples until it ends. Sources are fused:
/// after the first `None`, [`next_sample`](Self::next_sample) returns
/// `None` forever.
pub trait Source {
    /// The sample rate the source plays at.
    fn rate(&self) -> Rate;

    /// The next sample, or `None` once the sound is over.
    fn next_sample(&mut self) -> Option<i16>;

    /// Fill `out` with interleaved frames of `channels` samples (each frame
    /// the same sample on every channel). Frames after the end are silence
    /// (zeros), so a driver can always hand the whole buffer to the device.
    /// Samples beyond the last whole frame are left alone.
    ///
    /// # Panics
    ///
    /// If `out` is shorter than one frame (a driver would otherwise spin
    /// on fills that never move).
    fn fill(&mut self, out: &mut [i16], channels: NonZeroU8) -> Filled {
        let channels = usize::from(channels.get());
        assert!(out.len() >= channels, "fill: a {}-sample buffer holds no {channels}-channel frame", out.len());
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
        Filled { frames, ended }
    }
}

/// A voice playing one note or rest for a number of samples: [`beep`]'s
/// tone, and each note of a [`Sequencer`]. A note is released so it is
/// silent by its last sample.
#[derive(Clone, Debug)]
pub struct Tone {
    voice: Voice,
    /// Samples left in the note or rest.
    left: u32,
    /// When `left` reaches this, the note is released over what is left.
    release_at: Option<u32>,
}

/// A square-wave tone of `freq` for `length` at `rate`, with click-free
/// edges ([`Adsr::CLICK_FREE`]); [`Tone::new`] for other waveforms.
pub fn beep(freq: Freq, length: Duration, rate: Rate) -> Result<Tone, TooHigh> {
    Tone::new(Waveform::Square, freq, length, rate)
}

impl Tone {
    /// A tone of `freq` in `wave` for `length` at `rate`, with click-free
    /// edges: it lasts exactly `length` rounded down to whole samples, and
    /// fades to 0 by its last sample. 0 Hz is silence; a frequency the
    /// rate can't carry is refused.
    pub fn new(wave: Waveform, freq: Freq, length: Duration, rate: Rate) -> Result<Tone, TooHigh> {
        rate.check(freq)?;
        let mut tone = Tone::idle(Voice::new(wave, Adsr::CLICK_FREE, rate));
        tone.play(freq, rate.samples_in(length));
        Ok(tone)
    }

    /// A tone that has ended, ready for [`play`](Self::play).
    fn idle(voice: Voice) -> Tone {
        Tone { voice, left: 0, release_at: None }
    }

    /// Play `freq` for `samples`, released in time to be silent at the end
    /// (over the voice's release time, or the second half of a shorter
    /// note).
    fn play(&mut self, freq: Freq, samples: u32) {
        self.voice.note_on(freq);
        self.left = samples;
        self.release_at = Some(self.voice.release_samples().min(samples / 2).max(1));
    }

    /// Rest for `samples` (a sounding note fades at its release rate).
    fn rest(&mut self, samples: u32) {
        self.voice.note_off();
        self.left = samples;
        self.release_at = None;
    }

    /// The volume in percent.
    pub fn volume(&self) -> u8 {
        self.voice.volume()
    }

    /// Set the volume in percent (0..=100; more is clamped to 100).
    pub fn set_volume(&mut self, percent: u8) {
        self.voice.set_volume(percent);
    }

    /// Fade out now (over at most the release time) and end when silent.
    pub fn stop(&mut self) {
        if self.voice.is_idle() {
            self.left = 0;
        } else if self.left > 0 {
            let n = self.voice.release_samples().min(self.left).max(1);
            self.voice.release_over(n);
            self.left = n;
            self.release_at = None;
        }
    }
}

impl Source for Tone {
    fn rate(&self) -> Rate {
        self.voice.rate()
    }

    fn next_sample(&mut self) -> Option<i16> {
        if self.left == 0 {
            return None;
        }
        if self.release_at == Some(self.left) {
            self.voice.release_over(self.left);
        }
        self.left -= 1;
        Some(self.voice.sample())
    }
}

/// A melody being played. It owns the melody's text (`S` as in
/// [`Melody`]), so with an owned string it can move to another thread.
#[derive(Clone, Debug)]
pub struct Sequencer<S> {
    text: S,
    /// The byte offset of the next word.
    pos: usize,
    tempo: u16,
    current: Tone,
    stopped: bool,
}

impl<S: AsRef<str>> Sequencer<S> {
    /// Play `melody` at `rate` in [`Adsr::PLUCK`] (`w=` changes only the
    /// waveform, never the envelope), starting in the default waveform. It
    /// lasts exactly [`duration_samples`](Self::duration_samples). A melody
    /// with a note `rate` can't carry is refused ([`Melody::check`]).
    pub fn new(melody: Melody<S>, rate: Rate) -> Result<Sequencer<S>, ParseError> {
        melody.check(rate)?;
        Ok(Sequencer {
            text: melody.into_text(),
            pos: 0,
            tempo: melody::DEFAULT_TEMPO,
            current: Tone::idle(Voice::new(Waveform::default(), Adsr::PLUCK, rate)),
            stopped: false,
        })
    }

    /// How many samples the whole melody lasts at the sequencer's rate.
    pub fn duration_samples(&self) -> u32 {
        melody::duration_samples(self.text.as_ref(), self.rate())
    }

    /// The volume in percent.
    pub fn volume(&self) -> u8 {
        self.current.volume()
    }

    /// Set the volume in percent (0..=100; more is clamped to 100).
    pub fn set_volume(&mut self, percent: u8) {
        self.current.set_volume(percent);
    }

    /// Fade out the note playing (over at most its release time) and end
    /// there, playing no more notes.
    pub fn stop(&mut self) {
        self.stopped = true;
        self.current.stop();
    }
}

impl<S: AsRef<str>> Source for Sequencer<S> {
    fn rate(&self) -> Rate {
        self.current.rate()
    }

    fn next_sample(&mut self) -> Option<i16> {
        loop {
            if let Some(sample) = self.current.next_sample() {
                return Some(sample);
            }
            if self.stopped {
                return None;
            }
            let rate = self.rate();
            match melody::next_token(self.text.as_ref(), &mut self.pos)? {
                Token::Note(pitch, len) => self.current.play(pitch.freq(), len.samples(self.tempo, rate)),
                Token::Rest(len) => self.current.rest(len.samples(self.tempo, rate)),
                Token::Tempo(t) => self.tempo = t,
                Token::Wave(w) => self.current.voice.set_wave(w),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{measured_hz, peak, rising_zero_crossings, within_1_percent};
    use std::string::String;
    use std::vec;
    use std::vec::Vec;

    const RATE: Rate = Rate::DEFAULT;
    const STEREO: NonZeroU8 = NonZeroU8::new(2).unwrap();
    const MONO: NonZeroU8 = NonZeroU8::MIN;

    fn collect(source: &mut impl Source) -> Vec<i16> {
        core::iter::from_fn(|| source.next_sample()).collect()
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn beep_hz(hz: u32, length: Duration) -> Tone {
        beep(Freq::from_hz(hz), length, RATE).unwrap()
    }

    fn play(text: &str) -> Vec<i16> {
        collect(&mut Sequencer::new(Melody::parse(text).unwrap(), RATE).unwrap())
    }

    #[test]
    fn mixing_saturates_instead_of_wrapping() {
        assert_eq!(mix([20_000, 20_000]), i16::MAX);
        assert_eq!(mix([-20_000, -20_000]), i16::MIN);
        assert_eq!(mix([i32::MAX, i32::MAX, i32::MAX]), i16::MAX);
        assert_eq!(mix([i32::MIN, i32::MIN]), i16::MIN);
        assert_eq!(mix([1000, -400, 25]), 625);
        assert_eq!(mix([]), 0);
    }

    #[test]
    fn beep_lasts_exactly_its_length_and_fades_at_both_ends() {
        let s = collect(&mut beep_hz(440, ms(250)));
        assert_eq!(s.len(), 12_000);
        assert!(s[0].unsigned_abs() < 2000, "starts quietly: {}", s[0]);
        assert_eq!(*s.last().unwrap(), 0, "ends silent");
        assert!(peak(&s).abs_diff(16_383) < 100, "default volume is half scale: {}", peak(&s));
    }

    #[test]
    fn short_beeps_and_notes_end_silent() {
        for len in [1, 2, 3, 4, 5, 6, 10] {
            let s = collect(&mut beep_hz(440, ms(len)));
            assert_eq!(s.len(), 48 * len as usize);
            assert_eq!(*s.last().unwrap(), 0, "{len} ms beep: ends on {}", s.last().unwrap());
        }
        for text in ["t=999 C4/32", "t=240 C4/32", "t=999 C4/32 C4/32 C4/32"] {
            let s = play(text);
            assert_eq!(*s.last().unwrap(), 0, "{text}");
        }
    }

    #[test]
    fn repeated_short_notes_are_separated() {
        let s = play("t=240 C5/32 C5/32");
        // Each note is 1500 samples, silent at its end.
        assert_eq!(s.len(), 3000);
        assert_eq!(s[1499], 0);
        assert!(peak(&s[1450..1500]) < peak(&s[1600..1800]) / 8, "a dip between the notes");
    }

    #[test]
    fn beep_has_its_frequency_within_one_percent() {
        for hz in [55, 220, 440, 1000, 3000] {
            let got = measured_hz(&collect(&mut beep_hz(hz, ms(1000))), RATE.hz());
            assert!(within_1_percent(got, f64::from(hz)), "{hz} Hz: {got}");
        }
    }

    #[test]
    fn zero_hz_zero_length_and_zero_volume_are_silent() {
        for wave in Waveform::ALL {
            let mut t = Tone::new(wave, Freq::from_hz(0), ms(100), RATE).unwrap();
            let s = collect(&mut t);
            assert_eq!(s.len(), 4800);
            assert!(s.iter().all(|&v| v == 0), "{wave}");
        }
        assert!(collect(&mut beep_hz(440, ms(0))).is_empty());
        let mut quiet = beep_hz(440, ms(10));
        quiet.set_volume(0);
        assert!(collect(&mut quiet).iter().all(|&v| v == 0));
    }

    #[test]
    fn too_high_frequencies_are_refused() {
        let low = Rate::new(8000).unwrap();
        let err = beep(Freq::from_hz(4000), ms(10), low).unwrap_err();
        assert_eq!(err, TooHigh { freq: Freq::from_hz(4000), rate: low });
        assert!(beep(Freq::from_hz(3999), ms(10), low).is_ok());
        let m = Melody::parse("C4 w=sine B#8").unwrap();
        let err = Sequencer::new(m, Rate::new(16_000).unwrap()).unwrap_err();
        assert_eq!(err, ParseError { pos: 10, kind: melody::ParseErrorKind::TooHigh });
    }

    #[test]
    fn fill_interleaves_and_pads_with_silence() {
        let mut tone = beep_hz(1000, ms(1)); // 48 samples
        let mut buf = vec![7i16; 2 * 64 + 1];
        assert_eq!(tone.fill(&mut buf, STEREO), Filled { frames: 48, ended: true });
        assert!(buf[..96].chunks(2).all(|f| f[0] == f[1]), "both channels the same");
        assert!(buf[96..128].iter().all(|&v| v == 0), "silence after the end");
        assert_eq!(buf[128], 7, "the partial frame is left alone");
        assert_eq!(tone.fill(&mut buf, STEREO), Filled { frames: 0, ended: true });
        // A buffer that ends exactly with the sound hasn't seen the end yet.
        let mut tone = beep_hz(1000, ms(1));
        assert_eq!(tone.fill(&mut [0; 48], MONO), Filled { frames: 48, ended: false });
        assert_eq!(tone.fill(&mut [0; 48], MONO), Filled { frames: 0, ended: true });
    }

    #[test]
    fn fill_in_periods_matches_sample_by_sample() {
        let whole = collect(&mut beep_hz(440, ms(20)));
        let mut tone = beep_hz(440, ms(20));
        let mut pieces = Vec::new();
        let mut period = [0i16; 100];
        loop {
            let filled = tone.fill(&mut period, MONO);
            pieces.extend_from_slice(&period[..filled.frames]);
            if filled.ended {
                break;
            }
        }
        assert_eq!(pieces, whole);
    }

    #[test]
    #[should_panic(expected = "holds no 2-channel frame")]
    fn fill_refuses_a_buffer_shorter_than_a_frame() {
        beep_hz(440, ms(10)).fill(&mut [0; 1], STEREO);
    }

    #[test]
    fn sources_are_fused() {
        let mut t = beep_hz(440, ms(1));
        collect(&mut t);
        assert!((0..10).all(|_| t.next_sample().is_none()));
        let mut seq = Sequencer::new(Melody::parse("t=999 C/32").unwrap(), RATE).unwrap();
        collect(&mut seq);
        assert!((0..10).all(|_| seq.next_sample().is_none()));
    }

    #[test]
    fn a_melody_lasts_its_duration_and_plays_its_notes_in_order() {
        let seq = Sequencer::new(Melody::parse("t=120 w=sine A4/8 r/8 E5/8 A5/8").unwrap(), RATE).unwrap();
        assert_eq!(seq.duration_samples(), 4 * 12_000);
        assert_eq!(seq.rate(), RATE);
        let s = collect(&mut { seq });
        assert_eq!(s.len(), 4 * 12_000);
        let notes: Vec<&[i16]> = s.chunks(12_000).collect();
        // Each note's first 200 ms, before its release.
        for (note, hz) in [(notes[0], 440.0), (notes[2], 659.255), (notes[3], 880.0)] {
            let got = measured_hz(&note[..9600], RATE.hz());
            assert!(within_1_percent(got, hz), "{hz}: {got}");
        }
        assert!(notes[1].iter().all(|&v| v == 0), "the rest is silent (the note before ended silent)");
    }

    #[test]
    fn tempo_changes_apply_from_where_they_are() {
        let m = Melody::parse("t=240 A4/4 t=60 A4/4").unwrap();
        assert_eq!(m.duration_samples(RATE), 12_000 + 48_000);
        let seq = Sequencer::new(m, RATE).unwrap();
        assert_eq!(seq.duration_samples(), 60_000);
        assert_eq!(collect(&mut { seq }).len(), 60_000);
    }

    #[test]
    fn waveform_changes_apply() {
        // The share of samples near the peak: a sine spends ~20% of its
        // time above 95%, a square nearly all of it.
        let share = |text: &str| {
            let s = play(text);
            let part = &s[7200..40_000];
            let top = u32::from(peak(part)) * 95 / 100;
            part.iter().filter(|v| u32::from(v.unsigned_abs()) > top).count() * 100 / part.len()
        };
        assert!(share("w=sine A4/2") < 40, "{}", share("w=sine A4/2"));
        assert!(share("w=square A4/2") > 90, "{}", share("w=square A4/2"));
    }

    #[test]
    fn volume_scales_the_output() {
        let peak_at = |vol| {
            let mut seq = Sequencer::new(Melody::parse("w=square A4/4").unwrap(), RATE).unwrap();
            seq.set_volume(vol);
            assert_eq!(seq.volume(), vol.min(100));
            peak(&collect(&mut seq))
        };
        assert_eq!(peak_at(0), 0);
        let (full, half) = (peak_at(100), peak_at(50));
        assert!(full > 32_000 && half.abs_diff(full / 2) < 100, "{full} {half}");
        assert_eq!(peak_at(200), full, "volume over 100 is clamped");
        let mut t = beep_hz(440, ms(5));
        assert_eq!(t.volume(), Voice::DEFAULT_VOLUME);
        t.set_volume(101);
        assert_eq!(t.volume(), 100);
    }

    #[test]
    fn stop_fades_out_then_ends() {
        let mut t = beep_hz(440, ms(1000));
        let before: Vec<i16> = (0..1000).map(|_| t.next_sample().unwrap()).collect();
        t.stop();
        let after = collect(&mut t);
        assert!(after.len() <= 240 && !after.is_empty(), "{} samples after stop", after.len());
        assert_eq!(*after.last().unwrap(), 0);
        assert!(peak(&after[..10]) > peak(&before) / 2, "a fade, not a cut");

        let mut seq = Sequencer::new(Melody::parse("C4/2 E4/2 G4/2").unwrap(), RATE).unwrap();
        (0..5000).for_each(|_| {
            seq.next_sample();
        });
        seq.stop();
        let tail = collect(&mut seq);
        assert!(tail.len() <= 1920, "ends within the release: {}", tail.len());
        assert_eq!(*tail.last().unwrap(), 0);
        assert!(seq.next_sample().is_none());
    }

    #[test]
    fn an_owned_sequencer_moves_to_another_thread() {
        let text = String::from("t=240 A4/8 C5/8");
        let mut seq = Sequencer::new(Melody::parse(text).unwrap(), RATE).unwrap();
        let samples = std::thread::spawn(move || collect(&mut seq)).join().unwrap();
        assert_eq!(samples.len(), 2 * 6000);
        assert!(rising_zero_crossings(&samples) > 100);
    }
}
