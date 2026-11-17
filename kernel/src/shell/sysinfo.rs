//! The shell's system commands: `cpus`, `mem`, `irqs`, `threads`, `lspci`,
//! `mouse`, `dmesg`, `date` and `top`. Each prints a header line (`<name>:
//! ...`), and all but `date` rows after it. In a pipeline `cpus`, `irqs`,
//! `mem`, `threads`, `lspci` and `mouse` give records instead
//! ([`RECORDS`]): `cpus`, `threads`, `lspci` and `mouse` the table they
//! print (under their header), `irqs` its per-vector table only (not the
//! I/O APIC's inputs), `mem` a table of what its prose says (in bytes).
//! `mouse events [n]` and `kbd events [n]` give the desktop's input rings'
//! newest events (#151), printed as a table or, piped, as records.
//! Output with rows is made into a `String` first and written in one write
//! (no log line lands inside it, and nothing allocating is formatted under
//! the screen's lock); `dmesg` writes its lines in chunks
//! (`shell::write_in_chunks`) instead, so a slow UART doesn't hold the
//! log's line lock (interrupts off) for long.

use super::{Ctx, write_in_chunks};
use crate::arch::{self, MAX_CPUS, idt};
use crate::sched::{self, CpuTicks, ThreadInfo};
use alloc::string::{String, ToString};
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
pub fn int(n: impl TryInto<i64>) -> Value {
    n.try_into().map_or(DASH, Value::Int)
}

pub fn text(s: &str) -> Value {
    Value::Text(String::from(s))
}

/// `s` as a text cell, or the absent value if it is `-` (a text `-` would
/// print quoted, since records quote a cell that reads as absent).
fn text_or_dash(s: String) -> Value {
    if s == "-" { DASH } else { Value::Text(s) }
}

/// No value, printed `-`.
const DASH: Value = Value::Absent("-");

/// What makes a system command's records from its arguments: `None` if
/// the command doesn't give records for these arguments (it runs as a
/// command: its usage line, or what they ask for, e.g. `mouse hold`), else
/// the table, or why there is none (said on the console, ending the
/// pipeline).
pub type MakeTable = fn(&[String]) -> Option<Result<pipe::Table, String>>;

/// The system commands that give records in a pipeline (instead of their
/// text), and the tables they give.
pub const RECORDS: [(&str, MakeTable); 11] = [
    ("cpus", |a| no_args(a, cpu_table)),
    ("desktop", crate::desktop::records),
    ("irqs", |a| no_args(a, vector_table)),
    ("kbd", kbd_records),
    ("lspci", |a| no_args(a, pci_table)),
    ("mem", |a| no_args(a, mem_table)),
    ("mouse", mouse_records),
    ("sound", |a| no_args(a, crate::sound::table)),
    ("threads", |a| no_args(a, thread_table)),
    ("ui", crate::ui::records),
    ("windows", crate::desktop::windows_records),
];

/// A command's records when it takes no arguments: `table`'s, given none.
fn no_args(args: &[String], table: fn() -> pipe::Table) -> Option<Result<pipe::Table, String>> {
    args.is_empty().then(|| Ok(table()))
}

/// `mouse`'s records: its row, or (`events [n]`) the pointer ring's newest
/// events; `hold`, `release` and anything else run as the command.
fn mouse_records(args: &[String]) -> Option<Result<pipe::Table, String>> {
    match args {
        [] => Some(Ok(mouse_table())),
        [what, rest @ ..] if what == "events" => Some(mouse_events(rest)),
        _ => None,
    }
}

/// `kbd`'s records: (`events [n]`) the key ring's newest events; `kbd`
/// itself is a line of text.
fn kbd_records(args: &[String]) -> Option<Result<pipe::Table, String>> {
    match args {
        [what, rest @ ..] if what == "events" => Some(kbd_events(rest)),
        _ => None,
    }
}

/// `mouse events [n]`: the pointer ring's newest frames, or (no tablet) the
/// line plain `mouse` gives.
pub fn mouse_events(args: &[String]) -> Result<pipe::Table, String> {
    if crate::mouse::status().is_none() {
        return Err(String::from(NO_TABLET));
    }
    Ok(pointer_events_table(&crate::input::POINTER.recent(events_count("mouse", args)?)))
}

/// `kbd events [n]`: the key ring's newest events, or (no keyboard) the
/// line plain `kbd` gives.
pub fn kbd_events(args: &[String]) -> Result<pipe::Table, String> {
    kbd_route(&crate::keyboard::stats())?;
    Ok(key_events_table(&crate::input::KEYS.recent(events_count("kbd", args)?)))
}

/// How many events `<name> events [n]` asks for: `n` (1 or more), 16
/// without it.
fn events_count(name: &str, args: &[String]) -> Result<usize, String> {
    match args {
        [] => Ok(EVENTS_SHOWN),
        [n] => n.parse().ok().filter(|&n| n > 0).ok_or_else(|| alloc::format!("usage: {name} events [n]")),
        _ => Err(alloc::format!("usage: {name} events [n]")),
    }
}

/// How many events `mouse events` and `kbd events` show by default.
const EVENTS_SHOWN: usize = 16;

/// What `mouse` says without a tablet.
const NO_TABLET: &str = "mouse: no tablet (QEMU: -device virtio-tablet-pci)";

/// The keyboard's route from a `kbd` snapshot, or why there is no
/// keyboard, as `kbd` says it.
pub fn kbd_route(k: &crate::keyboard::Stats) -> Result<crate::interrupts::Route, String> {
    match (k.result, k.route) {
        (Some(Ok(())), Some(route)) => Ok(route),
        (Some(Err(e)), Some(route)) => Err(alloc::format!(
            "kbd: no keyboard ({e}); IRQ {} routed (vector {:#x}), masked",
            route.irq(),
            route.vector()
        )),
        (Some(Err(e)), None) => Err(alloc::format!("kbd: no keyboard ({e})")),
        _ => Err(String::from("kbd: not set up")),
    }
}

/// A line for `kbd events` when no screen shell runs: they run the
/// keyboard's decoding pass, so nothing fills the key ring.
fn kbd_events_note() -> Option<String> {
    (crate::vt::readers() == 0).then(|| {
        let why = super::screen_line_width().err().unwrap_or("none started");
        alloc::format!("kbd: key events are decoded by the screen shells; none run ({why})")
    })
}

/// The pointer ring's `events` (`crate::input::POINTER`), oldest first: each
/// frame's number, position, buttons held and whether frames may have
/// been lost at the device before it.
fn pointer_events_table(events: &[(lumen_input::Seq, lumen_input::PointerEvent)]) -> pipe::Table {
    const COLUMNS: [Column; 5] = [
        Column::new("seq", Kind::Int, Align::Right, 3),
        Column::new("x", Kind::Int, Align::Right, 4),
        Column::new("y", Kind::Int, Align::Right, 4),
        Column::new("buttons", Kind::Text, Align::Left, 7),
        Column::new("after_overrun", Kind::Text, Align::Left, 0),
    ];
    let mut t = pipe::Table::new(&COLUMNS);
    for (seq, e) in events {
        t.push(vec![
            int(seq.get()),
            int(e.state.x),
            int(e.state.y),
            Value::Text(e.state.buttons.to_string()),
            text(if e.after_overrun { "yes" } else { "no" }),
        ]);
    }
    t
}

/// The key ring's `events` (`crate::input::KEYS`), oldest first: each
/// event's number, key, press or release, the press's text (quoted,
/// escaped; `-` for none), the modifiers after it and whether bytes were
/// dropped before it.
fn key_events_table(events: &[(lumen_input::Seq, lumen_input::KeyEvent)]) -> pipe::Table {
    const COLUMNS: [Column; 6] = [
        Column::new("seq", Kind::Int, Align::Right, 3),
        Column::new("code", Kind::Text, Align::Left, 10),
        Column::new("action", Kind::Text, Align::Left, 7),
        Column::new("text", Kind::Text, Align::Left, 6),
        Column::new("mods", Kind::Text, Align::Left, 9),
        Column::new("after_drop", Kind::Text, Align::Left, 0),
    ];
    let mut t = pipe::Table::new(&COLUMNS);
    for (seq, e) in events {
        let (action, typed) = match e.action {
            lumen_input::KeyAction::Press { text: Some(c) } => ("press", Value::Text(alloc::format!("{c:?}"))),
            lumen_input::KeyAction::Press { text: None } => ("press", DASH),
            lumen_input::KeyAction::Release => ("release", DASH),
        };
        t.push(vec![
            int(seq.get()),
            Value::Text(alloc::format!("{:?}", e.code)),
            text(action),
            typed,
            text_or_dash(mods_text(e.mods)),
            text(if e.after_drop { "yes" } else { "no" }),
        ]);
    }
    t
}

/// The modifiers as names joined by `+` (`shift+ctrl`), `-` for none.
fn mods_text(m: lumen_input::Mods) -> String {
    let names = [
        (m.is_shifted(), "shift"),
        (m.is_ctrl(), "ctrl"),
        (m.is_alt(), "alt"),
        (m.is_altgr(), "altgr"),
        (m.capslock(), "caps"),
        (m.numlock(), "num"),
    ];
    let on: Vec<&str> = names.iter().filter(|(on, _)| *on).map(|(_, n)| *n).collect();
    if on.is_empty() { String::from("-") } else { on.join("+") }
}

/// Write a table (or why there is none) to the console, as text.
pub fn print_table(ctx: &mut Ctx<'_>, table: Result<pipe::Table, String>) {
    match table {
        Ok(t) => {
            let mut out = String::new();
            t.render(&mut out);
            emit(ctx, &out);
        }
        Err(e) => ctx.fail(e),
    }
}

/// A line for the console when `name`'s records come with a caveat the
/// table can't carry: `lspci`'s when PCI devices are missing, `sound`'s
/// when there is no sound device, `kbd`'s (its events) when no screen
/// shell runs to decode keys.
pub fn records_note(name: &str) -> Option<String> {
    match name {
        "lspci" => {
            let status = crate::pci::status();
            (!status.is_clean()).then(|| alloc::format!("lspci: {}", status.note()))
        }
        "sound" => crate::sound::records_note(),
        "kbd" => kbd_events_note(),
        _ => None,
    }
}

/// What makes the records `name` gives in a pipeline, if it is one of
/// [`RECORDS`].
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
/// starting. (An idle boot CPU reads nearly 100% busy only where the
/// serial shell polls: without COM1's interrupt, see `serial_in`.)
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
/// taken on each vector from 32 on that has taken any, has a routed input
/// or is a device's MSI-X vector (its use `msix <pci address> #<table
/// entry>`).
fn vector_table() -> pipe::Table {
    const COLUMNS: [Column; 3] = [
        Column::new("vector", Kind::Hex, Align::Left, 6),
        Column::new("use", Kind::Text, Align::Left, 16),
        Column::new("count", Kind::Int, Align::Left, 0),
    ];
    let inputs = crate::interrupts::inputs();
    let msix: Vec<crate::pci::MsixRecord> = crate::pci::msix_vectors();
    let mut t = pipe::Table::new(&COLUMNS);
    for v in 32..=255u8 {
        let count = idt::vector_count(v);
        let routed = inputs.iter().any(|i| i.routed && i.entry.vector() == v);
        let device = msix.iter().find(|m| m.idt_vector == v);
        if count > 0 || routed || device.is_some() {
            let what = match device {
                Some(m) => alloc::format!("msix {} #{}", m.pci, m.table_entry),
                None => alloc::format!("{}", idt::vector_use(v)),
            };
            t.push(vec![int(v), Value::Text(what), int(count)]);
        }
    }
    t
}

/// `lspci`: every PCI function found at boot, and why there are none (or
/// some may be missing).
pub fn lspci(ctx: &mut Ctx<'_>, args: &[String]) {
    if extra_args(ctx, "lspci", args) {
        return;
    }
    let table = pci_table();
    let mut t = String::new();
    let n = table.rows().len();
    let _ = writeln!(t, "lspci: {n} function{}{}", if n == 1 { "" } else { "s" }, crate::pci::status());
    table.render(&mut t);
    emit(ctx, &t);
}

/// `lspci`'s driver column for `addr`: its binding (`no driver`,
/// `<driver>: not probed: <reason>`, `<driver>: probing` while its probe
/// runs, `<driver>: bound` or `<driver>: failed: <reason>`, the probe having
/// failed or the driver having given the device up after binding it).
fn driver_cell(addr: crate::pci::PciAddr) -> Value {
    crate::pci::binding(addr).map_or(DASH, |b| Value::Text(b.to_string()))
}

/// Ktests: `addr`'s driver column, as `lspci` shows it.
#[cfg(feature = "ktest")]
pub fn driver_text(addr: crate::pci::PciAddr) -> String {
    match driver_cell(addr) {
        Value::Text(t) => t,
        _ => String::from("-"),
    }
}

/// `lspci`'s rows: the address, vendor and device ids (four hex digits,
/// so `where vendor = 0x1af4` works), the class's name and the device's
/// (`-` if Lumen doesn't know it), and its driver ([`driver_cell`]).
fn pci_table() -> pipe::Table {
    const COLUMNS: [Column; 6] = [
        Column::new("addr", Kind::Text, Align::Left, 12),
        Column::new("vendor", Kind::Hex16, Align::Left, 6),
        Column::new("device", Kind::Hex16, Align::Left, 6),
        Column::new("class", Kind::Text, Align::Left, 0),
        Column::new("name", Kind::Text, Align::Left, 0),
        Column::new("driver", Kind::Text, Align::Left, 0),
    ];
    let mut t = pipe::Table::new(&COLUMNS);
    for d in crate::pci::devices() {
        let addr = Value::Text(alloc::format!("{}", d.addr));
        t.push(vec![
            addr,
            int(d.vendor),
            int(d.device),
            text(d.class_name()),
            d.name().map_or(DASH, text),
            driver_cell(d.addr),
        ]);
    }
    t
}

/// `mouse`: the tablet (#98), where it points and the buttons pressed: a header
/// naming the device (or saying there is none) and its row. `mouse hold`
/// stops its thread reading it until `mouse release` (`mouse::hold`: the
/// hold is every console's; releasing a tablet that isn't held just says
/// `released`).
pub fn mouse(ctx: &mut Ctx<'_>, args: &[String]) {
    match args {
        [] => {}
        [a] if a == "hold" => {
            return match crate::mouse::hold() {
                Ok(()) => emit(ctx, "mouse: held: reading nothing until `mouse release`\n"),
                Err(e) => ctx.fail(format_args!("mouse: can't hold: {e}")),
            };
        }
        [a] if a == "release" => {
            crate::mouse::release();
            return emit(ctx, "mouse: released\n");
        }
        [a, rest @ ..] if a == "events" => return print_table(ctx, mouse_events(rest)),
        _ => return ctx.fail("usage: mouse [hold|release|events [n]]"),
    }
    // One snapshot for the header and the row.
    let bound = crate::mouse::status();
    let mut t = mouse_header(bound.as_ref(), crate::mouse::reads());
    t.push('\n');
    mouse_table_of(bound.as_ref()).render(&mut t);
    emit(ctx, &t);
}

/// `mouse`'s header line: the device, the screen and the counts (whole
/// frames are the state's `seq`; overruns the batches that may have lost
/// frames), whether its reads are held (`mouse hold`: `, held`, or
/// `, holding` until the thread parks) and why it failed; or
/// that there is no tablet.
fn mouse_header(bound: Option<&crate::mouse::Bound>, reads: crate::mouse::Reads) -> String {
    let Some(b) = bound else { return String::from(NO_TABLET) };
    let (d, c) = (&b.device, &b.counts);
    let frames = b.state.map_or(0, |s| s.seq);
    let held = match reads {
        crate::mouse::Reads::Running => "",
        crate::mouse::Reads::HoldAsked => ", holding",
        crate::mouse::Reads::Held => ", held",
    };
    let failed = b.failed.map_or(String::new(), |why| alloc::format!(", failed: {why}"));
    let newest = crate::input::POINTER.newest().get();
    alloc::format!(
        "mouse: \"{}\" on {}, screen {}x{}, {} events in {frames} frames ({} ignored, {} dropped, {} \
         overruns, newest event {newest}){held}{failed}",
        d.name,
        d.addr,
        d.screen.0,
        d.screen.1,
        c.events,
        c.ignored,
        c.dropped,
        c.overruns
    )
}

fn mouse_table() -> pipe::Table {
    mouse_table_of(crate::mouse::status().as_ref())
}

/// `mouse`'s row: always one (`none` for its device without a tablet;
/// `-` before the first report): the pointer's position, the buttons held
/// (`left right`, or `none`), every button's presses so far, summed (a
/// click between two looks shows here), the wheels' clicks so far, the
/// pointer (`drawn`, `hidden` while a command draws on the screen, `none`
/// without a terminal, `-` once the tablet failed), whether frames may
/// have been missed since the newest one (`stale`: `yes` after an overrun)
/// and why the tablet failed (`-` while it works).
fn mouse_table_of(bound: Option<&crate::mouse::Bound>) -> pipe::Table {
    const COLUMNS: [Column; 10] = [
        Column::new("device", Kind::Text, Align::Left, 12),
        Column::new("x", Kind::Int, Align::Right, 4),
        Column::new("y", Kind::Int, Align::Right, 4),
        Column::new("buttons", Kind::Text, Align::Left, 7),
        Column::new("presses", Kind::Int, Align::Right, 7),
        Column::new("wheel", Kind::Int, Align::Right, 5),
        Column::new("hwheel", Kind::Int, Align::Right, 6),
        Column::new("pointer", Kind::Text, Align::Left, 7),
        Column::new("stale", Kind::Text, Align::Left, 5),
        Column::new("failed", Kind::Text, Align::Left, 0),
    ];
    let mut t = pipe::Table::new(&COLUMNS);
    let device = bound.map_or(text("none"), |b| Value::Text(b.device.addr.to_string()));
    let failed = bound.and_then(|b| b.failed);
    let failed_cell = failed.map_or(DASH, |why| Value::Text(why.to_string()));
    let row = match bound.and_then(|b| b.state.map(|s| (s, b.stale))) {
        Some((s, stale)) => {
            let pointer = if failed.is_some() {
                "-"
            } else if crate::term::screen_size().is_none() {
                "none"
            } else if crate::term::pointer_drawn().is_some() {
                "drawn"
            } else {
                "hidden"
            };
            vec![
                device,
                int(s.x),
                int(s.y),
                Value::Text(s.buttons.to_string()),
                int(s.total_presses()),
                int(s.wheel),
                int(s.hwheel),
                text_or_dash(String::from(pointer)),
                text(if stale { "yes" } else { "no" }),
                failed_cell,
            ]
        }
        None => vec![device, DASH, DASH, DASH, DASH, DASH, DASH, DASH, DASH, failed_cell],
    };
    t.push(row);
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
        let events = [String::from("events")];
        let status = [String::from("status")];
        let tree = [String::from("tree")];
        for (name, make) in RECORDS {
            // Each with no arguments, `events` for the two that take it
            // (`kbd` gives records only for that), `status` for `desktop`
            // and `tree` for `ui`. Without a device the events are its line
            // instead (the ktest machine has no tablet), and without a
            // desktop (the test kernel starts none) `windows` and `ui tree`
            // say so.
            for args in [&[][..], &events[..], &status[..], &tree[..]] {
                let Some(table) = make(args) else { continue };
                let table = match table {
                    Ok(table) => table,
                    Err(e) if name == "mouse" && e == NO_TABLET => continue,
                    Err(e) if name == "windows" && e == "windows: no desktop" => continue,
                    Err(e) if name == "ui" && e == "ui tree: no desktop" => continue,
                    Err(_) => return Err("a records error"),
                };
                let rows = table.rows().len();
                crate::ktest::diag(format_args!("{name} {args:?}: {rows} rows"));
                // (`sound` has a row only with a device, and the ktest
                // machine has none: `sound::the_record_fits_its_columns`
                // checks one. The rings may be empty: nothing typed or
                // moved, though other ktests push keys.)
                kassert!(rows > 0 || name == "sound" || !args.is_empty());
            }
        }
        // Rows of both rings fit their columns (events of a ring of our own,
        // not the live ones).
        let mut pointers = lumen_input::Ring::<lumen_input::PointerEvent, 2>::new();
        let state = lumen_input::PointerState { x: 1, y: 2, buttons: lumen_input::Buttons::LEFT, ..Default::default() };
        pointers.push(lumen_input::PointerEvent { state, after_overrun: true });
        let rows: Vec<_> = pointers.recent(2).collect();
        let table = pointer_events_table(&rows);
        kassert!(table.rows().len() == 1 && table.rows()[0][4] == text("yes"));
        let mut keys = lumen_input::Ring::<lumen_input::KeyEvent, 2>::new();
        keys.push(lumen_input::KeyEvent {
            code: lumen_input::KeyCode::A,
            action: lumen_input::KeyAction::Press { text: Some('\n') },
            mods: lumen_input::Mods::NONE.with_shift(),
            after_drop: true,
        });
        let rows: Vec<_> = keys.recent(2).collect();
        let table = key_events_table(&rows);
        kassert!(table.rows().len() == 1);
        kassert!(table.rows()[0][3] == text("'\\n'") && table.rows()[0][4] == text("shift"));
        kassert!(table.rows()[0][5] == text("yes"));
        kassert!(records("dmesg").is_none());
        let inputs = crate::interrupts::inputs();
        kassert!(input_table(&inputs).rows().len() == inputs.len());
        Ok(())
    }
);

crate::ktest!(
    fn mouse_shows_a_failed_tablet_and_its_counts() {
        // A tablet that failed after reporting (built here: QEMU can't make
        // one fail): the header gives every count and why it failed; the
        // row keeps the last position, no pointer (`-`), stale, and why.
        use crate::kassert_eq;
        use crate::mouse::{Bound, Counts, Device, Failure};
        use lumen_input::{Buttons, PointerState};
        let addr = lumen_pci::PciAddr::new(0, 0, 3, 0).ok_or("addr")?;
        let state =
            PointerState { x: 5, y: 6, buttons: Buttons::NONE, presses: [2, 1, 0, 0, 0], seq: 7, ..Default::default() };
        let bound = Bound {
            device: Device { addr, name: String::from("ktest tablet"), screen: (1280, 800) },
            state: Some(state),
            counts: Counts { events: 9, ignored: 1, dropped: 2, overruns: 3 },
            stale: true,
            failed: Some(Failure::Failed),
        };
        // (The pointer ring's newest number: whatever earlier tests pushed.)
        let newest = crate::input::POINTER.newest().get();
        kassert_eq!(
            mouse_header(Some(&bound), crate::mouse::Reads::Running),
            alloc::format!(
                "mouse: \"ktest tablet\" on 0000:00:03.0, screen 1280x800, 9 events in 7 frames (1 ignored, 2 \
                 dropped, 3 overruns, newest event {newest}), failed: FAILED"
            )
        );
        let table = mouse_table_of(Some(&bound));
        let row = &table.rows()[0];
        kassert_eq!(row[0], Value::Text(String::from("0000:00:03.0")));
        kassert_eq!((row[1].clone(), row[2].clone(), row[4].clone()), (Value::Int(5), Value::Int(6), Value::Int(3)));
        kassert_eq!(row[7], DASH);
        kassert_eq!(row[8], text("yes"));
        kassert_eq!(row[9], text("FAILED"));
        // Without a tablet: one row of dashes.
        // The hold shows at the end, before why it failed: held, or holding
        // until the thread parks; nothing while it reads.
        use crate::mouse::Reads;
        let counts = alloc::format!("9 events in 7 frames (1 ignored, 2 dropped, 3 overruns, newest event {newest})");
        for (reads, suffix) in [(Reads::Running, ""), (Reads::HoldAsked, ", holding"), (Reads::Held, ", held")] {
            let header = mouse_header(Some(&bound), reads);
            kassert_eq!(
                header,
                alloc::format!(
                    "mouse: \"ktest tablet\" on 0000:00:03.0, screen 1280x800, {counts}{suffix}, failed: FAILED"
                )
            );
        }
        kassert_eq!(mouse_header(None, Reads::Held), "mouse: no tablet (QEMU: -device virtio-tablet-pci)");
        kassert_eq!(mouse_table_of(None).rows()[0][9], DASH);
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
