//! Pipelines on the shell's command line: `irqs | where count > 0 | sort
//! count desc | first 3`.
//!
//! The first command must not be a filter, and every command after a `|`
//! must be one (`lumen_shell::pipe`). The first runs as it would alone on a
//! plain console (no escape codes), but what it writes goes into a
//! [`Capture`] (up to [`CAPTURE_LIMIT`] bytes) and becomes lines of text; a
//! system command with records (`sysinfo::RECORDS`) gives those instead.
//! Its keys still come from the console (`top 5 | count` stops on `q`),
//! until it has written more than the pipeline takes: then a key it asks
//! for is Ctrl-C, so a command polling for one stops.
//! Its usage and error lines go to the console through [`Ctx::fail`] and
//! end the pipeline; so do the refusals of commands that would wait on the
//! console unseen: the rhai REPL and demos, `top` without a frame count,
//! and whatever draws on the screen (`mandelbrot`, `life`, a script's
//! drawing), which then waits for a key. Each filter takes what came
//! before; the last one's output is printed in chunks of whole lines
//! (`shell::write_in_chunks`). Nothing runs if a filter is wrong (unknown,
//! or the wrong arguments); a filter that fails on its input (no such
//! column) stops the pipeline.

use super::commands::{self, Run};
use super::{Console, Ctx, sysinfo, write_in_chunks};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use lumen_shell::pipe::{Data, FILTERS, Filter};
use lumen_shell::{Key, Table};

/// The most a command may write into a pipeline.
const CAPTURE_LIMIT: usize = 64 * 1024;

/// The console a pipeline's first command writes to: what it writes is
/// kept, its keys come from the shell's console.
struct Capture<'a> {
    /// Reserved whole before the command runs, so writing never allocates
    /// (a command may write while holding a lock under which the heap
    /// can't be used).
    text: String,
    /// More than [`CAPTURE_LIMIT`] bytes were written (the rest dropped):
    /// from then on a key asked for is Ctrl-C, and a failure isn't shown
    /// (the overflow is said instead).
    overflowed: bool,
    /// The command failed or refused to run ([`Console::fail`]).
    failed: bool,
    console: &'a mut dyn Console,
}

impl<'a> Capture<'a> {
    fn new(console: &'a mut dyn Console) -> Capture<'a> {
        Capture { text: String::with_capacity(CAPTURE_LIMIT), overflowed: false, failed: false, console }
    }

    /// The key an overflowed pipeline's command is given: Ctrl-C, or
    /// `Key::Closed` once its console closed (a terminal window, #155).
    fn stop_key(&self) -> Key {
        if self.console.closed() { Key::Closed } else { Key::Ctrl('c') }
    }
}

impl Write for Capture<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if self.overflowed || self.text.len() + s.len() > CAPTURE_LIMIT {
            self.overflowed = true;
            return Err(fmt::Error);
        }
        self.text.push_str(s);
        Ok(())
    }
}

impl Console for Capture<'_> {
    fn read_key(&mut self) -> Key {
        if self.overflowed { self.stop_key() } else { self.console.read_key() }
    }

    fn try_key(&mut self) -> Option<Key> {
        if self.overflowed { Some(self.stop_key()) } else { self.console.try_key() }
    }

    fn name(&self) -> &'static str {
        self.console.name()
    }

    /// The console's: a command at the start of a pipeline on a virtual
    /// terminal that takes the canvas brings that terminal to the front, and
    /// in a terminal window (#155) gets none.
    fn place(&self) -> crate::term::Place {
        self.console.place()
    }

    /// The console's (a terminal window's, #155, closing): so the shell on
    /// it ends after a pipeline as after a command, and an overflowed
    /// pipeline's key is `Key::Closed` rather than Ctrl-C once it closed.
    fn closed(&self) -> bool {
        self.console.closed()
    }

    fn piped(&self) -> bool {
        true
    }

    fn fail(&mut self, line: &str) {
        self.failed = true;
        if !self.overflowed {
            self.console.fail(line);
        }
    }
}

/// Run a pipeline of two commands or more (`stages`, each its words, none
/// empty: `words::split_pipeline`'s) with the commands in `table`.
pub fn run(table: &Table<Run>, stages: &[Vec<String>], ctx: &mut Ctx<'_>) {
    let Some((first, rest)) = stages.split_first() else { return };
    let mut filters = Vec::with_capacity(rest.len());
    for words in rest {
        match Filter::parse(words) {
            Some(Ok(filter)) => filters.push(filter),
            Some(Err(e)) => return ctx.fail(e),
            None => {
                let name = words.first().map_or("", String::as_str);
                let what = if table.find(name).is_some() { "not a filter" } else { "unknown filter" };
                let names: Vec<&str> = FILTERS.iter().map(|(n, _)| *n).collect();
                return ctx.fail(format_args!("{name}: {what} (after a | come {})", names.join(", ")));
            }
        }
    }
    let Some(mut data) = source(table, first, ctx) else { return };
    for filter in &filters {
        data = match filter.apply(data) {
            Ok(data) => data,
            Err(e) => return ctx.fail(e),
        };
    }
    write_in_chunks(ctx.out, &data.render());
}

/// What the pipeline's first command (`words`) gives, or `None` if it
/// didn't run, failed or wrote too much (said on the console).
fn source(table: &Table<Run>, words: &[String], ctx: &mut Ctx<'_>) -> Option<Data> {
    let (name, args) = words.split_first()?;
    // (A filter here runs as a command: it says it goes after a `|`.)
    let Some(command) = table.find(name) else {
        commands::unknown(ctx, name);
        return None;
    };
    // (Arguments it gives no records for run it as a command: its usage
    // line, or what they ask for, e.g. `mouse hold`.)
    if let Some(make) = sysinfo::records(name)
        && let Some(records) = make(args)
    {
        let table = match records {
            Ok(table) => table,
            Err(e) => {
                ctx.fail(e);
                return None;
            }
        };
        // A caveat the records can't carry goes to the console first
        // (`lspci`: why devices are missing).
        if let Some(note) = sysinfo::records_note(name) {
            let _ = writeln!(ctx.out, "{note}");
        }
        return Some(Data::Table(table));
    }
    let mut capture = Capture::new(&mut *ctx.out);
    // `false`: no escape codes into a pipeline (`colors` and `clear`
    // refuse).
    commands::run_command(command, &mut Ctx::new(&mut capture, false, ctx.history), args);
    let Capture { text, overflowed, failed, .. } = capture;
    if overflowed {
        ctx.fail(format_args!("{name}: wrote more than {} KiB, the most a pipeline takes", CAPTURE_LIMIT / 1024));
        return None;
    }
    (!failed).then(|| Data::from_text(&text))
}

/// A filter run without a `|` before it: say where it goes.
pub fn needs_input(ctx: &mut Ctx<'_>, name: &str) {
    let usage = FILTERS.iter().find(|(f, _)| *f == name).map_or("", |(_, u)| *u);
    let space = if usage.is_empty() { "" } else { " " };
    ctx.fail(format_args!(
        "{name}: a filter: it takes what a command gives it after a | (cpus | {name}{space}{usage})"
    ));
}

#[cfg(feature = "ktest")]
fn run_line(line: &str, keys: &[Key]) -> (String, bool, usize) {
    let mut console = super::Captured::default();
    console.keys.extend(keys.iter().copied());
    let history = lumen_shell::History::new(0);
    commands::run(line, &mut Ctx::new(&mut console, false, &history));
    (console.text, console.ran_dry, console.keys.len())
}

crate::ktest!(
    fn a_pipeline_filters_what_a_command_writes() {
        use crate::kassert_eq;
        kassert_eq!(run_line("seq 12 | grep 1", &[]).0.as_str(), "1\n10\n11\n12\n");
        kassert_eq!(run_line("seq 12 | grep 1 | count", &[]).0.as_str(), "4\n");
        // Lines that are numbers compare as numbers with one: 6 to 12.
        kassert_eq!(run_line("seq 12 | where line > 5 | count", &[]).0.as_str(), "7\n");
        // ... and sort by value, as `where` reads them.
        kassert_eq!(run_line("seq 12 | where line > 5 | sort line desc | first 2", &[]).0.as_str(), "12\n11\n");
        // A script's printed lines go on.
        kassert_eq!(run_line("rhai -e 'print(\"a\"); print(\"b\")' | count", &[]).0.as_str(), "2\n");
        kassert_eq!(run_line("echo 'a | b' | grep '|'", &[]).0.as_str(), "a | b\n");
        Ok(())
    }
);

crate::ktest!(
    fn system_commands_hand_on_records() {
        use crate::{kassert, kassert_eq};
        let online = (0..crate::arch::MAX_CPUS).filter(|&c| crate::smp::online_lapic_id(c).is_some()).count();
        let cpus = alloc::format!("{online}\n");
        kassert_eq!(run_line("cpus | count", &[]).0, cpus);
        let (text, ..) = run_line("cpus | where cpu = 0 | select cpu scheduler", &[]);
        kassert_eq!(text.as_str(), "  cpu  scheduler\n    0  running\n");
        // The busiest vector has taken interrupts, and the timer's has.
        let (text, ..) = run_line("irqs | where count > 0 | sort count desc | first 1 | select count", &[]);
        let rows: Vec<&str> = text.lines().collect();
        kassert!(rows.len() == 2 && rows[0] == "  count");
        kassert!(rows[1].trim().parse::<u64>().is_ok_and(|n| n > 0));
        let (text, ..) = run_line("irqs | where use = 'local APIC timer' | select count", &[]);
        let rows: Vec<&str> = text.lines().collect();
        kassert!(rows.len() == 2 && rows[1].trim().parse::<u64>().is_ok_and(|n| n > 0));
        // (A column keeps its width through `select`: `thread` is 16 wide.)
        let (text, ..) = run_line("threads | where thread = main | select thread state", &[]);
        kassert_eq!(text.as_str(), "  thread            state\n  main              running\n");
        let (text, ..) = run_line("mem | select area | first 2", &[]);
        kassert_eq!(text.as_str(), "  area\n  frames\n  heap\n");
        Ok(())
    }
);

crate::ktest!(
    fn records_come_from_commands_given_no_arguments() {
        // Each name in `RECORDS` is a command (a misspelt one would give
        // text), it gives no records for an argument it doesn't take (its
        // maker says `None`: it runs as a command), and as a command it
        // then says its usage (its table's usage line), piped or not.
        use crate::{kassert, kassert_eq};
        let x = [alloc::string::String::from("x")];
        for (name, make) in sysinfo::RECORDS {
            let command = commands::COMMANDS.find(name);
            kassert_eq!(command.map(|c| c.name), Some(name));
            kassert!(make(&x).is_none());
            let usage = match command.map_or("", |c| c.usage) {
                "" => alloc::format!("usage: {name}\n"),
                words => alloc::format!("usage: {name} {words}\n"),
            };
            for line in [alloc::format!("{name} x | count"), alloc::format!("{name} x")] {
                let (text, ..) = run_line(&line, &[]);
                if text != usage {
                    crate::ktest::diag(format_args!("{line}: {text:?}"));
                }
                kassert_eq!(text, usage);
            }
        }
        Ok(())
    }
);

crate::ktest!(
    fn records_come_from_commands_with_the_arguments_they_take() {
        // `mouse events` and `kbd events [n]` give the rings' records in a
        // pipeline; a bad count says the usage and ends it; `mouse hold`
        // and `mouse release` still run as commands (`None`). Without a
        // tablet (the ktest machine has none) `mouse events` says so as
        // `mouse` does; with no screen shell running (none in the test
        // kernel) `kbd events` first says nothing fills its ring.
        use crate::{kassert, kassert_eq};
        let s = |w: &[&str]| w.iter().map(|w| alloc::string::String::from(*w)).collect::<Vec<_>>();
        let make = |name| sysinfo::records(name).expect("a records command");
        kassert!(make("mouse")(&s(&["hold"])).is_none() && make("mouse")(&s(&["release"])).is_none());
        kassert!(make("kbd")(&[]).is_none());
        kassert!(matches!(make("kbd")(&s(&["events", "3"])), Some(Ok(_))));
        kassert_eq!(run_line("kbd events 0 | count", &[]).0.as_str(), "usage: kbd events [n]\n");
        kassert_eq!(run_line("kbd events 1 2", &[]).0.as_str(), "usage: kbd events [n]\n");
        let no_tablet = "mouse: no tablet (QEMU: -device virtio-tablet-pci)\n";
        kassert!(crate::mouse::status().is_none());
        kassert_eq!(run_line("mouse events | count", &[]).0.as_str(), no_tablet);
        kassert_eq!(run_line("mouse events 3", &[]).0.as_str(), no_tablet);
        kassert_eq!(crate::vt::readers(), 0);
        let note = "kbd: key events are decoded by the screen shells; none run (none started)\n";
        let (text, ..) = run_line("kbd events | count", &[]);
        kassert_eq!(text, alloc::format!("{note}{}\n", crate::input::KEYS.recent(16).len()));
        let (text, ..) = run_line("kbd events", &[]);
        kassert!(text.starts_with(note) && text.lines().nth(1).is_some_and(|h| h.trim_start().starts_with("seq")));
        Ok(())
    }
);

crate::ktest!(
    fn every_filter_is_a_command_with_its_usage() {
        // Help and completion know the filters; their usage is the one
        // their errors give.
        use crate::kassert_eq;
        for (name, usage) in FILTERS {
            let command = commands::COMMANDS.find(name).ok_or("a filter isn't a command")?;
            kassert_eq!(command.usage, usage);
        }
        Ok(())
    }
);

crate::ktest!(
    fn a_piped_command_takes_keys_from_the_console() {
        // `top` polls for `q` on the shell's console while its frames would
        // go down the pipeline: `q`, waiting before the first frame, stops
        // it with its one line.
        use crate::{kassert, kassert_eq};
        let (text, ran_dry, left) = run_line("top 5 | count", &[Key::Char('q')]);
        kassert_eq!(text.as_str(), "1\n");
        kassert!(!ran_dry && left == 0);
        Ok(())
    }
);

crate::ktest!(
    fn a_command_that_would_wait_unseen_refuses_and_ends_the_pipeline() {
        // Each says so on the console, once, and nothing goes on (no count
        // follows); none waits for a key (the test console has none).
        use crate::{kassert, kassert_eq};
        let cases = [
            ("top | count", "top: in a pipeline it needs a number of frames (top 1 | ...)\n"),
            ("rhai | count", "rhai: the REPL doesn't run in a pipeline (rhai -e <expr> does)\n"),
            ("rhai demo count | count", "rhai: demos don't run in a pipeline (rhai -e <expr> does)\n"),
            ("mandelbrot | count", "mandelbrot: draws on the screen and waits for a key: not in a pipeline\n"),
            ("life glider 5 | count", "life: draws on the screen and waits for a key: not in a pipeline\n"),
            (
                "rhai -e 'print(1); clear(0)' | count",
                "rhai: Runtime error: drawing needs the screen, which a command in a pipeline doesn't get (line 1, position 11)\n",
            ),
        ];
        for (line, want) in cases {
            let (text, ran_dry, _) = run_line(line, &[]);
            if text != want {
                crate::ktest::diag(format_args!("{line}: {text:?}"));
            }
            kassert_eq!(text.as_str(), want);
            kassert!(!ran_dry);
        }
        Ok(())
    }
);

crate::ktest!(
    fn a_command_s_usage_and_errors_reach_the_console_not_the_pipeline() {
        use crate::kassert_eq;
        let cases = [
            ("seq x | count", "usage: seq <n> (up to 10000)\n"),
            ("seq x | grep 1", "usage: seq <n> (up to 10000)\n"),
            ("trace x | count", "usage: trace [n | runq [park <cpu> <n>] | xray <cpu> [since]]\n"),
            ("sleep x | count", "usage: sleep <ms> (up to 60000)\n"),
            ("top 0 | count", "usage: top [frames] (q or Ctrl-C quits)\n"),
            ("cpus x | where busy > 5", "usage: cpus\n"),
            ("life a b c | count", "life: too many arguments\nusage: life [pattern] [generations]\n"),
            ("colors | count", "colors: escape codes don't go into a pipeline\n"),
            ("clear | count", "clear: escape codes don't go into a pipeline\n"),
            ("reboot | count", "reboot: not in a pipeline\n"),
            ("shutdown | count", "shutdown: not in a pipeline\n"),
            ("term nope | count", "usage: term [text [window]]\n"),
        ];
        for (line, want) in cases {
            let (text, ..) = run_line(line, &[]);
            if text != want {
                crate::ktest::diag(format_args!("{line}: {text:?}"));
            }
            kassert_eq!(text.as_str(), want);
        }
        // Their first line (the rest names things that may change).
        for (line, first) in [
            ("crash nope | count", "crash: unknown kind 'nope' ("),
            ("rhai -e 'nosuchfn()' | count", "rhai: "),
            ("rhai demo nope | count", "rhai: no demo called nope"),
        ] {
            let (text, ..) = run_line(line, &[]);
            if !text.starts_with(first) || text.lines().count() != 1 {
                crate::ktest::diag(format_args!("{line}: {text:?}"));
            }
            crate::kassert!(text.starts_with(first) && text.lines().count() == 1 && text != "1\n");
        }
        Ok(())
    }
);

crate::ktest!(
    fn too_much_output_stops_the_pipeline() {
        // Up to the limit it all goes on; a byte more, and the pipeline
        // stops with a line saying so.
        use crate::kassert_eq;
        use lumen_shell::Command;
        fn fill(ctx: &mut Ctx<'_>, args: &[String]) {
            let extra = usize::from(!args.is_empty());
            let line = "x".repeat(1023);
            for _ in 0..CAPTURE_LIMIT / 1024 {
                let _ = writeln!(ctx.out, "{line}");
            }
            let _ = ctx.out.write_str(&"y".repeat(extra));
        }
        static TABLE: Table<Run> =
            Table(&[Command { name: "fill", args: &[], usage: "[more]", about: "write the limit", run: fill }]);
        let mut console = super::Captured::default();
        let history = lumen_shell::History::new(0);
        let mut ctx = Ctx::new(&mut console, false, &history);
        commands::run_in(&TABLE, "fill | count", &mut ctx);
        commands::run_in(&TABLE, "fill more | count", &mut ctx);
        kassert_eq!(console.text.as_str(), "64\nfill: wrote more than 64 KiB, the most a pipeline takes\n");
        Ok(())
    }
);

crate::ktest!(
    fn a_full_pipeline_stops_a_command_polling_for_a_key() {
        // Past the limit a key asked for is Ctrl-C (the console's own keys
        // stay), and the command's failure isn't shown: the overflow is.
        use crate::{kassert, kassert_eq};
        use lumen_shell::Command;
        fn flood(ctx: &mut Ctx<'_>, _: &[String]) {
            let line = "x".repeat(1023);
            for _ in 0..=CAPTURE_LIMIT / 1024 {
                let _ = writeln!(ctx.out, "{line}");
            }
            if ctx.poll_key() == Some(Key::Ctrl('c')) && ctx.wait_key() == Key::Ctrl('c') {
                ctx.fail("flood: stopped (Ctrl-C)");
            }
        }
        static TABLE: Table<Run> =
            Table(&[Command { name: "flood", args: &[], usage: "", about: "write too much", run: flood }]);
        let mut console = super::Captured::default();
        console.keys.push_back(Key::Char('q'));
        let history = lumen_shell::History::new(0);
        commands::run_in(&TABLE, "flood | count", &mut Ctx::new(&mut console, false, &history));
        kassert_eq!(console.text.as_str(), "flood: wrote more than 64 KiB, the most a pipeline takes\n");
        kassert!(console.keys.len() == 1 && !console.ran_dry);
        Ok(())
    }
);

crate::ktest!(
    fn pipeline_mistakes_are_one_line() {
        use crate::kassert_eq;
        let one = |line| run_line(line, &[]).0;
        kassert_eq!(one("| count").as_str(), "syntax error: a | with nothing before it\n");
        kassert_eq!(one("cpus |").as_str(), "syntax error: a | with nothing after it\n");
        kassert_eq!(one("nope | count").as_str(), "unknown command: nope (try 'help')\n");
        kassert_eq!(
            one("cpus | echo x").as_str(),
            "echo: not a filter (after a | come grep, where, sort, first, count, select)\n"
        );
        kassert_eq!(
            one("cpus | uniq").as_str(),
            "uniq: unknown filter (after a | come grep, where, sort, first, count, select)\n"
        );
        kassert_eq!(
            one("grep x | count").as_str(),
            "grep: a filter: it takes what a command gives it after a | (cpus | grep <text>)\n"
        );
        kassert_eq!(
            one("count").as_str(),
            "count: a filter: it takes what a command gives it after a | (cpus | count)\n"
        );
        kassert_eq!(one("cpus | where busy > lots").as_str(), "where: busy holds numbers, and 'lots' isn't one\n");
        kassert_eq!(one("irqs | where vector = 64").as_str(), "where: vector is hex: write it with 0x (e.g. 0x20)\n");
        kassert_eq!(one("cpus | sort").as_str(), "usage: sort <column> [desc]\n");
        kassert_eq!(one("echo ran | first x").as_str(), "first: 'x' isn't a number of lines\n");
        kassert_eq!(one("echo ran | select a a").as_str(), "select: a named twice\n");
        Ok(())
    }
);

crate::ktest!(
    fn a_wrong_filter_stops_the_pipeline_before_the_command_runs() {
        // `top` would take the `q` waiting (`a_piped_command_takes_keys_
        // from_the_console`): it stays, so `top` never ran.
        use crate::{kassert, kassert_eq};
        for line in ["top 5 | first x", "top 5 | count | select a a", "top 5 | count | uniq"] {
            let (text, ran_dry, left) = run_line(line, &[Key::Char('q')]);
            kassert_eq!(text.lines().count(), 1);
            kassert!(!text.starts_with("top") && !ran_dry && left == 1);
        }
        Ok(())
    }
);

crate::ktest!(
    fn a_pipeline_in_a_terminal_window_gets_no_canvas() {
        // A command at the head of a pipeline typed in a terminal window
        // (#155) runs on a capture wrapping the window's console: it is in
        // the window all the same (no canvas: drawing over the screen from
        // a window would leave a picture no key can take away), and closed
        // with it.
        use crate::kassert;
        use crate::term::{CanvasError, Place};
        struct InWindow {
            closed: bool,
        }
        impl Write for InWindow {
            fn write_str(&mut self, _: &str) -> fmt::Result {
                Ok(())
            }
        }
        impl Console for InWindow {
            fn read_key(&mut self) -> Key {
                Key::Closed
            }
            fn try_key(&mut self) -> Option<Key> {
                None
            }
            fn name(&self) -> &'static str {
                "a window"
            }
            fn place(&self) -> Place {
                Place::Window(3)
            }
            fn closed(&self) -> bool {
                self.closed
            }
        }
        let mut window = InWindow { closed: true };
        let mut capture = Capture::new(&mut window);
        kassert!(capture.place() == Place::Window(3) && capture.closed());
        let history = lumen_shell::History::new(0);
        let ctx = Ctx::new(&mut capture, false, &history);
        kassert!(matches!(ctx.canvas(), Err(CanvasError::InWindow)));
        Ok(())
    }
);
