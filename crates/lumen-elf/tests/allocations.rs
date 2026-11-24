use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

use core::num::NonZeroUsize;
use lumen_elf::{
    args::{ArgBudget, BudgetError, RewriteError, normalize_argv},
    shebang::parse,
};

struct TestAllocator;

thread_local! {
    // Per-thread, const-initialized state keeps the test harness and parallel
    // tests independent, without allocating from inside the allocator.
    static TRACK: Cell<Option<(bool, usize)>> = const { Cell::new(None) };
}

// SAFETY: successful allocations and all deallocations use System with the
// original layout; refusal returns null as GlobalAlloc's contract permits.
unsafe impl GlobalAlloc for TestAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let refuse = TRACK
            .try_with(|track| {
                if let Some((refuse, count)) = track.get() {
                    track.set(Some((refuse, count + 1)));
                    refuse
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if refuse {
            core::ptr::null_mut()
        } else {
            // SAFETY: the caller supplies a valid allocation layout.
            unsafe { System.alloc(layout) }
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: nonnull allocations came from System with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: TestAllocator = TestAllocator;

fn measured<T>(refuse: bool, operation: impl FnOnce() -> T) -> (T, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TRACK.with(|track| track.set(None));
        }
    }
    TRACK.with(|track| {
        assert!(track.get().is_none());
        track.set(Some((refuse, 0)));
    });
    let reset = Reset;
    let result = operation();
    let count = TRACK.with(|track| track.get().unwrap().1);
    drop(reset); // Re-enable allocation before assertions or formatting.
    (result, count)
}

fn charged(original: &[&[u8]], execfn: &[u8], limit: usize) -> ArgBudget {
    let mut budget = ArgBudget::new(original.len(), 0, limit).unwrap();
    for string in core::iter::once(execfn).chain(normalize_argv(original).iter().rev().copied()) {
        budget.charge(NonZeroUsize::new(string.len() + 1).unwrap()).unwrap();
    }
    budget
}

// Run refusal under a child test process: an infallible-allocation regression
// must fail the parent assertion instead of aborting the entire test binary.
fn isolated(name: &str, child: impl FnOnce()) {
    const MARKER: &str = "LUMEN_ELF_ALLOCATION_TEST_CHILD";
    if std::env::var(MARKER).as_deref() == Ok(name) {
        child();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(MARKER, name)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "allocation child failed: {:?}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn rewrite_allocation_failure_leaves_budget_unchanged() {
    isolated("rewrite_allocation_failure_leaves_budget_unchanged", || {
        for header in [b"#!/bin/sh -e\n".as_slice(), b"#!"] {
            let sh = parse(header).unwrap().unwrap();
            for original in [&[][..], &[b"old".as_slice(), b"arg"][..]] {
                let mut budget = charged(original, b"script", 1024);
                let before = (budget.string_bytes(), budget.pointer_bytes(), budget.remaining());
                let (result, calls) = measured(true, || budget.rewrite(&sh, b"script", original));
                assert_eq!(result, Err(RewriteError::NoMem));
                assert_eq!(calls, 1);
                assert_eq!((budget.string_bytes(), budget.pointer_bytes(), budget.remaining()), before);
            }
        }
    });
}

#[test]
fn rewrite_budget_error_precedes_allocation_failure() {
    isolated("rewrite_budget_error_precedes_allocation_failure", || {
        let sh = parse(b"#!").unwrap().unwrap();
        let mut budget = charged(&[b"/s"], b"/s", 14);
        let (result, calls) = measured(true, || budget.rewrite(&sh, b"/s", &[b"/s"]));
        assert_eq!(result, Err(RewriteError::Budget(BudgetError::TooBig)));
        assert_eq!(calls, 0);
        assert_eq!(budget.string_bytes(), 6);
        assert_eq!(budget.remaining(), 0);
    });
}

#[test]
fn rewrite_borrows_strings_with_one_allocation() {
    let sh = parse(b"#!/bin/sh -e\n").unwrap().unwrap();
    let original: &[&[u8]] = &[b"old", b"arg"];
    let script_path = b"script".as_slice();
    let mut budget = charged(original, script_path, 1024);
    let (result, calls) = measured(false, || budget.rewrite(&sh, script_path, original));
    let result = result.unwrap();
    assert_eq!(calls, 1);
    assert_eq!(result.capacity(), original.len() + 2);
    assert_eq!(result, [b"/bin/sh".as_slice(), b"-e", b"script", b"arg"]);
    assert!(core::ptr::eq(result[0], sh.interpreter()));
    assert!(core::ptr::eq(result[1], sh.argument().unwrap()));
    assert!(core::ptr::eq(result[2], script_path));
    assert!(core::ptr::eq(result[3], original[1]));
    let mut budget = charged(&[], script_path, 1024);
    let (empty, calls) = measured(false, || budget.rewrite(&sh, script_path, &[]));
    let empty = empty.unwrap();
    assert_eq!(calls, 1);
    assert_eq!(empty.capacity(), normalize_argv(&[]).len() + 2);
    // Empty argv plus optional argument needs all three reserved slots.
    assert_eq!(empty, [b"/bin/sh".as_slice(), b"-e", b"script"]);
}
