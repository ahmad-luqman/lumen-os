//! The shell's demos: `mandelbrot`, drawn by every CPU at once, and
//! `life`, Conway's Game of Life. The arithmetic and the rules are
//! `lumen-demo`'s (tested on the host); this draws them on a
//! [`Canvas`](crate::term::Canvas).
//!
//! # Drawing on every CPU
//!
//! The canvas is split into bands of [`BAND`] rows, dealt round-robin to
//! the CPUs taking part (band `i` to slot `i % n`; interleaving spreads the
//! costly rows near the set's middle). The calling thread draws slot 0 on
//! its own CPU; every other CPU whose scheduler runs gets a thread pinned to
//! it (`sched::spawn_on`; threads never migrate) holding a clone of the
//! `Arc<Canvas>`. [`draw_mandelbrot`] joins them all before it returns, so
//! no thread draws once the command drops the canvas (whose drop draws the
//! terminal again) and the last reference is the command's. A CPU that
//! gets no thread (spawning failed) has its slot drawn by the caller.
//!
//! (The issue named `smp::run_on`, whose jobs are a bare `fn()` run on an
//! idle CPU's idle thread: reaching the canvas through it needs statics, a
//! raw pointer and a take-back timeout. Pinned threads draw the same bands
//! on the same CPUs with none of that.)

#![cfg_attr(feature = "ktest", allow(dead_code))] // the test kernel runs no shell

use crate::arch::{self, MAX_CPUS};
use crate::sched;
use crate::shell::Ctx;
use crate::term::Canvas;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};
use lumen_demo::life::{self, Life, PATTERNS};
use lumen_demo::mandel::{self, View};

/// Rows per band.
const BAND: usize = 8;
/// Iterations before a point counts as inside the set.
const MAX_ITER: u32 = 64;

/// A finished drawing.
#[derive(Clone, Copy, Debug)]
pub struct Drawn {
    /// CPUs that drew (each drew at least its slot's bands).
    pub cpus: usize,
    pub ns: u64,
    /// Rows each CPU drew, by CPU index.
    pub rows: [usize; MAX_CPUS],
}

/// What one drawing draws: the top-left `width` x `height` pixels of the
/// canvas, in `slots` slots.
#[derive(Clone, Copy)]
struct Job {
    view: View,
    width: usize,
    height: usize,
    slots: usize,
}

impl Job {
    /// Draw slot `slot`'s bands on `canvas`, adding the rows to this CPU's
    /// count in `rows`.
    fn draw(&self, canvas: &Canvas, slot: usize, rows: &[AtomicUsize; MAX_CPUS]) {
        let mut drawn = 0;
        for band in (slot..self.height.div_ceil(BAND)).step_by(self.slots) {
            for y in band * BAND..((band + 1) * BAND).min(self.height) {
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
    let here = arch::cpu_index();
    let others: Vec<usize> =
        if all { (0..MAX_CPUS).filter(|&c| c != here && sched::running_on(c)).collect() } else { Vec::new() };
    let job = Job { view, width, height, slots: others.len() + 1 };
    let rows: Arc<[AtomicUsize; MAX_CPUS]> = Arc::new([const { AtomicUsize::new(0) }; MAX_CPUS]);
    let mut threads = Vec::with_capacity(others.len());
    let mut orphans = Vec::new();
    for (i, &cpu) in others.iter().enumerate() {
        let (canvas, rows) = (Arc::clone(canvas), Arc::clone(&rows));
        match sched::spawn_on(cpu, "mandelbrot", move || job.draw(&canvas, i + 1, &rows)) {
            Ok(thread) => threads.push(thread),
            Err(e) => {
                log::warn!(target: "demo", "no thread on cpu {cpu} ({e}); drawing its bands here");
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
    Drawn { cpus: rows.iter().filter(|&&r| r > 0).count(), ns: crate::time::now_ns() - start, rows }
}

/// `mandelbrot`: the set drawn by the boot CPU alone, then by every CPU,
/// with both times; it stays until a key.
pub fn mandelbrot(ctx: &mut Ctx<'_>, args: &[String]) {
    if !args.is_empty() {
        let _ = writeln!(ctx.out, "usage: mandelbrot");
        return;
    }
    let canvas = match ctx.canvas() {
        Ok(canvas) => Arc::new(canvas),
        Err(e) => {
            let _ = writeln!(ctx.out, "mandelbrot: {e}");
            return;
        }
    };
    let (w, h) = (canvas.width(), canvas.height());
    let view = View::fit(w, h, MAX_ITER);
    canvas.clear(0);
    let one = draw_mandelbrot(&canvas, view, w, h, false);
    canvas.clear(0);
    let all = draw_mandelbrot(&canvas, view, w, h, true);
    let ms = |d: Drawn| d.ns / 1_000_000;
    let tenths = one.ns * 10 / all.ns.max(1);
    let _ = writeln!(
        ctx.out,
        "mandelbrot: {w}x{h}, 1 CPU {} ms, {} CPUs {} ms ({}.{}x)",
        ms(one),
        all.cpus,
        ms(all),
        tenths / 10,
        tenths % 10
    );
    let mut per_cpu = String::new();
    for (cpu, rows) in all.rows.iter().enumerate().filter(|&(_, &r)| r > 0) {
        let _ = write!(per_cpu, " cpu{cpu}={rows}");
    }
    let _ = writeln!(ctx.out, "mandelbrot: rows drawn by each CPU:{per_cpu}");
    if canvas.is_live() {
        let _ = writeln!(ctx.out, "mandelbrot: press a key");
        ctx.wait_key();
    } else {
        let _ = writeln!(ctx.out, "mandelbrot: the screen was taken back");
    }
}

/// Pixels per cell side.
const CELL: usize = 8;
/// Live and dead cells' colours, and the status line's.
const ALIVE: u32 = 0x7ee787;
const DEAD: u32 = 0x0b0f17;
const STATUS: u32 = 0x8a93a6;
/// The status line's height (the terminal's font), below the grid.
const STATUS_H: usize = 16;
/// Generations a second when running until a key.
const LIFE_HZ: u64 = 20;

/// `life [pattern] [generations]`: the pattern (default `gun`) in the
/// grid's top-left area; it runs until a key or, given a count, for exactly
/// that many generations (as fast as it can), then stays until a key. A
/// status line under the grid shows the generation and the population.
pub fn life(ctx: &mut Ctx<'_>, args: &[String]) {
    let name = args.first().map_or("gun", String::as_str);
    let count = match args.get(1).map(|n| n.parse::<u64>()) {
        None => None,
        Some(Ok(n)) => Some(n),
        Some(Err(_)) => {
            let _ = writeln!(ctx.out, "usage: life [pattern] [generations]");
            return;
        }
    };
    let Some(pattern) = (args.len() <= 2).then(|| life::pattern(name)).flatten() else {
        let _ = writeln!(ctx.out, "usage: life [pattern] [generations]");
        for p in PATTERNS {
            let _ = writeln!(ctx.out, "  {:<12}{}", p.name, p.about);
        }
        return;
    };
    let canvas = match ctx.canvas() {
        Ok(canvas) => canvas,
        Err(e) => {
            let _ = writeln!(ctx.out, "life: {e}");
            return;
        }
    };
    let mut grid = Life::new(canvas.width() / CELL, canvas.height().saturating_sub(STATUS_H) / CELL);
    grid.place(pattern, 4, 4);
    canvas.clear(DEAD);
    let cell = |x: usize, y: usize, alive: bool| {
        canvas.fill_rect(x * CELL, y * CELL, CELL - 1, CELL - 1, if alive { ALIVE } else { DEAD });
    };
    for (x, y) in pattern.cells() {
        cell(x + 4, y + 4, grid.get(x + 4, y + 4));
    }
    let _ = writeln!(ctx.out, "life: {} on {}x{} cells; any key quits", pattern.name, grid.width(), grid.height());
    let status_y = grid.height() * CELL;
    let mut line = String::new();
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
        line.clear();
        let _ = write!(line, "{}  generation {}  alive {}", pattern.name, grid.generation(), grid.population());
        canvas.fill_rect(0, status_y, canvas.width(), STATUS_H, DEAD);
        canvas.text(CELL, status_y, &line, STATUS, DEAD);
        if count.is_some_and(|n| grid.generation() >= n) {
            break End::Count;
        }
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
            let _ = writeln!(ctx.out, "life: press a key");
            ctx.wait_key();
        }
        End::Key => {}
        End::Lost => {
            let _ = writeln!(ctx.out, "life: stopped: the screen was taken back");
        }
    }
}

crate::ktest!(
    fn every_cpu_rendered_rows() {
        // Drawn by every online CPU, each draws at least one band, all the
        // rows are drawn once, and the pixels match this CPU drawing alone
        // (a small view with few iterations, to keep it quick). When it
        // returns, no CPU's thread holds the canvas any more.
        use crate::{kassert, kassert_eq};
        let canvas = Arc::new(crate::term::canvas(crate::term::Owner::new()).map_err(|_| "no canvas")?);
        let (w, h) = (canvas.width().min(320), canvas.height().min(200));
        let view = View::fit(w, h, 24);
        let sum = || {
            (0..h).fold(0u64, |acc, y| {
                (0..w).fold(acc, |a, x| a.wrapping_mul(31).wrapping_add(u64::from(canvas.pixel(x, y))))
            })
        };
        canvas.clear(0);
        let one = draw_mandelbrot(&canvas, view, w, h, false);
        let alone = sum();
        kassert_eq!(one.cpus, 1);
        kassert_eq!(one.rows[arch::cpu_index()], h);
        canvas.clear(0);
        let all = draw_mandelbrot(&canvas, view, w, h, true);
        let online: Vec<usize> = (0..MAX_CPUS).filter(|&c| crate::smp::online_lapic_id(c).is_some()).collect();
        let rows: Vec<usize> = online.iter().map(|&c| all.rows[c]).collect();
        crate::ktest::diag(format_args!(
            "{w}x{h}: 1 CPU {} us, {} CPUs {} us, rows per CPU {rows:?}",
            one.ns / 1000,
            all.cpus,
            all.ns / 1000
        ));
        kassert!(online.len() > 1);
        kassert_eq!(all.cpus, online.len());
        kassert!(rows.iter().all(|&r| r > 0));
        kassert_eq!(all.rows.iter().sum::<usize>(), h);
        kassert_eq!(sum(), alone);
        kassert_eq!(Arc::strong_count(&canvas), 1);
        Ok(())
    }
);
