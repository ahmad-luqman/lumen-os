//! C13 ring-3 timer checks. Confirm effects before proceeding; time checks are floors.
use crate::common::{self, R, ensure, eq, ok, sys};
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicU64, Ordering::SeqCst};

const ALRM: u32 = 14;
const RT: u32 = 35;
const STEP: u64 = 30_000_000;
static COUNT: AtomicU64 = AtomicU64::new(0);
static CALLBACK: AtomicU64 = AtomicU64::new(0);

// SAFETY: these signatures match musl's x86-64 ABI. The restorer is the
// assembly sigreturn trampoline already linked by the signal test module.
unsafe extern "C" {
    fn c5_restorer();
    fn timer_create(clock: c_int, event: *const u8, id: *mut usize) -> c_int;
    fn timer_settime(id: usize, flags: c_int, new: *const u64, old: *mut u64) -> c_int;
    fn timer_delete(id: usize) -> c_int;
    fn pthread_create(
        thread: *mut usize,
        attr: *const c_void,
        run: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
}
extern "C" fn handler(_: c_int) {
    COUNT.fetch_add(1, SeqCst);
}
extern "C" fn callback(value: u64) {
    CALLBACK.store(value, SeqCst);
}
fn action(signal: u32, disposition: u64) -> R {
    let action = [disposition, 0x04000000, c5_restorer as *const () as u64, 0];
    eq("sigaction", sys(13, [signal as u64, action.as_ptr() as u64, 0, 8, 0, 0]), 0)
}
fn block(signals: u64) -> R {
    eq("block", sys(14, [0, &signals as *const _ as u64, 0, 8, 0, 0]), 0)
}
fn now(clock: u64) -> Result<u64, common::Fail> {
    let mut ts = [0u64; 2];
    eq("clock", sys(228, [clock, ts.as_mut_ptr() as u64, 0, 0, 0, 0]), 0)?;
    Ok(ts[0] * 1_000_000_000 + ts[1])
}
fn nap() {
    sys(7, [0, 0, 1, 0, 0, 0]);
}
fn wait_count(count: u64) -> R {
    while COUNT.load(SeqCst) < count {
        nap();
    }
    Ok(())
}
fn pending(signal: u32) -> Result<bool, common::Fail> {
    let mut bits = 0u64;
    eq("sigpending", sys(127, [&mut bits as *mut _ as u64, 8, 0, 0, 0, 0]), 0)?;
    Ok(bits & (1 << (signal - 1)) != 0)
}
fn queued(signal: u32) -> R {
    while !pending(signal)? {
        nap();
    }
    Ok(())
}
fn take(signal: u32) -> Result<[u8; 128], common::Fail> {
    let set = 1u64 << (signal - 1);
    let timeout = [30u64, 0];
    let mut info = [0; 128];
    eq(
        "sigtimedwait",
        sys(128, [&set as *const _ as u64, info.as_mut_ptr() as u64, timeout.as_ptr() as u64, 8, 0, 0]),
        signal as i64,
    )?;
    Ok(info)
}
fn create(clock: u64, mode: u32, signal: u32) -> Result<i32, common::Fail> {
    let mut event = [0u64; 8];
    event[0] = 0x12345678;
    event[1] = u64::from(signal) | (u64::from(mode) << 32);
    event[2] = sys(186, [0; 6]) as u64;
    let mut id = -1i32;
    eq("timer_create", sys(222, [clock, event.as_ptr() as u64, &mut id as *mut _ as u64, 0, 0, 0]), 0)?;
    Ok(id)
}
fn setting(interval: u64, value: u64) -> [u64; 4] {
    [interval / 1_000_000_000, interval % 1_000_000_000, value / 1_000_000_000, value % 1_000_000_000]
}
fn arm(id: i32, interval: u64, value: u64, flags: u64) -> R {
    let new = setting(interval, value);
    eq("timer_settime", sys(223, [id as u64, flags, new.as_ptr() as u64, 0, 0, 0]), 0)
}
fn get(id: i32) -> Result<[u64; 4], common::Fail> {
    let mut value = [0; 4];
    eq("timer_gettime", sys(224, [id as u64, value.as_mut_ptr() as u64, 0, 0, 0, 0]), 0)?;
    Ok(value)
}
fn delete(id: i32) -> R {
    eq("timer_delete", sys(226, [id as u64, 0, 0, 0, 0, 0]), 0)
}
fn interval(value: u64, period: u64) -> R {
    let mut new = setting(period, value);
    new[1] /= 1000;
    new[3] /= 1000;
    eq("setitimer", sys(38, [0, new.as_ptr() as u64, 0, 0, 0, 0]), 0)
}
fn real() -> R {
    action(ALRM, handler as *const () as u64)?;
    COUNT.store(0, SeqCst);
    let start = now(1)?;
    eq("alarm", sys(37, [1, 0, 0, 0, 0, 0]), 0)?;
    wait_count(1)?;
    ensure("alarm floor", now(1)? - start >= 1_000_000_000, 0)?;
    COUNT.store(0, SeqCst);
    let start = now(1)?;
    interval(STEP, STEP)?;
    wait_count(1)?;
    ensure("setitimer floor", now(1)? - start >= STEP, 0)?;
    wait_count(2)?;
    let after_two = COUNT.load(SeqCst);
    wait_count(after_two + 1)?;
    interval(0, 0)?;
    let mut current = [99u64; 4];
    eq("getitimer", sys(36, [0, current.as_mut_ptr() as u64, 0, 0, 0, 0]), 0)?;
    ensure("disarmed real", current == [0; 4], current[2] as i64)
}
fn notifications() -> R {
    block((1 << (RT - 1)) | (1 << 11))?;
    for mode in [0, 2, 4] {
        let id = create(1, mode, RT)?;
        let start = now(1)?;
        arm(id, 0, STEP, 0)?;
        let info = take(RT)?;
        ensure("relative floor", now(1)? - start >= STEP, 0)?;
        eq("SI_TIMER", i32::from_le_bytes(info[8..12].try_into().unwrap()) as i64, -2)?;
        eq("timer id", i32::from_le_bytes(info[16..20].try_into().unwrap()) as i64, id as i64)?;
        eq("sival", u64::from_le_bytes(info[24..32].try_into().unwrap()) as i64, 0x12345678)?;
        delete(id)?;
    }
    for periodic in [false, true] {
        let none = create(1, 1, 12)?;
        let control = create(1, 0, RT)?;
        let deadline = now(1)? + STEP;
        arm(none, if periodic { STEP } else { 0 }, deadline, 1)?;
        arm(control, 0, deadline, 1)?;
        take(RT)?; // positive control: the shared deadline has actually passed
        ensure("NONE sends nothing", !pending(12)?, 0)?;
        let value = get(none)?;
        let remaining = value[2] * 1_000_000_000 + value[3];
        ensure(
            "NONE remainder",
            if periodic { remaining > 0 && remaining <= STEP } else { remaining == 0 },
            remaining as i64,
        )?;
        eq("NONE overrun", sys(225, [none as u64, 0, 0, 0, 0, 0]), 0)?;
        delete(none)?;
        delete(control)?;
    }
    for clock in [0, 1, 7] {
        let id = create(clock, 0, RT)?;
        let deadline = now(clock)? + STEP;
        arm(id, 0, deadline, 0x101)?; // unknown flag bits ignored
        take(RT)?;
        ensure("absolute floor", now(clock)? >= deadline, 0)?;
        delete(id)?;
    }
    Ok(())
}
fn replacements() -> R {
    let id = create(1, 0, RT)?;
    arm(id, STEP, 1, 0)?;
    queued(RT)?;
    let start = now(1)?;
    arm(id, 0, STEP * 2, 0)?;
    take(RT)?;
    ensure("rearm floor", now(1)? - start >= STEP * 2, 0)?;
    arm(id, STEP, 1, 0)?;
    queued(RT)?;
    delete(id)?;
    ensure("delete discards pending", !pending(RT)?, 0)?;
    let control = create(1, 0, 12)?;
    arm(control, 0, STEP * 2, 0)?;
    take(12)?;
    delete(control)?;
    ensure("deleted periodic sends nothing", !pending(RT)?, 0)?;
    let reused = create(1, 0, RT)?;
    eq("id reused", reused as i64, id as i64)?;
    let start = now(1)?;
    arm(reused, 0, STEP * 2, 0)?;
    take(RT)?;
    ensure("reuse floor", now(1)? - start >= STEP * 2, 0)?;
    delete(reused)
}
fn overruns_and_ignore() -> R {
    let id = create(1, 0, RT)?;
    arm(id, STEP, 1, 0)?;
    queued(RT)?;
    // A second timer's later expiry confirms the timers worker advanced past
    // several periods, even if the host stalled either vCPU for a long time.
    let control = create(1, 0, 12)?;
    arm(control, 0, STEP * 4, 0)?;
    take(12)?;
    eq("overrun only on delivery", sys(225, [id as u64, 0, 0, 0, 0, 0]), 0)?;
    let info = take(RT)?;
    let over = i32::from_le_bytes(info[20..24].try_into().unwrap());
    ensure("blocked overruns", over >= 1, over as i64)?;
    eq("getoverrun agrees", sys(225, [id as u64, 0, 0, 0, 0, 0]), over as i64)?;
    queued(RT)?;
    action(RT, 1)?; // discards the already queued signal
    // Unblock while ignored, then install a handler. No cached pending flag
    // may prevent the periodic timer from ever firing again.
    let bit = 1u64 << (RT - 1);
    eq("unblock ignored", sys(14, [1, &bit as *const _ as u64, 0, 8, 0, 0]), 0)?;
    COUNT.store(0, SeqCst);
    action(RT, handler as *const () as u64)?;
    wait_count(1)?;
    delete(id)?;
    block(bit)?;
    action(RT, 0)?;
    delete(control)
}
fn errors_and_fork() -> R {
    block(1 << (ALRM - 1))?;
    let mut default_id = -1i32;
    eq("default event", sys(222, [1, 0, &mut default_id as *mut _ as u64, 0, 0, 0]), 0)?;
    arm(default_id, 0, 1, 0)?;
    let default_info = take(ALRM)?;
    eq("default signo", default_info[0] as i64, ALRM as i64)?;
    eq("default sival", u64::from_le_bytes(default_info[24..32].try_into().unwrap()) as i64, default_id as i64)?;
    delete(default_id)?;
    for (mode, signo, tid) in [(0u32, 0u32, 0u64), (0, 65, 0), (3, ALRM, 0), (4, ALRM, u32::MAX as u64)] {
        let mut event = [0u64; 8];
        event[1] = signo as u64 | ((mode as u64) << 32);
        event[2] = tid;
        let mut id = -1i32;
        eq("invalid sigevent", sys(222, [1, event.as_ptr() as u64, &mut id as *mut _ as u64, 0, 0, 0]), -22)?;
    }
    let mut old = [0u64; 4];
    let bad_timeval = [0i64, 1_000_000, 0, 1];
    eq("bad timeval", sys(38, [0, bad_timeval.as_ptr() as u64, 0, 0, 0, 0]), -22)?;
    interval(60_000_000_000, STEP)?;
    eq("NULL setitimer disarms", sys(38, [0, 0, old.as_mut_ptr() as u64, 0, 0, 0]), 0)?;
    ensure("old real interval", old[0] == 0 && old[1] == STEP / 1000, old[1] as i64)?;
    eq("getitimer EFAULT", sys(36, [0, 1, 0, 0, 0, 0]), -14)?;
    let mut ids = [-1i32; 32];
    for id in &mut ids {
        *id = create(1, 1, ALRM)?;
    }
    eq("33rd timer", sys(222, [1, 0, &mut ids[0] as *mut _ as u64, 0, 0, 0]), -11)?;
    for id in ids {
        delete(id)?;
    }
    eq("unknown timer", sys(225, [0; 6]), -22)?;
    for clock in [2, 3, 4, 5, 6, 8, 9, u64::MAX] {
        let mut id = 0;
        eq("invalid clock", sys(222, [clock, 0, &mut id as *mut _ as u64, 0, 0, 0]), -22)?;
    }
    eq("create EFAULT", sys(222, [1, 0, 1, 0, 0, 0]), -14)?;
    let id = create(1, 0, RT)?;
    eq("fault rollback frees id", id as i64, 0)?;
    eq("gettime EFAULT", sys(224, [id as u64, 1, 0, 0, 0, 0]), -14)?;
    let new = setting(STEP, 60_000_000_000);
    eq("old setting EFAULT", sys(223, [id as u64, 0, new.as_ptr() as u64, 1, 0, 0]), -14)?;
    let seen = get(id)?;
    ensure("EFAULT retains installed interval", seen[0] == 0 && seen[1] == STEP, seen[1] as i64)?;

    for new in [[0i64, 1_000_000_000, 0, 1], [-1, 0, 0, 1], [0, 0, 0, -1]] {
        eq("bad itimerspec", sys(223, [id as u64, 0, new.as_ptr() as u64, 0, 0, 0]), -22)?;
    }
    for candidate in [id as u64, u64::MAX] {
        eq("NULL settime EINVAL", sys(223, [candidate, 0, 0, 1, 0, 0]), -22)?;
    }
    eq("settime EFAULT", sys(223, [id as u64, 0, 1, 0, 0, 0]), -14)?;
    for which in [1, 2, 3] {
        eq("unsupported itimer", sys(36, [which, 0, 0, 0, 0, 0]), -22)?;
    }
    // A long-lived parent timer makes inheritance observable by state alone.
    arm(id, STEP, 60_000_000_000, 0)?;
    interval(60_000_000_000, STEP)?;
    let pid = crate::fork()?;
    if pid == 0 {
        let mut state = [99u64; 4];
        let absent = sys(224, [id as u64, state.as_mut_ptr() as u64, 0, 0, 0, 0]) == -22;
        let real = sys(36, [0, state.as_mut_ptr() as u64, 0, 0, 0, 0]) == 0 && state == [0; 4];
        crate::exit(if absent && real { 0 } else { 99 });
    }
    crate::wait(pid, 0)?;
    interval(0, 0)?;
    delete(id)
}
extern "C" fn exec_worker(_: *mut c_void) -> *mut c_void {
    let result: R = (|| {
        ensure("exec from nonleader", sys(186, [0; 6]) != sys(39, [0; 6]), 0)?;
        let argv = [c"m8_proc".as_ptr(), c"timer-image".as_ptr(), core::ptr::null()];
        let env: [*const u8; 1] = [core::ptr::null()];
        ok(
            "exec timer image",
            sys(59, [c"/usr/local/bin/m8_proc".as_ptr() as u64, argv.as_ptr() as u64, env.as_ptr() as u64, 0, 0, 0]),
        )?;
        Err(common::Fail { what: "exec returned", got: 0 })
    })();
    if let Err(common::Fail { what, got }) = result {
        say!("FAIL timer exec: {what} {got}");
    }
    sys(231, [99, 0, 0, 0, 0, 0]);
    core::ptr::null_mut()
}
fn exec_timers() -> R {
    let pid = crate::fork()?;
    if pid == 0 {
        let result: R = (|| {
            block((1 << (ALRM - 1)) | (1 << (RT - 1)))?;
            let id = create(1, 0, RT)?;
            arm(id, STEP, 1, 0)?;
            queued(RT)?;
            eq("alarm before exec", sys(37, [1, 0, 0, 0, 0, 0]), 0)?;
            let mut thread = 0;
            // SAFETY: the output is writable; the static callback uses no
            // argument and replaces the entire process image with execve.
            eq(
                "exec pthread",
                unsafe { pthread_create(&mut thread, core::ptr::null(), exec_worker, core::ptr::null_mut()) } as i64,
                0,
            )?;
            loop {
                nap();
            }
        })();
        if let Err(common::Fail { what, got }) = result {
            say!("FAIL timer exec: {what} {got}");
        }
        crate::exit(99);
    }
    crate::wait(pid, 0)
}
pub fn image() -> c_int {
    common::run_checks(&[("timer-exec-image", &|| {
        eq("exec took leader tid", sys(186, [0; 6]), sys(39, [0; 6]))?;
        eq("exec deletes POSIX", sys(225, [0; 6]), -22)?;
        ensure("exec clears SI_TIMER", !pending(RT)?, 0)?;
        let info = take(ALRM)?;
        eq("alarm survived exec", i32::from_le_bytes(info[8..12].try_into().unwrap()) as i64, 128)
    })])
}
fn musl_thread() -> R {
    let mut event = [0u64; 8];
    event[0] = 0x9876;
    event[1] = 2u64 << 32; // SIGEV_THREAD, signo ignored by libc
    event[2] = callback as *const () as u64;
    let mut id = 0usize;
    // SAFETY: event has musl sigevent's 64-byte layout, the output is writable,
    // and the callback has the sigval calling convention and static lifetime.
    eq("musl timer_create", unsafe { timer_create(1, event.as_ptr().cast(), &mut id) } as i64, 0)?;
    let new = setting(0, STEP);
    // SAFETY: id is live, and new holds the four i64 itimerspec fields.
    eq("musl timer_settime", unsafe { timer_settime(id, 0, new.as_ptr(), core::ptr::null_mut()) } as i64, 0)?;
    while CALLBACK.load(SeqCst) == 0 {
        nap();
    }
    eq("musl callback value", CALLBACK.load(SeqCst) as i64, 0x9876)?;
    // SAFETY: id is the timer created above, and has not been deleted.
    eq("musl timer_delete", unsafe { timer_delete(id) } as i64, 0)
}
pub fn run() -> c_int {
    common::run_checks(&[
        ("timer-real", &real),
        ("timer-notifications", &notifications),
        ("timer-replacements", &replacements),
        ("timer-overruns-ignore", &overruns_and_ignore),
        ("timer-errors-fork", &errors_and_fork),
        ("timer-exec", &exec_timers),
        ("timer-musl-thread", &musl_thread),
    ])
}
