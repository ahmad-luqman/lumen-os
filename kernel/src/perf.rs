//! `perf`: a sampling profiler on the local APIC timer (#205).
//!
//! While a profile runs, each CPU's timer interrupt ([`on_tick`], from
//! `idt`'s dispatch) records what it interrupted: the instruction pointer,
//! the running thread's id and whether that was the CPU's idle thread, into
//! that CPU's [`lumen_perf::Buffer`] (lock-free, never waits, allocates
//! nothing). Otherwise it costs the tick one atomic load. A profile is a
//! number of samples per CPU, not a duration ([`profile`]): it ends when
//! every CPU it covers has taken that many (or the caller gives up, e.g.
//! on Ctrl-C), so a loaded host makes it slower, never smaller.
//!
//! - `perf top [n] [--samples N] [--threads]`: the busy samples ranked by
//!   function (symbolized with `symbols`), or by thread, the top `n`
//!   printed as a table (function or thread, samples, share of the busy
//!   samples); idle samples are counted in the header, in no row.
//! - `perf burn [cpu]` / `perf burn stop`: a thread spinning in
//!   [`burn_loop`] on one CPU (by default the last one with a scheduler),
//!   something known to profile.
//!
//! A WASM app (M5) shows up by function as the interpreter's functions,
//! not its own; `--threads` names it by its thread.

use crate::arch::MAX_CPUS;
use crate::sched::{self, JoinHandle};
use crate::shell::Ctx;
use crate::shell::sysinfo::{DASH, int, text};
use crate::sync::SpinLock;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use lumen_perf::{Buffer, Ranking, Sample, TopArgs};
use lumen_shell::pipe::{self, Align, Column, Kind};

/// The most samples a profile takes per CPU (`perf top --samples`):
/// about 2 s of ticks at 1 kHz. 16 bytes a sample: 256 KiB for 8 CPUs.
pub const CAPACITY: usize = 2048;

/// Each CPU's samples, written only by that CPU's timer interrupt.
static BUFFERS: [Buffer<CAPACITY>; MAX_CPUS] = [const { Buffer::new() }; MAX_CPUS];
/// Whether a profile runs: the timer's fast path (a disarmed buffer would
/// refuse the sample anyway).
static ARMED: AtomicBool = AtomicBool::new(false);
/// Held by the one profile that may run at a time.
static PROFILING: AtomicBool = AtomicBool::new(false);

/// From CPU `cpu`'s timer interrupt (interrupts off): record the
/// interrupted `rip` if a profile runs. Never waits, allocates nothing.
pub fn on_tick(cpu: usize, rip: u64) {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    let (thread, idle) = sched::running_id_here().unwrap_or((0, false));
    if let Some(b) = BUFFERS.get(cpu) {
        b.record(Sample { rip, thread, idle });
    }
}

/// Another profile is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Busy;

/// A finished (or stopped) profile.
pub struct Profile {
    /// The samples asked for per CPU.
    pub want: usize,
    /// The CPUs profiled (those whose scheduler ran at the start) and the
    /// samples each took.
    pub taken: Vec<(usize, usize)>,
    /// Every CPU's samples together.
    pub samples: Vec<Sample>,
    /// Whether the caller stopped it before every CPU had `want`.
    pub stopped: bool,
}

/// How long [`profile`] waits between looks at the counts (it sleeps, so it
/// takes few samples of its own).
const POLL_NS: u64 = 10_000_000;

/// Take `want` samples (at most [`CAPACITY`]) on every CPU whose scheduler
/// runs, or until `stop()` says to give up (asked between waits). Blocks
/// the calling thread meanwhile.
pub fn profile(want: usize, mut stop: impl FnMut() -> bool) -> Result<Profile, Busy> {
    if PROFILING.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return Err(Busy);
    }
    let cpus: Vec<usize> = (0..MAX_CPUS).filter(|&c| sched::running_on(c)).collect();
    let want = want.min(CAPACITY);
    for &cpu in &cpus {
        BUFFERS[cpu].arm(want);
    }
    ARMED.store(true, Ordering::Release);
    let full = |cpus: &[usize]| cpus.iter().all(|&c| BUFFERS[c].progress().0 >= want);
    let mut stopped = false;
    while !full(&cpus) {
        if stop() {
            stopped = true;
            break;
        }
        crate::time::sleep(POLL_NS);
    }
    let mut samples = Vec::with_capacity(want * cpus.len());
    let mut taken = Vec::with_capacity(cpus.len());
    for &cpu in &cpus {
        let before = samples.len();
        BUFFERS[cpu].copy_into(&mut samples);
        taken.push((cpu, samples.len() - before));
    }
    ARMED.store(false, Ordering::Release);
    for &cpu in &cpus {
        BUFFERS[cpu].disarm();
    }
    PROFILING.store(false, Ordering::Release);
    Ok(Profile { want, taken, samples, stopped })
}

/// A sample's function: its symbol's raw name (`None`: nothing resolves).
fn function_of(rip: u64) -> Option<&'static str> {
    crate::symbols::resolve(rip).map(|l| l.name)
}

/// The busy samples ranked by function.
pub fn by_function(p: &Profile) -> Ranking<Option<&'static str>> {
    lumen_perf::by_function(&p.samples, function_of)
}

/// `perf`'s entry point.
pub fn command(ctx: &mut Ctx<'_>, args: &[String]) {
    match args {
        [what, rest @ ..] if what == "top" => top(ctx, rest),
        [what, rest @ ..] if what == "burn" => burn_command(ctx, rest),
        _ => ctx.fail("usage: perf top [n] [--samples N] [--threads] | perf burn [cpu|stop]"),
    }
}

/// `perf top`: profile, then print the header and the top rows.
fn top(ctx: &mut Ctx<'_>, args: &[String]) {
    let a = match lumen_perf::parse_top(args, CAPACITY) {
        Ok(a) => a,
        Err(e) => return ctx.fail(format_args!("perf top: {e}")),
    };
    let p =
        match profile(a.samples, || ctx.poll_key().is_some_and(|k| k == lumen_shell::Key::Char('q') || k.interrupts()))
        {
            Ok(p) => p,
            Err(Busy) => return ctx.fail("perf top: another profile is running"),
        };
    let text = render(&p, a);
    let _ = ctx.out.write_str(&text);
}

/// The header line and the table of `perf top`.
fn render(p: &Profile, a: TopArgs) -> String {
    let mut t = String::new();
    let per_cpu = if p.stopped {
        let counts: Vec<String> = p.taken.iter().map(|(c, n)| alloc::format!("cpu {c}: {n}")).collect();
        alloc::format!("stopped at {} of {} samples per CPU", counts.join(", "), p.want)
    } else {
        alloc::format!("{} samples per CPU", p.want)
    };
    let n = p.taken.len();
    let table = if a.threads { thread_table(p, a.rows) } else { function_table(p, a.rows) };
    let idle = p.samples.iter().filter(|s| s.idle).count();
    let _ = writeln!(
        t,
        "perf top: {per_cpu} on {n} CPU{}: {} samples, {idle} idle, {} busy",
        if n == 1 { "" } else { "s" },
        p.samples.len(),
        p.samples.len() - idle,
    );
    table.render(&mut t);
    t
}

fn function_table(p: &Profile, rows: usize) -> pipe::Table {
    const COLUMNS: [Column; 3] = [
        Column::new("function", Kind::Text, Align::Left, 40),
        Column::new("samples", Kind::Int, Align::Right, 7),
        Column::new("share", Kind::Percent, Align::Right, 5),
    ];
    let r = by_function(p);
    let mut t = pipe::Table::new(&COLUMNS);
    for row in r.rows.iter().take(rows) {
        let name = match row.key {
            Some(raw) => alloc::format!("{:#}", rustc_demangle::demangle(raw)),
            None => String::from("<unknown>"),
        };
        t.push(vec![text(&name), int(row.samples), int(r.share(row))]);
    }
    t
}

fn thread_table(p: &Profile, rows: usize) -> pipe::Table {
    const COLUMNS: [Column; 4] = [
        Column::new("thread", Kind::Text, Align::Left, 16),
        Column::new("id", Kind::Int, Align::Right, 2),
        Column::new("samples", Kind::Int, Align::Right, 7),
        Column::new("share", Kind::Percent, Align::Right, 5),
    ];
    let names: BTreeMap<u64, &'static str> = sched::threads().into_iter().map(|t| (t.id, t.name)).collect();
    let r = lumen_perf::by_thread(&p.samples);
    let mut t = pipe::Table::new(&COLUMNS);
    for row in r.rows.iter().take(rows) {
        // A thread gone since its samples: `-` (absent).
        let name = names.get(&row.key).map_or(DASH, |n| text(n));
        t.push(vec![name, int(row.key), int(row.samples), int(r.share(row))]);
    }
    t
}

/// Set to end the burn thread's loop.
static BURN_STOP: AtomicBool = AtomicBool::new(false);
/// The burn thread's id once it has entered [`burn_loop`] (0: not yet).
static BURN_THREAD: AtomicU64 = AtomicU64::new(0);
/// The burn thread, while one runs.
static BURNING: SpinLock<Option<(usize, JoinHandle)>> = SpinLock::new(None);

/// Spin until [`BURN_STOP`]: the hot function `perf burn` profiles. Never
/// inlined, so its samples carry its name; how many turns it took.
#[inline(never)]
pub fn burn_loop() -> u64 {
    let id = sched::running_here().map_or(u64::MAX, |(id, _)| id);
    BURN_THREAD.store(id, Ordering::Release);
    let mut turns = 0u64;
    while !BURN_STOP.load(Ordering::Relaxed) {
        turns = core::hint::black_box(turns.wrapping_add(1));
        core::hint::spin_loop();
    }
    turns
}

/// Why [`start_burn`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BurnError {
    /// One is already running (on that CPU).
    Running(usize),
    /// That CPU has no scheduler.
    NoSuchCpu,
}

/// The last CPU whose scheduler runs.
fn last_cpu() -> usize {
    (0..MAX_CPUS).rev().find(|&c| sched::running_on(c)).unwrap_or(0)
}

/// Start the burn thread on `cpu` (default: [`last_cpu`]) and wait until
/// it is in [`burn_loop`]: its CPU and thread id.
pub fn start_burn(cpu: Option<usize>) -> Result<(usize, u64), BurnError> {
    let cpu = cpu.unwrap_or_else(last_cpu);
    let mut burning = BURNING.lock();
    if let Some((c, _)) = burning.as_ref() {
        return Err(BurnError::Running(*c));
    }
    BURN_STOP.store(false, Ordering::Relaxed);
    BURN_THREAD.store(0, Ordering::Relaxed);
    let handle = sched::spawn_on(cpu, "perf-burn", || {
        core::hint::black_box(burn_loop());
    })
    .map_err(|_| BurnError::NoSuchCpu)?;
    *burning = Some((cpu, handle));
    drop(burning);
    // Confirmed before it returns: the next step (a profile) finds it
    // spinning. It is queued on a running scheduler, so it gets its turn.
    loop {
        let id = BURN_THREAD.load(Ordering::Acquire);
        if id != 0 {
            return Ok((cpu, id));
        }
        crate::time::sleep(1_000_000);
    }
}

/// Stop the burn thread and wait for it to end: the CPU it ran on, if one
/// ran.
pub fn stop_burn() -> Option<usize> {
    let (cpu, handle) = BURNING.lock().take()?;
    BURN_STOP.store(true, Ordering::Relaxed);
    handle.join();
    Some(cpu)
}

/// `perf burn [cpu]`, `perf burn stop`.
fn burn_command(ctx: &mut Ctx<'_>, args: &[String]) {
    match args {
        [stop] if stop == "stop" => match stop_burn() {
            Some(cpu) => {
                let _ = writeln!(ctx.out, "perf burn: stopped on cpu {cpu}");
            }
            None => ctx.fail("perf burn: none running"),
        },
        [] | [_] => {
            let cpu = match args.first().map(|a| a.parse::<usize>()) {
                None => None,
                Some(Ok(c)) => Some(c),
                Some(Err(_)) => return ctx.fail("usage: perf burn [cpu|stop]"),
            };
            match start_burn(cpu) {
                Ok((cpu, id)) => {
                    let _ = writeln!(ctx.out, "perf burn: spinning on cpu {cpu} (thread perf-burn, id {id})");
                }
                Err(BurnError::Running(c)) => ctx.fail(format_args!("perf burn: already spinning on cpu {c}")),
                Err(BurnError::NoSuchCpu) => ctx.fail("perf burn: no scheduler runs on that CPU"),
            }
        }
        _ => ctx.fail("usage: perf burn [cpu|stop]"),
    }
}

crate::ktest!(
    fn samples_land_while_armed_and_stop_when_disarmed() {
        use crate::{kassert, kassert_eq};
        const WANT: usize = 20;
        let p = profile(WANT, || false).map_err(|_| "a profile was already running")?;
        kassert!(!p.stopped && !p.taken.is_empty());
        // Every CPU profiled took exactly what was asked for: no more (the
        // buffer refuses past it), no fewer (it waited).
        kassert!(p.taken.iter().all(|&(_, n)| n == WANT));
        kassert_eq!(p.samples.len(), WANT * p.taken.len());
        // No profile running: the timer records nothing, even into an
        // armed buffer (the fast path), until a profile arms the timer.
        kassert!(!ARMED.load(Ordering::Relaxed));
        let cpu = crate::arch::cpu_index();
        BUFFERS[cpu].arm(WANT);
        let ticks = crate::arch::idt::ticks_on(cpu);
        while crate::arch::idt::ticks_on(cpu) < ticks + 3 {
            crate::time::sleep(1_000_000);
        }
        let left = BUFFERS[cpu].progress();
        BUFFERS[cpu].disarm();
        kassert_eq!(left, (0, WANT));
        // One at a time.
        PROFILING.store(true, Ordering::Relaxed);
        let second = profile(1, || false);
        PROFILING.store(false, Ordering::Relaxed);
        kassert!(second.is_err());
        Ok(())
    }
);

crate::ktest!(
    fn a_spinning_thread_ranks_first_by_function_and_by_thread() {
        use crate::{kassert, kassert_eq};
        let (_, id) = start_burn(None).map_err(|_| "perf burn didn't start")?;
        let p = profile(50, || false);
        stop_burn();
        let p = p.map_err(|_| "a profile was already running")?;
        let by_thread = lumen_perf::by_thread(&p.samples);
        kassert_eq!(by_thread.rows.first().map(|r| r.key), Some(id));
        let by_fn = by_function(&p);
        let top = by_fn.rows.first().and_then(|r| r.key).unwrap_or("");
        kassert!(top.contains("burn_loop"));
        kassert_eq!(by_fn.rows.iter().map(|r| r.samples).sum::<u64>(), by_fn.busy());
        Ok(())
    }
);
