//! Argument/environment validation for exec, using NUL-free byte strings.
//!
//! Limits apply to the normalized argv and must be checked again after any
//! shebang rewriting. File access and the process's stack rlimit are external.

use alloc::{vec, vec::Vec};
use core::fmt;

use crate::loader::PAGE_SIZE;

/// Maximum string storage including its NUL: 32 x86-64 pages (128 KiB).
/// Linux `include/uapi/linux/binfmts.h`, `MAX_ARG_STRLEN`.
pub const MAX_ARG_STRLEN: usize = 32 * PAGE_SIZE as usize;

/// Maximum count in each of argv and envp, independently.
/// Linux `include/uapi/linux/binfmts.h`, `MAX_ARG_STRINGS`.
pub const MAX_ARG_STRINGS: usize = 0x7fff_ffff;

/// Exec size errors map to `-E2BIG`. Interior NUL is an invalid byte-string
/// input, also rejected by [`crate::stack::build_initial_stack`]; the caller
/// must extract user C strings before invoking this API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgError {
    /// One string plus its NUL exceeds [`MAX_ARG_STRLEN`].
    StringTooLong,
    /// An argument or environment count exceeds [`MAX_ARG_STRINGS`].
    TooManyStrings,
    /// Strings plus live pointer slots exceed the budget, or size arithmetic overflows.
    TotalTooLarge,
    /// An input contains a NUL rather than only the bytes preceding its terminator.
    InteriorNul,
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::StringTooLong => "exec string exceeds MAX_ARG_STRLEN",
            Self::TooManyStrings => "exec vector exceeds MAX_ARG_STRINGS",
            Self::TotalTooLarge => "exec arguments and environment exceed total limit",
            Self::InteriorNul => "exec string contains an interior NUL byte",
        })
    }
}

/// Argument and environment storage after empty-argv normalization.
///
/// These are the variable parts of the initial stack. Its two null pointer
/// slots, `argc` word, `execfn`, random bytes, auxv and alignment padding are
/// additional; callers must reserve them separately from this API's argv/envp
/// budget. This is not the full initial stack size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgSizes {
    pub argc: usize,
    pub envc: usize,
    /// All argv/envp string bytes including one NUL per string.
    pub string_bytes: usize,
    /// Eight bytes per live argv/envp pointer (the target is x86-64).
    pub pointer_bytes: usize,
    /// `string_bytes + pointer_bytes`, bounded by the supplied total limit.
    pub total_bytes: usize,
}

/// Linux string/count limits with a caller-supplied total byte budget.
#[derive(Debug, Clone, Copy)]
pub struct ArgLimits;

impl ArgLimits {
    /// Validate argv/envp and return their storage sizes without allocating.
    ///
    /// Slices exclude C terminators. Empty argv is charged as one empty string,
    /// matching Linux 5.18+ `do_execveat_common`; use [`normalize_argv`] before
    /// passing it to the stack builder. Counts are limited separately.
    ///
    /// The budget includes string NULs and `(max(argc, 1) + envc) * 8` pointer
    /// bytes, as in Linux `fs/exec.c::bprm_stack_limits`. Exactly the limit is
    /// accepted. The kernel chooses `total_limit`; no stack rlimit is assumed.
    pub fn check(argv: &[&[u8]], envp: &[&[u8]], total_limit: usize) -> Result<ArgSizes, ArgError> {
        let pointer_bytes = check_counts(argv.len(), envp.len())?;
        let mut string_bytes = usize::from(argv.is_empty());
        for &string in argv.iter().chain(envp) {
            // Compare before adding the terminator, so even extreme lengths cannot overflow.
            if string.len() >= MAX_ARG_STRLEN {
                return Err(ArgError::StringTooLong);
            }
            if string.contains(&0) {
                return Err(ArgError::InteriorNul);
            }
            string_bytes = string_bytes.checked_add(string.len() + 1).ok_or(ArgError::TotalTooLarge)?;
        }
        let total_bytes = checked_total(string_bytes, pointer_bytes, total_limit)?;
        Ok(ArgSizes { argc: argv.len().max(1), envc: envp.len(), string_bytes, pointer_bytes, total_bytes })
    }
}

fn check_counts(argc: usize, envc: usize) -> Result<usize, ArgError> {
    if argc > MAX_ARG_STRINGS || envc > MAX_ARG_STRINGS {
        return Err(ArgError::TooManyStrings);
    }
    argc.max(1).checked_add(envc).and_then(|n| n.checked_mul(8)).ok_or(ArgError::TotalTooLarge)
}

fn checked_total(strings: usize, pointers: usize, limit: usize) -> Result<usize, ArgError> {
    strings.checked_add(pointers).filter(|&total| total <= limit).ok_or(ArgError::TotalTooLarge)
}

/// Borrow the original arguments, supplying a single empty `argv[0]` if absent.
///
/// This is Linux 5.18+'s empty-argv normalization. Existing arguments, including
/// an explicitly empty first argument, are preserved without copying strings.
pub fn normalize_argv<'a>(argv: &[&'a [u8]]) -> Vec<&'a [u8]> {
    if argv.is_empty() { vec![b""] } else { argv.to_vec() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stack::{AuxvInfo, StackBuildError, USER_STACK_TOP, build_initial_stack};

    #[test]
    fn string_length_limit_includes_nul() {
        assert_eq!(MAX_ARG_STRLEN, 131_072);
        let at_limit = vec![b'a'; MAX_ARG_STRLEN - 1];
        let past_limit = vec![b'a'; MAX_ARG_STRLEN];
        for environment in [false, true] {
            let argv: &[&[u8]] = if environment { &[b"p"] } else { &[&at_limit] };
            let envp: &[&[u8]] = if environment { &[&at_limit] } else { &[] };
            let sizes = ArgLimits::check(argv, envp, usize::MAX).unwrap();
            assert_eq!(sizes.string_bytes, MAX_ARG_STRLEN + if environment { 2 } else { 0 });
            let argv: &[&[u8]] = if environment { &[b"p"] } else { &[&past_limit] };
            let envp: &[&[u8]] = if environment { &[&past_limit] } else { &[] };
            assert_eq!(ArgLimits::check(argv, envp, usize::MAX), Err(ArgError::StringTooLong));
        }
    }

    #[test]
    fn vector_count_limits_are_independent() {
        assert_eq!(MAX_ARG_STRINGS, 2_147_483_647);
        // The production count gate takes lengths: test its real boundary
        // without allocating billions of strings or fabricating invalid slices.
        assert_eq!(check_counts(MAX_ARG_STRINGS, 0), Ok(MAX_ARG_STRINGS * 8));
        assert_eq!(check_counts(1, MAX_ARG_STRINGS), Ok((MAX_ARG_STRINGS + 1) * 8));
        assert_eq!(check_counts(MAX_ARG_STRINGS, MAX_ARG_STRINGS), Ok(MAX_ARG_STRINGS * 16));
        assert_eq!(check_counts(MAX_ARG_STRINGS + 1, 0), Err(ArgError::TooManyStrings));
        assert_eq!(check_counts(1, MAX_ARG_STRINGS + 1), Err(ArgError::TooManyStrings));
        assert_eq!(check_counts(usize::MAX, 0), Err(ArgError::TooManyStrings));
        assert_eq!(check_counts(0, usize::MAX), Err(ArgError::TooManyStrings));
    }

    #[test]
    fn total_limit_includes_strings_nuls_and_pointers() {
        // "p\0", "\0", "K=V\0", and three 8-byte live pointers.
        let argv: &[&[u8]] = &[b"p", b""];
        let envp: &[&[u8]] = &[b"K=V"];
        let expected = ArgSizes { argc: 2, envc: 1, string_bytes: 7, pointer_bytes: 24, total_bytes: 31 };
        assert_eq!(ArgLimits::check(argv, envp, 31), Ok(expected));
        assert_eq!(ArgLimits::check(argv, envp, 30), Err(ArgError::TotalTooLarge));
        assert_eq!(ArgLimits::check(&[b"p", b"x"], envp, 31), Err(ArgError::TotalTooLarge));
        assert_eq!(ArgLimits::check(argv, &[b"K=Vx"], 31), Err(ArgError::TotalTooLarge));
        // Even zero-length strings need both a NUL and a pointer.
        assert_eq!(ArgLimits::check(&[b""], &[], 9).unwrap().total_bytes, 9);
        assert_eq!(ArgLimits::check(&[b"", b""], &[], 9), Err(ArgError::TotalTooLarge));
        assert_eq!(ArgLimits::check(&[b""], &[b""], 9), Err(ArgError::TotalTooLarge));
    }

    #[test]
    fn empty_argv_normalization_and_budget() {
        let empty = normalize_argv(&[]);
        assert_eq!(empty, [b"".as_slice()]);
        assert_eq!(normalize_argv(&empty), empty);
        let original: &[&[u8]] = &[b"", b"arg\xff"];
        assert_eq!(normalize_argv(original), original);
        let original: &[&[u8]] = &[b"program", b"arg"];
        assert_eq!(normalize_argv(original), original);
        let expected = ArgSizes { argc: 1, envc: 0, string_bytes: 1, pointer_bytes: 8, total_bytes: 9 };
        assert_eq!(ArgLimits::check(&[], &[], 9), Ok(expected));
        assert_eq!(ArgLimits::check(&empty, &[], 9), Ok(expected));
        assert_eq!(ArgLimits::check(&[], &[], 8), Err(ArgError::TotalTooLarge));
        assert_eq!(ArgLimits::check(&[], &[], 0), Err(ArgError::TotalTooLarge));
        assert_eq!(ArgLimits::check(&[], &[b"K=V"], 21).unwrap().total_bytes, 21);
        assert_eq!(ArgLimits::check(&[], &[b"K=V"], 20), Err(ArgError::TotalTooLarge));
    }

    #[test]
    fn interior_nuls_are_rejected() {
        for string in [b"\0".as_slice(), b"a\0b", b"end\0"] {
            assert_eq!(ArgLimits::check(&[string], &[], usize::MAX), Err(ArgError::InteriorNul));
            assert_eq!(ArgLimits::check(&[b"p"], &[string], usize::MAX), Err(ArgError::InteriorNul));
        }
        assert!(ArgLimits::check(&[b"\xff\xfe"], &[b"X=\xff"], usize::MAX).is_ok());
    }

    #[test]
    fn total_size_overflow_is_rejected() {
        assert_eq!(checked_total(usize::MAX - 8, 8, usize::MAX), Ok(usize::MAX));
        assert_eq!(checked_total(usize::MAX - 7, 8, usize::MAX), Err(ArgError::TotalTooLarge));
    }

    #[test]
    fn rewritten_argv_must_fit_limits() {
        let original: &[&[u8]] = &[b"p", b"arg"];
        let budget = ArgLimits::check(original, &[], usize::MAX).unwrap().total_bytes;
        let sh = crate::shebang::parse(b"#!/bin/sh -e\n").unwrap().unwrap();
        let rewritten = crate::shebang::argv(&sh, b"/script", original);
        let rewritten: Vec<&[u8]> = rewritten.iter().map(Vec::as_slice).collect();
        assert_eq!(ArgLimits::check(&rewritten, &[], budget), Err(ArgError::TotalTooLarge));
        assert_eq!(ArgLimits::check(&rewritten, &[], 55).unwrap().total_bytes, 55);
        assert_eq!(ArgLimits::check(&rewritten, &[], 54), Err(ArgError::TotalTooLarge));
    }

    #[test]
    fn sizes_agree_with_initial_stack_builder() {
        let args: &[&[u8]] = &[b"p", b"", b"a\xff"];
        let envs: &[&[u8]] = &[b"A=1", b"", b"B=\xfe"];
        for base in [None, Some(0x7000_0000)] {
            let auxv = AuxvInfo {
                phdr: 0x400040,
                phent: 56,
                phnum: 1,
                pagesz: 4096,
                base,
                entry: 0x401000,
                uid: 0,
                euid: 0,
                gid: 0,
                egid: 0,
                secure: 0,
                clktck: 100,
            };
            for argc in 0..=args.len() {
                for envc in 0..=envs.len() {
                    let sizes = ArgLimits::check(&args[..argc], &envs[..envc], usize::MAX).unwrap();
                    let argv = normalize_argv(&args[..argc]);
                    let envp = &envs[..envc];
                    assert_eq!((sizes.argc, sizes.envc), (argv.len(), envp.len()));
                    // Vary execfn across every string alignment residue as well
                    // as both pointer-table parities and both auxv layouts.
                    for path_len in 0..=17 {
                        let execfn = vec![b'x'; path_len];
                        let strings = sizes.string_bytes + execfn.len() + 1;
                        let table = sizes.pointer_bytes + 3 * 8; // argc + both NULL slots
                        let aux_bytes = if base.is_some() { 15 * 16 } else { 14 * 16 };
                        let required = strings.div_ceil(16) * 16 + 16 + table.div_ceil(16) * 16 + aux_bytes;
                        let mut buf = vec![0xcc; required];
                        let built =
                            build_initial_stack(&mut buf, USER_STACK_TOP, &argv, envp, &execfn, [42; 16], &auxv)
                                .unwrap();
                        assert_eq!(built.bytes_written, required);
                        assert_eq!(built.rsp, USER_STACK_TOP - required as u64);
                        assert_eq!(u64::from_le_bytes(buf[..8].try_into().unwrap()), sizes.argc as u64);
                        // Inspect each emitted pointer/string to catch a size
                        // estimate that matches by accident but omits an input.
                        let mut slot = 8;
                        let mut actual_string_bytes = 0;
                        for vector in [argv.as_slice(), envp] {
                            for &expected in vector {
                                let ptr = u64::from_le_bytes(buf[slot..slot + 8].try_into().unwrap());
                                let offset = (ptr - built.rsp) as usize;
                                assert_eq!(&buf[offset..offset + expected.len()], expected);
                                assert_eq!(buf[offset + expected.len()], 0);
                                actual_string_bytes += expected.len() + 1;
                                slot += 8;
                            }
                            assert_eq!(&buf[slot..slot + 8], &[0; 8]);
                            slot += 8;
                        }
                        assert_eq!(actual_string_bytes, sizes.string_bytes);
                        assert_eq!(slot, table);
                        let mut short = vec![0xcc; required - 1];
                        assert_eq!(
                            build_initial_stack(&mut short, USER_STACK_TOP, &argv, envp, &execfn, [42; 16], &auxv),
                            Err(StackBuildError::BufferTooSmall { required, available: required - 1 })
                        );
                        assert!(short.iter().all(|&b| b == 0xcc));
                    }
                }
            }
        }
    }
}
