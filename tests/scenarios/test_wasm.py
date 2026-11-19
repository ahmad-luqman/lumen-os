"""wasmi in the booted kernel (#197): the shell's `wasm` runs the
hand-assembled modules in `kernel/src/app/modules.rs` on the shell's own
thread, so wasmi is linked into the kernel that boots (and `make audit`
checks it there), not only into the ktest one. The ktests
`app::wasmi_runs_a_long_loop_in_fuel_slices` and `app::wasm_floats_match`
share its checks: the first runs the loop's on a 64 KiB thread (with
`wasm`'s slices and with slices small enough that the deep call traps
after resumes); the second prints each float that differs."""

import re

from lumen_harness.screen import PROMPT
from lumen_harness.shell import run

# `run(300000)`: acc = acc * 31 + i for i in 0..300000, wrapping (as
# kernel/src/app.rs's loop_expected, and the host's wasmi, computes it).
LOOP_RESULT = "0xdc2069f0"


def test_wasm_command_runs_a_loop_in_fuel_slices(boot):
    m = boot("qemutest")
    m.expect(PROMPT, 60)
    out = run(m, "wasm")
    lines = out.splitlines()
    assert len(lines) == 4, out
    # Out of fuel and resumed at least twice, and the right sum. `wasm`
    # itself checks the resumes against the band the loop's fuel gives
    # (fuel is counted in instructions, so no matter of time): outside it,
    # a failure line would make a fifth line.
    loop = re.fullmatch(
        rf"wasm: run\(300000\) = {LOOP_RESULT} after (\d+) resumes \(100000 fuel a slice\)",
        lines[0],
    )
    assert loop, out
    assert int(loop[1]) >= 2, out
    assert lines[1] == "wasm: depth(900) = 900", out
    # Past wasmi's call depth: that trap (`TrapCode::StackOverflow`, and
    # no other), and the kernel and the shell carry on.
    assert lines[2] == "wasm: depth(5000) trapped: call stack exhausted", out
    assert lines[3] == "wasm: floats: 29 of 29 match", out
    assert run(m, "echo still here") == "still here\n"
