//! `window` (#206): a test app built on the SDK (`lumen-app`), so the
//! scenarios exercise the SDK as an app uses it. With no argument it opens
//! a 200 × 100 window titled `window`, logs `window: opened`, and reads its
//! events, logging each kind (`window: event Theme`) and returning 0 at
//! `Close`.
//!
//! - `wait`: logs `window: waiting`, then blocks in `window_wait` with no
//!   deadline for good, reading whatever wakes it (only `kill` ends it).
//! - `ignore-close`: given `Close`, logs `window: ignoring close` and spins
//!   for good, never closing its window: the close grace ends it after
//!   `CLOSE_GRACE_SLICES` slice ends.
//! - `close`: its window titled `a<ESC>b<LF>c` (shown sanitised); given
//!   `Close`, it drops the window (`window: closed it`), then sleeps 50
//!   times 100 ms, far past the close grace's slice ends, and returns 0.
//! - `rects`: fills its canvas red and commits 20 rectangles covering only
//!   its right half (`x` from 100: more than `COMMIT_RECTS`, so the SDK
//!   commits in turns), logs `window: rects: committed 20`, then reads
//!   events until `Close`.
//! - `log`: logs one 1,100-byte line (`log: ` then `L`s): the SDK cuts it
//!   to `TEXT_BYTES`, so the kernel logs its start rather than refusing it.
//! - `panic`: panics with a 300-byte message: the SDK's panic handler cuts
//!   it to fit ` at <file>:<line>`.
//! - `alloc`: allocates and frees 256 KiB 64 times (16 MiB in all, in a
//!   2 MiB memory): only a heap that frees lasts; logs `window: alloc: 64
//!   rounds` and returns 0.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::hint::black_box;
use lumen_app::{Desktop, Event, Rect, Time, log};

lumen_app::manifest!(time, desktop);
lumen_app::main!(run);

const W: u32 = 200;
const H: u32 = 100;

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
    let args = match lumen_app::args() {
        Ok(a) => a,
        Err(e) => {
            log::error(&format!("window: args: {e}"));
            return 1;
        }
    };
    let mode = args.first().map_or("", |a| a.as_str());
    // The modes that need no window.
    match mode {
        "log" => {
            let mut line = String::from("log: ");
            line.extend(core::iter::repeat_n('L', 1095));
            log::info(&line);
            return 0;
        }
        "panic" => {
            let long: String = core::iter::repeat_n('p', 300).collect();
            panic!("{long}");
        }
        "alloc" => {
            for _ in 0..64 {
                let block: Vec<u8> = black_box(alloc::vec![1; 256 * 1024]);
                drop(block);
            }
            log::info("window: alloc: 64 rounds");
            return 0;
        }
        _ => {}
    }
    let title = if mode == "close" { "a\u{1b}b\nc" } else { "window" };
    let mut w = match Desktop::get().and_then(|d| d.open_window(title, W, H)) {
        Ok(w) => w,
        Err(e) => {
            log::error(&format!("window: no window: {e}"));
            return 1;
        }
    };
    if mode == "rects" {
        w.fill(w.bounds(), 0x00ff_0000);
        // 20 columns 5 pixels wide, from x = 100.
        let rects: Vec<Rect> = (0..20).map(|i| Rect::new(100 + 5 * i, 0, 5, H)).collect();
        if let Err(e) = w.commit(&rects) {
            log::error(&format!("window: rects: {e}"));
            return 2;
        }
        log::info("window: rects: committed 20");
    } else {
        w.fill(w.bounds(), 0x00_33_66_99);
        if w.commit_all().is_err() {
            return 2;
        }
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
        if e != Event::Close {
            continue;
        }
        match mode {
            "ignore-close" => {
                log::info("window: ignoring close");
                let mut n = 0u64;
                loop {
                    n = black_box(n.wrapping_add(1));
                }
            }
            "close" => {
                drop(w);
                log::info("window: closed it");
                let Ok(time) = Time::get() else { return 4 };
                for _ in 0..50 {
                    if time.sleep(100_000_000).is_err() {
                        return 5;
                    }
                }
                return 0;
            }
            _ => return 0,
        }
    }
}
