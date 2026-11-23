"""Make the ktests' ext4 image with unsigned directory hashes (#308), written to
kernel/src/vfs/testdata/unsigned-hash.ext4:

a 160 KiB ext4 volume (1 KiB blocks; mkrootfs.sh's #248 feature mask, so no
journal, minus `resize_inode` to keep it small) whose superblock sets `EXT2_FLAGS_UNSIGNED_HASH` (`s_flags` 0x2, as
mke2fs does where `char` is unsigned, e.g. Linux on aarch64), holding one
indexed (htree, half_md4) directory `/d` built by `e2fsck -fyD` with the
unsigned hash: 80 ASCII names `entry-number-NN` (each file holding `NN`) and
three names with bytes of 0x80 or above,

- `é-utf8` (valid UTF-8, holding `x`),
- `ff fe -odd` (not UTF-8, holding `y`),
- `80 81 82 83 84` (not UTF-8, holding `z`),

each filed in a leaf block that the *signed* hash of its name doesn't route to,
which this script checks (the ktest's claim needs it: a lookup hashing signed,
as ext4plus 0.1.0-rc.2 does, misses all three). It also checks the image with
`e2fsck -fn`.

The names go in through `mke2fs -d` as ASCII stand-ins of the same length
(macOS refuses names that aren't UTF-8), patched to their bytes in the image
before `e2fsck -fyD` indexes the directory.

Deterministic for a given e2fsprogs and mke2fs.conf (fixed UUID, hash seed
and times): the committed file was made with e2fsprogs 1.47.4 and its stock
mke2fs.conf (Homebrew's; `-t ext4` takes its feature defaults from there,
and half_md4 is mke2fs's built-in default hash), found as `lumen_harness.rootfs.find_e2fsprogs`
finds them (`LUMEN_E2FSPROGS_SBIN` first). It is committed, so the build
never runs this; run it again only to change it:

    uv run python tools/ext4-unsigned-hash-image.py
"""

from __future__ import annotations

import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools" / "harness"))

from lumen_harness.rootfs import find_e2fsprogs

OUT = ROOT / "kernel" / "src" / "vfs" / "testdata" / "unsigned-hash.ext4"
SIZE = "160K"
EPOCH = "1700000000"
# The hash seed decides each name's hash, and so the leaf it lands in. This
# one sends all three names' signed hashes to another leaf than their entry's
# (checked below); if a change breaks that, `main` tries the other seeds and
# names one that works.
UUID = "4c756d65-6e2d-4d37-8000-00000000030c"
SEEDS = [
    UUID,
    *(s for n in range(0x309, 0x400) if (s := f"4c756d65-6e2d-4d37-8000-{n:012x}") != UUID),
]
# (stand-in, name, contents): each stand-in as long as its name.
ODD = (
    (b"QQ-utf8", "é-utf8".encode(), b"x"),
    (b"QQ-odd", b"\xff\xfe-odd", b"y"),
    (b"Zq8x3", b"\x80\x81\x82\x83\x84", b"z"),
)
ENTRIES = 80
FIELDS = (
    b"uid 0",
    b"gid 0",
    b"ctime @" + EPOCH.encode(),
    b"ctime_extra 0",
    b"atime @" + EPOCH.encode(),
    b"atime_extra 0",
)


def run(*args: object, env_extra: dict[str, str] | None = None, ok=(0,)) -> bytes:
    env = {
        **os.environ,
        "LC_ALL": "C",
        "E2FSPROGS_FAKE_TIME": EPOCH,
        "E2FSCK_TIME": EPOCH,
        **(env_extra or {}),
    }
    proc = subprocess.run([str(a) for a in args], capture_output=True, env=env, check=False)
    if proc.returncode not in ok:
        sys.stderr.buffer.write(proc.stdout + proc.stderr)
        raise SystemExit(f"{args[0]} exited {proc.returncode}")
    return proc.stdout


DEBUGFS_BANNER = re.compile(rb"debugfs \S+ \(.*\)$")


def run_debugfs(debugfs: Path, *args: object) -> bytes:
    """`run` for debugfs, which exits 0 when a request fails: any stderr line
    but its version banner fails it too."""
    env = {**os.environ, "LC_ALL": "C", "E2FSPROGS_FAKE_TIME": EPOCH}
    proc = subprocess.run(
        [str(debugfs), *map(str, args)], capture_output=True, env=env, check=False
    )
    errors = [line for line in proc.stderr.splitlines() if line and not DEBUGFS_BANNER.match(line)]
    if proc.returncode != 0 or errors:
        sys.stderr.buffer.write(proc.stdout + proc.stderr)
        raise SystemExit(f"debugfs {args} exited {proc.returncode}")
    return proc.stdout


ROOT_ENTRY = re.compile(rb"Entry #\d+: Hash 0x([0-9a-f]{8}), block (\d+)")
LEAF = re.compile(rb"Reading directory block (\d+), phys \d+")
DIRENT = re.compile(rb"(\d+) 0x([0-9a-f]{8})-[0-9a-f]{8} \(\d+\) (\S+)")


def htree(debugfs: Path, image: Path) -> tuple[list[tuple[int, int]], dict[bytes, tuple[int, int]]]:
    """`/d`'s root entries (hash, block) and each name's (hash, leaf block)."""
    dump = run(debugfs, "-R", "htree d", image)
    root_part, _, leaves_part = dump.partition(b"\n\n")
    roots = [(int(h, 16), int(b)) for h, b in ROOT_ENTRY.findall(root_part)]
    names: dict[bytes, tuple[int, int]] = {}
    block = None
    for line in leaves_part.splitlines():
        if m := LEAF.search(line):
            block = int(m.group(1))
            continue
        for _ino, h, name in DIRENT.findall(line):
            names[name] = (int(h, 16), block)
    if len(roots) < 2:
        raise SystemExit(f"/d isn't indexed: {dump!r}")
    return roots, names


def leaf_for(roots: list[tuple[int, int]], h: int) -> int:
    """The leaf block a lookup of hash `h` reads first."""
    return [b for start, b in roots if start <= h][-1]  # the first entry starts at 0


def build(seed: str, mke2fs: Path, e2fsck: Path, debugfs: Path, work: Path) -> Path:
    stage = work / "stage"
    (stage / "d").mkdir(parents=True)
    for n in range(ENTRIES):
        (stage / "d" / f"entry-number-{n:02}").write_bytes(f"{n:02}".encode())
    for stand_in, _name, data in ODD:
        (stage / "d" / stand_in.decode()).write_bytes(data)
    for p in sorted(stage.rglob("*"), reverse=True) + [stage]:
        os.utime(p, (int(EPOCH), int(EPOCH)), follow_symlinks=False)
    image = work / "unsigned.ext4"
    run(
        mke2fs, "-q", "-F", "-t", "ext4", "-b", "1024", "-I", "256", "-N", "128",
        "-O", "^orphan_file,^metadata_csum_seed,^metadata_csum,^has_journal,^resize_inode",
        "-m", "0", "-L", "lumen-unsigned", "-U", UUID,
        "-E", f"hash_seed={seed},root_owner=0:0,lazy_itable_init=0,no_copy_xattrs",
        "-d", stage, image, SIZE,
        env_extra={"COPYFILE_DISABLE": "1"},
    )  # fmt: skip
    data = bytearray(image.read_bytes())
    for stand_in, name, _data in ODD:
        assert len(stand_in) == len(name) and data.count(stand_in) == 1, stand_in
        at = data.index(stand_in)
        data[at : at + len(name)] = name
    image.write_bytes(data)
    # Owners as root and the change and access times fixed: `mke2fs -d`
    # copies the host's owners and times, and `os.utime` above can't set the
    # change time (nor keep a later read from moving the access time).
    paths = [
        b"d",
        *(b"d/entry-number-%02d" % i for i in range(ENTRIES)),
        *(b"d/" + n for _s, n, _d in ODD),
    ]
    (work / "sif.cmd").write_bytes(
        b"".join(b"".join(b"sif %s %s\n" % (p, f) for f in FIELDS) for p in paths)
    )
    run_debugfs(debugfs, "-w", "-f", work / "sif.cmd", image)
    run_debugfs(debugfs, "-w", "-R", "ssv flags 2", image)
    run(e2fsck, "-fyD", image, ok=(0, 1))
    run(e2fsck, "-fn", image)
    return image


def main() -> int:
    mke2fs, e2fsck = find_e2fsprogs()
    debugfs = mke2fs.parent / "debugfs"
    version = subprocess.run([str(mke2fs), "-V"], capture_output=True, text=True, check=False)
    print(f"with {(version.stderr or version.stdout).splitlines()[0]}")
    for seed in SEEDS:
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            image = build(seed, mke2fs, e2fsck, debugfs, work)
            roots, names = htree(debugfs, image)
            # The same tree with the signed flag, re-indexed: each name's signed hash.
            signed = work / "signed.ext4"
            signed.write_bytes(image.read_bytes())
            run_debugfs(debugfs, "-w", "-R", "ssv flags 1", signed)
            run(e2fsck, "-fyD", signed, ok=(0, 1))
            _, signed_names = htree(debugfs, signed)
            missed = [
                name
                for _s, name, _d in ODD
                if leaf_for(roots, signed_names[name][0]) != names[name][1]
            ]
            if len(missed) == len(ODD) and len(names) == ENTRIES + len(ODD):
                if seed != UUID:
                    print(f"seed {seed} works: set UUID to it", file=sys.stderr)
                    return 1
                OUT.parent.mkdir(parents=True, exist_ok=True)
                OUT.write_bytes(image.read_bytes())
                print(f"wrote {OUT.relative_to(ROOT)} ({len(roots)} leaves)")
                return 0
    print("no seed sends all three signed hashes to another leaf", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
