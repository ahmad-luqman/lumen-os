//! The shell's commands: kmon's, with their names and output (`echo`
//! joins its words with single spaces; `clear` and `colors` need a console
//! that takes escape codes), plus `history` and `term text`. A table the
//! line editor completes from. Each writes to the console that ran it.

use super::{Ctx, crash};
use crate::arch;
use alloc::string::String;
use core::arch::asm;
use lumen_shell::{Command, Table, words};

type Run = fn(&mut Ctx<'_>, &[String]);

pub static COMMANDS: Table<Run> = Table(&[
    Command { name: "help", args: &[], run: help },
    Command { name: "echo", args: &[], run: echo },
    Command { name: "int3", args: &[], run: int3 },
    Command { name: "crash", args: &[crash::KINDS, &["thread"]], run: crash::run },
    Command { name: "trace", args: &[], run: trace },
    Command { name: "heap", args: &[], run: heap },
    Command { name: "ticks", args: &[], run: ticks },
    Command { name: "kbd", args: &[], run: kbd },
    Command { name: "term", args: &[&["text"]], run: term },
    Command { name: "clear", args: &[], run: clear },
    Command { name: "colors", args: &[], run: colors },
    Command { name: "seq", args: &[], run: seq },
    Command { name: "sleep", args: &[], run: sleep },
    Command { name: "uptime", args: &[], run: uptime },
    Command { name: "clocks", args: &[], run: clocks },
    Command { name: "history", args: &[], run: history },
    Command { name: "exit", args: &[], run: exit },
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

fn help(ctx: &mut Ctx<'_>, _: &[String]) {
    let _ = writeln!(
        ctx.out,
        "commands:\n  help              this list\n  echo <text>       print text\n  \
         int3              take a breakpoint and continue\n  \
         crash <kind> [cpu|thread]\n                    deliberate fault (on that CPU, or in a new thread): {}\n  \
         trace [n]         the boot CPU's last n trace events (default 16)\n  \
         heap              heap usage and free-list summary\n  \
         ticks             the boot CPU's timer interrupts (and stray ones)\n  \
         kbd               the keyboard's layout and interrupts\n  \
         term [text]       the screen's terminal: size, cursor, scrollback (text: its rows)\n  \
         clear             clear the screen and its scrollback\n  \
         colors            a colour test pattern\n  \
         seq <n>           print 1 to n, a line each\n  \
         sleep <ms>        wait that long (guest time)\n  \
         uptime            nanoseconds since the TSC started\n  \
         clocks            the TSC's frequency, measured again against each clock\n  \
         history           the lines entered on this console\n  \
         exit              end QEMU (only with lumen.qemu_exit)\n\
         keys: Tab completes; Up/Down history; Ctrl-K/U/W edit, Ctrl-C drops the line; \
         the cursor keys, Home/End, Ctrl-A/E/B/F and Ctrl-L on the screen (or serial with lumen.ansi)",
        crash::KINDS.join(" ")
    );
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
                    None => writeln!(out, "term text: {} rows, scrolled back", view.rows.len()),
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
        let mut text = String::new();
        let history = lumen_shell::History::new(0);
        help(&mut Ctx { out: &mut text, ansi: false, history: &history }, &[]);
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
