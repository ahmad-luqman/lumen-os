"""Ring-3 entry/exit, syscall/sysretq, SWAPGS, IST_DEBUG (CVE-2018-8897), and
non-fatal ring-3 exception handling (M6, issue #236)."""

import re

from lumen_harness.crash import BEGIN as CRASH_BEGIN
from lumen_harness.qemu import EXIT_SUCCESS

BOOT_TIMEOUT = 60


def boot_shell(boot):
    m = boot("qemutest")
    m.expect(r"lumen> ", BOOT_TIMEOUT)
    return m


def test_ring3_entry_and_exit(boot):
    """Ring 3 enters via iretq, issues write(2) through syscall/sysretq
    (preserving all callee-saved registers), exercises the iretq return path,
    and exits cleanly back to the shell."""
    m = boot_shell(boot)
    assert "INFO  syscall: syscall/sysretq ready" in m.output

    m.send_line("usertest ok")
    m.expect(r'usertest: exit=42 stdout="hello from ring3"\n', 15)
    m.expect(r"lumen> ", 10)

    # Also exercise the rewritten-frame iretq return path.
    m.send_line("usertest iret")
    m.expect(r'usertest: exit=7 stdout=""\n', 15)
    m.expect(r"lumen> ", 10)

    assert CRASH_BEGIN not in m.output
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_user_fault_kills_process_not_kernel(boot):
    """Any ring-3 exception (#PF, #UD, #GP, #BP, #DB, and deferred #DB at
    LSTAR on IST_DEBUG + CVE-2018-8897 MOV SS; SYSCALL) logs a single WARN
    and terminates only the user process, leaving the kernel healthy."""
    m = boot_shell(boot)

    cases = [
        ("pf", 14, r"0xdeadb000"),
        ("ud", 6, r"0x0"),
        ("gp", 13, r"0x0"),
        ("bp", 3, r"0x0"),
        ("db", 1, r"0x0"),
        ("lstar_db", 1, r"0x0"),
        ("movss_bp", 3, r"0x0"),
        ("stub_db", 3, r"0x0"),
    ]
    for kind, vec, cr2_pat in cases:
        start = len(m.output)
        m.send_line(f"usertest {kind}")
        line = m.expect(
            rf"usertest: fault vector={vec} \([^)]+\) error=0x[0-9a-f]+ "
            rf"rip=0x[0-9a-f]+ cr2={cr2_pat} deferred_lstar=(\d+) deferred_stub=(\d+)\n",
            15,
        )
        m.expect(r"lumen> ", 10)
        chunk = m.output[start:]
        assert CRASH_BEGIN not in chunk, chunk
        # Every ring-3 fault logs one WARN naming pid and vector.
        warns = re.findall(r"WARN  user: pid \d+: .*", chunk)
        assert len(warns) == 1, f"{kind}: expected 1 user WARN, got {warns}\n{chunk}"
        if kind == "lstar_db":
            assert int(line.group(1)) == 1, (
                f"lstar_db did not exercise the IST_DEBUG / LSTAR deferred #DB handoff:\n{chunk}"
            )
        if kind == "stub_db":
            assert int(line.group(2)) == 1, (
                f"stub_db did not exercise the IST_DEBUG / IDT exception stub deferred #DB handoff:\n{chunk}"
            )

    # Also exercise bare `MOV SS; SYSCALL` with TF=1 (CVE-2018-8897): on KVM /
    # real x86-64 hardware this fires the deferred #DB at LSTAR (vector=1,
    # deferred_lstar=1); under QEMU TCG (which clears TF on SYSCALL without
    # latching a MOV SS debug trap) it exits cleanly with 77. Never crashes.
    start = len(m.output)
    m.send_line("usertest cve_db")
    cve_line = m.expect(
        r"usertest: (?:fault vector=1 \(#DB debug\) error=0x0 rip=0x[0-9a-f]+ cr2=0x0 "
        r"deferred_lstar=(\d+) deferred_stub=\d+|exit=77 stdout=\"\")\n",
        15,
    )
    m.expect(r"lumen> ", 10)
    assert CRASH_BEGIN not in m.output[start:], m.output[start:]
    if cve_line.group(1) is not None:
        assert int(cve_line.group(1)) == 1, m.output[start:]

    # Kernel and kernel-mode #BP still work normally after all ring-3 faults.
    m.send_line("int3")
    m.expect(r"breakpoints taken: 1\n", 10)
    m.expect(r"lumen> ", 10)
    m.send_line("echo kernel-alive")
    m.expect(r"\nkernel-alive\n", 10)
    m.expect(r"lumen> ", 10)
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_non_canonical_sysret_does_not_crash_kernel(boot):
    """A non-canonical return RIP (0x0000_8000_0000_0000) is rejected before
    swapgs/sysretq or swapgs/iretq executes, terminating only the process."""
    m = boot_shell(boot)

    for kind in ("noncanon", "noncanon-iret"):
        start = len(m.output)
        m.send_line(f"usertest {kind}")
        m.expect(r"usertest: non-canonical rip=0x800000000000\n", 15)
        m.expect(r"lumen> ", 10)
        chunk = m.output[start:]
        assert CRASH_BEGIN not in chunk, chunk
        warns = re.findall(
            r"WARN  user: pid \d+: non-canonical user return rip 0x800000000000", chunk
        )
        assert len(warns) == 1, chunk

    # Kernel remains healthy and can still run a clean ring-3 program afterwards.
    m.send_line("usertest ok")
    m.expect(r'usertest: exit=42 stdout="hello from ring3"\n', 15)
    m.expect(r"lumen> ", 10)
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_bad_elf_rejected(boot):
    """Malformed, W+X, overlapping, kernel-half, PT_INTERP, W+X-shared-page,
    and stack-overlapping ELF64 images are rejected cleanly with zero leaked
    physical frames or heap."""
    m = boot_shell(boot)

    m.send_line("usertest bad_elf")
    m.expect(r"usertest: bad_elf rejected=7 leaked_frames=0 leaked_heap=0\n", 15)
    m.expect(r"lumen> ", 10)

    assert CRASH_BEGIN not in m.output
    m.send_line("usertest ok")
    m.expect(r'usertest: exit=42 stdout="hello from ring3"\n', 15)
    m.expect(r"lumen> ", 10)
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_mmap_and_brk(boot):
    """Ring-3 ELF64 program executes lazy anonymous mmap (#PF zero-fill),
    mprotect(PROT_NONE) -> mprotect(RW) preserving frames/data, byte-granular
    brk growth/shrink/failure, and munmap with zero leaked frames."""
    m = boot_shell(boot)

    m.send_line("usertest mmap_brk")
    m.expect(r"usertest: mmap_brk exit=0 leaked_frames=0\n", 20)
    m.expect(r"lumen> ", 10)

    assert CRASH_BEGIN not in m.output
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_sse_and_tls_preserved_across_preemption(boot):
    """Two concurrent user processes pinned to the same CPU hold distinct
    xmm0-xmm7 patterns and distinct IA32_FS_BASE TLS pointers across multiple
    real timer-driven scheduler switches with zero corruption or frame leaks."""
    m = boot_shell(boot)

    m.send_line("usertest sse_tls")
    line = m.expect(
        r"usertest: sse_tls cpu=\d+ exit_a=0 exit_b=0 switches=(\d+) leaked_frames=0\n",
        30,
    )
    m.expect(r"lumen> ", 10)
    assert int(line.group(1)) >= 6, f"expected >= 6 preemptive switches, got {line.group(0)}"

    assert CRASH_BEGIN not in m.output
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
