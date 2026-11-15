//! The melody notation `play` takes: words separated by ASCII whitespace.
//!
//! | Word | Meaning |
//! |---|---|
//! | `C#4/8.` | a note: a letter `A`–`G` (either case), then optionally `#` (sharp) or `b` (flat, lowercase only), the octave `0`–`8` (default 4), `/` and the length as a fraction of a whole note: 1, 2, 4, 8, 16 or 32 (default 4, a quarter), and `.` for a dotted note (half as long again) |
//! | `r/4` | a rest (`r` or `R`), optionally with a length and dot like a note's |
//! | `t=140` | the tempo from here on, in quarter notes a minute (20–999; default 120) |
//! | `w=sine` | the waveform from here on: `square` (default), `triangle`, `sine`, `noise` |
//!
//! Settings and waveform names are lowercase, and numbers are plain digits
//! without a sign or leading zeros: each number has one spelling, and the
//! canonical text [`Token`]'s `Display` prints parses back to the same
//! tokens.
//! [`Melody::parse`] checks the whole text first, so playing it never meets
//! a bad word halfway. Octave 4 starts at middle C, A4 is 440 Hz, and
//! accidentals may cross octaves (`Cb4` is B3, `B#3` is C4).

use core::fmt;

use crate::osc::Waveform;
use crate::{Freq, Rate};

/// The tempo before any `t=`, in quarter notes a minute.
pub const DEFAULT_TEMPO: u16 = 120;

/// The slowest and fastest tempo `t=` accepts.
pub const TEMPO_RANGE: core::ops::RangeInclusive<u16> = 20..=999;

/// A note's letter, `C` to `B`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Letter {
    C,
    D,
    E,
    F,
    G,
    A,
    B,
}

impl Letter {
    fn from_byte(b: u8) -> Option<Letter> {
        Some(match b.to_ascii_uppercase() {
            b'C' => Letter::C,
            b'D' => Letter::D,
            b'E' => Letter::E,
            b'F' => Letter::F,
            b'G' => Letter::G,
            b'A' => Letter::A,
            b'B' => Letter::B,
            _ => return None,
        })
    }

    /// Semitones above C in the same octave.
    const fn semitone(self) -> i32 {
        match self {
            Letter::C => 0,
            Letter::D => 2,
            Letter::E => 4,
            Letter::F => 5,
            Letter::G => 7,
            Letter::A => 9,
            Letter::B => 11,
        }
    }

    const fn name(self) -> char {
        match self {
            Letter::C => 'C',
            Letter::D => 'D',
            Letter::E => 'E',
            Letter::F => 'F',
            Letter::G => 'G',
            Letter::A => 'A',
            Letter::B => 'B',
        }
    }
}

/// Sharp, flat or neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Accidental {
    Flat,
    Natural,
    Sharp,
}

/// A pitch: letter, accidental and octave, from C0 (`Cb0` is refused) to
/// B#8 (C9).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pitch {
    letter: Letter,
    accidental: Accidental,
    octave: u8,
}

/// The highest octave a pitch is written in.
pub const MAX_OCTAVE: u8 = 8;

/// Octave 4's notes from C, in millihertz (equal temperament, A4 = 440 Hz).
const OCTAVE_4: [u32; 12] =
    [261_626, 277_183, 293_665, 311_127, 329_628, 349_228, 369_994, 391_995, 415_305, 440_000, 466_164, 493_883];

impl Pitch {
    /// The pitch, or `None` for an octave above [`MAX_OCTAVE`] or a note
    /// below C0.
    pub fn new(letter: Letter, accidental: Accidental, octave: u8) -> Option<Pitch> {
        let pitch = Pitch { letter, accidental, octave };
        (octave <= MAX_OCTAVE && pitch.semitones() >= 0).then_some(pitch)
    }

    pub fn letter(self) -> Letter {
        self.letter
    }

    pub fn accidental(self) -> Accidental {
        self.accidental
    }

    /// The octave as written (the accidental may move the sounding note an
    /// octave).
    pub fn octave(self) -> u8 {
        self.octave
    }

    /// Semitones above C0 (0..=108 for any pitch [`Pitch::new`] accepts).
    fn semitones(self) -> i32 {
        let accidental = match self.accidental {
            Accidental::Flat => -1,
            Accidental::Natural => 0,
            Accidental::Sharp => 1,
        };
        i32::from(self.octave) * 12 + self.letter.semitone() + accidental
    }

    /// The pitch's frequency (C0 is 16.352 Hz, C9 8372.032 Hz).
    pub fn freq(self) -> Freq {
        // 0..=108 by construction, so the octave is 0..=9 and the shift
        // can't overflow.
        let n = self.semitones().clamp(0, 108) as u32;
        let (octave, step) = (n / 12, (n % 12) as usize);
        let base = OCTAVE_4[step];
        Freq::from_millihz(if octave >= 4 {
            base << (octave - 4)
        } else {
            let shift = 4 - octave;
            (base + (1 << (shift - 1))) >> shift
        })
    }
}

impl fmt::Display for Pitch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let accidental = match self.accidental {
            Accidental::Flat => "b",
            Accidental::Natural => "",
            Accidental::Sharp => "#",
        };
        write!(f, "{}{accidental}{}", self.letter.name(), self.octave)
    }
}

/// A note or rest's length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Length {
    denominator: u8,
    dotted: bool,
}

impl Length {
    /// A quarter note, the default.
    pub const QUARTER: Length = Length { denominator: 4, dotted: false };

    /// `1/denominator` of a whole note (half as long again if `dotted`), or
    /// `None` unless the denominator is 1, 2, 4, 8, 16 or 32.
    pub fn new(denominator: u8, dotted: bool) -> Option<Length> {
        matches!(denominator, 1 | 2 | 4 | 8 | 16 | 32).then_some(Length { denominator, dotted })
    }

    pub fn denominator(self) -> u8 {
        self.denominator
    }

    pub fn dotted(self) -> bool {
        self.dotted
    }

    /// How many samples the length lasts at `tempo` quarter notes a minute
    /// (0 counts as 1) and `rate` (rounded down).
    pub fn samples(self, tempo: u16, rate: Rate) -> u32 {
        // A whole note is 4 beats = 240 / tempo seconds.
        let (num, den) = if self.dotted { (3, 2) } else { (1, 1) };
        let n = u64::from(rate.hz()) * 240 * num / (u64::from(tempo.max(1)) * u64::from(self.denominator) * den);
        u32::try_from(n).unwrap_or(u32::MAX)
    }
}

impl fmt::Display for Length {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "/{}{}", self.denominator, if self.dotted { "." } else { "" })
    }
}

/// One word of a melody.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Token {
    Note(Pitch, Length),
    Rest(Length),
    /// `t=`: quarter notes a minute from here on.
    Tempo(u16),
    /// `w=`: the waveform from here on.
    Wave(Waveform),
}

/// Words print in a canonical form ([`Melody::parse`] reads it back
/// unchanged): every note with its octave and length.
impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Token::Note(pitch, len) => write!(f, "{pitch}{len}"),
            Token::Rest(len) => write!(f, "r{len}"),
            Token::Tempo(t) => write!(f, "t={t}"),
            Token::Wave(w) => write!(f, "w={w}"),
        }
    }
}

/// What is wrong with a melody.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParseErrorKind {
    /// No notes or rests at all.
    Empty,
    /// A word without `=` that starts with something other than a note
    /// letter or `r`.
    UnknownNote(char),
    /// An octave that isn't a single digit 0-8 (`C9`, `C04`, `C#10`), or
    /// a note below C0 (`Cb0`).
    BadOctave,
    /// A length other than 1, 2, 4, 8, 16 or 32 after `/`.
    BadLength,
    /// A tempo outside 20..=999 or not plain digits.
    BadTempo,
    /// A waveform name that doesn't exist.
    BadWave,
    /// An `x=` setting other than `t=` or `w=`.
    UnknownSetting,
    /// A character left over after a complete word (`C4x`).
    Unexpected(char),
    /// A note at or above half the sample rate ([`Melody::check`]), with
    /// its frequency and the rate.
    TooHigh(crate::TooHigh),
}

/// A melody error and where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ParseError {
    /// The byte offset in the melody's text of the character at fault
    /// (a char boundary), or where it should be when it is missing: the
    /// text's length for no notes at all ([`ParseErrorKind::Empty`]), just
    /// past `t=`, `w=` or `/` when nothing follows.
    pub pos: usize,
    pub kind: ParseErrorKind,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pos = self.pos;
        match self.kind {
            ParseErrorKind::Empty => write!(f, "no notes to play"),
            ParseErrorKind::UnknownNote(c) => write!(f, "{c:?} at {pos} is not a note (A-G), rest (r), t= or w="),
            ParseErrorKind::BadOctave => write!(f, "octave at {pos} is out of range (0-8, C0 lowest)"),
            ParseErrorKind::BadLength => write!(f, "length at {pos} must be 1, 2, 4, 8, 16 or 32"),
            ParseErrorKind::BadTempo => write!(f, "tempo at {pos} must be 20-999"),
            ParseErrorKind::BadWave => write!(f, "waveform at {pos} must be square, triangle, sine or noise"),
            ParseErrorKind::UnknownSetting => write!(f, "setting at {pos} is not t= (tempo) or w= (waveform)"),
            ParseErrorKind::Unexpected(c) => write!(f, "unexpected {c:?} at {pos}"),
            ParseErrorKind::TooHigh(e) => write!(f, "note at {pos}: {e}"),
        }
    }
}

impl core::error::Error for ParseError {}

/// A checked melody, holding its text as any `S: AsRef<str>`: a `&str`
/// to play it in place, or an owned string to send it to another thread.
/// `S::as_ref` must return the same text every time (as `String`, `&str`,
/// `Box<str>` and `Arc<str>` do): a text that changes after parsing ends
/// the melody at the first word that no longer parses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Melody<S> {
    text: S,
}

impl<S: AsRef<str>> Melody<S> {
    /// Check `text`: every word must parse and there must be at least one
    /// note or rest.
    pub fn parse(text: S) -> Result<Melody<S>, ParseError> {
        let s = text.as_ref();
        let mut pos = 0;
        let mut sounds = false;
        while let Some((_, token)) = next_word(s, &mut pos) {
            sounds |= matches!(token?, Token::Note(..) | Token::Rest(_));
        }
        if !sounds {
            return Err(ParseError { pos: s.len(), kind: ParseErrorKind::Empty });
        }
        Ok(Melody { text })
    }

    /// The melody's text.
    pub fn text(&self) -> &str {
        self.text.as_ref()
    }

    /// The melody's words, in order.
    pub fn tokens(&self) -> Tokens<'_> {
        Tokens { text: self.text(), pos: 0 }
    }

    /// `Ok` if `rate` carries every note ([`Rate::carries`]); otherwise the
    /// first note that is too high.
    pub fn check(&self, rate: Rate) -> Result<(), ParseError> {
        let mut pos = 0;
        while let Some((start, token)) = next_word(self.text(), &mut pos) {
            if let Ok(Token::Note(pitch, _)) = token {
                rate.check(pitch.freq()).map_err(|e| ParseError { pos: start, kind: ParseErrorKind::TooHigh(e) })?;
            }
        }
        Ok(())
    }

    /// How many samples the melody lasts at `rate` (each length rounded
    /// down to whole samples, as [`crate::Sequencer`] plays it).
    pub fn duration_samples(&self, rate: Rate) -> u32 {
        let mut tempo = DEFAULT_TEMPO;
        let mut total = 0u32;
        for token in self.tokens() {
            match token {
                Token::Note(_, len) | Token::Rest(len) => total = total.saturating_add(len.samples(tempo, rate)),
                Token::Tempo(t) => tempo = t,
                Token::Wave(_) => {}
            }
        }
        total
    }
}

/// A checked melody's words ([`Melody::tokens`]).
#[derive(Clone, Debug)]
pub struct Tokens<'a> {
    text: &'a str,
    pos: usize,
}

impl Iterator for Tokens<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Token> {
        next_token(self.text, &mut self.pos)
    }
}

/// The checked word at byte `*pos` of `text` (a melody's), advancing `*pos`
/// past it. A word that doesn't parse (only if `S::as_ref` returned a
/// different text, see [`Melody`]) ends the melody.
pub(crate) fn next_token(text: &str, pos: &mut usize) -> Option<Token> {
    match next_word(text, pos)? {
        (_, Ok(token)) => Some(token),
        (_, Err(_)) => {
            *pos = text.len();
            None
        }
    }
}

/// The word at or after byte `*pos` of `text` and where it starts,
/// advancing `*pos` past it.
fn next_word(text: &str, pos: &mut usize) -> Option<(usize, Result<Token, ParseError>)> {
    let bytes = text.as_bytes();
    while bytes.get(*pos).is_some_and(u8::is_ascii_whitespace) {
        *pos += 1;
    }
    // (`*pos` off a char boundary only if the text changed under us.)
    if *pos >= bytes.len() || !text.is_char_boundary(*pos) {
        return None;
    }
    let start = *pos;
    let end = bytes[start..].iter().position(u8::is_ascii_whitespace).map_or(bytes.len(), |n| start + n);
    *pos = end;
    let result = parse_word(&text[start..end]).map_err(|(at, kind)| ParseError { pos: start + at, kind });
    Some((start, result))
}

/// Plain decimal digits without a sign or leading zero (and short enough
/// for a u32).
fn decimal(s: &str) -> Option<u32> {
    let plain = !s.is_empty() && s.len() <= 9 && s.bytes().all(|b| b.is_ascii_digit());
    if !plain || (s.len() > 1 && s.starts_with('0')) {
        return None;
    }
    s.parse().ok()
}

/// The character at byte `at` of `word` (a char boundary).
fn char_at(word: &str, at: usize) -> char {
    word[at..].chars().next().unwrap_or(' ')
}

/// One word (non-empty, no whitespace); errors carry the offset in it.
fn parse_word(word: &str) -> Result<Token, (usize, ParseErrorKind)> {
    use ParseErrorKind::*;
    let bytes = word.as_bytes();
    if let Some(eq) = word.find('=') {
        let (key, value, at) = (&word[..eq], &word[eq + 1..], eq + 1);
        return match key {
            "t" => decimal(value)
                .and_then(|t| u16::try_from(t).ok())
                .filter(|t| TEMPO_RANGE.contains(t))
                .map(Token::Tempo)
                .ok_or((at, BadTempo)),
            "w" => Waveform::from_name(value).map(Token::Wave).ok_or((at, BadWave)),
            _ => Err((0, UnknownSetting)),
        };
    }
    let mut i = 1;
    let token = if bytes[0].eq_ignore_ascii_case(&b'r') {
        Token::Rest(parse_length(word, &mut i)?)
    } else if let Some(letter) = Letter::from_byte(bytes[0]) {
        let accidental = match bytes.get(i) {
            Some(b'#') => Accidental::Sharp,
            Some(b'b') => Accidental::Flat,
            _ => Accidental::Natural,
        };
        if accidental != Accidental::Natural {
            i += 1;
        }
        let digits = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
        let octave = match &bytes[i..i + digits] {
            [] => 4,
            [d @ b'0'..=b'8'] => d - b'0',
            _ => return Err((i, BadOctave)),
        };
        i += digits;
        let pitch = Pitch::new(letter, accidental, octave).ok_or((0, BadOctave))?;
        Token::Note(pitch, parse_length(word, &mut i)?)
    } else {
        return Err((0, UnknownNote(char_at(word, 0))));
    };
    match bytes.get(i) {
        None => Ok(token),
        Some(_) => Err((i, Unexpected(char_at(word, i)))),
    }
}

/// An optional `/n` and `.` at `word[*i..]`; advances `*i` past them.
fn parse_length(word: &str, i: &mut usize) -> Result<Length, (usize, ParseErrorKind)> {
    let bytes = word.as_bytes();
    let mut denominator = 4;
    if bytes.get(*i) == Some(&b'/') {
        let at = *i + 1;
        let digits = bytes[at..].iter().take_while(|b| b.is_ascii_digit()).count();
        denominator = decimal(&word[at..at + digits])
            .and_then(|n| u8::try_from(n).ok())
            .filter(|&n| Length::new(n, false).is_some())
            .ok_or((at, ParseErrorKind::BadLength))?;
        *i = at + digits;
    }
    let dotted = bytes.get(*i) == Some(&b'.');
    if dotted {
        *i += 1;
    }
    Ok(Length { denominator, dotted })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::{String, ToString};
    use std::vec::Vec;

    const RATE: Rate = Rate::DEFAULT;

    fn tokens(text: &str) -> Vec<Token> {
        Melody::parse(text).unwrap().tokens().collect()
    }

    fn note(text: &str) -> (Pitch, Length) {
        match tokens(text)[..] {
            [Token::Note(p, l)] => (p, l),
            ref other => panic!("{text}: {other:?}"),
        }
    }

    fn pitch(letter: Letter, accidental: Accidental, octave: u8) -> Pitch {
        Pitch::new(letter, accidental, octave).unwrap()
    }

    fn canonical(tokens: &[Token]) -> String {
        tokens.iter().map(ToString::to_string).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn parses_notes_rests_and_settings() {
        let t = tokens("t=140 w=sine C4/8 Eb5/16. r/2 g#");
        assert_eq!(
            t,
            [
                Token::Tempo(140),
                Token::Wave(Waveform::Sine),
                Token::Note(pitch(Letter::C, Accidental::Natural, 4), Length::new(8, false).unwrap()),
                Token::Note(pitch(Letter::E, Accidental::Flat, 5), Length::new(16, true).unwrap()),
                Token::Rest(Length::new(2, false).unwrap()),
                Token::Note(pitch(Letter::G, Accidental::Sharp, 4), Length::QUARTER),
            ]
        );
        assert_eq!(tokens("r/4"), [Token::Rest(Length::QUARTER)], "a rest alone is a melody");
    }

    #[test]
    fn letters_and_rests_ignore_case_and_extra_spaces() {
        assert_eq!(tokens("  a4\tR  bb3\n"), tokens("A4 r Bb3"));
        // A lowercase `b` is the note B; after a letter it is a flat.
        assert_eq!(note("b").0.letter(), Letter::B);
        assert_eq!(note("bb").0, pitch(Letter::B, Accidental::Flat, 4));
    }

    #[test]
    fn frequencies_follow_equal_temperament() {
        let mhz = |t: &str| note(t).0.freq().millihz();
        assert_eq!(mhz("A4"), 440_000);
        assert_eq!(mhz("A5"), 880_000);
        assert_eq!(mhz("A3"), 220_000);
        assert_eq!(mhz("A0"), 27_500);
        assert_eq!(mhz("C4"), 261_626);
        assert_eq!(mhz("C#4"), mhz("Db4"));
        assert_eq!(mhz("Cb4"), mhz("B3"));
        assert_eq!(mhz("B#3"), mhz("C4"));
        assert_eq!(mhz("B#8"), 261_626 << 5, "C9");
        assert_eq!(mhz("C0"), (261_626 + 8) >> 4);
        // Every semitone is 2^(1/12) above the last, within 0.01%.
        let names = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
        let all: Vec<f64> =
            (1..8).flat_map(|o| names.iter().map(move |n| f64::from(mhz(&std::format!("{n}{o}"))))).collect();
        for w in all.windows(2) {
            let ratio = w[1] / w[0];
            assert!((ratio - 2f64.powf(1.0 / 12.0)).abs() < 1e-4, "{w:?}");
        }
    }

    #[test]
    fn pitches_and_lengths_are_only_built_in_range() {
        assert!(Pitch::new(Letter::C, Accidental::Natural, 9).is_none());
        assert!(Pitch::new(Letter::C, Accidental::Flat, 0).is_none());
        assert!(Pitch::new(Letter::B, Accidental::Sharp, 8).is_some());
        // Every constructible pitch has a frequency, without overflow.
        for octave in 0..=255u8 {
            for letter in [Letter::C, Letter::D, Letter::E, Letter::F, Letter::G, Letter::A, Letter::B] {
                for acc in [Accidental::Flat, Accidental::Natural, Accidental::Sharp] {
                    if let Some(p) = Pitch::new(letter, acc, octave) {
                        assert!((16_000..8_400_000).contains(&p.freq().millihz()), "{p}");
                    }
                }
            }
        }
        for d in 0..=255u8 {
            assert_eq!(Length::new(d, false).is_some(), [1, 2, 4, 8, 16, 32].contains(&d), "{d}");
        }
    }

    #[test]
    fn lengths_follow_the_tempo() {
        let ms = |len: Length, tempo| len.samples(tempo, Rate::new(1000).unwrap());
        assert_eq!(ms(Length::QUARTER, 120), 500);
        assert_eq!(ms(Length::QUARTER, 60), 1000);
        assert_eq!(ms(Length::new(1, false).unwrap(), 120), 2000);
        assert_eq!(ms(Length::new(8, true).unwrap(), 120), 375);
        assert_eq!(Length::new(32, false).unwrap().samples(999, RATE), 360);
        let m = Melody::parse("C4 t=60 C4/2. r/8").unwrap();
        assert_eq!(m.duration_samples(RATE), 4 * 48_000);
    }

    #[test]
    fn check_names_the_first_note_too_high_for_the_rate() {
        let m = Melody::parse("w=sine C4 B#8 C8").unwrap();
        assert_eq!(m.check(RATE), Ok(()));
        let low = Rate::new(16_000).unwrap(); // carries up to 8 kHz; B#8 is 8372 Hz
        let b_sharp_8 = Freq::from_millihz(8_372_032);
        let too_high = |rate| ParseErrorKind::TooHigh(crate::TooHigh { freq: b_sharp_8, rate });
        assert_eq!(m.check(low), Err(ParseError { pos: 10, kind: too_high(low) }));
        let lower = Rate::new(8000).unwrap();
        assert_eq!(m.check(lower), Err(ParseError { pos: 10, kind: too_high(lower) }));
        assert_eq!(
            m.check(lower).unwrap_err().to_string(),
            "note at 10: 8372.032 Hz is too high for a 8000 Hz sample rate (it must be below half)"
        );
        assert_eq!(Melody::parse("C8").unwrap().check(lower).unwrap_err().pos, 0);
    }

    #[test]
    fn canonical_text_round_trips() {
        let text = "t=90 w=triangle c d#5/8 Gb3/16. r R/32. w=noise A/1 B#8/2 t=999 cb1";
        let first = tokens(text);
        let canon = canonical(&first);
        assert_eq!(canon, "t=90 w=triangle C4/4 D#5/8 Gb3/16. r/4 r/32. w=noise A4/1 B#8/2 t=999 Cb1/4");
        assert_eq!(tokens(&canon), first);
    }

    #[test]
    fn rejects_bad_words_with_their_position() {
        use ParseErrorKind::*;
        let cases: &[(&str, usize, ParseErrorKind)] = &[
            ("", 0, Empty),
            ("   ", 3, Empty),
            ("t=100 w=sine", 12, Empty),
            ("C4 H4", 3, UnknownNote('H')),
            ("C4 é4", 3, UnknownNote('é')),
            ("C9", 1, BadOctave),
            ("C#10", 2, BadOctave),
            ("C04", 1, BadOctave),
            ("A Cb0", 2, BadOctave),
            ("C4/3", 3, BadLength),
            ("C4/", 3, BadLength),
            ("r/64", 2, BadLength),
            ("C4/300", 3, BadLength),
            ("C/08", 2, BadLength),
            ("C/+8", 2, BadLength),
            ("t=0 C", 2, BadTempo),
            ("t=19 C", 2, BadTempo),
            ("t=1000 C", 2, BadTempo),
            ("C t=fast", 4, BadTempo),
            ("t=+140 C", 2, BadTempo),
            ("t=0140 C", 2, BadTempo),
            ("t= C", 2, BadTempo),
            ("w=saw C", 2, BadWave),
            ("w=Sine C", 2, BadWave),
            ("C x=1", 2, UnknownSetting),
            ("C T=100", 2, UnknownSetting),
            ("C4x", 2, Unexpected('x')),
            ("C4/8..", 5, Unexpected('.')),
            ("C##4", 2, Unexpected('#')),
            ("CB4", 1, Unexpected('B')),
            ("r4", 1, Unexpected('4')),
            ("C4é", 2, Unexpected('é')),
        ];
        for &(text, pos, kind) in cases {
            assert_eq!(Melody::parse(text), Err(ParseError { pos, kind }), "{text:?}");
        }
        assert!(Melody::parse("t=20 C t=999 C").is_ok(), "the tempo range's edges");
    }

    #[test]
    fn errors_print_their_position() {
        let e = Melody::parse("C4 H4").unwrap_err();
        assert_eq!(e.to_string(), "'H' at 3 is not a note (A-G), rest (r), t= or w=");
        assert!(Melody::parse("C4/3").unwrap_err().to_string().contains("1, 2, 4, 8, 16 or 32"));
    }

    #[test]
    fn owned_text_parses_like_borrowed() {
        let owned = Melody::parse(String::from("A4 r/8")).unwrap();
        assert_eq!(owned.tokens().collect::<Vec<_>>(), tokens("A4 r/8"));
        assert_eq!(owned.text(), "A4 r/8");
    }

    /// Random words from the notation's alphabet: parsing never panics,
    /// accepted text round-trips through its canonical form, and every
    /// error points at a char boundary.
    #[test]
    fn fuzzed_words_never_panic_and_round_trip() {
        const ALPHABET: [&str; 32] = [
            "A", "C", "G", "b", "e", "r", "R", "H", "#", "b", ".", "/", "=", "t", "w", "0", "1", "2", "4", "8", "9",
            "3", "6", "é", " ", "\t", "+", "sine", "noise", "x", "t=", "w=",
        ];
        let mut state = 0xACE1u32;
        let mut next = || {
            // xorshift32
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        let mut accepted = 0;
        for _ in 0..100_000 {
            let len = 1 + next() % 8;
            let text: String = (0..len).map(|_| ALPHABET[(next() % 32) as usize]).collect();
            match Melody::parse(text.as_str()) {
                Ok(m) => {
                    accepted += 1;
                    let first: Vec<Token> = m.tokens().collect();
                    let canon = canonical(&first);
                    let again: Vec<Token> = Melody::parse(canon.as_str()).unwrap().tokens().collect();
                    assert_eq!(again, first, "{text:?} -> {canon:?}");
                }
                Err(e) => {
                    assert!(e.pos <= text.len() && text.is_char_boundary(e.pos), "{text:?}: {e:?}");
                }
            }
        }
        assert!(accepted > 1000, "the generator should hit valid melodies too: {accepted}");
    }
}
