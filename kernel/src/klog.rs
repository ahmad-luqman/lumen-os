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
//! terminal shows lines up to `info` without it, so screenshots stay
//! comparable, with the level coloured (`crate::term`).
//!
//! The tag is the `target:` given at the call site, or else the module path
//! with the `lumen_kernel::` prefix removed. The level threshold comes from
//! `lumen.log=<level>` on the command line (default `info`).
//!
//! Every emitted line also records a `Log` event in the trace ring
//! (`crate::trace`); recording it doesn't change the printed line. The
//! lines as printed on serial are also kept, the newest [`RING_BYTES`]
//! bytes of them, for the shell's `dmesg` ([`copy_recent`]).

use crate::arch::serial;
use crate::sync::SpinLock;
use core::fmt::{self, Write};
use log::{Level, LevelFilter, Log, Metadata, Record};
use lumen_trace::TextRing;

struct KernelLogger;

static LOGGER: KernelLogger = KernelLogger;
/// Serializes every serial write but the crash reporter's: whole log lines
/// never interleave, and two CPUs never burst into the UART's FIFO at once
/// (`serial::burst_bytes`).
static LINE_LOCK: SpinLock<()> = SpinLock::new(());

/// How many bytes of log lines [`RING`] keeps.
pub const RING_BYTES: usize = 16 * 1024;
/// The newest log lines, as printed on serial. Written only by the logger
/// (holding the line lock, which orders the lines). It has a lock of its
/// own so that readers ([`copy_recent`]) needn't take the line lock; that
/// lock is a leaf (nothing is taken while it is held), so taking it under
/// the line lock can't deadlock.
static RING: SpinLock<TextRing<RING_BYTES>> = SpinLock::new(TextRing::new());

/// Copy the newest whole log lines into `out` (as many as fit, oldest
/// first), as serial printed them; returns the bytes copied and the bytes
/// of log written since boot (more than [`RING_BYTES`]: the oldest lines
/// were overwritten), both read at once. A line another CPU is still
/// writing (pushed in pieces) is left out. Never allocates (the caller
/// makes `out`).
pub fn copy_recent(out: &mut [u8]) -> (usize, u64) {
    let ring = RING.lock();
    (ring.copy_lines(out), ring.written())
}

/// Run `f` holding the log's line lock, so no log line (or other serial
/// write) lands in the middle of what it does. `f` must not re-take it: no
/// logging, [`LockedSerial`], `kassert!` or `ktest::diag` inside (the
/// re-lock is a crash report: `crash relocklog`).
pub fn with_line_lock<R>(f: impl FnOnce() -> R) -> R {
    let _line = LINE_LOCK.lock();
    f()
}

/// Serial output for the serial shell, the ktest runner and the `crash`
/// command (the logger holds the line lock itself; the crash reporter
/// writes byte by byte without it): each write (a whole `write!`) is made
/// holding the line lock, in FIFO bursts.
#[derive(Clone, Copy, Default)]
pub struct LockedSerial;

impl Write for LockedSerial {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        with_line_lock(|| Burst.write_str(s))
    }

    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        with_line_lock(|| Burst.write_fmt(args))
    }
}

/// Bursts to serial; only made where the line lock is held.
struct Burst;

impl Write for Burst {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        burst_held(s.as_bytes());
        Ok(())
    }
}

/// Bursts to serial and keeps a copy in [`RING`]; only made where the line
/// lock is held.
struct Kept;

impl Write for Kept {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        RING.lock().push(s.as_bytes());
        burst_held(s.as_bytes());
        Ok(())
    }
}

/// How the log writes serial, with the line lock held (the caller's job):
/// in bursts (`serial::burst_bytes`); the waits (for the loopback ktest,
/// which goes through here to check the logger's own path).
pub(crate) fn burst_held(bytes: &[u8]) -> usize {
    serial::burst_bytes(bytes)
}

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
        if writeln!(Kept, "{stamp} {:<5} {}: {}", record.level(), tag, record.args()).is_err() {
            // An argument's formatting failed part way: end the line, so the
            // next one isn't glued to it (on serial and in the ring).
            let _ = Kept.write_str(" <formatting failed>\n");
        }
        if record.level() <= Level::Info {
            crate::term::console_log(record.level(), tag, record.args());
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

crate::ktest!(
    fn log_lines_are_kept_for_dmesg() {
        // A line logged is in the ring as serial printed it (timestamp,
        // level, tag, message), among the newest (another CPU may log
        // after it); a small buffer gets the newest whole lines only.
        use crate::kassert;
        log::info!(target: "klogtest", "kept for dmesg");
        let mut out = alloc::vec![0; RING_BYTES];
        let (n, written) = copy_recent(&mut out);
        let text = core::str::from_utf8(&out[..n]).map_err(|_| "the ring isn't UTF-8")?;
        let kept =
            text.lines().rev().take(5).any(|l| l.starts_with('[') && l.ends_with("] INFO  klogtest: kept for dmesg"));
        kassert!(kept);
        kassert!(written >= n as u64);
        let mut small = [0u8; 200];
        let (n, _) = copy_recent(&mut small);
        let tail = core::str::from_utf8(&small[..n]).map_err(|_| "a cut line")?;
        kassert!(n > 0 && tail.ends_with('\n') && tail.lines().all(|l| l.starts_with('[')));
        Ok(())
    }
);
