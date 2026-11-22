"""Run the in-kernel test suite (`--features ktest`) and check its TAP output."""

import re

import pytest
from lumen_harness import save_ktest_tap, tap
from lumen_harness.clock_diags import check_clock_diags
from lumen_harness.qemu import EXIT_CRASH, EXIT_FAILURE, EXIT_SUCCESS, Devices


def boot_ktests(boot, cmdline=None):
    """The ktest machine: with other CPUs online (idle), so the per-CPU
    state, the heap, the stack registry and the frame allocator are tested
    with their stacks and state in place, as on any multi-CPU machine. With
    virtio devices no driver takes (network, 9p, a memory balloon, a console,
    SCSI, GPU, four keyboards and a mouse), for the virtio adapter's and
    MSI-X's ktests to claim and probe, plus virtio-blk and virtio-rng, bound
    at boot, for the blk and rng ktests."""
    extra = ["-smp", "4"]
    for device in (
        "virtio-balloon-pci",
        "virtio-serial-pci",
        "virtio-scsi-pci",
        "virtio-gpu-pci",
        "virtio-mouse-pci",
        "virtio-keyboard-pci",
        "virtio-keyboard-pci",
        "virtio-keyboard-pci",
    ):
        extra += ["-device", device]
    return boot(
        "ktest",
        extra=extra,
        cmdline=cmdline,
        devices=Devices(blk_mib=1, net=True, share=True, rng=True, keyboard=True),
    )


def finished(m, save=False):
    """The ktest run's output and TAP report, every test passed; `save` keeps
    the TAP for `make status`."""
    code = m.wait_exit(120)
    out = m.output
    report = tap.parse(out)
    # Saved for `make status` (each ktest is listed individually in STATUS.md);
    # a re-run's (LUMEN_ARTIFACTS_DIR) is kept apart: see save_ktest_tap.
    if save:
        save_ktest_tap(out[out.find("TAP version") :] if "TAP version" in out else "")
    if code == EXIT_CRASH:
        crash = out[out.find("==== LUMEN CRASH REPORT") :]
        last = report.cases[-1].name if report.cases else "(before the first test)"
        raise AssertionError(f"kernel crashed during the test after {last}:\n{crash}")
    assert report.complete, f"incomplete TAP stream: {report.summary()}\n{out[-2000:]}"
    assert report.all_passed, report.summary()
    assert code == EXIT_SUCCESS, f"QEMU exit {code}, expected {EXIT_SUCCESS}"
    return out, report


def test_in_kernel_tests_pass(boot):
    out, report = finished(boot_ktests(boot), save=True)
    # The other CPUs really were online while the tests ran.
    assert "INFO  smp: 4 CPUs online\n" in out, "the APs didn't come online"
    assert len(report.cases) >= 7
    # `device_vectors_are_not_fatal` raises the unused vector twice: one
    # warning, not one per interrupt.
    assert out.count("unexpected interrupt on vector 0x30") == 1, "warn-once"
    # It raises the last MSI vector twice and the one below once, and
    # `msi_vector_delivered_by_the_apic_is_eoid` sends the third from the
    # top as a self-IPI: vectors the allocator (lowest first) hasn't handed
    # out, so each is reported once, as an error.
    for vector in ("0xef", "0xee", "0xed"):
        line = f"ERROR irq: interrupt on MSI vector {vector}, which no device has claimed"
        assert out.count(line) == 1, f"MSI vector {vector} not reported exactly once as an error"
    # The last I/O APIC vector (no handler) is still a plain warning.
    assert out.count("WARN  irq: unexpected interrupt on vector 0x8f") == 1, "0x8f not a warning"
    # `an_unexpected_storm_is_reported_sparsely` raises 0x31 2048 times:
    # reported at the 1st, 1024th and 2048th only.
    storm = re.findall(r"unexpected interrupt on vector 0x31 \(rip 0x[0-9a-f]+, #(\d+)\)", out)
    assert storm == ["1", "1024", "2048"], f"0x31 storm reports: {storm}"
    # `probing_records_each_outcome` (#95): boot's own pass binds
    # virtio-rng (#97) and virtio-blk (#249; the tablet's and sound's rows,
    # #98 and #99, find no device on the ktest machine); the ktest's table
    # declines the 9p device once alone, then declines it (leaving a run) and
    # fails it (leaving one of its own), finds the balloon not ready, binds
    # the console, and refuses the SCSI device's decline (its probe kept the
    # resources); offered again, the 9p device isn't probed.
    assert re.search(
        r"INFO  drivers: 2 bound, 0 failed, 0 declined, 0 not probed \(\d+ driver rows?\)\n", out
    )
    assert "INFO  rng: sources: virtio-rng\n" in out
    p9 = re.search(r"INFO  drivers: probing ktest-9p on (\S+)\n", out)
    assert p9, "no ktest-9p probe logged"
    addr = re.escape(p9[1])
    assert len(re.findall(rf"INFO  drivers: probing ktest-decline on {addr}\n", out)) == 2
    assert len(re.findall(rf"INFO  drivers: {addr}: ktest-decline declined it\n", out)) == 1
    assert re.search(
        rf"WARN  drivers: {addr}: ktest-decline declined it \(1 DMA runs left outstanding\)\n", out
    ), "the declined probe's leftover run isn't reported"
    # The failed probe is charged only its own run, not the declined one's.
    assert re.search(
        rf"WARN  drivers: {addr}: ktest-9p probe failed: the device didn't answer in 0 ms "
        r"\(status 0x00\) \(1 DMA runs left outstanding\)\n",
        out,
    ), "the failed probe isn't logged as expected"
    assert re.search(
        r"WARN  drivers: \S+: ktest-unready probe failed: the device status reads 0x00 after setup: "
        r"FEATURES_OK clear",
        out,
    ), "a probe whose device isn't ready isn't failed"
    assert re.search(r"INFO  drivers: probing ktest-console on \S+\n", out)
    assert re.search(r"WARN  pci: \S+: ktest-keeper declined it but kept its resources", out)
    # ... and `pci::failed_after_bind` on the bound console, whose transport
    # the ktest keeps: warned about.
    assert re.search(
        r"WARN  pci: \S+: ktest-console gave it up after binding but its resources are still live\n",
        out,
    )
    for summary in (
        "0 bound, 0 failed, 1 declined, 0 not probed (1 driver row)",
        "1 bound, 3 failed, 1 declined, 0 not probed (7 driver rows)",
        "0 bound, 0 failed, 0 declined, 1 not probed (1 driver row)",
    ):
        assert f"INFO  drivers: {summary}\n" in out, summary
    assert re.search(
        rf"WARN  drivers: {addr}: not probed for ktest-9p-again: already claimed \(ktest-9p: failed: ",
        out,
    ), "the second offer isn't refused as expected"
    assert "probing never on" not in out
    # shell::the_serial_prompt_waits_on_past_its_warning: the serial
    # prompt's warning names the stand-in shell not yet waiting, with its
    # state (sleeping; ready if the warning came before it first ran, and
    # then the early one may be named too), and once both wait the release
    # is logged with how long it held.
    assert re.search(
        r"WARN  shell: serial prompt held: (waits-early \(ready\), )?"
        r"waits-late \((sleeping|ready)\) not yet waiting for keys \(still waiting\)\n",
        out,
    ), "the serial prompt's warning"
    assert re.search(
        r"INFO  shell: serial prompt released after \d+ ms: every screen shell waits for keys\n",
        out,
    ), "the serial prompt's release"
    # The only mouse error is the hold no thread answers, made on purpose by
    # mouse::a_hold_that_no_thread_answers_is_taken_back (logged as a bug).
    mouse_errors = re.findall(r"ERROR mouse: (.*)\n", out)
    assert mouse_errors == [
        "0000:ff:1e.7: the mouse thread didn't stop reading within 1000 ms (a bug): not held"
    ], mouse_errors
    # The only virtio errors are the refused frees the DMA ktests make on
    # purpose, each saying why.
    errors = re.findall(r"ERROR virtio: (.*)\n", out)
    # (Counted, in any order: the ktests' order is the linker's.)
    refusals = {
        r"dma_dealloc\(0x[0-9a-f]+, 0x[0-9a-f]+, 2 pages\) refused: the run there is Dma \{ pages: 3 \}": 1,
        # Another address inside the run, and the second free.
        r"dma_dealloc\(0x[0-9a-f]+, 0x[0-9a-f]+, 3 pages\) refused: no run is outstanding there": 2,
        r"dma_dealloc\(0x[0-9a-f]+, 0x1, 3 pages\) refused: dma_alloc returned 0x[0-9a-f]+ for it": 1,
        r"dma_dealloc\(0x[0-9a-f]+, 0x[0-9a-f]+, 1 pages\) refused: the run there is Bounce \{ len: 100 \}": 1,
    }
    counts = {want: sum(1 for e in errors if re.fullmatch(want, e)) for want in refusals}
    assert counts == refusals and len(errors) == sum(refusals.values()), errors
    # Every transport dropped in the ktests reset its device.
    assert "after a reset" not in out
    check_vfs_log_lines(out)
    # Issue #3: the trace cost per event is printed as a TAP diagnostic.
    assert re.search(r"^# trace: \d+ TSC ticks per event", out, re.MULTILINE), (
        "no trace cost diagnostic"
    )
    # #117: the clock cross-checks report their brackets, retries and
    # placement tries on passing runs too, where nobody reads the TAP.
    check_clock_diags(report, out)


# The vfs ktests' deliberate warnings and errors, each made by a ktest that
# injects the failure (fault-injecting block caches, a hard page limit, a
# quota over-release, a non-UTF-8 name, a read-only feature bit). The fault
# sweeps' families (link counts kept in memory after a failed undo, rename
# undos left inconsistent, unlinks whose outcome can't be read back, renames'
# replaced targets whose free failed) depend on how many writes `ext4plus`
# makes, so they're counted as present; every
# other line is counted exactly, and a line matching none of these fails.
_DEV = r"DevId\(\d+\)"
VFS_SWEEP_LINES = (
    (
        rf"ERROR vfs: ext4 {_DEV} inode \d+: links_count on disk is (\d+|unreadable), keeping (at least )?\d+ "
        r"in memory until a write lands; run e2fsck if the volume is unmounted first"
    ),
    (
        rf"ERROR vfs: ext4 {_DEV}: undoing a failed (rename|exchange) between directories \d+ and \d+ left its "
        r"entries inconsistent or unreadable; run e2fsck"
    ),
    (
        rf"ERROR vfs: ext4 {_DEV} inode \d+: unlink failed and neither its directory nor the inode reads back; "
        r"assuming the entry survived; run e2fsck"
    ),
    # A rename's replaced target whose free failed inside `ext4plus` (left
    # for e2fsck; the rename itself went through).
    (
        rf"ERROR vfs: ext4 {_DEV} directory \d+: freeing inode \d+, replaced by a rename, failed: \w+; "
        r"run e2fsck"
    ),
)
VFS_EXACT_LINES = {
    # page_quota_release_saturates_and_detached_quota_reports_nospc
    r"ERROR vfs: page quota ktest \(4 pages\): released 5 pages with only 2 reserved": 1,
    # ext4_unlink_restores_an_inline_symlink_when_the_directory_cant_be_read_back
    r"ERROR vfs: ext4 inode \d+: restoring the inline symlink target failed; run e2fsck": 1,
    # ext4_truncate_and_open_unlink_preserve_state_on_io_error
    rf"ERROR vfs: ext4 {_DEV} inode \d+: truncate failed and its tail bytes couldn't be restored on disk": 1,
    # The writeback-error ktests (reclaim, sync, full disk, failing device,
    # a failing page among good ones; linux::syscall::io's
    # io_fsync_writes_an_ext4_file_back_and_reports_eio: one failed page).
    rf"WARN  vfs: ext4 {_DEV} inode \d+: writeback of page \d+ \(offset \d+\) failed: NoSpc": 4,
    rf"WARN  vfs: ext4 {_DEV} inode \d+: writeback of page \d+ \(offset \d+\) failed: Io": 3,
    rf"WARN  vfs: ext4 {_DEV} inode \d+: writeback of its metadata failed: Io": 1,
    # ext4_mmap_page_reports_a_failed_disk_read: two streaks (two failed
    # fills, a good one, a third failed fill), one warning each; and
    # linux::syscall::io's io_pread_pwrite_report_filesystem_errors: one.
    rf"WARN  vfs: ext4 {_DEV} inode \d+: reading page 1 \(offset 4096\) failed: Io": 3,
    # mm::user_vm::file_vm_disk_read_error_is_a_bus_error: a fault's fill.
    rf"WARN  vfs: ext4 {_DEV} inode \d+: reading page 2 \(offset 8192\) failed: Io": 1,
    rf"WARN  vfs: sync: ext4 {_DEV} inode \d+: NoSpc": 1,
    rf"WARN  vfs: sync: ext4 {_DEV} inode \d+: Io": 1,
    # linux::syscall::io: io_fsync_writes_an_ext4_file_back_and_reports_eio's
    # `fsync` with the block cache's sync failing, and
    # io_sync_flushes_every_mounted_filesystem's `sync` with one volume's.
    rf"WARN  vfs: fsync: ext4 {_DEV} inode \d+: block cache: block device I/O error": 1,
    rf"WARN  vfs: sync: ext4 {_DEV} block cache: block device I/O error": 1,
    # ext4_writes_past_the_hard_page_limit_fail: two writes refused;
    # ext4_mmap_page_hands_out_the_cached_page_filled_from_disk: a page.
    (
        r"WARN  vfs: ext4 page cache at its hard limit \(\d+ pages\): refusing new pages \(ENOSPC\) until "
        r"writeback or unmapping frees some"
    ): 3,
    # ext4_unwritable_ro_compat_features_force_read_only
    (
        rf"WARN  vfs: ext4 on {_DEV}: mounting read-only: ro_compat feature not supported for writing "
        r"\(metadata_csum, quota, bigalloc, orphan_present, \.\.\.\) \(ro_compat 0x[0-9a-f]+\)"
    ): 1,
    # ext4_readdir_skips_names_that_are_not_utf8 and
    # linux::syscall::dir::dir_getdents64_lists_across_a_gap_in_ext4_cursors:
    # one directory each.
    rf"WARN  vfs: ext4 {_DEV} directory \d+: skipping entries whose names aren't UTF-8 \(inode \d+\)": 2,
}


def check_vfs_log_lines(out):
    """Pin the vfs ktests' WARN and ERROR lines (see VFS_EXACT_LINES)."""
    lines = re.findall(r"((?:WARN |ERROR) vfs: .*)\n", out)
    counts = {
        want: sum(1 for line in lines if re.fullmatch(want, line)) for want in VFS_EXACT_LINES
    }
    assert counts == VFS_EXACT_LINES, counts
    sweeps = {
        want: sum(1 for line in lines if re.fullmatch(want, line)) for want in VFS_SWEEP_LINES
    }
    assert all(sweeps.values()), sweeps
    known = (*VFS_EXACT_LINES, *VFS_SWEEP_LINES)
    unknown = [line for line in lines if not any(re.fullmatch(want, line) for want in known)]
    assert not unknown, unknown


# The modules holding the ktests whose claims are tight to the boot's
# calibration (its TSC rate, the timer's, the tick): time and the scheduler
# (others use it, if at all, against loose bounds or the same clock on both
# sides). The stalled
# runs run only these (`lumen.ktest=`): the claim is about them, and the
# rest would only meet a busy host three times per CI run instead of once
# (#183 failed there first).
CALIBRATION_KTESTS = ("lumen_kernel::time::", "lumen_kernel::sched::")


@pytest.mark.parametrize(
    "key, ppm", [("tsc", 6000), ("timer", 12000)], ids=["tsc-0.6%", "timer-1.2%"]
)
def test_in_kernel_tests_pass_with_calibrations_stalled_within_tolerance(boot, key, ppm):
    # Every check moves together (#139): a boot whose calibration filled in
    # (each window's end stalled, so none steady) but stayed within its
    # tolerance logs no warning, and the ktests whose claims are tight to the
    # boot's calibration (`CALIBRATION_KTESTS`) still pass, as do the clock
    # diagnostics' checks (each warning following from its bound). The TSC at
    # 0.6% (under its 1%); the timer at 1.2%, which with the TSC's bound is
    # still under its 2% but leaves its rate about 1.2% off (a ktest budgeting
    # the calibration 1% fails here). At risk on a busy host (CLAUDE.md, "Tests
    # on a busy host"): the host's own stalls, on top of the injected one, can
    # take the boot's calibration past its tolerance, which this claim doesn't
    # cover.
    name = {"tsc": "tsc", "timer": "local APIC timer"}[key]
    only = ",".join(CALIBRATION_KTESTS)
    out, report = finished(
        boot_ktests(boot, cmdline=f"lumen.stall_{key}_windows={ppm} lumen.ktest={only}")
    )
    # Only those ran, some of each, as many as the kernel says it selected,
    # and fewer than all.
    said = re.search(rf"^# ktest: only {re.escape(only)}: (\d+) of (\d+)$", out, re.MULTILINE)
    assert said, "no filter comment"
    # After the results, so it is no test's diagnostic.
    assert out.index("# ktest: only") > out.rindex("\nok "), out[-2000:]
    assert int(said[1]) == report.planned < int(said[2]), (said[0], report.summary())
    assert all(c.name.startswith(CALIBRATION_KTESTS) for c in report.cases), report.summary()
    ran = {p for p in CALIBRATION_KTESTS for c in report.cases if c.name.startswith(p)}
    assert ran == set(CALIBRATION_KTESTS), (ran, report.summary())
    assert (
        f"INFO  time: stalling calibration windows by {ppm},0 ppm (lumen.stall_{key}_windows)\n"
        in out
    )
    assert re.search(
        rf"INFO  time: {name} calibration took 20 windows again, 0 of 5 kept steady", out
    ), out
    # Boot's own lines (the ktests warn on purpose, e.g. a PIT made to fail).
    boot_lines = out[: out.index("TAP version")]
    assert "WARN  time" not in boot_lines, boot_lines
    check_clock_diags(report, out)


def test_a_ktest_filter_that_selects_nothing_fails(boot):
    # `lumen.ktest=` matching no test checks nothing: the run fails (a `1..0`
    # plan alone is TAP's "skip all", which would read as a pass).
    m = boot_ktests(boot, cmdline="lumen.ktest=lumen_kernel::no_such_module::")
    code = m.wait_exit(120)
    out = m.output
    assert code != EXIT_CRASH, out[out.find("==== LUMEN CRASH REPORT") :]
    report = tap.parse(out)
    assert report.planned == 0 and not report.cases, (report.summary(), out[-2000:])
    said = r"^# ktest: only lumen_kernel::no_such_module::: 0 of \d+$"
    assert re.search(said, out, re.MULTILINE), out[-2000:]
    assert "# ktest: no test selected, a failure\n" in out, out[-2000:]
    assert code == EXIT_FAILURE, f"QEMU exit {code}, expected {EXIT_FAILURE}"
