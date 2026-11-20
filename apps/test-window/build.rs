//! The app's fixed memory (ARCHITECTURE section 8, "Its memory is fixed"):
//! 4 MiB, for wasm-ld's 1 MiB stack, its data and a heap that holds `ui-big`'s
//! toolkit of more widgets than `TREE_NODES` (its tree built to be refused).
//! `alloc` still allocates four times this in all (16 MiB), so only a heap
//! that frees lasts. These link arguments come after the workspace's default
//! (apps/.cargo/config.toml's 2 MiB), and wasm-ld takes the last.

/// The app's memory, in bytes: a multiple of 64 KiB.
const MEMORY_BYTES: u32 = 4 * 1024 * 1024;

fn main() {
    println!("cargo:rustc-link-arg=--initial-memory={MEMORY_BYTES}");
    println!("cargo:rustc-link-arg=--max-memory={MEMORY_BYTES}");
}
