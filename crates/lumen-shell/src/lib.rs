//! The shell's core, without the consoles: a command line in, words and a
//! command to run out.
//!
//! - [`words`]: splitting a line into words the way a POSIX shell quotes
//!   them (`'...'`, `"..."`, backslashes, `#` comments), and into commands
//!   at each `|` outside quotes; the word under the cursor for completion.
//! - [`Editor`]: the line editor. It takes [`Key`]s and writes what the
//!   console must show, either with ANSI escapes ([`Mode::Ansi`]: the
//!   screen, or serial with `lumen.ansi`) or as plain text ([`Mode::Plain`]:
//!   serial, which the test harness reads line by line).
//! - [`History`]: the lines entered, oldest first.
//! - [`Table`]: a command table that completes command names and their
//!   arguments.
//! - [`input::Decoder`]: bytes from a serial terminal (escape sequences for
//!   the arrows, UTF-8) to [`Key`]s.
//! - [`Key::from_press`]: a key press from the keyboard (its text and code)
//!   to the line editor's key, the one mapping text mode and terminal
//!   windows (with #155) share.
//! - [`pipe`]: pipelines (`cpus | where busy > 50`): text or records
//!   ([`pipe::Table`]) handed along, and the filters that take them.
//!
//! The kernel runs one [`Editor`] per console (`kernel/src/shell/`).

#![no_std]

extern crate alloc;

mod editor;
mod history;
pub mod input;
pub mod pipe;
mod table;
pub mod words;

pub use editor::{Complete, Editor, Event, Mode};
pub use history::History;
pub use table::{Command, Table};

use lumen_input::{KeyCode, Mods};

/// A key, as the line editor sees it: typed on the keyboard or decoded from
/// a serial terminal's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A printable character.
    Char(char),
    Enter,
    Tab,
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    /// Ctrl and a letter (`'a'` to `'z'`).
    Ctrl(char),
    /// The console's input ended for good (its terminal window closed,
    /// #155): no key comes after it, and a console that gave it gives it
    /// for every key asked from then on. Never typed: no decoder or
    /// [`Key::from_press`] makes it. Every reader stops on it: the line
    /// editor ends the line with [`Event::Closed`].
    Closed,
}

impl Key {
    /// Whether a command running until a key stops on this one: Ctrl-C, or
    /// [`Key::Closed`] (its console is gone). The one stop set every
    /// command polling for a key uses (`top`, `play`, a script's Ctrl-C
    /// check); the line editor tells the two apart.
    pub const fn interrupts(self) -> bool {
        matches!(self, Key::Ctrl('c') | Key::Closed)
    }

    /// The line editor's key for a key press, if it takes one: from the
    /// press's text if it has one (`pc-keyboard` makes Enter `\n`,
    /// Backspace U+0008, Delete U+007F, Tab `\t`, Escape U+001B and Ctrl
    /// with a letter U+0001 to U+001A, so Ctrl+H, Ctrl+I, Ctrl+J and Ctrl+M
    /// come out as Backspace, Tab, Enter and Enter; a printable character
    /// is itself; Escape and any other control character make nothing),
    /// else from its key code (the arrows, Home, End). The one mapping text mode and terminal windows
    /// (with #155) share (ARCHITECTURE section 7): text mode passes a
    /// `Char(c)` as `(Some(c), None, ..)` and a raw key as `(None,
    /// Some(code), mods)`; a terminal window a key event press's text, code
    /// and modifiers. `mods` are the modifiers with the press (no key takes
    /// them yet).
    pub fn from_press(text: Option<char>, code: Option<KeyCode>, mods: Mods) -> Option<Key> {
        let _ = mods;
        if let Some(c) = text {
            return match c {
                '\n' | '\r' => Some(Key::Enter),
                '\u{8}' => Some(Key::Backspace),
                '\u{7f}' => Some(Key::Delete),
                '\t' => Some(Key::Tab),
                '\u{1}'..='\u{1a}' => Some(Key::Ctrl(char::from(b'a' + c as u8 - 1))),
                c if !c.is_control() => Some(Key::Char(c)),
                _ => None,
            };
        }
        Some(match code? {
            KeyCode::ArrowLeft => Key::Left,
            KeyCode::ArrowRight => Key::Right,
            KeyCode::ArrowUp => Key::Up,
            KeyCode::ArrowDown => Key::Down,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod pipe_tests;
