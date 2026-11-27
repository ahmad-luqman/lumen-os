"""M8 ring-3 scenarios of pipes, FIFOs, readiness (`poll`, `select`), PTYs
and the console as a tty (ARCHITECTURE section 15, "M8's tests"): most
checks an `ok` line of `tools/linux-m8/m8_io`, the console's job control
(C10) Alpine's `ash -i` on the serial console."""

import re

from lumen_harness.crash import BEGIN as CRASH_BEGIN
from lumen_harness.qemu import EXIT_SUCCESS, Devices
from lumen_harness.rootfs import copy_rootfs, debugfs, debugfs_names, e2fsck_check
from lumen_harness.shell import run

BOOT_TIMEOUT = 60
ATTEMPTS = 20
M8_IO = "/usr/local/bin/m8_io"
SERIAL_POLL = re.compile(
    r"serial: poll safety-step hits: (?P<hits>\d+), woken: (?P<woken>\d+), subscribers: (?P<subs>\d+)\n"
)


def _serial_poll(m) -> tuple[int, int, int]:
    out = run(m, "serial poll")
    found = SERIAL_POLL.fullmatch(out)
    assert found, out
    return int(found["hits"]), int(found["woken"]), int(found["subs"])


def boot_shell(boot, *, devices=None):
    m = boot("qemutest", devices=devices)
    m.expect(r"lumen> ", BOOT_TIMEOUT)
    return m


def _console_poll_run(m) -> tuple[int, int, int]:
    """Run every console readiness check and return the checked poll counters."""
    start = len(m.output)
    m.send_line(f"linux {M8_IO} console-poll")

    for check in (
        "poll_nval",
        "poll_timeout_no_input",
        "ppoll_timeout_no_input",
        "select_timeout_no_input",
        "pselect6_timeout_no_input",
    ):
        m.expect(re.compile(rf"^ok {check}\r?$", re.MULTILINE), 20)

    # Interactive readiness checks: `m8_io` prints `ready <tag>` immediately
    # before each syscall; send the serial bytes and confirm the check passes.
    for tag, payload, check in (
        ("poll", "ab", "poll_with_input"),
        ("ppoll", "c", "ppoll_with_input"),
        ("select", "d", "select_with_input"),
        ("pselect6", "e", "pselect6_with_input"),
    ):
        m.expect(re.compile(rf"^ready {tag}\r?$", re.MULTILINE), 20)
        m.send(payload)
        m.expect(re.compile(rf"^ok {check}\r?$", re.MULTILINE), 20)

    m.expect(rf"linux: {re.escape(M8_IO)} exit=0 leaked_frames=0\n", 20)
    m.expect(r"lumen> ", 10)
    assert CRASH_BEGIN not in m.output[start:]

    # C10's terminal pollers subscribe to the discipline, with no timed
    # console recheck. The legacy safety counter stays zero; the subscriber
    # count includes the terminal and must return to zero on every exit.
    hits, woken, subs = _serial_poll(m)
    assert hits == 0, (hits, woken, subs)
    assert subs == 0, (hits, woken, subs)

    return hits, woken, subs


def test_poll_and_select_on_console(boot, tmp_path):
    """`poll`, `ppoll`, `select` and `pselect6` on the console's stdin with and
    without typed input; timeouts last at least their requested floor; remaining
    time is written back (0 on timeout; `rem < asked` and `rem + elapsed >= asked`
    when woken); invalid/O_PATH descriptors report POLLNVAL / ready / EBADF.

    COM1 must report input by interrupt. C10's input thread feeds the tty,
    whose notifications wake pollers. `serial poll` counts those notifications
    and checks that all subscriptions are removed. Repeat up to `ATTEMPTS`:
    input can legitimately arrive before a poll subscribes, but twenty runs
    without a notification fail. The kernel test separately confirms a blocked
    poller wakes, so this counter is not a wall-clock latency assertion.
    """
    disk = copy_rootfs(tmp_path)
    m = boot_shell(boot, devices=Devices(blk_image=disk, rng=True))

    serial = run(m, "serial")
    assert "serial: COM1 input by interrupt (ISA IRQ 4," in serial, serial
    assert _serial_poll(m) == (0, 0, 0)

    # `ready` precedes syscall entry, not the block. Repeat up to ATTEMPTS
    # successful runs: one with no wake is legitimate, ATTEMPTS without one
    # fails. Bound the run count so a missing wake cannot hang the CI job.
    for _ in range(ATTEMPTS):
        hits, woken, subs = _console_poll_run(m)
        if woken >= 1:
            break
    else:
        raise AssertionError(
            f"no console poll subscription was notified in {ATTEMPTS} runs: {(hits, woken, subs)}"
        )

    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_pipes_and_poll(boot, tmp_path):
    """M8 C7 ring-3 check of pipes, FIFOs, `dup`/`dup2`/`dup3`, `O_CLOEXEC`,
    `SIGPIPE`, `F_SETPIPE_SZ`, `FIONREAD`, `posix_spawn`, `fork`'s `inc_fd_ref`
    for pipes and `flock`, `mknodat` errno rows, and an ext4 FIFO checked with
    host `e2fsck -fn`."""
    disk = copy_rootfs(tmp_path)
    m = boot_shell(boot, devices=Devices(blk_image=disk, rng=True))

    start = len(m.output)
    m.send_line(f"linux {M8_IO} pipes")

    for check in (
        "busybox_pipeline",
        "pipe_buf_atomicity",
        "eof_fionread_and_sigpipe",
        "nonblock_and_setpipe_sz",
        "pipe_writev_and_faults",
        "blocked_sigpipe_and_writev",
        "proc_pipe_reopen_and_large_write",
        "dup_redirections",
        "fifo_open_rules_and_poll",
        "posix_spawn_cloexec",
        "fork_inc_fd_ref",
        "mknodat_and_ext4_fifo",
    ):
        m.expect(re.compile(rf"^ok {check}\r?$", re.MULTILINE), 30)

    m.expect(rf"linux: {re.escape(M8_IO)} exit=0 leaked_frames=-?\d+\n", 20)
    m.expect(r"lumen> ", 10)
    assert CRASH_BEGIN not in m.output[start:]

    run(m, "sync")
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS

    # Confirm the ext4 FIFO created (and the second one unlinked) on the rootfs
    # leave an e2fsck-clean ext4 image.
    e2fsck_check(disk)

    names = debugfs_names(disk, "/")
    assert "ext4-fifo-keep" in names
    assert "ext4-fifo-del" not in names
    stat = debugfs(disk, "stat /ext4-fifo-keep")
    assert "Type: FIFO" in stat, stat
    flags = re.search(r"Flags:\s+(0x[0-9a-fA-F]+)", stat)
    assert flags and int(flags[1], 16) & 0x80000 == 0, stat
    assert "Blockcount: 0" in stat, stat
    # imap gives the on-disk inode position; inspect all 60 i_block bytes,
    # including bytes debugfs's block listing may omit for special files.
    location = debugfs(disk, "imap /ext4-fifo-keep")
    match = re.search(r"block (\d+), offset (0x[0-9a-fA-F]+)", location)
    assert match, location
    stats = debugfs(disk, "stats")
    block_size = re.search(r"Block size:\s+(\d+)", stats)
    assert block_size, stats
    with disk.open("rb") as image:
        image.seek(int(match[1]) * int(block_size[1]) + int(match[2], 16) + 40)
        assert image.read(60) == bytes(60)


def test_pty_termios_and_job_control(boot, tmp_path):
    """C9: real PTY data, timers, process groups, session detach and hangup."""
    disk = copy_rootfs(tmp_path)
    m = boot_shell(boot, devices=Devices(blk_image=disk, rng=True))
    start = len(m.output)
    program = "/usr/local/bin/m8_pty"
    m.send_line(f"linux {program}")
    for check in (
        "pty_data_and_timers",
        "pty_job_control_and_hangup",
        "pty_orphans_and_leader_exit",
    ):
        m.expect(re.compile(rf"^ok {check}\r?$", re.MULTILINE), 60)
    m.expect(rf"linux: {re.escape(program)} exit=0 leaked_frames=-?\d+\n", 30)
    m.expect(r"lumen> ", 10)
    assert CRASH_BEGIN not in m.output[start:]
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_console_ash_job_control(boot, tmp_path):
    """C10: ash owns the console, controls real jobs, and returns it to Lumen."""
    from lumen_harness.gui import type_confirmed
    from lumen_harness.screen import expect_screen

    disk = copy_rootfs(tmp_path)
    m = boot_shell(boot, devices=Devices(blk_image=disk, rng=True))
    start = len(m.output)
    prompt = r"[~/][^\n]* # "
    m.send_line("linux /bin/busybox ash -i")
    m.expect(prompt, 30)

    def command(text, result):
        m.send_line(text)
        m.expect(re.compile(result, re.MULTILINE), 30)
        m.expect(prompt, 30)

    command("printf 'console-pipe\\n' | /bin/busybox tr a-z A-Z", r"^CONSOLE-PIPE\r?$")
    m.send_line("/bin/busybox ash -c 'printf \"BG_%s\\n\" READY; while :; do sleep 1; done' &")
    # The child can print after the prompt. Require its completed output line;
    # the split format keeps a partially echoed command from matching readiness.
    m.expect(re.compile(r"BG_READY\r?\n"), 30)
    command("jobs", r"Running.*ash -c")
    command("kill $!; wait $!; printf 'BG_DONE\\n'", r"^BG_DONE\r?$")

    m.send_line(
        "/bin/busybox ash -c 'trap \"echo FG_CONTINUED\" CONT; echo FG_READY; while :; do sleep 1; done'"
    )
    m.expect(re.compile(r"^FG_READY\r?$", re.MULTILINE), 30)
    m.send("\x1a")
    m.expect(r"Stopped", 30)
    m.expect(prompt, 30)
    command("jobs", r"Stopped.*ash -c")
    m.send_line("fg")
    m.expect(re.compile(r"^FG_CONTINUED\r?$", re.MULTILINE), 30)
    m.send("\x03")
    m.expect(prompt, 30)
    command("printf 'SHELL_ALIVE\\n'", r"^SHELL_ALIVE\r?$")

    m.send_line(
        "stty -echo; printf 'READ_READY\\n'; read -r value < /dev/tty; stty echo; printf 'TTY_READ:%s\\n' \"$value\""
    )
    m.expect(re.compile(r"^READ_READY\r?$", re.MULTILINE), 30)
    noecho = len(m.output)
    m.send("console-abx\x7fc\n")
    m.expect(re.compile(r"^TTY_READ:console-abc\r?$", re.MULTILINE), 30)
    m.expect(prompt, 30)
    assert "console-abx" not in m.output[noecho:]

    m.send_line(
        "stty -icanon -echo min 1 time 0; printf 'RAW_READY\\n'; dd bs=1 count=1 2>/dev/null | od -An -tu1; stty sane; printf 'RAW_DONE\\n'"
    )
    m.expect(re.compile(r"^RAW_READY\r?$", re.MULTILINE), 30)
    m.send("Q")
    m.expect(re.compile(r"^\s*81\r?$", re.MULTILINE), 30)
    m.expect(re.compile(r"^RAW_DONE\r?$", re.MULTILINE), 30)
    m.expect(prompt, 30)
    m.send_line("exit 0")
    m.expect(r"linux: /bin/busybox exit=0 leaked_frames=-?\d+\n", 30)
    m.expect(r"lumen> ", 10)
    assert run(m, "echo console-returned") == "console-returned\n"
    assert "can't access tty" not in m.output[start:]
    assert "job control turned off" not in m.output[start:]
    assert CRASH_BEGIN not in m.output[start:]

    # Serial is back at its kernel prompt, so each PS/2 key is confirmed by kbd.
    type_confirmed(
        m,
        'linux /bin/busybox ash -c \'echo VT_READY; read x < /dev/tty; printf "VT_READ:%s\\n" "$x"\'\n',
    )
    expect_screen(m, r"^VT_READY$")
    type_confirmed(m, "console-vt\n")
    expect_screen(m, r"^VT_READ:console-vt$")
    expect_screen(m, r"linux: /bin/busybox exit=0")
    type_confirmed(m, "echo vt-returned\n")
    expect_screen(m, r"^vt-returned$")
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
