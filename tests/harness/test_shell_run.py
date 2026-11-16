"""lumen_harness.shell: a serial command's output, the log's lines taken
out wherever they land, warnings failing the run unless expected (#124);
and lumen_harness.klog's line format."""

import re

import pytest
from lumen_harness.klog import LOG_LINE, level_of
from lumen_harness.qemu import ExpectTimeout
from lumen_harness.shell import Output, echo_pattern, read_output, run, split_logs

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
# #124's failure, as serial printed it (TCG run 26): the mouse thread's
# warning came after `mouse`'s three lines, before the prompt.
RUN_26 = f"lumen> mouse\n{HEADER}\n{COLUMNS}\n{ROW}\n{WARN}\nlumen> "
INFO = "[   12.000001] INFO  drivers: probing virtio-tablet on 0000:00:03.0"
DEBUG = '[   12.000002] DEBUG term: view 20 lines up, top row "81"'


class FakeMachine:
    """Serial output given up front; `expect` searches it from the previous
    match, like `Machine.expect` (without waiting: no match times out)."""

    def __init__(self, output: str) -> None:
        self.output = output
        self.sent: list[str] = []
        self._pos = 0

    def send_line(self, line: str) -> None:
        self.sent.append(line)

    def expect(self, pattern: str, timeout: float = 30) -> re.Match[str]:
        found = re.compile(pattern).search(self.output, self._pos)
        if not found:
            raise ExpectTimeout(f"timeout waiting for {pattern!r}")
        self._pos = found.end()
        return found


def test_a_warning_fails_the_run_unless_allowed():
    with pytest.raises(AssertionError, match=r"'mouse': a warning or error came with its output"):
        run(FakeMachine(RUN_26), "mouse")
    out = run(FakeMachine(RUN_26), "mouse", allow=r"WARN  mouse: \S+: \d+ events in one batch")
    assert out == f"{HEADER}\n{COLUMNS}\n{ROW}\n"
    assert isinstance(out, Output) and out.logs == [WARN]


def test_allow_lets_only_its_lines_through():
    error = "[    1.000000] ERROR mm: something else"
    m = FakeMachine(f"lumen> x\n{WARN}\n{error}\nok\nlumen> ")
    with pytest.raises(AssertionError, match="ERROR mm: something else") as e:
        run(m, "x", allow="WARN  mouse")
    assert "WARN  mouse" not in str(e.value).split("---")[0].splitlines()[1]


def test_quiet_lines_inside_the_echo_and_between_writes_are_taken_out():
    # The editor echoes each key in its own write (Enter's newline too),
    # and a command may write part of a line: a log line can follow any of
    # them.
    m = FakeMachine(f"lumen> mo{INFO}\nuse{DEBUG}\n\nmouse: {INFO}\none\ntwo\n{DEBUG}\nlumen> ")
    out = run(m, "mouse")
    assert out == "mouse: one\ntwo\n"
    assert out.logs == [INFO, DEBUG, INFO, DEBUG]


def test_a_log_line_before_the_echo_is_not_part_of_it():
    m = FakeMachine(f"lumen> {INFO}\nmouse\nout\nlumen> ")
    assert run(m, "mouse") == "out\n"


def test_the_echo_starts_after_the_prompt_or_a_newline():
    # A log line whose message ends with the command's text isn't its echo.
    m = FakeMachine("lumen> [    1.000000] INFO  shell: typed mouse\nmouse\nout\nlumen> ")
    assert run(m, "mouse") == "out\n"


def test_only_log_lines_may_sit_inside_the_echo():
    m = FakeMachine("lumen> mo\nnot a log line\nuse\nout\nlumen> ")
    with pytest.raises(ExpectTimeout, match=r"no echo of 'mouse'") as e:
        run(m, "mouse")
    assert "not a log line" in str(e.value)
    assert isinstance(e.value.__cause__, ExpectTimeout)


def echoed(line: str, text: str) -> bool:
    """Whether `text`, right after the prompt, is `line`'s whole echo."""
    found = re.compile(echo_pattern(line)).match("lumen> " + text, len("lumen> "))
    return bool(found) and found.end() == len("lumen> " + text)


def test_echo_pattern():
    assert echoed("a|b (c)", "a|b (c)\n")
    assert not echoed("ab", "a b\n")
    assert echoed("ab", f"a{WARN}\n{WARN}\nb{INFO}\n\n")
    assert not echoed("ab", "a\nx\nb\n")
    assert not re.search(echo_pattern("ab"), "xab\n"), "not after the prompt or a newline"


def test_keep_logs_leaves_the_output_as_printed():
    # dmesg's output is log lines (a warning among them doesn't fail it).
    m = FakeMachine(f"lumen> dmesg\ndmesg: the newest 2 log lines\n{INFO}\n{WARN}\nlumen> ")
    out = run(m, "dmesg", keep_logs=True)
    assert out == f"dmesg: the newest 2 log lines\n{INFO}\n{WARN}\n"
    assert out.logs == []


def test_split_logs_returns_the_lines_in_order():
    out, logs = split_logs(f"a\n{WARN}\nb\n[    ?.??????] ERROR mm::frame: x\n")
    assert (out, logs) == ("a\nb\n", [WARN, "[    ?.??????] ERROR mm::frame: x"])


def test_read_output_reads_a_line_sent_in_pieces():
    m = FakeMachine(f"lumen> echo hel{DEBUG}\nlo\nhello\nlumen> ")
    out = read_output(m, "echo hello")
    assert (out, out.logs, m.sent) == ("hello\n", [DEBUG], [])


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


@pytest.mark.parametrize(
    ("line", "level"),
    [
        ("[123456.000000] TRACE x: \n", "TRACE"),
        ("[    1.000000] DEBUG shell-vt2: y\n", "DEBUG"),
        ("[    1.000000] INFO  klogtest: kept for dmesg <formatting failed>\n", "INFO "),
        ("[    ?.??????] WARN  mem: no ACPI RSDP from Limine\n", "WARN "),
        ("[   10.123456] ERROR mm::frame: z\n", "ERROR"),
    ],
)
def test_log_lines_and_their_levels(line, level):
    assert LOG_LINE.fullmatch(line), line
    assert level_of(line) == level
