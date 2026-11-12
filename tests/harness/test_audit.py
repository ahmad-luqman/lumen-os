"""The flag audit must actually catch the problems it claims to catch."""

from lumen_harness.audit import check_image_bounds, check_ir, check_sections, scan_disassembly

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
    assert [f.check for f in found] == ["image"] and "writable" in found[0].detail


def test_executable_section_outside_the_text_segment_fails():
    found = check_image_bounds(
        IMAGE_READOBJ + section(".xtra", 0xFFFFFFFF80001000, 8, "SHF_EXECINSTR"), IMAGE_NM
    )
    assert [f.check for f in found] == ["image"] and "executable" in found[0].detail


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
    assert "elf" in [f.check for f in check_image_bounds(garbled, IMAGE_NM)]


IR_OK = """
define void @f() unnamed_addr #0 {
}
attributes #0 = { noredzone nounwind "frame-pointer"="all" "target-features"="-mmx,-sse,-sse2,+soft-float" }
!1 = !{i32 1, !"Code Model", i32 2}
"""


def test_ir_ok():
    assert check_ir(IR_OK) == []


def test_ir_missing_noredzone_and_sse_disable():
    bad = IR_OK.replace("noredzone ", "").replace("-sse,-sse2,", "")
    assert sorted(f.check for f in check_ir(bad)) == ["red-zone", "simd"]


def test_ir_wrong_code_model():
    bad = IR_OK.replace('"Code Model", i32 2', '"Code Model", i32 1')
    assert [f.check for f in check_ir(bad)] == ["code-model"]
