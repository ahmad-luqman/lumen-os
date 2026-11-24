//! Incremental exec argument accounting, before copying each user string.
//!
//! The original pointer reservation survives shebang substitutions; execfn
//! and each copied string share the remaining byte budget.

use crate::{loader::PAGE_SIZE, shebang::Shebang};
use alloc::vec::Vec;
use core::{fmt, num::NonZeroUsize};

/// Kernel-facing errno categories, independent of any syscall ABI crate.
/// EFAULT (`strnlen_user == 0`) and ENOENT (`PATH_INACCESSIBLE`) are the
/// kernel's responsibility; neither is produced by this byte-logic API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Errno {
    /// ENOEXEC: invalid executable/script header.
    NoExec,
    /// EACCES: the interpreter path resolves to a directory.
    Access,
    /// ELOOP: too many interpreter substitutions.
    Loop,
    /// E2BIG: argument counts, string lengths, or byte budget exceeded.
    TooBig,
    /// ENOMEM: allocation for the rewritten argument vector failed.
    NoMem,
}

/// Maximum string storage including its NUL: 32 x86-64 pages (128 KiB).
/// Linux `include/uapi/linux/binfmts.h`, `MAX_ARG_STRLEN`.
pub const MAX_ARG_STRLEN: usize = 32 * PAGE_SIZE as usize;

/// argv and envp counts are each limited to MAX_ARG_STRINGS, apart from the
/// byte budget. Linux `include/uapi/linux/binfmts.h`, `MAX_ARG_STRINGS`.
pub const MAX_ARG_STRINGS: usize = 0x7fff_ffff;

/// Exec size failures, all mapped to E2BIG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetError {
    /// One string plus its NUL exceeds [`MAX_ARG_STRLEN`].
    StringTooLong,
    /// Either vector exceeds [`MAX_ARG_STRINGS`].
    TooManyStrings,
    /// The byte budget is exceeded or a refund exceeds the charged strings.
    TooBig,
}

impl BudgetError {
    pub const fn errno(&self) -> Errno {
        match self {
            Self::StringTooLong | Self::TooManyStrings | Self::TooBig => Errno::TooBig,
        }
    }
}

impl fmt::Display for BudgetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::StringTooLong => "exec string exceeds MAX_ARG_STRLEN",
            Self::TooManyStrings => "exec vector exceeds MAX_ARG_STRINGS",
            Self::TooBig => "exec argument byte budget exceeded or invalid refund",
        })
    }
}

/// Failures specific to the whole-slice convenience [`check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckError {
    Budget(BudgetError),
    /// Caller bug: a slice contains a C terminator. Properly extracted user
    /// C strings never contain interior NULs, so this has no syscall errno.
    InteriorNul,
}

impl CheckError {
    pub const fn errno(&self) -> Option<Errno> {
        match self {
            Self::Budget(error) => Some(error.errno()),
            Self::InteriorNul => None,
        }
    }
}

impl From<BudgetError> for CheckError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Budget(error) => error.fmt(f),
            Self::InteriorNul => f.write_str("exec string contains an interior NUL byte"),
        }
    }
}

/// Rewriting charges all strings before attempting allocation: E2BIG precedes ENOMEM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteError {
    Budget(BudgetError),
    NoMem,
}

impl RewriteError {
    pub const fn errno(&self) -> Errno {
        match self {
            Self::Budget(error) => error.errno(),
            Self::NoMem => Errno::NoMem,
        }
    }
}

impl From<BudgetError> for RewriteError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl fmt::Display for RewriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Budget(error) => error.fmt(f),
            Self::NoMem => f.write_str("cannot allocate rewritten exec argument vector"),
        }
    }
}

/// An exec's original pointer reservation and incrementally charged strings.
///
/// Follow Linux `fs/exec.c::do_execveat_common` in this order:
/// 1. Linux `count` (argv, then envp), then `bprm_stack_limits`; [`Self::new`]
///    checks those counts and reserves pointers before any string copying.
/// 2. Charge execfn (`copy_string_kernel(bprm->filename)`).
/// 3. Charge envp, last to first (`copy_strings`).
/// 4. Charge argv, last to first (`copy_strings`).
/// 5. If the original argc was zero, charge one byte for the synthetic "".
///
/// Pass each `strnlen_user` length including NUL to [`Self::charge`] before
/// copying that string. The caller chooses the byte limit from its stack
/// policy. Neither the budget nor its state can be cloned or copied.
///
/// ```compile_fail
/// use lumen_elf::args::ArgBudget;
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<ArgBudget>();
/// ```
///
/// ```compile_fail
/// use lumen_elf::args::ArgBudget;
/// fn requires_copy<T: Copy>() {}
/// requires_copy::<ArgBudget>();
/// ```
#[must_use]
#[derive(Debug)]
pub struct ArgBudget {
    pointer_bytes: usize,
    string_bytes: usize,
    total_limit: usize,
}

impl ArgBudget {
    /// Each count is checked against `MAX_ARG_STRINGS` (Linux `count`) before
    /// the pointer reservation `(max(argc, 1) + envc) * 8`. A limit no greater
    /// than the pointer reservation fails with E2BIG.
    pub fn new(argc: usize, envc: usize, total_limit: usize) -> Result<Self, BudgetError> {
        if argc > MAX_ARG_STRINGS || envc > MAX_ARG_STRINGS {
            return Err(BudgetError::TooManyStrings);
        }
        let pointer_bytes = argc.max(1).checked_add(envc).and_then(|n| n.checked_mul(8)).ok_or(BudgetError::TooBig)?;
        if total_limit <= pointer_bytes {
            return Err(BudgetError::TooBig);
        }
        Ok(Self { pointer_bytes, string_bytes: 0, total_limit })
    }

    /// Charge a C string's length including its NUL before copying it.
    /// The kernel must turn `strnlen_user == 0` into EFAULT before calling
    /// this method. Errors leave the budget unchanged.
    pub fn charge(&mut self, len_with_nul: NonZeroUsize) -> Result<(), BudgetError> {
        let len = len_with_nul.get();
        if len > MAX_ARG_STRLEN {
            return Err(BudgetError::StringTooLong);
        }
        if len > self.remaining() {
            return Err(BudgetError::TooBig);
        }
        self.string_bytes += len;
        Ok(())
    }

    /// Refund normalized argv[0], charge the replacement strings, and borrow
    /// `[interpreter, argument?, script_path, original[1..]...]` in one transaction.
    /// No new pointer bytes are charged. All failures leave the budget unchanged.
    ///
    /// `original` must be the argv already charged to this budget, including
    /// the synthetic empty argv[0]'s byte when the original argc was zero.
    /// `original` and `script_path` are NUL-free C-string contents.
    /// `script_path` is Linux's `bprm->interp`: execfn for the first script,
    /// then the previous `#!` line's interpreter string, exactly as written.
    ///
    /// Linux `remove_arg_zero` advances `bprm->p` past argv[0] and its NUL,
    /// refunding those bytes to `bprm_hit_stack_limit`. `load_script` then
    /// copies script path, optional argument, and interpreter in that order.
    /// Only after charging does this method allocate the pointer vector,
    /// preserving E2BIG before ENOMEM (Linux charges before `bprm_change_interp`).
    pub fn rewrite<'a>(
        &mut self,
        sh: &Shebang<'a>,
        script_path: &'a [u8],
        original: &[&'a [u8]],
    ) -> Result<Vec<&'a [u8]>, RewriteError> {
        let original = normalize_argv(original);
        let refund = original[0].len().checked_add(1).ok_or(BudgetError::TooBig)?;
        let mut next =
            Self { string_bytes: self.string_bytes.checked_sub(refund).ok_or(BudgetError::TooBig)?, ..*self };
        for string in core::iter::once(script_path).chain(sh.argument()).chain(core::iter::once(sh.interpreter())) {
            next.charge(string_length(string)?)?;
        }
        let mut result = Vec::new();
        // A real slice's len() + 2 cannot overflow. An empty original normalizes
        // to one element, so three slots suffice even with an optional argument.
        result.try_reserve_exact(original.len() + 2).map_err(|_| RewriteError::NoMem)?;
        result.push(sh.interpreter());
        if let Some(argument) = sh.argument() {
            result.push(argument);
        }
        result.push(script_path);
        result.extend_from_slice(&original[1..]);
        *self = next;
        Ok(result)
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
/// Each string's length is checked before its interior NUL.
/// Use [`normalize_argv`] on the same argv when calling the stack builder.
pub fn check(argv: &[&[u8]], envp: &[&[u8]], execfn: &[u8], total_limit: usize) -> Result<ArgSizes, CheckError> {
    let argv = normalize_argv(argv);
    let mut budget = ArgBudget::new(argv.len(), envp.len(), total_limit)?;
    for string in core::iter::once(execfn).chain(envp.iter().rev().copied()).chain(argv.iter().rev().copied()) {
        let len = string_length(string)?;
        if string.contains(&0) {
            return Err(CheckError::InteriorNul);
        }
        budget.charge(len)?;
    }
    Ok(ArgSizes {
        argc: argv.len(),
        envc: envp.len(),
        string_bytes: budget.string_bytes,
        pointer_bytes: budget.pointer_bytes,
    })
}

fn string_length(string: &[u8]) -> Result<NonZeroUsize, BudgetError> {
    if string.len() >= MAX_ARG_STRLEN {
        return Err(BudgetError::StringTooLong);
    }
    NonZeroUsize::MIN.checked_add(string.len()).ok_or(BudgetError::StringTooLong)
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

    fn nz(len: usize) -> NonZeroUsize {
        NonZeroUsize::new(len).unwrap()
    }

    fn charged(argv: &[&[u8]], execfn: &[u8], limit: usize) -> ArgBudget {
        let mut budget = ArgBudget::new(argv.len(), 0, limit).unwrap();
        for string in core::iter::once(execfn).chain(normalize_argv(argv).iter().rev().copied()) {
            budget.charge(nz(string.len() + 1)).unwrap();
        }
        budget
    }

    #[test]
    fn string_length_limit_includes_nul() {
        assert_eq!(MAX_ARG_STRLEN, 131_072);
        let at_limit = vec![b'a'; MAX_ARG_STRLEN - 1];
        let past_limit = vec![b'a'; MAX_ARG_STRLEN];
        assert_eq!(check(&[&at_limit], &[], b"p", usize::MAX).unwrap().string_bytes(), MAX_ARG_STRLEN + 2);
        assert_eq!(check(&[b"p"], &[&at_limit], b"p", usize::MAX).unwrap().string_bytes(), MAX_ARG_STRLEN + 4);
        assert_eq!(check(&[&past_limit], &[], b"p", usize::MAX), Err(CheckError::Budget(BudgetError::StringTooLong)));
        assert_eq!(
            check(&[b"p"], &[&past_limit], b"p", usize::MAX),
            Err(CheckError::Budget(BudgetError::StringTooLong))
        );
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
        assert_eq!(ArgBudget::new(MAX_ARG_STRINGS + 1, 0, usize::MAX).unwrap_err(), BudgetError::TooManyStrings);
        assert_eq!(ArgBudget::new(0, MAX_ARG_STRINGS + 1, usize::MAX).unwrap_err(), BudgetError::TooManyStrings);
        assert_eq!(ArgBudget::new(usize::MAX, 0, usize::MAX).unwrap_err(), BudgetError::TooManyStrings);
        assert_eq!(ArgBudget::new(0, usize::MAX, usize::MAX).unwrap_err(), BudgetError::TooManyStrings);
    }

    #[test]
    fn budget_reserves_pointers_before_charging_strings() {
        assert_eq!(ArgBudget::new(0, 0, 8).unwrap_err(), BudgetError::TooBig);
        assert_eq!(ArgBudget::new(1, 1, 16).unwrap_err(), BudgetError::TooBig);
        let mut budget = ArgBudget::new(1, 1, 17).unwrap();
        assert_eq!(budget.pointer_bytes(), 16);
        assert_eq!(budget.remaining(), 1);
        assert_eq!(budget.charge(nz(1)), Ok(()));
        assert_eq!(budget.remaining(), 0);
        assert_eq!(budget.charge(nz(1)), Err(BudgetError::TooBig));
        assert_eq!(budget.string_bytes(), 1);
        assert_eq!(budget.charge(nz(MAX_ARG_STRLEN + 1)), Err(BudgetError::StringTooLong));
        let mut large = ArgBudget::new(1, 0, usize::MAX).unwrap();
        assert_eq!(large.charge(nz(MAX_ARG_STRLEN)), Ok(()));
        assert_eq!(large.charge(nz(MAX_ARG_STRLEN + 1)), Err(BudgetError::StringTooLong));
        assert_eq!(large.charge(nz(usize::MAX)), Err(BudgetError::StringTooLong));
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
        assert_eq!(check(argv, envp, b"/p", 33), Err(CheckError::Budget(BudgetError::TooBig)));
        assert_eq!(check(&[b"p", b"x"], envp, b"/p", 34), Err(CheckError::Budget(BudgetError::TooBig)));
        assert_eq!(check(argv, &[b"K=Vx"], b"/p", 34), Err(CheckError::Budget(BudgetError::TooBig)));
        assert_eq!(check(argv, envp, b"/px", 34), Err(CheckError::Budget(BudgetError::TooBig)));
        // Review probe: argv alone fits 25, but execfn shares the budget.
        assert_eq!(check(&[b"prog", b"arg"], &[], b"/usr/bin/prog", 25), Err(CheckError::Budget(BudgetError::TooBig)));
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
        assert_eq!(check(&[], &[], b"p", 10), Err(CheckError::Budget(BudgetError::TooBig)));
        assert_eq!(check(&[], &[], b"p", 0), Err(CheckError::Budget(BudgetError::TooBig)));
        assert_eq!(check(&[], &[b"x"], b"p", 21).unwrap().total_bytes(), 21);
        assert_eq!(check(&[], &[b"x"], b"p", 20), Err(CheckError::Budget(BudgetError::TooBig)));
    }

    #[test]
    fn interior_nuls_and_oversized_strings_have_distinct_errors() {
        for string in [b"\0".as_slice(), b"a\0b", b"end\0"] {
            assert_eq!(check(&[string], &[], b"p", usize::MAX), Err(CheckError::InteriorNul));
            assert_eq!(check(&[b"p"], &[string], b"p", usize::MAX), Err(CheckError::InteriorNul));
            assert_eq!(check(&[b"p"], &[], string, usize::MAX), Err(CheckError::InteriorNul));
        }
        let oversized_nul = vec![0; MAX_ARG_STRLEN];
        assert_eq!(
            check(&[&oversized_nul], &[], b"p", usize::MAX),
            Err(CheckError::Budget(BudgetError::StringTooLong))
        );
        assert_eq!(
            check(&[b"p"], &[], &oversized_nul, usize::MAX),
            Err(CheckError::Budget(BudgetError::StringTooLong))
        );
        assert!(check(&[b"\xff\xfe"], &[b"X=\xff"], b"p", usize::MAX).is_ok());
        assert_eq!(CheckError::InteriorNul.errno(), None);
        assert_eq!(RewriteError::NoMem.errno(), Errno::NoMem);
        for error in [BudgetError::StringTooLong, BudgetError::TooManyStrings, BudgetError::TooBig] {
            assert_eq!(error.errno(), Errno::TooBig);
            assert_eq!(CheckError::Budget(error).errno(), Some(Errno::TooBig));
            assert_eq!(RewriteError::Budget(error).errno(), Errno::TooBig);
        }
    }

    #[test]
    fn check_follows_linux_copy_order() {
        let long = vec![b'x'; MAX_ARG_STRLEN];
        assert_eq!(check(&[], &[], &long, 8), Err(CheckError::Budget(BudgetError::TooBig))); // pointer gate first
        assert_eq!(check(&[b"x"], &[], &long, 11), Err(CheckError::Budget(BudgetError::StringTooLong))); // execfn first
        assert_eq!(check(&[b"p"], &[&long, b"x"], b"p", 27), Err(CheckError::Budget(BudgetError::TooBig)));
        assert_eq!(check(&[b"p"], &[b"x", &long], b"p", 27), Err(CheckError::Budget(BudgetError::StringTooLong)));
        assert_eq!(check(&[&long, b"x"], &[], b"p", 19), Err(CheckError::Budget(BudgetError::TooBig)));
        assert_eq!(check(&[b"x", &long], &[], b"p", 19), Err(CheckError::Budget(BudgetError::StringTooLong)));
        assert_eq!(check(&[&long], &[b"x"], b"p", 19), Err(CheckError::Budget(BudgetError::TooBig))); // env before argv
    }

    #[test]
    fn rewrite_refunds_argv0_without_recharging_pointers() {
        let sh = parse(b"#!/bin/sh -e\n").unwrap().unwrap();
        let mut budget = charged(&[b"/s", b"a"], b"/s", 35);
        let current = budget.rewrite(&sh, b"/s", &[b"/s", b"a"]).unwrap();
        assert_eq!(current, [b"/bin/sh".as_slice(), b"-e", b"/s", b"a"]);
        assert_eq!(budget.pointer_bytes(), 16);
        assert_eq!(budget.string_bytes(), 19);
        assert_eq!(budget.remaining(), 0); // review's L=35 probe
        let before = (budget.pointer_bytes(), budget.string_bytes(), budget.remaining());
        assert_eq!(budget.rewrite(&sh, sh.interpreter(), &current), Err(RewriteError::Budget(BudgetError::TooBig)));
        assert_eq!((budget.pointer_bytes(), budget.string_bytes(), budget.remaining()), before);
    }

    #[test]
    fn rewrite_can_reassign_argv_across_three_scripts() {
        let headers: [&[u8]; 3] = [b"#!i -x\n", b"#!long\n", b"#!z "];
        let expected: [&[&[u8]]; 3] = [
            &[b"i", b"-x", b"/s", b"a"],
            &[b"long", b"i", b"-x", b"/s", b"a"],
            &[b"z", b"", b"long", b"i", b"-x", b"/s", b"a"],
        ];
        let mut current: Vec<&[u8]> = vec![b"alias", b"a"];
        let mut budget = charged(&current, b"/s", 37);
        assert_eq!(budget.string_bytes(), 11);
        let mut script_path = b"/s".as_slice();
        for (index, header) in headers.iter().enumerate() {
            let sh = parse(header).unwrap().unwrap();
            current = budget.rewrite(&sh, script_path, &current).unwrap();
            assert_eq!(current, expected[index]);
            assert_eq!(budget.string_bytes(), [13, 18, 21][index]);
            assert_eq!(budget.pointer_bytes(), 16);
            script_path = sh.interpreter();
        }
        assert_eq!(budget.remaining(), 0);
    }

    #[test]
    fn rewrite_with_and_without_optional_argument() {
        let original: &[&[u8]] = &[b"discarded", b"one", b"", b"two\xff"];
        let sh = parse(b"#!/bin/interp\n").unwrap().unwrap();
        assert_eq!(
            charged(original, b"script path", 1024).rewrite(&sh, b"script path", original).unwrap(),
            [b"/bin/interp".as_slice(), b"script path", b"one", b"", b"two\xff"]
        );
        let sh = parse(b"#!/bin/interp -a -b\n").unwrap().unwrap();
        assert_eq!(
            charged(original, b"script path", 1024).rewrite(&sh, b"script path", original).unwrap(),
            [b"/bin/interp".as_slice(), b"-a -b", b"script path", b"one", b"", b"two\xff"]
        );
        let sh = parse(b"#!/bin/interp ").unwrap().unwrap();
        assert_eq!(
            charged(original, b"script path", 1024).rewrite(&sh, b"script path", original).unwrap(),
            [b"/bin/interp".as_slice(), b"", b"script path", b"one", b"", b"two\xff"]
        );
    }

    #[test]
    fn rewrite_with_empty_original_refunds_synthetic_nul() {
        let plain = parse(b"#!/bin/sh\n").unwrap().unwrap();
        let option = parse(b"#!/bin/sh -e\n").unwrap().unwrap();
        let empty_option = parse(b"#!/bin/sh ").unwrap().unwrap();
        for original in [&[][..], &[b"".as_slice()][..]] {
            let mut budget = charged(original, b"script", 30);
            assert_eq!(budget.string_bytes(), 8);
            assert_eq!(budget.rewrite(&plain, b"script", original).unwrap(), [b"/bin/sh".as_slice(), b"script"]);
            assert_eq!(budget.string_bytes(), 22);
            let mut budget = charged(original, b"script", 33);
            assert_eq!(
                budget.rewrite(&option, b"script", original).unwrap(),
                [b"/bin/sh".as_slice(), b"-e", b"script"]
            );
            assert_eq!(budget.string_bytes(), 25);
            let mut budget = charged(original, b"script", 31);
            assert_eq!(
                budget.rewrite(&empty_option, b"script", original).unwrap(),
                [b"/bin/sh".as_slice(), b"", b"script"]
            );
            assert_eq!(budget.string_bytes(), 23);
        }
    }

    #[test]
    fn empty_interpreter_budget_error_precedes_open_error() {
        let sh = parse(b"#!").unwrap().unwrap();
        let mut tight = charged(&[b"/s"], b"/s", 14);
        assert_eq!(tight.rewrite(&sh, b"/s", &[b"/s"]), Err(RewriteError::Budget(BudgetError::TooBig)));
        assert_eq!(tight.string_bytes(), 6);
        let mut room = charged(&[b"/s"], b"/s", 15);
        assert_eq!(room.rewrite(&sh, b"/s", &[b"/s"]).unwrap(), [b"".as_slice(), b"/s"]);
        assert_eq!(room.string_bytes(), 7);
        assert_eq!(room.remaining(), 0);
        assert_eq!(sh.open_error().unwrap().errno(), Errno::Access);
    }

    #[test]
    fn rewrite_limits_and_errors_leave_budget_unchanged() {
        let sh = parse(b"#!i ").unwrap().unwrap(); // Some("") still charges its NUL
        let at_limit = vec![b'x'; MAX_ARG_STRLEN - 1];
        let past_limit = vec![b'x'; MAX_ARG_STRLEN];
        let mut budget = charged(&[b"i"], &at_limit, usize::MAX);
        let current = budget.rewrite(&sh, &at_limit, &[b"i"]).unwrap();
        assert_eq!(budget.string_bytes(), 2 * MAX_ARG_STRLEN + 3);
        let before = (budget.string_bytes(), budget.remaining());
        assert_eq!(budget.rewrite(&sh, &past_limit, &current), Err(RewriteError::Budget(BudgetError::StringTooLong)));
        assert_eq!((budget.string_bytes(), budget.remaining()), before);
        let mut uncharged = ArgBudget::new(1, 0, 100).unwrap();
        assert_eq!(uncharged.rewrite(&sh, b"script", &[b"i"]), Err(RewriteError::Budget(BudgetError::TooBig)));
        assert_eq!(uncharged.string_bytes(), 0);
    }

    #[test]
    fn sizes_agree_with_initial_stack_builder() {
        let args: &[&[u8]] = &[b"p", b"", b"a\xff"];
        let envs: &[&[u8]] = &[b"A=1", b"", b"B=\xfe"];
        for base in [None, Some(0x7000_0000)] {
            let auxv = sample_auxv(base);
            let aux_bytes = if base.is_some() { 15 * 16 } else { 14 * 16 };
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
