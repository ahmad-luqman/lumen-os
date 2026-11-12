"""Kernel code-generation audit (hard rule 3: compiler flags are tested, not assumed).

v1 silently never applied its architecture flags. This audit checks the
artifacts themselves, with the pinned toolchain's LLVM tools so macOS and CI
behave the same:

Binary checks (the built kernel ELFs):
  * ELF64 x86-64 executable, every allocated section in the top 2 GiB
    (0xffffffff80000000+), as the kernel code model requires.
  * Every allocated section inside [__kernel_start, __kernel_end): the
    kernel's page tables map exactly that range and the frame allocator
    reserves it, so a section the linker script doesn't place (an orphan,
    like `.got` once was) would be unmapped and unprotected. Writable
    sections must be in the data segment and executable ones in the text
    segment, which is how the page tables map them.
  * No SSE/AVX/MMX/AVX-512 register or state instruction anywhere in the
    disassembly. The kernel never saves vector state on interrupts, so one
    stray `movaps` would corrupt user state later.
  * No red-zone use: no memory access below %rsp (`-0x..(%rsp)`). Interrupts
    push onto the current stack and would clobber a red zone.
  * No 64-bit absolute kernel addresses materialized with `movabs` (a sign of
    the large code model).

Compiler checks (LLVM IR of the kernel crate, built separately):
  * Module flag "Code Model" == 2 (kernel).
  * Every function carries `noredzone`, `"frame-pointer"="all"` and target
    features that disable SSE/SSE2.
"""

from __future__ import annotations

import json
import os
import platform
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

from . import BUILD_DIR, REPO_ROOT

KERNEL_BASE = 0xFFFFFFFF80000000

SIMD_REG = re.compile(r"%(?:[xyz]mm\d+|mm[0-7]|k[0-7])\b")
SIMD_MNEMONICS = {
    "fxsave", "fxsave64", "fxrstor", "fxrstor64", "xsave", "xsave64", "xsaveopt",
    "xsavec", "xsaves", "xrstor", "xrstor64", "xrstors", "ldmxcsr", "stmxcsr",
    "vldmxcsr", "vstmxcsr", "emms", "femms", "vzeroupper", "vzeroall",
}  # fmt: skip
RED_ZONE = re.compile(r"-0x[0-9a-f]+\(%rsp\)")
MOVABS_KERNEL = re.compile(r"movabsq?\s+\$0xffffffff8[0-9a-f]{7}\b")
INSN = re.compile(r"^\s*([0-9a-f]+):\s+(\S+)\s*(.*)$")


@dataclass
class Finding:
    check: str
    detail: str

    def __str__(self) -> str:
        return f"[{self.check}] {self.detail}"


def llvm_tool(name: str) -> str:
    """Path to an LLVM tool from the pinned toolchain's llvm-tools component."""
    sysroot = subprocess.check_output(["rustc", "--print", "sysroot"], text=True, cwd=REPO_ROOT)
    host = subprocess.check_output(["rustc", "--print", "host-tuple"], text=True, cwd=REPO_ROOT)
    path = Path(sysroot.strip()) / "lib/rustlib" / host.strip() / "bin" / name
    if not path.exists():
        raise SystemExit(f"audit: {path} missing; is the llvm-tools component installed?")
    return str(path)


def scan_disassembly(text: str) -> list[Finding]:
    """Check AT&T-syntax `llvm-objdump -d` output for forbidden instructions."""
    findings: list[Finding] = []
    func = "?"
    for line in text.splitlines():
        if line.endswith(">:"):
            func = line.split("<", 1)[-1].rstrip(">:")
            continue
        m = INSN.match(line)
        if not m:
            continue
        addr, mnemonic, operands = m.groups()
        where = f"{func} @ {addr}: {mnemonic} {operands}".strip()
        if mnemonic in SIMD_MNEMONICS or SIMD_REG.search(operands):
            findings.append(Finding("simd", where))
        if RED_ZONE.search(operands) and not mnemonic.startswith("lea"):
            findings.append(Finding("red-zone", where))
        if MOVABS_KERNEL.search(f"{mnemonic} {operands}"):
            findings.append(Finding("code-model", where))
    return findings


@dataclass
class Section:
    name: str
    addr: int
    size: int | None  # None if the readobj output has no Size line
    flags: set[str]


def alloc_sections(readobj: str) -> tuple[list[Section], list[Finding]]:
    """The SHF_ALLOC sections in `llvm-readobj --sections` output. An
    allocated section whose name or address can't be parsed is a finding,
    not skipped: a format change must not make the checks pass vacuously."""
    sections: list[Section] = []
    findings: list[Finding] = []
    for block in readobj.split("Section {")[1:]:
        flags = set(re.findall(r"^\s*(SHF_\w+) \(", block, re.MULTILINE))
        if "SHF_ALLOC" not in flags:
            continue
        name = re.search(r"^\s*Name: (\S+)", block, re.MULTILINE)
        addr = re.search(r"^\s*Address: (0x[0-9A-Fa-f]+)$", block, re.MULTILINE)
        size = re.search(r"^\s*Size: (\d+)$", block, re.MULTILINE)
        if not (name and addr):
            findings.append(Finding("elf", f"unparseable allocated section: {block[:80]!r}"))
            continue
        sections.append(
            Section(
                name.group(1), int(addr.group(1), 16), int(size.group(1)) if size else None, flags
            )
        )
    return sections, findings


def check_sections(readobj: str) -> list[Finding]:
    """Check `llvm-readobj --file-headers --sections` output."""
    findings: list[Finding] = []
    if "Class: 64-bit" not in readobj:
        findings.append(Finding("elf", "not an ELF64 file"))
    if "Machine: EM_X86_64" not in readobj:
        findings.append(Finding("elf", "not an x86-64 file"))
    if "Type: Executable" not in readobj:
        findings.append(Finding("elf", "not a static executable (ET_EXEC)"))
    sections, problems = alloc_sections(readobj)
    findings += problems
    if not any(s.name == ".text" for s in sections):
        findings.append(
            Finding("elf", "no allocated .text section found (readobj format changed?)")
        )
    for s in sections:
        if s.addr < KERNEL_BASE:
            findings.append(Finding("code-model", f"section {s.name} at {s.addr:#x} below -2GiB"))
    return findings


# Sections the linker flags writable that the kernel maps read-only on
# purpose. `.got`: the kernel is statically linked, so its entries are fixed
# at link time and nothing (no dynamic loader) writes them; read-only keeps
# those function pointers from being overwritten.
READ_ONLY_WRITABLE = {".got"}


def check_image_bounds(readobj: str, nm: str) -> list[Finding]:
    """Check every allocated section (`llvm-readobj --sections`) lies inside
    [__kernel_start, __kernel_end), and in the segment the page tables give
    the rights it needs: writable sections in [__data_start, __kernel_end),
    executable ones in [__kernel_start, __rodata_start) (symbols from
    `llvm-nm`)."""
    symbols = {
        name: int(addr, 16)
        for addr, _, name in re.findall(r"^([0-9a-f]+) (\w) (\S+)$", nm, re.MULTILINE)
    }
    names = ("__kernel_start", "__rodata_start", "__data_start", "__kernel_end")
    missing = [n for n in names if n not in symbols]
    if missing:
        return [Finding("image", f"{', '.join(missing)} missing from the symbol table")]
    start, rodata, data, end = (symbols[n] for n in names)
    sections, findings = alloc_sections(readobj)
    if not sections:
        findings.append(Finding("image", "no allocated sections parsed (readobj format changed?)"))
    for s in sections:
        if s.size is None:
            findings.append(Finding("image", f"section {s.name} has no size"))
            continue
        lo, hi = s.addr, s.addr + s.size
        if lo < start or hi > end:
            findings.append(
                Finding(
                    "image",
                    f"section {s.name} [{lo:#x}, {hi:#x}) is outside the kernel image "
                    f"[{start:#x}, {end:#x}): place it in linker.ld",
                )
            )
        elif "SHF_WRITE" in s.flags and lo < data and s.name not in READ_ONLY_WRITABLE:
            findings.append(
                Finding(
                    "image",
                    f"writable section {s.name} at {lo:#x} is below __data_start (mapped read-only)",
                )
            )
        elif "SHF_EXECINSTR" in s.flags and hi > rodata:
            findings.append(
                Finding(
                    "image",
                    f"executable section {s.name} ends at {hi:#x}, past __rodata_start (mapped NX)",
                )
            )
    return findings


def check_ir(ir: str) -> list[Finding]:
    """Check LLVM IR (`--emit=llvm-ir`) of the kernel crate."""
    findings: list[Finding] = []
    if not re.search(r'!"Code Model", i32 2\}', ir):
        findings.append(Finding("code-model", "LLVM module flag 'Code Model' is not kernel (2)"))
    groups = dict(re.findall(r"^attributes (#\d+) = \{(.*)\}$", ir, re.MULTILINE))
    defines = re.findall(r"^define .*?\)\s.*?(#\d+)", ir, re.MULTILINE)
    if not defines:
        findings.append(Finding("ir", "no function definitions found in IR"))
    for group in sorted(set(defines)):
        attrs = groups.get(group, "")
        if "noredzone" not in attrs:
            findings.append(Finding("red-zone", f"attribute group {group} lacks noredzone"))
        if '"frame-pointer"="all"' not in attrs:
            findings.append(
                Finding("frame-pointer", f"attribute group {group} lacks frame pointers")
            )
        feats = re.search(r'"target-features"="([^"]*)"', attrs)
        feat_set = set(feats.group(1).split(",")) if feats else set()
        if not {"-sse", "-sse2"} <= feat_set:
            findings.append(Finding("simd", f"attribute group {group} does not disable SSE/SSE2"))
    return findings


def emit_kernel_ir() -> str:
    target_dir = BUILD_DIR / "audit-target"
    env = dict(os.environ, CARGO_TARGET_DIR=str(target_dir))
    subprocess.run(
        ["cargo", "rustc", "-q", "-p", "lumen-kernel", "--target", "x86_64-unknown-none",
         "--release", "--", "--emit=llvm-ir"],
        check=True, cwd=REPO_ROOT, env=env,
    )  # fmt: skip
    lls = sorted(
        (target_dir / "x86_64-unknown-none/release/deps").glob("lumen_kernel-*.ll"),
        key=lambda p: p.stat().st_mtime,
    )
    if not lls:
        raise SystemExit("audit: no LLVM IR produced")
    return lls[-1].read_text()


def run_audit(kernels: list[Path]) -> int:
    objdump = llvm_tool("llvm-objdump")
    readobj = llvm_tool("llvm-readobj")
    nm = llvm_tool("llvm-nm")
    total: list[Finding] = []
    for k in kernels:
        hdr = subprocess.check_output([readobj, "--file-headers", "--sections", str(k)], text=True)
        dis = subprocess.check_output(
            [objdump, "-d", "--no-show-raw-insn", "--x86-asm-syntax=att", str(k)], text=True
        )
        syms = subprocess.check_output([nm, str(k)], text=True)
        found = check_sections(hdr) + check_image_bounds(hdr, syms) + scan_disassembly(dis)
        n_insns = sum(1 for line in dis.splitlines() if INSN.match(line))
        status = "ok" if not found else f"{len(found)} finding(s)"
        print(f"audit: {k}: {n_insns} instructions scanned, {status}")
        total += found
    ir_findings = check_ir(emit_kernel_ir())
    print(f"audit: LLVM IR: {'ok' if not ir_findings else f'{len(ir_findings)} finding(s)'}")
    total += ir_findings
    results = BUILD_DIR / "test-results"
    results.mkdir(parents=True, exist_ok=True)
    (results / "audit.json").write_text(json.dumps({"flags": not total}) + "\n")
    for f in total[:50]:
        print(f"  {f}", file=sys.stderr)
    if total:
        print(f"audit: FAILED ({len(total)} findings)", file=sys.stderr)
        return 1
    print(f"audit: passed ({platform.system()})")
    return 0
