//! The terminal on the framebuffer: `lumen-term`'s grid drawn below the
//! header, fed by log lines (coloured by level) and the screen's shell.
//!
//! Drawing keeps a copy of every cell it drew (the shadow) and draws only
//! the cells that differ from it, in the rows the terminal marked as changed.
//! A scroll moves the pixels (`Framebuffer::move_rows`) and the shadow
//! together, so a line feed at the bottom draws one new line, not the whole
//! screen. The cursor flips every 500 ms (the boot CPU's timer interrupt
//! does it: no thread, so nothing switches or traces for it) and shows
//! solid while output arrives; it isn't drawn while the view is scrolled
//! back (Shift+PageUp/PageDown in the screen's shell).
//!
//! Nothing allocates under the screen's lock (the terminal and the shadow
//! are allocated in [`init`]): log lines come while the frame allocator's or
//! the page tables' lock is held, and the heap would take them, so the
//! arguments of a log line (formatted under the lock) and of
//! [`write_fmt`] must not allocate either.
//!
//! A command can take the pixels below the header for itself ([`canvas`]):
//! the terminal then keeps taking output but draws none of it until the
//! [`Canvas`] is dropped (or taken back, [`reclaim_canvas`]), which draws
//! the whole terminal again.
//!
//! The terminal takes `\n` as a line feed only: [`write_str`], [`write_fmt`]
//! and log lines turn it into CR LF.

use crate::fb::{self, Framebuffer, Style};
use crate::sync::SpinLock;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::num::NonZeroU64;
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
    /// Cells drawn so far (the shell's `term` shows it; the tests count redraws
    /// with it).
    cells_drawn: u64,
    /// Whose [`Canvas`] has the pixels, if one has: draw nothing then.
    canvas: Option<Owner>,
}

static SCREEN: SpinLock<Option<Screen>> = SpinLock::new(None);
/// The CPU drawing (holding `SCREEN`) plus one; 0 when none.
static DRAWING: AtomicUsize = AtomicUsize::new(0);
/// Set when a crash report starts: no drawing starts afterwards (one under
/// way on another CPU runs until that CPU is stopped).
static STOPPED: AtomicBool = AtomicBool::new(false);
/// The boot CPU's tick at which the cursor flips next (output moves it on).
static NEXT_BLINK: AtomicU64 = AtomicU64::new(0);
/// Which canvas may draw: each [`Canvas`] keeps the value it was made
/// with, and draws nothing once it changed (a crash report started, or the
/// shell took the screen back from a canvas its command left behind).
static CANVAS_EPOCH: AtomicU64 = AtomicU64::new(0);

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
    let screen =
        Screen { fb, term, shadow, x0, y0, cell_w, cursor_drawn: None, blink_on: true, cells_drawn: 0, canvas: None };
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

/// A crash report started: draw nothing more, and no canvas draws either
/// (the crash screen draws straight to the pixels).
pub fn stop() {
    STOPPED.store(true, Ordering::Release);
    CANVAS_EPOCH.fetch_add(1, Ordering::AcqRel);
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

    /// Output shows the cursor at once, for a whole blink.
    fn show_cursor(&mut self) {
        self.blink_on = true;
        NEXT_BLINK.store(crate::time::ticks() + BLINK_TICKS, Ordering::Relaxed);
    }

    /// Draw what changed since the last time (nothing while a [`Canvas`]
    /// has the pixels: [`Self::resume`] draws every cell afterwards).
    fn render(&mut self) {
        if self.canvas.is_some() {
            return;
        }
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

    /// A [`Canvas`] gave the pixels back, or the shell took them back
    /// ([`reclaim_canvas`]): clear what it drew (everything below the
    /// header) and draw every cell of the terminal again.
    fn resume(&mut self) {
        self.canvas = None;
        let (w, h) = (self.fb.width(), self.fb.height());
        self.fb.fill_rect(0, fb::CONSOLE_TOP, w, h.saturating_sub(fb::CONSOLE_TOP), fb::BG);
        // A blank cell draws as the background just filled in.
        self.shadow.fill(Cell::BLANK);
        self.cursor_drawn = None;
        self.term.touch_all();
        self.render();
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

/// Who holds a [`Canvas`]: the shell makes one for each command it runs
/// (`Ctx::canvas` takes the canvas for it) and, when the command returns,
/// takes back a canvas that owner left behind ([`reclaim_canvas`]), but
/// never another's (the other console's command may be drawing).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner(NonZeroU64);

impl Owner {
    /// A new owner, equal to no other.
    pub fn new() -> Owner {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Owner(NonZeroU64::new(NEXT.fetch_add(1, Ordering::Relaxed)).expect("2^64 owners"))
    }
}

/// The pixels below the header, for a command to draw on (a demo, a
/// script) while the terminal is paused: output still reaches the terminal
/// but isn't drawn until the canvas is dropped, which clears what was drawn
/// and draws the whole terminal again. One at a time ([`canvas`]).
///
/// Coordinates are the canvas's own ((0, 0) is its top-left corner, just
/// below the header); drawing outside it does nothing. Its methods take
/// `&self` and it is `Sync` (pixel stores are atomic), so several CPUs can
/// draw on it at once.
///
/// Drop it when done: until then the screen shows nothing new. One its
/// command left behind is taken back by the shell when the command returns
/// ([`reclaim_canvas`]) and, like every canvas once a crash report starts,
/// draws nothing from then on ([`Canvas::is_live`]); a `fill_rect` or
/// `text` under way then (on another CPU, or preempted on this one)
/// stops at its next row or glyph. Drawing from other CPUs, either share it
/// as an `Arc<Canvas>` with threads that are joined before the command
/// returns, or (for `smp::run_on` jobs, which take a plain `fn()`) reach it
/// through a static that is cleared only after every job has finished
/// drawing, and drop it after that: a job still drawing would paint over
/// the terminal the drop drew.
pub struct Canvas {
    fb: Framebuffer,
    width: usize,
    height: usize,
    /// [`CANVAS_EPOCH`] when it was made.
    epoch: u64,
}

// SAFETY: drawing is atomic 32-bit stores to the framebuffer through `&self`
// (`Framebuffer::put`; no other state changes), so CPUs drawing at once
// don't race with each other. The terminal draws nothing while the canvas
// is live (`Screen::canvas` is cleared only by its drop or together with an
// epoch change); once it isn't, it stores nothing more, except a row or
// glyph under way, which may overlap the terminal's redraw (garbled pixels
// until those cells change, never memory outside the framebuffer). The
// crash screen takes the pixels only on the crash path, after stopping the
// terminal, the canvases and the other CPUs.
unsafe impl Sync for Canvas {}

/// Why there is no [`Canvas`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanvasError {
    /// No terminal (no usable framebuffer, or one too small).
    NoTerminal,
    /// A crash report started: the crash screen has the pixels.
    Stopped,
    /// Another command has the canvas.
    InUse,
}

impl fmt::Display for CanvasError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CanvasError::NoTerminal => "no screen to draw on",
            CanvasError::Stopped => "the screen shows a crash report",
            CanvasError::InUse => "the screen is in use by another command",
        })
    }
}

/// Take the pixels below the header for `owner` (see [`Canvas`]; commands
/// use `Ctx::canvas`).
pub fn canvas(owner: Owner) -> Result<Canvas, CanvasError> {
    // `with_screen` refuses on a CPU already drawing too: never one taking
    // a canvas, as drawing holds the screen's lock with interrupts off
    // (which is why that case has no error of its own).
    debug_assert!(!drawing_here());
    let taken = with_screen(|s| {
        if s.canvas.is_some() {
            return Err(CanvasError::InUse);
        }
        // Changed only under the screen's lock, or by `stop`: an epoch read
        // after `stop` changed it comes with `STOPPED` set (it is stored
        // first), so a canvas never gets the epoch `stop` set.
        let epoch = CANVAS_EPOCH.load(Ordering::Acquire);
        if STOPPED.load(Ordering::Acquire) {
            return Err(CanvasError::Stopped);
        }
        s.canvas = Some(owner);
        // SAFETY: the terminal's handle draws nothing while the canvas is
        // live (`Screen::canvas`), so the two never draw at the same time
        // (see `Canvas`'s `Sync` for a canvas no longer live).
        let fb = unsafe { s.fb.alias() };
        let (width, height) = (fb.width(), fb.height().saturating_sub(fb::CONSOLE_TOP));
        Ok(Canvas { fb, width, height, epoch })
    });
    match taken {
        Some(result) => result,
        None if STOPPED.load(Ordering::Acquire) => Err(CanvasError::Stopped),
        None => Err(CanvasError::NoTerminal),
    }
}

impl Canvas {
    /// Width in pixels.
    pub fn width(&self) -> usize {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> usize {
        self.height
    }

    /// Whether it still draws: false once the shell took it back
    /// ([`reclaim_canvas`]) or a crash report started. A command drawing
    /// frame after frame checks it and stops (saying it lost the screen).
    pub fn is_live(&self) -> bool {
        CANVAS_EPOCH.load(Ordering::Acquire) == self.epoch
    }

    /// Set the pixel at (`x`, `y`) to `rgb` (`0xRRGGBB`).
    pub fn put(&self, x: usize, y: usize, rgb: u32) {
        if x < self.width && y < self.height && self.is_live() {
            self.fb.put(x, y + fb::CONSOLE_TOP, rgb);
        }
    }

    /// Fill a rectangle, clipped to the canvas (row by row, stopping if
    /// the canvas stops being live).
    pub fn fill_rect(&self, x: usize, y: usize, w: usize, h: usize, rgb: u32) {
        if x < self.width && y < self.height {
            let (w, h) = (w.min(self.width - x), h.min(self.height - y));
            for row in y..y + h {
                if !self.is_live() {
                    return;
                }
                self.fb.fill_rect(x, row + fb::CONSOLE_TOP, w, 1, rgb);
            }
        }
    }

    /// Fill the whole canvas.
    pub fn clear(&self, rgb: u32) {
        self.fill_rect(0, 0, self.width, self.height, rgb);
    }

    /// Draw `s` in the terminal's font (16 pixels high) with its top-left
    /// corner at (`x`, `y`), `fg` on `bg`, clipped to the canvas; returns
    /// the width drawn in pixels (0 if it starts outside, or the canvas
    /// isn't live).
    pub fn text(&self, x: usize, y: usize, s: &str, fg: u32, bg: u32) -> usize {
        if y >= self.height {
            return 0;
        }
        // The canvas spans the framebuffer's width and reaches its bottom,
        // so the framebuffer's clipping is the canvas's.
        let st = Style::new(FontWeight::Regular, fb::CONSOLE_SIZE, fg, bg);
        let mut cx = x;
        for c in s.chars() {
            if cx >= self.width || !self.is_live() {
                break;
            }
            cx += self.fb.glyph_clipped(cx, y + fb::CONSOLE_TOP, c, st, self.width - cx);
        }
        cx - x
    }

    /// The pixel at (`x`, `y`) as stored (0 outside the canvas; for the
    /// tests).
    #[cfg(feature = "ktest")]
    pub fn pixel(&self, x: usize, y: usize) -> u32 {
        if x < self.width && y < self.height { self.fb.pixel(x, y + fb::CONSOLE_TOP) } else { 0 }
    }
}

impl Drop for Canvas {
    fn drop(&mut self) {
        // A canvas taken back already (or after a crash report started)
        // leaves the terminal alone: it may have a newer canvas by now.
        let done = with_screen(|s| {
            if self.is_live() {
                s.resume();
            }
        });
        // `with_screen` refuses only after a crash report started: there is
        // a screen (a canvas comes only from one, and it is never removed),
        // and a CPU is never drawing while it drops a canvas (drawing holds
        // the screen's lock with interrupts off). So the terminal never
        // stays paused for good.
        debug_assert!(
            done.is_some() || STOPPED.load(Ordering::Acquire),
            "a canvas dropped without resuming the terminal"
        );
    }
}

/// Take the screen back from `owner`'s canvas, if it still has one (the
/// shell does when a command returns: one left behind would keep the
/// screen frozen): it draws nothing from then on, and the terminal is
/// drawn again. Another owner's canvas is left alone. True if there was
/// one.
pub fn reclaim_canvas(owner: Owner) -> bool {
    with_screen(|s| {
        if s.canvas != Some(owner) {
            return false;
        }
        CANVAS_EPOCH.fetch_add(1, Ordering::AcqRel);
        s.resume();
        true
    })
    .unwrap_or(false)
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

/// Write `s` to the terminal (`\n` as CR LF) and draw it; false if the
/// terminal didn't take it (no terminal, a crash report started, or this
/// CPU is already drawing). Under a [`Canvas`] it is taken, and drawn when
/// the canvas is dropped or taken back.
pub fn write_str(s: &str) -> bool {
    write_fmt(format_args!("{s}"))
}

/// Write formatted text (`\n` as CR LF) and draw it, all under one lock: a
/// log line from another CPU can't land inside an escape sequence written
/// in this one call. `args`
/// must not allocate (see the module comment). False if the terminal
/// didn't take it (see [`write_str`]).
pub fn write_fmt(args: fmt::Arguments<'_>) -> bool {
    with_screen(|screen| {
        let _ = Feed(screen).write_fmt(args);
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
/// It starts from the default colours, whatever the shell's output left set.
pub fn console_log(level: Level, tag: &str, args: &fmt::Arguments<'_>) {
    let (lvl, msg) = level_sgr(level);
    write_fmt(format_args!("\x1b[0;{lvl}m{level:<5}\x1b[0m \x1b[90m{tag}:\x1b[0m \x1b[{msg}m{args}\x1b[0m\n"));
}

/// Move the view with `scroll` (which returns whether it moved) and draw
/// it; a move is logged (debug: serial only, the screen shows up to `info`).
fn move_view(scroll: impl FnOnce(&mut Terminal) -> bool) {
    let moved = with_screen(|s| {
        // The view stays put while a canvas has the screen (nothing would
        // show the move; the terminal comes back as it was left).
        if s.canvas.is_some() || !scroll(&mut s.term) {
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

/// The start of a row of the view, as text (for the shell's `term`).
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

/// What the shell's `term` shows.
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
    /// A [`Canvas`] has the screen: nothing is drawn.
    pub paused: bool,
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
            paused: s.canvas.is_some(),
        }
    })
}

/// The view's cells, row by row, and the cursor if it is in the view (its
/// row counted in the view; shown or hidden), read under one lock: a log line from another
/// CPU can't land between them. The vector is allocated before taking the
/// lock again, as nothing under it may allocate.
fn view_cells() -> Option<(usize, Vec<Cell>, Option<Cursor>)> {
    let (cols, rows) = with_screen(|s| (s.term.cols(), s.term.rows()))?;
    let mut cells = Vec::with_capacity(cols * rows);
    let cursor = with_screen(|s| {
        let t = &s.term;
        // The size never changes: this fits the capacity.
        cells.extend((0..rows).flat_map(|r| (0..cols).map(move |c| t.cell(c, r))));
        let c = t.cursor();
        let row = c.row + t.view_offset();
        (row < rows).then_some(Cursor { row, ..c })
    })?;
    Some((cols, cells, cursor))
}

/// The view's rows as text, and the cursor if it is in the view.
pub struct ViewText {
    /// Each row of the view, wide characters' spacers skipped and trailing
    /// blanks trimmed.
    pub rows: Vec<String>,
    /// The cursor, its row counted in the view and its column in cells (a
    /// wide character before it counts two), or `None` if the view is
    /// scrolled back past it.
    pub cursor: Option<Cursor>,
}

/// The view as text: what the shell's `term text` prints, and how the tests
/// read what the screen shell did.
pub fn view_text() -> Option<ViewText> {
    let (cols, cells, cursor) = view_cells()?;
    let rows = cells
        .chunks(cols)
        .map(|row| {
            let text: String = row.iter().filter(|c| !c.flags.contains(Flags::SPACER)).map(|c| c.ch).collect();
            String::from(text.trim_end())
        })
        .collect();
    Some(ViewText { rows, cursor })
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
        let (cols, cells, _) = view_cells().ok_or("no terminal")?;
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
    fn output_keeps_a_scrolled_back_view() {
        // Scrolled back, new lines scroll the screen under the view, which
        // stays on the lines it shows (they are just more lines up); showing
        // the screen again brings the view back to the bottom.
        use crate::{kassert, kassert_eq};
        use alloc::format;
        for i in 0..60 {
            write_fmt(format_args!("keep {i}\n"));
        }
        let (moved, top, view) = with_screen(|s| {
            let moved = s.term.scroll_view(5);
            s.render();
            (moved, top_row(&s.term), s.term.view_offset())
        })
        .ok_or("no terminal")?;
        kassert!(moved && view == 5);
        kassert!(write_str("more 1\nmore 2\n"));
        let after = status().ok_or("no terminal")?;
        kassert_eq!(format!("{}", after.top_row), format!("{top}"));
        kassert_eq!(after.view, view + 2);
        reset_view();
        kassert_eq!(status().ok_or("no terminal")?.view, 0);
        Ok(())
    }
);

crate::ktest!(
    fn a_log_line_starts_from_the_default_colours() {
        // Whatever the shell's output left set (bold, inverse, a background), a
        // log line starts afresh.
        use crate::kassert;
        write_str("\x1b[1;7;44m");
        log::info!(target: "termtest", "after a dirty pen");
        write_str("\x1b[0m");
        let (cols, cells, _) = view_cells().ok_or("no terminal")?;
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
        Ok(())
    }
);

crate::ktest!(
    fn a_canvas_pauses_the_terminal_until_dropped() {
        // While a canvas has the pixels there is no second one, output
        // reaches the terminal without being drawn, the canvas draws only
        // below the header and clips (without panicking) what falls outside
        // it. Dropping it draws the whole terminal again: every cell as the
        // terminal holds it, the text from before the canvas and from under
        // it in ink, not background.
        use crate::{kassert, kassert_eq};
        const BEFORE: &str = "drawn before the canvas";
        const UNDER: &str = "written under a canvas";
        reset_view();
        kassert!(write_str(&alloc::format!("{BEFORE}\n")));
        let drawn = || status().map(|s| s.cells_drawn).ok_or("no terminal");
        // Every pixel of the header, folded into one number (FNV-1a: no
        // allocation under the screen's lock).
        let header = || {
            with_screen(|s| {
                let mut h: u64 = 0xcbf2_9ce4_8422_2325;
                for y in 0..fb::CONSOLE_TOP {
                    for x in 0..s.fb.width() {
                        h = (h ^ u64::from(s.fb.pixel(x, y))).wrapping_mul(0x100_0000_01b3);
                    }
                }
                h
            })
            .ok_or("no terminal")
        };
        let (bg, green, fb_height) =
            with_screen(|s| (s.fb.encode(fb::BG), s.fb.encode(0x00ff00), s.fb.height())).ok_or("no terminal")?;
        let header_before = header()?;
        {
            let owner = Owner::new();
            let c = canvas(owner).map_err(|_| "no canvas")?;
            kassert_eq!(canvas(owner).err(), Some(CanvasError::InUse));
            kassert_eq!(canvas(Owner::new()).err(), Some(CanvasError::InUse));
            kassert!(status().ok_or("no terminal")?.paused);
            let before = drawn()?;
            kassert!(write_str(&alloc::format!("{UNDER}\n")));
            kassert_eq!(drawn()?, before);
            c.clear(0x00ff00);
            kassert_eq!(c.height(), fb_height - fb::CONSOLE_TOP);
            kassert_eq!(c.pixel(0, 0), green);
            kassert_eq!(c.pixel(c.width() - 1, c.height() - 1), green);
            c.fill_rect(usize::MAX - 1, usize::MAX - 1, 4, 4, 0);
            c.fill_rect(c.width() - 1, c.height() - 1, usize::MAX, usize::MAX, 0x00ff00);
            c.put(usize::MAX, usize::MAX, 0);
            kassert_eq!(c.text(0, usize::MAX, "x", 0, 0), 0);
            kassert_eq!(c.text(usize::MAX, 0, "x", 0, 0), 0);
            let clipped = c.text(c.width() - 3, 0, "abc", 0xffffff, 0);
            kassert!(clipped > 0 && clipped <= 3);
            kassert!(c.text(0, c.height() - 1, "below", 0xffffff, 0) > 0);
            kassert_eq!(header()?, header_before);
        }
        let (paused, cells_match, ink) = with_screen(|s| {
            let (cols, rows) = (s.term.cols(), s.term.rows());
            let cells_match = (0..rows).all(|r| (0..cols).all(|c| s.shadow[r * cols + c] == s.term.cell(c, r)));
            // Whether the row starting with `needle` is on screen with a
            // pixel of its text that isn't background.
            let ink = |needle: &str| {
                let row = (0..rows).find(|&r| needle.chars().enumerate().all(|(i, ch)| s.term.cell(i, r).ch == ch));
                row.is_some_and(|r| {
                    let (y, w) = (s.y0 + r * CELL_H, needle.len() * s.cell_w);
                    (y..y + CELL_H).any(|y| (s.x0..s.x0 + w).any(|x| s.fb.pixel(x, y) != bg))
                })
            };
            // Every visible character has ink on screen (not just in the
            // shadow, which the redraw keeps in step either way).
            let all_inked = (0..rows).all(|r| {
                (0..cols).all(|c| {
                    s.term.cell(c, r).ch == ' ' || {
                        let (x, y) = (s.x0 + c * s.cell_w, s.y0 + r * CELL_H);
                        (y..y + CELL_H).any(|y| (x..x + s.cell_w).any(|x| s.fb.pixel(x, y) != bg))
                    }
                })
            });
            (s.canvas.is_some(), cells_match && all_inked, [ink(BEFORE), ink(UNDER)])
        })
        .ok_or("no terminal")?;
        kassert!(!paused);
        kassert!(cells_match);
        kassert_eq!(ink, [true, true]);
        // The margin, which no cell covers: the canvas's green cleared.
        kassert_eq!(with_screen(|s| s.fb.pixel(0, fb::CONSOLE_TOP)).ok_or("no terminal")?, bg);
        kassert_eq!(header()?, header_before);
        Ok(())
    }
);

crate::ktest!(
    fn a_canvas_is_taken_back_only_from_its_owner() {
        // Taking back another owner's canvas does nothing; taking back the
        // owner's makes it draw nothing more, and its drop leaves alone the
        // canvas taken after it; dropping that one resumes the terminal.
        use crate::{kassert, kassert_eq};
        let corner = || with_screen(|s| s.fb.pixel(0, fb::CONSOLE_TOP)).ok_or("no terminal");
        let paused = || status().map(|s| s.paused).ok_or("no terminal");
        let owner = Owner::new();
        let old = canvas(owner).map_err(|_| "no canvas")?;
        kassert!(!reclaim_canvas(Owner::new()));
        kassert!(paused()? && old.is_live());
        kassert!(reclaim_canvas(owner));
        kassert!(!reclaim_canvas(owner));
        kassert!(!paused()? && !old.is_live());
        let bg = corner()?;
        old.put(0, 0, 0x00ff00);
        old.fill_rect(0, 0, 1, 1, 0x00ff00);
        kassert_eq!(old.text(0, 0, "x", 0x00ff00, 0x00ff00), 0);
        kassert_eq!(corner()?, bg);
        let new = canvas(Owner::new()).map_err(|_| "no second canvas")?;
        drop(old);
        kassert!(paused()?);
        new.put(0, 0, 0x00ff00);
        kassert!(corner()? != bg);
        drop(new);
        kassert!(!paused()?);
        kassert_eq!(corner()?, bg);
        Ok(())
    }
);
