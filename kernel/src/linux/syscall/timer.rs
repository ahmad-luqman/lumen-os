//! Timers (M8's cluster, ARCHITECTURE section 8, "B0's stubs"): `alarm`,
//! `getitimer`, `setitimer`, `timer_create`, `timer_settime`,
//! `timer_gettime`, `timer_getoverrun`, `timer_delete`.

use super::at::UserPtr;
use super::not_implemented;
use crate::linux::general;

/// `alarm(seconds)` (37): M8 stub, replaced by C13 (#392).
pub(super) fn sys_alarm(_seconds: u32) -> u64 {
    not_implemented(general::__NR_alarm)
}

/// `getitimer(which, value)` (36): M8 stub, replaced by C13 (#392).
pub(super) fn sys_getitimer(_which: i32, _value: UserPtr) -> u64 {
    not_implemented(general::__NR_getitimer)
}

/// `setitimer(which, new_value, old_value)` (38): M8 stub, replaced by C13 (#392).
pub(super) fn sys_setitimer(_which: i32, _new_value: UserPtr, _old_value: UserPtr) -> u64 {
    not_implemented(general::__NR_setitimer)
}

/// `timer_create(clockid, sevp, timerid)` (222): M8 stub, replaced by C13 (#392).
pub(super) fn sys_timer_create(_clockid: i32, _sevp: UserPtr, _timerid: UserPtr) -> u64 {
    not_implemented(general::__NR_timer_create)
}

/// `timer_settime(timerid, flags, new_value, old_value)` (223): M8 stub, replaced by C13 (#392).
pub(super) fn sys_timer_settime(_timerid: i32, _flags: i32, _new_value: UserPtr, _old_value: UserPtr) -> u64 {
    not_implemented(general::__NR_timer_settime)
}

/// `timer_gettime(timerid, curr_value)` (224): M8 stub, replaced by C13 (#392).
pub(super) fn sys_timer_gettime(_timerid: i32, _curr_value: UserPtr) -> u64 {
    not_implemented(general::__NR_timer_gettime)
}

/// `timer_getoverrun(timerid)` (225): M8 stub, replaced by C13 (#392).
pub(super) fn sys_timer_getoverrun(_timerid: i32) -> u64 {
    not_implemented(general::__NR_timer_getoverrun)
}

/// `timer_delete(timerid)` (226): M8 stub, replaced by C13 (#392).
pub(super) fn sys_timer_delete(_timerid: i32) -> u64 {
    not_implemented(general::__NR_timer_delete)
}
