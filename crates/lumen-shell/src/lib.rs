//! The shell's core, without the consoles: a command line in, words and a
//! command to run out.
//!
//! - [`words`]: splitting a line into words the way a POSIX shell quotes
//!   them (`'...'`, `"..."`, backslashes, `#` comments), and the word under
//!   the cursor for completion.
//! - [`Editor`]: the line editor. It takes [`Key`]s and writes what the
//!   console must show, either with ANSI escapes ([`Mode::Ansi`]: the
//!   screen, or serial with `lumen.ansi`) or as plain text ([`Mode::Plain`]:
//!   serial, which the test harness reads line by line).
//! - [`History`]: the lines entered, oldest first.
//! - [`Table`]: a command table that completes command names and their
//!   arguments.
//! - [`input::Decoder`]: bytes from a serial terminal (escape sequences for
//!   the arrows, UTF-8) to [`Key`]s.
//!
//! The kernel runs one [`Editor`] per console (`kernel/src/shell/`).

#![no_std]

extern crate alloc;

mod editor;
mod history;
pub mod input;
mod table;
pub mod words;

pub use editor::{Complete, Editor, Event, Mode};
pub use history::History;
pub use table::{Command, Table};

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
}

#[cfg(test)]
mod tests;
