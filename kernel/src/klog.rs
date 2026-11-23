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
//! The line is also an interface: the harness matches it
//! (tools/harness/lumen_harness/klog.py), taking log lines out of a serial
//! command's output (a line can land between two of the shell's writes,
//! #124) and checking every boot line: change both together.
//!
//! The tag is the `target:` given at the call site, or else the module path
//! with the `lumen_kernel::` prefix removed. The level threshold comes from
//! `lumen.log=<level>` on the command line (default `info`).
//!
//! Every emitted line also records a `Log` event in the trace ring
//! (`crate::trace`); recording it doesn't change the printed line. The
//! lines as printed on serial are also kept, the newest [`RING_BYTES`]
//! bytes of them, for the shell's `dmesg` ([`copy_recent`]).
//!
//! In the ktest kernel, [`hold_repeats`] holds a ktest's repeated warnings
//! and errors back from serial while it runs (#347): the fault sweeps log
//! thousands, and under KVM each line on the UART takes milliseconds.

use crate::arch::serial;
use crate::sync::SpinLock;
use core::fmt::{self, Write};
use log::{Level, LevelFilter, Log, Metadata, Record};
use lumen_trace::TextRing;

struct KernelLogger;

static LOGGER: KernelLogger = KernelLogger;
/// Serializes every serial write but the crash reporter's: whole log lines
/// never interleave, and two CPUs never burst into the UART's FIFO at once
/// (`serial::burst_bytes`). In the ktest kernel it also holds the repeats
/// [`hold_repeats`] counts.
static LINE_LOCK: SpinLock<Lines> = SpinLock::new(Lines::new());

/// What the line lock guards besides the serial port itself.
struct Lines {
    #[cfg(feature = "ktest")]
    held: held::Held,
}

impl Lines {
    const fn new() -> Self {
        Lines {
            #[cfg(feature = "ktest")]
            held: held::Held::new(),
        }
    }
}

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
        RingOnly.write_str(s)?;
        burst_held(s.as_bytes());
        Ok(())
    }
}

/// Keeps a line in [`RING`] without printing it: a repeat held back from
/// serial ([`hold_repeats`]). Only made where the line lock is held.
struct RingOnly;

impl Write for RingOnly {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        RING.lock().push(s.as_bytes());
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
        #[cfg_attr(not(feature = "ktest"), allow(unused_mut))]
        let mut lines = LINE_LOCK.lock();
        #[cfg(feature = "ktest")]
        if lines.held.holds_back(record, tag) {
            // Kept for `dmesg` only: no serial, no screen.
            let stamp = lumen_time::Stamp(crate::time::since_boot_ns());
            line_to(RingOnly, stamp, record.level(), tag, record.args());
            return;
        }
        write_line(&lines, record.level(), tag, record.args());
    }

    fn flush(&self) {}
}

/// Print one log line (serial, the ring, and up to `info` the screen); only
/// called holding the line lock (`_lines`, what it guards, is the proof).
fn write_line(_lines: &Lines, level: Level, tag: &str, args: &fmt::Arguments<'_>) {
    // Under the line lock: lines come out in the order of their stamps.
    let stamp = lumen_time::Stamp(crate::time::since_boot_ns());
    line_to(Kept, stamp, level, tag, args);
    if level <= Level::Info {
        crate::term::console_log(level, tag, args);
    }
}

/// Write one log line, stamped, to `out`.
fn line_to(mut out: impl Write, stamp: lumen_time::Stamp, level: Level, tag: &str, args: &fmt::Arguments<'_>) {
    if writeln!(out, "{stamp} {level:<5} {tag}: {args}").is_err() {
        // An argument's formatting failed part way: end the line, so the
        // next one isn't glued to it (on serial and in the ring).
        let _ = out.write_str(" <formatting failed>\n");
    }
}

/// Hold `target`'s repeated warnings and errors back from serial until the
/// guard drops (ktest kernels only, #347). Each call site's first such line
/// is printed as always; its later ones go only to the ring (`dmesg`), and
/// when the guard drops, one line per call site that had any held says how
/// many and repeats the first, e.g. `INFO  klog: 56 more held back like:
/// ERROR vfs: ...` (test_ktest counts each family from these). Every other
/// line, from any CPU, is printed as always: other targets', and `info` and
/// below. Panics if a guard is already alive (ktests run one at a time).
///
/// For a sweep logging the same failures thousands of times: the UART takes
/// milliseconds a line under KVM, which took the ktest run past its harness
/// bound. Don't hold a target whose lines a test counts one by one.
#[cfg(feature = "ktest")]
pub fn hold_repeats(target: &'static str) -> HeldRepeats {
    // The line lock let go before a panic (whose report is printed).
    let already = LINE_LOCK.lock().held.start(target);
    if let Some(other) = already {
        panic!("klog: repeats are already held for {other}");
    }
    HeldRepeats { _on_this_thread: core::marker::PhantomData }
}

/// [`hold_repeats`]'s guard: dropping it prints the held lines' summaries
/// and holds no more.
#[cfg(feature = "ktest")]
#[must_use = "repeats are held only while the guard lives"]
pub struct HeldRepeats {
    // Not `Send`: dropped by the ktest that made it, on its thread.
    _on_this_thread: core::marker::PhantomData<*const ()>,
}

#[cfg(feature = "ktest")]
impl Drop for HeldRepeats {
    fn drop(&mut self) {
        // Under the line lock throughout: the table is read in place (not
        // moved), and no held line can come between the summaries and the
        // end of holding.
        let mut lines = LINE_LOCK.lock();
        for (count, first) in lines.held.summaries() {
            write_line(&lines, Level::Info, "klog", &format_args!("{count} more held back like: {first}"));
        }
        lines.held.end();
    }
}

#[cfg(feature = "ktest")]
mod held {
    use log::{Level, Record};
    use lumen_trace::repeats::{Repeats, Seen, Text};

    /// Call sites one guard can hold (a sweep's have been under ten); one
    /// past them is printed as if nothing were held.
    const SITES: usize = 32;
    /// A first line's text kept, from its level on (the vfs sweeps' are
    /// under 200 bytes); a longer one is shown cut, ending `...`.
    const TEXT: usize = 320;

    /// A call site, at its level: source file, line, level.
    type Site = (&'static str, u32, Level);

    /// The repeats held while a guard lives (in the line lock's [`Lines`]).
    ///
    /// [`Lines`]: super::Lines
    pub(super) struct Held {
        /// The target held, while a guard lives.
        target: Option<&'static str>,
        repeats: Repeats<Site, SITES, TEXT>,
    }

    impl Held {
        pub(super) const fn new() -> Self {
            Held { target: None, repeats: Repeats::new() }
        }

        /// Hold `target`'s repeats, unless another target's already are
        /// (returned, and nothing changed).
        pub(super) fn start(&mut self, target: &'static str) -> Option<&'static str> {
            if self.target.is_some() {
                return self.target;
            }
            self.target = Some(target);
            None
        }

        /// Whether `record` (tagged `tag`) is a repeat to hold back: its call
        /// site's line counted if so, its text kept if it is the first.
        pub(super) fn holds_back(&mut self, record: &Record<'_>, tag: &str) -> bool {
            if self.target != Some(tag) || record.level() > Level::Warn {
                return false;
            }
            // `log`'s macros always give both.
            let site = (record.file_static().unwrap_or("?"), record.line().unwrap_or(0), record.level());
            let seen = self.repeats.see(site, |text| {
                use core::fmt::Write;
                // A cut is shown (`Text`'s `...`); nothing else fails.
                let _ = write!(text, "{:<5} {tag}: {}", record.level(), record.args());
            });
            seen == Seen::Repeat
        }

        /// Each call site that had lines held: how many, and its first line.
        pub(super) fn summaries(&self) -> impl Iterator<Item = (u64, &Text<TEXT>)> {
            self.repeats.held()
        }

        /// Hold no more, and forget every call site.
        pub(super) fn end(&mut self) {
            self.target = None;
            self.repeats.clear();
        }
    }
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

crate::ktest!(
    fn repeats_held_back_from_serial_are_summarized_when_the_guard_drops() {
        // Under a guard for `klogheld`: one call site's warning three times,
        // another's error once, an info line twice. Serial gets the first
        // warning, the error, both info lines and, at the drop, one summary
        // for the two held warnings (test_ktest checks serial); the ring
        // keeps every line, the held ones too.
        use crate::kassert;
        {
            let _held = hold_repeats("klogheld");
            for i in 0..3 {
                log::warn!(target: "klogheld", "repeated {i}");
            }
            log::error!(target: "klogheld", "once");
            for i in 0..2 {
                log::info!(target: "klogheld", "info {i}, never held");
            }
        }
        // Holding ended with the guard: this one is printed.
        log::warn!(target: "klogheld", "after the guard");
        let mut out = alloc::vec![0; RING_BYTES];
        let (n, _) = copy_recent(&mut out);
        let text = core::str::from_utf8(&out[..n]).map_err(|_| "the ring isn't UTF-8")?;
        // Every line is in the ring, in order (other CPUs' lines may come
        // between them).
        let want = [
            "] WARN  klogheld: repeated 0",
            "] WARN  klogheld: repeated 1",
            "] WARN  klogheld: repeated 2",
            "] ERROR klogheld: once",
            "] INFO  klogheld: info 0, never held",
            "] INFO  klogheld: info 1, never held",
            "] INFO  klog: 2 more held back like: WARN  klogheld: repeated 0",
            "] WARN  klogheld: after the guard",
        ];
        let mut lines = text.lines().filter(|l| l.starts_with('['));
        for w in want {
            kassert!(lines.any(|l| l.ends_with(w)));
        }
        // A second guard can start once the first is gone.
        drop(hold_repeats("klogheld"));
        Ok(())
    }
);
