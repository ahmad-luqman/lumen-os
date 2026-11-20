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
//!   4 MiB memory: its `build.rs`): only a heap that frees lasts; logs `window: alloc: 64
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
//!   Lock, Quit), with a label `Long` of `2 × TEXT_BYTES` characters (the
//!   SDK's text cuts it), in the SDK's `ui::Toolkit`, published after each change,
//!   `ui invoke` reaching it as a click does; returns 0 when its window
//!   closes (Quit, or `Close`).
//! - `ui-trap`: the same, but a press on Add traps (`unreachable`) while
//!   its request runs: the request is answered `OwnerEnded`.
//! - `ui-big`: a button `Grow` whose press adds `TREE_NODES + 1` labels:
//!   the SDK refuses that tree itself (one WARN `ui: a tree not published:
//!   the tree: ...`), the kernel keeps the last one, and the press (and any
//!   later request) is answered `couldn't publish: ...`.
//! - `deep`: publishes the deepest tree there is, `TREE_NODES` groups each
//!   holding the next (`window: deep: published <n> nodes (<b> bytes)`),
//!   then goes on as with no argument (events read until `Close`).
//! - `deaf`: publishes a button `Go` (app id 1) and a label `Say` whose
//!   text holds ESC, LF and U+2028, then a tree the host refuses
//!   (`window: deaf: published; a bad tree then: -5`: the last one stays),
//!   then sleeps for good, never reading its events: a request to it stays
//!   queued, so its window's one request slot stays taken.
//!
//! The SDK's tick loop (`ui::Toolkit::run_until`), its window titled
//! `window`, 320 × 160:
//!
//! - `tick`: a tree on the left (`Ticks`, a label counting the ticks;
//!   `State`, `running` or `stopped`; buttons `Stop` and `Start`) and the
//!   app's own area on the right, where a 16-pixel square in the palette's
//!   accent moves 8 pixels a tick (its own damage, `take_drawn`). It ticks
//!   every 50 ms while running (an animating wait) and not at all stopped
//!   (so the desktop settles). `n` moves the square without a tick (taken
//!   by the app: its drawing committed, the tree unchanged); Escape quits
//!   (`Taken::Quit`). It logs `window: tick: running`, `window: tick:
//!   accent 0x<rrggbb>` each time its frame is drawn in a new accent, a
//!   WARN if tick `k` of a stretch (since launch or the last Start) comes
//!   less than `k` periods after it began (the pacer's floor), and
//!   `window: tick: ended after <n> ticks; the last stretch <k> ticks in
//!   <ms> ms` (from the stretch's start to its last tick) when the loop
//!   returns (`Close` or Escape), then returns 0.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::hint::black_box;
use lumen_app::api::kind::Stream;
use lumen_app::api::layout::Rect as WireRect;
use lumen_app::api::limits::{TEXT_BYTES, TREE_NODES};
use lumen_app::api::trace::{EVENT_SIZE, HEADER_SIZE, decode_header};
use lumen_app::api::tree::{Common, Node, Text, Tree};
use lumen_app::gfx::Rect as Area;
use lumen_app::ui::{Flow, Taken, Ticking, Toolkit};
use lumen_app::{Desktop, Error, Event, KeyAction, KeyCode, Rect, Time, Trace, log};
use lumen_ui::counter::Counter;
use lumen_ui::{App, Over, Then, Ui, UiEvent, Widget};

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
        "tick" => return tick(),
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
        "ui" => {
            // A label past TEXT_BYTES: the SDK's text cuts it before the
            // host's calls (a refused one would log `ui: ... refused`).
            let mut counter = Counter::new();
            counter.ui_mut().label(None, "Long", &"x".repeat(2 * TEXT_BYTES));
            return toolkit(counter, w);
        }
        "ui-trap" => return toolkit(Trapping(Counter::new()), w),
        "ui-big" => return toolkit(Grower::new(), w),
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

/// A button `Grow` whose press adds more labels than `TREE_NODES`: the
/// SDK can't publish that tree (it refuses it itself), so the kernel keeps
/// the last one and the press is answered failed.
struct Grower {
    ui: Ui,
    grow: lumen_ui::Widget,
}

impl Grower {
    fn new() -> Grower {
        let mut ui = Ui::new();
        let grow = ui.button(None, "Grow");
        Grower { ui, grow }
    }
}

impl App for Grower {
    fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn apply(&mut self, events: &[UiEvent]) -> Then {
        if events.contains(&UiEvent::Pressed(self.grow)) {
            for _ in 0..=TREE_NODES {
                self.ui.label(None, "n", "");
            }
        }
        Then::Carry
    }
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
    Common { id, name: Text::cut(""), bounds: WireRect::new(0, 0, 10, 10), disabled: false, hidden: false }
}

/// The `deep` and `deaf` modes: a hand-made tree published; `deaf` then
/// sleeps for good (its events never read); `deep` goes on as the plain
/// mode (events read until `Close`). An exit code if it failed.
fn publish(mode: &str, w: &lumen_app::Window) -> Result<(), i32> {
    let nodes: Vec<Node> = if mode == "deep" {
        let n = TREE_NODES as u64;
        (1..=n).map(|id| Node::Group { common: common(id), children: u32::from(id < n) }).collect()
    } else {
        // A button, and a label whose text holds an ESC, a line feed and a
        // line separator (`ui tree` shows U+FFFD for each).
        let odd = "a\u{1b}b\nc\u{2028}d";
        alloc::vec![
            Node::Button { common: Common { name: Text::cut("Go"), ..common(1) }, focused: false },
            Node::Label { common: Common { name: Text::cut("Say"), ..common(2) }, text: Text::cut(odd) },
        ]
    };
    let count = nodes.len();
    let tree = match Tree::new(nodes) {
        Ok(t) => t,
        Err(e) => {
            log::error(&format!("window: {mode}: {e}"));
            return Err(1);
        }
    };
    if let Err(e) = w.publish(&tree) {
        log::error(&format!("window: {mode}: publish: {e}"));
        return Err(2);
    }
    if mode == "deaf" {
        // Then a tree the host refuses (a node that doesn't decode): the
        // last one stays published.
        let junk = [0xffu8; 4];
        let r = lumen_app::sys::window_publish(w.handle().raw(), lumen_app::sys::ptr(junk.as_ptr()), 4);
        log::info(&format!("window: deaf: published; a bad tree then: {r}"));
        let Ok(time) = Time::get() else { return Err(4) };
        loop {
            if time.sleep(1_000_000_000).is_err() {
                return Err(5);
            }
        }
    }
    let bytes = tree.encode().map_or(0, |b| b.len());
    log::info(&format!("window: deep: published {count} nodes ({bytes} bytes)"));
    Ok(())
}

/// The `tick` mode's window and its tree's width (the app's area is the
/// rest).
const TICK_W: u32 = 320;
const TICK_H: u32 = 160;
const TICK_TREE_W: u32 = 140;
/// Its ticks while running, and how far the square moves each one.
const TICK_NS: u64 = 50_000_000;
const STEP: i32 = 8;
const SQUARE: u32 = 16;

/// The `tick` mode's app: see the crate's doc.
struct Ticker {
    ui: Ui,
    count: Widget,
    state: Widget,
    stop: Widget,
    start: Widget,
    ticks: u64,
    running: bool,
    /// The clock, and the current stretch of ticks (since launch or the
    /// last Start): when it started, how many, the last one's time. Tick
    /// `k` of a stretch must come at least `k` periods after its start (the
    /// pacer's floor): an earlier one logs a WARN.
    time: Time,
    stretch_start: u64,
    stretch: u64,
    last_tick: u64,
    square: Area,
    drawn: Vec<Area>,
    /// The accent its frame was last drawn in (logged when it changes).
    accent: Option<u32>,
}

impl Ticker {
    fn new(time: Time) -> Ticker {
        let mut ui = Ui::new();
        let count = ui.label(None, "Ticks", "0");
        let state = ui.label(None, "State", "running");
        let stop = ui.button(None, "Stop");
        let start = ui.button(None, "Start");
        let square = Area::new(TICK_TREE_W as i32 + 10, 70, SQUARE, SQUARE);
        let now = time.now_ns().unwrap_or(0);
        Ticker {
            ui,
            count,
            state,
            stop,
            start,
            ticks: 0,
            running: true,
            time,
            stretch_start: now,
            stretch: 0,
            last_tick: now,
            square,
            drawn: Vec::new(),
            accent: None,
        }
    }

    /// The app's own area: right of the tree.
    fn area() -> Area {
        Area::new(TICK_TREE_W as i32, 0, TICK_W - TICK_TREE_W, TICK_H)
    }

    /// The square moved `STEP` pixels right, back to the area's left past
    /// its right edge; its old and new places are the app's damage.
    fn move_square(&mut self) {
        let a = Ticker::area();
        let mut x = self.square.x() + STEP;
        if x + SQUARE as i32 > a.right() {
            x = a.x() + 10;
        }
        let old = self.square;
        self.square = Area::new(x, self.square.y(), SQUARE, SQUARE);
        self.drawn.push(old);
        self.drawn.push(self.square);
    }
}

impl App for Ticker {
    fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn apply(&mut self, events: &[UiEvent]) -> Then {
        for e in events {
            match e {
                UiEvent::Pressed(w) if *w == self.stop => {
                    self.running = false;
                    self.ui.set_text(self.state, "stopped");
                }
                UiEvent::Pressed(w) if *w == self.start => {
                    if !self.running {
                        let now = self.time.now_ns().unwrap_or(0);
                        (self.stretch_start, self.stretch, self.last_tick) = (now, 0, now);
                    }
                    self.running = true;
                    self.ui.set_text(self.state, "running");
                }
                _ => {}
            }
        }
        Then::Carry
    }

    fn paint_over(&mut self, over: &mut Over<'_>) {
        let area = Ticker::area();
        for r in over.rects {
            if let Some(part) = r.intersect(&area) {
                over.surface.fill_rect(part, over.palette.background);
            }
            if let Some(part) = r.intersect(&self.square) {
                over.surface.fill_rect(part, over.palette.accent);
            }
        }
        if self.accent != Some(over.palette.accent) {
            self.accent = Some(over.palette.accent);
            log::info(&format!("window: tick: accent 0x{:06x}", over.palette.accent));
        }
    }

    fn take_drawn(&mut self) -> Vec<Area> {
        core::mem::take(&mut self.drawn)
    }
}

impl Ticking for Ticker {
    fn period(&self) -> Option<u64> {
        self.running.then_some(TICK_NS)
    }

    fn tick(&mut self, now: u64) -> Flow {
        self.ticks += 1;
        self.stretch += 1;
        self.last_tick = now;
        let floor = self.stretch_start + self.stretch * TICK_NS;
        if now < floor {
            log::warn(&format!("window: tick: tick {} of its stretch came {} ns early", self.stretch, floor - now));
        }
        let text = format!("{}", self.ticks);
        self.ui.set_text(self.count, &text);
        self.move_square();
        Flow::Carry
    }
}

/// The `tick` mode: [`Ticker`] in the SDK's `run_until` until its window
/// closes or Escape quits; 0 then.
fn tick() -> i32 {
    let (Ok(desk), Ok(time)) = (Desktop::get(), Time::get()) else { return 1 };
    let w = match desk.open_window("window", TICK_W, TICK_H) {
        Ok(w) => w,
        Err(e) => {
            log::error(&format!("window: no window: {e}"));
            return 1;
        }
    };
    let tree = Area::new(0, 0, TICK_TREE_W, TICK_H);
    let mut tk = match Toolkit::with_area(Ticker::new(time), w, tree) {
        Ok(tk) => tk,
        Err(e) => {
            log::error(&format!("window: no toolkit: {e}"));
            return 1;
        }
    };
    log::info("window: tick: running");
    let ended = tk.run_until(time, |app, e| match *e {
        Event::Key { code: KeyCode::Escape, action: KeyAction::Press { .. }, .. } => Taken::Quit,
        Event::Key { code: KeyCode::N, action: KeyAction::Press { .. }, .. } => {
            app.move_square();
            Taken::Took
        }
        _ => Taken::Pass,
    });
    match ended {
        Ok(()) => {
            let t = tk.app_mut();
            let elapsed_ms = (t.last_tick - t.stretch_start) / 1_000_000;
            log::info(&format!(
                "window: tick: ended after {} ticks; the last stretch {} ticks in {elapsed_ms} ms",
                t.ticks, t.stretch
            ));
            0
        }
        Err(e) => {
            log::error(&format!("window: tick: {e}"));
            2
        }
    }
}
