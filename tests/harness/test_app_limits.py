"""The app limits the max-apps scenario copies (#278) agree with
`lumen_app_api::limits`: `MAX_APPS`, `KERNEL_RESERVE` and `APP_WORST`, the
last as `limits.rs`'s own test pins it."""

from __future__ import annotations

import ast
import operator
import re
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
LIMITS = ROOT / "crates" / "lumen-app-api" / "src" / "limits.rs"
SCENARIO = ROOT / "tests" / "scenarios" / "test_apps.py"
UNITS = {"KIB": 1024, "MIB": 1024 * 1024}
COPIED = ("MAX_APPS", "KERNEL_RESERVE", "APP_WORST")
OPS = {ast.Add: operator.add, ast.Mult: operator.mul}


def value(expr: str, names: dict[str, int]) -> int:
    """An integer expression of literals, `+`, `*` and `names` (Rust's or
    Python's: the same syntax for these)."""

    def walk(node: ast.expr) -> int:
        if isinstance(node, ast.Constant) and type(node.value) is int:
            return node.value
        if isinstance(node, ast.Name) and node.id in names:
            return names[node.id]
        if isinstance(node, ast.BinOp) and type(node.op) in OPS:
            return OPS[type(node.op)](walk(node.left), walk(node.right))
        raise AssertionError(f"not a plain integer expression: {expr!r}")

    return walk(ast.parse(expr.replace("_", ""), mode="eval").body)


def rust_limits() -> dict[str, int]:
    text = LIMITS.read_text()
    found = {}
    for name in ("MAX_APPS", "KERNEL_RESERVE"):
        m = re.search(rf"^pub const {name}: \w+ = ([^;]+);$", text, re.MULTILINE)
        assert m, f"no `pub const {name}` in {LIMITS}"
        found[name] = value(m[1], UNITS)
    # APP_WORST is a sum of other constants; its test pins it in numbers.
    m = re.search(r"assert_eq!\(APP_WORST, ([^;]+)\);", text)
    assert m, f"no `assert_eq!(APP_WORST, ...)` in {LIMITS}"
    found["APP_WORST"] = value(m[1], UNITS)
    return found


def scenario_limits() -> dict[str, int]:
    names = dict(UNITS)
    for node in ast.parse(SCENARIO.read_text()).body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1:
            target = node.targets[0]
            if isinstance(target, ast.Name) and target.id in {"MIB", *COPIED}:
                names[target.id] = value(ast.unparse(node.value), names)
    return {k: names[k] for k in COPIED if k in names}


def test_the_scenario_copies_the_app_limits_exactly():
    assert scenario_limits() == rust_limits()


def test_value_takes_only_plain_integer_expressions():
    assert value("27 * MIB + 3 * 640 * 480 * 4", UNITS) == 27 * 2**20 + 3 * 640 * 480 * 4
    assert value("1_000", {}) == 1000
    for bad in ("MIB - 1", "PAGE_BYTES * 2", "f(1)", "1.5"):
        with pytest.raises(AssertionError, match="not a plain integer expression"):
            value(bad, UNITS)
