"""Every key a scenario types is a confirmed step (#128), or says why not.

Keys typed with `Machine.press`, `Machine.type_text` or QMP's `send-key`
go to QEMU and return; a key lost under load then shows only as a wrong
screen. Where #128's keys were lost wasn't found; the likely place is the
keyboard's own queue, which only the guest drains, filling while QEMU sends
keys to a stalled guest. `lumen_harness.gui.confirmed_keys` sends the next
key only once the guest counted the last (and fails naming a lost one).

So in tests/scenarios, each statement that uses a raw key path (calls or
names `.press`, `.type_text` or `.send_key`, or sends QMP's `"send-key"`)
needs an `# unconfirmed: <why>` comment of its own: on its line, or in the
comment block directly above it. The reasons: a race on purpose, no serial
shell free to run `kbd`, or an effect read next that a confirmed step's
`kbd` would read past. In tools/harness/lumen_harness only the functions
that implement those paths may use them (`RAW_ALLOWED`), so a helper
scenarios call can't type raw unseen. (Keys sent as `input-send-event`
key events aren't looked for: nothing sends any.)
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
# The harness functions that implement the raw paths, by file.
RAW_ALLOWED = {
    "qemu.py": {"press", "type_text"},
    "qmp.py": {"send_key"},
    "gui.py": {"confirmed_keys"},
}


def raw_lines(source: str) -> dict[int, str]:
    """The first line of each statement in `source` that uses a raw key
    path, with the innermost function it's in ("" at module level)."""
    found: dict[int, str] = {}

    def raw(node: ast.AST) -> bool:
        if isinstance(node, ast.Attribute) and node.attr in RAW_ATTRS:
            return True
        return (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Attribute)
            and node.func.attr == "command"
            and bool(node.args)
            and isinstance(node.args[0], ast.Constant)
            and node.args[0].value == "send-key"
        )

    def visit(node: ast.AST, function: str) -> None:
        for child in ast.iter_child_nodes(node):
            if isinstance(child, ast.FunctionDef | ast.AsyncFunctionDef):
                # Its decorators and defaults belong to the enclosing scope.
                outer = [*child.decorator_list, *child.args.defaults, *child.args.kw_defaults]
                if any(raw(n) for expr in outer if expr is not None for n in ast.walk(expr)):
                    found.setdefault(child.lineno, function)
                visit(child, child.name)
            elif isinstance(child, ast.stmt) and not isinstance(child, ast.ClassDef):
                own = [n for n in ast.iter_child_nodes(child) if not isinstance(n, ast.stmt)]
                if any(raw(n) for expr in own for n in ast.walk(expr)):
                    found.setdefault(child.lineno, function)
                visit(child, function)
            else:
                visit(child, function)

    visit(ast.parse(source), "")
    return found


def marks(source: str) -> set[int]:
    """The lines of `source` with an `# unconfirmed: <why>` comment (a
    comment token, not text in a string)."""
    tokens = tokenize.generate_tokens(io.StringIO(source).readline)
    return {t.start[0] for t in tokens if t.type == tokenize.COMMENT and MARK.match(t.string)}


def unmarked(source: str) -> list[int]:
    """The raw-key statements of `source` (`raw_lines`) without a mark on
    their line or in the comment block directly above them."""
    lines, marked = source.splitlines(), marks(source)
    bad = []
    for n in sorted(raw_lines(source)):
        above = n - 1
        while above >= 1 and above not in marked and lines[above - 1].lstrip().startswith("#"):
            above -= 1
        if n not in marked and above not in marked:
            bad.append(n)
    return bad


def test_every_raw_key_in_the_scenarios_says_why():
    files = sorted(SCENARIOS.glob("test_*.py"))
    assert len(files) >= 15 and SCENARIOS / "test_input.py" in files, f"scanned {files}"
    bad = []
    for path in files:
        source = path.read_text()
        lines = source.splitlines()
        bad += [f"{path.name}:{n}: {lines[n - 1].strip()}" for n in unmarked(source)]
    assert not bad, (
        "raw keys without `# unconfirmed: <why>` (use gui.confirmed_keys):\n" + "\n".join(bad)
    )


def test_the_harness_types_raw_only_where_it_implements_it():
    files = sorted(HARNESS.glob("*.py"))
    assert HARNESS / "gui.py" in files, f"scanned {files}"
    bad = [
        f"{path.name}:{n} (in {function or 'module'})"
        for path in files
        for n, function in raw_lines(path.read_text()).items()
        if function not in RAW_ALLOWED.get(path.name, set())
    ]
    assert not bad, "raw keys in a harness helper scenarios call:\n" + "\n".join(bad)


def test_a_raw_key_needs_its_own_mark():
    assert unmarked('m.press(["x"])\n') == [1]
    assert unmarked('m.press(["x"])  # unconfirmed: a race\n') == []
    assert unmarked('# unconfirmed: a race\n# (more of why)\nm.press(["x"])\n') == []
    # A mark covers only the statement right below its comment block.
    assert unmarked('# unconfirmed: a race\nm.press(["x"])\nm.type_text("y")\n') == [3]
    assert unmarked('# unconfirmed: a race\nx = 1\nm.press(["x"])\n') == [3]
    assert unmarked('# unconfirmed:\nm.press(["x"])\n') == [2], "a reason is required"
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
    ):
        assert unmarked(source) == [1], source
    assert unmarked('got.presses\nm.qmp().command("query-pci")\nconfirmed_keys(m, "x")\n') == []


def test_a_harness_helper_is_judged_by_its_function():
    assert raw_lines("class M:\n    def press(self, q):\n        self.qmp().send_key(q)\n") == {
        3: "press"
    }
    assert raw_lines("def helper(m):\n    m.press(['x'])\n") == {2: "helper"}
