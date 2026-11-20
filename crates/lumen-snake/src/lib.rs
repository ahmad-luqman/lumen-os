//! Comet's logic (#201; `apps/comet` is its window): an original
//! snake-style game. A comet crosses a night sky of cells; each grain of
//! stardust it reaches lengthens its tail; the sky's edge, its own tail and
//! each level's asteroids end the flight. Host-tested, `no_std` with
//! `alloc`.
//!
//! - **One step at a time:** [`Game::step`] moves the comet one cell in its
//!   heading and says which cells it changed (the app's damage, nothing
//!   more: `every_step_reports_exactly_the_cells_it_changed`). The app
//!   steps on its own timer or on a Step press; the game has no clock.
//! - **Steering:** [`Game::turn`] sets the heading the next step takes; a
//!   turn straight back into its own tail is refused.
//! - **Growth:** a grain adds [`GROWTH`] cells to the tail, one each step
//!   (the tail's end stays put meanwhile), and one to the score.
//! - **Levels:** every [`LEVEL_EVERY`] grains the sky changes to the next
//!   of [`LEVELS`]' asteroid fields (the last one stays), the old asteroids
//!   gone and the new ones placed only on empty cells.
//! - **Stardust** goes on an empty cell chosen by the game's own
//!   generator ([`Rng`], seeded at [`Game::new`]), never on the comet or an
//!   asteroid; with no empty cell left the sky is full ([`State::Full`]).
//!   So one seed and one sequence of turns and steps always play the same
//!   game.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// Cells a grain of stardust adds to the tail.
pub const GROWTH: u32 = 3;
/// Grains between one level and the next.
pub const LEVEL_EVERY: u32 = 5;
/// The comet's cells at the start (its head and tail).
pub const START_LEN: u16 = 3;
/// The smallest sky a game takes: room for the comet's start and every
/// level's field.
pub const MIN_W: u16 = 12;
pub const MIN_H: u16 = 8;

/// Comet's sky (the app's board): columns and rows.
pub const SKY_W: u16 = 24;
pub const SKY_H: u16 = 18;

/// The levels' names, in order: each one's asteroid field is [`rocks`]'.
pub const LEVELS: [&str; 4] = ["Open sky", "Twin belts", "Crossing", "Ring of rocks"];

/// The game's generator: SplitMix64, small and good enough for a game, the
/// same on every machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A number in `0..n` (`n` > 0), every one equally likely: draws past
    /// the largest multiple of `n` are drawn again.
    pub fn below(&mut self, n: u32) -> u32 {
        assert!(n > 0, "below(0)");
        let n = u64::from(n);
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let v = self.next_u64();
            if v < zone {
                return (v % n) as u32;
            }
        }
    }
}

/// A cell of the sky: column `x`, row `y`, from the top left.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cell {
    pub x: u16,
    pub y: u16,
}

impl Cell {
    pub const fn new(x: u16, y: u16) -> Cell {
        Cell { x, y }
    }
}

/// Where the comet heads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

impl Dir {
    pub fn opposite(self) -> Dir {
        match self {
            Dir::Up => Dir::Down,
            Dir::Down => Dir::Up,
            Dir::Left => Dir::Right,
            Dir::Right => Dir::Left,
        }
    }
}

/// What a cell holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Content {
    Empty,
    Head,
    Tail,
    Dust,
    Rock,
}

/// What ended a flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Crash {
    /// The sky's edge.
    Edge,
    /// Its own tail.
    Tail,
    /// An asteroid.
    Rock,
}

/// Where a game is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Flying,
    Over(Crash),
    /// No empty cell is left for the next grain: the comet fills the sky.
    Full,
}

/// One game: see the crate's doc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Game {
    w: u16,
    h: u16,
    grid: Vec<Content>,
    /// The comet, its head first.
    body: VecDeque<Cell>,
    /// The heading of the last move, and the one the next step takes.
    moved: Dir,
    heading: Dir,
    grow: u32,
    dust: Option<Cell>,
    score: u32,
    level: usize,
    steps: u64,
    rng: Rng,
    state: State,
}

impl Game {
    /// A new game on a `w` × `h` sky (at least [`MIN_W`] × [`MIN_H`], else
    /// `None`), seeded with `seed`: the comet on the middle row at the left,
    /// heading right, its first grain placed.
    pub fn new(w: u16, h: u16, seed: u64) -> Option<Game> {
        if w < MIN_W || h < MIN_H {
            return None;
        }
        let mut g = Game {
            w,
            h,
            grid: alloc::vec![Content::Empty; usize::from(w) * usize::from(h)],
            body: VecDeque::new(),
            moved: Dir::Right,
            heading: Dir::Right,
            grow: 0,
            dust: None,
            score: 0,
            level: 0,
            steps: 0,
            rng: Rng::new(seed),
            state: State::Flying,
        };
        let y = h / 2;
        for x in (1..=START_LEN).rev() {
            let c = Cell::new(x, y);
            g.body.push_back(c);
            g.set(c, if x == START_LEN { Content::Head } else { Content::Tail });
        }
        let mut ignore = Vec::new();
        g.place_dust(&mut ignore);
        Some(g)
    }

    pub fn width(&self) -> u16 {
        self.w
    }

    pub fn height(&self) -> u16 {
        self.h
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn score(&self) -> u32 {
        self.score
    }

    /// The level, from 0 ([`LEVELS`]' index).
    pub fn level(&self) -> usize {
        self.level
    }

    pub fn steps(&self) -> u64 {
        self.steps
    }

    pub fn head(&self) -> Cell {
        self.body[0]
    }

    /// The comet's cells, its head first.
    pub fn comet(&self) -> impl Iterator<Item = Cell> + '_ {
        self.body.iter().copied()
    }

    pub fn len(&self) -> usize {
        self.body.len()
    }

    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }

    pub fn dust(&self) -> Option<Cell> {
        self.dust
    }

    /// The heading the next step takes.
    pub fn heading(&self) -> Dir {
        self.heading
    }

    /// What cell `c` holds (`Empty` off the sky).
    pub fn content(&self, c: Cell) -> Content {
        self.index(c).map_or(Content::Empty, |i| self.grid[i])
    }

    /// Head `d` from the next step on: `false` (nothing changed) once the
    /// flight is over, or for a turn straight back over the last move,
    /// into its own tail.
    pub fn turn(&mut self, d: Dir) -> bool {
        if self.state != State::Flying || d == self.moved.opposite() {
            return false;
        }
        self.heading = d;
        true
    }

    /// One step: the comet moves a cell in its heading, eating a grain
    /// there (its score, its growth, maybe a new level, the next grain), or
    /// the flight ends against the edge, its tail or an asteroid (the comet
    /// left where it was). The cells it changed, each once, in order; none
    /// once the flight is over.
    pub fn step(&mut self) -> Vec<Cell> {
        let mut changed = Vec::new();
        if self.state != State::Flying {
            return changed;
        }
        let head = self.head();
        let Some(to) = self.next_cell(head, self.heading) else {
            self.state = State::Over(Crash::Edge);
            return changed;
        };
        // The tail's end moves away this step unless the comet is growing,
        // so its cell is free to move into.
        let vacates = self.grow == 0;
        let end = *self.body.back().expect("a comet has cells");
        match self.content(to) {
            Content::Rock => {
                self.state = State::Over(Crash::Rock);
                return changed;
            }
            Content::Tail | Content::Head if !(vacates && to == end) => {
                self.state = State::Over(Crash::Tail);
                return changed;
            }
            _ => {}
        }
        let eats = self.content(to) == Content::Dust;
        self.moved = self.heading;
        self.steps += 1;
        if vacates {
            self.body.pop_back();
            self.set(end, Content::Empty);
            changed.push(end);
        } else {
            self.grow -= 1;
        }
        self.set(head, Content::Tail);
        changed.push(head);
        self.body.push_front(to);
        self.set(to, Content::Head);
        changed.push(to);
        if eats {
            self.dust = None;
            self.score += 1;
            self.grow += GROWTH;
            let level = ((self.score / LEVEL_EVERY) as usize).min(LEVELS.len() - 1);
            if level != self.level {
                self.change_level(level, &mut changed);
            }
            self.place_dust(&mut changed);
        }
        dedup_in_order(&mut changed);
        changed
    }

    /// The old level's asteroids gone, the new one's placed on the empty
    /// cells of its field; a cell that is a rock in both is left alone (and
    /// not reported: it didn't change).
    fn change_level(&mut self, level: usize, changed: &mut Vec<Cell>) {
        let old = rocks(self.level, self.w, self.h);
        let new = rocks(level, self.w, self.h);
        for &c in old.iter().filter(|c| !new.contains(c)) {
            if self.content(c) == Content::Rock {
                self.set(c, Content::Empty);
                changed.push(c);
            }
        }
        self.level = level;
        for &c in new.iter().filter(|c| !old.contains(c)) {
            if self.content(c) == Content::Empty {
                self.set(c, Content::Rock);
                changed.push(c);
            }
        }
    }

    /// The next grain on an empty cell, each equally likely; with none
    /// left, the sky is full.
    fn place_dust(&mut self, changed: &mut Vec<Cell>) {
        let free = self.grid.iter().filter(|&&c| c == Content::Empty).count();
        if free == 0 {
            self.state = State::Full;
            return;
        }
        let pick = self.rng.below(free as u32) as usize;
        let i = self
            .grid
            .iter()
            .enumerate()
            .filter(|&(_, &c)| c == Content::Empty)
            .nth(pick)
            .map(|(i, _)| i)
            .expect("pick is below the empty cells' count");
        let c = Cell::new((i % usize::from(self.w)) as u16, (i / usize::from(self.w)) as u16);
        self.set(c, Content::Dust);
        self.dust = Some(c);
        changed.push(c);
    }

    fn next_cell(&self, c: Cell, d: Dir) -> Option<Cell> {
        let (x, y) = match d {
            Dir::Up => (Some(c.x), c.y.checked_sub(1)),
            Dir::Down => (Some(c.x), c.y.checked_add(1).filter(|&y| y < self.h)),
            Dir::Left => (c.x.checked_sub(1), Some(c.y)),
            Dir::Right => (c.x.checked_add(1).filter(|&x| x < self.w), Some(c.y)),
        };
        Some(Cell::new(x?, y?))
    }

    fn index(&self, c: Cell) -> Option<usize> {
        (c.x < self.w && c.y < self.h).then(|| usize::from(c.y) * usize::from(self.w) + usize::from(c.x))
    }

    fn set(&mut self, c: Cell, to: Content) {
        let i = self.index(c).expect("a cell on the sky");
        self.grid[i] = to;
    }
}

/// `cells` with each cell kept at its first place only.
fn dedup_in_order(cells: &mut Vec<Cell>) {
    let mut seen: Vec<Cell> = Vec::with_capacity(cells.len());
    cells.retain(|c| {
        if seen.contains(c) {
            false
        } else {
            seen.push(*c);
            true
        }
    });
}

/// Level `level`'s asteroid field on a `w` × `h` sky (our own layouts,
/// scaled to the sky; the cells may hold the comet or a grain when placed,
/// which then keep them):
/// - 0, "Open sky": none;
/// - 1, "Twin belts": two rows a third of the way down and up, across the
///   middle half;
/// - 2, "Crossing": a cross through the middle, its centre open;
/// - 3, "Ring of rocks": a ring two cells in from the edge, every third
///   cell open.
pub fn rocks(level: usize, w: u16, h: u16) -> Vec<Cell> {
    let mut out = Vec::new();
    match level {
        1 => {
            for y in [h / 3, h - 1 - h / 3] {
                for x in w / 4..w - w / 4 {
                    out.push(Cell::new(x, y));
                }
            }
        }
        2 => {
            let (cx, cy) = (w / 2, h / 2);
            for x in w / 4..w - w / 4 {
                if x.abs_diff(cx) > 1 {
                    out.push(Cell::new(x, cy));
                }
            }
            for y in h / 4..h - h / 4 {
                if y.abs_diff(cy) > 1 {
                    out.push(Cell::new(cx, y));
                }
            }
        }
        3 => {
            let (x0, y0, x1, y1) = (2, 2, w - 3, h - 3);
            let mut ring = Vec::new();
            for x in x0..=x1 {
                ring.push(Cell::new(x, y0));
            }
            for y in y0 + 1..=y1 {
                ring.push(Cell::new(x1, y));
            }
            for x in (x0..x1).rev() {
                ring.push(Cell::new(x, y1));
            }
            for y in (y0 + 1..y1).rev() {
                ring.push(Cell::new(x0, y));
            }
            out.extend(ring.into_iter().enumerate().filter(|(i, _)| i % 3 != 2).map(|(_, c)| c));
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests;
