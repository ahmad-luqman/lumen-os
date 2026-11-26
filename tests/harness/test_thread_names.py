"""Every thread name written as a literal at a call that names a thread fits
in a thread's name (#276).

A thread's name is a `lumen_trace::ThreadName`: 16 bytes inline, at most 15
of them the name, like Linux's `comm`. A longer one is cut where it is
made, and nothing says so: `threads`, `top` and a crash report then show a
name the source doesn't have, and a scenario looking for the whole name
(`"x" not in thread_names(m)`) passes whatever runs. The cut is for names
from outside the source (a path given to `linux /path`), so every string
literal in the name argument of a function that names a thread
(`NAME_ARG`: the spawn functions and the kernel's wrappers that pass a name
on) must be at most 15 bytes, with no character `ThreadName::new` would
replace and no escape.

It fails closed: a name argument holding no literal must be one of the
variable forms in `VARIABLES` (each a name passed on, or one taken from a
table), and every function in kernel/src with a `ThreadName` parameter must
be in `NAME_ARG` (or `NOT_NAMING`, saying why). Names kept in a table or a
variable (the desktop's demo table, the shells' `shell-vt<n>`, `spawn_test`'s
lists) aren't read here: `From<&str>`, the spawn functions' conversion,
refuses one that wouldn't fit with a debug assertion (the kernel keeps
them), and an app's `app:<name>` is kept to 11 bytes of name by
`kernel/build.rs`.
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
    "spawn_migratable": 0,
    "run_sized": 0,
    "run_elf_process": 0,
    "hold_with": 0,
    "adopted": 0,
    "spawned": 0,
    "spawn_on": 1,
    "spawn_user_on": 1,
    "spawn_user_pinned_on": 1,
    "spawn_fork_child": 0,
    "spawn_test": 1,
    "spawn_test_migratable": 0,
    "spawn_ring3_on": 1,
    "spawn_with": 1,
    "spawn_configured": 1,
    # A boot module's name for its thread (`ThreadName::new(module_name)`).
    "run_exec": 1,
    "run_spinning": 1,
}
# Functions with a `ThreadName` parameter that aren't in `NAME_ARG`, and why.
NOT_NAMING = {
    # `Thread::new`: only `spawned` and `adopted` call it, with the name
    # they were given.
    "new": "sched.rs's Thread::new, called by spawned and adopted",
    # `execve`'s name for the running thread, from its path (`from_path`
    # cuts it), never a literal.
    "set_current_name": "sched.rs's Thread::set_exec_name, called by execve with ThreadName::from_path",
}
# A name argument holding no literal: a name passed on by a wrapper, or one
# taken from a table (checked when converted: `From<&str>`'s assertion).
VARIABLES = re.compile(
    r"name|name\.into\(\)|thread|thread_name|module_name|app\.thread"
    # procfs's `spawn` makes an inode, not a thread.
    r"|ProcKind::\w+(?:\([^()]*\))?"
)
CALL = re.compile(r"\b(" + "|".join(NAME_ARG) + r")\s*\(")
FN = re.compile(r"\bfn\s+(\w+)\s*(?:<[^{};]*?>)?\s*\(")
# A string literal, raw (`r"x"`, `r#"x"#`) or not, its text in group 2 or 3.
STRING = re.compile(r'r(#*)"(.*?)"\1|"((?:[^"\\]|\\.)*)"', re.DOTALL)
MACRO = re.compile(r"\b\w+!\s*[(\[{]")
CHAR = re.compile(r"'(?:\\u\{[0-9a-fA-F]+\}|\\x[0-9a-fA-F]{2}|\\.|[^\\'])'")
# What `ThreadName::new` replaces: C0, DEL, C1, U+2028, U+2029.
REPLACED = re.compile("[\x00-\x1f\x7f-\x9f  ]")


def literal_at(src: str, i: int) -> re.Match[str] | None:
    """The string or char literal starting at `i`, if one does (`r` only
    where it doesn't end an identifier)."""
    if src[i] == '"' or (
        src[i] == "r" and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_"))
    ):
        return STRING.match(src, i)
    if src[i] == "'":
        return CHAR.match(src, i)
    return None


def strip_comments(src: str) -> str:
    """`src` with its `//` comments blanked (literals kept; line breaks
    kept, so line numbers hold)."""
    out = []
    i = 0
    while i < len(src):
        if src.startswith("//", i):
            end = src.find("\n", i)
            end = len(src) if end < 0 else end
            out.append(" " * (end - i))
            i = end
        elif m := literal_at(src, i):
            out.append(m.group(0))
            i = m.end()
        else:
            out.append(src[i])
            i += 1
    return "".join(out)


def arguments(code: str, start: int) -> list[str]:
    """The top-level arguments of the call whose `(` is just before
    `start` (literals skipped over)."""
    args, depth, i, begin = [], 0, start, start
    while i < len(code):
        if m := literal_at(code, i):
            i = m.end()
            continue
        c = code[i]
        if c in "([{":
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


def scan(src: str) -> tuple[list[tuple[int, str]], list[tuple[int, str]]]:
    """In `src`: each (line, name) written as a literal in a name argument,
    and each (line, argument) holding no literal that isn't a known
    variable form (a name the lint can't read)."""
    code = strip_comments(src)
    names, unread = [], []
    for m in CALL.finditer(code):
        if re.search(r"\bfn\s+$", code[: m.start()]):
            continue  # the function's definition
        args = arguments(code, m.end())
        idx = NAME_ARG[m.group(1)]
        if idx >= len(args):
            continue  # not this function (a shorter call of the same name)
        line = code.count("\n", 0, m.start()) + 1
        if MACRO.search(args[idx]):
            unread.append((line, args[idx]))  # `concat!`, `format!`: built, not written
            continue
        found = [
            s.group(2) if s.group(2) is not None else s.group(3) for s in STRING.finditer(args[idx])
        ]
        names += [(line, name) for name in found]
        if not found and not VARIABLES.fullmatch(args[idx]):
            unread.append((line, args[idx]))
    return names, unread


def naming_functions(src: str) -> list[str]:
    """Each function in `src` with a `ThreadName` parameter."""
    code = strip_comments(src)
    return [
        m.group(1) for m in FN.finditer(code) if "ThreadName" in ",".join(arguments(code, m.end()))
    ]


def unfit(name: str) -> bool:
    """Over 15 bytes, or holding what `new` replaces; a name written with
    an escape (`\\n`) counts as one (none needs one)."""
    return len(name.encode()) > MAX_LEN or bool(REPLACED.search(name)) or "\\" in name


def rel(path: Path) -> str:
    return str(path.relative_to(REPO_ROOT))


def test_every_literal_thread_name_in_the_kernel_fits():
    names, unread = [], []
    for path in sorted(KERNEL.rglob("*.rs")):
        found, odd = scan(path.read_text())
        names += [(path, line, name) for line, name in found]
        unread += [f"{rel(path)}:{line}: {arg}" for line, arg in odd]
    bad = [f"{rel(p)}:{line}: {name!r}" for p, line, name in names if unfit(name)]
    assert not bad, (
        f"thread names over {MAX_LEN} bytes or with a replaced character:\n" + "\n".join(bad)
    )
    assert not unread, (
        "name arguments the lint can't read (add the form to VARIABLES?):\n" + "\n".join(unread)
    )
    # The scan sees the kernel's names (it isn't checking nothing): the
    # adopted and spawned ones, and those through the wrappers.
    seen = {name for _, _, name in names}
    assert {
        "main",
        "idle",
        "exe-route",
        "ktest-process",
        "ktest-module",
        "hold-ktest-fail",
        "desk-mandelbrot",
    } <= seen, seen
    assert len(names) >= 50, len(names)


def test_every_function_taking_a_thread_name_is_scanned():
    missing = [
        f"{rel(path)}: fn {name}"
        for path in sorted(KERNEL.rglob("*.rs"))
        for name in naming_functions(path.read_text())
        if name not in NAME_ARG and name not in NOT_NAMING
    ]
    assert not missing, "functions taking a ThreadName that NAME_ARG doesn't list:\n" + "\n".join(
        missing
    )


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
        let h = spawn(
            "line-broken-into"
                .into(),
            f,
        );
        let i = spawn(lumen_trace::ThreadName::new(
            "qualified-and-broken",
        ), f);
        let j = spawn(r"raw-string-name", f);
        let k = spawn(r#"raw "quoted" x"#, f);
        let l = spawn(NAME, f);
        let m = spawn(concat!("a", "b"), f);
        let n = run_spinning(exec, "ktest-module", "x");
        let o = self.spawn(ProcKind::PidDir(pid));
    """
    names, unread = scan(src)
    assert names == [
        (3, "short"),
        (5, "on-a-cpu"),
        (6, "idle"),
        (7, "sixteen-bytes-xx"),
        (13, "nl\\n"),
        (15, "line-broken-into"),
        (20, "qualified-and-broken"),
        (23, "raw-string-name"),
        (24, 'raw "quoted" x'),
        (27, "ktest-module"),
    ], names
    # A char literal, a const and a macro: read as neither a literal nor a
    # known variable, so the lint fails on them rather than passing them by.
    assert unread == [(14, "'\"'"), (25, "NAME"), (26, 'concat!("a", "b")')], unread
    fits = [not unfit(n) for _, n in names]
    assert fits == [True, True, True, False, False, False, False, True, True, True], fits
    assert unfit("a b") and unfit("a\x85") and unfit("a\x1fb") and not unfit("my prog")
    assert not unfit("exactly-15-byte") and unfit("exactly-16-bytes")
    assert naming_functions(src) == ["spawn"]
    assert naming_functions("fn running_here() -> Option<(u64, ThreadName)> {}") == []
    assert naming_functions("pub fn spawn_on<F: Fn()>(cpu: usize, name: ThreadName) {}") == [
        "spawn_on"
    ]
