"""Run the in-kernel test suite (`--features ktest`) and check its TAP output."""

import re

from lumen_harness import save_ktest_tap, tap
from lumen_harness.qemu import EXIT_CRASH, EXIT_SUCCESS, Devices


def test_in_kernel_tests_pass(boot):
    # With other CPUs online (idle), so the per-CPU state, the heap, the
    # stack registry and the frame allocator are tested with their stacks and
    # state in place, as on any multi-CPU machine. With virtio devices no
    # driver takes in M3 (network, block, 9p, a memory balloon, a console,
    # SCSI), for the virtio adapter's ktests to claim and probe.
    extra = ["-smp", "4"]
    for device in ("virtio-balloon-pci", "virtio-serial-pci", "virtio-scsi-pci"):
        extra += ["-device", device]
    m = boot("ktest", extra=extra, devices=Devices(blk_mib=1, net=True, share=True))
    code = m.wait_exit(120)
    out = m.output
    report = tap.parse(out)
    # Saved for `make status` (each ktest is listed individually in STATUS.md);
    # a re-run's (LUMEN_ARTIFACTS_DIR) is kept apart: see save_ktest_tap.
    save_ktest_tap(out[out.find("TAP version") :] if "TAP version" in out else "")
    if code == EXIT_CRASH:
        crash = out[out.find("==== LUMEN CRASH REPORT") :]
        last = report.cases[-1].name if report.cases else "(before the first test)"
        raise AssertionError(f"kernel crashed during the test after {last}:\n{crash}")
    assert report.complete, f"incomplete TAP stream: {report.summary()}\n{out[-2000:]}"
    assert report.all_passed, report.summary()
    assert code == EXIT_SUCCESS, f"QEMU exit {code}, expected {EXIT_SUCCESS}"
    # The other CPUs really were online while the tests ran.
    assert "INFO  smp: 4 CPUs online\n" in out, "the APs didn't come online"
    assert len(report.cases) >= 7
    # `device_vectors_are_not_fatal` raises the unused vector twice: one
    # warning, not one per interrupt.
    assert out.count("unexpected interrupt on vector 0x30") == 1, "warn-once"
    # It raises the first MSI vector twice and the last once, and
    # `msi_vector_delivered_by_the_apic_is_eoid` sends 0x91 as a self-IPI:
    # each is reported once, as an error (#91: no device may send an MSI
    # until the vector allocator exists, #96).
    for vector in ("0x90", "0xef", "0x91"):
        line = f"ERROR irq: interrupt on MSI vector {vector}, which no device has claimed"
        assert out.count(line) == 1, f"MSI vector {vector} not reported exactly once as an error"
    # The last I/O APIC vector (no handler) is still a plain warning.
    assert out.count("WARN  irq: unexpected interrupt on vector 0x8f") == 1, "0x8f not a warning"
    # `an_unexpected_storm_is_reported_sparsely` raises 0x31 2048 times:
    # reported at the 1st, 1024th and 2048th only.
    storm = re.findall(r"unexpected interrupt on vector 0x31 \(rip 0x[0-9a-f]+, #(\d+)\)", out)
    assert storm == ["1", "1024", "2048"], f"0x31 storm reports: {storm}"
    # `probing_records_each_outcome` (#95): boot's own pass binds nothing
    # (the ktest machine has no device sound (#99) takes); the ktest's table declines the 9p device once alone, then
    # declines it (leaving a run) and fails it (leaving one of its own),
    # finds the balloon not ready, binds the console, and refuses the SCSI
    # device's decline (its probe kept the resources); offered again, the
    # 9p device isn't probed.
    assert "INFO  drivers: 0 bound, 0 failed, 0 declined, 0 not probed (1 driver row)\n" in out
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
    # Issue #3: the trace cost per event is printed as a TAP diagnostic.
    assert re.search(r"^# trace: \d+ TSC ticks per event", out, re.MULTILINE), (
        "no trace cost diagnostic"
    )
