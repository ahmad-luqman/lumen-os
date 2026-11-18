"""Every key a scenario types is a confirmed step (#128), or says why not.

Keys typed with `Machine.press`, `Machine.type_text` or QMP's `send-key`
go to QEMU and return; a key lost under load then shows only as a wrong
screen. Where #128's keys were lost wasn't found; the likely place is QEMU's
emulated PS/2 keyboard queue, which only the guest drains, filling while
keys are sent to a stalled guest (not the input queue `Machine.press`
paces, nor the kernel's, whose drops `kbd` counts).
`lumen_harness.gui.confirmed_keys` sends the next key only once the guest
counted the last (and fails naming a lost one).

So in tests/scenarios, each statement that uses a raw key path (calls or
names `.press`, `.type_text` or `.send_key`, or names QMP's `send-key`
anywhere, as a string) needs an `# unconfirmed: <why>` comment of its own:
on its line, or in the comment-only block directly above it. Reasons such
as: a race on purpose, raw typing being what the test is about, no serial
shell free to run `kbd`, or an effect read next that a confirmed step's
`kbd` reading would take into its own output. In tools/harness/lumen_harness
and tests/conftest.py only `RAW_ALLOWED`'s functions may use them (the ones
implementing the raw paths, and `confirmed_keys`, which confirms each
press), so a helper scenarios call can't type raw unseen. (QMP
`input-send-event` key events can't be sent: `QmpClient.send_input` refuses
them.)
"""

from __future__ import annotations

import ast
import io
import re
import tokenize
from pathlib import Path

TESTS = Path(__file__).resolve().parents[1]
SCENARIOS = TESTS / "scenarios"
HARNESS = TESTS.parent / "tools" / "harness" / "lumen_harness"
RAW_ATTRS = {"press", "type_text", "send_key"}
MARK = re.compile(r"#\s*unconfirmed:\s*\S")
# Per harness file, the functions (qualified by class) allowed raw paths;
# exactly these must use them, so a stale entry is noticed.
RAW_ALLOWED = {
    "qemu.py": {"Machine.press", "Machine.type_text"},
    "qmp.py": {"QmpClient.send_key"},
    "gui.py": {"confirmed_keys"},
}


def raw_lines(source: str) -> dict[int, str]:
    """The first line of each statement in `source` that uses a raw key
    path, with the innermost function it's in, qualified by its class ("" at
    module level). A function's own body is in it; its decorators,
    defaults and annotations are in the enclosing scope."""
    found: dict[int, str] = {}

    def raw(node: ast.AST) -> bool:
        if isinstance(node, ast.Attribute):
            return node.attr in RAW_ATTRS
        return isinstance(node, ast.Constant) and node.value == "send-key"

    def visit(node: ast.AST, line: int, function: str, cls: str) -> None:
        if isinstance(node, ast.stmt):
            line = node.lineno
        if raw(node):
            found.setdefault(line, function)
        is_def = isinstance(node, ast.FunctionDef | ast.AsyncFunctionDef)
        for child in ast.iter_child_nodes(node):
            if is_def and isinstance(child, ast.stmt):
                visit(child, line, f"{cls}.{node.name}" if cls else node.name, "")
            elif isinstance(node, ast.ClassDef) and isinstance(child, ast.stmt):
                visit(child, line, function, node.name)
            else:
                visit(child, line, function, cls)

    visit(ast.parse(source), 0, "", "")
    return found


def marks(source: str) -> set[int]:
    """The lines of `source` with an `# unconfirmed: <why>` comment (a
    comment token, not text in a string)."""
    tokens = tokenize.generate_tokens(io.StringIO(source).readline)
    return {t.start[0] for t in tokens if t.type == tokenize.COMMENT and MARK.match(t.string)}


def unmarked(source: str) -> list[int]:
    """The raw-key statements of `source` (`raw_lines`) without a mark on
    their line or in the comment-only block directly above them (a code
    line's trailing mark is its own statement's, not the next one's)."""
    lines, marked = source.splitlines(), marks(source)
    bad = []
    for n in sorted(raw_lines(source)):
        top = n
        while top > 1 and lines[top - 2].lstrip().startswith("#"):
            top -= 1
        if marked.isdisjoint(range(top, n + 1)):
            bad.append(n)
    return bad


def test_every_raw_key_in_the_scenarios_says_why():
    files = sorted(SCENARIOS.glob("*.py"))
    assert len(files) >= 15 and SCENARIOS / "test_input.py" in files, f"scanned {files}"
    bad = []
    for path in files:
        source = path.read_text()
        lines = source.splitlines()
        bad += [f"{path.name}:{n}: {lines[n - 1].strip()}" for n in unmarked(source)]
    assert not bad, (
        "raw keys without `# unconfirmed: <why>` (use gui.confirmed_keys):\n" + "\n".join(bad)
    )


def harness_problem(name: str, source: str) -> str | None:
    """Why harness file `name` (`source`) breaks `RAW_ALLOWED`, or None: the
    functions using raw key paths must be exactly the ones listed for it (a
    listed one that no longer does is a stale entry, which would let a later
    function of that name through)."""
    used, allowed = set(raw_lines(source).values()), RAW_ALLOWED.get(name, set())
    if used == allowed:
        return None
    return (
        f"{name}: raw keys in {sorted(used)}, allowed {sorted(allowed)}: type through "
        "gui.confirmed_keys, or (implementing a raw path) list the function in RAW_ALLOWED"
    )


def test_the_harness_types_raw_only_where_it_implements_it():
    files = [*sorted(HARNESS.glob("*.py")), TESTS / "conftest.py"]
    assert HARNESS / "gui.py" in files and all(f.exists() for f in files), f"scanned {files}"
    problems = [p for f in files if (p := harness_problem(f.name, f.read_text()))]
    assert not problems, "\n".join(problems)


def test_a_harness_file_uses_raw_keys_exactly_where_listed():
    gui = "def confirmed_keys(m):\n    m.press(['x'])\n"
    assert harness_problem("gui.py", gui) is None
    assert harness_problem("gui.py", gui + "def helper(m):\n    m.press(['y'])\n"), (
        "an extra function"
    )
    assert harness_problem("gui.py", "def confirmed_keys(m):\n    pass\n"), "a stale entry"
    assert harness_problem("conftest.py", "def boot(m):\n    m.type_text('x')\n"), (
        "none allowed there"
    )


def test_a_raw_key_needs_its_own_mark():
    assert unmarked('m.press(["x"])\n') == [1]
    assert unmarked('m.press(["x"])  # unconfirmed: a race\n') == []
    assert unmarked('# unconfirmed: a race\n# (more of why)\nm.press(["x"])\n') == []
    # A mark covers only the statement right below its comment block.
    assert unmarked('# unconfirmed: a race\nm.press(["x"])\nm.type_text("y")\n') == [3]
    assert unmarked('# unconfirmed: a race\nx = 1\nm.press(["x"])\n') == [3]
    # A code line's trailing mark is its own statement's only.
    assert unmarked('m.press(["a"])  # unconfirmed: a race\nm.press(["b"])\n') == [2]
    assert unmarked('x = 1  # unconfirmed: a race\n# more\nm.press(["b"])\n') == [3]
    assert unmarked("for k in ks:  # unconfirmed: a race\n    m.press([k])\n") == [2]
    assert unmarked('# unconfirmed:\nm.press(["x"])\n') == [2], "a reason is required"
    assert unmarked('# unconfirmed:   \nm.press(["x"])\n') == [2], "spaces aren't a reason"
    assert unmarked('# unconfirmed: a race\n\nm.press(["x"])\n') == [3], (
        "a blank line ends the block"
    )
    assert unmarked('m.send_line("# unconfirmed: no")\nm.press(["x"])\n') == [2], "not in a string"


def test_every_raw_key_path_is_seen():
    for source in (
        'm.send_line("#1"); m.press(["x"])\n',
        "def f(): m.press(['x'])\n",
        "press = m.press\n",
        "keys(map(m.type_text, ['a']))\n",
        'm.qmp().command("send-key", keys=[])\n',
        "m.qmp().send_key(['a'], hold_ms=1)\n",
        "m.press(\n    ['x'],\n)\n",
        'm.qmp().command(name="send-key", keys=[])\n',
        'K = "send-key"\n',
        "class A(B, k=m.press):\n    pass\n",
    ):
        assert unmarked(source) == [1], source
    # Nested in compound statements' bodies, each at its own line.
    assert unmarked("with boot() as m:\n    for k in 'ab':\n        m.press([k])\n") == [3]
    # Only the statement itself, not one nested in its except or case.
    for source in (
        "try:\n    pass\nexcept OSError:\n    m.press(['x'])  # unconfirmed: y\n",
        "match k:\n    case 1:\n        m.press(['x'])  # unconfirmed: y\n",
    ):
        assert unmarked(source) == [], source
    assert unmarked('got.presses\nm.qmp().command("query-pci")\nconfirmed_keys(m, "x")\n') == []
    assert unmarked('"""How send-key works."""\nraise ValueError("send-key needs a key")\n') == []


def test_a_harness_helper_is_judged_by_its_function():
    m_press = "class M:\n    def press(self, q):\n        self.qmp().send_key(q)\n"
    assert raw_lines(m_press) == {3: "M.press"}
    assert raw_lines("def press(m):\n    m.press(['x'])\n") == {2: "press"}, "not Machine's"
    assert raw_lines("def helper(m):\n    def inner():\n        m.press(['x'])\n") == {3: "inner"}
    # Its defaults and decorators are the enclosing scope's.
    assert raw_lines("def helper(m, k=m.press):\n    pass\n") == {1: ""}
    assert raw_lines("@d(m.press)\ndef press():\n    pass\n") == {2: ""}
    assert raw_lines("def press(k=m.press): m.press()\n") == {1: ""}
    assert unmarked("def f(k=m.press):\n    pass\n") == [1]
