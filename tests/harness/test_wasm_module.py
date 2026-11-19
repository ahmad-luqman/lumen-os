"""The apps/ workspace's output (#196): `make build` builds every app into
build/apps/<name>.wasm, and each is a WASM module that exports what it says.
Also the parser itself (lumen_harness.wasm) on handcrafted modules."""

import re
import subprocess

import pytest
from lumen_harness import REPO_ROOT
from lumen_harness.wasm import Export, Signature, WasmError, parse

APPS = REPO_ROOT / "build" / "apps"

# What rustc 1.99.1 lets itself use for wasm32-unknown-unknown by default:
# its target_features section (bulk-memory-opt and call-indirect-overlong
# are LLVM's names for parts of bulk-memory and reference-types). The
# kernel's wasmi must accept every one (#197); a toolchain bump that changes
# this list fails here, so it gets checked against wasmi again.
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


def built(name: str) -> bytes:
    path = APPS / f"{name}.wasm"
    assert path.exists(), f"{path} is missing: `make build` (or `make test-host`) builds it"
    return path.read_bytes()


def test_every_app_is_built():
    crates = {p.parent.name for p in (REPO_ROOT / "apps").glob("*/Cargo.toml")}
    assert crates, "apps/ has no apps"
    for name in crates:
        parse(built(name))


def test_hello_exports_answer_returning_i32():
    module = parse(built("hello"))
    assert module.imports == []
    assert set(module.exports) == {"memory", "answer"}
    assert module.exports["memory"].kind == "memory"
    assert module.signature("answer") == Signature(params=(), results=("i32",))


def test_apps_use_only_the_default_wasm_features():
    features = parse(built("hello")).target_features()
    assert set(features) == DEFAULT_FEATURES
    assert set(features.values()) == {"+"}


def test_make_build_builds_the_apps():
    run = subprocess.run(
        ["make", "-n", "-C", str(REPO_ROOT), "build", "test-host"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert run.returncode == 0, run.stderr
    # Once for `build`, and `test-host` depends on the same target.
    assert re.search(
        r"cd apps && cargo build --release --target wasm32-unknown-unknown", run.stdout
    )


def test_the_toolchain_file_pins_the_wasm_target():
    toolchain = (REPO_ROOT / "rust-toolchain.toml").read_text()
    assert re.search(r'^targets = \[.*"wasm32-unknown-unknown".*\]$', toolchain, re.MULTILINE)
    # CI installs whatever the file lists: no toolchain named on the command line.
    ci = (REPO_ROOT / ".github/workflows/ci.yml").read_text()
    assert re.search(r"^\s+rustup toolchain install\s*$", ci, re.MULTILINE)


# --- the parser, on handcrafted modules ---


def section(sid: int, body: bytes) -> bytes:
    return bytes([sid]) + uleb(len(body)) + body


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


HEADER = b"\0asm\x01\0\0\0"
# (func (param i64 f32) (result f64)) imported as env.log; one defined
# function of type 1, () -> (i32 i32), exported as "pair" (index 1).
SAMPLE = (
    HEADER
    + section(1, b"\x02" + b"\x60\x02\x7e\x7d\x01\x7c" + b"\x60\x00\x02\x7f\x7f")
    + section(2, b"\x01" + name("env") + name("log") + b"\x00\x00")
    + section(3, b"\x01\x01")
    + section(7, b"\x02" + name("pair") + b"\x00\x01" + name("mem") + b"\x02\x00")
    + section(
        0, name("target_features") + b"\x02" + b"+" + name("sign-ext") + b"-" + name("simd128")
    )
)


def test_parser_reads_types_imports_and_exports():
    module = parse(SAMPLE)
    assert module.types == [Signature(("i64", "f32"), ("f64",)), Signature((), ("i32", "i32"))]
    assert [(i.module, i.name, i.kind) for i in module.imports] == [("env", "log", "func")]
    assert module.exports == {"pair": Export("func", 1), "mem": Export("memory", 0)}
    # Function index 1 is the first defined one: imports come first.
    assert module.signature("pair") == Signature((), ("i32", "i32"))
    assert module.target_features() == {"sign-ext": "+", "simd128": "-"}
    with pytest.raises(KeyError):
        module.signature("mem")


def test_parser_reads_multibyte_lengths():
    long = "x" * 200  # a 2-byte LEB128 length
    module = parse(HEADER + section(0, name(long) + b"\x01\x02"))
    assert module.custom == {long: b"\x01\x02"}


@pytest.mark.parametrize(
    "data, message",
    [
        (b"\0asx\x01\0\0\0", "not a WASM module"),
        (b"\0asm\x02\0\0\0", "version 2"),
        (b"\0asm", "not a WASM module"),
        (HEADER + b"\x01\x05\x01", "truncated"),
        (HEADER + section(1, b"\x01\x61\x00\x00"), "function type form"),
        (HEADER + section(1, b"\x01\x60\x01\x01\x00"), "unknown value type"),
        (HEADER + section(3, b"\x01\x00\x00"), "left over"),
        (
            HEADER + section(7, b"\x02" + name("a") + b"\x00\x00" + name("a") + b"\x00\x00"),
            "exported twice",
        ),
        (HEADER + section(0, b"\x02\xff\xfe"), "not UTF-8"),
        (HEADER + b"\x00\xff\xff\xff\xff\xff\x01", "LEB128"),
    ],
)
def test_parser_refuses_malformed_modules(data, message):
    with pytest.raises(WasmError, match=message):
        parse(data)


def test_every_truncation_and_flipped_byte_parses_or_is_refused():
    # A module cut at a section boundary is a valid, smaller module; anywhere
    # else, and with any byte flipped, the parser says WasmError, never
    # another exception (an IndexError, a KeyError) or a hang.
    data = built("hello")
    for cut in range(len(data)):
        try:
            parse(data[:cut]).target_features()
        except WasmError:
            pass
    for i in range(len(data)):
        flipped = bytearray(data)
        flipped[i] ^= 0xFF
        try:
            parse(bytes(flipped)).target_features()
        except WasmError:
            pass


def test_signature_of_a_missing_function_type_is_an_error():
    bad = HEADER + section(3, b"\x01\x05") + section(7, b"\x01" + name("f") + b"\x00\x00")
    with pytest.raises(WasmError, match="type 5"):
        parse(bad).signature("f")


def test_target_features_with_a_bad_prefix_is_an_error():
    module = parse(HEADER + section(0, name("target_features") + b"\x01" + b"?" + name("x")))
    with pytest.raises(WasmError, match="prefix"):
        module.target_features()
