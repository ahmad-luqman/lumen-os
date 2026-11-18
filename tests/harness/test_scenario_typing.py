"""Every key a scenario types is a confirmed step (#128), or says why not.

Keys typed with `Machine.press`, `Machine.type_text` or QMP's `send_key`
go to QEMU and return; a key lost under load then shows only as a wrong
screen, and keys sent back to back while the guest is stalled can overflow
QEMU's queues. `lumen_harness.gui.confirmed_keys` sends the next key only
once the guest counted the last (and fails naming a lost one). So a raw
press in tests/scenarios needs an `# unconfirmed: <why>` comment in its
paragraph (the lines above it up to a blank line): a race on purpose, no
serial shell free to run `kbd`, or an effect read next that a confirmed
step's `kbd` would read past.
"""

from __future__ import annotations

import re
from pathlib import Path

SCENARIOS = Path(__file__).resolve().parents[1] / "scenarios"
RAW = re.compile(r"\.(?:press|type_text|send_key)\(")
MARK = "# unconfirmed: "


def unmarked(source: str) -> list[int]:
    """The 1-based lines of `source` with a raw key call and no
    `# unconfirmed: ` comment in their paragraph above (definitions of
    such calls aside)."""
    lines = source.splitlines()
    found = []
    for i, line in enumerate(lines):
        code = line.split("#", 1)[0]
        if not RAW.search(code) or code.lstrip().startswith("def "):
            continue
        j = i
        while j >= 0 and lines[j].strip() and MARK not in lines[j]:
            j -= 1
        if j < 0 or MARK not in lines[j]:
            found.append(i + 1)
    return found


def test_every_raw_key_in_the_scenarios_says_why():
    bad = [
        f"{p.name}:{n}: {p.read_text().splitlines()[n - 1].strip()}"
        for p in sorted(SCENARIOS.glob("*.py"))
        for n in unmarked(p.read_text())
    ]
    assert not bad, "raw key presses without `# unconfirmed: <why>` (use gui.confirmed_keys):\n" + (
        "\n".join(bad)
    )


def test_a_raw_key_needs_the_mark_in_its_paragraph():
    assert unmarked('    m.press(["x"])\n') == [1]
    assert unmarked('    # unconfirmed: a race\n    m.press(["x"])\n    m.type_text("y")\n') == []
    assert unmarked('    # unconfirmed: a race\n\n    m.press(["x"])\n') == [3], (
        "a blank line ends it"
    )
    assert unmarked("    gui.confirmed_keys(m, 'x')\n    m.send_line('kbd')\n") == []
    assert unmarked("def press(self, qcodes):\n") == []
    assert unmarked("    x = 1  # m.press(['x']) in a comment\n") == []
