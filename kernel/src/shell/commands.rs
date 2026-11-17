//! The shell's commands: kmon's, with their names and output (`echo`
//! joins its words with single spaces; `clear` and `colors` need a console
//! that takes escape codes), plus `history`, `term text`, `vt`, `reboot`,
//! `shutdown`, `random`, `screenshot` (`crate::screenshot`), the system commands (`cpus`, `mem`, `irqs`, `threads`,
//! `lspci`, `mouse`, `dmesg`, `date`, `top`: `sysinfo.rs`) and the filters that come after a
//! `|` (`grep`, `where`, `sort`, `first`, `count`, `select`: `pipe.rs`). A
//! table the line editor completes from. Each writes to the console that
//! ran it (or, at the start of a pipeline, to the next command; a usage or
//! error line goes to the console all the same: `Ctx::fail`).

use super::{Ctx, crash, pipe, sysinfo};
use crate::arch;
use alloc::string::String;
use core::arch::asm;
use core::fmt::Write;
use lumen_shell::{Command, Table, words};

pub type Run = fn(&mut Ctx<'_>, &[String]);

pub static COMMANDS: Table<Run> = Table(&[
    Command { name: "help", args: &[], usage: "", about: "this list", run: help },
    Command { name: "echo", args: &[], usage: "<text>", about: "print text", run: echo },
    Command {
        name: "rhai",
        args: &[&["-e", "demo"], crate::script::DEMO_NAMES],
        usage: "[-e <expr> | demo [name]]",
        about: "rhai scripts: a REPL, an expression, or a built-in demo (Ctrl-C stops one)",
        run: crate::script::command,
    },
    Command { name: "int3", args: &[], usage: "", about: "take a breakpoint and continue", run: int3 },
    Command {
        name: "crash",
        args: &[crash::KINDS, &["thread"]],
        usage: "<kind> [cpu|thread]",
        about: "deliberate fault (on that CPU, or in a new thread):",
        run: crash::run,
    },
    Command {
        name: "trace",
        args: &[],
        usage: "[n]",
        about: "the boot CPU's last n trace events (default 16)",
        run: trace,
    },
    Command { name: "heap", args: &[], usage: "", about: "heap usage and free-list summary", run: heap },
    Command {
        name: "ticks",
        args: &[],
        usage: "",
        about: "the boot CPU's timer interrupts (and stray ones)",
        run: ticks,
    },
    Command { name: "kbd", args: &[], usage: "", about: "the keyboard's layout and interrupts", run: kbd },
    Command {
        name: "term",
        args: &[&["text"]],
        usage: "[text]",
        about: "the shown terminal: size, cursor, scrollback (text: its rows)",
        run: term,
    },
    Command { name: "clear", args: &[], usage: "", about: "clear the screen and its scrollback", run: clear },
    Command {
        name: "vt",
        args: &[&["1", "2", "3", "4"]],
        usage: "[n]",
        about: "the virtual terminal shown (n: show that one, as Alt+Fn does)",
        run: vt,
    },
    Command { name: "colors", args: &[], usage: "", about: "a colour test pattern", run: colors },
    Command {
        name: "screenshot",
        args: &[],
        usage: "",
        about: "the screen as a PNG, in base64 lines framed for the harness (serial only)",
        run: crate::screenshot::command,
    },
    Command {
        name: "mandelbrot",
        args: &[],
        usage: "",
        about: "the Mandelbrot set, drawn by one CPU then by all of them; a key ends it",
        run: crate::demo::mandelbrot,
    },
    Command {
        name: "life",
        args: &[&lumen_demo::life::NAMES],
        usage: "[pattern] [generations]",
        about: "Conway's Game of Life (default gun; generations: stop after that many, a key ends it early):",
        run: crate::demo::life,
    },
    Command {
        name: "beep",
        args: &[],
        usage: "[hz] [ms]",
        about: "a square-wave beep (default 440 Hz for 200 ms), played while the shell goes on",
        run: crate::sound::beep_command,
    },
    Command {
        name: "play",
        args: &[],
        usage: "<melody>",
        about: "play notes, e.g. t=140 C4/8 E4/8 G4/4 r/8 (w=sine changes the waveform; q or Ctrl-C stops it)",
        run: crate::sound::play_command,
    },
    Command {
        name: "volume",
        args: &[],
        usage: "[percent]",
        about: "the volume of new sounds",
        run: crate::sound::volume_command,
    },
    Command {
        name: "sound",
        args: &[],
        usage: "",
        about: "the sound device, its format and what it played",
        run: crate::sound::sound_command,
    },
    Command { name: "seq", args: &[], usage: "<n>", about: "print 1 to n, a line each", run: seq },
    Command { name: "sleep", args: &[], usage: "<ms>", about: "wait that long (guest time)", run: sleep },
    Command {
        name: "random",
        args: &[],
        usage: "[bytes | --stats]",
        about: "random bytes in hex (default 16, up to 4096) from the pool RDRAND and/or virtio-rng feed; \
                --stats: its sources and counters",
        run: random,
    },
    Command { name: "uptime", args: &[], usage: "", about: "nanoseconds since the TSC started", run: uptime },
    Command {
        name: "clocks",
        args: &[],
        usage: "",
        about: "the TSC's frequency, measured again against each clock",
        run: clocks,
    },
    Command {
        name: "cpus",
        args: &[],
        usage: "",
        about: "each CPU: its scheduler, ticks, how busy, threads",
        run: sysinfo::cpus,
    },
    Command {
        name: "mem",
        args: &[],
        usage: "",
        about: "physical frames, the heap and kernel stacks (piped: in bytes)",
        run: sysinfo::mem,
    },
    Command {
        name: "irqs",
        args: &[],
        usage: "",
        about: "the I/O APIC's inputs decoded, and interrupts per vector (piped: the per-vector counts only)",
        run: sysinfo::irqs,
    },
    Command {
        name: "serial",
        args: &[],
        usage: "",
        about: "how COM1's input comes (by IRQ 4 or polled), and how the serial shell's waits for it ended",
        run: crate::serial_in::serial_command,
    },
    Command {
        name: "lspci",
        args: &[],
        usage: "",
        about: "the PCI functions found at boot: address, vendor and device ids, class, name, driver",
        run: sysinfo::lspci,
    },
    Command {
        name: "mouse",
        args: &[&["hold", "release"]],
        usage: "[hold|release]",
        about: "the tablet: where the pointer is, the buttons held, the wheels (hold: stop reading it until release)",
        run: sysinfo::mouse,
    },
    Command {
        name: "dmesg",
        args: &[],
        usage: "",
        about: "the newest log lines, as serial printed them",
        run: sysinfo::dmesg,
    },
    Command {
        name: "date",
        args: &[],
        usage: "",
        about: "the date and time from the RTC (taken as UTC)",
        run: sysinfo::date,
    },
    Command {
        name: "top",
        args: &[],
        usage: "[frames]",
        about: "CPUs, interrupts, heap and threads each second (q quits)",
        run: sysinfo::top,
    },
    Command { name: "history", args: &[], usage: "", about: "the lines entered on this console", run: history },
    Command {
        name: "threads",
        args: &[],
        usage: "",
        about: "each thread: its CPU, state and the ticks it ran",
        run: sysinfo::threads,
    },
    Command {
        name: "grep",
        args: &[],
        usage: "<text>",
        about: "after a |: the lines (or rows) containing text",
        run: |ctx, _| pipe::needs_input(ctx, "grep"),
    },
    Command {
        name: "where",
        args: &[&[], &lumen_shell::pipe::OPS],
        usage: "<column> <op> <value>",
        about: "after a |: the rows whose column compares with value by op (~: contains)",
        run: |ctx, _| pipe::needs_input(ctx, "where"),
    },
    Command {
        name: "sort",
        args: &[&[], &["desc"]],
        usage: "<column> [desc]",
        about: "after a |: the rows in that column's order",
        run: |ctx, _| pipe::needs_input(ctx, "sort"),
    },
    Command {
        name: "first",
        args: &[],
        usage: "<n>",
        about: "after a |: the first n lines (or rows)",
        run: |ctx, _| pipe::needs_input(ctx, "first"),
    },
    Command {
        name: "count",
        args: &[],
        usage: "",
        about: "after a |: how many lines (or rows)",
        run: |ctx, _| pipe::needs_input(ctx, "count"),
    },
    Command {
        name: "select",
        args: &[],
        usage: "<column>...",
        about: "after a |: only those columns, in that order",
        run: |ctx, _| pipe::needs_input(ctx, "select"),
    },
    Command { name: "reboot", args: &[], usage: "", about: "restart the machine", run: reboot },
    Command { name: "shutdown", args: &[], usage: "", about: "power off (ACPI S5)", run: shutdown },
    Command { name: "exit", args: &[], usage: "", about: "end QEMU (only with lumen.qemu_exit)", run: exit },
]);

/// Split `line` into words and run the command the first names (one line
/// of error if it can't be split or no command has that name), or the
/// pipeline it is (`cpus | count`: `pipe.rs`).
pub fn run(line: &str, ctx: &mut Ctx<'_>) {
    run_in(&COMMANDS, line, ctx);
}

/// [`run`] with the commands in `table`.
pub(super) fn run_in(table: &Table<Run>, line: &str, ctx: &mut Ctx<'_>) {
    let stages = match words::split_pipeline(line) {
        Ok(stages) => stages,
        Err(e) => {
            return ctx.fail(format_args!("syntax error: {e}"));
        }
    };
    match stages.as_slice() {
        [] => {}
        [words] => {
            let (name, args) = words.split_first().expect("split_pipeline gives no empty command");
            match table.find(name) {
                Some(command) => run_command(command, ctx, args),
                None => unknown(ctx, name),
            }
        }
        _ => pipe::run(table, &stages, ctx),
    }
}

/// Say no command is called `name`.
pub(super) fn unknown(ctx: &mut Ctx<'_>, name: &str) {
    ctx.fail(format_args!("unknown command: {name} (try 'help')"));
}

/// Run `command` with `args`. It gets a canvas owner of its own (on the
/// console's virtual terminal, if it is one); when it returns, a canvas it
/// left behind (kept in a static, leaked) is taken back, so the screen
/// doesn't stay frozen. A canvas held by a command on another console isn't
/// this one's, and stays.
pub(super) fn run_command(command: &Command<Run>, ctx: &mut Ctx<'_>, args: &[String]) {
    // (A command run from inside another, e.g. by a script, gets its own
    // owner too, and the outer one's is back afterwards.)
    let outer = ctx.owner;
    ctx.owner = outer.renew();
    (command.run)(ctx, args);
    if crate::term::reclaim_canvas(ctx.owner) {
        log::warn!(target: "shell", "{} left the screen paused; took it back", command.name);
    }
    ctx.owner = outer;
}

/// The only argument as a number, if there is exactly one and it is one.
fn number<T: core::str::FromStr>(args: &[String]) -> Option<T> {
    match args {
        [arg] => arg.parse().ok(),
        _ => None,
    }
}

/// Where `help`'s descriptions start.
const HELP_COLUMN: usize = 20;

/// Every command with its usage and what it does, then the keys. An
/// `about` ending in `:` is followed by the words its first argument can
/// be. Built in a `String` first (nothing may allocate while writing to the
/// screen: see `Ctx::out`), then written in one write, so another CPU's log
/// line can't land inside it.
fn help(ctx: &mut Ctx<'_>, _: &[String]) {
    let mut text = String::from("commands:\n");
    for c in COMMANDS.0 {
        let start = text.len();
        let _ = write!(text, "  {} {}", c.name, c.usage);
        let mut width = text.len() - start;
        if width >= HELP_COLUMN {
            text.push('\n');
            width = 0;
        }
        text.extend(core::iter::repeat_n(' ', HELP_COLUMN - width));
        text.push_str(c.about);
        if c.about.ends_with(':') {
            for word in c.args.first().copied().unwrap_or_default() {
                let _ = write!(text, " {word}");
            }
        }
        text.push('\n');
    }
    text.push_str("pipes: a command's output through filters, e.g. irqs | where count > 0 | sort count desc\n");
    text.push_str(
        "keys: Tab completes; Up/Down history; Ctrl-U/W delete, Ctrl-C drops the line; \
         the cursor keys, Home/End, Delete, Ctrl-A/E/B/F/D/K and Ctrl-L on the screen (or serial with lumen.ansi)\n",
    );
    let _ = ctx.out.write_str(&text);
}

fn echo(ctx: &mut Ctx<'_>, args: &[String]) {
    let _ = writeln!(ctx.out, "{}", args.join(" "));
}

fn int3(ctx: &mut Ctx<'_>, _: &[String]) {
    // SAFETY: #BP is handled and returns (see idt::lumen_exception_dispatch).
    unsafe { asm!("int3") };
    let n = arch::idt::BREAKPOINTS.load(core::sync::atomic::Ordering::Relaxed);
    let _ = writeln!(ctx.out, "breakpoints taken: {n}");
}

fn trace(ctx: &mut Ctx<'_>, args: &[String]) {
    let n = if args.is_empty() { Some(16) } else { number(args) };
    let Some(n) = n else {
        return ctx.fail("usage: trace [n]");
    };
    let out = &mut *ctx.out;
    let ring = crate::trace::ring();
    let cap = ring.capacity() as u64;
    if n > cap {
        let _ = writeln!(out, "(the ring holds {cap} events; showing those)");
    }
    let _ = crate::trace::write_recent(out, n, "");
    let _ = writeln!(out, "trace: {} events claimed, {} dropped", ring.written(), ring.dropped());
}

fn heap(ctx: &mut Ctx<'_>, _: &[String]) {
    let _ = crate::mm::heap::write_stats(ctx.out);
}

fn ticks(ctx: &mut Ctx<'_>, _: &[String]) {
    let s = arch::idt::strays();
    let _ = writeln!(
        ctx.out,
        "ticks: {} (spurious {}, unexpected {}, local APIC errors {})",
        crate::time::ticks(),
        s.spurious,
        s.unexpected,
        s.lapic_errors
    );
}

fn kbd(ctx: &mut Ctx<'_>, _: &[String]) {
    let k = crate::keyboard::stats();
    let out = &mut *ctx.out;
    let _ = match (k.result, k.route) {
        (Some(Ok(())), Some(route)) => writeln!(
            out,
            "kbd: layout {}, IRQ {} (vector {:#x}), {} interrupts, {} bytes, \
             {} dropped, {} undecoded, {} line errors, {} skipped at setup",
            k.layout,
            route.irq(),
            route.vector(),
            k.irqs,
            k.bytes,
            k.dropped,
            k.undecoded,
            k.line_errors,
            k.skipped
        ),
        (Some(Err(e)), Some(route)) => {
            writeln!(out, "kbd: no keyboard ({e}); IRQ {} routed (vector {:#x}), masked", route.irq(), route.vector())
        }
        (Some(Err(e)), None) => writeln!(out, "kbd: no keyboard ({e})"),
        _ => writeln!(out, "kbd: not set up"),
    };
}

/// `term`: the shown virtual terminal; `term text`: its rows, each after a
/// `|`, then a summary line.
fn term(ctx: &mut Ctx<'_>, args: &[String]) {
    const NONE: &str = "term: no terminal (no usable framebuffer, or one too small)";
    let out = &mut *ctx.out;
    match args {
        [] => {
            let _ = match crate::term::status() {
                Some(t) => writeln!(
                    out,
                    "term: {}x{}, cursor ({}, {}){}, scrollback {}/{} lines, view {} lines up, \
                     top row \"{}\", {} cells drawn{}",
                    t.cols,
                    t.rows,
                    t.cursor.col,
                    t.cursor.row,
                    if t.cursor.visible { "" } else { " hidden" },
                    t.scrollback,
                    t.capacity,
                    t.view,
                    t.top_row,
                    t.cells_drawn,
                    if t.paused { ", paused (a command is drawing)" } else { "" }
                ),
                None => return ctx.fail(NONE),
            };
        }
        [arg] if arg == "text" => match crate::term::view_text() {
            Some(view) => {
                for row in &view.rows {
                    let _ = writeln!(out, "|{row}");
                }
                let _ = match view.cursor {
                    Some(c) => writeln!(out, "term text: {} rows, cursor ({}, {})", view.rows.len(), c.col, c.row),
                    None => writeln!(out, "term text: {} rows, cursor below the view", view.rows.len()),
                };
            }
            None => ctx.fail(NONE),
        },
        _ => ctx.fail("usage: term [text]"),
    }
}

/// `vt`'s usage line.
const VT_USAGE: &str = "usage: vt [n] (n from 1 to 4)";

/// `vt`: which virtual terminal is shown, keys each dropped (its shell
/// behind) and discarded (no shell), and switches refused; `vt <n>`: show
/// terminal `n` (1 to `term::VTS`), unless a command has the screen.
fn vt(ctx: &mut Ctx<'_>, args: &[String]) {
    use crate::term::VTS;
    let wanted = match args {
        [] => None,
        [n] => match n.parse::<usize>() {
            Ok(n @ 1..=VTS) => Some(n),
            _ => return ctx.fail(VT_USAGE),
        },
        _ => return ctx.fail(VT_USAGE),
    };
    let Some(t) = crate::term::status() else {
        return ctx.fail("vt: no terminal");
    };
    match wanted {
        None => {
            let s = crate::vt::stats();
            let out = &mut *ctx.out;
            let _ = write!(out, "vt: terminal {} of {VTS} shown (Alt+F1 to F{VTS} switch); keys dropped", t.shown + 1);
            for d in s.dropped {
                let _ = write!(out, " {d}");
            }
            let _ = write!(out, ", discarded");
            for d in s.discarded {
                let _ = write!(out, " {d}");
            }
            let _ = writeln!(out, "; {} switches refused", s.refused);
        }
        Some(n) => match crate::vt::show(n - 1) {
            Ok(()) => {
                let _ = writeln!(ctx.out, "vt: terminal {n} shown");
            }
            Err(e) => ctx.fail(format_args!("vt: not showing terminal {n}: {e}")),
        },
    }
}

/// Whether the console takes escape codes; if not, `name` says why (in a
/// pipeline, or a plain console) and fails.
fn takes_escapes(ctx: &mut Ctx<'_>, name: &str) -> bool {
    if ctx.piped() {
        ctx.fail(format_args!("{name}: escape codes don't go into a pipeline"));
    } else if !ctx.ansi {
        ctx.fail(format_args!("{name}: this console is plain text (boot with lumen.ansi for escape codes)"));
    }
    ctx.ansi && !ctx.piped()
}

fn clear(ctx: &mut Ctx<'_>, _: &[String]) {
    // Home, erase the screen, erase the scrollback.
    if takes_escapes(ctx, "clear") {
        let _ = ctx.out.write_str("\x1b[H\x1b[2J\x1b[3J");
    }
}

/// A colour test pattern: the 16 named colours as backgrounds (`48;5;n`)
/// and as text (SGR 30-37 and 90-97), the 6x6x6 cube and the grey ramp
/// (256-colour SGR), bold, inverse and a truecolor gradient; then
/// `colors: done`.
fn colors(ctx: &mut Ctx<'_>, _: &[String]) {
    if !takes_escapes(ctx, "colors") {
        return;
    }
    let s = &mut *ctx.out;
    let _ = write!(s, "16 colours  ");
    for i in 0..16 {
        let _ = write!(s, "\x1b[48;5;{i}m   ");
        if i == 7 {
            let _ = write!(s, "\x1b[0m ");
        }
    }
    let _ = write!(s, "\x1b[0m\r\ntext        ");
    for i in 0..8 {
        let _ = write!(s, "\x1b[3{i}mAa\x1b[9{i}mAa ");
    }
    let _ = write!(s, "\x1b[0m\r\n");
    for g in 0..6 {
        let _ = write!(s, "{}", if g == 0 { "256 colours " } else { "            " });
        for r in 0..6 {
            for b in 0..6 {
                let _ = write!(s, "\x1b[48;5;{}m  ", 16 + 36 * r + 6 * g + b);
            }
            let _ = write!(s, "\x1b[0m ");
        }
        let _ = write!(s, "\r\n");
    }
    let _ = write!(s, "grey ramp   ");
    for i in 232..=255 {
        let _ = write!(s, "\x1b[48;5;{i}m  ");
    }
    let _ = write!(
        s,
        "\x1b[0m\r\nattributes  \x1b[1mbold\x1b[0m \x1b[7minverse\x1b[0m \x1b[1;7;94mboth\x1b[0m \x1b[1;91mbright\x1b[0m\r\ntruecolor   "
    );
    for i in 0..48 {
        let v = i * 255 / 47;
        let _ = write!(s, "\x1b[48;2;{v};{};{}m ", 255 - v, (v / 2) + 64);
    }
    let _ = write!(s, "\x1b[0m\r\n");
    let _ = writeln!(s, "colors: done");
}

fn seq(ctx: &mut Ctx<'_>, args: &[String]) {
    match number::<u32>(args) {
        Some(n) if n <= 10_000 => {
            for i in 1..=n {
                let _ = writeln!(ctx.out, "{i}");
            }
        }
        _ => ctx.fail("usage: seq <n> (up to 10000)"),
    }
}

/// `random`'s default and largest byte counts.
const RANDOM_BYTES: usize = 16;
const RANDOM_MAX: usize = 4096;

fn random(ctx: &mut Ctx<'_>, args: &[String]) {
    if matches!(args, [a] if a == "--stats") {
        let _ = writeln!(ctx.out, "rng: sources: {}; {}", crate::rng::sources(), crate::rng::stats());
        return;
    }
    let n = if args.is_empty() { Some(RANDOM_BYTES) } else { number::<usize>(args) };
    let Some(n) = n.filter(|n| (1..=RANDOM_MAX).contains(n)) else {
        return ctx.fail(format_args!("usage: random [bytes | --stats] (bytes: 1 to {RANDOM_MAX})"));
    };
    match random_hex(n) {
        Ok(hex) => {
            let _ = writeln!(ctx.out, "{hex}");
        }
        Err(e) => ctx.fail(format_args!("random: {e}")),
    }
}

/// `n` random bytes as lowercase hex.
fn random_hex(n: usize) -> Result<String, crate::rng::NoSource> {
    let mut bytes = alloc::vec![0u8; n];
    crate::rng::fill(&mut bytes)?;
    let mut s = String::with_capacity(2 * n);
    for b in &bytes {
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}

fn sleep(ctx: &mut Ctx<'_>, args: &[String]) {
    match number::<u64>(args) {
        Some(ms) if ms <= 60_000 => {
            crate::time::sleep(ms * 1_000_000);
            let _ = writeln!(ctx.out, "slept {ms} ms");
        }
        _ => ctx.fail("usage: sleep <ms> (up to 60000)"),
    }
}

fn uptime(ctx: &mut Ctx<'_>, _: &[String]) {
    let _ = writeln!(ctx.out, "uptime: {} ns", crate::time::now_ns());
}

/// The TSC's boot calibration, and its frequency measured again now against
/// the PIT and the HPET (the tests compare them), each with how far off its
/// median can be (`time::Calibration::median_bound_ppm`).
fn clocks(ctx: &mut Ctx<'_>, _: &[String]) {
    use crate::time::{self, Reference};
    let (Some(tsc), Some(reference), Some(boot)) = (time::tsc(), time::reference(), time::boot_calibration()) else {
        return ctx.fail("clocks: the TSC isn't calibrated");
    };
    let out = &mut *ctx.out;
    let boot_ppm = boot.median_bound_ppm();
    let _ = write!(out, "clocks: tsc {} Hz (against the {reference} at boot, within {boot_ppm} ppm)", tsc.hz());
    let measured = [Reference::Pit, Reference::Hpet].map(|r| (r, time::measure(r)));
    for (r, m) in &measured {
        let _ = match m {
            Some(Ok(cal)) => write!(out, "; {r} {} Hz within {} ppm", cal.hz, cal.median_bound_ppm()),
            Some(Err(e)) => write!(out, "; {r} failed ({e})"),
            None => write!(out, "; {r} none"),
        };
    }
    let _ = writeln!(out);
    // The windows behind those, for diagnosing: `clocks: windows: boot <cal>;
    // pit <cal>; hpet <cal>` (a failed or missing one left out), each `<cal>`
    // a time::Calibration (`<hz> Hz [<w> ...] retook <n>`).
    let _ = write!(out, "clocks: windows: boot {boot}");
    for (r, m) in &measured {
        if let Some(Ok(cal)) = m {
            let _ = write!(out, "; {r} {cal}");
        }
    }
    let _ = writeln!(out);
}

fn history(ctx: &mut Ctx<'_>, _: &[String]) {
    for (n, line) in ctx.history.numbered() {
        let _ = writeln!(ctx.out, "{n:5}  {line}");
    }
}

/// Refuse in a pipeline (`name`: the command), where the announcement it
/// makes before going would be lost; whether it did.
fn refuse_in_pipeline(ctx: &mut Ctx<'_>, name: &str) -> bool {
    if ctx.piped() {
        ctx.fail(format_args!("{name}: not in a pipeline"));
    }
    ctx.piped()
}

fn reboot(ctx: &mut Ctx<'_>, _: &[String]) {
    if refuse_in_pipeline(ctx, "reboot") {
        return;
    }
    let _ = writeln!(ctx.out, "reboot: restarting");
    crate::power::reboot()
}

/// Power off, or say why it can't be done or didn't happen.
fn shutdown(ctx: &mut Ctx<'_>, _: &[String]) {
    if refuse_in_pipeline(ctx, "shutdown") {
        return;
    }
    let why = match crate::power::power_off_plan() {
        Ok(plan) => {
            let _ = writeln!(ctx.out, "shutdown: powering off");
            crate::power::power_off(plan)
        }
        Err(why) => why,
    };
    ctx.fail(format_args!("shutdown: can't power off: {why}"));
}

fn exit(ctx: &mut Ctx<'_>, _: &[String]) {
    if crate::cmdline::qemu_exit_enabled() {
        arch::qemu::exit(arch::qemu::ExitCode::Success);
    }
    ctx.fail("exit: lumen.qemu_exit not set");
}

crate::ktest!(
    fn help_lists_every_command_once() {
        // Every command in the table has a line in `help` and a
        // description, and no name is in the table twice (`find` would take
        // the first).
        use crate::{kassert, kassert_eq};
        let mut console = super::Captured::default();
        let history = lumen_shell::History::new(0);
        help(&mut Ctx::new(&mut console, false, &history), &[]);
        let text = console.text;
        for (i, c) in COMMANDS.0.iter().enumerate() {
            let helped = text.lines().any(|l| l.starts_with(&alloc::format!("  {} ", c.name)));
            let once = COMMANDS.0[..i].iter().all(|d| d.name != c.name);
            if !(helped && once) {
                crate::ktest::diag(format_args!("{}: in help {helped}, once {once}", c.name));
            }
            kassert!(helped && once && !c.about.is_empty());
        }
        // The layout: descriptions from `HELP_COLUMN` (20), a usage too long
        // for that wraps to it, and an `about` ending in `:` lists its first
        // argument's words.
        let lines: alloc::vec::Vec<&str> = text.lines().collect();
        kassert!(lines.contains(&"  echo <text>       print text"));
        let crash = lines.iter().position(|l| *l == "  crash <kind> [cpu|thread]").ok_or("no crash line")?;
        let kinds = alloc::format!(
            "{:HELP_COLUMN$}deliberate fault (on that CPU, or in a new thread): {}",
            "",
            crash::KINDS.join(" ")
        );
        kassert_eq!(lines.get(crash + 1).copied(), Some(kinds.as_str()));
        for c in COMMANDS.0.iter().filter(|c| c.name.len() + c.usage.len() + 3 < HELP_COLUMN) {
            let line = alloc::format!("{:HELP_COLUMN$}{}", alloc::format!("  {} {}", c.name, c.usage), c.about);
            kassert!(lines.iter().any(|l| l.starts_with(line.as_str())));
        }
        Ok(())
    }
);

crate::ktest!(
    fn a_command_s_canvas_is_taken_back_and_no_other() {
        // A canvas a command leaves behind is taken back when it returns;
        // one held by a command on another console (another owner) stays
        // while commands run and return here.
        use crate::kassert;
        use crate::term::{self, Owner};
        fn leak(ctx: &mut Ctx<'_>, _: &[String]) {
            if let Ok(canvas) = ctx.canvas() {
                core::mem::forget(canvas);
            }
        }
        fn nothing(_: &mut Ctx<'_>, _: &[String]) {}
        static TABLE: Table<Run> = Table(&[
            Command { name: "leak", args: &[], usage: "", about: "leak a canvas", run: leak },
            Command { name: "nothing", args: &[], usage: "", about: "do nothing", run: nothing },
        ]);
        let paused = || term::status().map(|s| s.paused).ok_or("no terminal");
        let mut console = super::Captured::default();
        let history = lumen_shell::History::new(0);
        let mut ctx = Ctx::new(&mut console, false, &history);
        run_in(&TABLE, "leak", &mut ctx);
        kassert!(!paused()?);
        let other = term::canvas(Owner::new()).map_err(|_| "no canvas")?;
        run_in(&TABLE, "nothing", &mut ctx);
        run_in(&TABLE, "leak", &mut ctx);
        kassert!(paused()? && other.is_live());
        drop(other);
        kassert!(!paused()?);
        Ok(())
    }
);

crate::ktest!(
    fn a_command_s_canvas_brings_its_terminal_to_the_front() {
        // A command run by the shell on the third virtual terminal's console
        // while the first is shown: its canvas shows the third, keys routed meanwhile go there
        // (not to the first), and once it is done the third stays shown.
        use crate::vt::{self, Routed};
        use crate::{kassert, kassert_eq, term};
        use core::sync::atomic::{AtomicUsize, Ordering};
        static SHOWN: AtomicUsize = AtomicUsize::new(usize::MAX);
        static ROUTED: crate::sync::SpinLock<Option<Routed>> = crate::sync::SpinLock::new(None);
        fn draw(ctx: &mut Ctx<'_>, _: &[String]) {
            let Ok(canvas) = ctx.canvas() else { return };
            SHOWN.store(term::shown(), Ordering::Relaxed);
            let mut rings = lumen_term::vt::Router::new();
            let routed = vt::route_key(&mut rings, crate::keyboard::Key::Char('q'), (1 << term::VTS) - 1);
            *ROUTED.lock() = Some(routed);
            drop(canvas);
        }
        static TABLE: Table<Run> =
            Table(&[Command { name: "draw", args: &[], usage: "", about: "draw on the screen", run: draw }]);
        kassert_eq!(term::shown(), 0);
        let _first = term::ShowFirstAfter;
        // The shell's own console for the third terminal (writing there).
        let mut console = super::Screen { vt: 2 };
        let history = lumen_shell::History::new(0);
        run_in(&TABLE, "draw", &mut Ctx::new(&mut console, true, &history));
        kassert_eq!(SHOWN.load(Ordering::Relaxed), 2);
        kassert_eq!(*ROUTED.lock(), Some(Routed::To(2)));
        kassert_eq!(term::shown(), 2);
        kassert!(!term::status().ok_or("no terminal")?.paused);
        Ok(())
    }
);
