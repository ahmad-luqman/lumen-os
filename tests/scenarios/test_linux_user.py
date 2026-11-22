"""Linux x86-64 ring-3 ABI scenarios: entry/exit, syscall/sysretq, SWAPGS,
IST_DEBUG (CVE-2018-8897), non-fatal ring-3 faults, per-process AddressSpace /
demand paging / SSE+TLS preemption, 30-syscall table, static musl hello, and
unmodified Alpine static musl busybox ash built-ins (M6, issues #236-#238), and
binaries loaded by path from the ext4 rootfs, dynamic ones through PT_INTERP (#255)."""

import re

from lumen_harness.crash import BEGIN as CRASH_BEGIN
from lumen_harness.qemu import EXIT_SUCCESS, Devices
from lumen_harness.rootfs import copy_rootfs

BOOT_TIMEOUT = 60


def boot_shell(boot, *, devices=None, cmdline=None):
    kwargs = {}
    if devices is not None:
        kwargs["devices"] = devices
    if cmdline is not None:
        kwargs["cmdline"] = cmdline
    m = boot("qemutest", **kwargs)
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


def test_syscall_bad_pointers_return_efault(boot):
    """Every pointer-taking M6 syscall rejects null, kernel-half, and unmapped
    user pointers with -EFAULT without panicking; unknown syscalls return
    -ENOSYS and closed-fd calls return -EBADF, with zero leaked frames."""
    m = boot_shell(boot, devices=Devices(rng=True))

    m.send_line("usertest bad_ptrs")
    m.expect(r"usertest: bad_ptrs exit=0 leaked_frames=0\n", 20)
    m.expect(r"lumen> ", 10)

    assert CRASH_BEGIN not in m.output
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_static_musl_hello(boot):
    """Unmodified static musl x86-64 ELF (`hello` loaded as a Limine boot
    module) prints its greeting, exits 0, and leaks zero physical frames."""
    m = boot_shell(boot, devices=Devices(rng=True))

    m.send_line("linux")
    m.expect(r"linux: \d+ boot module\(s\):", 10)
    m.expect(r"  hello \(\d+ KiB\)\n", 10)
    m.expect(r"  busybox \(\d+ KiB\)\n", 10)
    m.expect(r"lumen> ", 10)

    m.send_line("linux hello")
    m.expect(r"hello from static musl on lumen\n", 20)
    m.expect(r"linux: hello exit=0 leaked_frames=0\n", 20)
    m.expect(r"lumen> ", 10)

    assert CRASH_BEGIN not in m.output
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_static_busybox_ash(boot):
    """Unmodified Alpine static musl `busybox ash -s` executes shell built-ins
    (echo, pwd, export, arithmetic, for loop, exit 0) interactively over serial
    and returns cleanly to the Lumen shell with zero leaked frames."""
    m = boot_shell(boot, devices=Devices(rng=True))

    m.send_line("linux busybox ash -s")
    m.expect(r"~ # ", 20)

    m.send_line("echo hello-from-ash")
    m.expect(re.compile(r"^hello-from-ash$", re.MULTILINE), 20)
    m.expect(r"~ # ", 15)

    m.send_line("pwd")
    m.expect(re.compile(r"^/$", re.MULTILINE), 15)
    m.expect(r"~ # ", 15)

    m.send_line("export FOO=bar; echo $FOO")
    m.expect(re.compile(r"^bar$", re.MULTILINE), 15)
    m.expect(r"~ # ", 15)

    m.send_line("echo $((6 * 7))")
    m.expect(re.compile(r"^42$", re.MULTILINE), 15)
    m.expect(r"~ # ", 15)

    m.send_line("for i in 1 2 3; do echo loop-$i; done")
    m.expect(re.compile(r"^loop-1\nloop-2\nloop-3$", re.MULTILINE), 15)
    m.expect(r"~ # ", 15)

    m.send_line("exit 0")
    m.expect(r"linux: busybox exit=0 leaked_frames=0\n", 20)
    m.expect(r"lumen> ", 10)

    assert CRASH_BEGIN not in m.output
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_dynamic_busybox_from_rootfs(boot, tmp_path):
    """`linux /path` loads binaries from the ext4 rootfs: Alpine's dynamic
    `/bin/busybox` runs through its `PT_INTERP` interpreter
    (`/lib/ld-musl-x86_64.so.1`, mapped at INTERP_BASE with AT_BASE set),
    and the static `hello` (`build/linux/hello`, copied into the rootfs's
    `/usr/local/bin`) runs by path too, each exiting 0 with zero leaked
    frames; a symlink to the interpreter is followed and runs it directly
    (no PT_INTERP: no AT_BASE); a path naming no file is refused with ENOENT,
    and a file without an execute bit with EACCES."""
    disk = copy_rootfs(tmp_path)
    m = boot_shell(boot, devices=Devices(blk_image=disk, rng=True))

    m.send_line("linux /bin/busybox echo hi from ld-musl")
    m.expect(re.compile(r"^hi from ld-musl\r?$", re.MULTILINE), 30)
    m.expect(r"linux: /bin/busybox exit=0 leaked_frames=0\n", 30)
    m.expect(r"lumen> ", 10)

    m.send_line("linux /bin/no-such-binary")
    m.expect(r"linux: cannot read '/bin/no-such-binary': ENOENT \(2\)\n", 10)
    m.expect(r"lumen> ", 10)

    # A regular file without an execute bit (0644) is refused, as execve(2).
    m.send_line("linux /etc/passwd")
    m.expect(r"linux: cannot read '/etc/passwd': EACCES \(13\)\n", 10)
    m.expect(r"lumen> ", 10)

    m.send_line("linux /usr/local/bin/hello")
    m.expect(r"hello from static musl on lumen\n", 20)
    m.expect(r"linux: /usr/local/bin/hello exit=0 leaked_frames=0\n", 20)
    m.expect(r"lumen> ", 10)

    # A symlink to the interpreter (Alpine's libc.musl -> ld-musl), followed
    # and run directly: an ET_DYN without PT_INTERP, so ld-musl starts as a
    # program, with no program to load it prints its usage and exits 1.
    m.send_line("linux /lib/libc.musl-x86_64.so.1")
    m.expect(r"musl libc \(x86_64\)", 20)
    m.expect(r"linux: /lib/libc.musl-x86_64.so.1 exit=1 leaked_frames=0\n", 20)
    m.expect(r"lumen> ", 10)

    assert CRASH_BEGIN not in m.output
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
