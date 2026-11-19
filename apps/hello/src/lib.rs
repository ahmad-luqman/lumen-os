//! The smallest Lumen app (#196): a `no_std` WASM module with one export.
//!
//! It imports nothing, so any WASM runtime can instantiate it. The app
//! interface (the `lumen` imports, capability handles) comes with M5's
//! runtime; the harness checks this module's exports and their types
//! (`tests/harness/test_wasm_module.py`).

#![no_std]

/// What `answer` returns: the harness and, later, the kernel check it.
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
