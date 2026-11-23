"""Self-tests for Alpine minirootfs harness helpers and `--rootfs` / `Makefile` wiring.

All tests are self-contained (`tmp_path`/`monkeypatch`) and never require
`build/deps/` or `build/alpine-rootfs.ext4` during `make test-host`.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

import pytest
from lumen_harness import REPO_ROOT, rootfs
from lumen_harness import __main__ as cli
from lumen_harness.qemu import qemu_args


def _make_exec(path: Path, body: str = "#!/bin/sh\nexit 0\n") -> Path:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(body)
    path.chmod(0o755)
    return path


def test_find_e2fsprogs_prefers_override_then_homebrew_then_path(tmp_path, monkeypatch):
    override = tmp_path / "override"
    brew = tmp_path / "brew"
    local_brew = tmp_path / "local_brew"
    path_dir = tmp_path / "path_bin"
    for d in (override, brew, local_brew, path_dir):
        _make_exec(d / "mke2fs")
        _make_exec(d / "e2fsck")

    monkeypatch.setattr(rootfs, "DEFAULT_E2FSPROGS_DIRS", (brew, local_brew))

    assert rootfs.find_e2fsprogs(
        {"LUMEN_E2FSPROGS_SBIN": str(override), "PATH": str(path_dir)}
    ) == (override / "mke2fs", override / "e2fsck")

    assert rootfs.find_e2fsprogs({"PATH": str(path_dir)}) == (brew / "mke2fs", brew / "e2fsck")

    (brew / "e2fsck").unlink()
    assert rootfs.find_e2fsprogs({"PATH": str(path_dir)}) == (
        local_brew / "mke2fs",
        local_brew / "e2fsck",
    )

    (local_brew / "mke2fs").unlink()
    assert rootfs.find_e2fsprogs({"PATH": str(path_dir)}) == (
        path_dir / "mke2fs",
        path_dir / "e2fsck",
    )


def test_find_e2fsprogs_skips_mke2fs_without_sibling_e2fsck(tmp_path, monkeypatch):
    android_sdk = tmp_path / "android_platform_tools"
    non_exec_dir = tmp_path / "non_exec"
    good_dir = tmp_path / "good_sbin"

    _make_exec(android_sdk / "mke2fs")
    _make_exec(non_exec_dir / "mke2fs")
    (non_exec_dir / "e2fsck").write_text("#!/bin/sh\nexit 0\n")
    (non_exec_dir / "e2fsck").chmod(0o644)
    _make_exec(good_dir / "mke2fs")
    _make_exec(good_dir / "e2fsck")

    monkeypatch.setattr(rootfs, "DEFAULT_E2FSPROGS_DIRS", ())
    env = {"PATH": os.pathsep.join(["", str(android_sdk), str(non_exec_dir), str(good_dir)])}
    assert rootfs.find_e2fsprogs(env) == (good_dir / "mke2fs", good_dir / "e2fsck")

    with pytest.raises(FileNotFoundError, match="matched \\(mke2fs, e2fsck\\)"):
        rootfs.find_e2fsprogs({"PATH": str(android_sdk)})


def test_find_e2fsprogs_rejects_bad_override_and_ignores_empty(tmp_path, monkeypatch):
    brew = tmp_path / "brew"
    _make_exec(brew / "mke2fs")
    _make_exec(brew / "e2fsck")
    monkeypatch.setattr(rootfs, "DEFAULT_E2FSPROGS_DIRS", (brew,))

    assert rootfs.find_e2fsprogs({"LUMEN_E2FSPROGS_SBIN": "", "PATH": ""}) == (
        brew / "mke2fs",
        brew / "e2fsck",
    )

    bad = tmp_path / "bad_override"
    _make_exec(bad / "mke2fs")
    with pytest.raises(FileNotFoundError, match="LUMEN_E2FSPROGS_SBIN"):
        rootfs.find_e2fsprogs({"LUMEN_E2FSPROGS_SBIN": str(bad), "PATH": ""})


def test_copy_rootfs_creates_writable_isolated_copy(tmp_path):
    master = tmp_path / "master.ext4"
    master.write_bytes(b"original-ext4-image-bytes")

    dest = rootfs.copy_rootfs(tmp_path / "worker1", src=master)
    assert dest == tmp_path / "worker1/alpine-rootfs.ext4"
    assert dest.read_bytes() == b"original-ext4-image-bytes"

    dest.write_bytes(b"mutated-in-test")
    assert master.read_bytes() == b"original-ext4-image-bytes"

    with pytest.raises(FileNotFoundError, match="make rootfs"):
        rootfs.copy_rootfs(tmp_path / "worker2", src=tmp_path / "missing.ext4")


def test_e2fsck_check_passes_and_reports_failures(tmp_path):
    img = tmp_path / "disk.ext4"
    img.write_bytes(b"\x00" * 16)
    args_log = tmp_path / "e2fsck.args"

    ok_tool = _make_exec(
        tmp_path / "e2fsck_ok",
        f'#!/bin/sh\nprintf "%s\\n" "$*" > "{args_log}"\nexit 0\n',
    )
    rootfs.e2fsck_check(img, e2fsck=ok_tool)
    assert args_log.read_text().strip() == f"-fn {img}"

    bad_tool = _make_exec(
        tmp_path / "e2fsck_bad",
        '#!/bin/sh\necho "Superblock invalid" >&2\nexit 8\n',
    )
    with pytest.raises(RuntimeError, match="Superblock invalid"):
        rootfs.e2fsck_check(img, e2fsck=bad_tool)


def test_debugfs_reads_and_fails_on_any_error_line(tmp_path):
    img = tmp_path / "disk.ext4"
    img.write_bytes(b"\x00" * 16)
    args_log = tmp_path / "debugfs.args"
    banner = "debugfs 1.47.4 (6-Mar-2025)"
    listing = "/2/040755/0/0/.//\\n/2/040755/0/0/..//\\n/12/0100644/0/0/group.sed/510/\\n/13/040755/0/0/untar//\\n"
    ok_tool = _make_exec(
        tmp_path / "debugfs_ok",
        f'#!/bin/sh\nprintf "%s\\n" "$*" > "{args_log}"\necho "{banner}" >&2\nprintf "{listing}"\n',
    )
    assert rootfs.debugfs_names(img, "/", tool=ok_tool) == {"group.sed", "untar"}
    assert args_log.read_text().strip() == f"-R ls -p / {img}"

    # debugfs exits 0 when a lookup fails, or when the image won't open.
    for error in ("/tmp: File not found by ext2_lookup ", "ls: Filesystem not open"):
        bad_tool = _make_exec(
            tmp_path / "debugfs_bad",
            f'#!/bin/sh\necho "{banner}" >&2\necho "{error}" >&2\nexit 0\n',
        )
        with pytest.raises(RuntimeError, match=error.split(":")[1].strip()):
            rootfs.debugfs(img, "ls -p /tmp", tool=bad_tool)
    failed = _make_exec(tmp_path / "debugfs_exit", "#!/bin/sh\nexit 1\n")
    with pytest.raises(RuntimeError, match="exit 1"):
        rootfs.debugfs(img, "cat /x", tool=failed)


def _sample_readobj(interp: str | None, needed: list[str]) -> str:
    lines = [
        "Program Headers:",
        "  Type           Offset   VirtAddr           PhysAddr           FileSiz  MemSiz   Flg Align",
    ]
    if interp is not None:
        lines += [
            "  INTERP         0x000238 0x0000000000000238 0x0000000000000238 0x000019 0x000019 R   0x1",
            f"      [Requesting program interpreter: {interp}]",
        ]
    lines += [
        "Dynamic section at offset 0x8c0 contains 2 entries:",
        "  Tag                Type        Name/Value",
    ]
    for lib in needed:
        lines.append(f"  0x0000000000000001 (NEEDED)    Shared library: [{lib}]")
    lines.append("  0x0000000000000000 (NULL)      0x0")
    return "\n".join(lines) + "\n"


def test_parse_and_validate_dynamic_elf(tmp_path):
    staged = tmp_path / "rootfs"
    (staged / "lib").mkdir(parents=True)
    (staged / "usr/local/lib").mkdir(parents=True)
    (staged / "lib/ld-musl-x86_64.so.1").write_bytes(b"ld-musl")
    (staged / "lib/libc.musl-x86_64.so.1").symlink_to("ld-musl-x86_64.so.1")
    (staged / "usr/local/lib/libm7.so").write_bytes(b"libm7")

    exe = tmp_path / "m7_test"
    exe.write_bytes(b"\x7fELF")

    parsed = rootfs.parse_gnu_readobj(
        _sample_readobj("/lib/ld-musl-x86_64.so.1", ["libc.musl-x86_64.so.1"])
    )
    assert parsed == ("/lib/ld-musl-x86_64.so.1", ("libc.musl-x86_64.so.1",))

    def stub_readobj(interp: str | None, needed: list[str], rc: int = 0) -> str:
        out = tmp_path / "readobj_out.txt"
        out.write_text(_sample_readobj(interp, needed))
        tool = _make_exec(
            tmp_path / "llvm-readobj",
            f'#!/bin/sh\ncat "{out}"\nexit {rc}\n',
        )
        return str(tool)

    assert rootfs.validate_dynamic_elf(
        exe,
        staged,
        expect_interp=True,
        readobj=stub_readobj("/lib/ld-musl-x86_64.so.1", ["libc.musl-x86_64.so.1"]),
    ) == ("/lib/ld-musl-x86_64.so.1", ("libc.musl-x86_64.so.1",))

    assert rootfs.validate_dynamic_elf(
        exe,
        staged,
        expect_interp=False,
        readobj=stub_readobj(None, []),
    ) == (None, ())

    with pytest.raises(ValueError, match="expected PT_INTERP"):
        rootfs.validate_dynamic_elf(exe, staged, expect_interp=True, readobj=stub_readobj(None, []))

    with pytest.raises(ValueError, match="expected no PT_INTERP"):
        rootfs.validate_dynamic_elf(
            exe,
            staged,
            expect_interp=False,
            readobj=stub_readobj("/lib/ld-musl-x86_64.so.1", []),
        )

    for bad_lib in ("libgcc_s.so.1", "libc.so.6"):
        with pytest.raises(ValueError, match="forbidden DT_NEEDED"):
            rootfs.validate_dynamic_elf(
                exe,
                staged,
                expect_interp=True,
                readobj=stub_readobj("/lib/ld-musl-x86_64.so.1", [bad_lib]),
            )

    with pytest.raises(ValueError, match="unresolved DT_NEEDED"):
        rootfs.validate_dynamic_elf(
            exe,
            staged,
            expect_interp=True,
            readobj=stub_readobj("/lib/ld-musl-x86_64.so.1", ["libmissing.so.1"]),
        )

    with pytest.raises(RuntimeError, match="llvm-readobj failed"):
        rootfs.read_dynamic_elf(exe, readobj=stub_readobj(None, [], rc=2))

    (staged / "lib/ld-musl-x86_64.so.1").unlink()
    with pytest.raises(ValueError, match="missing in"):
        rootfs.validate_dynamic_elf(
            exe,
            staged,
            expect_interp=True,
            readobj=stub_readobj("/lib/ld-musl-x86_64.so.1", []),
        )


def test_rootfs_symlink_resolution_stays_anchored_to_rootfs(tmp_path):
    staged = tmp_path / "rootfs"
    (staged / "lib").mkdir(parents=True)
    (staged / "usr/lib").mkdir(parents=True)
    (staged / "usr/local/lib").mkdir(parents=True)

    host_only = tmp_path / "host_only.so"
    host_only.write_bytes(b"host-file")
    host_interp = tmp_path / "host_ld.so"
    host_interp.write_bytes(b"host-interp")

    (staged / "lib/ld-musl-x86_64.so.1").write_bytes(b"ld-musl")
    (staged / "usr/local/lib/libreal.so").write_bytes(b"real-lib")
    # Multi-hop relative + absolute in-rootfs symlink chain.
    (staged / "usr/lib/libhop.so").symlink_to("../local/lib/libreal.so")
    (staged / "lib/libchain.so").symlink_to("/usr/lib/libhop.so")
    # Absolute symlinks targeting existing host paths absent inside staged rootfs.
    (staged / "lib/libhost_abs.so").symlink_to(str(host_only))
    (staged / "lib/libhost_escape.so").symlink_to(f"../../{host_only.name}")
    # Symlink cycle.
    (staged / "lib/libloop_a.so").symlink_to("libloop_b.so")
    (staged / "lib/libloop_b.so").symlink_to("libloop_a.so")

    exe = tmp_path / "m7_test"
    exe.write_bytes(b"\x7fELF")
    out = tmp_path / "readobj_symlink.txt"

    def stub_readobj(interp: str | None, needed: list[str]) -> str:
        out.write_text(_sample_readobj(interp, needed))
        return str(_make_exec(tmp_path / "llvm-readobj-sym", f'#!/bin/sh\ncat "{out}"\n'))

    assert rootfs.validate_dynamic_elf(
        exe,
        staged,
        expect_interp=True,
        readobj=stub_readobj("/lib/ld-musl-x86_64.so.1", ["libchain.so"]),
    ) == ("/lib/ld-musl-x86_64.so.1", ("libchain.so",))

    for bad_needed in ("libhost_abs.so", "libhost_escape.so", "libloop_a.so"):
        with pytest.raises(ValueError, match="unresolved DT_NEEDED"):
            rootfs.validate_dynamic_elf(
                exe,
                staged,
                expect_interp=True,
                readobj=stub_readobj("/lib/ld-musl-x86_64.so.1", [bad_needed]),
            )

    (staged / "lib/ld-musl-x86_64.so.1").unlink()
    (staged / "lib/ld-musl-x86_64.so.1").symlink_to(str(host_interp))
    with pytest.raises(ValueError, match="PT_INTERP .* missing in"):
        rootfs.validate_dynamic_elf(
            exe,
            staged,
            expect_interp=True,
            readobj=stub_readobj("/lib/ld-musl-x86_64.so.1", []),
        )


def test_run_options_and_cmd_run_accept_rootfs(tmp_path, monkeypatch):
    img = tmp_path / "alpine-rootfs.ext4"
    img.write_bytes(b"ext4")

    opts = cli.run_options(Path("x.iso"), Path("v.fd"), headless=True, rootfs=img)
    args = qemu_args(opts)
    assert f"if=none,id=lumen-disk,format=raw,file={img}" in args
    assert "virtio-blk-pci,id=lumen-blk,drive=lumen-disk" in args

    ran: list[list[str]] = []
    monkeypatch.setattr(cli, "BUILD_DIR", tmp_path)
    monkeypatch.setattr(cli, "fresh_vars", lambda dest: tmp_path / "vars.fd")
    monkeypatch.setattr(cli.subprocess, "call", lambda argv: ran.append(argv) or 0)
    monkeypatch.setattr(
        sys,
        "argv",
        ["lumen_harness", "run", "--iso", str(tmp_path / "x.iso"), "--rootfs", str(img)],
    )
    assert cli.main() == 0
    assert "virtio-blk-pci,id=lumen-blk,drive=lumen-disk" in ran[0]

    with pytest.raises(SystemExit, match="make rootfs"):
        cli.cmd_run(
            argparse.Namespace(
                iso=tmp_path / "x.iso",
                headless=True,
                sound=False,
                rootfs=tmp_path / "missing.ext4",
            )
        )

    inherited = {"MAKEFLAGS", "MFLAGS", "MAKELEVEL", "MAKEOVERRIDES", "SCENARIO_JOBS"}
    clean_env = {k: v for k, v in os.environ.items() if k not in inherited}

    dry_scenarios = subprocess.run(
        ["make", "-n", "-W", "tools/mkrootfs.sh", "-C", str(REPO_ROOT), "test-scenarios"],
        capture_output=True,
        text=True,
        check=False,
        env=clean_env,
    )
    assert dry_scenarios.returncode == 0, dry_scenarios.stderr
    assert "tools/mkrootfs.sh build/alpine-rootfs.ext4" in dry_scenarios.stdout

    dry_run = subprocess.run(
        ["make", "-n", "-C", str(REPO_ROOT), "run", "ROOTFS=1"],
        capture_output=True,
        text=True,
        check=False,
        env=clean_env,
    )
    assert dry_run.returncode == 0, dry_run.stderr
    assert "--rootfs build/alpine-rootfs.ext4" in dry_run.stdout


def test_copy_rootfs_rejects_invalid_names(tmp_path):
    master = tmp_path / "master.ext4"
    master.write_bytes(b"original-ext4-image-bytes")

    for bad_name in (
        "",
        ".",
        "..",
        "../escape.ext4",
        "sub/rootfs.ext4",
        "sub\\rootfs.ext4",
        "/abs.ext4",
    ):
        with pytest.raises(ValueError, match="invalid rootfs filename"):
            rootfs.copy_rootfs(tmp_path / "worker", src=master, name=bad_name)

    assert not (tmp_path / "escape.ext4").exists()
    assert not (tmp_path / "worker").exists()


def test_resolve_in_rootfs_rejects_non_directory_parent_traversal(tmp_path):
    staged = tmp_path / "rootfs"
    (staged / "bin").mkdir(parents=True)
    (staged / "bin/sub").mkdir()
    (staged / "lib").mkdir()
    (staged / "bin/busybox").write_bytes(b"bb")
    (staged / "bin/sh").symlink_to("busybox")
    (staged / "lib/ld-musl-x86_64.so.1").write_bytes(b"ld")

    assert rootfs._resolve_in_rootfs(staged, "/bin/busybox/../sh") is None
    assert rootfs._resolve_in_rootfs(staged, "/bin/sh/../busybox") is None
    assert rootfs._resolve_in_rootfs(staged, "/bin/sub/../busybox") == staged / "bin/busybox"
    assert rootfs._resolve_in_rootfs(staged, "/../lib/ld-musl-x86_64.so.1") == (
        staged / "lib/ld-musl-x86_64.so.1"
    )


def test_rootfs_cli_main_exit_codes(tmp_path, monkeypatch, capsys):
    staged = tmp_path / "rootfs"
    (staged / "lib").mkdir(parents=True)
    (staged / "lib/ld-musl-x86_64.so.1").write_bytes(b"ld-musl")
    (staged / "lib/libc.musl-x86_64.so.1").symlink_to("ld-musl-x86_64.so.1")

    exe = tmp_path / "m7_test"
    exe.write_bytes(b"\x7fELF")
    so = tmp_path / "libm7.so"
    so.write_bytes(b"\x7fELF")

    readobj_out = tmp_path / "readobj_cli.txt"
    stub_tool = _make_exec(
        tmp_path / "llvm-readobj",
        f'#!/bin/sh\ncat "{readobj_out}"\n',
    )
    monkeypatch.setattr(rootfs, "llvm_tool", lambda name: str(stub_tool))

    readobj_out.write_text(_sample_readobj("/lib/ld-musl-x86_64.so.1", ["libc.musl-x86_64.so.1"]))
    assert rootfs.main([str(exe), "--rootfs-dir", str(staged)]) == 0
    out = capsys.readouterr().out
    assert "m7_test ok (interp=/lib/ld-musl-x86_64.so.1" in out

    readobj_out.write_text(_sample_readobj(None, []))
    assert rootfs.main([str(so), "--rootfs-dir", str(staged), "--no-interp"]) == 0
    out = capsys.readouterr().out
    assert "libm7.so ok (interp=None, needed=[])" in out

    # Missing expected PT_INTERP triggers non-zero exit code 1.
    assert rootfs.main([str(exe), "--rootfs-dir", str(staged)]) == 1
    err = capsys.readouterr().err
    assert "rootfs check-elf:" in err
    assert "expected PT_INTERP" in err


SIGNED_HASH = REPO_ROOT / "tools" / "ext4-signed-hash.sh"
UNSIGNED_IMAGE = REPO_ROOT / "kernel" / "src" / "vfs" / "testdata" / "unsigned-hash.ext4"
S_FLAGS_AT = 1024 + 0x160


def _e2fsprogs() -> tuple[Path, Path, Path]:
    """The real (mke2fs, e2fsck, debugfs): skipped locally without them,
    failed in CI (which installs them)."""
    try:
        mke2fs, e2fsck = rootfs.find_e2fsprogs()
    except FileNotFoundError:
        if os.environ.get("CI"):
            raise
        pytest.skip("e2fsprogs not installed")
    debugfs = e2fsck.parent / "debugfs"
    if not os.access(debugfs, os.X_OK):
        if os.environ.get("CI"):
            raise FileNotFoundError(f"no debugfs beside {e2fsck}")
        pytest.skip(f"no debugfs beside {e2fsck}")
    return mke2fs, e2fsck, debugfs


def _sign(debugfs: Path, image: Path) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [str(SIGNED_HASH), str(debugfs), str(image)],
        capture_output=True,
        text=True,
        check=False,
        env={**os.environ, "E2FSPROGS_FAKE_TIME": "1700000000"},
    )


def _s_flags(image: Path) -> int:
    return int.from_bytes(image.read_bytes()[S_FLAGS_AT : S_FLAGS_AT + 4], "little")


def test_signed_hash_switches_only_the_flag(tmp_path):
    """#308: mkrootfs.sh's switch on an image with unsigned directory hashes,
    or with neither hash flag, sets s_flags to signed and changes nothing
    else; an image already signed is left as it is, without running
    debugfs."""
    _, e2fsck, debugfs = _e2fsprogs()
    image = tmp_path / "unsigned.ext4"
    image.write_bytes(UNSIGNED_IMAGE.read_bytes())
    assert _s_flags(image) == 0x2

    proc = _sign(debugfs, image)
    assert proc.returncode == 0, proc.stderr
    assert "had unsigned directory hashes (s_flags 2); set s_flags to 1" in proc.stderr
    assert _s_flags(image) == 0x1
    before, after = UNSIGNED_IMAGE.read_bytes(), image.read_bytes()
    assert [i for i in range(len(before)) if before[i] != after[i]] == [S_FLAGS_AT]
    # debugfs reads the same byte as the flag (S_FLAGS_AT is right).
    stats = rootfs.debugfs(image, "stats", tool=debugfs)
    assert re.search(r"^Filesystem flags:\s+signed_directory_hash\s*$", stats, re.MULTILINE), stats
    # Only the flag: this image's htree `/d` is still ordered by the unsigned
    # hash, and its names with bytes of 0x80 or above now sit in leaves their
    # signed hashes don't lead to, which `e2fsck -fn` (mkrootfs.sh's last
    # check) reports.
    with pytest.raises(RuntimeError, match="exit 4"):
        rootfs.e2fsck_check(image, e2fsck=e2fsck)

    # s_flags' other bits are kept: 0x6 (unsigned and test_filesys) -> 0x5.
    other = bytearray(UNSIGNED_IMAGE.read_bytes())
    other[S_FLAGS_AT] = 0x6
    image.write_bytes(other)
    proc = _sign(debugfs, image)
    assert proc.returncode == 0, proc.stderr
    assert "had unsigned directory hashes (s_flags 6); set s_flags to 5" in proc.stderr
    after = image.read_bytes()
    assert [i for i in range(len(other)) if other[i] != after[i]] == [S_FLAGS_AT]
    assert _s_flags(image) == 0x5

    # Neither flag (which the kernel mounts read-only beside dir_index): set
    # to signed too.
    neither = bytearray(UNSIGNED_IMAGE.read_bytes())
    neither[S_FLAGS_AT] = 0x0
    image.write_bytes(neither)
    proc = _sign(debugfs, image)
    assert proc.returncode == 0, proc.stderr
    assert "had no directory hash flag (s_flags 0); set s_flags to 1" in proc.stderr
    after = image.read_bytes()
    assert [i for i in range(len(neither)) if neither[i] != after[i]] == [S_FLAGS_AT]
    assert _s_flags(image) == 0x1

    signed = image.read_bytes()
    no_debugfs = _make_exec(tmp_path / "debugfs_never", "#!/bin/sh\nexit 1\n")
    proc = _sign(no_debugfs, image)
    assert proc.returncode == 0, proc.stderr
    assert proc.stderr == ""
    assert image.read_bytes() == signed


def test_signed_hash_fails_when_the_flag_stays_unsigned(tmp_path):
    """debugfs exits 0 when `ssv` fails: an error line on its stderr fails
    the switch, and so does an image still unsigned afterwards."""
    _, _, debugfs = _e2fsprogs()
    image = tmp_path / "unsigned.ext4"
    banner = "debugfs 1.47.4 (6-Mar-2025)"
    # Writes the flag but says it failed: only the stderr check catches it.
    noisy = _make_exec(
        tmp_path / "debugfs_noisy",
        f'#!/bin/sh\n"{debugfs}" "$@" 2>/dev/null\necho "{banner}" >&2\n'
        'echo "ssv: Filesystem not open" >&2\nexit 0\n',
    )
    # Says nothing and writes nothing: only the read-back catches it.
    silent = _make_exec(tmp_path / "debugfs_silent", f'#!/bin/sh\necho "{banner}" >&2\nexit 0\n')
    failing = _make_exec(tmp_path / "debugfs_exit", "#!/bin/sh\nexit 1\n")
    for tool, why in (
        (noisy, "ssv: Filesystem not open"),
        (silent, "still doesn't say its directories hash signed (s_flags 2)"),
        (failing, "failed (exit 1)"),
    ):
        image.write_bytes(UNSIGNED_IMAGE.read_bytes())
        proc = _sign(tool, image)
        assert proc.returncode != 0, tool
        assert why in proc.stderr, proc.stderr


def test_signed_hash_refuses_a_file_without_an_ext4_superblock(tmp_path):
    """A file without ext4's magic (0xEF53 at 1024 + 0x38), or no file, fails
    the switch with its own message, before debugfs runs."""
    image = tmp_path / "zeros.img"
    image.write_bytes(bytes(64 * 1024))
    never = _make_exec(tmp_path / "debugfs_never", "#!/bin/sh\necho ran >&2\nexit 1\n")
    proc = _sign(never, image)
    assert proc.returncode == 1
    assert "has no ext4 superblock (no magic 0xEF53)" in proc.stderr
    assert "ran" not in proc.stderr
    proc = _sign(never, tmp_path / "missing.img")
    assert proc.returncode == 1
    assert "ext4-signed-hash: can't read" in proc.stderr
    assert "ran" not in proc.stderr


def _mkrootfs_with_unsigned_mke2fs(tmp_path: Path, stage_extra=None, index: bool = False):
    """tools/mkrootfs.sh run on a stub minirootfs with an `mke2fs` that makes
    the image unsigned, as it does where `char` is unsigned (aarch64 Linux),
    and with `index`, then indexes its directories by the unsigned hash
    (`e2fsck -fyD`). Returns (the process, the output image)."""
    mke2fs, e2fsck, debugfs = _e2fsprogs()
    tools = tmp_path / "tools"
    tools.mkdir()
    for name in ("mkrootfs.sh", "ext4-signed-hash.sh"):
        shutil.copy2(REPO_ROOT / "tools" / name, tools / name)
    mini = tmp_path / "build" / "deps" / "alpine-minirootfs"
    (mini / "lib").mkdir(parents=True)
    (mini / ".version").write_text("stub\n")
    (mini / "lib" / "ld-musl-x86_64.so.1").write_bytes(b"")
    _make_exec(mini / "bin" / "busybox")
    if stage_extra:
        stage_extra(mini)
    sbin = tmp_path / "sbin"
    sbin.mkdir()
    (sbin / "e2fsck").symlink_to(e2fsck)
    (sbin / "debugfs").symlink_to(debugfs)
    reindex = f'"{e2fsck}" -fyD "$img" >/dev/null 2>&1; [ $? -le 1 ] || exit 1\n' if index else ""
    _make_exec(
        sbin / "mke2fs",
        f'#!/bin/sh\n"{mke2fs}" "$@" || exit 1\n'
        # The image is the next-to-last argument (the size is the last).
        "img=; prev=\nfor arg; do img=$prev; prev=$arg; done\n"
        f'"{debugfs}" -w -R "ssv flags 2" "$img" 2>/dev/null || exit 1\n' + reindex,
    )
    out = tmp_path / "out.ext4"
    env = {
        **os.environ,
        "LUMEN_E2FSPROGS_SBIN": str(sbin),
        "ROOTFS_SIZE": "4M",
        "TMPDIR": str(tmp_path),
    }
    proc = subprocess.run(
        ["bash", str(tools / "mkrootfs.sh"), str(out)],
        capture_output=True,
        text=True,
        check=False,
        env=env,
    )
    return proc, out


def _big_dir(name_of):
    """A stage hook adding `big`, a directory of 300 files named by
    `name_of(n)`, spanning several 4 KiB blocks."""

    def add(mini: Path) -> None:
        (mini / "big").mkdir()
        for n in range(300):
            (mini / "big" / name_of(n)).write_bytes(b"x")

    return add


def _htree(image: Path, path: str) -> str:
    _, _, debugfs = _e2fsprogs()
    return subprocess.run(
        [str(debugfs), "-R", f"htree {path}", str(image)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=True,
        env={**os.environ, "LC_ALL": "C"},
    ).stdout


def test_mkrootfs_makes_an_unsigned_image_signed(tmp_path):
    """#308: mkrootfs.sh on a host whose mke2fs makes unsigned images (here a
    wrapper does) still builds an image saying its directories hash signed,
    which `e2fsck -fn` passes, so Lumen mounts its rootfs writable. Only the
    flag needed changing: `mke2fs -d` indexed no directory, not even one
    spanning several blocks."""
    proc, out = _mkrootfs_with_unsigned_mke2fs(
        tmp_path, _big_dir(lambda n: f"a-long-file-name-number-{n:03}")
    )
    assert proc.returncode == 0, proc.stderr
    assert "had unsigned directory hashes (s_flags 2); set s_flags to 1" in proc.stderr
    assert _s_flags(out) == 0x1
    assert "Not a hash-indexed directory" in _htree(out, "big")
    assert int(rootfs.debugfs(out, "stat big").split("Size: ")[1].split()[0]) > 4096


def test_mkrootfs_passes_an_ascii_directory_indexed_by_the_unsigned_hash(tmp_path):
    """A directory indexed by the unsigned hash whose names are all ASCII
    hashes alike signed, so after the switch `e2fsck -fn` passes it."""
    proc, out = _mkrootfs_with_unsigned_mke2fs(
        tmp_path, _big_dir(lambda n: f"a-long-file-name-number-{n:03}"), index=True
    )
    assert proc.returncode == 0, proc.stderr
    assert _s_flags(out) == 0x1
    assert "Root node dump" in _htree(out, "big")


def test_mkrootfs_fails_on_a_directory_indexed_by_the_unsigned_hash(tmp_path):
    """The switch changes only the flag, so an image with a directory
    indexed by the unsigned hash holding names with bytes of 0x80 or above
    fails mkrootfs.sh's final `e2fsck -fn`, naming #308, and leaves no
    output."""

    proc, out = _mkrootfs_with_unsigned_mke2fs(
        tmp_path, _big_dir(lambda n: f"é-a-long-file-name-number-{n:03}"), index=True
    )
    assert proc.returncode == 1, proc.stderr
    assert "had unsigned directory hashes (s_flags 2); set s_flags to 1" in proc.stderr
    assert "mkrootfs: e2fsck -fn failed on the image" in proc.stderr
    assert "#308" in proc.stderr
    assert not out.exists()


def test_run_busybox_splits_the_linux_line_and_refuses_output_without_it(monkeypatch):
    # `run_busybox`: the applet's output, then the `linux:` line's exit code
    # and leaked frames; no such line last is an AssertionError naming it.
    # `fake_run` stands in for `shell.run`, handing back the machine it is
    # given as the command's output: each call below passes the output as `m`.
    sent = []

    def fake_run(m, line, timeout=10):
        sent.append((line, timeout))
        return m

    monkeypatch.setattr("lumen_harness.shell.run", fake_run)
    out = "a\nb\nlinux: /bin/busybox exit=2 leaked_frames=-1\n"
    assert rootfs.run_busybox(out, "ls /x") == rootfs.BusyboxRun(
        output="a\nb\n", status=2, leaked=-1, raw=out
    )
    assert sent == [("linux /bin/busybox ls /x", 60)]
    out = "linux: /bin/busybox exit=0 leaked_frames=3\n"
    assert rootfs.run_busybox(out, "true") == rootfs.BusyboxRun(
        output="", status=0, leaked=3, raw=out
    )
    for bad in (
        "",
        "a\n",
        "linux: /bin/busybox exit=0 leaked_frames=0\nafter\n",
        "linux: /bin/busybox exit=0 leaked_frames=0 x\n",
    ):
        with pytest.raises(AssertionError, match="no `linux: /bin/busybox exit=...` line last"):
            rootfs.run_busybox(bad, "true")
