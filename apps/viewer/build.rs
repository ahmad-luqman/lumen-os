//! The viewer's fixed memory (ARCHITECTURE section 8, "Its memory is
//! fixed"): `lumen_view::MEMORY_BYTES`, 16 MiB, the most `MEMORY_PAGES`
//! allows. lumen-view's `tests/memory.rs` checks the app's allocations (its
//! canvas, the decoder's buffers for the costliest PNG it takes, the image
//! shown and its fitted copy) fit the heap that leaves, through the app's
//! own allocator logic; the app checks at its start that its memory is
//! that size (a number here that drifted from lumen-view's would end it
//! there, with an ERROR). These link arguments come after the workspace's
//! default (apps/.cargo/config.toml's 2 MiB), and wasm-ld takes the last.

/// The app's memory, in bytes: `lumen_view::MEMORY_BYTES`.
const MEMORY_BYTES: u32 = 16 * 1024 * 1024;

fn main() {
    println!("cargo:rustc-link-arg=--initial-memory={MEMORY_BYTES}");
    println!("cargo:rustc-link-arg=--max-memory={MEMORY_BYTES}");
}
