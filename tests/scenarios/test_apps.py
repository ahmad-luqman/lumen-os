"""The WASM app runtime (#199; ARCHITECTURE section 8): `run`, `apps`,
`kill`, the host functions, how an app ends, and apps' windows (#206: the
template, the window calls' checks, the close grace, a kill waking a
`window_wait`).

The test apps are embedded in the kernel as `TEST_APPS` (ARCHITECTURE
section 8, "What #199 adds"). Judged by confirmed effects and counts only
(CLAUDE.md, "Tests on a busy host"): an app's log lines and end line,
`apps`' records polled until they say what is expected, a thread's id gone
from `threads`, never a time. A kill may answer "still ending (<place>)" on
a host that stalls the app's vCPU past `KILL_WAIT_NS`, so the kill tests
accept "killed" or any place (`kill`) and confirm the end by the app's end
line and its thread's join.
"""

import re

from lumen_harness import REPO_ROOT, gui, klog
from lumen_harness.gui import parse_records, poll
from lumen_harness.klog import STAMP
from lumen_harness.screen import PROMPT
from lumen_harness.shell import heap_line, run, wait_for_output

BOOT_TIMEOUT = 60
STARTED = re.compile(r"^run: started (\S+) as app (\d+)$")


def boot_shell(boot, extra=None):
    m = boot("qemutest", extra=extra or [])
    m.expect(PROMPT, BOOT_TIMEOUT)
    return m


def start(m, line: str, allow: str | None = None) -> tuple[int, int]:
    """`run <line>`: the app's id, and where serial was before it (for
    waiting on its lines)."""
    mark = len(m.output)
    out = run(m, f"run {line}", allow=allow).strip()
    found = STARTED.match(out)
    assert found, out
    return int(found[2]), mark


def apps(m, allow: str | None = None) -> list[dict[str, str]]:
    return parse_records(run(m, "apps", allow=allow), "id")


def app(m, app_id: int, allow: str | None = None) -> dict[str, str] | None:
    return next((a for a in apps(m, allow) if a["id"] == str(app_id)), None)


def thread_ids(m, allow: str | None = None) -> set[str]:
    return {
        t["id"] for t in parse_records(run(m, "threads", allow=allow).split("\n", 1)[1], "thread")
    }


def wait_unlisted(m, app_id: int, allow: str | None = None) -> None:
    """Until `apps` no longer lists the app (the reaper joined it)."""
    _, done = poll(m, lambda: app(m, app_id, allow), lambda a: a is None, 30)
    assert done, f"app {app_id} still listed"


def wait_gone(m, app_id: int, thread: str, allow: str | None = None) -> None:
    """Until `apps` no longer lists the app and its thread is gone from
    `threads` (joined by the reaper)."""
    wait_unlisted(m, app_id, allow)
    _, done = poll(m, lambda: thread_ids(m, allow), lambda ids: thread not in ids, 30)
    assert done, f"app {app_id}'s thread {thread} not joined"


# Every place `kill` can say an app still ending is (runtime.rs,
# `Phase::place`).
KILL_PLACES = ("loading", "in a slice", "in time_sleep", "in window_wait", "ending")


def kill(m, app_id: int, name: str, allow: str | None = None) -> None:
    """`kill <id>`: "killed", or "still ending (<place>)" on a host that
    stalls the app past `KILL_WAIT_NS` (the caller confirms the end)."""
    out = run(m, f"kill {app_id}", allow=allow).strip()
    places = "|".join(re.escape(p) for p in KILL_PLACES)
    assert re.fullmatch(
        rf"kill: app {app_id} \({re.escape(name)}\) (killed|still ending \(({places})\))", out
    ), out


def end_line(m, mark: int, name: str, app_id: int, what: str, level: str = "INFO") -> None:
    """Until the app's end line: a whole log line, from its stamp (at a
    line's start, or right after the prompt it interrupted) to its end, so
    no app's own log text can stand in for it."""
    start = rf"(?:^|{re.escape(PROMPT)})"
    wait_for_output(
        m,
        mark,
        rf"(?m){start}{STAMP} {level}  app: {re.escape(name)} \(app {app_id}\) {what}$",
        30,
    )


def test_hello_wasm(boot):
    m = boot_shell(boot)
    app_id, mark = start(m, "hello")
    assert app_id == 1
    wait_for_output(m, mark, r"INFO  app: hello: hello from wasm\n", 30)
    end_line(m, mark, "hello", app_id, "exited with 0")
    wait_unlisted(m, app_id)
    # `run` alone: its usage and the bundled apps (not the test apps).
    out = run(m, "run")
    assert out == "usage: run [--without <grant>] <app> [args...]\napps: template\n", out
    assert "run: no app nope" in run(m, "run nope")
    # `exit(n)` is an ending like returning n.
    app_id, mark = start(m, "hello exit 7")
    end_line(m, mark, "hello", app_id, "exited with 7")


def test_busy_app_is_preempted(boot):
    m = boot_shell(boot)
    busy, _ = start(m, "busy")
    first, done = poll(
        m, lambda: app(m, busy), lambda a: a is not None and int(a["slices"]) > 0, 30
    )
    assert done, first
    # The shell answers while it runs, and another app runs to its end.
    assert run(m, "echo still here") == "still here\n"
    hello, mark = start(m, "hello")
    end_line(m, mark, "hello", hello, "exited with 0")
    # Its slices grow (a count: the slice ends it reached).
    later, done = poll(
        m, lambda: app(m, busy), lambda a: int(a["slices"]) > int(first["slices"]), 30
    )
    assert done, (first, later)
    assert later["state"] in ("running", "waiting"), later
    assert int(later["fuel"]) > 0
    mark = len(m.output)
    kill(m, busy, "busy")
    end_line(m, mark, "busy", busy, "killed")
    wait_gone(m, busy, later["thread"])


def test_run_refuses_past_max_apps(boot):
    m = boot_shell(boot)
    ids = [start(m, "busy")[0] for _ in range(4)]
    assert ids == [1, 2, 3, 4]
    assert run(m, "run hello").strip() == "run: hello: 4 apps running already"
    record = app(m, ids[0])
    kill(m, ids[0], "busy")
    wait_gone(m, ids[0], record["thread"])
    # A slot again, and a new id (never a used one).
    again, mark = start(m, "hello")
    assert again == 5
    end_line(m, mark, "hello", again, "exited with 0")
    for app_id in ids[1:]:
        kill(m, app_id, "busy")


TRAP_ENDS = r"WARN  app: (trap|greedy) \(app \d+\) ended"
BAD_TEXT = "ended: panicked: bad\ufffd" + "y" * 250


def heap_used(m) -> int:
    return int(heap_line(run(m, "heap", allow=TRAP_ENDS))["used"])


def test_app_trap_kills_app_not_kernel(boot):
    m = boot_shell(boot)
    cases = [
        ("trap unreachable", "trap", "ended: trap: wasm `unreachable` instruction executed"),
        # The message's newline and BEL each shown as U+FFFD: no forged line.
        ("trap panic", "trap", "ended: panicked: boom�WARN  app: forged�"),
        ("greedy", "greedy", "ended: panicked: out of memory"),
        ("trap depth", "trap", "ended: trap: call stack exhausted"),
        # A message past the memory's end; one whose claimed 2000 bytes
        # would be, but whose first TEXT_BYTES fit: invalid UTF-8 as
        # U+FFFD, cut at LOG_BYTES (3 + 3 + 250 bytes).
        ("trap badptr", "trap", "ended: panicked: <a bad pointer>"),
        ("trap badtext", "trap", BAD_TEXT),
    ]

    def round_of_traps() -> None:
        for line, name, what in cases:
            app_id, mark = start(m, line, allow=TRAP_ENDS)
            # The whole line: nothing after what is expected.
            end_line(m, mark, name, app_id, re.escape(what), "WARN")
            if name == "greedy":
                # Its memory is fixed: memory.grow gives -1.
                wait_for_output(
                    m, mark, r"INFO  app: greedy: greedy: 32 pages, memory\.grow\(1\) gave -1\n"
                )
            wait_unlisted(m, app_id, TRAP_ENDS)
            assert run(m, "echo still here", allow=TRAP_ENDS) == "still here\n"
        # The other endings must give the heap back too: a kill (its store
        # dropped on the app's thread) and an exit.
        app_id, mark = start(m, "busy", allow=TRAP_ENDS)
        kill(m, app_id, "busy", allow=TRAP_ENDS)
        end_line(m, mark, "busy", app_id, "killed")
        wait_unlisted(m, app_id, TRAP_ENDS)
        app_id, mark = start(m, "hello exit 7", allow=TRAP_ENDS)
        end_line(m, mark, "hello", app_id, "exited with 7")
        wait_unlisted(m, app_id, TRAP_ENDS)

    # The shell's history at its cap first (100 lines), so the lines run
    # meanwhile replace others; one round to warm up (first-use
    # allocations); then a round must give the heap back.
    for i in range(100):
        run(m, f"echo filling-the-history-{i}")
    start_at = len(m.output)
    round_of_traps()
    before = heap_used(m)
    round_of_traps()
    after = heap_used(m)
    assert abs(after - before) <= 4096, (before, after)
    # Each ending gave exactly one WARN (its end line), and nothing an ERROR.
    serial = m.output[start_at:]
    assert len(re.findall(TRAP_ENDS, serial)) == 2 * len(cases)
    assert "ERROR" not in serial
    assert not re.search(r"^\[[ .\d]+\] WARN  app: forged", serial, re.MULTILINE)


def test_app_without_capability_is_not_granted(boot):
    m = boot_shell(boot)
    # Granted: its manifest lists time.
    app_id, mark = start(m, "nocap")
    wait_for_output(m, mark, r"INFO  app: nocap: nocap: time: granted\n", 30)
    end_line(m, mark, "nocap", app_id, "exited with 0")
    # Withheld: an error code, not a trap; a made-up handle too.
    app_id, mark = start(m, "--without time nocap")
    wait_for_output(m, mark, r"INFO  app: nocap: nocap: time: NotGranted\n", 30)
    wait_for_output(m, mark, r"INFO  app: nocap: nocap: made-up handle: BadHandle\n", 30)
    end_line(m, mark, "nocap", app_id, "exited with 0")


def test_handles_refuse_wrong_kind_and_closed(boot):
    m = boot_shell(boot)
    app_id, mark = start(m, "abuse handles")
    found = wait_for_output(
        m, mark, r"INFO  app: abuse: abuse: handles: (\d+) checks, (\d+) failed\n", 30
    )
    assert int(found[1]) >= 15 and found[2] == "0", m.output[mark:]
    end_line(m, mark, "abuse", app_id, "exited with 0")
    assert "FAILED" not in m.output[mark:]
    # `log` can't be withheld: every app holds it.
    assert run(m, "run --without log hello").strip() == "run: --without log: every app holds log"


def test_host_calls_refuse_bad_arguments(boot):
    m = boot_shell(boot)
    app_id, mark = start(m, "abuse calls")
    found = wait_for_output(
        m, mark, r"INFO  app: abuse: abuse: calls: (\d+) checks, (\d+) failed\n", 30
    )
    end_line(m, mark, "abuse", app_id, "exited with 0")
    assert int(found[1]) >= 30 and found[2] == "0", m.output[mark:]
    serial = m.output[mark:]
    assert "FAILED" not in serial and "WARN" not in serial and "ERROR" not in serial, serial


def test_kill_wakes_a_waiting_app(boot):
    m = boot_shell(boot)
    app_id, mark = start(m, "busy sleep")
    wait_for_output(m, mark, r"INFO  app: busy: busy: sleeping\n", 30)
    record, done = poll(
        m, lambda: app(m, app_id), lambda a: a is not None and a["state"] == "waiting", 30
    )
    assert done, record
    mark = len(m.output)
    kill(m, app_id, "busy")
    end_line(m, mark, "busy", app_id, "killed")
    wait_gone(m, app_id, record["thread"])
    assert run(m, f"kill {app_id}").strip() == f"kill: no app {app_id}"


def test_app_log_is_cut_and_sanitised(boot):
    m = boot_shell(boot)
    app_id, mark = start(m, "hello log")
    # Cut at 256 bytes on a char boundary: the `é` across the cut goes.
    wait_for_output(m, mark, r"INFO  app: hello: x{255}\n", 30)
    # The newline shown as U+FFFD: the rest stays on the app's line.
    wait_for_output(
        m, mark, r"INFO  app: hello: forged�\[    1\.000000\] INFO  boot: boot complete\n", 30
    )
    end_line(m, mark, "hello", app_id, "exited with 0")
    assert not re.search(r"^\[    1\.000000\] INFO  boot", m.output[mark:], re.MULTILINE)


# -- windows (#206) -------------------------------------------------------------


def boot_desktop(boot):
    m = boot_shell(boot)
    gui.start_desktop(m)
    return m


def test_template_app_runs(boot):
    # The committed template (what `make app` copies): `run` opens its
    # window, focused in front; a key typed through confirmed steps
    # changes what it draws; closing the window ends it (exit 0), its
    # thread joined and its window gone.
    m = boot_desktop(boot)
    app_id, mark = start(m, "template")
    row = gui.wait_window(m, "template")
    assert row["focused"] == "yes", row
    record, done = poll(
        m, lambda: app(m, app_id), lambda a: a is not None and a["windows"] == "1", 30
    )
    assert done, record
    gui.wait_settled(m)
    before = gui.client_pixels(m, "template", "template-before.png")
    assert len(set(before)) > 2, "the template drew nothing"
    gui.type_confirmed(m, "a")
    gui.wait_for(
        m,
        "the key redrawn",
        lambda: gui.client_pixels(m, "template", "template-after.png"),
        lambda c: c != before,
    )
    assert gui.invoke_close(m, "template") == f"ui invoke: window {row['id']} closed\n"
    end_line(m, mark, "template", app_id, "exited with 0")
    wait_gone(m, app_id, record["thread"])
    gui.wait_no_window(m, "template")


def test_app_ignoring_close_is_ended(boot):
    # A busy app given `Close` that keeps its window: ended after
    # CLOSE_GRACE_SLICES slice ends ("didn't close its window"), its window
    # gone with it and its thread joined. `ui invoke`'s close says closed
    # or, if the grace outlasts its deadline, still open.
    m = boot_desktop(boot)
    app_id, mark = start(m, "window ignore-close")
    wait_for_output(m, mark, r"INFO  app: window: window: opened\n", 30)
    row = gui.wait_window(m, "window")
    record = app(m, app_id)
    out = gui.invoke_close(m, "window")
    assert out in (
        f"ui invoke: window {row['id']} closed\n",
        f"ui invoke: window {row['id']} still open (its close is still queued)\n",
    ), out
    wait_for_output(m, mark, r"INFO  app: window: window: ignoring close\n", 30)
    end_line(m, mark, "window", app_id, "ended: didn't close its window")
    wait_gone(m, app_id, record["thread"])
    gui.wait_no_window(m, "window")


def test_window_calls_refuse_bad_arguments(boot):
    # The `abuse` app's `window` checks (see its doc): every refusal an
    # error code, each asserted by the app (all of them run: the count is
    # exact); exit 0, no WARN or ERROR but what a loaded host can cause.
    m = boot_desktop(boot)
    app_id, mark = start(m, "abuse window")
    found = wait_for_output(
        m, mark, r"INFO  app: abuse: abuse: window: (\d+) checks, (\d+) failed\n", 30
    )
    end_line(m, mark, "abuse", app_id, "exited with 0")
    assert (int(found[1]), found[2]) == (ABUSE_WINDOW_CHECKS, "0"), m.output[mark:]
    serial = m.output[mark:]
    assert "FAILED" not in serial, serial
    assert not klog.unexpected_warnings(serial), serial
    wait_unlisted(m, app_id)
    gui.wait_no_window(m, "abuse")


# `abuse window`'s checks: every one runs, whatever the answers.
ABUSE_WINDOW_CHECKS = 83


def test_kill_wakes_an_app_waiting_on_its_window(boot):
    # `window_wait(-1, 0)` blocks (`waiting`); `kill` wakes it through its
    # window: "killed", or any place `kill` can print on a host that
    # stalls the app; confirmed by the end line and the thread's join.
    m = boot_desktop(boot)
    app_id, mark = start(m, "window wait")
    wait_for_output(m, mark, r"INFO  app: window: window: waiting\n", 30)
    record, done = poll(
        m,
        lambda: app(m, app_id),
        lambda a: a is not None and a["state"] == "waiting" and a["windows"] == "1",
        30,
    )
    assert done, record
    mark = len(m.output)
    kill(m, app_id, "window")
    end_line(m, mark, "window", app_id, "killed")
    wait_gone(m, app_id, record["thread"])
    gui.wait_no_window(m, "window")


def test_sdk_logs_a_long_line_cut(boot):
    # The SDK cuts a line past TEXT_BYTES before `log` (the kernel would
    # refuse it whole): its start is logged, cut at LOG_BYTES (5 + 251).
    m = boot_shell(boot)
    app_id, mark = start(m, "window log")
    wait_for_output(m, mark, r"INFO  app: window: log: L{251}\n", 30)
    end_line(m, mark, "window", app_id, "exited with 0")


PANIC_END = r"WARN  app: window \(app \d+\) ended"


def test_sdk_panic_says_where(boot):
    # The SDK's panic handler: a 300-byte message cut so ` at <file>:<line>`
    # fits; one WARN, the end line.
    m = boot_shell(boot)
    app_id, mark = start(m, "window panic", allow=PANIC_END)
    end_line(m, mark, "window", app_id, r"ended: panicked: p{50,299} at \S+\.rs:\d+", "WARN")
    wait_unlisted(m, app_id, PANIC_END)
    warnings = klog.unexpected_warnings(m.output[mark:])
    assert len(warnings) == 1 and "ended: panicked" in warnings[0], warnings


def test_sdk_panic_cuts_on_a_char_boundary(boot):
    # 200 three-byte characters: the message is cut on a char boundary, so
    # nothing turns into U+FFFD and pushes the end past LOG_BYTES: the line
    # number shown is exactly the panic's.
    source = (REPO_ROOT / "apps" / "test-window" / "src" / "lib.rs").read_text().splitlines()
    line = next(i for i, text in enumerate(source, 1) if text.endswith("// marker: panic-utf8"))
    m = boot_shell(boot)
    app_id, mark = start(m, "window panic-utf8", allow=PANIC_END)
    end_line(m, mark, "window", app_id, rf"ended: panicked: €+ at \S+\.rs:{line}", "WARN")
    wait_unlisted(m, app_id, PANIC_END)
    assert "\ufffd" not in m.output[mark:]


def test_sdk_allocator_frees(boot):
    # 64 rounds of 256 KiB allocated and freed in a 2 MiB memory: only a
    # heap that frees gets through them.
    m = boot_shell(boot)
    app_id, mark = start(m, "window alloc")
    wait_for_output(m, mark, r"INFO  app: window: window: alloc: 64 rounds\n", 30)
    end_line(m, mark, "window", app_id, "exited with 0")


RED = (0xFF, 0x00, 0x00)


def test_sdk_commits_more_rects_than_one_call_takes(boot):
    # 20 rectangles (past COMMIT_RECTS: the SDK commits in turns) over the
    # right half of a red canvas, from x = 100: that half is red on the
    # screen, the left half (never committed) isn't.
    m = boot_desktop(boot)
    app_id, mark = start(m, "window rects")
    wait_for_output(m, mark, r"INFO  app: window: window: rects: committed 20\n", 30)
    row = gui.wait_window(m, "window")
    gui.wait_settled(m)
    w = int(row["w"]) - 2
    pixels = gui.client_pixels(m, "window", "rects.png")

    def at(x: int, y: int) -> tuple[int, ...]:
        i = (y * w + x) * 3
        return tuple(pixels[i : i + 3])

    for y in (0, 50, 99):
        assert all(at(x, y) == RED for x in range(100, 200)), y
        assert all(at(x, y) != RED for x in range(100)), y
    assert gui.invoke_close(m, "window") == f"ui invoke: window {row['id']} closed\n"
    end_line(m, mark, "window", app_id, "exited with 0")


def test_an_app_that_closes_its_window_has_no_close_grace(boot):
    # Its title sanitised (ESC and LF as U+FFFD); given `Close`, it drops
    # its window, then sleeps far past CLOSE_GRACE_SLICES slice ends: listed
    # with no window meanwhile, and ending with its own 0, never "didn't
    # close its window".
    m = boot_desktop(boot)
    app_id, mark = start(m, "window close")
    title = "a\ufffdb\ufffdc"
    row = gui.wait_window(m, title)
    assert gui.invoke_close(m, title) == f"ui invoke: window {row['id']} closed\n"
    wait_for_output(m, mark, r"INFO  app: window: window: closed it\n", 30)
    record, done = poll(
        m,
        lambda: app(m, app_id),
        lambda a: a is None or (a["windows"] == "0" and int(a["slices"]) > 4),
        30,
    )
    assert done and record is not None, f"app {app_id} gone before it was seen without its window"
    end_line(m, mark, "window", app_id, "exited with 0")
    assert "didn't close its window" not in m.output[mark:]
