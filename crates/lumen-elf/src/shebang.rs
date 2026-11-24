//! Bounded byte parsing and argument rewriting for `#!` scripts.
//!
//! No path lookup or interpreter loading is performed here. Pass the first
//! 256 bytes, or the whole file if shorter, as `prepare_binprm`'s `kernel_read`
//! does. A short read before EOF would defeat the interpreter truncation check.

use core::fmt;

use crate::Errno;

/// Linux's executable header window (`include/uapi/linux/binfmts.h`).
pub const BINPRM_BUF_SIZE: usize = 256;

/// Maximum interpreter substitutions in a successful exec (five).
///
/// Linux [`exec_binprm`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/exec.c)
/// tries handlers at depths 0 through 5 and returns `-ELOOP` at depth 6.
/// Five scripts followed by a binary fit; six scripts followed by a binary do not.
pub const MAX_NESTING: usize = 5;

/// An interpreter and at most one argument borrowed from the header.
/// The interpreter may be empty only on the NUL/EOF path; [`Self::open_error`]
/// reports that after budgeting. If whitespace trimming reaches `#!` itself
/// (including NUL/EOF at byte 255), parsing instead returns ENOEXEC.
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

    /// The error from opening this interpreter, if known without filesystem I/O.
    ///
    /// Caller sequence: [`crate::args::ArgBudget::rewrite`], the kernel's ENOENT
    /// check for PATH_INACCESSIBLE, this check, then opening the interpreter.
    /// This defers empty-path EACCES until after E2BIG/ENOMEM. Linux itself
    /// checks PATH_INACCESSIBLE before copying; exact ENOENT precedence requires
    /// that kernel precheck before rewrite as well.
    pub const fn open_error(&self) -> Option<ShebangError> {
        if self.interpreter.is_empty() { Some(ShebangError::EmptyInterpreterPath) } else { None }
    }
}

/// Why a script's interpreter cannot be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShebangError {
    /// No interpreter remains before the newline/byte-255 trim endpoint: ENOEXEC.
    /// This includes `#!` followed by whitespace through byte 254, even when
    /// byte 255 is NUL or EOF padding.
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
fn skip_space_tab(bytes: &[u8], from: usize) -> usize {
    from + bytes[from..].iter().take_while(|&&b| space_tab(b)).count()
}
fn terminator(byte: u8) -> bool {
    byte == 0 || space_tab(byte)
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
/// An empty NUL/EOF interpreter is returned for budgeting before its
/// [`Shebang::open_error`] (EACCES) is checked. Whitespace through byte 254,
/// followed by NUL or EOF at byte 255, is trimmed away and yields ENOEXEC.
pub fn parse(head: &[u8]) -> Result<Option<Shebang<'_>>, ShebangError> {
    let head = &head[..head.len().min(BINPRM_BUF_SIZE)];
    if !head.starts_with(b"#!") {
        return Ok(None);
    }
    let mut padded = [0; BINPRM_BUF_SIZE];
    padded[..head.len()].copy_from_slice(head);
    let end = if let Some(i) = padded.iter().take_while(|&&b| b != 0).position(|&b| b == b'\n') {
        i
    } else {
        let start = skip_space_tab(&padded, 2);
        if start == padded.len() {
            return Err(ShebangError::EmptyInterpreter);
        }
        if !padded[start..].iter().copied().any(terminator) {
            return Err(ShebangError::TruncatedInterpreter);
        }
        BINPRM_BUF_SIZE - 1
    };
    let end = trim_end(&padded[..end]).len();
    let start = skip_space_tab(&padded[..end], 2);
    if start == end {
        return Err(ShebangError::EmptyInterpreter);
    }
    let split = (start..end).find(|&i| terminator(padded[i])).unwrap_or(end);
    let argument = if split < end && space_tab(padded[split]) {
        let arg_start = skip_space_tab(&padded[..end], split);
        if arg_start == end {
            None
        } else {
            let arg_end = (arg_start..end).find(|&i| padded[i] == 0).unwrap_or(end);
            Some(&head[arg_start..arg_end])
        }
    } else {
        None
    };
    Ok(Some(Shebang { interpreter: &head[start..split], argument }))
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

/// Interpreter substitutions followed during one exec; cannot be cloned,
/// copied, or reset via `mem::take`.
///
/// ```compile_fail
/// use lumen_elf::shebang::Nesting;
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<Nesting>();
/// ```
///
/// ```compile_fail
/// use lumen_elf::shebang::Nesting;
/// fn requires_copy<T: Copy>() {}
/// requires_copy::<Nesting>();
/// ```
///
/// ```compile_fail
/// use lumen_elf::shebang::Nesting;
/// let mut nesting = Nesting::new();
/// let _ = core::mem::take(&mut nesting);
/// ```
#[must_use]
#[derive(Debug, PartialEq, Eq)]
pub struct Nesting {
    depth: usize,
}

#[expect(clippy::new_without_default, reason = "Default would let mem::take reset an exec's nesting depth")]
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
    use alloc::{vec, vec::Vec};

    const ALPHABET: [u8; 6] = [0, b' ', b'\t', b'\n', b'\r', b'x'];

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
            let sh = parse(head).unwrap().unwrap();
            assert_eq!(sh.interpreter(), b"");
            assert_eq!(sh.open_error(), Some(ShebangError::EmptyInterpreterPath));
            assert_eq!(sh.open_error().unwrap().errno(), Errno::Access);
        }
    }

    #[test]
    fn parse_short_eof_and_256_byte_newline_boundary() {
        assert_parse(b"#!/bin/sh", b"/bin/sh", None);
        let mut head = vec![b'x'; BINPRM_BUF_SIZE - 1];
        head[..2].copy_from_slice(b"#!");
        assert_parse(&head, &head[2..], None); // 255-byte file: byte 255 is EOF padding.
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
    fn parse_nul_at_byte_255_preserves_trim_endpoint() {
        let mut head = [b' '; 256];
        head[..2].copy_from_slice(b"#!");
        for i in (2..255).step_by(2) {
            head[i] = b'\t';
        }
        assert_eq!(parse(&head[..255]), Err(ShebangError::EmptyInterpreter));
        head[255] = 0;
        assert_eq!(parse(&head), Err(ShebangError::EmptyInterpreter));
        let sh = parse(&head[..254]).unwrap().unwrap();
        assert_eq!(sh.interpreter(), b"");
        assert_eq!(sh.open_error(), Some(ShebangError::EmptyInterpreterPath));
        check_properties(&head[..254]);
        check_properties(&head[..255]);
        check_properties(&head);
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
    fn nesting_accepts_limit_and_rejects_next() {
        assert_eq!(MAX_NESTING, 5);
        let mut nesting = Nesting::new();
        assert_eq!(nesting.depth(), 0);
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
            assert_eq!(sh.open_error(), sh.interpreter().is_empty().then_some(ShebangError::EmptyInterpreterPath));
            if newline {
                assert!(!sh.interpreter().is_empty());
            }
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
        let mut checked = 0;
        // Includes every 7-byte `#!` header over the syntax ALPHABET
        // (ALPHABET.len().pow(5)), and all other lengths from 2 through 9.
        for len in 2..=9 {
            for pattern in 0..ALPHABET.len().pow((len - 2) as u32) {
                let mut head = *b"#!xxxxxxx";
                let mut value = pattern;
                for byte in &mut head[2..len] {
                    *byte = ALPHABET[value % ALPHABET.len()];
                    value /= ALPHABET.len();
                }
                check_properties(&head[..len]);
                checked += 1;
            }
        }
        assert_eq!(checked, 335_923);
    }

    #[test]
    fn parse_matches_linux_for_full_window_tails() {
        let mut checked = 0;
        for prefix in [b"#!sh ".as_slice(), b"#!xxx"] {
            for pattern in 0..ALPHABET.len().pow(6) {
                let mut head = [b'x'; 256];
                head[..prefix.len()].copy_from_slice(prefix);
                let mut value = pattern;
                for byte in &mut head[250..256] {
                    *byte = ALPHABET[value % ALPHABET.len()];
                    value /= ALPHABET.len();
                }
                check_properties(&head);
                checked += 1;
            }
        }
        assert_eq!(checked, 2 * 46_656);
        for endpoint in [Some(253), Some(254), Some(255), None] {
            for byte in [0, b' ', b'\t', b'\n'] {
                let mut head = [b'x'; 256];
                head[..2].copy_from_slice(b"#!");
                if let Some(i) = endpoint {
                    head[i] = byte;
                }
                check_properties(&head);
                if endpoint.is_none() {
                    assert_eq!(parse(&head), Err(ShebangError::TruncatedInterpreter));
                }
            }
        }
    }

    #[test]
    fn parse_properties_for_arbitrary_inputs() {
        let mut successes = 0;
        let mut seed = 0x343a_2024_9876_1234u64;
        for len in 0..=300 {
            for sample in 0..64 {
                let mut head = vec![0; len];
                for byte in &mut head {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    *byte = if sample % 2 == 0 { seed as u8 } else { ALPHABET[seed as usize % ALPHABET.len()] };
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
