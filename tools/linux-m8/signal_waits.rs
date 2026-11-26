//! C6: signal waits, mask restoration, child notifications and restart outcomes.
use super::*;
use common::{ensure, run_checks};
use core::arch::global_asm;
use core::sync::atomic::AtomicU64;

const INT: u64 = 2;
const STOP: u64 = 19;
const CONT: u64 = 18;
const SIGPENDING: u64 = 127;
const SIGTIMEDWAIT: u64 = 128;
const SIGSUSPEND: u64 = 130;
const PSELECT: u64 = 270;
const PPOLL: u64 = 271;
const NANOSLEEP: u64 = 35;
const SA_RESTART: u64 = 0x10000000;
static COUNT: AtomicU64 = AtomicU64::new(0);
static LAST_PID: AtomicU64 = AtomicU64::new(0);
static LAST_CODE: AtomicU64 = AtomicU64::new(0);
static LAST_STATUS: AtomicU64 = AtomicU64::new(0);
static ANNOUNCE: AtomicU64 = AtomicU64::new(0);

// SAFETY: the assembly restorer uses Linux's rt_sigreturn ABI.
unsafe extern "C" {
    fn c6_restorer();
}
global_asm!(".global c6_restorer", "c6_restorer:", "mov eax, 15", "syscall", "ud2");
extern "C" fn handler(_: u64, info: *const i32, _: *const u8) {
    COUNT.fetch_add(1, SeqCst);
    // SAFETY: SA_SIGINFO supplies a readable initialized siginfo for this call.
    unsafe {
        LAST_PID.store(*info.add(SI_PID) as u64, SeqCst);
        LAST_CODE.store(*info.add(SI_CODE) as u64, SeqCst);
        LAST_STATUS.store(*info.add(SI_STATUS) as u64, SeqCst);
    }
    if ANNOUNCE.load(SeqCst) != 0 {
        say!("c6: handler");
    }
}
fn action(sig: u64, flags: u64) -> R {
    let a = [handler as *const () as u64, flags | 0x04000004, c6_restorer as *const () as u64, 0];
    eq("install handler", sys(SYS_RT_SIGACTION, [sig, a.as_ptr() as u64, 0, 8, 0, 0]), 0)
}
fn mask(bits: u64) -> R {
    eq("set mask", sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, &bits as *const _ as u64, 0, 8, 0, 0]), 0)
}
fn mask_is(want: u64) -> R {
    let mut got = 0u64;
    eq("get mask", sys(SYS_RT_SIGPROCMASK, [SIG_SETMASK, 0, &mut got as *mut _ as u64, 8, 0, 0]), 0)?;
    eq("restored mask", got as i64, want as i64)
}
fn kill_pid(pid: i64, sig: u64) -> R {
    eq("kill", sys(SYS_KILL, [pid as u64, sig, 0, 0, 0, 0]), 0)
}
fn pending() -> Result<u64, common::Fail> {
    let mut got = 0u64;
    eq("sigpending", sys(SIGPENDING, [&mut got as *mut _ as u64, 8, 0, 0, 0, 0]), 0)?;
    Ok(got)
}
fn timed(bits: u64, info: &mut [i32; 32], timeout: Option<&[i64; 2]>) -> i64 {
    sys(
        SIGTIMEDWAIT,
        [&bits as *const _ as u64, info.as_mut_ptr() as u64, timeout.map_or(0, |t| t.as_ptr() as u64), 8, 0, 0],
    )
}
fn basic_waits() -> R {
    let bits = 1 << (SIGUSR1 - 1);
    mask(bits)?;
    kill_pid(sys(SYS_GETPID, [0; 6]), SIGUSR1)?;
    eq("blocked signal pending", pending()? as i64, bits as i64)?;
    let mut info = [0; 32];
    eq("wanted signal", timed(bits, &mut info, Some(&[0, 0])), SIGUSR1 as i64)?;
    eq("wanted info", info[SI_SIGNO] as i64, SIGUSR1 as i64)?;
    eq("wanted sender", info[SI_PID] as i64, sys(SYS_GETPID, [0; 6]))?;
    eq("consumed pending", pending()? as i64, 0)?;
    eq("zero timeout", timed(bits, &mut info, Some(&[0, 0])), -11)?;
    eq("elapsed timeout", timed(bits, &mut info, Some(&[0, 1_000_000])), -11)?;
    eq("invalid timeout", timed(bits, &mut info, Some(&[-1, 0])), -EINVAL)?;
    mask_is(bits)?;
    mask(0)
}
fn child_masks() -> R {
    let bits = 1 << (SIGCHLD - 1);
    for nr in [PSELECT, PPOLL, SIGSUSPEND] {
        action(SIGCHLD, SA_RESTART)?;
        mask(bits)?;
        COUNT.store(0, SeqCst);
        let child = child_exits(37)?;
        let mut info = [0; 32];
        eq(
            "confirm child exited",
            sys(SYS_WAITID, [P_PID, child as u64, info.as_mut_ptr() as u64, WNOWAIT | WEXITED, 0, 0]),
            0,
        )?;
        eq("SIGCHLD pending", pending()? as i64, bits as i64)?;
        let empty = 0u64;
        let pack = [&empty as *const _ as u64, 8];
        let result = match nr {
            PSELECT => sys(nr, [0, 0, 0, 0, 0, pack.as_ptr() as u64]),
            PPOLL => sys(nr, [0, 0, 0, &empty as *const _ as u64, 8, 0]),
            _ => sys(nr, [&empty as *const _ as u64, 8, 0, 0, 0, 0]),
        };
        eq("temporary mask returns EINTR", result, -4)?;
        eq("exactly one handler", COUNT.load(SeqCst) as i64, 1)?;
        eq("SIGCHLD child", LAST_PID.load(SeqCst) as i64, child)?;
        eq("SIGCHLD code", LAST_CODE.load(SeqCst) as i64, 1)?;
        eq("SIGCHLD status", LAST_STATUS.load(SeqCst) as i64, 37)?;
        mask_is(bits)?;
        wait(child, 37)?;
    }
    action(SIGCHLD, 2)?; // SA_NOCLDWAIT still notifies a handler.
    COUNT.store(0, SeqCst);
    let child = child_exits(38)?;
    let empty = 0u64;
    eq("autoreap suspend", sys(SIGSUSPEND, [&empty as *const _ as u64, 8, 0, 0, 0, 0]), -4)?;
    eq("autoreap handler", COUNT.load(SeqCst) as i64, 1)?;
    eq("autoreap child", LAST_PID.load(SeqCst) as i64, child)?;
    gone(child)?;
    mask(0)
}
fn wait_status(pid: i64, options: u64, want: i64) -> R {
    let mut status = 0i32;
    eq("state wait pid", sys(SYS_WAIT4, [pid as u64, &mut status as *mut _ as u64, options, 0, 0, 0]), pid)?;
    eq("state wait status", status as i64, want)
}
fn child_info(pid: i64, code: i64, status: i64) -> R {
    let mut info = [0; 32];
    eq("child notification", timed(1 << (SIGCHLD - 1), &mut info, Some(&[0, 0])), SIGCHLD as i64)?;
    eq("child info pid", info[SI_PID] as i64, pid)?;
    eq("child info code", info[SI_CODE] as i64, code)?;
    eq("child info status", info[SI_STATUS] as i64, status)
}
fn stop_continue() -> R {
    let blocked = (1 << (SIGCHLD - 1)) | (1 << (CONT - 1));
    mask(blocked)?;
    // A root process group is orphaned: a job-control stop is ignored.
    kill_pid(sys(SYS_GETPID, [0; 6]), 20)?;
    for no_child_stop in [false, true] {
        action(SIGCHLD, u64::from(no_child_stop))?;
        let child = fork()?;
        if child == 0 {
            kill_pid(sys(SYS_GETPID, [0; 6]), STOP).unwrap_or_else(|_| exit(99));
            loop {
                sys(SYS_PAUSE, [0; 6]);
            }
        }
        wait_status(child, 2, (STOP as i64) << 8 | 0x7f)?;
        if !no_child_stop {
            child_info(child, 5, STOP as i64)?;
        }
        // A blocked SIGCONT still resumes, and WNOWAIT retains the report.
        kill_pid(child, CONT)?;
        let mut info = [0; 32];
        eq("continued waitid", sys(SYS_WAITID, [P_PID, child as u64, info.as_mut_ptr() as u64, 8 | WNOWAIT, 0, 0]), 0)?;
        eq("continued code", info[SI_CODE] as i64, 6)?;
        wait_status(child, 8, 0xffff)?;
        if !no_child_stop {
            child_info(child, 6, CONT as i64)?;
        } else {
            eq("NOCLDSTOP suppresses both signals", timed(1 << (SIGCHLD - 1), &mut info, Some(&[0, 0])), -11)?;
        }
        kill_pid(child, STOP)?;
        wait_status(child, 2, (STOP as i64) << 8 | 0x7f)?;
        if !no_child_stop {
            child_info(child, 5, STOP as i64)?;
        }
        kill_pid(child, SIGKILL)?;
        wait_status(child, 0, SIGKILL as i64)?;
        child_info(child, 2, SIGKILL as i64)?;
    }
    // A thread-directed default-fatal signal cannot end a stopped child.
    let child = fork()?;
    if child == 0 {
        kill_pid(sys(SYS_GETPID, [0; 6]), STOP).unwrap_or_else(|_| exit(99));
        loop {
            sys(SYS_PAUSE, [0; 6]);
        }
    }
    wait_status(child, 2, (STOP as i64) << 8 | 0x7f)?;
    eq("tgkill stopped child", sys(SYS_TGKILL, [child as u64, child as u64, 15, 0, 0, 0]), 0)?;
    let mut status = 0;
    eq(
        "stopped child has not exited",
        sys(SYS_WAIT4, [child as u64, &mut status as *mut _ as u64, WNOHANG, 0, 0, 0]),
        0,
    )?;
    kill_pid(child, CONT)?;
    wait_status(child, 0, 15)?;
    child_info(child, 2, 15)?;
    // Kernel init remains a valid, inert kill target.
    kill_pid(1, SIGKILL)?;
    mask(0)
}
pub fn run() -> c_int {
    run_checks(&[
        ("signal-wanted-and-timeout", &basic_waits),
        ("signal-child-mask-swap", &child_masks),
        ("signal-stop-continue", &stop_continue),
    ])
}
pub fn send(pid: &[u8], sig: &[u8]) -> c_int {
    let parse = |b: &[u8]| b.iter().fold(0u64, |n, c| n * 10 + u64::from(c - b'0'));
    run_checks(&[("c6-send", &|| kill_pid(parse(pid) as i64, parse(sig)))])
}
pub fn blocking(kind: &[u8], restart: bool) -> c_int {
    if kind == b"poll" {
        match fork() {
            Ok(0) => {}
            Ok(pid) => {
                // Leave the serial shell free to inspect ps and serial poll.
                say!("c6: poll-child={pid}");
                return 0;
            }
            Err(_) => return 99,
        }
    }
    run_checks(&[("c6-blocking", &|| {
        action(INT, if restart { SA_RESTART } else { 0 })?;
        ANNOUNCE.store(1, SeqCst);
        let child = if kind == b"wait4" {
            let child = fork()?;
            if child == 0 {
                loop {
                    sys(SYS_PAUSE, [0; 6]);
                }
            }
            child
        } else {
            0
        };
        say!("c6: pid={} child={} ready", sys(SYS_GETPID, [0; 6]), child);
        let mut byte = [0u8; 1];
        let mut status = 0i32;
        let mut ts = [3600i64, 0];
        let mut rem = [0i64; 2];
        // poll's empty event mask still subscribes but serial shell input
        // cannot satisfy it; only the signal ends that background wait.
        let mut pollfd = [0i32, i32::from(kind != b"poll")]; // fd=0, events, revents=0
        let mut readfds = 1u64;
        let empty = 0u64;
        let result = match kind {
            b"read" => sys(SYS_READ, [0, byte.as_mut_ptr() as u64, 1, 0, 0, 0]),
            b"wait4" => sys(SYS_WAIT4, [child as u64, &mut status as *mut _ as u64, 0, 0, 0, 0]),
            b"nanosleep" => sys(NANOSLEEP, [ts.as_ptr() as u64, rem.as_mut_ptr() as u64, 0, 0, 0, 0]),
            b"pselect6" => sys(PSELECT, [1, &mut readfds as *mut _ as u64, 0, 0, ts.as_mut_ptr() as u64, 0]),
            b"ppoll" => sys(PPOLL, [pollfd.as_mut_ptr() as u64, 1, ts.as_mut_ptr() as u64, 0, 8, 0]),
            b"poll" => sys(SYS_POLL, [pollfd.as_mut_ptr() as u64, 1, u64::MAX, 0, 0, 0]),
            b"suspend" => sys(SIGSUSPEND, [&empty as *const _ as u64, 8, 0, 0, 0, 0]),
            b"timedwait" => {
                let mut info = [0; 32];
                timed(1 << (SIGUSR1 - 1), &mut info, None)
            }
            _ => sys(SYS_PAUSE, [0; 6]),
        };
        if kind == b"wait4" && result == -4 {
            kill_pid(child, SIGKILL)?;
            wait_status(child, 0, SIGKILL as i64)?;
        }
        ensure("handler ran", COUNT.load(SeqCst) > 0, 0)?;
        if restart && kind == b"read" {
            eq("restarted read", result, 1)?;
            eq("read data", byte[0] as i64, b'z' as i64)?;
        } else if restart && kind == b"wait4" {
            eq("restarted wait", result, child)?;
            eq("killed child", status as i64, SIGKILL as i64)?;
        } else {
            eq("interrupted call", result, -4)?;
        }
        if kind == b"nanosleep" {
            ensure("sleep remainder", rem[0] > 0 || rem[1] > 0, rem[0])?;
        }
        Ok(())
    })])
}
