//! Drawing a [`Terminal`] on something, a cell at a time, drawing only what
//! changed: the renderer the kernel's text console and its terminal windows
//! share (#155). What the cells are drawn on (the framebuffer, a window's
//! surface) is a [`CellTarget`]; the renderer decides which cells to draw.
//!
//! It keeps a copy of every cell it drew (the shadow) and draws only the
//! cells that differ from it, in the rows the terminal marked as changed. A
//! scroll moves what was drawn (the target's rows and the shadow together),
//! so a line feed at the bottom draws one new line, not every cell. The
//! cursor, when drawn, is taken off first (its cell drawn again from the
//! shadow) and drawn last, solid or hollow ([`CursorLook`]); never while
//! the view is scrolled back.

use alloc::vec;
use alloc::vec::Vec;

use crate::{Cell, Terminal};

/// How the cursor is drawn on its cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorLook {
    /// The cell's colours swapped (an inverse cell shows plain).
    Solid,
    /// The cell as it is, outlined: a terminal that doesn't have the keys
    /// (an unfocused terminal window).
    Hollow,
}

/// What a [`Renderer`] draws on, in cells.
pub trait CellTarget {
    /// Draw `cell` at (`col`, `row`), with the cursor on it if `cursor`.
    fn draw_cell(&mut self, col: usize, row: usize, cell: Cell, cursor: Option<CursorLook>);
    /// Move `count` rows of what was drawn from row `from` to row `to` (up
    /// or down; the ranges may overlap): a scroll.
    fn move_rows(&mut self, from: usize, to: usize, count: usize);
}

/// A terminal's view as drawn on one target.
pub struct Renderer {
    cols: usize,
    /// What each cell of the view was last drawn as (row by row).
    shadow: Vec<Cell>,
    /// Where the cursor is drawn, if it is.
    cursor_drawn: Option<(usize, usize)>,
    /// Cells drawn so far (the shell's `term` shows the console's).
    cells_drawn: u64,
}

impl Renderer {
    /// A renderer for a `cols` x `rows` terminal whose target shows blank
    /// cells (it was just cleared to the default background).
    pub fn new(cols: usize, rows: usize) -> Renderer {
        Renderer { cols, shadow: vec![Cell::BLANK; cols * rows], cursor_drawn: None, cells_drawn: 0 }
    }

    /// Cells drawn so far.
    pub fn cells_drawn(&self) -> u64 {
        self.cells_drawn
    }

    /// The target was cleared to blank cells: forget what was drawn, and
    /// mark every row of `t` to be drawn again.
    pub fn forget(&mut self, t: &mut Terminal) {
        self.shadow.fill(Cell::BLANK);
        self.cursor_drawn = None;
        t.touch_all();
    }

    /// What the target shows is unknown (drawn over by someone else): the
    /// next [`Self::render`] draws every cell (and nothing is taken off
    /// first).
    pub fn invalidate(&mut self, t: &mut Terminal) {
        // No cell holds U+0001 (a control character is never printed).
        self.shadow.fill(Cell { ch: '\u{1}', ..Cell::BLANK });
        self.cursor_drawn = None;
        t.touch_all();
    }

    /// Where the cursor is drawn, if it is.
    pub fn cursor_drawn(&self) -> Option<(usize, usize)> {
        self.cursor_drawn
    }

    /// Draw what changed in `t` since the last time on `target`, then the
    /// cursor as `cursor` says (if it is in the view and the view isn't
    /// scrolled back).
    ///
    /// # Panics
    /// If `t` isn't the size the renderer was made for.
    pub fn render(&mut self, t: &mut Terminal, target: &mut impl CellTarget, cursor: Option<CursorLook>) {
        let cols = self.cols;
        assert_eq!(t.cols() * t.rows(), self.shadow.len(), "a terminal of another size");
        // Take the cursor off first, as the shadow has the cell (the
        // terminal may already hold other cells there, before the scroll
        // below moves what was drawn).
        if let Some((c, r)) = self.cursor_drawn.take() {
            self.draw(target, c, r, self.shadow[r * cols + c], None);
        }
        if let Some(s) = t.take_scroll() {
            let n = s.lines.unsigned_abs();
            let count = s.bottom - s.top - n;
            let rows = s.top * cols..s.bottom * cols;
            if s.lines > 0 {
                target.move_rows(s.top + n, s.top, count);
                self.shadow[rows].copy_within(n * cols.., 0);
            } else {
                target.move_rows(s.top, s.top + n, count);
                let end = rows.len() - n * cols;
                self.shadow[rows].copy_within(..end, n * cols);
            }
        }
        for r in 0..t.rows() {
            if !t.take_dirty(r) {
                continue;
            }
            for c in 0..cols {
                let cell = t.cell(c, r);
                if self.shadow[r * cols + c] != cell {
                    self.draw(target, c, r, cell, None);
                    self.shadow[r * cols + c] = cell;
                }
            }
        }
        if let Some(look) = cursor
            && t.view_offset() == 0
            && let Some((c, r)) = t.cursor_in_view()
        {
            self.draw(target, c, r, t.cell(c, r), Some(look));
            self.cursor_drawn = Some((c, r));
        }
    }

    /// Whether what was drawn (the shadow) is `t`'s view, cell for cell.
    pub fn matches(&self, t: &Terminal) -> bool {
        let cols = self.cols;
        (0..t.rows()).all(|r| (0..cols).all(|c| self.shadow[r * cols + c] == t.cell(c, r)))
    }

    fn draw(&mut self, target: &mut impl CellTarget, col: usize, row: usize, cell: Cell, cursor: Option<CursorLook>) {
        target.draw_cell(col, row, cell, cursor);
        self.cells_drawn += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Color;
    use std::vec::Vec;

    extern crate std;

    /// A target that keeps what was drawn on each cell (and where the
    /// cursor shows, and how), applying moves as pixels would move.
    struct Model {
        cols: usize,
        cells: Vec<Cell>,
        cursor: Vec<Option<CursorLook>>,
        draws: usize,
        moves: Vec<(usize, usize, usize)>,
    }

    impl Model {
        fn new(cols: usize, rows: usize) -> Model {
            Model {
                cols,
                cells: std::vec![Cell::BLANK; cols * rows],
                cursor: std::vec![None; cols * rows],
                draws: 0,
                moves: Vec::new(),
            }
        }
    }

    impl CellTarget for Model {
        fn draw_cell(&mut self, col: usize, row: usize, cell: Cell, cursor: Option<CursorLook>) {
            self.cells[row * self.cols + col] = cell;
            self.cursor[row * self.cols + col] = cursor;
            self.draws += 1;
        }

        fn move_rows(&mut self, from: usize, to: usize, count: usize) {
            let c = self.cols;
            self.cells.copy_within(from * c..(from + count) * c, to * c);
            self.cursor.copy_within(from * c..(from + count) * c, to * c);
            self.moves.push((from, to, count));
        }
    }

    fn view(t: &Terminal) -> Vec<Cell> {
        (0..t.rows()).flat_map(|r| (0..t.cols()).map(move |c| t.cell(c, r))).collect()
    }

    #[test]
    fn only_the_cells_that_changed_are_drawn() {
        // A blank terminal draws nothing but the cursor; one character more
        // draws that cell (and the cursor taken off and put back).
        let mut t = Terminal::new(10, 4, 0);
        let mut r = Renderer::new(10, 4);
        let mut m = Model::new(10, 4);
        r.render(&mut t, &mut m, None);
        assert_eq!(m.draws, 0);
        t.write(b"ab");
        r.render(&mut t, &mut m, None);
        assert_eq!(m.draws, 2);
        assert_eq!(m.cells, view(&t));
        t.write(b"c");
        r.render(&mut t, &mut m, None);
        assert_eq!(m.draws, 3);
        assert_eq!(r.cells_drawn(), 3);
        assert!(r.matches(&t));
    }

    #[test]
    fn a_line_feed_at_the_bottom_moves_the_rows_and_draws_one_line() {
        let mut t = Terminal::new(3, 3, 5);
        let mut r = Renderer::new(3, 3);
        let mut m = Model::new(3, 3);
        t.write(b"a\r\nb\r\nc");
        r.render(&mut t, &mut m, None);
        let before = m.draws;
        t.write(b"\r\nd");
        r.render(&mut t, &mut m, None);
        assert_eq!(m.moves, std::vec![(1, 0, 2)]);
        // The new bottom line's one character; the blanks there were
        // moved up as "c"'s row and drawn blank again where they differ.
        assert_eq!(m.cells, view(&t));
        assert!(m.draws - before <= 3, "{} cells drawn for one line", m.draws - before);
        // Scrolling the other way (reverse index at the top) moves down.
        t.write(b"\x1b[H\x1bM");
        r.render(&mut t, &mut m, None);
        assert_eq!(m.moves.last(), Some(&(0, 1, 2)));
        assert_eq!(m.cells, view(&t));
    }

    #[test]
    fn the_cursor_is_taken_off_then_drawn_solid_or_hollow() {
        let mut t = Terminal::new(5, 2, 0);
        let mut r = Renderer::new(5, 2);
        let mut m = Model::new(5, 2);
        t.write(b"x");
        r.render(&mut t, &mut m, Some(CursorLook::Solid));
        assert_eq!(m.cursor[1], Some(CursorLook::Solid));
        // Steady and hollow: the same cell, outlined.
        r.render(&mut t, &mut m, Some(CursorLook::Hollow));
        assert_eq!(m.cursor[1], Some(CursorLook::Hollow));
        // Off (a blink): the cell is drawn again plain, nowhere a cursor.
        r.render(&mut t, &mut m, None);
        assert!(m.cursor.iter().all(Option::is_none));
        assert_eq!(m.cells, view(&t));
        // It follows the text, leaving no cursor behind.
        r.render(&mut t, &mut m, Some(CursorLook::Solid));
        t.write(b"yz");
        r.render(&mut t, &mut m, Some(CursorLook::Solid));
        let at: Vec<usize> = (0..10).filter(|&i| m.cursor[i].is_some()).collect();
        assert_eq!(at, std::vec![3]);
    }

    #[test]
    fn no_cursor_is_drawn_while_the_view_is_scrolled_back() {
        let mut t = Terminal::new(4, 2, 10);
        let mut r = Renderer::new(4, 2);
        let mut m = Model::new(4, 2);
        t.write(b"1\r\n2\r\n3\r\n4");
        r.render(&mut t, &mut m, Some(CursorLook::Solid));
        assert!(t.scroll_view(1));
        r.render(&mut t, &mut m, Some(CursorLook::Solid));
        assert!(m.cursor.iter().all(Option::is_none));
        assert_eq!(m.cells, view(&t));
    }

    #[test]
    fn forget_draws_everything_again_on_a_cleared_target() {
        let mut t = Terminal::new(4, 2, 0);
        let mut r = Renderer::new(4, 2);
        let mut m = Model::new(4, 2);
        t.write(b"\x1b[31mab");
        r.render(&mut t, &mut m, None);
        // The target is cleared (a redraw); the renderer is told.
        m = Model::new(4, 2);
        r.forget(&mut t);
        r.render(&mut t, &mut m, None);
        assert_eq!(m.cells, view(&t));
        assert_eq!(m.cells[0].fg, Color::Indexed(1));
    }

    #[test]
    fn invalidate_draws_every_cell_once() {
        let mut t = Terminal::new(4, 3, 0);
        let mut r = Renderer::new(4, 3);
        let mut m = Model::new(4, 3);
        t.write(b"ab");
        r.render(&mut t, &mut m, Some(CursorLook::Solid));
        assert_eq!(r.cursor_drawn(), Some((2, 0)));
        let before = m.draws;
        r.invalidate(&mut t);
        assert_eq!(r.cursor_drawn(), None);
        r.render(&mut t, &mut m, None);
        assert_eq!(m.draws - before, 12);
        assert_eq!(m.cells, view(&t));
    }

    /// A tiny deterministic generator (xorshift64*).
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    #[test]
    fn drawing_in_steps_shows_what_a_full_repaint_shows() {
        // Random writes (scrolls, regions, wide characters, colours),
        // view scrolls and cursor looks, rendered after each step on a
        // target that moves what it drew: it always shows the view, with
        // the cursor exactly where (and as) asked, and nowhere else.
        const PIECES: &[&str] = &[
            "a",
            "Z",
            " ",
            "\r\n",
            "\n",
            "\r",
            "\t",
            "\x08",
            "\u{4e00}",
            "\x1b[2J",
            "\x1b[K",
            "\x1b[3;1H",
            "\x1b[1;2r",
            "\x1b[r",
            "\x1bM",
            "\x1b[S",
            "\x1b[T",
            "\x1b[L",
            "\x1b[M",
            "\x1b[@",
            "\x1b[P",
            "\x1b[7m",
            "\x1b[0m",
            "\x1b[32m",
            "\x1b[?25l",
            "\x1b[?25h",
            "\x1b[3J",
            "\x1bc",
        ];
        let looks = [None, Some(CursorLook::Solid), Some(CursorLook::Hollow)];
        let mut rng = Rng(0x7e44_0155_dead_beef);
        for case in 0..500 {
            let (cols, rows) = (1 + rng.below(8) as usize, 1 + rng.below(6) as usize);
            let mut t = Terminal::new(cols, rows, rng.below(6) as usize);
            let mut r = Renderer::new(cols, rows);
            let mut m = Model::new(cols, rows);
            for step in 0..40 {
                for _ in 0..rng.below(6) {
                    t.write(PIECES[rng.below(PIECES.len() as u64) as usize].as_bytes());
                }
                if rng.below(6) == 0 {
                    t.scroll_view(rng.below(5) as isize - 2);
                }
                let look = looks[rng.below(3) as usize];
                r.render(&mut t, &mut m, look);
                assert_eq!(m.cells, view(&t), "case {case} step {step}: cells");
                let want = look.filter(|_| t.view_offset() == 0).and_then(|l| t.cursor_in_view().map(|at| (at, l)));
                for (i, &shown) in m.cursor.iter().enumerate() {
                    let here = want.filter(|&((c, rr), _)| rr * cols + c == i).map(|(_, l)| l);
                    assert_eq!(shown, here, "case {case} step {step}: cursor at cell {i}");
                }
                assert!(r.matches(&t));
            }
        }
    }
}
