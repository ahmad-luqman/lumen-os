//! X-ray (#208): a live view of what the kernel is doing, read from the
//! trace rings through the `Trace` capability (ARCHITECTURE section 8).
//!
//! - **The tree** (on the left, in lumen-ui through the SDK's toolkit):
//!   Pause and Resume, a `State` label (`live`, `paused` or `replay`), and
//!   a row for each CPU of fixed-width labels: `cpu <n>` (`cpu <n> off`
//!   only while its reads are refused), `cpu <n> runq` (its run queue's
//!   length, from its newest `runq` event), `cpu <n> thread` (the thread
//!   running, from its newest `switch`), `cpu <n> switches`, `cpu <n> irqs`
//!   and `cpu <n> locks` (the switches, handled device interrupts and locks
//!   that waited, read so far) and `cpu <n> missed` (sequence numbers gone
//!   before they were read, after the first read).
//!   A value not known yet (nothing read, or the queue and thread of a CPU
//!   gone offline) is `-`.
//! - **The lanes** (on the right, the app's own drawing): one per CPU, a
//!   column per refresh, swept left to right over the oldest, the column
//!   written next a line (the sweep's front): the run queue's longest
//!   length in the column as a bar (the palette's accent), its interrupts
//!   as a mark at the top (the text colour), a lock that waited as an
//!   orange mark, its switches as a grey mark; a backlog's column (a gap)
//!   and an offline CPU's are blank. Each refresh repaints two columns'
//!   strips (its damage), and a CPU's whole lane when its presence changes.
//! - **Live** (`run xray`): every half second (`run_until`, animating),
//!   each CPU's two rings are read on from where the last read stopped,
//!   up to a whole ring's worth a refresh, and the labels follow. Each
//!   ring's first read, and its first after Resume, is a backlog (what
//!   piled up since boot or while paused): only its newest run queue and
//!   thread are taken, and its column is a gap. Pause stops the reads (no
//!   period: the window settles); Resume reads on.
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
use lumen_xray::{COLUMNS, Cpu, Span, View, replay};

lumen_app::manifest!(time, desktop, trace);
lumen_app::main!(run);

/// How often live X-ray reads the rings.
const REFRESH_NS: u64 = 500_000_000;
/// Reads of one ring a refresh at most: 16 × `TRACE_EVENTS_PER_READ`
/// (256) = 4096 events, a whole ring (the kernel's `trace::EVENTS`), so it
/// keeps up unless a ring fills faster than that; what it can't keep up
/// with the kernel counts missed.
const READS_PER_RING: usize = 16;
const _: () = assert!(READS_PER_RING * TRACE_EVENTS_PER_READ == 4096);
/// The CPUs shown at most (the kernel's `MAX_CPUS`).
const MAX_SHOWN: usize = 8;

/// The tree's part of the window, on the left.
const TREE_W: u32 = 424;
/// Where the lanes start, and each column's width.
const LANES_X: i32 = TREE_W as i32 + 4;
const COL_W: u32 = 4;
const LANES_W: u32 = COLUMNS as u32 * COL_W;
const WIDTH: u32 = LANES_X as u32 + LANES_W + 8;
/// The first lane's top, each lane's height and the space below it.
const LANE_Y0: i32 = 10;
const LANE_H: u32 = 56;
const LANE_GAP: u32 = 6;
/// The tree's height above its first CPU row (Pause and Resume, `State`,
/// the heading, with lumen-ui's padding and gaps), and each CPU row's (a
/// label's line and the gap below it): what `ui tree` shows of the layout.
const TREE_HEAD_H: u32 = 107;
const TREE_ROW_H: u32 = 29;
/// The tallest window (`WINDOW_PIXELS` allows 640 × 480).
const MAX_HEIGHT: u32 = 480;
/// A run-queue length's pixels in a bar, and the longest a bar shows.
const RUNQ_UNIT: u32 = 3;
const RUNQ_CAP: u64 = 8;
/// The interrupt mark's pixels an interrupt, and the most.
const IRQ_CAP: u32 = 8;
/// The switch mark's pixels a switch, and the most.
const SWITCH_CAP: u32 = 6;
/// A lock that waited: an orange mark (the palettes have no such colour).
const LOCK_RGB: u32 = 0x00d8_3b01;
/// The labels' widths in a CPU's row.
const CPU_W: u32 = 64;
const RUNQ_W: u32 = 40;
const THREAD_W: u32 = 48;
const SWITCHES_W: u32 = 72;
const IRQS_W: u32 = 48;
const LOCKS_W: u32 = 40;
const MISSED_W: u32 = 56;

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
    switches: Widget,
    irqs: Widget,
    locks: Widget,
    missed: Widget,
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
    drawn: Vec<Area>,
    /// Whether a read failed (logged, naming its CPU, ring and `since`;
    /// the loop ends then and the app exits with 1).
    failed: bool,
    /// Whether an odd event (one that didn't decode, or in the wrong ring)
    /// was logged: once, the first time any CPU's count rises past 0.
    warned_odd: bool,
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

/// A ring's name in a log line.
fn ring_name(stream: Stream) -> &'static str {
    match stream {
        Stream::Main => "main",
        Stream::Xray => "X-ray",
    }
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
        for (name, w) in [
            ("cpu", CPU_W),
            ("runq", RUNQ_W),
            ("thread", THREAD_W),
            ("switches", SWITCHES_W),
            ("irqs", IRQS_W),
            ("locks", LOCKS_W),
            ("missed", MISSED_W),
        ] {
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
                    switches: ui.fixed_label(Some(row), &format!("cpu {i} switches"), "-", SWITCHES_W),
                    irqs: ui.fixed_label(Some(row), &format!("cpu {i} irqs"), "-", IRQS_W),
                    locks: ui.fixed_label(Some(row), &format!("cpu {i} locks"), "-", LOCKS_W),
                    missed: ui.fixed_label(Some(row), &format!("cpu {i} missed"), "-", MISSED_W),
                }
            })
            .collect();
        if mode == Mode::Replay {
            ui.set_disabled(pause, true);
            ui.set_disabled(resume, true);
        }
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
            drawn: Vec::new(),
            failed: false,
            warned_odd: false,
        };
        x.show();
        x
    }

    /// The labels set from the view (`set_text` changes only what differs).
    fn show(&mut self) {
        for (i, (row, c)) in self.rows.iter().zip(self.view.cpus()).enumerate() {
            self.ui.set_text(row.cpu, &c.label(i));
            self.ui.set_text(row.runq, &shown(c.runq));
            self.ui.set_text(row.thread, &shown(c.thread));
            self.ui.set_text(row.switches, &shown(c.switches));
            self.ui.set_text(row.irqs, &shown(c.irqs));
            self.ui.set_text(row.locks, &shown(c.locks));
            self.ui.set_text(row.missed, &shown(c.missed));
        }
        self.ui.set_text(self.state, self.mode.name());
    }

    /// Each CPU's two rings read on from where the last read stopped; a
    /// read that fails other than for an offline CPU is logged (one ERROR,
    /// naming the CPU, the ring and `since`) and is `Err`.
    fn read_all(&mut self) -> Result<(), ()> {
        let Some(trace) = self.trace else { return Ok(()) };
        for cpu in 0..self.view.cpus().len() {
            'rings: for stream in [Stream::Main, Stream::Xray] {
                for _ in 0..READS_PER_RING {
                    let since = self.view.since(cpu, stream);
                    let copied = match trace.read(stream, cpu as u32, since, &mut self.buf) {
                        Ok(n) => n as usize,
                        // A CPU not online (an index hole below `trace_cpus`),
                        // or a `since` past the ring's next number, which a
                        // read from the last `next` can't be unless the
                        // kernel is wrong: a read from 0 tells them apart.
                        Err(Error::BadArgument) => {
                            if since != 0 && trace.read(stream, cpu as u32, 0, &mut self.buf).is_ok() {
                                log::error(&format!(
                                    "cpu {cpu}: its {} ring refused a read from {since} (its last next) but not \
                                     one from 0",
                                    ring_name(stream)
                                ));
                                return Err(());
                            }
                            self.view.offline(cpu);
                            break 'rings;
                        }
                        Err(e) => {
                            log::error(&format!("cpu {cpu}: reading its {} ring from {since}: {e}", ring_name(stream)));
                            return Err(());
                        }
                    };
                    // The buffer is made with room for the header and
                    // `TRACE_EVENTS_PER_READ` events.
                    let (header, rest) = self.buf.split_first_chunk::<HEADER_SIZE>().expect("a header's room");
                    let (next, missed) = decode_header(header);
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
    let switches = col.switches.min(SWITCH_CAP);
    if switches > 0 {
        fill(Area::new(x, top + IRQ_CAP as i32 + 6, COL_W - 1, switches), over.palette.disabled);
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
                (UiEvent::Pressed(w), Mode::Paused) if *w == self.resume => {
                    // What piled up while paused is a backlog: the column it
                    // is read in is a gap, its events left out of the counts.
                    self.view.resume();
                    self.mode = Mode::Live;
                }
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
                    draw_column(over, part, cpu, i, c, cursor);
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
        if self.read_all().is_err() {
            self.failed = true;
            return Flow::Quit;
        }
        let n = self.view.cpus().len();
        let written = self.view.end_column();
        self.drawn.push(strip(written, n));
        self.drawn.push(strip(self.view.cursor(), n));
        for i in self.view.presence_changes() {
            self.drawn.push(lane(i));
        }
        if !self.warned_odd
            && let Some((i, c)) = self.view.cpus().iter().enumerate().find(|(_, c)| c.odd > 0)
        {
            self.warned_odd = true;
            log::warn(&format!(
                "cpu {i}: {} trace events that didn't decode or were in the wrong ring (left out)",
                c.odd
            ));
        }
        self.show();
        Flow::Carry
    }
}

/// The app's exit code: 0 once its window closes; 1 once a failure is
/// logged (no trace grant, no window, a read that failed other than for an
/// offline CPU, whose refused reads mark it off and the app goes on); 2
/// for arguments it doesn't take.
fn run() -> i32 {
    let replay = match lumen_app::args().as_deref() {
        Ok([]) => false,
        Ok([a]) if a == "--replay" => true,
        _ => {
            log::error("usage: xray [--replay]");
            return 2;
        }
    };
    match app(replay) {
        Ok(true) => 0,
        // Its read's failure is logged already.
        Ok(false) => 1,
        Err(e) => lumen_app::report(Err(e)),
    }
}

/// The app, live or frozen: whether it ended well (`false`: a read failed,
/// logged), or what failed and how.
fn app(replay: bool) -> Result<bool, (&'static str, Error)> {
    let time = Time::get().map_err(|e| ("no time grant", e))?;
    let (view, mode, trace) = if replay {
        (replay::view(), Mode::Replay, None)
    } else {
        let trace = Trace::get().map_err(|e| ("no trace grant", e))?;
        let cpus = trace.cpus().map_err(|e| ("no cpu count", e))? as usize;
        (View::new(cpus.min(MAX_SHOWN)), Mode::Live, Some(trace))
    };
    let n = view.cpus().len();
    let lanes = 2 * LANE_Y0 as u32 + n as u32 * LANE_H;
    let tree = TREE_HEAD_H + n as u32 * TREE_ROW_H;
    let height = lanes.max(tree).min(MAX_HEIGHT);
    let desktop = Desktop::get().map_err(|e| ("no desktop grant", e))?;
    let window = desktop.open_window("X-ray", WIDTH, height).map_err(|e| ("no window", e))?;
    let app = Xray::new(view, mode, trace);
    let mut tk = Toolkit::with_area(app, window, Area::new(0, 0, TREE_W, height)).map_err(|e| ("no toolkit", e))?;
    tk.run_until(time, |_: &mut Xray, _: &WindowEvent| Taken::Pass).map_err(|e| ("toolkit", e))?;
    Ok(!tk.app_mut().failed)
}
