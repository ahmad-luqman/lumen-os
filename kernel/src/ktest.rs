//! In-kernel test runner.
//!
//! Tests are registered with [`ktest!`](crate::ktest!) anywhere in the kernel.
//! Each one places a [`KTest`] descriptor in the `.ktests` linker section, so
//! adding a test never means editing a shared list (important when several
//! agents add tests in parallel). With `--features ktest` the kernel runs
//! every test after boot (or those `lumen.ktest=` selects), prints TAP
//! version 14 over serial, and, with `lumen.qemu_exit`, exits QEMU with
//! `Success` or `Failure` (else halts).
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

/// Is the test `name` run, with `lumen.ktest=` set to `only` (a
/// comma-separated list of name prefixes, e.g.
/// `lumen_kernel::time::,lumen_kernel::sched::`; without it, every test)? A
/// prefix should end in `::` to mean a whole module: `lumen_kernel::time`
/// would also select a `lumen_kernel::timer`. An empty item (a stray comma) matches no test, so a bare
/// `lumen.ktest=` selects none.
#[cfg(feature = "ktest")]
fn selected(name: &str, only: Option<&str>) -> bool {
    only.is_none_or(|prefixes| prefixes.split(',').any(|p| !p.is_empty() && name.starts_with(p)))
}

/// The tests run before every other, in this order: they read the boot
/// CPU's trace ring back to its boot events, which the ring holds only
/// until it has taken `trace::EVENTS` events. In link order (the linker's,
/// changing with any change to the kernel) the tests before them once took
/// more than that on a TCG run, and the ring wrapped before they ran. Each
/// must be registered: a renamed one fails the run, loudly, rather than
/// quietly losing its place.
#[cfg(feature = "ktest")]
const FIRST: [&str; 2] = [
    "lumen_kernel::trace::boot_phases_recorded_in_order",
    "lumen_kernel::trace::log_stamps_count_from_the_timelines_origin",
];

/// `all` in the order run: [`FIRST`]'s tests, then the rest in link order.
/// Panics if one of `FIRST` isn't in `all`.
#[cfg(feature = "ktest")]
fn in_run_order(all: &[KTest]) -> impl Iterator<Item = &KTest> {
    let first = FIRST.map(|name| {
        all.iter().find(|t| t.name == name).unwrap_or_else(|| panic!("ktest: {name} (ktest::FIRST) isn't registered"))
    });
    first.into_iter().chain(all.iter().filter(|t| !FIRST.contains(&t.name)))
}

/// Run every registered test, or only those `lumen.ktest=` selects (for a
/// scenario that checks only some, so the rest don't run under conditions
/// they don't claim to hold up in), [`FIRST`]'s first. Returns true if at
/// least one test ran and every one that ran passed.
#[cfg(feature = "ktest")]
pub fn run_all() -> bool {
    let only = crate::cmdline::value("lumen.ktest");
    let all = registered();
    let tests = || in_run_order(all).filter(move |t| selected(t.name, only));
    let count = tests().count();
    // Each line whole, under the log's line lock.
    let mut out = LockedSerial;
    let _ = writeln!(out, "TAP version 14");
    let _ = writeln!(out, "1..{count}");
    let mut failed = 0;
    for (i, t) in tests().enumerate() {
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
    let _ = writeln!(out, "# ktest: {} passed, {} failed", count - failed, failed);
    // After the results, so `lumen_harness.tap` attaches them to no test.
    if let Some(prefixes) = only {
        let _ = writeln!(out, "# ktest: only {prefixes}: {count} of {}", all.len());
    }
    if count == 0 {
        let _ = writeln!(out, "# ktest: no test selected, a failure");
    }
    // A run that tests nothing checks nothing: a failure.
    count > 0 && failed == 0
}

crate::ktest!(
    fn ktest_registry_is_populated() {
        crate::kassert!(registered().len() >= 2);
        Ok(())
    }
);

crate::ktest!(
    fn a_filter_selects_tests_by_name_prefix() {
        let name = "lumen_kernel::time::sleep_10ms_elapses";
        crate::kassert!(selected(name, None));
        crate::kassert!(selected(name, Some("lumen_kernel::time::")));
        crate::kassert!(selected(name, Some("lumen_kernel::sched::,lumen_kernel::time::")));
        crate::kassert!(!selected(name, Some("lumen_kernel::sched::")));
        // A prefix of the whole path, not a piece inside it.
        crate::kassert!(!selected(name, Some("time::")));
        crate::kassert!(!selected(name, Some("sleep")));
        // An empty item (a stray comma) matches no test, not every test.
        crate::kassert!(!selected(name, Some("")));
        crate::kassert!(!selected(name, Some(",lumen_kernel::sched::,")));
        crate::kassert!(selected(name, Some("lumen_kernel::time::,")));
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

// The IR audit builds the normal kernel, where `ktest!` compiles out (#459).
// Skip that macro: #[used] retains this descriptor and its smoltcp code in
// kernel-crate IR; the linker's KEEP on .ktests retains them in both ELFs.
// Only feature = "ktest" code reads .ktests today; a future normal-kernel
// reader must skip this test descriptor.
#[used]
#[unsafe(link_section = ".ktests")]
static SMOLTCP_LINK_CHECK: KTest = KTest {
    name: concat!(module_path!(), "::smoltcp_links_and_echoes_over_loopback"),
    func: smoltcp_links_and_echoes_over_loopback,
};

fn smoltcp_links_and_echoes_over_loopback() -> TestResult {
    use smoltcp::iface::{Config, Interface, SocketSet};
    use smoltcp::phy::{Loopback, Medium};
    use smoltcp::socket::tcp;
    use smoltcp::time::{Duration, Instant};
    use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr};

    // Fixed steps at made-up times: a slow host can't fail it, and a
    // connection that doesn't progress within 8 steps fails.
    fn poll_steps(iface: &mut Interface, device: &mut Loopback, sockets: &mut SocketSet<'_>, now: &mut Instant) {
        for _ in 0..8 {
            iface.poll(*now, device, sockets);
            *now += Duration::from_millis(1);
        }
    }

    for (family, addr, prefix) in
        [("IPv4", IpAddress::v4(127, 0, 0, 1), 8), ("IPv6", IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1), 128)]
    {
        diag(format_args!("smoltcp loopback: {family} ({addr}/{prefix})"));
        let mut now = Instant::ZERO;
        let mut device = Loopback::new(Medium::Ip);
        let mut iface = Interface::new(Config::new(HardwareAddress::Ip), &mut device, Instant::ZERO);
        iface.update_ip_addrs(|addrs| {
            addrs.push(IpCidr::new(addr, prefix)).unwrap();
        });
        let socket = || {
            tcp::Socket::new(tcp::SocketBuffer::new(alloc::vec![0; 256]), tcp::SocketBuffer::new(alloc::vec![0; 256]))
        };
        let mut sockets = SocketSet::new(alloc::vec::Vec::new());
        let server = sockets.add(socket());
        let client = sockets.add(socket());
        crate::kassert_eq!(sockets.get_mut::<tcp::Socket>(server).listen((addr, 459)), Ok(()));
        crate::kassert_eq!(
            sockets.get_mut::<tcp::Socket>(client).connect(iface.context(), (addr, 459), (addr, 32768)),
            Ok(())
        );
        poll_steps(&mut iface, &mut device, &mut sockets, &mut now);
        crate::kassert_eq!(sockets.get::<tcp::Socket>(client).state(), tcp::State::Established);
        crate::kassert_eq!(sockets.get::<tcp::Socket>(server).state(), tcp::State::Established);

        let payload = b"Lumen smoltcp #459";
        crate::kassert_eq!(sockets.get_mut::<tcp::Socket>(client).send_slice(payload), Ok(payload.len()));
        poll_steps(&mut iface, &mut device, &mut sockets, &mut now);
        let mut received = [0; 64];
        crate::kassert_eq!(sockets.get_mut::<tcp::Socket>(server).recv_slice(&mut received), Ok(payload.len()));
        crate::kassert_eq!(&received[..payload.len()], payload);
        let tag = b"echo: ";
        crate::kassert_eq!(sockets.get_mut::<tcp::Socket>(server).send_slice(tag), Ok(tag.len()));
        crate::kassert_eq!(
            sockets.get_mut::<tcp::Socket>(server).send_slice(&received[..payload.len()]),
            Ok(payload.len())
        );
        poll_steps(&mut iface, &mut device, &mut sockets, &mut now);
        received.fill(0);
        let expected = b"echo: Lumen smoltcp #459";
        crate::kassert_eq!(sockets.get_mut::<tcp::Socket>(client).recv_slice(&mut received), Ok(expected.len()));
        crate::kassert_eq!(&received[..expected.len()], expected);
    }
    Ok(())
}
