//! Kernel command line (from `cmdline:` in limine.conf).
//!
//! Space-separated words; each is either a flag (`lumen.qemu_exit`) or a
//! `key=value` pair (`lumen.log=debug`).

use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

static PTR: AtomicPtr<u8> = AtomicPtr::new(core::ptr::null_mut());
static LEN: AtomicUsize = AtomicUsize::new(0);

pub fn set(cmdline: &'static str) {
    LEN.store(cmdline.len(), Ordering::Relaxed);
    PTR.store(cmdline.as_ptr().cast_mut(), Ordering::Release);
}

pub fn get() -> &'static str {
    let ptr = PTR.load(Ordering::Acquire);
    if ptr.is_null() {
        return "";
    }
    // SAFETY: `set` stored a pointer/length pair from a `&'static str`.
    unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(ptr, LEN.load(Ordering::Relaxed))) }
}

/// Is the bare word `flag` present in `line`?
pub fn has_flag_in(line: &str, flag: &str) -> bool {
    line.split_ascii_whitespace().any(|w| w == flag)
}

/// Value of `key=value` in `line`, if present.
pub fn value_in<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    line.split_ascii_whitespace().find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
}

pub fn has_flag(flag: &str) -> bool {
    has_flag_in(get(), flag)
}

pub fn value(key: &str) -> Option<&'static str> {
    value_in(get(), key)
}

/// Whether the kernel may end QEMU through `isa-debug-exit`.
pub fn qemu_exit_enabled() -> bool {
    has_flag("lumen.qemu_exit")
}

crate::ktest!(
    fn cmdline_parses_flags_and_values() {
        let line = "lumen.qemu_exit  lumen.log=debug other=1";
        crate::kassert!(has_flag_in(line, "lumen.qemu_exit"));
        crate::kassert!(!has_flag_in(line, "lumen.qemu"));
        crate::kassert_eq!(value_in(line, "lumen.log"), Some("debug"));
        crate::kassert_eq!(value_in(line, "lumen.lo"), None);
        crate::kassert_eq!(value_in("", "x"), None);
        Ok(())
    }
);
