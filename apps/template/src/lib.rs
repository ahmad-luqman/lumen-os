//! A Lumen app to start from (`make app NAME=foo` copies it to apps/foo):
//! it opens a window titled with its crate's name, draws its name and the
//! keys typed so far in the desktop theme's colours, redraws on each key
//! and on each theme switch, and returns 0 when its window is closed.

#![no_std]

extern crate alloc;

use alloc::format;
use lumen_app::{Desktop, Event, KeyAction, Palette, Rect, Window, log};

lumen_app::manifest!(desktop);
lumen_app::main!(run);

const NAME: &str = env!("CARGO_PKG_NAME");
const WIDTH: u32 = 360;
const HEIGHT: u32 = 160;
/// Pixels from the edges to the text.
const PAD: i32 = 16;
const TEXT_PX: u32 = 16;
const HEADING_PX: u32 = 24;

/// What the window shows.
struct State {
    palette: Palette,
    keys: u32,
    last: Option<char>,
}

fn run() -> i32 {
    let desktop = match Desktop::get() {
        Ok(d) => d,
        Err(e) => {
            log::error(&format!("no desktop handle: {e}"));
            return 1;
        }
    };
    let mut window = match desktop.open_window(NAME, WIDTH, HEIGHT) {
        Ok(w) => w,
        Err(e) => {
            log::error(&format!("no window: {e}"));
            return 1;
        }
    };
    let mut state = State { palette: Palette::default(), keys: 0, last: None };
    loop {
        let event = match window.wait_event() {
            Ok(e) => e,
            Err(e) => {
                log::error(&format!("events: {e}"));
                return 1;
            }
        };
        match event {
            // The first event, and again after each switch.
            Event::Theme { palette, .. } => state.palette = palette,
            Event::Key { action: KeyAction::Press { text }, .. } => {
                state.keys += 1;
                state.last = text.filter(|c| !c.is_control());
            }
            // Closing the window ends the app.
            Event::Close => return 0,
            _ => continue,
        }
        if let Err(e) = draw(&desktop, &mut window, &state) {
            log::error(&format!("draw: {e}"));
            return 1;
        }
    }
}

fn draw(desktop: &Desktop, w: &mut Window, s: &State) -> lumen_app::Result<()> {
    let p = s.palette;
    let all = w.bounds();
    w.fill(all, p.background);
    let heading = desktop.line_height(HEADING_PX)? as i32;
    w.fill(Rect::new(0, 0, WIDTH, (PAD + heading + PAD / 2) as u32), p.accent);
    w.text(PAD, PAD / 2, HEADING_PX, p.accent_text, NAME)?;
    let y = PAD + heading + PAD;
    let line = desktop.line_height(TEXT_PX)? as i32;
    w.text(PAD, y, TEXT_PX, p.text, "Type a key; close the window to end.")?;
    let keys = match s.last {
        Some(c) => format!("{} keys typed, the last {c:?}", s.keys),
        None => format!("{} keys typed", s.keys),
    };
    w.text(PAD, y + line + PAD / 2, TEXT_PX, p.text, &keys)?;
    w.commit_all()
}
