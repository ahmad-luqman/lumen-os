//! The clock (#200): an analog face above the time as a label, the time
//! from the RTC (`time_wall`, taken as UTC, as `date` shows it), on the
//! SDK's tick loop (`ui::Toolkit::run_until`).
//!
//! - **The window** is `lumen_clock::face::Face`, host-tested: the face
//!   (the app's own drawing: sixty dots, three hands and a hub, in the
//!   theme's colours; when the time changes only the boxes of the hands
//!   that moved are repainted) above a tree holding one label, `Time`,
//!   whose value is the time (`HH:MM:SS`): `ui tree` shows it.
//! - **Ticks:** every 250 ms it reads the RTC; a new second changes the label
//!   and the hands. Its waits aren't animating (a tick that finds the same
//!   second draws nothing), so the desktop settles between ticks.
//! - **`run clock --at H[H]:MM[:SS]`** freezes it at that time: no ticks at
//!   all, so the window settles for good (its golden).
//!
//! This crate only wires `lumen_clock`'s pieces to the host: its window,
//! the RTC and the tick loop.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use lumen_app::gfx::Rect;
use lumen_app::ui::{Flow, Taken, Ticking, Toolkit};
use lumen_app::{Desktop, Time, log};
use lumen_clock::TimeOfDay;
use lumen_clock::face::{Face, HEIGHT, TREE, WIDTH};
use lumen_ui::{App, Over, Then, Ui, UiEvent};

lumen_app::manifest!(time, desktop);
lumen_app::main!(run);

/// How often it reads the RTC (a second shows at most this late).
const TICK_NS: u64 = 250_000_000;

/// The app: its window ([`Face`]) and where its time comes from.
struct Clock {
    face: Face,
    /// The RTC, or `None` when frozen (`--at`).
    time: Option<Time>,
    /// Whether a failed RTC read was logged (once: the clock keeps its last
    /// reading meanwhile).
    warned: bool,
}

impl App for Clock {
    fn ui_mut(&mut self) -> &mut Ui {
        self.face.ui_mut()
    }

    fn apply(&mut self, events: &[UiEvent]) -> Then {
        self.face.apply(events)
    }

    fn paint_over(&mut self, over: &mut Over<'_>) {
        self.face.paint_over(over);
    }

    fn take_drawn(&mut self) -> Vec<Rect> {
        self.face.take_drawn()
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
                Ok(secs) => self.face.show(Some(TimeOfDay::from_unix(secs))),
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
    let window = desktop.open_window("Clock", WIDTH, HEIGHT).map_err(|e| ("no window", e))?;
    let clock = Clock { face: Face::new(Some(shown)), time: at.is_none().then_some(time), warned: false };
    let mut tk = Toolkit::with_area(clock, window, TREE).map_err(|e| ("no toolkit", e))?;
    tk.run_until(time, |_, _| Taken::Pass).map(|_| ()).map_err(|e| ("toolkit", e))
}
