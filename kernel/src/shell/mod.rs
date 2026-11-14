//! The shell: one per console, each with its own line and history.
//!
//! - The serial shell reads serial and answers on serial, in plain text
//!   (appends and `BS space BS` erases only, never an escape code: the test
//!   harness reads serial line by line) unless the command line has
//!   `lumen.ansi`, which gives it the screen shell's editing. It runs on the
//!   boot CPU's `main` thread (kmain's context), polling serial (no serial
//!   interrupt yet, #61) and yielding to the CPU's other threads between
//!   polls.
//! - The screen shells, one per virtual terminal (`term::VTS`), read the
//!   PS/2 keyboard while their terminal is shown (Alt+F1 to Alt+F4 switch:
//!   `crate::vt`) and answer on it, with ANSI line editing. Each runs in its
//!   own thread (`shell-vt1` to `shell-vt4`, on the boot CPU), blocked until
//!   a key arrives; they start only with a keyboard and a terminal wide
//!   enough for a 16-column line. Shift+PageUp and Shift+PageDown scroll
//!   the shell's own terminal back and forward by half a screen; any key
//!   the line editor takes shows its screen again.
//!
//! Log lines go to serial and the first virtual terminal, between the
//! shells' output. The line
//! editor, the word splitting and completion are `lumen-shell`'s (tested on
//! the host); the commands are in [`commands`], and pipelines (`cpus |
//! count`) in [`pipe`].

#![cfg_attr(feature = "ktest", allow(dead_code))] // the test kernel runs no shell

mod commands;
mod crash;
mod pipe;
mod sysinfo;

use crate::arch::serial;
use crate::keyboard::{self, KeyCode};
use crate::term;
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

/// What a command can use: its console (output, and keys through
/// [`Ctx::poll_key`]/[`Ctx::wait_key`]), the screen's pixels
/// ([`Ctx::canvas`]) and the shell's history.
pub struct Ctx<'a> {
    /// The console. A `write!` reaches it whole (on a virtual terminal it
    /// is formatted first, then drawn under one lock; on serial it is
    /// formatted under the log's line lock, so a `Display` that logs would
    /// take that lock again, a re-lock crash report: make such text before
    /// the call). While a command holds the canvas
    /// (`term::status().paused`), output to the terminals reaches them but
    /// isn't drawn.
    pub out: &'a mut dyn Console,
    /// Whether the console takes escape codes.
    pub ansi: bool,
    pub history: &'a History,
    /// Who holds a canvas this command takes: a new owner for each command
    /// (`commands::run_command`), so the shell takes back only its own.
    owner: term::Owner,
}

impl<'a> Ctx<'a> {
    /// A context for running commands on `out`: each run gets its own
    /// canvas owner, on `out`'s virtual terminal if it is one (its canvas
    /// brings that terminal to the front).
    pub fn new(out: &'a mut dyn Console, ansi: bool, history: &'a History) -> Ctx<'a> {
        let owner = out.vt().map_or_else(term::Owner::new, term::Owner::on_vt);
        Ctx { out, ansi, history, owner }
    }

    /// Take the screen's pixels for this command (see `term::Canvas`; drop
    /// it before returning: the shell takes back one left behind).
    #[allow(dead_code, reason = "the first users are the demos and scripts (#67, #68)")]
    pub fn canvas(&self) -> Result<term::Canvas, term::CanvasError> {
        term::canvas(self.owner)
    }

    /// Who holds a canvas this command takes, for code taking it away from
    /// the `Ctx` (a script's thread): `term::canvas(ctx.owner())`.
    pub fn owner(&self) -> term::Owner {
        self.owner
    }

    /// The next key typed on this console, if one is waiting (never
    /// blocks): for a command that runs until a key (`q`, Ctrl-C) stops it.
    /// On a virtual terminal, the keyboard's keys routed to it (routed while
    /// it was shown: `crate::vt`); Shift+PageUp/PageDown still scroll it and
    /// aren't returned, and keys the line editor doesn't take (Esc, F1) are
    /// dropped. Keys typed ahead for the next line are taken too. While a
    /// command holds the canvas, Shift+PageUp/PageDown do nothing (the view
    /// stays where it was).
    pub fn poll_key(&mut self) -> Option<Key> {
        self.out.try_key()
    }

    /// Wait for the next key typed on this console (see [`Self::poll_key`]):
    /// on a virtual terminal the thread sleeps until IRQ 1 (or another
    /// shell routing it a key); on serial it polls, yielding to the CPU's
    /// other threads.
    #[allow(dead_code, reason = "the first users are the demos and scripts (#67, #68)")]
    pub fn wait_key(&mut self) -> Key {
        self.out.read_key()
    }

    /// Where this console's keys are typed ("serial", "the keyboard").
    pub fn console_name(&self) -> &'static str {
        self.out.name()
    }

    /// Whether this command's output goes to the next command in a
    /// pipeline (`top 3 | count`) rather than to the console. Its keys
    /// still come from the console.
    pub fn piped(&self) -> bool {
        self.out.piped()
    }

    /// Say why the command failed or won't run (`line`: a usage line, an
    /// error; without its newline, though it may hold several lines): on
    /// the console, even in a pipeline, which it then ends. Made into a
    /// `String` first (see [`Ctx::out`]), so it allocates: not under a lock
    /// that forbids the heap.
    pub fn fail(&mut self, line: impl fmt::Display) {
        self.out.fail(&alloc::format!("{line}"));
    }
}

/// A console a shell runs on.
pub trait Console: Write + Send {
    /// Wait for the next key.
    fn read_key(&mut self) -> Key;
    /// The next key, if one is waiting (never blocks).
    fn try_key(&mut self) -> Option<Key>;
    /// Where its keys are typed, for a prompt shown elsewhere ("press a key
    /// on serial").
    fn name(&self) -> &'static str;
    /// The virtual terminal it is, if it is one (a canvas its command takes
    /// brings it to the front).
    fn vt(&self) -> Option<usize> {
        None
    }
    /// Whether what is written goes to the next command in a pipeline.
    /// (A console overrides both this and [`Console::fail`], or neither.)
    fn piped(&self) -> bool {
        false
    }
    /// Write `line` and a newline, in one write: why a command failed or
    /// won't run. In a pipeline it goes to the console, and the pipeline
    /// stops.
    fn fail(&mut self, line: &str) {
        let mut text = String::with_capacity(line.len() + 1);
        text.push_str(line);
        text.push('\n');
        let _ = self.write_str(&text);
    }
}

/// About how many bytes [`write_in_chunks`] writes at a time: a serial
/// write holds the log's line lock, with interrupts off, while the UART
/// sends it (512 bytes take 44 ms at 115200 baud).
const CHUNK: usize = 512;

/// Write `text` to `out` in pieces of whole lines (`lumen_shell::pipe::
/// chunks`: each at least [`CHUNK`] bytes but the last, and less than that
/// plus one line), so a long output doesn't hold the log's line lock for
/// long (a log line may land between pieces, never inside a line).
pub fn write_in_chunks(out: &mut dyn Console, text: &str) {
    for piece in lumen_shell::pipe::chunks(text, CHUNK) {
        let _ = out.write_str(piece);
    }
}

/// The screen shells' threads, one per virtual terminal.
const SCREEN_THREADS: [&str; term::VTS] = ["shell-vt1", "shell-vt2", "shell-vt3", "shell-vt4"];

/// Start the screen shells (if there is a terminal and a keyboard), then
/// run the serial shell here.
pub fn run() -> ! {
    match screen_line_width() {
        // Logged before the screen's shells start, so the first's prompt
        // comes after this line on the screen.
        Ok(max) => {
            log::info!(target: "shell", "shells on serial and the screen; type 'help'");
            for (vt, name) in SCREEN_THREADS.into_iter().enumerate() {
                let editor = Editor::new(PROMPT, Mode::Ansi, max, HISTORY);
                // Before it runs: keys typed on it from then on are its own.
                crate::vt::attach(vt);
                match crate::sched::spawn(name, move || run_on(Screen { vt }, editor)) {
                    Ok(handle) => drop(handle), // it runs for good
                    Err(e) => {
                        crate::vt::detach(vt);
                        log::error!(target: "shell", "no shell on terminal {} after all: no thread for it: {e}", vt + 1);
                        // Not a blank screen: say so where its prompt would be
                        // (formatted under the screen's lock, without
                        // allocating: memory may be what ran out).
                        crate::term::write_fmt_to(vt, format_args!("no shell on this terminal: no thread for it: {e}\n"));
                    }
                }
            }
        }
        Err(why) => log::info!(target: "shell", "shell on serial (none on the screen: {why}); type 'help'"),
    }
    let mode = if crate::cmdline::has_flag("lumen.ansi") { Mode::Ansi } else { Mode::Plain };
    let max = if mode == Mode::Ansi { max_line(SERIAL_COLUMNS) } else { MAX_LINE };
    run_on(Serial { decoder: Decoder::new() }, Editor::new(PROMPT, mode, max, HISTORY))
}

/// The screen shells' widest line, if there is a terminal and a keyboard.
fn screen_line_width() -> Result<usize, &'static str> {
    let cols = crate::term::status().ok_or("no terminal")?.cols;
    if keyboard::stats().result != Some(Ok(())) {
        return Err("no keyboard");
    }
    let max = max_line(cols);
    if max < MIN_LINE {
        return Err("terminal too narrow");
    }
    Ok(max)
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
            commands::run(&line, &mut Ctx::new(&mut console, ansi, editor.history()));
        }
    }
}

/// Write what the line editor left in `shown` to `console` (one write), and
/// empty it.
pub fn flush(console: &mut (impl Console + ?Sized), shown: &mut String) {
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
    fn name(&self) -> &'static str {
        "serial"
    }

    fn read_key(&mut self) -> Key {
        loop {
            if let Some(key) = self.try_key() {
                return key;
            }
            // Let the CPU's other threads run (the screen shells, woken by a
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

/// A virtual terminal as a console: in, the keyboard's keys routed to it
/// (routed while it is shown); out, its terminal. Each `write!` reaches the
/// terminal whole (one lock), not in pieces another CPU's log line could
/// land between. It is formatted
/// before taking the screen's lock: formatting may allocate (a script
/// value's `Debug` does), which the heap refuses under that lock.
struct Screen {
    vt: usize,
}

impl Write for Screen {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        crate::term::write_str_to(self.vt, s);
        Ok(())
    }

    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        match args.as_str() {
            Some(s) => crate::term::write_str_to(self.vt, s),
            None => crate::term::write_str_to(self.vt, &alloc::fmt::format(args)),
        };
        Ok(())
    }
}

impl Screen {
    /// A key typed on this terminal: Shift+PageUp/PageDown scroll it and
    /// give `None`, as does a key the line editor doesn't take; any other
    /// brings its view back to the bottom and is the editor's key.
    fn take(&self, key: keyboard::Key) -> Option<Key> {
        match key {
            keyboard::Key::Raw { code: KeyCode::PageUp, shift: true, .. } => {
                crate::term::scroll_half_pages(self.vt, 1);
                None
            }
            keyboard::Key::Raw { code: KeyCode::PageDown, shift: true, .. } => {
                crate::term::scroll_half_pages(self.vt, -1);
                None
            }
            _ => {
                let key = screen_key(key)?;
                crate::term::reset_view(self.vt);
                Some(key)
            }
        }
    }
}

impl Console for Screen {
    fn name(&self) -> &'static str {
        "the keyboard"
    }

    fn vt(&self) -> Option<usize> {
        Some(self.vt)
    }

    fn read_key(&mut self) -> Key {
        loop {
            if let Some(key) = self.take(crate::vt::wait_key(self.vt)) {
                return key;
            }
        }
    }

    fn try_key(&mut self) -> Option<Key> {
        while let Some(key) = crate::vt::read_key(self.vt) {
            if let Some(key) = self.take(key) {
                return Some(key);
            }
        }
        None
    }
}

/// A console for the tests: what is written to it, and keys typed on it.
/// With none left, `read_key` is Ctrl-C, and so is `try_key` after
/// [`Captured::DRY_POLLS`] polls in a row found none (a command polling for
/// a key ends instead of spinning); both set `ran_dry`, which a test that
/// meant its own keys to stop a command checks stayed false.
#[cfg(feature = "ktest")]
#[derive(Default)]
pub struct Captured {
    pub text: String,
    pub keys: alloc::collections::VecDeque<Key>,
    pub ran_dry: bool,
    /// Polls in a row that found no key.
    empty_polls: u32,
}

#[cfg(feature = "ktest")]
impl Captured {
    pub const DRY_POLLS: u32 = 100_000;

    /// Ctrl-C, noting the keys ran out.
    fn dry(&mut self) -> Key {
        self.ran_dry = true;
        Key::Ctrl('c')
    }
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
    fn name(&self) -> &'static str {
        "the test console"
    }

    fn read_key(&mut self) -> Key {
        self.keys.pop_front().unwrap_or_else(|| self.dry())
    }

    fn try_key(&mut self) -> Option<Key> {
        let key = self.keys.pop_front();
        self.empty_polls = if key.is_some() { 0 } else { self.empty_polls + 1 };
        if self.empty_polls >= Self::DRY_POLLS {
            self.empty_polls = 0;
            return Some(self.dry());
        }
        key
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
        let raw = |code| K::Raw { code, shift: false, alt: false };
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
    fn the_screen_console_formats_then_draws_each_write_whole() {
        // A whole `write!` is formatted before taking the screen's lock (an
        // argument that allocates there would be refused by the heap), then
        // drawn as one piece.
        use crate::kassert;
        use crate::term::LockProbe;
        use core::sync::atomic::{AtomicBool, Ordering};
        /// Allocates while formatting (as a script value's `Debug` does).
        struct Allocating;
        impl fmt::Display for Allocating {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let text = core::hint::black_box(alloc::format!("allocated while formatting {}", 42));
                f.write_str(&text)
            }
        }
        let held = AtomicBool::new(false);
        let _ = writeln!(Screen { vt: 0 }, "{}{}", LockProbe(&held), Allocating);
        kassert!(!held.load(Ordering::Relaxed));
        let view = crate::term::view_text().ok_or("no terminal")?;
        kassert!(view.rows.iter().any(|r| r.contains("allocated while formatting 42")));
        Ok(())
    }
);

crate::ktest!(
    fn a_terminal_s_scroll_keys_scroll_it_even_when_hidden() {
        // The second terminal's console, given Shift+PageUp while the first
        // is shown (a key routed to it before a switch), scrolls the second
        // back and leaves the first alone; a key the editor takes brings the
        // second back to its screen.
        use crate::{kassert, kassert_eq, term};
        kassert_eq!(term::shown(), 0);
        /// Brings both views back when dropped.
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                term::reset_view(0);
                term::reset_view(1);
            }
        }
        let _reset = Reset;
        for i in 0..60 {
            kassert!(term::write_str_to(1, &alloc::format!("second {i}\n")));
        }
        let second = Screen { vt: 1 };
        kassert_eq!(second.vt(), Some(1));
        let page_up = keyboard::Key::Raw { code: KeyCode::PageUp, shift: true, alt: false };
        kassert_eq!(second.take(page_up), None);
        let views = || term::view_offset(0).zip(term::view_offset(1)).ok_or("no terminal");
        let (first, scrolled) = views()?;
        kassert!(first == 0 && scrolled > 0);
        kassert_eq!(second.take(keyboard::Key::Char('x')), Some(Key::Char('x')));
        kassert_eq!(views()?, (0, 0));
        Ok(())
    }
);
