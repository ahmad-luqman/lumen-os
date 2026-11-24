use lumen_elf::{
    args::{ArgBudget, ArgError},
    shebang::{Shebang, parse},
};

// Linux's downward-growing bprm->p, with the stack limit at zero. This model
// uses signed wide arithmetic rather than ArgBudget's accumulated byte count.
struct LinuxBudget {
    p: i128,
    start: i128,
    pointers: usize,
}

impl LinuxBudget {
    fn bprm_stack_limits(argc: usize, envc: usize, limit: usize) -> Result<Self, ArgError> {
        if argc > 0x7fff_ffff || envc > 0x7fff_ffff {
            return Err(ArgError::TooManyStrings);
        }
        let pointers = (argc.max(1) as u128 + envc as u128) * 8;
        if pointers >= limit as u128 {
            return Err(ArgError::TooBig);
        }
        let start = limit as i128 - pointers as i128;
        Ok(Self { p: start, start, pointers: pointers as usize })
    }

    fn copy_string_kernel(&mut self, len_with_nul: usize) -> Result<(), ArgError> {
        if len_with_nul > 32 * 4096 {
            return Err(ArgError::StringTooLong);
        }
        let next = self.p - len_with_nul as i128;
        if len_with_nul == 0 || next < 0 {
            return Err(ArgError::TooBig);
        }
        self.p = next;
        Ok(())
    }

    fn load_script(&mut self, sh: &Shebang<'_>, path_len: usize, argv0_len: usize) -> Result<(), ArgError> {
        self.p += argv0_len as i128 + 1; // remove_arg_zero: includes its NUL
        self.copy_string_kernel(path_len + 1)?;
        if let Some(argument) = sh.argument() {
            self.copy_string_kernel(argument.len() + 1)?;
        }
        self.copy_string_kernel(sh.interpreter().len() + 1)
    }
}

fn assert_state(real: &ArgBudget, model: &LinuxBudget) {
    assert_eq!(real.pointer_bytes(), model.pointers);
    assert_eq!(real.string_bytes() as i128, model.start - model.p);
    assert_eq!(real.remaining() as i128, model.p);
}

fn compare_case(argv: &[usize], envp: &[usize], execfn: usize, limit: usize, rewrites: &[(Shebang<'_>, usize); 5]) {
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
    // do_execveat_common: execfn, envp reversed, argv reversed, then a
    // synthetic empty argv[0] only if the original vector was empty.
    let lengths = core::iter::once(execfn)
        .chain(envp.iter().rev().copied())
        .chain(argv.iter().rev().copied())
        .chain(argv.is_empty().then_some(0));
    for len in lengths {
        let before = (real.string_bytes(), real.remaining());
        let actual = real.charge(len + 1);
        assert_eq!(actual, model.copy_string_kernel(len + 1));
        if actual.is_err() {
            assert_eq!((real.string_bytes(), real.remaining()), before);
            return;
        }
        assert_state(&real, &model);
    }
    let mut argv0 = argv.first().copied().unwrap_or(0);
    for (sh, path_len) in rewrites {
        let before = (real.string_bytes(), real.remaining());
        let actual = real.rewrite(sh, *path_len, Some(argv0));
        assert_eq!(actual, model.load_script(sh, *path_len, argv0));
        if actual.is_err() {
            // Linux abandons exec on failure. Our transaction also preserves
            // the original budget, rather than exposing partial refunds.
            assert_eq!((real.string_bytes(), real.remaining()), before);
            return;
        }
        assert_state(&real, &model);
        argv0 = sh.interpreter().len();
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
    let lengths = [0, 1, 2, 7, 16, 131_070, 131_071, 131_072];
    let headers: [&[u8]; 4] = [b"#!i\n", b"#!/bin/sh -e\n", b"#!/bin/interp ", b"#!env one two\n"];
    let mut seed = 0x0034_3367_5eed_u64;
    let mut checked = 0;
    for _ in 0..256 {
        let argc = next(&mut seed) % 5;
        let envc = next(&mut seed) % 5;
        let argv = core::array::from_fn::<_, 4, _>(|_| lengths[next(&mut seed) % lengths.len()]);
        let envp = core::array::from_fn::<_, 4, _>(|_| lengths[next(&mut seed) % lengths.len()]);
        let execfn = lengths[next(&mut seed) % lengths.len()];
        let rewrites = core::array::from_fn(|_| {
            (
                parse(headers[next(&mut seed) % headers.len()]).unwrap().unwrap(),
                lengths[next(&mut seed) % lengths.len()],
            )
        });
        let argv = &argv[..argc];
        let envp = &envp[..envc];
        let pointers = (argc.max(1) + envc) * 8;
        // Generate thresholds at every copy and rewrite, then compare just
        // below, exactly at, and just above each threshold.
        let mut thresholds = vec![pointers];
        let mut total = pointers;
        for len in core::iter::once(execfn)
            .chain(envp.iter().rev().copied())
            .chain(argv.iter().rev().copied())
            .chain(argv.is_empty().then_some(0))
        {
            total += len + 1;
            thresholds.push(total);
        }
        let mut argv0 = argv.first().copied().unwrap_or(0);
        for (sh, path_len) in &rewrites {
            total -= argv0 + 1;
            for len in core::iter::once(*path_len)
                .chain(sh.argument().map(<[u8]>::len))
                .chain(core::iter::once(sh.interpreter().len()))
            {
                total += len + 1;
                thresholds.push(total);
            }
            argv0 = sh.interpreter().len();
        }
        for boundary in thresholds {
            for limit in [boundary.saturating_sub(1), boundary, boundary + 1] {
                compare_case(argv, envp, execfn, limit, &rewrites);
                checked += 1;
            }
        }
        compare_case(argv, envp, execfn, usize::MAX, &rewrites);
    }
    assert!(checked > 10_000);
}

#[test]
fn budget_model_pins_execfn_and_rewrite_review_probes() {
    let mut real = ArgBudget::new(2, 0, 25).unwrap();
    let mut model = LinuxBudget::bprm_stack_limits(2, 0, 25).unwrap();
    assert_eq!(real.charge(b"/usr/bin/prog".len() + 1), Err(ArgError::TooBig));
    assert_eq!(model.copy_string_kernel(14), Err(ArgError::TooBig));

    let sh = parse(b"#!/bin/sh -e\n").unwrap().unwrap();
    let mut real = ArgBudget::new(2, 0, 35).unwrap();
    let mut model = LinuxBudget::bprm_stack_limits(2, 0, 35).unwrap();
    for len in [3, 2, 3] {
        real.charge(len).unwrap();
        model.copy_string_kernel(len).unwrap();
    }
    assert_eq!(real.rewrite(&sh, 2, Some(2)), Ok(()));
    assert_eq!(model.load_script(&sh, 2, 2), Ok(()));
    assert_state(&real, &model);
    assert_eq!(real.remaining(), 0);
}
