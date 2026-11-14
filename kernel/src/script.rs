//! Scripting with rhai (#67): `rhai` opens a REPL, `rhai -e <expr>`
//! evaluates an expression, `rhai demo <name>` runs a built-in script (until
//! M6 brings files).
//!
//! Scripts see the machine through a few functions ([`FUNCTIONS`]): `cpus()`,
//! `mem()`, `uptime()`, `trace(n)`, `sleep(ms)`, `print`, and drawing
//! (`draw_pixel`, `draw_rect`, `draw_text`, `clear`, `screen_width()`,
//! `screen_height()`) on the terminal's [`Canvas`], taken on the first draw.
//! A script that drew keeps the canvas after it ends until a key is pressed
//! on its console (`rhai: press a key` says so), then the terminal comes
//! back.
//!
//! **Limits.** A script runs in a thread of its own ([`SCRIPT_STACK`]; the
//! shell waits for it), and its stack and the kernel's heap are the
//! machine's: call depth,
//! expression depth, string, array and map sizes are capped (an error, not
//! a guard-page crash or an out-of-memory panic), and so are operations
//! ([`MAX_OPERATIONS`]). Ctrl-C on the console stops a script: rhai calls
//! back on every operation, and every [`POLL_EVERY`] operations (and while
//! `sleep` waits) the console is asked for a key. Other keys typed while a
//! script runs are dropped.
//!
//! **The console.** rhai's callbacks (`print`, the progress hook, registered
//! functions) must be `'static`, so they can't borrow the command's
//! [`Ctx`]. They share a [`State`] (`Rc`: the engine isn't `Sync`) whose
//! console slot holds the console, its lifetime erased, only while an
//! evaluation runs ([`State::lend`] empties it before returning).

use crate::shell::{Console, Ctx};
use crate::term::{self, Canvas};
use alloc::boxed::Box;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::fmt::Write;
use core::ptr::NonNull;
use lumen_shell::{Complete, Editor, Event, Key, Mode};
use rhai::{Array, Dynamic, Engine, EvalAltResult, ImmutableString, Map, Scope};

/// Operations a script may run (a `loop {}` reaches it in tens of seconds;
/// Ctrl-C is the quicker way out).
pub const MAX_OPERATIONS: u64 = 200_000_000;
/// The stack each script thread runs on ([`in_thread`]).
const SCRIPT_STACK: u64 = 256 * 1024;
/// How often (in operations) the console is asked for Ctrl-C.
const POLL_EVERY: u64 = 4096;
/// Nested function calls. A call level can take several KiB of stack (12
/// levels with nested expressions overflowed 64 KiB); the ktest
/// `deep_recursion_is_an_error_not_a_crash` runs into this cap and to the
/// deepest expression inside it, and passed with a [`SCRIPT_STACK`] half as
/// big (measured when it was set).
const MAX_CALL_LEVELS: usize = 16;
/// Nesting of expressions, at the top level and inside functions (parsing
/// recurses too).
const MAX_EXPR_DEPTH: usize = 24;
const MAX_FUNCTION_EXPR_DEPTH: usize = 16;
/// Characters in a string, items in an array or a map.
const MAX_STRING: usize = 64 * 1024;
const MAX_ARRAY: usize = 64 * 1024;
const MAX_MAP: usize = 4096;
/// The longest `sleep(ms)`.
const MAX_SLEEP_MS: i64 = 60_000;
/// rhai hashes function names with a seed it otherwise takes from a random
/// value fixed when it was compiled; a fixed one keeps every build alike.
const HASHING_SEED: [u64; 4] = [0x6c75_6d65_6e00_0001, 2, 3, 4];

/// What the engine's callbacks share with the command running the script.
#[derive(Default)]
struct State {
    /// The console, lent for one evaluation (see [`State::lend`]).
    console: Cell<Option<NonNull<dyn Console>>>,
    /// The canvas, once a script drew.
    canvas: RefCell<Option<Canvas>>,
    /// Ctrl-C was pressed: stop at the next operation.
    stopped: Cell<bool>,
}

impl State {
    /// Run `f` with `console` in the console slot, emptied again before
    /// returning.
    fn lend<R>(&self, console: &mut dyn Console, f: impl FnOnce() -> R) -> R {
        struct Empty<'a>(&'a State);
        impl Drop for Empty<'_> {
            fn drop(&mut self) {
                self.0.console.set(None);
            }
        }
        let ptr = NonNull::from(console);
        // SAFETY: only the trait object's lifetime bound changes (same
        // layout). The pointer is dereferenced only by `with_console`, from
        // rhai's callbacks, which run only during an evaluation, inside
        // `f`; `Empty` clears the slot when `f` returns (a panic aborts the
        // kernel), so it is never used after `console`'s borrow ends.
        let ptr: NonNull<dyn Console + 'static> = unsafe { core::mem::transmute(ptr) };
        self.console.set(Some(ptr));
        let _empty = Empty(self);
        f()
    }

    /// Run `f` on the lent console (`None` if none is lent: outside an
    /// evaluation, or inside another `with_console`).
    fn with_console<R>(&self, f: impl FnOnce(&mut dyn Console) -> R) -> Option<R> {
        // Taken out while in use, so a nested call can't make a second
        // `&mut` to it.
        let ptr = self.console.take()?;
        // SAFETY: a lent console is alive for the evaluation and reached
        // only through the slot (`lend`), which is empty while this
        // reference exists.
        let r = f(unsafe { &mut *ptr.as_ptr() });
        self.console.set(Some(ptr));
        Some(r)
    }

    /// Whether Ctrl-C was pressed, asking the console (keys other than
    /// Ctrl-C are dropped).
    fn ctrl_c(&self) -> bool {
        if !self.stopped.get() {
            let pressed = self.with_console(|c| core::iter::from_fn(|| c.try_key()).any(|k| k == Key::Ctrl('c')));
            self.stopped.set(pressed == Some(true));
        }
        self.stopped.get()
    }

    /// Run `f` on the canvas, taken on first use.
    fn draw(&self, f: impl FnOnce(&Canvas)) -> Result<(), Box<EvalAltResult>> {
        let mut canvas = self.canvas.borrow_mut();
        if canvas.is_none() {
            *canvas = Some(term::canvas().map_err(|e| e.to_string())?);
        }
        if let Some(c) = canvas.as_ref() {
            f(c);
        }
        Ok(())
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
    "draw_pixel(x, y, rgb)",
    "draw_rect(x, y, w, h, rgb)",
    "draw_text(x, y, text, rgb[, bg])",
    "clear([rgb])",
    "screen_width()",
    "screen_height()",
];

/// The built-in scripts: name, what it does, source.
pub const DEMOS: &[(&str, &str, &str)] = &[
    ("rainbow", "colour bars, boxes and text on the screen", include_str!("script/rainbow.rhai")),
    ("fib", "the first Fibonacci numbers, recursively", include_str!("script/fib.rhai")),
    ("count", "count up twice a second until Ctrl-C", include_str!("script/count.rhai")),
];

/// The names of [`DEMOS`] (for completion; a ktest checks they agree).
pub const DEMO_NAMES: &[&str] = &["rainbow", "fib", "count"];

/// A coordinate or size from a script: negative is 0 (the canvas clips the
/// rest).
fn px(v: i64) -> usize {
    usize::try_from(v).unwrap_or(0)
}

/// A colour from a script (`0xRRGGBB`; higher bits ignored).
fn rgb(v: i64) -> u32 {
    (v as u64 & 0xff_ffff) as u32
}

/// An engine with Lumen's functions and limits, its callbacks sharing
/// `state`. `max_operations` is [`MAX_OPERATIONS`] but in the tests.
fn engine(state: &Rc<State>, max_operations: u64) -> Engine {
    // Only the first call sets it (later ones get an error back), before
    // any engine exists, as rhai requires.
    let _ = rhai::config::hashing::set_hashing_seed(Some(HASHING_SEED));
    let mut e = Engine::new();
    e.set_max_operations(max_operations)
        .set_max_call_levels(MAX_CALL_LEVELS)
        .set_max_expr_depths(MAX_EXPR_DEPTH, MAX_FUNCTION_EXPR_DEPTH)
        .set_max_string_size(MAX_STRING)
        .set_max_array_size(MAX_ARRAY)
        .set_max_map_size(MAX_MAP);

    let s = state.clone();
    e.on_print(move |text| {
        s.with_console(|c| writeln!(c, "{text}"));
    });
    let s = state.clone();
    e.on_debug(move |text, _, _| {
        s.with_console(|c| writeln!(c, "[debug] {text}"));
    });
    let s = state.clone();
    e.on_progress(move |ops| {
        let stop = s.stopped.get() || (ops % POLL_EVERY == 0 && s.ctrl_c());
        stop.then(|| Dynamic::from("Ctrl-C"))
    });

    e.register_fn("cpus", cpus);
    e.register_fn("mem", mem);
    e.register_fn("uptime", || (crate::time::now_ns() / 1_000_000) as i64);
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
    let s = state.clone();
    e.register_fn("screen_width", move || -> Result<i64, Box<EvalAltResult>> {
        let mut w = 0;
        s.draw(|cv| w = cv.width())?;
        Ok(w as i64)
    });
    let s = state.clone();
    e.register_fn("screen_height", move || -> Result<i64, Box<EvalAltResult>> {
        let mut h = 0;
        s.draw(|cv| h = cv.height())?;
        Ok(h as i64)
    });
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

/// `trace(n)`: the boot CPU's last `n` trace events, a string each.
fn trace(n: i64) -> Array {
    let mut text = String::new();
    let _ = crate::trace::write_recent(&mut text, n.max(0) as u64, "");
    text.lines().map(|l| Dynamic::from(l.to_string())).collect()
}

/// `sleep(ms)`: wait (at most [`MAX_SLEEP_MS`]), in 10 ms steps so Ctrl-C
/// ends it early.
fn sleep(state: &State, ms: i64) {
    let end = crate::time::now_ns() + ms.clamp(0, MAX_SLEEP_MS) as u64 * 1_000_000;
    loop {
        let now = crate::time::now_ns();
        if now >= end || state.ctrl_c() {
            return;
        }
        crate::time::sleep((end - now).min(10_000_000));
    }
}

/// One script session: an engine and the variables its lines left.
struct Session {
    state: Rc<State>,
    engine: Engine,
    scope: Scope<'static>,
}

impl Session {
    fn new(max_operations: u64) -> Session {
        let state = Rc::new(State::default());
        let engine = engine(&state, max_operations);
        Session { state, engine, scope: Scope::new() }
    }

    /// Evaluate `source` on `console`, then show its value (unless it is
    /// `()`) or its error. If it drew, wait for a key before giving the
    /// screen back.
    fn run(&mut self, source: &str, console: &mut dyn Console) {
        self.state.stopped.set(false);
        let (engine, scope) = (&self.engine, &mut self.scope);
        let result = self.state.lend(console, || engine.eval_with_scope::<Dynamic>(scope, source));
        let _ = match result {
            Ok(v) if v.is_unit() => Ok(()),
            Ok(v) => writeln!(console, "{v}"),
            Err(e) => match *e {
                EvalAltResult::ErrorTerminated(..) => writeln!(console, "rhai: stopped (Ctrl-C)"),
                e => writeln!(console, "rhai: {e}"),
            },
        };
        if self.state.canvas.borrow().is_some() {
            let _ = writeln!(console, "rhai: press a key");
            console.read_key();
            self.state.canvas.borrow_mut().take();
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
    /// Evaluate a script, with this cap on operations.
    Eval { source: String, max_operations: u64 },
}

/// Run `job` on `console` in a thread of its own with a [`SCRIPT_STACK`]
/// stack (rhai recurses deeply: a shell thread's 64 KiB holds only a few
/// call levels), waiting for it to end.
fn in_thread(console: &mut dyn Console, job: Job) {
    /// The console, its lifetime erased, sent to the script thread.
    struct Lent(NonNull<dyn Console + 'static>);
    // SAFETY: the console is used by one thread at a time: the script
    // thread, while the thread that lent it waits in `join` (below).
    unsafe impl Send for Lent {}
    impl Lent {
        /// # Safety
        /// The lender must still hold the borrow and not use it.
        unsafe fn console<'a>(self) -> &'a mut dyn Console {
            // SAFETY: as the caller promises.
            unsafe { &mut *self.0.as_ptr() }
        }
    }
    let ptr = NonNull::from(&mut *console);
    // SAFETY: only the trait object's lifetime bound changes (same layout);
    // the script thread is done with the pointer before `join` returns,
    // while `console`'s borrow is still held here.
    let lent = Lent(unsafe { core::mem::transmute::<NonNull<dyn Console + '_>, NonNull<dyn Console + 'static>>(ptr) });
    let spawned = crate::sched::spawn_sized("rhai", SCRIPT_STACK, move || {
        // SAFETY: the lender waits in `join` until this thread ends.
        let console = unsafe { lent.console() };
        match job {
            Job::Repl { ansi } => repl(console, ansi),
            Job::Eval { source, max_operations } => Session::new(max_operations).run(&source, console),
        }
    });
    match spawned {
        Ok(thread) => thread.join(),
        Err(e) => {
            let _ = writeln!(console, "rhai: no thread for the script: {e}");
        }
    }
}

/// The shell's `rhai` command.
pub fn command(ctx: &mut Ctx<'_>, args: &[String]) {
    let eval = |source: String| Job::Eval { source, max_operations: MAX_OPERATIONS };
    let job = match args {
        [] => Job::Repl { ansi: ctx.ansi },
        [flag, rest @ ..] if flag == "-e" && !rest.is_empty() => eval(rest.join(" ")),
        [demo] if demo == "demo" => {
            let mut text = String::from("demos (rhai demo <name>):\n");
            for (name, about, _) in DEMOS {
                let _ = writeln!(text, "  {name:<10} {about}");
            }
            let _ = ctx.out.write_str(&text);
            return;
        }
        [demo, name] if demo == "demo" => match DEMOS.iter().find(|d| d.0 == name.as_str()) {
            Some((_, _, source)) => eval(source.to_string()),
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
    in_thread(ctx.out, job);
}

/// `rhai` alone: read lines with the shell's line editor and evaluate each,
/// keeping variables between them, until `exit` (or Ctrl-D on an empty
/// line).
fn repl(console: &mut dyn Console, ansi: bool) {
    const PROMPT: &str = "rhai> ";
    let mode = if ansi { Mode::Ansi } else { Mode::Plain };
    // Like the shell's lines: no wider than the console (the screen's, or
    // 80 columns for serial with escape codes); plain serial doesn't care.
    let max = match mode {
        Mode::Ansi => term::status().map_or(80, |t| t.cols).min(80).saturating_sub(PROMPT.len() + 3),
        Mode::Plain => 160,
    };
    let mut editor = Editor::new(PROMPT, mode, max, 50);
    let mut session = Session::new(MAX_OPERATIONS);
    let mut shown = String::new();
    let banner = FUNCTIONS.join(", ");
    let _ = writeln!(console, "rhai: Lumen's functions are {banner}; 'exit' or Ctrl-D leaves");
    loop {
        let _ = editor.start(&mut shown);
        flush(console, &mut shown);
        let line = loop {
            let key = console.read_key();
            if key == Key::Ctrl('d') && editor.line().is_empty() {
                let _ = writeln!(console);
                return;
            }
            let event = editor.key(key, &Functions, &mut shown);
            flush(console, &mut shown);
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

fn flush(console: &mut dyn Console, shown: &mut String) {
    if !shown.is_empty() {
        let _ = console.write_str(shown);
        shown.clear();
    }
}

/// Evaluate `source` with `max_operations` on a captured console, `keys`
/// typed on it beforehand; what it printed (for the tests), then
/// `[ran dry]` if it waited for a key after `keys` ran out.
#[cfg(feature = "ktest")]
fn run_captured(source: &str, max_operations: u64, keys: &[Key]) -> String {
    let mut console = crate::shell::Captured::default();
    console.keys.extend(keys.iter().copied());
    in_thread(&mut console, Job::Eval { source: source.to_string(), max_operations });
    if console.ran_dry {
        console.text.push_str("[ran dry]");
    }
    console.text
}

crate::ktest!(
    fn lumen_functions_answer() {
        use crate::{kassert, kassert_eq};
        use alloc::format;
        let online = (0..crate::arch::MAX_CPUS).filter(|&i| crate::smp::online_lapic_id(i).is_some()).count();
        kassert_eq!(run_captured("cpus().len()", 10_000, &[]), format!("{online}\n"));
        kassert_eq!(run_captured("cpus()[0].index", 10_000, &[]), "0\n");
        kassert_eq!(run_captured("let m = mem(); m.heap_used + m.heap_free == m.heap_size", 10_000, &[]), "true\n");
        kassert_eq!(run_captured("print(6 * 7); uptime() > 0", 10_000, &[]), "42\ntrue\n");
        kassert_eq!(run_captured("trace(3).len()", 10_000, &[]), "3\n");
        kassert!(run_captured("frobnicate()", 10_000, &[]).starts_with("rhai: Function not found: frobnicate"));
        kassert!(DEMOS.iter().map(|d| d.0).eq(DEMO_NAMES.iter().copied()));
        Ok(())
    }
);

crate::ktest!(
    fn operations_are_capped_and_ctrl_c_stops() {
        // A runaway loop ends at the operation cap, or at once on Ctrl-C
        // (typed before it started: the first poll sees it); a key that
        // isn't Ctrl-C doesn't stop it. `sleep` stops early on Ctrl-C too.
        use crate::{kassert, kassert_eq};
        let capped = run_captured("loop {}", 100_000, &[]);
        crate::ktest::diag(format_args!("capped: {capped}"));
        kassert!(capped.starts_with("rhai: Too many operations"));
        let stopped = run_captured("loop {}", MAX_OPERATIONS, &[Key::Char('x'), Key::Ctrl('c')]);
        kassert_eq!(stopped, "rhai: stopped (Ctrl-C)\n");
        kassert!(run_captured("loop {}", 100_000, &[Key::Char('x')]).starts_with("rhai: Too many operations"));
        let start = crate::time::now_ns();
        kassert_eq!(run_captured("sleep(5000); 1", MAX_OPERATIONS, &[Key::Ctrl('c')]), "rhai: stopped (Ctrl-C)\n");
        kassert!(crate::time::now_ns() - start < 1_000_000_000);
        Ok(())
    }
);

crate::ktest!(
    fn deep_recursion_is_an_error_not_a_crash() {
        // Runaway recursion (inside nested expressions) and too deeply
        // nested expressions are errors, not a guard-page crash; the deepest
        // expression allowed, in a function at the deepest call allowed,
        // runs.
        use crate::{kassert, kassert_eq};
        let nested = |depth| alloc::format!("{}1{}", "(".repeat(depth), ")".repeat(depth));
        let calls = run_captured("fn f(n) { 1 + (2 * (3 + [f(n + 1)][0])) } f(0)", MAX_OPERATIONS, &[]);
        kassert_eq!(calls.split(" (").next(), Some("rhai: Stack overflow"));
        let parsed = run_captured(&nested(200), MAX_OPERATIONS, &[]);
        kassert_eq!(parsed.split(" (").next(), Some("rhai: Syntax error: Expression exceeds maximum complexity"));
        // The deepest expression the parser takes there, found by trying.
        let at_depth = |depth| {
            alloc::format!(
                "fn g(n) {{ if n < {} {{ g(n + 1) }} else {{ [{}][0] }} }} g(1)",
                MAX_CALL_LEVELS - 1,
                nested(depth)
            )
        };
        let mut depth = 0;
        let mut ran = alloc::string::String::new();
        while depth < MAX_EXPR_DEPTH {
            let text = run_captured(&at_depth(depth + 1), MAX_OPERATIONS, &[]);
            if text.contains("exceeds maximum complexity") {
                break;
            }
            (depth, ran) = (depth + 1, text);
        }
        crate::ktest::diag(format_args!("deepest expression at the deepest call: {depth} parentheses"));
        kassert!(depth > 0);
        kassert_eq!(ran, "1\n");
        Ok(())
    }
);

crate::ktest!(
    fn a_script_that_drew_waits_for_a_key() {
        // Drawing takes the canvas; after the script the console is asked
        // for a key, then the canvas is given back.
        use crate::{kassert, kassert_eq};
        let text = run_captured("draw_rect(0, 0, 10, 10, 0xff0000); screen_width() > 0", 10_000, &[Key::Char(' ')]);
        kassert_eq!(text, "true\nrhai: press a key\n");
        kassert!(term::canvas().is_ok());
        Ok(())
    }
);
