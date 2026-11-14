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
//!   A number column takes a number (decimal, or hex with `0x`; a `%` after
//!   a percentage is allowed); a text column compares text, byte by byte. A
//!   row without a value there (printed as `-`) never matches.
//! - `sort <column> [desc]`: the rows ordered by `column` (numbers as
//!   numbers), equal ones kept in their order, rows without a value last.
//! - `first <n>`: the first `n` lines or rows.
//! - `count`: how many lines or rows, as one line.
//! - `select <column>...`: those columns, in that order.
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
    fn show(&self, kind: Kind) -> String {
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

/// Records: rows of values under named columns. Every row has a value for
/// each column, of the kind the column holds (or [`Value::Absent`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table {
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
}

impl Table {
    /// A table with these columns and no rows.
    pub fn new(columns: &[Column]) -> Table {
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
    /// heading, or [`Column::width`]); the last column isn't padded on the
    /// right.
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
                .map(|mut row| match row.pop() {
                    Some(Value::Text(t)) => t,
                    _ => unreachable!("the line column holds text"),
                })
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

impl Op {
    const WORDS: [(&'static str, Op); 7] = [
        ("=", Op::Eq),
        ("!=", Op::Ne),
        ("<", Op::Lt),
        ("<=", Op::Le),
        (">", Op::Gt),
        (">=", Op::Ge),
        ("~", Op::Contains),
    ];

    fn parse(word: &str) -> Option<Op> {
        Op::WORDS.iter().find(|(w, _)| *w == word).map(|&(_, op)| op)
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
            Op::Contains => unreachable!("~ isn't an ordering"),
        }
    }
}

/// A filter: a command that only runs after a `|`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    Grep(String),
    Where { column: String, op: Op, value: String },
    Sort { column: String, desc: bool },
    First(usize),
    Count,
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

impl Filter {
    /// The filter `words` names (its name, then its arguments), or `None`
    /// if the first word isn't a filter's name.
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
            ("select", [_, ..]) => Ok(Filter::Select(args.to_vec())),
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
                    let columns = table.columns.clone();
                    table.rows.retain(|row| row.iter().zip(&columns).any(|(v, c)| v.show(c.kind).contains(text.as_str())));
                    Data::Table(table)
                }
            }),
            Filter::First(n) => Ok(match data {
                Data::Text(mut lines) => {
                    lines.truncate(*n);
                    Data::Text(lines)
                }
                Data::Table(mut table) => {
                    table.rows.truncate(*n);
                    Data::Table(table)
                }
            }),
            Filter::Count => {
                let n = match &data {
                    Data::Text(lines) => lines.len(),
                    Data::Table(table) => table.rows.len(),
                };
                Ok(Data::Text(alloc::vec![n.to_string()]))
            }
            Filter::Where { column, op, value } => {
                let (mut table, was_text) = data.into_table();
                let i = table.column("where", column)?;
                let kind = table.columns[i].kind;
                let wanted = if kind.holds_numbers() {
                    if *op == Op::Contains {
                        return Err(PipeError::ContainsNumbers(column.clone()));
                    }
                    let n = number(value, kind).ok_or_else(|| PipeError::NotANumber {
                        column: column.clone(),
                        value: value.clone(),
                    })?;
                    Value::Int(n)
                } else {
                    Value::Text(value.clone())
                };
                table.rows.retain(|row| matches(&row[i], *op, &wanted));
                Ok(Data::from_table(table, was_text))
            }
            Filter::Sort { column, desc } => {
                let (mut table, was_text) = data.into_table();
                let i = table.column("sort", column)?;
                table.rows.sort_by(|a, b| order(&a[i], &b[i], *desc));
                Ok(Data::from_table(table, was_text))
            }
            Filter::Select(names) => {
                let (table, was_text) = data.into_table();
                let mut picked: Vec<usize> = Vec::with_capacity(names.len());
                for name in names {
                    let i = table.column("select", name)?;
                    if picked.contains(&i) {
                        return Err(PipeError::Twice(name.clone()));
                    }
                    picked.push(i);
                }
                let columns = picked.iter().map(|&i| table.columns[i]).collect();
                let rows = table.rows.into_iter().map(|row| picked.iter().map(|&i| row[i].clone()).collect()).collect();
                Ok(Data::from_table(Table { columns, rows }, was_text))
            }
        }
    }
}

/// `word` as a number for a column holding `kind`: decimal (maybe
/// negative) or hex after `0x`; a percentage may end in `%`.
fn number(word: &str, kind: Kind) -> Option<i64> {
    let word = if kind == Kind::Percent { word.strip_suffix('%').unwrap_or(word) } else { word };
    match word.strip_prefix("0x") {
        Some(hex) => i64::from_str_radix(hex, 16).ok(),
        None => word.parse().ok(),
    }
}

/// Whether `value` compares with `wanted` (of its column's kind) by `op`.
fn matches(value: &Value, op: Op, wanted: &Value) -> bool {
    match (value, wanted) {
        (Value::Text(t), Value::Text(w)) if op == Op::Contains => t.contains(w.as_str()),
        (Value::Text(t), Value::Text(w)) => op.passes(t.as_str().cmp(w.as_str())),
        (Value::Int(n), Value::Int(w)) => op.passes(n.cmp(w)),
        _ => false,
    }
}

/// How `sort` orders two values of a column: numbers or text in order
/// (reversed with `desc`), a missing value after any other.
fn order(a: &Value, b: &Value, desc: bool) -> Ordering {
    let ordering = match (a, b) {
        (Value::Absent(_), Value::Absent(_)) => return Ordering::Equal,
        (Value::Absent(_), _) => return Ordering::Greater,
        (_, Value::Absent(_)) => return Ordering::Less,
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Text(a), Value::Text(b)) => a.cmp(b),
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
    /// `where` with `~` on a number column.
    ContainsNumbers(String),
    /// `select` naming a column twice.
    Twice(String),
}

impl fmt::Display for PipeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipeError::Usage { filter, usage } => {
                write!(f, "usage: {filter} {usage}")?;
                if *filter == "where" {
                    f.write_str(" (op: = != < <= > >= ~)")?;
                }
                Ok(())
            }
            PipeError::BadOp(op) => write!(f, "where: no operator '{op}' (one of = != < <= > >= ~)"),
            PipeError::BadCount(n) => write!(f, "first: '{n}' isn't a number of lines"),
            PipeError::NoColumn { filter, column, columns } => {
                write!(f, "{filter}: no column '{column}' (columns: {})", columns.join(", "))
            }
            PipeError::NotANumber { column, value } => {
                write!(f, "where: {column} holds numbers, and '{value}' isn't one")
            }
            PipeError::ContainsNumbers(column) => write!(f, "where: ~ is for text, and {column} holds numbers"),
            PipeError::Twice(column) => write!(f, "select: {column} named twice"),
        }
    }
}
