//! The kernel trace ring: always-on, fixed-size binary events behind the boot
//! timeline (#8) and X-ray mode (M5). The lock-free ring itself is the
//! host-tested `lumen-trace` crate; this module owns the kernel's ring, the
//! event kinds and [`ktrace!`](crate::ktrace!).
//!
//! Timestamps are raw TSC ticks (the boot timeline converts its phases with
//! the frequency `time` calibrates; kmon's `trace` shows them raw). There is
//! one ring because there is one CPU; with SMP (#9) each CPU gets its own
//! and [`ring`] picks the current one.

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
    /// Recorded for every CPU exception and for device interrupts nothing
    /// handles; not for the timer, spurious interrupts or local APIC errors
    /// (counted instead: at 1 kHz the timer would flood the ring).
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

/// Boot phases, in order. `Entry` is the start of the timeline (the TSC as
/// `kmain` began); every other phase is recorded when it ends, whether or
/// not it succeeded (e.g. `Framebuffer` with no framebuffer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootPhase {
    Entry = 1,
    /// GDT, IDT and serial are up.
    ArchInit = 2,
    Logger = 3,
    Symbols = 4,
    /// Frame allocator, page tables, heap.
    Memory = 5,
    /// Allocated kernel and IST stacks.
    Stacks = 6,
    Acpi = 7,
    /// Interrupt controllers.
    Interrupts = 8,
    /// TSC and timer calibration, interrupts on, the 10 ms first-tick check.
    Time = 9,
    Framebuffer = 10,
    Complete = 11,
}

impl BootPhase {
    pub const ALL: [BootPhase; 11] = [
        BootPhase::Entry,
        BootPhase::ArchInit,
        BootPhase::Logger,
        BootPhase::Symbols,
        BootPhase::Memory,
        BootPhase::Stacks,
        BootPhase::Acpi,
        BootPhase::Interrupts,
        BootPhase::Time,
        BootPhase::Framebuffer,
        BootPhase::Complete,
    ];

    fn from_raw(raw: u64) -> Option<BootPhase> {
        let i = usize::try_from(raw).ok()?.checked_sub(1)?;
        Self::ALL.get(i).copied()
    }

    fn name(self) -> &'static str {
        match self {
            BootPhase::Entry => "entry",
            BootPhase::ArchInit => "arch-init",
            BootPhase::Logger => "logger",
            BootPhase::Symbols => "symbols",
            BootPhase::Memory => "memory",
            BootPhase::Stacks => "stacks",
            BootPhase::Acpi => "acpi",
            BootPhase::Interrupts => "interrupts",
            BootPhase::Time => "time",
            BootPhase::Framebuffer => "framebuffer",
            BootPhase::Complete => "complete",
        }
    }
}

// `ALL` lists every phase, numbered 1, 2, ... in boot order, entry first and
// complete last: `from_raw` and the timeline index it by number.
const _: () = {
    let mut i = 0;
    while i < BootPhase::ALL.len() {
        assert!(BootPhase::ALL[i] as usize == i + 1);
        i += 1;
    }
    assert!(matches!(BootPhase::ALL[0], BootPhase::Entry));
    assert!(matches!(BootPhase::ALL[BootPhase::ALL.len() - 1], BootPhase::Complete));
};

/// The current CPU's ring.
pub fn ring() -> &'static TraceRing<EVENTS> {
    &RING
}

/// Record an event in `ring` now. Use [`ktrace!`](crate::ktrace!) instead.
pub fn record_in<const N: usize>(ring: &TraceRing<N>, kind: Kind, args: [u64; 4]) {
    record_at(ring, arch::rdtsc(), kind, args);
}

/// Record an event in `ring` stamped `tsc`, read earlier. For
/// `BootPhase::Entry`: its TSC is read before `arch::init_early` (only a bare
/// `rdtsc` may run before GDT/IDT setup, hard rule 6), and the event is
/// recorded after.
pub fn record_at<const N: usize>(ring: &TraceRing<N>, tsc: u64, kind: Kind, args: [u64; 4]) {
    // cpu 0 and pid 0 until SMP (#9) and processes (M2) exist. A dropped
    // event is counted in the ring's `dropped()`; recording never fails loudly.
    let _ = ring.record(tsc, 0, kind as u16, 0, args);
}

/// The boot timeline from a ring's boot-phase events, as one line:
/// `entry +0.000 ms, arch-init +0.012 ms, ..., complete +340.000 ms (total
/// 340.000 ms)`, times since `Entry` with `clock`. A phase with no event is
/// named as missing (with the ring's dropped count, if any), one stamped
/// before entry says so, and the total is `complete`'s (unknown without it).
/// Without a clock, an entry event, or once the ring has wrapped past the
/// boot's events, it says it is unavailable and why.
pub struct Timeline<'a, const N: usize> {
    ring: &'a TraceRing<N>,
    clock: Option<lumen_time::Clock>,
}

impl Timeline<'static, EVENTS> {
    /// The boot's timeline: this CPU's ring and the calibrated TSC.
    pub fn boot() -> Self {
        Timeline { ring: ring(), clock: crate::time::tsc() }
    }
}

impl<'a, const N: usize> Timeline<'a, N> {
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // ktests build timelines of their own rings
    pub fn new(ring: &'a TraceRing<N>, clock: Option<lumen_time::Clock>) -> Self {
        Timeline { ring, clock }
    }

    /// Each phase's TSC (the first event recorded for it), unless the ring
    /// has wrapped past the boot's events.
    fn phases(&self) -> Option<[Option<u64>; BootPhase::ALL.len()]> {
        if self.ring.written() > self.ring.capacity() as u64 {
            return None;
        }
        let mut at = [None; BootPhase::ALL.len()];
        for e in (0..self.ring.written()).filter_map(|seq| self.ring.read(seq)) {
            if e.kind == Kind::BootPhase as u16
                && let Some(p) = BootPhase::from_raw(e.args[0])
            {
                at[p as usize - 1].get_or_insert(e.tsc);
            }
        }
        Some(at)
    }

    /// Every phase is there, in order from entry, and can be converted.
    pub fn is_complete(&self) -> bool {
        self.clock.is_some()
            && self.phases().is_some_and(|at| at.iter().all(Option::is_some) && at.windows(2).all(|w| w[0] <= w[1]))
    }
}

impl<const N: usize> fmt::Display for Timeline<'_, N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(clock) = self.clock else {
            return f.write_str("unavailable (the TSC isn't calibrated)");
        };
        let Some(at) = self.phases() else {
            let (written, holds) = (self.ring.written(), self.ring.capacity());
            return write!(f, "unavailable ({written} events since boot; the ring holds {holds})");
        };
        let Some(entry) = at[0] else {
            return f.write_str("unavailable (no entry event)");
        };
        for (i, (phase, tsc)) in BootPhase::ALL.iter().zip(at).enumerate() {
            let sep = if i == 0 { "" } else { ", " };
            match tsc.map(|t| t.checked_sub(entry)) {
                Some(Some(ticks)) => write!(f, "{sep}{} {:+}", phase.name(), lumen_time::Millis(clock.ns(ticks)))?,
                Some(None) => write!(f, "{sep}{} before entry", phase.name())?,
                None if self.ring.dropped() > 0 => {
                    write!(f, "{sep}{} missing ({} events dropped)", phase.name(), self.ring.dropped())?
                }
                None => write!(f, "{sep}{} missing", phase.name())?,
            }
        }
        match at[BootPhase::ALL.len() - 1].and_then(|t| t.checked_sub(entry)) {
            Some(ticks) => write!(f, " (total {})", lumen_time::Millis(clock.ns(ticks))),
            None => f.write_str(" (total unknown)"),
        }
    }
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

/// How an event is printed (for tests).
#[cfg(feature = "ktest")]
fn describe(e: &Event) -> crate::ktest::Buf {
    let mut line = crate::ktest::Buf::new();
    let _ = fmt::Write::write_fmt(&mut line, format_args!("{}", Describe(e)));
    line
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
        // wrapped since boot (true while ktests log little). Every phase,
        // once, in order, with non-decreasing timestamps from the boot's
        // origin.
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
                if found == 0 {
                    kassert_eq!(Some(e.tsc), crate::time::boot_tsc()); // entry is the origin
                }
                prev_tsc = e.tsc;
                phases[found] = e.args[0];
                found += 1;
            }
        }
        kassert_eq!(phases, BootPhase::ALL.map(|p| p as u64));
        // The origin is kmain's start, not the TSC's reset or firmware
        // handoff: GDT/IDT setup takes well under 50 ms.
        let clock = crate::time::tsc().ok_or("TSC not calibrated")?;
        let entry = crate::time::boot_tsc().ok_or("boot not marked")?;
        let arch_init = (0..ring.written())
            .filter_map(|seq| ring.read(seq))
            .find(|e| e.kind == Kind::BootPhase as u16 && e.args[0] == BootPhase::ArchInit as u64)
            .ok_or("no arch-init event")?;
        kassert!(clock.ns(arch_init.tsc - entry) < 50_000_000);
        Ok(())
    }
);

crate::ktest!(
    fn the_timeline_says_what_it_lacks() {
        // On a ring of our own, with a clock of 1 kHz (a tick is 1 ms).
        use core::fmt::Write;

        use crate::{kassert, kassert_eq};
        let clock = lumen_time::Clock::new(1000);
        let text = |t: &Timeline<'_, 16>| {
            let mut b = crate::ktest::Buf::new();
            let _ = write!(b, "{t}");
            b
        };
        let ring = TraceRing::<16>::new();
        let phase = |tsc, p: BootPhase| record_at(&ring, tsc, Kind::BootPhase, args([p as u64]));
        kassert_eq!(text(&Timeline::new(&ring, clock)).as_str(), "unavailable (no entry event)");
        phase(10, BootPhase::Entry);
        for (tsc, p) in [(11, BootPhase::ArchInit), (12, BootPhase::Logger), (99, BootPhase::Logger)] {
            phase(tsc, p); // the second logger event is ignored: the first wins
        }
        for (tsc, p) in [(5, BootPhase::Symbols), (14, BootPhase::Memory), (16, BootPhase::Acpi)] {
            phase(tsc, p); // symbols before entry; no stacks
        }
        for (tsc, p) in [(17, BootPhase::Interrupts), (18, BootPhase::Time), (19, BootPhase::Framebuffer)] {
            phase(tsc, p); // no complete: the total isn't framebuffer's
        }
        let t = Timeline::new(&ring, clock);
        kassert_eq!(
            text(&t).as_str(),
            "entry +0.000 ms, arch-init +1.000 ms, logger +2.000 ms, symbols before entry, memory +4.000 ms, \
             stacks missing, acpi +6.000 ms, interrupts +7.000 ms, time +8.000 ms, framebuffer +9.000 ms, \
             complete missing (total unknown)"
        );
        kassert!(!t.is_complete());
        kassert_eq!(text(&Timeline::new(&ring, None)).as_str(), "unavailable (the TSC isn't calibrated)");
        // Every phase, in order: complete, with the total.
        let full = TraceRing::<16>::new();
        for (i, p) in BootPhase::ALL.into_iter().enumerate() {
            record_at(&full, 100 + 2 * i as u64, Kind::BootPhase, args([p as u64]));
        }
        let t = Timeline::new(&full, clock);
        kassert!(t.is_complete());
        kassert!(text(&t).as_str().ends_with("complete +20.000 ms (total 20.000 ms)"));
        // Once the boot's events are overwritten.
        let small = TraceRing::<4>::new();
        for i in 0..5 {
            record_at(&small, i, Kind::BootPhase, args([BootPhase::Entry as u64]));
        }
        let mut b = crate::ktest::Buf::new();
        let _ = write!(b, "{}", Timeline::new(&small, clock));
        kassert_eq!(b.as_str(), "unavailable (5 events since boot; the ring holds 4)");
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
