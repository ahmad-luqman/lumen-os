//! The text editor's logic (#203; `apps/editor` is its window): a text
//! buffer with a cursor, edits that can be undone and redone, and the
//! scroll of a view of so many rows. Host-tested, `no_std` with `alloc`.
//!
//! - **The text** is a `String` of lines separated by `\n`. The cursor is
//!   a byte offset on a char boundary; columns count chars. Control
//!   characters other than `\n` never enter it: [`Command::Insert`] does
//!   nothing with one, Tab inserts [`TAB`], and a text given
//!   ([`Editor::new`], [`Editor::replace_all`]) is [`normalise`]d (a tab as
//!   [`TAB`], `\r\n` and a lone `\r` as `\n`, the others dropped).
//! - **Edits** each replace one range with another text, recorded for
//!   undo with the cursor before and after. A run of characters typed one
//!   after another (no newline, no other command between) is one edit, as
//!   editors undo typing a word at a time; anything else is its own. A new
//!   edit drops what could be redone. Past [`Limits::undo_edits`] edits, or
//!   [`Limits::undo_bytes`] bytes of them (each edit's removed and inserted
//!   text), the oldest are forgotten, so the history's memory is bounded.
//! - **The size** is capped at [`Limits::text_bytes`], and each line at
//!   [`Limits::line_bytes`]: an edit that would pass either is refused
//!   ([`Change::Refused`]), changing nothing.
//! - **The view** shows `rows` lines from `top` ([`Editor::scroll_to`]):
//!   the cursor's line is kept inside it.
//! - **What changed** ([`Change`]) is what a window needs to redraw: the
//!   cursor alone, one line, or every line from one on.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

mod keys;
pub mod view;
pub use keys::command;

/// What Tab inserts: four spaces.
pub const TAB: &str = "    ";

// Spaces only, and some: a tab is never a control character in the text.
const _: () = {
    let bytes = TAB.as_bytes();
    assert!(!bytes.is_empty());
    let mut i = 0;
    while i < bytes.len() {
        assert!(bytes[i] == b' ');
        i += 1;
    }
};

/// `text` as the editor holds a text: a tab as [`TAB`], `\r\n` and a lone
/// `\r` as `\n`, the other control characters dropped (so a file's tabs and
/// line ends survive, M6).
pub fn normalise(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\t' => out.push_str(TAB),
            '\r' => {
                chars.next_if_eq(&'\n');
                out.push('\n');
            }
            '\n' => out.push('\n'),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// What [`Editor::new`] did with the text it was given.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Loaded {
    /// The text didn't fit its limits (its bytes, or a line's) and was
    /// cut where it stopped fitting: the rest is not in the editor.
    pub cut: bool,
}

/// The most rows a view is taken to have ([`Editor::scroll_to`]).
pub const MAX_ROWS: usize = 1 << 20;

/// What an editor holds at most.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The text's bytes.
    pub text_bytes: usize,
    /// A line's bytes (without its `\n`).
    pub line_bytes: usize,
    /// Edits kept to undo.
    pub undo_edits: usize,
    /// Bytes of the edits kept to undo (each one's removed and inserted
    /// text): one bigger than this alone can't be undone.
    pub undo_bytes: usize,
}

/// What the editor is asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Type a character (refused if it is a control character: Enter is
    /// [`Command::Newline`], Tab [`Command::Tab`]).
    Insert(char),
    Newline,
    /// [`TAB`], as one edit.
    Tab,
    /// The character before the cursor (joining lines at a line's start).
    Backspace,
    /// The character after the cursor (joining lines at a line's end).
    Delete,
    Left,
    Right,
    /// A line up or down, at the column the cursor was last moved to
    /// across (or the line's end if shorter).
    Up,
    Down,
    /// The line's start or end.
    Home,
    End,
    /// A view's rows up or down ([`Editor::scroll_to`]'s rows).
    PageUp,
    PageDown,
    /// The text's start or end.
    Top,
    Bottom,
    Undo,
    Redo,
}

/// What a command changed (lines numbered from 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    /// Nothing (a move at an edge, an undo with nothing to undo).
    None,
    /// Nothing: the edit would pass the size cap.
    Refused,
    /// Only the cursor moved, from line `from` to line `to`.
    Cursor { from: usize, to: usize },
    /// The text of one line changed (no line came or went), and the
    /// cursor is on it.
    Line(usize),
    /// Every line from this one on may have changed (lines came or went).
    From(usize),
}

/// One edit: `removed` at `at` replaced by `inserted`, and the cursor
/// before and after. Its invariant: `at <= before <= at + removed.len()`
/// and `after == at + inserted.len()` (an edit is made at the cursor, or
/// replaces the whole text), all on char boundaries, so undoing or redoing
/// it puts the cursor inside what it puts back.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Edit {
    at: usize,
    removed: String,
    inserted: String,
    before: usize,
    after: usize,
}

/// The buffer, its cursor, its history and its view.
#[derive(Clone, Debug)]
pub struct Editor {
    text: String,
    /// Each line's first byte (the first is 0).
    starts: Vec<usize>,
    cursor: usize,
    /// The column Up and Down aim for (chars), set by moving across.
    goal: Option<usize>,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    /// Whether the last edit is a run of typing the next character typed
    /// right after it may join.
    typing: bool,
    limits: Limits,
    /// The text as given (to tell whether it was modified).
    original: String,
    /// Whether the last command or replacement was refused (past a limit).
    refused: bool,
    /// How many edits the history's limits forgot (its tests follow it).
    forgotten: usize,
    top: usize,
    rows: usize,
}

impl Editor {
    /// An editor holding `text` ([`normalise`]d, then cut where it passes
    /// `limits.text_bytes` or a line passes `limits.line_bytes`, on a char
    /// boundary: [`Loaded::cut`] says so), the cursor at its start.
    pub fn new(text: &str, limits: Limits) -> (Editor, Loaded) {
        let mut clean = String::new();
        let mut line = 0;
        let mut cut = false;
        for c in normalise(text).chars() {
            line = if c == '\n' { 0 } else { line + c.len_utf8() };
            if clean.len() + c.len_utf8() > limits.text_bytes || line > limits.line_bytes {
                cut = true;
                break;
            }
            clean.push(c);
        }
        let mut e = Editor {
            text: clean.clone(),
            starts: Vec::new(),
            cursor: 0,
            goal: None,
            undo: Vec::new(),
            redo: Vec::new(),
            typing: false,
            limits,
            original: clean,
            refused: false,
            forgotten: 0,
            top: 0,
            rows: 1,
        };
        e.index();
        (e, Loaded { cut })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the text differs from the one it started with.
    pub fn modified(&self) -> bool {
        self.text != self.original
    }

    /// Whether the last command or replacement was refused: it would have
    /// passed a limit (the text's size, or a line's).
    pub fn full(&self) -> bool {
        self.refused
    }

    /// The status line: `Ln <line>, Col <column>` from 1, then `, modified`
    /// once the text differs from the one it started with, and `, full`
    /// while the last edit was refused.
    pub fn status(&self) -> String {
        let (line, col) = self.line_col();
        let modified = if self.modified() { ", modified" } else { "" };
        let full = if self.refused { ", full" } else { "" };
        alloc::format!("Ln {}, Col {}{modified}{full}", line + 1, col + 1)
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// Line `n` without its `\n` (empty past the last).
    pub fn line(&self, n: usize) -> &str {
        let Some(&start) = self.starts.get(n) else { return "" };
        let end = self.starts.get(n + 1).map_or(self.text.len(), |&s| s - 1);
        &self.text[start..end]
    }

    /// The line the cursor is on.
    pub fn cursor_line(&self) -> usize {
        self.line_of(self.cursor)
    }

    /// The cursor's line and column (chars), from 0.
    pub fn line_col(&self) -> (usize, usize) {
        (self.cursor_line(), self.before_cursor().chars().count())
    }

    /// The cursor's line's text before the cursor.
    pub fn before_cursor(&self) -> &str {
        &self.text[self.starts[self.cursor_line()]..self.cursor]
    }

    /// How many edits can be undone and redone.
    pub fn undo_depth(&self) -> usize {
        self.undo.len()
    }

    pub fn redo_depth(&self) -> usize {
        self.redo.len()
    }

    /// The view's first line.
    pub fn top(&self) -> usize {
        self.top
    }

    /// Show `rows` lines (at least 1), scrolled so the cursor's line is
    /// among them: whether the view's first line moved.
    pub fn scroll_to(&mut self, rows: usize) -> bool {
        // At least one, and few enough that a page's move (`rows` as an
        // `isize`) never overflows.
        self.rows = rows.clamp(1, MAX_ROWS);
        let line = self.cursor_line();
        // The cursor's line in the view, and the view as full as the text
        // allows (lines removed): `lowest <= highest` always.
        let lowest = (line + 1).saturating_sub(self.rows);
        let highest = line.min(self.line_count().saturating_sub(self.rows));
        let top = self.top.min(highest).max(lowest);
        let moved = top != self.top;
        self.top = top;
        moved
    }

    /// Do `cmd`: what changed.
    pub fn apply(&mut self, cmd: Command) -> Change {
        let change = match cmd {
            Command::Insert(c) if c.is_control() => Change::None,
            Command::Insert(c) => {
                let mut buf = [0u8; 4];
                self.edit(self.cursor, self.cursor, c.encode_utf8(&mut buf), true)
            }
            Command::Newline => self.edit(self.cursor, self.cursor, "\n", false),
            Command::Tab => self.edit(self.cursor, self.cursor, TAB, false),
            Command::Backspace => match self.char_before() {
                Some(at) => self.edit(at, self.cursor, "", false),
                None => Change::None,
            },
            Command::Delete => match self.char_after() {
                Some(at) => self.edit(self.cursor, at, "", false),
                None => Change::None,
            },
            Command::Left => self.move_to(self.char_before().unwrap_or(self.cursor)),
            Command::Right => self.move_to(self.char_after().unwrap_or(self.cursor)),
            Command::Up => self.vertical(-1),
            Command::Down => self.vertical(1),
            Command::PageUp => self.vertical(-(self.rows as isize)),
            Command::PageDown => self.vertical(self.rows as isize),
            Command::Home => self.move_to(self.starts[self.cursor_line()]),
            Command::End => {
                let line = self.cursor_line();
                self.move_to(self.starts[line] + self.line(line).len())
            }
            Command::Top => self.move_to(0),
            Command::Bottom => self.move_to(self.text.len()),
            Command::Undo => self.undo(),
            Command::Redo => self.redo(),
        };
        // Only typing right after typing joins it: any other command, a
        // refused or empty one included, ends the run.
        // ...and only while the history still holds the run (the limits
        // may have forgotten it).
        self.typing = matches!((cmd, change), (Command::Insert(_), Change::Line(_))) && !self.undo.is_empty();
        self.refused = change == Change::Refused;
        change
    }

    /// Replace the whole text with `text` ([`normalise`]d) as one edit, the
    /// cursor at its end; refused past a limit, nothing if it is the same.
    pub fn replace_all(&mut self, text: &str) -> Change {
        let clean = normalise(text);
        self.typing = false;
        let change = if clean == self.text {
            Change::None
        } else {
            match self.edit(0, self.text.len(), &clean, false) {
                Change::Refused => Change::Refused,
                _ => Change::From(0),
            }
        };
        self.refused = change == Change::Refused;
        change
    }

    /// The line holding byte `at`.
    fn line_of(&self, at: usize) -> usize {
        match self.starts.binary_search(&at) {
            Ok(n) => n,
            Err(n) => n - 1,
        }
    }

    /// Recompute where each line starts.
    fn index(&mut self) {
        self.starts.clear();
        self.starts.push(0);
        self.starts.extend(self.text.match_indices('\n').map(|(i, _)| i + 1));
    }

    /// Where the character before the cursor starts, if there is one.
    fn char_before(&self) -> Option<usize> {
        self.text[..self.cursor].chars().next_back().map(|c| self.cursor - c.len_utf8())
    }

    /// Where the character after the cursor ends, if there is one.
    fn char_after(&self) -> Option<usize> {
        self.text[self.cursor..].chars().next().map(|c| self.cursor + c.len_utf8())
    }

    /// The cursor moved to `to`, forgetting the goal column.
    fn move_to(&mut self, to: usize) -> Change {
        self.goal = None;
        if to == self.cursor {
            return Change::None;
        }
        let from = self.cursor_line();
        self.cursor = to;
        Change::Cursor { from, to: self.cursor_line() }
    }

    /// `by` lines down (up if negative), at the goal column.
    fn vertical(&mut self, by: isize) -> Change {
        let (line, col) = self.line_col();
        let goal = self.goal.unwrap_or(col);
        let last = self.line_count() - 1;
        let target = line.saturating_add_signed(by).min(last);
        let to = if by < 0 && line == 0 {
            0
        } else if by > 0 && line == last {
            self.text.len()
        } else {
            let text = self.line(target);
            self.starts[target] + text.char_indices().nth(goal).map_or(text.len(), |(i, _)| i)
        };
        let change = self.move_to(to);
        self.goal = Some(goal);
        change
    }

    /// Replace `start..end` with `with` as an edit (`typed`: one character
    /// typed, which may join the run before it).
    fn edit(&mut self, start: usize, end: usize, with: &str, typed: bool) -> Change {
        if self.text.len() - (end - start) + with.len() > self.limits.text_bytes || !self.lines_fit(start, end, with) {
            return Change::Refused;
        }
        let before = self.cursor;
        let removed = String::from(&self.text[start..end]);
        let change = self.replace(start, end - start, with, start + with.len());
        self.redo.clear();
        let joined = typed
            && self.typing
            && self.undo.last().is_some_and(|last| last.removed.is_empty() && last.at + last.inserted.len() == start);
        match self.undo.last_mut() {
            Some(last) if joined => {
                last.inserted.push_str(with);
                last.after = self.cursor;
            }
            _ => self.undo.push(Edit { at: start, removed, inserted: String::from(with), before, after: self.cursor }),
        }
        self.forget_past_limits();
        change
    }

    /// Whether every line `start..end` replaced by `with` would make is at
    /// most [`Limits::line_bytes`].
    fn lines_fit(&self, start: usize, end: usize, with: &str) -> bool {
        let head = start - self.starts[self.line_of(start)];
        let tail = self.text[end..].find('\n').unwrap_or(self.text.len() - end);
        let last = with.split('\n').count() - 1;
        with.split('\n').enumerate().all(|(i, piece)| {
            let len = piece.len() + if i == 0 { head } else { 0 } + if i == last { tail } else { 0 };
            len <= self.limits.line_bytes
        })
    }

    /// The oldest edits forgotten while the history is past its limits.
    fn forget_past_limits(&mut self) {
        let size = |e: &Edit| e.removed.len() + e.inserted.len();
        let mut bytes: usize = self.undo.iter().map(size).sum();
        let mut gone = 0;
        while self.undo.len() - gone > self.limits.undo_edits || bytes > self.limits.undo_bytes {
            bytes -= size(&self.undo[gone]);
            gone += 1;
        }
        self.undo.drain(..gone);
        self.forgotten += gone;
    }

    fn undo(&mut self) -> Change {
        let Some(e) = self.undo.pop() else { return Change::None };
        let change = self.replace(e.at, e.inserted.len(), &e.removed, e.before);
        self.redo.push(e);
        change
    }

    fn redo(&mut self) -> Change {
        let Some(e) = self.redo.pop() else { return Change::None };
        let change = self.replace(e.at, e.removed.len(), &e.inserted, e.after);
        self.undo.push(e);
        change
    }

    /// `len` bytes at `at` replaced by `with`, the cursor then at `cursor`
    /// (an edit's, an undo's and a redo's change to the text).
    /// `cursor` is within what goes in (`at..=at + with.len()`, the
    /// [`Edit`] invariant), so without a line break gone or come it is on
    /// `at`'s line: one line changed, unless the cursor left another.
    fn replace(&mut self, at: usize, len: usize, with: &str, cursor: usize) -> Change {
        debug_assert!(at <= cursor && cursor <= at + with.len(), "{at} {cursor} {}", with.len());
        debug_assert!(self.text.is_char_boundary(at) && self.text.is_char_boundary(at + len));
        let first = self.line_of(at);
        let lines_change = self.text[at..at + len].contains('\n') || with.contains('\n');
        let from = self.cursor_line();
        self.text.replace_range(at..at + len, with);
        self.cursor = cursor;
        self.goal = None;
        self.index();
        if lines_change || from != first { Change::From(first.min(from)) } else { Change::Line(first) }
    }
}

#[cfg(test)]
mod tests;
