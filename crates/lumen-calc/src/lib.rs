//! The calculator app's logic (#207; `apps/calculator` is its window):
//! keys in, the display's text out. Host-tested, `no_std` with `alloc`.
//!
//! - **Left to right**, as a pocket calculator: `2 + 3 × 4 =` is 20 (each
//!   operator applies to what was shown when it was pressed).
//! - **Exact decimal fixed point**: numbers are `i128` counts of
//!   10^-[`DECIMALS`], so `0.1 + 0.2` is `0.3`; a product or quotient is
//!   rounded half away from zero to [`DECIMALS`] places (`2 ÷ 3` is
//!   `0.666666667`).
//! - **Limits**: an entry takes at most [`DIGITS`] digits, [`DECIMALS`] of
//!   them after the point (more are ignored); a result whose integer part
//!   needs more than [`DIGITS`] digits, or a division by zero, shows
//!   [`ERROR`] until the next digit or `C`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

use alloc::string::String;

/// Digits an entry takes, and a result's integer part may have.
pub const DIGITS: usize = 15;
/// Places after the point.
pub const DECIMALS: u32 = 9;
/// What the display shows after a division by zero or an overflow.
pub const ERROR: &str = "Error";

const SCALE: i128 = 10i128.pow(DECIMALS);
/// The first magnitude a result can't show: 10^DIGITS.
const LIMIT: i128 = 10i128.pow(DIGITS as u32) * SCALE;

/// An operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Add,
    Sub,
    Mul,
    Div,
}

/// A calculator key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// `0` to `9`.
    Digit(u8),
    Point,
    Op(Op),
    Equals,
    /// `C`: everything cleared.
    Clear,
    /// The entry's last character removed.
    Backspace,
}

impl Key {
    /// The key a typed character stands for (`0`-`9`, `.`, `+ - * /`, `x`,
    /// `=`, `c`/`C`; Enter and Backspace are the toolkit's keys, not
    /// characters).
    pub fn from_char(c: char) -> Option<Key> {
        Some(match c {
            '0'..='9' => Key::Digit(c as u8 - b'0'),
            '.' | ',' => Key::Point,
            '+' => Key::Op(Op::Add),
            '-' => Key::Op(Op::Sub),
            '*' | 'x' | 'X' | '×' => Key::Op(Op::Mul),
            '/' | '÷' => Key::Op(Op::Div),
            '=' => Key::Equals,
            'c' | 'C' => Key::Clear,
            _ => return None,
        })
    }
}

/// A number: a count of 10^-DECIMALS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Dec(i128);

/// `n / d` rounded half away from zero (`d > 0`).
fn div_round(n: i128, d: i128) -> i128 {
    let (q, r) = (n / d, n % d);
    if 2 * r.abs() >= d { q + n.signum() } else { q }
}

impl Dec {
    /// `a op b`, or `None` (a division by zero, or past [`LIMIT`]).
    fn apply(a: Dec, op: Op, b: Dec) -> Option<Dec> {
        let raw = match op {
            Op::Add => a.0.checked_add(b.0)?,
            Op::Sub => a.0.checked_sub(b.0)?,
            Op::Mul => div_round(a.0.checked_mul(b.0)?, SCALE),
            Op::Div => {
                if b.0 == 0 {
                    return None;
                }
                let n = a.0.checked_mul(SCALE)?;
                let d = b.0;
                // Rounded with the divisor's sign moved to the dividend.
                if d < 0 { div_round(-n, -d) } else { div_round(n, d) }
            }
        };
        (raw.abs() < LIMIT).then_some(Dec(raw))
    }

    /// As the display shows it: the integer part, then the decimals that
    /// aren't trailing zeros.
    fn text(self) -> String {
        use core::fmt::Write;
        let mut out = String::new();
        if self.0 < 0 {
            out.push('-');
        }
        let abs = self.0.unsigned_abs();
        let (int, frac) = (abs / SCALE as u128, abs % SCALE as u128);
        let _ = write!(out, "{int}");
        if frac != 0 {
            let mut digits = alloc::format!("{frac:0width$}", width = DECIMALS as usize);
            while digits.ends_with('0') {
                digits.pop();
            }
            out.push('.');
            out.push_str(&digits);
        }
        out
    }

    /// The number an entry's text (digits and at most one point) is.
    fn parse(entry: &str) -> Dec {
        let (int, frac) = entry.split_once('.').unwrap_or((entry, ""));
        let digits = |s: &str| s.bytes().fold(0i128, |n, b| n * 10 + i128::from(b - b'0'));
        let places = frac.len() as u32;
        Dec(digits(int) * SCALE + digits(frac) * 10i128.pow(DECIMALS - places))
    }
}

/// The calculator's state.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Calc {
    /// What is being typed (digits and maybe a point), if anything.
    entry: Option<String>,
    /// The number the pending operator applies to (the last result).
    acc: Option<Dec>,
    /// The operator waiting for its second number.
    op: Option<Op>,
    /// A division by zero or an overflow is shown.
    error: bool,
}

impl Calc {
    pub fn new() -> Calc {
        Calc::default()
    }

    /// What the display shows: the entry being typed, or else the last
    /// result (`0` at the start), or [`ERROR`].
    pub fn display(&self) -> String {
        if self.error {
            return String::from(ERROR);
        }
        match (&self.entry, self.acc) {
            (Some(e), _) => e.clone(),
            (None, Some(acc)) => acc.text(),
            (None, None) => String::from("0"),
        }
    }

    /// Press `key`.
    pub fn press(&mut self, key: Key) {
        if self.error && !matches!(key, Key::Digit(_) | Key::Point | Key::Clear) {
            return;
        }
        match key {
            Key::Digit(d) => {
                self.start_entry();
                let e = self.entry.get_or_insert_with(String::new);
                let digits = e.bytes().filter(u8::is_ascii_digit).count();
                let decimals = e.split_once('.').map_or(0, |(_, f)| f.len());
                if d > 9 || digits >= DIGITS || (e.contains('.') && decimals >= DECIMALS as usize) {
                    return;
                }
                if e == "0" {
                    e.clear();
                }
                e.push(char::from(b'0' + d));
            }
            Key::Point => {
                self.start_entry();
                let e = self.entry.get_or_insert_with(String::new);
                if e.is_empty() {
                    e.push('0');
                }
                if !e.contains('.') {
                    e.push('.');
                }
            }
            Key::Op(op) => {
                if let Some(entry) = self.entry.take() {
                    let n = Dec::parse(&entry);
                    let value = match (self.acc, self.op) {
                        (Some(acc), Some(pending)) => Dec::apply(acc, pending, n),
                        _ => Some(n),
                    };
                    if !self.set(value) {
                        return;
                    }
                } else if self.acc.is_none() {
                    self.acc = Some(Dec(0));
                }
                self.op = Some(op);
            }
            Key::Equals => match (self.acc, self.op, self.entry.take()) {
                // (An operator is only ever pending with a number before it.)
                (acc, Some(op), Some(entry)) => {
                    let acc = acc.unwrap_or(Dec(0));
                    if self.set(Dec::apply(acc, op, Dec::parse(&entry))) {
                        self.op = None;
                    }
                }
                // No operator: the entry becomes the result as it is.
                (_, None, Some(entry)) => {
                    self.acc = Some(Dec::parse(&entry));
                }
                // No second number yet: it waits for one.
                (_, _, None) => {}
            },
            Key::Clear => *self = Calc::new(),
            Key::Backspace => {
                if let Some(e) = &mut self.entry {
                    e.pop();
                    if e.is_empty() {
                        self.entry = None;
                    }
                }
            }
        }
    }

    /// A digit or a point after a result with no operator pending (`=`, or
    /// an error) starts a new calculation.
    fn start_entry(&mut self) {
        if self.error || (self.entry.is_none() && self.op.is_none()) {
            *self = Calc::new();
        }
    }

    /// The result of an operation: shown, or the error (cleared but for
    /// the error flag). Whether it was a number.
    fn set(&mut self, value: Option<Dec>) -> bool {
        match value {
            Some(v) => {
                self.acc = Some(v);
                true
            }
            None => {
                *self = Calc { error: true, ..Calc::new() };
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The display after typing `keys` (characters, `<` for Backspace).
    fn calc(keys: &str) -> String {
        let mut c = Calc::new();
        for ch in keys.chars().filter(|c| !c.is_whitespace()) {
            let key = if ch == '<' { Key::Backspace } else { Key::from_char(ch).unwrap() };
            c.press(key);
        }
        c.display()
    }

    #[test]
    fn it_computes_left_to_right() {
        assert_eq!(calc(""), "0");
        assert_eq!(calc("2 + 3 ="), "5");
        assert_eq!(calc("2 + 3 * 4 ="), "20", "left to right, not precedence");
        assert_eq!(calc("2 + 3 *"), "5", "an operator shows what it applies to");
        assert_eq!(calc("10 - 4 - 3 ="), "3");
        assert_eq!(calc("2 - 5 ="), "-3");
        assert_eq!(calc("7 / 2 ="), "3.5");
        assert_eq!(calc("- 3 ="), "-3", "an operator first applies to 0");
    }

    #[test]
    fn decimals_are_exact_and_rounded_half_away_from_zero() {
        assert_eq!(calc("0.1 + 0.2 ="), "0.3");
        assert_eq!(calc("1 / 3 ="), "0.333333333");
        assert_eq!(calc("2 / 3 ="), "0.666666667");
        assert_eq!(calc("0 - 2 = / 3 ="), "-0.666666667");
        assert_eq!(calc("0.5 * 0.5 ="), "0.25");
        assert_eq!(calc("0.000000001 * 0.5 ="), "0.000000001", "half away from zero");
        assert_eq!(calc("1.50"), "1.50", "an entry shows as typed");
        assert_eq!(calc("1.50 ="), "1.5", "a result as a number");
        assert_eq!(calc(". 5 + 1 ="), "1.5");
    }

    #[test]
    fn dividing_by_zero_is_an_error_until_a_digit_or_clear() {
        assert_eq!(calc("5 / 0 ="), ERROR);
        assert_eq!(calc("5 / 0 = + ="), ERROR, "operators and = do nothing");
        assert_eq!(calc("5 / 0 = 7"), "7", "a digit starts again");
        assert_eq!(calc("5 / 0 = + 3 ="), "3");
        assert_eq!(calc("5 / 0 = c"), "0");
        assert_eq!(calc("5 / 0 +"), ERROR, "at the next operator too");
        assert_eq!(calc("5 / 0 + 1"), "1");
    }

    #[test]
    fn long_inputs_stop_at_the_digit_limits() {
        assert_eq!(calc("1234567890123456789"), "123456789012345");
        assert_eq!(calc("0.12345678901"), "0.123456789");
        assert_eq!(calc("000"), "0");
        assert_eq!(calc("1..2"), "1.2");
        // A result past 15 integer digits overflows.
        assert_eq!(calc("999999999999999 + 1 ="), ERROR);
        assert_eq!(calc("999999999999999 * 999999999999999 ="), ERROR);
        assert_eq!(calc("999999999999999 + 0 ="), "999999999999999");
        assert_eq!(calc("0 - 999999999999999 - 1 ="), ERROR);
    }

    #[test]
    fn results_chain_and_a_digit_after_equals_starts_again() {
        assert_eq!(calc("2 + 3 = + 1 ="), "6");
        assert_eq!(calc("2 + 3 = 7"), "7");
        assert_eq!(calc("2 + 3 = 7 + 1 ="), "8");
        assert_eq!(calc("2 + * 3 ="), "6", "a second operator replaces the first");
        assert_eq!(calc("2 + ="), "2", "= without a second number waits for it");
        assert_eq!(calc("2 + = 3 ="), "5");
    }

    #[test]
    fn backspace_and_clear_edit_the_entry() {
        assert_eq!(calc("123<"), "12");
        assert_eq!(calc("1<"), "0");
        assert_eq!(calc("1.5<<"), "1");
        assert_eq!(calc("1.5<<<"), "0");
        assert_eq!(calc("2 + 3 = <"), "5", "a result isn't edited");
        assert_eq!(calc("2 + 3 c"), "0");
        assert_eq!(Key::from_char('q'), None);
        assert_eq!(Key::from_char('×'), Some(Key::Op(Op::Mul)));
    }
}
