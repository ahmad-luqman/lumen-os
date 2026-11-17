//! The terminals on the framebuffer: [`VTS`] virtual terminals, each its
//! own `lumen-term` grid and scrollback (allocated in [`init`]), one drawn
//! below the header at a time. Log lines (coloured by level) go to the
//! first, each screen shell writes to its own ([`write_str_to`]), and
//! [`show`] switches which is drawn (`crate::vt` routes each key to the
//! terminal [`shown`] when it routes it). The header's right end shows
//! which one is shown. Writing to a hidden one draws nothing; showing it
//! draws all of it.
//!
//! Drawing keeps a copy of every cell it drew (the shadow) and draws only
//! the cells that differ from it, in the rows the terminal marked as changed.
//! A scroll moves the pixels (`Framebuffer::move_rows`) and the shadow
//! together, so a line feed at the bottom draws one new line, not the whole
//! screen. The cursor flips every 500 ms (the boot CPU's timer interrupt
//! does it: no thread, so nothing switches or traces for it) and shows
//! solid while output arrives; it isn't drawn while the view is scrolled
//! back (Shift+PageUp/PageDown in a screen shell).
//!
//! Nothing allocates under the screen's lock (the terminals and the shadow
//! are allocated in [`init`]): log lines come while the frame allocator's or
//! the page tables' lock is held, and the heap would take them, so the
//! arguments of a log line (formatted under the lock) and of
//! [`write_fmt_to`] must not allocate either.
//!
//! A command can take the pixels below the header for itself ([`canvas`]):
//! the terminals then keep taking output but draw none of it until the
//! [`Canvas`] is dropped (or taken back, [`reclaim_canvas`]), which draws
//! the shown terminal again. A command on a virtual terminal that takes it
//! brings that terminal to the front first, and while a canvas is held no
//! other terminal can be shown: the keys stay with the command drawing. A
//! command on serial (on no terminal) leaves the shown one, and its keys,
//! where they are.
//!
//! The desktop (#153) takes the whole screen, header included, the same
//! way ([`take_screen`]: a [`ScreenCanvas`], whose rows are written under
//! the screen's lock, one row per hold, and only while it is live). Alt+F1
//! to F4 give the screen back from it ([`leave_desktop`], called by
//! `crate::vt` under its router lock): the one way a terminal is shown
//! while a canvas is held.
//!
//! The terminal takes `\n` as a line feed only: [`write_str_to`],
//! [`write_fmt_to`] and log lines turn it into CR LF.
//!
//! The mouse pointer (#98) is drawn here too, over everything, header
//! included, once the tablet has reported a position ([`move_pointer`];
//! never before, so a boot without a tablet draws exactly what it did).
//! It is a save-under arrow (`lumen_input::Cursor`): every drawing step
//! that would touch its box takes it off first (putting back the pixels it
//! covered), and the end of every step under the screen's lock puts it
//! back, at its newest position. So redrawing the terminal never erases
//! it and moving it never leaves a trail. While a [`Canvas`] has the
//! pixels it is taken off and stays off (a canvas draws without the lock,
//! so it can't be kept out of the arrow's box); dropping the canvas draws
//! it again.

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

/// Lines kept above the screen (by each virtual terminal).
pub const SCROLLBACK: usize = 500;
/// Virtual terminals (Alt+F1 to Alt+F4).
pub const VTS: usize = 4;
// Written for exactly four: Alt+F1 to F4 (`crate::vt`), the `vt` command's
// words 1 to 4, the screen shells' thread names (`shell::SCREEN_THREADS`),
// the header's one-digit labels and the ktests' arrays.
const _: () = assert!(VTS == 4, "four virtual terminals");
/// How long the cursor stays on, then off, in the boot CPU's timer ticks
/// (1 kHz).
const BLINK_TICKS: u64 = 500;
const CELL_H: usize = 16;

/// The terminals and what is drawn of the shown one.
struct Screen {
    fb: Framebuffer,
    /// Each virtual terminal's grid.
    terms: [Terminal; VTS],
    /// Which virtual terminal is shown (0 is the first).
    shown: usize,
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
    /// Whose [`Canvas`] (or [`ScreenCanvas`]) has the pixels, if one has:
    /// draw nothing then.
    canvas: Option<Owner>,
    /// The canvas held is a [`ScreenCanvas`] (the desktop's: the whole
    /// screen, header included), not a command's [`Canvas`].
    whole: bool,
    /// The epoch the [`ScreenCanvas`] held was taken at: which take it is
    /// (the desktop takes the screen with one owner each time it resumes).
    whole_epoch: u64,
    /// Where the mouse pointer's tip should be (framebuffer pixels), if the
    /// tablet reported a position (and the pointer wasn't hidden since).
    pointer_at: Option<(usize, usize)>,
    /// The arrow as drawn, and the pixels it covers.
    pointer: lumen_input::Cursor,
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
    let terms = core::array::from_fn(|_| Terminal::new(cols, rows, SCROLLBACK));
    let shadow = vec![Cell::BLANK; cols * rows];
    let mut screen = Screen {
        fb,
        terms,
        shown: 0,
        shadow,
        x0,
        y0,
        cell_w,
        cursor_drawn: None,
        blink_on: true,
        cells_drawn: 0,
        canvas: None,
        whole: false,
        whole_epoch: 0,
        pointer_at: None,
        pointer: lumen_input::Cursor::default(),
    };
    screen.draw_tabs();
    *SCREEN.lock() = Some(screen);
    log::info!(
        target: "term",
        "{VTS} terminals {cols}x{rows} (Alt+F1 to F{VTS} switch), {SCROLLBACK} lines of scrollback each"
    );
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

/// Run `f` on `screen`, marked as this CPU's drawing (the lock is held),
/// then put the mouse pointer where it belongs (`f` may have taken it off,
/// or moved it).
fn drawing<R>(screen: &mut Screen, f: impl FnOnce(&mut Screen) -> R) -> R {
    DRAWING.store(drawing_id(), Ordering::Relaxed);
    let r = f(screen);
    screen.place_pointer();
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

/// Run `f` on the screen's pixels with the screen's lock held, so nothing
/// draws through it meanwhile (no terminal output, cursor flip or pointer
/// move): the `screenshot`'s capture. Returns whether a [`Canvas`] held
/// the pixels meanwhile: its command draws without the lock, so the read
/// may be torn (canvases come and go under the lock, so the answer holds
/// for the whole read; a command that holds one but has stopped drawing,
/// such as a finished `mandelbrot` waiting for a key, counts too). `f`
/// must not allocate (see the module comment), and runs with interrupts
/// off: keep it short. `None` without a terminal, once a crash report
/// started, or when this CPU is already drawing (see [`with_screen`]).
pub fn read_pixels(f: impl FnOnce(&Framebuffer)) -> Option<bool> {
    with_screen(|s| {
        f(&s.fb);
        s.canvas.is_some()
    })
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
    /// Feed `s` to virtual terminal `vt`, `\n` as CR LF (not drawn yet).
    fn feed(&mut self, vt: usize, s: &str) {
        let term = &mut self.terms[vt];
        for (i, line) in s.split('\n').enumerate() {
            if i > 0 {
                term.write(b"\r\n");
            }
            term.write(line.as_bytes());
        }
        if vt == self.shown {
            self.show_cursor();
        }
    }

    /// The shown virtual terminal's grid.
    fn term(&self) -> &Terminal {
        &self.terms[self.shown]
    }

    fn term_mut(&mut self) -> &mut Terminal {
        &mut self.terms[self.shown]
    }

    /// Show virtual terminal `vt` instead, drawing all of it unless a
    /// canvas has the pixels (dropping it will).
    fn switch(&mut self, vt: usize) {
        if vt == self.shown {
            return;
        }
        self.shown = vt;
        self.draw_tabs();
        if self.canvas.is_none() {
            self.redraw();
        }
    }

    /// The header's right end: a box per virtual terminal, numbered from 1,
    /// the shown one filled (see [`tab_x`]).
    fn draw_tabs(&mut self) {
        for vt in 0..VTS {
            let Some(x) = tab_x(self.fb.width(), vt) else { return };
            let (fg, bg) = if vt == self.shown { (fb::BG, fb::TEXT) } else { (fb::TEXT, fb::BG) };
            self.uncover(x, TAB_Y, TAB_W, TAB_H);
            // Outlined, so a hidden one's box shows too.
            self.fb.fill_rect(x, TAB_Y, TAB_W, TAB_H, fb::TEXT);
            self.fb.fill_rect(x + 1, TAB_Y + 1, TAB_W - 2, TAB_H - 2, bg);
            let digit = char::from_digit(vt as u32 + 1, 10).unwrap_or('?');
            let style = Style::new(FontWeight::Bold, fb::CONSOLE_SIZE, fg, bg);
            let (gx, gy) = (x + (TAB_W - self.cell_w) / 2, TAB_Y + (TAB_H - CELL_H) / 2);
            self.fb.glyph_clipped(gx, gy, digit, style, self.cell_w);
        }
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
        let cols = self.term().cols();
        // Take the cursor off first, as the shadow has the cell (the
        // terminal may already hold other cells there, before the scroll
        // below moves the pixels).
        if let Some((c, r)) = self.cursor_drawn.take() {
            self.draw(c, r, self.shadow[r * cols + c], false);
        }
        if let Some(s) = self.term_mut().take_scroll() {
            let n = s.lines.unsigned_abs();
            let (top, bottom) = (self.y0 + s.top * CELL_H, self.y0 + s.bottom * CELL_H);
            let len = (s.bottom - s.top - n) * CELL_H;
            let shift = n * CELL_H;
            let rows = s.top * cols..s.bottom * cols;
            // The pixels move: the pointer mustn't move with them.
            self.uncover(0, top, self.fb.width(), bottom - top);
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
        for r in 0..self.term().rows() {
            if !self.term_mut().take_dirty(r) {
                continue;
            }
            for c in 0..cols {
                let cell = self.term().cell(c, r);
                if self.shadow[r * cols + c] != cell {
                    self.draw(c, r, cell, false);
                    self.shadow[r * cols + c] = cell;
                }
            }
        }
        if self.blink_on
            && self.term().view_offset() == 0
            && let Some((c, r)) = self.term().cursor_in_view()
        {
            self.draw(c, r, self.term().cell(c, r), true);
            self.cursor_drawn = Some((c, r));
        }
    }

    /// A [`Canvas`] gave the pixels back, or the shell took them back
    /// ([`reclaim_canvas`]): clear what it drew (everything below the
    /// header) and draw every cell of the terminal again.
    fn resume(&mut self) {
        self.canvas = None;
        self.redraw();
    }

    /// The desktop gave the whole screen back: draw the header, the
    /// terminals' boxes and the shown terminal, all of it.
    fn redraw_all(&mut self) {
        let w = self.fb.width();
        self.uncover(0, 0, w, fb::CONSOLE_TOP);
        fb::draw_header(&self.fb);
        self.draw_tabs();
        self.redraw();
    }

    /// Clear everything below the header and draw every cell of the shown
    /// terminal.
    fn redraw(&mut self) {
        let (w, h) = (self.fb.width(), self.fb.height());
        self.uncover(0, fb::CONSOLE_TOP, w, h.saturating_sub(fb::CONSOLE_TOP));
        self.fb.fill_rect(0, fb::CONSOLE_TOP, w, h.saturating_sub(fb::CONSOLE_TOP), fb::BG);
        // A blank cell draws as the background just filled in.
        self.shadow.fill(Cell::BLANK);
        self.cursor_drawn = None;
        self.term_mut().touch_all();
        self.render();
    }

    /// Whether what was drawn (the shadow) is the shown terminal's view, cell
    /// for cell (for the tests).
    #[cfg(feature = "ktest")]
    fn shadow_matches_grid(&self) -> bool {
        let t = self.term();
        let cols = t.cols();
        (0..t.rows()).all(|r| (0..cols).all(|c| self.shadow[r * cols + c] == t.cell(c, r)))
    }

    /// Draw `cell` at (`col`, `row`), colours swapped for inverse or for the
    /// cursor (not for both).
    fn draw(&mut self, col: usize, row: usize, cell: Cell, cursor: bool) {
        let (mut fg, mut bg) = (rgb(cell.fg, fb::TEXT), rgb(cell.bg, fb::BG));
        if cell.flags.contains(Flags::INVERSE) != cursor {
            core::mem::swap(&mut fg, &mut bg);
        }
        let (x, y, w) = (self.x0 + col * self.cell_w, self.y0 + row * CELL_H, self.cell_w);
        self.uncover(x, y, w, CELL_H);
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

    /// About to draw the `w` x `h` pixels at (`x`, `y`): take the mouse
    /// pointer off if it is drawn there (putting back what it covered;
    /// [`drawing`] draws it again afterwards).
    fn uncover(&mut self, x: usize, y: usize, w: usize, h: usize) {
        if self.pointer.overlaps(x, y, w, h) {
            self.pointer.hide(&mut Pixels(&self.fb));
        }
    }

    /// Draw the mouse pointer at [`Self::pointer_at`] (moving it there), or
    /// take it off if there is none or a canvas has the pixels (a canvas
    /// draws without the lock, so the arrow can't be kept out of its way).
    /// The one place that decides whether the pointer shows.
    fn place_pointer(&mut self) {
        let px = &mut Pixels(&self.fb);
        match self.pointer_at.filter(|_| self.canvas.is_none()) {
            Some((x, y)) => {
                let (outline, fill) = (self.fb.encode(POINTER_OUTLINE), self.fb.encode(POINTER_FILL));
                self.pointer.show(px, x, y, outline, fill);
            }
            None => self.pointer.hide(px),
        }
    }
}

/// The mouse pointer's colours.
const POINTER_OUTLINE: u32 = 0x000000;
const POINTER_FILL: u32 = 0xffffff;

/// The framebuffer as the pointer's save-under sees it: encoded pixels.
struct Pixels<'a>(&'a Framebuffer);

impl lumen_input::Surface for Pixels<'_> {
    fn size(&self) -> (usize, usize) {
        (self.0.width(), self.0.height())
    }

    fn get(&self, x: usize, y: usize) -> u32 {
        self.0.pixel(x, y)
    }

    fn set(&mut self, x: usize, y: usize, value: u32) {
        self.0.put_encoded(x, y, value);
    }
}

/// The header's virtual-terminal boxes: their size, the gap between them
/// and their top edge.
const TAB_W: usize = 28;
const TAB_H: usize = 24;
const TAB_GAP: usize = 6;
const TAB_Y: usize = 36;

/// The left edge of virtual terminal `vt`'s box in the header of a screen
/// `width` pixels wide: the boxes end a margin from its right edge. `None`
/// on a screen too narrow to keep them clear of the header's text (no boxes
/// then).
fn tab_x(width: usize, vt: usize) -> Option<usize> {
    let all = VTS * TAB_W + (VTS - 1) * TAB_GAP;
    let x0 = width.checked_sub(fb::MARGIN + all).filter(|&x| x >= fb::HEADER_W)?;
    Some(x0 + vt * (TAB_W + TAB_GAP))
}

/// Who holds a [`Canvas`]: the shell makes one for each command it runs
/// (`Ctx::canvas` takes the canvas for it) and, when the command returns,
/// takes back a canvas that owner left behind ([`reclaim_canvas`]), but
/// never another's (another console's command may be drawing). A command
/// on a virtual terminal's shell has that terminal, which its canvas
/// brings to the front.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owner {
    id: NonZeroU64,
    vt: Option<usize>,
}

impl Owner {
    /// A new owner, equal to no other, on no virtual terminal (a command on
    /// serial).
    pub fn new() -> Owner {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Owner { id: NonZeroU64::new(NEXT.fetch_add(1, Ordering::Relaxed)).expect("2^64 owners"), vt: None }
    }

    /// A new owner for a command on virtual terminal `vt`.
    ///
    /// # Panics
    /// If `vt >= VTS`.
    pub fn on_vt(vt: usize) -> Owner {
        assert!(vt < VTS, "no virtual terminal {vt}");
        Owner { vt: Some(vt), ..Owner::new() }
    }

    /// A new owner, equal to no other, on the same virtual terminal as this
    /// one (or none): for each command a shell runs.
    pub fn renew(self) -> Owner {
        Owner { vt: self.vt, ..Owner::new() }
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
/// use `Ctx::canvas`). An owner on a virtual terminal shows it first (its
/// keys go there: `crate::vt` follows [`shown`]), and while the canvas is
/// held no other terminal can be shown ([`show`]).
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
        // Drawn when the canvas goes (`resume`); the header's tabs now.
        if let Some(vt) = owner.vt {
            s.switch(vt);
        }
        // (`drawing` takes the pointer off now: `place_pointer`.)
        // SAFETY: the terminal's handle draws nothing while the canvas is
        // live (`Screen::canvas`), so the two never draw at the same time
        // (see `Canvas`'s `Sync` for a canvas no longer live).
        let fb = unsafe { s.fb.alias() };
        let (width, height) = canvas_dims(&fb);
        Ok(Canvas { fb, width, height, epoch })
    });
    match taken {
        Some(result) => result,
        None if STOPPED.load(Ordering::Acquire) => Err(CanvasError::Stopped),
        None => Err(CanvasError::NoTerminal),
    }
}

/// The size in pixels a [`Canvas`] has (or would have), without taking one;
/// `None` without a terminal (or once a crash report started).
pub fn canvas_size() -> Option<(usize, usize)> {
    with_screen(|s| canvas_dims(&s.fb))
}

/// The size of a canvas on `fb`: all of it below the header.
fn canvas_dims(fb: &Framebuffer) -> (usize, usize) {
    (fb.width(), fb.height().saturating_sub(fb::CONSOLE_TOP))
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

/// The whole screen, header included, for the desktop (#153): a
/// [`Canvas`]'s rules (one holder at a time, the terminals drawing nothing
/// meanwhile but still taking output, their pointer off, its draws ending
/// when the epoch changes), but it draws through the screen's lock: each
/// row of [`ScreenCanvas::write_span`] takes the lock, checks the canvas is
/// still live and still the holder, and writes the row or drops it. So a
/// present preempted halfway can't land a stale row on the terminal
/// [`leave_desktop`] redrew under the same lock, and once a crash report
/// begins (`stop`, which changes the epoch) no row starts.
///
/// Its pixels are `lumen_gfx`'s (`0xXXRRGGBB`), converted to the
/// framebuffer's channel layout as they are written. Dropping it gives the
/// screen back (the header and the shown terminal are drawn again) if it
/// still holds it; [`leave_desktop`] does that without dropping it.
pub struct ScreenCanvas {
    owner: Owner,
    width: usize,
    height: usize,
    /// [`CANVAS_EPOCH`] when it was taken.
    epoch: u64,
    /// For the ktests: change the epoch after this many more rows written
    /// (`a_present_stops_at_the_next_row_once_the_canvas_dies`).
    #[cfg(feature = "ktest")]
    kill_after: Option<usize>,
}

/// Take the whole screen for `owner` (the desktop's): refused while a
/// command holds a [`Canvas`] (`CanvasError::InUse`), with no terminal, or
/// once a crash report started.
pub fn take_screen(owner: Owner) -> Result<ScreenCanvas, CanvasError> {
    debug_assert!(!drawing_here());
    let taken = with_screen(|s| {
        if s.canvas.is_some() {
            return Err(CanvasError::InUse);
        }
        // As in `canvas`: an epoch read after `stop` comes with `STOPPED`.
        let epoch = CANVAS_EPOCH.load(Ordering::Acquire);
        if STOPPED.load(Ordering::Acquire) {
            return Err(CanvasError::Stopped);
        }
        s.canvas = Some(owner);
        s.whole = true;
        s.whole_epoch = epoch;
        // (`drawing` takes the terminals' pointer off now.)
        Ok(ScreenCanvas {
            owner,
            width: s.fb.width(),
            height: s.fb.height(),
            epoch,
            #[cfg(feature = "ktest")]
            kill_after: None,
        })
    });
    match taken {
        Some(result) => result,
        None if STOPPED.load(Ordering::Acquire) => Err(CanvasError::Stopped),
        None => Err(CanvasError::NoTerminal),
    }
}

impl ScreenCanvas {
    /// Whether its rows are still written: false once [`leave_desktop`]
    /// took the screen back or a crash report started.
    pub fn is_live(&self) -> bool {
        CANVAS_EPOCH.load(Ordering::Acquire) == self.epoch
    }

    /// Row `y` from column `x` on, as `0x00RRGGBB` pixels, into `out` (as
    /// many as fit, at most to the row's end), read under the screen's
    /// lock: `desktop check` compares the screen with the back buffer.
    /// False, reading nothing, once it isn't live.
    pub fn read_span(&self, x: usize, y: usize, out: &mut [u32]) -> bool {
        with_screen(|s| {
            if !self.is_live() || s.canvas != Some(self.owner) || y >= self.height {
                return false;
            }
            for (i, px) in out.iter_mut().take(self.width.saturating_sub(x)).enumerate() {
                *px = s.fb.decode(s.fb.pixel(x + i, y));
            }
            true
        })
        .unwrap_or(false)
    }

    /// For the ktests: the epoch changes (as `leave_desktop` or a crash
    /// would change it) once `rows` more rows were written.
    #[cfg(feature = "ktest")]
    pub fn kill_after(&mut self, rows: usize) {
        self.kill_after = Some(rows);
    }
}

impl lumen_gfx::Target for ScreenCanvas {
    fn size(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }

    /// One row under the screen's lock, dropped (never a panic) outside
    /// the screen or once the canvas isn't live. Nothing allocates.
    fn write_span(&mut self, x: u32, y: u32, pixels: &[u32]) {
        let (x, y) = (x as usize, y as usize);
        let _ = with_screen(|s| {
            if !self.is_live() || s.canvas != Some(self.owner) || y >= self.height {
                return;
            }
            for (i, &p) in pixels.iter().take(self.width.saturating_sub(x)).enumerate() {
                s.fb.put_encoded(x + i, y, s.fb.encode(p));
            }
        });
        #[cfg(feature = "ktest")]
        if let Some(n) = self.kill_after.as_mut() {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.kill_after = None;
                CANVAS_EPOCH.fetch_add(1, Ordering::AcqRel);
            }
        }
    }
}

impl Drop for ScreenCanvas {
    fn drop(&mut self) {
        // This take still holds the screen (not taken back by
        // `leave_desktop`, after which a resume's newer take may hold it
        // for the same owner): give it back, live or not (a ktest's hook
        // may have changed the epoch without taking it back). After a crash
        // report started, nothing (`with_screen` refuses).
        let _ = with_screen(|s| {
            if s.canvas == Some(self.owner) && s.whole && s.whole_epoch == self.epoch {
                s.canvas = None;
                s.whole = false;
                s.redraw_all();
            }
        });
    }
}

/// Alt+F`n` while the desktop is shown (`crate::vt`, under its router
/// lock): if `owner` holds the [`ScreenCanvas`], under the screen's lock,
/// change the canvas epoch (no desktop row is written after this), give
/// the screen back, show terminal `vt` and draw the header, the boxes and
/// that terminal. True if it did; false (nothing changed) if `owner`
/// doesn't hold the whole screen (another holder's canvas still refuses
/// the switch, as `show` does), there is no terminal, or a crash report
/// started.
///
/// # Panics
/// If `vt >= VTS`.
pub fn leave_desktop(owner: Owner, vt: usize) -> bool {
    assert!(vt < VTS, "no virtual terminal {vt}");
    with_screen(|s| {
        if s.canvas != Some(owner) || !s.whole {
            return false;
        }
        CANVAS_EPOCH.fetch_add(1, Ordering::AcqRel);
        s.canvas = None;
        s.whole = false;
        s.shown = vt;
        s.redraw_all();
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

/// Feeds virtual terminal `.1` through `fmt::Write` (`\n` as CR LF; see
/// [`Screen::feed`]), drawing nothing.
struct Feed<'a>(&'a mut Screen, usize);

impl Write for Feed<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.0.feed(self.1, s);
        Ok(())
    }
}

/// Write `s` to virtual terminal `vt` (`\n` as CR LF) and draw it if that
/// one is shown; false if the terminal didn't take it (no terminal, a crash
/// report started, or this CPU is already drawing). Under a [`Canvas`] it
/// is taken, and drawn when the canvas is dropped or taken back.
///
/// # Panics
/// If `vt >= VTS`.
pub fn write_str_to(vt: usize, s: &str) -> bool {
    write_fmt_to(vt, format_args!("{s}"))
}

/// Write formatted text (`\n` as CR LF) to virtual terminal `vt` and draw
/// it if that one is shown, all under one lock: a log line from another
/// CPU can't land inside an escape sequence written in this one call.
/// `args` must not allocate (see the module comment). False if the
/// terminal didn't take it (see [`write_str_to`]).
///
/// # Panics
/// If `vt >= VTS`.
pub fn write_fmt_to(vt: usize, args: fmt::Arguments<'_>) -> bool {
    assert!(vt < VTS, "no virtual terminal {vt}");
    with_screen(|screen| {
        let _ = Feed(screen, vt).write_fmt(args);
        if vt == screen.shown {
            screen.render();
        }
    })
    .is_some()
}

/// Why [`show`] didn't switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShowError {
    /// No terminal, a crash report started, or this CPU is already drawing
    /// (see [`write_str_to`]).
    NoTerminal,
    /// There are only [`VTS`].
    NoSuchTerminal,
    /// A command has the screen ([`Canvas`]): its terminal stays in front.
    InUse,
}

impl fmt::Display for ShowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ShowError::NoTerminal => f.write_str("no terminal"),
            ShowError::NoSuchTerminal => write!(f, "there are terminals 1 to {VTS}"),
            ShowError::InUse => f.write_str("a command has the screen"),
        }
    }
}

/// Show virtual terminal `vt` (0 is the first) and draw it (nothing to do
/// if it is shown already, also while a command has the screen). This is
/// the screen side only: `crate::vt` routes each key to the terminal
/// [`shown`] when it routes it, so keys follow; `crate::vt::show` switches
/// under the router's lock, so a key being routed lands on one side (a
/// canvas's switch, in [`canvas`], doesn't take it).
pub fn show(vt: usize) -> Result<(), ShowError> {
    if vt >= VTS {
        return Err(ShowError::NoSuchTerminal);
    }
    with_screen(|s| {
        if s.canvas.is_some() && vt != s.shown {
            return Err(ShowError::InUse);
        }
        s.switch(vt);
        Ok(())
    })
    .unwrap_or(Err(ShowError::NoTerminal))
}

/// The virtual terminal shown (0 without a terminal, once a crash report
/// started, or on a CPU already drawing).
pub fn shown() -> usize {
    with_screen(|s| s.shown).unwrap_or(0)
}

/// Point the mouse pointer's tip at framebuffer pixel (`x`, `y`) and draw
/// it there (taken off its old place), or, while a [`Canvas`] has the
/// pixels, when the canvas goes. False if there is no terminal to draw it
/// with, a crash report started, or this CPU is already drawing.
pub fn move_pointer(x: usize, y: usize) -> bool {
    with_screen(|s| s.pointer_at = Some((x, y))).is_some()
}

/// Take the mouse pointer off the screen (what it covered comes back)
/// until [`move_pointer`] again.
pub fn hide_pointer() {
    with_screen(|s| s.pointer_at = None);
}

/// Where the mouse pointer's tip is drawn, if it is (not before the first
/// [`move_pointer`], after [`hide_pointer`], while a [`Canvas`] has the
/// pixels, with the tip off the screen, or without a terminal).
pub fn pointer_drawn() -> Option<(usize, usize)> {
    with_screen(|s| s.pointer.drawn_at()).flatten()
}

/// The framebuffer's width and height in pixels (`None` without a
/// terminal, or once a crash report started): what the tablet's positions
/// are scaled to.
pub fn screen_size() -> Option<(usize, usize)> {
    with_screen(|s| (s.fb.width(), s.fb.height()))
}

/// How many lines virtual terminal `vt`'s view is scrolled back (for the
/// tests); `None` without a terminal.
#[cfg(feature = "ktest")]
pub fn view_offset(vt: usize) -> Option<usize> {
    with_screen(|s| s.terms[vt].view_offset())
}

/// Shows the first virtual terminal when dropped: a test that shows another
/// keeps one, so failing half-way doesn't leave the next tests on it.
#[cfg(feature = "ktest")]
pub struct ShowFirstAfter;

#[cfg(feature = "ktest")]
impl Drop for ShowFirstAfter {
    fn drop(&mut self) {
        let _ = show(0);
    }
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
    write_fmt_to(0, format_args!("\x1b[0;{lvl}m{level:<5}\x1b[0m \x1b[90m{tag}:\x1b[0m \x1b[{msg}m{args}\x1b[0m\n"));
}

/// Move virtual terminal `vt`'s view with `scroll` (which returns whether
/// it moved) and draw it if shown; a move is logged (debug: serial only,
/// the screen shows up to `info`).
fn move_view(vt: usize, scroll: impl FnOnce(&mut Terminal) -> bool) {
    assert!(vt < VTS, "no virtual terminal {vt}");
    let moved = with_screen(|s| {
        // The view stays put while a canvas has the screen (nothing would
        // show the move; the terminal comes back as it was left).
        if s.canvas.is_some() || !scroll(&mut s.terms[vt]) {
            return None;
        }
        if vt == s.shown {
            s.render();
        }
        let t = &s.terms[vt];
        Some((t.view_offset(), top_row(t)))
    });
    // Logged outside the screen's lock: the logger takes its line lock
    // before the screen's, so logging under the screen's lock could
    // deadlock with another CPU's log line.
    if let Some(Some((view, top))) = moved {
        log::debug!(target: "term", "view {view} lines up, top row \"{top}\"");
    }
}

/// Scroll virtual terminal `vt`'s view back (`pages` > 0) or forward by
/// `pages` half screens.
pub fn scroll_half_pages(vt: usize, pages: isize) {
    move_view(vt, |t| t.scroll_view(pages * (t.rows() as isize / 2).max(1)));
}

/// Show virtual terminal `vt`'s screen again (typing does).
pub fn reset_view(vt: usize) {
    move_view(vt, Terminal::reset_view);
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
    /// The virtual terminal shown (0 is the first): the fields above but
    /// `cells_drawn` and `paused` are its own.
    pub shown: usize,
}

/// The shown virtual terminal's state, or `None` without a terminal (or
/// once a crash report started).
pub fn status() -> Option<Status> {
    with_screen(|s| {
        let t = s.term();
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
            shown: s.shown,
        }
    })
}

/// The shown virtual terminal's view: its cells, row by row, and the
/// cursor if it is in the view (its row counted in the view; shown or
/// hidden). The cells and the cursor are read under one lock (a log line
/// from another CPU can't land between them), after a first look at the
/// size: the vector is allocated between the two, as nothing may allocate
/// under the lock. (Every terminal has the same size, which never changes,
/// so a switch in between doesn't matter.)
fn view_cells() -> Option<(usize, Vec<Cell>, Option<Cursor>)> {
    let (cols, rows) = with_screen(|s| (s.term().cols(), s.term().rows()))?;
    let mut cells = Vec::with_capacity(cols * rows);
    let cursor = with_screen(|s| {
        let t = s.term();
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

/// The shown virtual terminal's view as text: what the shell's `term text`
/// prints, and how the tests read what the screen shells did.
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
        let rows = with_screen(|s| s.term().rows()).ok_or("no terminal")?;
        let setup = alloc::format!("\x1b[{rows};1H\x1b[2K");
        let counts = with_screen(|s| {
            let mut count = |bytes: &[u8]| {
                let before = s.cells_drawn;
                s.term_mut().write(bytes);
                s.render();
                s.cells_drawn - before
            };
            count(setup.as_bytes());
            let two = count(b"ab");
            let feed = count(b"\r\n");
            let idle = count(b"");
            (two, feed, idle, s.term().cols())
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
        reset_view(0);
        let (cols, rows, w, h) = with_screen(|s| {
            let (cols, rows) = (s.term().cols(), s.term().rows());
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
            s.term_mut().write(fill.as_bytes());
            s.render();
            for step in STEPS {
                s.term_mut().write(step);
                s.render();
            }
            let ok = s.shadow_matches_grid();
            snapshot(s, &mut drawn);
            // Forget what was drawn (no cell holds U+0001) and draw it all.
            s.shadow.fill(Cell { ch: '\u{1}', ..Cell::BLANK });
            s.term_mut().touch_all();
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
    fn the_pointer_survives_redraws_and_leaves_no_trail() {
        // The mouse pointer over the terminal while it draws, scrolls (whole
        // screen and a region), clears, redraws everything and redraws the
        // header's boxes below it, moved across all of it: after each step
        // its tip is drawn where it was put, and once it is taken off, the
        // pixels (everything below the header, its margins included, and
        // the header's boxes) are exactly what drawing everything afresh
        // gives. Then a canvas takes it off until dropped. (The ktests boot
        // with no tablet: nothing else moves it.)
        use crate::{kassert, kassert_eq};
        const STEPS: [&[u8]; 7] = [
            b"\x1b[r\x1b[999;1H\r\nunder the pointer",
            b"\r\nscrolled below the pointer",
            b"\x1b[3;12r\x1b[12;1H\nregion scrolled",
            b"\x1b[r\x1b[H\x1b[2Jcleared",
            b"\x1b[1;1H\x1b[44m          \x1b[0m",
            b"\x1b[999;1H\r\n",
            // A step that also redraws everything (below).
            b"",
        ];
        reset_view(0);
        kassert_eq!(pointer_drawn(), None);
        let (rows, x0, y0, cell_w, fb_w, fb_h) =
            with_screen(|s| (s.term().rows(), s.x0, s.y0, s.cell_w, s.fb.width(), s.fb.height()))
                .ok_or("no terminal")?;
        let tabs = tab_x(fb_w, 0).ok_or("no header boxes on this screen")?;
        // Tips: inside the grid on a cell corner and between cell edges, on
        // the bottom row's first cell, straddling the header and the grid, on the header's boxes,
        // at the right edge, and in the bottom margin's corner (clipped).
        let tips = [
            (x0 + 3 * cell_w, y0 + 2 * CELL_H),
            (x0 + 3 * cell_w + 5, y0 + 2 * CELL_H + 9),
            (x0, y0 + (rows - 1) * CELL_H),
            (x0 + 40, y0 - 8),
            (tabs + 3, TAB_Y + 2),
            (tabs + TAB_W + 1, TAB_Y + TAB_H - 4),
            (fb_w - 1, y0 + 5 * CELL_H),
            (x0 + 2, fb_h - 3),
        ];
        let mut fill = alloc::string::String::new();
        for r in 1..=rows {
            let _ = write!(fill, "\x1b[{r};1Hrow {r} \x1b[4{}m{r}{r}{r}\x1b[0m", r % 8);
        }
        // Allocated before taking the screen's lock again.
        let below = fb_w * (fb_h - fb::CONSOLE_TOP);
        let (mut drawn, mut fresh) = (Vec::with_capacity(below), Vec::with_capacity(below));
        let (mut tabs_drawn, mut tabs_fresh) = (Vec::with_capacity(fb_w * TAB_H), Vec::with_capacity(fb_w * TAB_H));
        let snapshot = |s: &Screen, grid: &mut Vec<u32>, header: &mut Vec<u32>| {
            grid.extend((fb::CONSOLE_TOP..fb_h).flat_map(|y| (0..fb_w).map(move |x| s.fb.pixel(x, y))));
            header.extend((TAB_Y..TAB_Y + TAB_H).flat_map(|y| (0..fb_w).map(move |x| s.fb.pixel(x, y))));
        };
        let result = with_screen(|s| {
            let tip_ok = |s: &Screen, (x, y): (usize, usize)| {
                s.pointer.drawn_at() == Some((x, y)) && s.fb.pixel(x, y) == s.fb.encode(POINTER_OUTLINE)
            };
            let mut bad = None;
            s.term_mut().write(fill.as_bytes());
            s.render();
            for (i, &tip) in tips.iter().enumerate() {
                s.pointer_at = Some(tip);
                s.place_pointer();
                for (j, step) in STEPS.iter().enumerate() {
                    s.term_mut().write(step);
                    if step.is_empty() {
                        s.redraw();
                    } else {
                        s.render();
                    }
                    // The header's boxes redrawn below it.
                    s.draw_tabs();
                    s.place_pointer();
                    if bad.is_none() && !tip_ok(s, tip) {
                        bad = Some((i, j));
                    }
                }
            }
            s.pointer_at = None;
            s.place_pointer();
            let off = s.pointer.drawn_at().is_none();
            snapshot(s, &mut drawn, &mut tabs_drawn);
            // Everything afresh: below the header cleared and every cell
            // drawn, and the header's boxes.
            s.redraw();
            s.draw_tabs();
            snapshot(s, &mut fresh, &mut tabs_fresh);
            (bad, off)
        });
        let (bad, off) = result.ok_or("no terminal")?;
        if let Some((tip, step)) = bad {
            crate::ktest::diag(format_args!("the pointer's tip {tip} isn't drawn after step {step}"));
        }
        kassert!(bad.is_none());
        kassert!(off);
        if let Some(i) = drawn.iter().zip(&fresh).position(|(a, b)| a != b) {
            crate::ktest::diag(format_args!("first differing pixel: ({}, {})", i % fb_w, fb::CONSOLE_TOP + i / fb_w));
        }
        kassert!(drawn == fresh);
        kassert!(tabs_drawn == tabs_fresh);
        // A canvas takes it off (the pixels below come back) until dropped;
        // moved meanwhile, it comes back at its new place.
        let (a, b) = (tips[0], tips[1]);
        kassert!(move_pointer(a.0, a.1));
        kassert_eq!(pointer_drawn(), Some(a));
        let canvas = canvas(Owner::new()).map_err(|_| "no canvas")?;
        kassert_eq!(pointer_drawn(), None);
        kassert!(move_pointer(b.0, b.1));
        kassert_eq!(pointer_drawn(), None);
        let restored = with_screen(|s| s.fb.pixel(a.0, a.1) != s.fb.encode(POINTER_OUTLINE)).ok_or("no terminal")?;
        kassert!(restored);
        drop(canvas);
        kassert_eq!(pointer_drawn(), Some(b));
        hide_pointer();
        kassert_eq!(pointer_drawn(), None);
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
            s.term_mut().write(b"\x1b[r\x1b[999;1H\r\nx\x1b[H");
            s.blink_on = true;
            s.render();
            let before = s.cursor_drawn.is_some();
            let moved = s.term_mut().scroll_view(1);
            s.blink_on = true;
            s.render();
            let hidden = s.cursor_drawn.is_none() && s.term().cursor_in_view().is_some();
            let _ = s.term_mut().reset_view();
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
            write_fmt_to(0, format_args!("keep {i}\n"));
        }
        let (moved, top, view) = with_screen(|s| {
            let moved = s.term_mut().scroll_view(5);
            s.render();
            (moved, top_row(s.term()), s.term().view_offset())
        })
        .ok_or("no terminal")?;
        kassert!(moved && view == 5);
        kassert!(write_str_to(0, "more 1\nmore 2\n"));
        let after = status().ok_or("no terminal")?;
        kassert_eq!(format!("{}", after.top_row), format!("{top}"));
        kassert_eq!(after.view, view + 2);
        reset_view(0);
        kassert_eq!(status().ok_or("no terminal")?.view, 0);
        Ok(())
    }
);

crate::ktest!(
    fn a_log_line_starts_from_the_default_colours() {
        // Whatever the shell's output left set (bold, inverse, a background), a
        // log line starts afresh.
        use crate::kassert;
        write_str_to(0, "\x1b[1;7;44m");
        log::info!(target: "termtest", "after a dirty pen");
        write_str_to(0, "\x1b[0m");
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
        kassert!(write_fmt_to(0, format_args!("{}", LockProbe(&under))));
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
        reset_view(0);
        kassert!(write_str_to(0, &alloc::format!("{BEFORE}\n")));
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
            kassert!(write_str_to(0, &alloc::format!("{UNDER}\n")));
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
            let (cols, rows) = (s.term().cols(), s.term().rows());
            let cells_match = s.shadow_matches_grid();
            // Whether the row starting with `needle` is on screen with a
            // pixel of its text that isn't background.
            let ink = |needle: &str| {
                let row = (0..rows).find(|&r| needle.chars().enumerate().all(|(i, ch)| s.term().cell(i, r).ch == ch));
                row.is_some_and(|r| {
                    let (y, w) = (s.y0 + r * CELL_H, needle.len() * s.cell_w);
                    (y..y + CELL_H).any(|y| (s.x0..s.x0 + w).any(|x| s.fb.pixel(x, y) != bg))
                })
            };
            // Every visible character has ink on screen (not just in the
            // shadow, which the redraw keeps in step either way).
            let all_inked = (0..rows).all(|r| {
                (0..cols).all(|c| {
                    s.term().cell(c, r).ch == ' ' || {
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

crate::ktest!(
    fn virtual_terminals_keep_their_own_text() {
        // Text written to a hidden terminal isn't drawn, and log lines go
        // to the first only; showing another draws all of it (every cell as
        // its grid holds it), the header's boxes marking it; showing the
        // first again brings its text back. No terminal past the last.
        use crate::{kassert, kassert_eq};
        use alloc::format;
        const FIRST: &str = "on the first terminal";
        const SECOND: &str = "on the second terminal";
        const LOGGED: &str = "logged while the second is shown";
        kassert_eq!(shown(), 0);
        let _first = ShowFirstAfter;
        reset_view(0);
        kassert!(write_str_to(0, &format!("{FIRST}\n")));
        let has = |needle: &str| view_text().map(|v| v.rows.iter().any(|r| r.contains(needle))).ok_or("no terminal");
        let drawn = || status().map(|s| s.cells_drawn).ok_or("no terminal");
        let before = drawn()?;
        kassert!(write_str_to(1, &format!("{SECOND}\n")));
        kassert_eq!(drawn()?, before);
        kassert!(has(FIRST)? && !has(SECOND)?);
        show(1).map_err(|_| "no switch")?;
        kassert_eq!(shown(), 1);
        kassert!(has(SECOND)? && !has(FIRST)?);
        log::info!(target: "termtest", "{LOGGED}");
        kassert!(!has(LOGGED)?);
        // What is drawn is the second's grid; the box of the shown terminal
        // is filled (a pixel inside it, clear of its digit and outline).
        let (cells_match, tabs) = with_screen(|s| {
            let cells_match = s.shadow_matches_grid();
            let text = s.fb.encode(fb::TEXT);
            let tabs: [Option<bool>; VTS] =
                core::array::from_fn(|vt| tab_x(s.fb.width(), vt).map(|x| s.fb.pixel(x + 2, TAB_Y + 2) == text));
            (cells_match, tabs)
        })
        .ok_or("no terminal")?;
        kassert!(cells_match);
        kassert_eq!(tabs, [Some(false), Some(true), Some(false), Some(false)]);
        kassert_eq!(show(VTS), Err(ShowError::NoSuchTerminal));
        kassert_eq!(shown(), 1);
        show(0).map_err(|_| "no switch back")?;
        kassert!(has(FIRST)? && has(LOGGED)? && !has(SECOND)?);
        Ok(())
    }
);

crate::ktest!(
    fn the_header_boxes_keep_clear_of_its_text() {
        // On a 1280-pixel screen the four boxes end a margin from the right
        // edge, in order; on one too narrow to keep them right of the
        // header's text there are none.
        use crate::kassert_eq;
        let all = VTS * TAB_W + (VTS - 1) * TAB_GAP;
        kassert_eq!(tab_x(1280, 0), Some(1280 - fb::MARGIN - all));
        kassert_eq!(tab_x(1280, VTS - 1).map(|x| x + TAB_W), Some(1280 - fb::MARGIN));
        kassert_eq!(tab_x(fb::HEADER_W + fb::MARGIN + all, 0), Some(fb::HEADER_W));
        kassert_eq!(tab_x(fb::HEADER_W + fb::MARGIN + all - 1, 0), None);
        kassert_eq!(tab_x(0, 0), None);
        Ok(())
    }
);

crate::ktest!(
    fn a_canvas_brings_its_terminal_to_the_front_and_keeps_it_there() {
        // A command on the third terminal taking the canvas shows the third
        // (its keys go there); no other can be shown while it holds it, and
        // dropping it draws the third, which stays shown. A canvas for a
        // command on serial (no terminal) leaves the shown one alone.
        use crate::{kassert, kassert_eq};
        kassert_eq!(shown(), 0);
        let _first = ShowFirstAfter;
        let c = canvas(Owner::on_vt(2)).map_err(|_| "no canvas")?;
        kassert_eq!(shown(), 2);
        kassert_eq!(show(0), Err(ShowError::InUse));
        kassert!(show(2).is_ok());
        kassert_eq!(shown(), 2);
        drop(c);
        let (paused, cells_match) =
            with_screen(|s| (s.canvas.is_some(), s.shadow_matches_grid())).ok_or("no terminal")?;
        kassert!(!paused && cells_match);
        kassert_eq!(shown(), 2);
        show(0).map_err(|_| "no switch back")?;
        let c = canvas(Owner::new()).map_err(|_| "no canvas")?;
        kassert_eq!(shown(), 0);
        drop(c);
        Ok(())
    }
);

crate::ktest!(
    fn scrolling_a_hidden_terminal_leaves_the_shown_one_alone() {
        // Shift+PageUp from the second terminal's shell (a key routed to it
        // before a switch) scrolls the second's view, not the first's shown
        // one, and draws nothing.
        use crate::{kassert, kassert_eq};
        kassert_eq!(shown(), 0);
        reset_view(0);
        /// Brings the second's view back when dropped.
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                reset_view(1);
            }
        }
        let _reset = Reset;
        for i in 0..60 {
            kassert!(write_str_to(1, &alloc::format!("second {i}\n")));
        }
        let drawn = || status().map(|s| s.cells_drawn).ok_or("no terminal");
        let before = drawn()?;
        scroll_half_pages(1, 1);
        let views = with_screen(|s| (s.terms[0].view_offset(), s.terms[1].view_offset())).ok_or("no terminal")?;
        kassert!(views.0 == 0 && views.1 > 0);
        kassert_eq!(drawn()?, before);
        reset_view(1);
        kassert_eq!(view_offset(1).ok_or("no terminal")?, 0);
        Ok(())
    }
);
