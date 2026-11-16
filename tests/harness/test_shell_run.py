"""lumen_harness.shell: a serial command's output, the log's lines taken
out wherever they land, warnings failing the run unless expected (#124);
and lumen_harness.klog's line format."""

import re

import pytest
from lumen_harness.klog import LOG_LINE, is_log_line, level_of
from lumen_harness.qemu import ExpectTimeout
from lumen_harness.shell import (
    Output,
    allowed,
    echo_pattern,
    read_output,
    run,
    run_dmesg,
    split_logs,
)

WARN = (
    "[    0.589291] WARN  mouse: 0000:00:03.0: 30 events in one batch: the event ring was nearly "
    "full, so frames may have been lost (said once; `mouse` counts such batches)"
)
OVERRUN = r"WARN  mouse: \S+: \d+ events in one batch"
# #124's failure, as serial printed it (TCG run 26): the mouse thread's
# warning came after `mouse`'s three lines, before the prompt.
RUN_26_LINES = [
    (
        'mouse: "QEMU Virtio Tablet" on 0000:00:03.0, screen 1280x800, 870 events in 290 frames '
        "(0 ignored, 0 dropped, at least 0 overruns)"
    ),
    "  device           x     y  buttons  presses  wheel  hwheel  pointer  stale  failed",
    "  0000:00:03.0   101   389  none           0      0       0  drawn    no     -",
]
RUN_26 = "lumen> mouse\n" + "".join(f"{line}\n" for line in RUN_26_LINES) + f"{WARN}\nlumen> "
INFO = "[   12.000001] INFO  drivers: probing virtio-tablet on 0000:00:03.0"
DEBUG = '[   12.000002] DEBUG term: view 20 lines up, top row "81"'
ERROR = "[    1.000000] ERROR mm: something else"


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
    out = run(FakeMachine(RUN_26), "mouse", allow=OVERRUN)
    assert out == "".join(f"{line}\n" for line in RUN_26_LINES)
    assert isinstance(out, Output) and out.logs == [WARN]


def test_allow_lets_only_its_lines_through():
    m = FakeMachine(f"lumen> x\n{WARN}\n{ERROR}\nok\nlumen> ")
    with pytest.raises(AssertionError) as e:
        run(m, "x", allow=OVERRUN)
    loud = str(e.value).split("\n--- output ---")[0].splitlines()[1:]
    assert loud == [ERROR]


def test_allow_is_matched_from_the_level_on():
    assert allowed(WARN, OVERRUN)
    assert not allowed(WARN, r"mouse: \S+: \d+ events"), "the level comes first"
    assert not allowed(WARN, r"ERROR mouse"), "nor another level"
    assert not allowed(WARN, None)


def test_a_warning_inside_the_echo_fails_the_run():
    m = FakeMachine(f"lumen> mo{WARN}\nuse\nout\nlumen> ")
    with pytest.raises(AssertionError, match="WARN  mouse"):
        run(m, "mouse")


def test_a_warning_followed_by_a_quiet_line_fails_the_run():
    m = FakeMachine(f"lumen> mouse\nout\n{WARN}\n{INFO}\nlumen> ")
    with pytest.raises(AssertionError, match="WARN  mouse"):
        run(m, "mouse")


def test_lines_between_the_prompt_and_the_echo_count_and_come_first():
    # A WARN there fails the run (seen in #124's runs: right after the
    # prompt, before the next `mouse`); an INFO there is kept, first.
    with pytest.raises(AssertionError, match="WARN  mouse"):
        run(FakeMachine(f"lumen> {WARN}\nmouse\nout\nlumen> "), "mouse")
    out = run(FakeMachine(f"lumen> {INFO}\nmouse\nout\n{DEBUG}\nlumen> "), "mouse")
    assert (out, out.logs) == ("out\n", [INFO, DEBUG])


def test_only_lines_since_the_last_prompt_count():
    # Boot's warning, before an earlier prompt, isn't this command's.
    m = FakeMachine(f"{WARN}\nlumen> echo a\na\nlumen> mouse\nout\nlumen> ")
    run(m, "echo a")
    out = run(m, "mouse")
    assert (out, out.logs) == ("out\n", [])


def test_quiet_lines_inside_the_echo_and_between_writes_are_taken_out():
    # The editor echoes each key in its own write (Enter's newline too),
    # and a command may write part of a line: a log line can follow any of
    # them.
    m = FakeMachine(f"lumen> mo{INFO}\nuse{DEBUG}\n\nmouse: {INFO}\none\ntwo\n{DEBUG}\nlumen> ")
    out = run(m, "mouse")
    assert out == "mouse: one\ntwo\n"
    assert out.logs == [INFO, DEBUG, INFO, DEBUG]


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
    assert out.logs is None
    with pytest.raises(ValueError, match="nothing to allow"):
        run(FakeMachine(""), "dmesg", keep_logs=True, allow=OVERRUN)


def test_run_dmesg_drops_live_lines_and_fails_on_a_live_warning():
    # INFO was printed before the command (dmesg copies it); DEBUG and the
    # WARN were logged while it ran.
    head = "dmesg: the newest 2 log lines"
    printed = f"{INFO}\nlumen> dmesg\n{head}\n{DEBUG}\n{INFO}\n"
    assert run_dmesg(FakeMachine(printed + "lumen> ")) == [head, INFO]
    with pytest.raises(AssertionError, match="WARN  mouse"):
        run_dmesg(FakeMachine(printed + f"{WARN}\nlumen> "))
    m = FakeMachine(printed + f"{WARN}\nlumen> ")
    assert run_dmesg(m, allow=OVERRUN) == [head, INFO]


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
        ("[    1.000000] INFO  klogtest: kept for dmesg <formatting failed>\n", "INFO"),
        ("[    ?.??????] WARN  mem: no ACPI RSDP from Limine\n", "WARN"),
        ("[   10.123456] ERROR mm::frame: z\n", "ERROR"),
    ],
)
def test_log_lines_and_their_levels(line, level):
    assert LOG_LINE.fullmatch(line), line
    assert is_log_line(line) and is_log_line(line[:-1])
    assert level_of(line) == level
