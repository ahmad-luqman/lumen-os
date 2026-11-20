//! The clock (#200): an analog face over the time as a label, the time
//! from the RTC (`time_wall`, taken as UTC, as `date` shows it), on the
//! SDK's tick loop (`ui::Toolkit::run_until`).
//!
//! - **The face** is the app's own drawing (`paint_over`): sixty dots, the
//!   hours bigger, and the hour, minute and second hands, in the theme's
//!   colours. When the time changes, only the boxes of the hands that moved
//!   (where they were and where they are) are repainted and committed
//!   (`take_drawn`).
//! - **The digital time** is a label in its tree, `Time`, whose value is
//!   the time (`HH:MM:SS`): `ui tree` shows it.
//! - **Ticks:** every 250 ms it reads the RTC; a new second changes the label
//!   and the hands. Its waits aren't animating (a tick that finds the same
//!   second draws nothing), so the desktop settles between ticks.
//! - **`run clock --at HH:MM[:SS]`** freezes it at that time: no ticks at
//!   all, so the window settles for good (its golden).
//!
//! The time of day, `--at`'s parsing and the face's geometry (an integer
//! sine table: apps have no `libm`) are `lumen_clock`'s, host-tested.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use lumen_app::gfx::Rect;
use lumen_app::ui::{Flow, Taken, Ticking, Toolkit};
use lumen_app::{Desktop, Time, log};
use lumen_clock::{TimeOfDay, line, point};
use lumen_ui::{App, Over, Palette, Then, Ui, UiEvent, Widget};

lumen_app::manifest!(time, desktop);
lumen_app::main!(run);

/// The window: the face (a square) over the tree (the label).
const WIDTH: u32 = 240;
const FACE_H: u32 = 240;
const TREE_H: u32 = 36;
const TREE: Rect = Rect::new(0, FACE_H as i32, WIDTH, TREE_H);
/// The face's centre, and the radius its dots are at.
const CX: i32 = (WIDTH / 2) as i32;
const CY: i32 = (FACE_H / 2) as i32;
const DOTS_R: i32 = 104;
/// A minute's dot and an hour's, each a square this wide.
const MINUTE_DOT: u32 = 2;
const HOUR_DOT: u32 = 6;
/// The hub over the hands' common end.
const HUB: u32 = 8;
/// The `Time` label's fixed width (`HH:MM:SS` at the toolkit's text size,
/// with room to spare): a label that changed width would move the widgets
/// after it and repaint the whole window.
const TIME_W: u32 = 96;
/// How often it reads the RTC (a second shows at most this late).
const TICK_NS: u64 = 250_000_000;

/// A hand: its length, its width (a square brush) and its colour.
#[derive(Clone, Copy)]
struct Hand {
    len: i32,
    brush: u32,
    colour: fn(&Palette) -> u32,
}

const HOUR: Hand = Hand { len: 56, brush: 6, colour: |p| p.text };
const MINUTE: Hand = Hand { len: 84, brush: 4, colour: |p| p.text };
const SECOND: Hand = Hand { len: 94, brush: 2, colour: |p| p.accent };

/// A `w`-wide square centred on (`x`, `y`).
fn square(x: i32, y: i32, w: u32) -> Rect {
    let half = (w / 2) as i32;
    Rect::new(x - half, y - half, w, w)
}

impl Hand {
    /// Its far end at angle `step`.
    fn end(&self, step: u32) -> (i32, i32) {
        point(CX, CY, self.len, step)
    }

    /// The box it covers at angle `step` (the hub's too: every hand starts
    /// under it).
    fn bounds(&self, step: u32) -> Rect {
        let (x, y) = self.end(step);
        let pad = self.brush.max(HUB) as i32 / 2 + 1;
        let (x0, x1) = (x.min(CX) - pad, x.max(CX) + pad);
        let (y0, y1) = (y.min(CY) - pad, y.max(CY) + pad);
        Rect::new(x0, y0, (x1 - x0) as u32, (y1 - y0) as u32)
    }

    /// Draw it at angle `step`, only inside `clip`.
    fn draw(&self, over: &mut Over<'_>, step: u32, clip: Rect) {
        if self.bounds(step).intersect(&clip).is_none() {
            return;
        }
        let colour = (self.colour)(over.palette);
        for (x, y) in line((CX, CY), self.end(step)) {
            if let Some(part) = square(x, y, self.brush).intersect(&clip) {
                over.surface.fill_rect(part, colour);
            }
        }
    }
}

/// The app: its tree (the `Time` label), the time shown, and where it
/// comes from.
struct Clock {
    ui: Ui,
    label: Widget,
    shown: TimeOfDay,
    /// The RTC, or `None` when frozen (`--at`).
    time: Option<Time>,
    /// The hands' boxes to repaint (where they were and are now).
    drawn: Vec<Rect>,
    /// Whether a failed RTC read was logged (once: the clock keeps its last
    /// reading meanwhile, as the taskbar's does).
    warned: bool,
}

impl Clock {
    fn new(shown: TimeOfDay, time: Option<Time>) -> Clock {
        let mut ui = Ui::new();
        let label = ui.fixed_label(None, "Time", &format!("{shown}"), TIME_W);
        Clock { ui, label, shown, time, drawn: Vec::new(), warned: false }
    }

    /// Show `t`: the label, and the boxes of the hands that moved.
    fn show(&mut self, t: TimeOfDay) {
        if t == self.shown {
            return;
        }
        let (was, now) = (self.shown.hands(), t.hands());
        for (hand, a, b) in [(HOUR, was.hour, now.hour), (MINUTE, was.minute, now.minute), (SECOND, was.second, now.second)]
        {
            if a != b {
                self.drawn.push(hand.bounds(a));
                self.drawn.push(hand.bounds(b));
            }
        }
        self.shown = t;
        self.ui.set_text(self.label, &format!("{t}"));
    }
}

impl App for Clock {
    fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn apply(&mut self, _: &[UiEvent]) -> Then {
        Then::Carry
    }

    /// The face's foreground (each rect has the tree's background already),
    /// only where the rects being repainted reach.
    fn paint_over(&mut self, over: &mut Over<'_>) {
        let hands = self.shown.hands();
        for &clip in over.rects {
            for i in 0..60u32 {
                let (x, y) = point(CX, CY, DOTS_R, i * 12);
                let (w, colour) = if i % 5 == 0 { (HOUR_DOT, over.palette.text) } else { (MINUTE_DOT, over.palette.border) };
                if let Some(part) = square(x, y, w).intersect(&clip) {
                    over.surface.fill_rect(part, colour);
                }
            }
            HOUR.draw(over, hands.hour, clip);
            MINUTE.draw(over, hands.minute, clip);
            SECOND.draw(over, hands.second, clip);
            if let Some(part) = square(CX, CY, HUB).intersect(&clip) {
                over.surface.fill_rect(part, over.palette.accent);
            }
        }
    }

    fn take_drawn(&mut self) -> Vec<Rect> {
        core::mem::take(&mut self.drawn)
    }
}

impl Ticking for Clock {
    /// Frozen, none: the window settles for good.
    fn period(&self) -> Option<u64> {
        self.time.map(|_| TICK_NS)
    }

    /// A tick that finds the same second draws nothing, and the next one is
    /// a quarter of a second away: not an animation (the desktop settles
    /// between ticks).
    fn animating(&self) -> bool {
        false
    }

    fn tick(&mut self, _now: u64) -> Flow {
        if let Some(time) = self.time {
            match time.wall() {
                Ok(secs) => self.show(TimeOfDay::from_unix(secs)),
                Err(e) if !self.warned => {
                    self.warned = true;
                    log::warn(&format!("clock: the RTC: {e} (showing the last time read)"));
                }
                Err(_) => {}
            }
        }
        Flow::Carry
    }
}

const USAGE: &str = "usage: clock [--at HH:MM[:SS]]";

/// What `run`'s arguments ask for: `Some(t)` frozen at `t`, `None` live.
fn frozen_at(args: &[String]) -> Result<Option<TimeOfDay>, String> {
    match args {
        [] => Ok(None),
        [at, t] if at == "--at" => {
            TimeOfDay::parse(t).map(Some).ok_or_else(|| format!("clock: '{t}' is not a time of day ({USAGE})"))
        }
        _ => Err(format!("clock: {USAGE}")),
    }
}

/// The app's exit code: 0 once its window is closed; 1 once a failure is
/// logged; 2 for arguments it doesn't take.
fn run() -> i32 {
    let at = match lumen_app::args().map_err(|e| format!("clock: its arguments: {e}")).and_then(|a| frozen_at(&a)) {
        Ok(at) => at,
        Err(e) => {
            log::error(&e);
            return 2;
        }
    };
    lumen_app::report(app(at))
}

fn app(at: Option<TimeOfDay>) -> Result<(), (&'static str, lumen_app::Error)> {
    let time = Time::get().map_err(|e| ("no time handle", e))?;
    let shown = match at {
        Some(t) => t,
        None => TimeOfDay::from_unix(time.wall().map_err(|e| ("the RTC", e))?),
    };
    let desktop = Desktop::get().map_err(|e| ("no desktop handle", e))?;
    let window = desktop.open_window("Clock", WIDTH, FACE_H + TREE_H).map_err(|e| ("no window", e))?;
    let live = at.is_none().then_some(time);
    let mut tk = Toolkit::with_area(Clock::new(shown, live), window, TREE).map_err(|e| ("no toolkit", e))?;
    tk.run_until(time, |_, _| Taken::Pass).map(|_| ()).map_err(|e| ("toolkit", e))
}
