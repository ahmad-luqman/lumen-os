"""Run the in-kernel test suite (`--features ktest`) and check its TAP output."""

import re

from lumen_harness import BUILD_DIR, tap
from lumen_harness.qemu import EXIT_CRASH, EXIT_SUCCESS

TAP_OUT = BUILD_DIR / "test-results" / "ktest.tap"


def test_in_kernel_tests_pass(boot):
    # With other CPUs online (idle), so the per-CPU state, the heap, the
    # stack registry and the frame allocator are tested with their stacks and
    # state in place, as on any multi-CPU machine.
    m = boot("ktest", extra=["-smp", "4"])
    code = m.wait_exit(120)
    out = m.output
    report = tap.parse(out)
    # Saved for `make status` (each ktest is listed individually in STATUS.md).
    TAP_OUT.parent.mkdir(parents=True, exist_ok=True)
    TAP_OUT.write_text(out[out.find("TAP version") :] if "TAP version" in out else "")
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
    # Issue #3: the trace cost per event is printed as a TAP diagnostic.
    assert re.search(r"^# trace: \d+ TSC ticks per event", out, re.MULTILINE), (
        "no trace cost diagnostic"
    )
