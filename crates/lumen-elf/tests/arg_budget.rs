use core::num::NonZeroUsize;
use lumen_elf::{
    args::{ArgBudget, BudgetError, RewriteError},
    shebang::{Shebang, parse},
};

fn copy_order<'a>(argv: &[&'a [u8]], envp: &[&'a [u8]], execfn: &'a [u8]) -> impl Iterator<Item = &'a [u8]> {
    core::iter::once(execfn)
        .chain(envp.iter().rev().copied())
        .chain(argv.iter().rev().copied())
        .chain(argv.is_empty().then_some(b"".as_slice()))
}

fn script_order<'a>(sh: &Shebang<'a>, script_path: &'a [u8]) -> impl Iterator<Item = &'a [u8]> {
    core::iter::once(script_path).chain(sh.argument()).chain(core::iter::once(sh.interpreter()))
}

// Linux's downward-growing bprm->p, with the stack limit at zero. This model
// uses signed wide arithmetic rather than ArgBudget's accumulated byte count.
struct LinuxBudget {
    p: i128,
    start: i128,
    pointers: usize,
}

impl LinuxBudget {
    fn bprm_stack_limits(argc: usize, envc: usize, limit: usize) -> Result<Self, BudgetError> {
        // Fold Linux count (argv, then envp) into the pointer-reservation model.
        if argc > 0x7fff_ffff || envc > 0x7fff_ffff {
            return Err(BudgetError::TooManyStrings);
        }
        let pointers = (argc.max(1) as u128 + envc as u128) * 8;
        if pointers >= limit as u128 {
            return Err(BudgetError::TooBig);
        }
        let start = limit as i128 - pointers as i128;
        Ok(Self { p: start, start, pointers: pointers as usize })
    }

    fn copy_string_kernel(&mut self, string: &[u8]) -> Result<(), BudgetError> {
        // strnlen_user == 0 is EFAULT in the kernel and never reaches charge.
        // These valid byte slices always have a nonzero length including NUL.
        let len = string.len() as i128 + 1;
        if len > 32 * 4096 {
            return Err(BudgetError::StringTooLong);
        }
        let next = self.p - len;
        if next < 0 {
            return Err(BudgetError::TooBig);
        }
        self.p = next;
        Ok(())
    }

    fn load_script(&mut self, sh: &Shebang<'_>, script_path: &[u8], original: &[&[u8]]) -> Result<(), BudgetError> {
        self.p += original.first().map_or(0, |arg| arg.len()) as i128 + 1; // remove_arg_zero
        for string in script_order(sh, script_path) {
            self.copy_string_kernel(string)?;
        }
        Ok(())
    }
}

fn assert_state(real: &ArgBudget, model: &LinuxBudget) {
    assert_eq!(real.pointer_bytes(), model.pointers);
    assert_eq!(real.string_bytes() as i128, model.start - model.p);
    assert_eq!(real.remaining() as i128, model.p);
}

fn compare_case<'a>(argv: &[&'a [u8]], envp: &[&'a [u8]], execfn: &'a [u8], limit: usize, rewrites: &[Shebang<'a>; 5]) {
    let real = ArgBudget::new(argv.len(), envp.len(), limit);
    let model = LinuxBudget::bprm_stack_limits(argv.len(), envp.len(), limit);
    let (mut real, mut model) = match (real, model) {
        (Ok(real), Ok(model)) => (real, model),
        (Err(real), Err(model)) => {
            assert_eq!(real, model);
            return;
        }
        _ => panic!("pointer gate disagrees for limit {limit}"),
    };
    assert_state(&real, &model);
    for string in copy_order(argv, envp, execfn) {
        let before = (real.string_bytes(), real.remaining());
        let actual = real.charge(NonZeroUsize::new(string.len() + 1).unwrap());
        assert_eq!(actual, model.copy_string_kernel(string));
        if actual.is_err() {
            assert_eq!((real.string_bytes(), real.remaining()), before);
            return;
        }
        assert_state(&real, &model);
    }
    let mut current = argv.to_vec();
    let mut script_path = execfn;
    for sh in rewrites {
        let before = (real.string_bytes(), real.remaining());
        let actual = real.rewrite(sh, script_path, &current);
        let status = actual.as_ref().map(|_| ()).map_err(|error| match error {
            RewriteError::Budget(error) => *error,
            RewriteError::NoMem => panic!("unexpected allocation failure in model comparison"),
        });
        assert_eq!(status, model.load_script(sh, script_path, &current));
        if status.is_err() {
            assert_eq!((real.string_bytes(), real.remaining()), before);
            return;
        }
        let expected: Vec<_> = core::iter::once(sh.interpreter())
            .chain(sh.argument())
            .chain(core::iter::once(script_path))
            .chain(current.iter().skip(1).copied())
            .collect();
        current = actual.unwrap();
        assert_eq!(current, expected);
        assert_state(&real, &model);
        if sh.open_error().is_some() {
            return;
        } // EACCES only after successful charging/allocation.
        script_path = sh.interpreter();
    }
}

#[test]
fn budget_matches_linux_model_at_copy_and_rewrite_boundaries() {
    fn next(seed: &mut u64) -> usize {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed as usize
    }
    let storage = vec![b'x'; 131_072];
    let lengths = [0, 1, 2, 7, 16, 131_070, 131_071, 131_072];
    let headers: [&[u8]; 5] = [b"#!i\n", b"#!/bin/sh -e\n", b"#!/bin/interp ", b"#!env one two\n", b"#!"];
    let mut seed = 0x0034_3367_5eed_u64;
    let mut checked = 0;
    for _ in 0..256 {
        let argc = next(&mut seed) % 5;
        let envc = next(&mut seed) % 5;
        let argv = core::array::from_fn::<_, 4, _>(|_| &storage[..lengths[next(&mut seed) % lengths.len()]]);
        let envp = core::array::from_fn::<_, 4, _>(|_| &storage[..lengths[next(&mut seed) % lengths.len()]]);
        let execfn = &storage[..lengths[next(&mut seed) % lengths.len()]];
        let rewrites = core::array::from_fn(|_| parse(headers[next(&mut seed) % headers.len()]).unwrap().unwrap());
        let argv = &argv[..argc];
        let envp = &envp[..envc];
        let pointers = (argc.max(1) + envc) * 8;
        // Compare just below, exactly at, and just above every copying threshold.
        let mut thresholds = vec![pointers];
        let mut total = pointers;
        for string in copy_order(argv, envp, execfn) {
            total += string.len() + 1;
            thresholds.push(total);
        }
        let mut argv0 = argv.first().copied().unwrap_or(b"");
        let mut script_path = execfn;
        for sh in &rewrites {
            total -= argv0.len() + 1;
            for string in script_order(sh, script_path) {
                total += string.len() + 1;
                thresholds.push(total);
            }
            if sh.open_error().is_some() {
                break;
            }
            argv0 = sh.interpreter();
            script_path = sh.interpreter();
        }
        for boundary in thresholds {
            for limit in [boundary.saturating_sub(1), boundary, boundary + 1] {
                compare_case(argv, envp, execfn, limit, &rewrites);
                checked += 1;
            }
        }
        compare_case(argv, envp, execfn, usize::MAX, &rewrites);
    }
    assert!(checked > 8_000);
}

#[test]
fn budget_model_pins_execfn_and_rewrite_review_probes() {
    // Fixed expectations check the model itself as well as the implementation.
    let mut real = ArgBudget::new(2, 0, 25).unwrap();
    let mut model = LinuxBudget::bprm_stack_limits(2, 0, 25).unwrap();
    assert_eq!(real.charge(NonZeroUsize::new(14).unwrap()), Err(BudgetError::TooBig));
    assert_eq!(model.copy_string_kernel(b"/usr/bin/prog"), Err(BudgetError::TooBig));

    let sh = parse(b"#!/bin/sh -e\n").unwrap().unwrap();
    let mut real = ArgBudget::new(2, 0, 35).unwrap();
    let mut model = LinuxBudget::bprm_stack_limits(2, 0, 35).unwrap();
    for string in copy_order(&[b"/s", b"a"], &[], b"/s") {
        real.charge(NonZeroUsize::new(string.len() + 1).unwrap()).unwrap();
        model.copy_string_kernel(string).unwrap();
    }
    assert_eq!(real.rewrite(&sh, b"/s", &[b"/s", b"a"]).unwrap(), [b"/bin/sh".as_slice(), b"-e", b"/s", b"a"]);
    assert_eq!(model.load_script(&sh, b"/s", &[b"/s", b"a"]), Ok(()));
    assert_state(&real, &model);
    assert_eq!(real.string_bytes(), 19);
    assert_eq!(real.remaining(), 0);

    let empty = parse(b"#!").unwrap().unwrap();
    for limit in [14, 15] {
        let mut real = ArgBudget::new(1, 0, limit).unwrap();
        let mut model = LinuxBudget::bprm_stack_limits(1, 0, limit).unwrap();
        for string in copy_order(&[b"/s"], &[], b"/s") {
            real.charge(NonZeroUsize::new(string.len() + 1).unwrap()).unwrap();
            model.copy_string_kernel(string).unwrap();
        }
        let actual = real.rewrite(&empty, b"/s", &[b"/s"]);
        if limit == 14 {
            assert_eq!(actual, Err(RewriteError::Budget(BudgetError::TooBig)));
            assert_eq!(model.load_script(&empty, b"/s", &[b"/s"]), Err(BudgetError::TooBig));
            assert_eq!(real.string_bytes(), 6);
        } else {
            assert_eq!(actual.unwrap(), [b"".as_slice(), b"/s"]);
            assert_eq!(model.load_script(&empty, b"/s", &[b"/s"]), Ok(()));
            assert_state(&real, &model);
            assert_eq!(real.string_bytes(), 7);
            assert!(empty.open_error().is_some());
        }
    }
}
