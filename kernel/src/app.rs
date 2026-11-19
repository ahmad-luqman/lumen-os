//! WASM apps (M5). So far: `wasmi` linked into the kernel and running a
//! module in fuel slices (#197), checked by the `wasm` command and its
//! ktests over hand-assembled modules (`modules`). The app runtime (#199)
//! builds on [`run_in_slices`]'s loop: run until the fuel of a slice is
//! spent, then (there) check for a kill or a closed window, and resume.
//!
//! wasmi interprets on the calling thread's own stack: its value and call
//! stacks are on the heap, so a module's recursion is bounded by
//! `Config`'s call depth (1000 frames by default), not by the kernel
//! stack. That holds only with wasmi's loop dispatch: see the `wasmi`
//! entry in the workspace's Cargo.toml for why `portable-dispatch` is on.

mod modules;

use crate::shell::Ctx;
use alloc::string::String;
use alloc::vec::Vec;
use core::hint::black_box;
use wasmi::{Config, Engine, Linker, Module, Store, TypedResumableCall, WasmParams, WasmResults};

/// Fuel given to a module per slice (`wasm`'s and the ktests'). The
/// runtime's own slice size comes with #199.
pub const SLICE_FUEL: u64 = 100_000;

/// The loop's iterations in `wasm` and its ktest: about 8 instructions
/// each, so a few million instructions, over several slices.
const LOOP_N: u32 = 300_000;

/// How deep `depth` recurses: within wasmi's default call depth (1000),
/// and past it.
const DEPTH_OK: i32 = 900;
const DEPTH_TOO_DEEP: i32 = 5_000;

/// A call's result and how many times it ran out of fuel and was resumed.
#[derive(Debug)]
pub struct Sliced<T> {
    pub value: T,
    pub resumes: u32,
}

fn engine() -> Engine {
    let mut config = Config::default();
    config.consume_fuel(true);
    Engine::new(&config)
}

/// `bytes` compiled and instantiated in a fresh store, with `fuel`, and
/// no imports.
fn instantiate(bytes: &[u8], fuel: u64) -> Result<(Store<()>, wasmi::Instance), wasmi::Error> {
    let engine = engine();
    let module = Module::new(&engine, bytes)?;
    let mut store = Store::new(&engine, ());
    store.set_fuel(fuel)?;
    let instance = Linker::<()>::new(&engine).instantiate_and_start(&mut store, &module)?;
    Ok((store, instance))
}

/// Call `export(arg)` in a fresh instance of `bytes`, `slice_fuel` at a
/// time: each time the fuel runs out the call is suspended, given another
/// slice, and resumed (after a [`yield_now`](crate::sched::yield_now), as
/// the runtime will between an app's slices). A trap is an error.
pub fn run_in_slices<P, R>(bytes: &[u8], export: &str, arg: P, slice_fuel: u64) -> Result<Sliced<R>, wasmi::Error>
where
    P: WasmParams,
    R: WasmResults,
{
    let (mut store, instance) = instantiate(bytes, slice_fuel)?;
    let func = instance.get_typed_func::<P, R>(&store, export)?;
    let mut resumes = 0;
    let mut call = func.call_resumable(&mut store, arg)?;
    loop {
        match call {
            TypedResumableCall::Finished(value) => return Ok(Sliced { value, resumes }),
            TypedResumableCall::OutOfFuel(out) => {
                resumes += 1;
                crate::sched::yield_now();
                store.set_fuel(slice_fuel)?;
                call = out.resume(&mut store)?;
            }
            // Only a host function can trap resumably, and these modules
            // import none.
            TypedResumableCall::HostTrap(_) => unreachable!("a module without imports called the host"),
        }
    }
}

/// `modules::LOOP`'s `run(n)`, computed in Rust: `acc = acc * 31 + i`
/// for `i` in `0..n`, wrapping, at least once.
fn loop_expected(n: u32) -> i32 {
    let (mut acc, mut i) = (0u32, 0u32);
    loop {
        acc = acc.wrapping_mul(31).wrapping_add(i);
        i += 1;
        if i >= n {
            return acc as i32;
        }
    }
}

/// What a float check expects: exact bits, or a quiet NaN of either sign
/// (wasm leaves a NaN's sign and payload open beyond that).
#[derive(Clone, Copy, Debug)]
enum Want {
    Bits(u64),
    QuietNan32,
    QuietNan64,
}

/// One float operation's result in `modules::FLOATS`, checked against bits
/// worked out on the host's hardware floats.
#[derive(Debug)]
pub struct FloatCheck {
    pub what: &'static str,
    pub got: u64,
    want: Want,
}

impl FloatCheck {
    pub fn ok(&self) -> bool {
        match self.want {
            Want::Bits(bits) => self.got == bits,
            Want::QuietNan32 => self.got & 0x7fc0_0000 == 0x7fc0_0000 && self.got >> 32 == 0,
            Want::QuietNan64 => self.got & 0x7ff8_0000_0000_0000 == 0x7ff8_0000_0000_0000,
        }
    }

    /// The expected value, for a failure line.
    pub fn want(&self) -> String {
        match self.want {
            Want::Bits(bits) => alloc::format!("{bits:#x}"),
            Want::QuietNan32 => "a quiet f32 NaN".into(),
            Want::QuietNan64 => "a quiet f64 NaN".into(),
        }
    }
}

/// Every operation in `modules::FLOATS` on inputs that LLVM can't see
/// through (`black_box`), each result as bits. Cases: rounding (0.1 + 0.2,
/// 1 / 3, a u64 past 2^53, round-half-even), infinity (1 / 0), signed zeros (`min`,
/// `nearest`), NaN (0 / 0, a NaN operand) and saturation.
pub fn float_checks() -> Result<Vec<FloatCheck>, wasmi::Error> {
    let (mut store, instance) = instantiate(modules::FLOATS, SLICE_FUEL)?;
    let f64_2 = |store: &Store<()>, name| instance.get_typed_func::<(f64, f64), f64>(store, name);
    let f64_1 = |store: &Store<()>, name| instance.get_typed_func::<f64, f64>(store, name);
    let (add, div, min) = (f64_2(&store, "add")?, f64_2(&store, "div")?, f64_2(&store, "min")?);
    let (sqrt, nearest) = (f64_1(&store, "sqrt")?, f64_1(&store, "nearest")?);
    let f32_div = instance.get_typed_func::<(f32, f32), f32>(&store, "f32_div")?;
    let u64_to_f64 = instance.get_typed_func::<i64, f64>(&store, "u64_to_f64")?;
    let trunc_sat = instance.get_typed_func::<f64, i32>(&store, "trunc_sat")?;

    use core::hint::black_box as b;
    let s = &mut store;
    let bits = |x: f64| x.to_bits();
    let checks = [
        ("0.1 + 0.2", bits(add.call(&mut *s, (b(0.1), b(0.2)))?), Want::Bits(0x3fd3_3333_3333_3334)),
        ("1 / 3", bits(div.call(&mut *s, (b(1.0), b(3.0)))?), Want::Bits(0x3fd5_5555_5555_5555)),
        ("1 / 0", bits(div.call(&mut *s, (b(1.0), b(0.0)))?), Want::Bits(0x7ff0_0000_0000_0000)),
        ("0 / 0", bits(div.call(&mut *s, (b(0.0), b(0.0)))?), Want::QuietNan64),
        ("min(-0, 0)", bits(min.call(&mut *s, (b(-0.0), b(0.0)))?), Want::Bits(0x8000_0000_0000_0000)),
        ("min(0, -0)", bits(min.call(&mut *s, (b(0.0), b(-0.0)))?), Want::Bits(0x8000_0000_0000_0000)),
        ("min(NaN, 1)", bits(min.call(&mut *s, (b(f64::NAN), b(1.0)))?), Want::QuietNan64),
        ("sqrt(2)", bits(sqrt.call(&mut *s, b(2.0))?), Want::Bits(0x3ff6_a09e_667f_3bcd)),
        ("nearest(2.5)", bits(nearest.call(&mut *s, b(2.5))?), Want::Bits(0x4000_0000_0000_0000)),
        ("nearest(3.5)", bits(nearest.call(&mut *s, b(3.5))?), Want::Bits(0x4010_0000_0000_0000)),
        ("nearest(-0.5)", bits(nearest.call(&mut *s, b(-0.5))?), Want::Bits(0x8000_0000_0000_0000)),
        ("f32 1 / 3", u64::from(f32_div.call(&mut *s, (b(1.0), b(3.0)))?.to_bits()), Want::Bits(0x3eaa_aaab)),
        ("f32 0 / 0", u64::from(f32_div.call(&mut *s, (b(0.0), b(0.0)))?.to_bits()), Want::QuietNan32),
        ("u64::MAX as f64", bits(u64_to_f64.call(&mut *s, b(-1))?), Want::Bits(0x43f0_0000_0000_0000)),
        ("(2^53 + 1) as f64", bits(u64_to_f64.call(&mut *s, b((1 << 53) + 1))?), Want::Bits(0x4340_0000_0000_0000)),
        ("trunc_sat(1e10)", trunc_sat.call(&mut *s, b(1e10))? as u32 as u64, Want::Bits(i32::MAX as u32 as u64)),
        ("trunc_sat(-1e10)", trunc_sat.call(&mut *s, b(-1e10))? as u32 as u64, Want::Bits(i32::MIN as u32 as u64)),
        ("trunc_sat(NaN)", trunc_sat.call(&mut *s, b(f64::NAN))? as u32 as u64, Want::Bits(0)),
        ("trunc_sat(-2.9)", trunc_sat.call(&mut *s, b(-2.9))? as u32 as u64, Want::Bits(-2i32 as u32 as u64)),
    ];
    Ok(checks.into_iter().map(|(what, got, want)| FloatCheck { what, got, want }).collect())
}

/// `wasm`: wasmi's checks, on the shell's own thread: the loop in fuel
/// slices, the call depth limit, and the floats.
pub fn command(ctx: &mut Ctx<'_>, _: &[String]) {
    let n = black_box(LOOP_N);
    match run_in_slices::<i32, i32>(modules::LOOP, "run", n as i32, SLICE_FUEL) {
        Ok(r) => {
            let want = loop_expected(n);
            let _ = writeln!(
                ctx.out,
                "wasm: run({n}) = {:#010x} after {} resumes ({SLICE_FUEL} fuel a slice)",
                r.value as u32, r.resumes
            );
            if r.value != want {
                ctx.fail(format_args!("wasm: run({n}) should be {:#010x}", want as u32));
            }
        }
        Err(e) => ctx.fail(format_args!("wasm: run({n}): {e}")),
    }
    match run_in_slices::<i32, i32>(modules::LOOP, "depth", DEPTH_OK, SLICE_FUEL) {
        Ok(r) if r.value == DEPTH_OK => {
            let _ = writeln!(ctx.out, "wasm: depth({DEPTH_OK}) = {}", r.value);
        }
        Ok(r) => ctx.fail(format_args!("wasm: depth({DEPTH_OK}) = {}, should be {DEPTH_OK}", r.value)),
        Err(e) => ctx.fail(format_args!("wasm: depth({DEPTH_OK}): {e}")),
    }
    match run_in_slices::<i32, i32>(modules::LOOP, "depth", DEPTH_TOO_DEEP, SLICE_FUEL) {
        Err(e) => {
            let _ = writeln!(ctx.out, "wasm: depth({DEPTH_TOO_DEEP}) trapped: {e}");
        }
        Ok(r) => ctx.fail(format_args!("wasm: depth({DEPTH_TOO_DEEP}) = {}, should trap", r.value)),
    }
    match float_checks() {
        Ok(checks) => {
            let good = checks.iter().filter(|c| c.ok()).count();
            let _ = writeln!(ctx.out, "wasm: floats: {good} of {} match", checks.len());
            for c in checks.iter().filter(|c| !c.ok()) {
                ctx.fail(format_args!("wasm: {} = {:#x}, should be {}", c.what, c.got, c.want()));
            }
        }
        Err(e) => ctx.fail(format_args!("wasm: floats: {e}")),
    }
}

crate::ktest!(
    fn wasmi_runs_a_long_loop_in_fuel_slices() {
        // On a thread with an ordinary kernel stack (64 KiB, guard page
        // below), the size an app's thread gets: wasmi's dispatch must not
        // grow the native stack per instruction (see the module docs).
        let n = black_box(LOOP_N);
        let mut out = None;
        crate::sched::run_sized("wasm-ktest", crate::mm::stack::STACK_SIZE, || {
            out = Some((
                run_in_slices::<i32, i32>(modules::LOOP, "run", n as i32, SLICE_FUEL),
                run_in_slices::<i32, i32>(modules::LOOP, "depth", DEPTH_OK, SLICE_FUEL),
                run_in_slices::<i32, i32>(modules::LOOP, "depth", DEPTH_TOO_DEEP, SLICE_FUEL),
            ))
        })
        .map_err(|_| "no thread for the test")?;
        let (run, depth, too_deep) = out.ok_or("the thread didn't run")?;
        let run = run.map_err(|e| {
            crate::ktest::diag(format_args!("run: {e}"));
            "run failed"
        })?;
        crate::ktest::diag(format_args!("run({n}): {} resumes of {SLICE_FUEL} fuel", run.resumes));
        crate::kassert_eq!(run.value, loop_expected(n));
        crate::kassert!(run.resumes >= 2);
        crate::kassert_eq!(depth.map(|r| r.value).map_err(|_| "trapped"), Ok(DEPTH_OK));
        // Too deep for wasmi's call stack: a trap, not a kernel stack overflow.
        let trap = too_deep.err().and_then(|e| e.as_trap_code());
        crate::kassert_eq!(trap, Some(wasmi::TrapCode::StackOverflow));
        Ok(())
    }
);

crate::ktest!(
    fn wasm_floats_match() {
        let checks = float_checks().map_err(|e| {
            crate::ktest::diag(format_args!("float module: {e}"));
            "the float module failed"
        })?;
        let mut bad = 0;
        for c in checks.iter().filter(|c| !c.ok()) {
            crate::ktest::diag(format_args!("{} = {:#x}, should be {}", c.what, c.got, c.want()));
            bad += 1;
        }
        crate::kassert_eq!(bad, 0);
        crate::kassert_eq!(checks.len(), 19);
        Ok(())
    }
);
