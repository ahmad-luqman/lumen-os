//! Terminal line discipline modelled on n_tty, separate from `lumen-term`.
//!
//! Construction is fallible. All subsequent operations use fixed input
//! storage and a caller-supplied output sink, with no allocation. Echo units
//! are atomic; insufficient sink room drops that unit and the rest of that
//! call's echo, reported as `Echo::Truncated`. Writes instead return the
//! consumed prefix for retry. Only accepted output changes the column.
//!
//! Known differences from n_tty: IMAXBEL rings BEL on input overflow as a
//! Lumen policy (Linux ignores IMAXBEL); echo truncation is explicit instead
//! of using Linux's internal echo ring. IXON/IXOFF, parity, baud-rate effects,
//! TIOCSTI and unlisted termios flags are not implemented. Thus VSTART/VSTOP
//! arrive as data. Unsupported bits are preserved in the termios value.
//!
//! A byte cannot distinguish BREAK from NUL: drivers supply BREAK metadata
//! with `receive_break`. The kernel owns timers per reader, drains output
//! before applying TCSADRAIN/TCSAFLUSH, and delivers signals and wakeups.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::VecDeque;
use core::num::NonZeroUsize;
use linux_raw_sys::general::*;

mod output;
mod readiness;
mod termios;
use output::is_continuation;
pub use output::{BackgroundWrite, Output, WriterFacts, Written, background_write_signal};
pub use readiness::{Decision, Readiness, TimerKind, TimerState, ready};
pub use termios::{Termios, Winsize};

/// Physical input slots, including the canonical terminator/overflow slot.
/// Raw mode admits 4095 bytes. Canonical lines retain 4095 plus a terminator.
pub const BUFFER_SIZE: usize = 4096;
const BS: u8 = 8;
const BEL: u8 = 7;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoMemory;

/// Edge notifications: polling became readable, or ordinary input room went
/// from zero to nonzero. The caller must wake the corresponding wait queues.
/// Raw readers with short destinations also inspect Input::Stored; those
/// with inter-byte timers inspect timer_restart, independently of poll edges.
#[must_use]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Wake {
    pub readers: bool,
    pub writers: bool,
}

/// A signal to the foreground process group. When flush is true, queued
/// discipline input and the driver's output were flushed before this call's
/// echo. Bytes not yet supplied to receive are still delivered normally.
#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Signal {
    pub number: u32,
    pub flush: bool,
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    Stored,
    Ignored,
    Dropped,
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Echo {
    Complete,
    Truncated,
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Effects {
    pub input: Input,
    pub echo: Echo,
    pub signal: Option<Signal>,
    pub wake: Wake,
    /// A raw byte was stored with VMIN>0 and VTIME>0. Each blocked reader
    /// owns and rearms its own inter-byte timer; this crate stores no timer.
    pub timer_restart: bool,
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    Bytes(NonZeroUsize),
    Eof,
    Nothing,
}

#[must_use]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Applied {
    pub flush_input: bool,
    pub wake: Wake,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Plain,
    LineEnd,
    Eof,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    byte: u8,
    kind: Kind,
    width: u8,
}

struct EchoOutput<'a> {
    out: &'a mut dyn Output,
    status: Echo,
}

/// Terminal state whose caller serializes access with its device output
/// queue. No internal locks, timers or staged ioctl requests are retained.
/// Reads return at most one completed canonical record; short reads retain
/// its remainder. Input entries occupy three bytes each.
pub struct LineDiscipline {
    termios: Termios,
    input: VecDeque<Entry>,
    pending: usize,
    records: usize,
    literal: bool,
    column: usize,
    winsize: Winsize,
}

impl LineDiscipline {
    /// Reserves all input storage or returns NoMemory without panicking on
    /// allocation failure. Subsequent operations do not grow this allocation.
    pub fn try_new(termios: Termios) -> Result<Self, NoMemory> {
        Self::with_capacity(termios, BUFFER_SIZE)
    }

    fn with_capacity(termios: Termios, capacity: usize) -> Result<Self, NoMemory> {
        let mut input = VecDeque::new();
        input.try_reserve_exact(capacity).map_err(|_| NoMemory)?;
        Ok(Self { termios, input, pending: 0, records: 0, literal: false, column: 0, winsize: Winsize::default() })
    }

    pub fn termios(&self) -> Termios {
        self.termios
    }
    pub fn winsize(&self) -> Winsize {
        self.winsize
    }

    /// True exactly when any of the four dimensions changed (send SIGWINCH).
    #[must_use]
    pub fn set_winsize(&mut self, winsize: Winsize) -> bool {
        let changed = self.winsize != winsize;
        self.winsize = winsize;
        changed
    }

    /// Ordinary bytes that fit without consuming the delimiter reserve or
    /// replacing overflow input. Zero does not prohibit editing, signals or
    /// a canonical delimiter: use accepts for a particular next byte.
    pub fn input_room(&self) -> usize {
        (BUFFER_SIZE - 1).saturating_sub(self.input.len())
    }

    /// Whether consuming this byte avoids input loss. Mapping, quoting and
    /// disabled controls match receive. Editing/signals/IGNCR are accepted
    /// even with no room. False asks a PTY master to leave this byte queued;
    /// receive may still retain it in the canonical overflow slot if forced.
    pub fn accepts(&self, mut byte: u8) -> bool {
        if self.input_flag(ISTRIP) {
            byte &= 0x7f;
        }
        if self.literal {
            return self.input_room() != 0;
        }
        if self.signal_number(byte).is_some() {
            return true;
        }
        if byte == b'\r' && self.input_flag(IGNCR) {
            return true;
        }
        let (byte, _) = self.map_input(byte);
        if self.local(ICANON) {
            if self.cc(VERASE, byte)
                || self.cc(VKILL, byte)
                || self.ext_cc(VWERASE, byte)
                || self.ext_cc(VLNEXT, byte)
                || (self.local(ECHO) && self.ext_cc(VREPRINT, byte))
            {
                return true;
            }
            if self.kind(byte) != Kind::Plain {
                return self.can_store();
            }
        }
        self.input_room() != 0
    }

    /// Poll/select readiness, independent of any reader's timer or buffer:
    /// canonical records (including EOF), otherwise at least one byte, or
    /// VMIN bytes when VTIME=0. An empty raw queue is never poll-readable.
    pub fn poll_readable(&self) -> bool {
        if self.local(ICANON) {
            return self.records != 0;
        }
        let min = if self.termios.c_cc[VTIME as usize] == 0 {
            usize::from(self.termios.c_cc[VMIN as usize]).max(1)
        } else {
            1
        };
        self.input.len() >= min
    }

    pub fn read_decision(&self, dest_len: usize, timer: TimerState) -> Decision {
        if dest_len == 0 {
            return Decision::Raw(Readiness::ReturnNow(0));
        }
        if self.local(ICANON) {
            if self.records == 0 {
                Decision::Wait
            } else if self.input.front().is_some_and(|e| e.kind == Kind::Eof) {
                Decision::Eof
            } else {
                Decision::Line
            }
        } else {
            let vmin = usize::from(self.termios.c_cc[VMIN as usize]).min(dest_len) as u8;
            Decision::Raw(ready(vmin, self.termios.c_cc[VTIME as usize], self.input.len().min(dest_len), timer))
        }
    }

    /// Applies settings immediately. The kernel drains output first for
    /// TCSADRAIN/TCSAFLUSH; pass flush=true for TCSAFLUSH. An interrupted drain
    /// must never call this method. No state belonging to readers is changed.
    pub fn set_termios(&mut self, new: Termios, flush: bool) -> Applied {
        let before = self.edges();
        let was_canon = self.local(ICANON);
        if flush {
            self.flush_input();
        }
        self.termios = new;
        if was_canon != self.local(ICANON) {
            // EOF's __DISABLED_CHAR survives canonical -> raw as NUL.
            for entry in &mut self.input {
                entry.kind = Kind::Plain;
            }
            self.reset_line_state();
            if self.local(ICANON)
                && let Some(last) = self.input.back_mut()
            {
                last.kind = Kind::LineEnd;
                self.records = 1;
            }
        }
        Applied { flush_input: flush, wake: self.wake_since(before) }
    }

    fn edges(&self) -> (bool, bool) {
        (self.poll_readable(), self.input_room() != 0)
    }
    fn wake_since(&self, before: (bool, bool)) -> Wake {
        let after = self.edges();
        Wake { readers: !before.0 && after.0, writers: !before.1 && after.1 }
    }
    fn local(&self, flag: u32) -> bool {
        self.termios.c_lflag & flag != 0
    }
    fn local_all(&self, flags: u32) -> bool {
        self.termios.c_lflag & flags == flags
    }
    fn input_flag(&self, flag: u32) -> bool {
        self.termios.c_iflag & flag != 0
    }
    fn cc(&self, index: u32, byte: u8) -> bool {
        byte != 0 && self.termios.c_cc[index as usize] == byte
    }
    fn ext_cc(&self, index: u32, byte: u8) -> bool {
        self.local(IEXTEN) && self.cc(index, byte)
    }
    fn reset_line_state(&mut self) {
        self.pending = 0;
        self.records = 0;
        self.literal = false;
    }
    fn flush_input(&mut self) {
        self.input.clear();
        self.reset_line_state();
    }
    fn signal_number(&self, byte: u8) -> Option<u32> {
        if !self.local(ISIG) {
            None
        } else if self.cc(VINTR, byte) {
            Some(SIGINT)
        } else if self.cc(VQUIT, byte) {
            Some(SIGQUIT)
        } else if self.cc(VSUSP, byte) {
            Some(SIGTSTP)
        } else {
            None
        }
    }
    fn signal(&mut self, number: u32, out: &mut dyn Output) -> Signal {
        let flush = !self.local(NOFLSH);
        if flush {
            self.flush_input();
            out.flush();
        }
        Signal { number, flush }
    }
    fn effects(&self, before: (bool, bool), input: Input, echo: Echo, signal: Option<Signal>) -> Effects {
        Effects {
            input,
            echo,
            signal,
            wake: self.wake_since(before),
            timer_restart: input == Input::Stored
                && !self.local(ICANON)
                && self.termios.c_cc[VMIN as usize] != 0
                && self.termios.c_cc[VTIME as usize] != 0,
        }
    }

    /// BREAK consumes literal-next, even when ignored. IGNBRK wins over
    /// BRKINT, which sends SIGINT without needing ISIG and respects NOFLSH.
    /// Otherwise a NUL is stored without echo. PARMRK is outside scope.
    pub fn receive_break(&mut self, out: &mut dyn Output) -> Effects {
        let before = self.edges();
        self.literal = false;
        let mut echo = EchoOutput { out, status: Echo::Complete };
        let (input, signal) = if self.input_flag(IGNBRK) {
            (Input::Ignored, None)
        } else if self.input_flag(BRKINT) {
            (Input::Ignored, Some(self.signal(SIGINT, echo.out)))
        } else {
            (self.store(0, Kind::Plain, 0, &mut echo), None)
        };
        self.effects(before, input, echo.status, signal)
    }

    /// Processes a byte without allocating. Input reports retention separately
    /// from echo truncation. ISTRIP precedes signals and literal-next bypasses
    /// mapping/editing/signals. Disabled control characters never match NUL.
    pub fn receive(&mut self, mut byte: u8, out: &mut dyn Output) -> Effects {
        let before = self.edges();
        let mut echo = EchoOutput { out, status: Echo::Complete };
        if self.input_flag(ISTRIP) {
            byte &= 0x7f;
        }
        let (input, signal) = if self.literal {
            self.literal = false;
            (self.store_echoed(byte, Kind::Plain, false, &mut echo), None)
        } else if let Some(number) = self.signal_number(byte) {
            let signal = self.signal(number, echo.out);
            if self.local(ECHO) {
                self.echo_char(byte, &mut echo);
            }
            (Input::Ignored, Some(signal))
        } else if byte == b'\r' && self.input_flag(IGNCR) {
            (Input::Ignored, None)
        } else {
            let (byte, mapped_cr) = self.map_input(byte);
            (self.receive_mapped(byte, mapped_cr, &mut echo), None)
        };
        self.effects(before, input, echo.status, signal)
    }

    fn map_input(&self, byte: u8) -> (u8, bool) {
        if byte == b'\r' && self.input_flag(ICRNL) {
            (b'\n', true)
        } else if byte == b'\n' && self.input_flag(INLCR) {
            (b'\r', false)
        } else {
            (byte, false)
        }
    }
    fn kind(&self, byte: u8) -> Kind {
        if self.cc(VEOF, byte) {
            Kind::Eof
        } else if byte == b'\n' || self.cc(VEOL, byte) || self.ext_cc(VEOL2, byte) {
            Kind::LineEnd
        } else {
            Kind::Plain
        }
    }
    fn receive_mapped(&mut self, byte: u8, mapped_cr: bool, echo: &mut EchoOutput<'_>) -> Input {
        if self.local(ICANON) {
            if self.cc(VERASE, byte) {
                self.erase_char(byte, echo);
            } else if self.ext_cc(VWERASE, byte) {
                self.erase_word(echo);
            } else if self.cc(VKILL, byte) {
                self.kill(byte, echo);
            } else if self.ext_cc(VLNEXT, byte) {
                self.literal = true;
                if self.local_all(ECHO | ECHOCTL) {
                    self.emit(&[b'^', BS], echo);
                }
            } else if self.local(ECHO) && self.ext_cc(VREPRINT, byte) {
                self.reprint(byte, echo);
            } else {
                let kind = self.kind(byte);
                return self.store_echoed(if kind == Kind::Eof { 0 } else { byte }, kind, byte == b'\n', echo);
            }
            Input::Ignored
        } else {
            self.store_echoed(byte, Kind::Plain, mapped_cr, echo)
        }
    }

    fn can_store(&self) -> bool {
        if self.local(ICANON) && self.records == 0 { true } else { self.input.len() < BUFFER_SIZE - 1 }
    }
    fn overflow_bell(&mut self, echo: &mut EchoOutput<'_>) {
        if self.input_flag(IMAXBEL) {
            self.emit(&[BEL], echo);
        }
    }
    fn store_echoed(&mut self, byte: u8, kind: Kind, literal_newline: bool, echo: &mut EchoOutput<'_>) -> Input {
        if !self.can_store() {
            self.overflow_bell(echo);
            return Input::Dropped;
        }
        let column = self.column;
        if kind != Kind::Eof
            && (self.local(ECHO) || (self.local_all(ICANON | ECHONL) && byte == b'\n' && literal_newline))
        {
            if literal_newline && byte == b'\n' {
                self.emit(&[byte], echo);
            } else {
                self.echo_char(byte, echo);
            }
        }
        let width = self.column.saturating_sub(column).min(8) as u8;
        self.store(byte, kind, width, echo)
    }
    fn store(&mut self, byte: u8, kind: Kind, width: u8, echo: &mut EchoOutput<'_>) -> Input {
        if !self.can_store() {
            self.overflow_bell(echo);
            return Input::Dropped;
        }
        if self.input.len() == BUFFER_SIZE {
            // No completed line: the last slot tracks the latest overflow
            // character until a delimiter replaces it and publishes the line.
            self.input.pop_back();
            self.pending -= 1;
            self.overflow_bell(echo);
        }
        self.input.push_back(Entry { byte, kind, width });
        if self.local(ICANON) {
            if kind != Kind::Plain {
                self.pending = 0;
                self.records += 1;
            } else {
                self.pending += 1;
            }
        }
        Input::Stored
    }

    fn last_character(&self) -> Option<usize> {
        let floor = self.input.len() - self.pending;
        // No lead byte means no partial erasure.
        (floor..self.input.len()).rev().find(|&i| !self.input_flag(IUTF8) || !is_continuation(self.input[i].byte))
    }
    fn erase_char(&mut self, byte: u8, echo: &mut EchoOutput<'_>) {
        let Some(start) = self.last_character() else {
            return;
        };
        self.remove_character(start, self.local(ECHOE), echo);
        if self.local(ECHO) && !self.local(ECHOE) {
            self.echo_char(byte, echo);
        }
    }
    fn erase_word(&mut self, echo: &mut EchoOutput<'_>) {
        let mut seen_word = false;
        for _ in 0..self.pending {
            let Some(start) = self.last_character() else {
                break;
            };
            let byte = self.input[start].byte;
            // Linux lib/ctype.c's Latin-1 alnum classification.
            let word = byte.is_ascii_alphanumeric() || byte == b'_' || (byte >= 0xc0 && byte != 0xd7 && byte != 0xf7);
            if seen_word && !word {
                break;
            }
            seen_word |= word;
            self.remove_character(start, true, echo);
        }
    }
    fn remove_character(&mut self, start: usize, visual: bool, echo: &mut EchoOutput<'_>) {
        let byte = self.input[start].byte;
        let width = if byte == b'\t' {
            self.input[start].width
        } else if byte.is_ascii_control() {
            if self.local(ECHOCTL) { 2 } else { 0 }
        } else {
            1
        };
        self.pending -= self.input.len() - start;
        self.input.truncate(start);
        if self.local(ECHO) && visual {
            for _ in 0..width {
                if byte == b'\t' {
                    self.emit(&[BS], echo);
                } else {
                    self.emit(&[BS, b' ', BS], echo);
                }
            }
        }
    }
    fn kill(&mut self, byte: u8, echo: &mut EchoOutput<'_>) {
        if self.pending == 0 {
            return;
        }
        if self.local_all(ECHO | ECHOE | ECHOK | ECHOKE) {
            for _ in 0..self.pending {
                let Some(start) = self.last_character() else {
                    break;
                };
                self.remove_character(start, true, echo);
            }
        } else {
            self.input.truncate(self.input.len() - self.pending);
            self.pending = 0;
            if self.local(ECHO) {
                self.echo_char(byte, echo);
                if self.local(ECHOK) {
                    self.emit(b"\n", echo);
                }
            }
        }
    }
    fn reprint(&mut self, byte: u8, echo: &mut EchoOutput<'_>) {
        self.echo_char(byte, echo);
        self.emit(b"\n", echo);
        for i in self.input.len() - self.pending..self.input.len() {
            let byte = self.input[i].byte;
            let column = self.column;
            self.echo_char(byte, echo);
            self.input[i].width = self.column.saturating_sub(column).min(8) as u8;
        }
    }
    fn echo_char(&mut self, byte: u8, echo: &mut EchoOutput<'_>) {
        if self.local(ECHOCTL) && byte.is_ascii_control() && byte != b'\t' {
            self.emit(&[b'^', byte ^ 0x40], echo);
        } else {
            self.emit(&[byte], echo);
        }
    }
    fn emit(&mut self, unit: &[u8], echo: &mut EchoOutput<'_>) {
        if echo.status == Echo::Complete
            && !output::push_unit(echo.out, unit, self.termios.c_oflag, self.input_flag(IUTF8), &mut self.column)
        {
            echo.status = Echo::Truncated;
        }
    }

    /// Copies available input without sleeping, returning the read outcome
    /// and wake edges. Nothing means empty/incomplete input or an empty
    /// destination; only a consumed canonical EOF record yields Eof. Raw
    /// callers use read_decision first to implement VMIN/VTIME waits.
    #[must_use = "handle EOF separately and wake blocked writers"]
    pub fn read(&mut self, buf: &mut [u8]) -> (Read, Wake) {
        let before = self.edges();
        if buf.is_empty() || (self.local(ICANON) && self.records == 0) {
            return (Read::Nothing, Wake::default());
        }
        let mut n = 0;
        let mut eof = false;
        for _ in 0..self.input.len() {
            let entry = self.input[0];
            if n == buf.len() && entry.kind != Kind::Eof {
                break;
            }
            self.input.pop_front();
            if entry.kind != Kind::Eof {
                buf[n] = entry.byte;
                n += 1;
            }
            if entry.kind != Kind::Plain {
                self.records -= 1;
                eof = entry.kind == Kind::Eof;
                break;
            }
        }
        let result = if let Some(n) = NonZeroUsize::new(n) {
            Read::Bytes(n)
        } else if eof {
            Read::Eof
        } else {
            Read::Nothing
        };
        (result, self.wake_since(before))
    }

    /// Accepts a prefix without allocating. Each transformed source byte is
    /// atomic against sink room; retry bytes[consumed..] after backpressure.
    pub fn write(&mut self, bytes: &[u8], out: &mut dyn Output) -> Written {
        let mut consumed = 0;
        for &byte in bytes {
            if !output::push_unit(out, &[byte], self.termios.c_oflag, self.input_flag(IUTF8), &mut self.column) {
                break;
            }
            consumed += 1;
        }
        Written { consumed }
    }

    #[cfg(test)]
    fn check_invariants(&self) {
        assert!(self.input.len() <= BUFFER_SIZE);
        assert!(self.input.capacity() >= BUFFER_SIZE);
        let records = self.input.iter().filter(|e| e.kind != Kind::Plain).count();
        assert_eq!(self.records, records);
        let pending =
            if self.local(ICANON) { self.input.iter().rev().take_while(|e| e.kind == Kind::Plain).count() } else { 0 };
        assert_eq!(self.pending, pending);
        if !self.local(ICANON) {
            assert_eq!(records, 0);
        }
        assert!(self.input.iter().all(|e| e.width <= 8 && (e.kind != Kind::Eof || e.byte == 0)));
    }
}

#[cfg(test)]
impl Default for LineDiscipline {
    fn default() -> Self {
        Self::try_new(Termios::linux_default()).unwrap()
    }
}

#[cfg(test)]
mod tests;
