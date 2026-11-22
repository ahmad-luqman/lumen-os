//! A kernel thread's name: [`ThreadName`], 16 bytes inline like Linux's
//! `comm` (`TASK_COMM_LEN`), so it is `Copy`, never allocates, and a reader
//! with no lock (the crash reporter, through `sched::running_here`) can
//! always read it.

use core::fmt;

/// A thread's name: at most [`ThreadName::MAX_LEN`] bytes of UTF-8 kept
/// inline, the rest of its 16 bytes zero (Linux's `comm`: 15 bytes and a
/// NUL).
///
/// Made from any `&str` by the same rule ([`ThreadName::new`]): a longer
/// name is cut to its longest prefix of at most 15 bytes that ends on a
/// character boundary (Linux cuts at byte 15 whatever it splits; cutting
/// whole characters keeps [`ThreadName::as_str`] valid UTF-8), then each
/// control character (C0 with NUL, DEL, and C1 with NEL) and each Unicode
/// line or paragraph separator (U+2028, U+2029) becomes one `?`, as
/// `lumen_app_api::text` treats log text, so a name printed on a line
/// (`thread: <name> (id N)`) stays on that line.
///
/// The cut is for names from outside the kernel's source (a path given to
/// `linux /path`), made with [`ThreadName::new`] or
/// [`ThreadName::from_path`]. The kernel's own names come through
/// `From<&str>`, which refuses one that wouldn't fit (a debug assertion);
/// `tests/harness/test_thread_names.py` checks each name written as a
/// literal at a call that names a thread, before the kernel runs; and an
/// app's name is at most 11 bytes (`app:<name>`; `kernel/build.rs`).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadName([u8; 16]);

impl ThreadName {
    /// The most bytes a name keeps (Linux's `TASK_COMM_LEN - 1`).
    pub const MAX_LEN: usize = 15;

    /// `name` by the rule above.
    pub const fn new(name: &str) -> Self {
        let bytes = name.as_bytes();
        let len = name.floor_char_boundary(Self::MAX_LEN);
        let mut out = [0u8; 16];
        let (mut i, mut o) = (0, 0);
        while i < len {
            let b = bytes[i];
            // The byte written for the character at `i`, and the character's
            // length. `len` is a character boundary of a `&str`, so a lead
            // byte before it has its whole character before it too.
            let (byte, step) = if b < 0x20 || b == 0x7f {
                (b'?', 1)
            } else if b == 0xc2 && bytes[i + 1] <= 0x9f {
                (b'?', 2) // C1: U+0080..=U+009F (C2 80..=C2 9F), NEL among them
            } else if b == 0xe2 && bytes[i + 1] == 0x80 && (bytes[i + 2] == 0xa8 || bytes[i + 2] == 0xa9) {
                (b'?', 3) // U+2028, U+2029
            } else {
                (b, 1)
            };
            out[o] = byte;
            i += step;
            o += 1;
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
        let base = trimmed.rsplit_once('/').map_or(trimmed, |(_, base)| base);
        Self::new(if base.is_empty() { path } else { base })
    }

    /// The name's text.
    pub fn as_str(&self) -> &str {
        let len = self.0.iter().position(|&b| b == 0).unwrap_or(self.0.len());
        // `new` copies a prefix of a `&str` that ends on a character
        // boundary, replacing whole characters with ASCII, so this is
        // always UTF-8. Never a panic, though: the crash reporter prints
        // the name of the thread that crashed, whose memory a wild write
        // may have hit (then the valid prefix).
        self.0[..len].utf8_chunks().next().map_or("", |chunk| chunk.valid())
    }
}

/// The kernel's own names (every spawn function takes `impl
/// Into<ThreadName>`): one that would be cut or changed is a bug, caught by
/// a debug assertion (the kernel keeps them). Text from outside the kernel
/// (a path, a future `execve`'s name) goes through [`ThreadName::new`] or
/// [`ThreadName::from_path`], never `.into()`.
impl From<&str> for ThreadName {
    fn from(name: &str) -> Self {
        let made = Self::new(name);
        debug_assert!(made == name, "thread name {name:?} is over 15 bytes or holds a character `new` replaces");
        made
    }
}

/// Equal when the name reads as `other` exactly ([`ThreadName::as_str`]):
/// a literal that wouldn't fit, or that holds a character `new` replaces,
/// matches no thread (none is spawned with one: see the type's doc).
impl PartialEq<&str> for ThreadName {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
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
        // The C0 range's edges: NUL and 0x1f go, a space (0x20) stays.
        assert_eq!(ThreadName::new("a\0b").as_str(), "a?b");
        assert_eq!(ThreadName::new("a\x1fb").as_str(), "a?b");
        assert_eq!(ThreadName::new("my prog").as_str(), "my prog");
        // 0x7e, just below DEL, stays.
        assert_eq!(ThreadName::new("a\x7eb").as_str(), "a~b");
    }

    #[test]
    fn c1_controls_and_line_separators_become_one_question_mark_each() {
        // NEL, an 8-bit CSI, the C1 range's ends, U+2028 and U+2029.
        assert_eq!(ThreadName::new("a\u{85}b\u{9b}2J").as_str(), "a?b?2J");
        assert_eq!(ThreadName::new("\u{80}\u{9f}").as_str(), "??");
        assert_eq!(ThreadName::new("a\u{2028}b\u{2029}c").as_str(), "a?b?c");
        // Their neighbours stay: U+00A0 (after C1), U+00E9, U+2027 and
        // U+202A (around the separators), U+20AC (E2 82 AC); and the
        // characters sharing a byte with them: U+00C0 and U+00DF (lead
        // bytes C3, C3), U+20A8 (E2 82 A8: the separators' last byte after
        // another second byte), U+3028 (E3 80 A8: the same two bytes after
        // another lead).
        for kept in ["\u{a0}", "\u{e9}", "\u{2027}", "\u{202a}", "\u{20ac}", "\u{c0}", "\u{df}", "\u{20a8}", "\u{3028}"]
        {
            assert_eq!(ThreadName::new(kept).as_str(), kept, "{kept:?}");
        }
        // Cut first (at byte 15, on a character), then replaced: the name
        // keeps what fitted.
        assert_eq!(ThreadName::new("abcdefghijkl\u{2028}xyz").as_str(), "abcdefghijkl?");
        assert_eq!(ThreadName::new("abcdefghijklm\u{2028}").as_str(), "abcdefghijklm");
        assert_eq!(ThreadName::new("abcdefghijklm\u{85}z").as_str(), "abcdefghijklm?");
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
        // A literal is compared as it is: one that wouldn't fit, or holds
        // a character `new` replaces, matches no name.
        assert!(ThreadName::new("sleep-mutex-waiter") != "sleep-mutex-waiter");
        assert!(ThreadName::new("sleep-mutex-waiter") == "sleep-mutex-wai");
        assert!(ThreadName::new("a\nb") != "a\nb");
        assert!(ThreadName::new("a\nb") == "a?b");
    }

    #[test]
    fn from_a_str_takes_a_name_that_fits_as_it_is() {
        assert_eq!(ThreadName::from("exactly-15-byte"), "exactly-15-byte");
        assert_eq!(ThreadName::from("idle"), ThreadName::new("idle"));
    }

    #[test]
    #[should_panic(expected = "over 15 bytes")]
    fn from_a_str_refuses_a_name_that_would_be_cut() {
        let _ = ThreadName::from("exactly-16-bytes");
    }

    #[test]
    #[should_panic(expected = "holds a character")]
    fn from_a_str_refuses_a_name_that_would_be_changed() {
        let _ = ThreadName::from("a\nb");
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
