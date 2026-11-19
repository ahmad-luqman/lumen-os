//! The kernel trace ring: always-on, fixed-size binary events behind the boot
//! timeline ([`Timeline`]) and X-ray mode (M5). The lock-free ring itself is the
//! host-tested `lumen-trace` crate; this module owns the kernel's ring, the
//! event kinds and [`ktrace!`](crate::ktrace!).
//!
//! Timestamps are raw TSC ticks (the boot timeline converts its phases with
//! the frequency `time` calibrates; the shell's `trace` shows them raw). Each CPU
//! records into its own ring ([`ring`] picks the current one), and each event
//! carries the CPU's index; the boot timeline reads the boot CPU's.
//!
//! X-ray's events (run-queue changes, device interrupts handled, contended
//! locks) go into a second set of rings, one per CPU ([`xray_ring`],
//! [`record_xray`]), so their volume never pushes the boot's events or the
//! crash report's last events out of the first; [`read_xray`] is the reader
//! the `Trace` capability (#195) hands apps: events since a sequence number,
//! with the gap counted.

use core::fmt;

use lumen_trace::{Event, Missing, Since, TraceRing};

use crate::arch;

/// Events a ring holds (a power of two). 4096 × 56 bytes = 224 KiB a CPU.
pub const EVENTS: usize = 4096;

static RINGS: [TraceRing<EVENTS>; arch::MAX_CPUS] = [const { TraceRing::new() }; arch::MAX_CPUS];

/// Events an X-ray ring holds (a power of two): 224 KiB a CPU, as the main
/// ring. An idle CPU records a few dozen a second (each wake of a thread
/// queues and dequeues it), a busy one a few hundred (a switch every 10 ms
/// slice queues one thread and dequeues another).
pub const XRAY_EVENTS: usize = 4096;

static XRAY_RINGS: [TraceRing<XRAY_EVENTS>; arch::MAX_CPUS] = [const { TraceRing::new() }; arch::MAX_CPUS];

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
    /// The scheduler switched threads on this CPU. `args`: the previous
    /// thread's id, the next thread's id, the previous thread's state
    /// (`sched::State`: ready when preempted or yielding (also the idle
    /// thread, and a thread another CPU woke before it switched out), or
    /// sleeping, blocked, dead).
    Switch = 5,
    /// X-ray ring: a CPU's run queue changed. `args`: the queue's CPU, its
    /// length after the change, the thread queued or taken off, and
    /// [`RUNQ_ENQUEUE`] or [`RUNQ_DEQUEUE`]. Recorded under that queue's lock,
    /// into that CPU's X-ray ring (by whichever CPU changed it: a wake from
    /// another CPU queues there), so the ring has that queue's changes in
    /// order and its newest says the queue's length.
    RunQueue = 6,
    /// X-ray ring: a device interrupt a handler took (an I/O APIC input or an
    /// MSI vector; not the timer, whose ticks are counted: `cpus`, `ticks`).
    /// `args`: vector, the interrupted rip.
    Irq = 7,
    /// X-ray ring: a `SpinLock` had to wait. `args`: the lock's address, how
    /// many times it looked before it was free, the TSC ticks it waited, and
    /// the line of the `lock` call.
    LockContended = 8,
    /// Recorded by in-kernel tests.
    Test = 0xff,
}

/// [`Kind::RunQueue`]'s `args[3]`: a thread was queued.
pub const RUNQ_ENQUEUE: u64 = 1;
/// [`Kind::RunQueue`]'s `args[3]`: a thread was taken off to run.
pub const RUNQ_DEQUEUE: u64 = 2;

impl Kind {
    // Keep in step with the enum: a kind missing here prints as a raw number.
    const ALL: [Kind; 9] = [
        Kind::BootPhase,
        Kind::Exception,
        Kind::Log,
        Kind::Panic,
        Kind::Switch,
        Kind::RunQueue,
        Kind::Irq,
        Kind::LockContended,
        Kind::Test,
    ];

    fn from_raw(raw: u16) -> Option<Kind> {
        Self::ALL.into_iter().find(|&k| k as u16 == raw)
    }

    fn name(self) -> &'static str {
        match self {
            Kind::BootPhase => "boot-phase",
            Kind::Exception => "exception",
            Kind::Log => "log",
            Kind::Panic => "panic",
            Kind::Switch => "switch",
            Kind::RunQueue => "runq",
            Kind::Irq => "irq",
            Kind::LockContended => "lock",
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
    /// The memory-map summary, then `mm::init`: frame allocator, page tables,
    /// heap, stack window.
    Memory = 5,
    /// Allocated kernel and IST stacks.
    Stacks = 6,
    Acpi = 7,
    /// PCI enumeration over ECAM (`pci::init`).
    Pci = 8,
    /// Interrupt controllers, then the other CPUs started (`smp::start_all`).
    Interrupts = 9,
    /// TSC and timer calibration, interrupts on, the first-tick check (10 ms,
    /// up to 100 ms while no tick has come).
    Time = 10,
    /// The wait until the other CPUs are online (`smp::wait_online`), then
    /// the scheduler started on every CPU (`sched::init`).
    Smp = 11,
    Framebuffer = 12,
    Complete = 13,
}

impl BootPhase {
    pub const ALL: [BootPhase; 13] = [
        BootPhase::Entry,
        BootPhase::ArchInit,
        BootPhase::Logger,
        BootPhase::Symbols,
        BootPhase::Memory,
        BootPhase::Stacks,
        BootPhase::Acpi,
        BootPhase::Pci,
        BootPhase::Interrupts,
        BootPhase::Time,
        BootPhase::Smp,
        BootPhase::Framebuffer,
        BootPhase::Complete,
    ];

    fn from_raw(raw: u64) -> Option<BootPhase> {
        let i = usize::try_from(raw).ok()?.checked_sub(1)?;
        Self::ALL.get(i).copied()
    }

    /// Its position in [`ALL`](Self::ALL), which is its number less one
    /// (checked below).
    const fn index(self) -> usize {
        self as usize - 1
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
            BootPhase::Pci => "pci",
            BootPhase::Interrupts => "interrupts",
            BootPhase::Time => "time",
            BootPhase::Smp => "smp",
            BootPhase::Framebuffer => "framebuffer",
            BootPhase::Complete => "complete",
        }
    }
}

// `ALL` is numbered 1, 2, ... in boot order, entry first and complete last
// (checked here): `from_raw`, `index` and the timeline rely on it. A new
// phase must be added to `ALL` by hand (`name`'s match is the reminder).
const _: () = {
    let mut i = 0;
    while i < BootPhase::ALL.len() {
        assert!(BootPhase::ALL[i].index() == i);
        i += 1;
    }
    assert!(matches!(BootPhase::ALL[0], BootPhase::Entry));
    assert!(matches!(BootPhase::ALL[BootPhase::ALL.len() - 1], BootPhase::Complete));
};

/// The current CPU's ring.
pub fn ring() -> &'static TraceRing<EVENTS> {
    &RINGS[arch::cpu_index()]
}

/// The boot CPU's ring (boot phases are recorded there).
pub fn boot_ring() -> &'static TraceRing<EVENTS> {
    &RINGS[0]
}

/// CPU `cpu`'s X-ray ring (`None` past the CPUs Lumen supports).
pub fn xray_ring(cpu: usize) -> Option<&'static TraceRing<XRAY_EVENTS>> {
    XRAY_RINGS.get(cpu)
}

/// Record an X-ray event in CPU `cpu`'s X-ray ring now (stamped with this
/// CPU's index, which differs from `cpu` when this CPU changes another's
/// run queue). Lock-free and never allocates, so `SpinLock::lock` and
/// interrupt handlers call it; a `cpu` past [`arch::MAX_CPUS`] is ignored.
pub fn record_xray(cpu: usize, kind: Kind, args: [u64; 4]) {
    if let Some(ring) = XRAY_RINGS.get(cpu) {
        record_in(ring, kind, args);
    }
}

/// Copy CPU `cpu`'s X-ray events from sequence number `since` on into `out`
/// (`lumen_trace::TraceRing::read_since`: each number copied or counted
/// missed, once; pass the result's `next` as the next `since`). `None` past
/// the CPUs Lumen supports. Lock-free.
pub fn read_xray(cpu: usize, since: u64, out: &mut [Event]) -> Option<Since> {
    Some(xray_ring(cpu)?.read_since(since, out))
}

/// Run-queue length the newest readable [`Kind::RunQueue`] event in CPU
/// `cpu`'s X-ray ring says, and its sequence number; `None` if the ring
/// holds none. Correct as the queue's length only while that queue's lock is
/// held (`sched::with_run_queue`), since a change records under it.
pub fn xray_run_queue_len(cpu: usize) -> Option<(u64, u64)> {
    let ring = xray_ring(cpu)?;
    let head = ring.written();
    (head.saturating_sub(XRAY_EVENTS as u64)..head)
        .rev()
        .filter_map(|seq| ring.read(seq))
        .find(|e| e.kind == Kind::RunQueue as u16 && e.args[0] == cpu as u64)
        .map(|e| (e.args[1], e.seq))
}

/// Record an event in `ring` now. Use [`ktrace!`](crate::ktrace!) instead.
pub fn record_in<const N: usize>(ring: &TraceRing<N>, kind: Kind, args: [u64; 4]) {
    record_at(ring, arch::rdtsc(), kind, args);
}

/// Record an event in `ring` stamped `tsc`, read earlier (`record_in` passes
/// the current TSC; ktests pass chosen stamps). Boot uses it for
/// `BootPhase::Entry`: its TSC is read before `arch::init_early` (only a bare
/// `rdtsc` may run before GDT/IDT setup, hard rule 6), and the event is
/// recorded after (it stamps this CPU's index, read through GS, which
/// `arch::init_early` sets).
pub fn record_at<const N: usize>(ring: &TraceRing<N>, tsc: u64, kind: Kind, args: [u64; 4]) {
    // pid 0 until processes (M2) exist. A dropped event is counted in the
    // ring's `dropped()`; recording never fails loudly.
    let _ = ring.record(tsc, arch::cpu_index() as u16, kind as u16, 0, args);
}

/// The boot timeline, read once from a ring's boot-phase events, as one line:
/// `entry +0.000 ms, arch-init +0.012 ms, ..., complete +340.000 ms (total
/// 340.000 ms)`, times since `Entry` with `clock`. Each phase is its first
/// event; one with none is named as missing (with the ring's dropped count,
/// if any), one stamped before entry or before an earlier phase says so, and
/// the total is `complete`'s (unknown without it). Without a clock, an entry
/// event, or once the ring has wrapped past the boot's events, it says it is
/// unavailable and why. [`is_complete`](Self::is_complete) is true exactly
/// when the line names no such problem.
pub struct Timeline {
    clock: Option<lumen_time::Clock>,
    /// Each phase's TSC; or, once the ring has wrapped past the boot's
    /// events, how many were written and how many it holds.
    phases: Result<[Option<u64>; BootPhase::ALL.len()], (u64, usize)>,
    dropped: u64,
}

/// How one phase reads on the timeline.
#[derive(Clone, Copy)]
enum Mark {
    /// Ticks since entry.
    At(u64),
    /// Ticks since entry, but earlier than a phase before it.
    OutOfOrder(u64),
    BeforeEntry,
    Missing,
}

impl Timeline {
    /// The boot's timeline: the boot CPU's ring and the calibrated TSC.
    pub fn boot() -> Self {
        Self::new(boot_ring(), crate::time::tsc())
    }

    pub fn new<const N: usize>(ring: &TraceRing<N>, clock: Option<lumen_time::Clock>) -> Self {
        let written = ring.written();
        let phases = if written > ring.capacity() as u64 {
            Err((written, ring.capacity()))
        } else {
            let mut at = [None; BootPhase::ALL.len()];
            for e in (0..written).filter_map(|seq| ring.read(seq)) {
                if e.kind == Kind::BootPhase as u16
                    && let Some(p) = BootPhase::from_raw(e.args[0])
                {
                    at[p.index()].get_or_insert(e.tsc);
                }
            }
            Ok(at)
        };
        Timeline { clock, phases, dropped: ring.dropped() }
    }

    /// The clock, the phases' marks, and complete's ticks since entry, if
    /// the timeline is available.
    fn marks(&self) -> Option<(lumen_time::Clock, [Mark; BootPhase::ALL.len()], Option<u64>)> {
        let at = self.phases.as_ref().ok()?;
        let (entry, clock) = self.origin()?;
        let mut latest = entry;
        let marks = at.map(|tsc| match tsc {
            None => Mark::Missing,
            Some(t) if t < entry => Mark::BeforeEntry,
            Some(t) if t < latest => Mark::OutOfOrder(t - entry),
            Some(t) => {
                latest = t;
                Mark::At(t - entry)
            }
        });
        let total = at[BootPhase::Complete.index()].and_then(|t| t.checked_sub(entry));
        Some((clock, marks, total))
    }

    /// The origin (the entry event's TSC) and the clock the phases are
    /// converted with; `None` while the timeline is unavailable (its
    /// `Display` says why: no clock, the ring wrapped past the boot's events,
    /// or no entry event).
    fn origin(&self) -> Option<(u64, lumen_time::Clock)> {
        let at = self.phases.as_ref().ok()?;
        Some((at[BootPhase::Entry.index()]?, self.clock?))
    }

    /// A TSC reading as the timeline converts a phase stamped with it:
    /// nanoseconds since its origin, with its clock. `None` while the
    /// timeline is unavailable, or for a reading before the origin.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first user: a ktest
    fn since_origin_ns(&self, tsc: u64) -> Option<u64> {
        let (entry, clock) = self.origin()?;
        Some(clock.ns(tsc.checked_sub(entry)?))
    }

    /// Every phase is there, in order from entry, and can be converted.
    pub fn is_complete(&self) -> bool {
        self.marks().is_some_and(|(_, marks, _)| marks.iter().all(|m| matches!(m, Mark::At(_))))
    }
}

impl fmt::Display for Timeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(clock) = self.clock else {
            return f.write_str("unavailable (the TSC isn't calibrated)");
        };
        if let Err((written, holds)) = self.phases {
            return write!(f, "unavailable ({written} events since boot; the ring holds {holds})");
        }
        let Some((_, marks, total)) = self.marks() else {
            return match self.dropped {
                0 => f.write_str("unavailable (no entry event)"),
                n => write!(f, "unavailable (no entry event; {n} events dropped)"),
            };
        };
        let ms = |ticks| lumen_time::Millis(clock.ns(ticks));
        for (i, (phase, mark)) in BootPhase::ALL.iter().zip(marks).enumerate() {
            let sep = if i == 0 { "" } else { ", " };
            write!(f, "{sep}{} ", phase.name())?;
            match mark {
                Mark::At(ticks) => write!(f, "{:+}", ms(ticks))?,
                Mark::OutOfOrder(ticks) => write!(f, "{:+} (out of order)", ms(ticks))?,
                Mark::BeforeEntry => f.write_str("before entry")?,
                Mark::Missing if self.dropped > 0 => write!(f, "missing ({} events dropped)", self.dropped)?,
                Mark::Missing => f.write_str("missing")?,
            }
        }
        match total {
            Some(ticks) => write!(f, " (total {})", ms(ticks)),
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
            Kind::Switch => {
                write!(f, " from={} to={}", a[0], a[1])?;
                match crate::sched::State::name_of(a[2]) {
                    Some(state) => write!(f, " ({state})"),
                    None => write!(f, " state={}", a[2]),
                }
            }
            Kind::RunQueue => {
                match a[3] {
                    RUNQ_ENQUEUE => write!(f, " queue={} enqueue", a[0])?,
                    RUNQ_DEQUEUE => write!(f, " queue={} dequeue", a[0])?,
                    op => write!(f, " queue={} op={op}", a[0])?,
                }
                write!(f, " thread={} len={}", a[2], a[1])
            }
            Kind::Irq => write!(f, " vector={:#x} rip={:#x}", a[0], a[1]),
            Kind::LockContended => write!(f, " lock={:#x} spins={} waited={} line={}", a[0], a[1], a[2], a[3]),
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
        let mut tscs = [0u64; BootPhase::ALL.len()];
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
                tscs[found] = e.tsc;
                found += 1;
            }
        }
        kassert_eq!(phases, BootPhase::ALL.map(|p| p as u64));
        // So `tscs` is in `ALL`'s order. Entry is the origin, at or before
        // arch-init (above), and at least Limine's post-handoff wait after
        // its handoff tick (`lumen_time::handoff::BeforeKmain::check`, which
        // also checks the tick's units, and that Limine's frequency is at
        // least half of ours). None of it is a time bound, so it holds
        // however long the boot took (#55).
        // That bounds the origin between Limine's handoff and arch-init; an
        // origin read later than `kmain`'s first instruction, before
        // arch-init, isn't caught.
        let (entry, arch_init) = (tscs[BootPhase::Entry.index()], tscs[BootPhase::ArchInit.index()]);
        kassert_eq!(Some(entry), crate::time::boot_tsc());
        let before = match crate::boot::before_kmain() {
            Ok(before) => before,
            Err(why) => {
                crate::ktest::diag(format_args!("before kmain: unknown ({why})"));
                return Err("Limine's times or the origin are unknown");
            }
        };
        kassert_eq!(before.entry(), entry);
        // Limine's frequency is the one Limine reported, not ours.
        kassert_eq!(Some(before.limine().hz()), crate::boot::limine_tsc_frequency());
        let (clock, limine_hz) = (before.clock(), before.limine().hz());
        crate::ktest::diag(format_args!(
            "arch-init {} after entry; {before}; Limine's TSC {limine_hz} Hz, ours {} Hz",
            lumen_time::Millis(clock.ns(arch_init - entry)),
            clock.hz()
        ));
        if let Err(disorder) = before.check() {
            crate::ktest::diag(format_args!("out of order: {disorder}"));
            return Err("the origin is out of order with Limine's handoff");
        }
        Ok(())
    }
);

crate::ktest!(
    fn log_stamps_count_from_the_timelines_origin() {
        // `time::since_boot_ns` (the logger's clock), read between two TSC
        // reads that the timeline converts as it converts a phase
        // (`Timeline::since_origin_ns`). The origin comes from the
        // timeline's entry event, not `boot_tsc()`, so the expected values
        // don't reuse the code under test; the clock is shared by design.
        // Interrupts off keeps the three reads on one CPU and close together,
        // and `cpuid` fences the stamp's read from the TSC reads on either
        // side. Order only: it can't fail on a correct kernel unless the boot
        // ring has wrapped (then it fails, saying so), and it catches a log
        // origin or unit error larger than its window (microseconds here,
        // wider if the host stalls between the reads). It covers
        // `since_boot_ns` and `mark_boot` only: that klog's printed stamps
        // agree with the timeline is test_boot_timeline_printed's to check
        // (#125).
        let timeline = Timeline::boot();
        let Some((entry, clock)) = timeline.origin() else {
            crate::ktest::diag(format_args!("timeline: {timeline}"));
            return Err("the timeline has no origin (the boot ring wrapped, no entry event, or no clock)");
        };
        let (earlier, stamp, later) = {
            let _off = arch::irq::push_off();
            let earlier = arch::rdtsc();
            arch::serialize();
            let stamp = crate::time::since_boot_ns();
            arch::serialize();
            (earlier, stamp, arch::rdtsc())
        };
        let stamp = stamp.ok_or("no log stamp (since_boot_ns is None)")?;
        let (Some(low), Some(high)) = (timeline.since_origin_ns(earlier), timeline.since_origin_ns(later)) else {
            return Err("a TSC read precedes the timeline's entry event");
        };
        if !(low..=high).contains(&stamp) {
            crate::ktest::diag(format_args!(
                "log stamp {stamp} ns; the timeline's reads around it {low} ns and {high} ns; \
                 entry tick {entry}, boot_tsc {:?}, {} Hz",
                crate::time::boot_tsc(),
                clock.hz()
            ));
            return Err("the log's stamp isn't between the timeline's reads around it");
        }
        Ok(())
    }
);

crate::ktest!(
    fn the_timeline_says_what_it_lacks() {
        // On rings of our own, with a clock of 1 kHz (a tick is 1 ms).
        use core::fmt::Write;

        use crate::{kassert, kassert_eq};
        let clock = lumen_time::Clock::new(1000);
        let text = |t: &Timeline| {
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
        // Only boot-phase events count (a log event's args[0] is its level).
        record_at(&ring, 15, Kind::Log, args([BootPhase::Stacks as u64]));
        for (tsc, p) in [(17, BootPhase::Interrupts), (18, BootPhase::Time), (19, BootPhase::Framebuffer)] {
            phase(tsc, p); // no smp, no complete: the total isn't framebuffer's
        }
        let t = Timeline::new(&ring, clock);
        kassert_eq!(
            text(&t).as_str(),
            "entry +0.000 ms, arch-init +1.000 ms, logger +2.000 ms, symbols before entry, memory +4.000 ms, \
             stacks missing, acpi +6.000 ms, pci missing, interrupts +7.000 ms, time +8.000 ms, smp missing, \
             framebuffer +9.000 ms, complete missing (total unknown)"
        );
        kassert!(!t.is_complete());
        kassert_eq!(text(&Timeline::new(&ring, None)).as_str(), "unavailable (the TSC isn't calibrated)");

        // Every phase, in order: complete, with the total; but not without a clock.
        let full = TraceRing::<16>::new();
        for (i, p) in BootPhase::ALL.into_iter().enumerate() {
            record_at(&full, 100 + 2 * i as u64, Kind::BootPhase, args([p as u64]));
        }
        let t = Timeline::new(&full, clock);
        kassert!(t.is_complete());
        kassert!(text(&t).as_str().ends_with("complete +24.000 ms (total 24.000 ms)"));
        kassert!(!Timeline::new(&full, None).is_complete());

        // Every phase after entry, but memory before symbols: it says so.
        let swapped = TraceRing::<16>::new();
        for (i, p) in BootPhase::ALL.into_iter().enumerate() {
            let tsc = match p {
                BootPhase::Memory => 105,
                _ => 100 + 2 * i as u64,
            };
            record_at(&swapped, tsc, Kind::BootPhase, args([p as u64]));
        }
        let t = Timeline::new(&swapped, clock);
        kassert!(!t.is_complete());
        kassert!(text(&t).as_str().contains("symbols +6.000 ms, memory +5.000 ms (out of order), stacks +10.000 ms"));

        // In order, but no complete.
        let unfinished = TraceRing::<16>::new();
        for (i, p) in BootPhase::ALL.into_iter().enumerate().take(BootPhase::ALL.len() - 1) {
            record_at(&unfinished, 100 + 2 * i as u64, Kind::BootPhase, args([p as u64]));
        }
        let t = Timeline::new(&unfinished, clock);
        kassert!(!t.is_complete());
        kassert!(text(&t).as_str().ends_with("framebuffer +22.000 ms, complete missing (total unknown)"));

        // Every phase but entry.
        let headless = TraceRing::<16>::new();
        for (i, p) in BootPhase::ALL.into_iter().enumerate().skip(1) {
            record_at(&headless, 100 + 2 * i as u64, Kind::BootPhase, args([p as u64]));
        }
        let t = Timeline::new(&headless, clock);
        kassert!(!t.is_complete());
        kassert_eq!(text(&t).as_str(), "unavailable (no entry event)");

        // A ring exactly full still holds the boot; one more event and it doesn't.
        let small = TraceRing::<4>::new();
        for p in &BootPhase::ALL[..4] {
            record_at(&small, *p as u64, Kind::BootPhase, args([*p as u64]));
        }
        kassert!(text(&Timeline::new(&small, clock)).as_str().starts_with("entry +0.000 ms, arch-init +1.000 ms"));
        record_at(&small, 5, Kind::BootPhase, args([BootPhase::Memory as u64]));
        let t = Timeline::new(&small, clock);
        kassert!(!t.is_complete());
        kassert_eq!(text(&t).as_str(), "unavailable (5 events since boot; the ring holds 4)");
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
        // The cost per event, in TSC ticks, printed for the record: the
        // fastest of BATCHES batches of N events, 1 ms apart, so a host
        // stalling the vCPU must slow every batch to move it. Uses its own
        // ring so the kernel's ring keeps its boot events.
        static BENCH: TraceRing<EVENTS> = TraceRing::new();
        const N: u64 = 1_000;
        const BATCHES: u64 = 20;
        let (mut per_event, mut slowest) = (u64::MAX, 0);
        for b in 0..BATCHES {
            if b > 0 {
                crate::time::spin(1_000_000);
            }
            let t0 = arch::rdtsc();
            for i in 0..N {
                record_in(&BENCH, Kind::Test, [b * N + i, 0, 0, 0]);
            }
            let this = (arch::rdtsc() - t0) / N;
            (per_event, slowest) = (per_event.min(this), slowest.max(this));
        }
        crate::ktest::diag(format_args!(
            "trace: {per_event} TSC ticks per event (the fastest of {BATCHES} batches of {N}; the slowest {slowest})"
        ));
        // Loose bounds: catches gross regressions such as waiting or doing
        // I/O (e.g. serial output) on every event, and a TSC that doesn't
        // move; not a cost paid less often than once per N events (some
        // batch has none, and the fastest is taken), nor an added
        // uncontended lock. Under TCG the TSC follows
        // a host clock, not guest cycles. At risk on a host slowing the vCPU
        // through every batch.
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

/// Ktests: CPU `cpu`'s X-ray events from `since` on (its whole ring at
/// most), and the numbers missed.
#[cfg(feature = "ktest")]
fn xray_since(cpu: usize, since: u64) -> (alloc::vec::Vec<Event>, u64) {
    let mut all = alloc::vec::Vec::new();
    let mut out = alloc::vec![Event::default(); 256];
    let (mut next, mut missed) = (since, 0);
    while let Some(r) = read_xray(cpu, next, &mut out) {
        all.extend_from_slice(&out[..r.copied]);
        missed += r.missed;
        if r.next == next {
            break;
        }
        next = r.next;
    }
    (all, missed)
}

/// Ktests: whether `kind` is one of X-ray's.
#[cfg(feature = "ktest")]
fn is_xray_kind(kind: u16) -> bool {
    [Kind::RunQueue, Kind::Irq, Kind::LockContended].iter().any(|&k| k as u16 == kind)
}

crate::ktest!(
    fn run_queue_changes_are_traced_in_order() {
        // A thread spawned on CPU 1 is queued there (by this CPU) and taken
        // off to run (by CPU 1): both are in CPU 1's X-ray ring, naming it,
        // in that order. Every change to that queue since then steps the
        // length by one (up for a queue, down for a dequeue), so no change
        // went unrecorded or out of order; and the newest says the queue's
        // length, read under its lock.
        use crate::{kassert, kassert_eq};
        let since = xray_ring(1).ok_or("no X-ray ring for cpu 1")?.written();
        let h = crate::sched::spawn_on(1, "xray-runq", || {}).map_err(|_| "spawn on cpu 1 failed")?;
        let id = h.thread().id();
        crate::sched::join_bounded(h)?;
        let (events, missed) = xray_since(1, since);
        kassert_eq!(missed, 0);
        let queue: alloc::vec::Vec<&Event> =
            events.iter().filter(|e| e.kind == Kind::RunQueue as u16 && e.args[0] == 1).collect();
        let at = |op| queue.iter().position(|e| e.args[2] == id && e.args[3] == op);
        let (queued, taken) = (at(RUNQ_ENQUEUE), at(RUNQ_DEQUEUE));
        if !matches!((queued, taken), (Some(q), Some(t)) if q < t) {
            crate::ktest::diag(format_args!("thread {id}: queued at {queued:?}, taken at {taken:?}"));
            return Err("the thread's queueing and dequeueing aren't both traced, in order");
        }
        kassert_eq!(queue[queued.unwrap_or(0)].cpu, arch::cpu_index() as u16);
        kassert_eq!(queue[taken.unwrap_or(0)].cpu, 1);
        for pair in queue.windows(2) {
            let step =
                if pair[1].args[3] == RUNQ_ENQUEUE { pair[0].args[1] + 1 } else { pair[0].args[1].wrapping_sub(1) };
            if pair[1].args[1] != step {
                crate::ktest::diag(format_args!("{} then {}", Describe(pair[0]), Describe(pair[1])));
                return Err("a run-queue change is missing from the trace");
            }
        }
        let (len, traced) = crate::sched::with_run_queue(1, |len| (len, xray_run_queue_len(1))).ok_or("no cpu 1")?;
        kassert!(traced.is_some());
        kassert_eq!(traced.map(|(l, _)| l), Some(len as u64));
        Ok(())
    }
);

crate::ktest!(
    fn a_contended_lock_is_traced() {
        // This CPU and CPU 1 take one lock in turn until one of them had to
        // wait for it, which then says so in its X-ray ring: the lock's
        // address, how often it looked, and the line of its `lock` call.
        // Waits for that effect (up to 10 s, a safety net), times nothing.
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        static HOT: crate::sync::SpinLock<u64> = crate::sync::SpinLock::new(0);
        static LINE: AtomicU32 = AtomicU32::new(0);
        static GO: AtomicBool = AtomicBool::new(false);
        static STOP: AtomicBool = AtomicBool::new(false);
        static DONE: AtomicBool = AtomicBool::new(false);
        fn hammer() {
            while !STOP.load(Ordering::Relaxed) {
                let (mut g, line) = (HOT.lock(), line!());
                LINE.store(line, Ordering::Relaxed);
                *g += 1;
                // Hold it a little, so the other CPU finds it held.
                for _ in 0..64 {
                    core::hint::spin_loop();
                }
            }
        }
        fn on_cpu_1() {
            let deadline = crate::time::now_ns() + 10_000_000_000;
            while !GO.load(Ordering::Acquire) && crate::time::now_ns() < deadline {
                core::hint::spin_loop();
            }
            hammer();
            DONE.store(true, Ordering::Release);
        }
        let addr = core::ptr::from_ref(&HOT).addr() as u64;
        let since = [0, 1].map(|c| xray_ring(c).map_or(0, |r| r.written()));
        let contended = |c: usize| {
            let (events, _) = xray_since(c, since[c]);
            events.into_iter().find(|e| e.kind == Kind::LockContended as u16 && e.args[0] == addr)
        };
        crate::smp::run_on_test(1, on_cpu_1)?;
        GO.store(true, Ordering::Release);
        let deadline = crate::time::now_ns() + 10_000_000_000;
        let mut found = None;
        while found.is_none() && crate::time::now_ns() < deadline {
            for _ in 0..1000 {
                let mut g = HOT.lock();
                *g += 1;
            }
            found = contended(0).or_else(|| contended(1));
        }
        STOP.store(true, Ordering::Relaxed);
        kassert!(crate::smp::job_reached(1, || DONE.load(Ordering::Acquire)));
        let e = found.ok_or("no contended-lock event for the lock in 10 s")?;
        kassert!(e.args[1] > 0);
        // CPU 1 waits in `hammer`; this CPU's loop above names another line.
        if e.cpu == 1 {
            kassert_eq!(e.args[3], u64::from(LINE.load(Ordering::Relaxed)));
        }
        kassert!(e.args[3] > 0);
        Ok(())
    }
);

crate::ktest!(
    fn xray_events_leave_the_main_ring_alone() {
        // With X-ray's kinds recorded for real (a thread queued and run on
        // CPU 1, and the X-ray rings of both CPUs filled two laps over),
        // the main rings get none of them: each main ring's new events are
        // of the old kinds, none dropped; the boot timeline reads as it did
        // before (complete, if it was: the ring hadn't wrapped since boot);
        // and the crash report's view of this CPU's ring (`write_recent`)
        // still ends with the events from before the load, the marker
        // among them.
        use core::fmt::Write;

        use crate::{kassert, kassert_eq};
        const MARK: u64 = 0x5a7a;
        let cpus = arch::MAX_CPUS;
        let complete_before = Timeline::boot().is_complete();
        let main_before: [u64; arch::MAX_CPUS] = core::array::from_fn(|c| RINGS[c].written());
        let dropped_before: [u64; arch::MAX_CPUS] = core::array::from_fn(|c| RINGS[c].dropped());
        let me = arch::cpu_index();
        let mark =
            ring().record(arch::rdtsc(), me as u16, Kind::Test as u16, 0, args([MARK])).ok_or("marker dropped")?;
        let xray_before = [me, 1].map(|c| xray_ring(c).map_or(0, |r| r.written()));
        let h = crate::sched::spawn_on(1, "xray-load", || {}).map_err(|_| "spawn on cpu 1 failed")?;
        crate::sched::join_bounded(h)?;
        for _ in 0..2 * XRAY_EVENTS {
            for c in [me, 1] {
                record_xray(c, Kind::Irq, [0, 0, 0, 0]);
            }
        }
        for (i, c) in [me, 1].into_iter().enumerate() {
            let grew = xray_ring(c).map_or(0, |r| r.written()) - xray_before[i];
            kassert!(grew >= 2 * XRAY_EVENTS as u64);
        }
        for c in 0..cpus {
            let ring = &RINGS[c];
            for seq in main_before[c]..ring.written() {
                if let Some(e) = ring.read(seq)
                    && is_xray_kind(e.kind)
                {
                    crate::ktest::diag(format_args!("cpu {c}'s main ring: {}", Describe(&e)));
                    return Err("an X-ray event in a main ring");
                }
            }
            kassert_eq!(ring.dropped(), dropped_before[c]);
        }
        let timeline = Timeline::boot();
        if timeline.is_complete() != complete_before {
            crate::ktest::diag(format_args!("before: complete {complete_before}; now: {timeline}"));
            return Err("the boot timeline changed under X-ray's load");
        }
        // The crash report prints the newest events; ask for all since the
        // marker (still well inside the ring) and find it first.
        let since_mark = RINGS[me].written() - mark;
        kassert!(since_mark < EVENTS as u64);
        let mut text = alloc::string::String::new();
        write_recent(&mut text, since_mark, "").map_err(|_| "write_recent failed")?;
        let first = text.lines().next().unwrap_or("");
        let mut want = alloc::string::String::new();
        let _ = write!(want, "#{mark:<6}");
        if !(first.starts_with(want.as_str()) && first.contains("test") && first.contains("0x5a7a")) {
            crate::ktest::diag(format_args!("first of {since_mark}: {first:?}"));
            return Err("the crash report's events don't reach back to the marker");
        }
        Ok(())
    }
);
