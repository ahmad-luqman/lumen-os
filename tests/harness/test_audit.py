"""The flag audit must actually catch the problems it claims to catch."""

from lumen_harness.audit import check_ir, check_sections, scan_disassembly

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
