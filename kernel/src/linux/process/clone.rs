//! Validated process-clone requests; construction excludes thread clones.

use crate::linux::{errno, general};

/// A supported process clone, validated before allocating child resources.
#[derive(Clone, Copy)]
pub(crate) struct ProcessClone(u32);

impl ProcessClone {
    pub(crate) const FORK: Self = Self(general::SIGCHLD);
    pub(crate) const VFORK: Self = Self(general::CLONE_VM | general::CLONE_VFORK | general::SIGCHLD);

    /// Linux clears the alternate stack only for shared VM without vfork.
    pub(super) fn keeps_alt_stack(self) -> bool {
        self.0 & (general::CLONE_VM | general::CLONE_VFORK) != general::CLONE_VM
    }

    pub(super) fn bits(self) -> u32 {
        self.0
    }
}

/// Validate M8's process-clone flags before allocating anything.
pub(crate) fn clone_flags(flags: u32, tls: u64) -> Result<ProcessClone, u32> {
    let allowed = general::CSIGNAL
        | general::CLONE_VM
        | general::CLONE_FS
        | general::CLONE_FILES
        | general::CLONE_SIGHAND
        | general::CLONE_THREAD
        | general::CLONE_VFORK
        | general::CLONE_SETTLS
        | general::CLONE_PARENT_SETTID
        | general::CLONE_CHILD_SETTID
        | general::CLONE_CHILD_CLEARTID
        | general::CLONE_SYSVSEM
        | general::CLONE_DETACHED
        | general::CLONE_IO
        | general::CLONE_PTRACE
        | general::CLONE_UNTRACED;
    if flags & !allowed != 0
        || (flags & general::CLONE_THREAD != 0 && flags & general::CLONE_SIGHAND == 0)
        || (flags & general::CLONE_SIGHAND != 0 && flags & general::CLONE_VM == 0)
        || (flags & general::CLONE_SIGHAND != 0 && flags & general::CLONE_THREAD == 0)
        || (flags & general::CLONE_THREAD == 0 && flags & general::CSIGNAL != general::SIGCHLD)
        || (flags & general::CLONE_THREAD != 0
            && flags & (general::CLONE_FILES | general::CLONE_FS) != (general::CLONE_FILES | general::CLONE_FS))
    {
        return Err(errno::EINVAL);
    }
    if flags & general::CLONE_SETTLS != 0 && tls >= crate::user::USER_MAX {
        return Err(errno::EPERM);
    }
    // Thread creation and group exit are C3; never silently create a process
    // for a request to create a thread.
    if flags & general::CLONE_THREAD != 0 {
        return Err(errno::ENOSYS);
    }
    Ok(ProcessClone(flags))
}
