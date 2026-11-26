//! C8: sessions, process groups, group kills, and process-group waits.

use crate::common::{self, R, close, ensure, eq, pipe, sys};
use crate::*;
use core::ffi::{c_int, c_void};
use core::sync::atomic::{AtomicU32, Ordering::SeqCst};

// SAFETY: these declarations match musl's x86-64 C ABI.
unsafe extern "C" {
    fn pthread_create(
        thread: *mut usize,
        attr: *const c_void,
        run: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    fn pthread_join(thread: usize, result: *mut *mut c_void) -> c_int;
}

const SYS_SETPGID: u64 = 109;
const SYS_GETPGRP: u64 = 111;
const SYS_SETSID: u64 = 112;
const SYS_GETPGID: u64 = 121;
const SYS_GETSID: u64 = 124;
const SYS_RT_SIGTIMEDWAIT: u64 = 128;
const P_PGID: u64 = 2;
const SI_USER: i64 = 0;
const EXEC_READY_FD: i32 = 10;
const EXEC_GO_FD: i32 = 11;

fn getpid() -> i64 {
    sys(SYS_GETPID, [0; 6])
}
fn getpgrp() -> i64 {
    sys(SYS_GETPGRP, [0; 6])
}
fn getpgid(pid: i64) -> i64 {
    sys(SYS_GETPGID, [pid as u64, 0, 0, 0, 0, 0])
}
fn getsid(pid: i64) -> i64 {
    sys(SYS_GETSID, [pid as u64, 0, 0, 0, 0, 0])
}
fn setpgid(pid: i64, pgid: i64) -> i64 {
    sys(SYS_SETPGID, [pid as u64, pgid as u64, 0, 0, 0, 0])
}
fn setsid() -> i64 {
    sys(SYS_SETSID, [0; 6])
}
fn kill(pid: i64, sig: u64) -> i64 {
    sys(SYS_KILL, [pid as u64, sig, 0, 0, 0, 0])
}

fn read_byte(fd: i32) -> Result<u8, common::Fail> {
    let mut b = [0u8; 1];
    eq("read_byte", sys(SYS_READ, [fd as u64, b.as_mut_ptr() as u64, 1, 0, 0, 0]), 1)?;
    Ok(b[0])
}

fn write_byte(fd: i32, b: u8) -> R {
    eq("write_byte", sys(SYS_WRITE, [fd as u64, &b as *const _ as u64, 1, 0, 0, 0]), 1)
}

fn read_u32(fd: i32) -> Result<u32, common::Fail> {
    let mut buf = [0u8; 4];
    eq("read_u32", sys(SYS_READ, [fd as u64, buf.as_mut_ptr() as u64, 4, 0, 0, 0]), 4)?;
    Ok(u32::from_ne_bytes(buf))
}

fn write_u32(fd: i32, v: u32) -> R {
    let buf = v.to_ne_bytes();
    eq("write_u32", sys(SYS_WRITE, [fd as u64, buf.as_ptr() as u64, 4, 0, 0, 0]), 4)
}

fn wait_zombie(pid: i64) -> R {
    let mut info = [0i32; 32];
    eq("waitid WNOWAIT", sys(SYS_WAITID, [P_PID, pid as u64, info.as_mut_ptr() as u64, WEXITED | WNOWAIT, 0, 0]), 0)?;
    eq("zombie si_pid", i64::from(info[SI_PID]), pid)
}

/// Run `f` inside a forked child that starts its own session, and reap it.
fn in_new_session(f: impl FnOnce(i64) -> R) -> R {
    let pid = fork()?;
    if pid == 0 {
        let me = getpid();
        let res = (|| -> R {
            eq("child setsid", setsid(), me)?;
            f(me)
        })();
        match res {
            Ok(()) => exit(0),
            Err(common::Fail { what, got }) => {
                say!("FAIL session-child: {what} (got {got})");
                exit(1);
            }
        }
    }
    wait(pid, 0)
}

fn setsid_and_inheritance() -> R {
    let root = getpid();
    eq("root getpgrp", getpgrp(), root)?;
    eq("root getpgid(0)", getpgid(0), root)?;
    eq("root getsid(0)", getsid(0), root)?;
    eq("init getpgid(1)", getpgid(1), 1)?;
    eq("init getsid(1)", getsid(1), 1)?;
    eq("root setsid EPERM", setsid(), -EPERM)?;

    // Forked child inherits pgid/sid, creates its own session, grandchild inherits it.
    let child = fork()?;
    if child == 0 {
        let me = getpid();
        let res = (|| -> R {
            eq("inherited getpgrp", getpgrp(), root)?;
            eq("inherited getpgid(0)", getpgid(0), root)?;
            eq("inherited getsid(0)", getsid(0), root)?;
            eq("child setsid", setsid(), me)?;
            eq("session leader getpgrp", getpgrp(), me)?;
            eq("session leader getpgid(0)", getpgid(0), me)?;
            eq("session leader getsid(0)", getsid(0), me)?;
            eq("repeat setsid EPERM", setsid(), -EPERM)?;
            let grand = fork()?;
            if grand == 0 {
                let ok_g = getpgrp() == me && getpgid(0) == me && getsid(0) == me;
                exit(if ok_g { 0 } else { 91 });
            }
            wait(grand, 0)
        })();
        exit(if res.is_ok() { 0 } else { 92 });
    }
    wait(child, 0)?;

    // A process group leader cannot call setsid.
    let leader = fork()?;
    if leader == 0 {
        let me = getpid();
        let ok_l = setpgid(0, 0) == 0 && getpgrp() == me && setsid() == -EPERM && getsid(0) == root;
        exit(if ok_l { 0 } else { 93 });
    }
    wait(leader, 0)?;

    // setsid fails while another process (live or zombie) still has caller's pid as its pgid.
    in_new_session(|s| {
        let a = fork()?;
        if a == 0 {
            let me = getpid();
            let res = (|| -> R {
                eq("a leads group a", setpgid(0, 0), 0)?;
                let ready = pipe(0)?;
                let go = pipe(0)?;
                let b = fork()?;
                if b == 0 {
                    let _ = close(ready[0]);
                    let _ = close(go[1]);
                    let _ = write_byte(ready[1], b'R');
                    let _ = read_byte(go[0]);
                    exit(0);
                }
                close(ready[1])?;
                close(go[0])?;
                eq("b ready", i64::from(read_byte(ready[0])?), i64::from(b'R'))?;
                close(ready[0])?;
                eq("a returns to group s", setpgid(0, s), 0)?;
                eq("a pgid is s", getpgrp(), s)?;
                eq("b still in group a", getpgid(b), me)?;
                eq("setsid EPERM while live b holds group a", setsid(), -EPERM)?;
                write_byte(go[1], b'G')?;
                close(go[1])?;
                wait_zombie(b)?;
                eq("setsid EPERM while zombie b holds group a", setsid(), -EPERM)?;
                wait(b, 0)?;
                eq("setsid succeeds once b is reaped", setsid(), me)?;
                eq("a new sid", getsid(0), me)?;
                eq("a new pgid", getpgrp(), me)
            })();
            match res {
                Ok(()) => exit(0),
                Err(common::Fail { what, got }) => {
                    say!("FAIL setsid-busy: {what} (got {got})");
                    exit(1);
                }
            }
        }
        wait(a, 0)
    })
}

fn setpgid_success_and_zombie() -> R {
    in_new_session(|s| {
        let p_c = pipe(0)?;
        let c = fork()?;
        if c == 0 {
            let _ = close(p_c[1]);
            let _ = read_byte(p_c[0]);
            exit(0);
        }
        close(p_c[0])?;
        eq("parent setpgid(c, 0)", setpgid(c, 0), 0)?;
        eq("getpgid(c) == c", getpgid(c), c)?;
        eq("getsid(c) == s", getsid(c), s)?;

        let p_d = pipe(0)?;
        let d = fork()?;
        if d == 0 {
            let _ = close(p_d[1]);
            let _ = read_byte(p_d[0]);
            exit(0);
        }
        close(p_d[0])?;
        eq("parent setpgid(d, c)", setpgid(d, c), 0)?;
        eq("getpgid(d) == c", getpgid(d), c)?;
        // Move an un-exec'd zombie child, and read its pgid/sid before and after reap.
        write_byte(p_d[1], b'G')?;
        close(p_d[1])?;
        wait_zombie(d)?;
        eq("zombie getpgid", getpgid(d), c)?;
        eq("zombie getsid", getsid(d), s)?;
        eq("parent setpgid on zombie", setpgid(d, s), 0)?;
        eq("zombie moved pgid", getpgid(d), s)?;
        wait(d, 0)?;
        eq("reaped getpgid ESRCH", getpgid(d), -ESRCH)?;
        eq("reaped getsid ESRCH", getsid(d), -ESRCH)?;

        // Join a group whose original leader has exited and been reaped.
        let p_l = pipe(0)?;
        let l = fork()?;
        if l == 0 {
            let _ = close(p_l[1]);
            let _ = read_byte(p_l[0]);
            exit(0);
        }
        close(p_l[0])?;
        eq("setpgid(l, 0)", setpgid(l, 0), 0)?;
        let p_m = pipe(0)?;
        let m = fork()?;
        if m == 0 {
            let _ = close(p_m[1]);
            let _ = read_byte(p_m[0]);
            exit(0);
        }
        close(p_m[0])?;
        eq("setpgid(m, l)", setpgid(m, l), 0)?;
        write_byte(p_l[1], b'G')?;
        close(p_l[1])?;
        wait(l, 0)?;
        let p_e = pipe(0)?;
        let e = fork()?;
        if e == 0 {
            let _ = close(p_e[1]);
            let _ = read_byte(p_e[0]);
            exit(0);
        }
        close(p_e[0])?;
        eq("join leaderless group l", setpgid(e, l), 0)?;
        eq("getpgid(e) == l", getpgid(e), l)?;
        eq("noop setpgid(e, l)", setpgid(e, l), 0)?;

        for (fd, pid) in [(p_e[1], e), (p_m[1], m), (p_c[1], c)] {
            write_byte(fd, b'G')?;
            close(fd)?;
            wait(pid, 0)?;
        }
        Ok(())
    })
}

fn spawn_exec_hold(new_session: bool) -> Result<(i64, i32, i32), common::Fail> {
    let ready = pipe(0)?;
    let go = pipe(0)?;
    let pid = fork()?;
    if pid == 0 {
        let _ = close(ready[0]);
        let _ = close(go[1]);
        if new_session && setsid() < 0 {
            exit(94);
        }
        if sys(common::SYS_DUP2, [ready[1] as u64, EXEC_READY_FD as u64, 0, 0, 0, 0]) != i64::from(EXEC_READY_FD) {
            exit(95);
        }
        if sys(common::SYS_DUP2, [go[0] as u64, EXEC_GO_FD as u64, 0, 0, 0, 0]) != i64::from(EXEC_GO_FD) {
            exit(96);
        }
        let argv = [c"m8_proc".as_ptr() as u64, c"sessions-exec-hold".as_ptr() as u64, 0];
        let envp = [0u64];
        sys(
            SYS_EXECVE,
            [c"/usr/local/bin/m8_proc".as_ptr() as u64, argv.as_ptr() as u64, envp.as_ptr() as u64, 0, 0, 0],
        );
        exit(97);
    }
    close(ready[1])?;
    close(go[0])?;
    eq("exec hold ready", i64::from(read_byte(ready[0])?), i64::from(b'R'))?;
    Ok((pid, ready[0], go[1]))
}

pub fn exec_hold() -> c_int {
    let res = (|| -> R {
        write_byte(EXEC_READY_FD, b'R')?;
        let cmd = read_byte(EXEC_GO_FD)?;
        if cmd == b'S' {
            let me = getpid();
            eq("self setpgid after exec", setpgid(0, 0), 0)?;
            eq("self getpgrp after exec", getpgrp(), me)?;
            write_byte(EXEC_READY_FD, b'O')?;
        }
        Ok(())
    })();
    if res.is_ok() { 0 } else { 1 }
}

fn setpgid_errnos_and_exec() -> R {
    let root = getpid();
    in_new_session(|s| {
        // Negative pgid is EINVAL before EPERM and ESRCH.
        eq("setpgid(0, -1) EINVAL", setpgid(0, -1), -EINVAL)?;
        eq("setpgid(99999, -1) EINVAL", setpgid(99999, -1), -EINVAL)?;
        eq("setpgid(1, INT_MIN) EINVAL", setpgid(1, i64::from(i32::MIN)), -EINVAL)?;
        eq("setpgid(-5, 0) EINVAL", setpgid(-5, 0), -EINVAL)?;

        // Session leader cannot change its own group.
        eq("leader setpgid(0, 0) EPERM", setpgid(0, 0), -EPERM)?;
        eq("leader setpgid(s, s) EPERM", setpgid(s, s), -EPERM)?;

        // ESRCH for nonexistent, negative, init, parent, sibling, grandchild.
        eq("setpgid(99999, 0) ESRCH", setpgid(99999, 0), -ESRCH)?;
        eq("setpgid(-5, s) ESRCH", setpgid(-5, s), -ESRCH)?;
        eq("setpgid(1, 0) ESRCH", setpgid(1, 0), -ESRCH)?;
        eq("setpgid(root, 0) ESRCH", setpgid(root, 0), -ESRCH)?;

        // getpgid / getsid ESRCH and high-32-bit register truncation.
        for bad in [-1i64, i64::from(i32::MIN), 99999] {
            eq("getpgid ESRCH", getpgid(bad), -ESRCH)?;
            eq("getsid ESRCH", getsid(bad), -ESRCH)?;
        }
        let hi = 0x1234_5678_0000_0000u64;
        eq("getpgid ignores high bits", sys(SYS_GETPGID, [hi, 0, 0, 0, 0, 0]), s)?;
        eq("getsid ignores high bits", sys(SYS_GETSID, [hi | (s as u32 as u64), 0, 0, 0, 0, 0]), s)?;

        // Sibling, grandchild, and bad target groups (from a non-leader child).
        let p_sib = pipe(0)?;
        let sib = fork()?;
        if sib == 0 {
            let _ = close(p_sib[1]);
            let _ = read_byte(p_sib[0]);
            exit(0);
        }
        close(p_sib[0])?;

        let p_gpid = pipe(0)?;
        let p_cgo = pipe(0)?;
        let c = fork()?;
        if c == 0 {
            let _ = close(p_sib[1]);
            let _ = close(p_gpid[0]);
            let _ = close(p_cgo[1]);
            let res = (|| -> R {
                eq("child setpgid(sib, 0) ESRCH", setpgid(sib, 0), -ESRCH)?;
                eq("child setpgid(parent, 0) ESRCH", setpgid(s, 0), -ESRCH)?;
                eq("other session init EPERM", setpgid(0, 1), -EPERM)?;
                eq("other session root EPERM", setpgid(0, root), -EPERM)?;
                eq("nonexistent group EPERM", setpgid(0, 99999), -EPERM)?;
                eq("non-leader pid EPERM", setpgid(0, sib), -EPERM)?;
                eq("pgid unchanged after EPERM", getpgrp(), s)?;
                let p_g = pipe(0)?;
                let g = fork()?;
                if g == 0 {
                    let _ = close(p_g[1]);
                    let _ = read_byte(p_g[0]);
                    exit(0);
                }
                close(p_g[0])?;
                write_u32(p_gpid[1], g as u32)?;
                let _ = read_byte(p_cgo[0])?;
                write_byte(p_g[1], b'G')?;
                close(p_g[1])?;
                wait(g, 0)
            })();
            exit(if res.is_ok() { 0 } else { 98 });
        }
        close(p_gpid[1])?;
        close(p_cgo[0])?;
        let grand = i64::from(read_u32(p_gpid[0])?);
        close(p_gpid[0])?;
        eq("grandchild setpgid ESRCH", setpgid(grand, 0), -ESRCH)?;
        write_byte(p_cgo[1], b'G')?;
        close(p_cgo[1])?;
        wait(c, 0)?;
        write_byte(p_sib[1], b'G')?;
        close(p_sib[1])?;
        wait(sib, 0)?;

        // Child that has exec'd: parent gets EACCES (before target-group EPERM),
        // while the child can still call setpgid(0, 0) itself.
        let (ex, ex_ready, ex_go) = spawn_exec_hold(false)?;
        eq("execd child setpgid EACCES", setpgid(ex, 0), -EACCES)?;
        eq("EACCES before target EPERM", setpgid(ex, 99999), -EACCES)?;
        write_byte(ex_go, b'S')?;
        eq("execd child self setpgid ok", i64::from(read_byte(ex_ready)?), i64::from(b'O'))?;
        eq("execd child moved itself", getpgid(ex), ex)?;
        close(ex_ready)?;
        close(ex_go)?;
        wait(ex, 0)?;

        // Child in another session: EPERM, even after exec (EPERM precedes EACCES).
        let (other, other_ready, other_go) = spawn_exec_hold(true)?;
        eq("other-session child sid", getsid(other), other)?;
        eq("other-session execd child EPERM", setpgid(other, s), -EPERM)?;
        write_byte(other_go, b'X')?;
        close(other_ready)?;
        close(other_go)?;
        wait(other, 0)
    })
}

struct ThreadProbe {
    tid: AtomicU32,
    pgid: AtomicU32,
    pgrp: AtomicU32,
    sid: AtomicU32,
    ready: AtomicU32,
    release: AtomicU32,
}

extern "C" fn thread_probe(arg: *mut c_void) -> *mut c_void {
    // SAFETY: arg points to a live ThreadProbe kept alive until pthread_join.
    let p = unsafe { &*(arg as *const ThreadProbe) };
    p.tid.store(sys(SYS_GETTID, [0; 6]) as u32, SeqCst);
    p.pgid.store(getpgid(0) as u32, SeqCst);
    p.pgrp.store(getpgrp() as u32, SeqCst);
    p.sid.store(getsid(0) as u32, SeqCst);
    p.ready.store(1, SeqCst);
    sys(SYS_FUTEX, [&p.ready as *const _ as u64, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, 0, 0, 0]);
    while p.release.load(SeqCst) == 0 {
        sys(SYS_FUTEX, [&p.release as *const _ as u64, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0]);
    }
    core::ptr::null_mut()
}

fn nonleader_tid_esrch() -> R {
    in_new_session(|s| {
        // Run inside a non-leader child so setpgid(0, tid) tests target-group EPERM,
        // not session-leader EPERM.
        let child = fork()?;
        if child == 0 {
            let me = getpid();
            let res = (|| -> R {
                let probe = ThreadProbe {
                    tid: AtomicU32::new(0),
                    pgid: AtomicU32::new(0),
                    pgrp: AtomicU32::new(0),
                    sid: AtomicU32::new(0),
                    ready: AtomicU32::new(0),
                    release: AtomicU32::new(0),
                };
                let mut handle = 0usize;
                // SAFETY: probe lives until pthread_join completes below.
                let rc = unsafe {
                    pthread_create(&mut handle, core::ptr::null(), thread_probe, &probe as *const _ as *mut c_void)
                };
                eq("pthread_create", i64::from(rc), 0)?;
                while probe.ready.load(SeqCst) == 0 {
                    sys(SYS_FUTEX, [&probe.ready as *const _ as u64, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 0, 0, 0, 0]);
                }
                let tid = i64::from(probe.tid.load(SeqCst));
                ensure("non-leader tid differs from pid", tid > 0 && tid != me, tid)?;
                eq("thread getpgid(0)", i64::from(probe.pgid.load(SeqCst)), s)?;
                eq("thread getpgrp()", i64::from(probe.pgrp.load(SeqCst)), s)?;
                eq("thread getsid(0)", i64::from(probe.sid.load(SeqCst)), s)?;
                // Positive control: the tid is live.
                eq("tkill(tid, 0)", sys(SYS_TKILL, [tid as u64, 0, 0, 0, 0, 0]), 0)?;
                // Lumen design (ARCHITECTURE section 8 & 15): process lookup by pid only.
                eq("getpgid(tid) ESRCH", getpgid(tid), -ESRCH)?;
                eq("getsid(tid) ESRCH", getsid(tid), -ESRCH)?;
                eq("setpgid(tid, 0) ESRCH", setpgid(tid, 0), -ESRCH)?;
                eq("setpgid(0, tid) EPERM", setpgid(0, tid), -EPERM)?;
                eq("kill(tid, 0) ESRCH", kill(tid, 0), -ESRCH)?;
                eq("kill(tid, SIGUSR1) ESRCH", kill(tid, SIGUSR1), -ESRCH)?;

                probe.release.store(1, SeqCst);
                sys(SYS_FUTEX, [&probe.release as *const _ as u64, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, 0, 0, 0]);
                let mut ret = core::ptr::null_mut();
                // SAFETY: handle is a joinable pthread created above.
                eq("pthread_join", i64::from(unsafe { pthread_join(handle, &mut ret) }), 0)
            })();
            match res {
                Ok(()) => exit(0),
                Err(common::Fail { what, got }) => {
                    say!("FAIL nonleader-tid: {what} (got {got})");
                    exit(1);
                }
            }
        }
        wait(child, 0)
    })
}

fn wait_sigusr1(expected_sender: i64) -> R {
    let mask = 1u64 << (SIGUSR1 - 1);
    let mut info = [0i32; 32];
    eq(
        "rt_sigtimedwait SIGUSR1",
        sys(SYS_RT_SIGTIMEDWAIT, [&mask as *const _ as u64, info.as_mut_ptr() as u64, 0, 8, 0, 0]),
        SIGUSR1 as i64,
    )?;
    eq("si_code SI_USER", i64::from(info[SI_CODE]), SI_USER)?;
    eq("si_pid", i64::from(info[SI_PID]), expected_sender)
}

fn kill_process_group() -> R {
    in_new_session(|s| {
        // Error precedence: ESRCH before EINVAL, including i32::MIN.
        eq("kill(-99999, 0) ESRCH", kill(-99999, 0), -ESRCH)?;
        eq("kill(-99999, 999) ESRCH", kill(-99999, 999), -ESRCH)?;
        eq("kill(INT_MIN, 999) ESRCH", kill(i64::from(i32::MIN), 999), -ESRCH)?;
        eq("kill(-s, 0) ok", kill(-s, 0), 0)?;
        eq("kill(-s, 999) EINVAL", kill(-s, 999), -EINVAL)?;
        eq("kill(0, 999) EINVAL", kill(0, 999), -EINVAL)?;

        // Block SIGUSR1 so children receive it synchronously via rt_sigtimedwait.
        let mask = 1u64 << (SIGUSR1 - 1);
        eq("block SIGUSR1", sys(SYS_RT_SIGPROCMASK, [SIG_BLOCK, &mask as *const _ as u64, 0, 8, 0, 0]), 0)?;

        let ready1 = pipe(0)?;
        let ack1 = pipe(0)?;
        let cmd1 = pipe(0)?;
        let ready2 = pipe(0)?;
        let ack2 = pipe(0)?;
        let ready_b = pipe(0)?;
        let go_b = pipe(0)?;

        // Spawn m1 in its own group; m2 joins m1's group; bystander joins its own group.
        let m1 = fork()?;
        if m1 == 0 {
            let me = getpid();
            let _ = close(ready1[0]);
            let _ = close(ack1[0]);
            let _ = close(cmd1[1]);
            let res = (|| -> R {
                eq("m1 setpgid(0, 0)", setpgid(0, 0), 0)?;
                eq("m1 getpgrp", getpgrp(), me)?;
                write_byte(ready1[1], b'R')?;
                wait_sigusr1(s)?;
                write_byte(ack1[1], b'1')?;
                eq("m1 cmd", i64::from(read_byte(cmd1[0])?), i64::from(b'K'))?;
                eq("m1 kill(0, SIGUSR1)", kill(0, SIGUSR1), 0)?;
                wait_sigusr1(me)?;
                write_byte(ack1[1], b'2')?;
                loop {
                    sys(SYS_PAUSE, [0; 6]);
                }
            })();
            match res {
                Ok(()) => exit(0),
                Err(common::Fail { what, got }) => {
                    say!("FAIL kill-m1: {what} (got {got})");
                    exit(1);
                }
            }
        }
        close(ready1[1])?;
        close(ack1[1])?;
        close(cmd1[0])?;
        eq("m1 ready", i64::from(read_byte(ready1[0])?), i64::from(b'R'))?;
        close(ready1[0])?;

        let m2 = fork()?;
        if m2 == 0 {
            let _ = close(ready2[0]);
            let _ = close(ack2[0]);
            let res = (|| -> R {
                eq("m2 join m1", setpgid(0, m1), 0)?;
                eq("m2 getpgrp", getpgrp(), m1)?;
                write_byte(ready2[1], b'R')?;
                wait_sigusr1(s)?;
                write_byte(ack2[1], b'1')?;
                wait_sigusr1(m1)?;
                write_byte(ack2[1], b'2')?;
                loop {
                    sys(SYS_PAUSE, [0; 6]);
                }
            })();
            match res {
                Ok(()) => exit(0),
                Err(common::Fail { what, got }) => {
                    say!("FAIL kill-m2: {what} (got {got})");
                    exit(1);
                }
            }
        }
        close(ready2[1])?;
        close(ack2[1])?;
        eq("m2 ready", i64::from(read_byte(ready2[0])?), i64::from(b'R'))?;
        close(ready2[0])?;

        let bystander = fork()?;
        if bystander == 0 {
            let me = getpid();
            let _ = close(ready_b[0]);
            let _ = close(go_b[1]);
            let res = (|| -> R {
                eq("bystander own group", setpgid(0, 0), 0)?;
                eq("bystander getpgrp", getpgrp(), me)?;
                write_byte(ready_b[1], b'R')?;
                let _ = read_byte(go_b[0])?;
                let mut pending = 0u64;
                eq(
                    "bystander pending",
                    sys(common::SYS_RT_SIGPENDING, [&mut pending as *mut _ as u64, 8, 0, 0, 0, 0]),
                    0,
                )?;
                eq("bystander received no SIGUSR1", (pending & mask) as i64, 0)
            })();
            exit(if res.is_ok() { 0 } else { 99 });
        }
        close(ready_b[1])?;
        close(go_b[0])?;
        eq("bystander ready", i64::from(read_byte(ready_b[0])?), i64::from(b'R'))?;
        close(ready_b[0])?;

        // 1. kill(-m1, SIGUSR1) reaches both m1 and m2.
        eq("kill(-m1, SIGUSR1)", kill(-m1, SIGUSR1), 0)?;
        eq("m1 got kill(-g)", i64::from(read_byte(ack1[0])?), i64::from(b'1'))?;
        eq("m2 got kill(-g)", i64::from(read_byte(ack2[0])?), i64::from(b'1'))?;

        // 2. kill(0, SIGUSR1) from m1 reaches both m1 and m2.
        write_byte(cmd1[1], b'K')?;
        close(cmd1[1])?;
        eq("m1 got kill(0)", i64::from(read_byte(ack1[0])?), i64::from(b'2'))?;
        eq("m2 got kill(0)", i64::from(read_byte(ack2[0])?), i64::from(b'2'))?;
        close(ack1[0])?;
        close(ack2[0])?;

        // 3. kill(-m1, SIGKILL) kills both m1 and m2; bystander is untouched.
        eq("kill(-m1, SIGKILL)", kill(-m1, SIGKILL), 0)?;
        for pid in [m1, m2] {
            let mut st = 0i32;
            eq("reap killed member", sys(SYS_WAIT4, [pid as u64, &mut st as *mut _ as u64, 0, 0, 0, 0]), pid)?;
            eq("killed by SIGKILL", i64::from(st), SIGKILL as i64)?;
        }
        eq("kill(-m1, 0) after reap ESRCH", kill(-m1, 0), -ESRCH)?;

        // Confirm bystander received neither SIGUSR1 nor SIGKILL.
        write_byte(go_b[1], b'G')?;
        close(go_b[1])?;
        wait(bystander, 0)?;

        // 4. Zombie still holds its process group until reaped.
        let p_z = pipe(0)?;
        let z = fork()?;
        if z == 0 {
            let _ = close(p_z[1]);
            let _ = read_byte(p_z[0]);
            exit(0);
        }
        close(p_z[0])?;
        eq("setpgid(z, 0)", setpgid(z, 0), 0)?;
        write_byte(p_z[1], b'G')?;
        close(p_z[1])?;
        wait_zombie(z)?;
        eq("kill(-zombie_pgid, 0)", kill(-z, 0), 0)?;
        eq("kill(-zombie_pgid, SIGUSR1)", kill(-z, SIGUSR1), 0)?;
        wait(z, 0)?;
        eq("kill(-reaped_pgid, 0) ESRCH", kill(-z, 0), -ESRCH)
    })
}

/// C8 addition to `fork-wait` (ARCHITECTURE section 15): `wait4` with `pid` 0 and `-pgid`,
/// plus `waitid(P_PGID, ...)`.
pub fn wait_pgrp() -> R {
    in_new_session(|_s| {
        let a = child_exits(11)?;
        let p_b = pipe(0)?;
        let b = fork()?;
        if b == 0 {
            let _ = close(p_b[1]);
            let _ = read_byte(p_b[0]);
            exit(22);
        }
        close(p_b[0])?;
        eq("setpgid(b, 0)", setpgid(b, 0), 0)?;
        wait_zombie(a)?;
        // Group -b has only live b, so WNOHANG returns 0 even though a is a zombie.
        let mut st = -1i32;
        eq("wait4(-b, WNOHANG)", sys(SYS_WAIT4, [(-b) as u64, &mut st as *mut _ as u64, WNOHANG, 0, 0, 0]), 0)?;
        write_byte(p_b[1], b'G')?;
        close(p_b[1])?;
        eq("wait4(-b)", sys(SYS_WAIT4, [(-b) as u64, &mut st as *mut _ as u64, 0, 0, 0, 0]), b)?;
        eq("wait4(-b) status", i64::from(st), 22 << 8)?;
        eq("wait4(-b) ECHILD", sys(SYS_WAIT4, [(-b) as u64, 0, WNOHANG, 0, 0, 0]), -ECHILD)?;
        // wait4(0, ...) selects only children in the caller's own group (a).
        eq("wait4(0)", sys(SYS_WAIT4, [0, &mut st as *mut _ as u64, 0, 0, 0, 0]), a)?;
        eq("wait4(0) status", i64::from(st), 11 << 8)?;
        eq("wait4(0) ECHILD", sys(SYS_WAIT4, [0, 0, WNOHANG, 0, 0, 0]), -ECHILD)?;

        // waitid(P_PGID, ...) with explicit pgid and 0 (caller's group).
        let c = child_exits(33)?;
        let p_d = pipe(0)?;
        let d = fork()?;
        if d == 0 {
            let _ = close(p_d[1]);
            let _ = read_byte(p_d[0]);
            exit(44);
        }
        close(p_d[0])?;
        eq("setpgid(d, 0)", setpgid(d, 0), 0)?;
        write_byte(p_d[1], b'G')?;
        close(p_d[1])?;
        let mut info = [0i32; 32];
        eq("waitid P_PGID d", sys(SYS_WAITID, [P_PGID, d as u64, info.as_mut_ptr() as u64, WEXITED, 0, 0]), 0)?;
        eq("waitid P_PGID d pid", i64::from(info[SI_PID]), d)?;
        eq("waitid P_PGID d status", i64::from(info[SI_STATUS]), 44)?;
        info.fill(0);
        eq("waitid P_PGID 0", sys(SYS_WAITID, [P_PGID, 0, info.as_mut_ptr() as u64, WEXITED, 0, 0]), 0)?;
        eq("waitid P_PGID 0 pid", i64::from(info[SI_PID]), c)?;
        eq("waitid P_PGID 0 status", i64::from(info[SI_STATUS]), 33)
    })
}

pub fn run() -> c_int {
    common::run_checks(&[
        ("setsid-and-inheritance", &setsid_and_inheritance),
        ("setpgid-success-and-zombie", &setpgid_success_and_zombie),
        ("setpgid-errnos-and-exec", &setpgid_errnos_and_exec),
        ("nonleader-tid-esrch", &nonleader_tid_esrch),
        ("kill-process-group", &kill_process_group),
        ("wait-pgrp", &wait_pgrp),
    ])
}
