//! ADSR envelopes: how a note's loudness rises when it starts (attack),
//! falls to a holding level (decay, sustain) and dies away when it is
//! released (release). Levels are linear, in Q24 inside and Q16 out.

use crate::samples_in_ms;

/// Full level inside the envelope (Q24).
const UNITY: u32 = 1 << 24;

/// The gain [`Envelope::gain`] returns at full level (Q16).
pub const FULL_GAIN: u32 = 1 << 16;

/// An envelope's shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Adsr {
    /// Time from silence to full level, in milliseconds (0: start at full).
    pub attack_ms: u32,
    /// Time from full level down to the sustain level, in milliseconds.
    pub decay_ms: u32,
    /// The level held while the note sounds, in percent of full (0..=100;
    /// more counts as 100).
    pub sustain: u8,
    /// Time from full level to silence after the note is released, in
    /// milliseconds (from a lower level it is proportionally shorter).
    pub release_ms: u32,
}

impl Adsr {
    /// Short ramps that only avoid clicks: 2 ms attack, full sustain, 5 ms
    /// release.
    pub const CLICK_FREE: Adsr = Adsr { attack_ms: 2, decay_ms: 0, sustain: 100, release_ms: 5 };

    /// A plucked, piano-like shape for melodies.
    pub const PLUCK: Adsr = Adsr { attack_ms: 5, decay_ms: 120, sustain: 60, release_ms: 40 };
}

impl Default for Adsr {
    fn default() -> Adsr {
        Adsr::PLUCK
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

/// An envelope running at one sample rate.
#[derive(Clone, Debug)]
pub struct Envelope {
    stage: Stage,
    level: u32,
    attack_step: u32,
    decay_step: u32,
    sustain_level: u32,
    release_step: u32,
    release_samples: u32,
}

/// The per-sample step that covers `span` in `ms` milliseconds (rounded up,
/// so the span is covered in exactly that many samples); all of it at once
/// when that is less than one sample.
fn step(span: u32, ms: u32, rate: u32) -> u32 {
    match samples_in_ms(ms, rate) {
        0 => span.max(1),
        n => span.div_ceil(n).max(1),
    }
}

impl Envelope {
    /// An idle (silent) envelope of shape `adsr` at `rate` samples a second.
    pub fn new(adsr: Adsr, rate: u32) -> Envelope {
        let sustain_level = (u64::from(UNITY) * u64::from(adsr.sustain.min(100)) / 100) as u32;
        Envelope {
            stage: Stage::Idle,
            level: 0,
            attack_step: step(UNITY, adsr.attack_ms, rate),
            decay_step: step(UNITY - sustain_level, adsr.decay_ms, rate),
            sustain_level,
            release_step: step(UNITY, adsr.release_ms, rate),
            release_samples: samples_in_ms(adsr.release_ms, rate),
        }
    }

    /// Start (or restart) a note: the attack rises from the current level,
    /// so retriggering a sounding note doesn't click.
    pub fn note_on(&mut self) {
        self.stage = Stage::Attack;
    }

    /// Release the note: the level falls from where it is to silence.
    pub fn note_off(&mut self) {
        if self.stage != Stage::Idle {
            self.stage = Stage::Release;
        }
    }

    /// Whether the envelope is silent and stays so until the next
    /// [`note_on`](Self::note_on).
    pub fn is_idle(&self) -> bool {
        self.stage == Stage::Idle
    }

    /// The number of samples a release from full level takes.
    pub fn release_samples(&self) -> u32 {
        self.release_samples
    }

    /// Advance one sample and return the gain for it, 0..=[`FULL_GAIN`].
    pub fn gain(&mut self) -> u32 {
        match self.stage {
            Stage::Idle | Stage::Sustain => {}
            Stage::Attack => {
                self.level = self.level.saturating_add(self.attack_step);
                if self.level >= UNITY {
                    self.level = UNITY;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                self.level = self.level.saturating_sub(self.decay_step).max(self.sustain_level);
                if self.level == self.sustain_level {
                    self.stage = Stage::Sustain;
                }
            }
            Stage::Release => {
                self.level = self.level.saturating_sub(self.release_step);
                if self.level == 0 {
                    self.stage = Stage::Idle;
                }
            }
        }
        self.level >> 8
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    // 1000 samples a second: one sample per millisecond.
    const RATE: u32 = 1000;

    fn run(env: &mut Envelope, n: usize) -> Vec<u32> {
        (0..n).map(|_| env.gain()).collect()
    }

    #[test]
    fn attack_rises_linearly_to_full_in_its_time() {
        let mut env = Envelope::new(Adsr { attack_ms: 10, decay_ms: 0, sustain: 100, release_ms: 0 }, RATE);
        assert_eq!(env.gain(), 0, "idle until note_on");
        env.note_on();
        let g = run(&mut env, 12);
        assert_eq!(g[9], FULL_GAIN, "full after exactly 10 samples: {g:?}");
        assert!(g[8] < FULL_GAIN);
        let diffs: Vec<u32> = g[..9].windows(2).map(|w| w[1] - w[0]).collect();
        assert!(diffs.iter().all(|&d| d.abs_diff(diffs[0]) <= 1), "linear: {diffs:?}");
        assert!(g[10..].iter().all(|&x| x == FULL_GAIN), "holds at full sustain");
    }

    #[test]
    fn decay_falls_to_sustain_and_holds() {
        let mut env = Envelope::new(Adsr { attack_ms: 0, decay_ms: 20, sustain: 50, release_ms: 10 }, RATE);
        env.note_on();
        let g = run(&mut env, 100);
        assert_eq!(g[0], FULL_GAIN, "zero attack jumps to full");
        let half = FULL_GAIN / 2;
        assert_eq!(g[20], half, "sustain reached after the decay: {:?}", &g[..25]);
        assert!(g[1..20].windows(2).all(|w| w[1] < w[0]), "decay strictly falls");
        assert!(g[20..].iter().all(|&x| x == half), "sustain holds");
    }

    #[test]
    fn release_falls_to_silence_then_idles() {
        let mut env = Envelope::new(Adsr { attack_ms: 0, decay_ms: 0, sustain: 100, release_ms: 10 }, RATE);
        env.note_on();
        run(&mut env, 5);
        env.note_off();
        let g = run(&mut env, 12);
        assert_eq!(g[9], 0, "silent after the release time: {g:?}");
        assert!(g[..9].windows(2).all(|w| w[1] < w[0]));
        assert!(env.is_idle());
        assert_eq!(env.release_samples(), 10);
    }

    #[test]
    fn release_during_the_attack_starts_from_the_current_level() {
        let mut env = Envelope::new(Adsr { attack_ms: 100, decay_ms: 0, sustain: 100, release_ms: 10 }, RATE);
        env.note_on();
        let reached = *run(&mut env, 50).last().unwrap();
        env.note_off();
        let next = env.gain();
        assert!(next < reached && reached - next < FULL_GAIN / 8, "{reached} -> {next}: no jump");
        // Half the level takes about half the full release time.
        let rest = run(&mut env, 10);
        let silent_at = rest.iter().position(|&g| g == 0).unwrap();
        assert!((3..=4).contains(&silent_at), "{rest:?}");
    }

    #[test]
    fn retriggering_rises_from_the_current_level() {
        let mut env = Envelope::new(Adsr { attack_ms: 10, decay_ms: 0, sustain: 100, release_ms: 100 }, RATE);
        env.note_on();
        run(&mut env, 20);
        env.note_off();
        let low = *run(&mut env, 30).last().unwrap();
        env.note_on();
        let up = env.gain();
        assert!(up > low && up - low <= FULL_GAIN / 9, "{low} -> {up}");
    }

    #[test]
    fn note_off_while_idle_stays_idle() {
        let mut env = Envelope::new(Adsr::PLUCK, RATE);
        env.note_off();
        assert!(env.is_idle());
        assert_eq!(env.gain(), 0);
    }

    #[test]
    fn sustain_over_100_counts_as_full() {
        let mut env = Envelope::new(Adsr { attack_ms: 0, decay_ms: 5, sustain: 255, release_ms: 0 }, RATE);
        env.note_on();
        assert!(run(&mut env, 10).iter().all(|&g| g == FULL_GAIN));
    }
}
