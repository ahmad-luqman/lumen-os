//! The melody notation `play` takes: words separated by spaces.
//!
//! | Word | Meaning |
//! |---|---|
//! | `C4/8` | a note: letter `A`–`G` (either case), then `#` (sharp) or `b` (flat), then the octave `0`–`8` (default 4), then `/` and the length as a fraction of a whole note: 1, 2, 4, 8, 16 or 32 (default 4, a quarter), then `.` for a dotted note (half as long again) |
//! | `r/4` | a rest (`r` or `R`), with a length like a note's |
//! | `t=140` | the tempo from here on, in quarter notes a minute (20–999; default 120) |
//! | `w=sine` | the waveform from here on: `square` (default), `triangle`, `sine`, `noise` |
//!
//! [`Melody::parse`] checks the whole text first, so playing it never meets
//! an error halfway; errors carry the byte offset of the offending
//! character. Octave 4 starts at middle C, A4 is 440 Hz, and accidentals
//! may cross octaves (`Cb4` is B3, `B#3` is C4).

use core::fmt;

use crate::Freq;
use crate::osc::Waveform;

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

/// A pitch: letter, accidental and octave.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pitch {
    pub letter: Letter,
    pub accidental: Accidental,
    /// 0..=8 as written (the accidental may move the sounding note an
    /// octave).
    pub octave: u8,
}

/// Octave 4's notes from C, in millihertz (equal temperament, A4 = 440 Hz).
const OCTAVE_4: [u32; 12] =
    [261_626, 277_183, 293_665, 311_127, 329_628, 349_228, 369_994, 391_995, 415_305, 440_000, 466_164, 493_883];

impl Pitch {
    /// Semitones above C0, or `None` below it (`Cb0`).
    fn semitones(self) -> Option<u32> {
        let accidental = match self.accidental {
            Accidental::Flat => -1,
            Accidental::Natural => 0,
            Accidental::Sharp => 1,
        };
        u32::try_from(i32::from(self.octave) * 12 + self.letter.semitone() + accidental).ok()
    }

    /// The pitch's frequency.
    pub fn freq(self) -> Freq {
        // Pitches come from the parser, which refuses the one below C0.
        let n = self.semitones().unwrap_or(0);
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
    /// The fraction of a whole note: 1, 2, 4, 8, 16 or 32.
    pub denominator: u8,
    /// Half as long again.
    pub dotted: bool,
}

impl Length {
    /// A quarter note, the default.
    pub const QUARTER: Length = Length { denominator: 4, dotted: false };

    /// How many samples the length lasts at `tempo` quarter notes a minute
    /// and `rate` samples a second (rounded down).
    pub fn samples(self, tempo: u16, rate: u32) -> u32 {
        // A whole note is 4 beats = 240 / tempo seconds.
        let (num, den) = if self.dotted { (3, 2) } else { (1, 1) };
        let n = u64::from(rate) * 240 * num / (u64::from(tempo.max(1)) * u64::from(self.denominator.max(1)) * den);
        u32::try_from(n).unwrap_or(u32::MAX)
    }

    /// How many milliseconds the length lasts at `tempo` (rounded down).
    pub fn ms(self, tempo: u16) -> u32 {
        self.samples(tempo, 1000)
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
    /// A word that starts with something other than a note letter, `r` or a
    /// known `x=` setting.
    UnknownNote(char),
    /// An octave above 8, or a note below C0 (`Cb0`).
    BadOctave,
    /// A length other than 1, 2, 4, 8, 16 or 32 after `/`.
    BadLength,
    /// A tempo outside 20..=999 or not a number.
    BadTempo,
    /// A waveform name that doesn't exist.
    BadWave,
    /// An `x=` setting other than `t=` or `w=`.
    UnknownSetting,
    /// A character left over after a complete word (`C4x`).
    Unexpected(char),
}

/// A melody error and where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ParseError {
    /// The byte offset in the melody's text of the character at fault.
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
        }
    }
}

impl core::error::Error for ParseError {}

/// A checked melody.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Melody<'a> {
    text: &'a str,
}

impl<'a> Melody<'a> {
    /// Check `text`: every word must parse and there must be at least one
    /// note or rest.
    pub fn parse(text: &'a str) -> Result<Melody<'a>, ParseError> {
        let mut words = Words { text, pos: 0 };
        let mut sounds = false;
        while let Some(token) = words.next_word() {
            sounds |= matches!(token?, Token::Note(..) | Token::Rest(_));
        }
        if !sounds {
            return Err(ParseError { pos: text.len(), kind: ParseErrorKind::Empty });
        }
        Ok(Melody { text })
    }

    /// The melody's words, in order.
    pub fn tokens(&self) -> Tokens<'a> {
        Tokens(Words { text: self.text, pos: 0 })
    }

    /// How long the melody plays, in milliseconds (each length rounded down
    /// to whole samples at `rate`, as [`crate::Sequencer`] plays it).
    pub fn duration_ms(&self, rate: u32) -> u64 {
        u64::from(self.duration_samples(rate)) * 1000 / u64::from(rate.max(1))
    }

    /// How many samples the melody lasts at `rate`.
    pub fn duration_samples(&self, rate: u32) -> u32 {
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
pub struct Tokens<'a>(Words<'a>);

impl Iterator for Tokens<'_> {
    type Item = Token;

    fn next(&mut self) -> Option<Token> {
        // The melody was checked when it was parsed, so no word fails.
        self.0.next_word().and_then(Result::ok)
    }
}

/// The word parser.
#[derive(Clone, Debug)]
struct Words<'a> {
    text: &'a str,
    pos: usize,
}

impl Words<'_> {
    fn next_word(&mut self) -> Option<Result<Token, ParseError>> {
        let bytes = self.text.as_bytes();
        while bytes.get(self.pos).is_some_and(u8::is_ascii_whitespace) {
            self.pos += 1;
        }
        if self.pos >= bytes.len() {
            return None;
        }
        let start = self.pos;
        let end = bytes[start..].iter().position(u8::is_ascii_whitespace).map_or(bytes.len(), |n| start + n);
        self.pos = end;
        Some(parse_word(self.text, start, end))
    }
}

fn error(pos: usize, kind: ParseErrorKind) -> ParseError {
    ParseError { pos, kind }
}

/// The character at byte `pos` of `text` (a char boundary).
fn char_at(text: &str, pos: usize) -> char {
    text[pos..].chars().next().unwrap_or(' ')
}

fn parse_word(text: &str, start: usize, end: usize) -> Result<Token, ParseError> {
    let word = &text[start..end];
    let bytes = word.as_bytes();
    if let Some(eq) = word.find('=') {
        let (key, value) = (&word[..eq], &word[eq + 1..]);
        let at = start + eq + 1;
        return match key {
            "t" => value
                .parse::<u16>()
                .ok()
                .filter(|t| TEMPO_RANGE.contains(t))
                .map(Token::Tempo)
                .ok_or(error(at, ParseErrorKind::BadTempo)),
            "w" => Waveform::from_name(value).map(Token::Wave).ok_or(error(at, ParseErrorKind::BadWave)),
            _ => Err(error(start, ParseErrorKind::UnknownSetting)),
        };
    }
    let mut i = 1;
    let token = if bytes[0].eq_ignore_ascii_case(&b'r') {
        Token::Rest(parse_length(text, start, bytes, &mut i)?)
    } else if let Some(letter) = Letter::from_byte(bytes[0]) {
        let accidental = match bytes.get(i) {
            Some(b'#') => Accidental::Sharp,
            Some(b'b') => Accidental::Flat,
            _ => Accidental::Natural,
        };
        if accidental != Accidental::Natural {
            i += 1;
        }
        let octave = match bytes.get(i) {
            Some(d @ b'0'..=b'8') => {
                i += 1;
                d - b'0'
            }
            Some(b'9') => return Err(error(start + i, ParseErrorKind::BadOctave)),
            _ => 4,
        };
        if bytes.get(i).is_some_and(u8::is_ascii_digit) {
            // A two-digit octave.
            return Err(error(start + i - 1, ParseErrorKind::BadOctave));
        }
        let pitch = Pitch { letter, accidental, octave };
        if pitch.semitones().is_none() {
            return Err(error(start, ParseErrorKind::BadOctave));
        }
        Token::Note(pitch, parse_length(text, start, bytes, &mut i)?)
    } else {
        return Err(error(start, ParseErrorKind::UnknownNote(char_at(text, start))));
    };
    match bytes.get(i) {
        None => Ok(token),
        Some(_) => Err(error(start + i, ParseErrorKind::Unexpected(char_at(text, start + i)))),
    }
}

/// An optional `/n` and `.` at `bytes[*i..]` (the word starts at `start`
/// in `text`); advances `*i` past them.
fn parse_length(text: &str, start: usize, bytes: &[u8], i: &mut usize) -> Result<Length, ParseError> {
    let mut len = Length::QUARTER;
    if bytes.get(*i) == Some(&b'/') {
        let digits = bytes[*i + 1..].iter().take_while(|b| b.is_ascii_digit()).count();
        let number = &text[start + *i + 1..start + *i + 1 + digits];
        len.denominator = match number.parse::<u8>() {
            Ok(n @ (1 | 2 | 4 | 8 | 16 | 32)) => n,
            _ => return Err(error(start + *i + 1, ParseErrorKind::BadLength)),
        };
        *i += 1 + digits;
    }
    if bytes.get(*i) == Some(&b'.') {
        len.dotted = true;
        *i += 1;
    }
    Ok(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::{String, ToString};
    use std::vec::Vec;

    fn tokens(text: &str) -> Vec<Token> {
        Melody::parse(text).unwrap().tokens().collect()
    }

    fn note(text: &str) -> (Pitch, Length) {
        match tokens(text)[..] {
            [Token::Note(p, l)] => (p, l),
            ref other => panic!("{text}: {other:?}"),
        }
    }

    fn err(text: &str) -> ParseError {
        Melody::parse(text).unwrap_err()
    }

    #[test]
    fn parses_notes_rests_and_settings() {
        let t = tokens("t=140 w=sine C4/8 Eb5/16. r/2 g#");
        assert_eq!(t.len(), 6);
        assert_eq!(t[0], Token::Tempo(140));
        assert_eq!(t[1], Token::Wave(Waveform::Sine));
        assert_eq!(
            t[2],
            Token::Note(
                Pitch { letter: Letter::C, accidental: Accidental::Natural, octave: 4 },
                Length { denominator: 8, dotted: false }
            )
        );
        assert_eq!(
            t[3],
            Token::Note(
                Pitch { letter: Letter::E, accidental: Accidental::Flat, octave: 5 },
                Length { denominator: 16, dotted: true }
            )
        );
        assert_eq!(t[4], Token::Rest(Length { denominator: 2, dotted: false }));
        assert_eq!(
            t[5],
            Token::Note(Pitch { letter: Letter::G, accidental: Accidental::Sharp, octave: 4 }, Length::QUARTER)
        );
    }

    #[test]
    fn letters_and_rests_ignore_case_and_extra_spaces() {
        assert_eq!(tokens("  a4\tR  bb3\n"), tokens("A4 r Bb3"));
        // A lowercase `b` is the note B; after a letter it is a flat.
        assert_eq!(note("b").0.letter, Letter::B);
        assert_eq!(note("bb").0, Pitch { letter: Letter::B, accidental: Accidental::Flat, octave: 4 });
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
    fn lengths_follow_the_tempo() {
        let quarter = Length::QUARTER;
        assert_eq!(quarter.ms(120), 500);
        assert_eq!(quarter.ms(60), 1000);
        assert_eq!(Length { denominator: 1, dotted: false }.ms(120), 2000);
        assert_eq!(Length { denominator: 8, dotted: true }.ms(120), 375);
        assert_eq!(Length { denominator: 32, dotted: false }.samples(999, 48_000), 360);
        let m = Melody::parse("C4 t=60 C4/2. r/8").unwrap();
        assert_eq!(m.duration_ms(48_000), 500 + 3000 + 500);
        assert_eq!(m.duration_samples(48_000), 4 * 48_000);
    }

    #[test]
    fn canonical_text_round_trips() {
        let text = "t=90 w=triangle c d#5/8 Gb3/16. r R/32. w=noise A/1 B#8/2 t=999 cb1";
        let first = tokens(text);
        let canonical: String = first.iter().map(ToString::to_string).collect::<Vec<_>>().join(" ");
        assert_eq!(canonical, "t=90 w=triangle C4/4 D#5/8 Gb3/16. r/4 r/32. w=noise A4/1 B#8/2 t=999 Cb1/4");
        assert_eq!(tokens(&canonical), first);
    }

    #[test]
    fn rejects_bad_words_with_their_position() {
        use ParseErrorKind::*;
        assert_eq!(err(""), ParseError { pos: 0, kind: Empty });
        assert_eq!(err("   "), ParseError { pos: 3, kind: Empty });
        assert_eq!(err("t=100 w=sine"), ParseError { pos: 12, kind: Empty });
        assert_eq!(err("C4 H4"), ParseError { pos: 3, kind: UnknownNote('H') });
        assert_eq!(err("C4 é4"), ParseError { pos: 3, kind: UnknownNote('é') });
        assert_eq!(err("C9"), ParseError { pos: 1, kind: BadOctave });
        assert_eq!(err("C#10"), ParseError { pos: 2, kind: BadOctave });
        assert_eq!(err("A Cb0"), ParseError { pos: 2, kind: BadOctave });
        assert_eq!(err("C4/3"), ParseError { pos: 3, kind: BadLength });
        assert_eq!(err("C4/"), ParseError { pos: 3, kind: BadLength });
        assert_eq!(err("r/64"), ParseError { pos: 2, kind: BadLength });
        assert_eq!(err("C4/300"), ParseError { pos: 3, kind: BadLength });
        assert_eq!(err("t=0 C"), ParseError { pos: 2, kind: BadTempo });
        assert_eq!(err("t=1000 C"), ParseError { pos: 2, kind: BadTempo });
        assert_eq!(err("C t=fast"), ParseError { pos: 4, kind: BadTempo });
        assert_eq!(err("w=saw C"), ParseError { pos: 2, kind: BadWave });
        assert_eq!(err("C x=1"), ParseError { pos: 2, kind: UnknownSetting });
        assert_eq!(err("C4x"), ParseError { pos: 2, kind: Unexpected('x') });
        assert_eq!(err("C4/8.."), ParseError { pos: 5, kind: Unexpected('.') });
        assert_eq!(err("C##4"), ParseError { pos: 2, kind: Unexpected('#') });
        assert_eq!(err("r4"), ParseError { pos: 1, kind: Unexpected('4') });
    }

    #[test]
    fn errors_print_their_position() {
        let e = err("C4 H4");
        assert_eq!(e.to_string(), "'H' at 3 is not a note (A-G), rest (r), t= or w=");
        assert!(err("C4/3").to_string().contains("1, 2, 4, 8, 16 or 32"));
    }
}
