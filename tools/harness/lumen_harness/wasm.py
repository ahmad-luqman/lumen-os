"""WASM modules as the apps/ workspace builds them (#196), checked before
anything in the kernel loads them.

`compile_module` validates a whole module with `wasmtime` under `engine()`, which
switches off every proposal rustc doesn't use by default for
wasm32-unknown-unknown (SIMD, threads, tail calls, ...), so a module that
needs one is refused; a `wasmtime.Module` also gives the imports, the
exports with their types, and can be instantiated and called.

wasmtime has no API for custom sections, so a small reader here walks the
section headers and keeps every custom section in order (`custom_sections`);
`custom_section` refuses a name that appears twice. Through it:
`target_features` (which features the compiler let itself use) and
`function_names` (the `name` section's function names).
"""

from __future__ import annotations

import wasmtime

MAGIC_AND_VERSION = b"\0asm\x01\0\0\0"
CUSTOM = 0  # section id (WebAssembly core spec, 5.5.2)

# Proposals past what rustc 1.99.1 enables by default for the target. With
# them off, validation proves an app uses none of them.
_OFF = (
    "wasm_simd",
    "wasm_relaxed_simd",
    "wasm_threads",
    "wasm_tail_call",
    "wasm_gc",
    "wasm_function_references",
    "wasm_exceptions",
    "wasm_memory64",
    "wasm_multi_memory",
    "wasm_wide_arithmetic",
    "wasm_custom_page_sizes",
    "wasm_stack_switching",
    "wasm_component_model",
)


class WasmError(ValueError):
    """A custom section, or the section headers around it, can't be read."""


def engine() -> wasmtime.Engine:
    config = wasmtime.Config()
    for proposal in _OFF:
        setattr(config, proposal, False)
    return wasmtime.Engine(config)


def compile_module(data: bytes, eng: wasmtime.Engine | None = None) -> wasmtime.Module:
    """Validate and compile `data`; raises `wasmtime.WasmtimeError` if invalid."""
    return wasmtime.Module(eng or engine(), data)


class _Reader:
    def __init__(self, data: bytes, pos: int = 0, end: int | None = None):
        self.data = data
        self.pos = pos
        self.limit = len(data) if end is None else end

    def at_end(self) -> bool:
        return self.pos >= self.limit

    def byte(self) -> int:
        if self.pos >= self.limit:
            raise WasmError(f"truncated at byte {self.pos}")
        b = self.data[self.pos]
        self.pos += 1
        return b

    def take(self, n: int) -> bytes:
        if n > self.limit - self.pos:
            raise WasmError(
                f"truncated: {n} bytes wanted at {self.pos}, {self.limit - self.pos} left"
            )
        out = self.data[self.pos : self.pos + n]
        self.pos += n
        return out

    def uleb(self) -> int:
        """An unsigned LEB128 u32: at most 5 bytes, the value within 32 bits."""
        result = 0
        for i in range(5):
            b = self.byte()
            result |= (b & 0x7F) << (7 * i)
            if not b & 0x80:
                if result >> 32:
                    raise WasmError(f"LEB128 value {result} doesn't fit 32 bits")
                return result
        raise WasmError(f"LEB128 longer than 5 bytes at {self.pos}")

    def name(self) -> str:
        raw = self.take(self.uleb())
        try:
            return raw.decode("utf-8")
        except UnicodeDecodeError as e:
            raise WasmError(f"name is not UTF-8: {raw!r}") from e

    def sub(self, size: int) -> _Reader:
        """The next `size` bytes as a reader of their own (skipped here)."""
        start = self.pos
        self.take(size)
        return _Reader(self.data, start, start + size)


def custom_sections(data: bytes) -> list[tuple[str, bytes]]:
    """Every custom section, in the order the module has them (a name may repeat)."""
    if data[:8] != MAGIC_AND_VERSION:
        raise WasmError(f"not a version 1 WASM module (starts {data[:8]!r})")
    r = _Reader(data, 8)
    found = []
    while not r.at_end():
        section = r.byte()
        body = r.sub(r.uleb())
        if section == CUSTOM:
            name = body.name()
            found.append((name, body.take(body.limit - body.pos)))
    return found


def custom_section(data: bytes, name: str) -> bytes | None:
    """The custom section called `name`, None if there is none; a name that
    appears twice is refused (which one would the kernel believe?)."""
    payloads = [payload for n, payload in custom_sections(data) if n == name]
    if len(payloads) > 1:
        raise WasmError(f"custom section {name!r} appears {len(payloads)} times")
    return payloads[0] if payloads else None


def target_features(data: bytes) -> dict[str, str]:
    """The `target_features` section: each feature with its prefix (`+`: the
    module uses it; `-`: it must not be used; `=`: used, and every module
    linked with this one must use it too), or {} if the module has none. A
    feature listed twice is refused."""
    payload = custom_section(data, "target_features")
    if payload is None:
        return {}
    r = _Reader(payload)
    features: dict[str, str] = {}
    for _ in range(r.uleb()):
        prefix = chr(r.byte())
        if prefix not in "+-=":
            raise WasmError(f"target feature prefix {prefix!r}")
        feature = r.name()
        if feature in features:
            raise WasmError(f"target feature {feature!r} listed twice")
        features[feature] = prefix
    if not r.at_end():
        raise WasmError(f"target_features: {r.limit - r.pos} bytes left over")
    return features


FUNCTION_NAMES = 1  # the `name` section's function-names subsection


def function_names(data: bytes) -> dict[int, str]:
    """The `name` section's function names by function index ({} without one)."""
    payload = custom_section(data, "name")
    names: dict[int, str] = {}
    if payload is None:
        return names
    r = _Reader(payload)
    while not r.at_end():
        subsection = r.byte()
        body = r.sub(r.uleb())
        if subsection != FUNCTION_NAMES:
            continue
        for _ in range(body.uleb()):
            index = body.uleb()
            names[index] = body.name()
    return names
