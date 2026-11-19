"""The apps/ workspace's output (#196): `make build` builds every app into
build/apps/<name>.wasm; each is a valid WASM module (wasmtime validates it)
using only rustc's default WASM features, and hello exports what it says
and returns what it says. Also lumen_harness.wasm's custom-section reader,
on handcrafted modules."""

import re
import subprocess

import pytest
import wasmtime
from lumen_harness import REPO_ROOT
from lumen_harness.wasm import (
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
# shows six of them; LLVM adds bulk-memory-opt (implied by bulk-memory) and
# call-indirect-overlong (implied by reference-types). The kernel's wasmi
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


def test_every_app_is_a_valid_module_without_later_proposals():
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


@pytest.mark.parametrize(
    "wat",
    [
        "(module (func (result v128) v128.const i64x2 0 0))",
        "(module (memory 1 1 shared))",
        "(module (func return_call 0))",
        "(module (memory i64 1))",
    ],
    ids=["simd", "threads", "tail-call", "memory64"],
)
def test_the_engine_refuses_later_proposals(wat):
    with pytest.raises(wasmtime.WasmtimeError):
        compile_module(wasmtime.wat2wasm(wat))


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
# A function section between two custom sections: skipped, the order kept.
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
    data = HEADER + features(
        b"+" + name("sign-ext"), b"-" + name("simd128"), b"=" + name("atomics")
    )
    assert target_features(data) == {"sign-ext": "+", "simd128": "-", "atomics": "="}
    assert target_features(HEADER + TYPED) == {}


@pytest.mark.parametrize(
    "data, message",
    [
        (HEADER + features(b"+" + name("a"), b"+" + name("a")), "listed twice"),
        (HEADER + features(b"?" + name("a")), "prefix"),
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
