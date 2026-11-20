//! The app's fixed memory (ARCHITECTURE section 8, "Its memory is fixed"):
//! its initial size is its maximum, at most 256 pages (16 MiB). This app's
//! 4 MiB hold wasm-ld's 1 MiB stack, its data and a heap for its canvas
//! (4 bytes a pixel). These link arguments come after the workspace's
//! default (apps/.cargo/config.toml's 2 MiB), and wasm-ld takes the last.

/// The app's memory, in bytes: a multiple of 64 KiB.
const MEMORY_BYTES: u32 = 4 * 1024 * 1024;

fn main() {
    println!("cargo:rustc-link-arg=--initial-memory={MEMORY_BYTES}");
    println!("cargo:rustc-link-arg=--max-memory={MEMORY_BYTES}");
}
