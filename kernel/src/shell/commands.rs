//! The shell's commands: kmon's, with their names and output (`echo`
//! joins its words with single spaces; `clear` and `colors` need a console
//! that takes escape codes), plus `history`, `term text`, `reboot` and
//! `shutdown`. A table the
//! line editor completes from. Each writes to the console that ran it.

use super::{Ctx, crash};
use crate::arch;
use alloc::string::String;
use core::arch::asm;
use core::fmt::Write;
use lumen_shell::{Command, Table, words};

type Run = fn(&mut Ctx<'_>, &[String]);

pub static COMMANDS: Table<Run> = Table(&[
    Command { name: "help", args: &[], usage: "", about: "this list", run: help },
    Command { name: "echo", args: &[], usage: "<text>", about: "print text", run: echo },
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
        about: "the screen's terminal: size, cursor, scrollback (text: its rows)",
        run: term,
    },
    Command { name: "clear", args: &[], usage: "", about: "clear the screen and its scrollback", run: clear },
    Command { name: "colors", args: &[], usage: "", about: "a colour test pattern", run: colors },
    Command { name: "seq", args: &[], usage: "<n>", about: "print 1 to n, a line each", run: seq },
    Command { name: "sleep", args: &[], usage: "<ms>", about: "wait that long (guest time)", run: sleep },
    Command { name: "uptime", args: &[], usage: "", about: "nanoseconds since the TSC started", run: uptime },
    Command {
        name: "clocks",
        args: &[],
        usage: "",
        about: "the TSC's frequency, measured again against each clock",
        run: clocks,
    },
    Command { name: "history", args: &[], usage: "", about: "the lines entered on this console", run: history },
    Command { name: "reboot", args: &[], usage: "", about: "restart the machine", run: reboot },
    Command { name: "shutdown", args: &[], usage: "", about: "power off (ACPI S5)", run: shutdown },
    Command { name: "exit", args: &[], usage: "", about: "end QEMU (only with lumen.qemu_exit)", run: exit },
]);

/// Split `line` into words and run the command the first names (one line
/// of error if it can't be split or no command has that name).
pub fn run(line: &str, ctx: &mut Ctx<'_>) {
    let words = match words::split(line) {
        Ok(words) => words,
        Err(e) => {
            let _ = writeln!(ctx.out, "syntax error: {e}");
            return;
        }
    };
    let Some((name, args)) = words.split_first() else { return };
    match COMMANDS.find(name) {
        Some(command) => (command.run)(ctx, args),
        None => {
            let _ = writeln!(ctx.out, "unknown command: {name} (try 'help')");
        }
    }
}

/// The only argument as a number, if there is exactly one and it is one.
fn number<T: core::str::FromStr>(args: &[String]) -> Option<T> {
    match args {
        [arg] => arg.parse().ok(),
        _ => None,
    }
}

/// Every command with its usage and what it does, then the keys. An
/// `about` ending in `:` is followed by the words its first argument can
/// be. Written in one write (made before it: see `Ctx::out`).
fn help(ctx: &mut Ctx<'_>, _: &[String]) {
    /// Where the descriptions start.
    const COLUMN: usize = 20;
    let mut text = String::from("commands:\n");
    for c in COMMANDS.0 {
        let start = text.len();
        let _ = write!(text, "  {} {}", c.name, c.usage);
        let width = text.len() - start;
        if width < COLUMN {
            text.extend(core::iter::repeat_n(' ', COLUMN - width));
        } else {
            let _ = write!(text, "\n{:COLUMN$}", "");
        }
        text.push_str(c.about);
        if c.about.ends_with(':') {
            for word in c.args.first().copied().unwrap_or_default() {
                let _ = write!(text, " {word}");
            }
        }
        text.push('\n');
    }
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
        let _ = writeln!(ctx.out, "usage: trace [n]");
        return;
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

/// `term`: the screen's terminal; `term text`: its rows, each after a `|`,
/// then a summary line.
fn term(ctx: &mut Ctx<'_>, args: &[String]) {
    const NONE: &str = "term: no terminal (no usable framebuffer, or one too small)";
    let out = &mut *ctx.out;
    match args {
        [] => {
            let _ = match crate::term::status() {
                Some(t) => writeln!(
                    out,
                    "term: {}x{}, cursor ({}, {}){}, scrollback {}/{} lines, view {} lines up, \
                     top row \"{}\", {} cells drawn",
                    t.cols,
                    t.rows,
                    t.cursor.col,
                    t.cursor.row,
                    if t.cursor.visible { "" } else { " hidden" },
                    t.scrollback,
                    t.capacity,
                    t.view,
                    t.top_row,
                    t.cells_drawn
                ),
                None => writeln!(out, "{NONE}"),
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
            None => {
                let _ = writeln!(out, "{NONE}");
            }
        },
        _ => {
            let _ = writeln!(out, "usage: term [text]");
        }
    }
}

/// The message for a command that needs escape codes on a plain console.
const PLAIN: &str = "this console is plain text (boot with lumen.ansi for escape codes)";

fn clear(ctx: &mut Ctx<'_>, _: &[String]) {
    // Home, erase the screen, erase the scrollback.
    let _ = if ctx.ansi { ctx.out.write_str("\x1b[H\x1b[2J\x1b[3J") } else { writeln!(ctx.out, "clear: {PLAIN}") };
}

/// A colour test pattern: the 16 named colours as backgrounds (`48;5;n`)
/// and as text (SGR 30-37 and 90-97), the 6x6x6 cube and the grey ramp
/// (256-colour SGR), bold, inverse and a truecolor gradient; then
/// `colors: done`.
fn colors(ctx: &mut Ctx<'_>, _: &[String]) {
    let s = &mut *ctx.out;
    if !ctx.ansi {
        let _ = writeln!(s, "colors: {PLAIN}");
        return;
    }
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
        _ => {
            let _ = writeln!(ctx.out, "usage: seq <n> (up to 10000)");
        }
    }
}

fn sleep(ctx: &mut Ctx<'_>, args: &[String]) {
    match number::<u64>(args) {
        Some(ms) if ms <= 60_000 => {
            crate::time::sleep(ms * 1_000_000);
            let _ = writeln!(ctx.out, "slept {ms} ms");
        }
        _ => {
            let _ = writeln!(ctx.out, "usage: sleep <ms> (up to 60000)");
        }
    }
}

fn uptime(ctx: &mut Ctx<'_>, _: &[String]) {
    let _ = writeln!(ctx.out, "uptime: {} ns", crate::time::now_ns());
}

/// The TSC's boot calibration, and its frequency measured again now against
/// the PIT and the HPET (the tests compare them).
fn clocks(ctx: &mut Ctx<'_>, _: &[String]) {
    use crate::time::{self, Reference};
    let out = &mut *ctx.out;
    let (Some(tsc), Some(reference)) = (time::tsc(), time::reference()) else {
        let _ = writeln!(out, "clocks: the TSC isn't calibrated");
        return;
    };
    let _ = write!(out, "clocks: tsc {} Hz (against the {reference} at boot)", tsc.hz());
    let measured = [Reference::Pit, Reference::Hpet].map(|r| (r, time::measure(r)));
    for (r, m) in &measured {
        let _ = match m {
            Some(Ok(cal)) => write!(out, "; {r} {} Hz", cal.hz),
            Some(Err(e)) => write!(out, "; {r} failed ({e})"),
            None => write!(out, "; {r} none"),
        };
    }
    let _ = writeln!(out);
    // The windows behind those, for diagnosing: `clocks: windows: boot <cal>;
    // pit <cal>; hpet <cal>` (a failed or missing one left out), each `<cal>`
    // a time::Calibration (`<hz> Hz [<w> ...] retook <n>`).
    let _ = write!(out, "clocks: windows:");
    if let Some(boot) = time::boot_calibration() {
        let _ = write!(out, " boot {boot}");
    }
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

fn reboot(ctx: &mut Ctx<'_>, _: &[String]) {
    let _ = writeln!(ctx.out, "reboot: restarting");
    crate::power::reboot()
}

/// Power off, or say why it can't be done or didn't happen.
fn shutdown(ctx: &mut Ctx<'_>, _: &[String]) {
    let why = match crate::power::power_off_plan() {
        Ok(plan) => {
            let _ = writeln!(ctx.out, "shutdown: powering off");
            crate::power::power_off(plan)
        }
        Err(why) => why,
    };
    let _ = writeln!(ctx.out, "shutdown: can't power off: {why}");
}

fn exit(ctx: &mut Ctx<'_>, _: &[String]) {
    if crate::cmdline::qemu_exit_enabled() {
        arch::qemu::exit(arch::qemu::ExitCode::Success);
    }
    let _ = writeln!(ctx.out, "exit: lumen.qemu_exit not set");
}

crate::ktest!(
    fn help_lists_every_command_once() {
        // Every command in the table has a line in `help`, and no name is
        // in the table twice (`find` would take the first).
        use crate::kassert;
        let mut console = super::Captured::default();
        let history = lumen_shell::History::new(0);
        help(&mut Ctx { out: &mut console, ansi: false, history: &history }, &[]);
        let text = console.text;
        for (i, c) in COMMANDS.0.iter().enumerate() {
            let helped = text.lines().any(|l| l.starts_with(&alloc::format!("  {} ", c.name)));
            let once = COMMANDS.0[..i].iter().all(|d| d.name != c.name);
            if !(helped && once) {
                crate::ktest::diag(format_args!("{}: in help {helped}, once {once}", c.name));
            }
            kassert!(helped && once);
        }
        Ok(())
    }
);
