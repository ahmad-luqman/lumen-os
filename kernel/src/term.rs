//! The terminal on the framebuffer: `lumen-term`'s grid drawn below the
//! header, fed by log lines (coloured by level) and kmon.
//!
//! Drawing keeps a copy of every cell it drew (the shadow) and draws only
//! the cells that differ from it, in the rows the terminal marked as changed.
//! A scroll moves the pixels (`Framebuffer::move_rows`) and the shadow
//! together, so a line feed at the bottom draws one new line, not the whole
//! screen. The cursor blinks every 500 ms (the boot CPU's timer interrupt
//! flips it: no thread, so nothing switches or traces for it) and shows
//! solid while output arrives; it is hidden while the view is scrolled back
//! (Shift+PageUp/PageDown in kmon).
//!
//! Writing never allocates (the terminal and the shadow are allocated in
//! [`init`]): log lines come while the frame allocator's or the page tables'
//! lock is held, and the heap would take them.
//!
//! The terminal takes `\n` as a line feed only; the writers here add the
//! carriage return.

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
    /// Cells drawn so far (the tests count redraws with it).
    cells_drawn: u64,
}

static SCREEN: SpinLock<Option<Screen>> = SpinLock::new(None);
/// The CPU drawing (holding `SCREEN`) plus one; 0 when none.
static DRAWING: AtomicUsize = AtomicUsize::new(0);
/// Set by the crash screen: nothing is drawn afterwards.
static STOPPED: AtomicBool = AtomicBool::new(false);
/// Log lines not shown because they came from inside drawing on the same
/// CPU (an exception while drawing).
static DROPPED: AtomicU64 = AtomicU64::new(0);
/// The boot CPU's tick at which the cursor flips next (output moves it on).
static NEXT_BLINK: AtomicU64 = AtomicU64::new(0);

/// Start the terminal below the header of `fb` (called by `fb::init`).
pub fn init(fb: Framebuffer) {
    let cell_w = get_raster_width(FontWeight::Regular, fb::CONSOLE_SIZE)
        .max(get_raster_width(FontWeight::Bold, fb::CONSOLE_SIZE));
    let (x0, y0) = (fb::MARGIN, fb::CONSOLE_TOP);
    let cols = fb.width.saturating_sub(x0 + fb::MARGIN / 2) / cell_w;
    let rows = fb.height.saturating_sub(y0) / CELL_H;
    if cols == 0 || rows == 0 {
        log::warn!(target: "term", "no room for a terminal on a {}x{} screen", fb.width, fb.height);
        return;
    }
    // Allocated here, before taking the lock (see the module comment).
    let term = Terminal::new(cols, rows, SCROLLBACK);
    let shadow = vec![Cell::BLANK; cols * rows];
    let screen = Screen { fb, term, shadow, x0, y0, cell_w, cursor_drawn: None, blink_on: true, cells_drawn: 0 };
    *SCREEN.lock() = Some(screen);
    log::info!(target: "term", "terminal {cols}x{rows}, {SCROLLBACK} lines of scrollback");
}

/// Run `f` on the screen, unless there is none, the crash screen replaced
/// it, or this CPU is already drawing (an exception in the middle of it:
/// waiting would never end).
fn with_screen<R>(f: impl FnOnce(&mut Screen) -> R) -> Option<R> {
    if STOPPED.load(Ordering::Acquire) {
        return None;
    }
    let me = crate::arch::cpu_index() + 1;
    if DRAWING.load(Ordering::Relaxed) == me {
        return None;
    }
    let mut guard = SCREEN.lock();
    let screen = guard.as_mut()?;
    DRAWING.store(me, Ordering::Relaxed);
    let r = f(screen);
    DRAWING.store(0, Ordering::Relaxed);
    Some(r)
}

/// The crash screen is up: draw nothing more.
pub fn stop() {
    STOPPED.store(true, Ordering::Release);
}

/// The boot CPU's timer tick `tick` (in its interrupt handler): flip the
/// cursor every [`BLINK_TICKS`]. It never waits: if another CPU is drawing,
/// this blink is skipped (the next tick tries again).
pub fn on_tick(tick: u64) {
    if tick < NEXT_BLINK.load(Ordering::Relaxed) || STOPPED.load(Ordering::Acquire) {
        return;
    }
    let Some(mut guard) = SCREEN.try_lock() else { return };
    let Some(s) = guard.as_mut() else {
        // No terminal (yet): look again a blink later.
        NEXT_BLINK.store(tick + BLINK_TICKS, Ordering::Relaxed);
        return;
    };
    // (This CPU can't be drawing: it does so with interrupts off.)
    DRAWING.store(crate::arch::cpu_index() + 1, Ordering::Relaxed);
    s.blink_on = !s.blink_on;
    s.render();
    NEXT_BLINK.store(tick + BLINK_TICKS, Ordering::Relaxed);
    DRAWING.store(0, Ordering::Relaxed);
}

impl Screen {
    /// Feed `s` to the terminal, `\n` as CR LF (not drawn yet).
    fn feed(&mut self, s: &str) {
        let mut lines = s.split('\n');
        if let Some(first) = lines.next() {
            self.term.write(first.as_bytes());
        }
        for line in lines {
            self.term.write(b"\r\n");
            self.term.write(line.as_bytes());
        }
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
            debug_assert!(bottom <= self.fb.height);
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
            && let Some((c, r)) = self.term.cursor_in_view()
        {
            self.draw(c, r, self.term.cell(c, r), true);
            self.cursor_drawn = Some((c, r));
        }
    }

    /// Draw `cell` at (`col`, `row`), colours swapped for the cursor.
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

/// Write `s` to the terminal (`\n` as CR LF) and draw it.
pub fn write_str(s: &str) {
    with_screen(|screen| {
        screen.feed(s);
        screen.render();
    });
}

/// Write raw bytes to the terminal (no `\n` translation) and draw them.
pub fn write_bytes(bytes: &[u8]) {
    with_screen(|screen| {
        screen.term.write(bytes);
        screen.show_cursor();
        screen.render();
    });
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
pub fn console_log(level: Level, tag: &str, args: &fmt::Arguments<'_>) {
    let shown = with_screen(|s| {
        let (lvl, msg) = level_sgr(level);
        let _ = writeln!(Feed(s), "\x1b[{lvl}m{level:<5}\x1b[0m \x1b[90m{tag}:\x1b[0m \x1b[{msg}m{args}\x1b[0m");
        s.render();
    });
    if shown.is_none() && DRAWING.load(Ordering::Relaxed) == crate::arch::cpu_index() + 1 {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Scroll the view back (`lines` > 0) or forward through the scrollback;
/// a move is logged (debug: serial only, the screen shows up to `info`).
pub fn scroll_view(lines: isize) {
    let moved = with_screen(|s| {
        if !s.term.scroll_view(lines) {
            return None;
        }
        s.render();
        Some((s.term.view_offset(), top_row(&s.term)))
    });
    // Logged outside the screen's lock: logging draws.
    if let Some(Some((view, top))) = moved {
        log::debug!(target: "term", "view {view} lines up, top row \"{top}\"");
    }
}

/// Scroll the view back by half a screen (`pages` > 0) or forward, per page.
pub fn scroll_half_pages(pages: isize) {
    let rows = with_screen(|s| s.term.rows()).unwrap_or(0) as isize;
    scroll_view(pages * (rows / 2).max(1));
}

/// Show the screen again (typing does).
pub fn reset_view() {
    let view = with_screen(|s| s.term.view_offset()).unwrap_or(0);
    if view > 0 {
        scroll_view(-(view as isize));
    }
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

/// The view's top row (its first 32 columns, trailing blanks trimmed).
fn top_row(t: &Terminal) -> RowText {
    let mut row = RowText { chars: [' '; 32], len: 0 };
    let mut n = 0;
    for c in 0..t.cols() {
        let cell = t.cell(c, 0);
        if cell.flags.contains(Flags::SPACER) {
            continue;
        }
        if n == row.chars.len() {
            break;
        }
        row.chars[n] = cell.ch;
        n += 1;
        if cell.ch != ' ' {
            row.len = n;
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
    /// The view's top row (its first 32 columns, trailing blanks trimmed).
    pub top_row: RowText,
    pub cells_drawn: u64,
    pub dropped: u64,
}

/// The terminal's state, or `None` without one.
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
            dropped: DROPPED.load(Ordering::Relaxed),
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
