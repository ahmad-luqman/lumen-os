//! X-ray (#208): a live view of what the kernel is doing, read from the
//! trace rings through the `Trace` capability (ARCHITECTURE section 8).
//!
//! - **The tree** (on the left, in lumen-ui through the SDK's toolkit):
//!   Pause and Resume, a `State` label (`live`, `paused` or `replay`), and
//!   a row for each CPU of fixed-width labels: `cpu <n>` (`cpu <n> off`
//!   only while its reads are refused), `cpu <n> runq` (its run queue's
//!   length, from its newest `runq` event), `cpu <n> thread` (the thread
//!   running, from its newest `switch`), `cpu <n> irqs` and `cpu <n> locks`
//!   (the handled device interrupts and the locks that waited, read so far).
//!   A value not known yet (nothing read, or the queue and thread of a CPU
//!   gone offline) is `-`.
//! - **The lanes** (on the right, the app's own drawing): one per CPU, a
//!   column per refresh, swept left to right over the oldest: the run
//!   queue's longest length in the column as a bar (the palette's accent),
//!   its interrupts as a mark at the top (the text colour), a lock that
//!   waited as an orange mark, and a gap at the column written next.
//!   Each refresh repaints two columns' strips (its damage).
//! - **Live** (`run xray`): every half second (`run_until`, animating),
//!   each CPU's two rings are read on from where the last read stopped,
//!   up to a whole ring's worth a refresh, and the labels follow. Pause
//!   stops the reads (no period: the window settles); Resume reads on.
//! - **Frozen** (`run xray --replay`): `lumen_xray::replay`'s fixed trace,
//!   read once: no period, Pause and Resume disabled, for the golden.
//! - **Grants:** `trace` (live only), `desktop`, and `time` for the
//!   toolkit's loop. Without `trace` (`run --without trace xray`), live
//!   X-ray logs one ERROR and exits with 1, opening nothing.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use lumen_app::api::kind::Stream;
use lumen_app::api::limits::TRACE_EVENTS_PER_READ;
use lumen_app::api::trace::{EVENT_SIZE, HEADER_SIZE, decode_event, decode_header};
use lumen_app::gfx::Rect as Area;
use lumen_app::ui::{Flow, Taken, Ticking, Toolkit};
use lumen_app::{Desktop, Error, Event as WindowEvent, Time, Trace, log};
use lumen_trace::Event;
use lumen_ui::{App, Over, Then, Ui, UiEvent, Widget};
use lumen_xray::{COLUMNS, Cpu, Presence, Span, View, replay};

lumen_app::manifest!(time, desktop, trace);
lumen_app::main!(run);

/// How often live X-ray reads the rings.
const REFRESH_NS: u64 = 500_000_000;
/// Reads of one ring a refresh at most: a whole ring's worth
/// (`TRACE_EVENTS_PER_READ` each), so it keeps up unless a ring fills
/// faster than that; what it can't keep up with the kernel counts missed.
const READS_PER_RING: usize = 16;
/// The CPUs shown at most (the kernel's `MAX_CPUS`).
const MAX_SHOWN: usize = 8;

/// The tree's part of the window, on the left.
const TREE_W: u32 = 300;
/// Where the lanes start, and each column's width.
const LANES_X: i32 = TREE_W as i32 + 4;
const COL_W: u32 = 4;
const LANES_W: u32 = COLUMNS as u32 * COL_W;
const WIDTH: u32 = LANES_X as u32 + LANES_W + 8;
/// The first lane's top, each lane's height and the space below it.
const LANE_Y0: i32 = 10;
const LANE_H: u32 = 52;
const LANE_GAP: u32 = 6;
/// A run-queue length's pixels in a bar, and the longest a bar shows.
const RUNQ_UNIT: u32 = 4;
const RUNQ_CAP: u64 = 8;
/// The interrupt mark's pixels an interrupt, and the most.
const IRQ_CAP: u32 = 8;
/// A lock that waited: an orange mark (the palettes have no such colour).
const LOCK_RGB: u32 = 0x00d8_3b01;
/// The labels' widths in a CPU's row.
const CPU_W: u32 = 64;
const RUNQ_W: u32 = 40;
const THREAD_W: u32 = 56;
const IRQS_W: u32 = 56;
const LOCKS_W: u32 = 48;

/// Live, paused or the frozen view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Live,
    Paused,
    Replay,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Live => "live",
            Mode::Paused => "paused",
            Mode::Replay => "replay",
        }
    }
}

/// One CPU's labels.
struct Row {
    cpu: Widget,
    runq: Widget,
    thread: Widget,
    irqs: Widget,
    locks: Widget,
}

/// The app: see the crate's doc.
struct Xray {
    ui: Ui,
    view: View,
    rows: Vec<Row>,
    state: Widget,
    pause: Widget,
    resume: Widget,
    mode: Mode,
    /// The rings (live; `None` for the frozen view).
    trace: Option<Trace>,
    /// One read's bytes, and its events decoded.
    buf: Vec<u8>,
    events: Vec<Event>,
    /// Each CPU's presence at the last refresh (a change repaints its
    /// lane).
    presence: Vec<Presence>,
    drawn: Vec<Area>,
    /// Why a read failed (the loop ends then; the app exits with 1).
    failed: Option<Error>,
}

/// A lane's top.
fn lane_top(cpu: usize) -> i32 {
    LANE_Y0 + cpu as i32 * LANE_H as i32
}

/// CPU `cpu`'s lane.
fn lane(cpu: usize) -> Area {
    Area::new(LANES_X, lane_top(cpu), LANES_W, LANE_H - LANE_GAP)
}

/// Column `c`'s strip across `cpus` lanes.
fn strip(c: usize, cpus: usize) -> Area {
    Area::new(LANES_X + (c as u32 * COL_W) as i32, LANE_Y0, COL_W, cpus as u32 * LANE_H)
}

/// A count or `-` for one not known yet.
fn shown(v: Option<u64>) -> String {
    v.map_or_else(|| String::from("-"), |v| format!("{v}"))
}

impl Xray {
    fn new(view: View, mode: Mode, trace: Option<Trace>) -> Xray {
        let mut ui = Ui::new();
        let controls = ui.hstack(None, "Controls");
        let pause = ui.button(Some(controls), "Pause");
        let resume = ui.button(Some(controls), "Resume");
        let state = ui.fixed_label(None, "State", mode.name(), CPU_W);
        let heading = ui.hstack(None, "Heading");
        for (name, w) in [("cpu", CPU_W), ("runq", RUNQ_W), ("thread", THREAD_W), ("irqs", IRQS_W), ("locks", LOCKS_W)]
        {
            ui.fixed_label(Some(heading), name, name, w);
        }
        let n = view.cpus().len();
        let rows = (0..n)
            .map(|i| {
                let row = ui.hstack(None, &format!("CPU {i}"));
                Row {
                    cpu: ui.fixed_label(Some(row), &format!("cpu {i}"), &format!("cpu {i}"), CPU_W),
                    runq: ui.fixed_label(Some(row), &format!("cpu {i} runq"), "-", RUNQ_W),
                    thread: ui.fixed_label(Some(row), &format!("cpu {i} thread"), "-", THREAD_W),
                    irqs: ui.fixed_label(Some(row), &format!("cpu {i} irqs"), "-", IRQS_W),
                    locks: ui.fixed_label(Some(row), &format!("cpu {i} locks"), "-", LOCKS_W),
                }
            })
            .collect();
        if mode == Mode::Replay {
            ui.set_disabled(pause, true);
            ui.set_disabled(resume, true);
        }
        let presence = view.cpus().iter().map(|c| c.presence).collect();
        let mut x = Xray {
            ui,
            view,
            rows,
            state,
            pause,
            resume,
            mode,
            trace,
            buf: vec![0; HEADER_SIZE + EVENT_SIZE * TRACE_EVENTS_PER_READ],
            events: Vec::with_capacity(TRACE_EVENTS_PER_READ),
            presence,
            drawn: Vec::new(),
            failed: None,
        };
        x.show();
        x
    }

    /// The labels set from the view (`set_text` changes only what differs).
    fn show(&mut self) {
        for (i, (row, c)) in self.rows.iter().zip(self.view.cpus()).enumerate() {
            let label = match c.presence {
                Presence::Offline => format!("cpu {i} off"),
                Presence::Unread | Presence::Online => format!("cpu {i}"),
            };
            self.ui.set_text(row.cpu, &label);
            self.ui.set_text(row.runq, &shown(c.runq));
            self.ui.set_text(row.thread, &shown(c.thread));
            self.ui.set_text(row.irqs, &shown(c.irqs));
            self.ui.set_text(row.locks, &shown(c.locks));
        }
        self.ui.set_text(self.state, self.mode.name());
    }

    /// Each CPU's two rings read on from where the last read stopped.
    fn read_all(&mut self) -> Result<(), Error> {
        let Some(trace) = self.trace else { return Ok(()) };
        for cpu in 0..self.view.cpus().len() {
            'rings: for stream in [Stream::Main, Stream::Xray] {
                for _ in 0..READS_PER_RING {
                    let since = self.view.since(cpu, stream);
                    let copied = match trace.read(stream, cpu as u32, since, &mut self.buf) {
                        Ok(n) => n as usize,
                        // Not online (an index hole below `trace_cpus`): the
                        // only argument a read from the last `next` can fail.
                        Err(Error::BadArgument) => {
                            self.view.offline(cpu);
                            break 'rings;
                        }
                        Err(e) => return Err(e),
                    };
                    let (header, rest) = self.buf.split_at(HEADER_SIZE);
                    let (next, missed) = decode_header(header.try_into().map_err(|_| Error::BadArgument)?);
                    self.events.clear();
                    self.events.extend(rest.as_chunks::<EVENT_SIZE>().0.iter().take(copied).map(decode_event));
                    self.view.read(cpu, stream, next, missed, &self.events);
                    if copied < TRACE_EVENTS_PER_READ {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    /// One CPU's column `c` drawn where `part` (inside its lane) reaches.
    fn draw_column(over: &mut Over<'_>, part: Area, cpu: &Cpu, i: usize, c: usize, cursor: usize) {
        let x = LANES_X + (c as u32 * COL_W) as i32;
        let top = lane_top(i);
        let mut fill = |r: Area, rgb: u32| {
            if let Some(r) = r.intersect(&part) {
                over.surface.fill_rect(r, rgb);
            }
        };
        if c == cursor {
            // The column written next: a gap with a line, the sweep's front.
            fill(Area::new(x, top, 1, LANE_H - LANE_GAP), over.palette.border);
            return;
        }
        let col = cpu.column(c);
        if col.span != Span::Counted {
            return;
        }
        let base = top + (LANE_H - LANE_GAP) as i32 - 2;
        let h = col.runq.min(RUNQ_CAP) as u32 * RUNQ_UNIT;
        if h > 0 {
            fill(Area::new(x, base - h as i32, COL_W - 1, h), over.palette.accent);
        }
        let irqs = col.irqs.min(IRQ_CAP);
        if irqs > 0 {
            fill(Area::new(x, top + 1, COL_W - 1, irqs), over.palette.text);
        }
        if col.locks > 0 {
            fill(Area::new(x, top + 1 + IRQ_CAP as i32 + 1, COL_W - 1, 3), LOCK_RGB);
        }
    }
}

impl App for Xray {
    fn ui_mut(&mut self) -> &mut Ui {
        &mut self.ui
    }

    fn apply(&mut self, events: &[UiEvent]) -> Then {
        for e in events {
            match (e, self.mode) {
                (UiEvent::Pressed(w), Mode::Live) if *w == self.pause => self.mode = Mode::Paused,
                (UiEvent::Pressed(w), Mode::Paused) if *w == self.resume => self.mode = Mode::Live,
                _ => {}
            }
        }
        self.show();
        Then::Carry
    }

    fn paint_over(&mut self, over: &mut Over<'_>) {
        // Only the foreground: each rect has the tree's background already.
        let cursor = self.view.cursor();
        for (i, cpu) in self.view.cpus().iter().enumerate() {
            let lane = lane(i);
            let base = Area::new(LANES_X, lane.bottom() - 1, LANES_W, 1);
            for r in over.rects {
                let Some(part) = r.intersect(&lane) else { continue };
                if let Some(b) = base.intersect(&part) {
                    over.surface.fill_rect(b, over.palette.border);
                }
                let first = ((part.x() - LANES_X) / COL_W as i32) as usize;
                let last = ((part.right() - 1 - LANES_X) / COL_W as i32) as usize;
                for c in first..=last.min(COLUMNS - 1) {
                    Xray::draw_column(over, part, cpu, i, c, cursor);
                }
            }
        }
    }

    fn take_drawn(&mut self) -> Vec<Area> {
        core::mem::take(&mut self.drawn)
    }
}

impl Ticking for Xray {
    fn period(&self) -> Option<u64> {
        (self.mode == Mode::Live).then_some(REFRESH_NS)
    }

    fn tick(&mut self, _now: u64) -> Flow {
        if let Err(e) = self.read_all() {
            self.failed = Some(e);
            return Flow::Quit;
        }
        let n = self.view.cpus().len();
        let written = self.view.end_column();
        self.drawn.push(strip(written, n));
        self.drawn.push(strip(self.view.cursor(), n));
        for (i, (was, c)) in self.presence.iter_mut().zip(self.view.cpus()).enumerate() {
            if *was != c.presence {
                *was = c.presence;
                self.drawn.push(lane(i));
            }
        }
        self.show();
        Flow::Carry
    }
}

/// The app's exit code: 0 once its window closes; 1 once a failure is
/// logged (no trace grant, no window, a read refused); 2 for arguments it
/// doesn't take.
fn run() -> i32 {
    let replay = match lumen_app::args().as_deref() {
        Ok([]) => false,
        Ok([a]) if a == "--replay" => true,
        _ => {
            log::error("usage: xray [--replay]");
            return 2;
        }
    };
    lumen_app::report(app(replay))
}

/// The app, live or frozen: what failed and how, if anything did.
fn app(replay: bool) -> Result<(), (&'static str, Error)> {
    let time = Time::get().map_err(|e| ("no time grant", e))?;
    let (view, mode, trace) = if replay {
        (replay::view(), Mode::Replay, None)
    } else {
        let trace = Trace::get().map_err(|e| ("no trace grant", e))?;
        let cpus = trace.cpus().map_err(|e| ("no cpu count", e))? as usize;
        (View::new(cpus.min(MAX_SHOWN)), Mode::Live, Some(trace))
    };
    let n = view.cpus().len();
    let height = (2 * LANE_Y0 as u32 + n as u32 * LANE_H).max(110 + 28 * n as u32).min(480);
    let desktop = Desktop::get().map_err(|e| ("no desktop grant", e))?;
    let window = desktop.open_window("X-ray", WIDTH, height).map_err(|e| ("no window", e))?;
    let app = Xray::new(view, mode, trace);
    let mut tk = Toolkit::with_area(app, window, Area::new(0, 0, TREE_W, height)).map_err(|e| ("no toolkit", e))?;
    tk.run_until(time, |_: &mut Xray, _: &WindowEvent| Taken::Pass).map_err(|e| ("toolkit", e))?;
    match tk.app_mut().failed {
        Some(e) => Err(("reading the trace", e)),
        None => Ok(()),
    }
}
