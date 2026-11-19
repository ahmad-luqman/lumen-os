"""The apps/ workspace's output (#196): `make build` builds every app into
build/apps/<name>.wasm; each is a valid WASM module (wasmtime validates it,
the later proposals it can switch off switched off) using only rustc's
default WASM features, and hello exports what it says and returns what it
says. Also lumen_harness.wasm's engine and custom-section reader, on
handcrafted modules."""

import re
import subprocess
import tomllib

import pytest
import wasmtime
from lumen_harness import REPO_ROOT, wasm
from lumen_harness.wasm import (
    OFF,
    WasmError,
    compile_module,
    custom_section,
    custom_sections,
    engine,
    function_names,
    target_features,
)

APPS = REPO_ROOT / "build" / "apps"

# What rustc 1.99.1 lets itself use for wasm32-unknown-unknown by default,
# as the modules' target_features section lists it. `rustc --print cfg`
# shows six of them; LLVM's generic CPU also enables bulk-memory-opt and
# call-indirect-overlong, which rustc doesn't list. The kernel's wasmi
# will have to accept every one (#197). A toolchain bump that changes the
# list fails here, so it gets checked against wasmi again.
DEFAULT_FEATURES = {
    "bulk-memory",
    "bulk-memory-opt",
    "call-indirect-overlong",
    "multivalue",
    "mutable-globals",
    "nontrapping-fptoint",
    "reference-types",
    "sign-ext",
}


def app_names() -> list[str]:
    names = sorted(p.parent.name for p in (REPO_ROOT / "apps").glob("*/Cargo.toml"))
    assert names, "apps/ has no apps"
    return names


def built(name: str) -> bytes:
    path = APPS / f"{name}.wasm"
    assert path.exists(), f"{path} is missing: `make build` (or `make test-host`) builds it"
    return path.read_bytes()


def test_every_app_validates_with_the_switchable_later_proposals_off():
    eng = engine()
    for name in app_names():
        compile_module(built(name), eng)


def test_hello_exports_answer_returning_42():
    eng = engine()
    module = compile_module(built("hello"), eng)
    assert module.imports == []
    exports = {e.name: e.type for e in module.exports}
    assert set(exports) == {"memory", "answer"}
    assert isinstance(exports["memory"], wasmtime.MemoryType)
    answer = exports["answer"]
    assert isinstance(answer, wasmtime.FuncType)
    assert (answer.params, answer.results) == ([], [wasmtime.ValType.i32()])
    store = wasmtime.Store(eng)
    instance = wasmtime.Instance(store, module, [])
    assert instance.exports(store)["answer"](store) == 42


def test_every_app_uses_only_the_default_wasm_features():
    for name in app_names():
        features = target_features(built(name))
        assert set(features) == DEFAULT_FEATURES, name
        assert set(features.values()) == {"+"}, name


def test_every_app_keeps_its_function_names():
    # The release profile keeps the name section, so a trap can name its function.
    for name in app_names():
        assert function_names(built(name)), name
    assert "answer" in function_names(built("hello")).values()


def test_every_app_is_named_after_its_directory():
    # `make apps` copies target-apps/.../<dir>.wasm: a crate or [lib] named
    # otherwise would leave that file stale (or missing) while cargo builds
    # another one.
    for name in app_names():
        manifest = tomllib.loads((REPO_ROOT / "apps" / name / "Cargo.toml").read_text())
        assert manifest["package"]["name"] == name
        assert "name" not in manifest.get("lib", {}), name


# Each setting in OFF: a module that needs its proposal, what wasmtime says
# refusing it, and the settings kept as wasmtime has them for the probe.
V128_PARAMS = "(param v128 v128 v128) (result v128) local.get 0 local.get 1 local.get 2"
I64_PARAMS = "(param i64 i64 i64 i64) (result i64 i64)"
PROBES = {
    "wasm_simd": (
        "(module (func (result v128) v128.const i64x2 0 0))",
        "SIMD support is not enabled",
        (),
    ),
    # With SIMD off wasmtime says "SIMD", so SIMD stays on here.
    "wasm_relaxed_simd": (
        f"(module (func {V128_PARAMS} i32x4.relaxed_laneselect))",
        "relaxed SIMD support is not enabled",
        ("wasm_simd",),
    ),
    "wasm_threads": ("(module (memory 1 1 shared))", "threads must be enabled", ()),
    "wasm_tail_call": ("(module (func return_call 0))", "tail calls support is not enabled", ()),
    "wasm_gc": (
        "(module (type (struct (field i32))))",
        "not supported without the gc feature",
        (),
    ),
    "wasm_function_references": (
        "(module (type $f (func)) (func (param (ref $f))))",
        "function references required",
        (),
    ),
    "wasm_exceptions": (
        "(module (tag $t) (func (throw $t)))",
        "exceptions proposal not enabled",
        (),
    ),
    "wasm_memory64": ("(module (memory i64 1))", "memory64 must be enabled", ()),
    "wasm_multi_memory": ("(module (memory 1) (memory 1))", "multiple memories", ()),
    "wasm_wide_arithmetic": (
        f"(module (func {I64_PARAMS} local.get 0 local.get 1 local.get 2 local.get 3 i64.add128))",
        "wide arithmetic support is not enabled",
        (),
    ),
    # Off in wasmtime by default: only the setting check in engine() keeps
    # these two meaning something if a wasmtime turns them on.
    "wasm_custom_page_sizes": (
        "(module (memory 1 (pagesize 1)))",
        "custom page sizes proposal must be enabled",
        (),
    ),
    "wasm_stack_switching": (
        "(module (type $f (func)) (type $c (cont $f)))",
        "stack switching is disabled",
        (),
    ),
}


def test_every_switched_off_proposal_has_a_probe():
    assert set(PROBES) == set(OFF)


@pytest.mark.parametrize("proposal", OFF)
def test_the_engine_refuses_each_later_proposal(proposal):
    wat, message, keep = PROBES[proposal]
    data = wasmtime.wat2wasm(wat)
    eng = engine(keep=keep)
    with pytest.raises(wasmtime.WasmtimeError, match=message):
        compile_module(data, eng)


def test_the_engine_refuses_a_setting_wasmtime_does_not_have(monkeypatch):
    monkeypatch.setattr(wasm, "OFF", (*OFF, "wasm_simd_typo"))
    with pytest.raises(AttributeError, match="wasm_simd_typo"):
        wasm.engine()
    with pytest.raises(ValueError, match="not in OFF"):
        engine(keep=("wasm_extended_const",))


def test_compile_module_takes_only_binary_modules():
    with pytest.raises(WasmError, match="binary"):
        compile_module(b"(module)")


def test_the_engine_accepts_the_default_features():
    wat = """(module (memory 1)
      (func (result i32 i32) i32.const 1 i32.extend8_s i32.const 2)
      (func (memory.fill (i32.const 0) (i32.const 0) (i32.const 0)))
      (func (param f32) (result i32) local.get 0 i32.trunc_sat_f32_s))"""
    compile_module(wasmtime.wat2wasm(wat))


@pytest.mark.parametrize("goal", ["build", "test-host"])
def test_make_goal_builds_the_apps(goal):
    run = subprocess.run(
        ["make", "-n", "-C", str(REPO_ROOT), goal],
        capture_output=True,
        text=True,
        check=False,
    )
    assert run.returncode == 0, run.stderr
    assert "cd apps && cargo build --release" in run.stdout


def test_the_toolchain_file_pins_the_wasm_target():
    toolchain = (REPO_ROOT / "rust-toolchain.toml").read_text()
    assert re.search(r'^targets = \[.*"wasm32-unknown-unknown".*\]$', toolchain, re.MULTILINE)
    # CI installs whatever the file lists: no toolchain named on the command line.
    ci = (REPO_ROOT / ".github/workflows/ci.yml").read_text()
    assert re.search(r"^\s+rustup toolchain install\s*$", ci, re.MULTILINE)


# --- the custom-section reader, on handcrafted modules ---


def uleb(n: int) -> bytes:
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        out.append(b | (0x80 if n else 0))
        if not n:
            return bytes(out)


def name(s: str) -> bytes:
    return uleb(len(s)) + s.encode()


def section(sid: int, body: bytes) -> bytes:
    return bytes([sid]) + uleb(len(body)) + body


def custom(cname: str, payload: bytes) -> bytes:
    return section(0, name(cname) + payload)


HEADER = b"\0asm\x01\0\0\0"
# A type and a function section between custom sections: skipped, the order kept.
TYPED = section(1, b"\x01\x60\x00\x00") + section(3, b"\x01\x00")


def features(*entries: bytes) -> bytes:
    return custom("target_features", uleb(len(entries)) + b"".join(entries))


def test_reader_keeps_custom_sections_in_order():
    long = "x" * 200  # a 2-byte LEB128 length
    data = HEADER + custom("b", b"\x01") + TYPED + custom(long, b"") + custom("b", b"\x02")
    assert custom_sections(data) == [("b", b"\x01"), (long, b""), ("b", b"\x02")]
    assert custom_section(data, long) == b""
    assert custom_section(data, "absent") is None


def test_reader_refuses_a_custom_section_named_twice():
    data = HEADER + custom("b", b"\x01") + TYPED + custom("b", b"\x02")
    with pytest.raises(WasmError, match="appears 2 times"):
        custom_section(data, "b")


def test_target_features_reads_each_prefix():
    data = HEADER + features(b"+" + name("sign-ext"), b"-" + name("simd128"))
    assert target_features(data) == {"sign-ext": "+", "simd128": "-"}
    assert target_features(HEADER + TYPED) == {}


@pytest.mark.parametrize(
    "data, message",
    [
        (HEADER + features(b"+" + name("a"), b"+" + name("a")), "listed twice"),
        (HEADER + features(b"?" + name("a")), "prefix"),
        (HEADER + features(b"=" + name("a")), "prefix"),
        (HEADER + custom("target_features", b"\x01+\x05ab"), "truncated"),
        (HEADER + custom("target_features", b"\x00\x00"), "left over"),
        (HEADER + features() + features(), "appears 2 times"),
    ],
)
def test_target_features_refuses_malformed_sections(data, message):
    with pytest.raises(WasmError, match=message):
        target_features(data)


def test_function_names_reads_the_name_section():
    # Subsection 0 (the module's name) skipped; 1: function 0 "f", 3 "g".
    names = section(0, name("m")) + section(1, b"\x02\x00" + name("f") + b"\x03" + name("g"))
    assert function_names(HEADER + custom("name", names)) == {0: "f", 3: "g"}
    assert function_names(HEADER) == {}


@pytest.mark.parametrize(
    "names, message",
    [
        (section(1, b"\x00") + section(1, b"\x00"), "subsection 1 appears twice"),
        (section(1, b"\x02\x03" + name("f") + b"\x03" + name("g")), "index 3 after 3"),
        (section(1, b"\x02\x03" + name("f") + b"\x01" + name("g")), "index 1 after 3"),
        (section(1, b"\x01\x00" + name("f") + b"\x00"), "1 bytes left over"),
    ],
)
def test_function_names_refuses_malformed_sections(names, message):
    with pytest.raises(WasmError, match=message):
        function_names(HEADER + custom("name", names))


@pytest.mark.parametrize(
    "data, message",
    [
        (b"\0asm\x02\0\0\0", "not a version 1"),
        (HEADER + b"\x01\x05\x01", "truncated"),
        (HEADER + section(0, b"\x02\xff\xfe"), "not UTF-8"),
        (HEADER + b"\x00\xff\xff\xff\xff\x7f", "doesn't fit 32 bits"),
        (HEADER + b"\x00\xff\xff\xff\xff\xff\x01", "longer than 5 bytes"),
    ],
)
def test_reader_refuses_broken_headers(data, message):
    with pytest.raises(WasmError, match=message):
        custom_sections(data)
