//! Pipelines on the shell's command line: `irqs | where count > 0 | sort
//! count desc | first 3`.
//!
//! The first command runs as it would alone, but what it writes goes into a
//! [`Capture`] (up to [`CAPTURE_LIMIT`] bytes) and becomes lines of text; a
//! system command with records (`sysinfo::records`) gives those instead.
//! Its keys still come from the console (`top 5 | count` stops on `q`), and
//! it may still draw on the screen. A command that would wait on the
//! console unseen (the rhai REPL, `top` without a frame count) refuses,
//! through [`Ctx::refuse`]: its line reaches the console and the pipeline
//! stops. Every command after a `|` is a filter (`lumen_shell::pipe`),
//! taking what came before; the last one's output is printed in one write.
//! Nothing runs if a filter is wrong (unknown, or the wrong arguments); a
//! filter that fails on its input (no such column) stops the pipeline.

use super::commands::{self, Run};
use super::{Console, Ctx, sysinfo};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use lumen_shell::pipe::{Data, FILTERS, Filter};
use lumen_shell::{Key, Table};

/// The most a command may write into a pipeline. Its buffer is reserved
/// before the command runs, so writing never allocates (a command may write
/// while holding a lock under which the heap can't be used).
pub const CAPTURE_LIMIT: usize = 64 * 1024;

/// The console a pipeline's first command writes to: what it writes is
/// kept, its keys come from the shell's console.
struct Capture<'a> {
    text: String,
    /// More than [`CAPTURE_LIMIT`] bytes were written (the rest dropped).
    overflowed: bool,
    /// The command refused to run ([`Console::refuse`]).
    refused: bool,
    console: &'a mut dyn Console,
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
        self.console.read_key()
    }

    fn try_key(&mut self) -> Option<Key> {
        self.console.try_key()
    }

    fn name(&self) -> &'static str {
        self.console.name()
    }

    fn piped(&self) -> bool {
        true
    }

    fn refuse(&mut self, line: &str) {
        self.refused = true;
        self.console.refuse(line);
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
            Some(Err(e)) => return say(ctx, format_args!("{e}")),
            None => {
                let name = words.first().map_or("", String::as_str);
                let what = if table.find(name).is_some() { "not a filter" } else { "unknown filter" };
                return say(ctx, format_args!("{name}: {what} (after a | come {})", filter_names()));
            }
        }
    }
    let Some(mut data) = source(table, first, ctx) else { return };
    for filter in &filters {
        data = match filter.apply(data) {
            Ok(data) => data,
            Err(e) => return say(ctx, format_args!("{e}")),
        };
    }
    let _ = ctx.out.write_str(&data.render());
}

/// What the pipeline's first command (`words`) gives, or `None` if it
/// didn't run, refused or wrote too much (said on the console).
fn source(table: &Table<Run>, words: &[String], ctx: &mut Ctx<'_>) -> Option<Data> {
    let (name, args) = words.split_first()?;
    if FILTERS.iter().any(|(f, _)| f == name) {
        needs_input(ctx, name);
        return None;
    }
    let Some(command) = table.find(name) else {
        say(ctx, format_args!("unknown command: {name} (try 'help')"));
        return None;
    };
    if args.is_empty()
        && let Some(records) = sysinfo::records(name)
    {
        return Some(Data::Table(records()));
    }
    let mut capture = Capture {
        text: String::with_capacity(CAPTURE_LIMIT),
        overflowed: false,
        refused: false,
        console: &mut *ctx.out,
    };
    commands::run_command(command, &mut Ctx::new(&mut capture, false, ctx.history), args);
    let Capture { text, overflowed, refused, .. } = capture;
    if refused {
        return None;
    }
    if overflowed {
        say(ctx, format_args!("{name}: wrote more than {} KiB, the most a pipeline takes", CAPTURE_LIMIT / 1024));
        return None;
    }
    Some(Data::from_text(&text))
}

/// A filter run without a `|` before it: say where it goes.
pub fn needs_input(ctx: &mut Ctx<'_>, name: &str) {
    say(ctx, format_args!("{name}: a filter: it takes what a command gives it after a | (cpus | {name} ...)"));
}

/// The filters' names, for an error: `grep, where, sort, first, count,
/// select`.
fn filter_names() -> String {
    let mut names = String::new();
    for (i, (name, _)) in FILTERS.iter().enumerate() {
        if i > 0 {
            names.push_str(", ");
        }
        names.push_str(name);
    }
    names
}

/// One line on the console, made first (see `Ctx::out`).
fn say(ctx: &mut Ctx<'_>, line: fmt::Arguments<'_>) {
    let mut text = alloc::fmt::format(line);
    text.push('\n');
    let _ = ctx.out.write_str(&text);
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
        kassert_eq!(run_line("seq 12 | where line > 5 | sort line | first 2", &[]).0.as_str(), "6\n7\n");
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
        // The timer's vector has taken interrupts.
        let (text, ..) = run_line("irqs | where count > 0 | sort count desc | first 1 | select use", &[]);
        crate::ktest::diag(format_args!("{}", text.replace('\n', " / ")));
        kassert!(text.starts_with("  use\n  "));
        let (text, ..) = run_line("threads | where thread = main | select thread state", &[]);
        // (A column keeps its width through `select`: `thread` is 16 wide.)
        kassert_eq!(text.as_str(), "  thread            state\n  main              running\n");
        let (text, ..) = run_line("mem | select area | first 2", &[]);
        kassert_eq!(text.as_str(), "  area\n  frames\n  heap\n");
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
    fn every_system_table_fits_its_columns() {
        // `Table::push` checks each row against the columns (a mistake is a
        // panic, here rather than at a user's prompt).
        use crate::kassert;
        for name in ["cpus", "irqs", "mem", "threads"] {
            let table = sysinfo::records(name).ok_or("no records")?();
            kassert!(!table.rows().is_empty());
        }
        kassert!(sysinfo::records("dmesg").is_none());
        Ok(())
    }
);

crate::ktest!(
    fn a_piped_command_takes_keys_from_the_console() {
        // `top` polls for `q` on the shell's console while its frames go
        // down the pipeline.
        use crate::{kassert, kassert_eq};
        let (text, ran_dry, left) = run_line("top 5 | count", &[Key::Char('q')]);
        kassert_eq!(text.as_str(), "1\n");
        kassert!(!ran_dry && left == 0);
        Ok(())
    }
);

crate::ktest!(
    fn a_command_that_would_wait_unseen_refuses_and_ends_the_pipeline() {
        use crate::kassert_eq;
        kassert_eq!(
            run_line("top | count", &[]).0.as_str(),
            "top: in a pipeline it needs a number of frames (top 1 | ...)\n"
        );
        kassert_eq!(
            run_line("rhai | count", &[]).0.as_str(),
            "rhai: the REPL doesn't run in a pipeline (rhai -e <expr> does)\n"
        );
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
            "grep: a filter: it takes what a command gives it after a | (cpus | grep ...)\n"
        );
        kassert_eq!(
            one("count").as_str(),
            "count: a filter: it takes what a command gives it after a | (cpus | count ...)\n"
        );
        kassert_eq!(
            one("cpus | where busy > lots").as_str(),
            "where: busy holds numbers, and 'lots' isn't one\n"
        );
        kassert_eq!(one("cpus | sort").as_str(), "usage: sort <column> [desc]\n");
        // A wrong filter stops the pipeline before the command runs.
        kassert_eq!(one("echo ran | first x").as_str(), "first: 'x' isn't a number of lines\n");
        Ok(())
    }
);
