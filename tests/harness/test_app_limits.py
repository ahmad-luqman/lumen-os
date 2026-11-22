"""`lumen_harness.app_limits` agrees with `lumen_app_api::limits` (#278):
`MAX_APPS`, `KERNEL_RESERVE` and `APP_WORST` (the last as limits.rs's own
test pins it), read from limits.rs with its own `KIB` and `MIB`; and
`slots` at its edges."""

from __future__ import annotations

import ast
import operator
import re

import pytest
from lumen_harness import REPO_ROOT, app_limits
from lumen_harness.app_limits import APP_WORST, KERNEL_RESERVE, MAX_APPS, SLOTS_NEED, slots

LIMITS = REPO_ROOT / "crates" / "lumen-app-api" / "src" / "limits.rs"
OPS = {ast.Add: operator.add, ast.Mult: operator.mul}
FRAME = 4096


def value(expr: str, names: dict[str, int]) -> int:
    """An integer expression of literals (`_` between digits allowed), `+`,
    `*` and `names`, as Rust writes these."""
    plain = re.sub(r"(?<=\d)_(?=\d)", "", expr)

    def walk(node: ast.expr) -> int:
        if isinstance(node, ast.Constant) and type(node.value) is int:
            return node.value
        if isinstance(node, ast.Name) and node.id in names:
            return names[node.id]
        if isinstance(node, ast.BinOp) and type(node.op) in OPS:
            return OPS[type(node.op)](walk(node.left), walk(node.right))
        raise AssertionError(f"not a plain integer expression: {expr!r}")

    try:
        tree = ast.parse(plain, mode="eval")
    except SyntaxError:
        raise AssertionError(f"not a plain integer expression: {expr!r}") from None
    return walk(tree.body)


def rust_limits(text: str) -> dict[str, int]:
    """The three limits from limits.rs's text, its `KIB` and `MIB` first."""
    names: dict[str, int] = {}

    def const(name: str, public: bool) -> int:
        vis = "pub " if public else ""
        m = re.search(rf"^{vis}const {name}: \w+ = ([^;]+);$", text, re.MULTILINE)
        assert m, f"no `{vis}const {name}` in {LIMITS}"
        return value(m[1], names)

    names["KIB"] = const("KIB", False)
    names["MIB"] = const("MIB", False)
    found = {name: const(name, True) for name in ("MAX_APPS", "KERNEL_RESERVE")}
    # APP_WORST is a sum of other constants; its test pins it in numbers.
    m = re.search(r"assert_eq!\(APP_WORST, ([^;]+)\);", text)
    assert m, f"no `assert_eq!(APP_WORST, ...)` in {LIMITS}"
    found["APP_WORST"] = value(m[1], names)
    return found


def harness_limits() -> dict[str, int]:
    return {
        "MAX_APPS": app_limits.MAX_APPS,
        "KERNEL_RESERVE": app_limits.KERNEL_RESERVE,
        "APP_WORST": app_limits.APP_WORST,
    }


def test_the_harness_copies_the_app_limits_exactly():
    assert harness_limits() == rust_limits(LIMITS.read_text())


def test_slots_at_their_edges():
    # Four slots from 39,440 frames (154.06 MiB), three a frame short.
    assert SLOTS_NEED == KERNEL_RESERVE + MAX_APPS * APP_WORST == 39_440 * FRAME
    assert slots(39_440 * FRAME) == MAX_APPS
    assert slots(39_439 * FRAME) == MAX_APPS - 1
    assert slots(SLOTS_NEED - 1) == MAX_APPS - 1
    assert slots(KERNEL_RESERVE + APP_WORST) == 1
    assert slots(KERNEL_RESERVE + APP_WORST - 1) == 0
    assert slots(0) == 0
    assert slots(2**64 - 1) == MAX_APPS


def test_rust_limits_reads_limits_rs_own_units():
    text = """const KIB: u64 = 1000;
const MIB: u64 = 1000 * KIB;
pub const MAX_APPS: usize = 4;
pub const KERNEL_RESERVE: u64 = 32 * MIB;
        assert_eq!(APP_WORST, 27 * MIB + 3 * 640 * 480 * 4);
"""
    got = rust_limits(text)
    assert got["KERNEL_RESERVE"] == 32_000_000, got
    assert got != harness_limits()
    with pytest.raises(AssertionError, match="no `const MIB`"):
        rust_limits(text.replace("const MIB", "const MEBI"))


def test_value_takes_only_plain_integer_expressions():
    units = {"KIB": 1024, "MIB": 1024 * 1024}
    assert value("27 * MIB + 3 * 640 * 480 * 4", units) == 27 * 2**20 + 3 * 640 * 480 * 4
    assert value("1_000", {}) == 1000
    for bad in ("MIB - 1", "PAGE_BYTES * 2", "f(1)", "1.5", "MIB as u64", "MIB / 2", "MI_B"):
        with pytest.raises(AssertionError, match="not a plain integer expression"):
            value(bad, units)
