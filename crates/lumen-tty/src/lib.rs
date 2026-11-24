//! Bounded terminal line discipline, separate from `lumen-term`'s display.
//!
//! Input and echo storage are allocated once at construction. Receive, read,
//! editing, mode changes and readiness never allocate. Each receive returns
//! a borrowed echo batch, already output-processed: consume it before the
//! next call and do not pass it through `write` again. A batch is bounded at
//! 4096 bytes; excess echo is dropped at emission boundaries. `write` allocates
//! one result buffer per call (at most twice the supplied byte count).
//!
//! IXON/IXOFF flow control, parity, baud-rate effects and TIOCSTI are outside
//! this crate. A byte cannot distinguish a hardware BREAK from NUL, so
//! IGNBRK/BRKINT have no effect on `receive`; use `receive_break` when the
//! driver has BREAK metadata. Sessions, sleeping and signal delivery belong
//! to the kernel. Unsupported termios bits are preserved, not implemented.

#![no_std]

extern crate alloc;

use alloc::{collections::VecDeque, vec::Vec};
use linux_raw_sys::general::*;

mod output;
mod readiness;
mod termios;
pub use readiness::{Readiness, TimerKind, TimerState, WriterFacts, background_write_signal, ready};
pub use termios::{Termios, Winsize};

pub const BUFFER_SIZE: usize = 4096;

/// A signal to the foreground process group, with flushes the caller must
/// apply to its device queues. The discipline has already flushed its input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signal {
    pub number: u32,
    pub flush_input: bool,
    pub flush_output: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Effects<'a> {
    pub echo: &'a [u8],
    pub signal: Option<Signal>,
    /// Current readiness after this input; true also for an empty EOF record.
    pub input_readable: bool,
}

/// Typed equivalents of TCSANOW, TCSADRAIN and TCSAFLUSH.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetWhen {
    Now,
    Drain,
    Flush,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TermiosChange {
    /// The caller must drain output, then call `output_drained`. Until then
    /// the old settings and input remain active.
    DrainRequired,
    Applied {
        flush_input: bool,
        input_readable: bool,
    },
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    byte: u8,
    end: bool,
    eof: bool,
    width: usize,
}

/// Serialized terminal state. Input (including EOF markers) never exceeds
/// 4096 slots. Canonical input reserves one slot for a delimiter. A read
/// returns at most one completed record; short reads retain its remainder.
pub struct LineDiscipline {
    termios: Termios,
    input: VecDeque<Entry>,
    echo: Vec<u8>,
    pending: usize,
    records: usize,
    literal: bool,
    column: usize,
    timer: TimerState,
    winsize: Winsize,
    deferred: Option<(Termios, SetWhen)>,
}

impl LineDiscipline {
    pub fn new(termios: Termios) -> Self {
        Self {
            termios,
            input: VecDeque::with_capacity(BUFFER_SIZE),
            echo: Vec::with_capacity(BUFFER_SIZE),
            pending: 0,
            records: 0,
            literal: false,
            column: 0,
            timer: TimerState::Idle,
            winsize: Winsize::default(),
            deferred: None,
        }
    }

    pub fn termios(&self) -> Termios {
        self.termios
    }
    pub fn winsize(&self) -> Winsize {
        self.winsize
    }

    /// True exactly when any of the four window-size fields changed.
    pub fn set_winsize(&mut self, winsize: Winsize) -> bool {
        let changed = self.winsize != winsize;
        self.winsize = winsize;
        changed
    }

    /// Free input slots, including the canonical delimiter reserve. Editing
    /// and signals are still processed when this returns zero.
    pub fn input_room(&self) -> usize {
        BUFFER_SIZE - self.input.len()
    }

    pub fn readable(&self) -> bool {
        if self.local(ICANON) { self.records != 0 } else { matches!(self.readiness(), Readiness::ReturnNow(_)) }
    }

    /// Current raw read decision. For canonical mode use `readable` instead.
    pub fn readiness(&self) -> Readiness {
        ready(self.termios.c_cc[VMIN as usize], self.termios.c_cc[VTIME as usize], self.input.len(), self.timer)
    }

    /// Supplies the current read's timer state. Begin each read with Idle;
    /// set Running after arming and Expired when the kernel clock expires it.
    /// Receiving a raw byte resets an inter-byte timer to Idle for rearming.
    pub fn set_timer_state(&mut self, timer: TimerState) {
        self.timer = timer;
    }

    /// Immediate changes apply now. Drain/Flush requests are staged until
    /// `output_drained`, even if the caller's output queue is already empty.
    /// A newer request supersedes any staged request.
    pub fn set_termios(&mut self, new: Termios, when: SetWhen) -> TermiosChange {
        self.deferred = None;
        if when != SetWhen::Now {
            self.deferred = Some((new, when));
            TermiosChange::DrainRequired
        } else {
            self.apply_termios(new, false)
        }
    }

    /// Applies a staged change after the caller has drained device output.
    pub fn output_drained(&mut self) -> Option<TermiosChange> {
        self.deferred.take().map(|(new, when)| self.apply_termios(new, when == SetWhen::Flush))
    }

    fn apply_termios(&mut self, new: Termios, flush: bool) -> TermiosChange {
        let was_canon = self.local(ICANON);
        if flush {
            self.flush_input();
        }
        self.termios = new;
        if was_canon != self.local(ICANON) {
            // EOF is metadata, never a byte exposed by a raw read.
            self.input.retain(|e| !e.eof);
            for e in &mut self.input {
                e.end = false;
            }
            self.pending = 0;
            self.records = 0;
            self.literal = false;
            if self.local(ICANON)
                && let Some(last) = self.input.back_mut()
            {
                // n_tty exposes pre-existing raw input as one synthetic line.
                last.end = true;
                self.records = 1;
            }
        }
        self.timer = TimerState::Idle;
        TermiosChange::Applied { flush_input: flush, input_readable: self.readable() }
    }

    fn local(&self, flag: u32) -> bool {
        self.termios.c_lflag & flag != 0
    }
    fn input_flag(&self, flag: u32) -> bool {
        self.termios.c_iflag & flag != 0
    }
    fn cc(&self, index: u32, byte: u8) -> bool {
        byte != 0 && self.termios.c_cc[index as usize] == byte
    }
    fn flush_input(&mut self) {
        self.input.clear();
        self.pending = 0;
        self.records = 0;
        self.literal = false;
        self.timer = TimerState::Idle;
    }
    fn signal(&mut self, number: u32) -> Signal {
        let flush = !self.local(NOFLSH);
        if flush {
            self.flush_input();
            self.echo.clear();
        }
        Signal { number, flush_input: flush, flush_output: flush }
    }
    fn effects(&self, signal: Option<Signal>) -> Effects<'_> {
        Effects { echo: &self.echo, signal, input_readable: self.readable() }
    }

    /// Processes a hardware BREAK. IGNBRK wins over BRKINT; BRKINT produces
    /// SIGINT even without ISIG, respecting NOFLSH. Otherwise a literal NUL
    /// enters the input stream. Parity marking (PARMRK) is outside scope.
    pub fn receive_break(&mut self) -> Effects<'_> {
        self.echo.clear();
        let signal = if self.input_flag(IGNBRK) {
            None
        } else if self.input_flag(BRKINT) {
            Some(self.signal(SIGINT))
        } else {
            self.store(0, false, false, false);
            None
        };
        self.effects(signal)
    }

    /// Processes one byte without allocating. Disabled (zero) control
    /// characters never match NUL. Literal-next bypasses mapping, editing
    /// and signals on the following byte, but ISTRIP still applies.
    pub fn receive(&mut self, mut byte: u8) -> Effects<'_> {
        self.echo.clear();
        if self.input_flag(ISTRIP) {
            byte &= 0x7f;
        }
        if self.literal {
            self.literal = false;
            self.store(byte, false, false, true);
            return self.effects(None);
        }
        if self.local(ISIG) {
            let number = if self.cc(VINTR, byte) {
                Some(SIGINT)
            } else if self.cc(VQUIT, byte) {
                Some(SIGQUIT)
            } else if self.cc(VSUSP, byte) {
                Some(SIGTSTP)
            } else {
                None
            };
            if let Some(number) = number {
                let signal = self.signal(number);
                if self.local(ECHO) {
                    self.echo_char(byte);
                }
                return self.effects(Some(signal));
            }
        }
        if byte == b'\r' {
            if self.input_flag(IGNCR) {
                return self.effects(None);
            }
            if self.input_flag(ICRNL) {
                byte = b'\n';
            }
        } else if byte == b'\n' && self.input_flag(INLCR) {
            byte = b'\r';
        }
        if self.local(ICANON) {
            if self.cc(VERASE, byte) {
                self.erase(false, byte);
            } else if self.local(IEXTEN) && self.cc(VWERASE, byte) {
                self.erase(true, byte);
            } else if self.cc(VKILL, byte) {
                self.kill(byte);
            } else if self.local(IEXTEN) && self.cc(VLNEXT, byte) {
                self.literal = true;
                if self.local(ECHO) && self.local(ECHOCTL) {
                    self.emit(b'^');
                    self.emit(8);
                }
            } else if self.local(IEXTEN) && self.local(ECHO) && self.cc(VREPRINT, byte) {
                self.echo_char(byte);
                self.emit(b'\n');
                let start = self.input.len() - self.pending;
                for i in start..self.input.len() {
                    let b = self.input[i].byte;
                    let col = self.column;
                    self.echo_char(b);
                    self.input[i].width = self.column.saturating_sub(col);
                }
            } else if self.cc(VEOF, byte) {
                self.store(0, true, true, false);
            } else {
                let end = byte == b'\n' || self.cc(VEOL, byte) || (self.local(IEXTEN) && self.cc(VEOL2, byte));
                self.store(byte, end, false, false);
            }
        } else {
            self.store(byte, false, false, false);
        }
        self.effects(None)
    }

    fn store(&mut self, byte: u8, end: bool, eof: bool, quoted: bool) {
        let canon = self.local(ICANON);
        if self.input.len() >= BUFFER_SIZE || (canon && !end && self.input.len() >= BUFFER_SIZE - 1) {
            // At the canonical line limit n_tty keeps processing and echoing
            // ordinary characters, but does not retain them.
            if self.input_flag(IMAXBEL) {
                self.emit(7);
            } else if canon && self.records == 0 && !eof && self.local(ECHO) {
                self.echo_char(byte);
            }
            return;
        }
        let column = self.column;
        if !eof && (self.local(ECHO) || (canon && byte == b'\n' && self.local(ECHONL))) {
            if byte == b'\n' && !quoted {
                self.emit(byte);
            } else {
                self.echo_char(byte);
            }
        }
        self.input.push_back(Entry { byte, end, eof, width: self.column.saturating_sub(column) });
        if canon {
            if end {
                self.pending = 0;
                self.records += 1;
            } else {
                self.pending += 1;
            }
        } else if self.termios.c_cc[VMIN as usize] != 0 && self.termios.c_cc[VTIME as usize] != 0 {
            self.timer = TimerState::Idle;
        }
    }

    fn erase(&mut self, word: bool, erase_byte: u8) {
        let mut seen_word = false;
        let mut removed = false;
        while self.pending != 0 {
            let start = self.last_character();
            let byte = self.input[start].byte;
            let is_word = byte.is_ascii_alphanumeric() || byte == b'_';
            if word && seen_word && !is_word {
                break;
            }
            seen_word |= is_word;
            self.remove_character(start, self.local(ECHOE));
            removed = true;
            if !word {
                break;
            }
        }
        if removed && self.local(ECHO) && !self.local(ECHOE) {
            self.echo_char(erase_byte);
        }
    }

    fn last_character(&self) -> usize {
        let floor = self.input.len() - self.pending;
        let mut start = self.input.len() - 1;
        if self.input_flag(IUTF8) {
            while start > floor && self.input[start].byte & 0xc0 == 0x80 {
                start -= 1;
            }
        }
        start
    }

    fn remove_character(&mut self, start: usize, visual: bool) {
        let byte = self.input[start].byte;
        let width = if byte == b'\t' {
            self.input[start].width
        } else if byte.is_ascii_control() {
            if self.local(ECHOCTL) { 2 } else { 0 }
        } else {
            1
        };
        let count = self.input.len() - start;
        self.input.truncate(start);
        self.pending -= count;
        if self.local(ECHO) && visual {
            for _ in 0..width {
                self.emit(8);
                if byte != b'\t' {
                    self.emit(b' ');
                    self.emit(8);
                }
            }
        }
    }

    fn kill(&mut self, byte: u8) {
        if self.pending == 0 {
            return;
        }
        if self.local(ECHO) && self.local(ECHOE) && self.local(ECHOK) && self.local(ECHOKE) {
            while self.pending != 0 {
                self.remove_character(self.last_character(), true);
            }
        } else {
            self.input.truncate(self.input.len() - self.pending);
            self.pending = 0;
            if self.local(ECHO) {
                self.echo_char(byte);
                if self.local(ECHOK) {
                    self.emit(b'\n');
                }
            }
        }
    }

    fn echo_char(&mut self, byte: u8) {
        if self.local(ECHOCTL) && byte.is_ascii_control() && byte != b'\t' {
            self.emit(b'^');
            self.emit(if byte == 127 { b'?' } else { byte + 64 });
        } else {
            self.emit(byte);
        }
    }

    fn emit(&mut self, byte: u8) {
        let mut column = self.column;
        let (bytes, n) = output::process(byte, self.termios.c_oflag, self.input_flag(IUTF8), &mut column);
        if self.echo.len() + n <= BUFFER_SIZE {
            self.echo.extend_from_slice(&bytes[..n]);
            self.column = column;
        }
    }

    /// Copies available data without waiting. Check readiness first to
    /// distinguish no complete input from an EOF record. Empty destinations
    /// consume nothing. Raw reads copy available bytes regardless of VMIN;
    /// the caller uses `ready` to decide when its read should complete.
    pub fn read(&mut self, buf: &mut [u8]) -> usize {
        if buf.is_empty() || (self.local(ICANON) && self.records == 0) {
            return 0;
        }
        let mut n = 0;
        while let Some(entry) = self.input.front().copied() {
            if n == buf.len() && !entry.eof {
                break;
            }
            self.input.pop_front();
            if !entry.eof {
                buf[n] = entry.byte;
                n += 1;
            }
            if entry.end {
                self.records -= 1;
                break;
            }
            if n == buf.len() && !self.input.front().is_some_and(|e| e.eof) {
                break;
            }
        }
        n
    }

    /// Output processing with a single allocation, sharing column tracking
    /// with echo (including tabs, backspaces and UTF-8 continuation bytes).
    pub fn write(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut result = Vec::with_capacity(bytes.len().saturating_mul(2));
        for &byte in bytes {
            let (out, n) = output::process(byte, self.termios.c_oflag, self.input_flag(IUTF8), &mut self.column);
            result.extend_from_slice(&out[..n]);
        }
        result
    }
}

impl Default for LineDiscipline {
    fn default() -> Self {
        Self::new(Termios::linux_default())
    }
}

#[cfg(test)]
mod tests;
