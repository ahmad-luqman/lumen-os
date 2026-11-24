//! Status words against the `W*` macros of musl and glibc, and `waitid`'s
//! `si_code`/`si_status` against Linux's derivation.

use lumen_signal::Signal;

use super::sig;
use crate::{CLD_CONTINUED, CLD_DUMPED, CLD_EXITED, CLD_KILLED, CLD_STOPPED, ChildInfo, ExitStatus, WaitStatus};

/// musl's `<sys/wait.h>`, transcribed.
mod musl {
    pub fn wexitstatus(s: i32) -> i32 {
        (s & 0xff00) >> 8
    }
    pub fn wtermsig(s: i32) -> i32 {
        s & 0x7f
    }
    pub fn wstopsig(s: i32) -> i32 {
        wexitstatus(s)
    }
    pub fn wcoredump(s: i32) -> bool {
        s & 0x80 != 0
    }
    pub fn wifexited(s: i32) -> bool {
        wtermsig(s) == 0
    }
    // ((short)((((s)&0xffff)*0x10001U)>>8) > 0x7f00)
    pub fn wifstopped(s: i32) -> bool {
        let v = (((s as u32) & 0xffff).wrapping_mul(0x10001)) >> 8;
        i32::from(v as u16 as i16) > 0x7f00
    }
    // (((s)&0xffff)-1U < 0xffu)
    pub fn wifsignaled(s: i32) -> bool {
        ((s as u32) & 0xffff).wrapping_sub(1) < 0xff
    }
    pub fn wifcontinued(s: i32) -> bool {
        s == 0xffff
    }
}

/// glibc's `bits/waitstatus.h`, transcribed.
mod glibc {
    pub fn wexitstatus(s: i32) -> i32 {
        (s & 0xff00) >> 8
    }
    pub fn wtermsig(s: i32) -> i32 {
        s & 0x7f
    }
    pub fn wstopsig(s: i32) -> i32 {
        wexitstatus(s)
    }
    pub fn wcoredump(s: i32) -> bool {
        s & 0x80 != 0
    }
    pub fn wifexited(s: i32) -> bool {
        wtermsig(s) == 0
    }
    // (((signed char) (((status) & 0x7f) + 1) >> 1) > 0)
    pub fn wifsignaled(s: i32) -> bool {
        i32::from(((s & 0x7f) + 1) as u8 as i8) >> 1 > 0
    }
    pub fn wifstopped(s: i32) -> bool {
        s & 0xff == 0x7f
    }
    pub fn wifcontinued(s: i32) -> bool {
        s == 0xffff
    }
}

/// The four predicates of both libcs, as `(exited, signaled, stopped, continued)`.
fn predicates(word: i32) -> (bool, bool, bool, bool) {
    let m = (musl::wifexited(word), musl::wifsignaled(word), musl::wifstopped(word), musl::wifcontinued(word));
    let g = (glibc::wifexited(word), glibc::wifsignaled(word), glibc::wifstopped(word), glibc::wifcontinued(word));
    assert_eq!(m, g, "musl and glibc disagree on {word:#x}");
    m
}

/// Linux's `wait_task_zombie`: `si_code`/`si_status` from an exit status word.
fn linux_zombie_siginfo(status: i32) -> ChildInfo {
    if status & 0x7f == 0 {
        ChildInfo { code: CLD_EXITED, status: status >> 8 }
    } else {
        let code = if status & 0x80 != 0 { CLD_DUMPED } else { CLD_KILLED };
        ChildInfo { code, status: status & 0x7f }
    }
}

fn signals() -> impl Iterator<Item = Signal> {
    (1..=64).map(sig)
}

#[test]
fn exited_status_round_trips_through_the_macros() {
    for code in 0..=255u8 {
        let w = WaitStatus::Exited(code).word();
        assert_eq!(w, i32::from(code) << 8);
        assert_eq!(predicates(w), (true, false, false, false), "code {code}");
        assert_eq!(musl::wexitstatus(w), i32::from(code));
        assert_eq!(glibc::wexitstatus(w), i32::from(code));
        let info = WaitStatus::Exited(code).siginfo();
        assert_eq!(info, ChildInfo { code: CLD_EXITED, status: i32::from(code) });
        assert_eq!(info, linux_zombie_siginfo(w));
    }
}

#[test]
fn exit_codes_keep_only_their_low_byte() {
    assert_eq!(ExitStatus::exited(0), ExitStatus::Exited(0));
    assert_eq!(ExitStatus::exited(256), ExitStatus::Exited(0));
    assert_eq!(ExitStatus::exited(257), ExitStatus::Exited(1));
    assert_eq!(ExitStatus::exited(-1), ExitStatus::Exited(255));
    assert_eq!(WaitStatus::from(ExitStatus::exited(-1)).word(), 0xff00);
}

#[test]
fn killed_status_round_trips_through_the_macros() {
    for signal in signals() {
        for core in [false, true] {
            let st = WaitStatus::Killed { signal, core };
            let w = st.word();
            let n = signal.number() as i32;
            assert_eq!(w, n | if core { 0x80 } else { 0 });
            assert_eq!(predicates(w), (false, true, false, false), "signal {n} core {core}");
            assert_eq!(musl::wtermsig(w), n);
            assert_eq!(glibc::wtermsig(w), n);
            assert_eq!(musl::wcoredump(w), core);
            assert_eq!(glibc::wcoredump(w), core);
            let code = if core { CLD_DUMPED } else { CLD_KILLED };
            assert_eq!(st.siginfo(), ChildInfo { code, status: n });
            assert_eq!(st.siginfo(), linux_zombie_siginfo(w));
        }
    }
}

#[test]
fn stopped_status_round_trips_through_the_macros() {
    for signal in signals() {
        let st = WaitStatus::Stopped(signal);
        let w = st.word();
        let n = signal.number() as i32;
        assert_eq!(w, (n << 8) | 0x7f);
        assert_eq!(predicates(w), (false, false, true, false), "signal {n}");
        assert_eq!(musl::wstopsig(w), n);
        assert_eq!(glibc::wstopsig(w), n);
        assert_eq!(st.siginfo(), ChildInfo { code: CLD_STOPPED, status: n });
    }
}

#[test]
fn continued_status_round_trips_through_the_macros() {
    let w = WaitStatus::Continued.word();
    assert_eq!(w, 0xffff);
    assert_eq!(predicates(w), (false, false, false, true));
    assert_eq!(WaitStatus::Continued.siginfo(), ChildInfo { code: CLD_CONTINUED, status: 18 });
}

#[test]
fn cld_codes_are_linuxs() {
    use linux_raw_sys::general as g;
    assert_eq!(
        [CLD_EXITED, CLD_KILLED, CLD_DUMPED, CLD_STOPPED, CLD_CONTINUED].map(|c| c as u32),
        [g::CLD_EXITED, g::CLD_KILLED, g::CLD_DUMPED, g::CLD_STOPPED, g::CLD_CONTINUED]
    );
}
