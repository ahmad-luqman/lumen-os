"""Every thread name written in the kernel's source fits in a thread's name
(#276).

A thread's name is a `lumen_trace::ThreadName`: 15 bytes inline, like
Linux's `comm`. A longer one is cut where it is made, and nothing says so:
`threads`, `top` and a crash report then show a name the source doesn't
have, and a scenario looking for the whole name (`"x" not in
thread_names(m)`) passes whatever runs. The cut is for names from outside
the source (a path given to `linux /path`), so each string literal passed
as a name to the spawn functions (and the kernel's wrappers that pass a
name on) must be at most 15 bytes, with no character `ThreadName::new`
would replace.

Names held in variables (the desktop's demo table, the shells' `shell-vt<n>`,
an app's `app:<name>`, which `kernel/build.rs` keeps to 11 bytes of name)
aren't literals at a call and aren't checked here.
"""

from __future__ import annotations

import re
from pathlib import Path

from lumen_harness import REPO_ROOT

KERNEL = REPO_ROOT / "kernel" / "src"
MAX_LEN = 15  # lumen_trace::ThreadName::MAX_LEN
# Each function taking a thread's name, and which argument it is.
NAME_ARG = {
    "spawn": 0,
    "run_sized": 0,
    "run_elf_process": 0,
    "hold_with": 0,
    "adopted": 0,
    "spawned": 0,
    "spawn_on": 1,
    "spawn_user_on": 1,
    "spawn_test": 1,
    "spawn_ring3_on": 1,
}
CALL = re.compile(r"\b(" + "|".join(NAME_ARG) + r")\s*\(")
# A name argument written as a literal: `"x"`, `"x".into()` or
# `ThreadName::new("x")`.
LITERAL = re.compile(
    r'"((?:[^"\\]|\\.)*)"(?:\.into\(\))?|ThreadName::(?:new|from)\("((?:[^"\\]|\\.)*)"\)'
)
# What `ThreadName::new` replaces: C0, DEL, C1, U+2028, U+2029.
REPLACED = re.compile("[\x00-\x1f\x7f-\x9f\u2028\u2029]")


def strip_comments(src: str) -> str:
    """`src` with its `//` comments blanked (strings and char literals
    kept; line breaks kept, so line numbers hold)."""
    out = []
    i = 0
    while i < len(src):
        c = src[i]
        if src.startswith("//", i):
            end = src.find("\n", i)
            end = len(src) if end < 0 else end
            out.append(" " * (end - i))
            i = end
        elif c == '"':
            j = i + 1
            while j < len(src) and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            out.append(src[i : j + 1])
            i = j + 1
        elif c == "'" and (
            m := re.match(r"'(?:\\.|\\u\{[0-9a-fA-F]+\}|\\x[0-9a-fA-F]{2}|[^\\'])'", src[i:])
        ):
            out.append(m.group(0))
            i += len(m.group(0))
        else:
            out.append(c)
            i += 1
    return "".join(out)


def arguments(code: str, start: int) -> list[str]:
    """The top-level arguments of the call whose `(` is just before
    `start` (strings and char literals skipped over)."""
    args, depth, i, begin = [], 0, start, start
    while i < len(code):
        c = code[i]
        if c == '"':
            i += 1
            while i < len(code) and code[i] != '"':
                i += 2 if code[i] == "\\" else 1
        elif c == "'" and (m := re.match(r"'(?:\\.|[^\\'])'", code[i:])):
            i += len(m.group(0)) - 1
        elif c in "([{":
            depth += 1
        elif c in ")]}":
            if depth == 0:
                args.append(code[begin:i])
                return [a.strip() for a in args]
            depth -= 1
        elif c == "," and depth == 0:
            args.append(code[begin:i])
            begin = i + 1
        i += 1
    return [a.strip() for a in args]


def literal_names(src: str) -> list[tuple[int, str]]:
    """Each (line, name) passed as a literal name in `src`."""
    code = strip_comments(src)
    found = []
    for m in CALL.finditer(code):
        if re.search(r"\bfn\s+$", code[: m.start()]):
            continue  # the function's definition
        args = arguments(code, m.end())
        idx = NAME_ARG[m.group(1)]
        if idx < len(args) and (lit := LITERAL.fullmatch(args[idx])):
            name = lit.group(1) if lit.group(1) is not None else lit.group(2)
            found.append((code.count("\n", 0, m.start()) + 1, name))
    return found


def unfit(name: str) -> bool:
    """Over 15 bytes, or holding what `new` replaces; a name written with
    an escape (`\\n`) counts as one (none needs one)."""
    return len(name.encode()) > MAX_LEN or bool(REPLACED.search(name)) or "\\" in name


def kernel_names() -> list[tuple[Path, int, str]]:
    return [
        (path, line, name)
        for path in sorted(KERNEL.rglob("*.rs"))
        for line, name in literal_names(path.read_text())
    ]


def test_every_literal_thread_name_in_the_kernel_fits():
    names = kernel_names()
    bad = [f"{p.relative_to(REPO_ROOT)}:{line}: {name!r}" for p, line, name in names if unfit(name)]
    assert not bad, (
        f"thread names over {MAX_LEN} bytes or with a replaced character:\n" + "\n".join(bad)
    )
    # The scan sees the kernel's names (it isn't checking nothing): the
    # adopted and spawned ones, and those through the wrappers.
    seen = {name for _, _, name in names}
    assert {
        "main",
        "idle",
        "exe-route",
        "ktest-process",
        "hold-ktest-fail",
        "desk-mandelbrot",
    } <= seen, seen
    assert len(names) >= 50, len(names)


def test_the_scan_reads_each_form_and_refuses_what_wont_fit():
    src = """
        fn spawn(name: impl Into<ThreadName>, f: F) {}
        let a = sched::spawn("short", move || work(',', "x,y"));
        // sched::spawn("a-commented-out-name-too-long", || {});
        let b = crate::sched::spawn_on(cpu_for(id, (1, 2)), "on-a-cpu", move || {});
        let c = Thread::spawned("idle".into(), 0, STACK_SIZE, f, None);
        let d = scope.spawn_on(
            cpu,
            ThreadName::new("sixteen-bytes-xx"),
            || {},
        );
        let e = spawn(name, || {});
        let f = spawn_ring3_on(cpu, "nl\\n", space);
        let g = hold_with('"', "x");
    """
    found = literal_names(src)
    assert found == [
        (3, "short"),
        (5, "on-a-cpu"),
        (6, "idle"),
        (7, "sixteen-bytes-xx"),
        (13, "nl\\n"),
    ], found
    assert [unfit(n) for _, n in found] == [False, False, False, True, True]
    assert unfit("a\u2028b") and unfit("a\x85") and unfit("a\x1fb") and not unfit("my prog")
    assert not unfit("exactly-15-byte") and unfit("exactly-16-bytes")
