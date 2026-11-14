//! The shell's demos: `mandelbrot`, drawn by every CPU at once, and
//! `life`, Conway's Game of Life. The arithmetic and the rules are
//! `lumen-demo`'s (tested on the host); this draws them on a
//! [`Canvas`](crate::term::Canvas).
//!
//! # Drawing on every CPU
//!
//! The canvas is split into bands of [`BAND`] rows, dealt round-robin to
//! the CPUs taking part (band `i` to slot `i % n`; interleaving spreads the
//! costly rows near the set's middle). The boot CPU is slot 0 and draws its
//! bands itself; each other online CPU gets [`helper`] through
//! [`smp::run_on`]. A job is a plain `fn()`, so what it draws comes through
//! statics: the canvas, the view and each CPU's slot, written before
//! [`SEALED`] is set (helpers spin until it is), and only while the drawing
//! command holds the canvas (one at a time), which it keeps until every
//! helper is done ([`PENDING`]). A CPU that never takes its job has it taken
//! back and its bands drawn by the boot CPU.
//!
//! [`draw_mandelbrot`] returns only once every helper has finished drawing
//! and [`CANVAS`] is cleared: dropping the canvas redraws the terminal, and
//! a helper still drawing would paint over it (or draw through a dangling
//! pointer). The ktest checks both after a drawing.

#![cfg_attr(feature = "ktest", allow(dead_code))] // the test kernel runs no shell

use crate::arch::{self, MAX_CPUS};
use crate::shell::Ctx;
use crate::smp;
use crate::sync::SpinLock;
use crate::term::{self, Canvas};
use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};
use lumen_demo::life::{self, Life, PATTERNS};
use lumen_demo::mandel::{self, View};

/// Rows per band.
const BAND: usize = 8;
/// Iterations before a point counts as inside the set.
const MAX_ITER: u32 = 64;
/// How long the boot CPU waits for the other CPUs to take their jobs
/// before drawing their bands itself (a CPU that took its job is waited
/// for until it finishes).
const TAKE_TIMEOUT_NS: u64 = 10_000_000_000;

/// What a parallel drawing draws (set before [`SEALED`]).
#[derive(Clone, Copy)]
struct Job {
    view: View,
    width: usize,
    height: usize,
    /// Slots taking part (the boot CPU's included).
    slots: usize,
}

static JOB: SpinLock<Option<Job>> = SpinLock::new(None);
/// The canvas drawn on, while a drawing is under way.
static CANVAS: AtomicPtr<Canvas> = AtomicPtr::new(core::ptr::null_mut());
/// Each CPU's slot (0: the boot CPU's, or none for the others).
static SLOT: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];
/// Set once [`JOB`] and [`SLOT`] are complete.
static SEALED: AtomicBool = AtomicBool::new(false);
/// Helpers given a job that haven't finished it.
static PENDING: AtomicUsize = AtomicUsize::new(0);
/// Rows each CPU drew in the last drawing.
static ROWS: [AtomicUsize; MAX_CPUS] = [const { AtomicUsize::new(0) }; MAX_CPUS];

/// A finished drawing.
#[derive(Clone, Copy, Debug)]
pub struct Drawn {
    /// CPUs that took part.
    pub cpus: usize,
    pub ns: u64,
}

/// Draw `view` on the top-left `width` x `height` pixels of `canvas`, on
/// the boot CPU alone or (`all`) on every online CPU. Called on the boot
/// CPU (the other CPUs' slots are dealt from it).
pub fn draw_mandelbrot(canvas: &Canvas, view: View, width: usize, height: usize, all: bool) -> Drawn {
    let start = crate::time::now_ns();
    for rows in &ROWS {
        rows.store(0, Ordering::Relaxed);
    }
    SEALED.store(false, Ordering::Relaxed);
    CANVAS.store(core::ptr::from_ref(canvas).cast_mut(), Ordering::Relaxed);
    let mut helpers = [0usize; MAX_CPUS];
    let mut slots = 1;
    if all {
        for (cpu, slot) in SLOT.iter().enumerate().skip(1) {
            if smp::online_lapic_id(cpu).is_none() {
                continue;
            }
            slot.store(slots, Ordering::Relaxed);
            PENDING.fetch_add(1, Ordering::Relaxed);
            // (Published by SEALED's release below, before the helper reads
            // them; it spins until then.)
            match smp::run_on(cpu, helper) {
                Ok(()) => {
                    helpers[slots] = cpu;
                    slots += 1;
                }
                Err(e) => {
                    PENDING.fetch_sub(1, Ordering::Relaxed);
                    slot.store(0, Ordering::Relaxed);
                    log::warn!(target: "demo", "cpu {cpu} draws no bands: it {e}");
                }
            }
        }
    }
    *JOB.lock() = Some(Job { view, width, height, slots });
    SEALED.store(true, Ordering::Release);
    draw_slot(0);
    // Wait for the helpers; take back the jobs of those that never started.
    let deadline = crate::time::now_ns() + TAKE_TIMEOUT_NS;
    while PENDING.load(Ordering::Acquire) != 0 && crate::time::now_ns() < deadline {
        crate::sched::yield_now();
        core::hint::spin_loop();
    }
    for (slot, &cpu) in helpers.iter().enumerate().take(slots).skip(1) {
        if PENDING.load(Ordering::Acquire) != 0 && smp::withdraw(cpu) {
            log::warn!(target: "demo", "cpu {cpu} didn't take its job; drawing its bands here");
            draw_slot(slot);
            PENDING.fetch_sub(1, Ordering::Release);
        }
    }
    while PENDING.load(Ordering::Acquire) != 0 {
        crate::sched::yield_now();
        core::hint::spin_loop();
    }
    CANVAS.store(core::ptr::null_mut(), Ordering::Relaxed);
    for slot in &SLOT {
        slot.store(0, Ordering::Relaxed);
    }
    Drawn { cpus: slots, ns: crate::time::now_ns() - start }
}

/// Another CPU's share of a drawing (its idle thread runs it).
fn helper() {
    while !SEALED.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    let slot = SLOT[arch::cpu_index()].load(Ordering::Relaxed);
    if slot != 0 {
        draw_slot(slot);
    }
    PENDING.fetch_sub(1, Ordering::Release);
}

/// Draw slot `slot`'s bands, counting the rows for this CPU.
fn draw_slot(slot: usize) {
    let Some(job) = *JOB.lock() else { return };
    // SAFETY: CANVAS points to the drawing command's canvas from before
    // SEALED was set until every helper finished (PENDING is 0), and slots
    // are drawn only in between.
    let Some(canvas) = (unsafe { CANVAS.load(Ordering::Relaxed).as_ref() }) else { return };
    let mut rows = 0;
    for band in (slot..job.height.div_ceil(BAND)).step_by(job.slots) {
        for y in band * BAND..((band + 1) * BAND).min(job.height) {
            for x in 0..job.width {
                canvas.put(x, y, mandel::color(job.view.escape_at(x, y), job.view.max_iter));
            }
            rows += 1;
        }
    }
    ROWS[arch::cpu_index()].fetch_add(rows, Ordering::Relaxed);
}

/// What a command prints when the screen can't be had.
const NO_SCREEN: &str = "the screen isn't free (no terminal, or another program is drawing on it)";

/// `mandelbrot`: the set drawn by the boot CPU alone, then by every CPU,
/// with both times; it stays until a key.
pub fn mandelbrot(ctx: &mut Ctx<'_>, args: &[String]) {
    if !args.is_empty() {
        let _ = writeln!(ctx.out, "usage: mandelbrot");
        return;
    }
    let Some(canvas) = term::canvas() else {
        let _ = writeln!(ctx.out, "mandelbrot: {NO_SCREEN}");
        return;
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
    let _ = writeln!(ctx.out, "mandelbrot: press a key");
    ctx.wait_key();
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
    let Some(canvas) = term::canvas() else {
        let _ = writeln!(ctx.out, "life: {NO_SCREEN}");
        return;
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
    let quit = loop {
        line.clear();
        let _ = write!(line, "{}  generation {}  alive {}", pattern.name, grid.generation(), grid.population());
        canvas.fill_rect(0, status_y, canvas.width(), STATUS_H, DEAD);
        canvas.text(CELL, status_y, &line, STATUS, DEAD);
        if count.is_some_and(|n| grid.generation() >= n) {
            break false;
        }
        if ctx.poll_key().is_some() {
            break true;
        }
        grid.step(cell);
        if count.is_none() {
            crate::time::sleep(1_000_000_000 / LIFE_HZ);
        }
    };
    let _ = writeln!(ctx.out, "life: generation {}, {} alive", grid.generation(), grid.population());
    if !quit {
        let _ = writeln!(ctx.out, "life: press a key");
        ctx.wait_key();
    }
}

crate::ktest!(
    fn every_cpu_rendered_rows() {
        // Drawn by every online CPU, each draws at least one band, all the
        // rows are drawn once, and the pixels match the boot CPU drawing
        // alone (a small view with few iterations, to keep it quick).
        use crate::{kassert, kassert_eq};
        let canvas = term::canvas().ok_or("no terminal")?;
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
        kassert_eq!(ROWS[0].load(Ordering::Relaxed), h);
        canvas.clear(0);
        let all = draw_mandelbrot(&canvas, view, w, h, true);
        let online: alloc::vec::Vec<usize> = (0..MAX_CPUS).filter(|&c| smp::online_lapic_id(c).is_some()).collect();
        let rows: alloc::vec::Vec<usize> = online.iter().map(|&c| ROWS[c].load(Ordering::Relaxed)).collect();
        crate::ktest::diag(format_args!(
            "{w}x{h}: 1 CPU {} us, {} CPUs {} us, rows per CPU {rows:?}",
            one.ns / 1000,
            all.cpus,
            all.ns / 1000
        ));
        kassert_eq!(all.cpus, online.len());
        kassert!(online.len() > 1);
        kassert!(rows.iter().all(|&r| r > 0));
        kassert_eq!(rows.iter().sum::<usize>(), h);
        kassert_eq!(sum(), alone);
        // Every helper finished and the canvas is no longer reachable from
        // them before it is dropped.
        kassert_eq!(PENDING.load(Ordering::Acquire), 0);
        kassert!(CANVAS.load(Ordering::Acquire).is_null());
        Ok(())
    }
);
