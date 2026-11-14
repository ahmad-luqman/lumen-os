//! Conway's Game of Life on a `width` x `height` grid whose outside counts
//! as dead: nothing wraps around (a glider running into the bottom edge
//! ends as a still block there; a test checks the one `life glider`
//! starts).

use alloc::vec;
use alloc::vec::Vec;

/// Where a pattern starts on the grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// [`ORIGIN`] cells in from the top-left corner (heading down-right).
    TopLeft,
    /// In the middle (room to grow every way).
    Centre,
    /// [`ORIGIN`] cells in from the right edge, halfway down (heading left).
    Right,
}

/// How far in from the edges a pattern placed at a side starts, in cells.
pub const ORIGIN: usize = 4;

/// A seed pattern: its rows, `O` alive and anything else dead, and where it
/// starts.
#[derive(Clone, Copy, Debug)]
pub struct Pattern {
    pub name: &'static str,
    pub about: &'static str,
    pub rows: &'static [&'static str],
    pub place: Place,
}

impl Pattern {
    /// Its top-left cell on a `width` x `height` grid (see [`Place`]).
    pub fn origin(&self, width: usize, height: usize) -> (usize, usize) {
        let (w, h) = self.size();
        let middle = |grid: usize, size: usize| grid.saturating_sub(size) / 2;
        match self.place {
            Place::TopLeft => (ORIGIN.min(width.saturating_sub(w)), ORIGIN.min(height.saturating_sub(h))),
            Place::Centre => (middle(width, w), middle(height, h)),
            Place::Right => (width.saturating_sub(w + ORIGIN), middle(height, h)),
        }
    }

    /// Its size in cells: (width, height).
    pub fn size(&self) -> (usize, usize) {
        (self.rows.iter().map(|r| r.len()).max().unwrap_or(0), self.rows.len())
    }

    /// Its live cells, relative to its top-left corner.
    pub fn cells(&self) -> impl Iterator<Item = (usize, usize)> + '_ {
        self.rows
            .iter()
            .enumerate()
            .flat_map(|(y, row)| row.bytes().enumerate().filter(|&(_, b)| b == b'O').map(move |(x, _)| (x, y)))
    }
}

/// The patterns' names, in [`PATTERNS`]' order (for completion).
pub const NAMES: [&str; PATTERNS.len()] = {
    let mut names = [""; PATTERNS.len()];
    let mut i = 0;
    while i < PATTERNS.len() {
        names[i] = PATTERNS[i].name;
        i += 1;
    }
    names
};

/// The patterns the `life` command knows, by name.
pub const PATTERNS: &[Pattern] = &[
    Pattern {
        name: "glider",
        about: "the smallest spaceship, heading down-right (a block at the edge)",
        rows: &[".O.", "..O", "OOO"],
        place: Place::TopLeft,
    },
    Pattern {
        name: "gun",
        about: "Gosper's glider gun: a new glider every 30 generations",
        rows: &[
            "........................O...........",
            "......................O.O...........",
            "............OO......OO............OO",
            "...........O...O....OO............OO",
            "OO........O.....O...OO..............",
            "OO........O...O.OO....O.O...........",
            "..........O.....O.......O...........",
            "...........O...O....................",
            "............OO......................",
        ],
        place: Place::TopLeft,
    },
    Pattern {
        name: "lwss",
        about: "the lightweight spaceship, heading left from the right edge",
        rows: &[".O..O", "O....", "O...O", "OOOO."],
        place: Place::Right,
    },
    Pattern {
        name: "rpentomino",
        about: "five cells that grow chaotically (1103 generations on an endless plane)",
        rows: &[".OO", "OO.", ".O."],
        place: Place::Centre,
    },
    Pattern {
        name: "acorn",
        about: "seven cells that grow for 5206 generations on an endless plane",
        rows: &[".O.....", "...O...", "OO..OOO"],
        place: Place::Centre,
    },
];

/// The pattern called `name`.
pub fn pattern(name: &str) -> Option<&'static Pattern> {
    PATTERNS.iter().find(|p| p.name == name)
}

/// A grid of cells and the generation it is at.
#[derive(Clone, Debug)]
pub struct Life {
    width: usize,
    height: usize,
    cells: Vec<bool>,
    next: Vec<bool>,
    generation: u64,
}

impl Life {
    /// An empty `width` x `height` grid at generation 0.
    ///
    /// # Panics
    /// If `width * height` overflows `usize`.
    pub fn new(width: usize, height: usize) -> Life {
        let n = width.checked_mul(height).expect("a Life grid's cell count overflows usize");
        Life { width, height, cells: vec![false; n], next: vec![false; n], generation: 0 }
    }

    /// Where cell (`x`, `y`) is kept (inside the grid).
    fn index(&self, x: usize, y: usize) -> usize {
        y * self.width + x
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn height(&self) -> usize {
        self.height
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether cell (`x`, `y`) is alive (false outside the grid).
    pub fn get(&self, x: usize, y: usize) -> bool {
        x < self.width && y < self.height && self.cells[self.index(x, y)]
    }

    /// Make cell (`x`, `y`) alive or dead (nothing outside the grid).
    pub fn set(&mut self, x: usize, y: usize, alive: bool) {
        if x < self.width && y < self.height {
            let i = self.index(x, y);
            self.cells[i] = alive;
        }
    }

    /// Bring `pattern`'s cells to life with its top-left corner at (`x`,
    /// `y`); cells falling outside the grid are left out.
    pub fn place(&mut self, pattern: &Pattern, x: usize, y: usize) {
        for (dx, dy) in pattern.cells() {
            self.set(x.saturating_add(dx), y.saturating_add(dy), true);
        }
    }

    /// How many cells are alive.
    pub fn population(&self) -> usize {
        self.cells.iter().filter(|&&c| c).count()
    }

    /// Live neighbours of (`x`, `y`) (cells outside the grid are dead).
    fn neighbours(&self, x: usize, y: usize) -> usize {
        let mut n = 0;
        for ny in y.saturating_sub(1)..=(y + 1).min(self.height - 1) {
            for nx in x.saturating_sub(1)..=(x + 1).min(self.width - 1) {
                if (nx, ny) != (x, y) && self.cells[self.index(nx, ny)] {
                    n += 1;
                }
            }
        }
        n
    }

    /// Advance one generation (a live cell with 2 or 3 live neighbours lives
    /// on, a dead one with exactly 3 is born), calling `changed(x, y, alive)`
    /// for every cell that changed.
    pub fn step(&mut self, mut changed: impl FnMut(usize, usize, bool)) {
        for y in 0..self.height {
            for x in 0..self.width {
                let i = self.index(x, y);
                let alive = matches!((self.cells[i], self.neighbours(x, y)), (true, 2 | 3) | (false, 3));
                self.next[i] = alive;
                if alive != self.cells[i] {
                    changed(x, y, alive);
                }
            }
        }
        core::mem::swap(&mut self.cells, &mut self.next);
        self.generation += 1;
    }
}
