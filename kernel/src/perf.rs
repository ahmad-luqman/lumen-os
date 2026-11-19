//! `perf`: a sampling profiler on the local APIC timer (#205).
//!
//! While a profile runs, each CPU's timer interrupt ([`on_tick`], from
//! `idt`'s dispatch) records what it interrupted: the instruction pointer,
//! the running thread's id and whether that was the CPU's idle thread, into
//! that CPU's [`lumen_perf::Buffer`] (lock-free, never waits, allocates
//! nothing). Otherwise it costs the tick one atomic load. A profile is a
//! number of samples per CPU, not a duration ([`profile`]): it ends when
//! every CPU it covers has taken that many (or the caller gives up, e.g.
//! on Ctrl-C), so a loaded host makes it slower, never smaller. A CPU whose
//! timer stops ticking while the others' go on is called silent
//! ([`lumen_perf::Silence`]) so the caller can say so and give up.
//!
//! - `perf top [n] [--samples N] [--threads]`: the busy samples ranked by
//!   function (symbolized through a cached, sorted index of the kernel's
//!   functions: [`function_index`]), or by thread, the top `n` printed as a
//!   table (function or thread, samples, share of the busy samples); idle
//!   samples are counted in the header, in no row. Not in a pipeline.
//! - `perf burn [cpu]` / `perf burn stop`: a thread spinning in
//!   [`burn_loop`] on one CPU (by default the last one with a scheduler),
//!   something known to profile.
//!
//! When WASM apps come (M5, #199), one will show up by function as the
//! interpreter's functions, not its own; `--threads` will name it by its
//! thread.

use crate::arch::MAX_CPUS;
use crate::sched::{self, JoinHandle, SpawnError};
use crate::shell::Ctx;
use crate::shell::sysinfo::{DASH, int, text};
use crate::sync::SpinLock;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::Write;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};
use lumen_perf::{Buffer, FnIndex, Ranking, Sample, Silence, TopArgs};
use lumen_shell::Key;
use lumen_shell::pipe::{self, Align, Column, Kind};

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
    /// Arm every CPU whose scheduler runs for `want` samples, at most
    /// [`CAPACITY`] (the one clamp): the guard and the count armed for.
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
    pub want: usize,
    /// The CPUs profiled (those whose scheduler ran at the start) and the
    /// samples each took.
    pub taken: Vec<(usize, usize)>,
    /// The CPUs found silent while it waited.
    pub silent: Vec<usize>,
    /// Every CPU's samples together, CPU by CPU in `taken`'s order.
    pub samples: Vec<Sample>,
}

impl Profile {
    /// Whether it ended before every CPU had `want` (stopped).
    #[cfg(feature = "ktest")]
    pub fn stopped(&self) -> bool {
        !lumen_perf::all_taken(self.taken.iter().map(|&(_, n)| n), self.want)
    }

    /// Each CPU's samples, with the CPU.
    #[cfg(feature = "ktest")]
    pub fn per_cpu(&self) -> impl Iterator<Item = (usize, &[Sample])> {
        let mut at = 0;
        self.taken.iter().map(move |&(cpu, n)| {
            let s = &self.samples[at..at + n];
            at += n;
            (cpu, s)
        })
    }

    /// The idle samples.
    pub fn idle(&self) -> u64 {
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
    let mut silence = Silence::new(cpus.len());
    let progress = || cpus.iter().map(|&c| BUFFERS[c].progress().0);
    while !lumen_perf::all_taken(progress(), want) {
        let ticks: Vec<u64> = cpus.iter().map(|&c| crate::arch::idt::ticks_on(c)).collect();
        let done: Vec<bool> = progress().map(|n| n >= want).collect();
        let silent: Vec<usize> = silence.observe(&ticks, &done).into_iter().map(|i| cpus[i]).collect();
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
        taken.push((cpu, samples.len() - before));
    }
    let silent = silence.silent().into_iter().map(|i| cpus[i]).collect();
    drop(guard);
    Ok(Profile { want, taken, silent, samples })
}

/// The kernel's functions, sorted (built on first use, about 10k entries,
/// and kept: a `Box` leaked on purpose, never freed or written again).
static INDEX: AtomicPtr<FnIndex<'static>> = AtomicPtr::new(ptr::null_mut());

/// The cached function index (empty without a symbol table). Two first
/// uses at once both build it; the first to publish wins.
fn function_index() -> &'static FnIndex<'static> {
    let current = INDEX.load(Ordering::Acquire);
    if !current.is_null() {
        // SAFETY: a non-null pointer was published (Release) from
        // `Box::into_raw` below and is never freed or written again; a
        // shared reference on any thread is sound (`FnIndex` holds only
        // `&'static str`s and integers: `Sync`).
        return unsafe { &*current };
    }
    let ours = Box::into_raw(Box::new(FnIndex::new(crate::symbols::functions())));
    match INDEX.compare_exchange(ptr::null_mut(), ours, Ordering::AcqRel, Ordering::Acquire) {
        // SAFETY: `ours` came from `Box::into_raw` and is now published,
        // never to be freed.
        Ok(_) => unsafe { &*ours },
        Err(theirs) => {
            // SAFETY: `ours` was never published, so this is its only owner.
            drop(unsafe { Box::from_raw(ours) });
            // SAFETY: `theirs` was published by the winner from
            // `Box::into_raw` and is never freed.
            unsafe { &*theirs }
        }
    }
}

/// The busy samples ranked by function: each address's symbol's raw name
/// (`None`: nothing resolves).
pub fn by_function(p: &Profile) -> Ranking<Option<&'static str>> {
    let index = function_index();
    lumen_perf::by_function(&p.samples, |rip| index.lookup(rip).map(|(name, _)| name))
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
        // Its records would be a table the filters can't read (a header
        // first), and it waits for its samples.
        return ctx.fail("perf top: not in a pipeline");
    }
    let a = match lumen_perf::parse_top(args, CAPACITY) {
        Ok(a) => a,
        Err(e) => return ctx.fail(format_args!("perf top: {e}")),
    };
    let p = profile(a.samples, |silent| {
        for cpu in silent {
            let _ = writeln!(ctx.out, "perf top: cpu {cpu} has taken no samples (interrupts off?); q stops");
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

/// The header line and the table of `perf top`.
fn render(p: &Profile, a: TopArgs) -> String {
    let mut t = String::new();
    let _ = writeln!(t, "{}", lumen_perf::header(p.want, &p.taken, &p.silent, p.idle()));
    let table = if a.threads { thread_table(p, a.rows) } else { function_table(p, a.rows) };
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
        // `-` (absent): no record of the thread any more (gone since its
        // samples), or id 0 (a tick before its CPU's threads were set up).
        let name = names.get(&row.key).map_or(DASH, |n| text(n));
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

/// The burn slot: free, a burn running, or one being stopped (reserved
/// until its thread has been joined, so no new burn starts meanwhile).
enum Burn {
    Idle,
    Running { cpu: usize, handle: JoinHandle, session: Arc<Session> },
    Stopping(usize),
}

static BURNING: SpinLock<Burn> = SpinLock::new(Burn::Idle);

/// Spin until `session.stop`: the hot function `perf burn` profiles. Never
/// inlined, so its samples carry its name; how many turns it took.
#[inline(never)]
fn burn_loop(session: &Session) -> u64 {
    let id = sched::running_here().map_or(u64::MAX, |(id, _)| id);
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
    /// One is being stopped (on that CPU): the slot is free once it has
    /// ended.
    Stopping(usize),
    /// That CPU has no scheduler (or doesn't exist).
    NoSuchCpu(usize),
    /// The thread couldn't be made (no stack, say).
    Spawn(SpawnError),
    /// Told to stop waiting before the thread (on that CPU) was in its
    /// loop: it is spawned and stoppable, just not confirmed.
    NotStarted(usize),
}

/// Why [`stop_burn`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopError {
    /// One is already being stopped (on that CPU).
    Stopping(usize),
    /// None is running.
    NotRunning,
}

/// The last CPU whose scheduler runs.
fn last_cpu() -> usize {
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
        Burn::Stopping(c) => return Err(StartError::Stopping(c)),
        Burn::Idle => {}
    }
    let mine = session.clone();
    let handle = sched::spawn_on(cpu, "perf-burn", move || {
        core::hint::black_box(burn_loop(&mine));
    })
    .map_err(|e| match e {
        SpawnError::NotRunning => StartError::NoSuchCpu(cpu),
        e @ (SpawnError::Stack(_) | SpawnError::CantWait) => StartError::Spawn(e),
    })?;
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

/// A stop under way: the slot says [`Burn::Stopping`] until
/// [`finish_stop`] has joined the thread.
struct Stop {
    cpu: usize,
    handle: JoinHandle,
}

/// Mark the running burn as stopping and tell it to stop.
fn begin_stop() -> Result<Stop, StopError> {
    let mut burning = BURNING.lock();
    match core::mem::replace(&mut *burning, Burn::Idle) {
        Burn::Running { cpu, handle, session } => {
            *burning = Burn::Stopping(cpu);
            drop(burning);
            session.stop.store(true, Ordering::Relaxed);
            Ok(Stop { cpu, handle })
        }
        Burn::Stopping(c) => {
            *burning = Burn::Stopping(c);
            Err(StopError::Stopping(c))
        }
        Burn::Idle => Err(StopError::NotRunning),
    }
}

/// Wait for the stopped burn's thread to end (not under the slot's lock:
/// a join blocks, and can't be interrupted), then free the slot: the CPU
/// it ran on.
fn finish_stop(stop: Stop) -> usize {
    stop.handle.join();
    *BURNING.lock() = Burn::Idle;
    stop.cpu
}

/// Stop the burn thread and wait for it to end: the CPU it ran on. The wait
/// can't be interrupted; it ends as soon as the thread sees its flag (at
/// its next turn round the loop, once its CPU runs it).
pub fn stop_burn() -> Result<usize, StopError> {
    begin_stop().map(finish_stop)
}

/// `perf burn [cpu]`, `perf burn stop`.
fn burn_command(ctx: &mut Ctx<'_>, args: &[String]) {
    const USAGE: &str = "usage: perf burn [cpu|stop]";
    match args {
        [stop] if stop == "stop" => match stop_burn() {
            Ok(cpu) => {
                let _ = writeln!(ctx.out, "perf burn: stopped on cpu {cpu}");
            }
            Err(StopError::Stopping(c)) => ctx.fail(format_args!("perf burn: already stopping on cpu {c}")),
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
                Err(StartError::Running(c)) => ctx.fail(format_args!("perf burn: already spinning on cpu {c}")),
                Err(StartError::Stopping(c)) => ctx.fail(format_args!("perf burn: still stopping on cpu {c}")),
                Err(StartError::NoSuchCpu(c)) => ctx.fail(format_args!("perf burn: no scheduler runs on cpu {c}")),
                Err(StartError::Spawn(e)) => ctx.fail(format_args!("perf burn: {e}")),
                Err(StartError::NotStarted(c)) => ctx
                    .fail(format_args!("perf burn: its thread on cpu {c} hasn't started yet; perf burn stop ends it")),
            }
        }
        _ => ctx.fail(USAGE),
    }
}

/// The ktests' bound on a wait: a hang guard, not a claim about speed (as
/// `sched`'s ktests do).
#[cfg(feature = "ktest")]
const TEST_DEADLINE_NS: u64 = 10_000_000_000;

/// The ktests' `profile`: gives up on a silent CPU or past
/// [`TEST_DEADLINE_NS`], saying which.
#[cfg(feature = "ktest")]
fn test_profile(want: usize) -> Result<Profile, &'static str> {
    let deadline = crate::time::now_ns() + TEST_DEADLINE_NS;
    let mut late = false;
    let p = profile(want, |silent| {
        late = crate::time::now_ns() > deadline;
        !silent.is_empty() || late
    })
    .map_err(|_| "a profile was already running")?;
    if !p.silent.is_empty() {
        crate::ktest::diag(format_args!("silent CPUs: {:?}", p.silent));
        return Err("a CPU has taken no samples (interrupts off?)");
    }
    if late {
        return Err("the profile didn't finish before the test's deadline");
    }
    Ok(p)
}

/// The ktests' `start_burn`, bounded by [`TEST_DEADLINE_NS`].
#[cfg(feature = "ktest")]
fn test_start_burn() -> Result<(usize, u64), StartError> {
    let deadline = crate::time::now_ns() + TEST_DEADLINE_NS;
    start_burn(None, || crate::time::now_ns() > deadline)
}

crate::ktest!(
    fn samples_land_while_armed_and_stop_when_disarmed() {
        use crate::{kassert, kassert_eq};
        const WANT: usize = 20;
        let p = test_profile(WANT)?;
        kassert!(!p.stopped() && !p.taken.is_empty());
        // Every CPU profiled took exactly what was asked for: no more (the
        // buffer refuses past it), no fewer (it waited).
        kassert!(p.taken.iter().all(|&(_, n)| n == WANT));
        // Each sample's idle flag says whether its thread was its CPU's
        // idle thread; the ktest thread sleeps while it waits, so its CPU
        // (at least) idled.
        for (cpu, samples) in p.per_cpu() {
            let idle_id = sched::idle_thread_id(cpu);
            kassert!(idle_id != 0);
            kassert!(samples.iter().all(|s| s.thread != 0 && s.idle == (s.thread == idle_id)));
        }
        kassert!(p.idle() > 0);
        // No profile running: the timer records nothing, even into an
        // armed buffer (the fast path), until a profile arms the timer.
        kassert!(!ARMED.load(Ordering::Relaxed));
        let cpu = crate::arch::cpu_index();
        BUFFERS[cpu].arm(WANT);
        let ticks = crate::arch::idt::ticks_on(cpu);
        let deadline = crate::time::now_ns() + TEST_DEADLINE_NS;
        while crate::arch::idt::ticks_on(cpu) < ticks + 3 && crate::time::now_ns() < deadline {
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
        kassert!(p.taken.iter().all(|&(_, n)| n <= WANT));
        kassert!(p.samples.len() == p.taken.iter().map(|&(_, n)| n).sum::<usize>());
        // The guard disarmed everything on the way out.
        kassert!(!ARMED.load(Ordering::Relaxed) && !PROFILING.load(Ordering::Relaxed));
        kassert!(BUFFERS.iter().all(|b| b.progress() == (0, 0)));
        Ok(())
    }
);

crate::ktest!(
    fn a_spinning_thread_ranks_first_by_function_and_by_thread() {
        use crate::{kassert, kassert_eq};
        let (_, id) = test_start_burn().map_err(|_| "perf burn didn't start")?;
        let p = test_profile(50);
        stop_burn().map_err(|_| "perf burn didn't stop")?;
        let p = p?;
        let by_thread = lumen_perf::by_thread(&p.samples);
        kassert_eq!(by_thread.rows.first().map(|r| r.key), Some(id));
        let by_fn = by_function(&p);
        let top = by_fn.rows.first().and_then(|r| r.key).unwrap_or("");
        kassert!(top.contains("burn_loop"));
        Ok(())
    }
);

crate::ktest!(
    fn a_burn_being_stopped_keeps_its_slot_until_its_thread_has_ended() {
        use crate::{kassert, kassert_eq};
        let (cpu, first) = test_start_burn().map_err(|_| "perf burn didn't start")?;
        // Between telling the thread to stop and joining it, the slot stays
        // taken: a start (or another stop) is refused, not run alongside.
        let stop = begin_stop().map_err(|_| "nothing to stop")?;
        kassert_eq!(test_start_burn(), Err(StartError::Stopping(cpu)));
        kassert_eq!(stop_burn(), Err(StopError::Stopping(cpu)));
        kassert_eq!(finish_stop(stop), cpu);
        kassert_eq!(stop_burn(), Err(StopError::NotRunning));
        // A new burn has its own flags: it runs (the old stop doesn't end
        // it) until its own stop.
        let (_, second) = test_start_burn().map_err(|_| "the second burn didn't start")?;
        kassert!(second != first);
        kassert!(matches!(*BURNING.lock(), Burn::Running { .. }));
        kassert_eq!(stop_burn(), Ok(cpu));
        Ok(())
    }
);

crate::ktest!(
    fn the_function_index_finds_what_a_scan_finds() {
        use crate::kassert_eq;
        // Kernel functions' own addresses and a few bytes in: the index and
        // the crash reporter's scan (`symbols::resolve`) agree.
        let index = function_index();
        let fns = [burn_loop as fn(&Session) -> u64 as usize as u64, on_tick as fn(u64) as usize as u64];
        for addr in fns.into_iter().flat_map(|a| [a, a + 1, a + 7]) {
            let scanned = crate::symbols::resolve(addr).map(|l| (l.name, l.offset));
            kassert_eq!(index.lookup(addr), scanned);
        }
        Ok(())
    }
);
