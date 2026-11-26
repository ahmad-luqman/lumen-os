//! `perf`: a sampling profiler on the local APIC timer (#205).
//!
//! While a profile runs, each CPU's timer interrupt ([`on_tick`], from
//! `idt`'s dispatch) records what it interrupted: the instruction pointer,
//! the running thread's id and whether that was the CPU's idle thread, into
//! that CPU's [`lumen_perf::Buffer`] (lock-free, never waits, allocates
//! nothing). Otherwise it costs the tick a call and one atomic load. A
//! profile is a number of samples per CPU, not a duration ([`profile`]): it
//! ends when every CPU it covers has taken that many (or the caller gives
//! up, e.g. on `q` or Ctrl-C), so a loaded host makes it slower, never
//! smaller. A CPU whose timer seems to have stopped while the others' go
//! on is called silent ([`lumen_perf::Silence`]): a hint for a person.
//!
//! - `perf top [n] [--samples N] [--threads]`: the busy samples ranked by
//!   function (through a sorted index of the kernel's functions, built for
//!   each ranking: [`function_index`]), or by thread, the top `n` printed
//!   as a table (function or thread, samples, share of the busy samples);
//!   idle samples are counted in the header, in no row. Not in a pipeline.
//! - `perf burn [cpu]` / `perf burn stop`: a thread spinning in
//!   [`burn_loop`] on one CPU (by default the last one with a scheduler),
//!   something known to profile. Every wait (for the thread to start, for
//!   it to end) stops on `q` or Ctrl-C.
//!
//! When WASM apps come (M5, #199), one will show up by function as the
//! interpreter's functions, not its own; `--threads` will name it by its
//! thread.

use crate::arch::MAX_CPUS;
use crate::sched::{self, JoinHandle, SpawnError};
use crate::shell::Ctx;
use crate::shell::sysinfo::{DASH, int, text};
use crate::sync::SpinLock;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use lumen_perf::{Buffer, FnIndex, Ranking, Sample, Silence, Taken, TopArgs};
use lumen_shell::Key;
use lumen_shell::pipe::{self, Align, Column, Kind};
use lumen_trace::ThreadName;

/// The most samples a profile takes per CPU (`perf top --samples`):
/// about 2 s of ticks at 1 kHz. 16 bytes a sample: 256 KiB for 8 CPUs.
pub const CAPACITY: usize = 2048;

/// Each CPU's samples, written only by that CPU's timer interrupt.
static BUFFERS: [Buffer<CAPACITY>; MAX_CPUS] = [const { Buffer::new() }; MAX_CPUS];
/// Whether a profile runs: the timer's fast path (a disarmed buffer would
/// refuse the sample anyway).
static ARMED: AtomicBool = AtomicBool::new(false);
/// Held by the one profile that may run at a time ([`Profiling`]).
static PROFILING: AtomicBool = AtomicBool::new(false);

/// From this CPU's timer interrupt (interrupts off): record the
/// interrupted `rip` if a profile runs. Never waits, allocates nothing.
pub fn on_tick(rip: u64) {
    if !ARMED.load(Ordering::Relaxed) {
        return;
    }
    let (thread, idle) = sched::running_id_here().unwrap_or((0, false));
    BUFFERS[crate::arch::cpu_index()].record(Sample { rip, thread, idle });
}

/// Another profile is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Busy;

/// The one profile running: its buffers are armed until it is dropped,
/// which disarms them and lets the next profile run, however [`profile`]
/// returns.
struct Profiling {
    cpus: Vec<usize>,
}

impl Profiling {
    /// Arm every CPU whose scheduler runs for `want` samples, clamped here
    /// to [`CAPACITY`] (`perf top` refuses more before it gets here): the
    /// guard and the count armed for.
    fn begin(want: usize) -> Result<(Profiling, usize), Busy> {
        if PROFILING.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            return Err(Busy);
        }
        let want = want.min(CAPACITY);
        let cpus: Vec<usize> = (0..MAX_CPUS).filter(|&c| sched::running_on(c)).collect();
        for &cpu in &cpus {
            BUFFERS[cpu].arm(want);
        }
        ARMED.store(true, Ordering::Release);
        Ok((Profiling { cpus }, want))
    }
}

impl Drop for Profiling {
    fn drop(&mut self) {
        ARMED.store(false, Ordering::Release);
        for &cpu in &self.cpus {
            BUFFERS[cpu].disarm();
        }
        PROFILING.store(false, Ordering::Release);
    }
}

/// A finished (or stopped) profile.
pub struct Profile {
    /// The samples asked for per CPU (after the clamp to [`CAPACITY`]).
    want: usize,
    /// The CPUs profiled (those whose scheduler ran at the start) and the
    /// samples each took, in CPU order.
    taken: Vec<Taken>,
    /// The CPUs found silent while it waited.
    silent: Vec<usize>,
    /// Every CPU's samples together, CPU by CPU in `taken`'s order.
    samples: Vec<Sample>,
}

impl Profile {
    /// Whether it ended before every CPU had `want` (stopped).
    #[cfg(feature = "ktest")]
    fn stopped(&self) -> bool {
        !lumen_perf::all_taken(self.taken.iter().map(|t| t.samples), self.want)
    }

    /// Each CPU's samples, with the CPU.
    #[cfg(feature = "ktest")]
    fn per_cpu(&self) -> impl Iterator<Item = (usize, &[Sample])> {
        let mut at = 0;
        self.taken.iter().map(move |t| {
            let s = &self.samples[at..at + t.samples];
            at += t.samples;
            (t.cpu, s)
        })
    }

    /// The idle samples.
    fn idle(&self) -> u64 {
        self.samples.iter().filter(|s| s.idle).count() as u64
    }
}

/// How long [`profile`] waits between looks at the counts (it sleeps, so it
/// takes few samples of its own).
const POLL_NS: u64 = 10_000_000;

/// Take `want` samples (at most [`CAPACITY`]) on every CPU whose scheduler
/// runs, or until `stop` says to give up. `stop` is asked between waits,
/// with the CPUs found silent at that poll (usually none). Blocks the
/// calling thread meanwhile.
pub fn profile(want: usize, mut stop: impl FnMut(&[usize]) -> bool) -> Result<Profile, Busy> {
    let (guard, want) = Profiling::begin(want)?;
    let cpus = &guard.cpus;
    let ticks = || cpus.iter().map(|&c| crate::arch::idt::ticks_on(c)).collect::<Vec<u64>>();
    let mut silence = Silence::new(&ticks());
    loop {
        // One snapshot of the counts per poll.
        let done: Vec<bool> = cpus.iter().map(|&c| BUFFERS[c].progress().0 >= want).collect();
        if done.iter().all(|&d| d) {
            break;
        }
        let silent: Vec<usize> = silence.observe(&ticks(), &done).into_iter().map(|i| cpus[i]).collect();
        if stop(&silent) {
            break;
        }
        crate::time::sleep(POLL_NS);
    }
    let mut samples = Vec::with_capacity(want * cpus.len());
    let mut taken = Vec::with_capacity(cpus.len());
    for &cpu in cpus {
        let before = samples.len();
        BUFFERS[cpu].copy_into(&mut samples);
        taken.push(Taken { cpu, samples: samples.len() - before });
    }
    let silent = silence.silent().into_iter().map(|i| cpus[i]).collect();
    drop(guard);
    Ok(Profile { want, taken, silent, samples })
}

/// The kernel's functions, sorted for binary search: several thousand,
/// filtered and sorted for each ranking (empty without a symbol table).
fn function_index() -> FnIndex<'static> {
    FnIndex::new(crate::symbols::functions())
}

/// A row's function: a symbol's raw name, or the address where none
/// resolves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Func {
    Named(&'static str),
    Unknown(u64),
}

/// The busy samples ranked by function.
fn by_function(p: &Profile, index: &FnIndex<'static>) -> Ranking<Func> {
    lumen_perf::by_function(&p.samples, |rip| index.lookup(rip).map_or(Func::Unknown(rip), |(n, _)| Func::Named(n)))
}

/// `perf`'s entry point.
pub fn command(ctx: &mut Ctx<'_>, args: &[String]) {
    match args {
        [what, rest @ ..] if what == "top" => top(ctx, rest),
        [what, rest @ ..] if what == "burn" => burn_command(ctx, rest),
        _ => ctx.fail("usage: perf top [n] [--samples N] [--threads] | perf burn [cpu|stop]"),
    }
}

/// Whether `key` stops a wait: `q`, Ctrl-C, or the console closing.
fn stops(key: Key) -> bool {
    key == Key::Char('q') || key.interrupts()
}

/// `perf top`: profile, then print the header and the top rows.
fn top(ctx: &mut Ctx<'_>, args: &[String]) {
    if ctx.piped() {
        return ctx.fail("perf top: not in a pipeline (it waits for its samples and stops on q; run it alone)");
    }
    let a = match lumen_perf::parse_top(args, CAPACITY) {
        Ok(a) => a,
        Err(e) => return ctx.fail(format_args!("perf top: {e}")),
    };
    let p = profile(a.samples, |silent| {
        for cpu in silent {
            let _ = writeln!(ctx.out, "perf top: cpu {cpu} has stopped taking samples (interrupts off?); q stops");
        }
        ctx.poll_key().is_some_and(stops)
    });
    match p {
        Ok(p) => {
            let text = render(&p, a);
            let _ = ctx.out.write_str(&text);
        }
        Err(Busy) => ctx.fail("perf top: another profile is running"),
    }
}

/// The header line and the table of `perf top` (by function: a note line
/// between them if there is no symbol table, every row then an address).
fn render(p: &Profile, a: TopArgs) -> String {
    let mut t = String::new();
    let _ = writeln!(t, "{}", lumen_perf::header(p.want, &p.taken, &p.silent, p.idle()));
    let table = if a.threads {
        thread_table(p, a.rows)
    } else {
        let index = function_index();
        if index.is_empty() {
            t.push_str("perf top: no kernel symbol table (see the boot log)\n");
        }
        function_table(p, &index, a.rows)
    };
    table.render(&mut t);
    t
}

fn function_table(p: &Profile, index: &FnIndex<'static>, rows: usize) -> pipe::Table {
    const COLUMNS: [Column; 3] = [
        Column::new("function", Kind::Text, Align::Left, 40),
        Column::new("samples", Kind::Int, Align::Right, 7),
        Column::new("share", Kind::Percent, Align::Right, 5),
    ];
    let r = by_function(p, index);
    let mut t = pipe::Table::new(&COLUMNS);
    for row in r.rows().iter().take(rows) {
        let name = match row.key {
            Func::Named(raw) => alloc::format!("{:#}", rustc_demangle::demangle(raw)),
            Func::Unknown(rip) => alloc::format!("<unknown {rip:#x}>"),
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
    let names: BTreeMap<u64, ThreadName> = sched::threads().into_iter().map(|t| (t.id, t.name)).collect();
    let r = lumen_perf::by_thread(&p.samples);
    let mut t = pipe::Table::new(&COLUMNS);
    for row in r.rows().iter().take(rows) {
        // `-` (absent): the scheduler has no record of the thread any more
        // (it ended and was dropped since its samples).
        let name = names.get(&row.key).map_or(DASH, |n| text(n.as_str()));
        t.push(vec![name, int(row.key), int(row.samples), int(r.share(row))]);
    }
    t
}

/// One `perf burn` thread's flags: each burn has its own, so a stop meant
/// for one can never end (or be undone by) the next.
struct Session {
    /// Set to end its loop.
    stop: AtomicBool,
    /// Its thread's id once it has entered [`burn_loop`] (0: not yet).
    thread: AtomicU64,
}

/// The burn slot: free, a burn running, or one told to stop whose thread
/// hasn't ended yet. A stopping burn keeps its thread's handle here until a
/// stop sees it finished, so a stop that gives up (on a key) leaves the
/// slot reserved and the next stop waits again; no new burn starts
/// meanwhile.
enum Burn {
    Idle,
    Running { cpu: usize, handle: JoinHandle, session: Arc<Session> },
    Stopping { cpu: usize, handle: JoinHandle },
}

static BURNING: SpinLock<Burn> = SpinLock::new(Burn::Idle);

/// Spin until `session.stop`: the hot function `perf burn` profiles. Never
/// inlined, so its samples carry its name; how many turns it took.
#[inline(never)]
fn burn_loop(session: &Session) -> u64 {
    let (id, _) = sched::running_here().expect("a spawned thread runs on a CPU with threads set up");
    session.thread.store(id, Ordering::Release);
    let mut turns = 0u64;
    while !session.stop.load(Ordering::Relaxed) {
        turns = core::hint::black_box(turns.wrapping_add(1));
        core::hint::spin_loop();
    }
    turns
}

/// Why [`start_burn`] refused or gave up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartError {
    /// One is running (on that CPU).
    Running(usize),
    /// One is being stopped (on that CPU): the slot is free once a stop has
    /// seen its thread end.
    Stopping(usize),
    /// That CPU has no scheduler (or doesn't exist).
    NoSuchCpu(usize),
    /// The thread could not reserve its queues or stack; `NotRunning` is
    /// reported as [`StartError::NoSuchCpu`].
    Spawn(SpawnError),
    /// Told to stop waiting before the thread (on that CPU) was in its
    /// loop: it is spawned and stoppable, just not confirmed.
    NotStarted(usize),
}

/// A spawn failure on `cpu` as a [`StartError`].
fn start_error(cpu: usize, e: SpawnError) -> StartError {
    match e {
        SpawnError::NotRunning => StartError::NoSuchCpu(cpu),
        e @ (SpawnError::NoMemory | SpawnError::Stack(_) | SpawnError::CantWait) => StartError::Spawn(e),
    }
}

/// What `perf burn` says for a [`StartError`].
fn start_message(e: StartError) -> String {
    match e {
        StartError::Running(c) => alloc::format!("perf burn: already spinning on cpu {c}"),
        StartError::Stopping(c) => alloc::format!("perf burn: still stopping on cpu {c}; perf burn stop waits for it"),
        StartError::NoSuchCpu(c) => alloc::format!("perf burn: no scheduler runs on cpu {c}"),
        StartError::Spawn(e) => alloc::format!("perf burn: {e}"),
        StartError::NotStarted(c) => {
            alloc::format!(
                "perf burn: cpu {c} hasn't run its thread yet (it spins once it does); perf burn stop stops it"
            )
        }
    }
}

/// Why [`stop_burn`] didn't stop one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopError {
    /// None is running.
    NotRunning,
    /// Told to stop waiting before the thread (on that CPU) ended: it is
    /// told to stop, and the slot stays reserved for the next stop.
    StillStopping(usize),
}

/// The last CPU whose scheduler runs.
pub(crate) fn last_cpu() -> usize {
    (0..MAX_CPUS).rev().find(|&c| sched::running_on(c)).unwrap_or(0)
}

/// Start the burn thread on `cpu` (default: [`last_cpu`]) and wait until
/// it is in [`burn_loop`], or until `stop()` (asked between waits) says to
/// give up: its CPU and thread id.
pub fn start_burn(cpu: Option<usize>, mut stop: impl FnMut() -> bool) -> Result<(usize, u64), StartError> {
    let cpu = cpu.unwrap_or_else(last_cpu);
    if !sched::running_on(cpu) {
        return Err(StartError::NoSuchCpu(cpu));
    }
    // Made before the slot's lock is taken (it allocates); the spawn below
    // allocates under it, which is fine: nothing takes the slot's lock
    // under any other lock.
    let session = Arc::new(Session { stop: AtomicBool::new(false), thread: AtomicU64::new(0) });
    let mut burning = BURNING.lock();
    match *burning {
        Burn::Running { cpu: c, .. } => return Err(StartError::Running(c)),
        Burn::Stopping { cpu: c, .. } => return Err(StartError::Stopping(c)),
        Burn::Idle => {}
    }
    let mine = session.clone();
    let handle = sched::spawn_on(cpu, "perf-burn", move || {
        core::hint::black_box(burn_loop(&mine));
    })
    .map_err(|e| start_error(cpu, e))?;
    *burning = Burn::Running { cpu, handle, session: session.clone() };
    drop(burning);
    // Confirmed before it returns: the next step (a profile) finds it
    // spinning. It is queued on a running scheduler, so it gets its turn
    // (a stop before then still lets it enter the loop, store its id and
    // leave).
    loop {
        let id = session.thread.load(Ordering::Acquire);
        if id != 0 {
            return Ok((cpu, id));
        }
        if stop() {
            return Err(StartError::NotStarted(cpu));
        }
        crate::time::sleep(1_000_000);
    }
}

/// Tell the running burn to stop, marking the slot stopping (a burn
/// already stopping stays so): its CPU.
fn begin_stop() -> Result<usize, StopError> {
    let mut burning = BURNING.lock();
    match core::mem::replace(&mut *burning, Burn::Idle) {
        Burn::Running { cpu, handle, session } => {
            session.stop.store(true, Ordering::Relaxed);
            *burning = Burn::Stopping { cpu, handle };
            Ok(cpu)
        }
        Burn::Stopping { cpu, handle } => {
            *burning = Burn::Stopping { cpu, handle };
            Ok(cpu)
        }
        Burn::Idle => Err(StopError::NotRunning),
    }
}

/// Wait for the stopping burn on `cpu` to end, or until `give_up()` (asked
/// between looks) says to stop waiting; then free the slot. Never blocks on
/// the thread itself: it joins only once the thread has finished, so a CPU
/// that never runs it (stuck with interrupts off) can't hang the caller.
fn finish_stop(cpu: usize, mut give_up: impl FnMut() -> bool) -> Result<usize, StopError> {
    loop {
        {
            let mut burning = BURNING.lock();
            match &*burning {
                Burn::Stopping { handle, .. } if handle.is_finished() => {
                    if let Burn::Stopping { handle, .. } = core::mem::replace(&mut *burning, Burn::Idle) {
                        drop(burning);
                        handle.join(); // returns at once: it has finished
                    }
                    return Ok(cpu);
                }
                Burn::Stopping { .. } => {}
                // Another stop saw it end (and maybe a new burn started).
                Burn::Idle | Burn::Running { .. } => return Ok(cpu),
            }
        }
        if give_up() {
            return Err(StopError::StillStopping(cpu));
        }
        crate::time::sleep(1_000_000);
    }
}

/// Stop the burn thread and wait for it to end, or until `give_up()` says
/// to stop waiting: the CPU it ran on.
pub fn stop_burn(give_up: impl FnMut() -> bool) -> Result<usize, StopError> {
    let cpu = begin_stop()?;
    finish_stop(cpu, give_up)
}

/// `perf burn [cpu]`, `perf burn stop`.
fn burn_command(ctx: &mut Ctx<'_>, args: &[String]) {
    const USAGE: &str = "usage: perf burn [cpu|stop]";
    match args {
        [stop] if stop == "stop" => match stop_burn(|| ctx.poll_key().is_some_and(stops)) {
            Ok(cpu) => {
                let _ = writeln!(ctx.out, "perf burn: stopped on cpu {cpu}");
            }
            Err(StopError::StillStopping(c)) => {
                ctx.fail(format_args!("perf burn: still stopping on cpu {c}; perf burn stop waits again"))
            }
            Err(StopError::NotRunning) => ctx.fail("perf burn: none running"),
        },
        [] | [_] => {
            let cpu = match args.first().map(|a| a.parse::<usize>()) {
                None => None,
                Some(Ok(c)) => Some(c),
                Some(Err(_)) => return ctx.fail(USAGE),
            };
            match start_burn(cpu, || ctx.poll_key().is_some_and(stops)) {
                Ok((cpu, id)) => {
                    let _ = writeln!(ctx.out, "perf burn: spinning on cpu {cpu} (thread perf-burn, id {id})");
                }
                Err(e) => ctx.fail(start_message(e)),
            }
        }
        _ => ctx.fail(USAGE),
    }
}

/// The ktests' bound on a wait: a hang guard, not a claim about speed (as
/// `sched`'s ktests do).
#[cfg(feature = "ktest")]
const TEST_DEADLINE_NS: u64 = 10_000_000_000;

/// A `give_up` for the ktests: true once [`TEST_DEADLINE_NS`] has passed.
#[cfg(feature = "ktest")]
fn test_deadline() -> impl FnMut() -> bool {
    let deadline = crate::time::now_ns() + TEST_DEADLINE_NS;
    move || crate::time::now_ns() > deadline
}

/// The ktests' `profile`: waits for every sample, failing only past
/// [`TEST_DEADLINE_NS`] (silent CPUs are named in the diag then, a hint
/// of why: a busy host can make a healthy CPU look silent for a while).
#[cfg(feature = "ktest")]
fn test_profile(want: usize) -> Result<Profile, &'static str> {
    let mut late = test_deadline();
    let p = profile(want, |_| late()).map_err(|_| "a profile was already running")?;
    if !lumen_perf::all_taken(p.taken.iter().map(|t| t.samples), p.want) {
        crate::ktest::diag(format_args!("taken {:?}, silent CPUs {:?}", p.taken, p.silent));
        return Err("the profile didn't finish before the test's deadline");
    }
    Ok(p)
}

/// The ktests' `start_burn`, bounded by [`TEST_DEADLINE_NS`], its error in
/// the diag.
#[cfg(feature = "ktest")]
fn test_start_burn(cpu: Option<usize>) -> Result<(usize, u64), &'static str> {
    start_burn(cpu, test_deadline()).map_err(|e| {
        crate::ktest::diag(format_args!("{}", start_message(e)));
        "perf burn didn't start"
    })
}

/// The ktests' `stop_burn`, bounded by [`TEST_DEADLINE_NS`].
#[cfg(feature = "ktest")]
fn test_stop_burn() -> Result<usize, &'static str> {
    stop_burn(test_deadline()).map_err(|e| {
        crate::ktest::diag(format_args!("{e:?}"));
        "perf burn didn't stop"
    })
}

crate::ktest!(
    fn samples_land_while_armed_and_stop_when_disarmed() {
        use crate::{kassert, kassert_eq};
        const WANT: usize = 20;
        let p = test_profile(WANT)?;
        kassert!(!p.stopped() && !p.taken.is_empty());
        // Every CPU profiled took exactly what was asked for: no more (the
        // buffer refuses past it), no fewer (it waited).
        kassert!(p.taken.iter().all(|t| t.samples == WANT));
        // Each sample's idle flag says whether its thread was its CPU's
        // idle thread (as the scheduler lists them); the ktest thread
        // sleeps while it waits, so its CPU (at least) idled.
        let threads = sched::threads();
        for (cpu, samples) in p.per_cpu() {
            let idle =
                threads.iter().find(|t| t.name == "idle" && t.cpu == cpu).ok_or("a CPU without an idle thread")?;
            kassert!(samples.iter().all(|s| s.thread != 0 && s.idle == (s.thread == idle.id)));
        }
        kassert!(p.idle() > 0);
        // No profile running: the timer records nothing, even into an
        // armed buffer (the fast path), until a profile arms the timer.
        kassert!(!ARMED.load(Ordering::Relaxed));
        let cpu = crate::arch::cpu_index();
        BUFFERS[cpu].arm(WANT);
        let ticks = crate::arch::idt::ticks_on(cpu);
        let mut late = test_deadline();
        while crate::arch::idt::ticks_on(cpu) < ticks + 3 && !late() {
            crate::time::sleep(1_000_000);
        }
        let ticked = crate::arch::idt::ticks_on(cpu) >= ticks + 3;
        let left = BUFFERS[cpu].progress();
        BUFFERS[cpu].disarm();
        kassert!(ticked);
        kassert_eq!(left, (0, WANT));
        // One at a time.
        PROFILING.store(true, Ordering::Relaxed);
        let second = profile(1, |_| false);
        PROFILING.store(false, Ordering::Relaxed);
        kassert!(second.is_err());
        Ok(())
    }
);

crate::ktest!(
    fn a_profile_stopped_at_once_says_so_and_disarms() {
        use crate::kassert;
        const WANT: usize = 1000;
        let p = profile(WANT, |_| true).map_err(|_| "a profile was already running")?;
        kassert!(p.stopped());
        kassert!(p.taken.iter().all(|t| t.samples <= WANT));
        kassert!(p.samples.len() == p.taken.iter().map(|t| t.samples).sum::<usize>());
        // The guard disarmed everything on the way out.
        kassert!(!ARMED.load(Ordering::Relaxed) && !PROFILING.load(Ordering::Relaxed));
        kassert!(BUFFERS.iter().all(|b| b.progress() == (0, 0)));
        Ok(())
    }
);

crate::ktest!(
    fn a_cpu_stuck_with_interrupts_off_is_called_silent() {
        // The last AP spins with interrupts off (a `run_on` job, bounded)
        // while a profile waits: the profile names it silent, marks it in
        // the header, and disarms. Deterministic: that CPU takes no tick
        // at all until it is let go.
        use crate::{kassert, kassert_eq};
        static PARKED: AtomicBool = AtomicBool::new(false);
        static RELEASE: AtomicBool = AtomicBool::new(false);
        let Some(cpu) = (1..MAX_CPUS).rev().find(|&c| sched::running_on(c)) else {
            crate::ktest::diag(format_args!("no AP: nothing to park"));
            return Ok(());
        };
        PARKED.store(false, Ordering::Relaxed);
        RELEASE.store(false, Ordering::Relaxed);
        crate::smp::run_on_test(cpu, || {
            let _off = crate::arch::irq::push_off();
            PARKED.store(true, Ordering::Release);
            crate::smp::spin_until(crate::time::now_ns() + TEST_DEADLINE_NS, || RELEASE.load(Ordering::Acquire));
        })?;
        if !crate::smp::job_reached(cpu, || PARKED.load(Ordering::Acquire)) {
            return Err("the AP didn't park");
        }
        let mut late = test_deadline();
        let p = profile(CAPACITY, |silent| !silent.is_empty() || late());
        RELEASE.store(true, Ordering::Release);
        let p = p.map_err(|_| "a profile was already running")?;
        kassert_eq!(p.silent, vec![cpu]);
        kassert!(p.stopped());
        let header = lumen_perf::header(p.want, &p.taken, &p.silent, p.idle());
        kassert!(header.contains(&alloc::format!("cpu {cpu}: 0 (silent)")));
        kassert!(!ARMED.load(Ordering::Relaxed) && BUFFERS.iter().all(|b| b.progress() == (0, 0)));
        Ok(())
    }
);

crate::ktest!(
    fn a_spinning_thread_ranks_first_by_function_and_by_thread() {
        use crate::{kassert, kassert_eq};
        let (_, id) = test_start_burn(None)?;
        let p = test_profile(50);
        test_stop_burn()?;
        let p = p?;
        let by_thread = lumen_perf::by_thread(&p.samples);
        kassert_eq!(by_thread.rows().first().map(|r| r.key), Some(id));
        let by_fn = by_function(&p, &function_index());
        let top = match by_fn.rows().first().map(|r| r.key) {
            Some(Func::Named(name)) => name,
            _ => "",
        };
        kassert!(top.contains("burn_loop"));
        Ok(())
    }
);

crate::ktest!(
    fn tables_show_the_rows_asked_for() {
        // A made-up profile with two busy functions and two busy threads:
        // each table shows as many rows as asked for, up to what there is.
        use crate::kassert_eq;
        let hot = burn_loop as fn(&Session) -> u64 as usize as u64;
        let warm = on_tick as fn(u64) as usize as u64;
        let s = |rip, thread| Sample { rip, thread, idle: false };
        let p = Profile {
            want: 5,
            taken: vec![Taken { cpu: 0, samples: 5 }],
            silent: Vec::new(),
            samples: vec![s(hot, 7), s(hot, 7), s(hot + 1, 7), s(warm, 8), Sample { rip: warm, thread: 1, idle: true }],
        };
        let index = function_index();
        for rows in [1, 2, 3] {
            kassert_eq!(function_table(&p, &index, rows).rows().len(), rows.min(2));
            kassert_eq!(thread_table(&p, rows).rows().len(), rows.min(2));
        }
        let r = by_function(&p, &index);
        kassert_eq!(r.rows().iter().map(|r| r.samples).collect::<Vec<_>>(), vec![3, 1]);
        Ok(())
    }
);

crate::ktest!(
    fn a_burn_being_stopped_keeps_its_slot_until_its_thread_has_ended() {
        use crate::{kassert, kassert_eq};
        // The burn on this CPU: with interrupts off here it can't run, so it
        // can't end, and every step below is deterministic.
        let here = crate::arch::cpu_index();
        let (cpu, first) = test_start_burn(Some(here))?;
        let (gave_up, refused) = {
            let _off = crate::arch::irq::push_off();
            let cpu = begin_stop().map_err(|_| "nothing to stop")?;
            // A stop that gives up leaves the slot reserved: a start is
            // still refused.
            (finish_stop(cpu, || true), start_burn(None, || true))
        };
        kassert_eq!(gave_up, Err(StopError::StillStopping(cpu)));
        kassert_eq!(refused, Err(StartError::Stopping(cpu)));
        // The next stop waits again, and frees it once the thread ended.
        kassert_eq!(test_stop_burn(), Ok(cpu));
        kassert_eq!(stop_burn(|| true), Err(StopError::NotRunning));
        // A new burn has its own flags: it runs (the old stop doesn't end
        // it) until its own stop.
        let (_, second) = test_start_burn(None)?;
        kassert!(second != first);
        kassert!(matches!(*BURNING.lock(), Burn::Running { .. }));
        test_stop_burn()?;
        Ok(())
    }
);

crate::ktest!(
    fn a_start_told_to_stop_waiting_leaves_a_stoppable_burn() {
        use crate::kassert_eq;
        // Spawned on this CPU with interrupts off here: it can't have run
        // when the wait gives up.
        let here = crate::arch::cpu_index();
        let started = {
            let _off = crate::arch::irq::push_off();
            start_burn(Some(here), || true)
        };
        kassert_eq!(started, Err(StartError::NotStarted(here)));
        kassert_eq!(test_stop_burn(), Ok(here));
        Ok(())
    }
);

crate::ktest!(
    fn start_errors_say_what_went_wrong() {
        use crate::kassert_eq;
        use crate::mm::stack::StackError;
        kassert_eq!(start_error(3, SpawnError::NotRunning), StartError::NoSuchCpu(3));
        let no_stack = SpawnError::Stack(StackError::OutOfMemory);
        kassert_eq!(start_error(3, no_stack), StartError::Spawn(no_stack));
        kassert_eq!(start_error(3, SpawnError::CantWait), StartError::Spawn(SpawnError::CantWait));
        kassert_eq!(start_error(3, SpawnError::NoMemory), StartError::Spawn(SpawnError::NoMemory));
        let says = |e| start_message(e);
        kassert_eq!(says(StartError::Spawn(no_stack)), "perf burn: no stack: out of memory");
        kassert_eq!(says(StartError::NoSuchCpu(7)), "perf burn: no scheduler runs on cpu 7");
        kassert_eq!(says(StartError::Running(1)), "perf burn: already spinning on cpu 1");
        kassert_eq!(says(StartError::Stopping(1)), "perf burn: still stopping on cpu 1; perf burn stop waits for it");
        kassert_eq!(
            says(StartError::NotStarted(2)),
            "perf burn: cpu 2 hasn't run its thread yet (it spins once it does); perf burn stop stops it"
        );
        Ok(())
    }
);

crate::ktest!(
    fn the_function_index_finds_what_a_scan_finds() {
        use crate::{kassert, kassert_eq};
        // Kernel functions' own addresses and a few bytes in: the index and
        // the crash reporter's scan (`symbols::resolve`) agree, and both
        // find them.
        let index = function_index();
        let fns = [burn_loop as fn(&Session) -> u64 as usize as u64, on_tick as fn(u64) as usize as u64];
        for addr in fns.into_iter().flat_map(|a| [a, a + 1, a + 7]) {
            let scanned = crate::symbols::resolve(addr).map(|l| (l.name, l.offset));
            kassert!(scanned.is_some());
            kassert_eq!(index.lookup(addr), scanned);
        }
        Ok(())
    }
);
