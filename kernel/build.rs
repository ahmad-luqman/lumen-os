fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let target = std::env::var("TARGET").unwrap_or_default();
    // Only the bare-metal build uses our linker script; `cargo check` on the
    // host (e.g. from an editor) must not pick it up.
    if target == "x86_64-unknown-none" {
        println!("cargo:rustc-link-arg-bins=-T{dir}/linker.ld");
        println!("cargo:rustc-link-arg-bins=--no-pie");
    }
    println!("cargo:rerun-if-changed={dir}/linker.ld");
}
