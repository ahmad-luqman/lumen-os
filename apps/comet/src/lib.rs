//! Comet (#201): an original snake-style game. Steer a comet across the
//! night sky with the arrow keys; each grain of stardust it reaches
//! lengthens its tail; the sky's edge, its own tail and each level's
//! asteroids end the flight. The game is `lumen_snake::Game`'s
//! (host-tested); this is its window, on the SDK's tick loop (#222):
//!
//! - **The sky** is the app's own drawing (`paint_over`) right of a column
//!   of widgets: each cell redrawn only when a step changed it (the game
//!   says which: `take_drawn`), in the theme's palette (the sky
//!   `field`, the comet `accent`, a grain `text`, an asteroid
//!   `disabled`, the frame `border`).
//! - **The widgets**, also its automation tree: `Score`, `Level`, `State`
//!   (`flying`, `stepping`, `over: edge`, `over: tail`, `over: asteroid`,
//!   `sky full`) and `Comet` (the head's cell, `at x,y`), fixed-width
//!   labels (a changing width would repaint the whole window); `Play`,
//!   `Step` and `Restart`.
//! - **Ticks:** flying, it steps every [`period`] (faster each level);
//!   `Step` switches to stepping, a step per press with no ticks at all (so
//!   the desktop settles between presses: goldens and the tree don't depend
//!   on the host's speed), and `Play` back to flying. Over, it waits for
//!   `Restart`.
//! - **Keys:** the arrows turn it (taken before the toolkit, which never
//!   sees them).
//! - **`run comet [--seed N] [--step]`:** a seed plays the same sky every
//!   time (also after `Restart`; without one, each game's seed is the clock);
//!   `--step` starts it stepping. Anything else is an ERROR naming it and
//!   the usage, then exit 2.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use lumen_app::gfx::Rect as Area;
use lumen_app::ui::{Flow, Taken, Ticking, Toolkit};
use lumen_app::{Desktop, Error, Event, KeyAction, KeyCode, Time, log};
use lumen_snake::{Cell, Content, Crash, Dir, Game, LEVELS, SKY_H, SKY_W, State};
use lumen_ui::{App, Over, Palette, Then, Ui, UiEvent, Widget};

lumen_app::manifest!(time, desktop);
lumen_app::main!(run);

/// A cell's side, in pixels.
const CELL: u32 = 16;
/// The widgets' column, then the sky (its frame a pixel outside it).
const TREE_W: u32 = 150;
const PAD: u32 = 8;
const SKY: Area = Area::new((TREE_W + PAD) as i32, PAD as i32, SKY_W as u32 * CELL, SKY_H as u32 * CELL);
const FRAMED: Area = Area::new(SKY.x() - 1, SKY.y() - 1, SKY.w() + 2, SKY.h() + 2);
const WIDTH: u32 = TREE_W + PAD + SKY.w() + PAD;
const HEIGHT: u32 = PAD + SKY.h() + PAD;
const TREE: Area = Area::new(0, 0, TREE_W, HEIGHT);
/// The labels' fixed width (the column's, less its padding).
const LABEL_W: u32 = TREE_W - 2 * PAD;
const USAGE: &str = "usage: comet [--seed N] [--step]";

/// The time between steps while flying at `level`: faster each level.
fn period(level: usize) -> u64 {
    [160, 140, 120, 100][level.min(3)] * 1_000_000
}

/// What `run` asked for.
struct Args {
    seed: Option<u64>,
    stepping: bool,
}

fn parse(words: &[String]) -> Result<Args, String> {
    let mut args = Args { seed: None, stepping: false };
    let mut it = words.iter();
    while let Some(w) = it.next() {
        match w.as_str() {
            "--step" if !args.stepping => args.stepping = true,
            "--seed" if args.seed.is_none() => {
                let n = it.next().ok_or_else(|| String::from("--seed needs a number"))?;
                args.seed = Some(n.parse().map_err(|_| format!("'{n}' is not a seed (0 to 2^64 - 1)"))?);
            }
            other => return Err(format!("'{other}' isn't an argument it takes")),
        }
    }
    Ok(args)
}

/// The app: the game, how it runs, its widgets, and the cells to redraw.
struct Comet {
    ui: Ui,
    game: Game,
    seed: Option<u64>,
    time: Time,
    stepping: bool,
    score: Widget,
    level: Widget,
    state: Widget,
    comet: Widget,
    play: Widget,
    step: Widget,
    restart: Widget,
    drawn: Vec<Area>,
}

impl Comet {
    fn new(time: Time, args: Args) -> Comet {
        let mut ui = Ui::new();
        let score = ui.fixed_label(None, "Score", "", LABEL_W);
        let level = ui.fixed_label(None, "Level", "", LABEL_W);
        let state = ui.fixed_label(None, "State", "", LABEL_W);
        let comet = ui.fixed_label(None, "Comet", "", LABEL_W);
        let play = ui.button(None, "Play");
        let step = ui.button(None, "Step");
        let restart = ui.button(None, "Restart");
        let game = new_game(time, args.seed);
        let mut c = Comet {
            ui,
            game,
            seed: args.seed,
            time,
            stepping: args.stepping,
            score,
            level,
            state,
            comet,
            play,
            step,
            restart,
            drawn: Vec::new(),
        };
        c.show();
        c
    }

    /// The labels from the game.
    fn show(&mut self) {
        let g = &self.game;
        let level = format!("Level {}: {}", g.level() + 1, LEVELS[g.level()]);
        let state = match g.state() {
            State::Flying if self.stepping => "stepping",
            State::Flying => "flying",
            State::Over(Crash::Edge) => "over: edge",
            State::Over(Crash::Tail) => "over: tail",
            State::Over(Crash::Rock) => "over: asteroid",
            State::Full => "sky full",
        };
        let head = g.head();
        let (score, at) = (format!("Score {}", g.score()), format!("at {},{}", head.x, head.y));
        self.ui.set_text(self.score, &score);
        self.ui.set_text(self.level, &level);
        self.ui.set_text(self.state, state);
        self.ui.set_text(self.comet, &at);
    }

    /// One step of the game: the cells it changed to redraw, the labels.
    fn advance(&mut self) {
        let changed = self.game.step();
        self.drawn.extend(changed.iter().map(|&c| cell_area(c)));
        self.show();
    }

    fn restart(&mut self) {
        self.game = new_game(self.time, self.seed);
        self.drawn.push(FRAMED);
        self.show();
    }

    /// An arrow turns the comet (taken: the toolkit never sees arrows).
    fn take(&mut self, e: &Event) -> Taken {
        let Event::Key { code, action: KeyAction::Press { .. }, .. } = *e else { return Taken::Pass };
        let d = match code {
            KeyCode::ArrowUp => Dir::Up,
            KeyCode::ArrowDown => Dir::Down,
            KeyCode::ArrowLeft => Dir::Left,
            KeyCode::ArrowRight => Dir::Right,
            _ => return Taken::Pass,
        };
        self.game.turn(d);
        Taken::Took
    }
}

/// A game on Comet's sky, seeded with `seed` or else the clock.
fn new_game(time: Time, seed: Option<u64>) -> Game {
    let seed = seed.unwrap_or_else(|| time.now_ns().unwrap_or(0).rotate_left(17));
    Game::new(SKY_W, SKY_H, seed).expect("Comet's sky is big enough")
}

/// Cell `c`'s pixels.
fn cell_area(c: Cell) -> Area {
    Area::new(SKY.x() + (u32::from(c.x) * CELL) as i32, SKY.y() + (u32::from(c.y) * CELL) as i32, CELL, CELL)
}

/// `r` filled with `rgb`, only where it meets `clip`.
fn fill(s: &mut lumen_app::Surface, clip: Area, r: Area, rgb: u32) {
    if let Some(part) = r.intersect(&clip) {
        s.fill_rect(part, rgb);
    }
}

/// Cell `c` holding `what`, drawn only inside `clip`: the sky, then its
/// shape (the comet's head a rounded square, its tail a small spark, a
/// grain a four-pointed star, an asteroid a lump).
fn draw_cell(s: &mut lumen_app::Surface, clip: Area, c: Cell, what: Content, p: &Palette) {
    let a = cell_area(c);
    let at = |dx: i32, dy: i32, w: u32, h: u32| Area::new(a.x() + dx, a.y() + dy, w, h);
    fill(s, clip, a, p.field);
    match what {
        Content::Empty => {}
        Content::Head => {
            fill(s, clip, at(3, 2, 10, 12), p.accent);
            fill(s, clip, at(2, 3, 12, 10), p.accent);
        }
        Content::Tail => fill(s, clip, at(5, 5, 6, 6), p.accent),
        Content::Dust => {
            fill(s, clip, at(7, 2, 2, 12), p.text);
            fill(s, clip, at(2, 7, 12, 2), p.text);
            fill(s, clip, at(6, 6, 4, 4), p.text);
        }
        Content::Rock => {
            fill(s, clip, at(2, 4, 12, 8), p.disabled);
            fill(s, clip, at(4, 2, 8, 12), p.disabled);
        }
    }
}

impl App for Comet {
    fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn apply(&mut self, events: &[UiEvent]) -> Then {
        for e in events {
            match *e {
                UiEvent::Pressed(w) if w == self.play => {
                    self.stepping = false;
                    self.show();
                }
                UiEvent::Pressed(w) if w == self.step => {
                    self.stepping = true;
                    self.advance();
                }
                UiEvent::Pressed(w) if w == self.restart => self.restart(),
                _ => {}
            }
        }
        Then::Carry
    }

    fn paint_over(&mut self, over: &mut Over<'_>) {
        // Only the sky's: each rect has the tree's background already.
        for &r in over.rects {
            let Some(clip) = r.intersect(&FRAMED) else { continue };
            // The frame, where the rect reaches it.
            let f = FRAMED;
            for edge in [
                Area::new(f.x(), f.y(), f.w(), 1),
                Area::new(f.x(), f.bottom() - 1, f.w(), 1),
                Area::new(f.x(), f.y(), 1, f.h()),
                Area::new(f.right() - 1, f.y(), 1, f.h()),
            ] {
                fill(over.surface, clip, edge, over.palette.border);
            }
            // The cells the rect reaches.
            let Some(inner) = clip.intersect(&SKY) else { continue };
            let col = |px: i32| ((px - SKY.x()) as u32 / CELL) as u16;
            let row = |py: i32| ((py - SKY.y()) as u32 / CELL) as u16;
            for y in row(inner.y())..=row(inner.bottom() - 1) {
                for x in col(inner.x())..=col(inner.right() - 1) {
                    let c = Cell::new(x, y);
                    draw_cell(over.surface, inner, c, self.game.content(c), over.palette);
                }
            }
        }
    }

    fn take_drawn(&mut self) -> Vec<Area> {
        core::mem::take(&mut self.drawn)
    }
}

impl Ticking for Comet {
    fn period(&self) -> Option<u64> {
        (!self.stepping && self.game.state() == State::Flying).then(|| period(self.game.level()))
    }

    fn tick(&mut self, _now: u64) -> Flow {
        self.advance();
        Flow::Carry
    }
}

/// The app's exit code: 0 once its window is closed; 2 for arguments it
/// doesn't take; 1 once another failure is logged.
fn run() -> i32 {
    let args = match lumen_app::args().map_err(|e| format!("{e}")).and_then(|w| parse(&w)) {
        Ok(a) => a,
        Err(why) => {
            log::error(&format!("{why}; {USAGE}"));
            return 2;
        }
    };
    lumen_app::report(app(args))
}

fn app(args: Args) -> Result<(), (&'static str, Error)> {
    let time = Time::get().map_err(|e| ("no time handle", e))?;
    let desktop = Desktop::get().map_err(|e| ("no desktop handle", e))?;
    let window = desktop.open_window("Comet", WIDTH, HEIGHT).map_err(|e| ("no window", e))?;
    let mut tk = Toolkit::with_area(Comet::new(time, args), window, TREE).map_err(|e| ("no toolkit", e))?;
    tk.run_until(time, Comet::take).map(|_| ()).map_err(|e| ("toolkit", e))
}
