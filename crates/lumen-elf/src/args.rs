//! Incremental exec argument accounting, before copying each user string.
//!
//! The original pointer reservation survives shebang substitutions; execfn
//! and each copied string share the remaining byte budget.

use crate::{loader::PAGE_SIZE, shebang::Shebang};
use core::fmt;

/// Kernel-facing errno categories, independent of any syscall ABI crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Errno {
    NoExec,
    Access,
    Loop,
    TooBig,
}

/// Maximum string storage including its NUL: 32 x86-64 pages (128 KiB).
/// Linux `include/uapi/linux/binfmts.h`, `MAX_ARG_STRLEN`.
pub const MAX_ARG_STRLEN: usize = 32 * PAGE_SIZE as usize;

/// argv and envp counts are each limited to MAX_ARG_STRINGS, apart from the
/// byte budget. Linux `include/uapi/linux/binfmts.h`, `MAX_ARG_STRINGS`.
pub const MAX_ARG_STRINGS: usize = 0x7fff_ffff;

/// Exec size failures, or a malformed input to the slice convenience [`check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgError {
    /// One string plus its NUL exceeds [`MAX_ARG_STRLEN`]: E2BIG.
    StringTooLong,
    /// Either vector exceeds [`MAX_ARG_STRINGS`]: E2BIG.
    TooManyStrings,
    /// The byte budget is exceeded, arithmetic overflows, or a supplied length
    /// cannot describe a charged C string: E2BIG.
    TooBig,
    /// Caller bug: a slice contains a C terminator. Only [`check`] can return
    /// this; properly extracted user C strings never contain interior NULs.
    InteriorNul,
}

impl ArgError {
    pub const fn errno(&self) -> Option<Errno> {
        match self {
            Self::InteriorNul => None,
            _ => Some(Errno::TooBig),
        }
    }
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::StringTooLong => "exec string exceeds MAX_ARG_STRLEN",
            Self::TooManyStrings => "exec vector exceeds MAX_ARG_STRINGS",
            Self::TooBig => "exec argument byte budget exceeded or invalid length",
            Self::InteriorNul => "exec string contains an interior NUL byte",
        })
    }
}

/// An exec's original pointer reservation and incrementally charged strings.
///
/// Follow Linux `fs/exec.c::do_execveat_common` in this order:
/// 1. [`Self::new`] (`bprm_stack_limits`) before reading/copying strings.
/// 2. Charge execfn (`copy_string_kernel(bprm->filename)`).
/// 3. Charge envp, last to first (`copy_strings`).
/// 4. Charge argv, last to first (`copy_strings`).
/// 5. If the original argc was zero, charge one byte for the synthetic "".
///
/// Pass each `strnlen_user` length including NUL to [`Self::charge`] before
/// copying that string. The caller chooses the byte limit from its stack
/// policy. Neither the budget nor its state can be cloned or copied.
#[must_use]
#[derive(Debug)]
pub struct ArgBudget {
    pointer_bytes: usize,
    string_bytes: usize,
    total_limit: usize,
}

impl ArgBudget {
    /// Reserve `(max(argc, 1) + envc) * 8` once. Counts are checked separately;
    /// a limit no greater than the pointer reservation fails with E2BIG.
    pub fn new(argc: usize, envc: usize, total_limit: usize) -> Result<Self, ArgError> {
        if argc > MAX_ARG_STRINGS || envc > MAX_ARG_STRINGS {
            return Err(ArgError::TooManyStrings);
        }
        let pointer_bytes = argc.max(1).checked_add(envc).and_then(|n| n.checked_mul(8)).ok_or(ArgError::TooBig)?;
        if total_limit <= pointer_bytes {
            return Err(ArgError::TooBig);
        }
        Ok(Self { pointer_bytes, string_bytes: 0, total_limit })
    }

    /// Charge a C string's length including its NUL before copying it.
    /// Zero is not a C-string length. Errors leave the budget unchanged.
    pub fn charge(&mut self, len_with_nul: usize) -> Result<(), ArgError> {
        if len_with_nul > MAX_ARG_STRLEN {
            return Err(ArgError::StringTooLong);
        }
        if len_with_nul == 0 || len_with_nul > self.remaining() {
            return Err(ArgError::TooBig);
        }
        self.string_bytes += len_with_nul;
        Ok(())
    }

    /// Refund argv[0], then charge script path, optional argument, interpreter,
    /// in Linux `fs/binfmt_script.c::load_script` order. No pointers are charged.
    ///
    /// Both length arguments exclude NUL. `old_argv0_len` must describe the
    /// first argument already charged to this budget; use `Some(0)` for the
    /// synthetic empty argv[0], or `None` only if there is no charged argv[0].
    /// Linux `remove_arg_zero` advances `bprm->p` past argv[0] and its NUL and
    /// decrements argc, giving those bytes back to `bprm_hit_stack_limit`.
    /// The original pointer reservation remains fixed throughout exec.
    ///
    /// On failure this budget is unchanged; the caller must abandon the rewrite.
    pub fn rewrite(
        &mut self,
        sh: &Shebang<'_>,
        script_path_len: usize,
        old_argv0_len: Option<usize>,
    ) -> Result<(), ArgError> {
        let refund = match old_argv0_len {
            Some(len) => len.checked_add(1).ok_or(ArgError::TooBig)?,
            None => 0,
        };
        let mut next = Self {
            string_bytes: self.string_bytes.checked_sub(refund).ok_or(ArgError::TooBig)?,
            pointer_bytes: self.pointer_bytes,
            total_limit: self.total_limit,
        };
        next.charge(script_path_len.checked_add(1).ok_or(ArgError::StringTooLong)?)?;
        if let Some(arg) = sh.argument() {
            next.charge(arg.len() + 1)?;
        }
        next.charge(sh.interpreter().len() + 1)?;
        *self = next;
        Ok(())
    }

    pub const fn pointer_bytes(&self) -> usize {
        self.pointer_bytes
    }
    /// Includes execfn and one NUL per charged string, net of argv[0] refunds.
    pub const fn string_bytes(&self) -> usize {
        self.string_bytes
    }
    /// Bytes available for further strings without changing the pointer reservation.
    pub const fn remaining(&self) -> usize {
        self.total_limit - self.pointer_bytes - self.string_bytes
    }
}

/// The budget figure for normalized argv, envp and execfn, not the stack size.
///
/// String bytes include execfn and NULs. The two NULL pointer slots, argc word,
/// AT_RANDOM, auxv and padding are outside Linux's budget. The stack builder
/// needs those in addition to these strings and live argv/envp pointers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgSizes {
    argc: usize,
    envc: usize,
    string_bytes: usize,
    pointer_bytes: usize,
}

impl ArgSizes {
    pub const fn argc(&self) -> usize {
        self.argc
    }
    pub const fn envc(&self) -> usize {
        self.envc
    }
    pub const fn string_bytes(&self) -> usize {
        self.string_bytes
    }
    pub const fn pointer_bytes(&self) -> usize {
        self.pointer_bytes
    }
    pub const fn total_bytes(&self) -> usize {
        self.string_bytes + self.pointer_bytes
    }
}

/// Validate complete NUL-free slices without allocating, using [`ArgBudget`].
///
/// Normalizes argv first, then reserves pointers and charges execfn, reversed
/// envp, reversed argv (or the synthetic empty string). For kernel copying use
/// [`ArgBudget`] directly so each length is checked before allocating/copying.
/// Oversized strings are rejected before checking for a caller-bug interior NUL.
/// Use [`normalize_argv`] on the same argv when calling the stack builder.
pub fn check(argv: &[&[u8]], envp: &[&[u8]], execfn: &[u8], total_limit: usize) -> Result<ArgSizes, ArgError> {
    let argv = normalize_argv(argv);
    let mut budget = ArgBudget::new(argv.len(), envp.len(), total_limit)?;
    for string in core::iter::once(execfn).chain(envp.iter().rev().copied()).chain(argv.iter().rev().copied()) {
        if string.len() >= MAX_ARG_STRLEN {
            return Err(ArgError::StringTooLong);
        }
        if string.contains(&0) {
            return Err(ArgError::InteriorNul);
        }
        budget.charge(string.len() + 1)?;
    }
    Ok(ArgSizes {
        argc: argv.len(),
        envc: envp.len(),
        string_bytes: budget.string_bytes,
        pointer_bytes: budget.pointer_bytes,
    })
}

/// Borrow argv unchanged, or a static single empty argv[0] if absent.
/// This Linux 5.18+ normalization performs no allocation or string copying.
#[must_use]
pub fn normalize_argv<'s, 'a>(argv: &'s [&'a [u8]]) -> &'s [&'a [u8]] {
    if argv.is_empty() { &[b""] } else { argv }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        shebang::parse,
        stack::{StackBuildError, USER_STACK_TOP, build_initial_stack, sample_auxv},
    };
    use alloc::vec;

    #[test]
    fn string_length_limit_includes_nul() {
        assert_eq!(MAX_ARG_STRLEN, 131_072);
        let at_limit = vec![b'a'; MAX_ARG_STRLEN - 1];
        let past_limit = vec![b'a'; MAX_ARG_STRLEN];
        assert_eq!(check(&[&at_limit], &[], b"p", usize::MAX).unwrap().string_bytes(), MAX_ARG_STRLEN + 2);
        assert_eq!(check(&[b"p"], &[&at_limit], b"p", usize::MAX).unwrap().string_bytes(), MAX_ARG_STRLEN + 4);
        assert_eq!(check(&[&past_limit], &[], b"p", usize::MAX), Err(ArgError::StringTooLong));
        assert_eq!(check(&[b"p"], &[&past_limit], b"p", usize::MAX), Err(ArgError::StringTooLong));
    }

    #[test]
    fn vector_count_limits_are_independent() {
        assert_eq!(MAX_ARG_STRINGS, 2_147_483_647);
        assert_eq!(ArgBudget::new(MAX_ARG_STRINGS, 0, usize::MAX).unwrap().pointer_bytes(), MAX_ARG_STRINGS * 8);
        assert_eq!(ArgBudget::new(1, MAX_ARG_STRINGS, usize::MAX).unwrap().pointer_bytes(), (MAX_ARG_STRINGS + 1) * 8);
        assert_eq!(
            ArgBudget::new(MAX_ARG_STRINGS, MAX_ARG_STRINGS, usize::MAX).unwrap().pointer_bytes(),
            MAX_ARG_STRINGS * 16
        );
        assert_eq!(ArgBudget::new(MAX_ARG_STRINGS + 1, 0, usize::MAX).unwrap_err(), ArgError::TooManyStrings);
        assert_eq!(ArgBudget::new(0, MAX_ARG_STRINGS + 1, usize::MAX).unwrap_err(), ArgError::TooManyStrings);
        assert_eq!(ArgBudget::new(usize::MAX, 0, usize::MAX).unwrap_err(), ArgError::TooManyStrings);
        assert_eq!(ArgBudget::new(0, usize::MAX, usize::MAX).unwrap_err(), ArgError::TooManyStrings);
    }

    #[test]
    fn budget_reserves_pointers_before_charging_strings() {
        assert_eq!(ArgBudget::new(0, 0, 8).unwrap_err(), ArgError::TooBig);
        assert_eq!(ArgBudget::new(1, 1, 16).unwrap_err(), ArgError::TooBig);
        let mut budget = ArgBudget::new(1, 1, 17).unwrap();
        assert_eq!(budget.pointer_bytes(), 16);
        assert_eq!(budget.remaining(), 1);
        assert_eq!(budget.charge(1), Ok(()));
        assert_eq!(budget.remaining(), 0);
        assert_eq!(budget.charge(1), Err(ArgError::TooBig));
        assert_eq!(budget.string_bytes(), 1);
        assert_eq!(budget.charge(MAX_ARG_STRLEN + 1), Err(ArgError::StringTooLong));
        let mut large = ArgBudget::new(1, 0, usize::MAX).unwrap();
        assert_eq!(large.charge(0), Err(ArgError::TooBig));
        assert_eq!(large.charge(MAX_ARG_STRLEN), Ok(()));
        assert_eq!(large.charge(MAX_ARG_STRLEN + 1), Err(ArgError::StringTooLong));
        assert_eq!(large.charge(usize::MAX), Err(ArgError::StringTooLong));
        assert_eq!(large.string_bytes(), MAX_ARG_STRLEN);
    }

    #[test]
    fn total_limit_includes_execfn_strings_nuls_and_pointers() {
        // p\0, \0, K=V\0, /p\0, and three live pointers: 10 + 24.
        let argv: &[&[u8]] = &[b"p", b""];
        let envp: &[&[u8]] = &[b"K=V"];
        let sizes = check(argv, envp, b"/p", 34).unwrap();
        assert_eq!(
            (sizes.argc(), sizes.envc(), sizes.string_bytes(), sizes.pointer_bytes(), sizes.total_bytes()),
            (2, 1, 10, 24, 34)
        );
        assert_eq!(check(argv, envp, b"/p", 33), Err(ArgError::TooBig));
        assert_eq!(check(&[b"p", b"x"], envp, b"/p", 34), Err(ArgError::TooBig));
        assert_eq!(check(argv, &[b"K=Vx"], b"/p", 34), Err(ArgError::TooBig));
        assert_eq!(check(argv, envp, b"/px", 34), Err(ArgError::TooBig));
        // Review probe: argv alone fits 25, but execfn shares the budget.
        assert_eq!(check(&[b"prog", b"arg"], &[], b"/usr/bin/prog", 25), Err(ArgError::TooBig));
    }

    #[test]
    fn empty_argv_normalization_and_budget() {
        let empty = normalize_argv(&[]);
        assert_eq!(empty, [b"".as_slice()]);
        assert_eq!(normalize_argv(empty), empty);
        let original: &[&[u8]] = &[b"", b"arg\xff"];
        assert!(core::ptr::eq(normalize_argv(original), original));
        let original: &[&[u8]] = &[b"program", b"arg"];
        assert!(core::ptr::eq(normalize_argv(original), original));
        let sizes = check(&[], &[], b"p", 11).unwrap();
        assert_eq!((sizes.argc(), sizes.envc(), sizes.string_bytes(), sizes.pointer_bytes()), (1, 0, 3, 8));
        assert_eq!(check(empty, &[], b"p", 11), Ok(sizes));
        assert_eq!(check(&[], &[], b"p", 10), Err(ArgError::TooBig));
        assert_eq!(check(&[], &[], b"p", 0), Err(ArgError::TooBig));
        assert_eq!(check(&[], &[b"x"], b"p", 21).unwrap().total_bytes(), 21);
        assert_eq!(check(&[], &[b"x"], b"p", 20), Err(ArgError::TooBig));
    }

    #[test]
    fn interior_nuls_and_oversized_strings_have_distinct_errors() {
        for string in [b"\0".as_slice(), b"a\0b", b"end\0"] {
            assert_eq!(check(&[string], &[], b"p", usize::MAX), Err(ArgError::InteriorNul));
            assert_eq!(check(&[b"p"], &[string], b"p", usize::MAX), Err(ArgError::InteriorNul));
            assert_eq!(check(&[b"p"], &[], string, usize::MAX), Err(ArgError::InteriorNul));
        }
        let oversized_nul = vec![0; MAX_ARG_STRLEN];
        assert_eq!(check(&[&oversized_nul], &[], b"p", usize::MAX), Err(ArgError::StringTooLong));
        assert_eq!(check(&[b"p"], &[], &oversized_nul, usize::MAX), Err(ArgError::StringTooLong));
        assert!(check(&[b"\xff\xfe"], &[b"X=\xff"], b"p", usize::MAX).is_ok());
        assert_eq!(ArgError::InteriorNul.errno(), None);
        for error in [ArgError::StringTooLong, ArgError::TooManyStrings, ArgError::TooBig] {
            assert_eq!(error.errno(), Some(Errno::TooBig));
        }
    }

    #[test]
    fn check_follows_linux_copy_order() {
        let long = vec![b'x'; MAX_ARG_STRLEN];
        assert_eq!(check(&[], &[], &long, 8), Err(ArgError::TooBig)); // pointer gate first
        assert_eq!(check(&[b"x"], &[], &long, 11), Err(ArgError::StringTooLong)); // execfn first
        assert_eq!(check(&[b"p"], &[&long, b"x"], b"p", 27), Err(ArgError::TooBig));
        assert_eq!(check(&[b"p"], &[b"x", &long], b"p", 27), Err(ArgError::StringTooLong));
        assert_eq!(check(&[&long, b"x"], &[], b"p", 19), Err(ArgError::TooBig));
        assert_eq!(check(&[b"x", &long], &[], b"p", 19), Err(ArgError::StringTooLong));
        assert_eq!(check(&[&long], &[b"x"], b"p", 19), Err(ArgError::TooBig)); // env before argv
    }

    #[test]
    fn rewrite_refunds_argv0_without_recharging_pointers() {
        let sh = parse(b"#!/bin/sh -e\n").unwrap().unwrap();
        let mut budget = ArgBudget::new(2, 0, 35).unwrap();
        for len in [3, 2, 3] {
            budget.charge(len).unwrap();
        } // execfn /s, argv a, /s
        assert_eq!(budget.rewrite(&sh, 2, Some(2)), Ok(()));
        assert_eq!(budget.pointer_bytes(), 16);
        assert_eq!(budget.string_bytes(), 19);
        assert_eq!(budget.remaining(), 0); // review's L=35 probe
        let before = (budget.pointer_bytes(), budget.string_bytes(), budget.remaining());
        assert_eq!(budget.rewrite(&sh, 2, Some(7)), Err(ArgError::TooBig));
        assert_eq!((budget.pointer_bytes(), budget.string_bytes(), budget.remaining()), before);
    }

    #[test]
    fn rewrite_limits_and_errors_leave_budget_unchanged() {
        let sh = parse(b"#!i ").unwrap().unwrap(); // Some("") still charges its NUL
        let mut budget = ArgBudget::new(0, 0, usize::MAX).unwrap();
        budget.charge(1).unwrap(); // synthetic argv[0]
        assert_eq!(budget.rewrite(&sh, 0, Some(0)), Ok(()));
        assert_eq!(budget.string_bytes(), 4); // path NUL + arg NUL + i NUL
        assert_eq!(budget.rewrite(&sh, MAX_ARG_STRLEN - 1, None), Ok(()));
        let before = budget.string_bytes();
        assert_eq!(budget.rewrite(&sh, MAX_ARG_STRLEN, None), Err(ArgError::StringTooLong));
        assert_eq!(budget.rewrite(&sh, usize::MAX, None), Err(ArgError::StringTooLong));
        assert_eq!(budget.rewrite(&sh, 0, Some(usize::MAX)), Err(ArgError::TooBig));
        assert_eq!(budget.rewrite(&sh, 0, Some(before)), Err(ArgError::TooBig));
        assert_eq!(budget.string_bytes(), before);
    }

    #[test]
    fn sizes_agree_with_initial_stack_builder() {
        let args: &[&[u8]] = &[b"p", b"", b"a\xff"];
        let envs: &[&[u8]] = &[b"A=1", b"", b"B=\xfe"];
        for base in [None, Some(0x7000_0000)] {
            let auxv = sample_auxv(base);
            for argc in 0..=args.len() {
                for envc in 0..=envs.len() {
                    let argv = normalize_argv(&args[..argc]);
                    let envp = &envs[..envc];
                    // Vary execfn across every string alignment residue as well
                    // as both pointer-table parities and both auxv layouts.
                    for path_len in 0..=17 {
                        let execfn = vec![b'x'; path_len];
                        let sizes = check(&args[..argc], envp, &execfn, usize::MAX).unwrap();
                        assert_eq!((sizes.argc(), sizes.envc()), (argv.len(), envp.len()));
                        let strings = sizes.string_bytes();
                        let table = sizes.pointer_bytes() + 3 * 8; // argc + both NULL slots
                        let aux_bytes = if base.is_some() { 15 * 16 } else { 14 * 16 };
                        let required = strings.div_ceil(16) * 16 + 16 + table.div_ceil(16) * 16 + aux_bytes;
                        let mut buf = vec![0xcc; required];
                        let built = build_initial_stack(&mut buf, USER_STACK_TOP, argv, envp, &execfn, [42; 16], &auxv)
                            .unwrap();
                        assert_eq!(built.bytes_written, required);
                        assert_eq!(built.rsp, USER_STACK_TOP - required as u64);
                        assert_eq!(u64::from_le_bytes(buf[..8].try_into().unwrap()), sizes.argc() as u64);
                        // Inspect each emitted pointer/string to catch a size
                        // estimate that matches by accident but omits an input.
                        let mut slot = 8;
                        let mut actual_string_bytes = 0;
                        for vector in [argv, envp] {
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
                        assert_eq!(actual_string_bytes + execfn.len() + 1, sizes.string_bytes());
                        assert_eq!(slot, table);
                        let mut short = vec![0xcc; required - 1];
                        assert_eq!(
                            build_initial_stack(&mut short, USER_STACK_TOP, argv, envp, &execfn, [42; 16], &auxv),
                            Err(StackBuildError::BufferTooSmall { required, available: required - 1 })
                        );
                        assert!(short.iter().all(|&b| b == 0xcc));
                    }
                }
            }
        }
    }
}
