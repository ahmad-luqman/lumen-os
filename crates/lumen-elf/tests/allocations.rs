use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

use lumen_elf::{
    args::normalize_argv,
    shebang::{argv, parse},
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

#[test]
fn argv_allocation_failure_returns_error() {
    let sh = parse(b"#!/bin/sh -e\n").unwrap().unwrap();
    for original in [&[][..], &[b"old".as_slice(), b"arg"][..]] {
        let (result, calls) = measured(true, || argv(&sh, b"script", original));
        assert!(result.is_err());
        assert_eq!(calls, 1);
    }
}

#[test]
fn argv_borrows_strings_with_one_allocation() {
    let sh = parse(b"#!/bin/sh -e\n").unwrap().unwrap();
    let original: &[&[u8]] = &[b"old", b"arg"];
    let script_path = b"script".as_slice();
    let (result, calls) = measured(false, || argv(&sh, script_path, original));
    let result = result.unwrap();
    assert_eq!(calls, 1);
    assert_eq!(result, [b"/bin/sh".as_slice(), b"-e", b"script", b"arg"]);
    assert!(core::ptr::eq(result[0], sh.interpreter()));
    assert!(core::ptr::eq(result[1], sh.argument().unwrap()));
    assert!(core::ptr::eq(result[2], script_path));
    assert!(core::ptr::eq(result[3], original[1]));
    let (empty, calls) = measured(false, || argv(&sh, b"script", &[]));
    assert_eq!(calls, 1); // Empty argv plus optional argument still needs three slots.
    assert_eq!(empty.unwrap(), [b"/bin/sh".as_slice(), b"-e", b"script"]);
}

#[test]
fn normalize_argv_allocates_nothing() {
    let original: &[&[u8]] = &[b"p", b"arg"];
    let ((empty, unchanged), calls) = measured(false, || (normalize_argv(&[]), normalize_argv(original)));
    assert_eq!(calls, 0);
    assert_eq!(empty, [b"".as_slice()]);
    assert!(core::ptr::eq(unchanged, original));
}
