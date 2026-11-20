"""The WASM app runtime (#199; ARCHITECTURE section 8): `run`, `apps`,
`kill`, the host functions, how an app ends, apps' windows (#206: the
template, the window calls' checks, the close grace, a kill waking a
`window_wait`), and apps' automation trees (#207: lumen-ui inside apps,
global ids, `ui invoke` reaching an app one request at a time, the
calculator).

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
from lumen_harness.image import assert_matches_golden
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
    assert out == "usage: run [--without <grant>] <app> [args...]\napps: calculator template\n", out
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
    before = gui.client_pixels(m, "template", "template-before.png").tobytes()
    assert len(set(before)) > 2, "the template drew nothing"
    gui.type_confirmed(m, "a")
    gui.wait_for(
        m,
        "the key redrawn",
        lambda: gui.client_pixels(m, "template", "template-after.png").tobytes(),
        lambda c: c != before,
    )
    gui.close_window(m, "template")
    end_line(m, mark, "template", app_id, "exited with 0")
    wait_gone(m, app_id, record["thread"])


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
ABUSE_WINDOW_CHECKS = 98


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
    # The SDK cuts a line past TEXT_BYTES on a char boundary before `log`
    # (byte TEXT_BYTES falls inside an `é`: cut there, the kernel would
    # refuse it whole as bad UTF-8): its start is logged, cut at LOG_BYTES
    # (5 + 125 two-byte characters).
    m = boot_shell(boot)
    app_id, mark = start(m, "window log")
    wait_for_output(m, mark, r"INFO  app: window: log: é{125}\n", 30)
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
    # 64 rounds of 256 KiB allocated and freed in a 4 MiB memory: only a
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
    client = gui.client_pixels(m, "window", "rects.png")
    assert client.size == (200, 100), (client.size, row)
    for y in (0, 50, 99):
        assert all(client.getpixel((x, y)) == RED for x in range(100, 200)), y
        assert all(client.getpixel((x, y)) != RED for x in range(100)), y
    gui.close_window(m, "window")
    end_line(m, mark, "window", app_id, "exited with 0")


def test_an_app_that_closes_its_window_has_no_close_grace(boot):
    # Given `Close`, it drops its window, then sleeps 100 ms at a time until
    # killed: once seen past CLOSE_GRACE_SLICES more slice ends than it had
    # when it said so, with no window, the grace can't have ended it; its
    # end is the kill's, never "didn't close its window".
    m = boot_desktop(boot)
    app_id, mark = start(m, "window close")
    gui.wait_window(m, CLOSE_TITLE)
    gui.close_window(m, CLOSE_TITLE)
    wait_for_output(m, mark, r"INFO  app: window: window: closed it\n", 30)
    closed = app(m, app_id)
    assert closed is not None, f"app {app_id} gone"
    record, done = poll(
        m,
        lambda: app(m, app_id),
        lambda a: (
            a is None
            or (
                a["windows"] == "0"
                and int(a["slices"]) >= int(closed["slices"]) + CLOSE_GRACE_SLICES + 1
            )
        ),
        30,
    )
    assert done and record is not None, (closed, record)
    mark = len(m.output)
    kill(m, app_id, "window")
    end_line(m, mark, "window", app_id, "killed")
    wait_gone(m, app_id, record["thread"])
    assert "didn't close its window" not in m.output


# `window close`'s title, `a ESC b LF c`, as `windows` shows it (the
# desktop's own copy is checked in `a_window_title_is_sanitised`).
CLOSE_TITLE = "a\ufffdb\ufffdc"
# lumen_app_api::limits::CLOSE_GRACE_SLICES.
CLOSE_GRACE_SLICES = 4


def test_sdk_wraps_its_other_calls(boot):
    # `window sdk`: open_window past WINDOW_PIXELS is BadArgument, Trace
    # (cpus, a read agreeing with its header), measure, the clocks, waits
    # with a deadline (animating or not), animating without one refused, a
    # toolkit's tree area off the client refused; then exit(7).
    m = boot_desktop(boot)
    app_id, mark = start(m, "window sdk")
    found = wait_for_output(
        m, mark, r"INFO  app: window: window: sdk: (\d+) checks, (\d+) failed\n", 30
    )
    assert (int(found[1]), found[2]) == (11, "0"), m.output[mark:]
    end_line(m, mark, "window", app_id, "exited with 7")


# -- apps' automation trees (#207) ------------------------------------------------


def below_root(rows: list[dict[str, str]]) -> list[dict[str, str]]:
    """A window's tree rows without its root (depth 1 on)."""
    return [r for r in rows if r["depth"] != "0"]


def wait_tree(m, title: str, name: str) -> tuple[str, list[dict[str, str]]]:
    """Until window `title`'s published tree has a node named `name`: its
    id and its rows below the root."""
    wid = gui.wait_window(m, title)["id"]
    rows = gui.wait_ui(
        m, f"{title}'s {name}", lambda r: any(x["name"] == name for x in below_root(r)), wid, 30
    )
    return wid, below_root(rows)


CALC_BUTTONS = [*"789/456*123-0.=+", "C", "Back"]


def calc_tree(m) -> tuple[str, list[dict[str, str]]]:
    return wait_tree(m, "Calculator", "Display")


def display(m, wid: str) -> str:
    return gui.node(gui.ui_tree(m, wid), "label", "Display")["value"]


def test_calculator_tree_and_invoke(boot):
    # The calculator (lumen-ui inside the app): `ui tree` lists its display
    # and every button, ids global; `ui invoke` presses them as clicks do:
    # 2 + 3 = shows 5 in the display's value, published before each answer
    # (read at once, no poll); keys typed through confirmed steps reach it
    # too (a sum with a new result, Backspace and Escape); closing its
    # window ends it with 0.
    m = boot_desktop(boot)
    app_id, mark = start(m, "calculator")
    wid, rows = calc_tree(m)
    assert gui.window(m, "Calculator")["focused"] == "yes"
    buttons = [r["name"] for r in rows if r["role"] == "button"]
    assert buttons == CALC_BUTTONS, buttons
    assert gui.node(rows, "label", "Display")["value"] == "0"
    gui.press_buttons(m, rows, "2", "+", "3")
    assert display(m, wid) == "3"
    gui.press_buttons(m, rows, "=")
    assert display(m, wid) == "5"

    # Keys: `c` clears, then 9 - 2 and Enter: 7 (a value the display
    # didn't show before).
    def shows(value: str):
        return lambda r: gui.node(r, "label", "Display")["value"] == value

    gui.confirmed_keys(m, "c9-2", ["ret"])
    gui.wait_ui(m, "9 - 2 = 7 typed", shows("7"), wid)
    # Backspace edits the entry; Escape clears.
    gui.confirmed_keys(m, "12", ["backspace"])
    gui.wait_ui(m, "12 then Backspace", shows("1"), wid)
    gui.confirmed_keys(m, ["esc"])
    gui.wait_ui(m, "Escape", shows("0"), wid)
    gui.close_window(m, "Calculator")
    end_line(m, mark, "calculator", app_id, "exited with 0")


# The longest value the display shows (lumen-calc's
# `the_longest_result_shows_whole`): 0 - 999999999999999 = / 7 =.
LONGEST = "-142857142857142.714285714"


def test_calculator_golden(boot, artifacts, golden_dir):
    # The calculator's client area showing its longest value whole, against
    # a golden: the kernel's toolkit and font, drawn inside the app.
    m = boot_desktop(boot)
    start(m, "calculator")
    wid, rows = calc_tree(m)
    gui.press_buttons(m, rows, "0", "-", *"999999999999999", "=", "/", "7", "=")
    assert display(m, wid) == LONGEST
    row = gui.window(m, "Calculator")
    x0, y0, x1, y1 = gui.client_box(row)
    shot = gui.settled_screendump(m, "calculator.png")
    assert_matches_golden(
        shot,
        golden_dir / "app-calculator.png",
        origin=(x0, y0),
        size=(x1 - x0, y1 - y0),
        artifacts=artifacts,
    )


def test_app_ids_are_global(boot):
    # Two copies of one app (the same app ids inside): their trees never
    # share a global id; once one closes, its ids name nothing (`no node`)
    # and the other's still answer.
    m = boot_desktop(boot)
    start(m, "calculator")
    start(m, "calculator")
    gui.wait_for(
        m,
        "two calculators",
        lambda: gui.windows(m),
        lambda w: sum(r["title"] == "Calculator" for r in w) == 2,
        30,
    )
    trees = {}
    for w in (r for r in gui.windows(m) if r["title"] == "Calculator"):
        rows = gui.wait_ui(m, f"window {w['id']}'s tree", lambda r: len(r) > 1, w["id"], 30)
        trees[w["id"]] = below_root(rows)
    (a, rows_a), (b, rows_b) = trees.items()
    assert [r["name"] for r in rows_a] == [r["name"] for r in rows_b]
    ids_a, ids_b = {r["id"] for r in rows_a}, {r["id"] for r in rows_b}
    assert len(ids_a) == len(rows_a) and not ids_a & ids_b, (ids_a, ids_b)
    root_a = gui.one(gui.ui_tree(m, a), "roots", depth="0")
    assert run(m, f"ui invoke {root_a['id']} close") in (
        f"ui invoke: window {a} closed\n",
        f"ui invoke: window {a} still open (its close is still queued)\n",
    )
    gui.wait_for(
        m, f"window {a} gone", lambda: gui.windows(m), lambda w: all(r["id"] != a for r in w), 30
    )
    seven = gui.node(rows_a, "button", "7")
    assert run(m, f"ui invoke {seven['id']} press") == f"ui invoke: no node {seven['id']}\n"
    gui.press_buttons(m, rows_b, "7")
    assert display(m, b) == "7"


def test_closed_its_window_answer_comes_after_the_window_closes(boot):
    # `window ui` runs Counter (the kernel's demo app) in the SDK's
    # toolkit: its answers come once their effect is published (Lock's own
    # row says checked); Quit closes its window, and `ui invoke`'s "ran;
    # window closed" comes once the window is gone (read at once, no poll;
    # on a host that stalled the owner past both deadlines, "still running"
    # then the window's going); the app returns 0. A label longer than
    # TEXT_BYTES is measured and drawn cut (no text call refused: no WARN).
    m = boot_desktop(boot)
    app_id, mark = start(m, "window ui")
    wid, rows = wait_tree(m, "window", "Quit")
    gui.press_buttons(m, rows, "Add")
    assert gui.node(gui.ui_tree(m, wid), "label", "Count")["value"] == "1"
    lock = gui.node(rows, "checkbox", "Lock")
    got = gui.invoke(m, lock["id"], "press")
    if got.still_running:
        gui.wait_ui(
            m, "Lock checked", lambda r: gui.node(r, "checkbox", "Lock")["value"] == "checked", wid
        )
    else:
        assert got.row is not None and got.row["value"] == "checked", got
    quit_ = gui.node(rows, "button", "Quit")
    got = gui.invoke(m, quit_["id"], "press")
    if got.still_running:
        gui.wait_no_window(m, "window")
    else:
        assert got.message == f"ui invoke: ran; window {wid} closed", got
        assert all(r["id"] != wid for r in gui.windows(m)), "answered before its window went"
    end_line(m, mark, "window", app_id, "exited with 0")
    assert "refused" not in m.output[mark:], "a text call refused"


TRAPPED = r"WARN  app: window \(app \d+\) ended: trap: "


def test_app_that_traps_closes_its_window_and_answers(boot):
    # `window ui-trap`: a press on Add traps while its request runs; the
    # app's end closes its window and answers the asker `OwnerEnded`
    # ("closed while running it", perhaps after "ran late"), or, on a host
    # that stalled the app past both deadlines, "still running"; never
    # "dropped" (an ERROR). A request cancelled or refused busy never ran:
    # tried again. Confirmed by the end line (one WARN), the window's going
    # and no ERROR.
    m = boot_desktop(boot)
    app_id, mark = start(m, "window ui-trap")
    wid, rows = wait_tree(m, "window", "Add")
    add = gui.node(rows, "button", "Add")
    got = gui.invoke(m, add["id"], "press", tries=20, allow=TRAPPED)
    ended = f"ui invoke: window {wid} closed while running it (it may have run)"
    assert got.message in (ended, f"ui invoke: still running (window {wid})"), got
    end_line(
        m, mark, "window", app_id, "ended: trap: wasm `unreachable` instruction executed", "WARN"
    )
    gui.wait_no_window(m, "window")
    assert "ERROR" not in m.output[mark:]


def test_deepest_tree_is_published(boot):
    # `window deep` publishes TREE_NODES groups each holding the next: the
    # deepest tree there is, decoded without recursion; `ui tree` lists all
    # of them under its root, the last at depth TREE_NODES.
    m = boot_desktop(boot)
    app_id, mark = start(m, "window deep")
    wait_for_output(
        m, mark, r"INFO  app: window: window: deep: published 1024 nodes \(\d+ bytes\)\n", 30
    )
    wid = gui.window(m, "window")["id"]
    rows = gui.ui_tree(m, wid)
    depths = [int(r["depth"]) for r in rows]
    assert depths == list(range(TREE_NODES + 1)), depths[:5]
    gui.close_window(m, "window")
    end_line(m, mark, "window", app_id, "exited with 0")


# lumen_app_api::limits::TREE_NODES.
TREE_NODES = 1024


def test_one_request_per_window_refuses_a_second_as_busy(boot):
    # `window deaf` publishes a button and a label whose text holds
    # ESC, LF and U+2028 (each shown U+FFFD: `ui tree` still parses), then a
    # tree the host refuses (the last one stays: `ui tree` still has them),
    # and never reads its events: a request to it is never taken (its asker
    # cancels it at the deadline, whatever the host's speed), so it stays
    # queued and holds the window's one request slot: a second `ui invoke`
    # is refused before anything is queued. (A request running, not queued,
    # holds it too: the window ktest.) Killing the app frees everything.
    m = boot_desktop(boot)
    app_id, mark = start(m, "window deaf")
    wait_for_output(
        m, mark, r"INFO  app: window: window: deaf: published; a bad tree then: -5\n", 30
    )
    wid, rows = wait_tree(m, "window", "Go")
    say = gui.node(rows, "label", "Say")
    assert say["value"] == "a\ufffdb\ufffdc\ufffdd", say
    go = gui.node(rows, "button", "Go")
    first = run(m, f"ui invoke {go['id']} press")
    assert first == f"ui invoke: cancelled: window {wid} didn't take it in 1 s (try again)\n", first
    second = run(m, f"ui invoke {go['id']} press")
    assert second == f"ui invoke: a request is still queued or running in window {wid}\n", second
    record = app(m, app_id)
    mark = len(m.output)
    kill(m, app_id, "window")
    end_line(m, mark, "window", app_id, "killed")
    wait_gone(m, app_id, record["thread"])
    gui.wait_no_window(m, "window")


# The SDK's line when its toolkit can't publish a tree (lumen-app's ui.rs).
NOT_PUBLISHED = r"WARN  app: window: ui: a tree not published: the tree: more than 1024 nodes"


def test_a_tree_the_sdk_cant_publish_answers_failed(boot):
    # `window ui-big`: a press on Grow adds more labels than TREE_NODES.
    # The SDK refuses that tree itself: one WARN, the kernel keeping the
    # last tree (`ui tree` still shows Grow alone), and the press answered
    # "couldn't publish", not with a row from the stale tree; a second
    # press is answered so too, with no second WARN (once a run).
    m = boot_desktop(boot)
    app_id, mark = start(m, "window ui-big")
    wid, rows = wait_tree(m, "window", "Grow")
    grow = gui.node(rows, "button", "Grow")
    failed = "ui invoke: couldn't publish: the tree: more than 1024 nodes"
    for _ in range(2):
        got = gui.invoke(m, grow["id"], "press", allow=NOT_PUBLISHED)
        if got.still_running:
            gui.wait_settled(m, allow=NOT_PUBLISHED)
        else:
            assert got.message == failed, got
    assert len(re.findall(NOT_PUBLISHED, m.output[mark:])) == 1, m.output[mark:]
    assert [r["name"] for r in below_root(gui.ui_tree(m, wid))] == ["Grow"]
    gui.close_window(m, "window")
    end_line(m, mark, "window", app_id, "exited with 0")


# -- the SDK's tick loop (`ui::Toolkit::run_until`): `window tick` -------------

# test-window's `tick` mode (apps/test-window/src/lib.rs): its client's
# width, its tree's (the app's own area is right of it) and its square's
# side.
TICK_W = 320
TICK_TREE_W = 140
SQUARE = 16
TICK_RUNNING = r"INFO  app: window: window: tick: running\n"
TICK_ACCENT = r"INFO  app: window: window: tick: accent 0x([0-9a-f]{6})\n"


def ticks(rows: list[dict[str, str]]) -> int:
    return int(gui.node(rows, "label", "Ticks")["value"])


def tick_state(rows: list[dict[str, str]]) -> str:
    return gui.node(rows, "label", "State")["value"]


def start_ticker(m, mode: str = "") -> tuple[int, int, str, list[dict[str, str]]]:
    """`run window tick [mode]`: its app id, the serial mark, its window's
    id and its tree's rows."""
    app_id, mark = start(m, f"window tick {mode}".strip())
    wait_for_output(m, mark, TICK_RUNNING, 30)
    wid, rows = wait_tree(m, "window", "Ticks")
    return app_id, mark, wid, rows


def stop_ticker(m, wid: str, rows: list[dict[str, str]]) -> list[dict[str, str]]:
    """Stop pressed while it ticks, confirmed by the tree; then settled (no
    period: nothing animates). The rows then."""
    gui.press_animated(m, rows, "Stop", "stopped", lambda r: tick_state(r) == "stopped", wid)
    gui.wait_settled(m)
    return gui.ui_tree(m, wid)


TICK_PERIOD_MS = 50
TICK_ENDED = (
    r"INFO  app: window: window: tick: {how} after (\d+) ticks, "
    r"(\d+) ms from the first to the last\n"
)


def tick_ended(m, mark: int, how: str = "closed") -> int:
    """The tick app's end line (`closed` or `quit`): its tick count, once
    the pacer's floor is checked: no tick came less than a period after the
    one before (the app's WARN), and from the first tick to the last took
    at least a period a tick."""
    assert "ns early" not in m.output[mark:], m.output[mark:]
    found = re.search(TICK_ENDED.format(how=how), m.output[mark:])
    assert found, m.output[mark:]
    count, elapsed = int(found[1]), int(found[2])
    assert elapsed >= max(count - 1, 0) * TICK_PERIOD_MS, found[0]
    return count


def square_moved(m, accent: int, before: list[tuple[int, int]], name: str) -> None:
    """Until the square is drawn one step on from `before` (test-window's
    `move_square`: 8 to the right, back to the area's left edge + 10 past
    its right one), seen on the screen."""
    x0 = min(x for x, _ in before)
    want = x0 + 8 if x0 + 8 + SQUARE <= TICK_W else TICK_TREE_W + 10
    gui.wait_for(
        m,
        f"the square moved to x {want}",
        lambda: accent_pixels(m, name, accent),
        lambda p: len(p) == SQUARE * SQUARE and min(x for x, _ in p) == want,
    )


def accent_pixels(m, name: str, accent: int) -> list[tuple[int, int]]:
    """The client pixels right of the tree in colour `accent`: the square."""
    img = gui.client_pixels(m, "window", name)
    rgb = ((accent >> 16) & 0xFF, (accent >> 8) & 0xFF, accent & 0xFF)
    w, h = img.size
    return [(x, y) for y in range(h) for x in range(TICK_TREE_W, w) if img.getpixel((x, y)) == rgb]


def test_sdk_tick_loop_answers_requests_and_closes_while_ticking(boot):
    # `run_until` ticks (the label's count grows: a floor, never a time)
    # and still hands requests to the toolkit: Stop, pressed while it
    # ticks, is answered (or still running, then seen in the tree); Start
    # sets it ticking again (once the stop is confirmed: settled); the
    # window's close, while it ticks, ends the loop: exit 0, its line
    # counting at least the ticks seen (each tick at least a period after
    # the one before), its thread joined.
    m = boot_desktop(boot)
    app_id, mark, wid, rows = start_ticker(m)
    record = app(m, app_id)
    rows = gui.wait_ui(m, "two ticks", lambda r: ticks(r) >= 2, wid, 30)
    rows = stop_ticker(m, wid, rows)
    seen = ticks(rows)
    rows = gui.press_animated(m, rows, "Start", "ticking again", lambda r: ticks(r) > seen + 1, wid)
    assert tick_state(rows) == "running"
    seen = ticks(rows)
    gui.close_window(m, "window")
    end_line(m, mark, "window", app_id, "exited with 0")
    assert tick_ended(m, mark) >= seen
    wait_gone(m, app_id, record["thread"])


def test_sdk_tick_loop_stopped_lets_the_desktop_settle(boot):
    # Ticking, its waits are an animation's (`Ticking::animating`): the
    # desktop never reads settled, however often it is asked. With no
    # period (Stop) the loop waits for events only: the desktop settles and
    # the count stays put across two settled reads; Start resumes it.
    m = boot_desktop(boot)
    app_id, mark, wid, rows = start_ticker(m)
    gui.wait_ui(m, "a tick", lambda r: ticks(r) >= 1, wid, 30)
    for _ in range(5):
        assert gui.desktop_status(m)["settled"] == "no"
    rows = stop_ticker(m, wid, rows)
    first = ticks(rows)
    gui.wait_settled(m)
    assert ticks(gui.ui_tree(m, wid)) == first
    gui.press_animated(m, rows, "Start", "ticking again", lambda r: ticks(r) > first, wid)
    gui.close_window(m, "window")
    end_line(m, mark, "window", app_id, "exited with 0")
    tick_ended(m, mark)


def test_sdk_app_takes_keys_draws_alone_and_quits(boot):
    # Keys the app takes before the toolkit (`Taken`): `n` moves its square
    # with no tick, the move committed (seen on the screen) and the tree
    # unchanged; Space, with Start focused, moves it too and never reaches
    # the tree (Start isn't pressed: still stopped, no tick); Escape quits
    # the loop (`Taken::Quit`): exit 0, the window gone, its thread joined.
    m = boot_desktop(boot)
    app_id, mark, wid, rows = start_ticker(m)
    record = app(m, app_id)
    accent = int(wait_for_output(m, mark, TICK_ACCENT, 30)[1], 16)
    rows = stop_ticker(m, wid, rows)
    count = ticks(rows)
    before = accent_pixels(m, "tick-before.png", accent)
    assert len(before) == SQUARE * SQUARE, len(before)
    gui.confirmed_keys(m, "n")
    square_moved(m, accent, before, "tick-after-n.png")
    gui.wait_settled(m)
    assert ticks(gui.ui_tree(m, wid)) == count, "a key the app took is no tick"
    start_button = gui.node(rows, "button", "Start")
    focused = gui.invoke(m, start_button["id"], "focus")
    assert focused.row is not None and "focused" in focused.row["state"], focused
    before = accent_pixels(m, "tick-before-space.png", accent)
    gui.confirmed_keys(m, " ")
    square_moved(m, accent, before, "tick-after-space.png")
    gui.wait_settled(m)
    rows = gui.ui_tree(m, wid)
    assert (tick_state(rows), ticks(rows)) == ("stopped", count), "Space reached the tree"
    gui.confirmed_keys(m, ["esc"])
    end_line(m, mark, "window", app_id, "exited with 0")
    assert tick_ended(m, mark, "quit") == count
    gui.wait_no_window(m, "window")
    wait_gone(m, app_id, record["thread"])


def test_sdk_custom_drawing_follows_the_theme(boot):
    # The app's own drawing gets the palette: its square is in the light
    # theme's accent, then (`theme dark`) the dark one's, at the same place,
    # with no tick (stopped: the switch's full repaint draws it).
    m = boot_desktop(boot)
    app_id, mark, wid, rows = start_ticker(m)
    light = int(wait_for_output(m, mark, TICK_ACCENT, 30)[1], 16)
    stop_ticker(m, wid, rows)
    recoloured(m, mark, light)
    gui.close_window(m, "window")
    end_line(m, mark, "window", app_id, "exited with 0")
    tick_ended(m, mark)


def recoloured(m, mark: int, light: int) -> None:
    """The stopped tick app's square in `light` (the light accent), then,
    after `theme dark`, in the dark accent (a new accent logged) at the same
    pixels."""
    square = accent_pixels(m, "tick-light.png", light)
    assert len(square) == SQUARE * SQUARE, len(square)
    switched = len(m.output)
    assert run(m, "theme dark") == "theme: dark\n"
    dark = int(wait_for_output(m, switched, TICK_ACCENT, 30)[1], 16)
    assert dark != light
    gui.wait_for(
        m,
        "the square in the dark accent",
        lambda: accent_pixels(m, "tick-dark.png", dark),
        lambda p: sorted(p) == sorted(square),
    )


def test_sdk_a_greedy_take_still_leaves_the_toolkit_its_events(boot):
    # `tick greedy` takes every event offered to it, yet requests, `Theme`
    # and `Close` never reach its `take`, and the focus reaches the tree
    # after it: a press is still answered and seen in the tree, a focused
    # button shows focused, `theme dark` still recolours its square, and
    # closing the window still ends it with 0.
    m = boot_desktop(boot)
    app_id, mark, wid, rows = start_ticker(m, "greedy")
    record = app(m, app_id)
    light = int(wait_for_output(m, mark, TICK_ACCENT, 30)[1], 16)
    gui.wait_ui(m, "a tick", lambda r: ticks(r) >= 1, wid, 30)
    rows = stop_ticker(m, wid, rows)
    # Focused only with the window's focus, which the app's `take` took
    # (`FocusGained`): it reached the tree through the `Notify` step.
    focused = gui.invoke(m, gui.node(rows, "button", "Start")["id"], "focus")
    assert focused.row is not None and "focused" in focused.row["state"], focused
    recoloured(m, mark, light)
    gui.close_window(m, "window")
    end_line(m, mark, "window", app_id, "exited with 0")
    tick_ended(m, mark)
    wait_gone(m, app_id, record["thread"])


def test_sdk_a_tick_can_quit(boot):
    # `tick quit`: its fifth tick says `Flow::Quit`: the loop returns
    # (`quit after 5 ticks`), exit 0, the window gone.
    m = boot_desktop(boot)
    app_id, mark, _, _ = start_ticker(m, "quit")
    end_line(m, mark, "window", app_id, "exited with 0")
    assert tick_ended(m, mark, "quit") == 5
    gui.wait_no_window(m, "window")


def test_sdk_an_overrunning_tick_still_waits_a_period(boot):
    # `tick slow`: each tick's work (a 75 ms sleep) is longer than its
    # 50 ms period, so the pacer's next deadline has always passed when the
    # work ends: only its count from the end of the work (`Pacer::ticked`)
    # keeps the next tick a period away. Deterministic: the work is longer
    # than the period however fast the host. The app WARNs on a tick less
    # than a period after the last one's work ended; a few ticks, then
    # closed.
    m = boot_desktop(boot)
    app_id, mark, wid, _ = start_ticker(m, "slow")
    gui.wait_ui(m, "four slow ticks", lambda r: ticks(r) >= 4, wid, 30)
    gui.close_window(m, "window")
    end_line(m, mark, "window", app_id, "exited with 0")
    assert tick_ended(m, mark) >= 4


# -- the image viewer (#204) ---------------------------------------------------

WALLPAPER = REPO_ROOT / "assets" / "wallpaper" / "lumen.png"
VIEWER_IMAGES = REPO_ROOT / "apps" / "viewer" / "images"
# lumen-view's VIEW (client coordinates: x, y, w, h) and PAN_STEP.
VIEW = (200, 0, 440, 480)
PAN_STEP = 40
# Decoding the wallpaper takes a few hundred fuel slices under TCG: a bound
# before failing, not a timing claim.
DECODE_TIMEOUT = 120


def viewer_tree(m) -> tuple[str, list[dict[str, str]]]:
    """The viewer's window id and its tree (the root's children), once
    `ui tree` shows its widgets."""
    wid = gui.wait_window(m, "Viewer")["id"]
    rows = gui.wait_ui(m, "the viewer's widgets", lambda r: len(r) > 1, wid, 30)
    return wid, below_root(rows)


def viewer_info(rows: list[dict[str, str]]) -> str:
    return gui.node(rows, "label", "Info")["value"]


def wait_info(m, wid: str, what: str, done) -> list[dict[str, str]]:
    """Until the viewer's `Info` label satisfies `done` (an image decoded
    is confirmed by its name and size there); its tree then."""
    rows = gui.wait_ui(m, what, lambda r: done(viewer_info(below_root(r))), wid, DECODE_TIMEOUT)
    return below_root(rows)


def view_of(m, name: str):
    """The viewer's view (lumen-view's VIEW of its client area) in a settled
    screendump: an RGB PIL image."""
    from PIL import Image

    x0, y0, _, _ = gui.client_box(gui.window(m, "Viewer"))
    shot = gui.settled_screendump(m, name)
    vx, vy, vw, vh = VIEW
    with Image.open(shot) as img:
        return img.convert("RGB").crop((x0 + vx, y0 + vy, x0 + vx + vw, y0 + vy + vh))


def fitted(png) -> tuple[tuple[int, int, int, int], object]:
    """Where `png` fitted to the view goes (a crop box in view coordinates)
    and what it shows there: lumen-view's `fit_size`, `fitted` and `source`
    (nearest neighbour, rounded down), restated."""
    from PIL import Image

    with Image.open(png) as img:
        src = img.convert("RGB")
    sw, sh = src.size
    vw, vh = VIEW[2], VIEW[3]
    w, h = (sw * vh // sh, vh) if sw * vh <= sh * vw else (vw, sh * vw // sw)
    w, h = max(w, 1), max(h, 1)
    x0, y0 = (vw - w) // 2, (vh - h) // 2
    out = Image.new("RGB", (w, h))
    out.putdata([src.getpixel((x * sw // w, y * sh // h)) for y in range(h) for x in range(w)])
    return (x0, y0, x0 + w, y0 + h), out


def differing(actual, want) -> int:
    """How many pixels of two same-sized RGB images differ."""
    assert actual.size == want.size, (actual.size, want.size)
    a, b = actual.tobytes(), want.tobytes()
    return sum(a[i : i + 3] != b[i : i + 3] for i in range(0, len(a), 3))


def set_fit(m, wid: str, rows: list[dict[str, str]], on: bool) -> list[dict[str, str]]:
    """Press `Fit` until it is `on` (its row's value), then wait for the
    desktop to settle (the view redrawn)."""
    box = gui.node(rows, "checkbox", "Fit")
    if (box["value"] == "checked") != on:
        got = gui.invoke(m, box["id"], "press")
        assert got.row is not None or got.still_running, got
    rows = gui.wait_ui(
        m,
        f"Fit {on}",
        lambda r: (gui.node(r, "checkbox", "Fit")["value"] == "checked") == on,
        wid,
        30,
    )
    gui.wait_settled(m)
    return below_root(rows)


def test_viewer_golden(boot):
    # The wallpaper, decoded inside the app, at 1:1: the view equals the
    # PNG's own pixels from its top left, and after the arrow keys pan it
    # (confirmed keys: right twice, down once) from where they moved it;
    # fitted, the view equals the PNG scaled by nearest neighbour (lumen-
    # view's rule, restated here) into the middle of the view.
    from PIL import Image

    m = boot_desktop(boot)
    app_id, mark = start(m, "viewer")
    wid, rows = viewer_tree(m)
    rows = wait_info(m, wid, "the wallpaper decoded", lambda v: v == "lumen.png 1280x800")
    assert gui.node(rows, "list", "Images")["value"] == "0"
    with Image.open(WALLPAPER) as img:
        wallpaper = img.convert("RGB")
    vw, vh = VIEW[2], VIEW[3]

    rows = set_fit(m, wid, rows, False)
    view = view_of(m, "viewer-1to1.png")
    assert differing(view, wallpaper.crop((0, 0, vw, vh))) == 0

    gui.confirmed_keys(m, ["right"], ["right"], ["down"])
    x, y = 2 * PAN_STEP, PAN_STEP
    want = wallpaper.crop((x, y, x + vw, y + vh))
    gui.wait_for(
        m,
        "the view panned",
        lambda: differing(view_of(m, "viewer-panned.png"), want),
        lambda n: n == 0,
        30,
    )

    rows = set_fit(m, wid, rows, True)
    box, want = fitted(WALLPAPER)
    assert differing(view_of(m, "viewer-fit.png").crop(box), want) == 0
    gui.close_window(m, "Viewer")
    end_line(m, mark, "viewer", app_id, "exited with 0")


def test_viewer_list_and_select(boot):
    # The image list through `ui invoke`: selecting `tiles.png` shows it
    # (the label its name and size once decoded, the list's value its
    # index), fitted: a small image enlarged by nearest neighbour into the
    # middle of the view; Next and Previous move through the list (Next
    # from the last goes to the first), the label following each time.
    m = boot_desktop(boot)
    app_id, mark = start(m, "viewer")
    wid, rows = viewer_tree(m)
    rows = wait_info(m, wid, "the wallpaper decoded", lambda v: v == "lumen.png 1280x800")
    items = [r["name"] for r in rows if r["role"] == "listitem"]
    assert items == ["lumen.png", "tiles.png", "grey16.png", "broken.png"], items
    tiles = gui.node(rows, "listitem", "tiles.png")
    got = gui.invoke(m, tiles["id"], "select")
    assert got.row is not None or got.still_running, got
    rows = wait_info(m, wid, "tiles.png decoded", lambda v: v == "tiles.png 96x64")
    assert gui.node(rows, "list", "Images")["value"] == "1"
    box, want = fitted(VIEWER_IMAGES / "tiles.png")
    assert differing(view_of(m, "viewer-tiles.png").crop(box), want) == 0

    gui.press_buttons(m, rows, "Next")
    rows = wait_info(m, wid, "grey16.png decoded", lambda v: v == "grey16.png 40x24")
    assert gui.node(rows, "list", "Images")["value"] == "2"
    gui.press_buttons(m, rows, "Previous")
    rows = wait_info(m, wid, "back to tiles.png", lambda v: v == "tiles.png 96x64")
    gui.press_buttons(m, rows, "Next", "Next", "Next")
    rows = wait_info(m, wid, "round to the wallpaper", lambda v: v == "lumen.png 1280x800")
    assert gui.node(rows, "list", "Images")["value"] == "0"
    gui.close_window(m, "Viewer")
    end_line(m, mark, "viewer", app_id, "exited with 0")


def test_viewer_bad_image_shows_an_error(boot):
    # A corrupt bundled image (`broken.png`: a flipped byte, its IDAT's CRC
    # wrong), chosen at the start: the decoder inside the app refuses it;
    # the label and the view say why (the view no longer one colour), one
    # INFO line logs it, no WARN or ERROR; the app keeps running: the
    # wallpaper, chosen next, is shown, and its close ends it with 0.
    m = boot_desktop(boot)
    app_id, mark = start(m, "viewer broken.png")
    wid, rows = viewer_tree(m)
    prefix = "broken.png: not a PNG it can read: "
    rows = wait_info(m, wid, "broken.png refused", lambda v: v.startswith(prefix))
    assert gui.node(rows, "list", "Images")["value"] == "3"
    why = viewer_info(rows)[len("broken.png: ") :]
    wait_for_output(m, mark, rf"INFO  app: viewer: viewer: broken\.png: {re.escape(why)}\n", 10)
    view = view_of(m, "viewer-broken.png")
    assert len(view.getcolors(1 << 20)) > 1, "the view shows nothing"
    record = app(m, app_id)
    assert record is not None and record["windows"] == "1", record
    assert not klog.unexpected_warnings(m.output[mark:]), klog.unexpected_warnings(m.output[mark:])
    wallpaper = gui.node(rows, "listitem", "lumen.png")
    got = gui.invoke(m, wallpaper["id"], "select")
    assert got.row is not None or got.still_running, got
    wait_info(m, wid, "the wallpaper decoded", lambda v: v == "lumen.png 1280x800")
    gui.close_window(m, "Viewer")
    end_line(m, mark, "viewer", app_id, "exited with 0")
