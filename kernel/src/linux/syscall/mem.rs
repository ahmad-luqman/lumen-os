//! Memory: `mmap` (anonymous; its file path is #255's), `mprotect`,
//! `munmap`, `brk` (M6).

use super::neg_errno;
use crate::linux::errno;
use crate::mm::user_vm::prot_from_linux;
use crate::sched;

pub(super) fn sys_mmap(addr: u64, len: u64, prot_raw: u64, flags: u64) -> u64 {
    let Some(prot) = prot_from_linux(prot_raw) else {
        return neg_errno(errno::EINVAL);
    };
    let Some(space) = sched::current_address_space() else {
        return neg_errno(errno::ENOMEM);
    };
    match space.mmap_anon(addr, len, prot, flags) {
        Ok(va) => va,
        Err(e) => neg_errno(e.to_errno()),
    }
}

pub(super) fn sys_mprotect(addr: u64, len: u64, prot_raw: u64) -> u64 {
    let Some(prot) = prot_from_linux(prot_raw) else {
        return neg_errno(errno::EINVAL);
    };
    let Some(space) = sched::current_address_space() else {
        return neg_errno(errno::ENOMEM);
    };
    match space.mprotect(addr, len, prot) {
        Ok(()) => 0,
        Err(e) => neg_errno(e.to_mprotect_errno()),
    }
}

pub(super) fn sys_munmap(addr: u64, len: u64) -> u64 {
    let Some(space) = sched::current_address_space() else {
        return neg_errno(errno::EINVAL);
    };
    match space.munmap(addr, len) {
        Ok(()) => 0,
        Err(e) => neg_errno(e.to_errno()),
    }
}

pub(super) fn sys_brk(addr: u64) -> u64 {
    let Some(space) = sched::current_address_space() else {
        return 0;
    };
    space.brk(addr)
}
