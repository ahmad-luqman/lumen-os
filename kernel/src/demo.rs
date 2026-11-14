//! The shell's demos: `mandelbrot`, drawn by every CPU at once, and
//! `life`, Conway's Game of Life. The arithmetic, the band dealing and the
//! rules are `lumen-demo`'s (tested on the host); this draws them on a
//! [`Canvas`].
//!
//! While a demo holds the canvas the terminal draws nothing, so what the
//! viewer needs to know (the prompt, why it stopped) is drawn on the canvas
//! too, in a status line at its bottom, as well as written to the console
//! (for serial, and the tests).
//!
//! # Drawing on every CPU
//!
//! The image is cut into bands of [`BAND`] rows dealt round-robin to the
//! CPUs taking part (`lumen_demo::bands`). The calling thread draws slot 0
//! on its own CPU; every other CPU whose scheduler runs gets a thread pinned
//! to it (`sched::spawn_on`; threads never migrate) holding a clone of the
//! `Arc<Canvas>`. [`draw_mandelbrot`] joins them all before it returns, so
//! no thread draws once the command drops the canvas (whose drop draws the
//! terminal again), and the last reference is the command's. A CPU that
//! gets no thread (spawning failed) has its slot drawn by the caller.
//! (Threads, not `smp::run_on` jobs: a job is a bare `fn()`, so reaching
//! the canvas from one would take a static raw pointer and `unsafe`.)

#![cfg_attr(feature = "ktest", allow(dead_code))] // the test kernel runs no shell

use crate::arch::{self, MAX_CPUS};
use crate::sched::{self, SpawnError};
use crate::shell::Ctx;
use crate::term::{Canvas, CanvasError};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::num::NonZeroUsize;
use core::sync::atomic::{AtomicUsize, Ordering};
use lumen_demo::bands;
use lumen_demo::life::{self, Life, PATTERNS};
use lumen_demo::mandel::{self, View};

/// Rows per band.
const BAND: NonZeroUsize = NonZeroUsize::new(8).unwrap();
/// Iterations before a point counts as inside the set.
const MAX_ITER: u32 = 64;
/// The status line's height: a row of the terminal's font, at the bottom
/// of the canvas.
const STATUS_H: usize = crate::fb::CONSOLE_SIZE.val();
/// The status line's colours.
const STATUS_FG: u32 = 0x8a93a6;
const STATUS_BG: u32 = 0x0b0f17;

/// Draw `text` as the canvas's status line (its bottom [`STATUS_H`] rows).
fn status_line(canvas: &Canvas, text: &str) {
    let y = canvas.height().saturating_sub(STATUS_H);
    canvas.fill_rect(0, y, canvas.width(), STATUS_H, STATUS_BG);
    canvas.text(8, y, text, STATUS_FG, STATUS_BG);
}

/// "1 CPU", "4 CPUs".
struct Cpus(usize);

impl core::fmt::Display for Cpus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} CPU{}", self.0, if self.0 == 1 { "" } else { "s" })
    }
}

/// A finished drawing.
#[derive(Clone, Copy, Debug)]
pub struct Drawn {
    /// How long it took, in nanoseconds (spawning and joining included).
    pub ns: u64,
    /// Rows each CPU drew, by CPU index.
    pub rows: [usize; MAX_CPUS],
    /// The CPU that called [`draw_mandelbrot`] (it draws the slots of CPUs
    /// that got no thread).
    pub caller: usize,
    /// CPUs that should have drawn but got no thread, and why.
    pub no_thread: [Option<SpawnError>; MAX_CPUS],
    /// CPUs online without a running scheduler (no thread can run there),
    /// left out.
    pub skipped: [bool; MAX_CPUS],
}

impl Drawn {
    /// CPUs that drew at least one row.
    pub fn cpus(&self) -> usize {
        self.rows.iter().filter(|&&r| r > 0).count()
    }
}

/// What one drawing draws: the top-left `width` x `height` pixels of the
/// canvas, in `slots` slots.
#[derive(Clone, Copy)]
struct Job {
    view: View,
    width: usize,
    height: usize,
    slots: NonZeroUsize,
}

impl Job {
    /// Draw slot `slot`'s bands on `canvas`, adding the rows to this CPU's
    /// count in `rows`.
    fn draw(&self, canvas: &Canvas, slot: usize, rows: &[AtomicUsize; MAX_CPUS]) {
        let mut drawn = 0;
        for band in bands::rows(slot, self.slots, self.height, BAND) {
            for y in band {
                for x in 0..self.width {
                    canvas.put(x, y, mandel::color(self.view.escape_at(x, y), self.view.max_iter));
                }
                drawn += 1;
            }
        }
        rows[arch::cpu_index()].fetch_add(drawn, Ordering::Relaxed);
    }
}

/// Draw `view` on the top-left `width` x `height` pixels of `canvas`, on
/// this CPU alone or (`all`) on every CPU whose scheduler runs; returns
/// once every CPU has finished (see the module comment).
pub fn draw_mandelbrot(canvas: &Arc<Canvas>, view: View, width: usize, height: usize, all: bool) -> Drawn {
    let start = crate::time::now_ns();
    let caller = arch::cpu_index();
    let mut skipped = [false; MAX_CPUS];
    let mut others = Vec::new();
    if all {
        for cpu in (0..MAX_CPUS).filter(|&c| c != caller) {
            if sched::running_on(cpu) {
                others.push(cpu);
            } else if crate::smp::online_lapic_id(cpu).is_some() {
                skipped[cpu] = true;
            }
        }
    }
    let job = Job { view, width, height, slots: NonZeroUsize::MIN.saturating_add(others.len()) };
    let rows: Arc<[AtomicUsize; MAX_CPUS]> = Arc::new([const { AtomicUsize::new(0) }; MAX_CPUS]);
    let mut threads = Vec::with_capacity(others.len());
    let mut no_thread = [None; MAX_CPUS];
    let mut orphans = Vec::new();
    for (i, &cpu) in others.iter().enumerate() {
        let (canvas, rows) = (Arc::clone(canvas), Arc::clone(&rows));
        match sched::spawn_on(cpu, "mandelbrot", move || job.draw(&canvas, i + 1, &rows)) {
            Ok(thread) => threads.push(thread),
            Err(e) => {
                no_thread[cpu] = Some(e);
                orphans.push(i + 1);
            }
        }
    }
    job.draw(canvas, 0, &rows);
    for slot in orphans {
        job.draw(canvas, slot, &rows);
    }
    for thread in threads {
        thread.join();
    }
    let rows = core::array::from_fn(|c| rows[c].load(Ordering::Relaxed));
    Drawn { ns: crate::time::now_ns() - start, rows, caller, no_thread, skipped }
}

/// Say on the console which CPUs got no thread or were left out of `drawn`
/// (their rows were drawn by the caller); how many were.
fn report_left_out(ctx: &mut Ctx<'_>, drawn: &Drawn) -> usize {
    let mut left_out = 0;
    for cpu in 0..MAX_CPUS {
        if let Some(e) = drawn.no_thread[cpu] {
            let _ = writeln!(
                ctx.out,
                "mandelbrot: cpu{cpu} got no thread ({e}); its rows were drawn by cpu{}",
                drawn.caller
            );
            left_out += 1;
        }
        if drawn.skipped[cpu] {
            let _ = writeln!(ctx.out, "mandelbrot: cpu{cpu} is online but runs no scheduler; it drew nothing");
            left_out += 1;
        }
    }
    left_out
}

/// `mandelbrot`: the set drawn by the calling CPU (the boot CPU, where the
/// shells run) alone, then by every CPU whose scheduler runs, with both
/// times; it stays until a key.
pub fn mandelbrot(ctx: &mut Ctx<'_>, args: &[String]) {
    if let Some(arg) = args.first() {
        let _ = writeln!(ctx.out, "mandelbrot: takes no arguments (got '{arg}')\nusage: mandelbrot");
        return;
    }
    let canvas = match ctx.canvas() {
        Ok(canvas) => Arc::new(canvas),
        Err(e) => {
            let _ = writeln!(ctx.out, "mandelbrot: {e}");
            return;
        }
    };
    let (w, h) = (canvas.width(), canvas.height().saturating_sub(STATUS_H));
    if w == 0 || h == 0 {
        let _ =
            writeln!(ctx.out, "mandelbrot: the screen is too small ({}x{} pixels)", canvas.width(), canvas.height());
        return;
    }
    let view = View::fit(w, h, MAX_ITER);
    canvas.clear(0);
    status_line(&canvas, "Mandelbrot set: drawing on one CPU");
    let one = draw_mandelbrot(&canvas, view, w, h, false);
    status_line(&canvas, "Mandelbrot set: drawing on every CPU");
    canvas.fill_rect(0, 0, w, h, 0);
    let all = draw_mandelbrot(&canvas, view, w, h, true);
    if !canvas.is_live() {
        report_left_out(ctx, &all);
        let _ = writeln!(ctx.out, "mandelbrot: stopped: {}", CanvasError::Stopped);
        return;
    }
    let ms = |d: &Drawn| d.ns / 1_000_000;
    let tenths = one.ns * 10 / all.ns.max(1);
    let _ = writeln!(
        ctx.out,
        "mandelbrot: {w}x{h}, {} {} ms, {} {} ms ({}.{}x)",
        Cpus(one.cpus()),
        ms(&one),
        Cpus(all.cpus()),
        ms(&all),
        tenths / 10,
        tenths % 10
    );
    let mut per_cpu = String::new();
    for (cpu, rows) in all.rows.iter().enumerate().filter(|&(_, &r)| r > 0) {
        let _ = write!(per_cpu, " cpu{cpu}={rows}");
    }
    let _ = writeln!(ctx.out, "mandelbrot: rows drawn by each CPU:{per_cpu}");
    let left_out = match report_left_out(ctx, &all) {
        0 => String::new(),
        n => format!(" ({n} left out: see the console)"),
    };
    let line =
        format!("Mandelbrot set, drawn by {}{left_out}  -  press a key on {}", Cpus(all.cpus()), ctx.console_name());
    status_line(&canvas, &line);
    let _ = writeln!(ctx.out, "mandelbrot: press a key");
    ctx.wait_key();
}

/// Pixels per cell side.
const CELL: usize = 8;
/// Live and dead cells' colours.
const ALIVE: u32 = 0x7ee787;
const DEAD: u32 = STATUS_BG;
/// The most generations a second when running until a key.
const LIFE_HZ: u64 = 20;
/// The pattern `life` runs without one named.
const DEFAULT_PATTERN: &str = "gun";
const LIFE_USAGE: &str = "usage: life [pattern] [generations]";

/// `life [pattern] [generations]`: the pattern (default `gun`) where it
/// starts best ([`life::Place`]); with a count it steps that many
/// generations as fast as it can (a key ends it early, and says so) and the
/// frame stays until a key, else it steps at most 20 a second until a key.
/// A status line under the grid shows the generation and the population.
pub fn life(ctx: &mut Ctx<'_>, args: &[String]) {
    if args.len() > 2 {
        let _ = writeln!(ctx.out, "life: too many arguments\n{LIFE_USAGE}");
        return;
    }
    let name = args.first().map_or(DEFAULT_PATTERN, String::as_str);
    let Some(pattern) = life::pattern(name) else {
        let _ = writeln!(ctx.out, "life: unknown pattern '{name}'\n{LIFE_USAGE}");
        for p in PATTERNS {
            let _ = writeln!(ctx.out, "  {:<12}{}", p.name, p.about);
        }
        return;
    };
    let count = match args.get(1) {
        None => None,
        Some(arg) => match arg.parse::<u64>() {
            Ok(n) => Some(n),
            Err(_) => {
                let _ = writeln!(ctx.out, "life: '{arg}' is not a number of generations\n{LIFE_USAGE}");
                return;
            }
        },
    };
    let canvas = match ctx.canvas() {
        Ok(canvas) => canvas,
        Err(e) => {
            let _ = writeln!(ctx.out, "life: {e}");
            return;
        }
    };
    let (cols, rows) = (canvas.width() / CELL, canvas.height().saturating_sub(STATUS_H) / CELL);
    let (pw, ph) = pattern.size();
    if cols < pw || rows < ph {
        let _ = writeln!(
            ctx.out,
            "life: the screen is too small for {} ({cols}x{rows} cells, it needs {pw}x{ph})",
            pattern.name
        );
        return;
    }
    let mut grid = Life::new(cols, rows);
    let (x0, y0) = pattern.origin(grid.width(), grid.height());
    grid.place(pattern, x0, y0);
    canvas.clear(DEAD);
    let cell = |x: usize, y: usize, alive: bool| {
        canvas.fill_rect(x * CELL, y * CELL, CELL - 1, CELL - 1, if alive { ALIVE } else { DEAD });
    };
    for (x, y) in pattern.cells() {
        let (x, y) = (x0.saturating_add(x), y0.saturating_add(y));
        cell(x, y, grid.get(x, y));
    }
    let _ = writeln!(ctx.out, "life: {} on {}x{} cells; a key quits", pattern.name, grid.width(), grid.height());
    let mut line = String::new();
    let mut show = |grid: &Life, tail: &str| {
        line.clear();
        let _ = write!(line, "{}  generation {}  alive {}{tail}", pattern.name, grid.generation(), grid.population());
        status_line(&canvas, &line);
    };
    /// Why the loop ended.
    enum End {
        Count,
        Key,
        Lost,
    }
    let end = loop {
        if !canvas.is_live() {
            break End::Lost;
        }
        if count.is_some_and(|n| grid.generation() >= n) {
            break End::Count;
        }
        show(&grid, "");
        if ctx.poll_key().is_some() {
            break End::Key;
        }
        grid.step(cell);
        if count.is_none() {
            crate::time::sleep(1_000_000_000 / LIFE_HZ);
        }
    };
    let _ = writeln!(ctx.out, "life: generation {}, {} alive", grid.generation(), grid.population());
    match end {
        End::Count => {
            show(&grid, &format!("  -  press a key on {}", ctx.console_name()));
            let _ = writeln!(ctx.out, "life: press a key");
            ctx.wait_key();
        }
        End::Key => {
            if let Some(n) = count {
                let _ = writeln!(ctx.out, "life: a key ended it before generation {n}");
            }
        }
        End::Lost => {
            let _ = writeln!(ctx.out, "life: stopped: {}", CanvasError::Stopped);
        }
    }
}

/// For [`every_cpu_rendered_rows`]: draw a `width` x `height` view alone,
/// then `rounds` times on every CPU, checking each parallel drawing right
/// as it returns.
#[cfg(feature = "ktest")]
fn draws_alike(canvas: &Arc<Canvas>, width: usize, height: usize, rounds: usize) -> crate::ktest::TestResult {
    use crate::{kassert, kassert_eq};
    let view = View::fit(width, height, 24);
    let row_sum =
        |y: usize| (0..width).fold(0u64, |a, x| a.wrapping_mul(31).wrapping_add(u64::from(canvas.pixel(x, y))));
    let sums = || (0..height).map(row_sum).collect::<Vec<u64>>();
    canvas.clear(0);
    let one = draw_mandelbrot(canvas, view, width, height, false);
    let alone = sums();
    kassert_eq!(one.cpus(), 1);
    kassert_eq!(one.rows[arch::cpu_index()], height);
    // Every row was drawn (none of them is all background, black).
    kassert!(alone.iter().all(|&s| s != 0));
    let online: Vec<usize> = (0..MAX_CPUS).filter(|&c| crate::smp::online_lapic_id(c).is_some()).collect();
    kassert!(online.len() > 1);
    for round in 0..rounds {
        canvas.clear(0);
        let all = draw_mandelbrot(canvas, view, width, height, true);
        // Checked at once: a thread not yet joined would still hold its
        // clone, or not have drawn (or counted) its rows yet.
        let held = Arc::strong_count(canvas);
        let same = sums() == alone;
        let rows: Vec<usize> = online.iter().map(|&c| all.rows[c]).collect();
        let left_out: Vec<(usize, Option<SpawnError>, bool)> = (0..MAX_CPUS)
            .filter(|&c| all.no_thread[c].is_some() || all.skipped[c])
            .map(|c| (c, all.no_thread[c], all.skipped[c]))
            .collect();
        if round == 0 || held != 1 || !same || all.cpus() != online.len() {
            crate::ktest::diag(format_args!(
                "{width}x{height} round {round}: alone {} us, {} {} us, rows per CPU {rows:?}, \
                 {held} references, same pixels {same}, left out (cpu, no thread, no scheduler) {left_out:?}",
                one.ns / 1000,
                Cpus(all.cpus()),
                all.ns / 1000
            ));
        }
        kassert_eq!(held, 1);
        kassert!(same);
        kassert_eq!(all.cpus(), online.len());
        kassert!(rows.iter().all(|&r| r > 0));
        kassert_eq!(all.rows.iter().sum::<usize>(), height);
    }
    Ok(())
}

crate::ktest!(
    fn every_cpu_rendered_rows() {
        // Drawn by every online CPU, each draws at least one band, all the
        // rows are drawn once, and the pixels match this CPU drawing alone;
        // when it returns no thread holds the canvas any more. A short image
        // (a band per CPU: a thread has barely started when the caller is
        // done, so a missing join shows), 20 times; and a height that ends
        // in a part band.
        let canvas = match crate::term::canvas(crate::term::Owner::new()) {
            Ok(canvas) => Arc::new(canvas),
            Err(e) => {
                crate::ktest::diag(format_args!("no canvas: {e}"));
                return Err("no canvas");
            }
        };
        let width = canvas.width().min(160);
        // As many bands as CPUs online (whatever -smp says).
        let cpus = (0..MAX_CPUS).filter(|&c| crate::smp::online_lapic_id(c).is_some()).count();
        draws_alike(&canvas, width, cpus * BAND.get(), 20)?;
        draws_alike(&canvas, width, 197, 1)
    }
);
