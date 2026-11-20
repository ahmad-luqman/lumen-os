//! Conway's Game of Life (#202): a grid of cells stepping on the SDK's
//! tick loop, the launcher's Life (it replaces #157's kernel window; the
//! text-mode `life` command stays). Its rules and patterns are
//! `lumen_demo::life`'s, host-tested, the `life` command's too.
//!
//! - **The window:** a tree on the left (through `Toolkit::with_area`): the
//!   `Patterns` list, `Start`, `Stop` and `Step`, and three fixed-width
//!   labels, `Pattern` (its name), `Generation` and `State` (`running` or
//!   `stopped`). The grid is the rest, the app's own, drawn in
//!   `paint_over`: a line in the palette's border colour between the two,
//!   live cells in its accent, so it follows the theme.
//! - **Running**, it steps a generation every 100 ms (`Ticking::period`; its
//!   waits are an animation's, focused or not), committing only the box
//!   around the cells that changed (`take_drawn`), as #157's window did.
//!   **Stopped**, it has no period: the window settles, and each `Step`
//!   advances one generation.
//! - **A pattern** chosen in the list starts it again from that pattern at
//!   generation 0, running or stopped as it was.
//! - **`run life [--stopped] [pattern]`:** the pattern to start with (`gun`
//!   without one) and whether to start stopped (the launcher starts it
//!   running with `gun`). Anything else is logged as an ERROR naming it,
//!   with the usage, and the app exits with 2.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ops::Range;
use lumen_app::gfx::Rect as Area;
use lumen_app::ui::{Flow, Taken, Ticking, Toolkit};
use lumen_app::{Desktop, Error, Time, log};
use lumen_demo::life::{self, Life, Pattern};
use lumen_ui::{App, Over, Then, Ui, UiEvent, Widget};

lumen_app::manifest!(time, desktop);
lumen_app::main!(run);

/// The window, the tree's part of it (left), the line between and the
/// grid's (the rest, a pixel in from the line).
const WIDTH: u32 = 600;
const HEIGHT: u32 = 320;
const TREE_W: u32 = 200;
const TREE: Area = Area::new(0, 0, TREE_W, HEIGHT);
const LINE: Area = Area::new(TREE_W as i32, 0, 1, HEIGHT);
const GRID: Area = Area::new(TREE_W as i32 + 2, 0, WIDTH - TREE_W - 2, HEIGHT);
/// A cell's pixels (a pixel's gap between cells).
const CELL: u32 = 8;
const COLS: usize = (GRID.w() / CELL) as usize;
const ROWS: usize = (GRID.h() / CELL) as usize;
/// A generation every 100 ms while running, as #157's window stepped.
const PERIOD_NS: u64 = 100_000_000;
/// The labels' fixed width: a label changing width would move the widgets
/// after it, repainting (and committing) the whole window.
const LABEL_W: u32 = 140;
const USAGE: &str = "usage: life [--stopped] [pattern]";

/// The app: see the crate's doc.
struct LifeApp {
    ui: Ui,
    patterns: Widget,
    start: Widget,
    stop: Widget,
    step: Widget,
    pattern_label: Widget,
    generation: Widget,
    state: Widget,
    grid: Life,
    running: bool,
    /// The grid's rectangles whose look changed since the last repaint.
    drawn: Vec<Area>,
}

impl LifeApp {
    fn new(pattern: &'static Pattern, running: bool) -> LifeApp {
        let mut ui = Ui::new();
        let patterns = ui.list(None, "Patterns", &life::NAMES);
        let controls = ui.hstack(None, "Controls");
        let start = ui.button(Some(controls), "Start");
        let stop = ui.button(Some(controls), "Stop");
        let step = ui.button(Some(controls), "Step");
        let pattern_label = ui.fixed_label(None, "Pattern", pattern.name, LABEL_W);
        let generation = ui.fixed_label(None, "Generation", "0", LABEL_W);
        let state = ui.fixed_label(None, "State", state_text(running), LABEL_W);
        let mut app = LifeApp {
            ui,
            patterns,
            start,
            stop,
            step,
            pattern_label,
            generation,
            state,
            grid: Life::new(COLS, ROWS),
            running,
            drawn: Vec::new(),
        };
        app.reset(pattern);
        app
    }

    /// The grid from `pattern` at generation 0, all of it to draw again.
    fn reset(&mut self, pattern: &'static Pattern) {
        self.grid = Life::new(COLS, ROWS);
        let (x, y) = pattern.origin(COLS, ROWS);
        self.grid.place(pattern, x, y);
        self.ui.set_text(self.pattern_label, pattern.name);
        self.ui.set_text(self.generation, "0");
        self.drawn.push(GRID);
    }

    /// One generation on: the box around the cells that changed is the
    /// app's damage (none if nothing did).
    fn advance(&mut self) {
        let mut changed: Option<Area> = None;
        self.grid.step(|x, y, _| {
            let r = cell(x, y);
            changed = Some(changed.map_or(r, |c| c.union(&r)));
        });
        if let Some(c) = changed {
            self.drawn.push(c);
        }
        let generation = format!("{}", self.grid.generation());
        self.ui.set_text(self.generation, &generation);
    }

    fn set_running(&mut self, running: bool) {
        self.running = running;
        self.ui.set_text(self.state, state_text(running));
    }
}

fn state_text(running: bool) -> &'static str {
    if running { "running" } else { "stopped" }
}

/// Cell (`x`, `y`)'s pixels: `CELL` less a pixel's gap, in the grid.
fn cell(x: usize, y: usize) -> Area {
    Area::new(GRID.x() + (x as u32 * CELL) as i32, GRID.y() + (y as u32 * CELL) as i32, CELL - 1, CELL - 1)
}

/// The cells whose pixels `part` (inside the grid) may reach: their columns
/// and their rows.
fn cells_under(part: Area) -> (Range<usize>, Range<usize>) {
    let first = |a: i32, edge: i32| ((a - edge) as u32 / CELL) as usize;
    let past = |a: i32, edge: i32| ((a - edge) as u32).div_ceil(CELL) as usize;
    (
        first(part.x(), GRID.x())..past(part.right(), GRID.x()).min(COLS),
        first(part.y(), GRID.y())..past(part.bottom(), GRID.y()).min(ROWS),
    )
}

impl App for LifeApp {
    fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn apply(&mut self, events: &[UiEvent]) -> Then {
        for e in events {
            match *e {
                UiEvent::Pressed(w) if w == self.start => self.set_running(true),
                UiEvent::Pressed(w) if w == self.stop => self.set_running(false),
                UiEvent::Pressed(w) if w == self.step => self.advance(),
                UiEvent::Selected(w, i) if w == self.patterns => {
                    if let Some(p) = life::PATTERNS.get(i) {
                        self.reset(p);
                    }
                }
                _ => {}
            }
        }
        Then::Carry
    }

    fn paint_over(&mut self, over: &mut Over<'_>) {
        // Each rect has the tree's background already: only the line and
        // the live cells are drawn.
        for r in over.rects {
            if let Some(line) = r.intersect(&LINE) {
                over.surface.fill_rect(line, over.palette.border);
            }
            let Some(part) = r.intersect(&GRID) else { continue };
            let (cols, rows) = cells_under(part);
            for y in rows {
                for x in cols.clone() {
                    if self.grid.get(x, y)
                        && let Some(c) = cell(x, y).intersect(&part)
                    {
                        over.surface.fill_rect(c, over.palette.accent);
                    }
                }
            }
        }
    }

    fn take_drawn(&mut self) -> Vec<Area> {
        core::mem::take(&mut self.drawn)
    }
}

impl Ticking for LifeApp {
    fn period(&self) -> Option<u64> {
        self.running.then_some(PERIOD_NS)
    }

    fn tick(&mut self, _now: u64) -> Flow {
        self.advance();
        Flow::Carry
    }
}

/// `run`'s arguments: the pattern and whether to start running, or why
/// not.
fn parse(args: &[String]) -> Result<(&'static Pattern, bool), String> {
    let mut running = true;
    let mut name = None;
    for a in args {
        match a.as_str() {
            "--stopped" if running => running = false,
            n if name.is_none() && !n.starts_with('-') => {
                name = Some(life::pattern(n).ok_or_else(|| format!("no pattern '{n}' ({})", life::NAMES.join(", ")))?);
            }
            other => return Err(format!("'{other}' isn't an argument it takes")),
        }
    }
    let pattern = name.unwrap_or_else(|| life::pattern(life::DEFAULT_PATTERN).expect("the default pattern exists"));
    Ok((pattern, running))
}

/// The app's exit code: 0 once its window is closed; 2 for arguments it
/// doesn't take; 1 once a failure is logged.
fn run() -> i32 {
    let args = match lumen_app::args() {
        Ok(a) => a,
        Err(e) => {
            log::error(&format!("arguments: {e} ({USAGE})"));
            return 2;
        }
    };
    let (pattern, running) = match parse(&args) {
        Ok(p) => p,
        Err(why) => {
            log::error(&format!("{why} ({USAGE})"));
            return 2;
        }
    };
    lumen_app::report(app(pattern, running))
}

fn app(pattern: &'static Pattern, running: bool) -> Result<(), (&'static str, Error)> {
    let time = Time::get().map_err(|e| ("no time handle", e))?;
    let desktop = Desktop::get().map_err(|e| ("no desktop handle", e))?;
    let window = desktop.open_window("Life", WIDTH, HEIGHT).map_err(|e| ("no window", e))?;
    let mut tk = Toolkit::with_area(LifeApp::new(pattern, running), window, TREE).map_err(|e| ("no toolkit", e))?;
    tk.run_until(time, |_, _| Taken::Pass).map(|_| ()).map_err(|e| ("toolkit", e))
}
