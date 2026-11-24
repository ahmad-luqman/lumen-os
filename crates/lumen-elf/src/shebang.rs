//! Bounded byte parsing and argument rewriting for `#!` scripts.
//!
//! No path lookup or interpreter loading is performed here. Inputs shorter
//! than [`BINPRM_BUF_SIZE`] represent EOF; longer inputs are ignored past it.

use alloc::vec::Vec;
use core::fmt;

/// Linux's executable header window (`include/uapi/linux/binfmts.h`).
pub const BINPRM_BUF_SIZE: usize = 256;

/// Maximum interpreter substitutions in a successful exec (five).
///
/// Linux [`exec_binprm`](https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git/tree/fs/exec.c?h=v6.12)
/// tries handlers at depths 0 through 5 and returns `-ELOOP` at depth 6.
/// Five scripts followed by a binary fit; six scripts followed by a binary do not.
pub const MAX_NESTING: usize = 5;

/// An interpreter and at most one argument, borrowed from the file header.
///
/// [`parse`] guarantees a nonempty interpreter, no NULs or newlines in either
/// value, and no leading or trailing spaces/tabs. Inner argument whitespace
/// and carriage returns are preserved; quotes have no special meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shebang<'a> {
    pub interpreter: &'a [u8],
    pub argument: Option<&'a [u8]>,
}

/// Invalid script headers map to `-ENOEXEC`; excessive nesting maps to `-ELOOP`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShebangError {
    /// The header contains no interpreter name.
    EmptyInterpreter,
    /// The interpreter name has no terminator in the header window.
    TruncatedInterpreter,
    /// Following another interpreter would exceed [`MAX_NESTING`].
    NestingLimit,
}

impl fmt::Display for ShebangError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::EmptyInterpreter => "empty shebang interpreter",
            Self::TruncatedInterpreter => "truncated shebang interpreter",
            Self::NestingLimit => "interpreter nesting limit exceeded",
        })
    }
}

fn space_tab(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t')
}

/// Parse only the first [`BINPRM_BUF_SIZE`] bytes, returning `None` without `#!`.
///
/// A newline, NUL, or short read ends the logical line. Only spaces and tabs
/// are trimmed. An empty remainder supplies no optional argument.
///
/// Following Linux `fs/binfmt_script.c::load_script`, a full window without
/// a newline/NUL must contain a space/tab terminating the interpreter. The
/// optional argument may be clipped: byte 255 is reserved for termination,
/// even when that byte is itself a space/tab terminating the interpreter.
pub fn parse(head: &[u8]) -> Result<Option<Shebang<'_>>, ShebangError> {
    let head = &head[..head.len().min(BINPRM_BUF_SIZE)];
    if !head.starts_with(b"#!") {
        return Ok(None);
    }

    let end = match head.iter().position(|&b| matches!(b, b'\n' | 0)) {
        Some(end) => end,
        None if head.len() < BINPRM_BUF_SIZE => head.len(),
        None => {
            let start = head[2..]
                .iter()
                .position(|&b| !space_tab(b))
                .map(|offset| offset + 2)
                .ok_or(ShebangError::EmptyInterpreter)?;
            if !head[start..].iter().any(|&b| space_tab(b)) {
                return Err(ShebangError::TruncatedInterpreter);
            }
            BINPRM_BUF_SIZE - 1
        }
    };
    // Only space/tab are separators, not the other ASCII whitespace bytes.
    let line = &head[2..end][head[2..end].iter().take_while(|&&b| space_tab(b)).count()..];
    let end = line.iter().rposition(|&b| !space_tab(b)).map_or(0, |i| i + 1);
    let line = &line[..end];
    if line.is_empty() {
        return Err(ShebangError::EmptyInterpreter);
    }
    let split = line.iter().position(|&b| space_tab(b)).unwrap_or(line.len());
    let argument = &line[split..];
    let argument = &argument[argument.iter().take_while(|&&b| space_tab(b)).count()..];
    Ok(Some(Shebang { interpreter: &line[..split], argument: (!argument.is_empty()).then_some(argument) }))
}

/// Replace `argv[0]` with `[interpreter, argument?, script_path]`.
///
/// All remaining arguments retain their bytes and order. Empty original argv
/// produces only this prefix: Linux's synthetic empty `argv[0]` is replaced
/// too. Callers must check argument limits again after each substitution.
pub fn argv(sh: &Shebang<'_>, script_path: &[u8], original: &[&[u8]]) -> Vec<Vec<u8>> {
    core::iter::once(sh.interpreter)
        .chain(sh.argument)
        .chain(core::iter::once(script_path))
        .chain(original.iter().skip(1).copied())
        .map(<[u8]>::to_vec)
        .collect()
}

/// Interpreter substitutions already followed during one exec.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Nesting {
    depth: usize,
}

impl Nesting {
    /// Start an exec with no interpreter substitutions.
    pub const fn new() -> Self {
        Self { depth: 0 }
    }

    /// Count one substitution, or return `-ELOOP` without changing the counter.
    /// Call once before following each interpreter, not for the final binary.
    pub fn follow_interpreter(&mut self) -> Result<(), ShebangError> {
        if self.depth == MAX_NESTING {
            return Err(ShebangError::NestingLimit);
        }
        self.depth += 1;
        Ok(())
    }

    /// Number of interpreter substitutions admitted so far.
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
    fn parse_empty_argument_is_absent() {
        for head in [b"#!sh \t\n".as_slice(), b"#!sh \t", b"#!sh \t\0"] {
            assert_parse(head, b"sh", None);
        }
        // No shell quoting: these are two literal quote bytes, not an empty argument.
        assert_parse(b"#!sh \"\"\n", b"sh", Some(b"\"\""));
    }

    #[test]
    fn parse_nul_ends_logical_line() {
        assert_parse(b"#!sh\0ignored -x\n", b"sh", None);
        assert_parse(b"#!sh -a\0ignored\n", b"sh", Some(b"-a"));
        assert_parse(b"#!sh -a \t\0ignored\n", b"sh", Some(b"-a"));
        assert_parse(b"#!sh \0ignored\n", b"sh", None);
        let mut head = [b'x'; BINPRM_BUF_SIZE];
        head[..4].copy_from_slice(b"#!sh");
        head[4] = 0;
        assert_parse(&head, b"sh", None);
    }

    #[test]
    fn parse_empty_interpreter_is_enoexec() {
        for head in [b"#!".as_slice(), b"#! \t", b"#!\nsh", b"#! \t\n", b"#!\0sh", b"#! \t\0sh"] {
            assert_eq!(parse(head), Err(ShebangError::EmptyInterpreter));
        }
        let mut head = [b' '; BINPRM_BUF_SIZE];
        head[..2].copy_from_slice(b"#!");
        assert_eq!(parse(&head), Err(ShebangError::EmptyInterpreter));
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
        for argument in [None, Some(b"-a -b".as_slice()), Some(b"")] {
            let sh = Shebang { interpreter: b"/bin/interp", argument };
            let mut expected = vec![b"/bin/interp".to_vec()];
            if let Some(arg) = argument {
                expected.push(arg.to_vec());
            }
            expected.extend([b"script path".to_vec(), b"one".to_vec(), vec![], b"two\xff".to_vec()]);
            assert_eq!(argv(&sh, b"script path", original), expected);
        }
    }

    #[test]
    fn argv_with_empty_original() {
        for argument in [None, Some(b"-e".as_slice())] {
            let sh = Shebang { interpreter: b"/bin/sh", argument };
            let expected = if argument.is_some() {
                vec![b"/bin/sh".to_vec(), b"-e".to_vec(), b"script".to_vec()]
            } else {
                vec![b"/bin/sh".to_vec(), b"script".to_vec()]
            };
            assert_eq!(argv(&sh, b"script", &[]), expected);
            assert_eq!(argv(&sh, b"script", &[b""]), expected);
        }
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
            assert_eq!(nesting.follow_interpreter(), Err(ShebangError::NestingLimit));
            assert_eq!(nesting.depth(), MAX_NESTING);
        }
    }

    #[test]
    fn parse_properties_for_arbitrary_inputs() {
        fn check(head: &[u8]) -> bool {
            let result = parse(head);
            assert_eq!(result, parse(&head[..head.len().min(BINPRM_BUF_SIZE)]));
            if let Ok(Some(sh)) = result {
                assert!(head.starts_with(b"#!"));
                assert!(!sh.interpreter.is_empty());
                assert!(!sh.interpreter.iter().any(|&b| space_tab(b)));
                for value in core::iter::once(sh.interpreter).chain(sh.argument) {
                    assert!(!value.is_empty());
                    assert!(!value.iter().any(|b| matches!(b, 0 | b'\n')));
                    assert!(!space_tab(value[0]));
                    assert!(!space_tab(value[value.len() - 1]));
                }
                true
            } else {
                false
            }
        }

        // Exhaust all short headers over the syntax alphabet, then generate
        // both arbitrary bytes and syntax-heavy bytes at every length 0..=300.
        let alphabet = [0, b' ', b'\t', b'\n', b'\r', b'x'];
        let mut successes = 0;
        for pattern in 0..6usize.pow(5) {
            let mut head = *b"#!xxxxx";
            let mut value = pattern;
            for byte in &mut head[2..] {
                *byte = alphabet[value % alphabet.len()];
                value /= alphabet.len();
            }
            successes += usize::from(check(&head));
        }
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
                successes += usize::from(check(&head));
            }
        }
        assert!(successes > 1000, "exercise successful parses as well as rejection paths");
    }
}
