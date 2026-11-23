"""Alpine minirootfs ext4 helpers and dynamic musl ELF validation.

Used by:
- `tools/mkrootfs.sh` / `Makefile` build-time ELF checks on `build/linux/*`
- `tests/scenarios/test_fs.py`, `test_linux_user.py`, `test_alpine.py` (M7-M8):
  matched `(mke2fs, e2fsck)` discovery, per-test copy isolation of
  `build/alpine-rootfs.ext4`, post-QEMU `e2fsck -fn` integrity checks, and
  `debugfs` reads of what a guest left on the disk
- `tests/harness/test_rootfs.py`
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
from collections import deque
from collections.abc import Mapping
from pathlib import Path
from typing import NamedTuple

from . import BUILD_DIR
from .audit import llvm_tool

DEFAULT_INTERP = "/lib/ld-musl-x86_64.so.1"
FORBIDDEN_NEEDED = frozenset({"libgcc_s.so.1", "libc.so.6"})
ROOTFS_LIB_DIRS = ("lib", "usr/local/lib", "usr/lib")
DEFAULT_E2FSPROGS_DIRS = (
    Path("/opt/homebrew/opt/e2fsprogs/sbin"),
    Path("/usr/local/opt/e2fsprogs/sbin"),
)

_INTERP_RE = re.compile(r"^\s*\[Requesting program interpreter:\s*(.+?)\]\s*$")
_DEBUGFS_BANNER = re.compile(r"debugfs \S+ \(.*\)$")
_NEEDED_RE = re.compile(r"\(NEEDED\)\s+Shared library:\s+\[([^\]]+)\]")


def _pair_in(directory: Path) -> tuple[Path, Path] | None:
    mke2fs = directory / "mke2fs"
    e2fsck = directory / "e2fsck"
    if (
        mke2fs.is_file()
        and os.access(mke2fs, os.X_OK)
        and e2fsck.is_file()
        and os.access(e2fsck, os.X_OK)
    ):
        return mke2fs, e2fsck
    return None


def find_e2fsprogs(env: Mapping[str, str] | None = None) -> tuple[Path, Path]:
    """Locate a matched `(mke2fs, e2fsck)` pair in the same directory.

    Search order (matching `docs/ARCHITECTURE.md` §15):
      1. `LUMEN_E2FSPROGS_SBIN` (if non-empty; fails loudly if invalid)
      2. `/opt/homebrew/opt/e2fsprogs/sbin`
      3. `/usr/local/opt/e2fsprogs/sbin`
      4. `$PATH` directories (accepting a dir only if both tools exist there)
    """
    environ = os.environ if env is None else env
    override = environ.get("LUMEN_E2FSPROGS_SBIN", "")
    if override:
        pair = _pair_in(Path(override))
        if pair is None:
            raise FileNotFoundError(
                f"LUMEN_E2FSPROGS_SBIN={override!r} must contain executable mke2fs and e2fsck"
            )
        return pair

    for candidate in DEFAULT_E2FSPROGS_DIRS:
        pair = _pair_in(candidate)
        if pair is not None:
            return pair

    for entry in environ.get("PATH", "").split(os.pathsep):
        if not entry:
            continue
        pair = _pair_in(Path(entry))
        if pair is not None:
            return pair

    raise FileNotFoundError(
        "could not find a matched (mke2fs, e2fsck) pair in the same directory; "
        "install e2fsprogs (`brew install e2fsprogs` on macOS, "
        "`sudo apt-get install e2fsprogs` on Ubuntu/Debian) or set LUMEN_E2FSPROGS_SBIN"
    )


# A `linux /bin/busybox ...` run's last line: its exit code and leaked frames.
BUSYBOX_EXIT = re.compile(r"linux: /bin/busybox exit=(\d+) leaked_frames=(-?\d+)")


class BusyboxRun(NamedTuple):
    """One `linux /bin/busybox <args>` run: what it printed before the
    `linux:` line, and that line's exit code and leaked frames; `raw`, the
    whole output, for failure messages."""

    output: str
    status: int
    leaked: int
    raw: str


def run_busybox(m, args: str, timeout: float = 60) -> BusyboxRun:
    """Run `/bin/busybox <args>` on machine `m` (booted with a rootfs) from
    the serial shell. Raises AssertionError, with the output, unless its
    last line is the `linux:` line giving the exit code and leaked frames
    (`BUSYBOX_EXIT`)."""
    from . import shell

    raw = shell.run(m, f"linux /bin/busybox {args}", timeout)
    lines = raw.splitlines()
    done = BUSYBOX_EXIT.fullmatch(lines[-1]) if lines else None
    assert done, f"no `linux: /bin/busybox exit=...` line last:\n{raw}"
    output = "".join(line + "\n" for line in lines[:-1])
    return BusyboxRun(output, int(done.group(1)), int(done.group(2)), raw)


def warm_up(m) -> None:
    """On machine `m` (booted with a rootfs), one `linux /bin/busybox true`, leaking 0 frames or 1: the first
    process's kernel stack may find no freed slot to reuse and take fresh
    addresses, whose 68 KiB can cross into one new 2 MiB page table; every
    later one reuses the last one's slot (#300), so its count is exact."""
    done = run_busybox(m, "true")
    assert done.status == 0 and done.leaked in (0, 1), done.raw


def copy_rootfs(
    dest_dir: Path, *, src: Path | None = None, name: str = "alpine-rootfs.ext4"
) -> Path:
    """Copy the master `alpine-rootfs.ext4` into a writable per-test path."""
    if not name or name in (".", "..") or Path(name).name != name or "/" in name or "\\" in name:
        raise ValueError(f"invalid rootfs filename: {name!r}")
    master = BUILD_DIR / "alpine-rootfs.ext4" if src is None else src
    if not master.is_file():
        raise FileNotFoundError(f"rootfs image not found: {master} (run `make rootfs` first)")
    dest_dir.mkdir(parents=True, exist_ok=True)
    dest = dest_dir / name
    shutil.copyfile(master, dest)
    return dest


def e2fsck_check(image: Path, *, e2fsck: Path | None = None) -> None:
    """Validate an ext4 image non-destructively using `e2fsck -fn`."""
    tool = find_e2fsprogs()[1] if e2fsck is None else e2fsck
    proc = subprocess.run(
        [str(tool), "-fn", str(image)],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        detail = "\n".join(part for part in (proc.stdout.strip(), proc.stderr.strip()) if part)
        raise RuntimeError(
            f"e2fsck -fn failed on {image} (exit {proc.returncode})"
            + (f":\n{detail}" if detail else "")
        )


def debugfs(image: Path, request: str, *, tool: Path | None = None) -> str:
    """What `debugfs -R <request>` prints for `image`, opened read-only.
    `debugfs` exits 0 even when it can't open the image or the request
    fails, so any stderr line but its version banner is an error."""
    if tool is None:
        _, e2fsck = find_e2fsprogs()
        tool = e2fsck.parent / "debugfs"
    proc = subprocess.run(
        [str(tool), "-R", request, str(image)], capture_output=True, text=True, check=False
    )
    errors = [line for line in proc.stderr.splitlines() if line and not _DEBUGFS_BANNER.match(line)]
    if proc.returncode != 0 or errors:
        raise RuntimeError(
            f"debugfs -R {request!r} on {image} (exit {proc.returncode}): {proc.stderr}"
        )
    return proc.stdout


def debugfs_names(image: Path, directory: str, *, tool: Path | None = None) -> set[str]:
    """The names in `directory` on `image` (`debugfs`'s `ls -p`:
    `/inode/mode/uid/gid/name/size/` lines), without `.` and `..`."""
    rows = [
        line.split("/") for line in debugfs(image, f"ls -p {directory}", tool=tool).splitlines()
    ]
    return {row[5] for row in rows if len(row) > 6} - {".", ".."}


def parse_gnu_readobj(text: str) -> tuple[str | None, tuple[str, ...]]:
    """Extract `(PT_INTERP, DT_NEEDED...)` from `llvm-readobj --elf-output-style=GNU -l -d`."""
    interp: str | None = None
    needed: list[str] = []
    for line in text.splitlines():
        if m := _INTERP_RE.match(line):
            interp = m.group(1)
        elif m := _NEEDED_RE.search(line):
            needed.append(m.group(1))
    return interp, tuple(needed)


def read_dynamic_elf(
    elf_path: Path, *, readobj: str | None = None
) -> tuple[str | None, tuple[str, ...]]:
    """Run `llvm-readobj --elf-output-style=GNU -l -d` on `elf_path`."""
    tool = llvm_tool("llvm-readobj") if readobj is None else readobj
    proc = subprocess.run(
        [tool, "--elf-output-style=GNU", "-l", "-d", str(elf_path)],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        detail = "\n".join(part for part in (proc.stdout.strip(), proc.stderr.strip()) if part)
        raise RuntimeError(
            f"llvm-readobj failed on {elf_path} (exit {proc.returncode})"
            + (f":\n{detail}" if detail else "")
        )
    return parse_gnu_readobj(proc.stdout)


def _resolve_in_rootfs(rootfs_dir: Path, rootfs_path: str, *, max_hops: int = 40) -> Path | None:
    """Resolve `rootfs_path` strictly inside `rootfs_dir` without host-FS symlink escape."""
    pending = deque(part for part in rootfs_path.split("/") if part)
    resolved: list[str] = []
    hops = 0

    while pending:
        part = pending.popleft()
        if part in ("", "."):
            continue
        if part == "..":
            if resolved:
                if not rootfs_dir.joinpath(*resolved).is_dir():
                    return None
                resolved.pop()
            continue
        candidate = rootfs_dir.joinpath(*resolved, part)
        if candidate.is_symlink():
            hops += 1
            if hops > max_hops:
                return None
            target = os.readlink(candidate)
            if target.startswith("/"):
                resolved = []
            target_parts = [seg for seg in target.split("/") if seg]
            pending.extendleft(reversed(target_parts))
            continue
        if not candidate.exists():
            return None
        resolved.append(part)

    if not resolved:
        return None
    final = rootfs_dir.joinpath(*resolved)
    if not final.is_symlink() and final.is_file():
        return final
    return None


def _resolves_in_rootfs(rootfs_dir: Path, soname: str) -> bool:
    if not soname:
        return False
    if soname.startswith("/"):
        return _resolve_in_rootfs(rootfs_dir, soname) is not None
    for rel_dir in ROOTFS_LIB_DIRS:
        if _resolve_in_rootfs(rootfs_dir, f"{rel_dir}/{soname}") is not None:
            return True
    return False


def validate_dynamic_elf(
    elf_path: Path,
    rootfs_dir: Path,
    *,
    expect_interp: bool = True,
    expected_interp: str = DEFAULT_INTERP,
    readobj: str | None = None,
) -> tuple[str | None, tuple[str, ...]]:
    """Validate `PT_INTERP` and `DT_NEEDED` resolution against `rootfs_dir`."""
    interp, needed = read_dynamic_elf(elf_path, readobj=readobj)
    if expect_interp:
        if interp != expected_interp:
            raise ValueError(f"{elf_path}: expected PT_INTERP {expected_interp!r}, got {interp!r}")
        if _resolve_in_rootfs(rootfs_dir, expected_interp) is None:
            raise ValueError(f"{elf_path}: PT_INTERP {expected_interp!r} missing in {rootfs_dir}")
    elif interp is not None:
        raise ValueError(f"{elf_path}: expected no PT_INTERP, got {interp!r}")

    forbidden = [lib for lib in needed if lib in FORBIDDEN_NEEDED]
    if forbidden:
        raise ValueError(f"{elf_path}: forbidden DT_NEEDED libraries: {forbidden}")

    missing = [lib for lib in needed if not _resolves_in_rootfs(rootfs_dir, lib)]
    if missing:
        raise ValueError(f"{elf_path}: unresolved DT_NEEDED in {rootfs_dir}: {missing}")

    return interp, needed


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="lumen_harness.rootfs")
    p.add_argument("elf", type=Path, help="ELF binary or shared object to inspect")
    p.add_argument("--rootfs-dir", type=Path, required=True, help="staged rootfs directory")
    p.add_argument(
        "--no-interp",
        action="store_true",
        help="expect no PT_INTERP (for shared libraries / cdylib)",
    )
    a = p.parse_args(argv)
    try:
        interp, needed = validate_dynamic_elf(a.elf, a.rootfs_dir, expect_interp=not a.no_interp)
    except (RuntimeError, ValueError, OSError) as exc:
        print(f"rootfs check-elf: {exc}", file=sys.stderr)
        return 1
    print(f"rootfs check-elf: {a.elf.name} ok (interp={interp}, needed={list(needed)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
