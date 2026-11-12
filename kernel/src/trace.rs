//! The kernel trace ring: always-on, fixed-size binary events behind the boot
//! timeline (#8) and X-ray mode (M5). The lock-free ring itself is the
//! host-tested `lumen-trace` crate; this module owns the kernel's ring, the
//! event kinds and [`ktrace!`](crate::ktrace!).
//!
//! Timestamps are raw TSC ticks for now; #8 converts them to nanoseconds
//! once #7 calibrates the TSC. There is one ring because there is one CPU;
//! with SMP (#9) each CPU gets its own and [`ring`] picks the current one.

use core::fmt;

use lumen_trace::{Event, Missing, TraceRing};

use crate::arch;

/// Events the ring holds (a power of two). 4096 × 56 bytes = 224 KiB.
pub const EVENTS: usize = 4096;

static RING: TraceRing<EVENTS> = TraceRing::new();

/// What an event records. Stored as the event's `kind`. Values start at 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    /// `args[0]`: a [`BootPhase`].
    BootPhase = 1,
    /// `args`: vector, rip, error code (0 if the vector pushes none).
    /// Recorded for every vector entering the IDT dispatcher, so hardware
    /// interrupts will show up here once they are enabled (#7) unless they
    /// get their own kind.
    Exception = 2,
    /// `args[0]`: level (1 = error .. 5 = trace); `args[1]`: the subsystem
    /// tag's first 8 bytes, little-endian (longer tags are cut). Only lines
    /// that pass the log-level filter are recorded.
    Log = 3,
    /// A panic is being reported; `args[0]`: the source line, if known.
    Panic = 4,
    /// Recorded by in-kernel tests.
    Test = 0xff,
}

impl Kind {
    // Keep in step with the enum: a kind missing here prints as a raw number.
    const ALL: [Kind; 5] = [Kind::BootPhase, Kind::Exception, Kind::Log, Kind::Panic, Kind::Test];

    fn from_raw(raw: u16) -> Option<Kind> {
        Self::ALL.into_iter().find(|&k| k as u16 == raw)
    }

    fn name(self) -> &'static str {
        match self {
            Kind::BootPhase => "boot-phase",
            Kind::Exception => "exception",
            Kind::Log => "log",
            Kind::Panic => "panic",
            Kind::Test => "test",
        }
    }
}

/// Boot phases, in order. Recorded when the phase ends, whether or not it
/// succeeded (e.g. `Framebuffer` with no framebuffer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootPhase {
    /// GDT, IDT and serial are up.
    ArchInit = 1,
    Logger = 2,
    Symbols = 3,
    Memory = 4,
    Framebuffer = 5,
    Complete = 6,
}

impl BootPhase {
    pub const ALL: [BootPhase; 6] = [
        BootPhase::ArchInit,
        BootPhase::Logger,
        BootPhase::Symbols,
        BootPhase::Memory,
        BootPhase::Framebuffer,
        BootPhase::Complete,
    ];

    fn from_raw(raw: u64) -> Option<BootPhase> {
        Self::ALL.into_iter().find(|&p| p as u64 == raw)
    }

    fn name(self) -> &'static str {
        match self {
            BootPhase::ArchInit => "arch-init",
            BootPhase::Logger => "logger",
            BootPhase::Symbols => "symbols",
            BootPhase::Memory => "memory",
            BootPhase::Framebuffer => "framebuffer",
            BootPhase::Complete => "complete",
        }
    }
}

/// The current CPU's ring.
pub fn ring() -> &'static TraceRing<EVENTS> {
    &RING
}

/// Record an event in `ring` now. Use [`ktrace!`](crate::ktrace!) instead.
pub fn record_in<const N: usize>(ring: &TraceRing<N>, kind: Kind, args: [u64; 4]) {
    // cpu 0 and pid 0 until SMP (#9) and processes (M2) exist. A dropped
    // event is counted in the ring's `dropped()`; recording never fails loudly.
    let _ = ring.record(arch::rdtsc(), 0, kind as u16, 0, args);
}

/// Pad up to four arguments to the event's four slots (more fail to compile).
pub const fn args<const M: usize>(given: [u64; M]) -> [u64; 4] {
    const { assert!(M <= 4, "a trace event has at most 4 arguments") };
    let mut out = [0; 4];
    out.split_at_mut(M).0.copy_from_slice(&given);
    out
}

/// Record a trace event in the current CPU's ring:
/// `ktrace!(Kind::Exception, vector, rip, error_code)`.
///
/// Named `ktrace!` rather than `trace!` so it can't be confused with (or
/// shadow) `log::trace!`. Never blocks and never allocates, so it is safe in
/// exception handlers. Arguments are converted with `as u64`: pass unsigned
/// integers or fieldless enums.
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

/// One event as a line of text: `#seq tsc=<ticks> kind details`. Values it
/// doesn't recognise are printed raw rather than hidden, and a non-zero cpu
/// or pid is shown.
pub struct Describe<'a>(pub &'a Event);

impl fmt::Display for Describe<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let e = self.0;
        write!(f, "#{:<6} tsc={:<14} ", e.seq, e.tsc)?;
        if e.cpu != 0 || e.pid != 0 {
            write!(f, "cpu={} pid={} ", e.cpu, e.pid)?;
        }
        let a = e.args;
        let Some(kind) = Kind::from_raw(e.kind) else {
            return write!(f, "kind={:#06x} args=[{:#x}, {:#x}, {:#x}, {:#x}]", e.kind, a[0], a[1], a[2], a[3]);
        };
        write!(f, "{:<10}", kind.name())?;
        match kind {
            Kind::BootPhase => match BootPhase::from_raw(a[0]) {
                Some(p) => write!(f, " {}", p.name()),
                None => write!(f, " phase={}", a[0]),
            },
            Kind::Exception => write!(f, " vector={} rip={:#x} error={:#x}", a[0], a[1], a[2]),
            Kind::Log => {
                match log::Level::iter().find(|&l| l as u64 == a[0]) {
                    Some(level) => write!(f, " {level}")?,
                    None => write!(f, " level={}", a[0])?,
                }
                let tag = a[1].to_le_bytes();
                match core::str::from_utf8(&tag) {
                    Ok(t) => write!(f, " {}", t.trim_end_matches('\0')),
                    Err(_) => write!(f, " tag={:#x}", a[1]),
                }
            }
            Kind::Panic => write!(f, " line={}", a[0]),
            Kind::Test => write!(f, " args=[{:#x}, {:#x}, {:#x}, {:#x}]", a[0], a[1], a[2], a[3]),
        }
    }
}

/// Print the newest `n` events of the current CPU's ring, oldest first, one
/// per line with `indent`; a gap says why that event is missing. Lock-free,
/// so the crash reporter can use it.
pub fn write_recent(w: &mut dyn fmt::Write, n: u64, indent: &str) -> fmt::Result {
    let ring = ring();
    let head = ring.written();
    for seq in head.saturating_sub(n.min(ring.capacity() as u64))..head {
        let why = match ring.try_read(seq) {
            Ok(e) => {
                writeln!(w, "{indent}{}", Describe(&e))?;
                continue;
            }
            Err(Missing::Overwritten) => "overwritten",
            Err(Missing::BeingWritten) => "being written",
            Err(Missing::NotRecorded) => "dropped or not yet written",
            Err(Missing::NotClaimed) => "not claimed",
        };
        writeln!(w, "{indent}#{seq:<6} ({why})")?;
    }
    Ok(())
}

/// A fixed buffer to format into (no heap); for tests.
#[cfg(feature = "ktest")]
struct Line {
    buf: [u8; 160],
    len: usize,
}

#[cfg(feature = "ktest")]
impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        self.buf.get_mut(self.len..end).ok_or(fmt::Error)?.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[cfg(feature = "ktest")]
fn describe(e: &Event) -> Line {
    let mut line = Line { buf: [0; 160], len: 0 };
    let _ = fmt::Write::write_fmt(&mut line, format_args!("{}", Describe(e)));
    line
}

#[cfg(feature = "ktest")]
impl Line {
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("<invalid utf-8>")
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
        let mut prev_tsc = 0;
        for i in 0..3u64 {
            let e = ring.read(start + i).ok_or("a recorded event is missing")?;
            kassert_eq!(e.kind, Kind::Test as u16);
            kassert_eq!(e.args, [0x7e57, i, i * 2, 0]);
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
        // Boot events are the oldest; this relies on the ring not having
        // wrapped since boot (true while ktests log little).
        use crate::{kassert, kassert_eq};
        let ring = ring();
        kassert!(ring.written() <= EVENTS as u64);
        let mut phases = [0u64; BootPhase::ALL.len()];
        let mut found = 0;
        let mut prev_tsc = 0;
        for seq in 0..ring.written() {
            let Some(e) = ring.read(seq) else { continue };
            if e.kind == Kind::BootPhase as u16 {
                if found == phases.len() {
                    return Err("more boot-phase events than phases");
                }
                kassert!(e.tsc >= prev_tsc);
                prev_tsc = e.tsc;
                phases[found] = e.args[0];
                found += 1;
            }
        }
        kassert_eq!(phases, BootPhase::ALL.map(|p| p as u64));
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
        log::info!(target: "tracetst", "a log line for the trace test");
        let events: [Option<Event>; 4] = core::array::from_fn(|i| ring.read(start + i as u64));
        let exception =
            events.iter().flatten().find(|e| e.kind == Kind::Exception as u16).ok_or("no exception event")?;
        kassert_eq!(exception.args[0], 3);
        let log = events
            .iter()
            .flatten()
            .find(|e| e.kind == Kind::Log as u16 && e.args[1] == u64::from_le_bytes(*b"tracetst"));
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
        // Loose bounds: catches gross regressions such as waiting or doing
        // I/O (e.g. serial output) per event, and a TSC that doesn't move. An
        // added uncontended lock would not trip it. Under TCG the TSC follows
        // a host clock, not guest cycles.
        crate::kassert!(per_event > 0 && per_event < 5_000);
        crate::kassert_eq!(BENCH.dropped(), 0);
        Ok(())
    }
);

crate::ktest!(
    fn describe_prints_unknown_values_raw() {
        use crate::kassert;
        let base = Event { seq: 7, tsc: 99, ..Event::default() };
        let cases: [(Event, &str); 6] = [
            (Event { kind: 0x99, args: [1, 2, 3, 4], ..base }, "kind=0x0099 args=[0x1, 0x2, 0x3, 0x4]"),
            (Event { kind: Kind::BootPhase as u16, args: [42, 0, 0, 0], ..base }, "boot-phase phase=42"),
            (Event { kind: Kind::Log as u16, args: [9, pack_tag("mem"), 0, 0], ..base }, "log        level=9 mem"),
            (Event { kind: Kind::Log as u16, args: [1, 0xff, 0, 0], ..base }, "log        ERROR tag=0xff"),
            (
                Event { kind: Kind::Test as u16, args: [0x7e57, 1, 2, 0], ..base },
                "test       args=[0x7e57, 0x1, 0x2, 0x0]",
            ),
            (Event { cpu: 2, pid: 5, kind: Kind::Panic as u16, args: [12, 0, 0, 0], ..base }, "cpu=2 pid=5 panic"),
        ];
        for (e, want) in cases {
            let line = describe(&e);
            if !line.as_str().contains(want) || line.as_str().contains('\n') {
                crate::ktest::diag(format_args!("got {:?}, want it to contain {want:?}", line.as_str()));
                return Err("an event was described wrongly");
            }
        }
        kassert!(
            describe(&Event { kind: Kind::Panic as u16, args: [12, 0, 0, 0], ..base }).as_str().ends_with("line=12")
        );
        // Tags longer than 8 bytes are cut, not mangled.
        crate::kassert_eq!(pack_tag("framebuffer"), u64::from_le_bytes(*b"framebuf"));
        crate::kassert_eq!(pack_tag(""), 0);
        Ok(())
    }
);
