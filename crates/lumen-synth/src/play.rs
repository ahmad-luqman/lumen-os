//! Mixing, and the sources a sound driver pulls PCM from: [`beep`]'s
//! [`Tone`] and a melody's [`Sequencer`].

use core::num::NonZeroU8;
use core::time::Duration;

use crate::env::{Adsr, Envelope, FULL_GAIN};
use crate::melody::{self, Melody, ParseError, Token};
use crate::osc::{Oscillator, Waveform};
use crate::{Freq, Rate, TooHigh};

/// The volume tones and melodies start at, in percent: half scale, so two
/// of them mix without clipping.
pub const DEFAULT_VOLUME: u8 = 50;

/// Clamp a mixed sample to 16 bits (never wrap: a wrapped peak is a loud
/// click).
fn saturate(sample: i64) -> i16 {
    sample.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16
}

/// Add samples together, saturating to 16 bits.
pub fn mix(samples: impl IntoIterator<Item = i32>) -> i16 {
    saturate(samples.into_iter().map(i64::from).sum())
}

/// An oscillator shaped by an envelope, at a volume. Its frequency is
/// clamped below half the rate ([`crate::osc::phase_step`]): callers check
/// it with [`Rate::check`] first.
#[derive(Clone, Debug)]
pub(crate) struct Voice {
    osc: Oscillator,
    env: Envelope,
    volume: u8,
}

impl Voice {
    fn new(wave: Waveform, adsr: Adsr, rate: Rate) -> Voice {
        Voice {
            osc: Oscillator::new(wave, Freq::default(), rate),
            env: Envelope::new(adsr, rate),
            volume: DEFAULT_VOLUME,
        }
    }

    fn rate(&self) -> Rate {
        self.osc.rate()
    }

    /// Start playing `freq` (the phase carries on, so no click; 0 Hz is
    /// silent).
    fn note_on(&mut self, freq: Freq) {
        self.osc.set_freq(freq);
        self.env.note_on();
    }

    /// The next sample (within ±32767).
    fn sample(&mut self) -> i16 {
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
    /// True if and only if `frames` is less than the buffer's whole frames:
    /// the sound ended in this buffer (or before it). A fill that ends
    /// exactly at the buffer's end reports `false`, and the next one
    /// `{ frames: 0, ended: true }`.
    pub ended: bool,
}

/// Something that produces mono samples until it ends. Implementations
/// must be fused: after the first `None`, [`next_sample`](Self::next_sample)
/// returns `None` forever ([`fill`](Self::fill)'s `ended` relies on it).
pub trait Source {
    /// The sample rate the source plays at.
    fn rate(&self) -> Rate;

    /// The next sample, or `None` once the sound is over.
    fn next_sample(&mut self) -> Option<i16>;

    /// Fade out now (over at most the release time) and end when silent;
    /// a source that is already silent ends at once.
    fn stop(&mut self);

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
/// tone, and the one tone a [`Sequencer`] replays for each of its notes. A
/// note is released so it is silent by its last sample.
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
    /// edges: it lasts exactly `length` rounded down to whole samples (at
    /// most `u32::MAX` samples, about 24.8 hours at 48 kHz), and fades to 0
    /// by its last sample. 0 Hz is silence; a frequency the rate can't
    /// carry is refused.
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

    /// Play `freq` for `samples`, released in time to be silent at the end:
    /// over the voice's release time, or over the second half of a note
    /// shorter than twice the release time.
    fn play(&mut self, freq: Freq, samples: u32) {
        self.voice.note_on(freq);
        self.left = samples;
        self.release_at = Some(self.voice.env.release_samples().min(samples / 2).max(1));
    }

    /// Rest for `samples` (a sounding note fades at its release rate).
    fn rest(&mut self, samples: u32) {
        self.voice.env.note_off();
        self.left = samples;
        self.release_at = None;
    }

    /// The volume in percent.
    pub fn volume(&self) -> u8 {
        self.voice.volume
    }

    /// Set the volume in percent (0..=100; more is clamped to 100).
    pub fn set_volume(&mut self, percent: u8) {
        self.voice.volume = percent.min(100);
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
            self.voice.env.release_over(self.left);
        }
        self.left -= 1;
        Some(self.voice.sample())
    }

    fn stop(&mut self) {
        if self.voice.env.is_idle() {
            self.left = 0;
        } else if self.left > 0 {
            let n = self.voice.env.release_samples().min(self.left).max(1);
            self.voice.env.release_over(n);
            self.left = n;
            self.release_at = None;
        }
    }
}

/// A melody being played. It keeps the melody's text as `S` (see
/// [`Melody`]), so with an owned string it can move to another thread.
#[derive(Clone, Debug)]
pub struct Sequencer<S> {
    melody: Melody<S>,
    /// The byte offset of the next word (the text's end once stopped).
    pos: usize,
    tempo: u16,
    current: Tone,
}

impl<S: AsRef<str>> Sequencer<S> {
    /// Play `melody` at `rate` in [`Adsr::PLUCK`] (`w=` changes only the
    /// waveform, never the envelope), starting in the default waveform. It
    /// lasts exactly [`duration_samples`](Self::duration_samples) (at most
    /// `u32::MAX` samples). A melody with a note `rate` can't carry is
    /// refused ([`Melody::check`]).
    pub fn new(melody: Melody<S>, rate: Rate) -> Result<Sequencer<S>, ParseError> {
        melody.check(rate)?;
        Ok(Sequencer {
            melody,
            pos: 0,
            tempo: melody::DEFAULT_TEMPO,
            current: Tone::idle(Voice::new(Waveform::default(), Adsr::PLUCK, rate)),
        })
    }

    /// How many samples the whole melody lasts at the sequencer's rate.
    pub fn duration_samples(&self) -> u32 {
        self.melody.duration_samples(self.rate())
    }

    /// The volume in percent.
    pub fn volume(&self) -> u8 {
        self.current.volume()
    }

    /// Set the volume in percent (0..=100; more is clamped to 100).
    pub fn set_volume(&mut self, percent: u8) {
        self.current.set_volume(percent);
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
            let rate = self.rate();
            match melody::next_token(self.melody.text(), &mut self.pos)? {
                Token::Note(pitch, len) => self.current.play(pitch.freq(), len.samples(self.tempo, rate)),
                Token::Rest(len) => self.current.rest(len.samples(self.tempo, rate)),
                Token::Tempo(t) => self.tempo = t,
                Token::Wave(w) => self.current.voice.osc.set_wave(w),
            }
        }
    }

    /// Fade out the note playing (a rest ends at once) and play no more.
    fn stop(&mut self) {
        self.pos = self.melody.text().len();
        self.current.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::{measured_hz, peak, within_1_percent};
    use std::string::String;
    use std::vec;
    use std::vec::Vec;

    const RATE: Rate = Rate::DEFAULT;
    const STEREO: NonZeroU8 = NonZeroU8::new(2).unwrap();
    const MONO: NonZeroU8 = NonZeroU8::MIN;

    fn collect(source: &mut impl Source) -> Vec<i16> {
        core::iter::from_fn(|| source.next_sample()).collect()
    }

    fn skip(source: &mut (impl Source + ?Sized), n: usize) {
        (0..n).for_each(|_| {
            source.next_sample();
        });
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn beep_hz(hz: u32, length: Duration) -> Tone {
        beep(Freq::from_hz(hz), length, RATE).unwrap()
    }

    fn sequencer(text: &str) -> Sequencer<&str> {
        Sequencer::new(Melody::parse(text).unwrap(), RATE).unwrap()
    }

    fn play(text: &str) -> Vec<i16> {
        collect(&mut sequencer(text))
    }

    /// The share of samples (percent) above 95% of the peak: a sine spends
    /// ~20% of its time there, a square nearly all of it.
    fn near_peak_share(part: &[i16]) -> usize {
        let top = u32::from(peak(part)) * 95 / 100;
        part.iter().filter(|v| u32::from(v.unsigned_abs()) > top).count() * 100 / part.len()
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
        // The end is a fade over the release (240 samples), not a cut.
        let tail = &s[s.len() - 240..];
        assert!(peak(&tail[..24]) > peak(&s) / 2, "the fade starts loud");
        assert!(peak(&tail[216..]) < peak(&s) / 4, "and ends quiet");
    }

    #[test]
    fn a_note_release_is_linear() {
        let s = collect(&mut Tone::new(Waveform::Sine, Freq::from_hz(1000), ms(250), RATE).unwrap());
        let tail = &s[s.len() - 240..];
        let (first, mid) = (peak(&tail[..48]), peak(&tail[96..144]));
        assert!(mid > first / 4 && mid < first * 3 / 4, "{first} {mid}");
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
        // One sample: released at once, so silent.
        assert_eq!(collect(&mut beep(Freq::from_hz(440), Duration::from_nanos(20_834), RATE).unwrap()), [0]);
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
        let rate = Rate::new(16_000).unwrap();
        let err = Sequencer::new(m, rate).unwrap_err();
        let too_high = TooHigh { freq: Freq::from_millihz(8_372_032), rate };
        assert_eq!(err, ParseError { pos: 10, kind: melody::ParseErrorKind::TooHigh(too_high) });
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
        let mut seq = sequencer("t=999 C/32");
        collect(&mut seq);
        assert!((0..10).all(|_| seq.next_sample().is_none()));
    }

    #[test]
    fn the_default_tempo_is_120() {
        assert_eq!(Melody::parse("A4/4").unwrap().duration_samples(RATE), 24_000);
        assert_eq!(play("A4/4").len(), 24_000);
    }

    #[test]
    fn a_melody_lasts_its_duration_and_plays_its_notes_in_order() {
        let mut seq = sequencer("t=120 w=sine A4/8 r/8 E5/8 A5/8");
        assert_eq!(seq.duration_samples(), 4 * 12_000);
        assert_eq!(seq.rate(), RATE);
        let s = collect(&mut seq);
        assert_eq!(s.len(), 4 * 12_000);
        let notes: Vec<&[i16]> = s.chunks(12_000).collect();
        // Each note's first 200 ms, before its release.
        for (note, hz) in [(notes[0], 440.0), (notes[2], 659.255), (notes[3], 880.0)] {
            let got = measured_hz(&note[..9600], RATE.hz());
            assert!(within_1_percent(got, hz), "{hz}: {got}");
        }
        assert!(notes[1].iter().all(|&v| v == 0), "the rest is silent (the note before ended silent)");
        // A note's end is a fade over its release (1920 samples).
        let tail = &notes[0][12_000 - 1920..];
        assert!(peak(&tail[..100]) > peak(notes[0]) / 3, "the note fades");
    }

    #[test]
    fn tempo_changes_apply_from_where_they_are() {
        let m = Melody::parse("t=240 A4/4 t=60 A4/4").unwrap();
        assert_eq!(m.duration_samples(RATE), 12_000 + 48_000);
        let mut seq = Sequencer::new(m, RATE).unwrap();
        assert_eq!(seq.duration_samples(), 60_000);
        assert_eq!(collect(&mut seq).len(), 60_000);
    }

    #[test]
    fn waveform_changes_apply() {
        assert!(near_peak_share(&play("w=sine A4/2")[7200..40_000]) < 40);
        assert!(near_peak_share(&play("w=square A4/2")[7200..40_000]) > 90);
        // In the middle of a melody too.
        let s = play("w=sine A4/2 w=square A4/2");
        assert!(near_peak_share(&s[7200..40_000]) < 40);
        assert!(near_peak_share(&s[48_000 + 7200..48_000 + 40_000]) > 90);
    }

    #[test]
    fn volume_scales_the_output() {
        let peak_at = |vol| {
            let mut seq = sequencer("w=square A4/4");
            seq.set_volume(vol);
            assert_eq!(seq.volume(), vol.min(100));
            peak(&collect(&mut seq))
        };
        assert_eq!(peak_at(0), 0);
        let (full, half) = (peak_at(100), peak_at(50));
        assert!(full > 32_000 && half.abs_diff(full / 2) < 100, "{full} {half}");
        assert_eq!(peak_at(200), full, "volume over 100 is clamped");
        let mut t = beep_hz(440, ms(5));
        assert_eq!(t.volume(), DEFAULT_VOLUME);
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

        let mut seq = sequencer("C4/2 E4/2 G4/2");
        let before: Vec<i16> = (0..5000).map(|_| seq.next_sample().unwrap()).collect();
        seq.stop();
        let tail = collect(&mut seq);
        assert!(tail.len() <= 1920 && tail.len() > 100, "ends within the release: {}", tail.len());
        assert_eq!(*tail.last().unwrap(), 0);
        assert!(peak(&tail[..100]) > peak(&before[4000..]) / 2, "a fade, not a cut");
        assert!(seq.next_sample().is_none());
    }

    #[test]
    fn stop_never_lengthens_and_ends_a_rest_at_once() {
        let mut t = beep_hz(440, ms(1000));
        skip(&mut t, 47_900);
        t.stop();
        assert!(collect(&mut t).len() <= 100);

        let mut seq = sequencer("t=240 C4/8 r/1 C4/8");
        skip(&mut seq, 10_000); // well into the rest
        seq.stop();
        assert_eq!(seq.next_sample(), None);
    }

    #[test]
    fn a_boxed_source_can_be_stopped() {
        let mut src: std::boxed::Box<dyn Source + Send> = std::boxed::Box::new(beep_hz(440, ms(1000)));
        skip(&mut *src, 100);
        src.stop();
        let mut n = 0;
        while src.next_sample().is_some() {
            n += 1;
        }
        assert!(n <= 240);
    }

    /// A text that changes after a few reads (a broken `AsRef`).
    struct Shifty(core::cell::Cell<u32>, &'static str);

    impl AsRef<str> for Shifty {
        fn as_ref(&self) -> &str {
            self.0.set(self.0.get() + 1);
            if self.0.get() <= 3 { "t=999 A4/32 A4/32 A4/32" } else { self.1 }
        }
    }

    #[test]
    fn a_text_that_changes_ends_the_melody_without_panicking() {
        for later in ["t=999 A4/32 x", "éééééééééééé", "", "t=999 A4/32 A4/32 A4/32 A4/32"] {
            let m = Melody::parse(Shifty(core::cell::Cell::new(0), later)).unwrap();
            let mut seq = Sequencer::new(m, RATE).unwrap();
            let n = collect(&mut seq).len();
            assert!(n <= 4 * 360, "{later:?}: {n}");
            assert!(seq.next_sample().is_none());
        }
    }

    #[test]
    fn an_owned_sequencer_moves_to_another_thread() {
        let text = "t=240 A4/8 C5/8";
        let mut seq = Sequencer::new(Melody::parse(String::from(text)).unwrap(), RATE).unwrap();
        let there = std::thread::spawn(move || collect(&mut seq)).join().unwrap();
        assert_eq!(there, play(text));
    }
}
