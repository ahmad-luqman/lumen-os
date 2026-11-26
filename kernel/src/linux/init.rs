//! Pid 1 (M8, #372): `linux::init` starts it at boot, after the VFS. The
//! design names `linux::init`; it lives here, re-exported by `linux`.

use crate::linux::process;

/// Start the Linux side at boot: pid 1, the kernel-owned process every exit
/// names as its reaper (`process::start_init`; it panics if pid 1 can't
/// start). Called once, by `kmain` after `vfs::init` and before the shells
/// and the ktests.
pub fn init() {
    process::start_init();
    crate::linux::timer::init();
    log::debug!(target: "linux", "pid 1 started");
}
