//! Scripting with rhai (#67): `rhai` opens a REPL, `rhai -e <expr>`
//! evaluates an expression, `rhai demo <name>` runs a built-in script (until
//! M6 brings files).
//!
//! Scripts see the machine through a few functions ([`FUNCTIONS`]): `cpus()`,
//! `mem()`, `uptime()`, `trace(n)`, `sleep(ms)`, `print`, `debug`, and
//! drawing (`draw_pixel`, `draw_rect`, `draw_text`, `clear`) on the
//! terminal's [`Canvas`], taken on the first draw (`screen_width()` and
//! `screen_height()` don't take it). A script that drew and ended without
//! an error shows `rhai: press a key` on a strip at the canvas's bottom (and
//! on its console) and keeps the canvas until a key is pressed on its
//! console; one that failed gives the canvas back first, so the error shows.
//! While a script holds the canvas, what it prints reaches the screen's
//! terminal but is drawn only once the canvas is given back.
//!
//! **Its own thread.** Each run (or REPL session) is a thread with a
//! [`SCRIPT_STACK`] stack, the shell waiting for it (`sched::run_sized`):
//! rhai recurses (calls, nested expressions, and every walk over a nested
//! value), far past a shell thread's 64 KiB.
//!
//! **Limits** ([`LIMITS`]), each an error rather than a guard-page crash or
//! an out-of-memory panic:
//! - operations, call levels, and expression depth (top level, functions);
//! - value sizes, counted by rhai over a whole value, nested values
//!   included: string bytes, array items, map items. They also bound how
//!   deeply values nest (each level is an item), so the walks rhai makes
//!   over a value (clone, drop, print, size checks) are bounded; equality,
//!   whose rhai version costs about 1.7 KiB of stack a level, is replaced
//!   for arrays and maps by [`equal`], which also caps the depth itself.
//!   `curry` is disabled and closures are compiled out (`no_closure`):
//!   curried arguments nest without being counted;
//! - variables in a scope, and heap growth during a run (checked every 64
//!   operations, so it can overshoot by what 64 operations allocate).
//!
//! **Ctrl-C** on the console stops a script: the console is asked for a key
//! at most every [`POLL_NS`], from rhai's progress hook (every 64
//! operations), from every drawing call and `print`, and while `sleep`
//! waits. Asking takes every key waiting on the console until a Ctrl-C;
//! the others are dropped.
//!
//! **The console.** rhai's callbacks (`print`, the progress hook, registered
//! functions) must be `'static`, so they can't borrow the command's
//! [`Ctx`]. They share a [`State`] (`Rc`: the engine isn't `Sync`) whose
//! [`ConsoleSlot`] holds the console only while an evaluation runs.

use crate::shell::{Console, Ctx};
use crate::term::{self, Canvas, Owner};
use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::fmt::Write;
use lumen_shell::{Complete, Editor, Event, Key, Mode};
use rhai::{Array, Dynamic, Engine, EvalAltResult, ImmutableString, Map, NativeCallContext, Position, Scope};
use slot::ConsoleSlot;

/// What a script may use. Values nest at most `array + map` deep (each
/// level is an item of one), which [`SCRIPT_STACK`] is sized for.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Operations (Ctrl-C is the quicker way out of a runaway loop).
    pub operations: u64,
    /// Nested function calls (rhai runs levels 1 to this).
    pub call_levels: usize,
    /// Nesting of expressions at the top level and inside functions
    /// (parsing recurses too).
    pub expr_depth: usize,
    pub function_expr_depth: usize,
    /// Bytes of string in one value (all its strings, nested ones counted).
    pub string: usize,
    /// Items of arrays in one value (nested arrays counted).
    pub array: usize,
    /// Items of maps in one value (nested maps counted).
    pub map: usize,
    /// Variables in a scope.
    pub variables: usize,
    /// Bytes the heap may grow by during one run.
    pub heap: usize,
}

/// The limits scripts run with.
pub const LIMITS: Limits = Limits {
    operations: 200_000_000,
    call_levels: 16,
    expr_depth: 24,
    function_expr_depth: 16,
    string: 64 * 1024,
    array: 1024,
    map: 256,
    variables: 256,
    heap: 32 * 1024 * 1024,
};

/// The stack each script thread runs on: twice what the deepest cases of
/// the ktests `deep_recursion_is_an_error_not_a_crash` and
/// `nested_values_are_an_error_not_a_crash` need (they run on half of it).
pub const SCRIPT_STACK: u64 = 1024 * 1024;
const _: () = assert!(SCRIPT_STACK > 0 && SCRIPT_STACK.is_multiple_of(4096));
/// How often the console is asked for Ctrl-C, at most.
pub const POLL_NS: u64 = 10_000_000;
/// How often (in operations) the progress hook asks for Ctrl-C (subject to
/// [`POLL_NS`]) and checks the heap.
const CHECK_EVERY: u64 = 64;
/// The longest `sleep(ms)`.
const MAX_SLEEP_MS: i64 = 60_000;
/// Lines the REPL keeps in its history.
const HISTORY: usize = 50;
/// rhai hashes function names with a seed it otherwise takes from a random
/// value `const-random` fixes when rhai is compiled (still linked in, just
/// unused): a fixed one keeps runs alike from build to build.
const HASHING_SEED: [u64; 4] = [0x6c75_6d65_6e00_0001, 2, 3, 4];
/// The strip at the canvas's bottom saying to press a key, in pixels.
const STRIP: usize = 24;

mod slot {
    //! A place for a console, filled only while a closure runs.

    use crate::shell::Console;
    use core::cell::Cell;
    use core::ptr::NonNull;

    /// Holds a console lent by [`ConsoleSlot::lend`], for callbacks that
    /// can't borrow it ([`ConsoleSlot::with`]).
    #[derive(Default)]
    pub struct ConsoleSlot(Cell<Option<NonNull<dyn Console>>>);

    impl ConsoleSlot {
        /// Run `f` with `console` in the slot; the slot holds what it held
        /// before again when `f` returns.
        pub fn lend<R>(&self, console: &mut dyn Console, f: impl FnOnce() -> R) -> R {
            struct Restore<'a>(&'a ConsoleSlot, Option<NonNull<dyn Console>>);
            impl Drop for Restore<'_> {
                fn drop(&mut self) {
                    self.0.0.set(self.1);
                }
            }
            let ptr = NonNull::from(console);
            // SAFETY: only the trait object's lifetime bound changes (same
            // layout). The pointer is dereferenced only by `with`, which
            // can be reached only through `&self` while `f` runs; `Restore`
            // takes it out of the slot when `f` returns (a panic aborts the
            // kernel), so it is never used after `console`'s borrow ends.
            let ptr = unsafe { core::mem::transmute::<NonNull<dyn Console + '_>, NonNull<dyn Console>>(ptr) };
            let _restore = Restore(self, self.0.replace(Some(ptr)));
            f()
        }

        /// Run `f` on the lent console; `None` if none is lent (outside
        /// `lend`, or inside another `with`).
        pub fn with<R>(&self, f: impl FnOnce(&mut dyn Console) -> R) -> Option<R> {
            // Taken out while in use, so a nested call can't make a second
            // `&mut` to it.
            let ptr = self.0.take()?;
            // SAFETY: a lent console is alive and borrowed for `lend`'s
            // closure, during which it is reached only through the slot,
            // which is empty while this reference exists.
            let r = f(unsafe { &mut *ptr.as_ptr() });
            self.0.set(Some(ptr));
            Some(r)
        }
    }
}

/// Why a run was stopped early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    CtrlC,
    Heap,
}

/// What the engine's callbacks share with the command running the script.
struct State {
    /// The command's canvas owner (the shell takes back only its canvas).
    owner: Owner,
    limits: Limits,
    console: ConsoleSlot,
    /// The canvas, once the script drew.
    canvas: RefCell<Option<Canvas>>,
    /// Set when the run must stop (at the next operation).
    stop: Cell<Option<Stop>>,
    /// When the console may be asked for a key next (`time::now_ns`).
    next_poll: Cell<u64>,
    /// The heap's bytes in use when the run started.
    heap_start: Cell<usize>,
}

impl State {
    fn new(owner: Owner, limits: Limits) -> State {
        State {
            owner,
            limits,
            console: ConsoleSlot::default(),
            canvas: RefCell::new(None),
            stop: Cell::new(None),
            next_poll: Cell::new(0),
            heap_start: Cell::new(0),
        }
    }

    /// Start a run (the console is first asked for a key [`POLL_NS`] in:
    /// keys typed before a short script stay for whatever reads them next).
    fn reset(&self) {
        self.stop.set(None);
        self.next_poll.set(crate::time::now_ns() + POLL_NS);
        self.heap_start.set(crate::mm::heap::stats().used);
    }

    /// Whether the run must stop: Ctrl-C was pressed (the console is asked
    /// at most every [`POLL_NS`]) or, with `heap`, it grew the heap past its
    /// limit.
    fn poll(&self, heap: bool) -> bool {
        if self.stop.get().is_none() {
            let now = crate::time::now_ns();
            if now >= self.next_poll.get() {
                self.next_poll.set(now + POLL_NS);
                let ctrl_c = self.console.with(|c| core::iter::from_fn(|| c.try_key()).any(|k| k == Key::Ctrl('c')));
                if ctrl_c == Some(true) {
                    self.stop.set(Some(Stop::CtrlC));
                }
            }
        }
        if heap && self.stop.get().is_none() {
            let grown = crate::mm::heap::stats().used.saturating_sub(self.heap_start.get());
            if grown > self.limits.heap {
                self.stop.set(Some(Stop::Heap));
            }
        }
        self.stop.get().is_some()
    }

    /// An error ending the run if it must stop (see [`Self::poll`]).
    fn check(&self) -> Result<(), Box<EvalAltResult>> {
        if self.poll(false) {
            Err(EvalAltResult::ErrorTerminated(Dynamic::UNIT, Position::NONE).into())
        } else {
            Ok(())
        }
    }

    /// Run `f` on the canvas, taken on first use; an error if the run must
    /// stop, there is no canvas, or it was taken back (a crash report).
    fn draw<R>(&self, f: impl FnOnce(&Canvas) -> R) -> Result<R, Box<EvalAltResult>> {
        self.check()?;
        let mut canvas = self.canvas.borrow_mut();
        if canvas.is_none() {
            *canvas = Some(term::canvas(self.owner).map_err(|e| e.to_string())?);
        }
        match canvas.as_ref() {
            Some(c) if c.is_live() => Ok(f(c)),
            _ => Err("the screen was taken back".into()),
        }
    }
}

/// The functions scripts get from Lumen, with their arguments (for the
/// REPL's banner and completion).
pub const FUNCTIONS: &[&str] = &[
    "cpus()",
    "mem()",
    "uptime()",
    "trace(n)",
    "sleep(ms)",
    "print(x)",
    "debug(x)",
    "draw_pixel(x, y, rgb)",
    "draw_rect(x, y, w, h, rgb)",
    "draw_text(x, y, text, rgb[, bg])",
    "clear([rgb])",
    "screen_width()",
    "screen_height()",
];

/// A built-in script.
pub struct Demo {
    pub name: &'static str,
    pub about: &'static str,
    pub source: &'static str,
}

/// The built-in scripts.
pub const DEMOS: &[Demo] = &[
    Demo {
        name: "rainbow",
        about: "colour bars, boxes and text on the screen",
        source: include_str!("script/rainbow.rhai"),
    },
    Demo { name: "fib", about: "the first Fibonacci numbers, recursively", source: include_str!("script/fib.rhai") },
    Demo { name: "count", about: "count up twice a second until Ctrl-C", source: include_str!("script/count.rhai") },
];

/// The names of [`DEMOS`] (for completion).
pub const DEMO_NAMES: &[&str] = &{
    let mut names = [""; DEMOS.len()];
    let mut i = 0;
    while i < DEMOS.len() {
        names[i] = DEMOS[i].name;
        i += 1;
    }
    names
};

/// A coordinate or size from a script: negative is 0 (the canvas clips the
/// rest).
fn px(v: i64) -> usize {
    usize::try_from(v).unwrap_or(0)
}

/// A colour from a script (`0xRRGGBB`; higher bits ignored).
fn rgb(v: i64) -> u32 {
    (v as u64 & 0xff_ffff) as u32
}

/// The canvas's size, without taking it (`screen_width()`/`screen_height()`).
fn canvas_size() -> Result<(i64, i64), Box<EvalAltResult>> {
    let (w, h) = term::canvas_size().ok_or("no screen to draw on")?;
    Ok((w as i64, h as i64))
}

/// Whether `a` and `b` are equal, arrays and maps item by item, `depth`
/// levels down (an error past `limit`, which the size limits keep values
/// from reaching). Replaces rhai's `==` on arrays and maps, which calls
/// back into the engine for every item (about 1.7 KiB of stack a level).
fn equal(
    ctx: &NativeCallContext,
    a: &Dynamic,
    b: &Dynamic,
    depth: usize,
    limit: usize,
) -> Result<bool, Box<EvalAltResult>> {
    if let (Some(a), Some(b)) = (a.read_lock::<Array>(), b.read_lock::<Array>()) {
        return equal_arrays(ctx, &a, &b, depth, limit);
    }
    if let (Some(a), Some(b)) = (a.read_lock::<Map>(), b.read_lock::<Map>()) {
        return equal_maps(ctx, &a, &b, depth, limit);
    }
    if a.is_array() || a.is_map() || b.is_array() || b.is_map() {
        return Ok(false);
    }
    // Neither is an array or a map: rhai's own `==` (no recursion).
    ctx.call_native_fn::<bool>("==", (a.clone(), b.clone())).or(Ok(false))
}

fn equal_arrays(
    ctx: &NativeCallContext,
    a: &Array,
    b: &Array,
    depth: usize,
    limit: usize,
) -> Result<bool, Box<EvalAltResult>> {
    if depth > limit {
        return Err(format!("values nested more than {limit} deep").into());
    }
    if a.len() != b.len() {
        return Ok(false);
    }
    for (x, y) in a.iter().zip(b.iter()) {
        if !equal(ctx, x, y, depth + 1, limit)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn equal_maps(
    ctx: &NativeCallContext,
    a: &Map,
    b: &Map,
    depth: usize,
    limit: usize,
) -> Result<bool, Box<EvalAltResult>> {
    if depth > limit {
        return Err(format!("values nested more than {limit} deep").into());
    }
    if a.len() != b.len() {
        return Ok(false);
    }
    for (k, x) in a.iter() {
        match b.get(k) {
            Some(y) if equal(ctx, x, y, depth + 1, limit)? => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// An array as rhai writes it (`[1, "a"]`), its items by `Dynamic`'s own
/// `Debug`. Replaces rhai's `to_string`/`print`/`debug`/`to_debug` on
/// arrays, which call back into the engine for every item, like `==`.
fn format_array(a: &Array) -> String {
    let mut text = String::from("[");
    for (i, x) in a.iter().enumerate() {
        let _ = write!(text, "{}{x:?}", if i > 0 { ", " } else { "" });
    }
    text.push(']');
    text
}

/// A map as rhai writes it (`#{"k": 1}`); see [`format_array`].
fn format_map(m: &Map) -> String {
    let mut text = String::from("#{");
    for (i, (k, v)) in m.iter().enumerate() {
        let _ = write!(text, "{}{:?}: {v:?}", if i > 0 { ", " } else { "" }, k.as_str());
    }
    text.push('}');
    text
}

/// An engine with Lumen's functions and limits, its callbacks sharing
/// `state`.
fn engine(state: &Rc<State>) -> Engine {
    let l = state.limits;
    // Only the first call sets it (later ones get an error back), before
    // any engine exists, as rhai requires.
    let _ = rhai::config::hashing::set_hashing_seed(Some(HASHING_SEED));
    let mut e = Engine::new();
    e.set_max_operations(l.operations)
        .set_max_call_levels(l.call_levels)
        .set_max_expr_depths(l.expr_depth, l.function_expr_depth)
        .set_max_string_size(l.string)
        .set_max_array_size(l.array)
        .set_max_map_size(l.map)
        .set_max_variables(l.variables);
    e.disable_symbol("curry");

    let s = state.clone();
    e.on_print(move |text| {
        s.poll(false);
        s.console.with(|c| writeln!(c, "{text}"));
    });
    let s = state.clone();
    e.on_debug(move |text, _, _| {
        s.poll(false);
        s.console.with(|c| writeln!(c, "[debug] {text}"));
    });
    let s = state.clone();
    e.on_progress(move |ops| {
        (s.stop.get().is_some() || (ops % CHECK_EVERY == 0 && s.poll(true))).then_some(Dynamic::UNIT)
    });

    let limit = l.array + l.map;
    e.register_fn("==", move |ctx: NativeCallContext, a: &mut Array, b: Array| equal_arrays(&ctx, a, &b, 0, limit));
    e.register_fn("!=", move |ctx: NativeCallContext, a: &mut Array, b: Array| {
        equal_arrays(&ctx, a, &b, 0, limit).map(|q| !q)
    });
    e.register_fn("==", move |ctx: NativeCallContext, a: &mut Map, b: Map| equal_maps(&ctx, a, &b, 0, limit));
    e.register_fn("!=", move |ctx: NativeCallContext, a: &mut Map, b: Map| {
        equal_maps(&ctx, a, &b, 0, limit).map(|q| !q)
    });
    for name in ["to_string", "to_debug", "print", "debug"] {
        e.register_fn(name, |a: &mut Array| format_array(a));
        e.register_fn(name, |m: &mut Map| format_map(m));
    }

    e.register_fn("cpus", cpus);
    e.register_fn("mem", mem);
    e.register_fn("uptime", || crate::time::since_boot_ns().map_or(0, |ns| (ns / 1_000_000) as i64));
    e.register_fn("trace", trace);
    let s = state.clone();
    e.register_fn("sleep", move |ms: i64| sleep(&s, ms));

    let s = state.clone();
    e.register_fn("draw_pixel", move |x: i64, y: i64, c: i64| {
        s.draw(|cv| {
            if x >= 0 && y >= 0 {
                cv.put(px(x), px(y), rgb(c));
            }
        })
    });
    let s = state.clone();
    e.register_fn("draw_rect", move |x: i64, y: i64, w: i64, h: i64, c: i64| {
        // A rectangle starting left of or above the canvas keeps the part
        // on it.
        let (w, h) = (w.saturating_add(x.min(0)), h.saturating_add(y.min(0)));
        s.draw(|cv| cv.fill_rect(px(x), px(y), px(w), px(h), rgb(c)))
    });
    let s = state.clone();
    e.register_fn("draw_text", move |x: i64, y: i64, text: ImmutableString, fg: i64| {
        s.draw(|cv| {
            cv.text(px(x), px(y), &text, rgb(fg), crate::fb::BG);
        })
    });
    let s = state.clone();
    e.register_fn("draw_text", move |x: i64, y: i64, text: ImmutableString, fg: i64, bg: i64| {
        s.draw(|cv| {
            cv.text(px(x), px(y), &text, rgb(fg), rgb(bg));
        })
    });
    let s = state.clone();
    e.register_fn("clear", move || s.draw(|cv| cv.clear(crate::fb::BG)));
    let s = state.clone();
    e.register_fn("clear", move |c: i64| s.draw(|cv| cv.clear(rgb(c))));
    e.register_fn("screen_width", || canvas_size().map(|(w, _)| w));
    e.register_fn("screen_height", || canvas_size().map(|(_, h)| h));
    e
}

/// `cpus()`: each online CPU as `#{index, apic}`.
fn cpus() -> Array {
    (0..crate::arch::MAX_CPUS)
        .filter_map(|i| {
            let apic = crate::smp::online_lapic_id(i)?;
            let mut m = Map::new();
            m.insert("index".into(), Dynamic::from(i as i64));
            m.insert("apic".into(), Dynamic::from(i64::from(apic)));
            Some(Dynamic::from_map(m))
        })
        .collect()
}

/// `mem()`: the heap's bytes (`heap_size`, `heap_used`, `heap_free`) and
/// its live allocations (`heap_live`).
fn mem() -> Map {
    let heap = crate::mm::heap::stats();
    let mut m = Map::new();
    for (k, v) in
        [("heap_size", heap.size), ("heap_used", heap.used), ("heap_free", heap.free()), ("heap_live", heap.live)]
    {
        m.insert(k.into(), Dynamic::from(v as i64));
    }
    m
}

/// `trace(n)`: the last `n` events of this CPU's trace ring (the boot
/// CPU's: both shells, and so their scripts, run there), a string each,
/// gap lines included.
fn trace(n: i64) -> Array {
    let mut text = String::new();
    let _ = crate::trace::write_recent(&mut text, n.max(0) as u64, "");
    text.lines().map(|l| Dynamic::from(l.to_string())).collect()
}

/// `sleep(ms)`: wait (`ms` clamped to 0 to [`MAX_SLEEP_MS`]), in 10 ms steps
/// so Ctrl-C ends it (and the run) early.
fn sleep(state: &State, ms: i64) -> Result<(), Box<EvalAltResult>> {
    let end = crate::time::now_ns() + ms.clamp(0, MAX_SLEEP_MS) as u64 * 1_000_000;
    loop {
        state.check()?;
        let now = crate::time::now_ns();
        if now >= end {
            return Ok(());
        }
        crate::time::sleep((end - now).min(POLL_NS));
    }
}

/// A size in bytes, as MiB or KiB when it is a whole number of them.
struct Bytes(usize);

impl core::fmt::Display for Bytes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            n if n >= 1 << 20 && n.is_multiple_of(1 << 20) => write!(f, "{} MiB", n >> 20),
            n if n >= 1 << 10 && n.is_multiple_of(1 << 10) => write!(f, "{} KiB", n >> 10),
            n => write!(f, "{n} bytes"),
        }
    }
}

/// `err` as one line (no `rhai: `): the innermost error, the limits in
/// their own words, and the function it happened in.
fn describe(err: &EvalAltResult, stop: Option<Stop>, l: &Limits) -> String {
    let inner = err.unwrap_inner();
    let mut text = match inner {
        EvalAltResult::ErrorTerminated(..) => {
            return match stop {
                Some(Stop::Heap) => format!("stopped: the script grew the heap by more than {}", Bytes(l.heap)),
                _ => "stopped (Ctrl-C)".to_string(),
            };
        }
        EvalAltResult::ErrorStackOverflow(_) => format!("more than {} nested calls", l.call_levels),
        EvalAltResult::ErrorTooManyOperations(_) => format!("more than {} operations", l.operations),
        EvalAltResult::ErrorTooManyVariables(_) => format!("more than {} variables", l.variables),
        EvalAltResult::ErrorDataTooLarge(what, _) if what.starts_with("Length of string") => {
            format!("more than {} bytes of string in one value", l.string)
        }
        EvalAltResult::ErrorDataTooLarge(what, _) if what.starts_with("Size of array") => {
            format!("more than {} array items in one value (nested arrays counted)", l.array)
        }
        EvalAltResult::ErrorDataTooLarge(what, _) if what.starts_with("Size of object map") => {
            format!("more than {} map items in one value (nested maps counted)", l.map)
        }
        // rhai's message, which ends with its position.
        e => return finish(format!("{e}"), err),
    };
    if !inner.position().is_none() {
        let _ = write!(text, " ({})", inner.position());
    }
    finish(text, err)
}

/// `text` on one line, with the function `err` happened in.
fn finish(mut text: String, err: &EvalAltResult) -> String {
    if let EvalAltResult::ErrorInFunctionCall(name, ..) = err {
        let _ = write!(text, " in function '{name}'");
    }
    text.replace('\n', " ")
}

/// One script session: an engine and the variables its lines left.
struct Session {
    state: Rc<State>,
    engine: Engine,
    scope: Scope<'static>,
}

impl Session {
    fn new(owner: Owner, limits: Limits) -> Session {
        let state = Rc::new(State::new(owner, limits));
        let engine = engine(&state);
        Session { state, engine, scope: Scope::new() }
    }

    /// Evaluate `source` on `console`, then show its value (unless it is
    /// `()`) or its error, in one line. If it drew, wait for a key on a
    /// strip at the canvas's bottom before giving the screen back; after an
    /// error, give the screen back first.
    fn run(&mut self, source: &str, console: &mut dyn Console) {
        self.state.reset();
        let (engine, scope) = (&self.engine, &mut self.scope);
        let result = self.state.console.lend(console, || engine.eval_with_scope::<Dynamic>(scope, source));
        // Text made before writing it (formatting a value allocates: see
        // `Ctx::out`).
        let text = match &result {
            Ok(v) if v.is_unit() => None,
            Ok(v) => Some(format!("{v}")),
            Err(e) => Some(format!("rhai: {}", describe(e, self.state.stop.get(), &self.state.limits))),
        };
        let canvas = self.state.canvas.borrow_mut().take();
        let canvas = canvas.filter(|_| result.is_ok());
        if let Some(text) = text {
            let _ = writeln!(console, "{text}");
        }
        if let Some(c) = canvas {
            let (w, h) = (c.width(), c.height());
            c.fill_rect(0, h.saturating_sub(STRIP), w, STRIP, 0x202020);
            c.text(8, h.saturating_sub(STRIP - 4), "rhai: press a key", crate::fb::TEXT, 0x202020);
            let _ = writeln!(console, "rhai: press a key");
            console.read_key();
        }
    }
}

/// What the REPL completes: Lumen's functions.
struct Functions;

impl Complete for Functions {
    fn options(&self, _: &[String], out: &mut Vec<String>) {
        out.extend(FUNCTIONS.iter().map(|f| f.split('(').next().unwrap_or(f).to_string()));
    }
}

/// What a script thread does.
enum Job {
    /// The REPL; whether the console takes escape codes.
    Repl { ansi: bool },
    /// Evaluate a script.
    Eval { source: String },
}

/// Run `job` on `console` with `limits` in a thread of its own with a
/// `stack`-byte stack (rhai recurses deeply), waiting for it to end.
fn in_thread(console: &mut dyn Console, owner: Owner, limits: Limits, stack: u64, job: Job) {
    let ran = crate::sched::run_sized("rhai", stack, || match job {
        Job::Repl { ansi } => repl(console, owner, limits, ansi),
        Job::Eval { source } => Session::new(owner, limits).run(&source, console),
    });
    if let Err(e) = ran {
        let _ = writeln!(console, "rhai: no thread for the script: {e}");
    }
}

/// The shell's `rhai` command.
pub fn command(ctx: &mut Ctx<'_>, args: &[String]) {
    let job = match args {
        [] => Job::Repl { ansi: ctx.ansi },
        [flag, rest @ ..] if flag == "-e" && !rest.is_empty() => Job::Eval { source: rest.join(" ") },
        [demo] if demo == "demo" => {
            let mut text = String::from("demos (rhai demo <name>):\n");
            for d in DEMOS {
                let _ = writeln!(text, "  {:<10} {}", d.name, d.about);
            }
            let _ = ctx.out.write_str(&text);
            return;
        }
        [demo, name] if demo == "demo" => match DEMOS.iter().find(|d| d.name == name.as_str()) {
            Some(d) => Job::Eval { source: d.source.to_string() },
            None => {
                let _ = writeln!(ctx.out, "rhai: no demo called {name} (try 'rhai demo')");
                return;
            }
        },
        _ => {
            let _ = writeln!(ctx.out, "usage: rhai [-e <expr> | demo [name]]");
            return;
        }
    };
    let owner = ctx.owner();
    in_thread(ctx.out, owner, LIMITS, SCRIPT_STACK, job);
}

/// `rhai` alone: read lines with the shell's line editor and evaluate each,
/// keeping variables between them, until `exit` or `quit` (or Ctrl-D on an
/// empty line).
fn repl(console: &mut dyn Console, owner: Owner, limits: Limits, ansi: bool) {
    const PROMPT: &str = "rhai> ";
    let mode = if ansi { Mode::Ansi } else { Mode::Plain };
    // Like the shell's lines, no wider than the console (taken to be at
    // most 80 columns wide, the screen's terminal narrower if it is), the
    // prompt and Ctrl-C's `^C` included; plain serial doesn't redraw, so
    // its lines can be longer.
    let max = match mode {
        Mode::Ansi => term::status().map_or(80, |t| t.cols).min(80).saturating_sub(PROMPT.len() + 3),
        Mode::Plain => 160,
    };
    let mut editor = Editor::new(PROMPT, mode, max, HISTORY);
    let mut session = Session::new(owner, limits);
    let mut shown = String::new();
    let banner = format!("rhai: Lumen's functions are {}; 'exit' or Ctrl-D leaves\n", FUNCTIONS.join(", "));
    let _ = console.write_str(&banner);
    loop {
        let _ = editor.start(&mut shown);
        crate::shell::flush(console, &mut shown);
        let line = loop {
            let key = console.read_key();
            if key == Key::Ctrl('d') && editor.line().is_empty() {
                let _ = writeln!(console);
                return;
            }
            let event = editor.key(key, &Functions, &mut shown);
            crate::shell::flush(console, &mut shown);
            match event {
                Ok(Some(Event::Line(line))) => break Some(line),
                Ok(Some(Event::Cancelled)) => break None,
                _ => {}
            }
        };
        match line.as_deref().map(str::trim) {
            Some("exit" | "quit") => return,
            Some("") | None => {}
            Some(source) => session.run(source, console),
        }
    }
}

/// The keys typing `text` makes (`\n` is Enter).
#[cfg(feature = "ktest")]
fn typed(text: &str) -> impl Iterator<Item = Key> + '_ {
    text.chars().map(|c| if c == '\n' { Key::Enter } else { Key::Char(c) })
}

/// Run `job` with `limits` on a captured console, `keys` typed on it
/// beforehand, on a `stack`-byte stack; what it printed (for the tests),
/// then `[ran dry]` if it waited for a key after `keys` ran out.
#[cfg(feature = "ktest")]
fn captured(job: Job, limits: Limits, stack: u64, keys: impl IntoIterator<Item = Key>) -> String {
    let mut console = crate::shell::Captured::default();
    console.keys.extend(keys);
    in_thread(&mut console, Owner::new(), limits, stack, job);
    if console.ran_dry {
        console.text.push_str("[ran dry]");
    }
    console.text
}

/// Evaluate `source` with `limits` (see [`captured`]), on the script stack.
#[cfg(feature = "ktest")]
fn eval_captured(source: &str, limits: Limits, keys: &[Key]) -> String {
    captured(Job::Eval { source: source.to_string() }, limits, SCRIPT_STACK, keys.iter().copied())
}

/// [`LIMITS`] with fewer operations, for tests of runaway scripts.
#[cfg(feature = "ktest")]
const FEW_OPS: Limits = Limits { operations: 100_000, ..LIMITS };

crate::ktest!(
    fn lumen_functions_answer() {
        use crate::{kassert, kassert_eq};
        let run = |s: &str| eval_captured(s, LIMITS, &[]);
        let online = (0..crate::arch::MAX_CPUS).filter(|&i| crate::smp::online_lapic_id(i).is_some()).count();
        kassert_eq!(run("cpus().len()"), format!("{online}\n"));
        kassert_eq!(run("cpus()[0].index"), "0\n");
        kassert_eq!(run("let m = mem(); m.heap_used + m.heap_free == m.heap_size"), "true\n");
        kassert_eq!(run("print(6 * 7); debug(\"x\"); uptime() > 0"), "42\n[debug] \"x\"\ntrue\n");
        kassert_eq!(run("trace(3).len()"), "3\n");
        // `sleep` waits (and no longer than 60 s).
        kassert_eq!(run("let t = uptime(); sleep(50); let d = uptime() - t; d >= 50 && d < 1000"), "true\n");
        kassert!(run("frobnicate()").starts_with("rhai: Function not found: frobnicate"));
        // The size of the screen, without taking it.
        kassert_eq!(run("screen_width() > 0 && screen_height() > 0"), "true\n");
        kassert!(term::canvas(Owner::new()).is_ok());
        // Equality on arrays and maps (Lumen's), and through `contains`.
        kassert_eq!(run("[1, [2, \"a\"], #{k: [3]}] == [1, [2, \"a\"], #{k: [3]}]"), "true\n");
        kassert_eq!(run("[[1]] == [[2]] || [1] == [1, 2] || [1] == [\"1\"] || #{a: 1} == #{b: 1}"), "false\n");
        kassert_eq!(run("[1] != [2] && #{a: [1]} != #{a: [2]} && !([1] != [1])"), "true\n");
        kassert_eq!(run("[[1], [2]].contains([2]) && [[1]].index_of([1]) == 0"), "true\n");
        // Arrays and maps written as rhai writes them (Lumen's).
        kassert_eq!(
            run("print([1, \"a\", [()]]); `${#{k: \"v\"}}` + [true].to_debug()"),
            "[1, \"a\", [()]]\n#{\"k\": \"v\"}[true]\n"
        );
        kassert!(DEMO_NAMES.iter().eq(DEMOS.iter().map(|d| &d.name)));
        Ok(())
    }
);

crate::ktest!(
    fn errors_are_one_line_and_name_the_limit() {
        use crate::{kassert, kassert_eq};
        let run = |s: &str| eval_captured(s, LIMITS, &[]);
        // In a function: one line, naming the function.
        let in_fn = run("fn f(x) { x.nope() } f(1)");
        crate::ktest::diag(format_args!("{in_fn}"));
        kassert!(in_fn.starts_with("rhai: Function not found: nope") && in_fn.ends_with(" in function 'f'\n"));
        kassert!(run("fn f(n) { f(n + 1) } f(0)").starts_with("rhai: more than 16 nested calls (line 1, position "));
        kassert_eq!(eval_captured("loop {}", FEW_OPS, &[]), "rhai: more than 100000 operations (line 1, position 6)\n");
        kassert!(
            run("let s = \"x\"; loop { s += s }").starts_with("rhai: more than 65536 bytes of string in one value")
        );
        kassert!(run("let a = []; loop { a.push(1) }").starts_with("rhai: more than 1024 array items in one value"));
        // A key added by assigning to it isn't counted until the map is
        // checked again (stored in another value, passed, returned): the
        // heap budget stops a map grown that way.
        kassert_eq!(
            eval_captured(
                "let m = #{}; let i = 0; loop { m[`k${i}`] = i; i += 1 }",
                Limits { heap: 1 << 20, ..LIMITS },
                &[]
            ),
            "rhai: stopped: the script grew the heap by more than 1 MiB\n"
        );
        kassert!(run("curry(Fn(\"f\"), 1)").contains("curry"));
        kassert!(run("Fn(\"f\").curry(1)").starts_with("rhai: Syntax error"));
        // Closures can't capture.
        kassert!(run("let y = 1; let f = |x| x + y; f.call(1)").starts_with("rhai: Variable not found: y"));
        // Every error is one line.
        for s in ["fn f(x) { x.nope() } f(1)", "fn g() { throw \"up\" } fn f() { g() } f()", "[1][5]"] {
            let text = run(s);
            kassert!(text.ends_with('\n') && text.lines().count() == 1);
        }
        Ok(())
    }
);

crate::ktest!(
    fn limits_are_set() {
        // The engine runs with `LIMITS`, the real operation cap included.
        use crate::kassert_eq;
        let s = Session::new(Owner::new(), LIMITS);
        let e = &s.engine;
        kassert_eq!(e.max_operations(), LIMITS.operations);
        kassert_eq!(e.max_call_levels(), LIMITS.call_levels);
        kassert_eq!((e.max_expr_depth(), e.max_function_expr_depth()), (LIMITS.expr_depth, LIMITS.function_expr_depth));
        kassert_eq!(
            (e.max_string_size(), e.max_array_size(), e.max_map_size()),
            (LIMITS.string, LIMITS.array, LIMITS.map)
        );
        kassert_eq!(e.max_variables(), LIMITS.variables);
        // How fast operations go (for the docs).
        let start = crate::time::now_ns();
        let _ = eval_captured("loop {}", Limits { operations: 1_000_000, ..LIMITS }, &[]);
        let ms = (crate::time::now_ns() - start) / 1_000_000;
        crate::ktest::diag(format_args!("1,000,000 operations in {ms} ms"));
        Ok(())
    }
);

crate::ktest!(
    fn ctrl_c_stops_a_script() {
        // Ctrl-C (after another key) stops a runaway loop, also inside a
        // closure or a native function's callback, and during `sleep` (even
        // the last statement); a key that isn't Ctrl-C doesn't stop it.
        use crate::{kassert, kassert_eq};
        let c = Key::Ctrl('c');
        let stopped = "rhai: stopped (Ctrl-C)\n";
        kassert_eq!(eval_captured("loop {}", LIMITS, &[Key::Char('x'), c]), stopped);
        kassert_eq!(eval_captured("[1, 2].map(|x| { loop {} })", LIMITS, &[c]), stopped);
        kassert_eq!(eval_captured("fn f() { loop {} } f()", LIMITS, &[c]), stopped);
        kassert!(eval_captured("loop {}", FEW_OPS, &[Key::Char('x')]).starts_with("rhai: more than 100000 operations"));
        let start = crate::time::now_ns();
        kassert_eq!(eval_captured("sleep(5000)", LIMITS, &[c]), stopped);
        kassert_eq!(eval_captured("print(1); sleep(5000); print(2)", LIMITS, &[c]), "1\nrhai: stopped (Ctrl-C)\n");
        kassert!(crate::time::now_ns() - start < 2_000_000_000);
        // The `count` demo counts until Ctrl-C.
        let count = DEMOS.iter().find(|d| d.name == "count").ok_or("no count demo")?;
        kassert_eq!(eval_captured(count.source, LIMITS, &[c]), "0\nrhai: stopped (Ctrl-C)\n");
        // A drawing loop is stopped too (polled in every drawing call), and
        // gives the screen back without waiting for a key.
        kassert_eq!(eval_captured("loop { draw_pixel(0, 0, 0) }", LIMITS, &[c]), stopped);
        Ok(())
    }
);

crate::ktest!(
    fn the_heap_budget_stops_a_script() {
        // A script keeping more and more on the heap is stopped at its
        // budget (here 256 KiB), not by running out of memory.
        use crate::kassert_eq;
        let small = Limits { heap: 256 * 1024, ..LIMITS };
        let text = eval_captured(
            "let all = []; for i in 0..200 { let a = []; a.pad(1000, i); all.push(a); } all.len()",
            Limits { array: 1_000_000, ..small },
            &[],
        );
        kassert_eq!(text, "rhai: stopped: the script grew the heap by more than 256 KiB\n");
        Ok(())
    }
);

crate::ktest!(
    fn deep_recursion_is_an_error_not_a_crash() {
        // At the deepest call level, the deepest expression the parser
        // takes of each shape around the recursive call (a native call,
        // `Fn(..).call`, `eval`), on half the script stack: the call-depth
        // error, not a guard-page crash.
        use crate::kassert_eq;
        const SHAPES: [(&str, &str); 5] =
            [("1 + (", ")"), ("if true { ", " } else { 0 }"), ("[", "][0]"), ("{ let x = ", "; x }"), ("abs(", ")")];
        const CALLS: [&str; 3] = ["f(n + 1)", "Fn(\"f\").call(n + 1)", "eval(\"f(\" + (n + 1) + \")\")"];
        let script = |shape: (&str, &str), call: &str, depth: usize| {
            format!("fn f(n) {{ {}{call}{} }} f(0)", shape.0.repeat(depth), shape.1.repeat(depth))
        };
        let parser = Session::new(Owner::new(), LIMITS);
        for shape in SHAPES {
            for call in CALLS {
                let mut depth = 0;
                while depth < LIMITS.function_expr_depth
                    && parser.engine.compile(script(shape, call, depth + 1)).is_ok()
                {
                    depth += 1;
                }
                let text = captured(Job::Eval { source: script(shape, call, depth) }, LIMITS, SCRIPT_STACK / 2, []);
                if !text.starts_with("rhai: more than 16 nested calls") {
                    crate::ktest::diag(format_args!("{}{call}{} x{depth}: {text}", shape.0, shape.1));
                }
                kassert_eq!(text.split(" (").next(), Some("rhai: more than 16 nested calls"));
            }
        }
        Ok(())
    }
);

crate::ktest!(
    fn nested_values_are_an_error_not_a_crash() {
        // Values nested as deeply as the size limits allow (arrays, maps,
        // both alternating) are built, compared, copied, printed and
        // dropped on half the script stack; one more level is an error.
        // Curried values can't be made.
        use crate::kassert;
        let run = |s: &str| captured(Job::Eval { source: s.to_string() }, LIMITS, SCRIPT_STACK / 2, []);
        let use_it = "let b = v; let same = v == b && !(v != b) && [v].contains(b) && [v].index_of(b) == 0; \
             let text = `${v}` + v.to_string() + v.to_debug(); print(v); debug(v); v = (); same";
        let arrays = format!("let v = []; for i in 1..{} {{ v = [v] }} {use_it}", LIMITS.array);
        let text = run(&arrays);
        kassert!(text.ends_with("\ntrue\n") && !text.contains("rhai:"));
        let maps = format!("let v = #{{}}; for i in 1..{} {{ v = #{{k: v}} }} {use_it}", LIMITS.map);
        let text = run(&maps);
        kassert!(text.ends_with("\ntrue\n") && !text.contains("rhai:"));
        // Maps around arrays, then arrays to the array limit: as deep as a
        // value can be.
        let both = format!(
            "let v = []; for i in 1..{} {{ v = #{{k: [v]}} }} for i in 1..{} {{ v = [v] }} {use_it}",
            LIMITS.map,
            LIMITS.array - LIMITS.map
        );
        let text = run(&both);
        kassert!(text.ends_with("\ntrue\n") && !text.contains("rhai:"));
        // Past the limits.
        kassert!(run("let v = []; loop { v = [v] }").starts_with("rhai: more than 1024 array items"));
        kassert!(run("let v = #{}; loop { v = #{k: v} }").starts_with("rhai: more than 256 map items"));
        kassert!(run("let v = []; loop { v = #{k: [v]} }").starts_with("rhai: more than 256 map items"));
        kassert!(run("let v = []; loop { v.push(v) }").starts_with("rhai: more than 1024 array items"));
        // Nesting by assigning to an item (the item is checked).
        kassert!(
            run("let v = []; loop { let w = [0]; w[0] = v; v = w }").starts_with("rhai: more than 1024 array items")
        );
        kassert!(
            run("let v = #{}; loop { let w = #{}; w[\"k\"] = v; v = w }").starts_with("rhai: more than 256 map items")
        );
        kassert!(
            run("let v = #{}; loop { let w = #{}; w.k = [v]; v = w }").starts_with("rhai: more than 256 map items")
        );
        kassert!(run("let f = Fn(\"f\"); loop { f = curry(f, f) }").starts_with("rhai: Syntax error"));
        Ok(())
    }
);

crate::ktest!(
    fn a_script_that_drew_waits_for_a_key() {
        // Drawing takes the canvas; after the script the console is asked
        // for a key (none left: it ran dry), then the canvas is given back.
        // After an error it isn't asked.
        use crate::{kassert, kassert_eq};
        // (Quick: the key typed ahead isn't taken by a Ctrl-C poll, the
        // first one POLL_NS after the start.)
        let draw = "draw_rect(0, 0, 10, 10, 0xff0000); draw_text(0, 20, \"hi\", 0xffffff); 1";
        kassert_eq!(eval_captured(draw, LIMITS, &[Key::Char(' ')]), "1\nrhai: press a key\n");
        kassert_eq!(eval_captured(draw, LIMITS, &[]), "1\nrhai: press a key\n[ran dry]");
        kassert!(
            eval_captured("clear(0x123456); throw \"oops\"", LIMITS, &[]).starts_with("rhai: Runtime error: oops")
        );
        kassert!(term::canvas(Owner::new()).is_ok());
        Ok(())
    }
);

crate::ktest!(
    fn the_repl_keeps_going() {
        // Variables last from line to line; after Ctrl-C stopped a line, or
        // an error, the next line runs; `quit`, `exit` and Ctrl-D leave.
        use crate::kassert;
        let repl = |keys: Vec<Key>| captured(Job::Repl { ansi: false }, LIMITS, SCRIPT_STACK, keys);
        let mut keys: Vec<Key> = typed("let x = 6 * 7\nloop {}\n").collect();
        keys.push(Key::Ctrl('c'));
        keys.extend(typed("x * 2\nnope()\nx + 1\nquit\n"));
        let text = repl(keys);
        crate::ktest::diag(format_args!("{text}"));
        kassert!(text.contains("loop {}\nrhai: stopped (Ctrl-C)\nrhai> x * 2\n84\n"));
        kassert!(
            text.contains(
                "nope()\nrhai: Function not found: nope () (line 1, position 1)\nrhai> x + 1\n43\nrhai> quit\n"
            )
        );
        kassert!(!text.contains("[ran dry]"));
        kassert!(repl(typed("exit\n").collect()).ends_with("rhai> exit\n"));
        kassert!(repl(alloc::vec![Key::Ctrl('d')]).ends_with("rhai> \n"));
        Ok(())
    }
);
