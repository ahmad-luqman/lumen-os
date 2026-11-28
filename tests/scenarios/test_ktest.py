"""Run the in-kernel test suite (`--features ktest`) and check its TAP output."""

import re
from collections import Counter

import pytest
from lumen_harness import REPO_ROOT, klog, save_ktest_tap, tap
from lumen_harness.clock_diags import check_clock_diags
from lumen_harness.qemu import EXIT_CRASH, EXIT_FAILURE, EXIT_SUCCESS, Devices

# kernel/src/ktest.rs's `FIRST`: the tests run before every other.
KTEST_FIRST = (
    "lumen_kernel::trace::boot_phases_recorded_in_order",
    "lumen_kernel::trace::log_stamps_count_from_the_timelines_origin",
)


def boot_ktests(boot, cmdline=None):
    """The ktest machine: with other CPUs online (idle), so the per-CPU
    state, the heap, the stack registry and the frame allocator are tested
    with their stacks and state in place, as on any multi-CPU machine. With
    virtio devices no driver takes (9p, a memory balloon, a console,
    SCSI, GPU, four keyboards and a mouse), for the virtio adapter's and
    MSI-X's ktests to claim and probe, plus virtio-blk, virtio-rng and virtio-net, bound
    at boot, for the blk, rng and net ktests."""
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
    # `ktest::FIRST` runs first, whatever the link order (they read the boot
    # CPU's trace ring back to boot, which the tests before them could wrap),
    # and every test runs once.
    assert [c.name for c in report.cases[:2]] == list(KTEST_FIRST), report.cases[:2]
    names = [c.name for c in report.cases]
    assert len(set(names)) == len(names), sorted(n for n in set(names) if names.count(n) > 1)
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
    # restart_with_no_handler_restarts_or_resumes rejects rip == 1 once
    # for NoIntr and once for RestartBlock, logging both corrupt frames.
    line = "ERROR signal: corrupt syscall restart frame: rip 0x1 is below 2"
    assert out.count(line) == 2, "corrupt restart frames not reported exactly twice"
    # The last I/O APIC vector (no handler) is still a plain warning.
    assert out.count("WARN  irq: unexpected interrupt on vector 0x8f") == 1, "0x8f not a warning"
    # `an_unexpected_storm_is_reported_sparsely` raises 0x31 2048 times:
    # reported at the 1st, 1024th and 2048th only.
    storm = re.findall(r"unexpected interrupt on vector 0x31 \(rip 0x[0-9a-f]+, #(\d+)\)", out)
    assert storm == ["1", "1024", "2048"], f"0x31 storm reports: {storm}"
    # `probing_records_each_outcome` (#95): boot's own pass binds
    # virtio-rng (#97), virtio-net (#473) and virtio-blk (#249; the tablet's and sound's rows,
    # #98 and #99, find no device on the ktest machine); the ktest's table
    # declines the 9p device once alone, then declines it (leaving a run) and
    # fails it (leaving one of its own), finds the balloon not ready, binds
    # the console, and refuses the SCSI device's decline (its probe kept the
    # resources); offered again, the 9p device isn't probed.
    assert re.search(
        r"INFO  drivers: 3 bound, 0 failed, 0 declined, 0 not probed \(\d+ driver rows?\)\n", out
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
    # shell::commands::power_commands_sync_once_then_announce: a failed sync
    # before `reboot` or `shutdown` is a warning on serial too, once each
    # (#352).
    for warning in (
        "reboot: sync failed: Io; restarting anyway",
        "shutdown: sync failed: Io; powering off anyway",
    ):
        assert out.count(f"WARN  power: {warning}\n") == 1, warning
    # The only mouse error is the hold no thread answers, made on purpose by
    # mouse::a_hold_that_no_thread_answers_is_taken_back (logged as a bug).
    mouse_errors = re.findall(r"ERROR mouse: (.*)\n", out)
    assert mouse_errors == [
        "0000:ff:1e.7: the mouse thread didn't stop reading within 1000 ms (a bug): not held"
    ], mouse_errors
    # The only virtio errors are the refused frees the DMA ktests make on
    # purpose, each saying why.
    errors = re.findall(r"ERROR virtio: (.*)\n", out)
    # (Counted, in any order: after `ktest::FIRST`, the ktests' order is the
    # linker's.)
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
    check_held_back_lines(out)
    # Issue #3: the trace cost per event is printed as a TAP diagnostic.
    assert re.search(r"^# trace: \d+ TSC ticks per event", out, re.MULTILINE), (
        "no trace cost diagnostic"
    )
    # #117: the clock cross-checks report their brackets, retries and
    # placement tries on passing runs too, where nobody reads the TAP.
    check_clock_diags(report, out)


# The vfs ktests' deliberate warnings and errors, each made by a ktest that
# injects the failure (fault-injecting block caches, a hard page limit, a
# quota over-release, a read-only feature bit). The fault sweeps' families
# (link counts kept in memory after a failed undo, rename undos left
# inconsistent, unlinks whose outcome can't be read back, renames' replaced
# targets whose free failed, rename targets that couldn't be taken out or put
# back, moved directories whose parents' fix-ups or `..` rewrite failed,
# unlinks and rmdirs whose free failed after the entry went, children
# forgotten after their blocks were being freed and children kept live with
# that untold, #330) depend on how
# many writes `ext4plus` makes, so they're counted as present; every other
# line is counted exactly, and a line matching none of these (or two)
# fails. The sweeps hold their repeats back from serial (`VFS_HELD_KTESTS`,
# `check_vfs_log_lines`): each sweep family has one call site, whose first
# line is printed and whose summary's count goes to that family. An exact
# family's site may make lines of several (writeback's `NoSpc` and `Io`), so
# none is held.
_DEV = r"DevId\(\d+\)"
VFS_SWEEP_LINES = (
    (
        rf"ERROR vfs: ext4 {_DEV} inode \d+: links_count on disk is (\d+|unreadable), keeping (at least )?\d+ "
        r"in memory (until a write lands; run e2fsck if the volume is unmounted first|until reboot \(the volume is "
        r"read-only\); run e2fsck)"
    ),
    (
        rf"ERROR vfs: ext4 {_DEV}: undoing a failed (rename|exchange) between directories \d+ and \d+ left its "
        r"entries inconsistent or unreadable; run e2fsck"
    ),
    # A rename's replaced target whose free failed inside `ext4plus` (left
    # for e2fsck; the rename itself went through).
    (
        rf"ERROR vfs: ext4 {_DEV} directory \d+: freeing inode \d+, replaced by a rename, failed: \w+; "
        r"run e2fsck"
    ),
    # A rename's target that couldn't be taken out, its directory unreadable
    # (ext4_failed_rename_that_cant_read_back_keeps_every_entry).
    (
        rf"ERROR vfs: ext4 {_DEV} directory \d+: taking rename target inode \d+ out failed and the directory "
        r"doesn't read back; assuming its entry is still there; run e2fsck"
    ),
    # A directory move whose unlink failed after taking the old entry out
    # (so it moved), its parents' fix-ups failing too
    # (ext4_directory_move_that_commits_before_failing_fixes_its_parents's
    # pairs of landed failed writes, #323).
    (
        rf"ERROR vfs: ext4 {_DEV}: directory inode \d+ moved from directory \d+ to \d+ despite an unlink error, "
        r"but fixing its parents failed: \w+; run e2fsck"
    ),
    # A moved directory whose `..` rewrite failed (the rename sweeps over
    # directories between parents, and the exchange sweeps).
    (
        rf"ERROR vfs: ext4 {_DEV} directory inode \d+: moved from directory \d+ to \d+, but rewriting its \.\. "
        r"failed: \w+; it may still name \d+; run e2fsck"
    ),
    # An unlink or rmdir that took a last entry out and then failed freeing
    # the inode (the unlink and rmdir fault sweeps).
    (
        rf"ERROR vfs: ext4 {_DEV} directory \d+: freeing inode \d+, whose last entry went, failed: \w+; "
        r"run e2fsck"
    ),
    # An unlink or rmdir whose directory couldn't be read back after
    # `delete_file` had begun freeing the inode's blocks, not yet zeroing it:
    # forgotten, not kept live (#330; the unlink and rmdir fault sweeps), one
    # family per op, so each must name its own.
    *(
        (
            rf"ERROR vfs: ext4 {_DEV} directory \d+: inode \d+ lost its entry and its blocks were being freed "
            rf"\(block \d+ is free\) when the {op} failed, the directory unreadable; forgetting it; run e2fsck"
        )
        for op in ("unlink", "rmdir")
    ),
    # One whose directory couldn't be read back and whose entry's and
    # blocks' state couldn't be told from its inode either (the inode, its
    # map or its bitmap byte unreadable, or a linked one reading corrupt):
    # kept live, saying so and why (the unlink and rmdir sweeps' legs with
    # the block bitmap unreadable, the corrupt-inode ktest, and the unlinks
    # with every read failing), one family per op.
    *(
        (
            rf"ERROR vfs: ext4 {_DEV} directory \d+: {op} of inode \d+ failed, the directory unreadable, "
            r"and whether its entry went and its blocks were freed can't be told \(.+\); keeping it live, "
            r"(so writes through it may reach another file if they were|the volume read-only, so nothing reaches "
            r"another file); run e2fsck"
        )
        for op in ("unlink", "rmdir")
    ),
    # A rename's target put back after a failed move, the put-back failing
    # too (ext4_failed_rename_over_whose_put_back_fails_leaves_the_target_allocated).
    (
        rf"ERROR vfs: ext4 {_DEV} directory \d+: putting rename target inode \d+ back after a failed move "
        r"failed: \w+; its data is intact; run e2fsck"
    ),
)
# The volume's `errors=remount-ro` line (#349): once per ext4 volume a
# failed write makes read-only, so once per leg of every write-fault sweep
# (held back, inside their guards; so is any ktest erroring more than one
# volume) and once in each other ktest that fails a write, printed outside
# any guard: counted as present, those outside guards exactly
# (`VFS_ERRORED_UNGUARDED`). One call site, so a guard prints it at most once.
VFS_ERRORED_LINES = (
    (
        rf"ERROR vfs: ext4 {_DEV}: (unlink|rmdir|rename|link|create|mkdir|symlink|truncate|chmod|chown|"
        r"utimensat|writeback|fsync|sync|a read's eviction) failed writing to the device \([^()]+\); the volume is read-only "
        r"until reboot \(errors=remount-ro\); run e2fsck -f"
    ),
)
VFS_HOLDABLE_LINES = (*VFS_SWEEP_LINES, *VFS_ERRORED_LINES)
# The ktests erroring one volume outside a guard, one line each: the
# writeback ktests (a sync stopping at a failing inode, a failing page's
# EIO, reclaim's write error, a write whose reclaim fails), vfs::tests'
# sync stopping at a failed write and flush/reclaim write error, io's sync
# of every filesystem, mem's msync, meta's failed chown, an older mapping's
# volume going read-only, the latch settling at the last operation's end,
# the device write failing part-way through a sync (#349), and
# ext4_allocation_corruption_keeps_the_first_failure_cause's device-first leg,
# and ext4_mount_unlink_inode_table_device_failure_is_read_only.
VFS_ERRORED_UNGUARDED = 14
VFS_EXACT_LINES = {
    # #355: count/ownership failures in both mapping formats, dangling
    # dirents (one from a real failed device write), and an unreadable bitmap.
    # Only proven inconsistency recommends e2fsck.
    (
        rf"WARN  vfs: ext4 on {_DEV}: mounting read-only: group 0 block free count 250 != bitmap 249; "
        r"run e2fsck -f"
    ): 1,
    rf"WARN  vfs: ext4 on {_DEV}: mounting read-only: inode \d+ block \d+ is mapped but free; run e2fsck -f": 2,
    rf"WARN  vfs: ext4 on {_DEV}: mounting read-only: inode \d+ block \d+ is mapped twice; run e2fsck -f": 2,
    rf"WARN  vfs: ext4 on {_DEV}: mounting read-only: directory inode 2 references free or out-of-range inode 11; run e2fsck -f": 2,
    rf"WARN  vfs: ext4 on {_DEV}: cannot check allocation: filesystem metadata block 2 read failed: Io; mounting read-only": 1,
    rf"WARN  vfs: ext4 on {_DEV}: mounting read-only: invalid allocation metadata: volume exceeds device capacity; run e2fsck -f": 1,
    # page_quota_release_saturates_and_detached_quota_reports_nospc
    r"ERROR vfs: quota ktest \(16384 bytes\): released 20480 bytes with only 8192 reserved": 1,
    r"ERROR vfs: quota ktest \(16384 bytes\): dropped with 12289 bytes still reserved": 1,
    # tmpfs_quota_dropped_with_bytes_reserved_logs_the_leak: a mount's own
    # quota, named by its device
    r"ERROR vfs: quota tmpfs 0:143 \(4096 bytes\): dropped with 16 bytes still reserved": 1,
    # ext4_unlink_restores_an_inline_symlink_when_the_directory_cant_be_read_back
    r"ERROR vfs: ext4 inode \d+: restoring the inline symlink target failed; run e2fsck": 1,
    # ext4_truncate_and_open_unlink_preserve_state_on_io_error
    rf"ERROR vfs: ext4 {_DEV} inode \d+: truncate failed and its tail bytes couldn't be restored on disk": 1,
    # Writeback-error ktests: NoSpc only for the full disk. Since #350,
    # allocation I/O failures keep EIO: ext4_sync_stops_at_the_first_failed_write,
    # ext4_a_failed_page_writeback_errors_the_volume_as_eio, reclaim's write
    # error and the write whose reclaim fails. The six older EIO cases are
    # flush/reclaim's two, io_fsync_writes_an_ext4_file_back_and_reports_eio,
    # mem_msync_returns_a_writeback_error_before_enomem, older shared mappings,
    # and ext4_nothing_reaches_the_device_after_the_first_failed_write.
    # Corruption adds three legs of
    # ext4_block_bitmap_mismatch_fails_read_only_without_reallocation and
    # four of ext4_block_bitmap_padding_is_never_allocated.
    rf"WARN  vfs: ext4 {_DEV} inode \d+: writeback of page \d+ \(offset \d+\) failed: NoSpc": 1,
    rf"WARN  vfs: ext4 {_DEV} inode \d+: writeback of page \d+ \(offset \d+\) failed: Io": 17,
    # Those same seven corrupt block-allocation legs: the precise group,
    # bitmap and operation in their one errors=remount-ro line (#350).
    (
        rf"ERROR vfs: ext4 {_DEV}: fsync found group 0's block bitmap has no free bit despite a positive free count; "
        r"the volume is read-only until reboot \(errors=remount-ro\); run e2fsck -f"
    ): 7,
    # ext4_inode_bitmap_mismatch_fails_read_only_without_reallocation.
    (
        rf"ERROR vfs: ext4 {_DEV}: create found group 0's inode bitmap has no free bit despite a positive free count; "
        r"the volume is read-only until reboot \(errors=remount-ro\); run e2fsck -f"
    ): 1,
    # ext4_allocation_corruption_allows_mkdir_undo_before_freezing: no orphan ERROR.
    (
        rf"ERROR vfs: ext4 {_DEV}: mkdir found group 0's block bitmap has no free bit despite a positive free count; "
        r"the volume is read-only until reboot \(errors=remount-ro\); run e2fsck -f"
    ): 1,
    # ext4_allocation_corruption_keeps_the_first_failure_cause: corruption-first leg.
    (
        rf"ERROR vfs: ext4 {_DEV}: writeback found group 0's block bitmap has no free bit despite a positive free count; "
        r"the volume is read-only until reboot \(errors=remount-ro\); run e2fsck -f"
    ): 1,
    rf"WARN  vfs: ext4 {_DEV} inode \d+: writeback of its metadata failed: Io": 1,
    # ext4_mmap_page_reports_a_failed_disk_read: two streaks (two failed
    # fills, a good one, a third failed fill), one warning each;
    # linux::syscall::io's io_pread_pwrite_report_filesystem_errors: one;
    # and #321's three: page_cache's
    # page_cache_a_read_failing_past_its_first_page_returns_the_bytes_before_it
    # (a short read, then the failing page's EIO: one streak) and
    # ext4_a_write_whose_fill_read_fails_logs_it_as_a_read_does (a write's
    # partial-page fill, logged as a read's is: two streaks, a failure past
    # the write's first page and one on it, a fill that reads between).
    rf"WARN  vfs: ext4 {_DEV} inode \d+: reading page 1 \(offset 4096\) failed: Io": 6,
    # mm::user_vm::file_vm_disk_read_error_is_a_bus_error: a fault's fill.
    rf"WARN  vfs: ext4 {_DEV} inode \d+: reading page 2 \(offset 8192\) failed: Io": 1,
    # (vfs_sync_stops_at_a_failed_write_and_reports_it's,
    # ext4_sync_stops_at_the_first_failed_write's, and
    # ext4_nothing_reaches_the_device_after_the_first_failed_write's: one
    # each, the sync stopping at the inode whose write failed, #349)
    rf"WARN  vfs: sync: ext4 {_DEV} inode \d+: Io": 3,
    # linux::syscall::io: io_fsync_writes_an_ext4_file_back_and_reports_eio's
    # `fsync` with the block cache's sync failing, and
    # io_sync_flushes_every_mounted_filesystem's `sync` with one volume's;
    # and ext4_a_failed_device_write_makes_the_volume_read_only (#331, #349):
    # its fsync's, the device's writes failing under the block cache (the
    # volume read-only after it, its own sync then writes nothing and warns
    # nothing).
    rf"WARN  vfs: fsync: ext4 {_DEV} inode \d+: block cache: block device I/O error": 2,
    # io_sync_flushes_every_mounted_filesystem and
    # ext4_mount_unlink_inode_table_device_failure_is_read_only, one each.
    rf"WARN  vfs: sync: ext4 {_DEV} block cache: block device I/O error": 2,
    # ext4_writes_past_the_hard_page_limit_fail: two writes refused;
    # ext4_mmap_page_hands_out_the_cached_page_filled_from_disk: a page;
    # page_cache_insert_refuses_a_dirty_ext4_page_at_the_hard_limit,
    # page_cache_write_from_refuses_at_the_hard_limit_before_filling,
    # page_cache_write_from_refuses_a_page_the_cache_filled_up_for,
    # page_cache_map_page_asks_the_hard_cap_before_and_after_make and
    # page_cache_ext4_fdatasync_after_a_refused_write_past_the_end_succeeds (#321,
    # a write refused past the end): one each
    # (one crossing each, however many refusals: page_cache's
    # hard_limit_crossing).
    (
        r"WARN  vfs: ext4 page cache at its hard limit \(\d+ pages\): refusing new pages \(ENOSPC\) until "
        r"writeback or unmapping frees some"
    ): 8,
    # ext4_a_bad_group_geometry_refuses_the_mount_and_bigalloc_still_mounts_read_only:
    # the refused superblock.
    rf"WARN  vfs: ext4 on {_DEV}: refusing to mount: s_blocks_per_group 32769 not in 1\.\.=32768": 1,
    # vfs::tests' ext4_adapter_pagecache_quirks_and_open_unlinked: a 64-bit
    # superblock with a 32-byte descriptor size.
    rf"WARN  vfs: ext4 on {_DEV}: refusing to mount: s_desc_size 32 on a 64-bit volume, not 64": 1,
    # ext4_unwritable_ro_compat_features_force_read_only, and the bigalloc
    # volume of ext4_a_bad_group_geometry_refuses_the_mount_and_bigalloc_still_mounts_read_only
    (
        rf"WARN  vfs: ext4 on {_DEV}: mounting read-only: ro_compat feature not supported for writing "
        r"\(metadata_csum, quota, bigalloc, orphan_present, \.\.\.\) \(ro_compat 0x[0-9a-f]+\)"
    ): 2,
    # ext4_an_unsigned_hash_volume_mounts_writable_and_hashes_names_unsigned
    # (#308, #313): the volume with neither hash flag (and dir_index), not the
    # unsigned, both-flags or signed one's; and the neither-flag one with a
    # journal needing recovery, which outranks it: its line names the journal
    # and shows no s_flags.
    (
        rf"WARN  vfs: ext4 on {_DEV}: mounting read-only: its directories may hash names unsigned "
        r"\(dir_index, but neither hash flag in s_flags\), which ext4plus can't write "
        r"\(ro_compat 0x[0-9a-f]+, s_flags 0x0\)"
    ): 1,
    rf"WARN  vfs: ext4 on {_DEV}: mounting read-only: the journal needs recovery \(ro_compat 0x[0-9a-f]+\)": 1,
}


# The ktests that hold their repeated vfs lines back from serial
# (`klog::hold_repeats`, #347): the fault sweeps, which logged about 3,600 of
# the run's 3,700 WARN and ERROR lines. In #332's failed CI run each took
# about 30 ms on the UART under KVM, taking the run past `finished`'s bound.
# Each guard is a `hold_repeats("vfs")` call in the kernel's source, and
# shows on serial between its first and last line (`klog.HOLDING`,
# `klog.HELD_ALL`).
_GUARD = 'hold_repeats("vfs")'
VFS_HELD_KTESTS = sum(
    1
    for path in (REPO_ROOT / "kernel/src").rglob("*.rs")
    for line in path.read_text().splitlines()
    if _GUARD in line and not line.lstrip().startswith("//")
)


def vfs_family(line):
    """The one family of VFS_EXACT_LINES or VFS_SWEEP_LINES that vfs line
    `line` (from its level on) fits, or None; a line fitting two fails."""
    fits = [want for want in (*VFS_EXACT_LINES, *VFS_HOLDABLE_LINES) if re.fullmatch(want, line)]
    assert len(fits) <= 1, (line, fits)
    return fits[0] if fits else None


def check_vfs_log_lines(out):
    """Pin the vfs ktests' WARN and ERROR lines (see VFS_EXACT_LINES), in one
    pass over the log's lines: those printed, plus those `klog::hold_repeats`
    held back, counted from their summaries (`klog.HELD_BACK`: a call site's
    lines after its printed first, and a copy of the newest, whose family the
    count goes to). Each vfs guard (`klog.HOLDING` to `klog.HELD_ALL`):
    - holds only sweep lines (each sweep family has one call site; an exact
      family's site can make lines of several, so none is held);
    - prints at most one line of each sweep family, the first of its site;
    - has a summary only for a family it printed, and only vfs summaries;
    - ends with a total that is the sum of its summaries.
    Every printed sweep line and every vfs summary falls inside a vfs guard,
    and the guards on serial are the kernel source's: a sweep without a guard
    fails here, not at the time bound."""
    tally = dict.fromkeys((*VFS_EXACT_LINES, *VFS_HOLDABLE_LINES), 0)
    unguarded = 0
    problems = []
    guards = 0
    held_any = False
    guard = None  # inside a vfs guard: its printed families and summaries' total
    for m in klog.LOG_LINE.finditer(out):
        if (start := klog.HOLDING.fullmatch(m[0])) and start["tag"] == "vfs":
            assert guard is None, "a vfs guard started inside another"
            guard = {"printed": Counter(), "held": 0}
            continue
        if (end := klog.HELD_ALL.fullmatch(m[0])) and end["tag"] == "vfs":
            assert guard is not None, "a vfs guard's last line without its first"
            if int(end["n"]) != guard["held"]:
                problems.append(("a guard's total isn't its summaries' sum", end[0], guard["held"]))
            over = [f for f, n in guard["printed"].items() if n > 1]
            if over:
                problems.append(("a sweep family printed more than once in a guard", over))
            guards += 1
            guard = None
            continue
        summary = klog.HELD_BACK.fullmatch(m[0])
        line, weight = (
            (summary["last"], int(summary["n"])) if summary else (klog.without_stamp(m[0])[:-1], 1)
        )
        if not re.fullmatch(r"(?:WARN |ERROR) vfs: .*", line):
            if summary and guard is not None:
                problems.append(("a summary inside a vfs guard that isn't a vfs line", line))
            continue
        family = vfs_family(line)
        if family is None:
            problems.append(("a vfs line fitting no family", line))
            continue
        tally[family] += weight
        if not summary and family not in VFS_HOLDABLE_LINES:
            continue
        if not summary and family in VFS_ERRORED_LINES and guard is None:
            unguarded += 1  # a single fault's volume, outside any sweep
            continue
        if guard is None:
            problems.append(("a sweep line or summary outside a vfs guard", line))
        elif not summary:
            guard["printed"][family] += 1
        elif family not in VFS_HOLDABLE_LINES:
            problems.append(("an exact family held back", line))
        elif family not in guard["printed"]:
            problems.append(("a summary of a family its guard didn't print", line))
        else:
            guard["held"] += weight
            held_any = True
    assert guard is None, "a vfs guard never ended"
    assert not problems, problems
    counts = {want: tally[want] for want in VFS_EXACT_LINES}
    assert counts == VFS_EXACT_LINES, counts
    sweeps = {want: tally[want] for want in VFS_HOLDABLE_LINES}
    assert all(sweeps.values()), sweeps
    assert guards == VFS_HELD_KTESTS, (guards, VFS_HELD_KTESTS)
    assert unguarded == VFS_ERRORED_UNGUARDED, (unguarded, VFS_ERRORED_UNGUARDED)
    assert held_any, "no vfs lines held back: the guards hold nothing"


def check_held_back_lines(out):
    """`klog::repeats_held_back_from_serial_are_summarized_when_the_guard_drops`
    on serial: the guard's first line, each warning site's first line, the
    error and both info lines printed, another tag's warnings printed too;
    at the drop, a summary for each warning site with its newest line, and
    the guard's last line; then the warning after it, and a second guard
    holding nothing."""
    lines = [klog.without_stamp(line) for line in klog.LOG_LINE.findall(out)]
    mine = [line for line in lines if re.search(r"klogheld|klogother", line)]
    assert mine == [
        "INFO  klog: holding klogheld's repeated warnings and errors back from serial\n",
        "WARN  klogheld: repeated 0\n",
        "WARN  klogheld: another site's 0\n",
        "ERROR klogheld: once\n",
        "INFO  klogheld: info 0, never held\n",
        "INFO  klogheld: info 1, never held\n",
        "WARN  klogother: another tag's 0, never held\n",
        "WARN  klogother: another tag's 1, never held\n",
        "INFO  klog: 2 more held back, the last: WARN  klogheld: repeated 2\n",
        "INFO  klog: 1 more held back, the last: WARN  klogheld: another site's 1\n",
        "INFO  klog: no longer holding klogheld's repeats: 3 held back\n",
        "WARN  klogheld: after the guard\n",
        "INFO  klog: holding klogheld's repeated warnings and errors back from serial\n",
        "INFO  klog: no longer holding klogheld's repeats: 0 held back\n",
    ], mine


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
