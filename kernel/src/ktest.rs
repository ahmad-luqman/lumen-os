//! In-kernel test runner.
//!
//! Tests are registered with [`ktest!`](crate::ktest!) anywhere in the kernel.
//! Each one places a [`KTest`] descriptor in the `.ktests` linker section, so
//! adding a test never means editing a shared list (important when several
//! agents add tests in parallel). With `--features ktest` the kernel runs
//! every test after boot, prints TAP version 14 over serial, and exits QEMU
//! with `Success` or `Failure`.
//!
//! A test returns `Ok(())` or `Err(reason)`. A panic or CPU exception inside a
//! test is caught by the crash reporter, which exits with `Crash`; the harness
//! reports that test as failed and shows the crash report.

use crate::klog::LockedSerial;
use core::fmt::{self, Write};

#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by ktests
pub type TestResult = Result<(), &'static str>;

#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by ktests
#[repr(C)]
pub struct KTest {
    pub name: &'static str,
    pub func: fn() -> TestResult,
}

/// Register an in-kernel test.
///
/// ```ignore
/// crate::ktest!(fn heap_reuses_freed_blocks() {
///     crate::kassert_eq!(1 + 1, 2);
///     Ok(())
/// });
/// ```
#[macro_export]
macro_rules! ktest {
    (fn $name:ident() $body:block) => {
        #[cfg(feature = "ktest")]
        fn $name() -> $crate::ktest::TestResult $body

        #[cfg(feature = "ktest")]
        const _: () = {
            #[used]
            #[unsafe(link_section = ".ktests")]
            static TEST: $crate::ktest::KTest = $crate::ktest::KTest {
                name: concat!(module_path!(), "::", stringify!($name)),
                func: $name,
            };
        };
    };
}

/// Fail the current test if `cond` is false.
#[macro_export]
macro_rules! kassert {
    ($cond:expr) => {
        if !$cond {
            $crate::ktest::diag(format_args!("assertion failed: {} ({}:{})", stringify!($cond), file!(), line!()));
            return Err(concat!("assertion failed: ", stringify!($cond)));
        }
    };
}

/// Fail the current test if the two values differ, printing both.
#[macro_export]
macro_rules! kassert_eq {
    ($left:expr, $right:expr) => {
        match (&$left, &$right) {
            (l, r) => {
                if *l != *r {
                    $crate::ktest::diag(format_args!(
                        "assertion failed: {} == {} ({}:{})",
                        stringify!($left),
                        stringify!($right),
                        file!(),
                        line!()
                    ));
                    $crate::ktest::diag(format_args!("   left: {:?}", l));
                    $crate::ktest::diag(format_args!("  right: {:?}", r));
                    return Err(concat!("assertion failed: ", stringify!($left), " == ", stringify!($right)));
                }
            }
        }
    };
}

#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // used by ktests
/// Print a TAP diagnostic line (`# ...`), whole (under the log's line lock).
/// So neither this nor `kassert!` may run inside `klog::with_line_lock`:
/// the CPU would re-lock the line lock.
pub fn diag(args: fmt::Arguments<'_>) {
    let _ = writeln!(LockedSerial, "# {args}");
}

#[cfg(feature = "ktest")]
fn registered() -> &'static [KTest] {
    unsafe extern "C" {
        static __ktests_start: u8;
        static __ktests_end: u8;
    }
    let start = (&raw const __ktests_start).cast::<KTest>();
    let end = (&raw const __ktests_end).cast::<KTest>();
    let count = (end as usize - start as usize) / core::mem::size_of::<KTest>();
    // SAFETY: the linker script places only `KTest` values between the two
    // symbols (see `.ktests` in linker.ld).
    unsafe { core::slice::from_raw_parts(start, count) }
}

/// Run every registered test; returns true if all passed.
#[cfg(feature = "ktest")]
pub fn run_all() -> bool {
    let tests = registered();
    // Each line whole, under the log's line lock.
    let mut out = LockedSerial;
    let _ = writeln!(out, "TAP version 14");
    let _ = writeln!(out, "1..{}", tests.len());
    let mut failed = 0;
    for (i, t) in tests.iter().enumerate() {
        match (t.func)() {
            Ok(()) => {
                let _ = writeln!(out, "ok {} - {}", i + 1, t.name);
            }
            Err(reason) => {
                failed += 1;
                // Diagnostics always precede their result line (kassert! prints
                // while the test runs), and the harness parses them that way.
                let _ = writeln!(out, "# {reason}");
                let _ = writeln!(out, "not ok {} - {}", i + 1, t.name);
            }
        }
    }
    let _ = writeln!(out, "# ktest: {} passed, {} failed", tests.len() - failed, failed);
    failed == 0
}

crate::ktest!(
    fn ktest_registry_is_populated() {
        crate::kassert!(registered().len() >= 2);
        Ok(())
    }
);

/// A fixed buffer for checking formatted output in tests without the heap
/// (some tests hold the heap's lock, or check code that must not allocate).
#[cfg(feature = "ktest")]
pub struct Buf {
    data: [u8; 512],
    len: usize,
}

#[cfg(feature = "ktest")]
impl Buf {
    pub const fn new() -> Self {
        Self { data: [0; 512], len: 0 }
    }

    pub fn as_str(&self) -> &str {
        // `write_str` appends whole `&str`s or nothing, so this is UTF-8.
        core::str::from_utf8(&self.data[..self.len]).unwrap_or("<invalid utf-8>")
    }
}

#[cfg(feature = "ktest")]
impl Write for Buf {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        self.data.get_mut(self.len..end).ok_or(fmt::Error)?.copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}
