"""WASM modules as the apps/ workspace builds them (#196): enough of the
binary format to check a built app before anything loads it.

`parse` reads a module's header and its sections: the types, imports,
functions and exports (each export with its kind, and a function's
signature), and the custom sections, among them `target_features`, which
says which WASM features the compiler let itself use (the kernel's `wasmi`
must accept every one, #197). It checks what it reads, so a truncated or
malformed module raises `WasmError` rather than parsing to something wrong;
it does not validate code bodies (wasmi does that when it loads a module).
"""

from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import Path

MAGIC = b"\0asm"
VERSION = 1

# Section ids (WebAssembly core spec, 5.5.2).
CUSTOM, TYPE, IMPORT, FUNCTION = 0, 1, 2, 3
EXPORT = 7

VALTYPES = {
    0x7F: "i32",
    0x7E: "i64",
    0x7D: "f32",
    0x7C: "f64",
    0x7B: "v128",
    0x70: "funcref",
    0x6F: "externref",
}
KINDS = {0: "func", 1: "table", 2: "memory", 3: "global"}


class WasmError(ValueError):
    """The bytes are not a WASM module this parser can read."""


@dataclass(frozen=True)
class Signature:
    params: tuple[str, ...]
    results: tuple[str, ...]


@dataclass(frozen=True)
class Export:
    kind: str
    index: int


@dataclass(frozen=True)
class Import:
    module: str
    name: str
    kind: str


@dataclass
class Module:
    types: list[Signature] = field(default_factory=list)
    imports: list[Import] = field(default_factory=list)
    # The type index of each function the module defines (not the imported ones).
    functions: list[int] = field(default_factory=list)
    exports: dict[str, Export] = field(default_factory=dict)
    custom: dict[str, bytes] = field(default_factory=dict)

    def signature(self, export: str) -> Signature:
        """The signature of the function exported as `export`."""
        found = self.exports.get(export)
        if found is None or found.kind != "func":
            raise KeyError(f"no function exported as {export!r}")
        imported = [i for i in self.imports if i.kind == "func"]
        if found.index < len(imported):
            raise WasmError(f"{export!r} re-exports an import; its type isn't read")
        defined = found.index - len(imported)
        if defined >= len(self.functions):
            raise WasmError(f"{export!r} names function {found.index}, which doesn't exist")
        type_index = self.functions[defined]
        if type_index >= len(self.types):
            raise WasmError(f"function {found.index} has type {type_index}, which doesn't exist")
        return self.types[type_index]

    def target_features(self) -> dict[str, str]:
        """The `target_features` custom section: each feature with its prefix
        (`+` used, `-` must not be used, `=` used and required by every
        module linked with it), or {} if the module has none."""
        payload = self.custom.get("target_features")
        if payload is None:
            return {}
        r = _Reader(payload)
        features = {}
        for _ in range(r.uleb()):
            prefix = chr(r.byte())
            if prefix not in "+-=":
                raise WasmError(f"target feature prefix {prefix!r}")
            features[r.name()] = prefix
        r.end("target_features")
        return features


class _Reader:
    def __init__(self, data: bytes, pos: int = 0, end: int | None = None):
        self.data = data
        self.pos = pos
        self.limit = len(data) if end is None else end

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

    def uleb(self, bits: int = 32) -> int:
        result = shift = 0
        while True:
            b = self.byte()
            result |= (b & 0x7F) << shift
            shift += 7
            if not b & 0x80:
                break
            if shift >= bits + 7:
                raise WasmError(f"LEB128 longer than {bits} bits at {self.pos}")
        if result >> bits:
            raise WasmError(f"LEB128 value {result} doesn't fit {bits} bits")
        return result

    def name(self) -> str:
        raw = self.take(self.uleb())
        try:
            return raw.decode("utf-8")
        except UnicodeDecodeError as e:
            raise WasmError(f"name is not UTF-8: {raw!r}") from e

    def valtype(self) -> str:
        b = self.byte()
        if b not in VALTYPES:
            raise WasmError(f"unknown value type 0x{b:02x}")
        return VALTYPES[b]

    def limits(self) -> None:
        flags = self.byte()
        self.uleb(64)
        if flags & 1:
            self.uleb(64)

    def end(self, what: str) -> None:
        if self.pos != self.limit:
            raise WasmError(f"{what}: {self.limit - self.pos} bytes left over")


def parse(data: bytes) -> Module:
    if len(data) < 8 or data[:4] != MAGIC:
        raise WasmError(f"not a WASM module (starts {data[:4]!r})")
    version = int.from_bytes(data[4:8], "little")
    if version != VERSION:
        raise WasmError(f"WASM version {version}, expected {VERSION}")
    module = Module()
    r = _Reader(data, 8)
    while r.pos < r.limit:
        section = r.byte()
        size = r.uleb()
        start = r.pos
        r.take(size)  # checks the section fits
        body = _Reader(data, start, start + size)
        if section == CUSTOM:
            custom_name = body.name()  # before the payload: Python evaluates `d[k] = v`'s v first
            module.custom[custom_name] = body.take(body.limit - body.pos)
        elif section == TYPE:
            for _ in range(body.uleb()):
                if (form := body.byte()) != 0x60:
                    raise WasmError(f"function type form 0x{form:02x}")
                params = tuple(body.valtype() for _ in range(body.uleb()))
                results = tuple(body.valtype() for _ in range(body.uleb()))
                module.types.append(Signature(params, results))
        elif section == IMPORT:
            for _ in range(body.uleb()):
                mod, name, kind = body.name(), body.name(), body.byte()
                if kind == 0:
                    body.uleb()
                elif kind == 1:
                    body.valtype()
                    body.limits()
                elif kind == 2:
                    body.limits()
                elif kind == 3:
                    body.valtype()
                    body.byte()
                else:
                    raise WasmError(f"import kind {kind}")
                module.imports.append(Import(mod, name, KINDS[kind]))
        elif section == FUNCTION:
            module.functions = [body.uleb() for _ in range(body.uleb())]
        elif section == EXPORT:
            for _ in range(body.uleb()):
                name, kind, index = body.name(), body.byte(), body.uleb()
                if kind not in KINDS:
                    raise WasmError(f"export kind {kind}")
                if name in module.exports:
                    raise WasmError(f"{name!r} exported twice")
                module.exports[name] = Export(KINDS[kind], index)
        else:
            continue  # sections not read here (tables, memory, code, data, ...)
        body.end(f"section {section}")
    return module


def load(path: Path) -> Module:
    return parse(path.read_bytes())
