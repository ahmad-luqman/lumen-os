//! The shell's system commands: `cpus`, `mem`, `irqs`, `dmesg`, `date` and
//! `top`. Each prints a header line (`<name>: ...`) and then its rows.
//! Output is made into a `String` first and written in one write (nothing
//! is formatted under the screen's lock, and no log line lands inside it).

use super::Ctx;
use crate::arch::{self, MAX_CPUS, idt};
use crate::sched::{self, CpuTicks, ThreadInfo};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use lumen_shell::Key;
use lumen_time::{NS_PER_SEC, busy_percent, per_second, percent};

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

/// A physical frame's size.
const FRAME_BYTES: u64 = 4096;

/// How busy `cpu` was over `ticks` scheduler ticks, `idle` of them idle:
/// `37%`, or `-` if its scheduler isn't running.
fn busy_text(cpu: usize, ticks: u64, idle: u64) -> String {
    if sched::running_on(cpu) { alloc::format!("{}%", busy_percent(ticks, idle)) } else { String::from("-") }
}

/// The CPUs online (index, local APIC id).
fn online() -> impl Iterator<Item = (usize, u32)> {
    (0..MAX_CPUS).filter_map(|c| crate::smp::online_lapic_id(c).map(|id| (c, id)))
}

/// Write `text` to the console in one write.
fn emit(ctx: &mut Ctx<'_>, text: &str) {
    let _ = ctx.out.write_str(text);
}

/// `cpus`: each CPU online, its scheduler's ticks and how busy it has been
/// since it started, and the threads homed there.
pub fn cpus(ctx: &mut Ctx<'_>, _: &[String]) {
    let threads = sched::threads();
    let mut t = String::new();
    let n = online().count();
    let _ = writeln!(t, "cpus: {n} online");
    let _ = writeln!(t, "  cpu  apic  scheduler  ticks       busy  threads");
    for (cpu, apic) in online() {
        let homed = threads.iter().filter(|th| th.cpu == cpu).count();
        let c = sched::cpu_ticks(cpu);
        let running = if sched::running_on(cpu) { "running" } else { "stopped" };
        let busy = busy_text(cpu, c.ticks, c.idle);
        let _ = writeln!(t, "  {cpu:>3}  {apic:>4}  {running:<9}  {:<10}  {busy:>4}  {homed:>7}", c.ticks);
    }
    emit(ctx, &t);
}

/// `mem`: physical frames, the heap and kernel stacks.
pub fn mem(ctx: &mut Ctx<'_>, _: &[String]) {
    let frames = crate::mm::frame::counts();
    let heap = crate::mm::heap::stats();
    let mut kinds: Vec<(&'static str, usize, u64)> = Vec::new();
    for s in arch::stacks::REGISTRY.iter() {
        let name = s.kind.name();
        match kinds.iter_mut().find(|k| k.0 == name) {
            Some(k) => {
                k.1 += 1;
                k.2 += s.range.size();
            }
            None => kinds.push((name, 1, s.range.size())),
        }
    }
    let (owned, free) = (frames.owned as u64, frames.free as u64);
    let mut t = String::new();
    let _ = writeln!(t, "mem: frames, heap and kernel stacks");
    let _ = writeln!(
        t,
        "  frames  {} of {} used ({} frames of {owned} free, 4 KiB each)",
        Bytes((owned - free) * FRAME_BYTES),
        Bytes(owned * FRAME_BYTES),
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
    let stacks: u64 = kinds.iter().map(|k| k.2).sum();
    let count: usize = kinds.iter().map(|k| k.1).sum();
    let _ = write!(t, "  stacks  {count} ({}, each with a guard page):", Bytes(stacks));
    for (name, n, size) in &kinds {
        let _ = write!(t, " {n} {name} ({})", Bytes(*size));
    }
    t.push('\n');
    emit(ctx, &t);
}

/// `irqs`: every I/O APIC input decoded, then the interrupts taken on each
/// vector that has taken any (or has a routed input).
pub fn irqs(ctx: &mut Ctx<'_>, _: &[String]) {
    use arch::apic::{Polarity, Trigger};
    let inputs = crate::interrupts::inputs();
    let mut t = String::new();
    let _ = writeln!(t, "irqs: {} I/O APIC inputs, interrupts per vector", inputs.len());
    let _ = writeln!(t, "  gsi  vector  delivery        trigger  polarity  dest  state     count");
    for i in &inputs {
        let e = i.entry;
        if !i.routed && e.masked() {
            let _ = writeln!(t, "  {:>3}  -       -               -        -         -     masked    -", i.gsi);
            continue;
        }
        let delivery = e.delivery().map_or(String::from("reserved"), |d| alloc::format!("{d:?}"));
        let trigger = if e.trigger() == Trigger::Level { "level" } else { "edge" };
        let polarity = if e.polarity() == Polarity::ActiveLow { "low" } else { "high" };
        let dest =
            if e.logical() { alloc::format!("l{}", e.destination()) } else { alloc::format!("{}", e.destination()) };
        let state = if e.masked() { "masked" } else { "unmasked" };
        let _ = writeln!(
            t,
            "  {:>3}  {:#04x}    {delivery:<14}  {trigger:<7}  {polarity:<8}  {dest:<4}  {state:<8}  {}",
            i.gsi,
            e.vector(),
            idt::vector_count(e.vector())
        );
    }
    let _ = writeln!(t, "  vector  use               count");
    for v in 32..=255u8 {
        let count = idt::vector_count(v);
        let routed = inputs.iter().any(|i| i.routed && i.entry.vector() == v);
        if count > 0 || routed {
            let _ = writeln!(t, "  {v:#04x}    {:<16}  {count}", alloc::format!("{}", idt::vector_use(v)));
        }
    }
    emit(ctx, &t);
}

/// `dmesg`: the newest log lines kept (`klog::RING_BYTES` of them), as
/// serial printed them.
pub fn dmesg(ctx: &mut Ctx<'_>, _: &[String]) {
    let mut buf = vec![0; crate::klog::RING_BYTES];
    let n = crate::klog::copy_recent(&mut buf);
    let text = String::from_utf8_lossy(&buf[..n]);
    let lines = text.lines().count();
    let mut t = String::with_capacity(n + 80);
    let _ = writeln!(t, "dmesg: the newest {lines} log lines");
    t.push_str(&text);
    if !text.is_empty() && !text.ends_with('\n') {
        t.push('\n');
    }
    emit(ctx, &t);
}

/// `date`: the RTC's date and time (UTC).
pub fn date(ctx: &mut Ctx<'_>, _: &[String]) {
    let _ = match crate::rtc::now() {
        Ok(now) => writeln!(ctx.out, "date: {now} UTC"),
        Err(e) => writeln!(ctx.out, "date: {e}"),
    };
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

/// `top [frames]`: a frame a second (per-CPU busy %, interrupt rates, the
/// heap, the threads and their share of their CPU), until `q` or Ctrl-C (or
/// `frames` frames). With escape codes each frame replaces the last; on a
/// plain console frames follow each other as lines.
pub fn top(ctx: &mut Ctx<'_>, args: &[String]) {
    let limit = match args {
        [] => Some(None),
        [n] => n.parse::<u64>().ok().filter(|&n| n > 0).map(Some),
        _ => None,
    };
    let Some(limit) = limit else {
        let _ = writeln!(ctx.out, "usage: top [frames] (q or Ctrl-C quits)");
        return;
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
    let up = crate::time::since_boot_ns().unwrap_or(0);
    let online: Vec<(usize, u32)> = online().collect();
    let _ = writeln!(
        t,
        "top: frame {frame}, up {}.{} s, {} CPUs; q quits",
        up / NS_PER_SEC,
        up % NS_PER_SEC / (NS_PER_SEC / 10),
        online.len()
    );
    // CPUs: busy over the span, and ticks taken in it.
    let _ = writeln!(t, "  cpu  busy  ticks/s");
    for &(cpu, _) in &online {
        let (p, n) = (prev.cpus[cpu], now.cpus[cpu]);
        let (dt, di) = (n.ticks - p.ticks, n.idle - p.idle);
        let busy = busy_text(cpu, dt, di);
        let _ = writeln!(t, "  {cpu:>3}  {busy:>4}  {:>7}", per_second(dt, span));
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
        Bytes((frames.owned - frames.free) as u64 * FRAME_BYTES),
        Bytes(frames.owned as u64 * FRAME_BYTES)
    );
    // Threads: each one's share of its CPU's ticks in the span.
    let _ = writeln!(t, "  thread            id  cpu  state     cpu%");
    for th in &now.threads {
        let before = prev.threads.iter().find(|p| p.id == th.id).map_or(0, |p| p.ticks);
        let cpu_ticks = now.cpus.get(th.cpu).zip(prev.cpus.get(th.cpu)).map_or(0, |(n, p)| n.ticks - p.ticks);
        let share = percent(th.ticks - before, cpu_ticks);
        let _ = writeln!(t, "  {:<16}  {:>2}  {:>3}  {:<8}  {share:>3}%", th.name, th.id, th.cpu, th.state.name());
    }
    t
}

crate::ktest!(
    fn top_shows_a_frame_and_quits_on_q() {
        // `top 1` prints one frame (a CPU row for the boot CPU, the main
        // thread) and stops; `q` waiting stops it before any frame.
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
        let mut console = super::Captured::default();
        console.keys.push_back(Key::Char('q'));
        top(&mut Ctx::new(&mut console, false, &history), &[]);
        kassert!(!console.ran_dry);
        kassert!(console.text == "top: quit after 0 frames\n");
        Ok(())
    }
);

crate::ktest!(
    fn system_commands_print_a_header_and_rows() {
        // Each prints `<name>: ...` and at least one row after it.
        use crate::kassert;
        let history = lumen_shell::History::new(0);
        let commands: [(&str, super::commands::Run); 5] =
            [("cpus", cpus), ("mem", mem), ("irqs", irqs), ("dmesg", dmesg), ("date", date)];
        for (name, run) in commands {
            let mut console = super::Captured::default();
            run(&mut Ctx::new(&mut console, false, &history), &[]);
            let mut lines = console.text.lines();
            let header = lines.next().unwrap_or("");
            let rows = lines.count();
            let has_header = header.starts_with(&alloc::format!("{name}: "));
            let has_rows = rows > 0 || name == "date";
            if !(has_header && has_rows) {
                crate::ktest::diag(format_args!("{name}: header {header:?}, {rows} rows"));
            }
            kassert!(has_header);
            kassert!(has_rows);
        }
        Ok(())
    }
);
