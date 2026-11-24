//! Bounded byte parsing and argument rewriting for `#!` scripts.
//!
//! No path lookup or interpreter loading is performed here. Pass the first
//! 256 bytes, or the whole file if shorter, as `prepare_binprm`'s `kernel_read`
//! does. A short read before EOF would defeat the interpreter truncation check.

use alloc::{collections::TryReserveError, vec::Vec};
use core::fmt;

use crate::{Errno, args::normalize_argv};

/// Linux's executable header window (`include/uapi/linux/binfmts.h`).
pub const BINPRM_BUF_SIZE: usize = 256;

/// Maximum interpreter substitutions in a successful exec (five).
///
/// Linux [`exec_binprm`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/exec.c)
/// tries handlers at depths 0 through 5 and returns `-ELOOP` at depth 6.
/// Five scripts followed by a binary fit; six scripts followed by a binary do not.
pub const MAX_NESTING: usize = 5;

/// A nonempty interpreter and at most one argument borrowed from the header.
///
/// Neither field contains NUL/newline or leading space/tab. An argument may
/// be empty or retain trailing space/tab on the NUL/EOF path. Quotes are literal.
/// A CRLF's `\r` stays in the last field (the argument, or the interpreter if
/// there is none).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shebang<'a> {
    interpreter: &'a [u8],
    argument: Option<&'a [u8]>,
}

impl<'a> Shebang<'a> {
    pub const fn interpreter(&self) -> &'a [u8] {
        self.interpreter
    }
    pub const fn argument(&self) -> Option<&'a [u8]> {
        self.argument
    }
}

/// Why a script's interpreter cannot be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShebangError {
    /// No interpreter before a newline, or an all-space/tab full window: ENOEXEC.
    EmptyInterpreter,
    /// The interpreter name has no terminator in the header window: ENOEXEC.
    TruncatedInterpreter,
    /// An empty C string on the NUL/EOF path: EACCES. Linux `open_exec("")`
    /// resolves to cwd; `fs/namei.c::may_open` rejects a directory with MAY_EXEC.
    EmptyInterpreterPath,
}

impl ShebangError {
    pub const fn errno(&self) -> Errno {
        match self {
            Self::EmptyInterpreter | Self::TruncatedInterpreter => Errno::NoExec,
            Self::EmptyInterpreterPath => Errno::Access,
        }
    }
}

impl fmt::Display for ShebangError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyInterpreter => "empty shebang interpreter",
            Self::TruncatedInterpreter => "truncated shebang interpreter",
            Self::EmptyInterpreterPath => "empty interpreter path resolves to a directory",
        })
    }
}

fn space_tab(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}
fn trim_start(bytes: &[u8]) -> &[u8] {
    &bytes[bytes.iter().take_while(|&&b| space_tab(b)).count()..]
}
fn trim_end(bytes: &[u8]) -> &[u8] {
    &bytes[..bytes.iter().rposition(|&b| !space_tab(b)).map_or(0, |i| i + 1)]
}

/// Parse Linux `fs/binfmt_script.c::load_script`'s header, or `None` without `#!`.
///
/// Only the first 256 bytes are used, with EOF zero-padding. A newline before
/// the first NUL ends the trim region; otherwise Linux uses byte 255, requiring
/// a terminated interpreter but permitting a clipped optional argument.
/// Spaces/tabs immediately before that endpoint are trimmed. On the NUL/EOF
/// path the intervening NUL normally prevents trimming earlier whitespace;
/// the optional argument runs to NUL and can be empty. Byte 255 is always
/// reserved for termination when no newline was found.
///
/// Unlike the issue's simplified ENOEXEC rule, an empty NUL/EOF interpreter
/// path is [`ShebangError::EmptyInterpreterPath`] (EACCES).
pub fn parse(head: &[u8]) -> Result<Option<Shebang<'_>>, ShebangError> {
    let head = &head[..head.len().min(BINPRM_BUF_SIZE)];
    if !head.starts_with(b"#!") {
        return Ok(None);
    }
    let mut padded = [0; BINPRM_BUF_SIZE];
    padded[..head.len()].copy_from_slice(head);
    let end = match padded.iter().position(|&b| matches!(b, 0 | b'\n')) {
        Some(i) if padded[i] == b'\n' => i,
        _ => {
            let name = trim_start(&padded[2..]);
            if name.is_empty() {
                return Err(ShebangError::EmptyInterpreter);
            }
            if !name.iter().any(|&b| b == 0 || space_tab(b)) {
                return Err(ShebangError::TruncatedInterpreter);
            }
            BINPRM_BUF_SIZE - 1
        }
    };
    let line = trim_end(&padded[2..end]);
    let name = trim_start(line);
    if name.is_empty() {
        return Err(ShebangError::EmptyInterpreter);
    }
    let start = 2 + line.len() - name.len();
    let split = name.iter().position(|&b| b == 0 || space_tab(b)).unwrap_or(name.len());
    if split == 0 {
        return Err(ShebangError::EmptyInterpreterPath);
    }
    let argument = if name.get(split).is_some_and(|&b| space_tab(b)) {
        let rest = trim_start(&name[split..]);
        if rest.is_empty() {
            None
        } else {
            let arg_start = start + name.len() - rest.len();
            let len = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
            Some(&head[arg_start..arg_start + len])
        }
    } else {
        None
    };
    Ok(Some(Shebang { interpreter: &head[start..start + split], argument }))
}

/// Borrow `[interpreter, argument?, script_path, original[1..]...]`.
///
/// Empty original argv is normalized first; its synthetic first argument is
/// replaced too. Only the pointer vector is allocated, fallibly (map allocation
/// failure to ENOMEM). Charge the strings with [`crate::args::ArgBudget::rewrite`].
#[must_use = "allocation failure must be handled"]
pub fn argv<'a>(
    sh: &Shebang<'a>,
    script_path: &'a [u8],
    original: &'a [&'a [u8]],
) -> Result<Vec<&'a [u8]>, TryReserveError> {
    let original = normalize_argv(original);
    let mut result = Vec::new();
    // A real slice's length cannot approach usize::MAX; normalization also
    // reserves three slots when an empty original has an optional argument.
    result.try_reserve_exact(original.len() + 2)?;
    result.push(sh.interpreter);
    if let Some(argument) = sh.argument {
        result.push(argument);
    }
    result.push(script_path);
    result.extend_from_slice(&original[1..]);
    Ok(result)
}

/// Handling the next interpreter would exceed Linux's exec loop depth: ELOOP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NestingLimit;

impl NestingLimit {
    pub const fn errno(&self) -> Errno {
        Errno::Loop
    }
}

impl fmt::Display for NestingLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("interpreter nesting limit exceeded")
    }
}

/// Interpreter substitutions followed during one exec; cannot be copied.
#[must_use]
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Nesting {
    depth: usize,
}

impl Nesting {
    /// Start an exec with no interpreter substitutions.
    pub const fn new() -> Self {
        Self { depth: 0 }
    }

    /// Call once after a script's handler succeeded (parsed, argv rewritten,
    /// interpreter opened), before handling the interpreter's file.
    ///
    /// Linux checks `depth > 5` at the top of `exec_binprm`'s loop. Errors from
    /// the sixth script's handler therefore precede ELOOP; its interpreter's
    /// file is never handled. Rejection leaves the counter unchanged.
    pub fn follow_interpreter(&mut self) -> Result<(), NestingLimit> {
        if self.depth == MAX_NESTING {
            return Err(NestingLimit);
        }
        self.depth += 1;
        Ok(())
    }

    pub const fn depth(&self) -> usize {
        self.depth
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn assert_parse(head: &[u8], interpreter: &[u8], argument: Option<&[u8]>) {
        assert_eq!(parse(head), Ok(Some(Shebang { interpreter, argument })));
    }

    #[test]
    fn parse_requires_initial_magic() {
        for head in [b"".as_slice(), b"#", b"!#sh", b" #!sh", b"\n#!sh", b"\x7fELF"] {
            assert_eq!(parse(head), Ok(None));
        }
    }

    #[test]
    fn parse_interpreter_and_single_optional_argument() {
        assert_parse(b"#!/bin/sh\nignored", b"/bin/sh", None);
        assert_parse(b"#!   /usr/bin/env  python -O  -u  \nbody", b"/usr/bin/env", Some(b"python -O  -u"));
        assert_parse(b"#!relative \xff\xfe\n", b"relative", Some(b"\xff\xfe"));
    }

    #[test]
    fn parse_tabs_and_only_space_tab_separators() {
        assert_parse(b"#!\t \t/bin/sh\t \t-a\t\t b \t\n", b"/bin/sh", Some(b"-a\t\t b"));
        assert_parse(b"#!\x0binterp\x0c \x0barg\x0c\n", b"\x0binterp\x0c", Some(b"\x0barg\x0c"));
    }

    #[test]
    fn parse_crlf_preserves_carriage_return() {
        assert_parse(b"#!/bin/sh -e\r\n", b"/bin/sh", Some(b"-e\r"));
        assert_parse(b"#!/bin/sh\r\n", b"/bin/sh\r", None);
        assert_parse(b"#!/bin/sh \r\n", b"/bin/sh", Some(b"\r"));
    }

    #[test]
    fn parse_empty_argument_depends_on_terminator() {
        assert_parse(b"#!sh \t\n", b"sh", None);
        assert_parse(b"#!sh \t", b"sh", Some(b""));
        assert_parse(b"#!sh \t\0", b"sh", Some(b""));
        assert_parse(b"#!/bin/sh -e ", b"/bin/sh", Some(b"-e "));
        assert_parse(b"#!/bin/sh ", b"/bin/sh", Some(b""));
        // No shell quoting: these are two literal quote bytes, not an empty argument.
        assert_parse(b"#!sh \"\"\n", b"sh", Some(b"\"\""));
    }

    #[test]
    fn parse_nul_ends_logical_line() {
        assert_parse(b"#!sh\0ignored -x\n", b"sh", None);
        assert_parse(b"#!sh -a\0ignored\n", b"sh", Some(b"-a"));
        assert_parse(b"#!sh -a \t\0ignored\n", b"sh", Some(b"-a \t"));
        assert_parse(b"#!sh \0ignored\n", b"sh", Some(b""));
        let mut head = [b'x'; BINPRM_BUF_SIZE];
        head[..4].copy_from_slice(b"#!sh");
        head[4] = 0;
        assert_parse(&head, b"sh", None);
    }

    #[test]
    fn parse_empty_interpreter_is_enoexec() {
        for head in [b"#!\nsh".as_slice(), b"#! \t\n"] {
            assert_eq!(parse(head), Err(ShebangError::EmptyInterpreter));
        }
        let mut head = [b' '; BINPRM_BUF_SIZE];
        head[..2].copy_from_slice(b"#!");
        assert_eq!(parse(&head), Err(ShebangError::EmptyInterpreter));
    }

    #[test]
    fn parse_empty_nul_or_eof_interpreter_is_eacces() {
        for head in [b"#!".as_slice(), b"#! \t", b"#!\0sh", b"#! \t\0sh"] {
            assert_eq!(parse(head), Err(ShebangError::EmptyInterpreterPath));
            assert_eq!(parse(head).unwrap_err().errno(), Errno::Access);
        }
    }

    #[test]
    fn parse_short_eof_and_256_byte_newline_boundary() {
        assert_parse(b"#!/bin/sh", b"/bin/sh", None);
        let mut head = vec![b'x'; BINPRM_BUF_SIZE - 1];
        head[..2].copy_from_slice(b"#!");
        assert_parse(&head, &head[2..], None); // Short read: implicit EOF/NUL.
        head.push(b'\n'); // A newline at byte 255 fits in the header.
        assert_parse(&head, &head[2..255], None);
        head.extend_from_slice(b"ignored");
        assert_parse(&head, &head[2..255], None);
        head[255] = b'x';
        head[256] = b'\n'; // A newline at byte 256 cannot rescue a cut path.
        assert_eq!(parse(&head), Err(ShebangError::TruncatedInterpreter));
        head[255] = 0;
        assert_parse(&head, &head[2..255], None);
    }

    #[test]
    fn parse_unterminated_window_requires_complete_interpreter() {
        let mut head = [b'x'; BINPRM_BUF_SIZE];
        head[..2].copy_from_slice(b"#!");
        assert_eq!(parse(&head), Err(ShebangError::TruncatedInterpreter));
        for terminator in *b" \t" {
            head[255] = terminator;
            assert_parse(&head, &head[2..255], None);
        }
        head[2..255].fill(b' ');
        head[255] = b'x';
        assert_eq!(parse(&head), Err(ShebangError::TruncatedInterpreter));
    }

    #[test]
    fn parse_unterminated_window_clips_optional_argument() {
        let mut head = [b'a'; 300];
        head[..5].copy_from_slice(b"#!sh ");
        assert_parse(&head, b"sh", Some(&head[5..255]));
        head[255] = b'\n';
        assert_parse(&head, b"sh", Some(&head[5..255]));
        head[255] = b'a';
        head[254] = b'\t';
        assert_parse(&head, b"sh", Some(&head[5..254]));
        head[5..256].fill(b' ');
        assert_parse(&head, b"sh", None);
    }

    #[test]
    fn argv_with_and_without_optional_argument() {
        let original: &[&[u8]] = &[b"discarded", b"one", b"", b"two\xff"];
        let sh = parse(b"#!/bin/interp\n").unwrap().unwrap();
        assert_eq!(
            argv(&sh, b"script path", original).unwrap(),
            [b"/bin/interp".as_slice(), b"script path", b"one", b"", b"two\xff"]
        );
        let sh = parse(b"#!/bin/interp -a -b\n").unwrap().unwrap();
        assert_eq!(
            argv(&sh, b"script path", original).unwrap(),
            [b"/bin/interp".as_slice(), b"-a -b", b"script path", b"one", b"", b"two\xff"]
        );
        let sh = parse(b"#!/bin/interp ").unwrap().unwrap();
        assert_eq!(
            argv(&sh, b"script path", original).unwrap(),
            [b"/bin/interp".as_slice(), b"", b"script path", b"one", b"", b"two\xff"]
        );
    }

    #[test]
    fn argv_with_empty_original() {
        let plain = parse(b"#!/bin/sh\n").unwrap().unwrap();
        let option = parse(b"#!/bin/sh -e\n").unwrap().unwrap();
        let empty_option = parse(b"#!/bin/sh ").unwrap().unwrap();
        assert_eq!(argv(&plain, b"script", &[]).unwrap(), [b"/bin/sh".as_slice(), b"script"]);
        assert_eq!(argv(&plain, b"script", &[b""]).unwrap(), [b"/bin/sh".as_slice(), b"script"]);
        assert_eq!(argv(&option, b"script", &[]).unwrap(), [b"/bin/sh".as_slice(), b"-e", b"script"]);
        assert_eq!(argv(&option, b"script", &[b""]).unwrap(), [b"/bin/sh".as_slice(), b"-e", b"script"]);
        assert_eq!(argv(&empty_option, b"script", &[]).unwrap(), [b"/bin/sh".as_slice(), b"", b"script"]);
    }

    #[test]
    fn nesting_accepts_limit_and_rejects_next() {
        assert_eq!(MAX_NESTING, 5);
        let mut nesting = Nesting::new();
        assert_eq!(nesting, Nesting::default());
        for depth in 1..=MAX_NESTING {
            assert_eq!(nesting.follow_interpreter(), Ok(()));
            assert_eq!(nesting.depth(), depth);
        }
        for _ in 0..2 {
            assert_eq!(nesting.follow_interpreter(), Err(NestingLimit));
            assert_eq!(nesting.depth(), MAX_NESTING);
        }
    }

    #[test]
    fn nesting_matches_exec_binprm_error_order() {
        fn run(headers: &[&[u8]; 7], use_counter: bool, open_error: Option<usize>) -> (Errno, usize) {
            let mut nesting = Nesting::new();
            for (depth, header) in headers.iter().enumerate() {
                if !use_counter && depth > 5 {
                    return (Errno::Loop, depth);
                }
                if let Err(error) = parse(header) {
                    return (error.errno(), depth + 1);
                }
                // A successful handler includes opening the interpreter.
                if open_error == Some(depth) {
                    return (Errno::Access, depth + 1);
                }
                if use_counter && let Err(error) = nesting.follow_interpreter() {
                    return (error.errno(), depth + 1);
                }
            }
            panic!("bounded exec model failed to terminate");
        }
        let valid: &[u8] = b"#!/bin/sh\n";
        let mut headers = [valid; 7];
        headers[5] = b"#!\n";
        assert_eq!(run(&headers, true, None), (Errno::NoExec, 6));
        assert_eq!(run(&headers, true, None), run(&headers, false, None));
        headers[5] = valid;
        assert_eq!(run(&headers, true, None), (Errno::Loop, 6));
        assert_eq!(run(&headers, true, None), run(&headers, false, None));
        assert_eq!(run(&headers, true, Some(5)), (Errno::Access, 6));
        assert_eq!(run(&headers, true, Some(5)), run(&headers, false, Some(5)));
    }

    #[test]
    fn errors_expose_errno() {
        assert_eq!(ShebangError::EmptyInterpreter.errno(), Errno::NoExec);
        assert_eq!(ShebangError::TruncatedInterpreter.errno(), Errno::NoExec);
        assert_eq!(ShebangError::EmptyInterpreterPath.errno(), Errno::Access);
        assert_eq!(NestingLimit.errno(), Errno::Loop);
    }

    type Fields = (Vec<u8>, Option<Vec<u8>>);
    fn owned(sh: Shebang<'_>) -> Fields {
        (sh.interpreter().to_vec(), sh.argument().map(<[u8]>::to_vec))
    }

    // Test-only transliteration of load_script. C helpers search through the
    // inclusive last byte; strnchr stops at NUL. No production parsing helpers.
    fn linux_parse(head: &[u8]) -> Result<Option<Fields>, ShebangError> {
        let mut buf = [0; 256];
        let len = head.len().min(256);
        buf[..len].copy_from_slice(&head[..len]);
        if &buf[..2] != b"#!" {
            return Ok(None);
        }
        let non_spacetab = |buf: &[u8], first, last| (first..=last).find(|&i| !matches!(buf[i], b' ' | b'\t'));
        let terminator = |buf: &[u8], first, last| (first..=last).find(|&i| matches!(buf[i], 0 | b' ' | b'\t'));
        let newline = buf.iter().take_while(|&&b| b != 0).position(|&b| b == b'\n');
        let mut end = if let Some(i) = newline {
            i
        } else {
            let first = non_spacetab(&buf, 2, 255).ok_or(ShebangError::EmptyInterpreter)?;
            terminator(&buf, first, 255).ok_or(ShebangError::TruncatedInterpreter)?;
            255
        };
        for _ in 0..256 {
            if !matches!(buf[end - 1], b' ' | b'\t') {
                break;
            }
            end -= 1;
        }
        assert!(end >= 2 && !matches!(buf[end - 1], b' ' | b'\t'));
        let name = non_spacetab(&buf, 2, end).filter(|&i| i != end).ok_or(ShebangError::EmptyInterpreter)?;
        let sep = terminator(&buf, name, end);
        let arg = sep.filter(|&i| buf[i] != 0).and_then(|i| non_spacetab(&buf, i, end));
        buf[end] = 0;
        if let (Some(_), Some(sep)) = (arg, sep) {
            buf[sep] = 0;
        }
        let cstr = |start| buf[start..].iter().copied().take_while(|&b| b != 0).collect::<Vec<_>>();
        let interpreter = cstr(name);
        if interpreter.is_empty() {
            return Err(ShebangError::EmptyInterpreterPath);
        }
        Ok(Some((interpreter, arg.map(cstr))))
    }

    fn check_properties(head: &[u8]) -> bool {
        let result = parse(head);
        assert_eq!(result.map(|sh| sh.map(owned)), linux_parse(head), "header {head:?}");
        assert_eq!(result, parse(&head[..head.len().min(BINPRM_BUF_SIZE)]));
        if let Ok(Some(sh)) = result {
            let separator = |b: &u8| matches!(b, b' ' | b'\t');
            let newline = head.iter().take(256).take_while(|&&b| b != 0).any(|&b| b == b'\n');
            assert!(head.starts_with(b"#!"));
            assert!(!sh.interpreter().is_empty());
            assert!(!sh.interpreter().iter().any(separator));
            for value in core::iter::once(sh.interpreter()).chain(sh.argument()) {
                assert!(!value.iter().any(|b| matches!(b, 0 | b'\n')));
                assert!(!value.first().is_some_and(separator));
                if newline {
                    assert!(!value.last().is_some_and(separator));
                }
            }
            true
        } else {
            false
        }
    }

    #[test]
    fn parse_matches_linux_for_all_short_headers() {
        let alphabet = [0, b' ', b'\t', b'\n', b'\r', b'x'];
        let mut checked = 0;
        // Includes every 7-byte `#!` header over the syntax alphabet
        // (alphabet.len().pow(5)), and all other lengths from 2 through 9.
        for len in 2..=9 {
            for pattern in 0..alphabet.len().pow((len - 2) as u32) {
                let mut head = *b"#!xxxxxxx";
                let mut value = pattern;
                for byte in &mut head[2..len] {
                    *byte = alphabet[value % alphabet.len()];
                    value /= alphabet.len();
                }
                check_properties(&head[..len]);
                checked += 1;
            }
        }
        assert_eq!(checked, 335_923);
    }

    #[test]
    fn parse_properties_for_arbitrary_inputs() {
        let alphabet = [0, b' ', b'\t', b'\n', b'\r', b'x'];
        let mut successes = 0;
        let mut seed = 0x343a_2024_9876_1234u64;
        for len in 0..=300 {
            for sample in 0..64 {
                let mut head = vec![0; len];
                for byte in &mut head {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    *byte = if sample % 2 == 0 { seed as u8 } else { alphabet[seed as usize % alphabet.len()] };
                }
                if sample % 4 != 0 && len >= 2 {
                    head[..2].copy_from_slice(b"#!");
                }
                successes += usize::from(check_properties(&head));
            }
        }
        assert!(successes > 1000, "exercise successful parses as well as rejection paths");
    }
}
