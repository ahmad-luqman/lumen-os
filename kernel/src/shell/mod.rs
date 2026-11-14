//! The shell: one per console, each with its own line and history.
//!
//! - The serial shell reads serial and answers on serial, in plain text
//!   (appends and `BS space BS` erases only, never an escape code: the test
//!   harness reads serial line by line) unless the command line has
//!   `lumen.ansi`, which gives it the screen shell's editing. It runs on the
//!   boot CPU's `main` thread (kmain's context), polling serial (no serial
//!   interrupt yet, #61) and yielding to the CPU's other threads between
//!   polls.
//! - The screen shell reads the PS/2 keyboard and answers on the screen's
//!   terminal, with ANSI line editing. It runs in its own thread
//!   (`shell-screen`, on the boot CPU), blocked until a key arrives; it
//!   starts only with both a terminal and a keyboard. Shift+PageUp and
//!   Shift+PageDown scroll the screen back and forward by half a screen;
//!   any key the line editor takes shows it again.
//!
//! Log lines go to both consoles, between the shells' output. The line
//! editor, the word splitting and completion are `lumen-shell`'s (tested on
//! the host); the commands are in [`commands`].

#![cfg_attr(feature = "ktest", allow(dead_code))] // the test kernel runs no shell

mod commands;
mod crash;

use crate::arch::serial;
use crate::keyboard::{self, KeyCode};
use alloc::string::String;
use core::fmt::{self, Write};
use lumen_shell::input::Decoder;
use lumen_shell::{Editor, Event, History, Key, Mode};

const PROMPT: &str = "lumen> ";
/// The longest line, in characters (less on a console too narrow for it).
const MAX_LINE: usize = 160;
/// Lines each shell keeps in its history.
const HISTORY: usize = 100;
/// How wide a serial terminal is taken to be with `lumen.ansi` (lines stay
/// shorter than it: the editor doesn't wrap).
const SERIAL_COLUMNS: usize = 80;

/// What a command can use: its console and the shell's history.
pub struct Ctx<'a> {
    pub out: &'a mut dyn Write,
    /// Whether the console takes escape codes.
    pub ansi: bool,
    pub history: &'a History,
}

/// A console a shell runs on.
trait Console: Write {
    /// Wait for the next key.
    fn read_key(&mut self) -> Key;
}

/// Start the screen shell (if there is a terminal and a keyboard), then run
/// the serial shell here.
pub fn run() -> ! {
    match screen_editor() {
        // Logged before the screen's shell starts, so its prompt comes after
        // this line on the screen.
        Ok(editor) => {
            log::info!(target: "shell", "shells on serial and the screen; type 'help'");
            match crate::sched::spawn("shell-screen", move || run_on(Screen, editor)) {
                Ok(handle) => drop(handle), // it runs for good
                Err(e) => log::warn!(target: "shell", "no thread for the screen's shell: {e}"),
            }
        }
        Err(why) => log::info!(target: "shell", "shell on serial (none on the screen: {why}); type 'help'"),
    }
    let mode = if crate::cmdline::has_flag("lumen.ansi") { Mode::Ansi } else { Mode::Plain };
    let max = if mode == Mode::Ansi { SERIAL_COLUMNS - PROMPT.len() - 1 } else { MAX_LINE };
    run_on(Serial { decoder: Decoder::new() }, Editor::new(PROMPT, mode, max, HISTORY))
}

/// The screen shell's editor, if there is a terminal and a keyboard.
fn screen_editor() -> Result<Editor, &'static str> {
    let cols = crate::term::status().ok_or("no terminal")?.cols;
    if keyboard::stats().result != Some(Ok(())) {
        return Err("no keyboard");
    }
    let max = MAX_LINE.min(cols.saturating_sub(PROMPT.len() + 1));
    Ok(Editor::new(PROMPT, Mode::Ansi, max, HISTORY))
}

/// A shell's loop: a line, its command, again. What the editor writes for a
/// key goes to the console in one write (on the screen: drawn whole, without
/// a log line in the middle of a redraw).
fn run_on(mut console: impl Console, mut editor: Editor) -> ! {
    let ansi = editor.mode() == Mode::Ansi;
    let mut shown = String::new();
    loop {
        let _ = editor.start(&mut shown);
        flush(&mut console, &mut shown);
        let line = loop {
            let key = console.read_key();
            let event = editor.key(key, &commands::COMMANDS, &mut shown);
            flush(&mut console, &mut shown);
            match event {
                Ok(Some(Event::Line(line))) => break Some(line),
                Ok(Some(Event::Cancelled)) => break None,
                _ => {}
            }
        };
        if let Some(line) = line {
            commands::run(&line, &mut Ctx { out: &mut console, ansi, history: editor.history() });
        }
    }
}

fn flush(console: &mut impl Console, shown: &mut String) {
    if !shown.is_empty() {
        let _ = console.write_str(shown);
        shown.clear();
    }
}

/// Serial, in and out.
struct Serial {
    decoder: Decoder,
}

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        serial::write_bytes(s.as_bytes());
        Ok(())
    }
}

impl Console for Serial {
    fn read_key(&mut self) -> Key {
        loop {
            match serial::try_read_byte() {
                Some(byte) => {
                    if let Some(key) = self.decoder.feed(byte) {
                        return key;
                    }
                }
                // Let the CPU's other threads run (the screen shell, woken by
                // a key, among them).
                None => crate::sched::yield_now(),
            }
        }
    }
}

/// The keyboard in, the screen's terminal out. Each `write!` reaches the
/// screen whole (one lock), not in pieces another CPU's log line could land
/// between.
struct Screen;

impl Write for Screen {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::term::write_str(s);
        Ok(())
    }

    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        crate::term::write_fmt(args);
        Ok(())
    }
}

impl Console for Screen {
    fn read_key(&mut self) -> Key {
        loop {
            let key = keyboard::wait_key();
            match key {
                keyboard::Key::Raw { code: KeyCode::PageUp, shift: true } => crate::term::scroll_half_pages(1),
                keyboard::Key::Raw { code: KeyCode::PageDown, shift: true } => crate::term::scroll_half_pages(-1),
                _ => {
                    if let Some(key) = screen_key(key) {
                        crate::term::reset_view();
                        return key;
                    }
                }
            }
        }
    }
}

/// The line editor's key for a key from the keyboard, if it takes one.
/// (`pc-keyboard` makes Enter `\n`, Backspace U+0008, Delete U+007F, Tab
/// `\t`, Escape U+001B and Ctrl with a letter U+0001 to U+001A.)
fn screen_key(key: keyboard::Key) -> Option<Key> {
    use keyboard::Key as K;
    Some(match key {
        K::Char('\n' | '\r') => Key::Enter,
        K::Char('\u{8}') => Key::Backspace,
        K::Char('\u{7f}') => Key::Delete,
        K::Char('\t') => Key::Tab,
        K::Char(c @ '\u{1}'..='\u{1a}') => Key::Ctrl(char::from(b'a' + c as u8 - 1)),
        K::Char(c) if !c.is_control() => Key::Char(c),
        K::Raw { code, .. } => match code {
            KeyCode::ArrowLeft => Key::Left,
            KeyCode::ArrowRight => Key::Right,
            KeyCode::ArrowUp => Key::Up,
            KeyCode::ArrowDown => Key::Down,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            _ => return None,
        },
        K::Char(_) => return None,
    })
}

crate::ktest!(
    fn keyboard_keys_reach_the_line_editor() {
        use crate::kassert_eq;
        use keyboard::Key as K;
        let raw = |code| K::Raw { code, shift: false };
        let cases = [
            (K::Char('a'), Some(Key::Char('a'))),
            (K::Char('é'), Some(Key::Char('é'))),
            (K::Char('\n'), Some(Key::Enter)),
            (K::Char('\u{8}'), Some(Key::Backspace)),
            (K::Char('\u{7f}'), Some(Key::Delete)),
            (K::Char('\t'), Some(Key::Tab)),
            (K::Char('\u{1}'), Some(Key::Ctrl('a'))),
            (K::Char('\u{17}'), Some(Key::Ctrl('w'))),
            (K::Char('\u{1b}'), None),
            (raw(KeyCode::ArrowLeft), Some(Key::Left)),
            (raw(KeyCode::ArrowRight), Some(Key::Right)),
            (raw(KeyCode::ArrowUp), Some(Key::Up)),
            (raw(KeyCode::ArrowDown), Some(Key::Down)),
            (raw(KeyCode::Home), Some(Key::Home)),
            (raw(KeyCode::End), Some(Key::End)),
            (raw(KeyCode::PageUp), None),
            (raw(KeyCode::LShift), None),
        ];
        for (key, want) in cases {
            kassert_eq!(screen_key(key), want);
        }
        Ok(())
    }
);

crate::ktest!(
    fn the_screen_console_writes_each_write_whole() {
        // A whole `write!` is formatted under the screen's lock.
        use crate::kassert;
        use crate::term::LockProbe;
        use core::sync::atomic::{AtomicBool, Ordering};
        let held = AtomicBool::new(false);
        let _ = write!(Screen, "{}", LockProbe(&held));
        kassert!(held.load(Ordering::Relaxed));
        Ok(())
    }
);
