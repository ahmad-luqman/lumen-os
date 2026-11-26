//! C12's raw syscall checks, including a real exec with a changed stack budget.
use crate::common::{self, R, ensure, eq, ok, sys};

const SELF: &[u8] = b"/usr/local/bin/m8_proc\0";
const STACK: u64 = 64 * 1024 * 1024;

fn get_usage(who: i64) -> Result<[u64; 18], common::Fail> {
    let mut out = [u64::MAX; 18];
    eq("getrusage", sys(98, [who as u64, out.as_mut_ptr() as u64, 0, 0, 0, 0]), 0)?;
    ensure("unused usage zero", out[4..].iter().all(|&v| v == 0), out[4] as i64)?;
    Ok(out)
}
fn micros(u: &[u64; 18], i: usize) -> u64 {
    u[i] * 1_000_000 + u[i + 1]
}
fn burn() -> R {
    loop {
        for i in 0..100_000 {
            core::hint::black_box(i);
        }
        let u = get_usage(1)?;
        if micros(&u, 0) >= 1000 && micros(&u, 2) >= 1000 {
            return Ok(());
        }
    }
}
fn affinity() -> R {
    let mut original = 0u64;
    eq("affinity read size", sys(204, [0, 8, &mut original as *mut _ as u64, 0, 0, 0]), 8)?;
    ensure("online CPUs", original != 0, original as i64)?;
    ensure("CPU 1 online", original & 2 != 0, original as i64)?;
    let cpu = 1;
    let mask = 1u64 << cpu;
    eq("pin", sys(203, [0, 8, &mask as *const _ as u64, 0, 0, 0]), 0)?;
    loop {
        eq("yield", sys(24, [0; 6]), 0)?;
        let mut here = u32::MAX;
        let mut node = u32::MAX;
        eq("getcpu", sys(309, [&mut here as *mut _ as u64, &mut node as *mut _ as u64, 1, 0, 0, 0]), 0)?;
        eq("node zero", node as i64, 0)?;
        if here == cpu {
            break;
        }
    }
    let mut got = [u64::MAX; 2];
    eq("affinity length", sys(204, [0, 16, got.as_mut_ptr() as u64, 0, 0, 0]), 8)?;
    eq("affinity mask", got[0] as i64, mask as i64)?;
    eq("affinity writes eight bytes", got[1] as i64, -1)?;
    for len in [0, 7, 9] {
        eq("bad affinity length", sys(204, [0, len, got.as_mut_ptr() as u64, 0, 0, 0]), -22)?;
    }
    let none = 1u64 << 63;
    eq("offline mask", sys(203, [0, 8, &none as *const _ as u64, 0, 0, 0]), -22)?;
    eq("restore affinity", sys(203, [0, 8, &original as *const _ as u64, 0, 0, 0]), 0)
}
fn priority_name_ids() -> R {
    let tid = ok("gettid", sys(186, [0; 6]))?;
    eq("set_tid_address", sys(218, [0; 6]), tid)?;
    eq("leader tid", tid, sys(39, [0; 6]))?;
    for nice in [-99i64, 5, 99] {
        eq("setpriority", sys(141, [0, tid as u64, nice as u64, 0, 0, 0]), 0)?;
        eq("getpriority raw", sys(140, [0, 0, 0, 0, 0, 0]), 20 - nice.clamp(-20, 19))?;
    }
    eq("root uid selection", sys(141, [2, 0, 3, 0, 0, 0]), 0)?;
    eq("root uid priority", sys(140, [2, 0, 0, 0, 0, 0]), 17)?;
    eq("group priority", sys(140, [1, 0, 0, 0, 0, 0]), 17)?;
    eq("restore nice", sys(141, [0, 0, 0, 0, 0, 0]), 0)?;
    let name = b"C12-raw\xff\n-abcdef-more\0";
    eq("set name", sys(157, [15, name.as_ptr() as u64, 0, 0, 0, 0]), 0)?;
    let mut out = [0xa5; 16];
    eq("get name", sys(157, [16, out.as_mut_ptr() as u64, 0, 0, 0, 0]), 0)?;
    ensure("name bytes and truncation", out[..15] == name[..15] && out[15] == 0, out[15] as i64)?;
    eq("setfsuid previous root", sys(122, [123; 6]), 0)?;
    eq("setfsgid previous root", sys(123, [123; 6]), 0)
}
fn filesystems() -> R {
    for (path, magic, ro) in [
        (b"/\0".as_slice(), 0xef53, false),
        (b"/tmp\0", 0x01021994, false),
        (b"/proc\0", 0x9fa0, true),
        (b"/dev\0", 0x01021994, false),
    ] {
        let mut info = [u64::MAX; 15];
        eq("statfs", sys(137, [path.as_ptr() as u64, info.as_mut_ptr() as u64, 0, 0, 0, 0]), 0)?;
        eq("filesystem magic", info[0] as i64, magic)?;
        eq("block size", info[1] as i64, 4096)?;
        eq("name limit", info[8] as i64, 255)?;
        eq("fragment size", info[9] as i64, 4096)?;
        eq("mount flags", info[10] as i64, if ro { 0x21 } else { 0x20 })?;
        ensure("capacity", info[3] <= info[2] && info[4] <= info[3] && info[6] <= info[5], info[3] as i64)?;
        ensure("spares zero", info[11..].iter().all(|&v| v == 0), info[11] as i64)?;
        if magic != 0x9fa0 && path != b"/dev\0" {
            ensure("real capacity", info[2] > 0 && info[5] > 0, info[2] as i64)?;
        }
        let fd = ok("O_PATH open", sys(2, [path.as_ptr() as u64, 0x200000, 0, 0, 0, 0]))?;
        let mut other = [0; 15];
        let result = sys(138, [fd as u64, other.as_mut_ptr() as u64, 0, 0, 0, 0]);
        eq("close", sys(3, [fd as u64, 0, 0, 0, 0, 0]), 0)?;
        eq("fstatfs O_PATH", result, 0)?;
        ensure("fd filesystem identity", other[0] == info[0] && other[7] == info[7], other[7] as i64)?;
    }
    eq("bad statfs pointer", sys(137, [1, 1, 0, 0, 0, 0]), -14)?;
    eq("bad statfs fd", sys(138, [u64::MAX, 1, 0, 0, 0, 0]), -9)
}
pub fn usage() -> R {
    burn()?;
    let thread = get_usage(1)?;
    let own = get_usage(0)?;
    ensure(
        "process includes thread",
        micros(&own, 0) >= micros(&thread, 0) && micros(&own, 2) >= micros(&thread, 2),
        own[1] as i64,
    )?;
    let before = get_usage(-1)?;
    let child = ok("fork usage", sys(57, [0; 6]))?;
    if child == 0 {
        let code = if burn().is_ok() { 0 } else { 1 };
        sys(60, [code, 0, 0, 0, 0, 0]);
        panic!("child exit returned");
    }
    let mut waited = [u64::MAX; 18];
    let mut status = -1i32;
    eq(
        "wait usage",
        sys(61, [child as u64, &mut status as *mut _ as u64, 0, waited.as_mut_ptr() as u64, 0, 0]),
        child,
    )?;
    eq("child status", status as i64, 0)?;
    ensure("wait reports both charges", micros(&waited, 0) >= 1000 && micros(&waited, 2) >= 1000, waited[1] as i64)?;
    let after = get_usage(-1)?;
    for i in [0, 2] {
        eq("reap adds exactly once", (micros(&after, i) - micros(&before, i)) as i64, micros(&waited, i) as i64)?;
    }
    eq("second wait ECHILD", sys(61, [child as u64, 0, 0, 0, 0, 0]), -10)?;
    let again = get_usage(-1)?;
    ensure("second wait no accounting", again == after, again[1] as i64)
}
fn info() -> R {
    let mut boot = [0u64; 2];
    eq("boottime before sysinfo", sys(228, [7, boot.as_mut_ptr() as u64, 0, 0, 0, 0]), 0)?;
    let mut first = [0u64; 14];
    eq("sysinfo first", sys(99, [first.as_mut_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;
    let mut info = [u64::MAX; 14];
    eq("sysinfo", sys(99, [info.as_mut_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;
    ensure("uptime uses boottime", first[0] >= boot[0], first[0] as i64)?;
    ensure("uptime monotonic", info[0] >= first[0], info[0] as i64)?;
    ensure("RAM bounds", info[4] > 0 && info[5] <= info[4], info[4] as i64)?;
    eq("mem unit", info[13] as i64, 1)?;
    for i in [1, 2, 3, 6, 8, 9, 11, 12] {
        eq("sysinfo zeros", info[i] as i64, 0)?;
    }
    ensure("process count", info[10] >= 2, info[10] as i64)?;
    say!("sysinfo totalram={} uptime={} before={}", info[4], info[0], first[0]);
    for clock in [0, 1, 4, 5, 6, 7] {
        let mut resolution = [u64::MAX; 2];
        eq("clock_getres", sys(229, [clock, resolution.as_mut_ptr() as u64, 0, 0, 0, 0]), 0)?;
        ensure("resolution one ns", resolution == [0, 1], resolution[1] as i64)?;
        eq("clock_getres null", sys(229, [clock, 0, 0, 0, 0, 0]), 0)?;
    }
    eq("unknown clock", sys(229, [2, 0, 0, 0, 0, 0]), -22)
}
fn stack_exec() -> R {
    let limits = [STACK, u64::MAX];
    let mut old = [0; 2];
    eq("set stack", sys(302, [0, 3, limits.as_ptr() as u64, old.as_mut_ptr() as u64, 0, 0]), 0)?;
    let child = ok("fork stack", sys(57, [0; 6]))?;
    if child == 0 {
        let result = exec_with_budget();
        if let Err(e) = result {
            say!("FAIL stack-exec: {} ({})", e.what, e.got);
        }
        sys(60, [1, 0, 0, 0, 0, 0]);
        panic!("exit returned");
    }
    let mut status = -1i32;
    eq("wait stack", sys(61, [child as u64, &mut status as *mut _ as u64, 0, 0, 0, 0]), child)?;
    eq("restore stack", sys(302, [0, 3, old.as_ptr() as u64, 0, 0, 0]), 0)?;
    eq("exec stack and budget", status as i64, 0)
}
fn exec_with_budget() -> R {
    let mut inherited = [0u64; 2];
    eq("inherited limit read", sys(302, [0, 3, 0, inherited.as_mut_ptr() as u64, 0, 0]), 0)?;
    ensure("fork inherits both limits", inherited == [STACK, u64::MAX], inherited[0] as i64)?;
    let small = [512 * 1024u64, u64::MAX];
    eq("small limit", sys(302, [0, 3, small.as_ptr() as u64, 0, 0, 0]), 0)?;
    static ENV: [u8; 65536] = {
        let mut bytes = [b'x'; 65536];
        bytes[65535] = 0;
        bytes
    };
    let argv = [SELF.as_ptr(), b"small-stack\0".as_ptr(), core::ptr::null()];
    let mut env = [ENV.as_ptr(); 101];
    env[100] = core::ptr::null();
    eq(
        "small argument budget",
        sys(59, [SELF.as_ptr() as u64, argv.as_ptr() as u64, env.as_ptr() as u64, 0, 0, 0]),
        -7,
    )?;
    eq("large limit", sys(302, [0, 3, inherited.as_ptr() as u64, 0, 0, 0]), 0)?;
    eq("six MiB cap", sys(59, [SELF.as_ptr() as u64, argv.as_ptr() as u64, env.as_ptr() as u64, 0, 0, 0]), -7)?;
    // 2.5 MiB exceeds the old default's 2 MiB budget; fits the new one.
    env[40] = core::ptr::null();
    let result = sys(59, [SELF.as_ptr() as u64, argv.as_ptr() as u64, env.as_ptr() as u64, 0, 0, 0]);
    Err(common::Fail { what: "large budget exec returned", got: result })
}
fn check_stack(expected: u64) -> R {
    let mut limits = [0u64; 2];
    eq("limit after exec", sys(302, [0, 3, 0, limits.as_mut_ptr() as u64, 0, 0]), 0)?;
    ensure("exec preserves limits", limits == [expected, u64::MAX], limits[0] as i64)?;
    let path = b"/proc/self/maps\0";
    let fd = ok("open maps", sys(2, [path.as_ptr() as u64, 0, 0, 0, 0, 0]))?;
    let mut bytes = [0; 16384];
    let n = ok("read maps", sys(0, [fd as u64, bytes.as_mut_ptr() as u64, bytes.len() as u64, 0, 0, 0]))? as usize;
    eq("close maps", sys(3, [fd as u64, 0, 0, 0, 0, 0]), 0)?;
    let text = core::str::from_utf8(&bytes[..n]).map_err(|_| common::Fail { what: "maps utf8", got: 0 })?;
    let line =
        text.lines().find(|line| line.ends_with("[stack]")).ok_or(common::Fail { what: "stack line", got: 0 })?;
    let range = line.split_whitespace().next().unwrap();
    let (lo, hi) = range.split_once('-').unwrap();
    let lo = u64::from_str_radix(lo, 16).unwrap();
    let hi = u64::from_str_radix(hi, 16).unwrap();
    eq("stack VMA size", (hi - lo) as i64, expected as i64)
}
pub fn stack_child(expected: u64) -> i32 {
    match check_stack(expected) {
        Ok(()) => 0,
        Err(e) => {
            say!("FAIL stack-child: {} ({})", e.what, e.got);
            1
        }
    }
}
pub fn run() -> i32 {
    common::run_checks(&[
        ("affinity-getcpu", &affinity),
        ("priority-name-ids", &priority_name_ids),
        ("fork-raw-comm", &fork_raw_comm),
        ("exec-raw-comm", &exec_raw_comm),
        ("zombie-priority", &zombie_priority),
        ("name-page-end", &name_page_end),
        ("syscall-error-order", &error_order),
        ("statfs", &filesystems),
        ("rusage", &usage),
        ("sysinfo-clocks", &info),
        ("stack-reduced", &reduced_stack),
        ("stack-reduced-budget", &reduced_budget),
        ("stack-exec", &stack_exec),
    ])
}

fn fork_check(check: impl FnOnce() -> R) -> R {
    let child = ok("fork check", sys(57, [0; 6]))?;
    if child == 0 {
        let code = match check() {
            Ok(()) => 0,
            Err(e) => {
                say!("FAIL child: {} ({})", e.what, e.got);
                1
            }
        };
        sys(60, [code, 0, 0, 0, 0, 0]);
        panic!("child exit returned");
    }
    let mut status = -1i32;
    eq("wait check", sys(61, [child as u64, &mut status as *mut _ as u64, 0, 0, 0, 0]), child)?;
    eq("child check status", status as i64, 0)
}
fn fork_raw_comm() -> R {
    eq("set raw comm before fork", sys(157, [15, b"a\nb\0".as_ptr() as u64, 0, 0, 0, 0]), 0)?;
    fork_check(|| {
        let mut comm = [0xa5u8; 16];
        eq("child get comm", sys(157, [16, comm.as_mut_ptr() as u64, 0, 0, 0, 0]), 0)?;
        ensure("fork preserves raw comm", &comm == b"a\nb\0\0\0\0\0\0\0\0\0\0\0\0\0", comm[1] as i64)
    })
}
fn name_page_end() -> R {
    let base = ok("map name pages", sys(9, [0, 8192, 3, 0x22, u64::MAX, 0]))? as u64;
    eq("unmap name guard", sys(11, [base + 4096, 4096, 0, 0, 0, 0]), 0)?;
    let name = base + 4096 - 15;
    // SAFETY: mmap returned a writable page; these fifteen bytes end at its boundary.
    unsafe {
        core::ptr::copy_nonoverlapping(b"abcdefghijklmno".as_ptr(), name as *mut u8, 15);
    }
    let result = (|| {
        eq("set fifteen bytes at page end", sys(157, [15, name, 0, 0, 0, 0]), 0)?;
        let mut got = [0xa5; 16];
        eq("get page-end name", sys(157, [16, got.as_mut_ptr() as u64, 0, 0, 0, 0]), 0)?;
        ensure("page-end name", &got == b"abcdefghijklmno\0", got[15] as i64)
    })();
    eq("unmap name page", sys(11, [base, 4096, 0, 0, 0, 0]), 0)?;
    result
}
fn error_order() -> R {
    let mask = 1u64;
    // Linux get_user_cpu_mask runs before find_process_by_pid for SET;
    // GET validates length, then finds the task, then copies out.
    for (label, nr, len, ptr, want) in [
        ("set bad tid bad pointer", 203, 8, 1, -14),
        ("set bad tid good pointer", 203, 8, &mask as *const _ as u64, -3),
        ("set bad tid empty mask", 203, 0, 1, -3),
        ("get bad tid bad pointer", 204, 8, 1, -3),
        ("get bad length bad tid bad pointer", 204, 7, 1, -22),
    ] {
        eq(label, sys(nr, [u64::MAX, len, ptr, 0, 0, 0]), want)?;
    }
    eq("set self empty mask", sys(203, [0, 0, 1, 0, 0, 0]), -22)?;
    for nr in [140, 141] {
        eq("priority bad selector before bad tid", sys(nr, [3, u64::MAX, i32::MIN as u64, 0, 0, 0]), -22)?;
        eq("priority bad tid", sys(nr, [0, u64::MAX, i32::MIN as u64, 0, 0, 0]), -3)?;
    }
    Ok(())
}
fn reduced_stack() -> R {
    fork_check(|| {
        let limits = [512 * 1024u64, u64::MAX];
        eq("reduce stack", sys(302, [0, 3, limits.as_ptr() as u64, 0, 0, 0]), 0)?;
        let argv = [SELF.as_ptr(), b"small-stack-reduced\0".as_ptr(), core::ptr::null()];
        Err(common::Fail {
            what: "small exec returned",
            got: sys(59, [SELF.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, 0]),
        })
    })
}
fn reduced_budget() -> R {
    fork_check(|| {
        let limits = [512 * 1024u64, u64::MAX];
        eq("reduce argument budget", sys(302, [0, 3, limits.as_ptr() as u64, 0, 0, 0]), 0)?;
        static ENV: [u8; 65536] = {
            let mut bytes = [b'x'; 65536];
            bytes[65535] = 0;
            bytes
        };
        let env = [ENV.as_ptr(), ENV.as_ptr(), ENV.as_ptr(), ENV.as_ptr(), core::ptr::null()];
        let argv = [SELF.as_ptr(), b"small-budget-must-fail\0".as_ptr(), core::ptr::null()];
        eq(
            "256 KiB exceeds reduced 128 KiB budget",
            sys(59, [SELF.as_ptr() as u64, argv.as_ptr() as u64, env.as_ptr() as u64, 0, 0, 0]),
            -7,
        )
    })
}

fn exec_raw_comm() -> R {
    let path = c"/tmp/a\nb";
    crate::exec::copy_self(path)?;
    let result = fork_check(|| {
        let argv = [path.as_ptr(), c"small-comm".as_ptr(), core::ptr::null()];
        Err(common::Fail {
            what: "raw-name exec returned",
            got: sys(59, [path.as_ptr() as u64, argv.as_ptr() as u64, 0, 0, 0, 0]),
        })
    });
    eq("unlink raw-name executable", sys(87, [path.as_ptr() as u64, 0, 0, 0, 0, 0]), 0)?;
    result
}

pub fn comm_child() -> i32 {
    let mut comm = [0xa5u8; 16];
    let result = sys(157, [16, comm.as_mut_ptr() as u64, 0, 0, 0, 0]);
    if result == 0 && &comm == b"a\nb\0\0\0\0\0\0\0\0\0\0\0\0\0" {
        0
    } else {
        say!("FAIL exec raw comm: syscall {} bytes {:?}", result, comm);
        1
    }
}

fn zombie_priority() -> R {
    let child = ok("fork nice child", sys(57, [0; 6]))?;
    if child == 0 {
        let result = sys(141, [0, 0, 5, 0, 0, 0]);
        sys(60, [if result == 0 { 0 } else { 1 }, 0, 0, 0, 0, 0]);
        panic!("child exit returned");
    }
    let mut info = [0i32; 32];
    eq("waitid nice child WNOWAIT", sys(247, [1, child as u64, info.as_mut_ptr() as u64, 0x01000004, 0, 0]), 0)?;
    eq("nice child exited", info[2] as i64, 1)?;
    eq("nice child pid", info[4] as i64, child)?;
    eq("nice child status", info[6] as i64, 0)?;
    let result = (|| {
        eq("zombie retains nice 5", sys(140, [0, child as u64, 0, 0, 0, 0]), 15)?;
        eq("set zombie nice", sys(141, [0, child as u64, 7, 0, 0, 0]), 0)?;
        eq("read updated zombie nice", sys(140, [0, child as u64, 0, 0, 0, 0]), 13)
    })();
    eq("reap nice child", sys(61, [child as u64, 0, 0, 0, 0, 0]), child)?;
    result?;
    eq("reaped getpriority ESRCH", sys(140, [0, child as u64, 0, 0, 0, 0]), -3)?;
    eq("reaped setpriority ESRCH", sys(141, [0, child as u64, 0, 0, 0, 0]), -3)
}
