//! Text an app hands the kernel: decoded within its cap, and made safe for
//! the log. A log line's text is cut at [`LOG_BYTES`] on a char boundary and
//! each control character shown as U+FFFD, so a newline can't start a line
//! of its own (one a test might match on, `INFO  boot: boot complete`).
//! `panic`'s message, titles and `Failed` answers go through the same.

use crate::Error;
use crate::limits::LOG_BYTES;
use alloc::string::String;

/// `bytes` as text: [`Error::BadArgument`] past `cap` bytes (checked before
/// decoding) or if it isn't UTF-8.
pub fn decode(bytes: &[u8], cap: usize) -> Result<&str, Error> {
    if bytes.len() > cap {
        return Err(Error::BadArgument);
    }
    core::str::from_utf8(bytes).map_err(|_| Error::BadArgument)
}

/// `text` as the log shows it: at most [`LOG_BYTES`] bytes, cut on a char
/// boundary, every control character (`\n`, `\r`, ESC, ...) U+FFFD.
pub fn log_line(text: &str) -> String {
    let mut end = text.len().min(LOG_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].chars().map(|c| if c.is_control() { char::REPLACEMENT_CHARACTER } else { c }).collect()
}

/// [`log_line`] for bytes that may not be UTF-8 (`panic`'s message: the app
/// ends however bad it is), invalid sequences shown as U+FFFD.
pub fn log_line_lossy(bytes: &[u8]) -> String {
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
        assert_eq!(log_line("short"), "short");
        let long = "a".repeat(LOG_BYTES + 10);
        assert_eq!(log_line(&long).len(), LOG_BYTES);
        // A 2-byte char straddling the cut is dropped whole.
        let straddling = alloc::format!("{}é", "a".repeat(LOG_BYTES - 1));
        assert_eq!(log_line(&straddling), "a".repeat(LOG_BYTES - 1));
        // One that ends exactly at the cut stays.
        let ending = alloc::format!("{}é", "a".repeat(LOG_BYTES - 2));
        assert_eq!(log_line(&ending), ending);
    }

    #[test]
    fn control_characters_cant_forge_a_line() {
        assert_eq!(
            log_line("ok\n[    0.1] INFO  boot: boot complete"),
            "ok\u{fffd}[    0.1] INFO  boot: boot complete"
        );
        assert_eq!(log_line("a\rb\x1b[2Jc\td"), "a\u{fffd}b\u{fffd}[2Jc\u{fffd}d");
        assert_eq!(log_line_lossy(b"bad \xff\nline"), "bad \u{fffd}\u{fffd}line");
    }
}
