"""`perf top`: the sampling profiler on the timer tick (#205).

The ranking, the header and the sample buffer are tested on the host
(`cargo test -p lumen-perf`) and in the kernel (`lumen_kernel::perf::*`);
this checks what a person sees. Judged by counts only (CLAUDE.md, "Tests
on a busy host"): a profile is a number of samples per CPU, not a time, so a
stalled host makes it slower, never different in what it counts.
"""

import re

from lumen_harness.gui import parse_records
from lumen_harness.screen import PROMPT
from lumen_harness.shell import run

BOOT_TIMEOUT = 60
SAMPLES = 500
BURN = "lumen_kernel::perf::burn_loop"
HEADER = re.compile(
    r"^perf top: (\d+) samples per CPU on (\d+) CPUs?: (\d+) samples, (\d+) idle, (\d+) busy$"
)
BURNING = re.compile(r"^perf burn: spinning on cpu (\d+) \(thread perf-burn, id (\d+)\)$")


def perf_top(m, args: str) -> tuple[dict[str, int], list[dict[str, str]]]:
    """Run `perf top <args>`: its header's numbers and its rows."""
    # A profile waits for its samples; 500 per CPU are 0.5 s of guest
    # time, more on a slow host.
    out = run(m, f"perf top {args}", timeout=120)
    header, _, table = out.strip().partition("\n")
    found = HEADER.match(header)
    assert found, out
    names = ("want", "cpus", "total", "idle", "busy")
    return dict(zip(names, map(int, found.groups()), strict=True)), parse_records(table)


def test_hot_function_is_on_top(boot):
    m = boot("qemutest", extra=["-smp", "2"])
    m.expect(PROMPT, BOOT_TIMEOUT)
    # Spinning before the profile starts: `perf burn` returns only once its
    # thread is in the loop.
    started = BURNING.match(run(m, "perf burn").strip())
    assert started, "perf burn didn't say where it spins"
    cpu, burn_id = started.groups()
    assert cpu == "1", "by default it spins on the last CPU"

    header, rows = perf_top(m, f"--samples {SAMPLES}")
    # The count asked for is the count taken, on every CPU (the header says
    # so only when each CPU's own count reached it).
    assert (header["want"], header["cpus"], header["total"]) == (SAMPLES, 2, 2 * SAMPLES)
    # CPU 1 runs nothing but the burn: all its samples are busy.
    assert header["busy"] >= SAMPLES, header
    # The busy loop has the most samples (not some share of them).
    assert rows[0]["function"] == BURN, rows
    assert all(int(r["samples"]) <= int(rows[0]["samples"]) for r in rows)

    # By thread: the burn thread first, by its name and id; the idle
    # threads are in the header, never a row.
    header, rows = perf_top(m, f"--samples {SAMPLES} --threads")
    assert header["want"] == SAMPLES
    assert (rows[0]["thread"], rows[0]["id"]) == ("perf-burn", burn_id), rows
    assert all(r["thread"] != "idle" for r in rows), rows

    # The row count asked for.
    for args in ("1 --samples 100", "1 --samples 100 --threads"):
        _, rows = perf_top(m, args)
        assert len(rows) == 1, (args, rows)

    assert run(m, "perf burn stop").strip() == "perf burn: stopped on cpu 1"
    # Stopped: a profile of the idle machine doesn't name it, and has idle
    # samples.
    header, rows = perf_top(m, "--samples 100")
    assert header["idle"] > 0, header
    assert all(r["function"] != BURN for r in rows), rows


def test_perf_usage_and_refusals(boot):
    m = boot("qemutest")
    m.expect(PROMPT, BOOT_TIMEOUT)
    assert "--samples takes 1 to 2048" in run(m, "perf top --samples 4096")
    assert "perf top: unexpected argument" in run(m, "perf top --bogus")
    assert "usage: perf top" in run(m, "perf")
    assert run(m, "perf top | count").strip() == "perf top: not in a pipeline"
    assert run(m, "perf burn stop").strip() == "perf burn: none running"
    assert run(m, "perf burn 7").strip() == "perf burn: no scheduler runs on cpu 7"
    assert run(m, "perf burn lots").strip() == "usage: perf burn [cpu|stop]"
    started = BURNING.match(run(m, "perf burn 0").strip())
    assert started and started.group(1) == "0"
    assert run(m, "perf burn").strip() == "perf burn: already spinning on cpu 0"
    assert run(m, "perf burn stop").strip() == "perf burn: stopped on cpu 0"
