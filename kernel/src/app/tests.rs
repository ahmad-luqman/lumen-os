//! The runtime's ktests (#199): load refusals over hand-assembled modules,
//! the code and memory charges against the heap's high-water mark, and
//! `trace_read`'s edges.

use super::runtime::{self, AppState};
use super::{BUNDLED, TEST_APPS, host};
use crate::mm::heap;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use lumen_app_api::limits::{APP_OVERHEAD, CODE_FACTOR, MEMORY_PAGES, MODULE_BYTES, PAGE_BYTES, TABLE_ELEMENTS};
use lumen_app_api::{Error, Grants, manifest};
use wasmi::{Engine, Module};

// --- A small module builder: just what the cases need. ---

fn leb(mut n: u32) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return out;
        }
        out.push(b | 0x80);
    }
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

/// What a test module holds; [`Spec::bytes`] assembles it.
#[derive(Clone)]
struct Spec {
    /// Imports' bytes (module, name, description).
    imports: Vec<Vec<u8>>,
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
            imports: Vec::new(),
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
    fn bytes(&self) -> Vec<u8> {
        let mut m = b"\0asm\x01\0\0\0".to_vec();
        let main_ty = [vec![0x60, 0x00], leb(self.main_results.len() as u32), self.main_results.clone()].concat();
        m.extend(section(1, &[main_ty, vec![0x60, 0x00, 0x00]]));
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
        let imported_funcs =
            self.imports.iter().filter(|i| i.last().is_some_and(|_| i.ends_with(&[0x00, FUNC_TYPE_MAIN]))).count()
                as u32;
        let mut exports = Vec::new();
        if self.export_memory {
            exports.push([name("memory"), vec![0x02, 0x00]].concat());
        }
        if self.export_main {
            exports.push([name("lumen_main"), vec![0x00], leb(imported_funcs)].concat());
        }
        m.extend(section(7, &exports));
        if self.start {
            let mut start = vec![8];
            let idx = leb(imported_funcs + 1);
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

/// What `run` does with a module before its thread runs it, then the load
/// itself (on this thread): the reason it is refused, if it is.
fn try_load(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() > MODULE_BYTES {
        return Err(format!("too big ({} bytes, past {MODULE_BYTES})", bytes.len()));
    }
    manifest::find(bytes).map_err(|e| format!("{e}"))?;
    let state = Arc::new(AppState::new(0, "test"));
    runtime::load(bytes, runtime::new_ctx(state, Grants::log(), Vec::new()), 1_000_000).map(|_| ())
}

crate::ktest!(
    fn load_refuses_bad_modules() {
        crate::kassert!(try_load(&Spec::default().bytes()).is_ok());
        let mut cases: Vec<(&str, Spec, Option<Vec<u8>>)> = Vec::new();
        let mut add = |what, f: &dyn Fn(&mut Spec)| {
            let mut s = Spec::default();
            f(&mut s);
            cases.push((what, s, None));
        };
        add("an import from another module", &|s| s.imports = vec![func_import("env", "f", FUNC_TYPE_MAIN)]);
        add("an unknown lumen name", &|s| s.imports = vec![func_import("lumen", "nope", FUNC_TYPE_MAIN)]);
        add("a lumen name with another signature", &|s| s.imports = vec![func_import("lumen", "log", FUNC_TYPE_MAIN)]);
        add("an imported memory", &|s| {
            // Its only memory, so this isn't a second memory too.
            s.imports = vec![[name("lumen"), name("mem"), vec![0x02], limits(1, Some(1))].concat()];
            s.memories = vec![];
        });
        add("an imported table", &|s| {
            s.imports = vec![[name("lumen"), name("t"), vec![0x01, 0x70], limits(1, None)].concat()]
        });
        add("an imported global", &|s| s.imports = vec![[name("lumen"), name("g"), vec![0x03, 0x7f, 0x00]].concat()]);
        add("a second memory", &|s| s.memories = vec![(1, Some(1)), (1, Some(1))]);
        add("a second table", &|s| s.tables = vec![(1, Some(1)), (1, Some(1))]);
        add("a start section", &|s| s.start = true);
        add("no memory export", &|s| s.export_memory = false);
        add("no lumen_main", &|s| s.export_main = false);
        add("lumen_main of another signature", &|s| {
            s.main_results = vec![];
            s.body = vec![];
        });
        add("a memory without a maximum", &|s| s.memories = vec![(1, None)]);
        add("a maximum unlike its initial size", &|s| s.memories = vec![(1, Some(2))]);
        add("past MEMORY_PAGES", &|s| s.memories = vec![(MEMORY_PAGES as u32 + 1, Some(MEMORY_PAGES as u32 + 1))]);
        add("past TABLE_ELEMENTS", &|s| s.tables = vec![(TABLE_ELEMENTS as u32 + 1, None)]);
        add("no manifest", &|s| s.manifests = vec![]);
        add("two manifests", &|s| s.manifests = vec![b"caps=", b"caps="]);
        add("past MODULE_BYTES", &|s| s.padding = MODULE_BYTES);
        cases.push(("invalid wasm", Spec::default(), Some(b"\0asm\x01\0\0\0\x0a\x05".to_vec())));

        // Each case twice: the first refusal may allocate for good (the
        // log's line, a lazily built table), the second must leave the heap
        // as it found it.
        let mut accepted = 0;
        let mut leaked = 0;
        for (what, spec, raw) in &cases {
            let bytes = raw.clone().unwrap_or_else(|| spec.bytes());
            let reason = try_load(&bytes).err();
            let before = heap::stats().used;
            let again = try_load(&bytes);
            drop(again);
            let after = heap::stats().used;
            drop(bytes);
            match reason {
                Some(why) => crate::ktest::diag(format_args!("{what}: {why}")),
                None => {
                    crate::ktest::diag(format_args!("{what}: ACCEPTED"));
                    accepted += 1;
                }
            }
            if after != before {
                crate::ktest::diag(format_args!("{what}: heap used {before} before, {after} after"));
                leaked += 1;
            }
        }
        crate::kassert_eq!(accepted, 0);
        crate::kassert_eq!(leaked, 0);
        Ok(())
    }
);

crate::ktest!(
    fn compiled_code_fits_its_charge() {
        let apps: Vec<_> = BUNDLED.iter().chain(TEST_APPS.iter()).collect();
        crate::kassert!(!apps.is_empty());
        let mut over = 0;
        for app in apps {
            let engine = Engine::new(&runtime::app_config());
            let before = heap::stats().used;
            heap::reset_peak();
            let module = Module::new(&engine, app.module).map_err(|_| "a bundled module didn't compile")?;
            let peak = heap::peak();
            drop(module);
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
        let state = Arc::new(AppState::new(0, "test"));
        let before = heap::stats().used;
        heap::reset_peak();
        let (mut store, main) = runtime::load(&bytes, runtime::new_ctx(state, Grants::log(), Vec::new()), 1_000_000)
            .map_err(|_| "the module didn't load")?;
        let grew = heap::peak().saturating_sub(before) as u64;
        let memory = u64::from(pages) * PAGE_BYTES;
        crate::ktest::diag(format_args!("instantiating: peak {grew} over, memory {memory}"));
        crate::kassert!(grew <= memory + APP_OVERHEAD);
        // Once to warm up (wasmi's first call may set up its call stack),
        // then the grow must allocate nothing.
        crate::kassert_eq!(main.call(&mut store, ()).map_err(|_| "lumen_main trapped")?, -1);
        let used = heap::stats().used;
        heap::reset_peak();
        crate::kassert_eq!(main.call(&mut store, ()).map_err(|_| "lumen_main trapped")?, -1);
        crate::kassert_eq!(heap::stats().used, used);
        crate::kassert_eq!(heap::peak(), used);
        Ok(())
    }
);

crate::ktest!(
    fn trace_read_edges() {
        use lumen_app_api::limits::TRACE_EVENTS_PER_READ;
        use lumen_app_api::trace::{EVENT_SIZE, HEADER_SIZE, decode_header};
        let cpu = crate::arch::cpu_index() as i32;
        let one = HEADER_SIZE + EVENT_SIZE;
        // Too small, and exactly one event.
        crate::kassert_eq!(host::read_trace(0, cpu, 0, one - 1), Err(Error::BufferTooSmall));
        crate::kassert!(host::read_trace(0, cpu, 0, one).is_ok());
        // Unknown streams and CPUs, negative values.
        for (stream, c, since) in [(2, cpu, 0), (-1, cpu, 0), (0, -1, 0), (0, 99, 0), (0, cpu, -1)] {
            crate::kassert_eq!(host::read_trace(stream, c, since, one), Err(Error::BadArgument));
        }
        // Enough events on this CPU's main ring to pass one read's cap.
        for i in 0..TRACE_EVENTS_PER_READ as u64 + 10 {
            crate::ktrace!(crate::trace::Kind::Test, 0x199, i);
        }
        // Room for two: two copied.
        let two = host::read_trace(0, cpu, 0, HEADER_SIZE + 2 * EVENT_SIZE).map_err(|_| "a read of two failed")?;
        crate::kassert_eq!(two.len(), HEADER_SIZE + 2 * EVENT_SIZE);
        // From 0 with room for everything: the cap, and the header agreeing
        // with what was copied (`next - since = copied + missed`).
        let bytes = host::read_trace(0, cpu, 0, usize::MAX / 2).map_err(|_| "a read from 0 failed")?;
        let copied = (bytes.len() - HEADER_SIZE) / EVENT_SIZE;
        let (next, missed) = decode_header(bytes[..HEADER_SIZE].try_into().unwrap());
        crate::ktest::diag(format_args!("from 0: {copied} copied, {missed} missed, next {next}"));
        crate::kassert_eq!(copied, TRACE_EVENTS_PER_READ);
        crate::kassert_eq!(next, copied as u64 + missed);
        // Each event copied is numbered in order up to `next`.
        let last = lumen_app_api::trace::decode_event(bytes[bytes.len() - EVENT_SIZE..].try_into().unwrap());
        crate::kassert!(last.seq < next);
        // A `since` past the ring's next number is refused; one at the last
        // `next` reads (what has come since, maybe nothing).
        crate::kassert_eq!(host::read_trace(0, cpu, i64::MAX, one), Err(Error::BadArgument));
        let mut since = next;
        for _ in 0..1000 {
            let more = host::read_trace(0, cpu, since as i64, usize::MAX / 2).map_err(|_| "a read at `next` failed")?;
            let (n, _) = decode_header(more[..HEADER_SIZE].try_into().unwrap());
            if n == since {
                break;
            }
            since = n;
        }
        crate::kassert!(host::read_trace(0, cpu, since as i64, one).is_ok());
        let x = host::read_trace(1, cpu, 0, one).map_err(|_| "the X-ray ring refused")?;
        crate::kassert!(x.len() >= HEADER_SIZE);
        Ok(())
    }
);
