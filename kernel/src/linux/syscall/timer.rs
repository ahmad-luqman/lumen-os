//! Linux interval and POSIX timer ABI. User copies never hold timer/signal locks.

use super::{at::UserPtr, copy_from_user, copy_to_user, finish};
use crate::linux::{
    errno,
    process::current_process,
    timer::{self, TimerId},
};

/// Decode both timeval/timespec fields, rejecting negative or unnormalized values.
fn read_setting(ptr: UserPtr, micros: bool) -> Result<(u64, u64), u32> {
    let mut bytes = [0; 32];
    copy_from_user(&mut bytes, ptr.addr()).map_err(|e| e.to_errno())?;
    let mut values = [0; 2];
    for (value, pair) in values.iter_mut().zip(bytes.as_chunks::<16>().0) {
        let sec = i64::from_le_bytes(pair[..8].try_into().unwrap());
        let sub = i64::from_le_bytes(pair[8..].try_into().unwrap());
        if sec < 0 || sub < 0 || sub >= if micros { 1_000_000 } else { 1_000_000_000 } {
            return Err(errno::EINVAL);
        }
        *value = (sec as u64).saturating_mul(1_000_000_000).saturating_add(sub as u64 * if micros { 1000 } else { 1 });
    }
    Ok((values[0], values[1]))
}
pub(super) fn write_setting(ptr: UserPtr, setting: (u64, u64), micros: bool) -> Result<u64, u32> {
    let mut bytes = [0; 32];
    for (ns, pair) in [setting.0, setting.1].into_iter().zip(bytes.as_chunks_mut::<16>().0) {
        // Round remaining nanoseconds up to timeval resolution so a live
        // sub-microsecond timer cannot appear disarmed.
        let units = if micros { ns.div_ceil(1000) } else { ns };
        let base = if micros { 1_000_000 } else { 1_000_000_000 };
        pair[..8].copy_from_slice(&(units / base).to_le_bytes());
        pair[8..].copy_from_slice(&(units % base).to_le_bytes());
    }
    copy_to_user(ptr.addr(), &bytes).map_err(|e| e.to_errno())?;
    Ok(0)
}

pub(super) fn sys_alarm(seconds: u32) -> u64 {
    let old = timer::set(&current_process(), TimerId::Real, 0, u64::from(seconds) * 1_000_000_000, false);
    finish(old.map(|(_, ns)| lumen_signal::timer::alarm_seconds(ns)))
}
pub(super) fn sys_getitimer(which: i32, value: UserPtr) -> u64 {
    finish((|| {
        if which != 0 {
            return Err(errno::EINVAL);
        }
        write_setting(value, timer::get(&current_process(), TimerId::Real)?, true)
    })())
}
pub(super) fn sys_setitimer(which: i32, new: UserPtr, old: UserPtr) -> u64 {
    finish((|| {
        let (interval, value) = if new.addr() == 0 { (0, 0) } else { read_setting(new, true)? };
        if which != 0 {
            return Err(errno::EINVAL);
        }
        let previous = timer::set(&current_process(), TimerId::Real, interval, value, false)?;
        if old.addr() == 0 { Ok(0) } else { write_setting(old, previous, true) }
    })())
}
pub(super) fn sys_timer_create(clock: i32, event: UserPtr, output: UserPtr) -> u64 {
    finish((|| {
        let (mode, signal, value, tid) = if event.addr() == 0 {
            (0, 14, None, 0)
        } else {
            let mut bytes = [0; 64];
            copy_from_user(&mut bytes, event.addr()).map_err(|e| e.to_errno())?;
            (
                i32::from_le_bytes(bytes[12..16].try_into().unwrap()),
                u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
                Some(u64::from_le_bytes(bytes[..8].try_into().unwrap())),
                u32::from_le_bytes(bytes[16..20].try_into().unwrap()),
            )
        };
        let process = current_process();
        let created = timer::create(&process, clock, mode, signal, value, tid)?;
        let id = created.id;
        if let Err(error) = copy_to_user(output.addr(), &id.to_le_bytes()) {
            timer::cancel_create(&process, created);
            return Err(error.to_errno());
        }
        Ok(0)
    })())
}
pub(super) fn sys_timer_settime(id: i32, flags: u32, new: UserPtr, old: UserPtr) -> u64 {
    finish((|| {
        if new.addr() == 0 {
            return Err(errno::EINVAL);
        }
        let (interval, value) = read_setting(new, false)?;
        let previous = timer::set(&current_process(), TimerId::Posix(id), interval, value, flags & 1 != 0)?;
        if old.addr() == 0 { Ok(0) } else { write_setting(old, previous, false) }
    })())
}
pub(super) fn sys_timer_gettime(id: i32, value: UserPtr) -> u64 {
    finish(timer::get(&current_process(), TimerId::Posix(id)).and_then(|setting| write_setting(value, setting, false)))
}
pub(super) fn sys_timer_getoverrun(id: i32) -> u64 {
    finish(timer::overrun(&current_process(), id).map(|n| n as u64))
}
pub(super) fn sys_timer_delete(id: i32) -> u64 {
    finish(timer::delete(&current_process(), id).map(|()| 0))
}

crate::ktest!(
    fn timer_disarm_and_null_settime_abi() {
        use super::at::FromReg;
        use crate::linux::process::{self, KTEST_USER_VA as A};
        use crate::{kassert, kassert_eq};
        process::in_test_process(|_| {
            let ptr = UserPtr::from_reg;
            let mut setting = [0u8; 32];
            setting[..8].copy_from_slice(&3u64.to_le_bytes());
            setting[8..16].copy_from_slice(&4u64.to_le_bytes());
            copy_to_user(A, &setting).map_err(|_| "setting")?;
            kassert_eq!(sys_setitimer(0, ptr(A), ptr(0)), 0);
            kassert_eq!(sys_getitimer(0, ptr(A + 64)), 0);
            let mut observed = [99; 32];
            copy_from_user(&mut observed, A + 64).map_err(|_| "real setting")?;
            kassert_eq!(observed, [0; 32]);
            let process = current_process();
            let id = timer::create(&process, 1, 1, 0, None, 0).map_err(|_| "NONE")?.id;
            kassert_eq!(sys_timer_settime(id, 0, ptr(A), ptr(0)), 0);
            kassert_eq!(sys_timer_gettime(id, ptr(A + 64)), 0);
            copy_from_user(&mut observed, A + 64).map_err(|_| "POSIX setting")?;
            kassert_eq!(observed, [0; 32]);
            // A NULL new setting is EINVAL even with an invalid id or old ptr.
            for candidate in [id, -1] {
                kassert_eq!(sys_timer_settime(candidate, 0, ptr(0), ptr(1)), super::neg_errno(errno::EINVAL));
            }
            kassert_eq!(sys_timer_delete(id), 0);
            // Disarming REAL never removes its already-pending SIGALRM.
            process::set_current_blocked(lumen_signal::Blocked::new(lumen_signal::SigSet::of(&[
                lumen_signal::Signal::ALRM,
            ])));
            process::send_process_checked(
                process.pid() as i32,
                Ok(Some(lumen_signal::pending::SigInfo::new(lumen_signal::Signal::ALRM, 128))),
                lumen_signal::pending::SenderClass::SiUser,
            )
            .map_err(|_| "ALRM")?;
            kassert_eq!(sys_alarm(0), 0);
            kassert!(process::current_pending().contains(lumen_signal::Signal::ALRM));
            Ok(())
        })?
    }
);
