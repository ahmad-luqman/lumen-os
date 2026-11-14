//! The line editor: keys in, the line out, and in between what the console
//! must show.
//!
//! Keys (both modes, unless marked ANSI only):
//! - a printable character: inserted at the cursor (the line holds up to
//!   its maximum; zero-width characters are refused);
//! - Left/Right, Ctrl-B/Ctrl-F, Home/End, Ctrl-A/Ctrl-E: move the cursor
//!   (ANSI only);
//! - Backspace: delete before the cursor; Delete, Ctrl-D: at it;
//! - Ctrl-K: delete to the end; Ctrl-U: to the start; Ctrl-W: the word
//!   before the cursor (and the blanks after it);
//! - Up/Down, Ctrl-P/Ctrl-N: older and newer lines from the history (going
//!   past the newest brings back the line being typed);
//! - Tab: complete the word before the cursor (see [`Editor::key`]);
//! - Ctrl-C: drop the line; Ctrl-L: clear the screen and show the line
//!   again (ANSI only); Enter: the line is done.
//!
//! In [`Mode::Plain`] the cursor stays at the end of the line, and the
//! console only ever gets characters to append and `BS space BS` to erase
//! the last one: a history line replaces the one typed by erasing it and
//! typing the other. In [`Mode::Ansi`] the editor redraws the line (`CR`,
//! the prompt, the line, `ESC [ K`, and the cursor moved back with
//! `ESC [ n D`) unless the change only appends or only moves the cursor.
//! Neither mode wraps: the console must be wider than the prompt and the
//! longest line.

use crate::{History, Key, words};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use unicode_width::UnicodeWidthChar;

/// How the console shows the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Plain text: appends and erases at the end only, no escape codes.
    Plain,
    /// With ANSI escapes: the cursor can move and the line is redrawn.
    Ansi,
}

/// What can complete a word.
pub trait Complete {
    /// Every word that can follow `words` (the first word of a line when
    /// `words` is empty). The editor keeps those that start with what was
    /// typed.
    fn options(&self, words: &[String], out: &mut Vec<String>);
}

/// What a key ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Enter: the line typed (also kept in the history unless blank or a
    /// repeat).
    Line(String),
    /// Ctrl-C: the line was dropped.
    Cancelled,
}

/// One console's line editor and history.
#[derive(Debug)]
pub struct Editor {
    prompt: &'static str,
    mode: Mode,
    /// The longest line, in characters.
    max: usize,
    line: Vec<char>,
    /// Where the cursor is in `line` (0 to its length).
    cursor: usize,
    /// What the console shows after the prompt, and where its cursor is in
    /// it.
    shown: Vec<char>,
    shown_cursor: usize,
    history: History,
    /// While a history line is shown: which one, and the line that was being
    /// typed before.
    browsing: Option<(usize, Vec<char>)>,
}

impl Editor {
    /// An editor showing `prompt` (no wider than the console less `max`
    /// columns), for lines of up to `max` characters, keeping the last
    /// `history` lines.
    pub fn new(prompt: &'static str, mode: Mode, max: usize, history: usize) -> Editor {
        Editor {
            prompt,
            mode,
            max,
            line: Vec::new(),
            cursor: 0,
            shown: Vec::new(),
            shown_cursor: 0,
            history: History::new(history),
            browsing: None,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    /// The line so far.
    pub fn line(&self) -> String {
        self.line.iter().collect()
    }

    /// Where the cursor is in the line, in characters.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Start a new line: show the prompt.
    pub fn start(&mut self, out: &mut dyn Write) -> fmt::Result {
        self.clear();
        out.write_str(self.prompt)
    }

    /// Take `key` and show what it changed. Tab completes the word before the
    /// cursor from `complete`'s options: one that fits is written in full
    /// (quoted as the word was, the quote closed) followed by a space;
    /// several are written up to where they differ, or listed (sorted, on
    /// a line of their own) if they already differ there.
    pub fn key(&mut self, key: Key, complete: &dyn Complete, out: &mut dyn Write) -> Result<Option<Event>, fmt::Error> {
        let ansi = self.mode == Mode::Ansi;
        match key {
            Key::Char(c) => self.insert(&[c]),
            Key::Enter => {
                let line = self.line();
                self.history.push(&line);
                self.newline(out)?;
                self.clear();
                return Ok(Some(Event::Line(line)));
            }
            Key::Ctrl('c') => {
                out.write_str("^C")?;
                self.newline(out)?;
                self.clear();
                return Ok(Some(Event::Cancelled));
            }
            Key::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.line.remove(self.cursor);
                }
            }
            Key::Delete | Key::Ctrl('d') => {
                if self.cursor < self.line.len() {
                    self.line.remove(self.cursor);
                }
            }
            Key::Left | Key::Ctrl('b') if ansi => self.cursor = self.cursor.saturating_sub(1),
            Key::Right | Key::Ctrl('f') if ansi => self.cursor = (self.cursor + 1).min(self.line.len()),
            Key::Home | Key::Ctrl('a') if ansi => self.cursor = 0,
            Key::End | Key::Ctrl('e') if ansi => self.cursor = self.line.len(),
            Key::Ctrl('k') => self.line.truncate(self.cursor),
            Key::Ctrl('u') => {
                self.line.drain(..self.cursor);
                self.cursor = 0;
            }
            Key::Ctrl('w') => {
                let start = word_start(&self.line, self.cursor);
                self.line.drain(start..self.cursor);
                self.cursor = start;
            }
            Key::Up | Key::Ctrl('p') => self.older(),
            Key::Down | Key::Ctrl('n') => self.newer(),
            Key::Ctrl('l') if ansi => {
                out.write_str("\x1b[H\x1b[2J")?;
                self.show_prompt_again(out)?;
            }
            Key::Tab => self.complete(complete, out)?,
            _ => {}
        }
        self.render(out)?;
        Ok(None)
    }

    fn clear(&mut self) {
        self.line.clear();
        self.cursor = 0;
        self.shown.clear();
        self.shown_cursor = 0;
        self.browsing = None;
    }

    fn newline(&self, out: &mut dyn Write) -> fmt::Result {
        out.write_str(if self.mode == Mode::Ansi { "\r\n" } else { "\n" })
    }

    /// Show the prompt on a new line (the cursor at its start), the line to
    /// be shown after it by [`render`](Self::render).
    fn show_prompt_again(&mut self, out: &mut dyn Write) -> fmt::Result {
        out.write_str(self.prompt)?;
        self.shown.clear();
        self.shown_cursor = 0;
        Ok(())
    }

    /// Insert `chars` at the cursor, if they are all printable and fit.
    fn insert(&mut self, chars: &[char]) {
        let fits = self.line.len() + chars.len() <= self.max;
        if fits && chars.iter().all(|&c| width(c) > 0) {
            self.line.splice(self.cursor..self.cursor, chars.iter().copied());
            self.cursor += chars.len();
        }
    }

    fn older(&mut self) {
        let i = match self.browsing {
            None if !self.history.is_empty() => {
                self.browsing = Some((self.history.len() - 1, self.line.clone()));
                self.history.len() - 1
            }
            Some((i, _)) if i > 0 => i - 1,
            _ => return,
        };
        self.show_history(i);
    }

    fn newer(&mut self) {
        let Some((i, _)) = self.browsing else { return };
        if i + 1 < self.history.len() {
            self.show_history(i + 1);
        } else {
            self.line = self.browsing.take().expect("browsing").1;
            self.cursor = self.line.len();
        }
    }

    fn show_history(&mut self, i: usize) {
        if let Some((at, _)) = &mut self.browsing {
            *at = i;
        }
        self.line = self.history.get(i).expect("a history line").chars().collect();
        self.cursor = self.line.len();
    }

    fn complete(&mut self, complete: &dyn Complete, out: &mut dyn Write) -> fmt::Result {
        let head: String = self.line[..self.cursor].iter().collect();
        let Some(word) = words::partial(&head) else {
            return Ok(());
        };
        let mut options = Vec::new();
        complete.options(&word.words, &mut options);
        options.retain(|o| o.starts_with(word.text.as_str()));
        options.sort();
        options.dedup();
        let (replacement, list) = match options.as_slice() {
            [] => return Ok(()),
            [only] => {
                let mut s = with_quote(only, word.quote);
                s.extend(word.quote);
                s.push(' ');
                (s, false)
            }
            [first, rest @ ..] => {
                let common = rest.iter().fold(first.len(), |n, o| common_len(&first[..n], o));
                if common > word.text.len() {
                    (with_quote(&first[..common], word.quote), false)
                } else {
                    (String::new(), true)
                }
            }
        };
        if list {
            self.newline(out)?;
            for (i, o) in options.iter().enumerate() {
                if i > 0 {
                    out.write_str("  ")?;
                }
                out.write_str(o)?;
            }
            self.newline(out)?;
            return self.show_prompt_again(out);
        }
        // Replace the word as typed with the completed one.
        let start = head[..word.start].chars().count();
        let replacement: Vec<char> = replacement.chars().collect();
        if self.line.len() - (self.cursor - start) + replacement.len() <= self.max {
            self.line.splice(start..self.cursor, replacement.iter().copied());
            self.cursor = start + replacement.len();
        }
        Ok(())
    }

    /// Bring the console up to date with the line and the cursor.
    fn render(&mut self, out: &mut dyn Write) -> fmt::Result {
        match self.mode {
            Mode::Plain => {
                // The cursor is always at the end: erase back to where the
                // line changed, then write the rest.
                let same = self.shown.iter().zip(&self.line).take_while(|(a, b)| a == b).count();
                for &c in self.shown[same..].iter().rev() {
                    let w = width(c);
                    for s in ["\x08", " ", "\x08"] {
                        (0..w).try_for_each(|_| out.write_str(s))?;
                    }
                }
                self.line[same..].iter().try_for_each(|&c| out.write_char(c))?;
            }
            Mode::Ansi if self.shown == self.line => {
                let (from, to) = (columns(&self.line[..self.shown_cursor]), columns(&self.line[..self.cursor]));
                if to < from {
                    write!(out, "\x1b[{}D", from - to)?;
                } else if to > from {
                    write!(out, "\x1b[{}C", to - from)?;
                }
            }
            Mode::Ansi
                if self.shown_cursor == self.shown.len()
                    && self.cursor == self.line.len()
                    && self.line.starts_with(&self.shown) =>
            {
                self.line[self.shown.len()..].iter().try_for_each(|&c| out.write_char(c))?;
            }
            Mode::Ansi => {
                out.write_char('\r')?;
                out.write_str(self.prompt)?;
                self.line.iter().try_for_each(|&c| out.write_char(c))?;
                out.write_str("\x1b[K")?;
                let back = columns(&self.line[self.cursor..]);
                if back > 0 {
                    write!(out, "\x1b[{back}D")?;
                }
            }
        }
        self.shown.clone_from(&self.line);
        self.shown_cursor = self.cursor;
        Ok(())
    }
}

/// `text` written as part of a word (inside `quote` if one is open, which it
/// opens again), quoted to read back as itself.
fn with_quote(text: &str, quote: Option<char>) -> String {
    let mut s: String = quote.into_iter().collect();
    s.push_str(&words::quote(text, quote));
    s
}

/// How many bytes `a` and `b` share at their start, on a character
/// boundary.
fn common_len(a: &str, b: &str) -> usize {
    a.char_indices().zip(b.chars()).find(|((_, x), y)| x != y).map_or(a.len().min(b.len()), |((i, _), _)| i)
}

/// Where the word before `cursor` starts (skipping blanks before the
/// cursor first), for Ctrl-W.
fn word_start(line: &[char], cursor: usize) -> usize {
    let blank = |c: &char| c.is_whitespace();
    let blanks = line[..cursor].iter().rev().take_while(|c| blank(c)).count();
    let word = line[..cursor - blanks].iter().rev().take_while(|c| !blank(c)).count();
    cursor - blanks - word
}

/// The columns a character takes (0 for a control or zero-width one).
fn width(c: char) -> usize {
    c.width().unwrap_or(0)
}

fn columns(chars: &[char]) -> usize {
    chars.iter().map(|&c| width(c)).sum()
}
