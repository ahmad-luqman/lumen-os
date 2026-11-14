"""TEMPORARY (#61, reverted before merge): boot an ISO N times and print
its boot timeline (`timeline ISO N`) or the serial bench line (`bench ISO N`)."""

import re
import sys
import tempfile
from pathlib import Path

from lumen_harness.qemu import Machine

kind, iso, runs = sys.argv[1], Path(sys.argv[2]), int(sys.argv[3])
rx = r"timeline: (.*\(total [^)]*\))" if kind == "timeline" else r"BENCH (\d+) bytes in (\d+) us"
values = []
for _ in range(runs):
    with tempfile.TemporaryDirectory() as d:
        m = Machine(iso, Path(d)).start()
        try:
            found = m.expect(rx, 120)
        finally:
            m.close()
    print(f"{iso.name}: {found.group(0)}", flush=True)
    if kind == "timeline":
        t = re.search(r"total \+?([\d.]+) ?ms", found.group(1))
        values.append(float(t.group(1)) if t else float("nan"))
    else:
        values.append(float(found.group(2)))
values.sort()
unit = "ms total" if kind == "timeline" else "us for the bench"
print(
    f"{iso}: {kind} runs={len(values)} median={values[len(values) // 2]} min={values[0]} max={values[-1]} ({unit})"
)
