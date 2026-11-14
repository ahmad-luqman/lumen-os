//! Conway's Game of Life on a `width` x `height` grid whose outside is dead
//! (a glider leaving it dies at the edge; nothing wraps around).

use alloc::vec;
use alloc::vec::Vec;

/// A seed pattern: its rows, `O` alive and anything else dead.
#[derive(Clone, Copy, Debug)]
pub struct Pattern {
    pub name: &'static str,
    pub about: &'static str,
    pub rows: &'static [&'static str],
}

impl Pattern {
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

/// The patterns the `life` command knows, by name.
pub const PATTERNS: &[Pattern] = &[
    Pattern { name: "glider", about: "the smallest spaceship, moving diagonally", rows: &[".O.", "..O", "OOO"] },
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
    },
    Pattern {
        name: "lwss",
        about: "the lightweight spaceship, moving sideways",
        rows: &[".O..O", "O....", "O...O", "OOOO."],
    },
    Pattern {
        name: "rpentomino",
        about: "five cells that settle after 1103 generations",
        rows: &[".OO", "OO.", ".O."],
    },
    Pattern {
        name: "acorn",
        about: "seven cells that grow for 5206 generations",
        rows: &[".O.....", "...O...", "OO..OOO"],
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
    pub fn new(width: usize, height: usize) -> Life {
        Life { width, height, cells: vec![false; width * height], next: vec![false; width * height], generation: 0 }
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
        x < self.width && y < self.height && self.cells[y * self.width + x]
    }

    /// Make cell (`x`, `y`) alive or dead (nothing outside the grid).
    pub fn set(&mut self, x: usize, y: usize, alive: bool) {
        if x < self.width && y < self.height {
            self.cells[y * self.width + x] = alive;
        }
    }

    /// Bring `pattern`'s cells to life with its top-left corner at (`x`,
    /// `y`); cells falling outside the grid are left out.
    pub fn place(&mut self, pattern: &Pattern, x: usize, y: usize) {
        for (dx, dy) in pattern.cells() {
            self.set(x + dx, y + dy, true);
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
                if (nx, ny) != (x, y) && self.cells[ny * self.width + nx] {
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
                let i = y * self.width + x;
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
