//! Pid 1 (M8, #372): `linux::init` starts it at boot, after the VFS.

use crate::linux::process;

/// Start the Linux side at boot: pid 1, the kernel-owned process every exit
/// names as its reaper (`process::start_init`). Once, after `vfs::init` and
/// before the shells and the ktests.
pub fn init() {
    process::start_init();
    log::debug!(target: "linux", "pid 1 started");
}
