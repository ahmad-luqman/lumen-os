//! A kernel thread's name: [`ThreadName`], 16 bytes inline like Linux's
//! `comm` (`TASK_COMM_LEN`), so it is `Copy`, never allocates, and a reader
//! with no lock (the crash reporter, the profiler) can always read it.

use core::fmt;

/// A thread's name: at most [`ThreadName::MAX_LEN`] bytes of UTF-8 kept
/// inline, the rest of its 16 bytes zero (Linux's `comm`: 15 bytes and a
/// NUL).
///
/// Made from any `&str` by the same rule ([`ThreadName::new`]): a longer
/// name is cut to its longest prefix of at most 15 bytes that ends on a
/// character boundary (Linux cuts at byte 15 whatever it splits; cutting
/// whole characters keeps [`ThreadName::as_str`] valid UTF-8), and each
/// ASCII control character (a newline, an escape) becomes `?`, so a name
/// printed on a line (`thread: <name> (id N)`) stays on that line.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadName([u8; 16]);

impl ThreadName {
    /// The most bytes a name keeps (Linux's `TASK_COMM_LEN - 1`).
    pub const MAX_LEN: usize = 15;

    /// `name` by the rule above.
    pub const fn new(name: &str) -> Self {
        let bytes = name.as_bytes();
        let mut len = if bytes.len() > Self::MAX_LEN { Self::MAX_LEN } else { bytes.len() };
        // Back up to a character boundary: a UTF-8 continuation byte is
        // 0b10xx_xxxx (`is_char_boundary` isn't `const` on the pinned Rust).
        while len < bytes.len() && bytes[len] & 0xc0 == 0x80 {
            len -= 1;
        }
        let mut out = [0u8; 16];
        let mut i = 0;
        while i < len {
            let b = bytes[i];
            out[i] = if b < 0x20 || b == 0x7f { b'?' } else { b };
            i += 1;
        }
        ThreadName(out)
    }

    /// The name a program read from `path` runs under: the path's last
    /// component (`/usr/local/bin/hello` gives `hello`), as Linux names a
    /// process after the file `execve` was given (not the file a symlink
    /// leads to). Trailing slashes are skipped; a path with no component
    /// (`/`, empty) gives the whole path.
    pub fn from_path(path: &str) -> Self {
        let trimmed = path.trim_end_matches('/');
        let base = trimmed.rsplit('/').next().unwrap_or(trimmed);
        Self::new(if base.is_empty() { path } else { base })
    }

    /// The name's text.
    pub fn as_str(&self) -> &str {
        let len = self.0.iter().position(|&b| b == 0).unwrap_or(self.0.len());
        let bytes = &self.0[..len];
        // `new` copies a prefix of a `&str` that ends on a character
        // boundary, replacing only ASCII bytes with ASCII, so this is
        // always UTF-8. Never a panic, though: the crash reporter prints
        // the name of the thread that crashed, whose memory a wild write
        // may have hit (then the valid prefix).
        match core::str::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => core::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or_default(),
        }
    }
}

impl From<&str> for ThreadName {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

/// Compared as a name made from `other` (cut by the same rule): a thread
/// spawned as `"sleep-mutex-waiter"` is found by that literal.
impl PartialEq<str> for ThreadName {
    fn eq(&self, other: &str) -> bool {
        *self == Self::new(other)
    }
}

impl PartialEq<&str> for ThreadName {
    fn eq(&self, other: &&str) -> bool {
        *self == Self::new(other)
    }
}

/// Pads like a `&str` (`{:<16}` lines up `top`'s columns).
impl fmt::Display for ThreadName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

impl fmt::Debug for ThreadName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::ThreadName;
    use std::format;

    #[test]
    fn short_names_are_kept_whole() {
        assert_eq!(ThreadName::new("idle").as_str(), "idle");
        assert_eq!(ThreadName::new("").as_str(), "");
        assert_eq!(ThreadName::from("shell-vt2"), "shell-vt2");
    }

    #[test]
    fn names_past_15_bytes_keep_15() {
        let fifteen = "abcdefghijklmno";
        assert_eq!(ThreadName::new(fifteen).as_str(), fifteen);
        assert_eq!(ThreadName::new("abcdefghijklmnop").as_str(), fifteen);
        assert_eq!(ThreadName::new("lumen-ktest-thread-name").as_str(), "lumen-ktest-thr");
        // The 16th byte is always the NUL Linux's comm keeps.
        assert_eq!(ThreadName::new("abcdefghijklmnopqrstuvwxyz").0[15], 0);
    }

    #[test]
    fn a_cut_never_splits_a_character() {
        // 14 ASCII bytes then a 2-byte 'é' at bytes 14..16: the cut at 15
        // would split it, so it goes whole.
        let name = "abcdefghijklmné";
        assert_eq!(name.len(), 16);
        assert_eq!(ThreadName::new(name).as_str(), "abcdefghijklmn");
        // None of its bytes are kept (not a stray lead byte `as_str` would
        // skip): the name is the same one.
        assert_eq!(ThreadName::new(name), ThreadName::new("abcdefghijklmn"));
        // A 4-byte character straddling byte 15 (bytes 12..16) goes too.
        assert_eq!(ThreadName::new("abcdefghijkl🦀x").as_str(), "abcdefghijkl");
        assert_eq!(ThreadName::new("abcdefghijkl🦀x"), ThreadName::new("abcdefghijkl"));
        // One ending exactly at byte 15 stays.
        assert_eq!(ThreadName::new("abcdefghijk🦀xyz").as_str(), "abcdefghijk🦀");
        // Under 15 bytes, multibyte names are kept whole.
        assert_eq!(ThreadName::new("née").as_str(), "née");
    }

    #[test]
    fn control_characters_become_question_marks() {
        assert_eq!(ThreadName::new("a\nb\x1b[2Jc\x7f").as_str(), "a?b?[2Jc?");
        assert_eq!(ThreadName::new("tab\there").as_str(), "tab?here");
    }

    #[test]
    fn from_path_takes_the_last_component() {
        assert_eq!(ThreadName::from_path("/usr/local/bin/hello"), "hello");
        assert_eq!(ThreadName::from_path("/a/b/foo"), "foo");
        assert_eq!(ThreadName::from_path("foo"), "foo");
        assert_eq!(ThreadName::from_path("/bin/busybox/"), "busybox");
        assert_eq!(ThreadName::from_path("/bin//sh//"), "sh");
        assert_eq!(ThreadName::from_path("/"), "/");
        assert_eq!(ThreadName::from_path("///"), "///");
        assert_eq!(ThreadName::from_path(""), "");
        // Cut as any name: the symlink scenario's interpreter path.
        assert_eq!(ThreadName::from_path("/lib/libc.musl-x86_64.so.1"), "libc.musl-x86_6");
    }

    #[test]
    fn display_pads_like_a_str_and_debug_quotes() {
        let n = ThreadName::new("idle");
        assert_eq!(format!("[{n:<8}]"), "[idle    ]");
        assert_eq!(format!("[{n:>6}]"), "[  idle]");
        assert_eq!(format!("{n}"), "idle");
        assert_eq!(format!("{n:?}"), "\"idle\"");
    }

    #[test]
    fn equal_names_compare_equal() {
        assert_eq!(ThreadName::new("abcdefghijklmnopq"), ThreadName::new("abcdefghijklmnoZZ"));
        assert_ne!(ThreadName::new("idle"), ThreadName::new("idl"));
        assert!(ThreadName::new("idle") == "idle");
        assert!(ThreadName::new("idle") != "idl");
        assert!(ThreadName::new("idl") != "idle");
        // A literal is cut as the name was.
        assert!(ThreadName::new("sleep-mutex-waiter") == "sleep-mutex-waiter");
        assert!(ThreadName::new("sleep-mutex-waiter") == "sleep-mutex-wai");
        assert!(ThreadName::new("sleep-mutex-wai") != "sleep-mutex-wa");
    }

    #[test]
    fn a_corrupted_name_reads_as_its_valid_prefix() {
        // Only a wild write makes one (`new` never does): read, not a panic.
        let mut n = ThreadName::new("abcdef");
        n.0[3] = 0xff;
        assert_eq!(n.as_str(), "abc");
        n.0[0] = 0x80;
        assert_eq!(n.as_str(), "");
    }
}
