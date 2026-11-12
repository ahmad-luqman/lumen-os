"""Crash reports instead of reboots (signature feature 5).

Every deliberate fault must produce a complete, symbolized crash report on
serial and exit QEMU with the crash code. A triple fault would instead end
QEMU with status 0 (`-no-reboot`), which fails these tests.
"""

import re

import pytest
from lumen_harness.crash import backtrace, page_walk
from lumen_harness.image import assert_matches_golden

SCREEN_DONE = "(crash report shown on screen)"

# kind -> (exception line regex, function the rip must resolve to)
FAULTS = {
    "pf": (r"exception: #PF page fault \(vector 14, error code 0x2\)", "kmon::crash_page_fault"),
    "ud": (r"exception: #UD invalid opcode \(vector 6,", "kmon::crash_invalid_opcode"),
    "de": (r"exception: #DE divide error \(vector 0,", "kmon::crash_divide_error"),
    "gp": (
        r"exception: #GP general protection fault \(vector 13,",
        "kmon::crash_general_protection",
    ),
    "df": (r"exception: #DF double fault \(vector 8,", "kmon::crash_double_fault"),
}


@pytest.mark.parametrize("kind", sorted(FAULTS))
def test_fault_produces_symbolized_report(crash_report, kind):
    report = crash_report(kind)
    exc_rx, func = FAULTS[kind]
    assert re.search(exc_rx, report), report
    rip = re.search(r"^rip: 0x[0-9a-f]{16} <(.+)>$", report, re.MULTILINE)
    assert rip and func in rip.group(1), report
    for reg in ("rax", "rbx", "rsp", "rbp", "r15", "cr0", "cr3", "cr4"):
        assert re.search(rf"{reg}\s*: 0x[0-9a-f]{{16}}", report), f"missing {reg}\n{report}"
    frames = backtrace(report)
    assert func in frames[0], report
    # The chain is walked all the way back to the entry point.
    assert any("kmain" in f for f in frames), report
    assert frames[-1].startswith("_start"), report
    assert "<unknown>" not in report, report


def test_report_shows_the_trace_events_leading_up_to_it(crash_report):
    report = crash_report("pf")
    trace = report[report.index("recent trace events (oldest first):") :]
    lines = re.findall(r"^  #\d+ +tsc=\d+ +(\S+.*)$", trace, re.MULTILINE)
    assert len(lines) == 16, trace
    # The last thing recorded before the report is the fault itself.
    assert re.match(r"exception +vector=14 rip=0x[0-9a-f]+ error=0x2$", lines[-1]), trace
    assert re.search(r"^trace: \d+ events claimed, 0 dropped$", trace, re.MULTILINE), trace


def test_panic_report_trace_ends_with_the_panic(crash_report):
    report = crash_report("panic")
    trace = report[report.index("recent trace events (oldest first):") :]
    lines = re.findall(r"^  #\d+ +tsc=\d+ +(\S+.*)$", trace, re.MULTILINE)
    at = re.search(r"^  at kernel/src/kmon\.rs:(\d+):\d+$", report, re.MULTILINE)
    assert at, report
    assert re.match(rf"panic +line={at.group(1)}$", lines[-1]), trace


def test_page_fault_report_names_address_and_cause(crash_report):
    report = crash_report("pf")
    assert "faulting address (cr2): 0x00000000deadb000" in report
    assert "page fault: write of non-present page (kernel mode)" in report
    # The page-table walk of cr2, after the backtrace: the lower half is
    # empty, so it stops at the first level.
    walk = page_walk(report)
    assert walk.virt == 0xDEADB000, report
    assert walk.steps == [(4, 0, 0)] and walk.end == "not present at L4", report
    assert report.index("page walk of") > report.index("backtrace:"), report


def test_panic_produces_report(crash_report):
    report = crash_report("panic")
    assert "panic: deliberate panic from kmon" in report
    assert re.search(r"at kernel/src/kmon\.rs:\d+:\d+", report)
    frames = backtrace(report)
    assert any("kmon::crash_panic" in f for f in frames), report
    assert frames[-1].startswith("_start"), report


def test_crash_screen_golden(boot, artifacts, golden_dir):
    """The crash report is also shown on screen (banner checked against a golden)."""
    m = boot("interactive")  # no qemu_exit: the crash screen stays up
    m.expect(r"kmon> ", 60)
    m.send_line("crash ud")
    m.expect(re.escape(SCREEN_DONE), 30)  # drawing finished, not just serial
    shot = m.screendump("crash.png")
    assert_matches_golden(
        shot, golden_dir / "crash-banner.png", origin=(0, 0), size=(640, 72), artifacts=artifacts
    )
