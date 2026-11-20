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
//! - `close`: its window titled `a<ESC>b<LF>c`; given `Close`, it drops
//!   the window (`window: closed it`), then sleeps 100 ms at a time until
//!   killed: each sleep a slice end, so it is soon far past the close
//!   grace's.
//! - `rects`: fills its canvas red and commits 20 rectangles covering only
//!   its right half (`x` from 100: more than `COMMIT_RECTS`, so the SDK
//!   commits in turns), logs `window: rects: committed 20`, then reads
//!   events until `Close`.
//! - `log`: logs one 1,205-byte line (`log: ` then 600 `é`s, 2 bytes each,
//!   so byte `TEXT_BYTES` falls inside one): the SDK cuts it to
//!   `TEXT_BYTES` on a char boundary, so the kernel logs its start rather
//!   than refusing it whole.
//! - `panic`: panics with a 300-byte message: the SDK's panic handler cuts
//!   it to fit ` at <file>:<line>`.
//! - `panic-utf8`: the same with 200 `€`s (3 bytes each): cut on a char
//!   boundary, so the line number shown is whole (the scenario finds the
//!   line by its `panic-utf8` marker).
//! - `alloc`: allocates and frees 256 KiB 64 times (16 MiB in all, in a
//!   2 MiB memory): only a heap that frees lasts; logs `window: alloc: 64
//!   rounds` and returns 0.
//! - `sdk`: the SDK's other calls, each checked (`window: sdk: <n> checks,
//!   <f> failed`, and a line per failure): `open_window` past
//!   `WINDOW_PIXELS` is `BadArgument` (not `OutOfMemory`), `Trace` (`cpus`
//!   at least 1, a read whose count agrees with its header), `Desktop::
//!   measure`, `Time::now_ns` and `wall`, `Window::wait` with a deadline,
//!   animating and not; then `exit(7)`.
//!
//! The toolkit modes (#207), each its window titled `window`:
//!
//! - `ui`: `lumen_ui::counter::Counter` (the kernel's demo app: Add, Reset,
//!   Lock, Quit) in the SDK's `ui::Toolkit`, published after each change,
//!   `ui invoke` reaching it as a click does; returns 0 when its window
//!   closes (Quit, or `Close`).
//! - `ui-trap`: the same, but a press on Add traps (`unreachable`) while
//!   its request runs: the request is answered `OwnerEnded`.
//! - `deep`: publishes the deepest tree there is, `TREE_NODES` groups each
//!   holding the next (`window: deep: published <n> nodes (<b> bytes)`),
//!   then goes on as with no argument (events read until `Close`).
//! - `deaf`: publishes one button (app id 1), logs `window: deaf:
//!   published`, then sleeps for good, never reading its events: a request
//!   to it stays queued, so its window's one request slot stays taken.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::hint::black_box;
use lumen_app::api::kind::Stream;
use lumen_app::api::layout::Rect as WireRect;
use lumen_app::api::limits::TREE_NODES;
use lumen_app::api::trace::{EVENT_SIZE, HEADER_SIZE, decode_header};
use lumen_app::api::tree::{Common, Node, Tree};
use lumen_app::ui::Toolkit;
use lumen_app::{Desktop, Error, Event, Rect, Time, Trace, log};
use lumen_ui::counter::Counter;
use lumen_ui::{App, Then, Ui, UiEvent};

lumen_app::manifest!(time, desktop, trace);
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
            line.extend(core::iter::repeat_n('é', 600));
            log::info(&line);
            return 0;
        }
        "sdk" => return sdk(),
        "panic" => {
            let long: String = core::iter::repeat_n('p', 300).collect();
            panic!("{long}");
        }
        "panic-utf8" => {
            let long: String = core::iter::repeat_n('€', 200).collect();
            panic!("{long}"); // marker: panic-utf8
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
    match mode {
        "ui" => return toolkit(Counter::new(), w),
        "ui-trap" => return toolkit(Trapping(Counter::new()), w),
        "deep" | "deaf" => {
            if let Err(code) = publish(mode, &w) {
                return code;
            }
        }
        _ => {}
    }
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
                loop {
                    if time.sleep(100_000_000).is_err() {
                        return 5;
                    }
                }
            }
            _ => return 0,
        }
    }
}

/// The `sdk` mode's checks: a line per failure, then the count; `exit(7)`.
fn sdk() -> i32 {
    let mut run = 0;
    let mut failed = 0;
    let mut check = |what: &str, ok: bool| {
        run += 1;
        if !ok {
            failed += 1;
            log::info(&format!("window: sdk: FAILED {what}"));
        }
    };
    let Ok(desk) = Desktop::get() else { return 1 };
    check(
        "a window past WINDOW_PIXELS is BadArgument",
        matches!(desk.open_window("x", 65536, 65536), Err(Error::BadArgument)),
    );
    check("measure", desk.measure(16, "abc").is_ok_and(|w| w > 0));
    match Trace::get() {
        Ok(trace) => {
            check("trace cpus", trace.cpus().is_ok_and(|n| n >= 1));
            let mut buf = [0u8; HEADER_SIZE + 4 * EVENT_SIZE];
            match trace.read(Stream::Main, 0, 0, &mut buf) {
                Ok(n) => {
                    let mut header = [0u8; HEADER_SIZE];
                    header.copy_from_slice(&buf[..HEADER_SIZE]);
                    let (next, missed) = decode_header(&header);
                    check("a trace read's count agrees with its header", n <= 4 && next == missed + u64::from(n));
                }
                Err(_) => check("a trace read", false),
            }
        }
        Err(_) => check("a trace handle", false),
    }
    let Ok(time) = Time::get() else { return 2 };
    check("now_ns", time.now_ns().is_ok_and(|ns| ns > 0));
    check("wall past 2020", time.wall().is_ok_and(|s| s > 1_577_836_800));
    match desk.open_window("sdk", 40, 20) {
        Ok(mut w) => {
            // Its first event (the theme) read, then waits with deadlines.
            check("wait for the theme", w.wait_event().is_ok_and(|e| matches!(e, Event::Theme { .. })));
            let soon = |t: &Time| t.now_ns().unwrap_or(0) + 10_000_000;
            check("wait with a deadline", w.wait(Some(soon(&time)), false).is_ok());
            check("an animating wait", w.wait(Some(soon(&time)), true).is_ok());
            check("animating without a deadline", w.wait(None, true) == Err(Error::BadArgument));
        }
        Err(_) => check("a window", false),
    }
    log::info(&format!("window: sdk: {run} checks, {failed} failed"));
    lumen_app::exit(7)
}

/// Counter, but a press on Add traps while its request runs.
struct Trapping(Counter);

impl App for Trapping {
    fn ui_mut(&mut self) -> &mut Ui {
        self.0.ui_mut()
    }

    fn apply(&mut self, events: &[UiEvent]) -> Then {
        if events.contains(&UiEvent::Pressed(self.0.add)) {
            core::arch::wasm32::unreachable();
        }
        self.0.apply(events)
    }
}

/// The `ui` modes: `app` in the SDK's toolkit until its window closes.
fn toolkit(app: impl App, w: lumen_app::Window) -> i32 {
    let mut tk = match Toolkit::new(app, w) {
        Ok(tk) => tk,
        Err(e) => {
            log::error(&format!("window: no toolkit: {e}"));
            return 1;
        }
    };
    log::info("window: ui: running");
    match tk.run() {
        Ok(()) => 0,
        Err(e) => {
            log::error(&format!("window: ui: {e}"));
            2
        }
    }
}

/// A node of the hand-made trees: id `id`, a 10 × 10 box.
fn common(id: u64) -> Common {
    Common { id, name: String::new(), bounds: WireRect::new(0, 0, 10, 10), disabled: false, hidden: false }
}

/// The `deep` and `deaf` modes: a hand-made tree published; `deaf` then
/// sleeps for good (its events never read); `deep` goes on as the plain
/// mode (events read until `Close`). An exit code if it failed.
fn publish(mode: &str, w: &lumen_app::Window) -> Result<(), i32> {
    let nodes: Vec<Node> = if mode == "deep" {
        let n = TREE_NODES as u64;
        (1..=n).map(|id| Node::Group { common: common(id), children: u32::from(id < n) }).collect()
    } else {
        alloc::vec![Node::Button { common: Common { name: String::from("Go"), ..common(1) }, focused: false }]
    };
    let count = nodes.len();
    let bytes = match Tree::new(nodes).and_then(|t| t.encode()) {
        Ok(b) => b,
        Err(e) => {
            log::error(&format!("window: {mode}: {e}"));
            return Err(1);
        }
    };
    if let Err(e) = w.publish(&bytes) {
        log::error(&format!("window: {mode}: publish: {e}"));
        return Err(2);
    }
    if mode == "deaf" {
        log::info("window: deaf: published");
        let Ok(time) = Time::get() else { return Err(4) };
        loop {
            if time.sleep(1_000_000_000).is_err() {
                return Err(5);
            }
        }
    }
    log::info(&format!("window: deep: published {count} nodes ({} bytes)", bytes.len()));
    Ok(())
}
