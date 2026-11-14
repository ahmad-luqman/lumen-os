//! The terminal on the framebuffer: `lumen-term`'s grid drawn below the
//! header, fed by log lines (coloured by level) and kmon.
//!
//! Drawing keeps a copy of every cell it drew (the shadow) and draws only
//! the cells that differ from it, in the rows the terminal marked as changed.
//! A scroll moves the pixels (`Framebuffer::move_rows`) and the shadow
//! together, so a line feed at the bottom draws one new line, not the whole
//! screen. The cursor flips every 500 ms (the boot CPU's timer interrupt
//! does it: no thread, so nothing switches or traces for it) and shows
//! solid while output arrives; it isn't drawn while the view is scrolled
//! back (Shift+PageUp/PageDown in kmon).
//!
//! Nothing allocates under the screen's lock (the terminal and the shadow
//! are allocated in [`init`]): log lines come while the frame allocator's or
//! the page tables' lock is held, and the heap would take them, so the
//! arguments of a log line (formatted under the lock) and of
//! [`write_fmt`] must not allocate either.
//!
//! The terminal takes `\n` as a line feed only: [`write_str`], [`write_fmt`]
//! and log lines turn it into CR LF; [`write_bytes`] and [`write_raw_fmt`]
//! pass bytes as they are.

use crate::fb::{self, Framebuffer, Style};
use crate::sync::SpinLock;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use log::Level;
use lumen_term::{Cell, Color, Cursor, Flags, Terminal, palette};
use noto_sans_mono_bitmap::{FontWeight, get_raster_width};

/// Lines kept above the screen.
pub const SCROLLBACK: usize = 500;
/// How long the cursor stays on, then off, in the boot CPU's timer ticks
/// (1 kHz).
const BLINK_TICKS: u64 = 500;
const CELL_H: usize = 16;

/// The terminal and what is drawn of it.
struct Screen {
    fb: Framebuffer,
    term: Terminal,
    /// What each cell of the view was last drawn as (row by row).
    shadow: Vec<Cell>,
    /// Top-left pixel of the grid.
    x0: usize,
    y0: usize,
    cell_w: usize,
    /// Where the cursor is drawn, if it is.
    cursor_drawn: Option<(usize, usize)>,
    /// The blink phase: the cursor is drawn while on.
    blink_on: bool,
    /// Cells drawn so far (kmon's `term` shows it; the tests count redraws
    /// with it).
    cells_drawn: u64,
}

static SCREEN: SpinLock<Option<Screen>> = SpinLock::new(None);
/// The CPU drawing (holding `SCREEN`) plus one; 0 when none.
static DRAWING: AtomicUsize = AtomicUsize::new(0);
/// Set when a crash report starts: no drawing starts afterwards (one under
/// way on another CPU runs until that CPU is stopped).
static STOPPED: AtomicBool = AtomicBool::new(false);
/// The boot CPU's tick at which the cursor flips next (output moves it on).
static NEXT_BLINK: AtomicU64 = AtomicU64::new(0);

/// Start the terminal below the header of `fb` (called by `fb::init`).
pub fn init(fb: Framebuffer) {
    let cell_w = get_raster_width(FontWeight::Regular, fb::CONSOLE_SIZE)
        .max(get_raster_width(FontWeight::Bold, fb::CONSOLE_SIZE));
    let (x0, y0) = (fb::MARGIN, fb::CONSOLE_TOP);
    let cols = fb.width().saturating_sub(x0 + fb::MARGIN / 2) / cell_w;
    let rows = fb.height().saturating_sub(y0) / CELL_H;
    if cols == 0 || rows == 0 {
        log::warn!(target: "term", "no room for a terminal on a {}x{} screen", fb.width(), fb.height());
        return;
    }
    // Allocated here, before taking the lock (see the module comment).
    let term = Terminal::new(cols, rows, SCROLLBACK);
    let shadow = vec![Cell::BLANK; cols * rows];
    let screen = Screen { fb, term, shadow, x0, y0, cell_w, cursor_drawn: None, blink_on: true, cells_drawn: 0 };
    *SCREEN.lock() = Some(screen);
    log::info!(target: "term", "terminal {cols}x{rows}, {SCROLLBACK} lines of scrollback");
}

/// This CPU's value in [`DRAWING`].
fn drawing_id() -> usize {
    crate::arch::cpu_index() + 1
}

/// Whether this CPU holds the screen's lock (the heap asserts it doesn't:
/// allocating under it could deadlock with a CPU logging under the frame
/// allocator's lock).
pub fn drawing_here() -> bool {
    let d = DRAWING.load(Ordering::Relaxed);
    d != 0 && d == drawing_id()
}

/// Run `f` on `screen`, marked as this CPU's drawing (the lock is held).
fn drawing<R>(screen: &mut Screen, f: impl FnOnce(&mut Screen) -> R) -> R {
    DRAWING.store(drawing_id(), Ordering::Relaxed);
    let r = f(screen);
    DRAWING.store(0, Ordering::Relaxed);
    r
}

/// Run `f` on the screen, unless there is none, a crash report started, or
/// this CPU is already drawing (an exception in the middle of it: waiting
/// would never end; nothing on that path logs to the screen today).
fn with_screen<R>(f: impl FnOnce(&mut Screen) -> R) -> Option<R> {
    if STOPPED.load(Ordering::Acquire) {
        return None;
    }
    if drawing_here() {
        return None;
    }
    let mut guard = SCREEN.lock();
    Some(drawing(guard.as_mut()?, f))
}

/// A crash report started: draw nothing more (the crash screen draws
/// straight to the pixels).
pub fn stop() {
    STOPPED.store(true, Ordering::Release);
}

/// The boot CPU's timer tick `tick` (in its interrupt handler): flip the
/// cursor every [`BLINK_TICKS`]. It never waits: if another CPU is drawing,
/// the flip is retried on the next tick.
pub fn on_tick(tick: u64) {
    if tick < NEXT_BLINK.load(Ordering::Relaxed) || STOPPED.load(Ordering::Acquire) {
        return;
    }
    let Some(mut guard) = SCREEN.try_lock() else { return };
    // The next flip, also without a terminal (yet): look again a blink
    // later. (Output moves it on only while holding the lock, like here.)
    NEXT_BLINK.store(tick + BLINK_TICKS, Ordering::Relaxed);
    let Some(s) = guard.as_mut() else { return };
    // (This CPU can't be drawing already: it does so with interrupts off.)
    drawing(s, |s| {
        s.blink_on = !s.blink_on;
        s.render();
    });
}

impl Screen {
    /// Feed `s` to the terminal, `\n` as CR LF (not drawn yet).
    fn feed(&mut self, s: &str) {
        for (i, line) in s.split('\n').enumerate() {
            if i > 0 {
                self.term.write(b"\r\n");
            }
            self.term.write(line.as_bytes());
        }
        self.show_cursor();
    }

    /// Feed `bytes` to the terminal as they are (not drawn yet).
    fn feed_raw(&mut self, bytes: &[u8]) {
        self.term.write(bytes);
        self.show_cursor();
    }

    /// Output shows the cursor at once, for a whole blink.
    fn show_cursor(&mut self) {
        self.blink_on = true;
        NEXT_BLINK.store(crate::time::ticks() + BLINK_TICKS, Ordering::Relaxed);
    }

    /// Draw what changed since the last time.
    fn render(&mut self) {
        let cols = self.term.cols();
        // Take the cursor off first, as the shadow has the cell (the
        // terminal may already hold other cells there, before the scroll
        // below moves the pixels).
        if let Some((c, r)) = self.cursor_drawn.take() {
            self.draw(c, r, self.shadow[r * cols + c], false);
        }
        if let Some(s) = self.term.take_scroll() {
            let n = s.lines.unsigned_abs();
            let (top, bottom) = (self.y0 + s.top * CELL_H, self.y0 + s.bottom * CELL_H);
            let len = (s.bottom - s.top - n) * CELL_H;
            let shift = n * CELL_H;
            let rows = s.top * cols..s.bottom * cols;
            if s.lines > 0 {
                self.fb.move_rows(top + shift, top, len);
                self.shadow[rows].copy_within(n * cols.., 0);
            } else {
                self.fb.move_rows(top, top + shift, len);
                let end = rows.len() - n * cols;
                self.shadow[rows].copy_within(..end, n * cols);
            }
            debug_assert!(bottom <= self.fb.height());
        }
        for r in 0..self.term.rows() {
            if !self.term.take_dirty(r) {
                continue;
            }
            for c in 0..cols {
                let cell = self.term.cell(c, r);
                if self.shadow[r * cols + c] != cell {
                    self.draw(c, r, cell, false);
                    self.shadow[r * cols + c] = cell;
                }
            }
        }
        if self.blink_on
            && self.term.view_offset() == 0
            && let Some((c, r)) = self.term.cursor_in_view()
        {
            self.draw(c, r, self.term.cell(c, r), true);
            self.cursor_drawn = Some((c, r));
        }
    }

    /// Draw `cell` at (`col`, `row`), colours swapped for inverse or for the
    /// cursor (not for both).
    fn draw(&mut self, col: usize, row: usize, cell: Cell, cursor: bool) {
        let (mut fg, mut bg) = (rgb(cell.fg, fb::TEXT), rgb(cell.bg, fb::BG));
        if cell.flags.contains(Flags::INVERSE) != cursor {
            core::mem::swap(&mut fg, &mut bg);
        }
        let (x, y, w) = (self.x0 + col * self.cell_w, self.y0 + row * CELL_H, self.cell_w);
        if cell.ch == ' ' || cell.flags.contains(Flags::SPACER) {
            self.fb.fill_rect(x, y, w, CELL_H, bg);
        } else {
            let weight = if cell.flags.contains(Flags::BOLD) { FontWeight::Bold } else { FontWeight::Regular };
            let drawn = self.fb.glyph_clipped(x, y, cell.ch, Style::new(weight, fb::CONSOLE_SIZE, fg, bg), w);
            if drawn < w {
                self.fb.fill_rect(x + drawn, y, w - drawn, CELL_H, bg);
            }
        }
        self.cells_drawn += 1;
    }
}

fn rgb(color: Color, default: u32) -> u32 {
    match color {
        Color::Default => default,
        Color::Indexed(i) => palette::rgb(i),
    }
}

/// Feeds the terminal through `fmt::Write` (`\n` as CR LF), drawing nothing.
struct Feed<'a>(&'a mut Screen);

impl Write for Feed<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.feed(s);
        Ok(())
    }
}

/// Feeds the terminal through `fmt::Write`, bytes as they are.
struct Raw<'a>(&'a mut Screen);

impl Write for Raw<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.feed_raw(s.as_bytes());
        Ok(())
    }
}

/// Write `s` to the terminal (`\n` as CR LF) and draw it; false if nothing
/// was drawn (no terminal, a crash report started, or this CPU is already
/// drawing).
pub fn write_str(s: &str) -> bool {
    write_fmt(format_args!("{s}"))
}

/// Write raw bytes to the terminal (no `\n` translation) and draw them;
/// false if nothing was drawn (see [`write_str`]).
pub fn write_bytes(bytes: &[u8]) -> bool {
    with_screen(|screen| {
        screen.feed_raw(bytes);
        screen.render();
    })
    .is_some()
}

/// Write formatted text (`\n` as CR LF) and draw it, all under one lock: a
/// log line from another CPU can't land inside an escape sequence written
/// in this one call. `args`
/// must not allocate (see the module comment). False if nothing was drawn
/// (see [`write_str`]).
pub fn write_fmt(args: fmt::Arguments<'_>) -> bool {
    with_screen(|screen| {
        let _ = Feed(screen).write_fmt(args);
        screen.render();
    })
    .is_some()
}

/// [`write_fmt`] with bytes as they are (no `\n` translation).
pub fn write_raw_fmt(args: fmt::Arguments<'_>) -> bool {
    with_screen(|screen| {
        let _ = Raw(screen).write_fmt(args);
        screen.render();
    })
    .is_some()
}

/// The SGR colours of a log line's level word (and message, for errors and
/// warnings).
fn level_sgr(level: Level) -> (&'static str, &'static str) {
    match level {
        Level::Error => ("1;91", "91"),
        Level::Warn => ("93", "93"),
        Level::Info => ("92", "39"),
        Level::Debug | Level::Trace => ("36", "39"),
    }
}

/// Show a log line, without the serial timestamp (so screenshots stay
/// comparable): the level coloured, the tag dimmed (called by the logger).
/// It starts from the default colours, whatever kmon's output left set.
pub fn console_log(level: Level, tag: &str, args: &fmt::Arguments<'_>) {
    let (lvl, msg) = level_sgr(level);
    write_fmt(format_args!("\x1b[0;{lvl}m{level:<5}\x1b[0m \x1b[90m{tag}:\x1b[0m \x1b[{msg}m{args}\x1b[0m\n"));
}

/// Move the view with `scroll` (which returns whether it moved) and draw
/// it; a move is logged (debug: serial only, the screen shows up to `info`).
fn move_view(scroll: impl FnOnce(&mut Terminal) -> bool) {
    let moved = with_screen(|s| {
        if !scroll(&mut s.term) {
            return None;
        }
        s.render();
        Some((s.term.view_offset(), top_row(&s.term)))
    });
    // Logged outside the screen's lock: the logger takes its line lock
    // before the screen's, so logging under the screen's lock could
    // deadlock with another CPU's log line.
    if let Some(Some((view, top))) = moved {
        log::debug!(target: "term", "view {view} lines up, top row \"{top}\"");
    }
}

/// Scroll the view back (`pages` > 0) or forward by `pages` half screens.
pub fn scroll_half_pages(pages: isize) {
    move_view(|t| t.scroll_view(pages * (t.rows() as isize / 2).max(1)));
}

/// Show the screen again (typing does).
pub fn reset_view() {
    move_view(Terminal::reset_view);
}

/// The start of a row of the view, as text (for kmon's `term`).
#[derive(Clone, Copy)]
pub struct RowText {
    chars: [char; 32],
    len: usize,
}

impl fmt::Display for RowText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.chars[..self.len].iter().try_for_each(|&c| f.write_char(c))
    }
}

/// The view's top row: its first 32 characters (wide characters' spacers
/// skipped), trailing blanks trimmed.
fn top_row(t: &Terminal) -> RowText {
    let mut row = RowText { chars: [' '; 32], len: 0 };
    let cells = (0..t.cols()).map(|c| t.cell(c, 0)).filter(|cell| !cell.flags.contains(Flags::SPACER));
    for (i, cell) in cells.take(row.chars.len()).enumerate() {
        row.chars[i] = cell.ch;
        if cell.ch != ' ' {
            row.len = i + 1;
        }
    }
    row
}

/// What kmon's `term` shows.
pub struct Status {
    pub cols: usize,
    pub rows: usize,
    pub cursor: Cursor,
    pub scrollback: usize,
    pub capacity: usize,
    pub view: usize,
    /// The view's top row (see [`top_row`]).
    pub top_row: RowText,
    pub cells_drawn: u64,
}

/// The terminal's state, or `None` without one (or once a crash report
/// started).
pub fn status() -> Option<Status> {
    with_screen(|s| {
        let t = &s.term;
        Status {
            cols: t.cols(),
            rows: t.rows(),
            cursor: t.cursor(),
            scrollback: t.scrollback_len(),
            capacity: t.scrollback_capacity(),
            view: t.view_offset(),
            top_row: top_row(t),
            cells_drawn: s.cells_drawn,
        }
    })
}

/// The view's cells, row by row (for the tests). The vector is allocated
/// before taking the screen's lock, as everything under it must not allocate.
#[cfg(feature = "ktest")]
fn view_cells() -> Option<(usize, Vec<Cell>)> {
    let (cols, rows) = with_screen(|s| (s.term.cols(), s.term.rows()))?;
    let mut cells = Vec::with_capacity(cols * rows);
    with_screen(|s| {
        let t = &s.term;
        // The size never changes: this fits the capacity.
        cells.extend((0..rows).flat_map(|r| (0..cols).map(move |c| t.cell(c, r))));
    })?;
    Some((cols, cells))
}

/// The bottom-most row of the view holding `needle`.
#[cfg(feature = "ktest")]
fn find_row<'a>(cols: usize, cells: &'a [Cell], needle: &str) -> Option<&'a [Cell]> {
    cells.chunks(cols).rev().find(|row| {
        let text: alloc::string::String = row.iter().map(|c| c.ch).collect();
        text.contains(needle)
    })
}

crate::ktest!(
    fn log_lines_are_coloured_by_level() {
        use crate::{kassert, kassert_eq};
        log::warn!(target: "termtest", "a yellow warning");
        log::info!(target: "termtest", "plain information");
        // (An error straight to the screen: an ERROR line on serial would
        // look like a real one.)
        console_log(Level::Error, "termtest", &format_args!("a red error"));
        let (cols, cells) = view_cells().ok_or("no terminal")?;
        let warn = find_row(cols, &cells, "a yellow warning").ok_or("the warning isn't on screen")?;
        let info = find_row(cols, &cells, "plain information").ok_or("the information isn't on screen")?;
        let error = find_row(cols, &cells, "a red error").ok_or("the error isn't on screen")?;
        // `ERROR termtest: a red error`, the level bold.
        kassert_eq!((error[0].ch, error[0].fg), ('E', Color::Indexed(9)));
        kassert!(error[0].flags.contains(Flags::BOLD) && !error[16].flags.contains(Flags::BOLD));
        kassert_eq!((error[16].ch, error[16].fg), ('a', Color::Indexed(9)));
        let yellow = Color::Indexed(11);
        // `WARN  termtest: a yellow warning`
        kassert_eq!(warn[0].ch, 'W');
        kassert_eq!(warn[0].fg, yellow);
        kassert_eq!(warn[6].ch, 't');
        kassert_eq!(warn[6].fg, Color::Indexed(8));
        kassert_eq!(warn[16].ch, 'a');
        kassert_eq!(warn[16].fg, yellow);
        kassert_eq!(info[0].fg, Color::Indexed(10));
        kassert_eq!(info[16].ch, 'p');
        kassert_eq!(info[16].fg, Color::Default);
        kassert!(warn.iter().chain(info).all(|c| c.bg == Color::Default));
        Ok(())
    }
);

crate::ktest!(
    fn only_changed_cells_are_drawn() {
        // Drawing compares every changed row with what was drawn: writing
        // two characters draws them (plus the cursor, off then on); a line
        // feed at the bottom moves the pixels and draws only the new line's
        // differing cells, not the screen; drawing again with nothing new
        // draws only the cursor.
        use crate::kassert;
        let rows = with_screen(|s| s.term.rows()).ok_or("no terminal")?;
        let setup = alloc::format!("\x1b[{rows};1H\x1b[2K");
        let counts = with_screen(|s| {
            let mut count = |bytes: &[u8]| {
                let before = s.cells_drawn;
                s.term.write(bytes);
                s.render();
                s.cells_drawn - before
            };
            count(setup.as_bytes());
            let two = count(b"ab");
            let feed = count(b"\r\n");
            let idle = count(b"");
            (two, feed, idle, s.term.cols())
        });
        let (two, feed, idle, cols) = counts.ok_or("no terminal")?;
        crate::ktest::diag(format_args!("two characters: {two} cells, a line feed: {feed}, nothing: {idle}"));
        kassert!(two <= 4);
        kassert!(feed < cols as u64);
        kassert!(idle <= 2);
        Ok(())
    }
);

crate::ktest!(
    fn the_cursor_blinks() {
        // The boot CPU's timer turns the cursor off and on every 500 ms: it
        // goes off, then on again (output in between turns it on, so only
        // the off phase proves the blinking). 5 s allows for a stalled vCPU.
        use crate::kassert;
        let phase = || with_screen(|s| s.blink_on).ok_or("no terminal");
        let deadline = crate::time::now_ns() + 5_000_000_000;
        let (mut off, mut on_again) = (false, false);
        while crate::time::now_ns() < deadline && !on_again {
            let on = phase()?;
            off |= !on;
            on_again = off && on;
            crate::time::sleep(20_000_000);
        }
        kassert!(off && on_again);
        Ok(())
    }
);

crate::ktest!(
    fn scrolled_pixels_match_a_full_redraw() {
        // Scrolls drawn by moving pixels, up and down, whole screen and a
        // region (line feeds at the bottom, a reverse index at the top,
        // lines inserted and deleted in a region, a line feed at a region's
        // bottom), each drawn on
        // its own, on rows that all differ (a moved row lands on a different
        // one): the shadow then matches the terminal, and the screen matches
        // drawing every cell afresh (which draws every cell).
        use crate::kassert;
        const STEPS: [&[u8]; 9] = [
            b"\x1b[r\x1b[999;1H\r\nfirst line at the bottom",
            b"\r\nsecond, the first moved up",
            b"\r\nthird \x1b[44mwith a background\x1b[0m",
            b"\x1b[H\x1bMreverse index at the top",
            b"\x1b[5;20r\x1b[8;1H\x1b[2Linserted in a region",
            b"\x1b[20;1H\nscrolled the region up",
            b"\x1b[6;1H\x1b[3Mdeleted three",
            b"\x1b[r\x1b[999;1H\r\nafter the region",
            b"\r\n",
        ];
        reset_view();
        let (cols, rows, w, h) = with_screen(|s| {
            let (cols, rows) = (s.term.cols(), s.term.rows());
            (cols, rows, cols * s.cell_w, rows * CELL_H)
        })
        .ok_or("no terminal")?;
        // Allocated before taking the screen's lock again.
        let (mut drawn, mut fresh) = (Vec::with_capacity(w * h), Vec::with_capacity(w * h));
        let mut fill = alloc::string::String::new();
        for r in 1..=rows {
            let _ = write!(fill, "\x1b[{r};1Hfill row {r} \x1b[4{}m{r}\x1b[0m", r % 8);
        }
        let snapshot = |s: &Screen, into: &mut Vec<u32>| {
            into.extend((s.y0..s.y0 + h).flat_map(|y| (s.x0..s.x0 + w).map(move |x| s.fb.pixel(x, y))));
        };
        let (shadow_ok, redrawn) = with_screen(|s| {
            s.term.write(fill.as_bytes());
            s.render();
            for step in STEPS {
                s.term.write(step);
                s.render();
            }
            let cols = s.term.cols();
            let ok = (0..s.term.rows()).all(|r| (0..cols).all(|c| s.shadow[r * cols + c] == s.term.cell(c, r)));
            snapshot(s, &mut drawn);
            // Forget what was drawn (no cell holds U+0001) and draw it all.
            s.shadow.fill(Cell { ch: '\u{1}', ..Cell::BLANK });
            s.term.touch_all();
            let before = s.cells_drawn;
            s.render();
            snapshot(s, &mut fresh);
            (ok, s.cells_drawn - before)
        })
        .ok_or("no terminal")?;
        kassert!(shadow_ok);
        kassert!(redrawn >= (cols * rows) as u64);
        if let Some(i) = drawn.iter().zip(&fresh).position(|(a, b)| a != b) {
            crate::ktest::diag(format_args!("first differing pixel: ({}, {}) of the grid", i % w, i / w));
        }
        kassert!(drawn == fresh);
        Ok(())
    }
);

crate::ktest!(
    fn the_cursor_is_not_drawn_while_scrolled_back() {
        // With the cursor on the top row, the view scrolled back one line
        // still holds it (a row down), but it isn't drawn; back at the
        // bottom it is.
        use crate::kassert;
        let (before, moved, hidden, back) = with_screen(|s| {
            s.term.write(b"\x1b[r\x1b[999;1H\r\nx\x1b[H");
            s.blink_on = true;
            s.render();
            let before = s.cursor_drawn.is_some();
            let moved = s.term.scroll_view(1);
            s.blink_on = true;
            s.render();
            let hidden = s.cursor_drawn.is_none() && s.term.cursor_in_view().is_some();
            let _ = s.term.reset_view();
            s.render();
            (before, moved, hidden, s.cursor_drawn.is_some())
        })
        .ok_or("no terminal")?;
        kassert!(before && moved && hidden && back);
        Ok(())
    }
);

crate::ktest!(
    fn a_log_line_starts_from_the_default_colours() {
        // Whatever kmon's output left set (bold, inverse, a background), a
        // log line starts afresh.
        use crate::kassert;
        write_raw_fmt(format_args!("\x1b[1;7;44m"));
        log::info!(target: "termtest", "after a dirty pen");
        write_raw_fmt(format_args!("\x1b[0m"));
        let (cols, cells) = view_cells().ok_or("no terminal")?;
        let row = find_row(cols, &cells, "after a dirty pen").ok_or("the line isn't on screen")?;
        kassert!(row[0].flags == Flags::NONE);
        kassert!(row.iter().all(|c| c.bg == Color::Default && !c.flags.contains(Flags::INVERSE)));
        Ok(())
    }
);

/// Formats as nothing, noting whether it was formatted while this CPU held
/// the screen's lock (for the tests).
#[cfg(feature = "ktest")]
pub struct LockProbe<'a>(pub &'a AtomicBool);

#[cfg(feature = "ktest")]
impl fmt::Display for LockProbe<'_> {
    fn fmt(&self, _: &mut fmt::Formatter<'_>) -> fmt::Result {
        if drawing_here() {
            self.0.store(true, Ordering::Relaxed);
        }
        Ok(())
    }
}

crate::ktest!(
    fn write_fmt_formats_under_the_lock() {
        // A whole `write!` (and so a log line) is formatted while holding the
        // screen's lock: no other CPU's line lands inside it.
        use crate::kassert;
        let under = AtomicBool::new(false);
        kassert!(write_fmt(format_args!("{}", LockProbe(&under))));
        kassert!(under.load(Ordering::Relaxed));
        let under_raw = AtomicBool::new(false);
        kassert!(write_raw_fmt(format_args!("{}", LockProbe(&under_raw))));
        kassert!(under_raw.load(Ordering::Relaxed));
        Ok(())
    }
);
