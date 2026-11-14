use crate::pipe::{Align, Column, Data, Filter, Kind, PipeError, Table, Value};
use crate::words::{self, SplitError};
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

fn stages(line: &str) -> Vec<Vec<String>> {
    words::split_pipeline(line).unwrap_or_else(|e| panic!("{line:?}: {e}"))
}

#[test]
fn a_pipe_outside_quotes_ends_a_command() {
    assert_eq!(stages("dmesg | grep time"), vec![strings(&["dmesg"]), strings(&["grep", "time"])]);
    assert_eq!(stages("a|b|c"), vec![strings(&["a"]), strings(&["b"]), strings(&["c"])]);
    assert_eq!(stages("a x|  b"), vec![strings(&["a", "x"]), strings(&["b"])]);
    // Quoted or escaped, it is a character.
    assert_eq!(stages("grep '|' | count"), vec![strings(&["grep", "|"]), strings(&["count"])]);
    assert_eq!(stages("echo \"a|b\" c\\|d"), vec![strings(&["echo", "a|b", "c|d"])]);
    // In a comment it is nothing.
    assert_eq!(stages("echo a # | grep b"), vec![strings(&["echo", "a"])]);
    assert_eq!(stages(""), Vec::<Vec<String>>::new());
    assert_eq!(stages("  # | x"), Vec::<Vec<String>>::new());
    // `split` keeps it in the word, as shlex does.
    assert_eq!(words::split("a|b c").unwrap(), strings(&["a|b", "c"]));
}

#[test]
fn a_pipe_needs_a_command_on_each_side() {
    assert_eq!(words::split_pipeline("| grep a"), Err(SplitError::NothingBeforePipe));
    assert_eq!(words::split_pipeline("cpus || count"), Err(SplitError::NothingBeforePipe));
    assert_eq!(words::split_pipeline("cpus | # count"), Err(SplitError::NothingAfterPipe));
    assert_eq!(words::split_pipeline("cpus |"), Err(SplitError::NothingAfterPipe));
    assert_eq!(words::split_pipeline("cpus | grep 'a"), Err(SplitError::UnclosedQuote('\'')));
    assert_eq!(SplitError::NothingBeforePipe.to_string(), "a | with nothing before it");
    assert_eq!(SplitError::NothingAfterPipe.to_string(), "a | with nothing after it");
}

#[test]
fn completion_sees_only_the_command_after_the_last_pipe() {
    let p = words::partial("irqs | wh").unwrap();
    assert_eq!((p.words, p.start, p.text.as_str()), (Vec::new(), 7, "wh"));
    let p = words::partial("irqs | sort count ").unwrap();
    assert_eq!((p.words, p.text.as_str()), (strings(&["sort", "count"]), ""));
    // A quoted pipe doesn't start a command.
    let p = words::partial("grep '| x").unwrap();
    assert_eq!((p.words, p.text.as_str(), p.quote), (strings(&["grep"]), "| x", Some('\'')));
}

#[test]
fn a_quoted_pipe_reads_back_as_itself() {
    let quoted = words::quote("a|b", None);
    assert_eq!(quoted, "a\\|b");
    assert_eq!(stages(&quoted), vec![strings(&["a|b"])]);
}

const CPU: Column = Column::new("cpu", Kind::Int, Align::Right, 3);
const SCHED: Column = Column::new("scheduler", Kind::Text, Align::Left, 11);
const TICKS: Column = Column::new("ticks", Kind::Int, Align::Left, 10);
const BUSY: Column = Column::new("busy", Kind::Percent, Align::Right, 4);
const VECTOR: Column = Column::new("vector", Kind::Hex, Align::Left, 6);
const USE: Column = Column::new("use", Kind::Text, Align::Left, 16);
const COUNT: Column = Column::new("count", Kind::Int, Align::Left, 0);

fn text(s: &str) -> Value {
    Value::Text(s.to_string())
}

fn cpus() -> Table {
    let mut t = Table::new(&[CPU, SCHED, TICKS, BUSY]);
    t.push(vec![Value::Int(0), text("running"), Value::Int(1234), Value::Int(97)]);
    t.push(vec![Value::Int(1), text("running"), Value::Int(1200), Value::Absent("stalled")]);
    t.push(vec![Value::Int(2), text("not started"), Value::Int(0), Value::Absent("-")]);
    t
}

fn vectors() -> Table {
    let mut t = Table::new(&[VECTOR, USE, COUNT]);
    for (v, name, n) in [
        (0x20, "timer", 5000),
        (0x21, "keyboard", 0),
        (0x22, "serial", 7),
        (0x23, "spurious", 7),
        (0x24, "error", 2),
        (0xfe, "remote wake", 7),
    ] {
        t.push(vec![Value::Int(v), text(name), Value::Int(n)]);
    }
    t
}

fn apply(data: Data, words: &[&str]) -> Result<Data, PipeError> {
    Filter::parse(&strings(words)).expect("a filter")?.apply(data)
}

fn table(data: Result<Data, PipeError>) -> Table {
    match data {
        Ok(Data::Table(t)) => t,
        other => panic!("not a table: {other:?}"),
    }
}

fn column(t: &Table, i: usize) -> Vec<Value> {
    t.rows().iter().map(|r| r[i].clone()).collect()
}

#[test]
fn a_table_prints_as_the_system_commands_lay_out_rows() {
    // As `cpus` printed them before tables: a right-aligned number, padded
    // text, the headings laid out like the values, nothing after the last.
    let mut out = String::new();
    cpus().render(&mut out);
    assert_eq!(
        out,
        // ("stalled" makes busy 7 wide.)
        "  cpu  scheduler    ticks          busy\n\
         \x20   0  running      1234            97%\n\
         \x20   1  running      1200        stalled\n\
         \x20   2  not started  0                 -\n"
    );
    let mut out = String::new();
    vectors().render(&mut out);
    assert!(out.starts_with("  vector  use               count\n  0x20    timer             5000\n"), "{out}");
}

#[test]
fn a_wide_value_widens_its_column_for_every_row() {
    let mut t = Table::new(&[USE, COUNT]);
    t.push(vec![text("a name longer than sixteen"), Value::Int(1)]);
    t.push(vec![text("short"), Value::Int(2)]);
    let mut out = String::new();
    t.render(&mut out);
    assert_eq!(
        out,
        "  use                         count\n  a name longer than sixteen  1\n  short                       2\n"
    );
}

#[test]
#[should_panic(expected = "doesn't fit column")]
fn a_row_with_text_in_a_number_column_is_a_bug() {
    Table::new(&[CPU]).push(vec![text("zero")]);
}

#[test]
#[should_panic(expected = "one value per column")]
fn a_short_row_is_a_bug() {
    Table::new(&[CPU, SCHED]).push(vec![Value::Int(0)]);
}

#[test]
fn where_compares_numbers_as_numbers_and_text_as_text() {
    let t = table(apply(Data::Table(vectors()), &["where", "count", ">", "6"]));
    assert_eq!(column(&t, 1), vec![text("timer"), text("serial"), text("spurious"), text("remote wake")]);
    let t = table(apply(Data::Table(vectors()), &["where", "count", "=", "7"]));
    assert_eq!(t.rows().len(), 3);
    let t = table(apply(Data::Table(vectors()), &["where", "count", "!=", "7"]));
    assert_eq!(t.rows().len(), 3);
    let t = table(apply(Data::Table(vectors()), &["where", "count", "<=", "2"]));
    assert_eq!(column(&t, 1), vec![text("keyboard"), text("error")]);
    // Hex: as printed, or in decimal.
    let t = table(apply(Data::Table(vectors()), &["where", "vector", ">=", "0x23"]));
    assert_eq!(t.rows().len(), 3);
    let t = table(apply(Data::Table(vectors()), &["where", "vector", "=", "254"]));
    assert_eq!(column(&t, 1), vec![text("remote wake")]);
    // Text: byte order, and ~ for contains.
    let t = table(apply(Data::Table(vectors()), &["where", "use", "<", "k"]));
    assert_eq!(column(&t, 1), vec![text("error")]);
    let t = table(apply(Data::Table(vectors()), &["where", "use", "~", "e"]));
    assert_eq!(
        column(&t, 1),
        vec![text("timer"), text("keyboard"), text("serial"), text("error"), text("remote wake")]
    );
    // A percentage may be written with its %; a missing value never matches.
    let t = table(apply(Data::Table(cpus()), &["where", "busy", ">", "50%"]));
    assert_eq!(column(&t, 0), vec![Value::Int(0)]);
    let t = table(apply(Data::Table(cpus()), &["where", "busy", "!=", "97"]));
    assert_eq!(t.rows().len(), 0);
    let t = table(apply(Data::Table(cpus()), &["where", "busy", "<", "-5"]));
    assert_eq!(t.rows().len(), 0);
}

#[test]
fn where_says_what_is_wrong() {
    let err = |words: &[&str]| apply(Data::Table(vectors()), words).unwrap_err().to_string();
    assert_eq!(err(&["where", "calls", ">", "1"]), "where: no column 'calls' (columns: vector, use, count)");
    assert_eq!(err(&["where", "count", ">", "many"]), "where: count holds numbers, and 'many' isn't one");
    assert_eq!(err(&["where", "count", "~", "1"]), "where: ~ is for text, and count holds numbers");
    assert_eq!(err(&["where", "count", "==", "1"]), "where: no operator '==' (one of = != < <= > >= ~)");
    assert_eq!(err(&["where", "count", ">"]), "usage: where <column> <op> <value> (op: = != < <= > >= ~)");
    assert_eq!(err(&["where", "count>1"]), "usage: where <column> <op> <value> (op: = != < <= > >= ~)");
}

#[test]
fn sort_keeps_equal_rows_in_order_and_missing_values_last() {
    let t = table(apply(Data::Table(vectors()), &["sort", "count"]));
    assert_eq!(
        column(&t, 1),
        vec![text("keyboard"), text("error"), text("serial"), text("spurious"), text("remote wake"), text("timer")]
    );
    // Descending: equal ones still in their order.
    let t = table(apply(Data::Table(vectors()), &["sort", "count", "desc"]));
    assert_eq!(
        column(&t, 1),
        vec![text("timer"), text("serial"), text("spurious"), text("remote wake"), text("error"), text("keyboard")]
    );
    for desc in [&["sort", "busy"][..], &["sort", "busy", "desc"]] {
        let t = table(apply(Data::Table(cpus()), desc));
        assert_eq!(column(&t, 0), vec![Value::Int(0), Value::Int(1), Value::Int(2)], "{desc:?}");
    }
    // Numbers as numbers (1200 before 1234 before... 0 first), text by bytes.
    let t = table(apply(Data::Table(cpus()), &["sort", "ticks"]));
    assert_eq!(column(&t, 2), vec![Value::Int(0), Value::Int(1200), Value::Int(1234)]);
    let t = table(apply(Data::Table(cpus()), &["sort", "scheduler", "desc"]));
    assert_eq!(column(&t, 0), vec![Value::Int(0), Value::Int(1), Value::Int(2)]);
    assert_eq!(
        apply(Data::Table(cpus()), &["sort", "busy", "up"]).unwrap_err().to_string(),
        "usage: sort <column> [desc]"
    );
}

/// A small deterministic generator (xorshift64).
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

#[test]
fn sort_is_stable_on_random_tables() {
    // Against a stable insertion sort, with few distinct keys (many ties)
    // and some missing values; each row's position before the sort is in
    // its second column.
    const KEY: Column = Column::new("key", Kind::Int, Align::Right, 0);
    const AT: Column = Column::new("at", Kind::Int, Align::Right, 0);
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..500 {
        let mut t = Table::new(&[KEY, AT]);
        let len = rng.below(40);
        for at in 0..len {
            let key = match rng.below(5) {
                0 => Value::Absent("-"),
                k => Value::Int(k as i64),
            };
            t.push(vec![key, Value::Int(at as i64)]);
        }
        for desc in [false, true] {
            let mut want: Vec<Vec<Value>> = Vec::new();
            for row in t.rows() {
                let key = |r: &Vec<Value>| match r[0] {
                    Value::Int(k) => Some(if desc { -k } else { k }),
                    _ => None,
                };
                // After every row that sorts before it or ties with it.
                let at = want
                    .iter()
                    .position(|w| match (key(w), key(row)) {
                        (None, Some(_)) => true,
                        (Some(a), Some(b)) => a > b,
                        _ => false,
                    })
                    .unwrap_or(want.len());
                want.insert(at, row.clone());
            }
            let words: &[&str] = if desc { &["sort", "key", "desc"] } else { &["sort", "key"] };
            let got = table(apply(Data::Table(t.clone()), words));
            assert_eq!(got.rows(), &want[..], "desc {desc}");
        }
    }
}

#[test]
fn first_count_and_grep_take_text_or_rows() {
    let lines = Data::from_text("one\ntwo\nthree\n");
    assert_eq!(apply(lines.clone(), &["first", "2"]), Ok(Data::Text(strings(&["one", "two"]))));
    assert_eq!(apply(lines.clone(), &["first", "9"]), Ok(lines.clone()));
    assert_eq!(apply(lines.clone(), &["count"]), Ok(Data::Text(strings(&["3"]))));
    assert_eq!(apply(lines.clone(), &["grep", "o"]), Ok(Data::Text(strings(&["one", "two"]))));
    assert_eq!(apply(Data::Table(vectors()), &["count"]), Ok(Data::Text(strings(&["6"]))));
    assert_eq!(table(apply(Data::Table(vectors()), &["first", "1"])).rows().len(), 1);
    assert_eq!(apply(lines.clone(), &["first", "-1"]).unwrap_err().to_string(), "first: '-1' isn't a number of lines");
    assert_eq!(apply(lines, &["count", "x"]).unwrap_err().to_string(), "usage: count ");
    // A row matches when a value as printed does: hex, a percentage, a
    // missing value's text; the headings stay.
    let t = table(apply(Data::Table(vectors()), &["grep", "0x2"]));
    assert_eq!(t.rows().len(), 5);
    let t = table(apply(Data::Table(cpus()), &["grep", "97%"]));
    assert_eq!(column(&t, 0), vec![Value::Int(0)]);
    let t = table(apply(Data::Table(cpus()), &["grep", "stalled"]));
    assert_eq!(column(&t, 0), vec![Value::Int(1)]);
    assert!(Data::Table(t).render().starts_with("  cpu  scheduler"));
}

#[test]
fn select_picks_columns_in_the_order_named() {
    let t = table(apply(Data::Table(vectors()), &["select", "count", "use"]));
    assert_eq!(t.columns().iter().map(|c| c.name).collect::<Vec<_>>(), ["count", "use"]);
    assert_eq!(t.rows()[0], vec![Value::Int(5000), text("timer")]);
    let err = |words: &[&str]| apply(Data::Table(vectors()), words).unwrap_err().to_string();
    assert_eq!(err(&["select", "use", "use"]), "select: use named twice");
    assert_eq!(err(&["select", "name"]), "select: no column 'name' (columns: vector, use, count)");
    assert_eq!(err(&["select"]), "usage: select <column>...");
}

#[test]
fn text_is_a_table_of_lines_and_comes_back_as_text() {
    let lines = Data::from_text("b 2\na 10\nc 1");
    assert_eq!(apply(lines.clone(), &["sort", "line"]), Ok(Data::Text(strings(&["a 10", "b 2", "c 1"]))));
    assert_eq!(apply(lines.clone(), &["where", "line", "~", "1"]), Ok(Data::Text(strings(&["a 10", "c 1"]))));
    assert_eq!(apply(lines.clone(), &["where", "line", ">=", "b"]), Ok(Data::Text(strings(&["b 2", "c 1"]))));
    assert_eq!(apply(lines.clone(), &["select", "line"]), Ok(lines.clone()));
    assert_eq!(apply(lines, &["sort", "count"]).unwrap_err().to_string(), "sort: no column 'count' (columns: line)");
}

#[test]
fn a_pipeline_of_filters() {
    // irqs | where count > 0 | sort count desc | first 3
    let mut data = Data::Table(vectors());
    for words in [&["where", "count", ">", "0"][..], &["sort", "count", "desc"], &["first", "3"]] {
        data = apply(data, words).unwrap();
    }
    assert_eq!(
        data.render(),
        "  vector  use               count\n  0x20    timer             5000\n  \
         0x22    serial            7\n  0x23    spurious          7\n"
    );
    // Not a filter's name: not one.
    assert!(Filter::parse(&strings(&["cpus"])).is_none());
    assert!(Filter::parse(&[]).is_none());
}
