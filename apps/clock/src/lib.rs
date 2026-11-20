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
use alloc::vec::Vec;
use lumen_app::gfx::Rect;
use lumen_app::ui::{Flow, Taken, Ticking, Toolkit};
use lumen_app::{Desktop, Time, log};
use lumen_clock::TimeOfDay;
use lumen_clock::args::{self, Mode};
use lumen_clock::face::{BLANK, Face, HEIGHT, TREE, WIDTH};
use lumen_clock::rtc::{Note, Rtc};
use lumen_ui::{App, Over, Then, Ui, UiEvent};

lumen_app::manifest!(time, desktop);
lumen_app::main!(run);

/// How often it reads the RTC (a new second shows about this late on an
/// idle host).
const TICK_NS: u64 = 250_000_000;

/// The app: its window ([`Face`]) and where its time comes from.
struct Clock {
    face: Face,
    /// The RTC and its readings so far, or `None` when frozen (`--at`).
    rtc: Option<(Time, Rtc)>,
}

impl Clock {
    /// Read the RTC (live only) and show what `Rtc` says to, logging what
    /// it asks: the first failure of a run, a recovery.
    fn read(&mut self) {
        let Some((time, rtc)) = &mut self.rtc else { return };
        let (shown, note) = rtc.read(time.wall());
        match note {
            Note::Nothing => {}
            Note::Failing { why, kept: true } => {
                log::warn(&format!("the clock can't read the RTC ({why}); it keeps its last reading for now"))
            }
            Note::Failing { why, kept: false } => {
                log::warn(&format!("the clock can't read the RTC ({why}); it shows {BLANK}"))
            }
            Note::Recovered => log::info("the clock reads the RTC again"),
        }
        self.face.show(shown);
    }
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
        self.rtc.as_ref().map(|_| TICK_NS)
    }

    /// A tick that finds the same second draws nothing, and the next one is
    /// a quarter of a second away: not an animation (the desktop settles
    /// between ticks).
    fn animating(&self) -> bool {
        false
    }

    fn tick(&mut self, _now: u64) -> Flow {
        self.read();
        Flow::Carry
    }
}

/// The app's exit code: 0 once its window is closed; 1 once a failure is
/// logged; 2 for arguments it doesn't take (an ERROR naming what was
/// wrong).
fn run() -> i32 {
    let words = match lumen_app::args() {
        Ok(w) => w,
        Err(e) => {
            log::error(&format!("its arguments: {e}"));
            return 2;
        }
    };
    match args::mode(&words) {
        Ok(Mode::Live) => lumen_app::report(app(None)),
        Ok(Mode::Frozen(t)) => lumen_app::report(app(Some(t))),
        Err(e) => {
            log::error(&format!("{e}"));
            2
        }
    }
}

fn app(at: Option<TimeOfDay>) -> Result<(), (&'static str, lumen_app::Error)> {
    let time = Time::get().map_err(|e| ("no time handle", e))?;
    // Live, its first reading now (one that fails shows no time, as a
    // later one would, rather than ending the app).
    let mut clock = Clock { face: Face::new(at), rtc: at.is_none().then(|| (time, Rtc::new())) };
    clock.read();
    let desktop = Desktop::get().map_err(|e| ("no desktop handle", e))?;
    let window = desktop.open_window("Clock", WIDTH, HEIGHT).map_err(|e| ("no window", e))?;
    let mut tk = Toolkit::with_area(clock, window, TREE).map_err(|e| ("no toolkit", e))?;
    tk.run_until(time, |_, _| Taken::Pass).map(|_| ()).map_err(|e| ("toolkit", e))
}
