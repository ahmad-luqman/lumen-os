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
//!   starts only with a keyboard and a terminal wide enough for a 16-column
//!   line. Shift+PageUp and
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
/// The widest line, in columns (less on a console too narrow for it).
const MAX_LINE: usize = 160;
/// The narrowest line a screen shell is started with.
const MIN_LINE: usize = 16;
/// Lines each shell keeps in its history.
const HISTORY: usize = 100;
/// How wide a serial terminal is taken to be with `lumen.ansi` (lines stay
/// shorter than it: the editor doesn't wrap).
const SERIAL_COLUMNS: usize = 80;

/// What a command can use: its console and the shell's history.
pub struct Ctx<'a> {
    /// The console. On the screen a `write!` is formatted under the screen's
    /// lock, so what it formats must not allocate (arguments that allocate,
    /// like a `join`, are made before the call).
    pub out: &'a mut dyn Console,
    /// Whether the console takes escape codes.
    pub ansi: bool,
    pub history: &'a History,
}

#[allow(dead_code, reason = "the first users are `top`, the demos and scripts (#63, #67, #68)")]
impl Ctx<'_> {
    /// The next key typed on this console, if one is waiting (never
    /// blocks): for a command that runs until a key (`q`, Ctrl-C) stops it.
    pub fn poll_key(&mut self) -> Option<Key> {
        self.out.try_key()
    }

    /// Wait for the next key typed on this console.
    pub fn wait_key(&mut self) -> Key {
        self.out.read_key()
    }
}

/// A console a shell runs on.
pub trait Console: Write {
    /// Wait for the next key.
    fn read_key(&mut self) -> Key;
    /// The next key, if one is waiting (never blocks).
    fn try_key(&mut self) -> Option<Key>;
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
                Err(e) => log::error!(target: "shell", "no shell on the screen after all: no thread for it: {e}"),
            }
        }
        Err(why) => log::info!(target: "shell", "shell on serial (none on the screen: {why}); type 'help'"),
    }
    let mode = if crate::cmdline::has_flag("lumen.ansi") { Mode::Ansi } else { Mode::Plain };
    let max = if mode == Mode::Ansi { max_line(SERIAL_COLUMNS) } else { MAX_LINE };
    run_on(Serial { decoder: Decoder::new() }, Editor::new(PROMPT, mode, max, HISTORY))
}

/// The screen shell's editor, if there is a terminal and a keyboard.
fn screen_editor() -> Result<Editor, &'static str> {
    let cols = crate::term::status().ok_or("no terminal")?.cols;
    if keyboard::stats().result != Some(Ok(())) {
        return Err("no keyboard");
    }
    let max = max_line(cols);
    if max < MIN_LINE {
        return Err("terminal too narrow");
    }
    Ok(Editor::new(PROMPT, Mode::Ansi, max, HISTORY))
}

/// The widest line on a console `cols` wide: the prompt, the line and the
/// `^C` Ctrl-C writes after it leave the last column free (the editor
/// doesn't wrap).
fn max_line(cols: usize) -> usize {
    MAX_LINE.min(cols.saturating_sub(PROMPT.len() + 3))
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

/// Serial, in and out. Each write (a whole `write!`) goes through
/// `klog::LockedSerial`, holding the log's line lock, so another CPU's log
/// line can't land in the middle of it (the harness reads `term text`'s
/// rows whole).
struct Serial {
    decoder: Decoder,
}

impl Write for Serial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::klog::LockedSerial.write_str(s)
    }

    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        crate::klog::LockedSerial.write_fmt(args)
    }
}

impl Console for Serial {
    fn read_key(&mut self) -> Key {
        loop {
            if let Some(key) = self.try_key() {
                return key;
            }
            // Let the CPU's other threads run (the screen shell, woken by a
            // key, among them).
            crate::sched::yield_now();
        }
    }

    fn try_key(&mut self) -> Option<Key> {
        while let Some(byte) = serial::try_read_byte() {
            if let Some(key) = self.decoder.feed(byte) {
                return Some(key);
            }
        }
        None
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

impl Screen {
    /// The line editor's key for `key`, after Shift+PageUp/PageDown (which
    /// scroll the screen) and keys it doesn't take; one it takes shows the
    /// screen again.
    fn take(key: keyboard::Key) -> Option<Key> {
        match key {
            keyboard::Key::Raw { code: KeyCode::PageUp, shift: true } => crate::term::scroll_half_pages(1),
            keyboard::Key::Raw { code: KeyCode::PageDown, shift: true } => crate::term::scroll_half_pages(-1),
            _ => {
                if let Some(key) = screen_key(key) {
                    crate::term::reset_view();
                    return Some(key);
                }
            }
        }
        None
    }
}

impl Console for Screen {
    fn read_key(&mut self) -> Key {
        loop {
            if let Some(key) = Self::take(keyboard::wait_key()) {
                return key;
            }
        }
    }

    fn try_key(&mut self) -> Option<Key> {
        while let Some(key) = keyboard::read_key() {
            if let Some(key) = Self::take(key) {
                return Some(key);
            }
        }
        None
    }
}

/// A console for the tests: what is written to it, and keys typed on it
/// (`read_key` with none left is Ctrl-C).
#[cfg(feature = "ktest")]
#[derive(Default)]
pub struct Captured {
    pub text: String,
    pub keys: alloc::collections::VecDeque<Key>,
}

#[cfg(feature = "ktest")]
impl Write for Captured {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.text.push_str(s);
        Ok(())
    }
}

#[cfg(feature = "ktest")]
impl Console for Captured {
    fn read_key(&mut self) -> Key {
        self.keys.pop_front().unwrap_or(Key::Ctrl('c'))
    }

    fn try_key(&mut self) -> Option<Key> {
        self.keys.pop_front()
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
