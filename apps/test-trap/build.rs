//! The app's memory is fixed (ARCHITECTURE section 8): its initial size is
//! its maximum, 2 MiB (wasm-ld's 1 MiB stack, the data, testkit's heap).
fn main() {
    println!("cargo:rustc-link-arg=--initial-memory=2097152");
    println!("cargo:rustc-link-arg=--max-memory=2097152");
}
