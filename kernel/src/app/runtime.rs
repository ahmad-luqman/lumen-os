//! The app runtime (#199; ARCHITECTURE section 8, "Running an app"): the
//! table of running apps, loading one on its own thread, its fuel slices,
//! how its life goes (one [`Phase`], moving only forward, and one
//! [`Outcome`], the first recorded), `kill` (section 8, "Kill"), the reaper
//! that joins ended apps, and the shell's `apps`, `run` and `kill`.
//!
//! Locks: `TABLE` (the apps), each app's `life`, `REAPER_THREAD`. None is
//! held across a wait, a spawn's join, a wake or a host call; `TABLE` is
//! taken before an app's own lock, never after. A wait queue's condition
//! may take an app's `life` (nobody wakes while holding it).

use super::window::AppWindow;
use super::{App, BUNDLED, TEST_APPS, host};
use crate::desktop::Waker;
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
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use lumen_app_api::end::{CloseGrace, EndCell, EndReason, Level, Outcome};
use lumen_app_api::handles::HandleTable;
use lumen_app_api::host::{ENTRY, MEMORY};
use lumen_app_api::limits::{
    CALL_DEPTH, KILL_WAIT_NS, MEMORY_PAGES, MODULE_BYTES, PAGE_BYTES, SLICE_FUEL, TABLE_ELEMENTS, VALUE_STACK_BYTES,
    slots,
};
use lumen_app_api::manifest::{self, Manifest};
use lumen_app_api::text::{self as app_text, LogText};
use lumen_app_api::{Error, Grant, Grants, Kind, Listable};
use lumen_shell::Key;
use lumen_shell::pipe::{self, Align, Column, Kind as ColKind};
use wasmi::errors::{ErrorKind, InstantiationError, MemoryError, TableError};
use wasmi::{
    AsContext, AsContextMut, CompilationMode, Config, Engine, ExternType, Linker, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder, TypedFunc, TypedResumableCall, ValType,
};

/// What a handle names: a grant, or the app's window.
pub(super) enum Obj {
    Grant(Grant),
    Window(alloc::boxed::Box<AppWindow>),
}

/// The signal a host function raises when the app must stop (`exit`,
/// `panic`, a kill seen at a call's entry or a slice end): never resumed.
/// Why it stops is recorded in the store's data first ([`AppCtx::stop`]),
/// and that record is what [`step`] reads, so the end reason doesn't
/// depend on how wasmi hands the error back (a `HostTrap`, a plain error
/// when the call it ends is a tail call from `lumen_main`'s frame or an
/// exported import, wrapped or not).
#[derive(Debug)]
pub(super) struct Stop(pub(super) EndReason);

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.what())
    }
}

impl wasmi::errors::HostError for Stop {}

/// An app's store data: what its host calls work on.
pub(super) struct AppCtx {
    pub(super) state: Arc<AppState>,
    /// The grants it holds now (closing one drops it); with `handles`,
    /// kept by the methods below: a handle names a grant only while the
    /// app holds it.
    grants: Grants,
    handles: HandleTable<Obj>,
    /// `run`'s arguments after its name, each followed by a NUL.
    pub(super) args: Vec<u8>,
    /// Its one memory, once instantiated.
    pub(super) memory: Option<Memory>,
    limits: StoreLimits,
    /// The fuel the current slice started with.
    slice_given: u64,
    /// Fuel charged past what a slice had left (a saturated charge),
    /// taken from the next slice.
    debt: u64,
    /// The close grace, once the app is given `Close`
    /// (`window_next_event`).
    close_grace: CloseGrace,
    /// Why the app is stopping, once a host function said so.
    stopping: Option<EndReason>,
    /// Its glyph cache over the UI font, made at its first text call.
    text: Option<crate::font::Text>,
    /// ktest seam: the desktop `window_open` opens on (the live one
    /// otherwise), `Desktop::without_screen` in the ktests.
    #[cfg(feature = "ktest")]
    pub(super) desktop: Option<crate::desktop::Desktop>,
}

impl AppCtx {
    /// `handle_find`: the one handle to `grant` (the same every time), or
    /// `NotGranted` if the app doesn't hold it.
    pub(super) fn find_grant(&mut self, grant: Grant) -> Result<i32, Error> {
        if !self.grants.has(grant) {
            return Err(Error::NotGranted);
        }
        match self.handles.find(|o| matches!(o, Obj::Grant(g) if *g == grant)) {
            Some(h) => Ok(h),
            None => self.handles.insert(Obj::Grant(grant)),
        }
    }

    /// `handle_close`: a closed grant is gone for the rest of the app's
    /// life; closing the window destroys it (and ends any close grace).
    pub(super) fn close(&mut self, h: i32) -> Result<(), Error> {
        match self.handles.remove(h)? {
            Obj::Grant(g) => self.grants = self.grants.without(g),
            Obj::Window(w) => {
                self.close_grace.closed();
                self.state.set_window(None);
                drop(w);
            }
        }
        Ok(())
    }

    /// Handle `h` names a grant of `kind` (the handle, then its kind).
    pub(super) fn grant_of(&self, h: i32, kind: Kind) -> Result<(), Error> {
        match self.handles.get(h)? {
            Obj::Grant(g) if g.kind() == kind => Ok(()),
            Obj::Grant(_) | Obj::Window(_) => Err(Error::WrongKind),
        }
    }

    /// The window handle `h` names (the handle, then its kind).
    pub(super) fn window_mut(&mut self, h: i32) -> Result<&mut AppWindow, Error> {
        match self.handles.get_mut(h)? {
            Obj::Window(w) => Ok(w),
            Obj::Grant(_) => Err(Error::WrongKind),
        }
    }

    /// Open a window for the app, taking one `ui invoke` request at a time
    /// from its opening on (`window_answer` answers *the* running request):
    /// on the live desktop, or in the ktests on the one their seam set
    /// (`desktop`).
    pub(super) fn open_window(
        &self,
        title: &str,
        w: u32,
        h: u32,
    ) -> Result<crate::desktop::WindowHandle, crate::desktop::OpenError> {
        use crate::desktop::Requests::OneAtATime;
        #[cfg(feature = "ktest")]
        if let Some(d) = &self.desktop {
            return d.open_window_with(title, w, h, OneAtATime);
        }
        crate::desktop::open_window_with(title, w, h, OneAtATime)
    }

    /// Whether the app holds a window (it may hold `WINDOWS`, one: the
    /// only window [`AppCtx::text_and_window`] can find).
    pub(super) fn has_window(&self) -> bool {
        self.handles.find(|o| matches!(o, Obj::Window(_))).is_some()
    }

    /// Hand the app its new window: its handle (`Limit` once the app's
    /// handles are used up, the window then closed), and `kill` can wake
    /// it.
    pub(super) fn add_window(&mut self, window: AppWindow) -> Result<i32, Error> {
        let waker = window.waker();
        let h = self.handles.insert(Obj::Window(alloc::boxed::Box::new(window)))?;
        self.state.set_window(Some(waker));
        Ok(h)
    }

    /// The app was given `Close`: its close grace runs (counted at slice
    /// ends from now on, until it closes its window).
    pub(super) fn given_close(&mut self) {
        self.close_grace.given();
    }

    /// Its glyph cache (made now if this is its first text call) and its
    /// window, if it has one; `Unavailable` if the UI font didn't load.
    pub(super) fn text_and_window(&mut self) -> Result<(&mut crate::font::Text, Option<&mut AppWindow>), Error> {
        const _: () = assert!(lumen_app_api::limits::WINDOWS == 1, "text_draw draws for the app's one window");
        if self.text.is_none() {
            self.text = Some(crate::font::Text::new().map_err(|_| Error::Unavailable)?);
        }
        let text = self.text.as_mut().ok_or(Error::Unavailable)?;
        let window = self.handles.iter_mut().find_map(|o| match o {
            Obj::Window(w) => Some(&mut **w),
            Obj::Grant(_) => None,
        });
        Ok((text, window))
    }

    /// Fuel charged past what the slice had left, not yet taken (ktests).
    #[cfg(feature = "ktest")]
    pub(super) fn debt(&self) -> u64 {
        self.debt
    }

    /// Glyphs its cache rasterized so far (ktests).
    #[cfg(feature = "ktest")]
    pub(super) fn glyphs_rasterized(&self) -> u64 {
        self.text.as_ref().map_or(0, |t| t.stats().misses)
    }

    /// The app stops with `reason`: recorded (the first said wins), and
    /// the host error that unwinds wasmi.
    pub(super) fn stop(&mut self, reason: EndReason) -> wasmi::Error {
        let signal = Stop(reason.clone());
        self.stopping.get_or_insert(reason);
        wasmi::Error::host(signal)
    }
}

/// The fuel the slice has left.
pub(super) fn fuel_left(cx: impl AsContext) -> u64 {
    let left = cx.as_context().get_fuel();
    debug_assert!(left.is_ok(), "fuel metering is on for every app");
    left.unwrap_or(0)
}

/// Charge `fuel` off what the slice has left; what it hasn't left becomes
/// debt, taken from the next slice (so a charge never fails a call and is
/// never lost).
pub(super) fn charge(mut cx: impl AsContextMut<Data = AppCtx>, fuel: u64) {
    let mut cx = cx.as_context_mut();
    let left = fuel_left(&cx);
    cx.data_mut().debt += fuel.saturating_sub(left);
    let set = cx.set_fuel(left.saturating_sub(fuel));
    debug_assert!(set.is_ok(), "fuel metering is on for every app");
}

/// A slice ended (out of fuel, at a blocking call, or at a host call's
/// entry with too little fuel left): counted with the fuel it used, then
/// why the app must end now, if it must (a kill, its close grace used
/// up). The one slice-end path.
pub(super) fn end_slice(mut cx: impl AsContextMut<Data = AppCtx>) -> Result<(), EndReason> {
    let mut cx = cx.as_context_mut();
    count_slice(&mut cx);
    let ctx = cx.data_mut();
    if ctx.state.kill_asked() {
        return Err(EndReason::Killed);
    }
    if ctx.close_grace.slice_end() {
        return Err(EndReason::CloseGrace);
    }
    Ok(())
}

/// Count the slice so far: one more slice, and the fuel it used (nothing
/// if it used none: it was just counted).
fn count_slice(mut cx: impl AsContextMut<Data = AppCtx>) {
    let mut cx = cx.as_context_mut();
    let left = fuel_left(&cx);
    let ctx = cx.data_mut();
    let used = ctx.slice_given.saturating_sub(left);
    // Counted already (a slice end just before the app stopped).
    if used == 0 {
        return;
    }
    ctx.state.slice_ended(used);
    ctx.slice_given = left;
}

/// The app ended within a slice: that slice counted, and any debt (no
/// slice will take it now) added to the fuel it used.
fn last_slice(store: &mut Store<AppCtx>) {
    count_slice(&mut *store);
    let ctx = store.data_mut();
    ctx.state.fuel.fetch_add(core::mem::take(&mut ctx.debt), Ordering::Relaxed);
}

/// Begin the next slice: [`SLICE_FUEL`], or what the instruction that ran
/// out needs if more, less the debt (counted as used now).
pub(super) fn begin_slice(mut cx: impl AsContextMut<Data = AppCtx>, required: u64) -> Result<(), EndReason> {
    let mut cx = cx.as_context_mut();
    let ctx = cx.data_mut();
    let given = SLICE_FUEL.max(required);
    let paid = ctx.debt.min(given);
    ctx.debt -= paid;
    ctx.state.fuel.fetch_add(paid, Ordering::Relaxed);
    ctx.slice_given = given - paid;
    let fuel = ctx.slice_given;
    cx.set_fuel(fuel).map_err(|e| EndReason::Trapped(first_line(&e)))
}

/// Where an app is in its life. It only moves forward (`Waiting` and
/// `Running` aside: a blocking call goes back to `Running`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    /// Its thread waits for `launch` to store its handle.
    Gated,
    /// Its thread compiles and instantiates it.
    Loading,
    /// `lumen_main` runs (`run`'s "started").
    Running,
    /// Blocked in this host call.
    Waiting(&'static str),
    /// Its outcome is recorded; its thread drops its store and logs its
    /// line.
    Ending,
    /// Its thread is done with it: the reaper may join it.
    Ended,
}

impl Phase {
    /// Its place in the order phases go through.
    fn rank(self) -> u8 {
        match self {
            Phase::Gated => 0,
            Phase::Loading => 1,
            Phase::Running | Phase::Waiting(_) => 2,
            Phase::Ending => 3,
            Phase::Ended => 4,
        }
    }

    /// As `apps` shows it.
    fn name(self) -> &'static str {
        match self {
            Phase::Gated | Phase::Loading => "loading",
            Phase::Running => "running",
            Phase::Waiting(_) => "waiting",
            Phase::Ending => "ending",
            Phase::Ended => "ended",
        }
    }

    /// Where `kill` says an app still ending is (ARCHITECTURE section 8,
    /// "Kill"): `loading`, `in a slice`, `in <host call>`, `ending`.
    pub(super) fn place(self) -> String {
        match self {
            Phase::Gated | Phase::Loading => String::from("loading"),
            Phase::Running => String::from("in a slice"),
            Phase::Waiting(call) => format!("in {call}"),
            Phase::Ending | Phase::Ended => String::from("ending"),
        }
    }
}

/// An app's life: its phase and outcome, changed together under one lock.
struct Life {
    phase: Phase,
    outcome: EndCell,
    /// A `run` waits to print how loading went. A load failure nobody waits
    /// for any more is logged instead (one WARN), by whichever of the app's
    /// thread and the waiter comes second.
    watched: bool,
    /// `lumen_main` was called (`run`'s "started").
    ran: bool,
}

impl Life {
    /// Move to `to` if that is forward (never back from `Ending` or
    /// `Ended`, never back to `Gated` or `Loading`).
    fn advance(&mut self, to: Phase) {
        debug_assert!(to.rank() > self.phase.rank(), "{:?} -> {to:?}", self.phase);
        if to.rank() > self.phase.rank() {
            self.phase = to;
        }
    }
}

/// One app's shared state: its thread, `run`, `kill`, `apps` and the reaper
/// each hold an `Arc` of it.
pub(super) struct AppState {
    pub(super) id: u64,
    pub(super) name: &'static str,
    kill: AtomicBool,
    life: SpinLock<Life>,
    /// Its thread's id.
    thread: AtomicU64,
    fuel: AtomicU64,
    slices: AtomicU64,
    memory_kib: AtomicU64,
    /// What the app's thread, `run` and `kill` wait on.
    waits: WaitQueue,
    /// Its window's waker, while it has one: `kill` wakes a `window_wait`.
    window: SpinLock<Option<Waker>>,
    /// ktest seam: the kill flag is set just before this resume of
    /// `lumen_main` (counted from 1; 0: never).
    #[cfg(feature = "ktest")]
    pub(super) kill_at_resume: AtomicU64,
}

impl AppState {
    /// An app's state, gated, watched by whoever launched it.
    pub(super) fn new(id: u64, name: &'static str) -> AppState {
        AppState {
            id,
            name,
            kill: AtomicBool::new(false),
            life: SpinLock::new(Life { phase: Phase::Gated, outcome: EndCell::new(), watched: true, ran: false }),
            thread: AtomicU64::new(0),
            fuel: AtomicU64::new(0),
            slices: AtomicU64::new(0),
            memory_kib: AtomicU64::new(0),
            waits: WaitQueue::new(),
            window: SpinLock::new(None),
            #[cfg(feature = "ktest")]
            kill_at_resume: AtomicU64::new(0),
        }
    }

    /// Change the app's life under its lock, then wake its waiters.
    fn update<R>(&self, f: impl FnOnce(&mut Life) -> R) -> R {
        let r = f(&mut self.life.lock());
        self.waits.wake_all();
        r
    }

    pub(super) fn kill_asked(&self) -> bool {
        self.kill.load(Ordering::Acquire)
    }

    /// Ask the app to end: its thread sees the flag at its next host call,
    /// slice end or wake (a `time_sleep`'s, or its window's `window_wait`).
    pub(super) fn ask_kill(&self) {
        self.kill.store(true, Ordering::Release);
        self.waits.wake_all();
        // Woken outside the lock (the waker takes the window's queue's).
        let waker = self.window.lock().clone();
        if let Some(w) = waker {
            w.wake();
        }
    }

    /// The app's window opened (its waker) or closed (`None`).
    pub(super) fn set_window(&self, waker: Option<Waker>) {
        let old = core::mem::replace(&mut *self.window.lock(), waker);
        drop(old);
    }

    pub(super) fn phase(&self) -> Phase {
        self.life.lock().phase
    }

    /// The gate opens: `loading`.
    pub(super) fn open_gate(&self) {
        self.update(|life| life.advance(Phase::Loading));
    }

    /// `lumen_main` is about to be called: `running`, and started.
    pub(super) fn begin_running(&self) {
        self.update(|life| {
            life.advance(Phase::Running);
            life.ran = true;
        });
    }

    /// Run `f` while `waiting` in host call `call`, then back to
    /// `running` (unless the app moved on meanwhile).
    pub(super) fn waiting<R>(&self, call: &'static str, f: impl FnOnce() -> R) -> R {
        self.update(|life| {
            debug_assert_eq!(life.phase, Phase::Running);
            if life.phase == Phase::Running {
                life.phase = Phase::Waiting(call);
            }
        });
        let r = f();
        self.update(|life| {
            if matches!(life.phase, Phase::Waiting(_)) {
                life.phase = Phase::Running;
            }
        });
        r
    }

    /// Wait until `deadline` (`time::now_ns`; `None`: none) or a kill. A
    /// deadline already reached doesn't block but gives up the CPU
    /// (`time_sleep(h, 0)` is a yield); the kill flag is checked just before
    /// blocking (the wait's own check).
    pub(super) fn wait_until(&self, deadline: Option<u64>) {
        match deadline {
            Some(d) if d <= time::now_ns() => sched::yield_now(),
            Some(d) => {
                let _ = self.waits.wait_until_or_deadline(d, || self.kill_asked());
            }
            None => self.waits.wait_until(|| self.kill_asked()),
        }
    }

    /// The outcome recorded, if any.
    pub(super) fn outcome(&self) -> Option<Outcome> {
        self.life.lock().outcome.outcome().cloned()
    }

    /// Slices ended so far.
    #[cfg(feature = "ktest")]
    pub(super) fn slices(&self) -> u64 {
        self.slices.load(Ordering::Relaxed)
    }

    /// Fuel used so far.
    #[cfg(feature = "ktest")]
    pub(super) fn fuel(&self) -> u64 {
        self.fuel.load(Ordering::Relaxed)
    }

    /// Whether `lumen_main` was called.
    #[cfg(feature = "ktest")]
    pub(super) fn ran(&self) -> bool {
        self.life.lock().ran
    }

    /// A slice ended, having used `fuel`: counted.
    fn slice_ended(&self, fuel: u64) {
        self.slices.fetch_add(1, Ordering::Relaxed);
        self.fuel.fetch_add(fuel, Ordering::Relaxed);
    }

    /// Record `outcome` unless one is recorded already: `ending`.
    pub(super) fn record(&self, outcome: impl Into<Outcome>) {
        let outcome = outcome.into();
        self.update(|life| {
            life.outcome.record(outcome);
            life.advance(Phase::Ending);
        });
    }

    /// Loading failed with `why`: recorded, and logged (one WARN) unless a
    /// `run` still waits to print it.
    fn not_started(&self, why: LogText) {
        let log = self.update(|life| {
            life.outcome.record(Outcome::NotStarted(why));
            life.advance(Phase::Ending);
            !life.watched
        });
        if log {
            self.log_outcome();
        }
    }

    /// Nobody waits for how loading went any more: a load failure recorded
    /// while someone did is handed back for the caller to report (it
    /// wasn't logged). Idempotent.
    pub(super) fn stop_watching(&self) -> Option<LogText> {
        let mut life = self.life.lock();
        let was = core::mem::replace(&mut life.watched, false);
        match life.outcome.outcome() {
            Some(Outcome::NotStarted(why)) if was => Some(why.clone()),
            _ => None,
        }
    }

    /// The app's line for its outcome (target `app`), at its level.
    fn log_outcome(&self) {
        let Some(outcome) = self.outcome() else { return };
        let line = outcome.line(self.name, self.id);
        match outcome.level() {
            Level::Info => log::info!(target: "app", "{line}"),
            Level::Warn => log::warn!(target: "app", "{line}"),
        }
    }

    /// Its thread is done with it: `ended`. Its outcome was recorded first.
    pub(super) fn finish(&self) {
        self.update(|life| {
            debug_assert!(life.outcome.outcome().is_some(), "an app ended without an outcome");
            life.advance(Phase::Ended);
        });
    }

    /// Its thread is done with it.
    pub(super) fn has_ended(&self) -> bool {
        self.phase() == Phase::Ended
    }
}

/// How an app's start went, as `run` waits for it.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Start {
    Pending,
    Started,
    /// Loading failed (the failure handed to the waiter, not logged).
    Failed(LogText),
    /// Loading failed and the failure was logged already (nobody waited).
    Logged,
    /// It ended before `lumen_main` ran (a kill while loading).
    Ended(EndReason),
}

/// The app's start, if it is decided.
pub(super) fn start_of(state: &AppState) -> Start {
    let life = state.life.lock();
    if life.ran {
        return Start::Started;
    }
    match life.outcome.outcome() {
        None => Start::Pending,
        Some(Outcome::Ended(reason)) => Start::Ended(reason.clone()),
        Some(Outcome::NotStarted(_)) => {
            drop(life);
            // Taken: the app's thread no longer logs it.
            state.stop_watching().map_or(Start::Logged, Start::Failed)
        }
    }
}

/// What `run` takes of the start on a pass of its wait: nothing once its
/// console has closed (nobody would see a line printed into it, so a load
/// failure is left to [`Launched`]'s drop to log), else [`start_of`].
pub(super) fn start_seen(state: &AppState, console_closed: bool) -> Option<Start> {
    (!console_closed).then(|| start_of(state))
}

/// What `run` prints for `start` (`Ok`: on its output, `Err`: as a
/// failure), or `None` while it is pending.
pub(super) fn start_line(name: &str, id: u64, start: &Start) -> Option<Result<String, String>> {
    Some(match start {
        Start::Pending => return None,
        Start::Started => Ok(format!("run: started {name} as app {id}")),
        Start::Failed(why) => Err(format!("run: {name}: {why}")),
        Start::Logged => Err(format!("run: {name}: didn't start (see the log)")),
        Start::Ended(reason) => Err(format!("run: {name}: {}", reason.what())),
    })
}

/// What `run` prints when it stops waiting because of `key`, if anything.
/// A closed console takes nothing: nobody would see a line printed into
/// it, so a load failure is left to [`Launched`]'s drop to log.
pub(super) fn stopped_by(launched: &Launched, name: &str, key: Key) -> Option<String> {
    if key == Key::Closed {
        return None;
    }
    Some(match launched.state.stop_watching() {
        Some(why) => format!("run: {name}: {why}"),
        None => format!("run: {name}: still loading (kill {} ends it)", launched.state.id),
    })
}

/// What `kill` says once it has waited: `killed`, `had already ended:
/// <what>` (an app that ended, or never started, before the kill), or
/// `still ending (<place>)`.
pub(super) fn kill_outcome(state: &AppState) -> String {
    let life = state.life.lock();
    match (life.phase, life.outcome.outcome()) {
        (Phase::Ended, Some(Outcome::Ended(EndReason::Killed))) => String::from("killed"),
        (Phase::Ended, Some(outcome)) => format!("had already ended: {}", outcome.what()),
        (phase, _) => format!("still ending ({})", phase.place()),
    }
}

/// An entry's thread.
enum Thread {
    /// Not spawned yet (its slot and id are taken).
    Starting,
    Spawned(JoinHandle),
    /// The reaper is joining it.
    Joining,
}

/// A running (or loading, or ending) app's entry.
struct Entry {
    state: Arc<AppState>,
    thread: Thread,
}

struct Table {
    entries: Vec<Entry>,
    next_id: u64,
}

static TABLE: SpinLock<Table> = SpinLock::new(Table { entries: Vec::new(), next_id: 1 });

/// The reaper's thread, once a spawn of it succeeded (kept rather than
/// detached, so the ktests can stop it).
static REAPER_THREAD: SpinLock<Option<JoinHandle>> = SpinLock::new(None);
/// A `run` is starting it (so two don't).
static REAPER_STARTING: AtomicBool = AtomicBool::new(false);
/// Its failure to start has been logged (once).
static REAPER_WARNED: AtomicBool = AtomicBool::new(false);
/// ktest seam: the reaper's next spawn fails, as one without a stack would.
#[cfg(feature = "ktest")]
pub(super) static FAIL_REAPER_SPAWN: AtomicBool = AtomicBool::new(false);
/// ktest seam: the reaper returns at its next wake.
#[cfg(feature = "ktest")]
static STOP_REAPER: AtomicBool = AtomicBool::new(false);

/// Apps ended and not yet joined; the reaper waits for it to be nonzero.
static PENDING: AtomicUsize = AtomicUsize::new(0);
/// What the reaper waits on.
static ENDED: WaitQueue = WaitQueue::new();

/// How many apps may be listed at once (loading until joined) here:
/// `MAX_APPS`, or fewer on a smaller machine (worked out at the first
/// `run`).
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

/// What `run` checks before the app's thread exists, `launch` and the
/// ktests alike: the module's size, then its manifest.
pub(super) fn precheck(module: &[u8]) -> Result<Manifest, String> {
    if module.len() > MODULE_BYTES {
        return Err(format!("too big ({} bytes, past {MODULE_BYTES})", module.len()));
    }
    manifest::find(module).map_err(|e| e.to_string())
}

/// Why a module is refused before it is instantiated, beyond wasmi's own
/// validation: its imports (only `lumen`'s functions, with their
/// signatures), its exports (`memory` and `lumen_main: [] -> [i32]`) and
/// its memory (fixed: a maximum equal to its initial size, within
/// `MEMORY_PAGES`).
pub(super) fn check_module(module: &Module) -> Result<(), String> {
    for import in module.imports() {
        let what = format!("import {}.{}", app_text::log_line(import.module()), app_text::log_line(import.name()));
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
    match module.get_export(ENTRY) {
        Some(ExternType::Func(ty)) if ty.params().is_empty() && ty.results() == [ValType::I32] => {}
        Some(_) => return Err(format!("export {ENTRY}: not [] -> [i32]")),
        None => return Err(format!("no export {ENTRY}")),
    }
    match module.get_export(MEMORY) {
        Some(ExternType::Memory(ty)) => {
            let (min, max) = (ty.minimum(), ty.maximum());
            if max != Some(min) {
                return Err(String::from("memory: not fixed (its maximum must equal its initial size)"));
            }
            if min > MEMORY_PAGES {
                return Err(format!("memory: {min} pages, past {MEMORY_PAGES}"));
            }
        }
        Some(_) => return Err(format!("export {MEMORY}: not a memory")),
        None => return Err(format!("no export {MEMORY}")),
    }
    Ok(())
}

/// wasmi's translator's message for running out of memory (its error type
/// isn't exported, so its message is what tells it apart).
const TRANSLATION_OOM: &str = "ran out of system memory during translation";

/// Whether `e` is wasmi running out of memory where it allocates
/// fallibly: compiling (the translator's buffers), or a linear memory or
/// table (at instantiation, wrapped in an `InstantiationError`, or
/// growing). Its other allocations, its execution stacks among them, are
/// infallible and end in the kernel's allocation-error panic.
pub(super) fn out_of_memory(e: &wasmi::Error) -> bool {
    match e.kind() {
        ErrorKind::Memory(MemoryError::OutOfSystemMemory)
        | ErrorKind::Table(TableError::OutOfSystemMemory)
        | ErrorKind::Instantiation(
            InstantiationError::FailedToInstantiateMemory(MemoryError::OutOfSystemMemory)
            | InstantiationError::FailedToInstantiateTable(TableError::OutOfSystemMemory),
        ) => true,
        ErrorKind::Translation(t) => t.to_string() == TRANSLATION_OOM,
        _ => false,
    }
}

/// A load step's error as `run` prints it: `not enough memory`, or the
/// first line of wasmi's message (after `prefix`).
fn load_error(prefix: &str, e: &wasmi::Error) -> String {
    if out_of_memory(e) { String::from("not enough memory") } else { format!("{prefix}{}", first_line(e)) }
}

/// The app loaded: its store and entry point (the store's fuel 0: the
/// caller begins a slice), or why it isn't.
pub(super) fn load(bytes: &[u8], ctx: AppCtx) -> Result<(Store<AppCtx>, TypedFunc<(), i32>), String> {
    let engine = Engine::new(&app_config());
    let module = Module::new(&engine, bytes).map_err(|e| load_error("not valid: ", &e))?;
    check_module(&module)?;
    let mut linker = Linker::<AppCtx>::new(&engine);
    host::define(&mut linker).map_err(|e| format!("host functions: {e}"))?;
    let mut store = Store::new(&engine, ctx);
    store.limiter(|ctx| &mut ctx.limits);
    let instance = linker.instantiate_and_start(&mut store, &module).map_err(|e| load_error("", &e))?;
    let memory = instance.get_memory(&store, MEMORY).ok_or_else(|| format!("no export {MEMORY}"))?;
    store.data_mut().memory = Some(memory);
    let main = instance.get_typed_func::<(), i32>(&store, ENTRY).map_err(|e| e.to_string())?;
    Ok((store, main))
}

/// The first line of `e` (some of wasmi's messages span several), made
/// safe for a log line.
fn first_line(e: &impl fmt::Display) -> LogText {
    app_text::log_line(format!("{e}").lines().next().unwrap_or(""))
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
        slice_given: 0,
        debt: 0,
        close_grace: CloseGrace::default(),
        stopping: None,
        text: None,
        #[cfg(feature = "ktest")]
        desktop: None,
    }
}

/// wasmi's answer to calling or resuming `lumen_main`, read in one place
/// for both: on, out of fuel, or why the app ended. A host function's
/// stop is the reason it recorded, however wasmi hands its error back;
/// any other error is a trap.
fn step(
    store: &mut Store<AppCtx>,
    answer: Result<TypedResumableCall<i32>, wasmi::Error>,
) -> Result<TypedResumableCall<i32>, EndReason> {
    let mut ended = |e: &wasmi::Error| store.data_mut().stopping.take().unwrap_or_else(|| stop_reason(e));
    match answer {
        Ok(TypedResumableCall::HostTrap(trap)) => Err(ended(trap.host_error())),
        Ok(on) => Ok(on),
        Err(e) => Err(ended(&e)),
    }
}

/// What an error means for the app's end, without the store's record: a
/// `Stop` (as wasmi handed it back), else a trap.
pub(super) fn stop_reason(e: &wasmi::Error) -> EndReason {
    match e.downcast_ref::<Stop>() {
        Some(Stop(reason)) => reason.clone(),
        None => EndReason::Trapped(first_line(e)),
    }
}

/// Run `lumen_main` in fuel slices until it ends: why it did. The last
/// slice, partial, is counted too.
pub(super) fn run_slices(store: &mut Store<AppCtx>, main: TypedFunc<(), i32>) -> EndReason {
    if let Err(reason) = begin_slice(&mut *store, 0) {
        return reason;
    }
    let mut answer = main.call_resumable(&mut *store, ());
    #[cfg(feature = "ktest")]
    let mut resumes = 0;
    loop {
        let out = match step(store, answer) {
            Ok(TypedResumableCall::OutOfFuel(out)) => out,
            Ok(TypedResumableCall::Finished(code)) => {
                last_slice(store);
                return EndReason::Exited(code);
            }
            Ok(TypedResumableCall::HostTrap(_)) => unreachable!("step reads every host trap"),
            Err(reason) => {
                last_slice(store);
                return reason;
            }
        };
        // A slice end, then the CPU given up, then a slice at least as big
        // as what the instruction that ran out needs.
        if let Err(reason) = end_slice(&mut *store) {
            return reason;
        }
        sched::yield_now();
        if let Err(reason) = begin_slice(&mut *store, out.required_fuel()) {
            return reason;
        }
        #[cfg(feature = "ktest")]
        {
            resumes += 1;
            if store.data().state.kill_at_resume.load(Ordering::Acquire) == resumes {
                store.data().state.ask_kill();
            }
        }
        answer = out.resume(&mut *store);
    }
}

/// The app's thread: wait at the gate, then live, then hand it to the
/// reaper.
fn app_thread(app: &'static App, state: Arc<AppState>, grants: Grants, args: Vec<u8>) {
    state.waits.wait_until(|| state.phase() != Phase::Gated);
    if let Some((id, _)) = sched::running_here() {
        state.thread.store(id, Ordering::Relaxed);
    }
    live(app.module, &state, grants, args);
    state.finish();
    PENDING.fetch_add(1, Ordering::AcqRel);
    ENDED.wake_all();
}

/// An app's life on its thread: load `module`, run it unless a kill came
/// while loading, record its outcome, drop everything it held, log its
/// line (a load failure's only if nobody waits for it).
pub(super) fn live(module: &[u8], state: &Arc<AppState>, grants: Grants, args: Vec<u8>) {
    match load(module, new_ctx(state.clone(), grants, args)) {
        Err(why) => state.not_started(app_text::log_line(&why)),
        Ok((mut store, main)) => {
            if let Some(m) = store.data().memory {
                state.memory_kib.store(m.data(&store).len() as u64 / 1024, Ordering::Relaxed);
            }
            // A kill while loading ends it before `lumen_main` runs.
            if state.kill_asked() {
                state.record(EndReason::Killed);
            } else {
                state.begin_running();
                let reason = run_slices(&mut store, main);
                state.record(reason);
            }
            // Closing its handles (its window destroyed) and dropping its
            // store, instance, module and engine: everything the app held.
            drop(store);
            state.set_window(None);
            state.log_outcome();
        }
    }
}

/// Whether the reaper has been asked to return (the ktests' seam).
fn reaper_stopping() -> bool {
    #[cfg(feature = "ktest")]
    return STOP_REAPER.load(Ordering::Acquire);
    #[cfg(not(feature = "ktest"))]
    false
}

/// The reaper: joins each ended app's thread (outside the table's lock),
/// then removes its entry, freeing its slot.
fn reaper() {
    loop {
        ENDED.wait_until(|| PENDING.load(Ordering::Acquire) > 0 || reaper_stopping());
        if reaper_stopping() {
            return;
        }
        let handles: Vec<(u64, JoinHandle)> = {
            let mut table = TABLE.lock();
            table
                .entries
                .iter_mut()
                .filter(|e| e.state.has_ended() && matches!(e.thread, Thread::Spawned(_)))
                .filter_map(|e| match core::mem::replace(&mut e.thread, Thread::Joining) {
                    Thread::Spawned(h) => Some((e.state.id, h)),
                    _ => None,
                })
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
/// joining ended apps (they keep their slots) until a later `run` starts
/// it. The failure is logged once.
fn ensure_reaper() {
    if REAPER_THREAD.lock().is_some() || REAPER_STARTING.swap(true, Ordering::AcqRel) {
        return;
    }
    #[cfg(feature = "ktest")]
    let seam = FAIL_REAPER_SPAWN.swap(false, Ordering::AcqRel);
    #[cfg(not(feature = "ktest"))]
    let seam = false;
    let spawned = if seam { Err(sched::SpawnError::NotRunning) } else { sched::spawn_on(0, "app-reaper", reaper) };
    match spawned {
        Ok(h) => *REAPER_THREAD.lock() = Some(h),
        Err(e) => {
            if !REAPER_WARNED.swap(true, Ordering::AcqRel) {
                log::warn!(target: "app", "no app-reaper thread ({e}): ended apps keep their slots until a later run starts it");
            }
        }
    }
    REAPER_STARTING.store(false, Ordering::Release);
}

/// Stop the reaper, if it runs, and wait for it: a ktest starts from no
/// reaper whatever ran before it.
#[cfg(feature = "ktest")]
pub(super) fn stop_reaper() {
    let Some(h) = REAPER_THREAD.lock().take() else { return };
    STOP_REAPER.store(true, Ordering::Release);
    ENDED.wake_all();
    h.join();
    STOP_REAPER.store(false, Ordering::Release);
}

/// Whether the reaper runs.
#[cfg(feature = "ktest")]
pub(super) fn reaper_running() -> bool {
    REAPER_THREAD.lock().is_some()
}

/// Whether app `id` is still in the table (not yet joined).
#[cfg(feature = "ktest")]
pub(super) fn listed(id: u64) -> bool {
    TABLE.lock().entries.iter().any(|e| e.state.id == id)
}

/// The apps listed, and the next id `launch` would give.
#[cfg(feature = "ktest")]
pub(super) fn table_counts() -> (usize, u64) {
    let t = TABLE.lock();
    (t.entries.len(), t.next_id)
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

/// A launched app, watched until this is dropped: a load failure nobody
/// took ([`start_of`], [`stopped_by`]) is then logged (one WARN).
pub(super) struct Launched {
    pub(super) state: Arc<AppState>,
}

impl Drop for Launched {
    fn drop(&mut self) {
        if self.state.stop_watching().is_some() {
            self.state.log_outcome();
        }
    }
}

/// Start app `name` with `args`, `without` those grants: the app found, its
/// module prechecked and its arguments encoded, then a slot, an id and an
/// entry under the table's lock, its thread spawned and its handle stored,
/// then the gate opened. The launched app, or the line `run` prints.
pub(super) fn launch(name: &str, args: &[String], without: Grants) -> Result<Launched, String> {
    ensure_reaper();
    let Some(app) = find_app(name) else { return Err(format!("run: no app {name}")) };
    let manifest = precheck(app.module).map_err(|why| format!("run: {name}: {why}"))?;
    let app_args =
        lumen_app_api::args::encode(args.iter().map(String::as_str)).map_err(|e| format!("run: {name}: {e}"))?;
    let mut grants = Grants::log();
    for g in manifest.caps.iter() {
        if !without.has(g) {
            grants = grants.with(g);
        }
    }
    let state = {
        let mut table = TABLE.lock();
        let running = table.entries.len();
        if running >= slot_count() {
            return Err(format!("run: {name}: {running} apps running already"));
        }
        let id = table.next_id;
        table.next_id += 1;
        let state = Arc::new(AppState::new(id, app.name));
        table.entries.push(Entry { state: state.clone(), thread: Thread::Starting });
        state
    };
    let id = state.id;
    let thread_state = state.clone();
    let handle = match sched::spawn_on(cpu_for(id), app.thread, move || app_thread(app, thread_state, grants, app_args))
    {
        Ok(h) => h,
        Err(e) => {
            forget(id);
            return Err(format!("run: {name}: {e}"));
        }
    };
    if let Some(entry) = TABLE.lock().entries.iter_mut().find(|e| e.state.id == id) {
        entry.thread = Thread::Spawned(handle);
    }
    state.open_gate();
    Ok(Launched { state })
}

/// The bundled apps whose manifest grants `desktop`, by name (the
/// launcher's; never the test apps).
pub fn desktop_apps() -> Vec<&'static str> {
    desktop_apps_in(BUNDLED)
}

/// The apps of `apps` whose manifest grants `desktop`. One that `precheck`
/// refuses (too big, a bad manifest: `run` would refuse it too) is left
/// out with a WARN naming why (`taskbar: <name>: left out of the launcher:
/// <why>`), never silently.
pub(super) fn desktop_apps_in(apps: &'static [App]) -> Vec<&'static str> {
    let mut names = Vec::new();
    for app in apps {
        match precheck(app.module) {
            Ok(m) if m.caps.has(Grant::Desktop) => names.push(app.name),
            Ok(_) => {}
            Err(why) => log::warn!(target: "taskbar", "{}: left out of the launcher: {why}", app.name),
        }
    }
    names
}

/// Start app `name` with no arguments and every grant, as `run` does but
/// without waiting for its start (the launcher, #206): its id, or why not.
/// A load failure nobody waits for is logged (one WARN).
pub fn start(name: &str) -> Result<u64, String> {
    // Dropped at once: nobody watches its start (a load failure is logged).
    Ok(launch(name, &[], Grants::NONE)?.state.id)
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
    let launched = match launch(name, app_args, without) {
        Ok(launched) => launched,
        Err(why) => return ctx.fail(why),
    };
    let state = &launched.state;

    // "started" or why not; `q` or Ctrl-C stops the wait, not the app.
    loop {
        // A console closed before the load ended takes nothing.
        let Some(start) = start_seen(state, ctx.out.closed()) else { return };
        match start_line(name, state.id, &start) {
            Some(Ok(line)) => {
                let _ = writeln!(ctx.out, "{line}");
                return;
            }
            Some(Err(line)) => return ctx.fail(line),
            None => {}
        }
        if let Some(key) = ctx.poll_key().filter(|k| *k == Key::Char('q') || k.interrupts()) {
            if let Some(line) = stopped_by(&launched, name, key) {
                ctx.fail(line);
            }
            return;
        }
        let deadline = time::now_ns().saturating_add(10_000_000);
        let _ = state.waits.wait_until_or_deadline(deadline, || {
            let life = state.life.lock();
            life.ran || life.outcome.outcome().is_some()
        });
    }
}

/// `kill <id>`.
pub fn kill(ctx: &mut Ctx<'_>, args: &[String]) {
    let [id] = args else { return ctx.fail("usage: kill <id>") };
    let Ok(id) = id.parse::<u64>() else { return ctx.fail("usage: kill <id>") };
    let Some(state) = TABLE.lock().entries.iter().find(|e| e.state.id == id).map(|e| e.state.clone()) else {
        return ctx.fail(format_args!("kill: no app {id}"));
    };
    state.ask_kill();
    let deadline = time::now_ns().saturating_add(KILL_WAIT_NS);
    let _ = state.waits.wait_until_or_deadline(deadline, || state.has_ended());
    let _ = writeln!(ctx.out, "kill: app {id} ({}) {}", state.name, kill_outcome(&state));
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
            // Windows it holds (0 or 1): one while it has a waker.
            int(u64::from(s.window.lock().is_some())),
        ]);
    }
    t
}
