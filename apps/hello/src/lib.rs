//! The smallest Lumen app (#196): a `no_std` WASM module exporting its
//! linear memory and one function, `answer`.
//!
//! It imports nothing, so any WASM runtime can instantiate it. The app
//! interface (the `lumen` imports, capability handles) comes with M5's
//! runtime (#199), which also renames the entry point to `lumen_main`, as
//! PR #210's design says. Until then the harness checks this module's
//! exports, their types, and what `answer` returns, running it in wasmtime
//! (`tests/harness/test_wasm_module.py`).

#![no_std]

/// What `answer` returns (checked by the harness, running the module).
const ANSWER: i32 = 42;

/// The module's one function: takes nothing, returns [`ANSWER`].
#[unsafe(no_mangle)]
pub extern "C" fn answer() -> i32 {
    ANSWER
}

/// A WASM app has nowhere to print a panic: it traps (`unreachable`), and
/// the runtime reports the trap.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
