//! The kernel trace ring: always-on, fixed-size binary events behind the boot
//! timeline (#8) and X-ray mode (M5). The lock-free ring itself is the
//! host-tested `lumen-trace` crate; this module owns the kernel's ring, the
//! event kinds and [`ktrace!`](crate::ktrace!).
//!
//! Timestamps are raw TSC ticks for now; #8 converts them to nanoseconds
//! once #7 calibrates the TSC. There is one ring because there is one CPU;
//! with SMP (#9) each CPU gets its own and [`ring`] picks the current one.

use core::fmt;

use lumen_trace::{Event, TraceRing};

use crate::arch;

/// Events the ring holds (a power of two). 4096 × 56 bytes = 224 KiB.
pub const EVENTS: usize = 4096;

static RING: TraceRing<EVENTS> = TraceRing::new();

/// What an event records. Stored as the event's `kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    /// `args[0]`: a [`BootPhase`].
    BootPhase = 1,
    /// `args`: vector, rip, error code.
    Exception = 2,
    /// `args[0]`: level (1 = error .. 5 = trace); `args[1]`: the subsystem
    /// tag's first 8 bytes, little-endian.
    Log = 3,
    /// Recorded by in-kernel tests.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))]
    Test = 0xff,
}

#[cfg_attr(feature = "ktest", allow(dead_code))] // used by kmon's `trace`
impl Kind {
    fn name(raw: u16) -> &'static str {
        match raw {
            1 => "boot-phase",
            2 => "exception",
            3 => "log",
            0xff => "test",
            _ => "unknown",
        }
    }
}

/// Boot phases, in order. Recorded when the phase is done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum BootPhase {
    /// GDT, IDT and serial are up.
    ArchInit = 1,
    Logger = 2,
    Symbols = 3,
    Memory = 4,
    Framebuffer = 5,
    Complete = 6,
}

#[cfg_attr(feature = "ktest", allow(dead_code))] // used by kmon's `trace`
impl BootPhase {
    fn name(raw: u64) -> &'static str {
        match raw {
            1 => "arch-init",
            2 => "logger",
            3 => "symbols",
            4 => "memory",
            5 => "framebuffer",
            6 => "complete",
            _ => "?",
        }
    }
}

/// The current CPU's ring.
pub fn ring() -> &'static TraceRing<EVENTS> {
    &RING
}

/// Record an event in `ring` now. Use [`ktrace!`](crate::ktrace!) instead.
pub fn record_in<const N: usize>(ring: &TraceRing<N>, kind: Kind, args: [u64; 4]) {
    // cpu 0 and pid 0 until SMP (#9) and processes (M2) exist.
    ring.record(arch::rdtsc(), 0, kind as u16, 0, args);
}

/// Pad up to four arguments to the event's four slots (more fail to compile).
pub const fn args<const M: usize>(given: [u64; M]) -> [u64; 4] {
    const { assert!(M <= 4, "a trace event has at most 4 arguments") };
    let mut out = [0; 4];
    let mut i = 0;
    while i < M {
        out[i] = given[i];
        i += 1;
    }
    out
}

/// Record a trace event in the current CPU's ring:
/// `ktrace!(Kind::Exception, vector, rip, error_code)`.
///
/// Named `ktrace!` rather than `trace!` so it can't be confused with (or
/// shadow) `log::trace!`. Never blocks and never allocates, so it is safe in
/// exception handlers.
#[macro_export]
macro_rules! ktrace {
    ($kind:expr $(, $arg:expr)* $(,)?) => {
        $crate::trace::record_in($crate::trace::ring(), $kind, $crate::trace::args([$($arg as u64),*]))
    };
}

/// The first 8 bytes of a tag, packed little-endian (zero-padded), so a log
/// event says which subsystem logged without storing a pointer.
pub fn pack_tag(tag: &str) -> u64 {
    let mut b = [0u8; 8];
    let n = tag.len().min(8);
    b[..n].copy_from_slice(&tag.as_bytes()[..n]);
    u64::from_le_bytes(b)
}

#[cfg_attr(feature = "ktest", allow(dead_code))] // used by kmon's `trace`
fn unpack_tag(v: u64) -> [u8; 8] {
    v.to_le_bytes()
}

/// One event as a line of text: `#seq tsc kind details`.
#[cfg_attr(feature = "ktest", allow(dead_code))] // used by kmon's `trace`
pub struct Describe<'a>(pub &'a Event);

impl fmt::Display for Describe<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let e = self.0;
        write!(f, "#{:<6} tsc={:<14} {:<10}", e.seq, e.tsc, Kind::name(e.kind))?;
        match e.kind {
            k if k == Kind::BootPhase as u16 => write!(f, " {}", BootPhase::name(e.args[0])),
            k if k == Kind::Exception as u16 => {
                write!(f, " vector={} rip={:#x} error={:#x}", e.args[0], e.args[1], e.args[2])
            }
            k if k == Kind::Log as u16 => {
                let tag = unpack_tag(e.args[1]);
                let tag = core::str::from_utf8(&tag).unwrap_or("?").trim_end_matches('\0');
                let level = ["?", "ERROR", "WARN", "INFO", "DEBUG", "TRACE"].get(e.args[0] as usize).unwrap_or(&"?");
                write!(f, " {level} {tag}")
            }
            _ => write!(f, " args={:#x?}", e.args),
        }
    }
}

crate::ktest!(
    fn events_recorded_in_order() {
        use crate::{kassert, kassert_eq};
        let ring = ring();
        let start = ring.written();
        for i in 0..3u64 {
            crate::ktrace!(Kind::Test, 0x7e57, i, i * 2);
        }
        let events = [ring.read(start), ring.read(start + 1), ring.read(start + 2)];
        let mut prev_tsc = 0;
        for (i, e) in events.iter().enumerate() {
            let e = e.ok_or("a recorded event is missing")?;
            kassert_eq!(e.kind, Kind::Test as u16);
            kassert_eq!(e.args, [0x7e57, i as u64, i as u64 * 2, 0]);
            kassert!(e.tsc >= prev_tsc);
            prev_tsc = e.tsc;
        }
        kassert_eq!(ring.written(), start + 3);
        kassert_eq!(ring.dropped(), 0);
        Ok(())
    }
);

crate::ktest!(
    fn boot_phases_recorded_in_order() {
        use crate::kassert_eq;
        let ring = ring();
        let mut phases = [0u64; 6];
        let mut found = 0;
        let mut prev_tsc = 0;
        // Boot events are the oldest; scan from the start of what's held.
        for seq in ring.written().saturating_sub(EVENTS as u64)..ring.written() {
            let Some(e) = ring.read(seq) else { continue };
            if e.kind == Kind::BootPhase as u16 && found < phases.len() {
                if e.tsc < prev_tsc {
                    return Err("boot phase timestamps go backwards");
                }
                prev_tsc = e.tsc;
                phases[found] = e.args[0];
                found += 1;
            }
        }
        kassert_eq!(phases, [1, 2, 3, 4, 5, 6]);
        Ok(())
    }
);

crate::ktest!(
    fn exceptions_and_logs_are_traced() {
        use crate::{kassert, kassert_eq};
        let ring = ring();
        let start = ring.written();
        // SAFETY: #BP is handled and returns (see idt::lumen_exception_dispatch).
        unsafe { core::arch::asm!("int3") };
        log::info!(target: "tracetest", "a log line for the trace test");
        let events: [Option<Event>; 4] = core::array::from_fn(|i| ring.read(start + i as u64));
        let exception =
            events.iter().flatten().find(|e| e.kind == Kind::Exception as u16).ok_or("no exception event")?;
        kassert_eq!(exception.args[0], 3);
        let log = events.iter().flatten().find(|e| e.kind == Kind::Log as u16 && e.args[1] == pack_tag("tracetest"));
        let log = log.ok_or("no log event")?;
        kassert_eq!(log.args[0], log::Level::Info as u64);
        kassert!(log.seq > exception.seq);
        Ok(())
    }
);

crate::ktest!(
    fn trace_overhead_is_small() {
        // The cost per event, in TSC ticks, printed for the record. Uses its
        // own ring so the kernel's ring keeps its boot events.
        static BENCH: TraceRing<EVENTS> = TraceRing::new();
        const N: u64 = 20_000;
        let t0 = arch::rdtsc();
        for i in 0..N {
            record_in(&BENCH, Kind::Test, [i, 0, 0, 0]);
        }
        let per_event = (arch::rdtsc() - t0) / N;
        crate::ktest::diag(format_args!("trace: {per_event} TSC ticks per event ({N} events)"));
        // Loose bound: catches a lock, a spin or a syscall sneaking in, not
        // noise (TCG's TSC is emulated).
        crate::kassert!(per_event < 5_000);
        crate::kassert_eq!(BENCH.dropped(), 0);
        Ok(())
    }
);
