"""Linux x86-64 ring-3 ABI scenarios: entry/exit, syscall/sysretq, SWAPGS,
IST_DEBUG (CVE-2018-8897), non-fatal ring-3 faults, per-process AddressSpace /
demand paging / SSE+TLS preemption, 30-syscall table, static musl hello, and
unmodified Alpine static musl busybox ash built-ins (M6, issues #236-#238), and
binaries loaded by path from the ext4 rootfs, dynamic ones through PT_INTERP,
and a dynamic musl program's dlopen and file mmap from ring 3 (#255)."""

import re

from lumen_harness.crash import BEGIN as CRASH_BEGIN
from lumen_harness.qemu import EXIT_SUCCESS, Devices
from lumen_harness.rootfs import copy_rootfs, e2fsck_check, warm_up
from lumen_harness.shell import run

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
        sig = {14: 11, 6: 4, 13: 11, 3: 5, 1: 5}[vec]
        m.send_line(f"usertest {kind}")
        line = m.expect(
            rf"usertest: exit={128 + sig} signal={sig} vector={vec} \([^)]+\) error=0x[0-9a-f]+ "
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
        r"usertest: (?:exit=133 signal=5 vector=1 \(#DB debug\) error=0x0 rip=0x[0-9a-f]+ cr2=0x0 "
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
        m.expect(r"usertest: exit=139 signal=11 non-canonical rip=0x800000000000\n", 15)
        m.expect(r"lumen> ", 10)
        chunk = m.output[start:]
        assert CRASH_BEGIN not in chunk, chunk
        warns = re.findall(r"WARN  user: pid \d+: killed by signal 11: NonCanonicalRip", chunk)
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
    and returns cleanly to the Lumen shell with zero leaked frames. On exit it
    appends its history to `$HOME/.ash_history` through `open`/`write` (#254),
    created here first so the page it keeps isn't counted as leaked."""
    m = boot_shell(boot, devices=Devices(rng=True))

    m.send_line("vfstest write /.ash_history x")
    m.expect(r"wrote 1\n", 10)
    m.expect(r"lumen> ", 10)

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

    # Appended after the "x" (no newline there): the first line runs on.
    m.send_line("vfstest cat /.ash_history")
    m.expect(re.compile(r"^xecho hello-from-ash\npwd\n", re.MULTILINE), 10)
    m.expect(r"lumen> ", 10)

    assert CRASH_BEGIN not in m.output
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_busybox_file_syscalls(boot):
    """Static musl busybox applets reach files through the VFS-backed FD
    table (#254): `pwd` (`getcwd`), `cat` of a relative path (`open` from
    the cwd, `read`, `close`) and of `/proc/version`, `head -c` (`read`,
    then `lseek` back), `tail -c` (`lseek` to `SEEK_END` and back), `od`
    (`readv` on a file), `uuencode` (`fstat`'s mode), `tee` (`open` with
    `O_TRUNC` of an existing file, a `write` to it) and `cat` of a missing file (ENOENT);
    `stat` (musl's `lstat`, and `stat` with `-L`: `SYS_lstat` and
    `SYS_stat`, both shims over `newfstatat`) of a
    file and of a symlink to it, and `which` of a path (`access` with
    `X_OK`, then `stat`) on a file with and without an execute bit;
    console stdio unchanged throughout."""
    m = boot_shell(boot, devices=Devices(rng=True))

    def run(cmd, *patterns, leaked=r"0"):
        m.send_line(cmd)
        for pattern in patterns:
            m.expect(pattern, 20)
        m.expect(rf"linux: busybox exit=0 leaked_frames={leaked}\n", 20)
        m.expect(r"lumen> ", 10)

    m.send_line("vfstest write /tmp/w 0123456789abcdef")
    m.expect(r"wrote 16\n", 10)
    m.expect(r"lumen> ", 10)
    m.send_line("vfstest chmod /tmp/w 640")
    m.expect(r"chmod: ok\n", 10)
    m.expect(r"lumen> ", 10)

    run("linux busybox pwd", re.compile(r"^/\r?\n", re.MULTILINE))
    run("linux busybox cat tmp/w", re.compile(r"\n0123456789abcdef(?=linux: )"))
    run(
        "linux busybox cat /proc/version",
        re.compile(r"^Linux version 6\.6\.0-lumen ", re.MULTILINE),
    )
    # No newline at the end of the file (nor of these): the next line
    # follows on.
    run("linux busybox head -c 4 /tmp/w", re.compile(r"\n0123(?=linux: )"))
    run("linux busybox tail -c 5 /tmp/w", re.compile(r"\nbcdef(?=linux: )"))
    run(
        "linux busybox od -c /tmp/w",
        re.compile(r"^0000000 +0 +1 +2 +3 +4 +5 +6 +7 +8 +9 +a +b +c +d +e +f\r?\n", re.MULTILINE),
    )
    run("linux busybox uuencode /tmp/w w", re.compile(r"^begin 640 w\r?\n", re.MULTILINE))

    # `stat` (#254 cluster B): busybox's applet `lstat`s unless `-L`.
    m.send_line("vfstest symlink /tmp/wl w")
    m.expect(r"symlink: ok\n", 10)
    m.expect(r"lumen> ", 10)
    run(
        "linux busybox stat -c '%s %a %F' /tmp/w",
        re.compile(r"^16 640 regular file\r?\n", re.MULTILINE),
    )
    run("linux busybox stat -c '%s %F' /tmp/wl", re.compile(r"^1 symbolic link\r?\n", re.MULTILINE))
    run(
        "linux busybox stat -L -c '%s %a %F' /tmp/wl",
        re.compile(r"^16 640 regular file\r?\n", re.MULTILINE),
    )
    # `which` of a path: `access(X_OK)`, then `stat`. Root passes `X_OK`
    # only with an execute bit set.
    m.send_line("vfstest write /tmp/x exec")
    m.expect(r"wrote 4\n", 10)
    m.expect(r"lumen> ", 10)
    m.send_line("vfstest chmod /tmp/x 744")
    m.expect(r"chmod: ok\n", 10)
    m.expect(r"lumen> ", 10)
    run("linux busybox which /tmp/x", re.compile(r"^/tmp/x\r?\n", re.MULTILINE))
    m.send_line("linux busybox which /tmp/w")
    m.expect(r"linux: busybox exit=1 leaked_frames=0\n", 20)
    m.expect(r"lumen> ", 10)

    # `tee` copies its input (a line, then Ctrl-D) to stdout and to the
    # file, which it opens `O_TRUNC`: the longer text there before goes, and
    # the page the file already held is all it keeps.
    m.send_line("vfstest write /tmp/t this-text-is-longer-than-the-line-tee-writes")
    m.expect(r"wrote 44\n", 10)
    m.expect(r"lumen> ", 10)
    m.send_line("linux busybox tee /tmp/t")
    m.send_line("tee-line-one")
    m.expect(re.compile(r"^tee-line-one\r?\n", re.MULTILINE), 20)
    m.send("\x04")
    m.expect(r"linux: busybox exit=0 leaked_frames=0\n", 20)
    m.expect(r"lumen> ", 10)
    m.send_line("vfstest cat /tmp/t")
    m.expect(re.compile(r"cat /tmp/t\r?\ntee-line-one\r?\nlumen> "), 10)

    m.send_line("linux busybox cat /no-such-file")
    m.expect(r"cat: can't open '/no-such-file': No such file or directory", 20)
    m.expect(r"linux: busybox exit=1 leaked_frames=0\n", 20)
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


M7_TEST = "/usr/local/bin/m7_test"
# One maps line (`lumen_vm::maps`, as Linux's `show_map_vma`): range,
# permissions, offset, device, inode, then the name from column 73.
MAPS_LINE = re.compile(
    r"([0-9a-f]{8,})-([0-9a-f]{8,}) ([r-][w-][x-][ps]) ([0-9a-f]{8}) "
    r"([0-9a-f]{2}:[0-9a-f]{2}) (\d+)(?: +(\S.*))?"
)


def m7_run(m, args):
    """`linux /usr/local/bin/m7_test <args>` on `m`: its output between its
    first line and the closing `linux:` line, which must say it exited 0
    with no frames leaked."""
    out = run(m, f"linux {M7_TEST} {args}".rstrip(), 60)
    lines = out.splitlines()
    assert lines and lines[0] == "m7_test: dynamic musl start", out
    assert lines[-1] == f"linux: {M7_TEST} exit=0 leaked_frames=0", out
    return lines[1:-1]


def vfs_stat(m, path):
    """`vfstest stat`'s `key=value` fields."""
    out = run(m, f"vfstest stat {path}")
    return dict(f.split("=", 1) for f in out.split() if "=" in f)


def test_dynamic_musl_binary_and_dlopen(boot, tmp_path):
    """A dynamic musl program (`m7_test`, from `tools/linux-m7/`) runs off
    the ext4 rootfs through `/lib/ld-musl-x86_64.so.1`, which `mmap`s
    `libm7.so` from its descriptor for `dlopen`; `dlsym` finds its two
    functions. Then, from ring 3, each mode in its own process (every one
    `dlopen`s first):

    - `MAP_PRIVATE` copy-on-write: a store stays the mapping's (the file
      and a shared mapping of it unchanged), and a later `pwrite` doesn't
      reach the copied page;
    - an `O_RDONLY` descriptor's `MAP_SHARED` mapping can't be made
      writable (`mprotect` and `mmap` fail `EACCES`), a private one can;
    - `ftruncate` shrinking a mapped file invalidates the PTE past the new
      end (regrown, the page reads zeroes through the mapping, not its old
      bytes); a page touched, truncated away and touched again is a bus
      error (exit 135) at that address, its frame freed;
    - `/proc/self/maps` names the binary's, `libm7.so`'s and ld-musl's
      lines with the files' device and inode (as `vfstest stat` gives
      them) and paths from column 73, each file's lines private, each with
      an `r-xp` code line, and has a `[stack]` line;
    - `MAP_SHARED` is coherent with `pread`/`pwrite` both ways and
      `msync(MS_SYNC)` puts the stores on the disk: the shell then exits
      without a `sync`, the image passes `e2fsck -fn`, and the next boot
      reads them back.

    Every run leaks zero frames, but the bus error's -1: the page the
    truncation freed: every count exact, the kernel stack's page tables
    included (#300: after a warm-up run, each run's stack reuses the last
    one's addresses, so no run maps a new table).
    The data files (and `libm7.so`'s one page) are in the page cache before
    the first run, so no run counts a cached page."""
    disk = copy_rootfs(tmp_path)
    m = boot_shell(boot, devices=Devices(blk_image=disk, rng=True))
    assert run(m, "vfstest write-pattern /m7-shared 8192") == "wrote 8192\n"
    assert run(m, "vfstest write-pattern /m7-data 8192") == "wrote 8192\n"
    assert run(m, "vfstest write-pattern /tmp/m7-bus 8192") == "wrote 8192\n"
    assert re.fullmatch(r"read: \d+\n", run(m, "vfstest read /usr/local/lib/libm7.so 8192"))
    # The files' blocks on the disk before the stores, for the reboot.
    assert run(m, "sync", 30) == "sync: ok\n"
    warm_up(m)

    assert m7_run(m, "") == ["m7_test: dlopen/dlsym ok"]
    assert m7_run(m, "private /m7-data") == ["m7_test: private ok"]
    assert m7_run(m, "mprotect /m7-data") == ["m7_test: mprotect ok"]
    assert m7_run(m, "truncate /m7-data") == ["m7_test: truncate ok"]

    # A bus error: the WARN it logs is the point here.
    out = run(
        m,
        f"linux {M7_TEST} sigbus /tmp/m7-bus",
        60,
        allow=r"WARN  user: pid \d+: killed by signal 7: BusError",
    )
    touched = re.search(r"^m7_test: sigbus: touching (0x[0-9a-f]+) past EOF$", out, re.MULTILINE)
    assert touched, out
    bus = re.search(
        rf"^linux: {M7_TEST} exit=135 signal=7 rip=0x[0-9a-f]+ "
        rf"cr2={touched.group(1)} leaked_frames=-1$",
        out,
        re.MULTILINE,
    )
    assert bus, out
    assert vfs_stat(m, "/tmp/m7-bus")["size"] == "4096"

    lines = m7_run(m, "maps")
    assert lines[0] == "m7_test: maps begin" and lines[-2:] == [
        "m7_test: maps end",
        "m7_test: maps ok",
    ]
    maps = lines[1:-2]
    by_name = {}
    for line in maps:
        match = MAPS_LINE.fullmatch(line)
        assert match, f"not a maps line: {line!r}"
        if match.group(7):
            assert match.start(7) == 73, f"name not at column 73: {line!r}"
        by_name.setdefault(match.group(7), []).append(match)
    for path in (M7_TEST, "/usr/local/lib/libm7.so", "/lib/ld-musl-x86_64.so.1"):
        rows = by_name.get(path)
        assert rows, f"no maps line for {path}:\n" + "\n".join(maps)
        st = vfs_stat(m, path)
        major, minor = (int(n) for n in st["dev"].split(":"))
        for row in rows:
            assert row.group(5) == f"{major:02x}:{minor:02x}", row.group(0)
            assert row.group(6) == st["ino"], row.group(0)
            assert row.group(3).endswith("p"), row.group(0)
        assert any(row.group(3) == "r-xp" for row in rows), f"{path}: no code line"
    assert "[stack]" in by_name, "\n".join(maps)

    assert m7_run(m, "shared /m7-shared") == ["m7_test: shared ok"]
    assert CRASH_BEGIN not in m.output
    # No `sync` (and `exit` makes none): only msync(MS_SYNC) wrote the stores.
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
    e2fsck_check(disk)

    m2 = boot_shell(boot, devices=Devices(blk_image=disk, rng=True))
    assert run(m2, "vfstest read /m7-shared 8192") == "read: 8192\n"
    warm_up(m2)
    out = run(m2, "linux /bin/busybox strings -n 8 /m7-shared", 60)
    # A printable pattern byte may run on before a store: matched, not split.
    assert re.findall(r"m7 [a-z ]+", out) == [
        "m7 shared store via mapping",
        "m7 pwrite into mapped page",
        "m7 shared page one",
    ], out
    assert out.endswith("linux: /bin/busybox exit=0 leaked_frames=0\n"), out
    m2.send_line("exit")
    assert m2.wait_exit(10) == EXIT_SUCCESS
