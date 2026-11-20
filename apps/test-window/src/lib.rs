//! `window` (#206): a test app built on the SDK. It opens a 200 × 100
//! window titled `window`, logs `window: opened`, and reads its events,
//! logging each kind (`window: event Theme`) and returning 0 at `Close`.
//!
//! - `wait`: logs `window: waiting`, then blocks in `window_wait` with no
//!   deadline for good, reading whatever wakes it (only `kill` ends it).
//! - `ignore-close`: given `Close`, logs `window: ignoring close` and spins
//!   for good, never closing its window: the close grace ends it after
//!   `CLOSE_GRACE_SLICES` slice ends.

#![no_std]

extern crate alloc;

use alloc::format;
use core::hint::black_box;
use lumen_app::{Desktop, Event, log};

lumen_app::manifest!(desktop);
lumen_app::main!(run);

fn kind(e: &Event) -> &'static str {
    match e {
        Event::Theme { .. } => "Theme",
        Event::Key { .. } => "Key",
        Event::PointerDown { .. } => "PointerDown",
        Event::PointerUp { .. } => "PointerUp",
        Event::PointerMove { .. } => "PointerMove",
        Event::FocusGained => "FocusGained",
        Event::FocusLost => "FocusLost",
        Event::Lost => "Lost",
        Event::Close => "Close",
        Event::Request { .. } => "Request",
    }
}

fn run() -> i32 {
    let args = lumen_app::args();
    let mode = args.first().map_or("", |a| a.as_str());
    let mut w = match Desktop::get().and_then(|d| d.open_window("window", 200, 100)) {
        Ok(w) => w,
        Err(e) => {
            log::error(&format!("window: no window: {e}"));
            return 1;
        }
    };
    w.fill(w.bounds(), 0x00_33_66_99);
    if w.commit_all().is_err() {
        return 2;
    }
    log::info("window: opened");
    if mode == "wait" {
        log::info("window: waiting");
        loop {
            // Whatever wakes it is read and ignored.
            let _ = w.wait(None, false);
            while let Ok(Some(_)) = w.next_event() {}
        }
    }
    loop {
        let e = match w.wait_event() {
            Ok(e) => e,
            Err(e) => {
                log::error(&format!("window: events: {e}"));
                return 3;
            }
        };
        log::info(&format!("window: event {}", kind(&e)));
        if e == Event::Close {
            if mode == "ignore-close" {
                log::info("window: ignoring close");
                let mut n = 0u64;
                loop {
                    n = black_box(n.wrapping_add(1));
                }
            }
            return 0;
        }
    }
}
