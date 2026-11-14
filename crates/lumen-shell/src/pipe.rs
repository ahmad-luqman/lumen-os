//! Pipelines: what a command hands the next one, and the filters.
//!
//! A command's output is [`Data`]: lines of text, or a [`Table`] of records
//! (named columns, each holding numbers or text). A [`Filter`] takes data
//! and gives data; what reaches the end of the pipeline is printed
//! ([`Data::render`]).
//!
//! - `grep <text>`: the lines, or rows, containing `text` (a row does when
//!   one of its values, as printed, does).
//! - `where <column> <op> <value>`: the rows whose `column` compares so
//!   with `value` (`=`, `!=`, `<`, `<=`, `>`, `>=`; `~` contains, for text).
//!   A number column takes a number: decimal, or hex after `0x` (which a
//!   hex column requires: `vector = 20` is refused, not read as `0x14`); a
//!   `%` after a percentage column's is allowed. A text column compares
//!   text byte by byte, unless the value is a decimal number (and the
//!   operator isn't `~`, which always looks for text): then rows whose text
//!   is one compare as numbers and the others don't match (`seq 12 | where
//!   line > 5`). A row without a value there (printed as `-` or `stalled`)
//!   never matches, not even `!=`.
//! - `sort <column> [desc]`: the rows ordered by `column`, numbers as
//!   numbers (in a text column, lines that are decimal numbers by value,
//!   before the rest, as `where` reads them), equal ones kept in their
//!   order, rows without a value last (`desc` reverses the rest).
//! - `first <n>`: the first `n` lines or rows.
//! - `count`: how many lines or rows, as one line.
//! - `select <column>...`: those columns, in that order (each once).
//!
//! Text going into `where`, `sort` or `select` is a table with one text
//! column, `line`, and comes out as text again.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::fmt;
use unicode_width::UnicodeWidthStr;

/// What a column holds, and how it is printed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A number, in decimal.
    Int,
    /// A number, in hex with at least two digits (`0x21`).
    Hex,
    /// A percentage (`37%`).
    Percent,
    /// Text.
    Text,
}

impl Kind {
    fn holds_numbers(self) -> bool {
        self != Kind::Text
    }
}

/// Which side of its column a value is printed against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Left,
    Right,
}

/// A table's column: its name (also its heading), what it holds, and how it
/// is laid out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Column {
    pub name: &'static str,
    pub kind: Kind,
    pub align: Align,
    /// The narrowest it is printed (wider if its name or a value is).
    pub width: usize,
}

impl Column {
    pub const fn new(name: &'static str, kind: Kind, align: Align, width: usize) -> Column {
        Column { name, kind, align, width }
    }
}

/// A value in a table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// For a column holding numbers.
    Int(i64),
    /// For a text column.
    Text(String),
    /// No value, printed as the text given (`-`, `stalled`): `where` never
    /// matches it and `sort` puts it last.
    Absent(&'static str),
}

impl Value {
    /// The value as printed in a column holding `kind`.
    pub fn show(&self, kind: Kind) -> String {
        match (self, kind) {
            (Value::Int(n), Kind::Hex) => format!("{n:#04x}"),
            (Value::Int(n), Kind::Percent) => format!("{n}%"),
            (Value::Int(n), _) => n.to_string(),
            (Value::Text(t), _) => t.clone(),
            (Value::Absent(s), _) => (*s).to_owned(),
        }
    }

    fn fits(&self, kind: Kind) -> bool {
        match self {
            Value::Int(_) => kind.holds_numbers(),
            Value::Text(_) => !kind.holds_numbers(),
            Value::Absent(_) => true,
        }
    }
}

/// Records: rows of values under named columns. Column names are unique,
/// and every row has a value for each column, of the kind the column holds
/// (or [`Value::Absent`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
}

impl Table {
    /// A table with these columns and no rows.
    ///
    /// # Panics
    /// If two columns have the same name (`where` and `select` couldn't
    /// tell them apart): the code making the table is wrong.
    pub fn new(columns: &[Column]) -> Table {
        for (i, c) in columns.iter().enumerate() {
            assert!(columns[..i].iter().all(|d| d.name != c.name), "two columns called {}", c.name);
        }
        Table { columns: columns.to_vec(), rows: Vec::new() }
    }

    /// Add a row.
    ///
    /// # Panics
    /// If it doesn't have one value per column, each fitting its column
    /// (a number in a column of text, say): the code making the table is
    /// wrong.
    pub fn push(&mut self, row: Vec<Value>) {
        assert_eq!(row.len(), self.columns.len(), "a row needs one value per column");
        for (value, column) in row.iter().zip(&self.columns) {
            assert!(value.fits(column.kind), "{value:?} doesn't fit column {} ({:?})", column.name, column.kind);
        }
        self.rows.push(row);
    }

    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }

    /// The headings, then a line per row: each line indented by two spaces,
    /// the columns two spaces apart, each as wide as its widest value (or
    /// heading, or [`Column::width`]); a left-aligned last column isn't
    /// padded on the right.
    pub fn render(&self, out: &mut String) {
        let cells: Vec<Vec<String>> =
            self.rows.iter().map(|row| row.iter().zip(&self.columns).map(|(v, c)| v.show(c.kind)).collect()).collect();
        let widths: Vec<usize> = self
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| cells.iter().map(|row| row[i].width()).fold(c.width.max(c.name.width()), usize::max))
            .collect();
        self.line(out, &widths, self.columns.iter().map(|c| c.name));
        for row in &cells {
            self.line(out, &widths, row.iter().map(String::as_str));
        }
    }

    fn line<'a>(&self, out: &mut String, widths: &[usize], cells: impl Iterator<Item = &'a str>) {
        out.push_str("  ");
        let last = self.columns.len().saturating_sub(1);
        for (i, ((cell, column), &width)) in cells.zip(&self.columns).zip(widths).enumerate() {
            if i > 0 {
                out.push_str("  ");
            }
            let pad = width.saturating_sub(cell.width());
            match column.align {
                Align::Right => {
                    out.extend(core::iter::repeat_n(' ', pad));
                    out.push_str(cell);
                }
                Align::Left => {
                    out.push_str(cell);
                    if i < last {
                        out.extend(core::iter::repeat_n(' ', pad));
                    }
                }
            }
        }
        out.push('\n');
    }

    /// The index of the column called `name`.
    fn column(&self, filter: &'static str, name: &str) -> Result<usize, PipeError> {
        self.columns.iter().position(|c| c.name == name).ok_or_else(|| PipeError::NoColumn {
            filter,
            column: name.to_owned(),
            columns: self.columns.iter().map(|c| c.name).collect(),
        })
    }
}

/// The column text becomes in a table.
const LINE: Column = Column::new("line", Kind::Text, Align::Left, 0);

/// A command's output, or a filter's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Data {
    /// Lines of text (without their newlines).
    Text(Vec<String>),
    Table(Table),
}

impl Data {
    /// Text as its lines: each line of `text` (one not ending in a newline
    /// too).
    pub fn from_text(text: &str) -> Data {
        Data::Text(text.lines().map(str::to_owned).collect())
    }

    /// The data as printed: each line and a newline, or the table.
    pub fn render(&self) -> String {
        let mut out = String::new();
        match self {
            Data::Text(lines) => {
                for line in lines {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Data::Table(table) => table.render(&mut out),
        }
        out
    }

    /// How many lines or rows.
    pub fn len(&self) -> usize {
        match self {
            Data::Text(lines) => lines.len(),
            Data::Table(table) => table.rows.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Keep the first `n` lines or rows.
    fn truncate(&mut self, n: usize) {
        match self {
            Data::Text(lines) => lines.truncate(n),
            Data::Table(table) => table.rows.truncate(n),
        }
    }

    /// The data as a table, and whether it was text (a table of one
    /// column, `line`).
    fn into_table(self) -> (Table, bool) {
        match self {
            Data::Table(table) => (table, false),
            Data::Text(lines) => {
                let rows = lines.into_iter().map(|l| alloc::vec![Value::Text(l)]).collect();
                (Table { columns: alloc::vec![LINE], rows }, true)
            }
        }
    }

    /// `table` as data: text again if it was (and still is one text column).
    fn from_table(table: Table, was_text: bool) -> Data {
        if was_text && table.columns == [LINE] {
            let lines = table
                .rows
                .into_iter()
                .map(|mut row| row.pop().map_or_else(String::new, |v| v.show(LINE.kind)))
                .collect();
            Data::Text(lines)
        } else {
            Data::Table(table)
        }
    }
}

/// How `where` compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// Contains (text only).
    Contains,
}

/// `where`'s operators as written, in [`Op::ALL`]'s order.
pub const OPS: [&str; 7] = ["=", "!=", "<", "<=", ">", ">=", "~"];

impl Op {
    const ALL: [Op; 7] = [Op::Eq, Op::Ne, Op::Lt, Op::Le, Op::Gt, Op::Ge, Op::Contains];

    fn parse(word: &str) -> Option<Op> {
        OPS.iter().position(|w| *w == word).map(|i| Op::ALL[i])
    }

    /// Whether `ordering` (a value against the one asked for) passes.
    fn passes(self, ordering: Ordering) -> bool {
        match self {
            Op::Eq => ordering.is_eq(),
            Op::Ne => ordering.is_ne(),
            Op::Lt => ordering.is_lt(),
            Op::Le => ordering.is_le(),
            Op::Gt => ordering.is_gt(),
            Op::Ge => ordering.is_ge(),
            Op::Contains => false, // not an ordering: `Wanted::Contains`
        }
    }
}

/// A filter: a command that only runs after a `|`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    Grep(String),
    Where {
        column: String,
        op: Op,
        value: String,
    },
    Sort {
        column: String,
        desc: bool,
    },
    First(usize),
    Count,
    /// Distinct column names.
    Select(Vec<String>),
}

/// Each filter's name and its arguments as help shows them.
pub const FILTERS: [(&str, &str); 6] = [
    ("grep", "<text>"),
    ("where", "<column> <op> <value>"),
    ("sort", "<column> [desc]"),
    ("first", "<n>"),
    ("count", ""),
    ("select", "<column>..."),
];

/// Whether `name` is a filter's.
pub fn is_filter(name: &str) -> bool {
    FILTERS.iter().any(|(f, _)| *f == name)
}

impl Filter {
    /// The filter `words` names (its name, then its arguments), or `None`
    /// if the first word isn't a filter's name. Everything that can be
    /// checked without the data is checked here.
    pub fn parse(words: &[String]) -> Option<Result<Filter, PipeError>> {
        let (name, args) = words.split_first()?;
        let (name, usage) = *FILTERS.iter().find(|(n, _)| n == name)?;
        let usage = PipeError::Usage { filter: name, usage };
        let strs: Vec<&str> = args.iter().map(String::as_str).collect();
        Some(match (name, strs.as_slice()) {
            ("grep", [text]) => Ok(Filter::Grep((*text).to_owned())),
            ("where", [column, op, value]) => match Op::parse(op) {
                Some(op) => Ok(Filter::Where { column: (*column).to_owned(), op, value: (*value).to_owned() }),
                None => Err(PipeError::BadOp((*op).to_owned())),
            },
            ("sort", [column]) => Ok(Filter::Sort { column: (*column).to_owned(), desc: false }),
            ("sort", [column, "desc"]) => Ok(Filter::Sort { column: (*column).to_owned(), desc: true }),
            ("first", [n]) => n.parse().map(Filter::First).map_err(|_| PipeError::BadCount((*n).to_owned())),
            ("count", []) => Ok(Filter::Count),
            ("select", [_, ..]) => match args.iter().enumerate().find(|(i, a)| args[..*i].contains(a)) {
                Some((_, twice)) => Err(PipeError::Twice(twice.clone())),
                None => Ok(Filter::Select(args.to_vec())),
            },
            _ => Err(usage),
        })
    }

    /// The filter's name.
    pub fn name(&self) -> &'static str {
        match self {
            Filter::Grep(_) => "grep",
            Filter::Where { .. } => "where",
            Filter::Sort { .. } => "sort",
            Filter::First(_) => "first",
            Filter::Count => "count",
            Filter::Select(_) => "select",
        }
    }

    /// What the filter makes of `data`.
    pub fn apply(&self, data: Data) -> Result<Data, PipeError> {
        match self {
            Filter::Grep(text) => Ok(match data {
                Data::Text(mut lines) => {
                    lines.retain(|l| l.contains(text.as_str()));
                    Data::Text(lines)
                }
                Data::Table(mut table) => {
                    let columns = &table.columns;
                    table
                        .rows
                        .retain(|row| row.iter().zip(columns).any(|(v, c)| v.show(c.kind).contains(text.as_str())));
                    Data::Table(table)
                }
            }),
            Filter::First(n) => {
                let mut data = data;
                data.truncate(*n);
                Ok(data)
            }
            Filter::Count => Ok(Data::Text(alloc::vec![data.len().to_string()])),
            Filter::Where { column, op, value } => {
                let (mut table, was_text) = data.into_table();
                let i = table.column(self.name(), column)?;
                let wanted = wanted(column, table.columns[i].kind, *op, value)?;
                table.rows.retain(|row| matches(&row[i], *op, &wanted));
                Ok(Data::from_table(table, was_text))
            }
            Filter::Sort { column, desc } => {
                let (mut table, was_text) = data.into_table();
                let i = table.column(self.name(), column)?;
                table.rows.sort_by(|a, b| order(&a[i], &b[i], *desc));
                Ok(Data::from_table(table, was_text))
            }
            Filter::Select(names) => {
                let (table, was_text) = data.into_table();
                let picked = names.iter().map(|n| table.column(self.name(), n)).collect::<Result<Vec<_>, _>>()?;
                let columns = picked.iter().map(|&i| table.columns[i]).collect();
                let rows = table.rows.into_iter().map(|row| picked.iter().map(|&i| row[i].clone()).collect()).collect();
                Ok(Data::from_table(Table { columns, rows }, was_text))
            }
        }
    }
}

/// What `where` compares a column's values with.
enum Wanted {
    /// A number: with a number column's values, or a text column's that
    /// are decimal numbers.
    Number(i64),
    /// Text, byte by byte.
    Text(String),
    /// Text to look for (`~`).
    Contains(String),
}

/// What `where <column> <op> <value>` compares with, for a column holding
/// `kind`; or why it can't.
fn wanted(column: &str, kind: Kind, op: Op, value: &str) -> Result<Wanted, PipeError> {
    if op == Op::Contains {
        return if kind.holds_numbers() {
            Err(PipeError::ContainsNumbers(column.to_owned()))
        } else {
            Ok(Wanted::Contains(value.to_owned()))
        };
    }
    if !kind.holds_numbers() {
        return Ok(decimal(value).map_or_else(|| Wanted::Text(value.to_owned()), Wanted::Number));
    }
    let digits = if kind == Kind::Percent { value.strip_suffix('%').unwrap_or(value) } else { value };
    let n = match (digits.strip_prefix("0x"), kind) {
        (Some(hex), _) if !hex.starts_with(['+', '-']) => i64::from_str_radix(hex, 16).ok(),
        (Some(_), _) => None,
        (None, Kind::Hex) if decimal(digits).is_some_and(|n| n >= 0) => {
            return Err(PipeError::HexNeeded(column.to_owned()));
        }
        (None, Kind::Hex) => None,
        (None, _) => decimal(digits),
    };
    n.map(Wanted::Number).ok_or_else(|| PipeError::NotANumber { column: column.to_owned(), value: value.to_owned() })
}

/// `word` as a decimal number (maybe signed: `-5`, `+5`).
fn decimal(word: &str) -> Option<i64> {
    word.parse().ok()
}

/// Whether `value` compares with `wanted` by `op`.
fn matches(value: &Value, op: Op, wanted: &Wanted) -> bool {
    match (value, wanted) {
        (Value::Text(t), Wanted::Contains(w)) => t.contains(w.as_str()),
        (Value::Text(t), Wanted::Text(w)) => op.passes(t.as_str().cmp(w.as_str())),
        (Value::Text(t), Wanted::Number(w)) => decimal(t).is_some_and(|n| op.passes(n.cmp(w))),
        (Value::Int(n), Wanted::Number(w)) => op.passes(n.cmp(w)),
        _ => false,
    }
}

/// How `sort` orders two values of a column: numbers, or text (decimal
/// numbers among it by value, first), in order (reversed with `desc`), a
/// missing value after any other.
fn order(a: &Value, b: &Value, desc: bool) -> Ordering {
    let ordering = match (a, b) {
        (Value::Absent(_), Value::Absent(_)) => return Ordering::Equal,
        (Value::Absent(_), _) => return Ordering::Greater,
        (_, Value::Absent(_)) => return Ordering::Less,
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        // Decimal numbers by value, before other text (as `where` reads
        // them).
        (Value::Text(a), Value::Text(b)) => match (decimal(a), decimal(b)) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.cmp(b),
        },
        _ => Ordering::Equal, // one column holds one kind
    };
    if desc { ordering.reverse() } else { ordering }
}

/// Why a filter couldn't run, as the shell prints it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PipeError {
    /// The wrong arguments.
    Usage { filter: &'static str, usage: &'static str },
    /// `where` with an operator it doesn't know.
    BadOp(String),
    /// `first` with something other than a count.
    BadCount(String),
    /// A column the data doesn't have.
    NoColumn { filter: &'static str, column: String, columns: Vec<&'static str> },
    /// `where` comparing a number column with something not a number.
    NotANumber { column: String, value: String },
    /// `where` comparing a hex column (named) with a number not written in
    /// hex.
    HexNeeded(String),
    /// `where` with `~` on a number column.
    ContainsNumbers(String),
    /// `select` naming a column twice.
    Twice(String),
}

impl fmt::Display for PipeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipeError::Usage { filter, usage } => {
                write!(f, "usage: {filter}")?;
                if !usage.is_empty() {
                    write!(f, " {usage}")?;
                }
                if *filter == "where" {
                    write!(f, " (op: {})", OPS.join(" "))?;
                }
                Ok(())
            }
            PipeError::BadOp(op) => write!(f, "where: no operator '{op}' (one of {})", OPS.join(" ")),
            PipeError::BadCount(n) => write!(f, "first: '{n}' isn't a number of lines"),
            PipeError::NoColumn { filter, column, columns } => {
                write!(f, "{filter}: no column '{column}' (columns: {})", columns.join(", "))
            }
            PipeError::NotANumber { column, value } => {
                write!(f, "where: {column} holds numbers, and '{value}' isn't one")
            }
            PipeError::HexNeeded(column) => write!(f, "where: {column} is hex: write it with 0x (e.g. 0x20)"),
            PipeError::ContainsNumbers(column) => write!(f, "where: ~ is for text, and {column} holds numbers"),
            PipeError::Twice(column) => write!(f, "select: {column} named twice"),
        }
    }
}

/// `text` in pieces to write a little at a time: each piece is whole lines,
/// up to and including the first that brings it to `size` bytes or more.
/// So every piece but the last holds at least `size` bytes, and less than
/// `size` plus its last line (a line longer than `size` ends the piece it
/// is in); the last may be shorter, and ends where `text` does.
pub fn chunks(text: &str, size: usize) -> Chunks<'_> {
    Chunks { rest: text, size }
}

/// [`chunks`]' pieces.
#[derive(Clone, Debug)]
pub struct Chunks<'a> {
    rest: &'a str,
    size: usize,
}

impl<'a> Iterator for Chunks<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.rest.is_empty() {
            return None;
        }
        let mut end = 0;
        for line in self.rest.split_inclusive('\n') {
            end += line.len();
            if end >= self.size {
                break;
            }
        }
        let (piece, rest) = self.rest.split_at(end);
        self.rest = rest;
        Some(piece)
    }
}
