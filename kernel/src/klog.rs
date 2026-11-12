//! Structured kernel logger (hard rule 8).
//!
//! Implements the `log` crate facade, so any crate can log without depending
//! on the kernel. Every line has a timestamp (on serial), a level and a
//! subsystem tag:
//!
//! ```text
//! [    ?.??????] INFO  boot: Lumen 0.0.1 (x86_64) booting
//! [    ?.??????] WARN  mem: no ACPI RSDP from Limine
//! [    0.072250] INFO  time: tsc 999.8 MHz (calibrated against hpet)
//! ```
//!
//! The timestamp is seconds since `kmain` began, to the microsecond; every
//! line before the TSC is calibrated (before the `time: tsc` line) shows `?`
//! instead (the boot timeline covers those phases afterwards). The screen's
//! console shows lines without it, so screenshots stay comparable (not
//! tested: the golden image covers only the header).
//!
//! The tag is the `target:` given at the call site, or else the module path
//! with the `lumen_kernel::` prefix removed. The level threshold comes from
//! `lumen.log=<level>` on the command line (default `info`).
//!
//! Every emitted line also records a `Log` event in the trace ring
//! (`crate::trace`); recording it doesn't change the printed line.

use crate::arch::serial::SerialWriter;
use crate::sync::SpinLock;
use core::fmt::Write;
use log::{Level, LevelFilter, Log, Metadata, Record};

struct KernelLogger;

static LOGGER: KernelLogger = KernelLogger;
/// Serializes whole log lines so they never interleave.
static LINE_LOCK: SpinLock<()> = SpinLock::new(());

impl Log for KernelLogger {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record<'_>) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let tag = tag_for(record.target());
        // Trace before taking the line lock: recording never waits.
        crate::ktrace!(crate::trace::Kind::Log, record.level(), crate::trace::pack_tag(tag));
        let _line = LINE_LOCK.lock();
        // Under the line lock: lines come out in the order of their stamps.
        let stamp = lumen_time::Stamp(crate::time::since_boot_ns());
        let mut out = SerialWriter;
        let _ = writeln!(out, "{stamp} {:<5} {}: {}", record.level(), tag, record.args());
        if record.level() <= Level::Info {
            crate::fb::console_log(record.level(), tag, record.args());
        }
    }

    fn flush(&self) {}
}

/// Shorten a `log` target to a subsystem tag.
pub fn tag_for(target: &str) -> &str {
    let t = target.strip_prefix("lumen_kernel::").unwrap_or(target);
    t.strip_prefix("arch::x86_64::").unwrap_or(t)
}

pub fn parse_level(s: &str) -> Option<LevelFilter> {
    Some(match s {
        "off" => LevelFilter::Off,
        "error" => LevelFilter::Error,
        "warn" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => return None,
    })
}

/// Install the logger. Safe to call once, early in boot.
pub fn init(level: LevelFilter) {
    // set_logger fails only if a logger is already installed; ignore that.
    let _ = log::set_logger(&LOGGER);
    log::set_max_level(level);
}

crate::ktest!(
    fn log_tags_strip_crate_and_arch_prefixes() {
        crate::kassert_eq!(tag_for("lumen_kernel::arch::x86_64::gdt"), "gdt");
        crate::kassert_eq!(tag_for("lumen_kernel::fb"), "fb");
        crate::kassert_eq!(tag_for("boot"), "boot");
        crate::kassert_eq!(parse_level("debug"), Some(LevelFilter::Debug));
        crate::kassert_eq!(parse_level("loud"), None);
        Ok(())
    }
);
