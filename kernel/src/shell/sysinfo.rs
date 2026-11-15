//! The shell's system commands: `cpus`, `mem`, `irqs`, `threads`, `lspci`,
//! `dmesg`, `date` and `top`. Each prints a header line (`<name>: ...`), and
//! all but `date` rows after it. In a pipeline `cpus`, `irqs`, `mem`,
//! `threads` and `lspci` give records instead ([`RECORDS`]): `cpus`,
//! `threads` and `lspci` the table they print (under their header), `irqs` its per-vector table only (not the
//! I/O APIC's inputs), `mem` a table of what its prose says (in bytes).
//! Output with rows is made into a `String` first and written in one write
//! (no log line lands inside it, and nothing allocating is formatted under
//! the screen's lock); `dmesg` writes its lines in chunks
//! (`shell::write_in_chunks`) instead, so a slow UART doesn't hold the
//! log's line lock (interrupts off) for long.

use super::{Ctx, write_in_chunks};
use crate::arch::{self, MAX_CPUS, idt};
use crate::sched::{self, CpuTicks, ThreadInfo};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use lumen_mm::FRAME_SIZE;
use lumen_shell::Key;
use lumen_shell::pipe::{self, Align, Column, Kind, Value};
use lumen_time::{NS_PER_SEC, per_second, percent};

/// A size in bytes for humans: `512 B`, `12.5 KiB`, `3.0 MiB`, `2.0 GiB`.
struct Bytes(u64);

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [(u64, &str); 3] = [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")];
        match UNITS.into_iter().find(|&(s, _)| self.0 >= s) {
            Some((scale, unit)) => {
                let tenths = (u128::from(self.0) * 10 + u128::from(scale / 2)) / u128::from(scale);
                write!(f, "{}.{} {unit}", tenths / 10, tenths % 10)
            }
            None => write!(f, "{} B", self.0),
        }
    }
}

/// How busy `cpu` was over `ticks`, for a percentage column: absent
/// (`-`) if its scheduler isn't running, `stalled` if it runs but took no
/// tick.
fn busy(cpu: usize, ticks: CpuTicks) -> Value {
    match (sched::running_on(cpu), ticks.ticks) {
        (false, _) => DASH,
        (true, 0) => Value::Absent("stalled"),
        (true, _) => int(ticks.busy_percent()),
    }
}

/// `n` as a table's number (absent, `-`, past `i64::MAX`).
fn int(n: impl TryInto<i64>) -> Value {
    n.try_into().map_or(DASH, Value::Int)
}

fn text(s: &str) -> Value {
    Value::Text(String::from(s))
}

/// No value, printed `-`.
const DASH: Value = Value::Absent("-");

/// What makes a system command's records.
pub type MakeTable = fn() -> pipe::Table;

/// The system commands that give records in a pipeline (instead of their
/// text), and the tables they give.
pub const RECORDS: [(&str, MakeTable); 5] =
    [("cpus", cpu_table), ("irqs", vector_table), ("lspci", pci_table), ("mem", mem_table), ("threads", thread_table)];

/// The records `name` gives in a pipeline, if it is one of [`RECORDS`].
pub fn records(name: &str) -> Option<MakeTable> {
    RECORDS.iter().find(|(n, _)| *n == name).map(|&(_, table)| table)
}

/// Whether there are `args` for a command taking none (`name`); if so, it
/// says its usage line (ending a pipeline).
fn extra_args(ctx: &mut Ctx<'_>, name: &str, args: &[String]) -> bool {
    if !args.is_empty() {
        ctx.fail(format_args!("usage: {name}"));
    }
    !args.is_empty()
}

/// The CPUs online (index, local APIC id).
fn online() -> impl Iterator<Item = (usize, u32)> {
    (0..MAX_CPUS).filter_map(|c| crate::smp::online_lapic_id(c).map(|id| (c, id)))
}

/// The CPUs started but not online (stuck coming up).
fn starting() -> impl Iterator<Item = usize> {
    (0..MAX_CPUS).filter(|&c| crate::smp::is_starting(c))
}

/// `, 1 starting, 2 halted past the 8 supported` for the CPUs that aren't
/// online, or nothing.
fn not_online() -> String {
    let mut t = String::new();
    let stuck = starting().count();
    if stuck > 0 {
        let _ = write!(t, ", {stuck} starting");
    }
    let halted = crate::smp::halted_past_max();
    if halted > 0 {
        let _ = write!(t, ", {halted} halted past the {MAX_CPUS} supported");
    }
    t
}

/// Write `text` to the console in one write.
fn emit(ctx: &mut Ctx<'_>, text: &str) {
    let _ = ctx.out.write_str(text);
}

/// `cpus`: each CPU online, its scheduler's ticks and how busy it has been
/// since it started, and the threads homed there; then any CPU stuck
/// starting. (The boot CPU reads nearly 100% busy even when nothing
/// happens: the serial shell polls serial, yielding, and never sleeps.)
pub fn cpus(ctx: &mut Ctx<'_>, args: &[String]) {
    if extra_args(ctx, "cpus", args) {
        return;
    }
    let mut t = String::new();
    let n = online().count();
    let _ = writeln!(t, "cpus: {n} online{}", not_online());
    cpu_table().render(&mut t);
    emit(ctx, &t);
}

/// `cpus`'s rows: busy is a percentage, `-` without a scheduler, `stalled`
/// if it runs but took no tick.
fn cpu_table() -> pipe::Table {
    const COLUMNS: [Column; 6] = [
        Column::new("cpu", Kind::Int, Align::Right, 3),
        Column::new("apic", Kind::Int, Align::Right, 4),
        Column::new("scheduler", Kind::Text, Align::Left, 11),
        Column::new("ticks", Kind::Int, Align::Left, 10),
        Column::new("busy", Kind::Percent, Align::Right, 4),
        Column::new("threads", Kind::Int, Align::Right, 7),
    ];
    let threads = sched::threads();
    let mut t = pipe::Table::new(&COLUMNS);
    for (cpu, apic) in online() {
        let homed = threads.iter().filter(|th| th.cpu == cpu).count();
        let c = sched::cpu_ticks(cpu);
        let running = if sched::running_on(cpu) { "running" } else { "not started" };
        t.push(vec![int(cpu), int(apic), text(running), int(c.ticks), busy(cpu, c), int(homed)]);
    }
    for cpu in starting() {
        t.push(vec![int(cpu), DASH, text("starting"), DASH, DASH, DASH]);
    }
    t
}

/// The kernel stacks registered, by kind: its name, how many, their bytes
/// (without guard pages).
fn stack_kinds() -> Vec<(&'static str, usize, u64)> {
    let mut kinds: Vec<(&'static str, usize, u64)> = Vec::new();
    for s in arch::stacks::REGISTRY.iter() {
        let name = s.kind.name();
        match kinds.iter_mut().find(|(n, ..)| *n == name) {
            Some((_, count, bytes)) => {
                *count += 1;
                *bytes += s.range.size();
            }
            None => kinds.push((name, 1, s.range.size())),
        }
    }
    kinds
}

/// `mem`: physical frames, the heap and kernel stacks.
pub fn mem(ctx: &mut Ctx<'_>, args: &[String]) {
    if extra_args(ctx, "mem", args) {
        return;
    }
    let frames = crate::mm::frame::counts();
    let heap = crate::mm::heap::stats();
    let kinds = stack_kinds();
    let (owned, free) = (frames.owned as u64, frames.free as u64);
    let mut t = String::new();
    let _ = writeln!(t, "mem: frames, heap and kernel stacks");
    let _ = writeln!(
        t,
        "  frames  {} of {} used ({} frames of {owned} free, 4 KiB each)",
        Bytes((owned - free) * FRAME_SIZE),
        Bytes(owned * FRAME_SIZE),
        free
    );
    let _ = writeln!(
        t,
        "  heap    {} of {} used in {} allocations, {} free",
        Bytes(heap.used as u64),
        Bytes(heap.size as u64),
        heap.live,
        Bytes(heap.free() as u64)
    );
    let stacks: u64 = kinds.iter().map(|&(_, _, bytes)| bytes).sum();
    let count: usize = kinds.iter().map(|&(_, count, _)| count).sum();
    let _ = write!(t, "  stacks  {count} ({}, not counting their guard pages):", Bytes(stacks));
    for (name, n, size) in &kinds {
        let _ = write!(t, " {n} {name} ({})", Bytes(*size));
    }
    t.push('\n');
    emit(ctx, &t);
}

/// `mem` in a pipeline: frames, the heap and each kind of kernel stack
/// (`<kind> stacks`), with their size and what is used of it in bytes (a
/// stack's all of it; guard pages not counted), and how many there are
/// (frames, live allocations, stacks).
fn mem_table() -> pipe::Table {
    const COLUMNS: [Column; 4] = [
        Column::new("area", Kind::Text, Align::Left, 0),
        Column::new("size", Kind::Int, Align::Right, 0),
        Column::new("used", Kind::Int, Align::Right, 0),
        Column::new("count", Kind::Int, Align::Right, 0),
    ];
    let frames = crate::mm::frame::counts();
    let heap = crate::mm::heap::stats();
    let mut t = pipe::Table::new(&COLUMNS);
    let (owned, free) = (frames.owned as u64, frames.free as u64);
    t.push(vec![text("frames"), int(owned * FRAME_SIZE), int((owned - free) * FRAME_SIZE), int(owned)]);
    t.push(vec![text("heap"), int(heap.size), int(heap.used), int(heap.live)]);
    for (name, count, bytes) in stack_kinds() {
        t.push(vec![Value::Text(alloc::format!("{name} stacks")), int(bytes), int(bytes), int(count)]);
    }
    t
}

/// `irqs`: every I/O APIC input decoded, then the interrupts taken on each
/// vector from 32 on that has taken any or has a routed input.
pub fn irqs(ctx: &mut Ctx<'_>, args: &[String]) {
    if extra_args(ctx, "irqs", args) {
        return;
    }
    let inputs = crate::interrupts::inputs();
    let mut t = String::new();
    if inputs.is_empty() {
        let _ = writeln!(t, "irqs: no I/O APIC, interrupts per vector");
    } else {
        let _ = writeln!(t, "irqs: {} I/O APIC inputs, interrupts per vector", inputs.len());
        input_table(&inputs).render(&mut t);
    }
    vector_table().render(&mut t);
    emit(ctx, &t);
}

/// `irqs`' first table: each I/O APIC input decoded (an unrouted, masked
/// one is only its number and `masked`).
fn input_table(inputs: &[crate::interrupts::Input]) -> pipe::Table {
    use arch::apic::{Polarity, Trigger};
    const COLUMNS: [Column; 8] = [
        Column::new("gsi", Kind::Int, Align::Right, 3),
        Column::new("vector", Kind::Hex, Align::Left, 6),
        Column::new("delivery", Kind::Text, Align::Left, 14),
        Column::new("trigger", Kind::Text, Align::Left, 7),
        Column::new("polarity", Kind::Text, Align::Left, 8),
        Column::new("dest", Kind::Text, Align::Left, 4),
        Column::new("state", Kind::Text, Align::Left, 8),
        Column::new("count", Kind::Int, Align::Left, 0),
    ];
    let mut t = pipe::Table::new(&COLUMNS);
    for i in inputs {
        let e = i.entry;
        if !i.routed && e.masked() {
            t.push(vec![int(i.gsi), DASH, DASH, DASH, DASH, DASH, text("masked"), DASH]);
            continue;
        }
        let delivery = e.delivery().map_or(String::from("reserved"), |d| alloc::format!("{d:?}"));
        let trigger = if e.trigger() == Trigger::Level { "level" } else { "edge" };
        let polarity = if e.polarity() == Polarity::ActiveLow { "low" } else { "high" };
        let dest =
            if e.logical() { alloc::format!("l{}", e.destination()) } else { alloc::format!("{}", e.destination()) };
        let state = if e.masked() { "masked" } else { "unmasked" };
        // A vector under 32 is an exception's: its count isn't this input's.
        let count = if e.vector() < 32 { DASH } else { int(idt::vector_count(e.vector())) };
        t.push(vec![
            int(i.gsi),
            int(e.vector()),
            Value::Text(delivery),
            text(trigger),
            text(polarity),
            Value::Text(dest),
            text(state),
            count,
        ]);
    }
    t
}

/// `irqs`' second table, and its records in a pipeline: the interrupts
/// taken on each vector from 32 on that has taken any or has a routed
/// input.
fn vector_table() -> pipe::Table {
    const COLUMNS: [Column; 3] = [
        Column::new("vector", Kind::Hex, Align::Left, 6),
        Column::new("use", Kind::Text, Align::Left, 16),
        Column::new("count", Kind::Int, Align::Left, 0),
    ];
    let inputs = crate::interrupts::inputs();
    let mut t = pipe::Table::new(&COLUMNS);
    for v in 32..=255u8 {
        let count = idt::vector_count(v);
        let routed = inputs.iter().any(|i| i.routed && i.entry.vector() == v);
        if count > 0 || routed {
            t.push(vec![int(v), Value::Text(alloc::format!("{}", idt::vector_use(v))), int(count)]);
        }
    }
    t
}

/// `lspci`: every PCI function found at boot.
pub fn lspci(ctx: &mut Ctx<'_>, args: &[String]) {
    if extra_args(ctx, "lspci", args) {
        return;
    }
    let table = pci_table();
    let mut t = String::new();
    let n = table.rows().len();
    let _ = writeln!(t, "lspci: {n} function{}", if n == 1 { "" } else { "s" });
    table.render(&mut t);
    emit(ctx, &t);
}

/// `lspci`'s rows: the address, vendor and device ids (hex, so `where
/// vendor = 0x1af4` works), the class's name and the device's (`-` if
/// Lumen doesn't know it).
fn pci_table() -> pipe::Table {
    use crate::pci::{self, names};
    const COLUMNS: [Column; 5] = [
        Column::new("addr", Kind::Text, Align::Left, 12),
        Column::new("vendor", Kind::Hex, Align::Left, 6),
        Column::new("device", Kind::Hex, Align::Left, 6),
        Column::new("class", Kind::Text, Align::Left, 0),
        Column::new("name", Kind::Text, Align::Left, 0),
    ];
    let mut t = pipe::Table::new(&COLUMNS);
    for d in pci::devices() {
        let name = names::device(d.vendor, d.device, d.subsystem.1).map_or(DASH, text);
        let addr = Value::Text(alloc::format!("{}", d.addr));
        t.push(vec![addr, int(d.vendor), int(d.device), text(names::class(d.class, d.subclass)), name]);
    }
    t
}

/// `threads`: every thread, its CPU, state and the timer ticks that found
/// it running.
pub fn threads(ctx: &mut Ctx<'_>, args: &[String]) {
    if extra_args(ctx, "threads", args) {
        return;
    }
    let table = thread_table();
    let mut t = String::new();
    let _ = writeln!(t, "threads: {}", table.rows().len());
    table.render(&mut t);
    emit(ctx, &t);
}

fn thread_table() -> pipe::Table {
    const COLUMNS: [Column; 5] = [
        Column::new("thread", Kind::Text, Align::Left, 16),
        Column::new("id", Kind::Int, Align::Right, 2),
        Column::new("cpu", Kind::Int, Align::Right, 3),
        Column::new("state", Kind::Text, Align::Left, 8),
        Column::new("ticks", Kind::Int, Align::Right, 0),
    ];
    let mut t = pipe::Table::new(&COLUMNS);
    for th in sched::threads() {
        t.push(vec![text(th.name), int(th.id), int(th.cpu), text(th.state.name()), int(th.ticks)]);
    }
    t
}

/// `dmesg`: the newest whole log lines kept (the last `klog::RING_BYTES`
/// bytes), as serial printed them, saying whether older ones were
/// overwritten.
pub fn dmesg(ctx: &mut Ctx<'_>, _: &[String]) {
    let mut buf = vec![0; crate::klog::RING_BYTES];
    let (n, written) = crate::klog::copy_recent(&mut buf);
    let lost = written > crate::klog::RING_BYTES as u64;
    let text = String::from_utf8_lossy(&buf[..n]);
    let lines = text.lines().count();
    let older = if lost { " (older ones overwritten)" } else { " (all since boot)" };
    let _ = writeln!(ctx.out, "dmesg: the newest {lines} log lines{older}");
    write_in_chunks(ctx.out, &text);
}

/// `date`: the RTC's date and time, a header and no rows. The clock is
/// taken as UTC (true under QEMU's `-rtc base=utc`, the tests' setting;
/// the RTC itself doesn't say).
pub fn date(ctx: &mut Ctx<'_>, _: &[String]) {
    match crate::rtc::now() {
        Ok(now) => {
            let _ = writeln!(ctx.out, "date: {now} UTC");
        }
        Err(e) => ctx.fail(format_args!("date: {e}")),
    }
}

/// What `top` compares from one frame to the next.
struct Sample {
    ns: u64,
    cpus: [CpuTicks; MAX_CPUS],
    vectors: [u64; 256],
    threads: Vec<ThreadInfo>,
}

impl Sample {
    fn take() -> Sample {
        Sample {
            ns: crate::time::now_ns(),
            cpus: core::array::from_fn(sched::cpu_ticks),
            vectors: core::array::from_fn(|v| idt::vector_count(v as u8)),
            threads: sched::threads(),
        }
    }
}

/// How often `top` checks for a key while it waits.
const KEY_POLL_NS: u64 = 20_000_000;

/// `top [frames]`: a frame a second (per-CPU busy %, interrupt rates by
/// vector from 32 on, the heap, the threads and their share of their CPU),
/// until `q` or Ctrl-C (or `frames` frames). With escape codes each frame
/// replaces the last; on a plain console frames follow each other as lines.
pub fn top(ctx: &mut Ctx<'_>, args: &[String]) {
    let usage = |ctx: &mut Ctx<'_>| ctx.fail("usage: top [frames] (q or Ctrl-C quits)");
    let limit: Option<u64> = match args {
        // Its frames would go down the pipeline unseen until a key.
        [] if ctx.piped() => {
            return ctx.fail("top: in a pipeline it needs a number of frames (top 1 | ...)");
        }
        [] => None,
        [n] => match n.parse() {
            Ok(n) if n > 0 => Some(n),
            _ => return usage(ctx),
        },
        _ => return usage(ctx),
    };
    let mut prev = Sample::take();
    let mut frame = 0;
    loop {
        let until = prev.ns + NS_PER_SEC;
        while crate::time::now_ns() < until {
            if let Some(Key::Char('q') | Key::Ctrl('c')) = ctx.poll_key() {
                let _ = writeln!(ctx.out, "top: quit after {frame} frames");
                return;
            }
            crate::time::sleep(KEY_POLL_NS);
        }
        let now = Sample::take();
        frame += 1;
        let text = render(frame, &prev, &now, ctx.ansi);
        emit(ctx, &text);
        prev = now;
        if limit.is_some_and(|l| frame >= l) {
            let _ = writeln!(ctx.out, "top: done after {frame} frames");
            return;
        }
    }
}

/// One frame of `top`: what changed from `prev` to `now`.
fn render(frame: u64, prev: &Sample, now: &Sample, ansi: bool) -> String {
    let span = now.ns.saturating_sub(prev.ns);
    let mut t = String::new();
    if ansi {
        // Home and clear: each frame replaces the last.
        t.push_str("\x1b[H\x1b[2J");
    }
    let up = match crate::time::since_boot_ns() {
        Some(up) => alloc::format!("{}.{} s", up / NS_PER_SEC, up % NS_PER_SEC / (NS_PER_SEC / 10)),
        None => String::from("?"),
    };
    let online: Vec<(usize, u32)> = online().collect();
    let _ = writeln!(t, "top: frame {frame}, up {up}, {} CPUs{}; q quits", online.len(), not_online());
    // CPUs: busy over the span, and ticks taken in it.
    let _ = writeln!(t, "  cpu  busy  ticks/s");
    for &(cpu, _) in &online {
        let d = now.cpus[cpu].since(prev.cpus[cpu]);
        let busy = busy(cpu, d).show(Kind::Percent);
        let _ = writeln!(t, "  {cpu:>3}  {busy:>4}  {:>7}", per_second(d.ticks, span));
    }
    // Interrupts per second, the vectors that took any.
    let _ = write!(t, "  irqs/s:");
    let mut any = false;
    for v in 32..256 {
        let d = now.vectors[v] - prev.vectors[v];
        if d > 0 {
            let sep = if any { "," } else { "" };
            let _ = write!(t, "{sep} {} {}", idt::vector_use(v as u8), per_second(d, span));
            any = true;
        }
    }
    if !any {
        t.push_str(" none");
    }
    t.push('\n');
    let heap = crate::mm::heap::stats();
    let frames = crate::mm::frame::counts();
    let _ = writeln!(
        t,
        "  heap {} used of {} in {} allocations; frames {} used of {}",
        Bytes(heap.used as u64),
        Bytes(heap.size as u64),
        heap.live,
        Bytes((frames.owned - frames.free) as u64 * FRAME_SIZE),
        Bytes(frames.owned as u64 * FRAME_SIZE)
    );
    // Threads: each one's share of its CPU's ticks in the span.
    let _ = writeln!(t, "  thread            id  cpu  state     cpu%");
    for th in &now.threads {
        let before = prev.threads.iter().find(|p| p.id == th.id).map_or(0, |p| p.ticks);
        let cpu_ticks = now.cpus.get(th.cpu).zip(prev.cpus.get(th.cpu)).map_or(0, |(n, p)| n.since(*p).ticks);
        let share = percent(th.ticks - before, cpu_ticks);
        let _ = writeln!(t, "  {:<16}  {:>2}  {:>3}  {:<8}  {share:>3}%", th.name, th.id, th.cpu, th.state.name());
    }
    t
}

crate::ktest!(
    fn top_shows_a_frame_and_quits_on_q() {
        // `top 1` prints one frame (a CPU row for the boot CPU, the main
        // thread) and stops; `q` or Ctrl-C waiting stops it before any
        // frame.
        use crate::kassert;
        let history = lumen_shell::History::new(0);
        let mut console = super::Captured::default();
        top(&mut Ctx::new(&mut console, false, &history), &[String::from("1")]);
        kassert!(!console.ran_dry);
        let text = console.text;
        crate::ktest::diag(format_args!("{}", text.lines().next().unwrap_or("")));
        kassert!(text.starts_with("top: frame 1, up "));
        kassert!(text.lines().any(|l| l.starts_with("    0  ")));
        kassert!(text.lines().any(|l| l.starts_with("  main ")));
        kassert!(text.ends_with("top: done after 1 frames\n"));
        kassert!(!text.contains('\x1b'));
        for key in [Key::Char('q'), Key::Ctrl('c')] {
            let mut console = super::Captured::default();
            console.keys.push_back(key);
            top(&mut Ctx::new(&mut console, false, &history), &[]);
            kassert!(!console.ran_dry);
            kassert!(console.text == "top: quit after 0 frames\n");
        }
        Ok(())
    }
);

crate::ktest!(
    fn system_commands_print_a_header_and_rows() {
        // Each prints `<name>: ...` and at least one row after it; `date`
        // only its header, a date and time.
        use crate::kassert;
        let history = lumen_shell::History::new(0);
        let commands: [(&str, super::commands::Run); 7] = [
            ("cpus", cpus),
            ("mem", mem),
            ("irqs", irqs),
            ("threads", threads),
            ("lspci", lspci),
            ("dmesg", dmesg),
            ("date", date),
        ];
        for (name, run) in commands {
            let mut console = super::Captured::default();
            run(&mut Ctx::new(&mut console, false, &history), &[]);
            let mut lines = console.text.lines();
            let header = lines.next().unwrap_or("");
            let rows = lines.count();
            let has_header = header.starts_with(&alloc::format!("{name}: "));
            let has_rows = if name == "date" { is_date_line(header) } else { rows > 0 };
            if !(has_header && has_rows) {
                crate::ktest::diag(format_args!("{name}: header {header:?}, {rows} rows"));
            }
            kassert!(has_header);
            kassert!(has_rows);
        }
        Ok(())
    }
);

/// Whether `line` is `date: YYYY-MM-DD HH:MM:SS UTC` (for the tests).
#[cfg(feature = "ktest")]
fn is_date_line(line: &str) -> bool {
    const PATTERN: &[u8] = b"date: ####-##-## ##:##:## UTC";
    line.len() == PATTERN.len()
        && line.bytes().zip(PATTERN).all(|(b, &p)| if p == b'#' { b.is_ascii_digit() } else { b == p })
}

crate::ktest!(
    fn every_record_table_fits_its_columns() {
        // `Table::new` and `Table::push` check each table against its
        // columns (a mistake is a panic, here rather than at a user's
        // prompt): the records each system command gives, and `irqs`'
        // table of inputs.
        use crate::kassert;
        for (name, table) in RECORDS {
            let rows = table().rows().len();
            crate::ktest::diag(format_args!("{name}: {rows} rows"));
            kassert!(rows > 0);
        }
        kassert!(records("dmesg").is_none());
        let inputs = crate::interrupts::inputs();
        kassert!(input_table(&inputs).rows().len() == inputs.len());
        Ok(())
    }
);

crate::ktest!(
    fn mem_records_add_up() {
        // Frames: their size is their count's 4 KiB pages, used no more
        // than that; the heap uses no more than it has; a stack's size is
        // all used.
        use crate::kassert;
        let table = mem_table();
        let number = |v: &Value| match v {
            Value::Int(n) => Ok(*n),
            _ => Err("not a number"),
        };
        for row in table.rows() {
            let Value::Text(area) = &row[0] else { return Err("an area without a name") };
            let (size, used, count) = (number(&row[1])?, number(&row[2])?, number(&row[3])?);
            kassert!(0 <= used && used <= size && count > 0);
            if area == "frames" {
                kassert!(size == count * FRAME_SIZE as i64);
            } else if area.ends_with(" stacks") {
                kassert!(size == used);
            }
        }
        kassert!(table.rows().iter().filter(|r| matches!(&r[0], Value::Text(a) if a.ends_with(" stacks"))).count() > 0);
        Ok(())
    }
);
