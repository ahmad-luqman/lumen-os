//! The app runtime (#199; ARCHITECTURE section 8, "Running an app"): the
//! table of running apps, loading one on its own thread, its fuel slices,
//! how it ends (one reason, the first recorded), `kill`, the reaper that
//! joins ended apps, and the shell's `apps`, `run` and `kill`.
//!
//! Locks: `TABLE` (the apps), each app's `end` cell and `started` slot.
//! None is held across a wait, a spawn's join or a host call; `TABLE` is
//! taken before an app's own locks, never after.

use super::{App, BUNDLED, TEST_APPS, host};
use crate::sched::{self, JoinHandle, WaitQueue};
use crate::shell::Ctx;
use crate::shell::sysinfo::{int, text};
use crate::sync::SpinLock;
use crate::time;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use lumen_app_api::end::{EndCell, EndReason, Level};
use lumen_app_api::handles::HandleTable;
use lumen_app_api::limits::{
    CALL_DEPTH, KILL_WAIT_NS, MEMORY_PAGES, MODULE_BYTES, PAGE_BYTES, SLICE_FUEL, TABLE_ELEMENTS, VALUE_STACK_BYTES,
    slots,
};
use lumen_app_api::{Grant, Grants, Listable, manifest};
use lumen_shell::Key;
use lumen_shell::pipe::{self, Align, Column, Kind as ColKind};
use wasmi::{
    CompilationMode, Config, Engine, ExternType, Linker, Memory, Module, Store, StoreLimits, StoreLimitsBuilder,
    TypedFunc, TypedResumableCall, ValType,
};

/// What a handle names (the `Window` kind comes with #206).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Obj {
    Grant(Grant),
}

/// Why the app stops running, raised by a host function: never resumed,
/// never reported as a trap.
#[derive(Debug)]
pub(super) enum Stop {
    Exit(i32),
    Panic(String),
    Killed,
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Stop::Exit(code) => write!(f, "exit({code})"),
            Stop::Panic(msg) => write!(f, "panic: {msg}"),
            Stop::Killed => f.write_str("killed"),
        }
    }
}

impl wasmi::errors::HostError for Stop {}

/// An app's store data: what its host calls work on.
pub(super) struct AppCtx {
    pub(super) state: Arc<AppState>,
    /// The grants it holds now (closing one drops it).
    pub(super) grants: Grants,
    pub(super) handles: HandleTable<Obj>,
    /// `run`'s arguments after its name, each followed by a NUL.
    pub(super) args: Vec<u8>,
    /// Its one memory, once instantiated.
    pub(super) memory: Option<Memory>,
    limits: StoreLimits,
    /// The fuel the current slice started with.
    pub(super) slice_given: u64,
}

/// An app's phase, as `apps` shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Phase {
    Loading = 0,
    Running = 1,
    Waiting = 2,
    Ending = 3,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Phase::Loading => "loading",
            Phase::Running => "running",
            Phase::Waiting => "waiting",
            Phase::Ending => "ending",
        }
    }

    fn from_u8(v: u8) -> Phase {
        match v {
            0 => Phase::Loading,
            1 => Phase::Running,
            2 => Phase::Waiting,
            _ => Phase::Ending,
        }
    }
}

/// One app's shared state: its thread, `run`, `kill`, `apps` and the reaper
/// each hold an `Arc` of it.
pub(super) struct AppState {
    pub(super) id: u64,
    pub(super) name: &'static str,
    kill: AtomicBool,
    phase: AtomicU8,
    /// The blocking host call it is in, while `waiting`.
    blocked_in: SpinLock<&'static str>,
    end: SpinLock<EndCell>,
    /// Its thread has finished with it (end line logged, store dropped).
    ended: AtomicBool,
    /// "started", or why it didn't, for `run` (taken once).
    started: SpinLock<Option<Result<(), String>>>,
    /// Opened once the entry holds the thread's handle.
    gate: AtomicBool,
    /// Its thread's id.
    thread: AtomicU64,
    fuel: AtomicU64,
    slices: AtomicU64,
    memory_kib: AtomicU64,
    /// What the app's thread, `run` and `kill` wait on.
    waits: WaitQueue,
}

impl AppState {
    pub(super) fn new(id: u64, name: &'static str) -> AppState {
        AppState {
            id,
            name,
            kill: AtomicBool::new(false),
            phase: AtomicU8::new(Phase::Loading as u8),
            blocked_in: SpinLock::new(""),
            end: SpinLock::new(EndCell::new()),
            ended: AtomicBool::new(false),
            started: SpinLock::new(None),
            gate: AtomicBool::new(false),
            thread: AtomicU64::new(0),
            fuel: AtomicU64::new(0),
            slices: AtomicU64::new(0),
            memory_kib: AtomicU64::new(0),
            waits: WaitQueue::new(),
        }
    }

    pub(super) fn kill_asked(&self) -> bool {
        self.kill.load(Ordering::Acquire)
    }

    fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Acquire))
    }

    fn set_phase(&self, p: Phase) {
        self.phase.store(p as u8, Ordering::Release);
    }

    /// A slice ended, having used `fuel`: counted.
    pub(super) fn slice_ended(&self, fuel: u64) {
        self.slices.fetch_add(1, Ordering::Relaxed);
        self.fuel.fetch_add(fuel, Ordering::Relaxed);
    }

    /// Block (`waiting`, in host call `call`) until `deadline`
    /// (`time::now_ns`; `None`: none) or a kill. A deadline already past
    /// waits 0; the kill flag is checked just before blocking (the wait's
    /// own check).
    pub(super) fn block(&self, call: &'static str, deadline: Option<u64>) {
        *self.blocked_in.lock() = call;
        self.set_phase(Phase::Waiting);
        match deadline {
            Some(d) => {
                let _ = self.waits.wait_until_or_deadline(d, || self.kill_asked());
            }
            None => self.waits.wait_until(|| self.kill_asked()),
        }
        self.set_phase(Phase::Running);
    }

    /// Record `reason` as the app's end unless one is recorded already.
    fn record_end(&self, reason: EndReason) {
        self.end.lock().record(reason);
    }

    fn end_reason(&self) -> Option<EndReason> {
        self.end.lock().reason().cloned()
    }

    fn report_start(&self, result: Result<(), String>) {
        *self.started.lock() = Some(result);
        self.waits.wake_all();
    }
}

/// A running (or loading, or ending) app's entry.
struct Entry {
    state: Arc<AppState>,
    /// Its thread, once spawned; the reaper takes it to join.
    handle: Option<JoinHandle>,
}

struct Table {
    entries: Vec<Entry>,
    next_id: u64,
}

static TABLE: SpinLock<Table> = SpinLock::new(Table { entries: Vec::new(), next_id: 1 });

/// The reaper runs (set once its spawn succeeded; never cleared).
static REAPER: AtomicBool = AtomicBool::new(false);
/// A `run` is starting it (so two don't).
static REAPER_STARTING: AtomicBool = AtomicBool::new(false);
/// Its failure to start has been logged (once).
static REAPER_WARNED: AtomicBool = AtomicBool::new(false);
/// ktest seam: the reaper's next spawn fails, as one without a stack would.
#[cfg(feature = "ktest")]
pub(super) static FAIL_REAPER_SPAWN: AtomicBool = AtomicBool::new(false);

/// Apps ended and not yet joined; the reaper waits for it to be nonzero.
static PENDING: AtomicUsize = AtomicUsize::new(0);
/// What the reaper waits on.
static ENDED: WaitQueue = WaitQueue::new();

/// How many apps may run at once here: `MAX_APPS`, or fewer on a smaller
/// machine (worked out once).
fn slot_count() -> usize {
    static SLOTS: AtomicUsize = AtomicUsize::new(usize::MAX);
    let s = SLOTS.load(Ordering::Relaxed);
    if s != usize::MAX {
        return s;
    }
    let usable = crate::mm::frame::counts().owned as u64 * lumen_mm::FRAME_SIZE;
    let s = slots(usable);
    SLOTS.store(s, Ordering::Relaxed);
    s
}

/// The wasmi configuration every app runs with (section 8, "The wasmi
/// configuration"): fuel, eager compilation (compiling charges no fuel
/// and stays inside `Module::new`), one memory, no start function, the
/// call depth and a value stack allocated once at its full size.
pub(super) fn app_config() -> Config {
    let mut config = Config::default();
    config.consume_fuel(true);
    config.compilation_mode(CompilationMode::Eager);
    config.wasm_multi_memory(false);
    config.allow_start_fn(false);
    config.set_max_recursion_depth(CALL_DEPTH);
    config.set_max_stack_height(VALUE_STACK_BYTES);
    config.set_min_stack_height(VALUE_STACK_BYTES);
    config
}

/// The store's limits: one memory, table and instance, the memory and
/// tables within their limits.
fn store_limits() -> StoreLimits {
    StoreLimitsBuilder::new()
        .memories(1)
        .tables(1)
        .instances(1)
        .memory_size((MEMORY_PAGES * PAGE_BYTES) as usize)
        .table_elements(TABLE_ELEMENTS)
        .build()
}

/// Why a module is refused before it is instantiated, beyond wasmi's own
/// validation: its imports (only `lumen`'s functions, with their
/// signatures), its exports (`memory` and `lumen_main: [] -> [i32]`) and
/// its memory (fixed: a maximum equal to its initial size, within
/// `MEMORY_PAGES`).
pub(super) fn check_module(module: &Module) -> Result<(), String> {
    for import in module.imports() {
        let what = format!("import {}.{}", text_safe(import.module()), text_safe(import.name()));
        if import.module() != lumen_app_api::host::MODULE {
            return Err(format!("{what}: only `lumen` imports"));
        }
        let ExternType::Func(ty) = import.ty() else {
            return Err(format!("{what}: not a function"));
        };
        let Some(want) = lumen_app_api::host::lookup(import.name()) else {
            return Err(format!("{what}: no such host function"));
        };
        let same = |got: &[ValType], want: &[lumen_app_api::host::Ty]| {
            got.len() == want.len()
                && got.iter().zip(want).all(|(g, w)| match w {
                    lumen_app_api::host::Ty::I32 => *g == ValType::I32,
                    lumen_app_api::host::Ty::I64 => *g == ValType::I64,
                })
        };
        if !same(ty.params(), want.params) || !same(ty.results(), want.results) {
            return Err(format!("{what}: another signature"));
        }
    }
    match module.get_export("lumen_main") {
        Some(ExternType::Func(ty)) if ty.params().is_empty() && ty.results() == [ValType::I32] => {}
        Some(_) => return Err(String::from("export lumen_main: not [] -> [i32]")),
        None => return Err(String::from("no export lumen_main")),
    }
    match module.get_export("memory") {
        Some(ExternType::Memory(ty)) => {
            let (min, max) = (ty.minimum(), ty.maximum());
            if max != Some(min) {
                return Err(String::from("memory: not fixed (its maximum must equal its initial size)"));
            }
            if min > MEMORY_PAGES {
                return Err(format!("memory: {min} pages, past {MEMORY_PAGES}"));
            }
        }
        Some(_) => return Err(String::from("export memory: not a memory")),
        None => return Err(String::from("no export memory")),
    }
    Ok(())
}

/// A module's name from itself, made safe for a log line.
fn text_safe(s: &str) -> String {
    lumen_app_api::text::log_line(s)
}

/// The app loaded: its store and entry point, or why it isn't.
pub(super) fn load(bytes: &[u8], ctx: AppCtx, fuel: u64) -> Result<(Store<AppCtx>, TypedFunc<(), i32>), String> {
    let engine = Engine::new(&app_config());
    let module = Module::new(&engine, bytes).map_err(|e| format!("not valid: {}", first_line(&e)))?;
    check_module(&module)?;
    let mut linker = Linker::<AppCtx>::new(&engine);
    host::define(&mut linker).map_err(|e| format!("host functions: {e}"))?;
    let mut store = Store::new(&engine, ctx);
    store.limiter(|ctx| &mut ctx.limits);
    store.set_fuel(fuel).map_err(|e| e.to_string())?;
    let instance = linker.instantiate_and_start(&mut store, &module).map_err(|e| first_line(&e))?;
    let memory = instance.get_memory(&store, "memory").ok_or("no export memory")?;
    store.data_mut().memory = Some(memory);
    let main = instance.get_typed_func::<(), i32>(&store, "lumen_main").map_err(|e| e.to_string())?;
    Ok((store, main))
}

/// The first line of `e` (some of wasmi's messages span several), made
/// safe for a log line.
fn first_line(e: &impl fmt::Display) -> String {
    text_safe(format!("{e}").lines().next().unwrap_or(""))
}

/// A fresh store's data for an app.
pub(super) fn new_ctx(state: Arc<AppState>, grants: Grants, args: Vec<u8>) -> AppCtx {
    AppCtx {
        state,
        grants,
        handles: HandleTable::new(),
        args,
        memory: None,
        limits: store_limits(),
        slice_given: SLICE_FUEL,
    }
}

/// Run `lumen_main` in fuel slices until it ends: why it did.
fn run_slices(store: &mut Store<AppCtx>, main: TypedFunc<(), i32>) -> EndReason {
    let state = store.data().state.clone();
    state.set_phase(Phase::Running);
    let mut call = match main.call_resumable(&mut *store, ()) {
        Ok(call) => call,
        Err(e) => return EndReason::trapped(&first_line(&e)),
    };
    loop {
        match call {
            TypedResumableCall::Finished(code) => return EndReason::Exited(code),
            TypedResumableCall::HostTrap(trap) => return stop_reason(trap.host_error()),
            TypedResumableCall::OutOfFuel(out) => {
                // A slice end: counted, the kill flag checked, the CPU
                // given up, then a slice at least as big as what the
                // instruction that ran out needs.
                let given = store.data().slice_given;
                state.slice_ended(given.saturating_sub(store.get_fuel().unwrap_or(0)));
                if state.kill_asked() {
                    return EndReason::Killed;
                }
                sched::yield_now();
                let refuel = SLICE_FUEL.max(out.required_fuel());
                if let Err(e) = store.set_fuel(refuel) {
                    return EndReason::trapped(&first_line(&e));
                }
                store.data_mut().slice_given = refuel;
                call = match out.resume(&mut *store) {
                    Ok(call) => call,
                    Err(e) => return stop_reason(&e),
                };
            }
        }
    }
}

/// What a host error or trap means for the app's end.
fn stop_reason(e: &wasmi::Error) -> EndReason {
    match e.downcast_ref::<Stop>() {
        Some(Stop::Exit(code)) => EndReason::Exited(*code),
        Some(Stop::Panic(msg)) => EndReason::panicked(msg),
        Some(Stop::Killed) => EndReason::Killed,
        None => EndReason::trapped(&first_line(e)),
    }
}

/// The app's thread: wait at the gate, load, report, run, end.
fn app_thread(app: &'static App, state: Arc<AppState>, grants: Grants, args: Vec<u8>) {
    state.waits.wait_until(|| state.gate.load(Ordering::Acquire));
    if let Some((id, _)) = sched::running_here() {
        state.thread.store(id, Ordering::Relaxed);
    }
    let loaded = load(app.module, new_ctx(state.clone(), grants, args), SLICE_FUEL);
    match loaded {
        Err(why) => {
            // A failure before `lumen_main` logs no end line.
            state.report_start(Err(why));
        }
        Ok((mut store, main)) => {
            if let Some(m) = store.data().memory {
                state.memory_kib.store(m.data(&store).len() as u64 / 1024, Ordering::Relaxed);
            }
            state.report_start(Ok(()));
            let reason = run_slices(&mut store, main);
            state.record_end(reason);
            state.set_phase(Phase::Ending);
            // Closing its handles and dropping its store, instance, module
            // and engine: everything the app held.
            drop(store);
            if let Some(reason) = state.end_reason() {
                let line = reason.line(state.name, state.id);
                match reason.level() {
                    Level::Info => log::info!(target: "app", "{line}"),
                    Level::Warn => log::warn!(target: "app", "{line}"),
                }
            }
        }
    }
    state.set_phase(Phase::Ending);
    state.ended.store(true, Ordering::Release);
    state.waits.wake_all();
    PENDING.fetch_add(1, Ordering::AcqRel);
    ENDED.wake_all();
}

/// The reaper: joins each ended app's thread (outside the table's lock),
/// then removes its entry, freeing its slot.
fn reaper() {
    loop {
        ENDED.wait_until(|| PENDING.load(Ordering::Acquire) > 0);
        let handles: Vec<(u64, JoinHandle)> = {
            let mut table = TABLE.lock();
            table
                .entries
                .iter_mut()
                .filter(|e| e.state.ended.load(Ordering::Acquire))
                .filter_map(|e| e.handle.take().map(|h| (e.state.id, h)))
                .collect()
        };
        for (id, handle) in handles {
            handle.join();
            TABLE.lock().entries.retain(|e| e.state.id != id);
            PENDING.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// Start the reaper unless it runs: each `run` tries, before taking a slot,
/// until a spawn succeeds, so a failed spawn (no stack) only delays
/// joining ended apps (they keep their slots) until a later `run`. The
/// failure is logged once.
fn ensure_reaper() {
    if REAPER.load(Ordering::Acquire) || REAPER_STARTING.swap(true, Ordering::AcqRel) {
        return;
    }
    #[cfg(feature = "ktest")]
    let seam = FAIL_REAPER_SPAWN.swap(false, Ordering::AcqRel);
    #[cfg(not(feature = "ktest"))]
    let seam = false;
    let spawned = if seam { Err(sched::SpawnError::NotRunning) } else { sched::spawn_on(0, "app-reaper", reaper) };
    match spawned {
        Ok(h) => {
            // Detached: it runs for the rest of the boot.
            core::mem::forget(h);
            REAPER.store(true, Ordering::Release);
        }
        Err(e) => {
            if !REAPER_WARNED.swap(true, Ordering::AcqRel) {
                log::warn!(target: "app", "no app-reaper thread ({e}): ended apps keep their slots until a later run starts it");
            }
        }
    }
    REAPER_STARTING.store(false, Ordering::Release);
}

/// Whether the reaper runs.
#[cfg(feature = "ktest")]
pub(super) fn reaper_running() -> bool {
    REAPER.load(Ordering::Acquire)
}

/// Whether app `id` is still in the table (not yet joined).
#[cfg(feature = "ktest")]
pub(super) fn listed(id: u64) -> bool {
    TABLE.lock().entries.iter().any(|e| e.state.id == id)
}

impl AppState {
    /// Its thread has finished with it.
    #[cfg(feature = "ktest")]
    pub(super) fn has_ended(&self) -> bool {
        self.ended.load(Ordering::Acquire)
    }
}

/// The CPUs apps are placed on, round robin by app id.
fn cpu_for(id: u64) -> usize {
    let cpus: Vec<usize> = (0..crate::arch::MAX_CPUS).filter(|&c| sched::running_on(c)).collect();
    cpus.get((id as usize - 1) % cpus.len().max(1)).copied().unwrap_or(0)
}

/// An app by name: bundled ones first, then the test apps.
fn find_app(name: &str) -> Option<&'static App> {
    BUNDLED.iter().chain(TEST_APPS.iter()).find(|a| a.name == name)
}

/// Remove app `id`'s entry (it never started a thread).
fn forget(id: u64) {
    TABLE.lock().entries.retain(|e| e.state.id != id);
}

/// Start app `name` with `args`, `without` those grants: a slot, an id and
/// an entry under the table's lock, the module found and checked, its
/// thread spawned and its handle stored, then the gate opened. Its state
/// (whose `started` says how loading went), or the line `run` prints.
pub(super) fn launch(name: &str, args: &[String], without: Grants) -> Result<Arc<AppState>, String> {
    ensure_reaper();
    let app_args = lumen_app_api::args::encode(args.iter().map(String::as_str))
        .map_err(|_| format!("run: {name}: arguments too long"))?;
    let state = {
        let mut table = TABLE.lock();
        let running = table.entries.len();
        if running >= slot_count() {
            return Err(format!("run: {name}: {running} apps running already"));
        }
        let id = table.next_id;
        table.next_id += 1;
        // The name is the app's once found; until then a placeholder (an
        // entry that never starts is forgotten at once).
        let state = Arc::new(AppState::new(id, find_app(name).map_or("?", |a| a.name)));
        table.entries.push(Entry { state: state.clone(), handle: None });
        state
    };
    let id = state.id;
    let refuse = |why: String| {
        forget(id);
        Err(why)
    };
    let Some(app) = find_app(name) else { return refuse(format!("run: no app {name}")) };
    if app.module.len() > MODULE_BYTES {
        return refuse(format!("run: {name}: too big ({} bytes, past {MODULE_BYTES})", app.module.len()));
    }
    let manifest = match manifest::find(app.module) {
        Ok(m) => m,
        Err(e) => return refuse(format!("run: {name}: {e}")),
    };
    let mut grants = Grants::log();
    for g in manifest.caps.iter() {
        if !without.has(g) {
            grants = grants.with(g);
        }
    }
    let thread_state = state.clone();
    let handle = match sched::spawn_on(cpu_for(id), app.thread, move || app_thread(app, thread_state, grants, app_args))
    {
        Ok(h) => h,
        Err(e) => return refuse(format!("run: {name}: {e}")),
    };
    if let Some(entry) = TABLE.lock().entries.iter_mut().find(|e| e.state.id == id) {
        entry.handle = Some(handle);
    }
    state.gate.store(true, Ordering::Release);
    state.waits.wake_all();
    Ok(state)
}

const RUN_USAGE: &str = "usage: run [--without <grant>] <app> [args...]";

/// `run [--without <grant>] <app> [args...]`.
pub fn run(ctx: &mut Ctx<'_>, args: &[String]) {
    let mut args = args;
    let mut without = Grants::NONE;
    while let [flag, grant, rest @ ..] = args
        && flag == "--without"
    {
        match Listable::from_name(grant) {
            Some(l) => without = without.with(l.grant()),
            None if grant == "log" => return ctx.fail("run: --without log: every app holds log"),
            None => return ctx.fail(format_args!("run: --without {grant}: not a grant (time, desktop or trace)")),
        }
        args = rest;
    }
    let Some((name, app_args)) = args.split_first() else {
        let mut names: Vec<&str> = BUNDLED.iter().map(|a| a.name).collect();
        names.sort_unstable();
        let list = if names.is_empty() { String::from("none") } else { names.join(" ") };
        return ctx.fail(format_args!("{RUN_USAGE}\napps: {list}"));
    };
    let state = match launch(name, app_args, without) {
        Ok(state) => state,
        Err(why) => return ctx.fail(why),
    };
    let id = state.id;

    // "started" or why not; `q` or Ctrl-C stops the wait, not the app.
    loop {
        if let Some(result) = state.started.lock().take() {
            match result {
                Ok(()) => {
                    let _ = writeln!(ctx.out, "run: started {name} as app {id}");
                }
                Err(why) => ctx.fail(format_args!("run: {name}: {why}")),
            }
            return;
        }
        if ctx.poll_key().is_some_and(|k| k == Key::Char('q') || k.interrupts()) {
            return ctx.fail(format_args!("run: {name}: still loading (kill {id} ends it)"));
        }
        let deadline = time::now_ns().saturating_add(10_000_000);
        let _ = state.waits.wait_until_or_deadline(deadline, || state.started.lock().is_some());
    }
}

/// `kill <id>`.
pub fn kill(ctx: &mut Ctx<'_>, args: &[String]) {
    let [id] = args else { return ctx.fail("usage: kill <id>") };
    let Ok(id) = id.parse::<u64>() else { return ctx.fail("usage: kill <id>") };
    let Some(state) = TABLE.lock().entries.iter().find(|e| e.state.id == id).map(|e| e.state.clone()) else {
        return ctx.fail(format_args!("kill: no app {id}"));
    };
    state.kill.store(true, Ordering::Release);
    state.waits.wake_all();
    let done = |s: &AppState| s.ended.load(Ordering::Acquire) && s.end_reason().is_some();
    let deadline = time::now_ns().saturating_add(KILL_WAIT_NS);
    let _ = state.waits.wait_until_or_deadline(deadline, || done(&state));
    let name = state.name;
    match state.end_reason() {
        Some(EndReason::Killed) if done(&state) => {
            let _ = writeln!(ctx.out, "kill: app {id} ({name}) killed");
        }
        Some(other) if done(&state) => {
            let _ = writeln!(ctx.out, "kill: app {id} ({name}) had already ended: {}", other.what());
        }
        _ => {
            let place = match state.phase() {
                Phase::Waiting => *state.blocked_in.lock(),
                Phase::Loading => "loading",
                _ => "a slice",
            };
            let _ = writeln!(ctx.out, "kill: app {id} ({name}) still ending (in {place})");
        }
    }
}

/// `apps`: one record per app in the table, by id.
pub fn apps(ctx: &mut Ctx<'_>, args: &[String]) {
    if !args.is_empty() {
        return ctx.fail("usage: apps");
    }
    let mut out = String::new();
    table().render(&mut out);
    let _ = ctx.out.write_str(&out);
}

/// `apps`'s records.
pub fn table() -> pipe::Table {
    const COLUMNS: [Column; 8] = [
        Column::new("id", ColKind::Int, Align::Right, 2),
        Column::new("name", ColKind::Text, Align::Left, 8),
        Column::new("state", ColKind::Text, Align::Left, 7),
        Column::new("thread", ColKind::Int, Align::Right, 6),
        Column::new("fuel", ColKind::Int, Align::Right, 10),
        Column::new("slices", ColKind::Int, Align::Right, 6),
        Column::new("memory", ColKind::Int, Align::Right, 6),
        Column::new("windows", ColKind::Int, Align::Right, 0),
    ];
    let states: Vec<Arc<AppState>> = TABLE.lock().entries.iter().map(|e| e.state.clone()).collect();
    let mut t = pipe::Table::new(&COLUMNS);
    for s in states {
        t.push(alloc::vec![
            int(s.id),
            text(s.name),
            text(s.phase().name()),
            int(s.thread.load(Ordering::Relaxed)),
            int(s.fuel.load(Ordering::Relaxed)),
            int(s.slices.load(Ordering::Relaxed)),
            int(s.memory_kib.load(Ordering::Relaxed)),
            int(0),
        ]);
    }
    t
}

/// `apps` in a pipeline: its records.
pub fn records(args: &[String]) -> Option<Result<pipe::Table, String>> {
    args.is_empty().then(|| Ok(table()))
}
