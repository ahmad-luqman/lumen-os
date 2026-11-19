//! WASM apps (M5). So far: `wasmi` linked into the kernel and running a
//! module in fuel slices (#197), checked by the `wasm` command and its
//! ktests over hand-assembled modules (`modules`). The app runtime (#199)
//! builds on [`run_in_slices`]'s loop: run until the fuel of a slice is
//! spent, then (there) check for a kill or a closed window, and resume.
//!
//! wasmi interprets on the calling thread's own stack: its value and call
//! stacks are on the heap, so a module's recursion is bounded by
//! `Config`'s call depth (1000 frames by default), not by the kernel
//! stack. That holds only with wasmi's loop dispatch, which keeps the
//! native stack's depth constant however long a module runs (host calls
//! aside): see the `wasmi` entry in the workspace's Cargo.toml for why
//! `portable-dispatch` is on.

mod modules;

use crate::shell::Ctx;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::hint::black_box;
use wasmi::{
    CompilationMode, Config, Engine, Instance, Linker, Module, Store, TrapCode, TypedResumableCall, WasmParams,
    WasmResults,
};

/// Fuel per slice in `wasm` and its ktests. The runtime's own slice size
/// comes with #199.
const CHECK_SLICE_FUEL: u64 = 100_000;

/// A slice small enough that a call is resumed many times and a trap comes
/// after resumes (the ktests').
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by ktests
const SMALL_SLICE_FUEL: u64 = 1_000;

/// The loop's iterations in `wasm` and its ktest: 13 instructions (14
/// fuel) each, so a few million instructions, over several slices.
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

/// A call that failed (compiling, instantiating or trapping), after how
/// many resumes.
#[derive(Debug)]
pub struct Failed {
    pub error: wasmi::Error,
    pub resumes: u32,
}

impl Failed {
    fn trap(&self) -> Option<TrapCode> {
        self.error.as_trap_code()
    }
}

impl fmt::Display for Failed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (after {} resumes)", self.error, self.resumes)
    }
}

impl From<wasmi::Error> for Failed {
    fn from(error: wasmi::Error) -> Self {
        Failed { error, resumes: 0 }
    }
}

fn engine() -> Engine {
    let mut config = Config::default();
    config.consume_fuel(true);
    // Compiled when the module is, not at each function's first call:
    // lazily, wasmi charges a body's compilation to the fuel of the call
    // that reaches it (7 to 9 fuel a byte), and a body costing more than a
    // slice ends that call with an error that can't be resumed.
    config.compilation_mode(CompilationMode::Eager);
    Engine::new(&config)
}

/// `bytes` compiled and instantiated in a fresh store, with `fuel`, and
/// no imports.
fn instantiate(bytes: &[u8], fuel: u64) -> Result<(Store<()>, Instance), wasmi::Error> {
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
/// the runtime will between an app's slices). A slice is never smaller
/// than what the next instruction needs: wasmi charges a bulk memory
/// operation all at once (1 fuel per 64 bytes), so a slice too small for
/// it would leave the call out of fuel on every resume.
pub fn run_in_slices<P, R>(bytes: &[u8], export: &str, arg: P, slice_fuel: u64) -> Result<Sliced<R>, Failed>
where
    P: WasmParams,
    R: WasmResults,
{
    run_bounded(bytes, export, arg, slice_fuel, u32::MAX)
}

/// [`run_in_slices`], failing once the call has been resumed
/// `max_resumes` times without finishing (for a ktest, whose regression
/// would otherwise resume forever).
fn run_bounded<P, R>(bytes: &[u8], export: &str, arg: P, slice_fuel: u64, max_resumes: u32) -> Result<Sliced<R>, Failed>
where
    P: WasmParams,
    R: WasmResults,
{
    let (mut store, instance) = instantiate(bytes, slice_fuel)?;
    let func = instance.get_typed_func::<P, R>(&store, export)?;
    let mut resumes = 0;
    let failed = |error, resumes| Failed { error, resumes };
    let mut call = func.call_resumable(&mut store, arg)?;
    loop {
        match call {
            TypedResumableCall::Finished(value) => return Ok(Sliced { value, resumes }),
            TypedResumableCall::OutOfFuel(_) if resumes == max_resumes => {
                return Err(failed(wasmi::Error::new("still out of fuel after the most resumes allowed"), resumes));
            }
            TypedResumableCall::OutOfFuel(out) => {
                resumes += 1;
                crate::sched::yield_now();
                store.set_fuel(slice_fuel.max(out.required_fuel())).map_err(|e| failed(e, resumes))?;
                call = out.resume(&mut store).map_err(|e| failed(e, resumes))?;
            }
            // Only a host function traps resumably, and `instantiate`'s
            // `Linker` defines none (a module importing one fails to
            // instantiate).
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

/// `modules::LOOP`'s calls at one slice size: `run(LOOP_N)`,
/// `depth(DEPTH_OK)` and `depth(DEPTH_TOO_DEEP)`. `wasm` prints them and
/// the ktest checks them, through [`lines`](Self::lines).
struct LoopChecks {
    slice_fuel: u64,
    run: Result<Sliced<i32>, Failed>,
    depth: Result<Sliced<i32>, Failed>,
    too_deep: Result<Sliced<i32>, Failed>,
}

impl LoopChecks {
    fn new(slice_fuel: u64) -> Self {
        let n = black_box(LOOP_N) as i32;
        LoopChecks {
            slice_fuel,
            run: run_in_slices(modules::LOOP, "run", n, slice_fuel),
            depth: run_in_slices(modules::LOOP, "depth", black_box(DEPTH_OK), slice_fuel),
            too_deep: run_in_slices(modules::LOOP, "depth", black_box(DEPTH_TOO_DEEP), slice_fuel),
        }
    }

    /// What `wasm` prints: `Ok` a result as expected, `Err` a failure line.
    /// The loop must be resumed at least twice and give `loop_expected`;
    /// the shallow `depth` must return its depth; the deep one must trap
    /// on wasmi's call depth (`TrapCode::StackOverflow`), nothing else.
    fn lines(&self) -> Vec<Result<String, String>> {
        let mut lines = Vec::new();
        let (n, fuel) = (LOOP_N, self.slice_fuel);
        match &self.run {
            Ok(r) => {
                lines.push(Ok(format!(
                    "wasm: run({n}) = {:#010x} after {} resumes ({fuel} fuel a slice)",
                    r.value as u32, r.resumes
                )));
                let want = loop_expected(n);
                if r.value != want {
                    lines.push(Err(format!("wasm: run({n}) should be {:#010x}", want as u32)));
                }
                if r.resumes < 2 {
                    lines.push(Err(format!("wasm: run({n}) should run out of fuel at least twice")));
                }
            }
            Err(e) => lines.push(Err(format!("wasm: run({n}): {e}"))),
        }
        lines.push(match &self.depth {
            Ok(r) if r.value == DEPTH_OK => Ok(format!("wasm: depth({DEPTH_OK}) = {DEPTH_OK}")),
            Ok(r) => Err(format!("wasm: depth({DEPTH_OK}) = {}, should be {DEPTH_OK}", r.value)),
            Err(e) => Err(format!("wasm: depth({DEPTH_OK}): {e}")),
        });
        lines.push(match &self.too_deep {
            Err(e) if e.trap() == Some(TrapCode::StackOverflow) => {
                Ok(format!("wasm: depth({DEPTH_TOO_DEEP}) trapped: {}", e.error))
            }
            Err(e) => Err(format!("wasm: depth({DEPTH_TOO_DEEP}) should exhaust the call stack: {e}")),
            Ok(r) => Err(format!("wasm: depth({DEPTH_TOO_DEEP}) = {}, should trap", r.value)),
        });
        lines
    }
}

/// What a float check expects: exact bits, wasm's canonical NaN (sign
/// open), or an arithmetic NaN (quiet bit set, sign and payload open).
/// Wasm requires the canonical NaN when no input is a non-canonical NaN.
#[derive(Clone, Copy, Debug)]
enum Want {
    Bits(u64),
    CanonicalNan32,
    CanonicalNan64,
    ArithmeticNan64,
}

impl fmt::Display for Want {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Want::Bits(bits) => write!(f, "{bits:#x}"),
            Want::CanonicalNan32 => f.write_str("the canonical f32 NaN"),
            Want::CanonicalNan64 => f.write_str("the canonical f64 NaN"),
            Want::ArithmeticNan64 => f.write_str("a quiet f64 NaN"),
        }
    }
}

/// One operation's result in `modules::FLOATS`, as bits, and what was
/// expected (worked out on the host's hardware floats).
#[derive(Debug)]
struct FloatCheck {
    what: &'static str,
    got: u64,
    want: Want,
}

impl FloatCheck {
    fn ok(&self) -> bool {
        let got = self.got;
        match self.want {
            Want::Bits(bits) => got == bits,
            Want::CanonicalNan32 => got >> 32 == 0 && got & 0x7fff_ffff == 0x7fc0_0000,
            Want::CanonicalNan64 => got & 0x7fff_ffff_ffff_ffff == 0x7ff8_0000_0000_0000,
            Want::ArithmeticNan64 => got & 0x7ff8_0000_0000_0000 == 0x7ff8_0000_0000_0000,
        }
    }
}

/// A result's bits, for a [`FloatCheck`].
trait Bits {
    fn bits(self) -> u64;
}

impl Bits for f64 {
    fn bits(self) -> u64 {
        self.to_bits()
    }
}

impl Bits for f32 {
    fn bits(self) -> u64 {
        u64::from(self.to_bits())
    }
}

impl Bits for i32 {
    fn bits(self) -> u64 {
        u64::from(self as u32)
    }
}

/// Every operation in `modules::FLOATS` on inputs that LLVM can't see
/// through (`black_box`). Cases: rounding (0.1 + 0.2 in f64 and f32, 0.1 *
/// 3, 1 / 3, sqrt(2), a u64 past 2^53, 0.1 as f32), round-half-even,
/// subnormal results, infinities (1 / 0, 1e300 as f32), signed zeros
/// (`min`, `nearest`, `eq`, `lt`), NaNs (canonical from 0 / 0 and a NaN
/// operand, arithmetic from a signalling one; comparisons with NaN) and
/// saturation.
fn float_checks() -> Result<Vec<FloatCheck>, wasmi::Error> {
    use Want::*;
    use core::hint::black_box as b;
    let (mut store, instance) = instantiate(modules::FLOATS, CHECK_SLICE_FUEL)?;
    let s = &mut store;
    macro_rules! check {
        ($what:literal, $name:literal, $args:expr => $r:ty, $want:expr) => {
            FloatCheck {
                what: $what,
                got: instance.get_typed_func::<_, $r>(&*s, $name)?.call(&mut *s, $args)?.bits(),
                want: $want,
            }
        };
    }
    let snan = f64::from_bits(0x7ff0_0000_0000_0001);
    Ok(alloc::vec![
        check!("0.1 + 0.2", "add", (b(0.1f64), b(0.2f64)) => f64, Bits(0x3fd3_3333_3333_3334)),
        check!("sNaN + 1", "add", (b(snan), b(1.0f64)) => f64, ArithmeticNan64),
        check!("0.1 * 3", "mul", (b(0.1f64), b(3.0f64)) => f64, Bits(0x3fd3_3333_3333_3334)),
        check!("MIN_POSITIVE * 0.5", "mul", (b(f64::MIN_POSITIVE), b(0.5f64)) => f64, Bits(0x0008_0000_0000_0000)),
        check!("1e-300 * 1e-10", "mul", (b(1e-300f64), b(1e-10f64)) => f64, Bits(0x0000_1268_8b70_e62b)),
        check!("1 / 3", "div", (b(1.0f64), b(3.0f64)) => f64, Bits(0x3fd5_5555_5555_5555)),
        check!("1 / 0", "div", (b(1.0f64), b(0.0f64)) => f64, Bits(0x7ff0_0000_0000_0000)),
        check!("0 / 0", "div", (b(0.0f64), b(0.0f64)) => f64, CanonicalNan64),
        check!("min(-0, 0)", "min", (b(-0.0f64), b(0.0f64)) => f64, Bits(0x8000_0000_0000_0000)),
        check!("min(0, -0)", "min", (b(0.0f64), b(-0.0f64)) => f64, Bits(0x8000_0000_0000_0000)),
        check!("min(NaN, 1)", "min", (b(f64::NAN), b(1.0f64)) => f64, CanonicalNan64),
        check!("sqrt(2)", "sqrt", b(2.0f64) => f64, Bits(0x3ff6_a09e_667f_3bcd)),
        check!("nearest(2.5)", "nearest", b(2.5f64) => f64, Bits(0x4000_0000_0000_0000)),
        check!("nearest(3.5)", "nearest", b(3.5f64) => f64, Bits(0x4010_0000_0000_0000)),
        check!("nearest(-0.5)", "nearest", b(-0.5f64) => f64, Bits(0x8000_0000_0000_0000)),
        check!("NaN < 1", "lt", (b(f64::NAN), b(1.0f64)) => i32, Bits(0)),
        check!("-0 < 0", "lt", (b(-0.0f64), b(0.0f64)) => i32, Bits(0)),
        check!("-0 == 0", "eq", (b(-0.0f64), b(0.0f64)) => i32, Bits(1)),
        check!("f32 1 / 3", "f32_div", (b(1.0f32), b(3.0f32)) => f32, Bits(0x3eaa_aaab)),
        check!("f32 0 / 0", "f32_div", (b(0.0f32), b(0.0f32)) => f32, CanonicalNan32),
        check!("f32 0.1 + 0.2", "f32_add", (b(0.1f32), b(0.2f32)) => f32, Bits(0x3e99_999a)),
        check!("0.1 as f32", "demote", b(0.1f64) => f32, Bits(0x3dcc_cccd)),
        check!("1e300 as f32", "demote", b(1e300f64) => f32, Bits(0x7f80_0000)),
        check!("u64::MAX as f64", "u64_to_f64", b(-1i64) => f64, Bits(0x43f0_0000_0000_0000)),
        check!("(2^53 + 1) as f64", "u64_to_f64", b((1i64 << 53) + 1) => f64, Bits(0x4340_0000_0000_0000)),
        check!("trunc_sat(1e10)", "trunc_sat", b(1e10f64) => i32, Bits(i32::MAX as u64)),
        check!("trunc_sat(-1e10)", "trunc_sat", b(-1e10f64) => i32, Bits(i32::MIN as u32 as u64)),
        check!("trunc_sat(NaN)", "trunc_sat", b(f64::NAN) => i32, Bits(0)),
        check!("trunc_sat(-2.9)", "trunc_sat", b(-2.9f64) => i32, Bits(-2i32 as u32 as u64)),
    ])
}

/// `wasm`: wasmi's checks, on the shell's own thread: the loop in fuel
/// slices, the call depth limit, and the floats.
pub fn command(ctx: &mut Ctx<'_>, _: &[String]) {
    for line in LoopChecks::new(CHECK_SLICE_FUEL).lines() {
        match line {
            Ok(line) => {
                let _ = writeln!(ctx.out, "{line}");
            }
            Err(line) => ctx.fail(line),
        }
    }
    match float_checks() {
        Ok(checks) => {
            let good = checks.iter().filter(|c| c.ok()).count();
            let _ = writeln!(ctx.out, "wasm: floats: {good} of {} match", checks.len());
            for c in checks.iter().filter(|c| !c.ok()) {
                ctx.fail(format_args!("wasm: {} = {:#x}, should be {}", c.what, c.got, c.want));
            }
        }
        Err(e) => ctx.fail(format_args!("wasm: floats: {e}")),
    }
}

/// Each failure line of `checks`, sent to the TAP diagnostics; how many.
#[cfg(feature = "ktest")]
fn diag_failures(checks: &LoopChecks) -> usize {
    let lines = checks.lines();
    for line in &lines {
        match line {
            Ok(line) => crate::ktest::diag(format_args!("{line}")),
            Err(line) => crate::ktest::diag(format_args!("FAILED {line}")),
        }
    }
    lines.iter().filter(|l| l.is_err()).count()
}

/// `f`'s result, run on a thread with an ordinary kernel stack (64 KiB,
/// guard page below): the size an app's thread gets.
#[cfg(feature = "ktest")]
fn on_kernel_stack<T: Send>(f: impl FnOnce() -> T + Send) -> Result<T, &'static str> {
    let mut out = None;
    crate::sched::run_sized("wasm-ktest", crate::mm::stack::STACK_SIZE, || out = Some(f())).map_err(|e| {
        crate::ktest::diag(format_args!("run_sized: {e:?}"));
        "no thread for the test"
    })?;
    out.ok_or("the thread didn't run")
}

crate::ktest!(
    fn wasmi_runs_a_long_loop_in_fuel_slices() {
        // The loop's own oracle, worked out on the host.
        crate::kassert_eq!(loop_expected(300_000) as u32, 0xdc20_69f0);
        // On a 64 KiB stack: wasmi's dispatch must not grow the native
        // stack per instruction (see the module docs). Once with `wasm`'s
        // slices, once with slices so small that the shallow call is
        // resumed and the deep one traps after resumes.
        let (big, small) = on_kernel_stack(|| (LoopChecks::new(CHECK_SLICE_FUEL), LoopChecks::new(SMALL_SLICE_FUEL)))?;
        crate::kassert_eq!(diag_failures(&big), 0);
        crate::kassert_eq!(diag_failures(&small), 0);
        let depth_resumes = small.depth.as_ref().map(|r| r.resumes).unwrap_or(0);
        crate::ktest::diag(format_args!("depth({DEPTH_OK}): {depth_resumes} resumes of {SMALL_SLICE_FUEL} fuel"));
        crate::kassert!(depth_resumes >= 2);
        let trapped_after = small.too_deep.as_ref().err().map(|e| e.resumes).unwrap_or(0);
        crate::ktest::diag(format_args!("depth({DEPTH_TOO_DEEP}) trapped after {trapped_after} resumes"));
        crate::kassert!(trapped_after >= 1);
        Ok(())
    }
);

crate::ktest!(
    fn a_slice_too_small_for_one_instruction_is_topped_up() {
        // One `memory.fill` of 100 000 bytes costs 1563 fuel at once, more
        // than a small slice: the call must still finish.
        let fill = 100_000;
        crate::kassert!(fill / 64 > SMALL_SLICE_FUEL as i32);
        // Bounded: with slices that stay too small it would resume forever.
        let r = run_bounded::<i32, i32>(modules::FILL, "fill", black_box(fill), SMALL_SLICE_FUEL, 10).map_err(|e| {
            crate::ktest::diag(format_args!("fill: {e}"));
            "fill failed"
        })?;
        crate::ktest::diag(format_args!("fill({fill}): {} resumes", r.resumes));
        crate::kassert_eq!(r.value, 0xab);
        crate::kassert!(r.resumes >= 1);
        Ok(())
    }
);

crate::ktest!(
    fn a_function_bigger_than_a_slice_compiles_and_runs() {
        // Lazily compiled, this body would cost 7 to 9 fuel a byte at its
        // first call, far past one slice; eagerly, only its instructions.
        let adds = 6_000;
        let bytes = modules::large_body(adds);
        crate::kassert!(bytes.len() as u64 > CHECK_SLICE_FUEL / 7);
        let r = run_in_slices::<i32, i32>(&bytes, "add", black_box(5), CHECK_SLICE_FUEL).map_err(|e| {
            crate::ktest::diag(format_args!("add: {e}"));
            "add failed"
        })?;
        crate::kassert_eq!(r.value, 5 + adds as i32);
        Ok(())
    }
);

crate::ktest!(
    fn malformed_modules_are_refused() {
        let mut bad_opcode = modules::LOOP.to_vec();
        // `run`'s `i32.mul` (the 0x6c after `i32.const 31`) made 0xff, no
        // instruction.
        let at = bad_opcode.windows(3).position(|w| w == [0x41, 0x1f, 0x6c]).ok_or("no i32.mul in LOOP")? + 2;
        bad_opcode[at] = 0xff;
        let cases: [(&str, &[u8]); 6] = [
            ("empty", &[]),
            ("magic only", &modules::LOOP[..4]),
            ("cut before the last byte", &modules::LOOP[..modules::LOOP.len() - 1]),
            ("bad magic", &[0x00, 0x61, 0x73, 0x6e, 0x01, 0x00, 0x00, 0x00]),
            ("version 2", &[0x00, 0x61, 0x73, 0x6d, 0x02, 0x00, 0x00, 0x00]),
            ("an opcode that isn't one", &bad_opcode),
        ];
        for (what, bytes) in cases {
            match instantiate(bytes, CHECK_SLICE_FUEL) {
                Ok(_) => {
                    crate::ktest::diag(format_args!("{what}: accepted"));
                    return Err("a malformed module was accepted");
                }
                // The first line: some of wasmparser's messages span several.
                Err(e) => crate::ktest::diag(format_args!("{what}: {}", format!("{e}").lines().next().unwrap_or(""))),
            }
        }
        Ok(())
    }
);

crate::ktest!(
    fn wasm_floats_match() {
        // The checker itself: what each kind of expectation lets through.
        let ok = |got, want| FloatCheck { what: "", got, want }.ok();
        crate::kassert!(ok(0xfff8_0000_0000_0000, Want::CanonicalNan64));
        crate::kassert!(!ok(0x7ff8_0000_0000_0001, Want::CanonicalNan64));
        crate::kassert!(!ok(0x7ff0_0000_0000_0000, Want::CanonicalNan64));
        crate::kassert!(ok(0x7ff8_0000_0000_0001, Want::ArithmeticNan64));
        crate::kassert!(!ok(0x7ff0_0000_0000_0001, Want::ArithmeticNan64));
        crate::kassert!(!ok(0x7ff0_0000_0000_0000, Want::ArithmeticNan64));
        crate::kassert!(ok(0xffc0_0000, Want::CanonicalNan32));
        crate::kassert!(!ok(0x7fc0_0001, Want::CanonicalNan32));
        crate::kassert!(!ok(0x1_7fc0_0000, Want::CanonicalNan32));
        crate::kassert!(!ok(0x7f80_0000, Want::CanonicalNan32));

        let checks = float_checks().map_err(|e| {
            crate::ktest::diag(format_args!("float module: {e}"));
            "the float module failed"
        })?;
        let mut bad = 0;
        for c in checks.iter().filter(|c| !c.ok()) {
            crate::ktest::diag(format_args!("{} = {:#x}, should be {}", c.what, c.got, c.want));
            bad += 1;
        }
        crate::kassert_eq!(bad, 0);
        crate::kassert_eq!(checks.len(), 29);
        Ok(())
    }
);
