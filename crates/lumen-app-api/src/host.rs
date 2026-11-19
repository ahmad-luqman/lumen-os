//! The functions a module may import, all from module [`MODULE`], with
//! their signatures: the kernel defines exactly these and refuses a module
//! importing anything else (another module, an unknown name, a known name
//! with another signature) at load, naming the import. The window and text
//! calls join with #206.

/// The one module apps import from.
pub const MODULE: &str = "lumen";
/// The function a module exports for the runtime to call: `[] -> [i32]`.
pub const ENTRY: &str = "lumen_main";
/// The memory a module exports (its one memory, fixed in size).
pub const MEMORY: &str = "memory";

/// A wasm value type a host function takes or returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    I32,
    I64,
}

/// A host function's name and signature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostFn {
    pub name: &'static str,
    pub params: &'static [Ty],
    pub results: &'static [Ty],
}

use Ty::{I32, I64};

const fn f(name: &'static str, params: &'static [Ty], results: &'static [Ty]) -> HostFn {
    HostFn { name, params, results }
}

/// Every host function, as ARCHITECTURE section 8's table gives it.
pub const FUNCTIONS: &[HostFn] = &[
    f("handle_find", &[I32], &[I32]),
    f("handle_close", &[I32], &[I32]),
    f("args_len", &[], &[I32]),
    f("args_read", &[I32, I32], &[I32]),
    f("exit", &[I32], &[]),
    f("panic", &[I32, I32], &[]),
    f("log", &[I32, I32, I32, I32], &[I32]),
    f("time_now", &[I32], &[I64]),
    f("time_wall", &[I32], &[I64]),
    f("time_sleep", &[I32, I64], &[I32]),
    f("trace_cpus", &[I32], &[I32]),
    f("trace_read", &[I32, I32, I32, I64, I32, I32], &[I32]),
];

/// The host function named `name`, if there is one.
pub fn lookup(name: &str) -> Option<&'static HostFn> {
    FUNCTIONS.iter().find(|f| f.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_found() {
        for (i, f) in FUNCTIONS.iter().enumerate() {
            assert!(FUNCTIONS[..i].iter().all(|g| g.name != f.name), "{} twice", f.name);
            assert_eq!(lookup(f.name), Some(f));
        }
        assert_eq!(lookup("window_open"), None);
        assert_eq!(lookup(""), None);
    }

    #[test]
    fn every_signature_is_pinned() {
        // The ABI: a change here breaks every built app.
        let pinned: &[(&str, &[Ty], &[Ty])] = &[
            ("handle_find", &[I32], &[I32]),
            ("handle_close", &[I32], &[I32]),
            ("args_len", &[], &[I32]),
            ("args_read", &[I32, I32], &[I32]),
            ("exit", &[I32], &[]),
            ("panic", &[I32, I32], &[]),
            ("log", &[I32, I32, I32, I32], &[I32]),
            ("time_now", &[I32], &[I64]),
            ("time_wall", &[I32], &[I64]),
            ("time_sleep", &[I32, I64], &[I32]),
            ("trace_cpus", &[I32], &[I32]),
            ("trace_read", &[I32, I32, I32, I64, I32, I32], &[I32]),
        ];
        let got: Vec<(&str, &[Ty], &[Ty])> = FUNCTIONS.iter().map(|f| (f.name, f.params, f.results)).collect();
        assert_eq!(got, pinned);
        assert_eq!((MODULE, ENTRY, MEMORY), ("lumen", "lumen_main", "memory"));
    }
}
