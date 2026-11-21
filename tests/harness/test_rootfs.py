"""Self-tests for Alpine minirootfs harness helpers and `--rootfs` / `Makefile` wiring.

All tests are self-contained (`tmp_path`/`monkeypatch`) and never require
`build/deps/` or `build/alpine-rootfs.ext4` during `make test-host`.
"""

from __future__ import annotations

import argparse
import os
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
