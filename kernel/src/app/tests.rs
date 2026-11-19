//! The runtime's ktests (#199): load refusals over hand-assembled modules,
//! a kill ending the app at a host call's entry however the call is made,
//! fuel, the code, memory and value-stack charges against the heap's
//! high-water mark, `trace_read`'s edges, `kill`'s lines, `launch`'s
//! refusals and the reaper.
//!
//! The charge tests read the global heap's counters: they assume nothing
//! else allocates meanwhile (ktests run one at a time, no app runs, and
//! the other CPUs' threads are idle in the ktest kernel). A test that
//! launches an app waits for it to be reaped before it returns.

use super::modules::{self, leb128};
use super::runtime::{self, AppState, Phase};
use super::{BUNDLED, TEST_APPS, host};
use crate::mm::heap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::hint::black_box;
use lumen_app_api::end::{EndReason, Outcome};
use lumen_app_api::host::{FUNCTIONS, MODULE, Ty};
use lumen_app_api::limits::{
    APP_OVERHEAD, BYTE_FUEL, CALL_FUEL, CODE_FACTOR, LOG_FUEL, MEMORY_PAGES, MODULE_BYTES, PAGE_BYTES, SLICE_FUEL,
    STORE_BYTES, TABLE_ELEMENTS, VALUE_STACK_BYTES,
};
use lumen_app_api::{Error, Grants, manifest, text};
use wasmi::{Engine, Linker, Module, Store};

// --- A small module builder: just what the cases need. ---

fn leb(n: u32) -> Vec<u8> {
    let mut out = Vec::new();
    leb128(&mut out, n);
    out
}

fn name(s: &str) -> Vec<u8> {
    let mut out = leb(s.len() as u32);
    out.extend_from_slice(s.as_bytes());
    out
}

fn section(id: u8, items: &[Vec<u8>]) -> Vec<u8> {
    let mut body = leb(items.len() as u32);
    for i in items {
        body.extend_from_slice(i);
    }
    let mut out = vec![id];
    out.extend(leb(body.len() as u32));
    out.extend(body);
    out
}

fn custom(section_name: &str, payload: &[u8]) -> Vec<u8> {
    let mut body = name(section_name);
    body.extend_from_slice(payload);
    let mut out = vec![0];
    out.extend(leb(body.len() as u32));
    out.extend(body);
    out
}

/// A memory's (or table's) limits: `min`, and `max` if any.
fn limits(min: u32, max: Option<u32>) -> Vec<u8> {
    match max {
        Some(max) => [vec![0x01], leb(min), leb(max)].concat(),
        None => [vec![0x00], leb(min)].concat(),
    }
}

/// A function type's bytes.
fn func_type(params: &[u8], results: &[u8]) -> Vec<u8> {
    [vec![0x60], leb(params.len() as u32), params.to_vec(), leb(results.len() as u32), results.to_vec()].concat()
}

/// A host function's signature as a function type's bytes.
fn host_type(params: &[Ty], results: &[Ty]) -> Vec<u8> {
    let val = |t: &Ty| match t {
        Ty::I32 => 0x7f,
        Ty::I64 => 0x7e,
    };
    func_type(&params.iter().map(val).collect::<Vec<_>>(), &results.iter().map(val).collect::<Vec<_>>())
}

/// What a test module holds; [`Spec::bytes`] assembles it.
#[derive(Clone)]
struct Spec {
    /// Types past the first two (`lumen_main`'s, `[] -> []`): index 2 on.
    types: Vec<Vec<u8>>,
    /// Imports' bytes (module, name, description).
    imports: Vec<Vec<u8>>,
    /// How many of `imports` are functions (they come first in the
    /// function index space).
    func_imports: u32,
    /// The function `lumen_main` exports: `None`, the first one defined
    /// (index `func_imports`).
    main_export: Option<u32>,
    memories: Vec<(u32, Option<u32>)>,
    tables: Vec<(u32, Option<u32>)>,
    /// `lumen_main`'s type: results (`[i32]` normally).
    main_results: Vec<u8>,
    export_memory: bool,
    export_main: bool,
    start: bool,
    /// `lumen_main`'s body (without locals or `end`).
    body: Vec<u8>,
    manifests: Vec<&'static [u8]>,
    /// A custom section of this many bytes, to pass a size limit.
    padding: usize,
}

impl Default for Spec {
    fn default() -> Spec {
        Spec {
            types: Vec::new(),
            imports: Vec::new(),
            func_imports: 0,
            main_export: None,
            memories: vec![(1, Some(1))],
            tables: Vec::new(),
            main_results: vec![0x7f],
            export_memory: true,
            export_main: true,
            start: false,
            body: vec![0x41, 0x00], // i32.const 0
            manifests: vec![b"caps="],
            padding: 0,
        }
    }
}

const FUNC_TYPE_MAIN: u8 = 0; // [] -> results
const FUNC_TYPE_NONE: u8 = 1; // [] -> []

impl Spec {
    /// A spec importing these `lumen` functions (types 2.., in order).
    fn importing(names: &[&str]) -> Spec {
        let mut s = Spec::default();
        for (i, n) in names.iter().enumerate() {
            let f = lumen_app_api::host::lookup(n).expect("a host function");
            s.types.push(host_type(f.params, f.results));
            s.imports.push(func_import(MODULE, n, 2 + i as u8));
        }
        s.func_imports = names.len() as u32;
        s
    }

    fn bytes(&self) -> Vec<u8> {
        let mut m = b"\0asm\x01\0\0\0".to_vec();
        let main_ty = func_type(&[], &self.main_results);
        let types = [vec![main_ty, func_type(&[], &[])], self.types.clone()].concat();
        m.extend(section(1, &types));
        if !self.imports.is_empty() {
            m.extend(section(2, &self.imports));
        }
        // Functions: lumen_main (type 0), and a start function (type 1).
        let funcs =
            if self.start { vec![vec![FUNC_TYPE_MAIN], vec![FUNC_TYPE_NONE]] } else { vec![vec![FUNC_TYPE_MAIN]] };
        m.extend(section(3, &funcs));
        if !self.tables.is_empty() {
            let tables: Vec<Vec<u8>> =
                self.tables.iter().map(|&(min, max)| [vec![0x70], limits(min, max)].concat()).collect();
            m.extend(section(4, &tables));
        }
        if !self.memories.is_empty() {
            let mems: Vec<Vec<u8>> = self.memories.iter().map(|&(min, max)| limits(min, max)).collect();
            m.extend(section(5, &mems));
        }
        let mut exports = Vec::new();
        if self.export_memory {
            exports.push([name("memory"), vec![0x02, 0x00]].concat());
        }
        if self.export_main {
            exports.push([name("lumen_main"), vec![0x00], leb(self.main_export.unwrap_or(self.func_imports))].concat());
        }
        m.extend(section(7, &exports));
        if self.start {
            let mut start = vec![8];
            let idx = leb(self.func_imports + 1);
            start.extend(leb(idx.len() as u32));
            start.extend(idx);
            m.extend(start);
        }
        let main_body = [vec![0x00], self.body.clone(), vec![0x0b]].concat();
        let mut bodies = vec![[leb(main_body.len() as u32), main_body].concat()];
        if self.start {
            bodies.push(vec![0x02, 0x00, 0x0b]);
        }
        m.extend(section(10, &bodies));
        for manifest in &self.manifests {
            m.extend(custom(manifest::SECTION, manifest));
        }
        if self.padding > 0 {
            m.extend(custom("padding", &vec![0; self.padding]));
        }
        m
    }
}

/// An import of `module.field`, a function of type `ty`.
fn func_import(module: &str, field: &str, ty: u8) -> Vec<u8> {
    [name(module), name(field), vec![0x00, ty]].concat()
}

/// A test app's state (not in the table).
fn test_state() -> Arc<AppState> {
    Arc::new(AppState::new(0, "test"))
}

/// What `run` does with a module before its thread runs it, then the load
/// itself (on this thread): the reason it is refused, if it is.
fn try_load(bytes: &[u8]) -> Result<(), String> {
    runtime::precheck(bytes)?;
    runtime::load(bytes, runtime::new_ctx(test_state(), Grants::log(), Vec::new())).map(|_| ())
}

crate::ktest!(
    fn load_refuses_bad_modules() {
        crate::kassert!(try_load(&Spec::default().bytes()).is_ok());
        // (what, the module, a part of the reason it must give)
        let mut cases: Vec<(&str, Spec, &str)> = Vec::new();
        let mut add = |what, why, f: &dyn Fn(&mut Spec)| {
            let mut s = Spec::default();
            f(&mut s);
            cases.push((what, s, why));
        };
        add("an import from another module", "import env.f: only `lumen` imports", &|s| {
            s.imports = vec![func_import("env", "f", FUNC_TYPE_MAIN)];
            s.func_imports = 1;
        });
        add("an unknown lumen name", "import lumen.nope: no such host function", &|s| {
            s.imports = vec![func_import("lumen", "nope", FUNC_TYPE_MAIN)];
            s.func_imports = 1;
        });
        add("a lumen name with another signature", "import lumen.log: another signature", &|s| {
            s.imports = vec![func_import("lumen", "log", FUNC_TYPE_MAIN)];
            s.func_imports = 1;
        });
        add("an imported memory", "import lumen.mem: not a function", &|s| {
            // Its only memory, so this isn't a second memory too.
            s.imports = vec![[name("lumen"), name("mem"), vec![0x02], limits(1, Some(1))].concat()];
            s.memories = vec![];
        });
        add("an imported table", "import lumen.t: not a function", &|s| {
            s.imports = vec![[name("lumen"), name("t"), vec![0x01, 0x70], limits(1, None)].concat()]
        });
        add("an imported global", "import lumen.g: not a function", &|s| {
            s.imports = vec![[name("lumen"), name("g"), vec![0x03, 0x7f, 0x00]].concat()]
        });
        add("a second memory", "not valid: ", &|s| s.memories = vec![(1, Some(1)), (1, Some(1))]);
        add("a second table", "table", &|s| s.tables = vec![(1, Some(1)), (1, Some(1))]);
        add("a start section", "not valid: ", &|s| s.start = true);
        add("no memory export", "no export memory", &|s| s.export_memory = false);
        add("no lumen_main", "no export lumen_main", &|s| s.export_main = false);
        add("lumen_main of another signature", "export lumen_main: not [] -> [i32]", &|s| {
            s.main_results = vec![];
            s.body = vec![];
        });
        add("a memory without a maximum", "memory: not fixed", &|s| s.memories = vec![(1, None)]);
        add("a maximum unlike its initial size", "memory: not fixed", &|s| s.memories = vec![(1, Some(2))]);
        add("past MEMORY_PAGES", "memory: 257 pages, past 256", &|s| {
            s.memories = vec![(MEMORY_PAGES as u32 + 1, Some(MEMORY_PAGES as u32 + 1))]
        });
        add("past TABLE_ELEMENTS", "table", &|s| s.tables = vec![(TABLE_ELEMENTS as u32 + 1, None)]);
        add("no manifest", "no lumen.manifest section", &|s| s.manifests = vec![]);
        add("two manifests", "two lumen.manifest sections", &|s| s.manifests = vec![b"caps=", b"caps="]);
        add("an unknown grant", "the manifest names an unknown grant `log`", &|s| s.manifests = vec![b"caps=log"]);
        add("past MODULE_BYTES", "too big (", &|s| s.padding = MODULE_BYTES);
        // Well framed, with a manifest, but `lumen_main` leaves an i64
        // where it returns an i32: wasmi's validation refuses it.
        add("invalid wasm", "not valid: ", &|s| s.body = vec![0x42, 0x00]);

        // Each case twice: the first refusal may allocate for good (the
        // log's line, a lazily built table), the second must leave the heap
        // as it found it.
        let mut wrong = 0;
        let mut leaked = 0;
        for (what, spec, want) in &cases {
            let bytes = spec.bytes();
            let reason = try_load(&bytes).err();
            let before = heap::stats().used;
            let again = try_load(&bytes);
            drop(again);
            let after = heap::stats().used;
            drop(bytes);
            let why = reason.unwrap_or_else(|| String::from("ACCEPTED"));
            crate::ktest::diag(format_args!("{what}: {why}"));
            if !why.contains(want) {
                crate::ktest::diag(format_args!("{what}: wanted {want:?}"));
                wrong += 1;
            }
            if after != before {
                crate::ktest::diag(format_args!("{what}: heap used {before} before, {after} after"));
                leaked += 1;
            }
        }
        crate::kassert_eq!(wrong, 0);
        crate::kassert_eq!(leaked, 0);
        Ok(())
    }
);

/// Instantiate `bytes` against `define`'s functions alone (no checks of
/// ours): whether wasmi links it.
fn links(bytes: &[u8]) -> bool {
    let engine = Engine::new(&runtime::app_config());
    let Ok(module) = Module::new(&engine, bytes) else { return false };
    let mut linker = Linker::new(&engine);
    if host::define(&mut linker).is_err() {
        return false;
    }
    let mut store = Store::new(&engine, runtime::new_ctx(test_state(), Grants::log(), Vec::new()));
    linker.instantiate_and_start(&mut store, &module).is_ok()
}

crate::ktest!(
    fn every_host_function_is_defined_as_listed() {
        // A module importing every one with its listed signature links
        // (wasmi matches each import's name and type against `define`'s).
        let names: Vec<&str> = FUNCTIONS.iter().map(|f| f.name).collect();
        crate::kassert!(links(&Spec::importing(&names).bytes()));
        crate::kassert!(try_load(&Spec::importing(&names).bytes()).is_ok());
        // Which it would not with another type, or a name `define` lacks.
        let mut wrong_type = Spec::importing(&["args_len"]);
        wrong_type.types = vec![host_type(&[Ty::I32], &[Ty::I32])];
        crate::kassert!(!links(&wrong_type.bytes()));
        let mut unknown = Spec::importing(&["args_len"]);
        unknown.imports = vec![func_import(MODULE, "window_open", 2)];
        crate::kassert!(!links(&unknown.bytes()));
        Ok(())
    }
);

/// `lumen_main` calling `args_len` (`[] -> [i32]`, like `lumen_main`):
/// by `call`, by `return_call`, or `lumen_main` the import itself.
fn probes() -> [(&'static str, Spec); 3] {
    let call = Spec { body: vec![0x10, 0x00], ..Spec::importing(&["args_len"]) };
    let tail = Spec { body: vec![0x12, 0x00], ..Spec::importing(&["args_len"]) };
    let exported = Spec { main_export: Some(0), ..Spec::importing(&["args_len"]) };
    [("call", call), ("return_call", tail), ("an exported import", exported)]
}

crate::ktest!(
    fn a_kill_at_a_host_call_is_never_a_trap() {
        for (how, spec) in probes() {
            let bytes = spec.bytes();
            // Without a kill: args_len's 0 is lumen_main's result.
            let state = test_state();
            let (mut store, main) =
                runtime::load(&bytes, runtime::new_ctx(state, Grants::log(), Vec::new())).map_err(|_| "no load")?;
            let ended = runtime::run_slices(&mut store, main);
            crate::ktest::diag(format_args!("{how}: {}", ended.what()));
            crate::kassert_eq!(ended, EndReason::Exited(0));
            // With the kill flag set: the call's entry ends the app, and
            // it's a kill however wasmi hands the host error back.
            let state = test_state();
            let (mut store, main) = runtime::load(&bytes, runtime::new_ctx(state.clone(), Grants::log(), Vec::new()))
                .map_err(|_| "no load")?;
            state.ask_kill();
            let ended = runtime::run_slices(&mut store, main);
            crate::ktest::diag(format_args!("{how}, killed: {}", ended.what()));
            crate::kassert_eq!(ended, EndReason::Killed);
        }
        Ok(())
    }
);

/// Run `bytes`' `lumen_main` once with a slice's fuel: its result and the
/// fuel it used.
fn fuel_used(bytes: &[u8]) -> Result<(i32, u64), &'static str> {
    let (mut store, main) =
        runtime::load(bytes, runtime::new_ctx(test_state(), Grants::log(), Vec::new())).map_err(|_| "no load")?;
    store.set_fuel(SLICE_FUEL).map_err(|_| "no fuel")?;
    let result = main.call(&mut store, ()).map_err(|_| "lumen_main trapped")?;
    Ok((result, SLICE_FUEL - store.get_fuel().map_err(|_| "no fuel")?))
}

crate::ktest!(
    fn host_calls_charge_their_fuel() {
        // 50 calls to args_len, each `call 0; drop`: each costs CALL_FUEL,
        // far more than the module's own instructions.
        let n = 50u64;
        let body: Vec<u8> = (0..n).flat_map(|_| [0x10, 0x00, 0x1a]).chain([0x41, 0x00]).collect();
        let (_, used) = fuel_used(&Spec { body, ..Spec::importing(&["args_len"]) }.bytes())?;
        crate::ktest::diag(format_args!("{n} args_len calls: {used} fuel"));
        crate::kassert!(used >= n * CALL_FUEL);
        // handle_find(Log), then one `log` of 60 bytes (NULs, at TRACE so
        // nothing prints): two calls, the line and its bytes. (Each
        // `i32.const` below 64, a one-byte signed LEB128.)
        let log = |level: u8| {
            let body = vec![0x41, 0x01, 0x10, 0x00, 0x41, level, 0x41, 0x00, 0x41, 60, 0x10, 0x01];
            Spec { body, ..Spec::importing(&["handle_find", "log"]) }.bytes()
        };
        let (result, used) = fuel_used(&log(5))?;
        crate::ktest::diag(format_args!("a 60-byte log: {result}, {used} fuel"));
        crate::kassert_eq!(result, 0);
        crate::kassert!(used >= 2 * CALL_FUEL + LOG_FUEL + 60 * BYTE_FUEL);
        // A refused one (level 0) still costs its call, and no line.
        let (result, used) = fuel_used(&log(0))?;
        crate::ktest::diag(format_args!("a refused log: {result}, {used} fuel"));
        crate::kassert_eq!(result, Error::BadArgument.returned());
        crate::kassert!((2 * CALL_FUEL..2 * CALL_FUEL + LOG_FUEL).contains(&used));
        Ok(())
    }
);

crate::ktest!(
    fn a_kill_while_loading_runs_nothing() {
        let state = test_state();
        state.ask_kill();
        runtime::live(&probes()[0].1.bytes(), &state, Grants::log(), Vec::new());
        crate::kassert_eq!(state.outcome(), Some(Outcome::Ended(EndReason::Killed)));
        crate::kassert!(!state.ran());
        crate::kassert_eq!(state.slices(), 0);
        Ok(())
    }
);

crate::ktest!(
    fn a_load_failure_is_kept_for_its_watcher() {
        // Watched (as `run` does): recorded, not logged, handed to the
        // watcher once.
        let state = test_state();
        runtime::live(&Spec { export_main: false, ..Spec::default() }.bytes(), &state, Grants::log(), Vec::new());
        let why = text::log_line("no export lumen_main");
        crate::kassert_eq!(state.outcome(), Some(Outcome::NotStarted(why.clone())));
        crate::kassert!(!state.ran());
        crate::kassert_eq!(state.stop_watching(), Some(why));
        crate::kassert_eq!(state.stop_watching(), None);
        Ok(())
    }
);

crate::ktest!(
    fn kill_says_where_the_app_is() {
        let ended = |outcome: Outcome| {
            let s = test_state();
            s.record(outcome);
            s.set_phase(Phase::Ended);
            runtime::kill_outcome(&s)
        };
        crate::kassert_eq!(ended(EndReason::Killed.into()), "killed");
        crate::kassert_eq!(ended(EndReason::Exited(0).into()), "had already ended: exited with 0");
        crate::kassert_eq!(
            ended(Outcome::NotStarted(text::log_line("memory: not fixed"))),
            "had already ended: didn't start: memory: not fixed"
        );
        let places = [
            (Phase::Gated, "loading"),
            (Phase::Loading, "loading"),
            (Phase::Running, "in a slice"),
            (Phase::Waiting("time_sleep"), "in time_sleep"),
            (Phase::Ending, "ending"),
        ];
        for (phase, place) in places {
            let s = test_state();
            s.set_phase(phase);
            crate::kassert_eq!(runtime::kill_outcome(&s), alloc::format!("still ending ({place})"));
        }
        // Killed, but its thread not yet done with it: still ending.
        let s = test_state();
        s.record(EndReason::Killed);
        crate::kassert_eq!(runtime::kill_outcome(&s), "still ending (ending)");
        Ok(())
    }
);

crate::ktest!(
    fn running_out_of_memory_is_not_enough_memory() {
        use wasmi::TrapCode;
        use wasmi::errors::{InstantiationError, MemoryError, TableError};
        // How wasmi says it, wherever it allocates fallibly.
        let oom = [
            wasmi::Error::from(MemoryError::OutOfSystemMemory),
            wasmi::Error::from(TableError::OutOfSystemMemory),
            wasmi::Error::from(InstantiationError::FailedToInstantiateMemory(MemoryError::OutOfSystemMemory)),
            wasmi::Error::from(InstantiationError::FailedToInstantiateTable(TableError::OutOfSystemMemory)),
            wasmi::Error::from(TrapCode::OutOfSystemMemory),
        ];
        crate::kassert!(oom.iter().all(runtime::out_of_memory));
        let other = [
            wasmi::Error::from(MemoryError::OutOfBoundsGrowth),
            wasmi::Error::from(InstantiationError::FailedToInstantiateMemory(MemoryError::OutOfBoundsGrowth)),
            wasmi::Error::from(TrapCode::UnreachableCodeReached),
        ];
        crate::kassert!(!other.iter().any(runtime::out_of_memory));
        // And the real path: a 16-page memory the heap refuses.
        let bytes = Spec { memories: vec![(16, Some(16))], ..Spec::default() }.bytes();
        heap::refuse_from(16 * PAGE_BYTES as usize);
        let refused = try_load(&bytes);
        heap::refuse_from(usize::MAX);
        crate::ktest::diag(format_args!("the memory refused: {refused:?}"));
        crate::kassert_eq!(refused, Err(String::from("not enough memory")));
        crate::kassert!(try_load(&bytes).is_ok());
        Ok(())
    }
);

crate::ktest!(
    fn the_heaps_peak_follows_its_use() {
        let used = heap::stats().used;
        heap::reset_peak();
        crate::kassert_eq!(heap::peak(), used);
        let block = black_box(vec![0u8; 64 * 1024]);
        crate::kassert!(heap::peak() >= used + 64 * 1024);
        drop(block);
        crate::kassert_eq!(heap::stats().used, used);
        crate::kassert!(heap::peak() >= used + 64 * 1024);
        heap::reset_peak();
        crate::kassert_eq!(heap::peak(), used);
        Ok(())
    }
);

crate::ktest!(
    fn compiled_code_fits_its_charge() {
        let apps: Vec<_> = BUNDLED.iter().chain(TEST_APPS.iter()).collect();
        crate::kassert!(!apps.is_empty());
        let mut over = 0;
        for app in apps {
            let before = heap::stats().used;
            heap::reset_peak();
            let engine = Engine::new(&runtime::app_config());
            let module = Module::new(&engine, app.module).map_err(|_| "a bundled module didn't compile")?;
            let peak = heap::peak();
            drop(module);
            drop(engine);
            let grew = peak.saturating_sub(before) as u64;
            let charge = CODE_FACTOR * app.module.len() as u64;
            crate::ktest::diag(format_args!(
                "{}: {} bytes, peak {grew} over, charge {charge}",
                app.name,
                app.module.len()
            ));
            if grew > charge {
                over += 1;
            }
        }
        crate::kassert_eq!(over, 0);
        Ok(())
    }
);

crate::ktest!(
    fn a_fixed_memory_is_allocated_once() {
        // A 16-page fixed memory, and `lumen_main` returning
        // `memory.grow(1)`: -1, as the memory can't grow.
        let pages = 16u32;
        // i32.const 1, memory.grow 0
        let spec = Spec { memories: vec![(pages, Some(pages))], body: vec![0x41, 0x01, 0x40, 0x00], ..Spec::default() };
        let bytes = spec.bytes();
        let before = heap::stats().used;
        heap::reset_peak();
        let (mut store, main) = runtime::load(&bytes, runtime::new_ctx(test_state(), Grants::log(), Vec::new()))
            .map_err(|_| "the module didn't load")?;
        let grew = heap::peak().saturating_sub(before) as u64;
        let memory = u64::from(pages) * PAGE_BYTES;
        crate::ktest::diag(format_args!("instantiating: peak {grew} over, memory {memory}"));
        crate::kassert!(grew <= memory + APP_OVERHEAD);
        store.set_fuel(SLICE_FUEL).map_err(|_| "no fuel")?;
        // The first call allocates the value stack, whole: within its
        // charge beside the overhead.
        let before = heap::stats().used;
        heap::reset_peak();
        crate::kassert_eq!(main.call(&mut store, ()).map_err(|_| "lumen_main trapped")?, -1);
        let first = heap::peak().saturating_sub(before) as u64;
        crate::ktest::diag(format_args!("the first call: peak {first} over"));
        crate::kassert!(first <= VALUE_STACK_BYTES as u64 + APP_OVERHEAD);
        // Then the grow must allocate nothing.
        let used = heap::stats().used;
        heap::reset_peak();
        crate::kassert_eq!(main.call(&mut store, ()).map_err(|_| "lumen_main trapped")?, -1);
        crate::kassert_eq!(heap::stats().used, used);
        crate::kassert_eq!(heap::peak(), used);
        Ok(())
    }
);

crate::ktest!(
    fn the_value_stack_is_allocated_once_at_its_full_size() {
        // `depth(n)` recurses n deep (modules::LOOP), on an app's engine.
        let engine = Engine::new(&runtime::app_config());
        let module = Module::new(&engine, modules::LOOP).map_err(|_| "LOOP didn't compile")?;
        let mut store = Store::new(&engine, ());
        let instance = Linker::new(&engine).instantiate_and_start(&mut store, &module).map_err(|_| "no instance")?;
        let depth = instance.get_typed_func::<i32, i32>(&store, "depth").map_err(|_| "no depth")?;
        store.set_fuel(SLICE_FUEL).map_err(|_| "no fuel")?;
        // A shallow first call allocates the whole value stack.
        let before = heap::stats().used;
        heap::reset_peak();
        crate::kassert_eq!(depth.call(&mut store, 1).map_err(|_| "depth(1) trapped")?, 1);
        let first = heap::peak().saturating_sub(before);
        crate::ktest::diag(format_args!("depth(1): peak {first} over"));
        crate::kassert!(first >= VALUE_STACK_BYTES);
        // 900 deep the first time grows only wasmi's call stack (its
        // frames, within the store's part of the overhead), never the value
        // stack again ...
        let used = heap::stats().used;
        heap::reset_peak();
        crate::kassert_eq!(depth.call(&mut store, 900).map_err(|_| "depth(900) trapped")?, 900);
        let frames = heap::peak().saturating_sub(used) as u64;
        crate::ktest::diag(format_args!("depth(900): peak {frames} over"));
        crate::kassert!(frames <= STORE_BYTES);
        // ... and nothing at all after that.
        let used = heap::stats().used;
        heap::reset_peak();
        crate::kassert_eq!(depth.call(&mut store, 900).map_err(|_| "depth(900) trapped")?, 900);
        crate::kassert_eq!(heap::peak(), used);
        Ok(())
    }
);

crate::ktest!(
    fn trace_read_edges() {
        use lumen_app_api::limits::TRACE_EVENTS_PER_READ;
        use lumen_app_api::trace::{EVENT_SIZE, HEADER_SIZE, decode_event, decode_header};
        // CPU 1's ring, not the boot CPU's: lapping that one would break
        // the trace ktests that read the boot's events from it.
        const CPU: usize = 1;
        crate::kassert!(crate::sched::running_on(CPU));
        let cpu = CPU as i32;
        let one = HEADER_SIZE + EVENT_SIZE;
        // Too small, and exactly one event.
        crate::kassert_eq!(host::read_trace(0, cpu, 0, one - 1), Err(Error::BufferTooSmall));
        crate::kassert!(host::read_trace(0, cpu, 0, one).is_ok());
        // Unknown streams and CPUs, negative values.
        for (stream, c, since) in [(2, cpu, 0), (-1, cpu, 0), (0, -1, 0), (0, 99, 0), (0, cpu, -1)] {
            crate::kassert_eq!(host::read_trace(stream, c, since, one), Err(Error::BadArgument));
        }
        // Enough events on that CPU's main ring to pass one read's cap,
        // and to lap the ring, so a read from 0 has missed some: recorded
        // by a thread on it.
        let lap = crate::sched::spawn_on(CPU, "trace-lap", || {
            for i in 0..crate::trace::EVENTS as u64 + 10 {
                crate::ktrace!(crate::trace::Kind::Test, 0x199, i);
            }
        })
        .map_err(|_| "no thread on CPU 1")?;
        lap.join();
        // Every read: a header, then whole events numbered on from
        // `since + missed`, and `next` just past the last.
        let read = |since: u64, len: usize| -> Result<(usize, u64, u64), &'static str> {
            let bytes = host::read_trace(0, cpu, since as i64, len).map_err(|_| "a read failed")?;
            if bytes.len() < HEADER_SIZE || !(bytes.len() - HEADER_SIZE).is_multiple_of(EVENT_SIZE) {
                return Err("not a header and whole events");
            }
            let (next, missed) = decode_header(bytes[..HEADER_SIZE].try_into().unwrap());
            let copied = (bytes.len() - HEADER_SIZE) / EVENT_SIZE;
            for (i, e) in bytes[HEADER_SIZE..].as_chunks::<EVENT_SIZE>().0.iter().enumerate() {
                if decode_event(e).seq != since + missed + i as u64 {
                    return Err("events not numbered on from since + missed");
                }
            }
            if next != since + missed + copied as u64 {
                return Err("next isn't since + missed + copied");
            }
            Ok((copied, next, missed))
        };
        // Room for two: two copied.
        crate::kassert_eq!(read(0, HEADER_SIZE + 2 * EVENT_SIZE)?.0, 2);
        // From 0 with room for everything: the cap, and the numbers the
        // ring has lapped counted missed.
        let (copied, mut since, missed) = read(0, usize::MAX / 2)?;
        crate::kassert_eq!(copied, TRACE_EVENTS_PER_READ);
        crate::kassert!(missed > 0);
        // A `since` past the ring's next number is refused; one at `next`
        // reads what has come since: once that is nothing, just a header.
        crate::kassert_eq!(host::read_trace(0, cpu, i64::MAX, one), Err(Error::BadArgument));
        let mut caught_up = false;
        for _ in 0..1000 {
            let (copied, next, _) = read(since, usize::MAX / 2)?;
            if copied == 0 {
                crate::kassert_eq!(next, since);
                caught_up = true;
                break;
            }
            since = next;
        }
        crate::kassert!(caught_up);
        let x = host::read_trace(1, cpu, 0, one).map_err(|_| "the X-ray ring refused")?;
        crate::kassert!(x.len() >= HEADER_SIZE);
        Ok(())
    }
);

/// Poll `done` until it holds or `ms` milliseconds pass (a safety bound,
/// not a measurement): whether it held.
fn wait_for(ms: u64, mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..ms {
        if done() {
            return true;
        }
        let _ = crate::sched::sleep(1_000_000);
    }
    done()
}

crate::ktest!(
    fn launch_refuses_before_taking_a_slot() {
        let before = runtime::table_counts();
        let long = "a".repeat(lumen_app_api::limits::ARGS_BYTES);
        let refusals = [
            ("hello", vec![long], "run: hello: arguments too long (past 1024 bytes)"),
            ("hello", vec![String::from("a\0b")], "run: hello: an argument holds a NUL"),
            ("nope", vec![], "run: no app nope"),
        ];
        for (app, args, want) in refusals {
            match runtime::launch(app, &args, Grants::NONE) {
                Ok(_) => return Err("launched"),
                Err(why) => crate::kassert_eq!(why, want),
            }
        }
        // No slot or id taken.
        crate::kassert_eq!(runtime::table_counts(), before);
        Ok(())
    }
);

crate::ktest!(
    fn a_reaper_that_failed_to_start_is_started_by_a_later_run() {
        // Whatever ran before: no reaper.
        runtime::stop_reaper();
        crate::kassert!(!runtime::reaper_running());
        // Its spawn fails once: the app still runs and ends, but nothing
        // joins it, so it keeps its entry (and its slot).
        runtime::FAIL_REAPER_SPAWN.store(true, core::sync::atomic::Ordering::Release);
        let first = runtime::launch("hello", &[], Grants::NONE).map_err(|_| "hello didn't launch")?;
        crate::kassert!(wait_for(10_000, || first.state.has_ended()));
        crate::kassert!(!runtime::reaper_running());
        crate::kassert!(runtime::listed(first.state.id));
        // The next run starts it, and both apps are joined and gone.
        let second = runtime::launch("hello", &[], Grants::NONE).map_err(|_| "hello didn't launch again")?;
        crate::kassert!(runtime::reaper_running());
        let (a, b) = (first.state.id, second.state.id);
        crate::kassert!(wait_for(10_000, || !runtime::listed(a) && !runtime::listed(b)));
        Ok(())
    }
);
