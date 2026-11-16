"""lumen_harness.shell: a serial command's output, the log's lines taken
out wherever they land (#124)."""

import re

import pytest
from lumen_harness.shell import LOG_LINE, echo_pattern, run, split_logs

WARN = (
    "[    0.589291] WARN  mouse: 0000:00:03.0: 30 events in one batch: the event ring was nearly "
    "full, so frames may have been lost (said once; `mouse` counts such batches)"
)
HEADER = (
    'mouse: "QEMU Virtio Tablet" on 0000:00:03.0, screen 1280x800, 870 events in 290 frames '
    "(0 ignored, 0 dropped, at least 0 overruns)"
)
COLUMNS = "  device           x     y  buttons  presses  wheel  hwheel  pointer  stale  failed"
ROW = "  0000:00:03.0   101   389  none           0      0       0  drawn    no     -"


class FakeMachine:
    """Serial output given up front; `expect` searches it from the previous
    match, like `Machine.expect` (without waiting)."""

    def __init__(self, output: str) -> None:
        self.output = output
        self.sent: list[str] = []
        self._pos = 0

    def send_line(self, line: str) -> None:
        self.sent.append(line)

    def expect(self, pattern: str, timeout: float = 30) -> re.Match[str]:
        found = re.compile(pattern).search(self.output, self._pos)
        assert found, f"{pattern!r} not in {self.output[self._pos :]!r}"
        self._pos = found.end()
        return found


def test_the_warn_before_the_prompt_is_not_output():
    # #124's failure, as serial printed it (TCG run 26): the mouse thread's
    # warning came after `mouse`'s three lines, before the prompt.
    m = FakeMachine(f"lumen> mouse\n{HEADER}\n{COLUMNS}\n{ROW}\n{WARN}\nlumen> ")
    assert run(m, "mouse") == f"{HEADER}\n{COLUMNS}\n{ROW}\n"
    assert m.sent == ["mouse"]


def test_a_log_line_inside_the_echo_or_between_writes():
    # The editor echoes each key in its own write, and a command may write
    # part of a line: a log line can follow either.
    info = "[   12.000001] INFO  drivers: probing virtio-tablet on 0000:00:03.0\n"
    m = FakeMachine(f"lumen> mo{info}use\nmouse: {info}one\ntwo\n{info}lumen> ")
    assert run(m, "mouse") == "mouse: one\ntwo\n"


def test_keep_logs_leaves_the_output_as_printed():
    # dmesg's output is log lines.
    line = "[    ?.??????] INFO  boot: Lumen 0.0.1 (x86_64) booting"
    m = FakeMachine(f"lumen> dmesg\ndmesg: the newest 1 log lines\n{line}\nlumen> ")
    assert run(m, "dmesg", keep_logs=True) == f"dmesg: the newest 1 log lines\n{line}\n"


def test_split_logs_returns_the_lines_in_order():
    out, logs = split_logs(f"a\n{WARN}\nb\n[    ?.??????] ERROR mm::frame: x\n")
    assert (out, logs) == ("a\nb\n", [WARN, "[    ?.??????] ERROR mm::frame: x"])


@pytest.mark.parametrize(
    "text",
    [
        "[    0.589291] WARN  mouse: no newline",  # not a whole line yet
        "[0.589291] WARN  mouse: x\n",  # the stamp is padded to 5 digits
        "[    0.58929] WARN  mouse: x\n",  # microseconds: 6 digits
        "[    0.589291] WARN mouse: x\n",  # the level is padded to 5
        "[    0.589291] NOTE  mouse: x\n",  # not a level
        "[    0.589291] WARN  mouse x\n",  # no tag
        "  addr   vendor  device\n",
    ],
)
def test_text_that_is_not_a_log_line_stays(text):
    assert split_logs(text) == (text, [])


def test_long_stamps_and_odd_tags_are_log_lines():
    for line in [
        "[123456.000000] TRACE x: \n",
        "[    1.000000] DEBUG shell-vt2: y\n",
        "[    1.000000] INFO  klogtest: kept for dmesg <formatting failed>\n",
    ]:
        assert LOG_LINE.fullmatch(line), line


def test_echo_pattern_matches_only_the_line():
    assert re.fullmatch(echo_pattern("a|b (c)"), "a|b (c)\n")
    assert not re.fullmatch(echo_pattern("ab"), "a b\n")
    assert re.fullmatch(echo_pattern("ab"), f"a{WARN}\n{WARN}\nb\n")
