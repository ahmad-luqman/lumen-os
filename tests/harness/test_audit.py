"""The flag audit must actually catch the problems it claims to catch."""

from lumen_harness.audit import (
    check_fixup_sections,
    check_image_bounds,
    check_ir,
    check_sections,
    scan_disassembly,
)

CLEAN = """
ffffffff80001000 <lumen_kernel::kmain>:
ffffffff80001000:      	pushq	%rbp
ffffffff80001001:      	movq	%rsp, %rbp
ffffffff80001004:      	subq	$0x20, %rsp
ffffffff80001008:      	movq	%rax, -0x8(%rbp)
ffffffff8000100c:      	leaq	-0x10(%rsp), %rdi
ffffffff80001010:      	movq	0x8(%rsp), %rax
"""


def test_clean_disassembly_passes():
    assert scan_disassembly(CLEAN) == []


def test_detects_sse_register():
    bad = CLEAN + "ffffffff80001014:      \tmovaps\t%xmm0, (%rdi)\n"
    assert [f.check for f in scan_disassembly(bad)] == ["simd"]


def test_detects_avx512_and_state_instructions():
    bad = (
        CLEAN
        + "ffffffff80001014:      \tkmovw\t%k1, %eax\n"
        + "ffffffff80001018:      \tfxsave64\t(%rdi)\n"
    )
    assert [f.check for f in scan_disassembly(bad)] == ["simd", "simd"]


def test_fpu_allowlist_rejects_simd_outside_named_stubs():
    # Exact allowlisted save/restore mnemonics inside the two named stubs pass.
    allowed_stubs = """
ffffffff80002000 <lumen_fpu_save>:
ffffffff80002000:      \ttestq\t%rsi, %rsi
ffffffff80002003:      \tje\t0xffffffff80002010
ffffffff80002005:      \txsave64\t(%rdi)
ffffffff80002009:      \tretq
ffffffff80002010:      \tfxsave64\t(%rdi)
ffffffff80002014:      \tretq

ffffffff80002020 <lumen_fpu_restore>:
ffffffff80002020:      \ttestq\t%rsi, %rsi
ffffffff80002023:      \tje\t0xffffffff80002030
ffffffff80002025:      \txrstor64\t(%rdi)
ffffffff80002029:      \tretq
ffffffff80002030:      \tfxrstor64\t(%rdi)
ffffffff80002034:      \tretq
"""
    assert scan_disassembly(allowed_stubs) == []

    # Swapped save/restore mnemonics inside the two named stubs are rejected.
    swapped_save = """
ffffffff80002000 <lumen_fpu_save>:
ffffffff80002000:      \tfxrstor64\t(%rdi)
ffffffff80002004:      \txrstor64\t(%rdi)
"""
    assert [f.check for f in scan_disassembly(swapped_save)] == ["simd", "simd"]

    swapped_restore = """
ffffffff80002020 <lumen_fpu_restore>:
ffffffff80002020:      \tfxsave64\t(%rdi)
ffffffff80002024:      \txsave64\t(%rdi)
"""
    assert [f.check for f in scan_disassembly(swapped_restore)] == ["simd", "simd"]

    # State instructions in any other symbol (including near-miss names) fail.
    for sym in ("lumen_kernel::sched::switch", "lumen_fpu_save_helper"):
        for insn in (
            "fxsave64\t(%rdi)",
            "xsave64\t(%rdi)",
            "fxrstor64\t(%rdi)",
            "xrstor64\t(%rdi)",
        ):
            bad = f"ffffffff80003000 <{sym}>:\nffffffff80003000:      \t{insn}\n"
            assert [f.check for f in scan_disassembly(bad)] == ["simd"], (sym, insn)

    # Vector/MMX/mask registers and non-allowlisted state instructions (including
    # 32-bit fxsave/xsave/fxrstor/xrstor and ldmxcsr/stmxcsr) are banned
    # everywhere, even inside the two named stubs.
    for stub in ("lumen_fpu_save", "lumen_fpu_restore"):
        for insn in (
            "movaps\t%xmm0, (%rdi)",
            "vmovdqu\t%ymm0, (%rdi)",
            "vmovdqu64\t%zmm0, (%rdi)",
            "movq\t%mm0, (%rdi)",
            "kmovw\t%k1, %eax",
            "fxsave\t(%rdi)",
            "xsave\t(%rdi)",
            "fxrstor\t(%rdi)",
            "xrstor\t(%rdi)",
            "ldmxcsr\t(%rdi)",
            "stmxcsr\t(%rdi)",
        ):
            bad = f"ffffffff80002000 <{stub}>:\nffffffff80002000:      \t{insn}\n"
            assert [f.check for f in scan_disassembly(bad)] == ["simd"], (stub, insn)


def test_detects_red_zone_access():
    bad = CLEAN + "ffffffff80001014:      \tmovq\t%rax, -0x18(%rsp)\n"
    findings = scan_disassembly(bad)
    assert [f.check for f in findings] == ["red-zone"]
    assert "lumen_kernel::kmain" in findings[0].detail


def test_detects_large_code_model_movabs():
    bad = CLEAN + "ffffffff80001014:      \tmovabsq\t$0xffffffff80012340, %rax\n"
    assert [f.check for f in scan_disassembly(bad)] == ["code-model"]


READOBJ_OK = """
  Class: 64-bit (0x2)
  Type: Executable (0x2)
  Machine: EM_X86_64 (0x3E)
  Section {
    Name: .text (1)
    Flags [ (0x6)
      SHF_ALLOC (0x2)
      SHF_EXECINSTR (0x4)
    ]
    Address: 0xFFFFFFFF80000000
  }
  Section {
    Name: .comment (5)
    Flags [ (0x30)
    ]
    Address: 0x0
  }
"""


def test_sections_in_top_2gib_pass():
    assert check_sections(READOBJ_OK) == []


def test_low_allocated_section_fails():
    bad = READOBJ_OK.replace("Address: 0xFFFFFFFF80000000", "Address: 0x200000")
    assert [f.check for f in check_sections(bad)] == ["code-model"]


def test_pie_is_rejected():
    bad = READOBJ_OK.replace("Type: Executable (0x2)", "Type: SharedObject (0x3)")
    assert [f.check for f in check_sections(bad)] == ["elf"]


IMAGE_READOBJ = """
  Section {
    Name: .text (1)
    Flags [ (0x6)
      SHF_ALLOC (0x2)
      SHF_EXECINSTR (0x4)
    ]
    Address: 0xFFFFFFFF80000000
    Size: 4096
  }
  Section {
    Name: .rodata (7)
    Flags [ (0x2)
      SHF_ALLOC (0x2)
    ]
    Address: 0xFFFFFFFF80001000
    Size: 4096
  }
  Section {
    Name: .bss (29)
    Flags [ (0x3)
      SHF_ALLOC (0x2)
      SHF_WRITE (0x1)
    ]
    Address: 0xFFFFFFFF80002000
    Size: 4096
  }
  Section {
    Name: .debug_info (53)
    Flags [ (0x0)
    ]
    Address: 0x0
    Size: 407006
  }
"""
IMAGE_NM = """\
ffffffff80000000 T __kernel_start
ffffffff80001000 T __rodata_start
ffffffff80002000 R __data_start
ffffffff80003000 B __kernel_end
ffffffff80000010 t lumen_kernel::kmain
"""


def section(name: str, addr: int, size: int, flags: str) -> str:
    lines = "".join(f"      {f} (0x0)\n" for f in ["SHF_ALLOC", *flags.split()])
    return f"""
  Section {{
    Name: {name} (99)
    Flags [ (0x0)
{lines}    ]
    Address: 0x{addr:X}
    Size: {size}
  }}
"""


def test_sections_inside_the_image_pass():
    assert check_image_bounds(IMAGE_READOBJ, IMAGE_NM) == []


def test_orphan_section_after_the_image_fails():
    # The real case: the linker put `.got` after __kernel_end, where the
    # kernel's page tables don't map it.
    found = check_image_bounds(
        IMAGE_READOBJ + section(".got", 0xFFFFFFFF80003000, 232, ""), IMAGE_NM
    )
    assert [f.check for f in found] == ["image"]
    assert ".got" in found[0].detail


def test_section_before_the_image_fails():
    found = check_image_bounds(
        IMAGE_READOBJ + section(".early", 0xFFFFFFFF7FFFF000, 16, ""), IMAGE_NM
    )
    assert [f.check for f in found] == ["image"]


def test_section_running_past_the_end_fails():
    long_bss = IMAGE_READOBJ.replace(
        "Address: 0xFFFFFFFF80002000\n    Size: 4096", "Address: 0xFFFFFFFF80002000\n    Size: 4097"
    )
    assert [f.check for f in check_image_bounds(long_bss, IMAGE_NM)] == ["image"]


def test_writable_section_outside_the_data_segment_fails():
    # Mapped read-only by the kernel: the first write would fault.
    found = check_image_bounds(
        IMAGE_READOBJ + section(".wdata", 0xFFFFFFFF80001800, 8, "SHF_WRITE"), IMAGE_NM
    )
    assert [f.check for f in found] == ["image"] and "data segment" in found[0].detail


def test_executable_section_outside_the_text_segment_fails():
    found = check_image_bounds(
        IMAGE_READOBJ + section(".xtra", 0xFFFFFFFF80001000, 8, "SHF_EXECINSTR"), IMAGE_NM
    )
    assert [f.check for f in found] == ["image"] and "text segment" in found[0].detail


def test_read_only_section_outside_the_rodata_segment_fails():
    # An orphan read-only section in the data segment would become writable
    # (in the text segment, executable).
    for addr in (0xFFFFFFFF80002800, 0xFFFFFFFF80000800):
        found = check_image_bounds(IMAGE_READOBJ + section(".orphan", addr, 8, ""), IMAGE_NM)
        assert [f.check for f in found] == ["image"] and "rodata segment" in found[0].detail


def test_segment_symbols_out_of_order_fail():
    swapped = IMAGE_NM.replace(
        "ffffffff80001000 T __rodata_start", "ffffffff80002800 T __rodata_start"
    )
    assert [f.check for f in check_image_bounds(IMAGE_READOBJ, swapped)] == ["image"]


def test_read_only_got_is_allowed():
    # The linker flags .got writable; the kernel maps it read-only on purpose.
    got = IMAGE_READOBJ + section(".got", 0xFFFFFFFF80001800, 232, "SHF_WRITE")
    assert check_image_bounds(got, IMAGE_NM) == []


def test_missing_image_symbols_fail():
    assert [f.check for f in check_image_bounds(IMAGE_READOBJ, "")] == ["image"]


def test_unparseable_readobj_fails():
    # A format change must not make the checks pass with nothing checked.
    assert [f.check for f in check_image_bounds("", IMAGE_NM)] == ["image"]
    assert "elf" in [
        f.check for f in check_sections("Class: 64-bit\nMachine: EM_X86_64\nType: Executable\n")
    ]
    garbled = IMAGE_READOBJ.replace("Address: 0xFFFFFFFF80001000", "Address: ???")
    assert "elf" in [f.check for f in check_sections(garbled)]


PLAIN_FIXUP = section(".lumen_fixup", 0xFFFFFFFF80001400, 64, "")
NOFAULT_FIXUP = section(".lumen_fixup_nofault", 0xFFFFFFFF80001440, 32, "")


def test_nofault_fixup_section_is_read_only_and_in_rodata():
    # Both fixup tables in the rodata segment, read-only: clean, and inside
    # the image as the bounds check sees it.
    image = IMAGE_READOBJ + PLAIN_FIXUP + NOFAULT_FIXUP
    assert check_fixup_sections(image, IMAGE_NM) == []
    assert check_image_bounds(image, IMAGE_NM) == []
    # The kernel as built before B2 (no nofault table): the #PF handler
    # would have nothing to look up.
    found = check_fixup_sections(IMAGE_READOBJ + PLAIN_FIXUP, IMAGE_NM)
    assert [f.check for f in found] == ["fixup"] and ".lumen_fixup_nofault" in found[0].detail
    nofault = ".lumen_fixup_nofault"
    bad = {
        # Writable (the linker's "aw"): a stray store could redirect a fault.
        "writable": section(nofault, 0xFFFFFFFF80001440, 32, "SHF_WRITE"),
        "executable": section(nofault, 0xFFFFFFFF80001440, 32, "SHF_EXECINSTR"),
        # In the data segment, or in the text one: mapped writable or executable.
        "in data": section(nofault, 0xFFFFFFFF80002800, 32, ""),
        "in text": section(nofault, 0xFFFFFFFF80000800, 32, ""),
        "straddling": section(nofault, 0xFFFFFFFF80001FF0, 32, ""),
        # Empty: no access registered (KEEP lost, or the routines gone).
        "empty": section(nofault, 0xFFFFFFFF80001440, 0, ""),
    }
    for why, nofault_section in bad.items():
        found = check_fixup_sections(IMAGE_READOBJ + PLAIN_FIXUP + nofault_section, IMAGE_NM)
        assert [f.check for f in found] == ["fixup"], why
        assert nofault in found[0].detail, why
    # The user copies' table is checked the same way.
    misplaced = section(".lumen_fixup", 0xFFFFFFFF80002800, 64, "")
    found = check_fixup_sections(IMAGE_READOBJ + misplaced + NOFAULT_FIXUP, IMAGE_NM)
    assert [f.check for f in found] == ["fixup"] and nofault not in found[0].detail


IR_OK = """
define void @f() unnamed_addr #0 {
}
attributes #0 = { noredzone nounwind "frame-pointer"="all" "probe-stack"="inline-asm" "target-features"="-mmx,-sse,-sse2,+soft-float" }
!1 = !{i32 1, !"Code Model", i32 2}
"""


def test_ir_ok():
    assert check_ir(IR_OK) == []


def test_ir_missing_noredzone_and_sse_disable():
    bad = IR_OK.replace("noredzone ", "").replace("-sse,-sse2,", "")
    assert sorted(f.check for f in check_ir(bad)) == ["red-zone", "simd"]


def test_ir_missing_stack_probes():
    # Without probes a big frame can jump over a stack's guard page.
    for bad in (
        IR_OK.replace('"probe-stack"="inline-asm" ', ""),
        IR_OK.replace("inline-asm", "__rust_probestack"),
    ):
        assert [f.check for f in check_ir(bad)] == ["stack-probe"]


def test_ir_wrong_code_model():
    bad = IR_OK.replace('"Code Model", i32 2', '"Code Model", i32 1')
    assert [f.check for f in check_ir(bad)] == ["code-model"]
