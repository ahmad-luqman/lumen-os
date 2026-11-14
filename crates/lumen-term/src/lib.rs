//! A terminal emulator's core, without the drawing: bytes in, a grid of
//! character cells out.
//!
//! [`Terminal::write`] runs the bytes through `vte`'s parser (ANSI/VT
//! escapes, UTF-8) into a `cols` x `rows` grid of [`Cell`]s, with lines that
//! scroll off the top kept in a scrollback ring. The kernel draws the grid on
//! the framebuffer; tests read it back.
//!
//! Everything is allocated in [`Terminal::new`]: writing never allocates, so
//! the kernel can log while it holds the frame allocator's lock (a host test
//! checks it with a counting allocator).
//!
//! Supported:
//! - printing with autowrap (the cursor waits at the last column until the
//!   next character), wide characters (two cells; `unicode-width`, ambiguous
//!   ones narrow), zero-width characters dropped;
//! - C0: BS, HT, LF/VT/FF (without a carriage return: the writer adds it),
//!   CR;
//! - ESC 7/8 (save/restore the cursor), D (index), E (next line),
//!   M (reverse index), H (set a tab stop), c (reset);
//! - CSI: cursor movement A B C D E F G H f d \`, erase J (0 to 3) K X,
//!   insert and delete @ P L M, scroll S T, SGR m, scroll region r, save and
//!   restore s u, tab clear g, DECSET/DECRST `?25` (cursor) and `?7`
//!   (autowrap);
//! - SGR: reset, bold, inverse, 16 colours (30-37, 90-97, 40-47, 100-107),
//!   256 colours (`38;5;n`, `48;5;n`, also with colons), truecolor
//!   (`38;2;r;g;b`) approximated to the nearest of the 256, defaults 39 49.
//!
//! Anything else (OSC, DCS, unknown sequences, malformed ones) is ignored.
//!
//! The view can be scrolled back into the scrollback ([`Terminal::scroll_view`]);
//! output doesn't move it: it stays on the lines it shows while new ones
//! arrive.

#![no_std]

extern crate alloc;

mod cell;
pub mod palette;

use alloc::vec;
use alloc::vec::Vec;
use core::ops::Range;
use unicode_width::UnicodeWidthChar;

pub use cell::{Cell, Color, Flags};

/// Columns between the default tab stops.
const TAB_WIDTH: usize = 8;

/// A scroll of the view's rows `top..bottom` by `lines` (positive: up, as a
/// line feed at the bottom does) that hasn't been drawn yet: the renderer can
/// move pixels instead of drawing every cell again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scroll {
    pub top: usize,
    pub bottom: usize,
    pub lines: isize,
}

/// Where the cursor is and whether it is shown (DECTCEM).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cursor {
    pub col: usize,
    pub row: usize,
    pub visible: bool,
}

/// The pen: what printed and erased cells get.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pen {
    fg: Color,
    bg: Color,
    flags: Flags,
}

impl Pen {
    const DEFAULT: Pen = Pen { fg: Color::Default, bg: Color::Default, flags: Flags::NONE };
}

/// What ESC 7 / CSI s save.
#[derive(Clone, Copy, Debug)]
struct Saved {
    col: usize,
    row: usize,
    pen: Pen,
    wrap_pending: bool,
}

/// The grid and the state the escape sequences change (what `vte` drives;
/// [`Terminal`] adds the parser).
struct Grid {
    cols: usize,
    rows: usize,
    /// The screen, row by row.
    cells: Vec<Cell>,
    /// Scrollback ring: `capacity` lines of `cols` cells; the oldest of the
    /// `len` lines in use is at `head`.
    history: Vec<Cell>,
    capacity: usize,
    head: usize,
    len: usize,
    /// How many lines the view is scrolled back (0: it shows the screen).
    view: usize,
    col: usize,
    row: usize,
    /// The cursor is at the last column and the next character wraps first.
    wrap_pending: bool,
    pen: Pen,
    saved: Option<Saved>,
    /// The scroll region, `top..bottom`.
    top: usize,
    bottom: usize,
    tabs: Vec<bool>,
    cursor_visible: bool,
    autowrap: bool,
    /// Rows of the view changed since the renderer last took them.
    dirty: Vec<bool>,
    scroll: Option<Scroll>,
}

impl Grid {
    fn new(cols: usize, rows: usize, capacity: usize) -> Grid {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let mut g = Grid {
            cols,
            rows,
            cells: vec![Cell::BLANK; cols * rows],
            history: vec![Cell::BLANK; cols * capacity],
            capacity,
            head: 0,
            len: 0,
            view: 0,
            col: 0,
            row: 0,
            wrap_pending: false,
            pen: Pen::DEFAULT,
            saved: None,
            top: 0,
            bottom: rows,
            tabs: vec![false; cols],
            cursor_visible: true,
            autowrap: true,
            dirty: vec![true; rows],
            scroll: None,
        };
        g.reset_tabs();
        g
    }

    fn reset_tabs(&mut self) {
        for (i, t) in self.tabs.iter_mut().enumerate() {
            *t = i > 0 && i % TAB_WIDTH == 0;
        }
    }

    // --- Damage -------------------------------------------------------

    /// Screen row `row` changed.
    fn touch(&mut self, row: usize) {
        if self.view == 0 {
            self.dirty[row] = true;
        } else {
            // Scrolled back: the screen's rows are lower in the view, or out
            // of it. Rare enough to mark the whole view (the renderer draws
            // only the cells that differ from what it drew).
            self.touch_all();
        }
    }

    fn touch_all(&mut self) {
        self.dirty.fill(true);
        self.scroll = None;
    }

    /// Record a scroll of the screen's rows `top..bottom` by `lines` (the
    /// cells already moved): the dirty marks move with their rows, and the
    /// rows brought in are dirty.
    fn record_scroll(&mut self, top: usize, bottom: usize, lines: isize) {
        if self.view != 0 {
            return self.touch_all();
        }
        let n = lines.unsigned_abs();
        if n >= bottom - top {
            // Nothing on show moves: draw the region's cells (a pending
            // scroll stays valid, drawn first).
            self.dirty[top..bottom].fill(true);
            return;
        }
        if lines > 0 {
            self.dirty.copy_within(top + n..bottom, top);
            self.dirty[bottom - n..bottom].fill(true);
        } else {
            self.dirty.copy_within(top..bottom - n, top + n);
            self.dirty[top..top + n].fill(true);
        }
        // Scrolls of one region add up (the marks moved with every row that
        // stayed in it, and rows brought in are dirty).
        self.scroll = match self.scroll {
            None => Some(Scroll { top, bottom, lines }),
            Some(s) if (s.top, s.bottom) == (top, bottom) => match s.lines + lines {
                0 => None,
                sum if sum.unsigned_abs() < bottom - top => Some(Scroll { lines: sum, ..s }),
                _ => {
                    self.dirty[top..bottom].fill(true);
                    None
                }
            },
            // Can't be merged with the one before: drop both and draw their
            // regions' cells (the marks moved as if both were drawn).
            Some(s) => {
                self.dirty[s.top..s.bottom].fill(true);
                self.dirty[top..bottom].fill(true);
                None
            }
        };
    }

    // --- Cells --------------------------------------------------------

    fn at(&self, col: usize, row: usize) -> Cell {
        self.cells[row * self.cols + col]
    }

    fn row_mut(&mut self, row: usize) -> &mut [Cell] {
        let c = self.cols;
        &mut self.cells[row * c..(row + 1) * c]
    }

    /// What erasing leaves: a blank in the pen's background.
    fn blank(&self) -> Cell {
        Cell::blank(self.pen.bg)
    }

    /// Blank any half of a wide character left alone on `row` (by erasing,
    /// inserting, deleting or overwriting cells).
    fn fix_wide(&mut self, row: usize) {
        let r = self.row_mut(row);
        for c in 0..r.len() {
            let cell = r[c];
            let lone = if cell.flags.contains(Flags::WIDE) {
                !r.get(c + 1).is_some_and(|n| n.flags.contains(Flags::SPACER))
            } else if cell.flags.contains(Flags::SPACER) {
                c == 0 || !r[c - 1].flags.contains(Flags::WIDE)
            } else {
                false
            };
            if lone {
                r[c] = Cell::blank(cell.bg);
            }
        }
    }

    /// Erase `cols` of `row` (clamped to the line).
    fn erase(&mut self, row: usize, cols: Range<usize>) {
        let blank = self.blank();
        let end = cols.end.min(self.cols);
        let start = cols.start.min(end);
        self.row_mut(row)[start..end].fill(blank);
        self.fix_wide(row);
        self.touch(row);
    }

    // --- Printing -----------------------------------------------------

    fn print(&mut self, ch: char) {
        let width = match ch.width() {
            Some(w @ 1..=2) => w,
            _ => return,
        };
        if width > self.cols {
            return;
        }
        if self.wrap_pending {
            self.wrap_pending = false;
            self.col = 0;
            self.line_feed();
        }
        if width == 2 && self.col + 1 == self.cols {
            if !self.autowrap {
                return;
            }
            // No room for both halves: leave the last column blank and wrap.
            let row = self.row;
            self.erase(row, self.col..self.cols);
            self.col = 0;
            self.line_feed();
        }
        let (col, row, pen) = (self.col, self.row, self.pen);
        let r = self.row_mut(row);
        if width == 2 {
            r[col] = Cell { ch, fg: pen.fg, bg: pen.bg, flags: pen.flags.union(Flags::WIDE) };
            r[col + 1] = Cell { ch: ' ', fg: pen.fg, bg: pen.bg, flags: pen.flags.union(Flags::SPACER) };
        } else {
            r[col] = Cell { ch, fg: pen.fg, bg: pen.bg, flags: pen.flags };
        }
        self.fix_wide(row);
        self.touch(row);
        if col + width == self.cols {
            // Stay on the last column; with autowrap, the next character
            // goes to the next line.
            self.col = self.cols - 1;
            self.wrap_pending = self.autowrap;
        } else {
            self.col = col + width;
        }
    }

    // --- Moving and scrolling -----------------------------------------

    fn goto(&mut self, col: usize, row: usize) {
        self.col = col.min(self.cols - 1);
        self.row = row.min(self.rows - 1);
        self.wrap_pending = false;
    }

    /// The rows the cursor moves up and down in: the scroll region if it is
    /// in it, else the screen.
    fn vertical_bounds(&self) -> (usize, usize) {
        if (self.top..self.bottom).contains(&self.row) { (self.top, self.bottom) } else { (0, self.rows) }
    }

    fn line_feed(&mut self) {
        if self.row + 1 == self.bottom {
            self.scroll_up(self.top, self.bottom, 1);
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
    }

    fn reverse_index(&mut self) {
        if self.row == self.top {
            self.scroll_down(self.top, self.bottom, 1);
        } else if self.row > 0 {
            self.row -= 1;
        }
    }

    /// Scroll rows `top..bottom` up by `n`; lines leaving the top of the
    /// whole screen go to the scrollback.
    fn scroll_up(&mut self, top: usize, bottom: usize, n: usize) {
        let n = n.min(bottom - top);
        if n == 0 {
            return;
        }
        if top == 0 && bottom == self.rows {
            for r in 0..n {
                self.keep(r);
            }
        }
        let c = self.cols;
        self.cells.copy_within((top + n) * c..bottom * c, top * c);
        let blank = self.blank();
        self.cells[(bottom - n) * c..bottom * c].fill(blank);
        self.record_scroll(top, bottom, n as isize);
    }

    fn scroll_down(&mut self, top: usize, bottom: usize, n: usize) {
        let n = n.min(bottom - top);
        if n == 0 {
            return;
        }
        let c = self.cols;
        self.cells.copy_within(top * c..(bottom - n) * c, (top + n) * c);
        let blank = self.blank();
        self.cells[top * c..(top + n) * c].fill(blank);
        self.record_scroll(top, bottom, -(n as isize));
    }

    /// Copy screen row `row` into the scrollback, dropping its oldest line
    /// when full. A view scrolled back stays on the lines it shows.
    fn keep(&mut self, row: usize) {
        if self.capacity == 0 {
            return;
        }
        let slot = (self.head + self.len) % self.capacity;
        if self.len == self.capacity {
            self.head = (self.head + 1) % self.capacity;
        } else {
            self.len += 1;
        }
        if self.view == self.len {
            // The view's top line (the oldest) is gone: it shows the next.
            self.touch_all();
        } else if self.view > 0 {
            self.view += 1;
        }
        let c = self.cols;
        self.history[slot * c..(slot + 1) * c].copy_from_slice(&self.cells[row * c..(row + 1) * c]);
    }

    // --- Escape sequences ---------------------------------------------

    fn reset(&mut self) {
        self.cells.fill(Cell::BLANK);
        self.goto(0, 0);
        self.pen = Pen::DEFAULT;
        self.saved = None;
        self.top = 0;
        self.bottom = self.rows;
        self.reset_tabs();
        self.cursor_visible = true;
        self.autowrap = true;
        self.touch_all();
    }

    fn save(&mut self) {
        self.saved = Some(Saved { col: self.col, row: self.row, pen: self.pen, wrap_pending: self.wrap_pending });
    }

    fn restore(&mut self) {
        let s = self.saved.unwrap_or(Saved { col: 0, row: 0, pen: Pen::DEFAULT, wrap_pending: false });
        self.goto(s.col, s.row);
        self.pen = s.pen;
        self.wrap_pending = s.wrap_pending && self.autowrap && self.col == self.cols - 1;
    }

    fn tab(&mut self) {
        self.col = (self.col + 1..self.cols).find(|&c| self.tabs[c]).unwrap_or(self.cols - 1);
        self.wrap_pending = false;
    }

    fn erase_display(&mut self, mode: u16) {
        let (col, row) = (self.col, self.row);
        match mode {
            0 => {
                self.erase(row, col..self.cols);
                for r in row + 1..self.rows {
                    self.erase(r, 0..self.cols);
                }
            }
            1 => {
                for r in 0..row {
                    self.erase(r, 0..self.cols);
                }
                self.erase(row, 0..col + 1);
            }
            2 => {
                for r in 0..self.rows {
                    self.erase(r, 0..self.cols);
                }
            }
            3 => {
                self.head = 0;
                self.len = 0;
                if self.view != 0 {
                    self.view = 0;
                    self.touch_all();
                }
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let (col, row) = (self.col, self.row);
        match mode {
            0 => self.erase(row, col..self.cols),
            1 => self.erase(row, 0..col + 1),
            2 => self.erase(row, 0..self.cols),
            _ => {}
        }
    }

    /// Insert `n` blank cells at the cursor, shifting the rest of the line
    /// right (`insert`), or delete `n`, shifting it left.
    fn shift_cells(&mut self, n: usize, insert: bool) {
        let (col, row, cols) = (self.col, self.row, self.cols);
        let n = n.min(cols - col);
        let blank = self.blank();
        let r = self.row_mut(row);
        if insert {
            r.copy_within(col..cols - n, col + n);
            r[col..col + n].fill(blank);
        } else {
            r.copy_within(col + n..cols, col);
            r[cols - n..].fill(blank);
        }
        self.fix_wide(row);
        self.touch(row);
        self.wrap_pending = false;
    }

    /// Insert (`insert`) or delete `n` lines at the cursor, within the
    /// scroll region.
    fn shift_lines(&mut self, n: usize, insert: bool) {
        if !(self.top..self.bottom).contains(&self.row) {
            return;
        }
        if insert {
            self.scroll_down(self.row, self.bottom, n);
        } else {
            self.scroll_up(self.row, self.bottom, n);
        }
        self.col = 0;
        self.wrap_pending = false;
    }

    /// DECSTBM: 1-based and inclusive; 0 (or missing) means the screen's
    /// edge. A region under two lines is refused.
    fn set_region(&mut self, top: usize, bottom: usize) {
        let top = top.max(1) - 1;
        let bottom = if bottom == 0 { self.rows } else { bottom.min(self.rows) };
        if top + 1 < bottom {
            self.top = top;
            self.bottom = bottom;
            self.goto(0, 0);
        }
    }

    fn sgr(&mut self, params: &vte::Params) {
        if params.is_empty() {
            self.pen = Pen::DEFAULT;
            return;
        }
        let mut it = params.iter();
        while let Some(p) = it.next() {
            let pen = &mut self.pen;
            match p[0] {
                0 => *pen = Pen::DEFAULT,
                1 => pen.flags = pen.flags.union(Flags::BOLD),
                22 => pen.flags = pen.flags.without(Flags::BOLD),
                7 => pen.flags = pen.flags.union(Flags::INVERSE),
                27 => pen.flags = pen.flags.without(Flags::INVERSE),
                n @ 30..=37 => pen.fg = Color::Indexed((n - 30) as u8),
                n @ 90..=97 => pen.fg = Color::Indexed((n - 90 + 8) as u8),
                n @ 40..=47 => pen.bg = Color::Indexed((n - 40) as u8),
                n @ 100..=107 => pen.bg = Color::Indexed((n - 100 + 8) as u8),
                39 => pen.fg = Color::Default,
                49 => pen.bg = Color::Default,
                n @ (38 | 48) => {
                    let color = if p.len() > 1 { extended(&p[1..]) } else { extended_from(&mut it) };
                    match (color, n) {
                        (Some(c), 38) => pen.fg = c,
                        (Some(c), _) => pen.bg = c,
                        (None, _) => {}
                    }
                }
                _ => {}
            }
        }
    }
}

/// An extended colour given with colons: `5:n`, `2:r:g:b`, or `2::r:g:b`
/// (with an empty colour space id).
fn extended(sub: &[u16]) -> Option<Color> {
    match *sub {
        [5, n, ..] => index(n),
        [2, _, r, g, b, ..] => truecolor(r, g, b),
        [2, r, g, b] => truecolor(r, g, b),
        _ => None,
    }
}

/// An extended colour given with semicolons (`5;n`, `2;r;g;b`): takes the
/// parameters it needs.
fn extended_from<'a>(it: &mut impl Iterator<Item = &'a [u16]>) -> Option<Color> {
    match it.next()?[0] {
        5 => index(it.next()?[0]),
        2 => {
            let (r, g, b) = (it.next()?[0], it.next()?[0], it.next()?[0]);
            truecolor(r, g, b)
        }
        _ => None,
    }
}

fn index(n: u16) -> Option<Color> {
    u8::try_from(n).ok().map(Color::Indexed)
}

fn truecolor(r: u16, g: u16, b: u16) -> Option<Color> {
    let (r, g, b) = (u8::try_from(r).ok()?, u8::try_from(g).ok()?, u8::try_from(b).ok()?);
    Some(Color::Indexed(palette::nearest(r, g, b)))
}

/// A sequence's `i`th parameter; `default` when missing or 0.
fn arg(params: &vte::Params, i: usize, default: u16) -> usize {
    match params.iter().nth(i).map(|p| p[0]) {
        None | Some(0) => usize::from(default),
        Some(n) => usize::from(n),
    }
}

/// A sequence's `i`th parameter; 0 when missing.
fn arg0(params: &vte::Params, i: usize) -> u16 {
    params.iter().nth(i).map_or(0, |p| p[0])
}

impl vte::Perform for Grid {
    fn print(&mut self, c: char) {
        Grid::print(self, c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => {
                self.col = self.col.saturating_sub(1);
                self.wrap_pending = false;
            }
            0x09 => self.tab(),
            0x0a..=0x0c => {
                self.wrap_pending = false;
                self.line_feed();
            }
            0x0d => {
                self.col = 0;
                self.wrap_pending = false;
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore {
            return;
        }
        if intermediates == b"?" {
            let on = match action {
                'h' => true,
                'l' => false,
                _ => return,
            };
            for p in params.iter() {
                match p[0] {
                    25 => self.cursor_visible = on,
                    7 => {
                        self.autowrap = on;
                        self.wrap_pending &= on;
                    }
                    _ => {}
                }
            }
            return;
        }
        if !intermediates.is_empty() {
            return;
        }
        let n = arg(params, 0, 1);
        let (col, row) = (self.col, self.row);
        let (top, bottom) = self.vertical_bounds();
        match action {
            'A' => self.goto(col, row.saturating_sub(n).max(top)),
            'B' => self.goto(col, (row + n).min(bottom - 1)),
            'C' => self.goto(col.saturating_add(n), row),
            'D' => self.goto(col.saturating_sub(n), row),
            'E' => self.goto(0, (row + n).min(bottom - 1)),
            'F' => self.goto(0, row.saturating_sub(n).max(top)),
            'G' | '`' => self.goto(n - 1, row),
            'd' => self.goto(col, n - 1),
            'H' | 'f' => self.goto(arg(params, 1, 1) - 1, n - 1),
            'J' => self.erase_display(arg0(params, 0)),
            'K' => self.erase_line(arg0(params, 0)),
            'X' => {
                self.erase(row, col..col.saturating_add(n));
                self.wrap_pending = false;
            }
            '@' => self.shift_cells(n, true),
            'P' => self.shift_cells(n, false),
            'L' => self.shift_lines(n, true),
            'M' => self.shift_lines(n, false),
            'S' => self.scroll_up(self.top, self.bottom, n),
            'T' => self.scroll_down(self.top, self.bottom, n),
            'm' => self.sgr(params),
            'r' => self.set_region(usize::from(arg0(params, 0)), usize::from(arg0(params, 1))),
            // With parameters (`CSI l ; r s`) it would set left and right
            // margins, which aren't supported.
            's' if params.len() <= 1 && arg0(params, 0) == 0 => self.save(),
            'u' => self.restore(),
            'g' => match arg0(params, 0) {
                0 => self.tabs[col] = false,
                3 => self.tabs.fill(false),
                _ => {}
            },
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        match byte {
            b'7' => self.save(),
            b'8' => self.restore(),
            b'D' => {
                self.wrap_pending = false;
                self.line_feed();
            }
            b'E' => {
                self.col = 0;
                self.wrap_pending = false;
                self.line_feed();
            }
            b'M' => {
                self.wrap_pending = false;
                self.reverse_index();
            }
            b'H' => self.tabs[self.col] = true,
            b'c' => self.reset(),
            _ => {}
        }
    }
}

/// A terminal: the parser and the grid it fills.
pub struct Terminal {
    parser: vte::Parser,
    grid: Grid,
}

impl Terminal {
    /// A blank `cols` x `rows` terminal (at least 1 x 1) keeping up to
    /// `scrollback` lines that scroll off the top.
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Terminal {
        Terminal { parser: vte::Parser::new(), grid: Grid::new(cols, rows, scrollback) }
    }

    /// Run `bytes` through the parser (a sequence may span calls).
    pub fn write(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.grid, bytes);
    }

    pub fn cols(&self) -> usize {
        self.grid.cols
    }

    pub fn rows(&self) -> usize {
        self.grid.rows
    }

    /// The cell the view shows at (`col`, `row`).
    ///
    /// # Panics
    /// If (`col`, `row`) is outside the grid.
    pub fn cell(&self, col: usize, row: usize) -> Cell {
        let g = &self.grid;
        assert!(col < g.cols && row < g.rows, "cell ({col}, {row}) outside {}x{}", g.cols, g.rows);
        // The view's row `row`, counted from the oldest scrollback line.
        let line = g.len - g.view + row;
        if line < g.len { g.history[(g.head + line) % g.capacity * g.cols + col] } else { g.at(col, line - g.len) }
    }

    /// The screen's cursor (scrolling the view back doesn't move it).
    pub fn cursor(&self) -> Cursor {
        Cursor { col: self.grid.col, row: self.grid.row, visible: self.grid.cursor_visible }
    }

    /// Where in the view the cursor is: `None` while it is hidden or
    /// scrolled out of the view.
    pub fn cursor_in_view(&self) -> Option<(usize, usize)> {
        let g = &self.grid;
        let row = g.row + g.view;
        (g.cursor_visible && row < g.rows).then_some((g.col, row))
    }

    /// Lines in the scrollback.
    pub fn scrollback_len(&self) -> usize {
        self.grid.len
    }

    pub fn scrollback_capacity(&self) -> usize {
        self.grid.capacity
    }

    /// How many lines the view is scrolled back.
    pub fn view_offset(&self) -> usize {
        self.grid.view
    }

    /// Scroll the view back (`lines` > 0) or forward, within the
    /// scrollback; returns whether it moved.
    pub fn scroll_view(&mut self, lines: isize) -> bool {
        let g = &mut self.grid;
        let view = g.view.saturating_add_signed(lines).min(g.len);
        if view == g.view {
            return false;
        }
        g.view = view;
        g.touch_all();
        true
    }

    /// Show the screen again; returns whether the view moved.
    pub fn reset_view(&mut self) -> bool {
        self.scroll_view(-(self.grid.view as isize))
    }

    /// The scroll to draw before the dirty rows, if one is pending (taking
    /// it).
    pub fn take_scroll(&mut self) -> Option<Scroll> {
        self.grid.scroll.take()
    }

    /// Whether row `row` of the view changed since it was last taken
    /// (clearing the mark).
    pub fn take_dirty(&mut self, row: usize) -> bool {
        core::mem::take(&mut self.grid.dirty[row])
    }

    /// Mark every row of the view to be drawn again.
    pub fn touch_all(&mut self) {
        self.grid.touch_all();
    }
}

#[cfg(test)]
mod tests;
