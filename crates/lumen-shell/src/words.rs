//! Splitting a command line into words, quoted as in a POSIX shell (and as
//! `shlex` splits them; a test compares the two on random lines):
//!
//! - blanks (space, tab, newline) separate words;
//! - `'...'` keeps everything inside as it is;
//! - `"..."` too, except that a backslash escapes `$`, `` ` ``, `"`, `\`
//!   and a newline (which it removes); before anything else it stays;
//! - outside quotes a backslash escapes the next character (a newline:
//!   removed);
//! - quoted and unquoted parts next to each other make one word
//!   (`a'b c'd` is `ab cd`), and `''` is an empty word;
//! - `#` at the start of a word starts a comment, up to the end of the line.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

/// Why a line couldn't be split.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitError {
    /// A quote (`'` or `"`) wasn't closed.
    UnclosedQuote(char),
    /// The line ends in a backslash that escapes nothing.
    TrailingBackslash,
}

impl fmt::Display for SplitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SplitError::UnclosedQuote(q) => write!(f, "unclosed {q} quote"),
            SplitError::TrailingBackslash => f.write_str("a backslash at the end of the line"),
        }
    }
}

/// The line's words, unquoted.
pub fn split(line: &str) -> Result<Vec<String>, SplitError> {
    let mut scan = scan(line);
    match scan.state {
        State::Blank | State::Comment => {}
        State::Word => scan.words.extend(scan.last.take().map(|w| w.text)),
        State::Single => return Err(SplitError::UnclosedQuote('\'')),
        State::Double | State::DoubleEscape => return Err(SplitError::UnclosedQuote('"')),
        State::Escape => return Err(SplitError::TrailingBackslash),
    }
    Ok(scan.words)
}

/// The word a line ends in, for completing it: the words before it, where
/// it starts (a byte offset) and what it says so far, unquoted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partial {
    /// The words before it.
    pub words: Vec<String>,
    /// Where it starts in the line (its length when the line ends in a
    /// blank: a word not started yet).
    pub start: usize,
    /// What it says so far, unquoted.
    pub text: String,
    /// The quote it is inside, if one is open.
    pub quote: Option<char>,
}

/// The word `line` ends in (see [`Partial`]); `None` inside a comment or
/// right after a backslash, where nothing is completed.
pub fn partial(line: &str) -> Option<Partial> {
    let scan = scan(line);
    let quote = match scan.state {
        State::Blank => {
            return Some(Partial { words: scan.words, start: line.len(), text: String::new(), quote: None });
        }
        State::Word => None,
        State::Single => Some('\''),
        State::Double => Some('"'),
        State::Comment | State::Escape | State::DoubleEscape => return None,
    };
    let last = scan.last.expect("a line ending inside a word has one");
    Some(Partial { words: scan.words, start: last.start, text: last.text, quote })
}

/// `word` written so that it reads back as itself: inside `quote` if one
/// is open (without the closing quote), else with backslashes before the
/// characters that need them (a newline, which a backslash would remove,
/// in single quotes; an empty word as `''`).
pub fn quote(word: &str, quote: Option<char>) -> String {
    if word.is_empty() && quote.is_none() {
        return String::from("''");
    }
    let mut out = String::with_capacity(word.len());
    for c in word.chars() {
        match quote {
            // A single quote can't be escaped inside single quotes: close,
            // escape it, open again.
            Some('\'') if c == '\'' => out.push_str("'\\''"),
            Some('"') if matches!(c, '$' | '`' | '"' | '\\') => {
                out.push('\\');
                out.push(c);
            }
            None if c == '\n' => out.push_str("'\n'"),
            None if matches!(c, ' ' | '\t' | '\'' | '"' | '\\' | '#' | '$' | '`') => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Where the scanner is in the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Between words.
    Blank,
    /// In a comment.
    Comment,
    /// In a word, outside quotes.
    Word,
    /// Right after a backslash outside quotes.
    Escape,
    /// Inside `'...'`.
    Single,
    /// Inside `"..."`.
    Double,
    /// Right after a backslash inside `"..."`.
    DoubleEscape,
}

struct Word {
    start: usize,
    text: String,
}

struct Scan {
    /// The words finished by a blank.
    words: Vec<String>,
    /// The word the line ends in, if it ends inside one.
    last: Option<Word>,
    state: State,
}

/// The text of the word being read.
fn text(word: &mut Option<Word>) -> &mut String {
    &mut word.as_mut().expect("inside a word").text
}

fn scan(line: &str) -> Scan {
    let mut words = Vec::new();
    let mut word: Option<Word> = None;
    let mut state = State::Blank;
    for (i, c) in line.char_indices() {
        state = match state {
            State::Blank => match c {
                ' ' | '\t' | '\n' => State::Blank,
                '#' => State::Comment,
                _ => {
                    word = Some(Word { start: i, text: String::new() });
                    in_word(c, text(&mut word))
                }
            },
            State::Comment if c == '\n' => State::Blank,
            State::Comment => State::Comment,
            State::Word if matches!(c, ' ' | '\t' | '\n') => {
                words.push(word.take().expect("inside a word").text);
                State::Blank
            }
            State::Word => in_word(c, text(&mut word)),
            State::Escape => {
                if c != '\n' {
                    text(&mut word).push(c);
                }
                State::Word
            }
            State::Single if c == '\'' => State::Word,
            State::Single => {
                text(&mut word).push(c);
                State::Single
            }
            State::Double => match c {
                '\\' => State::DoubleEscape,
                '"' => State::Word,
                _ => {
                    text(&mut word).push(c);
                    State::Double
                }
            },
            State::DoubleEscape => {
                let text = text(&mut word);
                match c {
                    '$' | '`' | '"' | '\\' => text.push(c),
                    '\n' => {}
                    _ => {
                        text.push('\\');
                        text.push(c);
                    }
                }
                State::Double
            }
        };
    }
    Scan { words, last: word, state }
}

/// A character in a word outside quotes (not a blank): the state after it.
fn in_word(c: char, text: &mut String) -> State {
    match c {
        '\'' => State::Single,
        '"' => State::Double,
        '\\' => State::Escape,
        _ => {
            text.push(c);
            State::Word
        }
    }
}
