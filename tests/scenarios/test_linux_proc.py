"""M8 ring-3 scenarios of processes, threads, signals, `futex`, timers,
sessions and the small syscalls (ARCHITECTURE section 15, "M8's tests"),
each check an `ok` line of `tools/linux-m8/m8_proc`.

Laid down empty by B0 (#371); each issue adds its own tests."""

import re
import time

from lumen_harness.qemu import EXIT_SUCCESS, Devices
from lumen_harness.rootfs import copy_rootfs, warm_up
from lumen_harness.shell import run


def test_fork_wait_and_exit_status(boot, tmp_path):
    """C1: fork, zombies, waits, autoreaping, reparenting and vfork.

    C3/C6/C8/C9/C12 extend this scenario with their design-tagged checks.
    """
    disk = copy_rootfs(tmp_path)
    m = boot("qemutest", devices=Devices(blk_image=disk, rng=True))
    m.expect(r"lumen> ", 60)
    warm_up(m)
    out = run(m, "linux /usr/local/bin/m8_proc fork-wait", 120)
    for check in (
        "exit-status",
        "wait-nohang",
        "waitid-zombie",
        "wait-copyout-fault",
        "sigchld-autoreap",
        "orphan-init",
        "vfork-clone",
        "clone-tids",
        "clone-wait-errors",
        "fork-context",
        "clone-stack-tls",
        "clone-shared-resources",
        "fork-inherited-state",
    ):
        assert f"ok {check}\n" in out, out
    assert re.search(r"linux: /usr/local/bin/m8_proc exit=0 leaked_frames=-?\d+", out), out
    # The orphan can outlive the root's command. Confirm init reaped it
    # before any later scenario counts frames; this is an outer hang guard.
    deadline = time.monotonic() + 30
    while True:
        ps = run(m, "ps")
        pids = [int(line.split()[0]) for line in ps.splitlines() if line.split() and line.split()[0].isdigit()]
        if pids == [1]:
            break
        assert time.monotonic() < deadline, ps
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
