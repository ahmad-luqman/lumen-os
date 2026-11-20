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
//!   animating and not; a toolkit whose tree's area is off the client is
//!   `BadArgument`; then `exit(7)`.
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
//! - `tick [greedy|quit|slow]`: a tree on the left (`Ticks`, a fixed-width
//!   label counting the ticks; `State`, `running` or `stopped`; buttons
//!   `Stop` and `Start`) and the app's own area on the right, where a
//!   16-pixel square in the palette's accent moves 8 pixels a tick (its own
//!   damage, `take_drawn`; it paints only the square, the background being
//!   the tree's). It ticks every 50 ms while running (an animating wait)
//!   and not at all stopped (so the desktop settles). `n` and Space move
//!   the square without a tick (taken by the app: its drawing committed,
//!   the tree unchanged, a focused button never pressed by Space); Escape
//!   quits (`Taken::Quit`). `greedy` also takes every other event offered
//!   (doing nothing with it: requests, `Close`, `Theme`, focus and `Lost`
//!   must still reach the toolkit); `quit` quits from its fifth tick
//!   (`Flow::Quit`); `slow` makes each tick's work 75 ms, longer than its
//!   period, and WARNs if a tick comes less than a period after the last
//!   one's work ended (the pacer counts from the end of an overrunning
//!   tick's work). It logs `window: tick: running`, `window: tick:
//!   accent 0x<rrggbb>` each time its frame is drawn in a new accent, a
//!   WARN if a tick comes less than a period after the one before (the
//!   launch before the first: what the pacer guarantees, across Stop and
//!   Start too), and `window: tick: <how> after <n> ticks, <ms> ms from the
//!   first to the last` (`closed` or `quit`) when the loop returns, then
//!   returns 0; with no time grant, it logs `window: tick: no time grant:
//!   <e>` and returns 1.

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
use lumen_app::ui::{Ended, Flow, Taken, Ticking, Toolkit};
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
        "tick" => return tick(args.get(1).map_or("", |a| a.as_str())),
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
    // A toolkit whose tree's area is off the client: BadArgument (its
    // window dropped with it).
    match desk.open_window("sdk", 40, 20) {
        Ok(w) => {
            let off = lumen_app::gfx::Rect::new(40, 0, 10, 10);
            check(
                "a tree's area off the client is BadArgument",
                matches!(Toolkit::with_area(Grower::new(), w, off), Err(Error::BadArgument)),
            );
        }
        Err(_) => check("a second window", false),
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

/// The `tick` mode's window, the tree's part of it (left) and the app's
/// own (the rest).
const TICK_W: u32 = 320;
const TICK_H: u32 = 160;
const TICK_TREE_W: u32 = 140;
const TICK_TREE: Area = Area::new(0, 0, TICK_TREE_W, TICK_H);
const TICK_OWN: Area = Area::new(TICK_TREE_W as i32, 0, TICK_W - TICK_TREE_W, TICK_H);
/// Its ticks while running, and how far the square moves each one.
const TICK_NS: u64 = 50_000_000;
const STEP: i32 = 8;
const SQUARE: u32 = 16;
/// Where the square starts, and goes back to past the area's right edge.
const SQUARE_HOME: i32 = TICK_OWN.x() + 10;
/// The `Ticks` label's fixed width: its count changing width would move
/// the widgets after it, repainting (and committing) the whole window.
const TICKS_W: u32 = 60;
/// `tick quit`: the tick that quits.
const QUIT_AT: u64 = 5;
/// `tick slow`: how long each tick's work takes, longer than its period.
const SLOW_NS: u64 = 75_000_000;

/// How the `tick` mode runs: plainly, taking every event offered
/// (`greedy`), quitting from its fifth tick (`quit`), or with each tick's
/// work longer than its period (`slow`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum TickMode {
    Plain,
    Greedy,
    Quit,
    Slow,
}

/// The `tick` mode's app: see the crate's doc.
struct Ticker {
    ui: Ui,
    count: Widget,
    state: Widget,
    stop: Widget,
    start: Widget,
    mode: TickMode,
    ticks: u64,
    running: bool,
    /// The time of the last tick (the launch before the first), which the
    /// next must come at least a period after (what the pacer guarantees,
    /// across Stop and Start too): an earlier one logs a WARN; and the first
    /// tick's time.
    last_tick: u64,
    first_tick: Option<u64>,
    /// `slow`: the clock, and when the last tick's work ended (the next
    /// must come at least a period after it, else a WARN).
    time: Time,
    work_ended: Option<u64>,
    square: Area,
    drawn: Vec<Area>,
    /// The accent its frame was last drawn in (logged when it changes).
    accent: Option<u32>,
}

impl Ticker {
    fn new(time: Time, mode: TickMode) -> Ticker {
        let mut ui = Ui::new();
        let count = ui.fixed_label(None, "Ticks", "0", TICKS_W);
        let state = ui.label(None, "State", "running");
        let stop = ui.button(None, "Stop");
        let start = ui.button(None, "Start");
        let now = clock(&time);
        Ticker {
            ui,
            count,
            state,
            stop,
            start,
            mode,
            ticks: 0,
            running: true,
            last_tick: now,
            first_tick: None,
            time,
            work_ended: None,
            square: Area::new(SQUARE_HOME, 70, SQUARE, SQUARE),
            drawn: Vec::new(),
            accent: None,
        }
    }

    /// The square moved `STEP` pixels right, back home past the area's
    /// right edge; its old and new places are the app's damage.
    fn move_square(&mut self) {
        let mut x = self.square.x() + STEP;
        if x + SQUARE as i32 > TICK_OWN.right() {
            x = SQUARE_HOME;
        }
        let old = self.square;
        self.square = Area::new(x, self.square.y(), SQUARE, SQUARE);
        self.drawn.push(old);
        self.drawn.push(self.square);
    }

    /// What `take` does with an event offered: Escape quits; `n` and Space
    /// move the square (taken: the tree never sees them, so Space never
    /// presses a focused button); in `greedy` mode everything else offered
    /// is taken too (doing nothing).
    fn take(&mut self, e: &Event) -> Taken {
        match *e {
            Event::Key { code: KeyCode::Escape, action: KeyAction::Press { .. }, .. } => Taken::Quit,
            Event::Key { code: KeyCode::N | KeyCode::Spacebar, action: KeyAction::Press { .. }, .. } => {
                self.move_square();
                Taken::Took
            }
            _ if self.mode == TickMode::Greedy => Taken::Took,
            _ => Taken::Pass,
        }
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
                    self.running = true;
                    self.ui.set_text(self.state, "running");
                }
                _ => {}
            }
        }
        Then::Carry
    }

    fn paint_over(&mut self, over: &mut Over<'_>) {
        // Only the foreground: each rect has the tree's background already.
        for r in over.rects {
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
        let floor = self.last_tick + TICK_NS;
        if now < floor {
            log::warn(&format!("window: tick: tick {} came {} ns early", self.ticks, floor - now));
        }
        if let Some(ended) = self.work_ended
            && now < ended + TICK_NS
        {
            log::warn(&format!(
                "window: tick: tick {} came {} ns early after the last one's work",
                self.ticks,
                ended + TICK_NS - now
            ));
        }
        self.last_tick = now;
        self.first_tick.get_or_insert(now);
        self.ui.set_text(self.count, &format!("{}", self.ticks));
        self.move_square();
        if self.mode == TickMode::Slow {
            if let Err(e) = self.time.sleep(SLOW_NS) {
                log::error(&format!("window: tick: slow: {e}"));
            }
            self.work_ended = Some(clock(&self.time));
        }
        if self.mode == TickMode::Quit && self.ticks == QUIT_AT { Flow::Quit } else { Flow::Carry }
    }
}

/// The time now, or 0 once an ERROR is logged (a 0 would switch the floor
/// checks off, so it isn't silent).
fn clock(time: &Time) -> u64 {
    time.now_ns().unwrap_or_else(|e| {
        log::error(&format!("window: tick: no clock: {e}"));
        0
    })
}

/// The `tick` mode ([`Ticker`] in the SDK's `run_until`, until its window
/// closes or it quits; 0 then), `mode` its argument after `tick`.
fn tick(mode: &str) -> i32 {
    let mode = match mode {
        "" => TickMode::Plain,
        "greedy" => TickMode::Greedy,
        "quit" => TickMode::Quit,
        "slow" => TickMode::Slow,
        other => {
            log::error(&format!("window: tick: no mode {other}"));
            return 1;
        }
    };
    let time = match Time::get() {
        Ok(t) => t,
        Err(e) => {
            log::error(&format!("window: tick: no time grant: {e}"));
            return 1;
        }
    };
    let w = match Desktop::get().and_then(|d| d.open_window("window", TICK_W, TICK_H)) {
        Ok(w) => w,
        Err(e) => {
            log::error(&format!("window: no window: {e}"));
            return 1;
        }
    };
    let mut tk = match Toolkit::with_area(Ticker::new(time, mode), w, TICK_TREE) {
        Ok(tk) => tk,
        Err(e) => {
            log::error(&format!("window: no toolkit: {e}"));
            return 1;
        }
    };
    log::info("window: tick: running");
    match tk.run_until(time, Ticker::take) {
        Ok(how) => {
            let t = tk.app_mut();
            let elapsed_ms = t.first_tick.map_or(0, |first| (t.last_tick - first) / 1_000_000);
            let how = if how == Ended::Quit { "quit" } else { "closed" };
            log::info(&format!(
                "window: tick: {how} after {} ticks, {elapsed_ms} ms from the first to the last",
                t.ticks
            ));
            0
        }
        Err(e) => {
            log::error(&format!("window: tick: {e}"));
            2
        }
    }
}
