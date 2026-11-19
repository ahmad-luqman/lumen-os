use std::fmt::Write as _;
use std::path::Path;

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
    embed_apps(Path::new(&dir).join("../build/apps").as_path());
}

/// Embed the WASM apps `make apps` built (ARCHITECTURE section 8,
/// "Building and bundling"): build/apps/<name>.wasm as `app::BUNDLED`,
/// build/apps/tests/<name>.wasm as `app::TEST_APPS`, each sorted by name,
/// each an `App { name, thread, module }` (`thread` the `app:<name>` string
/// its thread is named with, since `sched::spawn` takes a `&'static str`).
/// A name outside `[a-z0-9_-]`, or one in both sets, fails the build; no
/// build/apps at all (`make lint` runs before `make build` in CI) gives
/// empty lists, said in a warning.
fn embed_apps(apps: &Path) {
    println!("cargo:rerun-if-changed={}", apps.display());
    println!("cargo:rerun-if-changed={}", apps.join("tests").display());
    if !apps.is_dir() {
        println!("cargo:warning=no {} (run `make apps`): the kernel embeds no apps", apps.display());
    }
    let bundled = modules(apps);
    let tests = modules(&apps.join("tests"));
    for (name, _) in &bundled {
        assert!(!tests.iter().any(|(t, _)| t == name), "app {name} is both a bundled app and a test app");
    }
    let mut out = String::new();
    for (list, set) in [("BUNDLED", &bundled), ("TEST_APPS", &tests)] {
        writeln!(out, "pub static {list}: &[App] = &[").unwrap();
        for (name, path) in set {
            println!("cargo:rerun-if-changed={}", path.display());
            writeln!(
                out,
                "    App {{ name: {name:?}, thread: \"app:{name}\", module: include_bytes!({:?}) }},",
                path.display().to_string()
            )
            .unwrap();
        }
        writeln!(out, "];").unwrap();
    }
    let dest = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("apps.rs");
    std::fs::write(dest, out).expect("writing apps.rs");
}

/// The `.wasm` files directly in `dir`, as (name, path), sorted by name.
fn modules(dir: &Path) -> Vec<(String, std::path::PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "wasm"))
        .map(|p| {
            let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
            assert!(
                !name.is_empty()
                    && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-'),
                "app name {name:?} ({}) isn't [a-z0-9_-]",
                p.display()
            );
            let path = p.canonicalize().expect("an app's path");
            (name, path)
        })
        .collect();
    found.sort();
    found
}
