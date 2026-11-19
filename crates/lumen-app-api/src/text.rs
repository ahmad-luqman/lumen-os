//! Text an app hands the kernel: decoded within its cap, and made safe for
//! the log. A log line's text is cut at [`LOG_BYTES`] on a char boundary and
//! each character that could end a line or drive a terminal shown as
//! U+FFFD, so an app can't start a line of its own (one a test might match
//! on, `INFO  boot: boot complete`): every control character, and the
//! Unicode line and paragraph separators (U+2028, U+2029), which Python's
//! `str.splitlines` (the harness's) splits on too. The result is a
//! [`LogText`], the only way such text reaches a log line: `panic`'s
//! message, a trap's, an unknown manifest grant, and later titles and
//! `Failed` answers.

use crate::Error;
use crate::limits::LOG_BYTES;
use alloc::string::String;
use core::fmt;
use core::ops::Deref;

/// The most bytes a [`LogText`] cut at `cap` holds: each character kept
/// is at most as long as it was, but a 1-byte control character becomes
/// U+FFFD's 3 bytes.
pub const fn log_text_bound(cap: usize) -> usize {
    3 * cap
}

/// The longest grant name an unknown-grant refusal shows.
pub const NAME_BYTES: usize = 32;

/// Text made safe for a log line ([`log_line`]): no character in it can
/// end the line or drive a terminal, and it is at most
/// [`log_text_bound`]`(cap)` bytes for the cap it was cut at.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LogText(String);

impl LogText {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for LogText {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LogText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `bytes` as text: [`Error::BadArgument`] past `cap` bytes (checked before
/// decoding) or if it isn't UTF-8.
pub fn decode(bytes: &[u8], cap: usize) -> Result<&str, Error> {
    if bytes.len() > cap {
        return Err(Error::BadArgument);
    }
    core::str::from_utf8(bytes).map_err(|_| Error::BadArgument)
}

/// Whether `c` could end a log line or drive a terminal.
fn unsafe_in_a_line(c: char) -> bool {
    c.is_control() || c == '\u{2028}' || c == '\u{2029}'
}

/// `text` as the log shows it: at most [`LOG_BYTES`] of its bytes, cut on
/// a char boundary, every control character (`\n`, `\r`, ESC, U+0085, ...)
/// and line or paragraph separator U+FFFD.
pub fn log_line(text: &str) -> LogText {
    cut(text, LOG_BYTES)
}

/// [`log_line`] cut at `cap` bytes instead.
pub fn cut(text: &str, cap: usize) -> LogText {
    let mut end = text.len().min(cap);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    LogText(text[..end].chars().map(|c| if unsafe_in_a_line(c) { char::REPLACEMENT_CHARACTER } else { c }).collect())
}

/// [`log_line`] for bytes that may not be UTF-8 (`panic`'s message: the app
/// ends however bad it is), invalid sequences shown as U+FFFD.
pub fn log_line_lossy(bytes: &[u8]) -> LogText {
    log_line(&String::from_utf8_lossy(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_checks_the_cap_then_utf8() {
        assert_eq!(decode(b"hi", 2), Ok("hi"));
        assert_eq!(decode(b"hi!", 2), Err(Error::BadArgument));
        assert_eq!(decode(b"\xff", 4), Err(Error::BadArgument));
        assert_eq!(decode(b"", 0), Ok(""));
    }

    #[test]
    fn a_log_line_is_cut_on_a_char_boundary() {
        assert_eq!(log_line("short").as_str(), "short");
        let long = "a".repeat(LOG_BYTES + 10);
        assert_eq!(log_line(&long).len(), LOG_BYTES);
        // A 2-byte char straddling the cut is dropped whole.
        let straddling = alloc::format!("{}é", "a".repeat(LOG_BYTES - 1));
        assert_eq!(log_line(&straddling).as_str(), "a".repeat(LOG_BYTES - 1));
        // One that ends exactly at the cut stays.
        let ending = alloc::format!("{}é", "a".repeat(LOG_BYTES - 2));
        assert_eq!(log_line(&ending).as_str(), ending);
        assert_eq!(cut("abcdef", 3).as_str(), "abc");
    }

    #[test]
    fn control_characters_cant_forge_a_line() {
        assert_eq!(
            log_line("ok\n[    0.1] INFO  boot: boot complete").as_str(),
            "ok\u{fffd}[    0.1] INFO  boot: boot complete"
        );
        assert_eq!(log_line("a\rb\x1b[2Jc\td").as_str(), "a\u{fffd}b\u{fffd}[2Jc\u{fffd}d");
        assert_eq!(log_line_lossy(b"bad \xff\nline").as_str(), "bad \u{fffd}\u{fffd}line");
    }

    #[test]
    fn unicode_line_breaks_cant_forge_a_line() {
        // Every character Python's `str.splitlines` splits on.
        for c in ['\n', '\r', '\x0b', '\x0c', '\x1c', '\x1d', '\x1e', '\u{85}', '\u{2028}', '\u{2029}'] {
            let line = log_line(&alloc::format!("a{c}INFO  boot: boot complete"));
            assert_eq!(line.as_str(), "a\u{fffd}INFO  boot: boot complete", "{:?}", c);
        }
    }

    #[test]
    fn the_output_stays_within_its_bound() {
        // All 1-byte control characters: each becomes 3 bytes.
        let worst = "\x07".repeat(LOG_BYTES + 5);
        let line = log_line(&worst);
        assert_eq!(line.len(), log_text_bound(LOG_BYTES));
        assert_eq!(cut(&worst, NAME_BYTES).len(), log_text_bound(NAME_BYTES));
    }
}
